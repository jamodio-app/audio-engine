//! Orchestrateur — enchaîne **denoise → VAD (sur la voix nettoyée) → gate** sur le
//! canal talkback. Point d'entrée unique du sous-système, consommé par `pipeline.rs`.
//!
//! Sortie = voix nettoyée quand on parle, **silence total** sinon. Renvoie l'état
//! « voix active » (pour le voyant de la tranche). Aucun accès réseau, aucun modèle
//! rechargé au fil de l'eau ; tout est instancié à `new()`.

use std::collections::VecDeque;

use super::denoise::Denoiser;
use super::gate::{GateParams, VoiceGate};
use super::resample::Decimator3;
use super::vad::{Vad, VAD_FRAME};
use super::IsolationError;

/// Fréquence du canal talkback (l'isolation n'opère qu'à 48 kHz).
pub const SAMPLE_RATE: u32 = 48_000;

/// Réglages de l'isolation (poussés depuis l'UI, plan §8).
#[derive(Debug, Clone, Copy)]
pub struct IsolationConfig {
    /// Seuil de proba VAD au-delà duquel on considère qu'il y a de la parole.
    pub vad_threshold: f32,
    /// Ballistique du gate.
    pub gate: GateParams,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self { vad_threshold: 0.5, gate: GateParams::default() }
    }
}

/// État renvoyé à chaque bloc (pour le voyant « à l'antenne »).
#[derive(Debug, Clone, Copy)]
pub struct VoiceState {
    /// Le gate est ouvert (on parle / dans le hangover).
    pub voice_active: bool,
}

pub struct VoiceIsolator {
    denoiser: Denoiser,
    decimator: Decimator3,
    vad: Vad,
    gate: VoiceGate,
    /// Échantillons 16 kHz en attente d'une trame VAD complète.
    vad_accum: VecDeque<f32>,
    /// Tampon de trame VAD (préalloué).
    vad_frame: Vec<f32>,
    /// Gains de gate par-échantillon (préalloué, redimensionné une fois).
    gate_gains: Vec<f32>,
    /// Dernière décision VAD (parole présente).
    speech: bool,
}

impl VoiceIsolator {
    /// Instancie toute la chaîne (charge les deux modèles). **Erreur explicite** si
    /// un modèle ne charge pas (l'appelant bascule alors en talkback brut + UI).
    pub fn new(cfg: IsolationConfig) -> Result<Self, IsolationError> {
        let denoiser = Denoiser::new()?;
        debug_assert_eq!(
            denoiser.sample_rate() as u32,
            SAMPLE_RATE,
            "le denoise doit être en 48 kHz (notre pipeline)"
        );
        let vad = Vad::new(cfg.vad_threshold)?;
        Ok(Self {
            denoiser,
            decimator: Decimator3::new(SAMPLE_RATE as f32),
            vad,
            gate: VoiceGate::new(SAMPLE_RATE as f32, cfg.gate),
            vad_accum: VecDeque::with_capacity(VAD_FRAME * 4),
            vad_frame: vec![0.0; VAD_FRAME],
            gate_gains: Vec::new(),
            speech: false,
        })
    }

    /// Traite un bloc voix mono 48 kHz **en place**. Ordre : denoise → VAD sur la
    /// voix nettoyée → gate. Renvoie l'état voix.
    pub fn process_block(&mut self, block: &mut [f32]) -> Result<VoiceState, IsolationError> {
        // 1) Débruitage (in place) → voix nettoyée (retardée de la latence modèle).
        self.denoiser.process_block(block)?;

        // 2) VAD sur la voix NETTOYÉE (instrument déjà retiré → décision fiable) :
        //    décime en 16 kHz, accumule, décide dès qu'une trame est complète.
        self.decimator.process_into(block, &mut self.vad_accum);
        while self.vad_accum.len() >= VAD_FRAME {
            for slot in self.vad_frame.iter_mut() {
                *slot = self.vad_accum.pop_front().expect("trame complète garantie par la condition");
            }
            self.speech = self.vad.is_speech(&self.vad_frame)?;
        }

        // 3) Gate : gain lissé par-échantillon → silence total hors parole.
        if self.gate_gains.len() != block.len() {
            self.gate_gains.resize(block.len(), 0.0);
        }
        self.gate.process_block(self.speech, &mut self.gate_gains);
        for (s, g) in block.iter_mut().zip(self.gate_gains.iter()) {
            *s *= *g;
        }

        Ok(VoiceState { voice_active: self.gate.is_open() })
    }

    /// Réinitialise toute la chaîne (à (ré)ouverture capture / hot-swap device).
    pub fn reset(&mut self) {
        self.denoiser.reset();
        self.decimator.reset();
        self.vad.reset();
        self.gate.reset();
        self.vad_accum.clear();
        self.speech = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOP: usize = 480; // taille de bloc typique (10 ms @48k)

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Bruit blanc déterministe (LCG) — pas de dépendance `rand`.
    fn noise(n: usize, amp: f32) -> Vec<f32> {
        let mut s: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 2.0 * amp
            })
            .collect()
    }

    #[test]
    fn silence_donne_silence_et_voix_inactive() {
        let mut iso = VoiceIsolator::new(IsolationConfig::default()).unwrap();
        let mut last = VoiceState { voice_active: true };
        let mut tail = vec![0.0f32; HOP];
        // ~1 s de silence (dépasse largement l'amorçage).
        for _ in 0..100 {
            tail = vec![0.0f32; HOP];
            last = iso.process_block(&mut tail).unwrap();
        }
        assert!(!last.voice_active, "silence ⇒ voix inactive");
        assert!(rms(&tail) < 1e-3, "silence ⇒ sortie ~nulle, rms={}", rms(&tail));
    }

    #[test]
    fn bruit_non_vocal_est_fortement_attenue() {
        // Un bruit large bande n'est pas de la parole → le gate doit se fermer →
        // sortie très atténuée (la propriété « je joue = rien ne repisse »).
        let mut iso = VoiceIsolator::new(IsolationConfig::default()).unwrap();
        let mut out_rms = 1.0;
        for _ in 0..150 {
            let mut b = noise(HOP, 0.3);
            iso.process_block(&mut b).unwrap();
            out_rms = rms(&b); // dernier bloc
        }
        assert!(out_rms < 0.05, "bruit non vocal ⇒ sortie fortement atténuée, rms={out_rms}");
    }

    #[test]
    fn reset_ok() {
        let mut iso = VoiceIsolator::new(IsolationConfig::default()).unwrap();
        let mut b = noise(HOP, 0.3);
        iso.process_block(&mut b).unwrap();
        iso.reset();
        assert!(iso.vad_accum.is_empty() && !iso.speech);
    }
}
