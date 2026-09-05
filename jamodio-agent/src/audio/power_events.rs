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
//! # VEILLE MODERNE (S0ix) — le trou qui laissait passer les craquements
//!
//! `PBT_APMRESUMEAUTOMATIC` / `PBT_APMRESUMESUSPEND` couvrent la veille S3. Ils ne
//! sont **PAS** délivrés quand Windows entre puis sort de la **veille moderne**
//! (Modern Standby / S0ix ; journal `Kernel-Power` 506/507, motif « Idle Timeout »)
//! : le système continue de tourner en basse consommation, donc rien ne ressemble à
//! un suspend/resume — pas même une discontinuité dans les timestamps du log.
//!
//! Mesuré le 05/09 sur ce chemin : trois transitions de veille moderne
//! (14:16:29→14:17:47, 14:46:15→14:46:58, 17:07:42→17:07:49 UTC) et trois rafales
//! de craquements qui épousent EXACTEMENT ces fenêtres — l'une se terminant à la
//! seconde même de la sortie de veille — avec **aucun** resume signalé. Le driver
//! ASIO ressortait dégradé et rien ne le ré-initialisait : le son restait haché
//! jusqu'à un redémarrage manuel de l'agent.
//!
//! D'où la seconde écoute : `PowerSettingRegisterNotification` sur
//! `GUID_CONSOLE_DISPLAY_STATE`, qui, LUI, se déclenche sur ces transitions. Le
//! passage écran **éteint → allumé** signale le même `ResumeSignal` ; tout le
//! chemin de re-init long-settle en aval est inchangé et déjà éprouvé.
//!
//! Deux précautions dans le filtrage :
//!   - Windows délivre l'état COURANT dès l'enregistrement — ce premier événement
//!     ne doit pas être pris pour un réveil (d'où l'état initial `UNKNOWN`) ;
//!   - seul le passage depuis `OFF` compte. `DIMMED` (l'écran qui se tamise) laisse
//!     le contrôleur audio intact : le traiter comme un réveil déclencherait des
//!     re-init parasites — donc des coupures — en pleine session.
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
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))] // appelé uniquement côté power events (Windows)
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

/// Valeurs de `GUID_CONSOLE_DISPLAY_STATE` (ABI Windows, figée). Déclarées ici —
/// hors du module `win` — pour que la décision de réveil reste testable sur toutes
/// les plateformes : c'est de la logique pure, pas de l'appel système.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const DISPLAY_OFF: u32 = 0;
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const DISPLAY_ON: u32 = 1;
/// L'écran se tamise. Le contrôleur audio n'est PAS endormi pour autant.
/// Jamais testé explicitement par `is_display_wake` (qui n'accepte que `OFF → ON`)
/// : la constante existe pour documenter la 3ᵉ valeur de l'ABI et pour que les
/// tests puissent exprimer ce rejet — d'où le `allow(dead_code)`.
#[allow(dead_code)]
pub(crate) const DISPLAY_DIMMED: u32 = 2;
/// Sentinelle : aucun état reçu encore.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const DISPLAY_UNKNOWN: u32 = u32::MAX;

