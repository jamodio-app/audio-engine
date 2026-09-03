//! Wrapper **DeepFilterNet** (denoise) — enlève la repisse d'instrument de la voix,
//! en **pur Rust via tract** (moteur embarqué, aucune dépendance native).
//!
//! Interface **streaming** : on lui passe des blocs de taille arbitraire, il rend
//! des blocs de **même taille**. Le modèle travaille par **hops** de `hop_size`
//! (480 éch. @48k) et a une latence algorithmique inhérente (~30 ms) : on l'absorbe
//! par un **amorçage** (silence au tout début) puis un coussin de sortie qui évite
//! toute sous-alimentation (donc jamais de bruit injecté — au pire du silence).

use std::collections::VecDeque;

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};

use super::IsolationError;

/// Réglages d'exécution de DeepFilterNet. Ce sont des SEUILS DE SNR LOCAL qui
/// pilotent des commutations FRANCHES à l'intérieur du modèle, trame par trame
/// (cf. `DfTract::apply_stages`) :
///   - sous `min_snr_db` → le modèle applique un **masque de zéros** (trame muette) ;
///   - au-dessus de `max_erb_snr_db` → signal jugé propre, **aucun traitement** ;
///   - au-dessus de `max_df_snr_db` → 2ᵉ étage (deep filtering) **sauté**.
///
/// Ces valeurs sont donc AUDIBLES : trop serrées, le modèle bascule d'un régime à
/// l'autre au fil des trames (voix hachée, effet « qui respire ») sur une captation
/// où l'instrument repisse — exactement notre cas.
///
/// **Défauts = ceux du binaire officiel `deep-filter`** (`libDF/src/bin/enhance_wav.rs`),
/// c'est-à-dire la configuration validée à l'oreille sur les vraies prises. ⚠️ Ils
/// diffèrent des `RuntimeParams::default()` de la bibliothèque (−10 / 30 / 20), bien
/// plus agressifs en commutation : ne PAS revenir à ces derniers.
#[derive(Debug, Clone, Copy)]
pub struct DenoiseParams {
    /// SNR local sous lequel la trame est mise à zéro (« que du bruit »), en dB.
    pub min_snr_db: f32,
    /// SNR local au-dessus duquel le signal est jugé propre (aucun traitement), en dB.
    pub max_erb_snr_db: f32,
    /// SNR local au-dessus duquel l'étage DF est sauté, en dB.
    pub max_df_snr_db: f32,
    /// Limite d'atténuation du bruit, en dB (100 = réduction totale).
    pub atten_lim_db: f32,
}

impl Default for DenoiseParams {
    fn default() -> Self {
        Self { min_snr_db: -15.0, max_erb_snr_db: 35.0, max_df_snr_db: 35.0, atten_lim_db: 100.0 }
    }
}

pub struct Denoiser {
    model: DfTract,
    hop: usize,
    sr: usize,
    /// Échantillons d'entrée en attente d'un hop complet.
    in_ring: VecDeque<f32>,
    /// Échantillons débruités prêts à émettre.
    out_ring: VecDeque<f32>,
    /// Buffers de travail préalloués (pas d'alloc dans le chemin chaud).
    in_hop: Vec<f32>,
    enh_hop: Vec<f32>,
    /// Coussin de sortie à atteindre avant de commencer à émettre (= latence algo).
    prime_target: usize,
    primed: bool,
}

impl Denoiser {
    /// Charge le modèle embarqué (feature `default-model`) avec les réglages par
    /// défaut ([`DenoiseParams`]). **Erreur explicite** si le chargement échoue
    /// (zéro fallback silencieux — l'appelant décide).
    pub fn new() -> Result<Self, IsolationError> {
        Self::with_params(DenoiseParams::default())
    }

    /// Idem [`Denoiser::new`] avec des seuils explicites (banc de réglage hors-ligne).
    pub fn with_params(p: DenoiseParams) -> Result<Self, IsolationError> {
        let rp = RuntimeParams::default()
            .with_atten_lim(p.atten_lim_db)
            .with_thresholds(p.min_snr_db, p.max_erb_snr_db, p.max_df_snr_db);
        let dp = DfParams::default();
        let model = DfTract::new(dp, &rp).map_err(|e| IsolationError::Denoise(e.to_string()))?;
        let hop = model.hop_size;
        let sr = model.sr;
        // Latence algo (échantillons) = retard STFT + look-ahead.
        let latency = (model.fft_size - model.hop_size) + model.lookahead * model.hop_size;
        // Coussin ≥ latence et ≥ 1 hop → jamais de sous-alimentation en régime.
        let prime_target = latency.max(hop);
        Ok(Self {
            model,
            hop,
            sr,
            in_ring: VecDeque::with_capacity(hop * 8),
            out_ring: VecDeque::with_capacity(hop * 8),
            in_hop: vec![0.0; hop],
            enh_hop: vec![0.0; hop],
            prime_target,
            primed: false,
        })
    }

