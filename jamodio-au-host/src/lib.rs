//! `jamodio-au-host` — AudioUnit plugin host pour macOS.
//!
//! Implémente le trait [`jamodio_audio_core::plugin_host::PluginHost`] en s'appuyant
//! sur les frameworks Apple (AudioToolbox, AVFoundation, CoreAudioKit). Toute la
//! logique ObjC++ vit dans `cpp/au_host.mm`, ce module expose uniquement les
//! bindings Rust safe.
//!
//! Cible exclusive : `target_os = "macos"`. Le crate compile à vide sur les
//! autres OS pour ne pas casser `cargo check --workspace` (cf. roadmap Windows
//! phase 2 = nouveau crate `jamodio-vst3-host`).

#![cfg(target_os = "macos")]

use jamodio_audio_core::plugin_host::{
    latency_exceeds_live_budget, MidiEvent, PluginError, PluginHandle, PluginHost, PluginInfo,
    PluginRef,
};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

// Sprint S2 — bindings CoreAudio Workgroup (os_workgroup_join). Module séparé
// pour ne pas surcharger ce fichier déjà long. API publique : AudioWorkgroup,
// is_available(). Cf. PLAN-EXECUTION-AGENT-STABILITE.md §S2.1-S2.2.
pub mod workgroup;

// ---------- FFI ----------

extern "C" {
    fn au_host_create() -> *mut c_void;
    fn au_host_destroy(p: *mut c_void);
    fn au_host_load(
        p: *mut c_void,
        au_type: u32,
        au_subtype: u32,
        au_manuf: u32,
        max_frames: u32,
        err_buf: *mut c_char,
        err_size: usize,
    ) -> u32;
    fn au_host_unload(p: *mut c_void, handle_id: u32) -> c_int;
    fn au_host_process_stereo(
        p: *mut c_void,
        handle_id: u32,
        left: *mut f32,
        right: *mut f32,
        n_frames: u32,
    ) -> c_int;
    fn au_host_dispatch_midi(
        p: *mut c_void,
        handle_id: u32,
        midi_data: *const u8,
        midi_count: u32,
    );
    fn au_host_latency_samples(p: *mut c_void, handle_id: u32) -> u32;
    fn au_host_open_editor(p: *mut c_void, handle_id: u32) -> c_int;
    fn au_host_close_editor(p: *mut c_void, handle_id: u32) -> c_int;
    // 0.5.9-2 — scan out-of-process (cf. section « Scan out-of-process »).
    fn jmo_au_enumerate(cb: AuEnumCb, ctx: *mut c_void);
    // 0.5.9-4 — masque le process worker de scan (Dock/focus). Cf. suppress_dock.
    fn jmo_suppress_dock();
    // 0.5.11-4 — fait tourner la run loop main du worker de scan. Cf. run_main_loop.
    fn jmo_run_main_loop();
    fn jmo_au_probe(
        au_type: u32,
        au_subtype: u32,
        au_manuf: u32,
        name_buf: *mut c_char,
        name_size: usize,
        latency_samples: *mut u32,
        has_input_bus: *mut c_int,
    ) -> c_int;
    // 0.5.11-4 — nom lisible d'un AU sans instanciation. Cf. component_name.
    fn jmo_au_name(
        au_type: u32,
        au_subtype: u32,
        au_manuf: u32,
        name_buf: *mut c_char,
        name_size: usize,
    ) -> c_int;
}

type AuEnumCb = unsafe extern "C" fn(ctx: *mut c_void, au_type: u32, au_subtype: u32, au_manuf: u32);

// ---------- Helpers fourcc ----------

