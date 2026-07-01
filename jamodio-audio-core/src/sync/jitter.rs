//! Interarrival jitter estimator (RFC 3550 §A.8) for remote audio streams.
//!
//! Mesure la **gigue réseau réelle** d'un flux entrant : la variation du délai
//! de transit entre paquets consécutifs. Deux sorties :
//!
//! * `jitter_ms()` — moyenne EWMA de `|D|` (RFC 3550 §A.8). Télémétrie + warmup.
//! * `jitter_tail_ms()` — **queue** de la distribution des délais, qui dimensionne
//!   le plancher du jitter buffer (cf. `ring_buffer::observe_jitter`).
//!
//! # Pourquoi un percentile glissant et pas un peak-hold (P0, 01/07)
//!
//! L'ancien estimateur de queue était un **peak-hold à attaque instantanée**
//! (`if |D| > tail { tail = |D| }`, release lent ~4 s). Problème mesuré sur PC
//! Windows (3 sessions réelles) : un **stall d'ordonnancement local** (worker
//! tokio préempté sous charge — éditeur de plugin, batterie) horodate un paquet
//! en retard → un `|D|` isolé de 30 ms **épinglait instantanément** la queue, qui
//! mettait plus de 4 s à redescendre. La gigue réseau RÉELLE restait à ~0,6 ms
//! (`jitter_ms`) mais `jitter_tail` grimpait à 15-100 ms → le buffer se calait sur
//! un **artefact de mesure local** et **ne redescendait jamais** (« le lien se
//! dégrade, on ne se suit plus »).
//!
//! Le fix mesure la queue comme un **écart inter-percentile (p95 − p10) de la
//! distribution du transit sur une fenêtre bornée** (~1,3 s). Propriétés :
//!   * **Récupération naturelle** : une valeur sort de la fenêtre au bout de
//!     ~1,3 s → fini le release lent + ratchet qui pinnait à 40 ms.
//!   * **Robuste aux outliers isolés** : un spike qui touche < 5 % des paquets de
//!     la fenêtre (stall local bref) ne bouge pas le p95 → le plancher ne gonfle
//!     pas. Le filet réactif (+5 ms/underrun, dans `ring_buffer`) reste le
//!     backstop qui absorbe le trou audio réel de ce stall bref.
//!   * **Répond au vrai besoin** : une lateness FRÉQUENTE ou soutenue (> 5 % des
//!     paquets, machine réellement surchargée ou réseau bursty) monte le p95 → le
//!     buffer grandit, comme il doit.
//!
//! On dimensionne sur le **transit** (arrivée − timestamp RTP) et non sur `|D|` :
//! une rafale réseau soutenue élève le transit d'un CLUSTER de paquets (le p95 le
//! capte), là où `|D|` ne voit que les FRONTS (entrée/sortie) et ne distingue pas
//! la durée. L'offset d'horloge absolu et le drift lent se simplifient dans la
//! soustraction p95 − p10.
//!
//! Indépendant de l'OS (calcul pur) : comportement identique macOS / Windows.

use std::collections::VecDeque;
use std::time::Instant;

/// Sample rate de référence (Opus + CPAL Jamodio).
const SAMPLE_RATE_HZ: f64 = 48_000.0;

/// Gain de l'EWMA RFC 3550. 1/16 = compromis standard réactivité/stabilité.
const EWMA_GAIN: f64 = 1.0 / 16.0;

/// Taille de la fenêtre glissante de transit pour le percentile de queue.
/// À ~400 paquets/s (frame 2,5 ms), 512 ≈ **1,3 s** d'historique — assez pour
/// capter une rafale réseau récente, assez court pour que la queue redescende
/// vite quand le lien se calme (récupération naturelle, vs release ~4 s du
/// peak-hold). CONSTANTE DE CALIBRATION.
const TAIL_WINDOW: usize = 512;

/// Percentile HAUT de la queue : le plancher couvre 95 % des délais récents.
const TAIL_P_HIGH: f64 = 0.95;

/// Percentile BAS = base de référence robuste (≈ le « chemin rapide »). On mesure
/// l'ÉTALEMENT p95 − p10 plutôt que p95 − min : un unique paquet anormalement tôt
/// (arrivée groupée après un stall) ne peut pas abaisser la base et gonfler
/// artificiellement la queue.
const TAIL_P_LOW: f64 = 0.10;

