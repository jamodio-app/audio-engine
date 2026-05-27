//! Sprint S2 (PLAN-EXECUTION-AGENT-STABILITE.md §S2.3–S2.5) — promotion
//! du thread courant en priorité audio RT, cross-platform.
//!
//! ## Pourquoi
//!
//! Avant S2, `encoder_thread` appelait `thread_priority::Crossplatform(95)`
//! qui sur macOS se traduit en `pthread_setschedparam` avec une nice value
//! que Darwin ignore largement → SCHED_OTHER en pratique → préemptible par
//! tout autre process. Baseline v0.4.1 mesurée 27/05 : spikes 10-25 ms sur
//! le pipeline_max alors que le budget RT est 2.7 ms par bloc.
//!
//! ## Stratégie par OS
//!
//! - **macOS** : tente `os_workgroup_join` (CoreAudio HAL workgroup) via
//!   le binding `jamodio_au_host::workgroup`. Si indisponible (macOS < 11,
//!   device sans workgroup, etc.), fallback `pthread_set_qos_class_self_np`
//!   en `USER_INTERACTIVE` + `thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)`
//!   avec un budget aligné sur la frame Opus (2.5 ms à 48k).
//! - **Windows** : `AvSetMmThreadCharacteristicsW("Pro Audio")` (MMCSS) —
//!   l'API officielle DAW. Pas de fallback nécessaire (dispo Vista+).
//! - **Linux/autres** : conserve `thread_priority::Crossplatform(95)` —
//!   utilisable sur Linux avec `CAP_SYS_NICE`, no-op sinon. Ce n'est pas
//!   une cible production mais le code reste fonctionnel pour les CI tests.
//!
//! ## Contrat de cycle de vie
//!
//! `promote_thread_for_audio(...)` doit être appelé **depuis le thread RT
//! qui doit être promu** (pas depuis le thread d'orchestration). Le handle
//! retourné est `!Sync` — son `Drop` doit s'exécuter sur le même thread.
//! L'usage canonique : binding au début de la closure de `thread::spawn`,
//! drop implicite en fin de boucle.

use std::sync::atomic::{AtomicBool, Ordering};

/// Détails sur la méthode retenue pour la promotion RT. Loggué via tracing
/// à `info` pour qu'on puisse confirmer dans `agent.log` lequel des chemins
/// (workgroup / qos / mmcss / generic / none) a été pris.
///
/// Sur un OS donné, certains variants ne sont jamais construits (ex. macOS
/// ne construit jamais `WindowsMmcss`/`Generic`). On garde l'enum complet
/// pour pouvoir sérialiser/matcher de façon uniforme côté caller multi-OS.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum PromotionMethod {
    /// macOS — `os_workgroup_join` sur le workgroup HAL du device output.
    /// Méthode optimale : le scheduler macOS connaît la deadline audio.
    MacOsWorkgroup,
    /// macOS — fallback `pthread_set_qos_class_self_np(USER_INTERACTIVE)`
    /// + `thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)`.
    MacOsTimeConstraint,
    /// Windows — `AvSetMmThreadCharacteristicsW("Pro Audio")` (MMCSS).
    WindowsMmcss,
    /// Linux/autres — `thread_priority::Crossplatform` (best-effort,
    /// nécessite `CAP_SYS_NICE` sur Linux). Ne s'applique pas sur macOS.
    Generic,
    /// Aucune promotion possible (toutes les méthodes ont échoué).
    /// Le thread tournera en priorité normale OS → spikes possibles.
    None,
}

impl PromotionMethod {
    /// String stable pour exposer la méthode retenue (futur PerfStats /
    /// UI overload S5). Marqué allow(dead_code) tant qu'aucun consumer
    /// externe n'est branché — l'API est volontairement exposée pour S5.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MacOsWorkgroup => "macos-workgroup",
            Self::MacOsTimeConstraint => "macos-time-constraint",
            Self::WindowsMmcss => "windows-mmcss",
            Self::Generic => "generic",
            Self::None => "none",
        }
    }
}

/// RAII handle de la promotion. Doit drop sur le même thread.
pub struct RtPriorityHandle {
    method: PromotionMethod,
    #[cfg(target_os = "macos")]
    workgroup: Option<jamodio_au_host::workgroup::AudioWorkgroup>,
    #[cfg(target_os = "windows")]
    mmcss_handle: windows_sys::Win32::Foundation::HANDLE,
    // Garde anti-double-drop sur le même thread (paranoïa : on log si
    // quelqu'un crée deux handles dans le même thread, ce qui empilerait
    // les promotions et compliquerait le revert).
    _not_sync: std::marker::PhantomData<*const ()>,
}