fn fourcc_to_u32(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.len() != 4 {
        return None;
    }
    Some(((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | (b[3] as u32))
}

fn u32_to_fourcc(v: u32) -> String {
    let buf = [
        ((v >> 24) & 0xFF) as u8,
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    ];
    String::from_utf8_lossy(&buf).into_owned()
}

// ---------- Wrapper ----------

/// Hôte AudioUnit. Détient un pointeur opaque vers l'objet ObjC++ géré par ARC.
/// Une instance par session studio. Sûr à `Send` (le code ObjC++ gère sa concurrence
/// avec `os_unfair_lock`).
pub struct AuHost {
    ptr: *mut c_void,
}

// SAFETY : le code ObjC++ sérialise les accès aux entries via os_unfair_lock,
// et l'invariant `process_stereo` non concurrent au load/unload est documenté
// pour le caller (capture thread = thread unique).
unsafe impl Send for AuHost {}

impl AuHost {
    pub fn new() -> Self {
        let ptr = unsafe { au_host_create() };
        assert!(!ptr.is_null(), "au_host_create returned null");
        AuHost { ptr }
    }
}

impl Default for AuHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AuHost {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { au_host_destroy(self.ptr) };
            self.ptr = ptr::null_mut();
        }
    }
}

// ---------- Construction PluginInfo ----------

/// Construit un `PluginInfo` depuis les attributs bruts d'un composant AU.
/// Source unique de vérité pour le nommage et la classification, utilisée par
/// la probe worker (`scan_component`, scan out-of-process 0.5.9-2).
fn build_plugin_info(
    au_type: u32,
    au_subtype: u32,
    au_manuf: u32,
    raw_name: &str,
    latency_samples: u32,
    has_editor: bool,
    has_input_bus: bool,
) -> PluginInfo {
    // Convention Apple : `AudioComponentCopyName` retourne "Vendor: PluginName".
    let (manufacturer, plugin_name) = match raw_name.split_once(": ") {
        Some((m, n)) => (m.to_string(), n.to_string()),
        None => (String::new(), raw_name.to_string()),
    };

    let au_type_fcc = u32_to_fourcc(au_type);
    // Un AU est un instrument ssi son composant est de type MusicDevice
    // (`aumu`). Reproduit à l'identique l'ancienne détection côté browser
    // (`format==='au' && auType==='aumu'`) — désormais autoritaire côté agent.
    let is_instrument = au_type_fcc == "aumu";

    PluginInfo {
        name: plugin_name,
        manufacturer,
        plugin_ref: PluginRef::Au {
            au_type: au_type_fcc,
            subtype: u32_to_fourcc(au_subtype),
            manufacturer: u32_to_fourcc(au_manuf),
        },
        latency_samples,
        has_editor,
        incompatible: latency_exceeds_live_budget(latency_samples),
        has_input_bus,
        is_instrument,
    }
}

// ---------- Scan out-of-process (0.5.9-2, PLAN-PLUGIN-SCAN-OOP) ----------

/// Identité d'un composant AU (4-CC), telle que produite par l'énumération
/// du registre. C'est l'« item » macOS du protocole worker (encodé
/// `au:{type}/{subtype}/{manufacturer}` côté agent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuComponentId {
    pub au_type: String,
    pub subtype: String,
    pub manufacturer: String,
}

/// Énumère le registre AudioComponent — lecture de données SEULE, aucun code
/// plugin exécuté : sûr dans le process agent (coordinateur). Mêmes types de
/// composants que le scan legacy (effets + instruments + music-effects).
pub fn enumerate_components() -> Vec<AuComponentId> {
    unsafe extern "C" fn thunk(ctx: *mut c_void, au_type: u32, au_subtype: u32, au_manuf: u32) {
        let out = &mut *(ctx as *mut Vec<AuComponentId>);
        out.push(AuComponentId {
            au_type: u32_to_fourcc(au_type),
            subtype: u32_to_fourcc(au_subtype),
            manufacturer: u32_to_fourcc(au_manuf),
        });
    }
    let mut out: Vec<AuComponentId> = Vec::new();
    unsafe { jmo_au_enumerate(thunk, &mut out as *mut Vec<AuComponentId> as *mut c_void) };
    out
}

