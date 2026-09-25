//! Vérification du rate RÉEL d'un pilote à l'ouverture — la décision, hors de tout
//! code plateforme, pour être testée partout (l'hôte ASIO qui l'appelle est
//! `cfg(windows)`).
//!
//! # Pourquoi mesurer
//!
//! Certains pilotes MENTENT : ils acceptent `set_sample_rate(48k)`, déclarent
//! 48 000, et continuent de délivrer à 44,1 (Focusrite natif, prouvé le 04/08 :
//! 689 callbacks/s × 64 frames ≈ 44 100). Le seul signal fiable est la cadence
//! des callbacks : `callbacks × frames / temps`. Si elle contredit le déclaré, la
//! garde R2 (`start_capture`, décision 48k/ASIO-only) refuse la capture — jamais
//! de resampling caché.
//!
//! # Pourquoi cette décision et pas « le mesuré, point »
//!
//! Cas Guillaume H. (UR22C, Yamaha Steinberg USB, 23/09/2026, agent 0.6.5) : le
//! pilote déclare 48 000 et livre bien à 48 000, mais il fait **un trou de
//! ~196 ms** (buffer 32) ou ~72 ms (buffer 48) juste après `ASIOStart`, à chaque
//! ouverture. Sur une fenêtre de 500 ms, la cadence brute tombait à 929 cb/s ×
//! 32 = « 29 731 Hz » — un rate qui n'existe pas, présenté à l'utilisateur comme
//! celui de son interface, et 18 refus d'entrée pour une carte correctement
//! réglée. Deux corrections structurelles :
//!
//! 1. **Le temps de trou est exclu de la mesure.** Le callback compte déjà chaque
//!    intervalle en retard (cf. `callback_health::stall_us_total`) ; la cadence
//!    se calcule sur le temps où le pilote livrait réellement. Un pilote menteur
//!    reste détecté : tous ses intervalles sont lents, aucun n'est un trou.
//! 2. **Une cadence qui ne tombe sur aucun rate standard n'est pas un rate.**
//!    C'est une mesure non concluante : on GARDE le déclaré et on le dit dans le
//!    journal. Le message « ton interface est en X Hz » ne peut plus afficher
//!    qu'un rate standard réellement livré.

// Lus par le seul chemin Windows (hôte ASIO) ; la logique reste multi-plateforme
// pour rester testable partout — `allow` CIBLÉ par item, jamais sur le module.
/// Rates standard que peut délivrer une interface audio, en Hz.
#[cfg_attr(not(windows), allow(dead_code))]
pub const STANDARD_RATES: [u32; 9] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 32_000, 22_050, 11_025,
];

/// Écart relatif au-delà duquel la cadence mesurée contredit le rate déclaré.
/// 44,1 vs 48 = 8,1 % ; la gigue résiduelle d'une mesure sans trou est ≈ 1-2 %.
#[cfg_attr(not(windows), allow(dead_code))]
pub const TOLERANCE: f64 = 0.03;

/// Distance (Hz) sous laquelle une cadence mesurée est rattachée à un rate
/// standard (le mesuré porte ±~1 % de bruit : un 44 096 EST du 44,1).
#[cfg_attr(not(windows), allow(dead_code))]
pub const SNAP_HZ: u32 = 400;

/// Ce que l'hôte a compté pendant la fenêtre d'observation, après le 1er callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RateSample {
    /// Rate déclaré par le pilote (`sample_rate()` après `set_sample_rate`).
    pub declared_sr: u32,
    /// Frames par callback (taille de buffer réellement créée).
    pub frames_per_cb: u32,
    /// Callbacks reçus pendant la fenêtre.
    pub callbacks: u64,
    /// Durée de la fenêtre (µs).
    pub window_us: u64,
    /// Temps passé dans des intervalles en retard pendant la fenêtre (µs), au-delà
    /// d'une période de bloc chacun — cf. `CallbackHealth::stall_us_total`.
    pub stall_us: u64,
}

