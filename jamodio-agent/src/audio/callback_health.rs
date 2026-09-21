//! Santé du callback audio temps-réel — ses irrégularités, mesurées (ligne « CALLBACK AUDIO IRRÉGULIER »).
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
    /// ── Ce que le PILOTE annonce à chaque bascule (mesure du 21/09/2026) ──
    ///
    /// Deux gels de ~14 ms du callback ASIO (Focusrite, 20/09 et 21/09) ont
    /// chacun été suivis d'une prise abîmée jusqu'à la réouverture du pilote.
    /// Hypothèse à trancher : après le gel, le pilote rattrape en rafale et la
    /// moitié de tampon qu'on lit n'est plus celle qu'il vient de remplir. Ces
    /// compteurs disent ce que le pilote a réellement annoncé.
    ///
    /// Même moitié de tampon annoncée deux bascules de suite (0,0 ou 1,1) :
    /// on relit alors un tampon que le pilote n'a pas rafraîchi.
    index_repeats: AtomicU64,
    /// Position annoncée qui n'avance pas d'exactement une taille de tampon.
    position_irregular: AtomicU64,
    /// Pire écart (échantillons) entre l'avance annoncée et la taille du tampon.
    position_worst_dev: AtomicU64,
    /// Bascules sans position valide (pilote qui ne la fournit pas).
    position_missing: AtomicU64,
    /// Bascules arrivées en rafale : moins de `BURST_GAP_US` après la
    /// précédente. Mesure seule : c'est aussi un régime SAIN chez certains
    /// pilotes (~250/s sur la Focusrite, 21/09/2026), donc jamais une anomalie
    /// en soi — on la lit en comparant avant/après un gel.
    burst_blocks: AtomicU64,
}

/// En deçà, deux bascules sont « en rafale ». Seuil de MESURE, pas d'alerte : la
/// Focusrite en produit ~250/s en régime sain (cf. `burst_blocks`).
pub const BURST_GAP_US: u64 = 300;

/// État PRIVÉ du callback (un seul thread l'écrit) : la bascule précédente.
#[derive(Debug, Default)]
pub struct SwitchTracker {
    prev_index: Option<usize>,
    prev_position: Option<i64>,
}

/// Instantané d'une fenêtre, rendu par [`CallbackHealth::drain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CallbackHealthWindow {
    pub blocks: u64,
    pub late_blocks: u64,
    pub over_budget_blocks: u64,
    pub worst_gap_us: u64,
    pub worst_work_us: u64,
    pub index_repeats: u64,
    pub position_irregular: u64,
    pub position_worst_dev: u64,
    pub position_missing: u64,
    pub burst_blocks: u64,
}

