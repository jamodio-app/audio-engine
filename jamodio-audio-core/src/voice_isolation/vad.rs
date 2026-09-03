//! Wrapper **Silero VAD** (détection de parole) — décide, trame par trame, si de
//! la voix est présente, en **pur Rust via tract** (aucune dépendance native).
//!
//! On utilise la variante **`silero_vad_op18_ifless.onnx`** (sans opérateur `If`)
//! car tract ne gère pas le control-flow — vendorisée dans `assets/` (Silero VAD,
//! licence MIT). Interface du modèle :
//!   entrées : `input` [1, 512] f32, `sr` (scalaire i64), `state` [2, 1, 128] f32
//!   sorties : `output` [1, 1] f32 (proba de parole), `stateN` [2, 1, 128] f32
//! Trame = **512 échantillons @ 16 kHz** (32 ms) ; l'état LSTM est reporté d'une
//! trame à l'autre (le modèle est causal/streaming).

use tract_onnx::prelude::*;

use super::IsolationError;

/// Fréquence d'échantillonnage attendue par le VAD (le talkback y est ramené).
pub const VAD_SR: i64 = 16_000;
/// Taille de trame VAD en échantillons @16 kHz (32 ms).
pub const VAD_FRAME: usize = 512;
/// Silero v5 préfixe à chaque trame un **contexte** = les 64 derniers échantillons
/// de la trame précédente. L'entrée réelle du modèle vaut donc `64 + 512 = 576`.
/// (Sans ce contexte, le modèle renvoie une proba ~0 sur toute parole — bug corrigé.)
const VAD_CONTEXT: usize = 64;

/// Modèle embarqué (variante ifless, compatible tract).
static MODEL_BYTES: &[u8] = include_bytes!("../../assets/silero_vad_ifless.onnx");

type Plan = TypedRunnableModel<TypedModel>;

pub struct Vad {
    plan: Plan,
    /// État LSTM [2, 1, 128], reporté entre trames.
    state: Tensor,
    /// Contexte = 64 derniers échantillons de la trame précédente (préfixé à l'entrée).
    context: Vec<f32>,
    /// Tampon d'entrée préalloué (64 + 512 = 576).
    input_buf: Vec<f32>,
    /// Fréquence (scalaire i64), constante.
    sr: Tensor,
}

fn zero_state() -> Tensor {
    Tensor::zero::<f32>(&[2, 1, 128]).expect("forme d'état valide")
}

