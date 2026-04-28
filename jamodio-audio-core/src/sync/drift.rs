//! Drift estimator for remote audio streams (T4.2a).
//!
//! Mesure la dérive d'horloge entre la carte son distante (sender) et la
//! nôtre, via la progression des timestamps RTP comparée à notre horloge
//! locale (`Instant`).
//!
//! Pour T4.2a (mesure-only) : log `drift_ppm` toutes les 30s. La compensation
//! par micro-resampling rubato sera branchée en T4.2b une fois les valeurs
//! réelles observées sur sessions longues (>30min).
//!
//! Précision attendue : ±5 ppm après ~1min, ±2 ppm après ~10min (la mesure
//! cumulative se stabilise avec le temps écoulé).

use std::time::Instant;

/// Sample rate de référence (Opus + CPAL Jamodio).
const SAMPLE_RATE_HZ: f64 = 48_000.0;

/// On commence à émettre une estimation après ce délai (avant : trop de bruit).
const WARMUP_SECS: f64 = 5.0;

/// Période de log (en secondes).
const LOG_INTERVAL_SECS: u64 = 30;

pub struct DriftEstimator {
    label: String,
    first_rtp_ts: Option<u32>,
    first_instant: Option<Instant>,
    last_log: Option<Instant>,
    drift_ppm: f64,
    observations: u64,
}

impl DriftEstimator {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            first_rtp_ts: None,
            first_instant: None,
            last_log: None,
            drift_ppm: 0.0,
            observations: 0,
        }
    }

    /// À appeler à chaque paquet RTP reçu (avec son timestamp et l'instant
    /// de réception local). Pas de calcul si pas assez de données.
    pub fn observe(&mut self, rtp_ts: u32, instant: Instant) {
        self.observations += 1;

        if self.first_rtp_ts.is_none() {
            self.first_rtp_ts = Some(rtp_ts);
            self.first_instant = Some(instant);
            self.last_log = Some(instant);
            return;
        }

        let first_rtp = self.first_rtp_ts.unwrap();
        let first_inst = self.first_instant.unwrap();
        let elapsed_secs = instant.duration_since(first_inst).as_secs_f64();

        if elapsed_secs < WARMUP_SECS {
            return;
        }

        // RTP timestamp wrap-around : 32-bit @ 48kHz = ~24h, on ne le gère
        // pas (sessions longues à venir mais < 24h pour l'instant).
        let rtp_advance = rtp_ts.wrapping_sub(first_rtp) as f64;
        let expected = elapsed_secs * SAMPLE_RATE_HZ;
        if expected <= 0.0 {
            return;
        }

        // drift_ppm > 0  → sender plus rapide que nous (RTP avance vite)
        // drift_ppm < 0  → sender plus lent
        self.drift_ppm = (rtp_advance / expected - 1.0) * 1e6;

        let last_log = self.last_log.unwrap();
        if instant.duration_since(last_log).as_secs() >= LOG_INTERVAL_SECS {
            eprintln!(
                "[DRIFT:{}] {:+.1} ppm after {:.0}s ({} packets)",
                self.label, self.drift_ppm, elapsed_secs, self.observations,
            );
            self.last_log = Some(instant);
        }
    }

    pub fn drift_ppm(&self) -> f64 {
        self.drift_ppm
    }
}