/// Probe RÉELLE d'un composant (instanciation `AUAudioUnit`) — à n'appeler
/// QUE depuis le process worker jetable : un crash du constructeur d'un
/// plugin tiers tue le process appelant. TOUS les fabricants sont probés
/// (l'isolation process rend inutile toute mitigation « Apple natives
/// seulement ») → vraie latence, vrai `has_input_bus`.
///
/// `None` = composant introuvable (désinstallé entre énumération et probe)
/// ou nom illisible — l'item est simplement absent de la liste finale.
pub fn scan_component(au_type: &str, subtype: &str, manufacturer: &str) -> Option<PluginInfo> {
    let t = fourcc_to_u32(au_type)?;
    let st = fourcc_to_u32(subtype)?;
    let mf = fourcc_to_u32(manufacturer)?;

    let mut name_buf = [0u8; 256];
    let mut latency_samples: u32 = 0;
    let mut has_input_bus: c_int = 1;
    let found = unsafe {
        jmo_au_probe(
            t,
            st,
            mf,
            name_buf.as_mut_ptr() as *mut c_char,
            name_buf.len(),
            &mut latency_samples,
            &mut has_input_bus,
        )
    };
    if found == 0 {
        return None;
    }
    let raw_name = CStr::from_bytes_until_nul(&name_buf).ok()?.to_str().ok()?;
    if raw_name.is_empty() {
        return None;
    }

    // `has_editor = true` pour tous : les AU v2 sont affichables via
    // AUGenericView (cf. commentaire dans au_host.mm).
    Some(build_plugin_info(
        t,
        st,
        mf,
        raw_name,
        latency_samples,
        true,
        has_input_bus != 0,
    ))
}

/// Masque le process courant du Dock et de l'activation (macOS) — à appeler
/// tôt dans le worker de scan out-of-process. Le worker partage le binaire de
/// l'app (donc son Info.plist « app Regular ») ; sans ça il rebondirait dans
/// le Dock le temps du scan. `NSApplicationActivationPolicyProhibited`.
pub fn suppress_dock_for_helper() {
    unsafe { jmo_suppress_dock() };
}

/// Fait tourner la run loop Cocoa du thread APPELANT (le thread principal du
/// worker de scan). Cf. au_host.mm#jmo_run_main_loop : indispensable pour que
/// l'instanciation des plugins (dispatchée sur la main queue) s'exécute sur un
/// main pompé — sinon l'XPC de licence des plugins lourds hang → blocklist.
/// Doit être appelée APRÈS `suppress_dock_for_helper` (qui crée `NSApp`). Ne
/// retourne pas en usage normal : le thread de scan sort le process via `exit`.
pub fn run_main_loop() {
    unsafe { jmo_run_main_loop() };
}

/// Nom lisible d'un composant AU, SANS instanciation (lecture seule du registre
/// AudioComponent — aucun code plugin exécuté, sûr partout). Utilisé pour nommer
/// un plugin blocklisté (logs worker + note UI) alors que l'instanciation a
/// échoué. `None` si le composant est introuvable ou sans nom.
pub fn component_name(au_type: &str, subtype: &str, manufacturer: &str) -> Option<String> {
    let t = fourcc_to_u32(au_type)?;
    let st = fourcc_to_u32(subtype)?;
    let mf = fourcc_to_u32(manufacturer)?;
    let mut name_buf = [0u8; 256];
    let found =
        unsafe { jmo_au_name(t, st, mf, name_buf.as_mut_ptr() as *mut c_char, name_buf.len()) };
    if found == 0 {
        return None;
    }
    let raw = CStr::from_bytes_until_nul(&name_buf).ok()?.to_str().ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(raw.to_string())
}

// ---------- Trait impl ----------

impl PluginHost for AuHost {
    fn load(
        &mut self,
        plugin_ref: &PluginRef,
        max_frames: u32,
    ) -> Result<PluginHandle, PluginError> {
        let (t, st, mf) = match plugin_ref {
            PluginRef::Au {
                au_type,
                subtype,
                manufacturer,
            } => {
                let t = fourcc_to_u32(au_type).ok_or(PluginError::NotFound)?;
                let st = fourcc_to_u32(subtype).ok_or(PluginError::NotFound)?;
                let mf = fourcc_to_u32(manufacturer).ok_or(PluginError::NotFound)?;
                (t, st, mf)
            }
            PluginRef::Vst3 { .. } => {
                return Err(PluginError::Init("VST3 not supported by AuHost".into()));
            }
        };

        let mut err_buf = [0u8; 256];
        let id = unsafe {
            au_host_load(
                self.ptr,
                t,
                st,
                mf,
                max_frames,
                err_buf.as_mut_ptr() as *mut c_char,
                err_buf.len(),
            )
        };
        if id == 0 {
            let msg = CStr::from_bytes_until_nul(&err_buf)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".into());
            return Err(PluginError::Init(msg));
        }
        Ok(PluginHandle(id))
    }

