//! Enveloppe de gate voix — transforme une décision binaire « parole présente »
//! (fournie par le VAD, une fois par bloc) en un **gain par échantillon lissé**
//! dans `[0, 1]`, avec attaque / maintien (hangover) / relâche.
//!
//! Rôle produit : quand l'utilisateur **ne parle pas**, le talkback est **coupé**
//! (gain → 0) ; quand il parle, le gain monte vite (attaque) puis reste ouvert un
//! court instant après le dernier mot (hangover) pour ne **pas couper les fins de
//! phrase ni « clignoter »** sur les micro-silences inter-mots, avant de relâcher.
//!
//! Ce module est **pur et déterministe** (aucune allocation, aucune I/O, aucun
//! modèle) → testé exhaustivement en isolation. Il ne connaît ni DeepFilterNet ni
//! Silero : il consomme seulement un booléen `speech` par bloc.

/// Constantes de temps de l'enveloppe. Les valeurs par défaut sont un point de
/// départ raisonnable pour de la voix parlée ; elles seront affinées en terrain
/// (Lot 2e).
#[derive(Debug, Clone, Copy)]
pub struct GateParams {
    /// Temps d'attaque (montée du gain) en millisecondes. Court = réactif.
    pub attack_ms: f32,
    /// Temps de relâche (descente du gain) en millisecondes.
    pub release_ms: f32,
    /// Maintien après la dernière trame de parole, en millisecondes : évite de
    /// couper les fins de phrase et lisse les silences inter-mots (anti-chatter).
    pub hangover_ms: f32,
}

impl Default for GateParams {
    fn default() -> Self {
        Self { attack_ms: 10.0, release_ms: 120.0, hangover_ms: 250.0 }
    }
}

/// Enveloppe de gate à un pôle, pilotée par une décision de parole par bloc.
#[derive(Debug, Clone)]
pub struct VoiceGate {
    attack_coeff: f32,
    release_coeff: f32,
    hangover_samples: u32,
    /// Gain courant appliqué, dans `[0, 1]`.
    gain: f32,
    /// Échantillons de maintien restants (> 0 ⇒ on garde la cible ouverte).
    hold: u32,
}

/// Coefficient one-pole `1 - exp(-1 / (tau·fs))` pour un temps `ms` donné.
/// `ms <= 0` ⇒ transition instantanée (coeff = 1).
fn one_pole_coeff(ms: f32, sample_rate: f32) -> f32 {
    if ms <= 0.0 {
        return 1.0;
    }
    let tau = ms / 1000.0;
    1.0 - (-1.0 / (tau * sample_rate)).exp()
}

impl VoiceGate {
    /// `sample_rate` en Hz (ex. 48000). `params` : cf. [`GateParams`].
    pub fn new(sample_rate: f32, params: GateParams) -> Self {
        debug_assert!(sample_rate > 0.0);
        Self {
            attack_coeff: one_pole_coeff(params.attack_ms, sample_rate),
            release_coeff: one_pole_coeff(params.release_ms, sample_rate),
            hangover_samples: (params.hangover_ms / 1000.0 * sample_rate).round() as u32,
            gain: 0.0,
            hold: 0,
        }
    }

    /// Réinitialise l'état (gain fermé, aucun maintien). À appeler à
    /// (ré)ouverture de capture / hot-swap device.
    pub fn reset(&mut self) {
        self.gain = 0.0;
        self.hold = 0;
    }

    /// Le gate est-il « ouvert » (gain significatif) ? Utilisé pour le voyant
    /// « à l'antenne » de la tranche voix.
    pub fn is_open(&self) -> bool {
        self.gain > 0.01
    }

