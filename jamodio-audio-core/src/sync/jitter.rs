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

#[derive(Default)]
pub struct JitterEstimator {
    /// Instant d'arrivée du 1er paquet — origine commune pour exprimer les
    /// arrivées dans la même base temporelle que les timestamps RTP.
    first_instant: Option<Instant>,
    /// Délai de transit du paquet précédent (en samples). `None` avant le 1er.
    prev_transit: Option<f64>,
    /// Gigue lissée `J`, en samples.
    jitter_samples: f64,
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
            self.jitter_samples += (d - self.jitter_samples) * EWMA_GAIN;
        }
        self.prev_transit = Some(transit);
    }

    /// Gigue lissée en millisecondes.
    pub fn jitter_ms(&self) -> f64 {
        self.jitter_samples / SAMPLE_RATE_HZ * 1000.0
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
}
