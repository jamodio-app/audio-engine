//! Santé du callback audio temps-réel — diagnostic des CRAQUEMENTS.
//!
//! # Pourquoi
//!
//! Un craquement (audio déchiré, injouable) n'a que deux causes possibles côté
//! hôte :
//!
//! 1. **bloc EN RETARD** — le driver/l'OS ne nous a pas rappelés à temps :
//!    l'intervalle entre deux callbacks a dépassé le budget du bloc. La sortie a
//!    forcément été servie en retard (ou pas du tout) ;
//! 2. **bloc HORS BUDGET** — notre propre traitement a dépassé le budget : on n'a
//!    pas rendu la main avant que le driver ne réclame le bloc suivant.
//!
//! **Aucune des deux n'était mesurée.** `drops_per_sec` ne compte que les envois
//! ratés vers l'encodeur (canal plein), pas les deadlines manquées ; et un débit
//! de callbacks moyenné à la seconde (`capture_cb_per_sec`) absorbe sans broncher
//! une poignée de blocs ratés — 744/s au lieu de 750/s reste dans le bruit normal
//! alors que six trous sont parfaitement audibles. D'où ce module : il rend les
//! craquements CHIFFRABLES au lieu de « ça craque ».
//!
//! # Contrainte temps-réel (garde-fou latence)
//!
//! Alimenté DEPUIS le callback temps-réel, donc : uniquement des atomiques
//! `Relaxed` et **deux lectures d'horloge par bloc**. Sur Windows `Instant::now()`
//! est un `QueryPerformanceCounter` (~30 ns) : ~60 ns par bloc contre un budget de
//! 1333 µs à 64 frames/48 kHz, soit **0,005 %**. Aucun log, aucune allocation,
//! aucun verrou, aucun syscall bloquant sur ce chemin — l'étage le plus sensible
//! du produit reste intact.
//!
//! # Lecture
//!
//! Le superviseur perfstats (1 Hz) appelle [`CallbackHealth::drain`] : il obtient
//! la fenêtre écoulée et remet les compteurs à zéro. Il ne journalise QUE si la
//! fenêtre n'est pas propre — une session saine n'ajoute donc **aucune ligne** de
//! log, et chaque ligne présente désigne une seconde réellement dégradée.
//!
//! Aujourd'hui seul l'hôte ASIO (Windows) alimente ces compteurs ; le module reste
//! multi-plateforme pour que le chemin CoreAudio puisse y venir sans refonte (sur
//! macOS les compteurs restent à zéro ⇒ fenêtre propre ⇒ silence).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Compteurs d'irrégularité du callback audio, partagés entre le thread du driver
/// (écriture) et le superviseur perfstats (lecture + reset). Voir le module.
#[derive(Debug, Default)]
pub struct CallbackHealth {
    /// Blocs réellement traités sur la fenêtre (dénominateur honnête des ratios).
    blocks: AtomicU64,
    /// Blocs dont l'intervalle depuis le précédent a dépassé le seuil de retard.
    late_blocks: AtomicU64,
    /// Blocs dont le traitement a dépassé le budget du bloc.
    over_budget_blocks: AtomicU64,
    /// Pire intervalle inter-callback de la fenêtre (µs).
    worst_gap_us: AtomicU64,
    /// Pire durée de traitement de la fenêtre (µs).
    worst_work_us: AtomicU64,
}

/// Instantané d'une fenêtre, rendu par [`CallbackHealth::drain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CallbackHealthWindow {
    pub blocks: u64,
    pub late_blocks: u64,
    pub over_budget_blocks: u64,
    pub worst_gap_us: u64,
    pub worst_work_us: u64,
}

impl CallbackHealthWindow {
    /// `true` si aucun bloc n'a été en retard ni hors budget — le cas nominal,
    /// pour lequel on ne journalise RIEN.
    pub fn is_clean(&self) -> bool {
        self.late_blocks == 0 && self.over_budget_blocks == 0
    }
}

