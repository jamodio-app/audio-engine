//! Gain lissé par échantillon — le motif anti-clic du projet, en un seul endroit.
//!
//! Multiplier un bloc par un gain qui a changé depuis le bloc précédent produit
//! une discontinuité : un CLIC, parfaitement audible même pour un écart de
//! quelques dB. Tout gain piloté par l'utilisateur doit donc rejoindre sa cible
//! progressivement.
//!
//! Le filtre est un one-pole ASYMÉTRIQUE : on monte vite et on descend
//! doucement. C'est ce que veut l'usage — rouvrir un micro doit être immédiat,
//! le couper doit s'éteindre sans claquer — et c'est le comportement que le
//! thread voix appliquait déjà avant que ce module existe. Il est ici pour être
//! PARTAGÉ, pas recopié.
//!
//! `coeff = 1 − exp(−1 / (τ · fs))`, avec τ la constante de temps en secondes.

/// Constante de temps de MONTÉE par défaut (s) — ouverture perçue immédiate.
pub const ATTACK_S: f32 = 0.015;
/// Constante de temps de DESCENTE par défaut (s) — extinction sans claquement.
pub const RELEASE_S: f32 = 0.080;

/// Coefficient one-pole pour une constante de temps et un taux d'échantillonnage.
#[inline]
pub fn coeff(tau_s: f32, sample_rate: f32) -> f32 {
    1.0 - (-1.0f32 / (tau_s * sample_rate)).exp()
}

/// Gain qui rejoint sa cible sans clic.
///
/// L'appelant garde l'instance entre deux blocs : c'est l'état de la rampe. La
/// cible peut changer à chaque bloc — elle vient d'un atomique écrit par le
/// thread WS — la rampe absorbe les sauts.
#[derive(Debug, Clone)]
pub struct SmoothGain {
    current: f32,
    attack: f32,
    release: f32,
}

impl SmoothGain {
    /// Démarre À la valeur donnée : pas de rampe au premier bloc, sinon chaque
    /// (ré)ouverture de flux commencerait par un fondu parasite.
    pub fn new(initial: f32, sample_rate: f32) -> Self {
        Self {
            current: initial,
            attack: coeff(ATTACK_S, sample_rate),
            release: coeff(RELEASE_S, sample_rate),
        }
    }

    /// Valeur courante, sans avancer la rampe.
    #[inline]
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Avance d'UN échantillon vers `target` et rend le gain à appliquer — pour
    /// les appelants qui traitent le signal échantillon par échantillon.
    #[inline]
    pub fn next(&mut self, target: f32) -> f32 {
        let c = if target > self.current { self.attack } else { self.release };
        self.current += (target - self.current) * c;
        self.current
    }

    /// Applique la rampe à un bloc INTERLEAVÉ STÉRÉO, en place.
    ///
    /// Les deux canaux d'une même frame reçoivent le MÊME gain — sinon la rampe
    /// déplacerait l'image stéréo pendant sa durée. Elle avance donc une fois
    /// par frame, pas une fois par échantillon.
    #[inline]
    pub fn apply_stereo_block(&mut self, buf: &mut [f32], target: f32) {
        // Cas ultra-majoritaire — gain neutre déjà atteint : on ne parcourt pas
        // le bloc du tout.
        if target == 1.0 && (self.current - 1.0).abs() < f32::EPSILON {
            return;
        }
        for frame in buf.chunks_exact_mut(2) {
            let g = self.next(target);
            frame[0] *= g;
            frame[1] *= g;
        }
        // Longueur impaire (cas dégénéré) : l'échantillon orphelin prend le gain
        // courant plutôt que de rester sans rampe.
        if buf.len() % 2 == 1 {
            let g = self.current;
            if let Some(last) = buf.last_mut() {
                *last *= g;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    #[test]
    fn demarre_a_la_valeur_initiale_sans_fondu() {
        let g = SmoothGain::new(0.5, FS);
        assert_eq!(g.current(), 0.5, "pas de rampe parasite à l'ouverture");
    }

    #[test]
    fn rejoint_la_cible_et_s_y_tient() {
        let mut g = SmoothGain::new(0.0, FS);
        for _ in 0..(FS as usize / 5) {
            g.next(1.0); // 200 ms, soit ~13 constantes de temps
        }
        assert!((g.current() - 1.0).abs() < 1e-3, "cible atteinte, got {}", g.current());
    }

    #[test]
    fn la_montee_est_plus_rapide_que_la_descente() {
        let n = (FS * ATTACK_S) as usize; // une constante de temps de montée
        let mut up = SmoothGain::new(0.0, FS);
        for _ in 0..n {
            up.next(1.0);
        }
        let mut down = SmoothGain::new(1.0, FS);
        for _ in 0..n {
            down.next(0.0);
        }
        // Après τ de montée on est à ~63 % ; sur la même durée la descente, cinq
        // fois plus lente, n'a presque pas bougé.
        assert!(up.current() > 0.6, "montée à ~63 % après τ, got {}", up.current());
        assert!(down.current() > 0.8, "descente encore haute après τ, got {}", down.current());
    }

    #[test]
    fn aucune_discontinuite_sur_un_saut_de_gain() {
        // Signal constant : toute variation d'un échantillon à l'autre vient de
        // la rampe, et d'elle seule.
        let mut g = SmoothGain::new(1.0, FS);
        let mut buf = vec![1.0f32; 480 * 2];
        g.apply_stereo_block(&mut buf, 0.0);
        let saut_max = buf
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(saut_max < 0.01, "pas de marche audible, plus gros écart {saut_max}");
    }

    #[test]
    fn les_deux_canaux_recoivent_le_meme_gain() {
        let mut g = SmoothGain::new(1.0, FS);
        let mut buf = vec![1.0f32; 64];
        g.apply_stereo_block(&mut buf, 0.0);
        for frame in buf.chunks_exact(2) {
            assert_eq!(frame[0], frame[1], "l'image stéréo ne bouge pas pendant la rampe");
        }
    }

    #[test]
    fn gain_neutre_laisse_le_bloc_intact() {
        let mut g = SmoothGain::new(1.0, FS);
        let mut buf = vec![0.25f32; 32];
        g.apply_stereo_block(&mut buf, 1.0);
        assert!(buf.iter().all(|&s| s == 0.25), "défaut 0 dB = signal bit-identique");
    }
}
