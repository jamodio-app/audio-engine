//! Précision des attentes minutées sous Windows.
//!
//! # Pourquoi
//!
//! Le thread de réception attend « un paquet OU l'échéance du prochain
//! examen » (`recv_timeout`) : c'est l'échéance qui déclenche le masquage d'un
//! paquet en retard. Sous Windows, une attente qui EXPIRE se réveille au tic de
//! la minuterie du PROCESSUS — 15,6 ms par défaut depuis Windows 10 2004, même
//! si le système est réglé plus fin pour un autre programme. Mesuré le
//! 22/09/2026 sur le PC de recette (hors agent, `recv_timeout(1 ms)` sur un
//! canal vide, fenêtre masquée comme l'agent) :
//!
//! | cas                                          | médiane | max     |
//! |----------------------------------------------|---------|---------|
//! | rien (l'agent jusqu'ici)                     | 15,6 ms | 67,6 ms |
//! | `timeBeginPeriod(1)` seul                    | 15,6 ms | 56,0 ms |
//! | thread MMCSS « Pro Audio » seul              | 15,3 ms | 18,5 ms |
//! | MMCSS + les deux réglages de ce module       | 1,85 ms | 4,3 ms  |
//!
//! Un masquage réveillé 15 ms trop tard laisse le tampon se vider : on joue un
//! trou au lieu de le boucher. `timeBeginPeriod(1)` seul ne suffit pas :
//! Windows 11 l'ignore pour un processus dont la fenêtre est masquée, ce qui est
//! le cas de l'agent. Il faut en plus exempter le processus de ce bridage.
//!
//! (La sonde de réveil du 19/09 lisait 0,4 ms parce qu'elle attendait par
//! `thread::sleep`, qui prend une minuterie haute résolution — pas le chemin de
//! `recv_timeout`.)
//!
//! # Ce que fait le module
//!
//! - au démarrage, [`exempt_process_from_timer_throttling`] : l'exemption seule
//!   ne coûte rien, elle rend seulement effective la demande de précision ;
//! - pendant une SESSION seulement, [`SessionTimerResolution`] : minuterie à
//!   1 ms, rendue à la fin. Hors session, l'agent tourne en fond sans rien
//!   demander (une minuterie fine coûte un peu d'énergie).
//!
//! Rien ici ne touche le callback audio ni le chemin du son ; macOS n'en a pas
//! besoin (attentes précises par construction) et n'exécute rien.

/// Exempte le processus du bridage de la minuterie que Windows 11 applique aux
/// processus sans fenêtre visible. Un échec est journalisé : la session tourne,
/// mais les masquages pourront partir en retard, et le journal doit le dire.
pub fn exempt_process_from_timer_throttling() {
    #[cfg(target_os = "windows")]
    windows::exempt_process();
}

/// Minuterie à 1 ms tenue pendant une session, rendue à la destruction — quel
/// que soit le chemin de sortie.
pub struct SessionTimerResolution {
    #[cfg(target_os = "windows")]
    held: bool,
}

impl SessionTimerResolution {
    pub fn acquire() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self { held: windows::begin_period() }
        }
        #[cfg(not(target_os = "windows"))]
        {
            Self {}
        }
    }
}

impl Drop for SessionTimerResolution {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        if self.held {
            windows::end_period();
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod, TIMERR_NOERROR};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE,
    };

    /// Période demandée, en ms.
    const PERIOD_MS: u32 = 1;

    pub(super) fn exempt_process() {
        // `ControlMask` = ce qu'on règle, `StateMask` = 0 : le bridage de la
        // minuterie est DÉSACTIVÉ pour ce processus.
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        // SAFETY : pseudo-handle du processus courant ; `state` vit le temps de
        // l'appel et la taille passée est la sienne.
        let ok = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok != 0 {
            tracing::info!(
                target: "jamodio::power",
                "processus exempté du bridage de la minuterie (fenêtre masquée)"
            );
        } else {
            tracing::warn!(
                target: "jamodio::power",
                win32_error = std::io::Error::last_os_error().raw_os_error(),
                "exemption du bridage de la minuterie refusée — en fenêtre masquée, les masquages pourront partir jusqu'à 15 ms en retard"
            );
        }
    }

    pub(super) fn begin_period() -> bool {
        // SAFETY : appel sans pointeur ; équilibré par `end_period` au Drop.
        let rc = unsafe { timeBeginPeriod(PERIOD_MS) };
        if rc == TIMERR_NOERROR {
            tracing::info!(target: "jamodio::power", period_ms = PERIOD_MS, "minuterie fine demandée pour la session");
            true
        } else {
            tracing::warn!(
                target: "jamodio::power",
                period_ms = PERIOD_MS,
                rc,
                "minuterie fine refusée — les masquages pourront partir jusqu'à 15 ms en retard"
            );
            false
        }
    }

    pub(super) fn end_period() {
        // SAFETY : rend exactement la période obtenue par `begin_period`.
        unsafe { timeEndPeriod(PERIOD_MS) };
        tracing::info!(target: "jamodio::power", "minuterie fine rendue");
    }
}