impl CallbackHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enregistre un bloc traité. Appelé **une fois par callback**, à la fin, avec
    /// les deux mesures prises autour du traitement.
    ///
    /// - `gap_us` : intervalle depuis le bloc précédent, `None` au tout premier
    ///   bloc (aucun précédent ⇒ aucun retard imputable) ;
    /// - `work_us` : durée du traitement de CE bloc ;
    /// - `budget_us` : budget du bloc (`frames × 1e6 / sample_rate`) ;
    /// - `late_us` : seuil au-delà duquel l'intervalle compte comme un retard
    ///   (cf. [`late_threshold_us`] — strictement supérieur au budget pour ne pas
    ///   compter la gigue normale du driver).
    pub fn record_block(
        &self,
        gap_us: Option<u64>,
        work_us: u64,
        budget_us: u64,
        late_us: u64,
    ) {
        self.blocks.fetch_add(1, Relaxed);
        if let Some(gap) = gap_us {
            self.worst_gap_us.fetch_max(gap, Relaxed);
            if gap > late_us {
                self.late_blocks.fetch_add(1, Relaxed);
            }
        }
        self.worst_work_us.fetch_max(work_us, Relaxed);
        if work_us > budget_us {
            self.over_budget_blocks.fetch_add(1, Relaxed);
        }
    }

    /// Rend la fenêtre écoulée et remet tous les compteurs à zéro (lecture 1 Hz).
    pub fn drain(&self) -> CallbackHealthWindow {
        CallbackHealthWindow {
            blocks: self.blocks.swap(0, Relaxed),
            late_blocks: self.late_blocks.swap(0, Relaxed),
            over_budget_blocks: self.over_budget_blocks.swap(0, Relaxed),
            worst_gap_us: self.worst_gap_us.swap(0, Relaxed),
            worst_work_us: self.worst_work_us.swap(0, Relaxed),
        }
    }
}

/// Budget d'un bloc en µs. `0` si l'un des paramètres est nul (pas encore mesuré)
/// — l'appelant traite alors la fenêtre comme non exploitable plutôt que de
/// diviser par zéro.
pub fn block_budget_us(frames: u32, sample_rate: u32) -> u64 {
    if frames == 0 || sample_rate == 0 {
        return 0;
    }
    (frames as u64) * 1_000_000 / (sample_rate as u64)
}

