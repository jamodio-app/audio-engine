//! Orchestrateur — enchaîne **denoise → VAD (sur la voix nettoyée) → gate** sur le
//! canal talkback. Point d'entrée unique du sous-système, consommé par `pipeline.rs`.
//!
//! Sortie = voix nettoyée quand on parle, **silence total** sinon. Renvoie l'état
//! « voix active » (pour le voyant de la tranche). Aucun accès réseau, aucun modèle
//! rechargé au fil de l'eau ; tout est instancié à `new()`.
//!
//! **Lookahead (indispensable, pas un confort).** Le VAD ne rend sa décision qu'à la
//! FIN de sa trame de 32 ms, et il lui faut parfois deux trames pour reconnaître une
//! attaque douce. Sans retard, cette décision s'appliquerait à des échantillons DÉJÀ
//! partis : le début de chaque mot sort atténué (mesuré sur prises réelles : 48
//! attaques sur 150 perdaient plus de 30 ms — c'est le « ça coupe » du terrain).
//! On retarde donc la voix de [`IsolationConfig::lookahead_ms`] AVANT le gate, si
//! bien que le gain est appliqué aux échantillons qui ont réellement été analysés.
//! Ce retard est le prix EXPLICITE de l'attaque propre ; il ne concerne que le canal
//! talkback, jamais le monitoring instrument.

use std::collections::VecDeque;

use super::denoise::Denoiser;
use super::gate::{GateParams, VoiceGate};
use super::resample::Decimator3;
use super::vad::{Vad, VAD_FRAME};
use super::IsolationError;

/// Fréquence du canal talkback (l'isolation n'opère qu'à 48 kHz).
pub const SAMPLE_RATE: u32 = 48_000;

