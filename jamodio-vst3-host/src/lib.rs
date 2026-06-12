//! `jamodio-vst3-host` — VST3 plugin host pour Windows.
//!
//! Implémente le trait [`jamodio_audio_core::plugin_host::PluginHost`] en s'appuyant
//! sur la crate `vst3` (coupler-rs : bindings Rust pur du SDK Steinberg) et
//! `libloading` pour charger dynamiquement les `.vst3` (DLL Windows).
//!
//! Cible exclusive : `target_os = "windows"`. Le crate compile à vide sur les
//! autres OS pour ne pas casser `cargo check --workspace`. Le crate frère
//! `jamodio-au-host` couvre macOS sous la même abstraction `PluginHost`.

#![cfg(target_os = "windows")]

mod discovery;
mod editor;
mod conn_proxy;
mod events;
mod host;
mod host_app;
mod loader;
mod main_thread;
mod state;

/// Doit être appelé UNE fois au démarrage du thread audio RT (encoder_thread)
/// pour permettre au ConnectionProxy de filtrer les notify() venant de ce
/// thread. Sans ce marquage, le proxy ne sait pas qui est le thread audio
/// et laisse passer les notify, ce qui peut deadlock pendant `attached()`.
pub fn register_audio_thread() {
    crate::conn_proxy::register_audio_thread();
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use jamodio_audio_core::plugin_host::{
    MidiEvent, PluginError, PluginHandle, PluginHost, PluginInfo, PluginRef,
    MAX_PLUGIN_LATENCY_SAMPLES,
};

use crate::editor::EditorWindow;
use crate::host::{enumerate_classes, factory_info, parse_uid_hex, Instance};
use crate::loader::LoadedModule;

/// Nombre de blocs de silence à passer dans le plugin après load pour absorber
/// le warmup du 1er process() (= cache cold + allocation interne d'état).
/// POC mesuré : 1er bloc ~3000 µs, blocs suivants ~75 µs. 8 blocs garantissent
/// que le warmup est totalement amorti avant que le bypass soit dé-activé.
const PRE_WARM_BLOCKS: usize = 8;

/// Sample rate Jamodio (WebRTC) — toujours 48 kHz côté agent.
const SAMPLE_RATE: f64 = 48_000.0;

/// VST3 plugin host.
///
/// Sûr à `Send` — l'accès est sérialisé par le caller (= `parking_lot::Mutex<Vst3Host>`
/// dans `PipelineState`, comme pour `AuHost`).
pub struct Vst3Host {
    next_handle: AtomicU32,
    entries: HashMap<u32, Entry>,
}

struct Entry {
    /// Déclaré AVANT `instance` → droppé AVANT elle : la fenêtre éditeur
    /// (view.removed) doit être fermée avant le `terminate()` du component.
    /// (Le teardown propre passe de toute façon par vst3-main : cf. `unload`
    /// et `Drop for Vst3Host` ; cet ordre de champs est une défense en plus.)
    editor: Option<EditorWindow>,
    instance: Instance,
    /// `Arc` car la registry éditeur sur vst3-main en garde aussi une
    /// référence (la DLL doit rester en vie pendant la durée de vie de la
    /// window). Déclaré APRÈS `instance` → droppé après elle (la DLL reste
    /// chargée tant que l'Instance tient ses ComPtr).
    module: Arc<LoadedModule>,
    #[allow(dead_code)]
    plugin_ref: PluginRef,
    latency: u32,
    /// Queue d'events MIDI poussés par `dispatch_midi_only` (= clavier HTML
    /// via WS PlayMidiNote). Drainée au prochain `process_stereo`.
    /// Concurrence : push depuis le thread WS, drain depuis l'encoder thread.
    /// `parking_lot::Mutex` pour acquire ~25ns négligeable vs budget bloc.
    pending_midi: parking_lot::Mutex<Vec<MidiEvent>>,
}

impl Vst3Host {
    pub fn new() -> Self {
        Self {
            next_handle: AtomicU32::new(0),
            entries: HashMap::new(),
        }
    }
}

impl Default for Vst3Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Vst3Host {
    fn drop(&mut self) {
        // Au shutdown agent, le Vst3Host peut être droppé depuis n'importe
        // quel thread. Or fermer les éditeurs (view.removed) et `terminate()`
        // les composants DOIVENT se faire sur vst3-main (règle single-main-
        // thread VST3). On route donc tout le teardown via `main_thread::run`,
        // éditeur fermé AVANT le drop de l'Instance.
        if self.entries.is_empty() {
            return;
        }
        let entries: Vec<Entry> = self.entries.drain().map(|(_, e)| e).collect();
        crate::main_thread::run(move || {
            for mut e in entries {
                drop(e.editor.take()); // close éditeur (inline car on est sur vst3-main)
                drop(e); // Instance::drop → terminate sur vst3-main
            }
        });
    }
}

// ---------- Scan ----------

/// Scan d'un seul plugin : load → factory info → énumère classes → instancie
/// chaque Audio Module Class pour lire latence/has_input_bus/has_editor.
///
/// Best-effort : si le plugin crash au load OU au setup, on log et on passe au
/// suivant. La crash isolation par sub-process viendra en S3.
fn scan_plugin_file(path: &Path, out: &mut Vec<PluginInfo>) {
    let module = match LoadedModule::load(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                target: "jamodio::vst3",
                path = %path.display(),
                error = %e,
                "scan: load failed"
            );
            return;
        }
    };

    let vendor = factory_info(&module)
        .map(|f| f.vendor)
        .unwrap_or_default();

    let classes = enumerate_classes(&module);
    for class in classes {
        if !class.is_audio_effect() {
            continue;
        }
        // Instancie pour récupérer latence + bus info + has_editor.
        let mut instance = match Instance::create_by_uid(&module, &class.cid) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(
                    target: "jamodio::vst3",
                    plugin = %class.name,
                    error = %e,
                    "scan: createInstance failed"
                );
                continue;
            }
        };
        if let Err(e) = instance.setup_stereo(SAMPLE_RATE, 64) {
            tracing::warn!(
                target: "jamodio::vst3",
                plugin = %class.name,
                error = %e,
                "scan: setup_stereo failed"
            );
            continue;
        }
        let latency = instance.latency_samples();
        let has_input_bus = instance.has_input_bus();

        // has_editor : best-effort. On considère qu'un plugin avec un
        // IEditController (= cast succès ou getControllerClassId valide) a un
        // éditeur. Pas de createView() en scan = trop coûteux.
        let has_editor = {
            use vst3::Steinberg::{
                Vst::{IComponentTrait, IEditController},
                TUID,
            };
            if instance.component.cast::<IEditController>().is_some() {
                true
            } else {
                let mut cid: TUID = [0; 16];
                let ok =
                    unsafe { instance.component.getControllerClassId(&mut cid as *mut TUID) };
                ok == 0
            }
        };

        out.push(PluginInfo {
            name: class.name.clone(),
            manufacturer: vendor.clone(),
            plugin_ref: PluginRef::Vst3 {
                path: path.to_string_lossy().into_owned(),
                uid: class.uid_hex(),
            },
            latency_samples: latency,
            has_editor,
            incompatible: latency > MAX_PLUGIN_LATENCY_SAMPLES,
            has_input_bus,
        });
        // Drop instance ici (Drop::drop → setActive(false) + terminate + release).
    }
    // Drop module ici → factory release puis Library dlclose.
}

