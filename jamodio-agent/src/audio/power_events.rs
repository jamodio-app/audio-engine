//! 0.5.4-18 — Réveil de veille Windows → re-init complet du driver ASIO.
//!
//! # Pourquoi (robustesse produit, tous modèles d'interface)
//!
//! Au retour de veille système (S3/S0ix) — ou simplement au réveil d'une interface
//! USB mise en sommeil — un driver ASIO peut : ne plus délivrer de callbacks, en
//! délivrer mais avec un contenu FIGÉ/railé (cf. bug cold-start 2026-07-03), ou
//! revenir dans un état bancal. Le filet de liveness (débit de callbacks) et le
//! `kAsioResetRequest` NE couvrent PAS le cas « callbacks vivants mais contenu
//! gelé ». La seule réponse fiable et **générique** (indépendante du vendor) après
//! un réveil système est un re-init propre `ASIOExit → ASIOInit → CreateBuffers →
//! ASIOStart` — exactement ce que fait déjà `ws_server::repair_audio_streams`.
//!
//! # Mécanisme
//!
//! On écoute les notifications d'alimentation via
//! `PowerRegisterSuspendResumeNotification` avec `DEVICE_NOTIFY_CALLBACK` : un
//! callback direct, **sans fenêtre ni pompe de messages**. Au resume, il incrémente
//! un compteur + réveille (`Notify`) le superviseur de liveness, qui déclenche le
//! reset borné existant. L'enregistrement dure toute la vie du process (jamais
//! désenregistré — inutile pour un agent).
//!
//! macOS/Linux : no-op total. `register()` renvoie un signal inerte (jamais
//! déclenché) ; CoreAudio récupère seul au réveil. Aucun code ASIO n'est touché.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// Signal de réveil : compteur cumulé de resumes + `Notify` pour réveiller le
/// superviseur immédiatement (sans attendre son tick). Clonable (Arc partagés).
#[derive(Clone)]
pub struct ResumeSignal {
    count: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl ResumeSignal {
    fn new() -> Self {
        Self {
            count: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Nombre cumulé de réveils système observés depuis le boot. Le superviseur
    /// compare un delta pour savoir si un resume est survenu depuis sa dernière
    /// observation.
    pub fn resume_count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Handle pour `.notified().await` côté superviseur.
    pub fn notify_handle(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Signale un réveil (incrément + notification). Appelé depuis le callback
    /// système Windows (thread arbitraire) — donc strictement non bloquant.
    fn signal(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_one();
    }
}

impl Default for ResumeSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Enregistre l'écoute des réveils système et renvoie le `ResumeSignal`.
///
/// Sur Windows : branche un callback d'alimentation (sans fenêtre) qui signale au
/// réveil. Ailleurs : renvoie un signal inerte. **Idempotent** — le premier appel
/// enregistre le handler système, les suivants renvoient le même signal (appeler
/// une fois au démarrage du superviseur suffit).
#[cfg(windows)]
pub fn register() -> ResumeSignal {
    win::register()
}

/// Hors Windows : pas de veille ASIO à gérer → signal inerte.
#[cfg(not(windows))]
pub fn register() -> ResumeSignal {
    ResumeSignal::new()
}

#[cfg(windows)]
mod win {
    use super::ResumeSignal;
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Power::PowerRegisterSuspendResumeNotification;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    };

    /// Signal global : le callback système (contexte C, sans état Rust propre) le
    /// lit ici. Posé une seule fois par `register`.
    static RESUME: OnceLock<ResumeSignal> = OnceLock::new();

    /// ABI stable de `_DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS` (Windows 8+). Non
    /// ré-exporté par `windows-sys` 0.59 → on le déclare (struct C figée).
    #[repr(C)]
    struct DeviceNotifySubscribeParameters {
        callback: DeviceNotifyCallbackRoutine,
        context: *mut c_void,
    }

    /// `PDEVICE_NOTIFY_CALLBACK_ROUTINE` : `ULONG cb(PVOID ctx, ULONG type, PVOID setting)`.
    type DeviceNotifyCallbackRoutine =
        Option<unsafe extern "system" fn(context: *mut c_void, event_type: u32, setting: *mut c_void) -> u32>;

    /// Callback système (thread arbitraire, hors runtime tokio). STRICTEMENT
    /// minimal : un atomique + un `notify_one` (tous deux appelables depuis
    /// n'importe quel thread). Le vrai reset ASIO se fait sur le superviseur tokio.
    unsafe extern "system" fn on_power_event(
        _ctx: *mut c_void,
        event_type: u32,
        _setting: *mut c_void,
    ) -> u32 {
        // `PBT_APMRESUMEAUTOMATIC` : réveil (avec ou sans utilisateur présent).
        // `PBT_APMRESUMESUSPEND` : réveil consécutif à une action utilisateur. On
        // couvre les deux — dans les deux cas le driver a pu perdre son état.
        if event_type == PBT_APMRESUMEAUTOMATIC || event_type == PBT_APMRESUMESUSPEND {
            if let Some(sig) = RESUME.get() {
                sig.signal();
            }
        }
        0 // NO_ERROR
    }

    pub(super) fn register() -> ResumeSignal {
        // Déjà enregistré ? (idempotence — le superviseur peut être relancé.)
        if let Some(sig) = RESUME.get() {
            return sig.clone();
        }
        let sig = ResumeSignal::new();
        if RESUME.set(sig.clone()).is_err() {
            // Course perdue : un autre appel a posé le signal entre-temps.
            return RESUME.get().expect("RESUME posé").clone();
        }

        // `params` doit survivre à l'enregistrement système : on le fuit
        // volontairement (durée de vie = process, jamais désenregistré).
        let params = Box::leak(Box::new(DeviceNotifySubscribeParameters {
            callback: Some(on_power_event),
            context: std::ptr::null_mut(),
        }));
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY : `params` (fui) vit pour toujours ; avec `DEVICE_NOTIFY_CALLBACK`,
        // `recipient` doit pointer sur un `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS` —
        // c'est le cas. `handle` est un out-param valide.
        let rc = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                params as *mut DeviceNotifySubscribeParameters as HANDLE,
                &mut handle,
            )
        };
        if rc == 0 {
            tracing::info!(
                target: "jamodio::power",
                "écoute des réveils de veille Windows activée (re-init ASIO au resume)"
            );
        } else {
            tracing::warn!(
                target: "jamodio::power",
                win32_error = rc,
                "PowerRegisterSuspendResumeNotification a échoué — pas de re-init auto au réveil"
            );
        }
        sig
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_signal_counts_and_defaults_zero() {
        let sig = ResumeSignal::new();
        assert_eq!(sig.resume_count(), 0, "aucun réveil au départ");
        sig.signal();
        sig.signal();
        assert_eq!(sig.resume_count(), 2, "deux réveils comptés");
        // Le handle de notification est clonable et indépendant du compteur.
        let _ = sig.notify_handle();
    }

    #[cfg(not(windows))]
    #[test]
    fn register_is_inert_off_windows() {
        // Hors Windows : jamais déclenché, compteur reste à zéro.
        let sig = register();
        assert_eq!(sig.resume_count(), 0);
    }
}