/// Recalcule le percentile (tri de la fenêtre) tous les N paquets seulement — le
/// résultat est mis en cache et `jitter_tail_ms()` le lit en O(1). À ~400 pkt/s,
/// 32 → ~12 recalculs/s (tri de 512 f64 ≈ négligeable, hors chemin par-paquet).
const TAIL_RECOMPUTE_EVERY: u64 = 32;

/// En-dessous de ce remplissage de fenêtre, la queue est jugée non fiable → 0
/// (pas de sur-provisionnement au tout début, cohérent avec `is_warm`).
const TAIL_MIN_SAMPLES: usize = 32;

/// Nombre de paquets observés avant que l'estimation soit jugée fiable. À ~400
/// paquets/s (frame 2,5 ms), 100 paquets = ~250 ms — assez pour que l'EWMA
/// (gain 1/16) ait convergé (~95 % après 48 échantillons). Avant ce seuil,
/// `is_warm()` renvoie `false` : un consommateur (le jitter buffer) ne doit PAS
/// abaisser sa cible sur une valeur encore sous-estimée (l'EWMA rampe depuis 0).
const WARMUP_PACKETS: u64 = 100;

pub struct JitterEstimator {
    /// Instant d'arrivée du 1er paquet — origine commune pour exprimer les
    /// arrivées dans la même base temporelle que les timestamps RTP.
    first_instant: Option<Instant>,
    /// Délai de transit du paquet précédent (en samples). `None` avant le 1er.
    prev_transit: Option<f64>,
    /// Gigue lissée `J`, en samples (MOYENNE — RFC 3550, télémétrie + warmup).
    jitter_samples: f64,
    /// Fenêtre glissante des délais de transit récents (samples). Base du
    /// percentile de queue. Bornée à `TAIL_WINDOW` (FIFO).
    transit_window: VecDeque<f64>,
    /// Scratch réutilisé pour le tri du percentile (zéro alloc en régime).
    tail_scratch: Vec<f64>,
    /// Queue mise en cache (samples), recalculée tous `TAIL_RECOMPUTE_EVERY`.
    tail_cached_samples: f64,
    /// Nombre de paquets observés (pour le warmup + cadence de recalcul).
    observations: u64,
}

impl JitterEstimator {
    pub fn new() -> Self {
        Self {
            first_instant: None,
            prev_transit: None,
            jitter_samples: 0.0,
            transit_window: VecDeque::with_capacity(TAIL_WINDOW),
            tail_scratch: Vec::with_capacity(TAIL_WINDOW),
            tail_cached_samples: 0.0,
            observations: 0,
        }
    }

    /// À appeler à chaque paquet RTP reçu, dans l'ordre d'arrivée (les paquets
    /// désordonnés produisent un `D` plus grand = vraie gigue, c'est correct).
    ///
    /// * `rtp_ts` — timestamp RTP du paquet (unités samples @48k).
    /// * `instant` — instant d'arrivée local du paquet.
    pub fn observe(&mut self, rtp_ts: u32, instant: Instant) {
        self.observations += 1;
        let first = *self.first_instant.get_or_insert(instant);

        // Arrivée locale exprimée en samples @48k depuis le 1er paquet, dans la
        // même base que les timestamps RTP. f64 garde une précision sous-sample
        // jusqu'à ~24h de session (< 2^52 samples), comme l'estimateur de drift.
        let arrival_samples = instant.duration_since(first).as_secs_f64() * SAMPLE_RATE_HZ;

        // transit = arrivée − émission. L'offset absolu entre les deux horloges
        // est arbitraire mais constant → il disparaît dans D = transit − prev
        // (moyenne) ET dans p95 − p10 (queue). `rtp_ts` peut wrapper (32-bit)
        // mais pas entre 2 paquets consécutifs (espacés de 2,5 ms ; wrap = ~24h).
        let transit = arrival_samples - rtp_ts as f64;

        if let Some(prev) = self.prev_transit {
            let d = (transit - prev).abs();
            // Moyenne EWMA (RFC 3550).
            self.jitter_samples += (d - self.jitter_samples) * EWMA_GAIN;
        }
        self.prev_transit = Some(transit);

        // Fenêtre glissante bornée (FIFO) pour le percentile de queue.
        if self.transit_window.len() == TAIL_WINDOW {
            self.transit_window.pop_front();
        }
        self.transit_window.push_back(transit);

        // Recalcul amorti du percentile (hors chemin par-paquet coûteux).
        if self.observations.is_multiple_of(TAIL_RECOMPUTE_EVERY) {
            self.recompute_tail();
        }
    }