/// Un changement d'état d'écran constitue-t-il un RÉVEIL à traiter ?
///
/// Uniquement `OFF → ON`. Les deux exclusions sont délibérées :
///   - `UNKNOWN → *` : le tout premier événement est l'état COURANT, livré par
///     Windows à l'enregistrement. Le compter déclencherait un re-init du driver
///     à chaque démarrage de l'agent ;
///   - `DIMMED → ON` : l'écran s'est seulement tamisé, rien ne s'est endormi côté
///     audio. Le compter provoquerait des coupures en pleine session.
// Consommé par le seul chemin Windows ; la logique reste multi-plateforme pour
// rester testable partout — `allow` CIBLÉ, jamais posé sur le module entier.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_display_wake(prev: u32, new: u32) -> bool {
    prev == DISPLAY_OFF && new == DISPLAY_ON
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Power::{
        PowerRegisterSuspendResumeNotification, PowerSettingRegisterNotification,
        POWERBROADCAST_SETTING,
    };
    use windows_sys::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
        PBT_POWERSETTINGCHANGE,
    };

    /// Signal global : le callback système (contexte C, sans état Rust propre) le
    /// lit ici. Posé une seule fois par `register`.
    static RESUME: OnceLock<ResumeSignal> = OnceLock::new();

    use super::{is_display_wake, DISPLAY_UNKNOWN};

    /// Dernier état d'écran observé. Seul `OFF → ON` signale un réveil.
    static DISPLAY_STATE: AtomicU32 = AtomicU32::new(DISPLAY_UNKNOWN);

    /// `windows_sys::core::GUID` n'implémente ni `PartialEq` ni `Eq` — comparaison
    /// champ à champ (struct C figée, 4 champs).
    fn guid_eq(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
        a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
    }

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
            return 0;
        }

        // Veille MODERNE (S0ix) : aucun PBT_APMRESUME* n'est délivré, l'état de
        // l'écran est le seul signal. `setting` pointe un POWERBROADCAST_SETTING
        // dont `Data` porte l'état sur 4 octets (DWORD).
        if event_type == PBT_POWERSETTINGCHANGE && !_setting.is_null() {
            // SAFETY : pour PBT_POWERSETTINGCHANGE, Windows garantit que
            // `setting` pointe un POWERBROADCAST_SETTING valide le temps de
            // l'appel. On ne lit `Data` qu'après avoir vérifié le GUID ET une
            // longueur suffisante — jamais sur la foi du seul type d'événement.
            let hdr = unsafe { &*(_setting as *const POWERBROADCAST_SETTING) };
            if guid_eq(&hdr.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE)
                && hdr.DataLength as usize >= std::mem::size_of::<u32>()
            {
                let state = unsafe { (hdr.Data.as_ptr() as *const u32).read_unaligned() };
                let prev = DISPLAY_STATE.swap(state, Ordering::Relaxed);
                if is_display_wake(prev, state) {
                    if let Some(sig) = RESUME.get() {
                        sig.signal();
                    }
                }
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

        // 2ᵉ écoute : état de l'écran → couvre la VEILLE MODERNE (S0ix), que le
        // suspend/resume ci-dessus ne signale PAS (cf. doc du module). Mêmes
        // `params` : un seul callback, aiguillé sur le type d'événement.
        let mut display_handle: *mut c_void = std::ptr::null_mut();
        // SAFETY : `params` est fui (vit pour toujours) et reste un
        // DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS valide, comme exigé par
        // DEVICE_NOTIFY_CALLBACK ; `display_handle` est un out-param valide.
        let rc_disp = unsafe {
            PowerSettingRegisterNotification(
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_CALLBACK,
                params as *mut DeviceNotifySubscribeParameters as HANDLE,
                &mut display_handle,
            )
        };
        if rc_disp == 0 {
            tracing::info!(
                target: "jamodio::power",
                "écoute de l'état écran activée (re-init ASIO en sortie de VEILLE MODERNE S0ix)"
            );
        } else {
            tracing::warn!(
                target: "jamodio::power",
                win32_error = rc_disp,
                "PowerSettingRegisterNotification(CONSOLE_DISPLAY_STATE) a échoué — \
                 les sorties de veille moderne ne déclencheront pas de re-init"
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

    /// Le cas qui MANQUAIT : la veille moderne (S0ix) ne délivre aucun
    /// `PBT_APMRESUME*`, seul l'écran qui se rallume la signale. Mesuré le 05/09 :
    /// 3 transitions de veille moderne, 3 rafales de craquements, 0 resume signalé.
    #[test]
    fn sortie_de_veille_moderne_ecran_eteint_puis_allume_est_un_reveil() {
        assert!(is_display_wake(DISPLAY_OFF, DISPLAY_ON));
    }

    #[test]
    fn le_premier_evenement_a_l_enregistrement_n_est_pas_un_reveil() {
        // Windows livre l'état COURANT dès l'enregistrement : le compter
        // déclencherait un re-init du driver à chaque démarrage de l'agent.
        assert!(!is_display_wake(DISPLAY_UNKNOWN, DISPLAY_ON));
        assert!(!is_display_wake(DISPLAY_UNKNOWN, DISPLAY_OFF));
    }

    #[test]
    fn l_ecran_qui_se_tamise_n_est_pas_un_reveil() {
        // DIMMED n'endort pas le contrôleur audio : un re-init ici couperait le
        // son en pleine session pour rien.
        assert!(!is_display_wake(DISPLAY_DIMMED, DISPLAY_ON));
        assert!(!is_display_wake(DISPLAY_ON, DISPLAY_DIMMED));
        assert!(!is_display_wake(DISPLAY_OFF, DISPLAY_DIMMED));
    }

    #[test]
    fn extinction_et_etats_stables_ne_sont_pas_des_reveils() {
        assert!(!is_display_wake(DISPLAY_ON, DISPLAY_OFF), "extinction");
        assert!(!is_display_wake(DISPLAY_ON, DISPLAY_ON), "pas de transition");
        assert!(!is_display_wake(DISPLAY_OFF, DISPLAY_OFF), "pas de transition");
    }

    #[cfg(not(windows))]
    #[test]
    fn register_is_inert_off_windows() {
        // Hors Windows : jamais déclenché, compteur reste à zéro.
        let sig = register();
        assert_eq!(sig.resume_count(), 0);
    }
}