/// Verdict sur le rate réellement livré.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum RateVerdict {
    /// Fenêtre inexploitable (aucun callback, géométrie inconnue, ou trous
    /// couvrant toute la fenêtre) : rien à conclure, on garde le déclaré.
    NotMeasurable,
    /// La cadence confirme le rate déclaré (à [`TOLERANCE`] près).
    Confirmed { measured_sr: u32 },
    /// Le pilote livre à un rate STANDARD différent de celui qu'il déclare :
    /// `actual_sr` est le rate à retenir (la garde R2 refusera s'il n'est pas 48 k).
    Lies { actual_sr: u32, measured_sr: u32 },
    /// La cadence ne correspond à aucun rate standard : la mesure est cassée
    /// (trous non comptés, fenêtre trop courte…), pas le pilote. On garde le
    /// déclaré — jamais un rate inexistant présenté à l'utilisateur.
    Inconclusive { measured_sr: u32 },
}

/// Rattache une cadence mesurée au rate standard le plus proche, si elle est à
/// moins de [`SNAP_HZ`] de l'un d'eux.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn snap_to_standard_rate(measured_sr: u32) -> Option<u32> {
    STANDARD_RATES
        .iter()
        .copied()
        .filter(|std| measured_sr.abs_diff(*std) <= SNAP_HZ)
        .min_by_key(|std| measured_sr.abs_diff(*std))
}

