//! Limiteur de crête à **lookahead**, placé juste avant l'encodeur Opus du canal
//! talkback.
//!
//! **Pourquoi.** Le signal qui arrive de la capture peut dépasser le plein échelle :
//! CoreAudio (comme WASAPI/ASIO en flottant) livre des `f32` qui ne sont PAS bornés
//! à ±1.0, et un micro-casque ou un micro interne — dont l'utilisateur ne règle
//! aucun gain matériel — peut être amplifié par le pilote ou l'OS bien au-delà.
//! Mesuré en session réelle (03/09) : des crêtes d'entrée à **2,62, soit +8,4 dB
//! au-dessus du plein échelle**, 193 fois au-dessus de 2,0 sur une journée. Ces
//! échantillons arrivaient tels quels dans Opus, qui les tronque → distorsion.
//!
//! **Comment.** On regarde `lookahead_ms` en avance (maximum glissant exact, deque
//! monotone) : quand une crête va dépasser le plafond, la réduction de gain commence
//! AVANT elle et l'accompagne, au lieu d'aplatir la forme d'onde. Sous le plafond, le
//! gain vaut exactement 1.0 → **le signal n'est pas touché** (pas de coloration
//! permanente). Le prix est le retard `lookahead_ms`, sur le canal talkback seul.

use std::collections::VecDeque;

/// Plafond de sortie par défaut : −1 dBFS. La marge sert au décodeur Opus, dont la
/// reconstruction peut légèrement dépasser le niveau encodé.
pub const DEFAULT_CEILING: f32 = 0.891_25;
/// Lookahead par défaut, en ms : assez pour envelopper une crête de parole sans
/// alourdir la latence du talkback.
pub const DEFAULT_LOOKAHEAD_MS: f32 = 3.0;

/// Limiteur de crête mono, sans allocation dans le chemin chaud.
pub struct PeakLimiter {
    ceiling: f32,
    /// Retard = fenêtre de lookahead. `delay[0]` est l'échantillon à sortir.
    delay: VecDeque<f32>,
    /// Maximum glissant sur la fenêtre : deque monotone décroissante de
    /// `(index absolu, |échantillon|)`.
    maxq: VecDeque<(u64, f32)>,
    /// Index absolu du prochain échantillon POUSSÉ (donc futur) et du prochain SORTI.
    idx_in: u64,
    idx_out: u64,
    gain: f32,
    attack_coeff: f32,
    release_coeff: f32,
    /// Réduction de gain maximale appliquée depuis le dernier `take_max_reduction_db`.
    max_reduction: f32,
}

fn one_pole(ms: f32, sample_rate: f32) -> f32 {
    if ms <= 0.0 {
        return 1.0;
    }
    1.0 - (-1.0 / (ms / 1000.0 * sample_rate)).exp()
}

impl PeakLimiter {
    /// `ceiling` en linéaire (cf. [`DEFAULT_CEILING`]), `lookahead_ms` > 0.
    pub fn new(sample_rate: f32, ceiling: f32, lookahead_ms: f32) -> Self {
        debug_assert!(sample_rate > 0.0 && ceiling > 0.0);
        let lookahead = ((lookahead_ms / 1000.0 * sample_rate).round() as usize).max(1);
        Self {
            ceiling,
            delay: VecDeque::from(vec![0.0; lookahead]),
            maxq: VecDeque::with_capacity(lookahead + 1),
            idx_in: lookahead as u64,
            idx_out: 0,
            gain: 1.0,
            // Attaque bien plus courte que le lookahead : le gain a atteint sa cible
            // quand la crête arrive. Relâche lente : pas de pompage entre deux mots.
            attack_coeff: one_pole(lookahead_ms / 3.0, sample_rate),
            release_coeff: one_pole(80.0, sample_rate),
            max_reduction: 0.0,
        }
    }

    /// Retard introduit, en échantillons (= la fenêtre de lookahead).
    pub fn latency_samples(&self) -> usize {
        self.delay.len()
    }

    /// Réduction de gain maximale appliquée depuis le dernier appel, en dB positifs
    /// (0 = le limiteur n'a rien eu à faire). Remet le compteur à zéro : sert à
    /// remonter à l'UI que l'entrée talkback sature — visible, jamais silencieux.
    pub fn take_max_reduction_db(&mut self) -> f32 {
        let r = self.max_reduction;
        self.max_reduction = 0.0;
        if r <= 0.0 {
            0.0
        } else {
            -20.0 * (1.0 - r).max(1e-6).log10()
        }
    }