// SAFETY : le handle ne contient pas d'état partagé. !Sync est garanti via
// le PhantomData. Send autorisé pour les rares cas où on voudrait passer le
// handle entre threads de construction et threads RT — mais le Drop DOIT
// s'exécuter sur le thread qui a fait promote (contrat documenté).
unsafe impl Send for RtPriorityHandle {}

impl RtPriorityHandle {
    /// Méthode RT effectivement appliquée. Réservé aux tests et à
    /// l'instrumentation S5 (UI badge "thread RT actif via X").
    #[allow(dead_code)]
    pub fn method(&self) -> PromotionMethod {
        self.method
    }
}

// ─── Anti-double-promotion guard ─────────────────────────────────
//
// Si un thread re-appelle `promote_thread_for_audio` sans drop le handle
// précédent, on log warn et on retourne un handle no-op. Pas un vrai
// thread-local (`thread_local!` est lourd), juste un AtomicBool global
// optimiste : le but est de signaler les bugs d'usage, pas d'enforcer.
// Reset à false dans Drop pour permettre une re-promotion légitime
// (ex. relance encoder thread après stop_capture).
static PROMOTION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Promeut le thread courant en priorité audio RT. Best-effort selon l'OS.
///
/// `output_device_name` : nom (ou sous-chaîne) du device output dont le
/// workgroup CoreAudio sera ciblé sur macOS. `None` ⇒ default output.
/// Ignoré sur Windows/Linux (le scheduler audio n'est pas device-spécifique).
///
/// Retourne un handle dont le `Drop` libère best-effort. Doit être drop sur
/// **le même thread** que celui qui a appelé `promote_thread_for_audio`.
pub fn promote_thread_for_audio(output_device_name: Option<&str>) -> RtPriorityHandle {
    if PROMOTION_ACTIVE.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            target: "jamodio::rt_priority",
            "double promotion détectée — le handle précédent n'a pas été drop sur ce thread (ou un autre thread a la promotion globale active). Retour d'un handle no-op."
        );
        return make_none_handle();
    }

    #[cfg(target_os = "macos")]
    {
        // 1er essai : workgroup CoreAudio du device output.
        if jamodio_au_host::workgroup::is_available() {
            let wg = match output_device_name {
                Some(name) => jamodio_au_host::workgroup::AudioWorkgroup::join_by_name(name),
                None => jamodio_au_host::workgroup::AudioWorkgroup::join_default(),
            };
            if let Some(workgroup) = wg {
                tracing::info!(
                    target: "jamodio::rt_priority",
                    method = "macos-workgroup",
                    device = output_device_name.unwrap_or("<default>"),
                    "thread promoted to CoreAudio workgroup"
                );
                return RtPriorityHandle {
                    method: PromotionMethod::MacOsWorkgroup,
                    workgroup: Some(workgroup),
                    _not_sync: std::marker::PhantomData,
                };
            }
        }

        // 2e essai : QoS USER_INTERACTIVE + THREAD_TIME_CONSTRAINT_POLICY.
        match macos_fallback::apply() {
            Ok(()) => {
                tracing::info!(
                    target: "jamodio::rt_priority",
                    method = "macos-time-constraint",
                    "thread promoted via QoS USER_INTERACTIVE + THREAD_TIME_CONSTRAINT_POLICY"
                );
                return RtPriorityHandle {
                    method: PromotionMethod::MacOsTimeConstraint,
                    workgroup: None,
                    _not_sync: std::marker::PhantomData,
                };
            }
            Err(e) => {
                tracing::warn!(
                    target: "jamodio::rt_priority",
                    error = %e,
                    "macos fallback (QoS + time-constraint) failed — running at normal priority"
                );
                return make_none_handle();
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = output_device_name; // ignored on Windows
        match windows_mmcss::apply() {
            Ok(h) => {
                tracing::info!(
                    target: "jamodio::rt_priority",
                    method = "windows-mmcss",
                    task = "Pro Audio",
                    "thread promoted via MMCSS Pro Audio"
                );
                return RtPriorityHandle {
                    method: PromotionMethod::WindowsMmcss,
                    mmcss_handle: h,
                    _not_sync: std::marker::PhantomData,
                };
            }
            Err(e) => {
                tracing::warn!(
                    target: "jamodio::rt_priority",
                    error = %e,
                    "windows MMCSS failed — running at normal priority"
                );
                return make_none_handle();
            }
        }
    }

    // Linux / autres : best-effort thread-priority (=existing behavior).
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = output_device_name;
        let prio = thread_priority::ThreadPriority::Crossplatform(
            95u8.try_into().expect("0..=100"),
        );
        match thread_priority::set_current_thread_priority(prio) {
            Ok(()) => {
                tracing::info!(
                    target: "jamodio::rt_priority",
                    method = "generic",
                    "thread promoted via thread-priority crossplatform"
                );
                RtPriorityHandle {
                    method: PromotionMethod::Generic,
                    _not_sync: std::marker::PhantomData,
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "jamodio::rt_priority",
                    error = ?e,
                    "thread-priority refused — running at normal priority"
                );
                make_none_handle()
            }
        }
    }
}