/// Réglages de l'isolation (poussés depuis l'UI, plan §8).
///
/// Les valeurs par défaut sont **mesurées**, pas devinées : banc
/// `cargo run --release --example iso_offline` sur les prises réelles (voix +
/// guitare, un seul micro).
#[derive(Debug, Clone, Copy)]
pub struct IsolationConfig {
    /// Proba VAD à partir de laquelle on OUVRE le gate. Ne pas descendre sous 0.5 :
    /// à 0.35, la repisse d'instrument suffit à ouvrir (mesuré : 6,8 % du temps de
    /// jeu, fuite à −56 dBFS) et la règle produit « je joue, rien ne sort » tombe.
    pub vad_open_threshold: f32,
    /// Proba VAD en dessous de laquelle on referme, une fois OUVERT (hystérésis).
    /// Plus bas que l'ouverture : évite le papillotement sur les consonnes sourdes.
    pub vad_close_threshold: f32,
    /// Retard appliqué à la voix avant le gate (cf. doc de module). 96 ms ⇒ 1 attaque
    /// de mot sur 150 encore rognée ; 64 ms ⇒ 14 sur 150 ; 0 ⇒ 48 sur 150.
    pub lookahead_ms: f32,
    /// Ballistique du gate.
    pub gate: GateParams,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            vad_open_threshold: 0.5,
            vad_close_threshold: 0.35,
            lookahead_ms: 96.0,
            // Ballistique mesurée pour la PAROLE : attaque courte (le lookahead a
            // déjà pré-ouvert), relâche douce, et un maintien de 400 ms car les
            // silences inter-phrases relevés sur les prises réelles montent
            // jusque-là — en dessous, le gate referme entre deux phrases et il
            // faut le rouvrir à chaque fois.
            gate: GateParams { attack_ms: 5.0, release_ms: 150.0, hangover_ms: 400.0 },
        }
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
    /// Ligne à retard du lookahead : la voix nettoyée y transite avant le gate, le
    /// temps que le VAD se prononce sur elle. Pré-remplie de silence à `new`/`reset`
    /// ⇒ on en sort toujours exactement autant qu'on y pousse.
    delay: VecDeque<f32>,
    /// Seuils VAD (ouverture / maintien) — hystérésis.
    vad_open: f32,
    vad_close: f32,
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
        let vad = Vad::new()?;
        let lookahead = (cfg.lookahead_ms / 1000.0 * SAMPLE_RATE as f32).round() as usize;
        let mut iso = Self {
            denoiser,
            decimator: Decimator3::new(SAMPLE_RATE as f32),
            vad,
            gate: VoiceGate::new(SAMPLE_RATE as f32, cfg.gate),
            vad_accum: VecDeque::with_capacity(VAD_FRAME * 4),
            vad_frame: vec![0.0; VAD_FRAME],
            gate_gains: Vec::new(),
            delay: VecDeque::from(vec![0.0; lookahead]),
            vad_open: cfg.vad_open_threshold,
            vad_close: cfg.vad_close_threshold,
            speech: false,
        };
        // Rodage : `tract` alloue ses tampons à la PREMIÈRE inférence. Sans ce tour
        // à blanc, ce coût tomberait sur le premier bloc de voix réel, au moment
        // précis où la capture commence à pousser. On force donc une passe complète
        // (assez de silence pour déclencher denoise ET VAD) avant de rendre la main,
        // puis on remet l'état à zéro.
        let mut rodage = vec![0.0f32; VAD_FRAME * 3];
        iso.process_block(&mut rodage)?;
        iso.reset();
        Ok(iso)
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
            let p = self.vad.speech_prob(&self.vad_frame)?;
            // Hystérésis : seuil d'OUVERTURE haut (la repisse ne doit pas ouvrir),
            // seuil de MAINTIEN plus bas (une consonne sourde ne doit pas refermer).
            self.speech = if self.speech { p >= self.vad_close } else { p >= self.vad_open };
        }

        // 3) Lookahead : la voix nettoyée passe par la ligne à retard, si bien que
        //    le gate agit sur les échantillons que le VAD vient d'analyser (et non
        //    sur les suivants, ce qui rognait le début des mots).
        for s in block.iter_mut() {
            self.delay.push_back(*s);
            *s = self.delay.pop_front().expect("ligne pré-remplie ⇒ jamais vide");
        }

        // 4) Gate : gain lissé par-échantillon → silence total hors parole.
        if self.gate_gains.len() != block.len() {
            self.gate_gains.resize(block.len(), 0.0);
        }
        self.gate.process_block(self.speech, &mut self.gate_gains);
        for (s, g) in block.iter_mut().zip(self.gate_gains.iter()) {
            *s *= *g;
        }

        Ok(VoiceState { voice_active: self.gate.is_open() })
    }

    /// Latence AJOUTÉE au canal talkback par l'isolation : denoise + lookahead du
    /// gate, en millisecondes. À logguer au démarrage — ce coût doit être visible,
    /// jamais découvert à l'oreille. (Canal talkback uniquement : le monitoring
    /// instrument ne traverse pas cette chaîne.)
    pub fn added_latency_ms(&self) -> f32 {
        (self.denoiser.latency_samples() + self.delay.len()) as f32 / SAMPLE_RATE as f32 * 1000.0
    }

    /// Réinitialise toute la chaîne (à (ré)ouverture capture / hot-swap device).
    pub fn reset(&mut self) {
        self.denoiser.reset();
        self.decimator.reset();
        self.vad.reset();
        self.gate.reset();
        self.vad_accum.clear();
        // La ligne à retard se re-remplit de silence (même longueur qu'à la
        // construction) : le lookahead reste EXACT après un hot-swap de device.
        let n = self.delay.len();
        self.delay.clear();
        self.delay.extend(std::iter::repeat_n(0.0, n));
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

    /// Parole réelle @48 kHz : la fixture 16 kHz (LibriSpeech, CC-BY) est
    /// sur-échantillonnée ×3 par interpolation linéaire, précédée de 300 ms de
    /// silence. Suffisant pour la propriété testée (préservation de l'attaque) —
    /// la bande utile de la parole est très en dessous de 8 kHz.
    fn parole_48k() -> Vec<f32> {
        const RAW: &[u8] = include_bytes!("../../tests/fixtures/speech_16k_mono_i16.raw");
        let s16: Vec<f32> = RAW
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect();
        let mut out = vec![0.0f32; SAMPLE_RATE as usize * 3 / 10]; // 300 ms de silence
        for w in s16.windows(2) {
            for k in 0..3 {
                out.push(w[0] + (w[1] - w[0]) * (k as f32 / 3.0));
            }
        }
        out
    }

    fn run_chain(cfg: IsolationConfig, sig: &[f32]) -> Vec<f32> {
        let mut iso = VoiceIsolator::new(cfg).unwrap();
        let mut out = Vec::with_capacity(sig.len());
        for chunk in sig.chunks(HOP) {
            let mut b = chunk.to_vec();
            iso.process_block(&mut b).unwrap();
            out.extend_from_slice(&b);
        }
        out
    }

    /// Gain moyen RÉELLEMENT appliqué par le gate sur les `dur_ms` qui suivent
    /// l'attaque du premier mot. Référence = la même voix nettoyée SANS gate ;
    /// `retard` aligne les deux timelines (denoise seul vs denoise + lookahead).
    fn gain_sur_attaque(sortie: &[f32], reference: &[f32], retard: usize, dur_ms: usize) -> f32 {
        let seuil = rms(reference) * 0.2;
        let debut = reference.iter().position(|x| x.abs() > seuil).unwrap_or(0);
        let fin = (debut + dur_ms * SAMPLE_RATE as usize / 1000).min(reference.len());
        let e_ref: f32 = reference[debut..fin].iter().map(|x| x * x).sum();
        let e_out: f32 = sortie[(debut + retard).min(sortie.len())..(fin + retard).min(sortie.len())]
            .iter()
            .map(|x| x * x)
            .sum();
        (e_out / e_ref.max(1e-12)).sqrt()
    }

    #[test]
    fn le_lookahead_preserve_l_attaque_des_mots() {
        // RÉGRESSION (terrain 03/09 « ça coupe quand il y a la voix ») : sans
        // lookahead, la décision du VAD s'applique à des échantillons DÉJÀ sortis
        // → le début de chaque mot part atténué. On mesure le gain effectivement
        // appliqué sur les 100 premières ms du premier mot, la référence étant la
        // même voix nettoyée sans gate. Ce test échoue si le retard disparaît.
        let sig = parole_48k();
        let cfg = IsolationConfig::default();
        let avec = run_chain(cfg, &sig);
        let sans = run_chain(IsolationConfig { lookahead_ms: 0.0, ..cfg }, &sig);

        let mut den = Denoiser::new().unwrap();
        let mut reference = Vec::with_capacity(sig.len());
        for chunk in sig.chunks(HOP) {
            let mut b = chunk.to_vec();
            den.process_block(&mut b).unwrap();
            reference.extend_from_slice(&b);
        }
        let retard = (cfg.lookahead_ms / 1000.0 * SAMPLE_RATE as f32) as usize;
        // Fenêtre courte (30 ms) : c'est très exactement la portion de mot que la
        // latence de décision du VAD fait disparaître.
        // Fenêtre de 20 ms : très exactement la portion de mot que la latence de
        // décision du VAD fait disparaître. (Sur les 10 premières ms, la chaîne
        // sans lookahead ne sort RIEN du tout.)
        let g_avec = gain_sur_attaque(&avec, &reference, retard, 20);
        let g_sans = gain_sur_attaque(&sans, &reference, 0, 20);
        eprintln!("gain sur l'attaque : avec lookahead={g_avec:.3}  sans={g_sans:.3}");
        assert!(
            g_avec > 0.9,
            "avec lookahead, l'attaque doit passer quasi intacte (gain={g_avec:.2})"
        );
        assert!(
            g_avec > g_sans * 1.8,
            "le lookahead doit préserver l'attaque : avec={g_avec:.2} sans={g_sans:.2}"
        );
    }

    #[test]
    fn le_lookahead_retarde_exactement_de_la_consigne() {
        // Le retard doit être EXACT (et le rester après reset) : c'est lui qui
        // aligne la décision du VAD sur les échantillons analysés.
        let cfg = IsolationConfig { lookahead_ms: 10.0, ..IsolationConfig::default() };
        let mut iso = VoiceIsolator::new(cfg).unwrap();
        assert_eq!(iso.delay.len(), SAMPLE_RATE as usize / 100, "10 ms @48k = 480 éch.");
        let mut b = noise(HOP, 0.3);
        iso.process_block(&mut b).unwrap();
        iso.reset();
        assert_eq!(iso.delay.len(), SAMPLE_RATE as usize / 100, "reset conserve la longueur du retard");
        assert!(iso.delay.iter().all(|&x| x == 0.0), "reset re-remplit de silence");
    }
}