/// Seuil de RETARD : **2 × le budget**, c'est-à-dire une PÉRIODE DE BLOC ENTIÈRE
/// manquée.
///
/// # Pourquoi pas plus serré (mesuré, ne pas re-resserrer sans données)
///
/// Un driver ASIO ne rappelle PAS à intervalle régulier. Mesure du 05/09 sur
/// Focusrite USB ASIO (64 frames @ 48 kHz, budget 1333 µs), session parfaitement
/// audible : la distribution des intervalles est **bimodale** — ~29 % des blocs
/// arrivent vers 2300 µs, les ~71 % restants vers 945 µs, la moyenne retombant
/// exactement sur les 1333 µs nominaux (750 blocs/s). Le driver livre donc par
/// à-coups, et c'est son régime SAIN.
///
/// Conséquence : un seuil à 1,5 × (1999 µs) déclenchait sur **100 % des fenêtres**
/// d'une session saine — métrique inutile, et 532 lignes de log pour rien. À 2 ×
/// (2666 µs) la même session ne retient que 22 fenêtres sur 532, groupées sur un
/// incident réel (rafale de 16 fenêtres en 47 s, intervalles de 3 à 4,2 ms).
///
/// La sémantique est aussi plus honnête : sous 2 × le budget, le bloc suivant est
/// arrivé avant que le précédent n'ait fini d'être joué — rien n'a manqué à la
/// sortie. Au-delà, une période entière est passée sans être servie : c'est
/// audible.
pub fn late_threshold_us(budget_us: u64) -> u64 {
    budget_us * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_et_seuil_a_64_frames_48k() {
        // 64 frames à 48 kHz = 1333 µs ; seuil de retard = 2 × = 2666 µs.
        let budget = block_budget_us(64, 48_000);
        assert_eq!(budget, 1333);
        assert_eq!(late_threshold_us(budget), 2666);
        // 128 frames = le double.
        assert_eq!(block_budget_us(128, 48_000), 2666);
    }

    #[test]
    fn budget_nul_si_pas_encore_mesure() {
        assert_eq!(block_budget_us(0, 48_000), 0);
        assert_eq!(block_budget_us(64, 0), 0);
    }

    #[test]
    fn fenetre_saine_est_propre_et_ne_compte_aucun_defaut() {
        let h = CallbackHealth::new();
        let (budget, late) = (1333, 2666);
        // Trois blocs dans les clous : intervalle nominal, traitement bref.
        for _ in 0..3 {
            h.record_block(Some(1333), 400, budget, late);
        }
        let w = h.drain();
        assert!(w.is_clean(), "aucun retard ni dépassement");
        assert_eq!(w.blocks, 3);
        assert_eq!(w.worst_gap_us, 1333);
        assert_eq!(w.worst_work_us, 400);
    }

    /// Régression du 05/09 : le mode LONG de la livraison bimodale d'un driver ASIO
    /// sain (~2300 µs mesurés sur Focusrite USB, cf. `late_threshold_us`) ne doit
    /// PAS être compté comme un retard — sinon la métrique déclenche sur 100 % des
    /// fenêtres d'une session parfaitement audible et ne discrimine plus rien.
    #[test]
    fn le_mode_long_d_un_driver_sain_n_est_pas_un_retard() {
        let h = CallbackHealth::new();
        let budget = block_budget_us(64, 48_000);
        let late = late_threshold_us(budget);
        // Les deux modes réellement mesurés sur une session saine.
        h.record_block(Some(945), 400, budget, late); // mode court
        h.record_block(Some(2300), 400, budget, late); // mode long
        let w = h.drain();
        assert_eq!(w.late_blocks, 0, "la livraison par à-coups du driver est SAINE");
        assert_eq!(w.worst_gap_us, 2300, "mais reste visible dans le pire cas");
        assert!(w.is_clean());
    }

    #[test]
    fn bloc_en_retard_est_compte() {
        let h = CallbackHealth::new();
        let (budget, late) = (1333, 2666);
        h.record_block(Some(1333), 400, budget, late); // sain
        h.record_block(Some(4200), 400, budget, late); // driver/OS a stallé
        let w = h.drain();
        assert_eq!(w.late_blocks, 1);
        assert_eq!(w.over_budget_blocks, 0, "notre traitement, lui, tenait");
        assert_eq!(w.worst_gap_us, 4200);
        assert!(!w.is_clean());
    }

    #[test]
    fn bloc_hors_budget_est_compte() {
        let h = CallbackHealth::new();
        let (budget, late) = (1333, 2666);
        h.record_block(Some(1333), 1500, budget, late); // on a débordé
        let w = h.drain();
        assert_eq!(w.over_budget_blocks, 1);
        assert_eq!(w.late_blocks, 0, "le driver, lui, était à l'heure");
        assert_eq!(w.worst_work_us, 1500);
        assert!(!w.is_clean());
    }

    #[test]
    fn premier_bloc_sans_precedent_n_est_jamais_en_retard() {
        let h = CallbackHealth::new();
        h.record_block(None, 400, 1333, 2666);
        let w = h.drain();
        assert_eq!(w.blocks, 1);
        assert_eq!(w.late_blocks, 0);
        assert_eq!(w.worst_gap_us, 0, "aucun intervalle mesurable au 1er bloc");
        assert!(w.is_clean());
    }

    #[test]
    fn drain_remet_tout_a_zero() {
        let h = CallbackHealth::new();
        h.record_block(Some(9000), 5000, 1333, 2666);
        let first = h.drain();
        assert!(!first.is_clean());
        let second = h.drain();
        assert_eq!(second, CallbackHealthWindow::default());
        assert!(second.is_clean(), "fenêtre suivante repart propre");
    }
}