fn make_none_handle() -> RtPriorityHandle {
    RtPriorityHandle {
        method: PromotionMethod::None,
        #[cfg(target_os = "macos")]
        workgroup: None,
        #[cfg(target_os = "windows")]
        mmcss_handle: 0 as windows_sys::Win32::Foundation::HANDLE,
        _not_sync: std::marker::PhantomData,
    }
}

impl Drop for RtPriorityHandle {
    fn drop(&mut self) {
        match self.method {
            PromotionMethod::MacOsWorkgroup => {
                // Le workgroup AudioWorkgroup a son propre Drop qui appelle
                // os_workgroup_leave. Rien à faire ici.
                #[cfg(target_os = "macos")]
                {
                    self.workgroup.take(); // explicit drop = leave
                }
            }
            PromotionMethod::MacOsTimeConstraint => {
                // Time-constraint sticky au thread — au shutdown du thread,
                // l'OS nettoie automatiquement. On pourrait re-apply STANDARD
                // policy mais ce thread va mourir immédiatement après → inutile.
            }
            PromotionMethod::WindowsMmcss => {
                #[cfg(target_os = "windows")]
                {
                    if self.mmcss_handle != 0 as windows_sys::Win32::Foundation::HANDLE {
                        // SAFETY : handle obtenu via AvSetMmThreadCharacteristicsW
                        // sur ce même thread, libéré ici depuis ce même thread.
                        unsafe {
                            windows_sys::Win32::System::Threading::AvRevertMmThreadCharacteristics(
                                self.mmcss_handle,
                            );
                        }
                    }
                }
            }
            PromotionMethod::Generic => {
                // thread-priority n'expose pas de revert. Le thread va mourir
                // après cette boucle → no-op.
            }
            PromotionMethod::None => {}
        }
        PROMOTION_ACTIVE.store(false, Ordering::SeqCst);
    }
}

// ─── macOS fallback : QoS + THREAD_TIME_CONSTRAINT_POLICY ────────
//
// QoS USER_INTERACTIVE annonce au scheduler "ce thread fait du travail user-
// facing prioritaire". À lui seul, c'est mieux que SCHED_OTHER nice value
// (que Darwin ignore). Combiné avec THREAD_TIME_CONSTRAINT_POLICY qui dit
// explicitement "je dois exécuter X cycles toutes les Y cycles avec une
// deadline Z", on obtient un scheduling déterministe digne de CoreAudio.
//
// Budget choisi : aligné sur la frame Opus 120 samples / 48 kHz = 2.5 ms.
// - period      = 2 500 000 ns
// - computation = 1 200 000 ns (= temps max qu'on garantit utiliser ≈ 50% budget)
// - constraint  = 2 000 000 ns (= deadline max entre start et fin ≈ 80% budget)
// - preemptible = true (on accepte la préemption si on dépasse — meilleur que
//   risque deadlock système si jamais on a un bug qui fait spin l'encoder)

#[cfg(target_os = "macos")]
mod macos_fallback {
    use std::io;

    // Constantes mach (extraites de <mach/thread_policy.h>).
    const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;
    const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;

    #[repr(C)]
    struct ThreadTimeConstraintPolicy {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: u32, // boolean_t
    }

    extern "C" {
        fn pthread_set_qos_class_self_np(
            qos_class: u32,
            relative_priority: i32,
        ) -> i32;
        fn thread_policy_set(
            thread: u32,
            flavor: u32,
            policy_info: *const std::ffi::c_void,
            policy_info_count: u32,
        ) -> i32;
        fn pthread_mach_thread_np(
            thread: libc::pthread_t,
        ) -> u32;
    }

    /// `qos_class_t` value pour USER_INTERACTIVE — la classe la plus haute
    /// pour le travail user-facing (cf. `<sys/qos.h>`).
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