impl CallbackHealthWindow {
    /// `true` si aucun bloc n'a été en retard ni hors budget — le cas nominal,
    /// pour lequel on ne journalise RIEN.
    ///
    /// Les bascules en rafale (`burst_blocks`) n'y figurent PAS : la Focusrite en
    /// livre ~250 par seconde en régime sain (mesuré sur 10 207 s le 21/09/2026),
    /// et les compter comme anomalie écrivait une ligne CHAQUE seconde. Elles
    /// restent mesurées et portées par la ligne quand une vraie anomalie l'ouvre.
    pub fn is_clean(&self) -> bool {
        self.late_blocks == 0
            && self.over_budget_blocks == 0
            && self.index_repeats == 0
            && self.position_irregular == 0
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
    // Consommé par le seul chemin Windows (callback ASIO) ; la LOGIQUE reste
    // multi-plateforme pour rester testable partout — d'où l'`allow` ciblé, et
    // surtout pas un `allow` posé sur le module entier.
    #[cfg_attr(not(windows), allow(dead_code))]
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

    /// Enregistre ce que le pilote annonce pour CETTE bascule. Appelé une fois par
    /// callback. Coût : quelques comparaisons d'entiers ; un atomique n'est
    /// touché QUE sur anomalie (hors `position_missing`, un par bloc quand le
    /// pilote ne donne pas de position).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn record_switch(
        &self,
        tracker: &mut SwitchTracker,
        index: usize,
        position: Option<i64>,
        buffer_frames: u32,
        gap_us: Option<u64>,
    ) {
        if tracker.prev_index == Some(index) {
            self.index_repeats.fetch_add(1, Relaxed);
        }
        tracker.prev_index = Some(index);
        match (tracker.prev_position, position) {
            (Some(prev), Some(pos)) => {
                let dev = (pos - prev - i64::from(buffer_frames)).unsigned_abs();
                if dev != 0 {
                    self.position_irregular.fetch_add(1, Relaxed);
                    self.position_worst_dev.fetch_max(dev, Relaxed);
                }
            }
            (_, None) => {
                self.position_missing.fetch_add(1, Relaxed);
            }
            (None, Some(_)) => {}
        }
        tracker.prev_position = position;
        if gap_us.is_some_and(|g| g < BURST_GAP_US) {
            self.burst_blocks.fetch_add(1, Relaxed);
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
            index_repeats: self.index_repeats.swap(0, Relaxed),
            position_irregular: self.position_irregular.swap(0, Relaxed),
            position_worst_dev: self.position_worst_dev.swap(0, Relaxed),
            position_missing: self.position_missing.swap(0, Relaxed),
            burst_blocks: self.burst_blocks.swap(0, Relaxed),
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
// Consommé par le seul chemin Windows ; la logique reste multi-plateforme pour
// rester testable partout — `allow` CIBLÉ, jamais posé sur le module entier.
#[cfg_attr(not(windows), allow(dead_code))]
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

    // ─── Bascules annoncées par le pilote (21/09/2026) ────────────────────

    #[test]
    fn des_bascules_saines_ne_laissent_aucune_trace() {
        let h = CallbackHealth::new();
        let mut t = SwitchTracker::default();
        for i in 0..100i64 {
            let gap = if i % 3 == 0 { 2300 } else { 945 }; // régime bimodal sain
            h.record_switch(&mut t, (i % 2) as usize, Some(i * 64), 64, Some(gap));
        }
        let w = h.drain();
        assert_eq!((w.index_repeats, w.position_irregular, w.burst_blocks), (0, 0, 0));
        assert_eq!(w.position_missing, 0);
        assert!(w.is_clean());
    }

    #[test]
    fn la_meme_moitie_deux_fois_est_comptee() {
        let h = CallbackHealth::new();
        let mut t = SwitchTracker::default();
        h.record_switch(&mut t, 0, Some(0), 64, None);
        h.record_switch(&mut t, 0, Some(64), 64, Some(900));
        let w = h.drain();
        assert_eq!(w.index_repeats, 1);
        assert!(!w.is_clean());
    }

    #[test]
    fn une_position_qui_saute_est_mesuree() {
        let h = CallbackHealth::new();
        let mut t = SwitchTracker::default();
        h.record_switch(&mut t, 0, Some(0), 64, None);
        // Gel : le pilote annonce 10 tampons plus loin d'un coup.
        h.record_switch(&mut t, 1, Some(640), 64, Some(14_000));
        // Puis une position qui recule (tampon relu).
        h.record_switch(&mut t, 0, Some(576), 64, Some(900));
        let w = h.drain();
        assert_eq!(w.position_irregular, 2);
        assert_eq!(w.position_worst_dev, 576, "640 − 0 − 64");
    }

    #[test]
    fn un_rattrapage_en_rafale_est_compte() {
        let h = CallbackHealth::new();
        let mut t = SwitchTracker::default();
        h.record_switch(&mut t, 0, Some(0), 64, Some(14_000));
        for i in 1..6i64 {
            h.record_switch(&mut t, (i % 2) as usize, Some(i * 64), 64, Some(40));
        }
        let w = h.drain();
        assert_eq!(w.burst_blocks, 5);
        // Le gel, lui, salit la fenêtre (bloc en retard) ; la rafale seule non.
        assert!(w.is_clean(), "record_switch seul ne compte pas le retard : c'est record_block");
    }

    #[test]
    fn un_pilote_sans_position_est_signale_sans_salir_la_fenetre() {
        let h = CallbackHealth::new();
        let mut t = SwitchTracker::default();
        for i in 0..4 {
            h.record_switch(&mut t, i % 2, None, 64, Some(1333));
        }
        let w = h.drain();
        assert_eq!(w.position_missing, 4);
        assert!(w.is_clean(), "l'absence de position n'est pas une anomalie de flux");
    }

    /// Régression du 21/09/2026 : la 0.6.5-21 écrivait une ligne par seconde,
    /// parce que les rafales — régime sain de la Focusrite — salissaient la fenêtre.
    #[test]
    fn des_rafales_seules_ne_font_pas_une_seconde_anormale() {
        let h = CallbackHealth::new();
        let mut t = SwitchTracker::default();
        for i in 0..750i64 {
            let gap = if i % 3 == 0 { 40 } else { 1900 };
            h.record_block(Some(gap), 80, 1333, 2666);
            h.record_switch(&mut t, (i % 2) as usize, Some(i * 64), 64, Some(gap));
        }
        let w = h.drain();
        assert_eq!(w.burst_blocks, 250);
        assert!(w.is_clean(), "régime sain : aucune ligne");
    }
}
