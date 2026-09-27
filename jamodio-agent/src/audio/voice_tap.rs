//! File capture → étage voix du talkback, bornée en DURÉE d'audio, et surveillance
//! de la saturation de l'isolation de voix.
//!
//! # Pourquoi une borne en durée
//!
//! La file était un canal de 32 BLOCS. Sa marge dépendait donc de la taille de bloc
//! du pilote : 170 ms à 256 frames, mais **32 ms à 48 frames** — le réglage basse
//! latence. L'isolation de voix travaille par tranches de 10 ms (débruitage) et
//! 32 ms (détection de parole), et une tranche peut coûter plus de 20 ms sur une
//! machine modeste (21 ms mesurés sur le NUC le 21/09/2026). Cas Guillaume H.
//! (UR22C, buffer 48, 26/09/2026) : 15 à 30 % des blocs voix jetés pendant toute la
//! répétition — une voix hachée, que le gate de parole, nourri d'un signal troué,
//! ouvrait et refermait au hasard (« un noise gate mal réglé »).
//!
//! La borne se compte désormais en échantillons : [`VOICE_TAP_MAX_MS`] de voix en
//! attente, quelle que soit la taille des blocs. Ce délai ne s'ajoute QUE quand
//! l'étage voix est en retard, et se résorbe dès qu'il rattrape ; il ne touche que
//! le talkback, jamais l'instrument ni le self-monitor.
//!
//! # Pourquoi une surveillance
//!
//! Aucune file ne sauve un étage qui, en moyenne, calcule moins vite que le temps
//! réel : elle se remplit, puis jette. Quand le talkback perd de la voix de façon
//! PERSISTANTE alors que l'isolation tourne, [`SaturationWatch`] le dit et l'étage
//! voix bascule en voix brute pour le reste de la session — un repli VISIBLE (l'UI
//! affiche « VOIX BRUTE »), pas une voix inintelligible.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};

/// Voix maximale en attente devant l'étage voix, en millisecondes. Absorbe une
/// tranche d'isolation lente (> 20 ms) ou une préemption, sans laisser la voix
/// dériver de plus de 100 ms quand la machine ne suit pas (coût annoncé : latence
/// TRANSITOIRE du talkback seul, cf. module).
pub const VOICE_TAP_MAX_MS: u32 = 100;

/// Fréquence du canal voix (48 kHz natif — R2).
const VOICE_RATE: u32 = 48_000;

/// Plus petit bloc que le canal doit pouvoir contenir en nombre suffisant pour
/// atteindre la borne en durée : le canal crossbeam, borné en ÉLÉMENTS, n'est plus
/// qu'un garde-fou ; la vraie borne est le compte d'échantillons.
const MIN_BLOCK_FRAMES: usize = 16;

/// Ce qu'est devenu un bloc poussé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Queued,
    /// Étage voix en retard de plus de la borne : bloc jeté (et compté).
    Dropped,
}

/// L'étage voix est terminé : le producteur doit cesser de pousser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disconnected;

#[derive(Debug)]
struct Shared {
    /// Échantillons poussés et pas encore reçus.
    queued: AtomicUsize,
    /// Échantillons jetés depuis la création — monotone.
    dropped: AtomicU64,
    max_queued: usize,
}

/// Côté producteur (capture instrument ou micro dédié). Non bloquant, aucune
/// allocation : deux opérations atomiques par bloc, hors du callback d'entrée
/// instrument (le tap instrument vit dans `capture_stage`).
#[derive(Debug, Clone)]
pub struct VoiceTapSender {
    tx: Sender<Vec<f32>>,
    shared: Arc<Shared>,
}

/// Côté étage voix.
#[derive(Debug)]
pub struct VoiceTapReceiver {
    rx: Receiver<Vec<f32>>,
    shared: Arc<Shared>,
}

/// Crée une file bornée à `max_ms` de voix à 48 kHz.
pub fn voice_tap(max_ms: u32) -> (VoiceTapSender, VoiceTapReceiver) {
    let max_queued = (max_ms as usize * VOICE_RATE as usize / 1000).max(1);
    let (tx, rx) = bounded::<Vec<f32>>(max_queued.div_ceil(MIN_BLOCK_FRAMES) + 1);
    let shared = Arc::new(Shared {
        queued: AtomicUsize::new(0),
        dropped: AtomicU64::new(0),
        max_queued,
    });
    (
        VoiceTapSender { tx, shared: shared.clone() },
        VoiceTapReceiver { rx, shared },
    )
}

