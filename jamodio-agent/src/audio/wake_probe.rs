//! Sonde de réveil — point 1.2a du chantier « plus jamais de trou sec ».
//!
//! # Ce qu'on cherche à savoir, et pourquoi avant d'écrire le masquage
//!
//! Le masquage anticipé (Lot 1.2) repose sur une promesse : « réveille-moi à
//! l'échéance de la prochaine trame » — 2,5 ms plus tard. Si l'OS réveille
//! systématiquement 10 ms trop tard, la promesse ne vaut rien : on déciderait de
//! masquer après coup, c'est-à-dire trop tard pour éviter le trou. Sous Windows,
//! la minuterie système est historiquement grossière (~15,6 ms) tant qu'on ne
//! demande pas explicitement mieux ; sur macOS la précision est connue
//! sub-milliseconde.
//!
//! Cette sonde MESURE le dépassement réel avant qu'on choisisse. Elle compare
//! deux façons d'attendre :
//!   - l'attente simple (`std::thread::sleep`), celle qu'on écrirait par défaut ;
//!   - sous Windows, une minuterie haute résolution
//!     (`CreateWaitableTimerExW` + `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`,
//!     Windows 10 1803+), qui coûte un handle et un peu de code.
//!
//! Si la première suffit, on n'écrira pas la seconde.
//!
//! # Ce qu'elle ne fait pas
//!
//! Elle ne tourne QUE sous interrupteur de banc (`wake-probe = 1`), sur un thread
//! à part, pendant environ deux secondes, à CHAQUE démarrage de capture tant que
//! l'interrupteur est posé (le fichier est relu à chaque capture). Aucune session
//! normale ne l'exécute, et elle ne touche à aucun étage audio. Le thread est promu comme le thread de décodage — c'est sa précision à
//! LUI qui nous intéresse, pas celle d'un thread quelconque.

use std::time::{Duration, Instant};

/// Durée d'une trame Opus : exactement ce que le masquage aura à attendre.
const FRAME: Duration = Duration::from_micros(2_500);

/// ~2 s de mesure. Assez pour un p99 qui veut dire quelque chose, assez court
/// pour ne pas peser sur le démarrage d'un banc.
const ITERATIONS: usize = 800;

/// Dépassement de l'échéance, en microsecondes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Overshoot {
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub samples: usize,
}

/// Résume des dépassements bruts. Fonction pure : c'est elle qu'on teste, la
/// mesure elle-même dépendant de l'OS sous le pied.
pub fn summarize(mut raw: Vec<u64>) -> Overshoot {
    if raw.is_empty() {
        return Overshoot::default();
    }
    raw.sort_unstable();
    let at = |f: f64| -> u64 {
        // Centile « nearest-rank », borné : sur 800 mesures, p99 = la 792e.
        let idx = ((raw.len() as f64) * f).ceil() as usize;
        raw[idx.saturating_sub(1).min(raw.len() - 1)]
    };
    Overshoot {
        p50_us: at(0.50),
        p95_us: at(0.95),
        p99_us: at(0.99),
        max_us: *raw.last().expect("non vide"),
        samples: raw.len(),
    }
}

/// Mesure le dépassement de `ITERATIONS` attentes d'une trame, avec la façon
/// d'attendre fournie. `wait` reçoit la durée RESTANTE jusqu'à l'échéance.
fn measure(mut wait: impl FnMut(Duration)) -> Overshoot {
    let mut raw = Vec::with_capacity(ITERATIONS);
    let mut deadline = Instant::now() + FRAME;
    for _ in 0..ITERATIONS {
        let now = Instant::now();
        if let Some(remaining) = deadline.checked_duration_since(now) {
            wait(remaining);
        }
        // Dépassement = ce qu'on aura de retard pour décider de masquer.
        raw.push(
            Instant::now()
                .saturating_duration_since(deadline)
                .as_micros() as u64,
        );
        // Échéances ancrées sur la précédente (comme le fera le masquage), pas
        // sur « maintenant » : sinon le retard se dilue au lieu de se voir.
        // Conséquence à garder en tête en lisant les centiles : UN réveil en
        // retard de R produit plusieurs échantillons décroissants (R, R − 2,5 ms,
        // R − 5 ms…, les attentes suivantes étant déjà échues), jusqu'à ce que
        // la série rattrape l'échéance. p95/p99 mesurent donc un retard CUMULÉ
        // (combien de temps on reste derrière la grille), pas des réveils isolés.
        deadline += FRAME;
    }
    summarize(raw)
}