// ---------- Trait impl ----------

impl PluginHost for Vst3Host {
    fn scan(&self) -> Vec<PluginInfo> {
        // Sur vst3-main : le PREMIER chargement d'un module JUCE lie son
        // MessageManager au thread courant — ce doit être vst3-main (= le
        // thread qui pompe), sinon l'éditeur deadlock plus tard (cf.
        // main_thread.rs).
        main_thread::run(|| {
            let mut out = Vec::new();
            for dir in discovery::system_paths() {
                for path in discovery::scan_directory(&dir) {
                    scan_plugin_file(&path, &mut out);
                }
            }
            out
        })
    }

    fn load(
        &mut self,
        plugin_ref: &PluginRef,
        max_frames: u32,
    ) -> Result<PluginHandle, PluginError> {
        let (path_str, uid_hex) = match plugin_ref {
            PluginRef::Vst3 { path, uid } => (path.as_str(), uid.as_str()),
            PluginRef::Au { .. } => {
                return Err(PluginError::Init(
                    "AU plugin requested on VST3 host".into(),
                ));
            }
        };
        let cid = parse_uid_hex(uid_hex).ok_or_else(|| {
            PluginError::Init(format!("UID hex invalide : {uid_hex}"))
        })?;

        // Load + setup + pre-warm sur vst3-main (cf. scan() pour le pourquoi).
        let path_buf = PathBuf::from(path_str);
        let (module, instance, latency) = main_thread::run(move || -> Result<_, PluginError> {
            let module =
                Arc::new(LoadedModule::load(&path_buf).map_err(PluginError::Init)?);
            let mut instance =
                Instance::create_by_uid(&module, &cid).map_err(PluginError::Init)?;
            instance
                .setup_stereo(SAMPLE_RATE, max_frames as i32)
                .map_err(PluginError::Init)?;
            let latency = instance.latency_samples();

            // Pre-warm : passe N blocs de silence pour absorber le coût du 1er
            // process() (cache cold + allocation interne du plugin). Sans ça, le
            // 1er bloc audio capturé après load() peut dépasser le budget temps
            // et glitcher. Mesuré POC : 1er bloc = ~3000 µs, suivants = ~75 µs.
            let block = max_frames as usize;
            let mut left = vec![0.0f32; block];
            let mut right = vec![0.0f32; block];
            for i in 0..PRE_WARM_BLOCKS {
                if let Err(e) = instance.process_stereo(&mut left, &mut right, &[]) {
                    tracing::warn!(
                        target: "jamodio::vst3",
                        block = i,
                        error = %e,
                        "pre-warm block failed (ignoré)"
                    );
                    break;
                }
            }
            Ok((module, instance, latency))
        })?;

        let handle_id = self.next_handle.fetch_add(1, Ordering::Relaxed) + 1;
        self.entries.insert(
            handle_id,
            Entry {
                instance,
                module,
                plugin_ref: plugin_ref.clone(),
                latency,
                editor: None,
                pending_midi: parking_lot::Mutex::new(Vec::with_capacity(32)),
            },
        );
        Ok(PluginHandle(handle_id))
    }

