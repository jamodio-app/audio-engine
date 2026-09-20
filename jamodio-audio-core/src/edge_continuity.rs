//! La prise est-elle CONTINUE au bord des blocs livrés par le pilote ?
//!
//! # Pourquoi cette mesure existe
//!
//! Le 19/09/2026, un musicien décrit un son « horrible » pendant une session,
//! redémarre l'Audio Engine sans rien changer d'autre, et tout redevient normal.
//! Les deux instances sont identiques dans TOUT ce que l'agent journalise :
//! même pilote, même taille de buffer, même fréquence, mêmes latences déclarées,
//! premier callback à une milliseconde près. La différence n'était donc dans
//! aucune de nos mesures.
//!
//! L'analyse de l'enregistrement a montré ce qui distinguait les deux : une
//! discontinuité du signal **au bord des blocs**, environ un bloc sur quatre.
//! Ni le niveau, ni le temps de traitement, ni le pilote n'en disaient rien —
//! parce qu'on mesurait *quand* les blocs arrivent et *à quel point* ils sont
//! forts, jamais si le signal se **recolle** d'un bloc au suivant.
//!
//! # Ce que ça coûte
//!
//! Trois soustractions par BLOC (750/s à 64 frames), pas par échantillon. Et
//! c'est lu sur le thread de capture, jamais dans le callback audio.
//!
//! # Ce que ça ne fait pas
//!
//! Ça ne corrige rien et ça n'accuse personne : un bord rugueux peut venir du
//! pilote, de l'interface ou de nous. C'est un FAIT daté, à confronter au reste.

/// Un bord est « rugueux » quand le saut qui recolle deux blocs dépasse ce
/// facteur fois le mouvement du signal juste après. Sur un signal continu, le
/// bord ne se distingue pas de son voisinage ; à 4×, il est franchement à part.
const ROUGH_FACTOR: f32 = 4.0;

/// En dessous, on ne juge pas : sur du quasi-silence, tout rapport est du bruit.
const FLOOR: f32 = 1e-4;

/// Continuité au bord des blocs, accumulée sur une fenêtre.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EdgeStats {
    /// Bords examinés (= blocs, moins le tout premier).
    pub edges: u64,
    /// Bords dont le saut sort nettement de l'ordinaire.
    pub rough: u64,
}

impl EdgeStats {
    /// Part de bords rugueux, en pourcentage. `None` si rien n'a été examiné —
    /// jamais un zéro qui ressemblerait à « tout va bien ».
    pub fn rough_pct(&self) -> Option<f32> {
        if self.edges == 0 {
            return None;
        }
        Some(100.0 * self.rough as f32 / self.edges as f32)
    }
}

/// Observe la continuité entre blocs successifs d'un même flux capté.
#[derive(Debug, Default)]
pub struct EdgeContinuity {
    /// Dernier échantillon du bloc précédent.
    last: Option<f32>,
    stats: EdgeStats,
}

