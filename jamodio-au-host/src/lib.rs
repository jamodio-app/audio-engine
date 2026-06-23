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
    MidiEvent, PluginError, PluginHandle, PluginHost, PluginInfo, PluginRef,
    MAX_PLUGIN_LATENCY_SAMPLES,
};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

// Sprint S2 — bindings CoreAudio Workgroup (os_workgroup_join). Module séparé
// pour ne pas surcharger ce fichier déjà long. API publique : AudioWorkgroup,
// is_available(). Cf. PLAN-EXECUTION-AGENT-STABILITE.md §S2.1-S2.2.
pub mod workgroup;

// ---------- FFI ----------

type AuScanCb = unsafe extern "C" fn(
    ctx: *mut c_void,
    au_type: u32,
    au_subtype: u32,
    au_manuf: u32,
    name: *const c_char,
    latency_samples: u32,
    has_editor: c_int,
    has_input_bus: c_int,
);

extern "C" {
    fn au_host_create() -> *mut c_void;
    fn au_host_destroy(p: *mut c_void);
    fn au_host_scan(p: *mut c_void, cb: AuScanCb, ctx: *mut c_void);
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
}

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

// ---------- Scan callback ----------
//
// Le callback C reçoit les attributs d'un AU à la fois. On les pousse
// dans un Vec<PluginInfo> dont le pointer est passé via `ctx`.

struct ScanCtx {
    out: Vec<PluginInfo>,
}

unsafe extern "C" fn scan_thunk(
    ctx: *mut c_void,
    au_type: u32,
    au_subtype: u32,
    au_manuf: u32,
    name: *const c_char,
    latency_samples: u32,
    has_editor: c_int,
    has_input_bus: c_int,
) {
    let ctx = &mut *(ctx as *mut ScanCtx);
    let raw = match CStr::from_ptr(name).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return,
    };
    // Convention Apple : `AudioComponentCopyName` retourne "Vendor: PluginName".
    let (manufacturer, plugin_name) = match raw.split_once(": ") {
        Some((m, n)) => (m.to_string(), n.to_string()),
        None => (String::new(), raw),
    };

    let au_type_fcc = u32_to_fourcc(au_type);
    // Un AU est un instrument ssi son composant est de type MusicDevice
    // (`aumu`). Reproduit à l'identique l'ancienne détection côté browser
    // (`format==='au' && auType==='aumu'`) — désormais autoritaire côté agent.
    let is_instrument = au_type_fcc == "aumu";

    ctx.out.push(PluginInfo {
        name: plugin_name,
        manufacturer,
        plugin_ref: PluginRef::Au {
            au_type: au_type_fcc,
            subtype: u32_to_fourcc(au_subtype),
            manufacturer: u32_to_fourcc(au_manuf),
        },
        latency_samples,
        has_editor: has_editor != 0,
        incompatible: latency_samples > MAX_PLUGIN_LATENCY_SAMPLES,
        has_input_bus: has_input_bus != 0,
        is_instrument,
    });
}

// ---------- Trait impl ----------

impl PluginHost for AuHost {
    fn scan(&self) -> Vec<PluginInfo> {
        let mut ctx = ScanCtx { out: Vec::new() };
        unsafe {
            au_host_scan(
                self.ptr,
                scan_thunk,
                &mut ctx as *mut ScanCtx as *mut c_void,
            );
        }
        ctx.out
    }

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

    #[test]
    fn scan_finds_apple_natives() {
        let h = AuHost::new();
        let plugins = h.scan();
        assert!(
            !plugins.is_empty(),
            "Scan should at least find Apple native AUs on macOS"
        );
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"AUMatrixReverb"),
            "AUMatrixReverb missing from scan: {:?}",
            names
        );
        assert!(
            names.contains(&"AUNBandEQ"),
            "AUNBandEQ missing from scan"
        );
    }

    #[test]
    fn scan_flags_apple_dynamics_incompatible() {
        let h = AuHost::new();
        let plugins = h.scan();
        let dcmp = plugins
            .iter()
            .find(|p| p.name == "AUDynamicsProcessor")
            .expect("AUDynamicsProcessor not found");
        // POC mesure : 256 samples → incompatible (>64).
        assert!(dcmp.incompatible);
        assert!(dcmp.latency_samples > MAX_PLUGIN_LATENCY_SAMPLES);
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