impl VoiceTapSender {
    /// Pousse un bloc mono 48 kHz, ou le jette (compté) si l'étage voix a déjà
    /// plus de la borne en attente. Une file VIDE accepte toujours le bloc, si
    /// grand soit-il : la borne limite le retard, elle ne refuse pas un pilote à
    /// gros blocs.
    pub fn push(&self, block: Vec<f32>) -> Result<Pushed, Disconnected> {
        let n = block.len();
        // Réservé AVANT l'envoi : le consommateur ne peut ainsi jamais retirer
        // des échantillons pas encore comptés (pas de sous-dépassement).
        let before = self.shared.queued.fetch_add(n, Relaxed);
        if before > 0 && before + n > self.shared.max_queued {
            return Ok(self.reject(n));
        }
        match self.tx.try_send(block) {
            Ok(()) => Ok(Pushed::Queued),
            Err(TrySendError::Full(_)) => Ok(self.reject(n)),
            Err(TrySendError::Disconnected(_)) => {
                self.shared.queued.fetch_sub(n, Relaxed);
                Err(Disconnected)
            }
        }
    }

    fn reject(&self, n: usize) -> Pushed {
        self.shared.queued.fetch_sub(n, Relaxed);
        self.shared.dropped.fetch_add(n as u64, Relaxed);
        Pushed::Dropped
    }
}

impl VoiceTapReceiver {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<f32>, RecvTimeoutError> {
        let block = self.rx.recv_timeout(timeout)?;
        self.shared.queued.fetch_sub(block.len(), Relaxed);
        Ok(block)
    }

    /// Échantillons jetés depuis la création (monotone).
    pub fn dropped_samples_total(&self) -> u64 {
        self.shared.dropped.load(Relaxed)
    }
}

/// Fenêtre d'observation de la saturation.
pub const SATURATION_WINDOW: Duration = Duration::from_secs(2);

/// Part de voix perdue au-delà de laquelle une fenêtre est dite saturée. Avec une
/// file de 100 ms, un seul bloc jeté signifie déjà un retard de 100 ms : 1 % (20 ms
/// perdus sur 2 s) écarte la poussière sans laisser passer une voix hachée.
pub const SATURATION_DROP_RATIO: f64 = 0.01;

/// Nombre de fenêtres saturées CONSÉCUTIVES avant de renoncer à l'isolation — une
/// préemption isolée ne coupe pas le filtre, une saturation durable si.
pub const SATURATION_DEBOUNCE: u32 = 2;

/// Surveillance de la saturation de l'étage voix, fenêtre par fenêtre.
#[derive(Debug, Default)]
pub struct SaturationWatch {
    streak: u32,
}

impl SaturationWatch {
    /// Juge une fenêtre (échantillons traités et jetés pendant la fenêtre) ; rend
    /// `true` quand la saturation est établie et qu'il faut quitter l'isolation.
    pub fn observe(&mut self, processed: u64, dropped: u64) -> bool {
        let total = processed + dropped;
        let saturated = total > 0 && dropped as f64 / total as f64 > SATURATION_DROP_RATIO;
        self.streak = if saturated { self.streak + 1 } else { 0 };
        self.streak >= SATURATION_DEBOUNCE
    }
}

/// Mesure du coût de l'isolation de voix, lue et remise à zéro par le journal
/// perfstats (1 Hz). Alimentée par l'étage voix (atomiques seuls, hors thread audio).
#[derive(Debug, Default)]
pub struct VoiceStageStats {
    /// Temps passé dans l'isolation pendant la fenêtre (µs).
    iso_work_us: AtomicU64,
    /// Durée d'audio passée par l'isolation pendant la fenêtre (µs).
    iso_audio_us: AtomicU64,
    /// Pire temps d'isolation d'un seul bloc (µs).
    iso_max_block_us: AtomicU64,
    /// Voix jetée devant l'étage voix pendant la fenêtre (µs d'audio).
    dropped_audio_us: AtomicU64,
}

/// Fenêtre drainée de [`VoiceStageStats`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VoiceStageWindow {
    /// Charge de l'isolation, en % d'un cœur : temps de calcul / durée d'audio.
    /// Au-delà de 100 %, l'étage ne peut pas suivre le temps réel.
    pub iso_load_pct: f64,
    pub iso_max_block_ms: f64,
    pub dropped_ms: f64,
}

impl VoiceStageStats {
    pub fn record_isolation(&self, work: Duration, audio_samples: usize) {
        let work_us = work.as_micros() as u64;
        self.iso_work_us.fetch_add(work_us, Relaxed);
        self.iso_audio_us.fetch_add(samples_to_us(audio_samples as u64), Relaxed);
        self.iso_max_block_us.fetch_max(work_us, Relaxed);
    }

    pub fn record_dropped(&self, samples: u64) {
        self.dropped_audio_us.fetch_add(samples_to_us(samples), Relaxed);
    }