/// Décide, à partir de la fenêtre observée, si le rate déclaré est le rate livré.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn resolve_measured_rate(s: &RateSample) -> RateVerdict {
    if s.callbacks == 0 || s.frames_per_cb == 0 || s.declared_sr == 0 {
        return RateVerdict::NotMeasurable;
    }
    let active_us = s.window_us.saturating_sub(s.stall_us);
    if active_us == 0 {
        return RateVerdict::NotMeasurable;
    }
    let measured =
        (s.callbacks as f64 * s.frames_per_cb as f64) * 1_000_000.0 / active_us as f64;
    let measured_sr = measured.round() as u32;
    let rel_err = (measured - s.declared_sr as f64).abs() / s.declared_sr as f64;
    if rel_err <= TOLERANCE {
        return RateVerdict::Confirmed { measured_sr };
    }
    match snap_to_standard_rate(measured_sr) {
        // Un rate standard qui n'est pas le déclaré (le snap ne peut pas rendre le
        // déclaré : à SNAP_HZ près, il serait sous la tolérance).
        Some(actual_sr) if actual_sr != s.declared_sr => {
            RateVerdict::Lies { actual_sr, measured_sr }
        }
        _ => RateVerdict::Inconclusive { measured_sr },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cas Guillaume H. (rapport du 25/09/2026, agent 0.6.5) : UR22C à 48 kHz,
    /// buffer 32, un trou de 195,8 ms dans la fenêtre de 500 ms, 464 callbacks.
    /// Avant : « 29 731 Hz » et refus. Attendu : 48 kHz confirmé.
    #[test]
    fn trou_de_demarrage_yamaha_buffer_32_confirme_48k() {
        let s = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 32,
            callbacks: 464,
            window_us: 500_000,
            stall_us: 195_827 - 667,
        };
        match resolve_measured_rate(&s) {
            RateVerdict::Confirmed { measured_sr } => {
                assert!((47_000..=49_500).contains(&measured_sr), "mesuré {measured_sr}");
            }
            other => panic!("attendu Confirmed, obtenu {other:?}"),
        }
    }

    /// Même rapport, buffer 48 : trou de 72 ms, ~433 callbacks (« 41 540 Hz » avant).
    #[test]
    fn trou_de_demarrage_yamaha_buffer_48_confirme_48k() {
        let s = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 48,
            callbacks: 433,
            window_us: 500_000,
            stall_us: 72_025 - 1_000,
        };
        assert!(matches!(resolve_measured_rate(&s), RateVerdict::Confirmed { .. }));
    }

    /// Le pilote menteur du 04/08 (Focusrite natif) : déclare 48 000, livre 689
    /// callbacks/s × 64 frames. Aucun trou (ses intervalles sont tous lents, pas
    /// en retard au sens du seuil 2 × budget). Attendu : 44 100 retenu.
    #[test]
    fn pilote_menteur_44100_est_detecte() {
        let s = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 64,
            callbacks: 345, // 689/s sur 500 ms
            window_us: 500_000,
            stall_us: 0,
        };
        match resolve_measured_rate(&s) {
            RateVerdict::Lies { actual_sr, measured_sr } => {
                assert_eq!(actual_sr, 44_100);
                assert!((43_700..=44_500).contains(&measured_sr), "mesuré {measured_sr}");
            }
            other => panic!("attendu Lies, obtenu {other:?}"),
        }
    }

    /// Le menteur reste détecté même avec un trou de démarrage en plus.
    #[test]
    fn pilote_menteur_avec_trou_reste_detecte() {
        let s = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 64,
            callbacks: 207, // 689/s sur 300 ms actifs
            window_us: 500_000,
            stall_us: 200_000,
        };
        assert!(matches!(
            resolve_measured_rate(&s),
            RateVerdict::Lies { actual_sr: 44_100, .. }
        ));
    }

    /// Sans trou compté (ancienne mesure brute), la cadence Yamaha « 29 731 Hz »
    /// ne tombe sur aucun rate standard : mesure non concluante, déclaré gardé —
    /// plus jamais un rate inexistant présenté à l'utilisateur.
    #[test]
    fn cadence_hors_grille_standard_est_non_concluante() {
        let s = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 32,
            callbacks: 464,
            window_us: 500_000,
            stall_us: 0,
        };
        match resolve_measured_rate(&s) {
            RateVerdict::Inconclusive { measured_sr } => assert_eq!(measured_sr, 29_696),
            other => panic!("attendu Inconclusive, obtenu {other:?}"),
        }
    }

    /// Session saine, sans trou : 750 cb/s × 64 = 48 000 exactement.
    #[test]
    fn cadence_nominale_sans_trou_confirme() {
        let s = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 64,
            callbacks: 375,
            window_us: 500_000,
            stall_us: 0,
        };
        assert_eq!(
            resolve_measured_rate(&s),
            RateVerdict::Confirmed { measured_sr: 48_000 }
        );
    }

    /// Deux trous dans la fenêtre : leur somme est exclue, le rate est confirmé.
    #[test]
    fn deux_trous_cumules_sont_exclus() {
        // 500 ms, deux trous de 60 et 90 ms → 350 ms actifs → ~262 cb à 64 frames.
        let s = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 64,
            callbacks: 262,
            window_us: 500_000,
            stall_us: 150_000,
        };
        assert!(matches!(resolve_measured_rate(&s), RateVerdict::Confirmed { .. }));
    }

    #[test]
    fn fenetre_inexploitable_ne_conclut_rien() {
        let base = RateSample {
            declared_sr: 48_000,
            frames_per_cb: 64,
            callbacks: 100,
            window_us: 500_000,
            stall_us: 0,
        };
        assert_eq!(
            resolve_measured_rate(&RateSample { callbacks: 0, ..base }),
            RateVerdict::NotMeasurable
        );
        assert_eq!(
            resolve_measured_rate(&RateSample { frames_per_cb: 0, ..base }),
            RateVerdict::NotMeasurable
        );
        assert_eq!(
            resolve_measured_rate(&RateSample { declared_sr: 0, ..base }),
            RateVerdict::NotMeasurable
        );
        // Trous couvrant toute la fenêtre (ou plus, par arrondi) : pas de temps actif.
        assert_eq!(
            resolve_measured_rate(&RateSample { stall_us: 600_000, ..base }),
            RateVerdict::NotMeasurable
        );
    }

    #[test]
    fn snap_rattache_a_moins_de_400_hz_seulement() {
        assert_eq!(snap_to_standard_rate(44_096), Some(44_100));
        assert_eq!(snap_to_standard_rate(48_390), Some(48_000));
        assert_eq!(snap_to_standard_rate(96_100), Some(96_000));
        assert_eq!(snap_to_standard_rate(29_731), None);
        assert_eq!(snap_to_standard_rate(41_540), None);
        assert_eq!(snap_to_standard_rate(46_050), None, "à mi-chemin, rattaché à rien");
    }
}
