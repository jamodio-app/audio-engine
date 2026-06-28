//! Interarrival jitter estimator (RFC 3550 §A.8) for remote audio streams.
//!
//! Mesure la **gigue réseau réelle** d'un flux entrant : la variation du délai
//! de transit entre paquets consécutifs, lissée par une moyenne mobile
//! exponentielle (EWMA). C'est le *capteur* du chantier jitter buffer adaptatif :
//! en Phase B, la cible du buffer sera dimensionnée sur cette mesure
//! (`target ≈ k·jitter`) au lieu d'une valeur fixe réactive.
//!
//! Algorithme (RFC 3550 §A.8), pour chaque paquet reçu :
//!   transit = arrivée_locale − timestamp_RTP        (en samples @48k)
//!   D       = transit − transit_précédent           (variation du délai)
//!   J      += (|D| − J) / 16                         (EWMA, gain 1/16)
//!
//! `J` est exprimé en samples ; `jitter_ms()` le convertit en millisecondes.
//! La valeur absolue de `transit` est sans intérêt (offset arbitraire entre les
//! deux horloges) ; seule sa **variation** D porte l'information de gigue, donc
//! l'offset se simplifie dans la soustraction.
//!
//! Indépendant de l'OS (calcul pur) : comportement identique macOS / Windows.

use std::time::Instant;

/// Sample rate de référence (Opus + CPAL Jamodio).
const SAMPLE_RATE_HZ: f64 = 48_000.0;

/// Gain de l'EWMA RFC 3550. 1/16 = compromis standard réactivité/stabilité.
const EWMA_GAIN: f64 = 1.0 / 16.0;

/// Chantier #1 — gain de RELEASE de l'estimateur de QUEUE (peak-hold).
/// Attaque instantanée (`|D|` plus grand → la queue saute dessus), release lent
/// (la queue redescend doucement vers la gigue courante). À ~400 paquets/s,
/// 1/1600 ≈ constante de temps ~4 s : la cible reste dimensionnée pour le pire
/// cas RÉCENT, puis se détend quand le lien se calme. CONSTANTE DE CALIBRATION.
const TAIL_RELEASE_GAIN: f64 = 1.0 / 1600.0;

/// Nombre de paquets observés avant que l'estimation soit jugée fiable. À ~400
/// paquets/s (frame 2,5 ms), 100 paquets = ~250 ms — assez pour que l'EWMA
/// (gain 1/16) ait convergé (~95 % après 48 échantillons). Avant ce seuil,
/// `is_warm()` renvoie `false` : un consommateur (le jitter buffer) ne doit PAS
/// abaisser sa cible sur une valeur encore sous-estimée (l'EWMA rampe depuis 0).
const WARMUP_PACKETS: u64 = 100;

#[derive(Default)]
pub struct JitterEstimator {
    /// Instant d'arrivée du 1er paquet — origine commune pour exprimer les
    /// arrivées dans la même base temporelle que les timestamps RTP.
    first_instant: Option<Instant>,
    /// Délai de transit du paquet précédent (en samples). `None` avant le 1er.
    prev_transit: Option<f64>,
    /// Gigue lissée `J`, en samples (MOYENNE — RFC 3550, télémétrie + warmup).
    jitter_samples: f64,
    /// Chantier #1 — estimateur de QUEUE de `|D|` (peak-hold attaque rapide /
    /// release lent), en samples. Capte le pire-cas RÉCENT de variation de
    /// transit (≈ p~max sur ~4 s), pas la moyenne. C'est lui qui doit piloter le
    /// plancher du jitter buffer : sur réseau bursty la queue est plusieurs × la
    /// moyenne → dimensionner sur la moyenne fait underrun-puis-réagir.
    jitter_tail_samples: f64,
    /// Nombre de paquets observés (pour le warmup).
    observations: u64,
}

impl JitterEstimator {
    pub fn new() -> Self {
        Self::default()
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
        // est arbitraire mais constant → il disparaît dans D = transit − prev.
        // `rtp_ts` peut wrapper (32-bit) mais pas entre 2 paquets consécutifs
        // (espacés de 2,5 ms ; wrap = ~24h), donc D reste exact.
        let transit = arrival_samples - rtp_ts as f64;

        if let Some(prev) = self.prev_transit {
            let d = (transit - prev).abs();
            // Moyenne EWMA (RFC 3550).
            self.jitter_samples += (d - self.jitter_samples) * EWMA_GAIN;
            // Queue (peak-hold) : attaque instantanée, release lent.
            if d > self.jitter_tail_samples {
                self.jitter_tail_samples = d;
            } else {
                self.jitter_tail_samples += (d - self.jitter_tail_samples) * TAIL_RELEASE_GAIN;
            }
        }
        self.prev_transit = Some(transit);
    }

    /// Gigue MOYENNE lissée en millisecondes (RFC 3550). Télémétrie + warmup.
    pub fn jitter_ms(&self) -> f64 {
        self.jitter_samples / SAMPLE_RATE_HZ * 1000.0
    }

    /// Chantier #1 — gigue de QUEUE (pire-cas récent de `|D|`) en millisecondes.
    /// ≥ `jitter_ms()`. C'est CE signal qui dimensionne le plancher du jitter
    /// buffer (proactif sur la queue, pas réactif sur la moyenne).
    pub fn jitter_tail_ms(&self) -> f64 {
        self.jitter_tail_samples / SAMPLE_RATE_HZ * 1000.0
    }

    /// `true` une fois l'estimation stabilisée (≥ `WARMUP_PACKETS` paquets).
    /// Tant que `false`, ne pas utiliser `jitter_ms()` pour abaisser une cible.
    pub fn is_warm(&self) -> bool {
        self.observations >= WARMUP_PACKETS
    }
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

    /// Chantier #1 — flux globalement régulier MAIS avec des rafales rares :
    /// la QUEUE doit ressortir bien au-dessus de la MOYENNE (c'est tout l'intérêt
    /// — dimensionner sur la queue plutôt que sur la moyenne sous-estimée).
    #[test]
    fn tail_exceeds_mean_on_bursty_stream() {
        let mut est = JitterEstimator::new();
        let t0 = Instant::now();
        // Émission régulière 2,5 ms ; arrivée régulière SAUF 1 paquet sur 50 qui
        // arrive 15 ms en retard (rafale). La moyenne EWMA reste petite (rare),
        // la queue (peak-hold) doit capter les ~15 ms.
        for i in 0..600u32 {
            let rtp_ts = i * 120;
            let late_us = if i % 50 == 25 { 15_000 } else { 0 };
            let arrival = t0 + Duration::from_micros((i as u64) * 2500 + late_us);
            est.observe(rtp_ts, arrival);
        }
        let mean = est.jitter_ms();
        let tail = est.jitter_tail_ms();
        assert!(tail > mean, "queue ({tail}) doit dépasser la moyenne ({mean})");
        assert!(tail > 5.0, "la queue doit capter la rafale ~15 ms, vu = {tail}");
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
}