    pub fn sample_rate(&self) -> usize {
        self.sr
    }

    /// Latence algorithmique du modèle, en échantillons (retard STFT + look-ahead
    /// interne). C'est le retard que le denoise ajoute au canal talkback.
    pub fn latency_samples(&self) -> usize {
        self.prime_target
    }

    /// Débruite `block` **en place**. La sortie est retardée de la latence du
    /// modèle (silence pendant l'amorçage initial).
    pub fn process_block(&mut self, block: &mut [f32]) -> Result<(), IsolationError> {
        // 1) Empile l'entrée, traite tous les hops complets disponibles.
        self.in_ring.extend(block.iter().copied());
        while self.in_ring.len() >= self.hop {
            for slot in self.in_hop.iter_mut() {
                *slot = self.in_ring.pop_front().expect("hop complet garanti par la condition while");
            }
            let noisy = ArrayView2::from_shape((1, self.hop), &self.in_hop)
                .map_err(|e| IsolationError::Denoise(e.to_string()))?;
            let enh = ArrayViewMut2::from_shape((1, self.hop), &mut self.enh_hop)
                .map_err(|e| IsolationError::Denoise(e.to_string()))?;
            self.model
                .process(noisy, enh)
                .map_err(|e| IsolationError::Denoise(e.to_string()))?;
            self.out_ring.extend(self.enh_hop.iter().copied());
        }

        // 2) Amorçage : tant que le coussin n'est pas constitué, on émet du silence
        //    (c'est la latence). Ensuite, on émet la voix débruitée.
        if !self.primed && self.out_ring.len() >= self.prime_target {
            self.primed = true;
        }
        if self.primed {
            for slot in block.iter_mut() {
                // `unwrap_or(0.0)` : au pire du silence, jamais de bruit résiduel.
                *slot = self.out_ring.pop_front().unwrap_or(0.0);
            }
        } else {
            block.fill(0.0);
        }
        Ok(())
    }

    /// Réinitialise les tampons + ré-amorce (à (ré)ouverture capture / hot-swap).
    /// L'état interne du modèle se purge de lui-même en quelques hops.
    pub fn reset(&mut self) {
        self.in_ring.clear();
        self.out_ring.clear();
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_le_modele_embarque() {
        // Prouve que le modèle embarqué se charge dans notre build (tract pur Rust).
        let d = Denoiser::new().expect("le modèle DeepFilterNet embarqué doit se charger");
        assert_eq!(d.sample_rate(), 48_000);
        assert_eq!(d.hop, 480);
    }

    #[test]
    fn bloc_meme_taille_et_silence_sur_zero() {
        let mut d = Denoiser::new().unwrap();
        // Entrée silencieuse → sortie de même taille, sans NaN, bornée.
        let mut block = vec![0.0f32; 1024];
        d.process_block(&mut block).unwrap();
        assert_eq!(block.len(), 1024);
        assert!(block.iter().all(|x| x.is_finite()));
        // Entrée nulle ⇒ sortie nulle (rien à débruiter).
        assert!(block.iter().all(|&x| x.abs() < 1e-3));
    }

    #[test]
    fn reset_ok() {
        let mut d = Denoiser::new().unwrap();
        let mut b = vec![0.1f32; 2048];
        d.process_block(&mut b).unwrap();
        d.reset();
        assert!(!d.primed);
        assert!(d.in_ring.is_empty() && d.out_ring.is_empty());
    }

    #[test]
    fn seuils_par_defaut_sont_ceux_du_binaire_valide() {
        // RÉGRESSION (terrain 03/09 « ce n'est pas propre ») : on embarquait les
        // `RuntimeParams::default()` de la bibliothèque (−10 / 30 / 20), bien plus
        // agressifs en COMMUTATION D'ÉTAGES que ceux du binaire officiel
        // `deep-filter` (−15 / 35 / 35) — seuls ces derniers ont été validés à
        // l'oreille sur les prises réelles. Sur une prise voix+guitare, les seuils
        // de la lib ne laissaient passer que 67 % du niveau contre 92 %.
        let p = DenoiseParams::default();
        assert_eq!(p.min_snr_db, -15.0);
        assert_eq!(p.max_erb_snr_db, 35.0);
        assert_eq!(p.max_df_snr_db, 35.0);
        assert_eq!(p.atten_lim_db, 100.0);
    }
}
