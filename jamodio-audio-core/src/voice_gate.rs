//! voice_gate — noise-gate présence-voix (talkback).
//!
//! Portage **1:1** du cœur JS `web/app/js/lib/groupe-audio/voice-gate-core.js`
//! (mode browser) → parité de comportement browser ↔ agent (spec unique :
//! `internal-docs/plans/PLAN-TALKBACK-GATE-2026-08.md` §3).
//!
//! Racine du chantier : le side-chain est la PRÉSENCE DE VOIX (bande-parole
//! ~300–3400 Hz), pas le niveau instrument. Auto-réparant : fail-open +
//! calibration continue asymétrique (suivi de minimum) + override voix soutenue.
//!
//! Pur (aucune I/O, aucun état global) → testable et réutilisable. Le side-chain
//! attendu par [`VoiceGate::process`] est le RMS d'un signal DÉJÀ filtré
//! bande-voix — voir [`SidechainBandpass`].

/// Paramètres du gate (mêmes valeurs que le cœur JS). Surchageables pour tuning.
#[derive(Clone, Copy, Debug)]
pub struct VoiceGateParams {
    /// Seuil d'OUVERTURE = plancher + offset (dB).
    pub open_offset_db: f32,
    /// Seuil de FERMETURE (hystérésis) = plancher + offset (dB).
    pub close_offset_db: f32,
    /// Montée du gain à l'ouverture (ms).
    pub attack_ms: f32,
    /// Maintien après dernière détection — ponte les silences inter-mots (ms).
    pub hold_ms: f32,
    /// Descente du gain à la fermeture (ms).
    pub release_ms: f32,
    /// EMA du plancher quand le niveau DESCEND — suivi rapide du silence (ms).
    pub calib_down_ms: f32,
    /// EMA du plancher quand le niveau MONTE — lent, n'avale pas une voix douce (ms).
    pub calib_up_ms: f32,
    /// Durée de voix soutenue → override (force l'ouverture + ré-apprend) (ms).
    pub sustained_ms: f32,
    /// Au-dessus de plancher+offset en continu → compte comme voix soutenue (dB).
    pub sustained_offset_db: f32,
    /// Plancher initial prudent-bas (FAIL-OPEN au démarrage) (dB).
    pub initial_floor_db: f32,
    /// Plafond du plancher appris (dB).
    pub floor_max_db: f32,
    /// Plancher minimal appris (dB).
    pub floor_min_db: f32,
}

impl Default for VoiceGateParams {
    fn default() -> Self {
        Self {
            open_offset_db: 12.0,
            close_offset_db: 6.0,
            attack_ms: 8.0,
            hold_ms: 280.0,
            release_ms: 220.0,
            calib_down_ms: 300.0,
            calib_up_ms: 8000.0,
            sustained_ms: 1600.0,
            sustained_offset_db: 6.0,
            initial_floor_db: -68.0,
            floor_max_db: -30.0,
            floor_min_db: -90.0,
        }
    }
}

/// Machine du noise-gate. Appeler [`process`](Self::process) bloc par bloc.
pub struct VoiceGate {
    p: VoiceGateParams,
    floor_db: f32,
    open: bool,
    gain: f32,
    hold_left: f32,
    sust_acc: f32,
}

impl VoiceGate {
    pub fn new(p: VoiceGateParams) -> Self {
        let floor = p.initial_floor_db;
        Self { p, floor_db: floor, open: false, gain: 0.0, hold_left: 0.0, sust_acc: 0.0 }
    }

    fn to_db(rms: f32) -> f32 {
        if rms <= 1e-7 {
            return -140.0;
        }
        let db = 20.0 * rms.log10();
        if db < -140.0 { -140.0 } else { db }
    }

    /// Coefficient EMA one-pole pour une constante de temps `tau` (ms) sur `dt`.
    fn alpha(dt_ms: f32, tau_ms: f32) -> f32 {
        if tau_ms <= 0.0 {
            return 1.0;
        }
        1.0 - (-dt_ms / tau_ms).exp()
    }