    pub fn drain(&self) -> VoiceStageWindow {
        let work = self.iso_work_us.swap(0, Relaxed);
        let audio = self.iso_audio_us.swap(0, Relaxed);
        VoiceStageWindow {
            iso_load_pct: if audio > 0 { work as f64 * 100.0 / audio as f64 } else { 0.0 },
            iso_max_block_ms: self.iso_max_block_us.swap(0, Relaxed) as f64 / 1000.0,
            dropped_ms: self.dropped_audio_us.swap(0, Relaxed) as f64 / 1000.0,
        }
    }
}

fn samples_to_us(samples: u64) -> u64 {
    samples * 1_000_000 / VOICE_RATE as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOC_48: usize = 48; // 1 ms, le réglage de Guillaume

    #[test]
    fn la_borne_se_compte_en_duree_pas_en_blocs() {
        // 100 ms à 48 frames = 100 blocs, là où l'ancienne file en prenait 32.
        let (tx, _rx) = voice_tap(VOICE_TAP_MAX_MS);
        let acceptes = (0..200)
            .filter(|_| tx.push(vec![0.0; BLOC_48]) == Ok(Pushed::Queued))
            .count();
        assert_eq!(acceptes, 100);
        // Même borne en durée avec de gros blocs : 256 frames ≈ 5,3 ms → 18 blocs.
        let (tx, _rx) = voice_tap(VOICE_TAP_MAX_MS);
        let acceptes = (0..50)
            .filter(|_| tx.push(vec![0.0; 256]) == Ok(Pushed::Queued))
            .count();
        assert_eq!(acceptes, 4_800 / 256);
    }

    #[test]
    fn les_blocs_jetes_sont_comptes_et_la_place_se_libere() {
        let (tx, rx) = voice_tap(VOICE_TAP_MAX_MS);
        for _ in 0..100 {
            assert_eq!(tx.push(vec![0.0; BLOC_48]), Ok(Pushed::Queued));
        }
        assert_eq!(tx.push(vec![0.0; BLOC_48]), Ok(Pushed::Dropped));
        assert_eq!(rx.dropped_samples_total(), BLOC_48 as u64);
        // L'étage voix consomme un bloc : un bloc repasse.
        rx.recv_timeout(Duration::from_millis(10)).expect("bloc en file");
        assert_eq!(tx.push(vec![0.0; BLOC_48]), Ok(Pushed::Queued));
        assert_eq!(rx.dropped_samples_total(), BLOC_48 as u64, "rien de plus jeté");
    }

    #[test]
    fn une_file_vide_accepte_un_bloc_plus_long_que_la_borne() {
        let (tx, _rx) = voice_tap(VOICE_TAP_MAX_MS);
        assert_eq!(tx.push(vec![0.0; 8_192]), Ok(Pushed::Queued));
        assert_eq!(tx.push(vec![0.0; BLOC_48]), Ok(Pushed::Dropped));
    }

    #[test]
    fn un_etage_voix_termine_est_signale() {
        let (tx, rx) = voice_tap(VOICE_TAP_MAX_MS);
        drop(rx);
        assert_eq!(tx.push(vec![0.0; BLOC_48]), Err(Disconnected));
    }

    #[test]
    fn une_fenetre_isolee_ne_coupe_pas_l_isolation() {
        let mut w = SaturationWatch::default();
        assert!(!w.observe(96_000, 4_800)); // 5 % perdus une fois
        assert!(!w.observe(96_000, 0));
        assert!(!w.observe(96_000, 4_800));
    }

    #[test]
    fn une_saturation_durable_fait_quitter_l_isolation() {
        // Cas Guillaume : 15 à 30 % de voix perdue, fenêtre après fenêtre.
        let mut w = SaturationWatch::default();
        assert!(!w.observe(76_800, 19_200));
        assert!(w.observe(81_600, 14_400));
    }

    #[test]
    fn une_perte_infime_n_est_pas_une_saturation() {
        let mut w = SaturationWatch::default();
        for _ in 0..10 {
            assert!(!w.observe(96_000, 480), "0,5 % : sous le seuil");
        }
        assert!(!w.observe(0, 0), "fenêtre sans voix : rien à juger");
    }

    #[test]
    fn la_charge_se_lit_en_pourcentage_d_un_coeur() {
        let s = VoiceStageStats::default();
        // 10 ms d'audio traités en 5 ms, puis 10 ms en 21 ms (tranche lente du NUC).
        s.record_isolation(Duration::from_millis(5), 480);
        s.record_isolation(Duration::from_millis(21), 480);
        s.record_dropped(4_800);
        let w = s.drain();
        assert!((w.iso_load_pct - 130.0).abs() < 0.01, "{}", w.iso_load_pct);
        assert!((w.iso_max_block_ms - 21.0).abs() < 0.01);
        assert!((w.dropped_ms - 100.0).abs() < 0.01);
        assert_eq!(s.drain(), VoiceStageWindow::default(), "fenêtre remise à zéro");
    }
}
