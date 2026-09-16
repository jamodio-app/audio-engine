//! Santé de la MACHINE, relevée hors du thread temps réel (tâche perf-stats, 1 Hz).
//!
//! Répond à « ma machine suit-elle ? » avec ce que le système et le moteur audio
//! constatent réellement, sans rien ajouter dans un callback :
//!   - **callbacks manquants** : callbacks audio servis comparés à ceux qu'exige le
//!     temps écoulé (48 kHz ÷ taille de bloc). Chaque callback manquant est un bloc
//!     de son perdu, que le pilote soit en ASIO ou en CoreAudio ;
//!   - **pression mémoire** déclarée par macOS (celle du Moniteur d'activité) ;
//!   - **mémoire utilisée** déclarée par Windows ;
//!   - **charge CPU** du système sur la fenêtre.
//!
//! Chiffres BRUTS : leur classement en défaut (seuils calibrés au banc) vit côté web
//! (`peer-net-quality.js`). Dépôt web : `internal-docs/plans/PLAN-INFOBULLE-LATENCE-2026-09.md`.

use jamodio_audio_core::protocol::MemoryPressure;

/// Un relevé système.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MachineSample {
    pub cpu_pct: Option<f32>,
    pub memory_pressure: Option<MemoryPressure>,
    pub memory_load_pct: Option<f32>,
}

/// Compteurs CPU cumulés du système (unités de l'OS : ticks ou 100 ns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

/// Relève la santé machine ; garde les compteurs CPU du relevé précédent pour
/// calculer une charge sur la fenêtre (pas depuis le démarrage).
#[derive(Debug, Default)]
pub struct MachineSampler {
    prev_cpu: Option<CpuTimes>,
}

impl MachineSampler {
    pub fn sample(&mut self) -> MachineSample {
        let now = platform::cpu_times();
        let cpu_pct = match (self.prev_cpu, now) {
            (Some(prev), Some(now)) => cpu_busy_pct(prev, now),
            _ => None,
        };
        self.prev_cpu = now;
        MachineSample {
            cpu_pct,
            memory_pressure: platform::memory_pressure(),
            memory_load_pct: platform::memory_load_pct(),
        }
    }
}

/// Callbacks manquants par seconde : attendus sur `elapsed_secs` (`sample_rate` ÷
/// `frames_per_callback`) moins réellement servis, jamais négatif. `None` tant que la
/// taille de bloc n'est pas mesurée ou si la fenêtre est vide.
pub fn callback_deficit_per_sec(
    elapsed_secs: f64,
    callbacks: u64,
    frames_per_callback: u32,
    sample_rate: u32,
) -> Option<f32> {
    if frames_per_callback == 0 || !(elapsed_secs.is_finite() && elapsed_secs > 0.0) {
        return None;
    }
    let expected = elapsed_secs * f64::from(sample_rate) / f64::from(frames_per_callback);
    let missing = (expected - callbacks as f64).max(0.0);
    Some((missing / elapsed_secs) as f32)
}

/// Charge CPU (%) entre deux relevés cumulés ; `None` si les compteurs n'ont pas
/// avancé ou ont reculé (relevé invalide).
fn cpu_busy_pct(prev: CpuTimes, now: CpuTimes) -> Option<f32> {
    let total = now.total.checked_sub(prev.total)?;
    let busy = now.busy.checked_sub(prev.busy)?;
    if total == 0 || busy > total {
        return None;
    }
    Some((busy as f64 * 100.0 / total as f64) as f32)
}