    /// Traite un bloc : `sidechain_rms` = RMS bande-voix du bloc, `dt_ms` = durée
    /// du bloc. Renvoie `(gain 0..1 lissé, open)`.
    pub fn process(&mut self, sidechain_rms: f32, dt_ms: f32) -> (f32, bool) {
        let dt = if dt_ms > 0.0 { dt_ms } else { 0.0 };
        let lvl = Self::to_db(sidechain_rms);
        let open_thresh = self.floor_db + self.p.open_offset_db;
        let close_thresh = self.floor_db + self.p.close_offset_db;

        // ── Override « voix soutenue » (auto-réparation) ──
        if lvl > self.floor_db + self.p.sustained_offset_db {
            self.sust_acc += dt;
            if self.sust_acc >= self.p.sustained_ms {
                self.open = true;
                self.hold_left = self.p.hold_ms;
                let target = lvl - self.p.open_offset_db - 2.0;
                if target < self.floor_db {
                    self.floor_db = target.max(self.p.floor_min_db);
                }
            }
        } else {
            self.sust_acc = 0.0;
        }

        // ── Machine d'états seuil + hystérésis + hold ──
        if lvl > open_thresh {
            self.open = true;
            self.hold_left = self.p.hold_ms;
        } else if self.open {
            if lvl > close_thresh {
                self.hold_left = self.p.hold_ms;
            } else {
                self.hold_left -= dt;
                if self.hold_left <= 0.0 {
                    self.open = false;
                }
            }
        }

        // ── Calibration continue asymétrique (suivi de minimum) ──
        if !self.open && lvl < open_thresh {
            let tau = if lvl < self.floor_db { self.p.calib_down_ms } else { self.p.calib_up_ms };
            let a = Self::alpha(dt, tau);
            self.floor_db += a * (lvl - self.floor_db);
            self.floor_db = self.floor_db.clamp(self.p.floor_min_db, self.p.floor_max_db);
        }

        // ── Enveloppe de gain (attack / release) ──
        let target = if self.open { 1.0 } else { 0.0 };
        let tau = if target > self.gain { self.p.attack_ms } else { self.p.release_ms };
        self.gain += Self::alpha(dt, tau) * (target - self.gain);
        self.gain = self.gain.clamp(0.0, 1.0);

        (self.gain, self.open)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn noise_floor_db(&self) -> f32 {
        self.floor_db
    }

    pub fn reset(&mut self) {
        self.floor_db = self.p.initial_floor_db;
        self.open = false;
        self.gain = 0.0;
        self.hold_left = 0.0;
        self.sust_acc = 0.0;
    }
}

/// Band-pass biquad (RBJ) mono pour le side-chain bande-voix (~300–3400 Hz).
/// Centre 1 kHz, Q 0.5 — mêmes coefficients que le worklet browser.
pub struct SidechainBandpass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl SidechainBandpass {
    pub fn new(sample_rate: f32) -> Self {
        let f0 = 1000.0f32;
        let q = 0.5f32;
        let w0 = 2.0 * std::f32::consts::PI * f0 / sample_rate;
        let (sw, cw) = (w0.sin(), w0.cos());
        let al = sw / (2.0 * q);
        let a0 = 1.0 + al;
        Self {
            b0: al / a0,
            b1: 0.0,
            b2: -al / a0,
            a1: (-2.0 * cw) / a0,
            a2: (1.0 - al) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// RMS bande-voix d'un bloc mono (fait avancer l'état du filtre).
    pub fn block_rms(&mut self, block: &[f32]) -> f32 {
        if block.is_empty() {
            return 0.0;
        }
        let mut sum = 0.0f32;
        for &s in block {
            let y = self.process(s);
            sum += y * y;
        }
        (sum / block.len() as f32).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // dBFS → RMS linéaire.
    fn rms(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    // Alimente le gate `ms` ms à niveau constant `db` (pas de 10 ms).
    fn feed(g: &mut VoiceGate, db: f32, ms: f32) -> (f32, bool) {
        let dt = 10.0;
        let n = (ms / dt).round() as i32;
        let mut out = (0.0, false);
        for _ in 0..n {
            out = g.process(rms(db), dt);
        }
        out
    }

    #[test]
    fn silence_stays_closed() {
        let mut g = VoiceGate::new(Default::default());
        let (gain, open) = feed(&mut g, -80.0, 1000.0);
        assert!(!open);
        assert!(gain < 0.02, "gain ~0 attendu, obtenu {gain}");
    }

    #[test]
    fn voice_opens_and_rises() {
        let mut g = VoiceGate::new(Default::default());
        let (_, open) = g.process(rms(-30.0), 10.0);
        assert!(open);
        let (gain, _) = feed(&mut g, -30.0, 60.0);
        assert!(gain > 0.95, "gain ~1 attendu, obtenu {gain}");
    }

    #[test]
    fn fail_open_initial_floor_permissive() {
        let mut g = VoiceGate::new(Default::default());
        let (_, open) = g.process(rms(-52.0), 10.0); // -52 > openThresh initial (-56)
        assert!(open);
    }

    #[test]
    fn hysteresis_stays_open_between_thresholds() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -30.0, 100.0);
        let (_, open) = feed(&mut g, -60.0, 600.0); // -60 > closeThresh (-62)
        assert!(open);
    }

    #[test]
    fn closes_after_hold() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -30.0, 100.0);
        let (_, open) = feed(&mut g, -90.0, 400.0);
        assert!(!open);
    }

    #[test]
    fn calibration_tracks_lower_silence() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -85.0, 1000.0);
        assert!(g.noise_floor_db() < -80.0, "plancher={}", g.noise_floor_db());
    }

    #[test]
    fn sustained_override_forces_open() {
        let mut g = VoiceGate::new(Default::default());
        let (_, before) = feed(&mut g, -58.0, 1200.0); // sous openThresh, > floor+6
        assert!(!before);
        let (_, after) = feed(&mut g, -58.0, 700.0); // ~1.9 s soutenu
        assert!(after, "override doit forcer l'ouverture");
        assert!(g.noise_floor_db() < -68.0, "plancher ré-appris vers le bas");
    }

    #[test]
    fn isolated_peak_does_not_trigger_override() {
        let mut g = VoiceGate::new(Default::default());
        g.process(rms(-58.0), 10.0);
        feed(&mut g, -90.0, 500.0);
        assert!(!g.is_open());
    }

    #[test]
    fn reset_restores_initial() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -30.0, 200.0);
        g.reset();
        assert!(!g.is_open());
        assert_eq!(g.noise_floor_db(), -68.0);
    }

    #[test]
    fn bandpass_passes_speech_more_than_sub_bass() {
        // Une sinus 1 kHz (bande voix) doit ressortir plus fort qu'une 60 Hz
        // (corps grave d'un ampli) à amplitude égale.
        let fs = 48000.0f32;
        let block: Vec<f32> = (0..4800)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / fs).sin())
            .collect();
        let sub: Vec<f32> = (0..4800)
            .map(|i| (2.0 * std::f32::consts::PI * 60.0 * i as f32 / fs).sin())
            .collect();
        let mut bp1 = SidechainBandpass::new(fs);
        let mut bp2 = SidechainBandpass::new(fs);
        let speech = bp1.block_rms(&block);
        let bass = bp2.block_rms(&sub);
        assert!(speech > bass * 3.0, "voix {speech} vs grave {bass}");
    }
}
