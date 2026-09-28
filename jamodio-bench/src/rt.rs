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

/// Promeut le fil courant. Rend ce qui a été obtenu, en clair.
pub fn promote_current_thread() -> Result<&'static str, String> {
    imp::promote()
}

#[cfg(target_os = "macos")]
mod imp {
    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    /// `QOS_CLASS_USER_INTERACTIVE` (`<sys/qos.h>`).
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

    pub fn promote() -> Result<&'static str, String> {
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
    use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;

    pub fn promote() -> Result<&'static str, String> {
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
    pub fn promote() -> Result<&'static str, String> {
        Err("pas de promotion sur ce système".into())
    }
}