    /// Traite un bloc **en place**. La sortie est retardée du lookahead.
    pub fn process_block(&mut self, block: &mut [f32]) {
        for s in block.iter_mut() {
            // 1) L'échantillon entrant rejoint la fenêtre (et le maximum glissant).
            let a = s.abs();
            while self.maxq.back().is_some_and(|&(_, v)| v <= a) {
                self.maxq.pop_back();
            }
            self.maxq.push_back((self.idx_in, a));
            self.delay.push_back(*s);
            self.idx_in += 1;

            // 2) L'échantillon sortant quitte la fenêtre.
            let out = self.delay.pop_front().expect("fenêtre pré-remplie ⇒ jamais vide");
            while self.maxq.front().is_some_and(|&(i, _)| i < self.idx_out) {
                self.maxq.pop_front();
            }
            self.idx_out += 1;

            // 3) Cible : juste assez de réduction pour que la crête à venir tienne
            //    sous le plafond. 1.0 (aucune retouche) tant qu'on est dessous.
            let peak = self.maxq.front().map_or(0.0, |&(_, v)| v);
            let target = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
            let coeff = if target < self.gain { self.attack_coeff } else { self.release_coeff };
            self.gain += (target - self.gain) * coeff;
            self.gain = self.gain.clamp(0.0, 1.0);
            self.max_reduction = self.max_reduction.max(1.0 - self.gain);

            // 4) Filet de sécurité : la rampe peut laisser passer quelques millièmes
            //    au tout premier échantillon d'une crête ; on borne, mais ce n'est
            //    JAMAIS ce qui fait le travail (sinon ce serait de l'écrêtage).
            *s = (out * self.gain).clamp(-self.ceiling, self.ceiling);
        }
    }

    /// Réinitialise (à (ré)ouverture de capture / hot-swap device).
    pub fn reset(&mut self) {
        let n = self.delay.len();
        self.delay.clear();
        self.delay.extend(std::iter::repeat_n(0.0, n));
        self.maxq.clear();
        self.idx_in = self.idx_out + n as u64;
        self.gain = 1.0;
        self.max_reduction = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn limiteur() -> PeakLimiter {
        PeakLimiter::new(SR, DEFAULT_CEILING, DEFAULT_LOOKAHEAD_MS)
    }

    fn sinus(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin() * amp)
            .collect()
    }

    #[test]
    fn sous_le_plafond_le_signal_n_est_pas_touche() {
        // Propriété essentielle : un limiteur qui colore en permanence est un
        // compresseur déguisé. Sous le plafond, gain = 1.0 exactement.
        let mut l = limiteur();
        let entree = sinus(4800, 440.0, 0.5);
        let mut sortie = entree.clone();
        l.process_block(&mut sortie);
        let retard = l.latency_samples();
        for (i, &x) in entree.iter().enumerate().take(entree.len() - retard) {
            assert_eq!(sortie[i + retard], x, "échantillon {i} modifié sous le plafond");
        }
        assert_eq!(l.take_max_reduction_db(), 0.0, "aucune réduction attendue");
    }

    #[test]
    fn les_cretes_ne_depassent_jamais_le_plafond() {
        // Cas terrain : entrée à +8,4 dB au-dessus du plein échelle (micro-casque
        // amplifié par l'OS). Rien ne doit sortir au-dessus du plafond.
        let mut l = limiteur();
        let mut sig = sinus(48_000, 220.0, 2.62);
        l.process_block(&mut sig);
        let pic = sig.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(pic <= DEFAULT_CEILING + 1e-6, "pic de sortie {pic} au-dessus du plafond");
        assert!(l.take_max_reduction_db() > 6.0, "la réduction doit être signalée");
    }

    #[test]
    fn la_reduction_arrive_avant_la_crete_pas_apres() {
        // C'est tout l'intérêt du lookahead : sans lui, le premier cycle de la crête
        // sort écrêté. On envoie du silence puis une crête brutale.
        let mut l = limiteur();
        let mut sig = vec![0.0f32; 2400];
        sig.extend(sinus(4800, 1000.0, 2.5));
        l.process_block(&mut sig);
        let pic = sig.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(pic <= DEFAULT_CEILING + 1e-6, "crête brutale mal maîtrisée : {pic}");
        // Et le gain ne doit pas s'être effondré d'un coup (pas de clic) : on vérifie
        // que la transition s'étale sur plusieurs échantillons autour de l'attaque.
        let zone = &sig[2400 - l.latency_samples()..2400 + 200];
        let sauts = zone.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(sauts < 0.2, "transition trop brutale (saut max {sauts})");
    }

    #[test]
    fn reset_repart_proprement() {
        let mut l = limiteur();
        let mut sig = sinus(4800, 220.0, 3.0);
        l.process_block(&mut sig);
        l.reset();
        assert_eq!(l.gain, 1.0);
        assert!(l.maxq.is_empty());
        assert_eq!(l.take_max_reduction_db(), 0.0);
        // Après reset, un signal sous le plafond ressort intact.
        let entree = sinus(4800, 440.0, 0.3);
        let mut sortie = entree.clone();
        l.process_block(&mut sortie);
        let retard = l.latency_samples();
        assert_eq!(sortie[retard + 100], entree[100]);
    }
}
