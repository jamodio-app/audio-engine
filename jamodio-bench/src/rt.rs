//! Priorité des fils du banc.
//!
//! Le banc mesure des retards de l'ordre de la milliseconde ; il doit donc être
//! lui-même PLUS précis que ce qu'il mesure. Premier essai réel (Mac de Ben,
//! 28/09/2026) : fils en priorité normale, retard d'envoi du faux serveur de
//! 2 à 8 ms presque chaque seconde, pic à 26,5 ms — et les trous relevés
//! suivaient ces pics. C'était le banc qui faisait la gigue.
//!
//! Même recette que le fil de décodage de l'agent (`audio/rt_priority.rs`) :
//! - macOS : QoS USER_INTERACTIVE + politique « à contrainte de temps »
//!   (bande temps réel du système, réveils précis, pas de regroupement) ;
//! - Windows : MMCSS « Pro Audio ».
//!
//! Un échec est DIT (il figure dans le résumé) : on ne publie pas des mesures en
//! laissant croire que le banc était précis.
//!
//! Option `--no-mmcss` (Lot W1, PLAN-FREINAGE-RESEAU-WINDOWS-2026-09) : sous
//! Windows, priorité de fil `THREAD_PRIORITY_TIME_CRITICAL` au lieu de MMCSS, pour
//! mesurer le freinage réseau de Windows sans qu'aucun fil MMCSS du banc ne le
//! déclenche. Le résumé note aussi le réglage du registre en vigueur
//! ([`multimedia_profile`]) : une campagne ne se compare qu'à réglage connu.

use std::sync::atomic::{AtomicBool, Ordering};

/// Choisi une fois par campagne, AVANT le lancement des fils du banc.
static WITHOUT_MMCSS: AtomicBool = AtomicBool::new(false);

/// `--no-mmcss` : les PROCHAINES promotions se font sans MMCSS (Windows).
pub fn set_without_mmcss(on: bool) {
    WITHOUT_MMCSS.store(on, Ordering::Relaxed);
}

/// Promeut le fil courant. Rend ce qui a été obtenu, en clair.
pub fn promote_current_thread() -> Result<&'static str, String> {
    imp::promote(WITHOUT_MMCSS.load(Ordering::Relaxed))
}

/// Le réglage du freinage réseau de Windows tel que le registre le donne
/// (`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile`),
/// en une ligne pour l'en-tête du résumé. Une valeur illisible est dite, jamais
/// supposée.
pub fn multimedia_profile() -> String {
    #[cfg(windows)]
    {
        describe_profile(registry::read("NetworkThrottlingIndex"), registry::read("SystemResponsiveness"))
    }
    #[cfg(not(windows))]
    {
        "sans objet (pas Windows)".into()
    }
}

/// Mise en mots d'une lecture du registre : `Ok(None)` = valeur absente.
/// (Hors Windows, seuls les tests l'appellent.)
#[cfg_attr(not(windows), allow(dead_code))]
fn describe_value(v: &Result<Option<u32>, String>) -> String {
    match v {
        Ok(Some(u32::MAX)) => "ffffffff (désactivé)".into(),
        Ok(Some(n)) => n.to_string(),
        Ok(None) => "absente".into(),
        Err(e) => format!("illisible ({e})"),
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn describe_profile(throttling: Result<Option<u32>, String>, responsiveness: Result<Option<u32>, String>) -> String {
    format!(
        "NetworkThrottlingIndex = {} ; SystemResponsiveness = {}",
        describe_value(&throttling),
        describe_value(&responsiveness)
    )
}

#[cfg(windows)]
mod registry {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    const PROFILE: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile";

    /// Lecture seule, sans droits administrateur.
    pub fn read(name: &str) -> Result<Option<u32>, String> {
        let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(PROFILE) {
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        match key.get_value::<u32, _>(name) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    /// `QOS_CLASS_USER_INTERACTIVE` (`<sys/qos.h>`).
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

    pub fn promote(_without_mmcss: bool) -> Result<&'static str, String> {
        // SAFETY : appel système sans pointeur.
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
        use mach2::thread_policy::{
            thread_policy_set, thread_time_constraint_policy,
            THREAD_TIME_CONSTRAINT_POLICY, THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        };
        let mut tb = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY : pointeur sur une structure de la pile.
        if unsafe { mach2::mach_time::mach_timebase_info(&mut tb) } != 0 || tb.numer == 0 {
            return Err("mach_timebase_info indisponible".into());
        }
        let ticks = |ns: f64| (ns * f64::from(tb.denom) / f64::from(tb.numer)) as u32;
        // Période d'une trame (2,5 ms), peu de calcul réservé (le banc ne fait
        // qu'envoyer), contrainte d'une période : on demande la bande temps réel
        // pour la PRÉCISION du réveil, pas du temps de calcul.
        let mut policy = thread_time_constraint_policy {
            period: ticks(2_500_000.0),
            computation: ticks(300_000.0),
            constraint: ticks(2_500_000.0),
            preemptible: 1,
        };
        // SAFETY : `policy` vit pendant l'appel ; le compte est celui de la structure.
        let st = unsafe {
            thread_policy_set(
                mach2::mach_init::mach_thread_self(),
                THREAD_TIME_CONSTRAINT_POLICY,
                &mut policy as *mut thread_time_constraint_policy as *mut mach2::vm_types::integer_t,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            )
        };
        if st != 0 {
            return Err(format!("thread_policy_set a refusé ({st})"));
        }
        Ok("temps réel (macOS, contrainte de temps)")
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };

    pub fn promote(without_mmcss: bool) -> Result<&'static str, String> {
        if without_mmcss {
            // SAFETY : pseudo-handle du fil courant ; valeur d'enum documentée.
            // Le fil garde cette priorité jusqu'à sa fin.
            if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } == 0 {
                return Err(format!("TIME_CRITICAL refusé : {}", std::io::Error::last_os_error()));
            }
            return Ok("TIME_CRITICAL, SANS MMCSS (Windows, --no-mmcss)");
        }
        let name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut index = 0u32;
        // SAFETY : chaîne UTF-16 terminée, index sur la pile. Le handle n'est pas
        // rendu : le fil garde sa priorité jusqu'à sa fin.
        let h = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut index) };
        if h.is_null() {
            return Err(format!("MMCSS refusé : {}", std::io::Error::last_os_error()));
        }
        Ok("MMCSS « Pro Audio » (Windows)")
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn promote(_without_mmcss: bool) -> Result<&'static str, String> {
        Err("pas de promotion sur ce système".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_registre_est_dit_tel_quel_jamais_suppose() {
        assert_eq!(
            describe_profile(Ok(Some(10)), Ok(Some(20))),
            "NetworkThrottlingIndex = 10 ; SystemResponsiveness = 20"
        );
        assert_eq!(
            describe_profile(Ok(Some(u32::MAX)), Ok(None)),
            "NetworkThrottlingIndex = ffffffff (désactivé) ; SystemResponsiveness = absente"
        );
        assert_eq!(
            describe_profile(Err("accès refusé".into()), Ok(Some(0))),
            "NetworkThrottlingIndex = illisible (accès refusé) ; SystemResponsiveness = 0"
        );
    }

    /// Sur la CI Windows, la vraie clé se lit sans droits administrateur.
    #[test]
    fn le_profil_de_la_machine_se_lit() {
        let line = multimedia_profile();
        if cfg!(windows) {
            assert!(line.starts_with("NetworkThrottlingIndex = "), "{line}");
            assert!(!line.contains("illisible"), "{line}");
        } else {
            assert_eq!(line, "sans objet (pas Windows)");
        }
    }
}