    /// Recalcule la queue = écart inter-percentile (p95 − p10) de la fenêtre de
    /// transit. Trie une copie (scratch réutilisé). Appelé ~12×/s.
    fn recompute_tail(&mut self) {
        if self.transit_window.len() < TAIL_MIN_SAMPLES {
            self.tail_cached_samples = 0.0;
            return;
        }
        self.tail_scratch.clear();
        self.tail_scratch.extend(self.transit_window.iter().copied());
        self.tail_scratch.sort_unstable_by(f64::total_cmp);
        let hi = percentile_of_sorted(&self.tail_scratch, TAIL_P_HIGH);
        let lo = percentile_of_sorted(&self.tail_scratch, TAIL_P_LOW);
        // `max(0)` : plancher numérique (hi ≥ lo par construction).
        self.tail_cached_samples = (hi - lo).max(0.0);
    }

    /// Gigue MOYENNE lissée en millisecondes (RFC 3550). Télémétrie + warmup.
    pub fn jitter_ms(&self) -> f64 {
        self.jitter_samples / SAMPLE_RATE_HZ * 1000.0
    }

    /// Gigue de QUEUE (écart inter-percentile p95 − p10 du transit récent) en
    /// millisecondes. C'est CE signal qui dimensionne le plancher du jitter
    /// buffer : il couvre la lateness soutenue/fréquente sans se laisser épingler
    /// par un stall d'ordonnancement local isolé. Valeur en cache (O(1)).
    pub fn jitter_tail_ms(&self) -> f64 {
        self.tail_cached_samples / SAMPLE_RATE_HZ * 1000.0
    }

    /// `true` une fois l'estimation stabilisée (≥ `WARMUP_PACKETS` paquets).
    /// Tant que `false`, ne pas utiliser `jitter_ms()` pour abaisser une cible.
    pub fn is_warm(&self) -> bool {
        self.observations >= WARMUP_PACKETS
    }
}

impl Default for JitterEstimator {
    fn default() -> Self {
        Self::new()
    }
}

