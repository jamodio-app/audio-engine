//! Pas de veille pendant une session (Lot V du chantier tampon).
//!
//! Un ordinateur qui s'endort pendant qu'on joue est une panne audio : sous
//! Windows, la veille moderne sort le pilote ASIO dégradé (cf.
//! `agent_veille_moderne_s0ix` : craquements persistants au réveil, corrigés en
//! 0.5.13-8 — mais la veille reste à éviter), et sur Mac l'audio s'arrête net.
//!
//! On demande donc au système de ne pas s'endormir **tant qu'une capture
//! tourne**, et on relâche à l'arrêt. La demande est portée par un objet : sa
//! destruction relâche, quel que soit le chemin de sortie (arrêt normal, erreur,
//! fermeture de l'application) — une demande ne peut pas fuir.
//!
//! Ce qu'on ne fait PAS : empêcher l'écran de s'éteindre. C'est l'extinction de
//! l'écran qui déclenche la veille moderne sous Windows ; si le banc BV montre
//! que « système requis » ne suffit pas sur le PC, on en discutera avant
//! d'imposer un écran allumé, qui est un effet de bord visible.

use std::sync::Arc;

/// Ce qu'un système sait faire : tenir une demande, la relâcher. Le vrai
/// système est branché en production ; les tests en branchent un faux pour
/// vérifier le cycle de vie sans toucher à la machine.
pub trait PowerSystem: Send + Sync + 'static {
    /// Demande à rester éveillé. `Err` = le système a refusé (on le dit, on ne
    /// prétend pas que la veille est écartée).
    fn prevent_sleep(&self, reason: &str) -> Result<u64, String>;
    /// Relâche la demande identifiée par `token`.
    fn allow_sleep(&self, token: u64);
}

/// Demande en cours. Relâchée à la destruction.
pub struct KeepAwake {
    system: Arc<dyn PowerSystem>,
    token: Option<u64>,
}

impl KeepAwake {
    /// Prend la demande pour la durée d'une session. Une demande refusée n'est
    /// pas une erreur fatale : la session continue, le journal le dit.
    pub fn acquire(system: Arc<dyn PowerSystem>, reason: &str) -> Self {
        let token = match system.prevent_sleep(reason) {
            Ok(token) => {
                tracing::info!(target: "jamodio::power", reason, "veille empêchée pendant la session");
                Some(token)
            }
            Err(e) => {
                tracing::warn!(
                    target: "jamodio::power",
                    error = %e,
                    "impossible d'empêcher la veille — la session continue, l'ordinateur peut s'endormir"
                );
                None
            }
        };
        Self { system, token }
    }

    /// Prend la demande auprès du système de cette machine.
    pub fn for_session(reason: &str) -> Self {
        Self::acquire(platform_system(), reason)
    }

    /// Vrai tant que la demande est tenue (vérifié par les tests du cycle de vie).
    #[cfg(test)]
    pub fn is_held(&self) -> bool {
        self.token.is_some()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.system.allow_sleep(token);
            tracing::info!(target: "jamodio::power", "veille de nouveau autorisée");
        }
    }
}

/// Système de cette plateforme.
fn platform_system() -> Arc<dyn PowerSystem> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::IoKitPower)
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(windows::PowerRequest::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Arc::new(UnsupportedPower)
    }
}

/// Plateformes sans implémentation (Linux de développement) : on le dit, on ne
/// fait pas semblant.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct UnsupportedPower;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl PowerSystem for UnsupportedPower {
    fn prevent_sleep(&self, _reason: &str) -> Result<u64, String> {
        Err("veille non gérée sur cette plateforme".into())
    }
    fn allow_sleep(&self, _token: u64) {}
}

#[cfg(target_os = "macos")]
mod macos {
    use super::PowerSystem;
    use core_foundation_sys::base::{kCFAllocatorDefault, Boolean, CFRelease};
    use core_foundation_sys::string::{
        kCFStringEncodingUTF8, CFStringCreateWithBytes, CFStringRef,
    };

    /// Niveau d'assertion « tenue » (`kIOPMAssertionLevelOn`).
    const ASSERTION_LEVEL_ON: u32 = 255;