    fn unload(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        let Some(entry) = self.entries.remove(&handle.0) else {
            return Err(PluginError::InvalidHandle);
        };
        // Teardown sur vst3-main, SANS bloquer le caller (qui tient le lock
        // plugin_host). Ordre critique : fermer l'éditeur d'abord (DestroyWindow
        // synchrone sur vst3-main → view.removed() + release controller/view),
        // PUIS dropper l'Instance (setActive(false) + terminate). L'ancien code
        // droppait l'Entry telle quelle = terminate AVANT la fermeture de
        // l'éditeur (use-after-terminate latent).
        main_thread::post(move || {
            let mut entry = entry;
            drop(entry.editor.take());
            drop(entry);
        });
        Ok(())
    }

    fn process_stereo(
        &mut self,
        handle: PluginHandle,
        left: &mut [f32],
        right: &mut [f32],
        midi_events: &[MidiEvent],
    ) -> Result<(), PluginError> {
        let entry = self
            .entries
            .get_mut(&handle.0)
            .ok_or(PluginError::InvalidHandle)?;

        // Drain les events MIDI accumulés via dispatch_midi_only (= clavier
        // HTML). Combine avec les events du param (= drain physique MIDI USB
        // fait par l'encoder_thread). Les 2 sources arrivent en même temps
        // au plugin via une seule IEventList → cohérent avec le sample offset.
        let pending: Vec<MidiEvent> = std::mem::take(&mut *entry.pending_midi.lock());
        let result = if pending.is_empty() && midi_events.is_empty() {
            entry.instance.process_stereo(left, right, &[])
        } else {
            let mut all = pending;
            all.extend_from_slice(midi_events);
            entry.instance.process_stereo(left, right, &all)
        };
        result.map_err(PluginError::Process)
    }