    pub fn apply() -> io::Result<()> {
        // 1. QoS class — Darwin scheduler hint.
        // SAFETY : appel libc, pas de pointeur invalide possible.
        let qos_status = unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0)
        };
        if qos_status != 0 {
            // Pas fatal — on continue pour appliquer time-constraint qui
            // est l'élément le plus efficace.
            tracing::debug!(
                target: "jamodio::rt_priority",
                status = qos_status,
                "pthread_set_qos_class_self_np non-zero (continuing with time-constraint)"
            );
        }

        // 2. Time-constraint policy — la vraie garantie de scheduling.
        // Conversion ns → mach absolute time ticks via mach_timebase_info.
        let nanos_to_ticks = mach_ticks_per_nanosecond();
        let policy = ThreadTimeConstraintPolicy {
            period:      (2_500_000.0 * nanos_to_ticks) as u32, // 2.5 ms
            computation: (1_200_000.0 * nanos_to_ticks) as u32, // 1.2 ms
            constraint:  (2_000_000.0 * nanos_to_ticks) as u32, // 2.0 ms
            preemptible: 1, // true — éviter deadlock système en cas de bug
        };

        let mach_thread = unsafe { pthread_mach_thread_np(libc::pthread_self()) };
        // SAFETY : `policy` est valide pour la durée de l'appel ; count
        // correspond exactement à la taille de la struct (4 × u32).
        let st = unsafe {
            thread_policy_set(
                mach_thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                &policy as *const _ as *const std::ffi::c_void,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            )
        };
        if st != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("thread_policy_set returned {}", st),
            ));
        }
        Ok(())
    }

    /// Retourne le facteur multiplicatif `ticks = ns × X` pour le scheduler
    /// Mach. Sur Apple Silicon ce ratio est typiquement 0.024 (= 41,67 ns/tick).
    /// Mémoïsé en static via OnceLock.
    fn mach_ticks_per_nanosecond() -> f64 {
        use std::sync::OnceLock;
        static CACHE: OnceLock<f64> = OnceLock::new();
        *CACHE.get_or_init(|| {
            let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
            // SAFETY : appel Mach kernel sans pointeur invalide.
            let st = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
            if st != 0 || info.numer == 0 {
                return 1.0; // fallback identité (= ticks ≈ ns sur les Mach modernes)
            }
            info.denom as f64 / info.numer as f64
        })
    }
}

// ─── Windows : MMCSS Pro Audio ─────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_mmcss {
    use std::io;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;

    /// UTF-16 NUL-terminated literal pour "Pro Audio" (sans dépendre du macro
    /// `w!` qui n'est pas exposé par windows-sys).
    const PRO_AUDIO_W: &[u16] = &[
        b'P' as u16, b'r' as u16, b'o' as u16, b' ' as u16,
        b'A' as u16, b'u' as u16, b'd' as u16, b'i' as u16, b'o' as u16,
        0,
    ];

    pub fn apply() -> io::Result<HANDLE> {
        let mut task_index: u32 = 0;
        // SAFETY : pointeur sur littéral statique valide pour la durée du
        // process ; out-param task_index borrow exclusif.
        let h = unsafe {
            AvSetMmThreadCharacteristicsW(PRO_AUDIO_W.as_ptr(), &mut task_index)
        };
        // Selon la doc Microsoft, AvSetMmThreadCharacteristicsW retourne NULL
        // (= 0) en cas d'échec et set GetLastError.
        if h == 0 as HANDLE {
            return Err(io::Error::last_os_error());
        }
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `promote_thread_for_audio` ne doit jamais paniquer même si toutes les
    /// méthodes échouent. Le handle no-op doit être drop-safe.
    #[test]
    fn promote_then_drop_is_safe() {
        // Sur CI macOS récent : devrait taper MacOsWorkgroup ou MacOsTimeConstraint.
        // Sur CI Linux : Generic ou None.
        // Sur Windows : WindowsMmcss ou None.
        // Quel que soit le path, drop ne doit pas crash.
        let h = promote_thread_for_audio(None);
        let m = h.method();
        // Sanity : on a au minimum sélectionné UN des paths (None inclus).
        assert!(matches!(
            m,
            PromotionMethod::MacOsWorkgroup
                | PromotionMethod::MacOsTimeConstraint
                | PromotionMethod::WindowsMmcss
                | PromotionMethod::Generic
                | PromotionMethod::None
        ));
        drop(h);
    }

    /// Le guard anti-double-promotion doit se reset après drop pour permettre
    /// une re-promotion légitime (ex : re-spawn encoder après stop_capture).
    #[test]
    fn double_promotion_after_drop_works() {
        let h1 = promote_thread_for_audio(None);
        drop(h1);
        let h2 = promote_thread_for_audio(None);
        drop(h2);
    }

    /// Une 2e promotion SANS drop doit donner un handle no-op (méthode None)
    /// + warning logué — pas de panic.
    #[test]
    fn double_promotion_without_drop_yields_none() {
        let _h1 = promote_thread_for_audio(None);
        let h2 = promote_thread_for_audio(None);
        assert!(matches!(h2.method(), PromotionMethod::None));
        // h1 drop en fin de scope reset le guard.
    }
}