    // L'assertion demande à ne pas s'endormir par INACTIVITÉ ; fermer le capot
    // ou demander la veille reste possible — c'est le choix de l'utilisateur,
    // on ne le lui reprend pas.
    const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: u32,
            assertion_name: CFStringRef,
            assertion_id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    }

    /// CFString à partir d'une chaîne Rust (UTF-8). À relâcher par l'appelant.
    fn cf_string(value: &str) -> CFStringRef {
        // SAFETY : `value` vit le temps de l'appel ; CoreFoundation copie les octets.
        unsafe {
            CFStringCreateWithBytes(
                kCFAllocatorDefault,
                value.as_ptr(),
                value.len() as isize,
                kCFStringEncodingUTF8,
                false as Boolean,
            )
        }
    }

    pub struct IoKitPower;

    impl PowerSystem for IoKitPower {
        fn prevent_sleep(&self, reason: &str) -> Result<u64, String> {
            let kind = cf_string(ASSERTION_TYPE);
            let name = cf_string(reason);
            if kind.is_null() || name.is_null() {
                return Err("CFString non créée".into());
            }
            let mut id: u32 = 0;
            // SAFETY : deux CFString valides le temps de l'appel, `id` écrit par
            // IOKit. Les deux chaînes sont relâchées juste après.
            let status = unsafe { IOPMAssertionCreateWithName(kind, ASSERTION_LEVEL_ON, name, &mut id) };
            unsafe {
                CFRelease(kind as *const std::ffi::c_void);
                CFRelease(name as *const std::ffi::c_void);
            }
            if status == 0 {
                Ok(u64::from(id))
            } else {
                Err(format!("IOPMAssertionCreateWithName a rendu {status}"))
            }
        }

        fn allow_sleep(&self, token: u64) {
            // SAFETY : `token` vient d'une assertion créée ci-dessus.
            let status = unsafe { IOPMAssertionRelease(token as u32) };
            if status != 0 {
                tracing::warn!(target: "jamodio::power", status, "IOPMAssertionRelease a échoué");
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::PowerSystem;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Power::{
        PowerClearRequest, PowerCreateRequest, PowerSetRequest, PowerRequestSystemRequired,
    };
    use windows_sys::Win32::System::Threading::{
        REASON_CONTEXT, REASON_CONTEXT_0, POWER_REQUEST_CONTEXT_SIMPLE_STRING,
    };

    /// Une demande à la fois : la session est unique. Le handle est gardé pour
    /// pouvoir la relâcher et la fermer proprement.
    pub struct PowerRequest {
        handle: Mutex<Option<HandleHolder>>,
    }

    struct HandleHolder(HANDLE);
    // SAFETY : un HANDLE de demande d'alimentation est utilisable depuis
    // n'importe quel thread ; il n'est jamais dupliqué ici.
    unsafe impl Send for HandleHolder {}

    impl PowerRequest {
        pub fn new() -> Self {
            Self { handle: Mutex::new(None) }
        }
    }

    impl Default for PowerRequest {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PowerSystem for PowerRequest {
        fn prevent_sleep(&self, reason: &str) -> Result<u64, String> {
            let mut wide: Vec<u16> = reason.encode_utf16().collect();
            wide.push(0);
            let context = REASON_CONTEXT {
                Version: 0,
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 { SimpleReasonString: wide.as_mut_ptr() },
            };
            // SAFETY : `context` et la chaîne vivent le temps de l'appel.
            let handle = unsafe { PowerCreateRequest(&context) };
            if handle.is_null() {
                return Err("PowerCreateRequest a échoué".into());
            }
            // SAFETY : handle valide, rendu par PowerCreateRequest.
            let set = unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) };
            if set == 0 {
                unsafe { CloseHandle(handle) };
                return Err("PowerSetRequest a échoué".into());
            }
            *self.handle.lock().unwrap() = Some(HandleHolder(handle));
            Ok(handle as usize as u64)
        }

        fn allow_sleep(&self, _token: u64) {
            if let Some(HandleHolder(handle)) = self.handle.lock().unwrap().take() {
                // SAFETY : handle tenu par nous, relâché puis fermé une seule fois.
                unsafe {
                    PowerClearRequest(handle, PowerRequestSystemRequired);
                    CloseHandle(handle);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Faux système : compte les prises et les relâchements.
    struct FakePower {
        held: AtomicUsize,
        takes: AtomicUsize,
        refuse: bool,
    }

    impl FakePower {
        fn new(refuse: bool) -> Arc<Self> {
            Arc::new(Self { held: AtomicUsize::new(0), takes: AtomicUsize::new(0), refuse })
        }
    }

    impl PowerSystem for FakePower {
        fn prevent_sleep(&self, _reason: &str) -> Result<u64, String> {
            if self.refuse {
                return Err("refusé".into());
            }
            self.held.fetch_add(1, Ordering::SeqCst);
            Ok(self.takes.fetch_add(1, Ordering::SeqCst) as u64)
        }
        fn allow_sleep(&self, _token: u64) {
            self.held.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn la_demande_est_relachee_a_la_fin_de_la_session() {
        let system = FakePower::new(false);
        {
            let guard = KeepAwake::acquire(system.clone(), "session");
            assert!(guard.is_held());
            assert_eq!(system.held.load(Ordering::SeqCst), 1);
        }
        assert_eq!(system.held.load(Ordering::SeqCst), 0, "rien ne doit fuir");
    }

    #[test]
    fn deux_sessions_de_suite_ne_laissent_rien_derriere() {
        let system = FakePower::new(false);
        drop(KeepAwake::acquire(system.clone(), "session 1"));
        drop(KeepAwake::acquire(system.clone(), "session 2"));
        assert_eq!(system.held.load(Ordering::SeqCst), 0);
        assert_eq!(system.takes.load(Ordering::SeqCst), 2);
    }

    /// Un système qui refuse ne casse pas la session, et ne relâche rien à la
    /// destruction (il n'y a rien à relâcher).
    #[test]
    fn un_refus_laisse_la_session_continuer() {
        let system = FakePower::new(true);
        let guard = KeepAwake::acquire(system.clone(), "session");
        assert!(!guard.is_held());
        drop(guard);
        assert_eq!(system.held.load(Ordering::SeqCst), 0);
    }
}