impl Vad {
    /// Charge le modèle embarqué. Le VAD ne rend qu'une **probabilité** : la
    /// décision (seuils d'ouverture / maintien) appartient à
    /// [`super::IsolationConfig`], source unique de vérité.
    /// **Erreur explicite** si le chargement échoue (zéro fallback silencieux).
    pub fn new() -> Result<Self, IsolationError> {
        let map = |e: TractError| IsolationError::Vad(e.to_string());
        // NB : on ne pose PAS de formes d'entrée fixes. tract analyse alors le
        // graphe avec des dims symboliques (dont le nœud `If` interne du modèle)
        // et le rend runnable ; les formes concrètes viennent des tenseurs au run.
        // (Poser des facts fixes fait échouer l'analyse du `If` — cf. spike 2b.)
        // Ordre des entrées côté tract : [input, sr, state].
        let plan = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(MODEL_BYTES))
            .map_err(map)?
            .into_optimized()
            .map_err(map)?
            .into_runnable()
            .map_err(map)?;
        Ok(Self {
            plan,
            state: zero_state(),
            context: vec![0.0; VAD_CONTEXT],
            input_buf: vec![0.0; VAD_CONTEXT + VAD_FRAME],
            sr: tensor0(VAD_SR),
        })
    }

    pub const fn frame_len(&self) -> usize {
        VAD_FRAME
    }

    /// Proba de parole `[0, 1]` pour une trame de [`VAD_FRAME`] échantillons @16k.
    /// Reporte l'état LSTM pour la trame suivante.
    pub fn speech_prob(&mut self, frame: &[f32]) -> Result<f32, IsolationError> {
        let map = |e: TractError| IsolationError::Vad(e.to_string());
        debug_assert_eq!(frame.len(), VAD_FRAME, "trame VAD = {VAD_FRAME} éch. @16k");
        // Entrée = contexte (64) ++ trame (512) = 576 (préproc. Silero v5, indispensable).
        self.input_buf[..VAD_CONTEXT].copy_from_slice(&self.context);
        self.input_buf[VAD_CONTEXT..].copy_from_slice(frame);
        let input =
            Tensor::from_shape(&[1, VAD_CONTEXT + VAD_FRAME], &self.input_buf).map_err(map)?;
        let out = self
            .plan
            .run(tvec!(input.into(), self.sr.clone().into(), self.state.clone().into()))
            .map_err(map)?;
        let prob = out[0].as_slice::<f32>().map_err(map)?[0];
        // Reporte l'état LSTM + le contexte (64 derniers éch. de la trame) pour la suivante.
        self.state = out[1].clone().into_tensor();
        self.context.copy_from_slice(&frame[VAD_FRAME - VAD_CONTEXT..]);
        Ok(prob)
    }

    /// Réinitialise l'état LSTM (à (ré)ouverture capture / hot-swap).
    pub fn reset(&mut self) {
        self.state = zero_state();
        self.context.iter_mut().for_each(|c| *c = 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_le_modele_vad() {
        let v = Vad::new().expect("le modèle Silero VAD embarqué doit se charger dans tract");
        assert_eq!(v.frame_len(), 512);
    }

    #[test]
    fn silence_faible_proba() {
        let mut v = Vad::new().unwrap();
        let frame = [0.0f32; VAD_FRAME];
        // Sur du silence, la proba de parole doit être basse (et jamais NaN).
        let mut p = 0.0;
        for _ in 0..10 {
            p = v.speech_prob(&frame).unwrap();
            assert!(p.is_finite() && (0.0..=1.0).contains(&p));
        }
        assert!(p < 0.5, "silence ⇒ proba basse, obtenu {p}");
    }

    #[test]
    fn reset_remet_etat_a_zero() {
        let mut v = Vad::new().unwrap();
        let frame = [0.1f32; VAD_FRAME];
        v.speech_prob(&frame).unwrap();
        v.reset();
        // Après reset, l'état est nul → même sortie qu'au démarrage sur une trame donnée.
        let a = v.speech_prob(&frame).unwrap();
        v.reset();
        let b = v.speech_prob(&frame).unwrap();
        assert!((a - b).abs() < 1e-6, "reset doit rendre l'inférence reproductible");
    }

    #[test]
    fn detecte_la_parole_reelle_regression_contexte() {
        // RÉGRESSION : sans le contexte de 64 éch. préfixé (entrée 576), Silero renvoie
        // ~0 sur TOUTE parole → gate fermé → talkback muet (bug terrain 03/09). Ce test
        // échoue si le contexte n'est plus appliqué.
        // Fixture : 1,5 s de parole @16k mono i16 (LibriSpeech dev-clean, CC-BY 4.0).
        const RAW: &[u8] = include_bytes!("../../tests/fixtures/speech_16k_mono_i16.raw");
        let samples: Vec<f32> = RAW
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect();
        let mut v = Vad::new().unwrap();
        let (mut maxp, mut speech, mut frames) = (0.0f32, 0usize, 0usize);
        for frame in samples.chunks_exact(VAD_FRAME) {
            let p = v.speech_prob(frame).unwrap();
            maxp = maxp.max(p);
            if p >= 0.5 {
                speech += 1;
            }
            frames += 1;
        }
        assert!(maxp > 0.9, "le VAD doit détecter la parole (max proba={maxp} — contexte manquant ?)");
        assert!(speech > frames / 4, "assez de trames de parole ({speech}/{frames})");
    }
}
