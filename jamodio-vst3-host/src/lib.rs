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
mod host;
mod host_app;
mod loader;
mod state;

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
    instance: Instance,
    /// `Arc` car l'editor thread l'utilise aussi (sa propre référence garde
    /// la DLL en vie pendant la durée de vie de la window).
    module: Arc<LoadedModule>,
    #[allow(dead_code)]
    plugin_ref: PluginRef,
    latency: u32,
    editor: Option<EditorWindow>,
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
        let mut out = Vec::new();
        for dir in discovery::system_paths() {
            for path in discovery::scan_directory(&dir) {
                scan_plugin_file(&path, &mut out);
            }
        }
        out
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

        let module =
            Arc::new(LoadedModule::load(&PathBuf::from(path_str)).map_err(PluginError::Init)?);
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
            if let Err(e) = instance.process_stereo(&mut left, &mut right) {
                tracing::warn!(
                    target: "jamodio::vst3",
                    block = i,
                    error = %e,
                    "pre-warm block failed (ignoré)"
                );
                break;
            }
        }

        let handle_id = self.next_handle.fetch_add(1, Ordering::Relaxed) + 1;
        self.entries.insert(
            handle_id,
            Entry {
                instance,
                module,
                plugin_ref: plugin_ref.clone(),
                latency,
                editor: None,
            },
        );
        Ok(PluginHandle(handle_id))
    }

    fn unload(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        if self.entries.remove(&handle.0).is_none() {
            return Err(PluginError::InvalidHandle);
        }
        Ok(())
    }

    fn process_stereo(
        &mut self,
        handle: PluginHandle,
        left: &mut [f32],
        right: &mut [f32],
        _midi_events: &[MidiEvent],
    ) -> Result<(), PluginError> {
        // S2 : `_midi_events` sera transmis au plugin via IEventList avant
        // process(). Pas implémenté en S1 — les plugins d'effets purs n'en
        // consomment pas, et le MIDI Windows arrive en S2.
        let entry = self
            .entries
            .get_mut(&handle.0)
            .ok_or(PluginError::InvalidHandle)?;
        entry
            .instance
            .process_stereo(left, right)
            .map_err(PluginError::Process)
    }

    fn latency_samples(&self, handle: PluginHandle) -> u32 {
        self.entries.get(&handle.0).map(|e| e.latency).unwrap_or(0)
    }

    fn open_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        let entry = self
            .entries
            .get_mut(&handle.0)
            .ok_or(PluginError::InvalidHandle)?;
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