    fn unload(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        let rc = unsafe { au_host_unload(self.ptr, handle.0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(PluginError::InvalidHandle)
        }
    }

    fn process_stereo(
        &mut self,
        handle: PluginHandle,
        left: &mut [f32],
        right: &mut [f32],
        midi_events: &[MidiEvent],
    ) -> Result<(), PluginError> {
        if left.len() != right.len() {
            return Err(PluginError::Process("L/R length mismatch".into()));
        }
        // S2 — Dispatche les events MIDI AVANT le render. `MidiEvent` est
        // `repr(Rust)` mais ses 3 bytes data sont contigus → on transmet
        // un buffer flat de N×3 bytes au C. Si pas d'events, no-op.
        if !midi_events.is_empty() {
            // Pré-allouer un Vec<u8> est OK ici (alloc dans le hot path mais
            // rare : ~10 events typiques par bloc, capacité 30 bytes). En
            // optim future on peut buffer-recycle.
            let mut packed = Vec::with_capacity(midi_events.len() * 3);
            for ev in midi_events {
                packed.push(ev.data[0]);
                packed.push(ev.data[1]);
                packed.push(ev.data[2]);
            }
            unsafe {
                au_host_dispatch_midi(
                    self.ptr,
                    handle.0,
                    packed.as_ptr(),
                    midi_events.len() as u32,
                );
            }
        }
        let rc = unsafe {
            au_host_process_stereo(
                self.ptr,
                handle.0,
                left.as_mut_ptr(),
                right.as_mut_ptr(),
                left.len() as u32,
            )
        };
        match rc {
            0 => Ok(()),
            -1 => Err(PluginError::InvalidHandle),
            _ => Err(PluginError::Process(format!("renderBlock rc={rc}"))),
        }
    }

    fn latency_samples(&self, handle: PluginHandle) -> u32 {
        unsafe { au_host_latency_samples(self.ptr, handle.0) }
    }

    fn open_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        let rc = unsafe { au_host_open_editor(self.ptr, handle.0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(PluginError::InvalidHandle)
        }
    }

    fn close_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError> {
        let rc = unsafe { au_host_close_editor(self.ptr, handle.0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(PluginError::InvalidHandle)
        }
    }
}

// MidiEvent réservé pour S2 — pas encore consommé par AuHost.
#[allow(dead_code)]
fn _midi_event_reserved(_e: &MidiEvent) {}

impl AuHost {
    /// S2.9 — Dispatche un batch d'events MIDI vers un plugin SANS process_stereo.
    /// Utilisé par le clavier virtuel HTML (= note ON/OFF déclenchées par click),
    /// le WS handler `PlayMidiNote` consomme ça. Le plugin scheduleera l'event
    /// au prochain render block depuis le encoder_thread.
    pub fn dispatch_midi_only(
        &mut self,
        handle: PluginHandle,
        midi_events: &[MidiEvent],
    ) -> Result<(), PluginError> {
        if midi_events.is_empty() {
            return Ok(());
        }
        let mut packed = Vec::with_capacity(midi_events.len() * 3);
        for ev in midi_events {
            packed.push(ev.data[0]);
            packed.push(ev.data[1]);
            packed.push(ev.data[2]);
        }
        unsafe {
            au_host_dispatch_midi(
                self.ptr,
                handle.0,
                packed.as_ptr(),
                midi_events.len() as u32,
            );
        }
        Ok(())
    }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_creates_and_drops() {
        let _h = AuHost::new();
        // Drop appelle au_host_destroy.
    }

    /// Le scan out-of-process = énumération du registre puis probe par
    /// composant. On vérifie que les AU Apple natifs (présents sur toute
    /// machine macOS) sont énumérés puis correctement probés.
    #[test]
    fn enumerate_and_probe_finds_apple_natives() {
        let ids = enumerate_components();
        assert!(!ids.is_empty(), "enumerate should list AudioComponents");

        for (st, name) in [("mrev", "AUMatrixReverb"), ("nbeq", "AUNBandEQ")] {
            assert!(
                ids.iter()
                    .any(|c| c.au_type == "aufx" && c.subtype == st && c.manufacturer == "appl"),
                "{name} absent de l'énumération"
            );
            let info = scan_component("aufx", st, "appl")
                .unwrap_or_else(|| panic!("probe {name} a échoué"));
            assert_eq!(info.name, name);
            assert_eq!(info.manufacturer, "Apple");
            assert!(info.has_input_bus);
            assert!(!info.is_instrument);
            assert!(!info.incompatible);
        }
    }

    /// La probe d'un composant désinstallé/inconnu retourne None (pas de
    /// panic, pas d'entrée fantôme dans la liste).
    #[test]
    fn probe_unknown_component_returns_none() {
        assert_eq!(scan_component("xxxx", "yyyy", "zzzz"), None);
        // 4-CC invalide (longueur ≠ 4) → None aussi.
        assert_eq!(scan_component("au", "mrev", "appl"), None);
    }

    #[test]
    fn scan_flags_apple_dynamics_incompatible() {
        let dcmp = scan_component("aufx", "dcmp", "appl")
            .expect("probe AUDynamicsProcessor a échoué");
        assert_eq!(dcmp.name, "AUDynamicsProcessor");
        // POC mesure : 256 samples → au-delà du budget live (128) → incompatible.
        assert!(dcmp.incompatible);
        assert!(latency_exceeds_live_budget(dcmp.latency_samples));
    }

    #[test]
    fn load_unload_aumatrixreverb() {
        let mut h = AuHost::new();
        let ref_ = PluginRef::Au {
            au_type: "aufx".into(),
            subtype: "mrev".into(),
            manufacturer: "appl".into(),
        };
        let handle = h.load(&ref_, 64).expect("load AUMatrixReverb");
        assert!(handle.is_valid());
        assert_eq!(h.latency_samples(handle), 0);
        h.unload(handle).expect("unload");
    }

    #[test]
    fn process_passes_through_eq() {
        // AUNBandEQ est un passthrough par défaut (gains à 0 dB sur toutes les bandes)
        // → sine in ≈ sine out. Test simple et déterministe.
        let mut h = AuHost::new();
        let handle = h
            .load(
                &PluginRef::Au {
                    au_type: "aufx".into(),
                    subtype: "nbeq".into(),
                    manufacturer: "appl".into(),
                },
                64,
            )
            .expect("load AUNBandEQ");

        // Sine 1kHz @ 48k sur les 64 samples.
        let mut left: Vec<f32> = (0..64)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin())
            .collect();
        let mut right = left.clone();
        let in_energy: f32 = left.iter().map(|s| s.abs()).sum();

        h.process_stereo(handle, &mut left, &mut right, &[]).expect("process");

        let out_energy: f32 = left.iter().chain(right.iter()).map(|s| s.abs()).sum();
        assert!(
            out_energy > 0.5 * in_energy,
            "EQ passthrough should preserve energy ; in={in_energy} out={out_energy}"
        );
        h.unload(handle).ok();
    }

    #[test]
    fn double_process_keeps_working() {
        // Régression : le premier appel pourrait passer mais le second échouer
        // (cursor sample_time, état stale, etc.). Vérifie 100 blocs consécutifs.
        let mut h = AuHost::new();
        let handle = h
            .load(
                &PluginRef::Au {
                    au_type: "aufx".into(),
                    subtype: "nbeq".into(),
                    manufacturer: "appl".into(),
                },
                64,
            )
            .expect("load");
        let mut left = vec![0.1f32; 64];
        let mut right = vec![0.1f32; 64];
        for _ in 0..100 {
            h.process_stereo(handle, &mut left, &mut right, &[]).expect("process");
        }
        h.unload(handle).ok();
    }
}