/// Niveau `kern.memorystatus_vm_pressure_level` de macOS → pression mémoire.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pressure_from_macos_level(level: i32) -> Option<MemoryPressure> {
    match level {
        1 => Some(MemoryPressure::Normal),
        2 => Some(MemoryPressure::Warning),
        4 => Some(MemoryPressure::Critical),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{pressure_from_macos_level, CpuTimes};
    use jamodio_audio_core::protocol::MemoryPressure;
    use std::sync::OnceLock;

    extern "C" {
        // Déclaré ici plutôt que via `libc` (où il est marqué déprécié) : même
        // symbole de libSystem.
        fn mach_host_self() -> libc::mach_port_t;
    }

    /// Port hôte obtenu UNE fois : chaque appel à `mach_host_self` crée un droit
    /// d'envoi, qu'un relevé par seconde ferait fuir.
    fn host_port() -> libc::mach_port_t {
        static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
        // SAFETY : appel sans argument, renvoie le port hôte du processus.
        *HOST.get_or_init(|| unsafe { mach_host_self() })
    }

    pub(super) fn cpu_times() -> Option<CpuTimes> {
        let mut info = libc::host_cpu_load_info {
            cpu_ticks: [0; libc::CPU_STATE_MAX as usize],
        };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY : `info` est la structure attendue pour HOST_CPU_LOAD_INFO et
        // `count` annonce sa taille en `integer_t`.
        let kr = unsafe {
            libc::host_statistics(
                host_port(),
                libc::HOST_CPU_LOAD_INFO,
                &mut info as *mut _ as libc::host_info_t,
                &mut count,
            )
        };
        if kr != libc::KERN_SUCCESS {
            return None;
        }
        let t = info.cpu_ticks.map(u64::from);
        let idle = t[libc::CPU_STATE_IDLE as usize];
        let total: u64 = t.iter().sum();
        Some(CpuTimes {
            busy: total - idle,
            total,
        })
    }

    pub(super) fn memory_pressure() -> Option<MemoryPressure> {
        let mut level: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY : nom terminé par 0 ; `level` offre exactement `size` octets.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.memorystatus_vm_pressure_level".as_ptr(),
                &mut level as *mut _ as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size != std::mem::size_of::<libc::c_int>() {
            return None;
        }
        pressure_from_macos_level(level)
    }

    /// macOS déclare une PRESSION, pas un pourcentage fiable : pas de chiffre inventé.
    pub(super) fn memory_load_pct() -> Option<f32> {
        None
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::CpuTimes;
    use jamodio_audio_core::protocol::MemoryPressure;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    fn filetime_u64(ft: &FILETIME) -> u64 {
        (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
    }

    pub(super) fn cpu_times() -> Option<CpuTimes> {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut idle, mut kernel, mut user) = (zero, zero, zero);
        // SAFETY : trois FILETIME vivants.
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return None;
        }
        // Le temps noyau INCLUT le temps d'inactivité.
        let total = filetime_u64(&kernel) + filetime_u64(&user);
        let idle = filetime_u64(&idle);
        Some(CpuTimes {
            busy: total.checked_sub(idle)?,
            total,
        })
    }

    /// Windows ne déclare pas de niveau de pression : voir `memory_load_pct`.
    pub(super) fn memory_pressure() -> Option<MemoryPressure> {
        None
    }

    pub(super) fn memory_load_pct() -> Option<f32> {
        // SAFETY : structure initialisée à zéro, `dwLength` renseigné comme exigé.
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY : `status` est une MEMORYSTATUSEX vivante au `dwLength` correct.
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return None;
        }
        Some(status.dwMemoryLoad as f32)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::CpuTimes;
    use jamodio_audio_core::protocol::MemoryPressure;

    pub(super) fn cpu_times() -> Option<CpuTimes> {
        None
    }
    pub(super) fn memory_pressure() -> Option<MemoryPressure> {
        None
    }
    pub(super) fn memory_load_pct() -> Option<f32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aucun_callback_manquant() {
        // 128 trames à 48 kHz = 375 callbacks par seconde.
        assert_eq!(callback_deficit_per_sec(1.0, 375, 128, 48_000), Some(0.0));
        // Plus que prévu (phase de fenêtre) : jamais négatif.
        assert_eq!(callback_deficit_per_sec(1.0, 376, 128, 48_000), Some(0.0));
    }

    #[test]
    fn callbacks_manquants_rapportes_au_temps_ecoule() {
        // 2 s à 64 trames = 1500 attendus, 1470 servis → 30 manquants → 15 par seconde.
        assert_eq!(callback_deficit_per_sec(2.0, 1470, 64, 48_000), Some(15.0));
    }

    #[test]
    fn taille_de_bloc_inconnue_ou_fenetre_vide_rien() {
        assert_eq!(callback_deficit_per_sec(1.0, 0, 0, 48_000), None);
        assert_eq!(callback_deficit_per_sec(0.0, 0, 128, 48_000), None);
        assert_eq!(callback_deficit_per_sec(f64::NAN, 0, 128, 48_000), None);
    }

    #[test]
    fn charge_cpu_sur_la_fenetre() {
        let prev = CpuTimes {
            busy: 100,
            total: 1000,
        };
        let now = CpuTimes {
            busy: 400,
            total: 2000,
        };
        assert_eq!(cpu_busy_pct(prev, now), Some(30.0));
    }

    #[test]
    fn compteurs_cpu_figes_ou_recules_rien() {
        let t = CpuTimes {
            busy: 100,
            total: 1000,
        };
        assert_eq!(cpu_busy_pct(t, t), None);
        assert_eq!(
            cpu_busy_pct(
                t,
                CpuTimes {
                    busy: 50,
                    total: 900
                }
            ),
            None
        );
    }

    #[test]
    fn niveaux_de_pression_macos() {
        assert_eq!(pressure_from_macos_level(1), Some(MemoryPressure::Normal));
        assert_eq!(pressure_from_macos_level(2), Some(MemoryPressure::Warning));
        assert_eq!(pressure_from_macos_level(4), Some(MemoryPressure::Critical));
        assert_eq!(pressure_from_macos_level(3), None);
    }

    /// Relevé réel de la machine (ignoré : dépend de la charge du moment).
    /// `cargo test -p jamodio-agent machine_health -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn releve_de_la_machine() {
        let mut sampler = MachineSampler::default();
        let _ = sampler.sample();
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("{:?}", sampler.sample());
    }
}