    fn latency_samples(&self, handle: PluginHandle) -> u32 {
        self.entries.get(&handle.0).map(|e| e.latency).unwrap_or(0)
    }

    fn open_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        let entry = self
            .entries
            .get_mut(&handle.0)
            .ok_or(PluginError::InvalidHandle)?;
        // Fenêtre fermée par l'utilisateur (X) ou setup échoué → autorise la
        // réouverture au lieu de retourner Ok sur un handle mort.
        if entry.editor.as_ref().is_some_and(|e| e.is_closed()) {
            entry.editor = None;
        }
        if entry.editor.is_some() {
            return Ok(());
        }
        let title = format!("{} — Jamodio", entry.instance.class.name);
        let module = entry.module.clone();
        let editor = EditorWindow::open(&entry.instance, module, &title)
            .map_err(PluginError::Process)?;
        entry.editor = Some(editor);
        Ok(())
    }

    fn close_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        let entry = self
            .entries
            .get_mut(&handle.0)
            .ok_or(PluginError::InvalidHandle)?;
        entry.editor = None; // Drop → close()
        Ok(())
    }
}

impl Vst3Host {
    /// Dispatche un batch d'events MIDI au plugin actif SANS appeler process.
    ///
    /// Utilisé par le clavier HTML virtuel (= note ON/OFF déclenchée par clic
    /// browser, WS handler `PlayMidiNote`). Les events sont stockés dans la
    /// queue `pending_midi` de l'entry et seront consommés au prochain
    /// `process_stereo` appelé par l'encoder_thread.
    ///
    /// Miroir API de `AuHost::dispatch_midi_only` pour que le call-site WS
    /// soit OS-agnostic : `pl.plugin_host.lock().dispatch_midi_only(handle, &[ev])`.
    pub fn dispatch_midi_only(
        &mut self,
        handle: PluginHandle,
        midi_events: &[MidiEvent],
    ) -> Result<(), PluginError> {
        if midi_events.is_empty() {
            return Ok(());
        }
        let entry = self
            .entries
            .get_mut(&handle.0)
            .ok_or(PluginError::InvalidHandle)?;
        entry.pending_midi.lock().extend_from_slice(midi_events);
        Ok(())
    }
}

// ---------- Tests ----------
//
// Les tests réels (avec plugins installés) tournent uniquement sur la VM
// Windows de Ben. Sur d'autres machines (CI, dev Mac compilant Win cross),
// ils sont compilés mais skipped.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_creates_and_drops() {
        let _h = Vst3Host::new();
    }

    #[test]
    fn scan_returns_a_vec() {
        // Sur une machine sans plugins, scan() peut retourner vide — ce n'est
        // pas une erreur. Sur la VM avec ValhallaFutureVerb installé, vide
        // = bug. Le test n'asserte pas la non-vacuité pour rester portable.
        let h = Vst3Host::new();
        let _plugins = h.scan();
    }

    #[test]
    fn unload_invalid_handle_errors() {
        let mut h = Vst3Host::new();
        let r = h.unload(PluginHandle(999));
        assert!(matches!(r, Err(PluginError::InvalidHandle)));
    }
}