impl EdgeContinuity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Examine un bloc INTERLEAVÉ de `channels` canaux. Seul le premier canal
    /// est suivi : une discontinuité de transport les touche tous, et suivre un
    /// seul canal suffit à la voir tout en gardant le coût constant.
    pub fn observe(&mut self, block: &[f32], channels: usize) {
        if channels == 0 || block.len() < channels * 3 {
            return;
        }
        let x0 = block[0];
        let x1 = block[channels];
        let x2 = block[2 * channels];
        if let Some(last) = self.last {
            // Le saut qui recolle les deux blocs…
            let edge = (x0 - last).abs();
            // …comparé à ce que le signal fait juste après, à l'intérieur.
            let inside = (x1 - x0).abs().max((x2 - x1).abs());
            if edge > FLOOR && edge > ROUGH_FACTOR * inside {
                self.stats.rough += 1;
            }
            self.stats.edges += 1;
        }
        self.last = Some(block[block.len() - channels]);
    }

    /// Instantané cumulé.
    pub fn stats(&self) -> EdgeStats {
        self.stats
    }

    /// Rend la fenêtre écoulée et repart à zéro — la continuité du signal, elle,
    /// n'est PAS rompue (on garde le dernier échantillon).
    pub fn drain(&mut self) -> EdgeStats {
        std::mem::take(&mut self.stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une sinusoïde découpée en blocs : les bords ne se distinguent en rien.
    fn sinus(n: usize, phase: f32) -> Vec<f32> {
        (0..n)
            .map(|i| ((i as f32 + phase) * 0.05).sin() * 0.3)
            .collect()
    }

    #[test]
    fn un_signal_continu_na_aucun_bord_rugueux() {
        let mut e = EdgeContinuity::new();
        for b in 0..100 {
            e.observe(&sinus(64, (b * 64) as f32), 1);
        }
        let s = e.stats();
        assert_eq!(s.edges, 99);
        assert_eq!(s.rough, 0, "aucun bord ne doit ressortir sur un signal continu");
        assert_eq!(s.rough_pct(), Some(0.0));
    }

    #[test]
    fn un_saut_a_chaque_bord_est_vu() {
        // Chaque bloc repart d'une valeur sans rapport avec la fin du précédent.
        let mut e = EdgeContinuity::new();
        for b in 0..100 {
            let offset = if b % 2 == 0 { 0.0 } else { 1000.0 };
            e.observe(&sinus(64, offset), 1);
        }
        let pct = e.stats().rough_pct().unwrap();
        assert!(pct > 50.0, "bords cassés à répétition, got {pct} %");
    }

    #[test]
    fn le_quasi_silence_ne_declenche_rien() {
        // Sur du bruit à -100 dBFS, tout rapport est du hasard : on ne juge pas.
        let mut e = EdgeContinuity::new();
        for b in 0..200 {
            let v: Vec<f32> = (0..64)
                .map(|i| if (i + b) % 7 == 0 { 1e-6 } else { -1e-6 })
                .collect();
            e.observe(&v, 1);
        }
        assert_eq!(e.stats().rough, 0, "pas de verdict sur du silence");
    }

    #[test]
    fn multicanal_le_premier_canal_est_suivi() {
        // 4 canaux entrelacés : le canal 0 est continu, les autres sont du bruit.
        let mut e = EdgeContinuity::new();
        for b in 0..50 {
            let mono = sinus(64, (b * 64) as f32);
            let mut inter = Vec::with_capacity(64 * 4);
            for (i, s) in mono.iter().enumerate() {
                inter.push(*s);
                for c in 1..4 {
                    inter.push(if (i + c) % 2 == 0 { 0.9 } else { -0.9 });
                }
            }
            e.observe(&inter, 4);
        }
        assert_eq!(e.stats().rough, 0, "le bruit des autres canaux ne doit pas compter");
    }

    #[test]
    fn sans_mesure_aucun_chiffre_invente() {
        let e = EdgeContinuity::new();
        assert_eq!(e.stats().rough_pct(), None);
        // Un bloc trop court pour être jugé ne crée pas de fausse mesure.
        let mut e2 = EdgeContinuity::new();
        e2.observe(&[0.1, 0.2], 1);
        assert_eq!(e2.stats().edges, 0);
    }

    #[test]
    fn drain_rend_la_fenetre_sans_casser_la_continuite() {
        let mut e = EdgeContinuity::new();
        for b in 0..10 {
            e.observe(&sinus(64, (b * 64) as f32), 1);
        }
        let w = e.drain();
        assert_eq!(w.edges, 9);
        assert_eq!(e.stats().edges, 0, "la fenêtre repart à zéro");
        // Le bloc suivant est toujours comparé au précédent : pas de faux bord.
        e.observe(&sinus(64, 640.0), 1);
        assert_eq!(e.stats().rough, 0, "la continuité n'est pas rompue par le drain");
    }
}
