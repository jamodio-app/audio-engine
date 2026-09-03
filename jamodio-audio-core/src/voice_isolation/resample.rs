//! Décimation **48 kHz → 16 kHz** (facteur 3 entier) pour alimenter le VAD.
//! Le VAD travaille en 16 kHz ; le talkback est en 48 kHz. On ne rééchantillonne
//! que dans ce sens (le VAD ne produit qu'une décision, pas d'audio).
//!
//! Anti-repliement par un **passe-bas biquad** (RBJ, ~7 kHz < Nyquist 16k = 8 kHz)
//! AVANT de garder un échantillon sur trois. Pur, déterministe, sans allocation
//! dans le chemin chaud.

use std::collections::VecDeque;
use std::f32::consts::PI;

/// Biquad direct-form I (coefficients RBJ passe-bas).
#[derive(Debug, Clone)]
struct Biquad {
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

impl Biquad {
    /// Passe-bas RBJ à `fc` Hz, facteur de qualité `q`, à la fréquence `fs`.
    fn lowpass(fs: f32, fc: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * fc / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b1 = 1.0 - cos;
        let b0 = b1 / 2.0;
        let b2 = b0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Décimateur 48 k → 16 k (facteur 3) avec anti-repliement.
pub struct Decimator3 {
    lp: Biquad,
    phase: u8,
}

impl Decimator3 {
    /// `sr_in` doit valoir 48000. Coupure ~7 kHz (marge sous Nyquist 16k).
    pub fn new(sr_in: f32) -> Self {
        debug_assert_eq!(sr_in as u32, 48_000);
        Self { lp: Biquad::lowpass(sr_in, 7_000.0, 0.707), phase: 0 }
    }

    /// Filtre `input` (48k) et pousse les échantillons décimés (16k) dans `sink`.
    pub fn process_into(&mut self, input: &[f32], sink: &mut VecDeque<f32>) {
        for &x in input {
            let y = self.lp.process(x);
            if self.phase == 0 {
                sink.push_back(y);
            }
            self.phase = (self.phase + 1) % 3;
        }
    }

    pub fn reset(&mut self) {
        self.lp.reset();
        self.phase = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_trois_en_sortie() {
        let mut d = Decimator3::new(48_000.0);
        let mut sink = VecDeque::new();
        d.process_into(&vec![0.0; 3000], &mut sink);
        // 3000 éch @48k → ~1000 éch @16k.
        assert!((sink.len() as i32 - 1000).abs() <= 1, "sortie={}", sink.len());
    }

    #[test]
    fn attenue_les_hautes_frequences() {
        // Un signal à ~20 kHz (au-dessus de Nyquist 16k) doit être fortement atténué
        // (sinon il se replierait dans la bande utile).
        let fs = 48_000.0;
        let f = 20_000.0;
        let n = 48_000;
        let sig: Vec<f32> = (0..n).map(|i| (2.0 * PI * f * i as f32 / fs).sin()).collect();
        let mut d = Decimator3::new(fs);
        let mut sink = VecDeque::new();
        d.process_into(&sig, &mut sink);
        let rms: f32 = (sink.iter().map(|x| x * x).sum::<f32>() / sink.len() as f32).sqrt();
        assert!(rms < 0.1, "20 kHz doit être atténué, rms sortie={rms}");
    }

    #[test]
    fn laisse_passer_les_basses() {
        // Un signal à 500 Hz (voix) doit passer sans atténuation notable.
        let fs = 48_000.0;
        let f = 500.0;
        let n = 48_000;
        let sig: Vec<f32> = (0..n).map(|i| (2.0 * PI * f * i as f32 / fs).sin()).collect();
        let mut d = Decimator3::new(fs);
        let mut sink = VecDeque::new();
        d.process_into(&sig, &mut sink);
        let rms: f32 = (sink.iter().map(|x| x * x).sum::<f32>() / sink.len() as f32).sqrt();
        // sinus d'amplitude 1 → rms ~0.707 ; on tolère large.
        assert!(rms > 0.5, "500 Hz doit passer, rms sortie={rms}");
    }
}