/// Percentile `p` (0..=1) d'un slice **déjà trié** croissant, par index au plus
/// proche. Slice vide → 0.
fn percentile_of_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Flux parfaitement régulier (aucune gigue) → J converge vers 0.
    #[test]
    fn no_jitter_converges_to_zero() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        // 100 paquets espacés exactement de 2,5 ms (120 samples) côté émission
        // ET côté arrivée → D = 0 à chaque pas.
        for i in 0..100u32 {
            let rtp_ts = i * 120;
            let arrival = t0 + Duration::from_micros((i as u64) * 2500);
            est.observe(rtp_ts, arrival);
        }
        assert!(est.jitter_ms() < 0.01, "gigue résiduelle = {}", est.jitter_ms());
    }

    /// Une gigue d'arrivée connue et constante doit ressortir à sa valeur.
    #[test]
    fn constant_arrival_jitter_is_measured() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        // Émission régulière à 2,5 ms ; arrivée alternée ±2 ms autour de la
        // cadence → |D| ≈ 4 ms en régime. L'EWMA doit s'en approcher.
        for i in 0..400u32 {
            let rtp_ts = i * 120;
            let wobble = if i % 2 == 0 { 0i64 } else { 4000 }; // µs
            let arrival =
                t0 + Duration::from_micros((i as u64) * 2500 + wobble.unsigned_abs());
            est.observe(rtp_ts, arrival);
        }
        let j = est.jitter_ms();
        assert!((2.0..6.0).contains(&j), "gigue mesurée hors plage = {j}");
    }

    /// P0 — une lateness RÉCURRENTE (1 paquet sur 10 en retard de 8 ms, soit
    /// 10 % des paquets > seuil 5 %) DOIT monter la queue : c'est un vrai besoin
    /// de buffer que la moyenne EWMA sous-estime. La queue doit dépasser la
    /// moyenne ET capter les ~8 ms.
    #[test]
    fn recurrent_lateness_raises_tail_above_mean() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        for i in 0..600u32 {
            let rtp_ts = i * 120;
            let late_us = if i % 10 == 0 { 8_000 } else { 0 };
            let arrival = t0 + Duration::from_micros((i as u64) * 2500 + late_us);
            est.observe(rtp_ts, arrival);
        }
        let mean = est.jitter_ms();
        let tail = est.jitter_tail_ms();
        assert!(tail > mean, "queue ({tail}) doit dépasser la moyenne ({mean})");
        assert!(tail > 4.0, "la queue doit capter la lateness récurrente ~8 ms, vu = {tail}");
    }

    /// P0 — cœur du fix : un stall d'ordonnancement LOCAL isolé (un unique paquet
    /// horodaté 40 ms en retard) NE DOIT PAS épingler la queue. < 5 % de la
    /// fenêtre → le p95 l'ignore. C'est ce qui pinnait le buffer à 40 ms sur PC.
    #[test]
    fn isolated_local_stall_does_not_pin_tail() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        for i in 0..600u32 {
            let rtp_ts = i * 120;
            // Un seul paquet (i=300) horodaté très en retard, le reste propre.
            let late_us = if i == 300 { 40_000 } else { 0 };
            let arrival = t0 + Duration::from_micros((i as u64) * 2500 + late_us);
            est.observe(rtp_ts, arrival);
        }
        let tail = est.jitter_tail_ms();
        assert!(tail < 3.0, "un spike isolé ne doit pas gonfler la queue, vu = {tail}");
    }

    /// P0 — un stall local BREF (rafale de ~6 paquets groupés, ~1 % de la fenêtre)
    /// reste sous le seuil → la queue ne gonfle pas ; le trou audio réel est
    /// couvert par le filet réactif (hors de cet estimateur).
    #[test]
    fn brief_local_burst_stays_low() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        for i in 0..600u32 {
            let rtp_ts = i * 120;
            // 6 paquets consécutifs horodatés en retard décroissant (catch-up
            // après préemption), une seule fois.
            let late_us = if (300..306).contains(&i) {
                (306 - i) as u64 * 2500
            } else {
                0
            };
            let arrival = t0 + Duration::from_micros((i as u64) * 2500 + late_us);
            est.observe(rtp_ts, arrival);
        }
        let tail = est.jitter_tail_ms();
        assert!(tail < 4.0, "un burst local bref ne doit pas pinner la queue, vu = {tail}");
    }

    /// Flux parfaitement régulier → la queue converge aussi vers ~0 (pas de
    /// sur-provisionnement sur lien propre = pas de régression latence).
    #[test]
    fn tail_stays_low_on_clean_stream() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        for i in 0..200u32 {
            let rtp_ts = i * 120;
            let arrival = t0 + Duration::from_micros((i as u64) * 2500);
            est.observe(rtp_ts, arrival);
        }
        assert!(est.jitter_tail_ms() < 0.05, "queue résiduelle = {}", est.jitter_tail_ms());
    }

    /// P0 — la queue REDESCEND une fois la perturbation passée (récupération
    /// naturelle par la fenêtre glissante, vs l'ancien peak-hold qui restait
    /// épinglé). Après une rafale récurrente puis un long calme, tail → ~0.
    #[test]
    fn tail_recovers_after_disturbance_clears() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        // Phase 1 : lateness récurrente sur 600 paquets → queue élevée.
        for i in 0..600u32 {
            let rtp_ts = i * 120;
            let late_us = if i % 10 == 0 { 8_000 } else { 0 };
            let arrival = t0 + Duration::from_micros((i as u64) * 2500 + late_us);
            est.observe(rtp_ts, arrival);
        }
        assert!(est.jitter_tail_ms() > 4.0, "sanity : queue élevée en phase perturbée");
        // Phase 2 : lien propre sur > 1 fenêtre (600 paquets) → la fenêtre se
        // vide de la perturbation.
        for i in 600..1200u32 {
            let rtp_ts = i * 120;
            let arrival = t0 + Duration::from_micros((i as u64) * 2500);
            est.observe(rtp_ts, arrival);
        }
        assert!(
            est.jitter_tail_ms() < 0.5,
            "la queue doit redescendre après le calme, vu = {}",
            est.jitter_tail_ms()
        );
    }
}