/// Lance la sonde si l'interrupteur de banc est armé. Rend la main tout de
/// suite : la mesure vit sur son propre thread.
pub fn run_if_enabled(enabled: bool) {
    if !enabled {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("jamodio-wake-probe".to_string())
        .spawn(|| {
            // Même promotion que le thread de décodage : on mesure SA précision.
            let _rt = super::rt_priority::promote_thread_for_audio_recv();

            let simple = measure(std::thread::sleep);
            tracing::info!(
                target: "jamodio::bench",
                mode = "attente simple",
                frame_us = FRAME.as_micros() as u64,
                p50_us = simple.p50_us,
                p95_us = simple.p95_us,
                p99_us = simple.p99_us,
                max_us = simple.max_us,
                samples = simple.samples,
                "sonde de réveil : dépassement de l'échéance"
            );

            #[cfg(windows)]
            match win_high_res::HighResTimer::new() {
                Some(timer) => {
                    let precise = measure(|d| timer.wait(d));
                    tracing::info!(
                        target: "jamodio::bench",
                        mode = "minuterie haute résolution",
                        frame_us = FRAME.as_micros() as u64,
                        p50_us = precise.p50_us,
                        p95_us = precise.p95_us,
                        p99_us = precise.p99_us,
                        max_us = precise.max_us,
                        samples = precise.samples,
                        "sonde de réveil : dépassement de l'échéance"
                    );
                }
                None => tracing::warn!(
                    target: "jamodio::bench",
                    error = %std::io::Error::last_os_error(),
                    "minuterie haute résolution indisponible — seule l'attente simple est mesurée"
                ),
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(
            target: "jamodio::bench",
            error = %e,
            "sonde de réveil non lancée (thread refusé)"
        );
    }
}

/// Minuterie haute résolution Windows — Windows 10 1803 et au-delà. Mesurée, pas
/// encore utilisée par la pipeline : c'est tout l'objet de 1.2a.
#[cfg(windows)]
mod win_high_res {
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{
        CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
    };

    /// `TIMER_ALL_ACCESS` — absent de `windows-sys`, repris tel quel de
    /// `winnt.h` (`STANDARD_RIGHTS_REQUIRED | SYNCHRONIZE | 0x3`).
    const TIMER_ALL_ACCESS: u32 = 0x001F_0003;

    pub struct HighResTimer(HANDLE);

    // Le handle n'est utilisé que par le thread qui l'a créé (la sonde).
    impl HighResTimer {
        pub fn new() -> Option<Self> {
            // SAFETY : aucun attribut de sécurité, aucun nom (les deux pointeurs
            // nuls sont explicitement acceptés par l'API) ; le handle rendu est
            // possédé par ce type, qui le ferme au drop.
            let handle = unsafe {
                CreateWaitableTimerExW(
                    std::ptr::null(),
                    std::ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                )
            };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                None
            } else {
                Some(Self(handle))
            }
        }

        /// Attend `d`. Une échéance NÉGATIVE est relative, en unités de 100 ns —
        /// c'est la convention de l'API, pas une astuce.
        pub fn wait(&self, d: Duration) {
            let due: i64 = -((d.as_nanos() / 100).min(i64::MAX as u128) as i64);
            // SAFETY : `self.0` est un handle valide tant que `self` vit ;
            // `due` est un i64 local dont l'adresse n'est pas conservée par
            // l'appelé ; aucune routine de complétion n'est fournie.
            unsafe {
                if SetWaitableTimer(self.0, &due, 0, None, std::ptr::null(), 0) != 0 {
                    WaitForSingleObject(self.0, INFINITE);
                }
            }
        }
    }

    impl Drop for HighResTimer {
        fn drop(&mut self) {
            // SAFETY : handle valide, fermé une seule fois (le type n'est ni
            // Clone ni Copy).
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aucune_mesure_ne_donne_aucun_chiffre() {
        // Jamais un zéro qui ressemblerait à « réveil parfait ».
        assert_eq!(summarize(Vec::new()), Overshoot::default());
        assert_eq!(summarize(Vec::new()).samples, 0);
    }

    #[test]
    fn centiles_au_rang_le_plus_proche() {
        let raw: Vec<u64> = (1..=100).collect();
        let o = summarize(raw);
        assert_eq!(o.p50_us, 50);
        assert_eq!(o.p95_us, 95);
        assert_eq!(o.p99_us, 99);
        assert_eq!(o.max_us, 100);
        assert_eq!(o.samples, 100);
    }

    #[test]
    fn une_seule_mesure_reste_lisible() {
        let o = summarize(vec![42]);
        assert_eq!((o.p50_us, o.p95_us, o.p99_us, o.max_us), (42, 42, 42, 42));
    }

    #[test]
    fn la_sonde_ne_tourne_pas_sans_interrupteur() {
        // Le contrat qui garantit qu'aucune session normale ne la rencontre :
        // désarmée, `run_if_enabled` ne crée aucun thread et rend la main.
        let avant = std::time::Instant::now();
        run_if_enabled(false);
        assert!(avant.elapsed() < Duration::from_millis(50));
    }
}