    /// Gain courant (dernier calculé), dans `[0, 1]`.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Fait avancer l'enveloppe sur `out.len()` échantillons sous une **unique**
    /// décision `speech` (celle de la trame VAD courante) et écrit le gain
    /// par-échantillon dans `out`. À multiplier ensuite au signal voix.
    pub fn process_block(&mut self, speech: bool, out: &mut [f32]) {
        for g in out.iter_mut() {
            // 1) Cible : ouverte tant qu'il y a de la parole OU qu'un maintien
            //    est en cours ; fermée sinon.
            if speech {
                self.hold = self.hangover_samples;
            } else if self.hold > 0 {
                self.hold -= 1;
            }
            let target = if speech || self.hold > 0 { 1.0 } else { 0.0 };

            // 2) Rampe one-pole vers la cible (attaque si on monte, relâche sinon).
            let coeff = if target > self.gain { self.attack_coeff } else { self.release_coeff };
            self.gain += (target - self.gain) * coeff;
            // Garde-fou numérique : borne stricte dans [0, 1].
            self.gain = self.gain.clamp(0.0, 1.0);

            *g = self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(gate: &mut VoiceGate, speech: bool, n: usize) -> Vec<f32> {
        let mut out = vec![0.0; n];
        gate.process_block(speech, &mut out);
        out
    }

    #[test]
    fn silence_reste_ferme() {
        let mut g = VoiceGate::new(SR, GateParams::default());
        let out = run(&mut g, false, SR as usize); // 1 s de silence
        assert!(out.iter().all(|&x| x < 1e-3), "gain doit rester ~0 en silence");
        assert!(!g.is_open());
    }

    #[test]
    fn attaque_monte_vite_a_un() {
        let mut g = VoiceGate::new(SR, GateParams::default());
        // Après ~5× la constante d'attaque (10 ms → 50 ms), le gain doit être ~1.
        let out = run(&mut g, true, (SR * 0.05) as usize);
        let last = *out.last().unwrap();
        assert!(last > 0.98, "gain après 50 ms de parole = {last}, attendu ~1");
        assert!(g.is_open());
    }

    #[test]
    fn hangover_garde_ouvert_puis_relache() {
        let mut g = VoiceGate::new(SR, GateParams { attack_ms: 5.0, release_ms: 50.0, hangover_ms: 200.0 });
        run(&mut g, true, (SR * 0.1) as usize); // ouvre
        // Juste après l'arrêt, dans la fenêtre de hangover (100 ms < 200 ms) : ouvert.
        let during = run(&mut g, false, (SR * 0.1) as usize);
        assert!(*during.last().unwrap() > 0.9, "doit rester ouvert pendant le hangover");
        // Bien après hangover + relâche (500 ms) : fermé.
        let after = run(&mut g, false, (SR * 0.5) as usize);
        assert!(*after.last().unwrap() < 1e-2, "doit être fermé après hangover+relâche");
    }

    #[test]
    fn micro_silence_inter_mots_ne_coupe_pas() {
        // Anti-chatter : un trou de parole plus court que le hangover ne doit pas
        // faire retomber le gain de façon audible.
        let mut g = VoiceGate::new(SR, GateParams { attack_ms: 5.0, release_ms: 80.0, hangover_ms: 250.0 });
        run(&mut g, true, (SR * 0.1) as usize); // parle
        let gap = run(&mut g, false, (SR * 0.15) as usize); // 150 ms de trou < 250 ms hangover
        assert!(gap.iter().all(|&x| x > 0.9), "le gain ne doit pas chuter sur un trou < hangover");
    }

    #[test]
    fn gain_toujours_borne() {
        let mut g = VoiceGate::new(SR, GateParams::default());
        for &sp in &[true, false, true, true, false] {
            let out = run(&mut g, sp, 1000);
            assert!(out.iter().all(|&x| (0.0..=1.0).contains(&x) && x.is_finite()));
        }
    }

    #[test]
    fn reset_referme() {
        let mut g = VoiceGate::new(SR, GateParams::default());
        run(&mut g, true, (SR * 0.1) as usize);
        assert!(g.is_open());
        g.reset();
        assert_eq!(g.gain(), 0.0);
        assert!(!g.is_open());
    }

    #[test]
    fn attack_zero_est_instantane() {
        let mut g = VoiceGate::new(SR, GateParams { attack_ms: 0.0, release_ms: 50.0, hangover_ms: 100.0 });
        let out = run(&mut g, true, 1);
        assert!((out[0] - 1.0).abs() < 1e-6, "attaque 0 ms ⇒ gain=1 immédiat");
    }
}
