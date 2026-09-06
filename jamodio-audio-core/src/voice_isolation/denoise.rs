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
    /// Plus grand bloc reçu jusqu'ici. Le coussin doit le couvrir : un pilote qui
    /// délivre des blocs plus GROS que la latence du modèle (buffer de 2048
    /// frames, vu sur certaines cartes) viderait l'anneau de sortie à chaque tour
    /// et on comblerait en silence — un trou périodique, inaudible à l'analyse et
    /// invisible dans les logs.
    max_block: usize,
    primed: bool,
    /// Nombre d'échantillons comblés par du silence faute de sortie disponible.
    /// Doit rester à 0 ; sinon c'est une dégradation, et elle se DIT.
    underruns: u64,
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
        // Garde AVANT tout chargement de modèle : sur macOS virtualisé, la première
        // inférence ne renvoie pas une erreur, elle TUE le processus (AMX absent en
        // VM — cf. `macos_virtualise`). Une erreur explicite ici fait basculer
        // l'appelant en voix brute, avec la raison affichée ; sans elle, l'agent
        // disparaîtrait au premier mot prononcé.
        if crate::voice_isolation::macos_virtualise() {
            return Err(IsolationError::Denoise(
                crate::voice_isolation::VM_NON_SUPPORTEE.to_string(),
            ));
        }
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
        let mut den = Self {
            model,
            hop,
            sr,
            in_ring: VecDeque::with_capacity(hop * 8),
            out_ring: VecDeque::with_capacity(hop * 8),
            in_hop: vec![0.0; hop],
            enh_hop: vec![0.0; hop],
            prime_target,
            max_block: 0,
            primed: false,
            underruns: 0,
        };
        // RODAGE : `tract` alloue ses tampons à la première inférence. Payé ici,
        // à la construction, plutôt que sur le premier bloc de voix réel — au
        // moment précis où la capture commence à pousser.
        //
        // Fait DANS le constructeur (et non chez l'appelant) pour que TOUT
        // `Denoiser` naisse dans le MÊME état interne : sans quoi une instance
        // rodée et une instance fraîche ne rendent pas exactement le même signal,
        // et le banc de diagnostic ne décrirait plus la production.
        let mut rodage = vec![0.0f32; hop * 3];
        den.process_block(&mut rodage)?;
        den.reset();
        Ok(den)
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
        self.max_block = self.max_block.max(block.len());
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
        //    (c'est la latence). Le coussin doit couvrir À LA FOIS la latence du
        //    modèle ET la taille du plus gros bloc reçu, sinon chaque bloc plus
        //    grand que le coussin sortirait à moitié vide.
        let cushion = self.prime_target.max(self.max_block);
        if !self.primed && self.out_ring.len() >= cushion {
            self.primed = true;
        }
        if self.primed {
            let mut missing = 0usize;
            for slot in block.iter_mut() {
                match self.out_ring.pop_front() {
                    Some(v) => *slot = v,
                    // Au pire du silence, jamais de bruit résiduel — mais on le
                    // COMPTE : un trou silencieux non signalé est indiagnostiquable.
                    None => {
                        *slot = 0.0;
                        missing += 1;
                    }
                }
            }
            if missing > 0 {
                self.underruns = self.underruns.saturating_add(missing as u64);
                if self.underruns.is_power_of_two() {
                    tracing::warn!(
                        target: "jamodio::voice_isolation",
                        missing,
                        total = self.underruns,
                        block = block.len(),
                        cushion,
                        "denoise sous-alimenté — silence comblé (coussin trop court ?)"
                    );
                }
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
        // La taille de bloc se RÉAPPREND : après un hot-swap de device, celle de
        // l'ancien pilote ne doit pas dimensionner le coussin du nouveau. C'est
        // aussi ce qui empêche le bloc de rodage de `VoiceIsolator::new` (plus
        // gros qu'un bloc réel) d'ajouter 2 ms de latence à tout le canal.
        self.max_block = 0;
    }

    /// Échantillons comblés par du silence faute de sortie disponible (doit
    /// rester à 0). Exposé pour les tests et le diagnostic.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_le_modele_embarque() {
        if crate::voice_isolation::inference_impossible_ici() {
            return;
        }
        // Prouve que le modèle embarqué se charge dans notre build (tract pur Rust).
        let d = Denoiser::new().expect("le modèle DeepFilterNet embarqué doit se charger");
        assert_eq!(d.sample_rate(), 48_000);
        assert_eq!(d.hop, 480);
    }

    #[test]
    fn bloc_meme_taille_et_silence_sur_zero() {
        if crate::voice_isolation::inference_impossible_ici() {
            return;
        }
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
        if crate::voice_isolation::inference_impossible_ici() {
            return;
        }
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

    #[test]
    fn gros_blocs_ne_creusent_pas_de_trous() {
        if crate::voice_isolation::inference_impossible_ici() {
            return;
        }
        // RÉGRESSION (revue 04/09) : avec un pilote qui délivre des blocs PLUS
        // GROS que la latence du modèle (2048 frames, vu sur certaines cartes),
        // un coussin figé sur la seule latence se vidait à chaque tour et on
        // comblait en silence — un trou périodique, muet dans les logs.
        let mut d = Denoiser::new().unwrap();
        // Signal continu : tout zéro en sortie APRÈS amorçage signalerait un trou.
        let bloc: Vec<f32> = (0..2048)
            .map(|i| (i as f32 * 0.01).sin() * 0.3)
            .collect();
        for _ in 0..12 {
            let mut b = bloc.clone();
            d.process_block(&mut b).unwrap();
        }
        assert_eq!(
            d.underruns(),
            0,
            "aucun échantillon ne doit être comblé par du silence sur des blocs de 2048"
        );
    }
}
