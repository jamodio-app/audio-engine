//! voice_gate — noise-gate présence-voix (talkback).
//!
//! Portage **1:1** du cœur JS `web/app/js/lib/groupe-audio/voice-gate-core.js`
//! (mode browser) → parité de comportement browser ↔ agent (spec unique :
//! `internal-docs/plans/PLAN-TALKBACK-GATE-2026-08.md`).
//!
//! ── Principe (suivi de MINIMUM adaptatif) ──
//! Le plancher de bruit suit le MINIMUM du niveau bande-voix : il DESCEND vite
//! (calib_down) vers le vrai silence, REMONTE lentement (calib_up), et tourne EN
//! PERMANENCE (même gate ouvert). Conséquences :
//! - démarrage : plancher initial HAUT → gate FERMÉ, puis convergence descendante
//!   rapide → marche quels que soient les niveaux absolus ;
//! - un bruit STEADY (ambiance plate / bleed) fait remonter le plancher jusqu'à
//!   REFERMER le gate → pas de « toujours ON AIR » (anti-deadlock) ;
//! - la VOIX est DYNAMIQUE : ses creux inter-mots (down-fast) gardent le plancher
//!   au niveau du silence → elle ouvre franchement et n'est pas avalée.
//!
//! Pur (aucune I/O) → testable et réutilisable. Le side-chain attendu par
//! [`VoiceGate::process`] est le RMS d'un signal DÉJÀ filtré bande-voix — voir
//! [`SidechainBandpass`].

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
    /// EMA du plancher quand le niveau MONTE — lent, n'avale pas la voix (ms).
    pub calib_up_ms: f32,
    /// Plancher initial HAUT → gate FERMÉ au démarrage, convergence descendante (dB).
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
            calib_down_ms: 150.0,
            calib_up_ms: 4000.0,
            initial_floor_db: -20.0,
            floor_max_db: -18.0,
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
}

impl VoiceGate {
    pub fn new(p: VoiceGateParams) -> Self {
        let floor = p.initial_floor_db;
        Self { p, floor_db: floor, open: false, gain: 0.0, hold_left: 0.0 }
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

        // ── Calibration : suivi de MINIMUM adaptatif (TOUJOURS actif) ──
        {
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

    fn rms(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

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
    fn starts_closed() {
        let mut g = VoiceGate::new(Default::default());
        assert!(!g.process(rms(-30.0), 10.0).1, "démarre fermé (plancher haut)");
    }

    #[test]
    fn silence_converges_and_closed() {
        let mut g = VoiceGate::new(Default::default());
        let (gain, open) = feed(&mut g, -70.0, 800.0);
        assert!(!open);
        assert!(g.noise_floor_db() < -60.0, "plancher={}", g.noise_floor_db());
        assert!(gain < 0.02);
    }

    #[test]
    fn voice_opens_after_calibration() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -65.0, 800.0);
        let (gain, open) = feed(&mut g, -30.0, 80.0);
        assert!(open);
        assert!(gain > 0.9, "gain={gain}");
    }

    // Régression terrain : « toujours ON AIR ».
    #[test]
    fn steady_ambient_does_not_stay_open() {
        let mut g = VoiceGate::new(Default::default());
        assert!(!feed(&mut g, -45.0, 3000.0).1, "ambiance -45 doit rester fermée");
        let mut g2 = VoiceGate::new(Default::default());
        assert!(!feed(&mut g2, -35.0, 3000.0).1, "ambiance -35 doit rester fermée");
    }

    #[test]
    fn voice_then_ambient_recloses() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -60.0, 500.0);
        feed(&mut g, -25.0, 500.0);
        assert!(g.is_open());
        assert!(!feed(&mut g, -45.0, 6000.0).1, "doit se refermer, pas stuck ON AIR");
    }

    #[test]
    fn hysteresis_stays_open_between_thresholds() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -65.0, 800.0);
        feed(&mut g, -30.0, 100.0);
        assert!(feed(&mut g, -56.0, 500.0).1, "-56 entre close et open → reste ouvert");
    }

    #[test]
    fn hold_bridges_short_dip() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -65.0, 800.0);
        feed(&mut g, -30.0, 100.0);
        feed(&mut g, -90.0, 150.0);
        assert!(g.is_open(), "le hold doit maintenir ouvert");
    }

    #[test]
    fn closes_after_hold() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -65.0, 800.0);
        feed(&mut g, -30.0, 100.0);
        assert!(!feed(&mut g, -95.0, 400.0).1);
    }

    #[test]
    fn reset_restores_initial() {
        let mut g = VoiceGate::new(Default::default());
        feed(&mut g, -30.0, 200.0);
        g.reset();
        assert!(!g.is_open());
        assert_eq!(g.noise_floor_db(), -20.0);
    }

    #[test]
    fn bandpass_passes_speech_more_than_sub_bass() {
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
