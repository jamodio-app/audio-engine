//! La rugosité du signal capté se concentre-t-elle au BORD des blocs ?
//!
//! # Pourquoi cette mesure existe
//!
//! Le 19/09/2026, un musicien décrit un son « horrible » en pleine session,
//! redémarre l'Audio Engine sans rien changer d'autre, et tout redevient normal.
//! Les deux instances sont identiques dans TOUT ce qu'on journalise : même
//! pilote, même buffer, même fréquence, mêmes latences déclarées, premier
//! callback à une milliseconde près. La différence n'était dans aucune mesure.
//!
//! L'analyse de l'enregistrement a montré ce qui les séparait : une **pluie de
//! micro-impulsions alignées sur le bord des blocs**, environ un bloc sur
//! quatre. Nos compteurs disent QUAND les blocs arrivent, COMBIEN DE TEMPS on
//! met à les traiter et À QUEL NIVEAU — jamais si la rugosité du signal se
//! concentre quelque part.
//!
//! # La statistique, et pourquoi c'est CELLE-LÀ
//!
//! Pour chaque bloc, on cherche **où** tombe le pic de rugosité (différence
//! seconde). Sur un signal sain, il tombe n'importe où : la part des blocs dont
//! le pic est près du bord vaut le hasard. Sur une prise abîmée, il tombe au
//! bord bien plus souvent. On publie donc le **rapport au hasard** : 1 = rien à
//! signaler, 2 et plus = la rugosité se groupe là où elle ne devrait pas.
//!
//! Deux statistiques plus simples ont été essayées et **mesurées insuffisantes**
//! sur l'enregistrement réel, avant d'arriver à celle-ci :
//! - le saut qui recolle deux blocs, comparé au mouvement juste après : lisait
//!   0,66 % sur la prise abîmée contre 2,94 % sur un micro calme — elle mesurait
//!   le bruit, pas le défaut, et criait au loup sur un micro tranquille ;
//! - la rugosité MOYENNE au bord rapportée à celle du milieu : 1,03 contre 1,00,
//!   soit rien du tout — la moyenne est noyée par le signal lui-même. Le défaut
//!   ne vit que dans la QUEUE de la distribution.
//!
//! Calibration sur données réelles (stem du 19/09) : prise abîmée **25,5 %**,
//! période saine 11-14 %, hasard 10,9 %.
//!
//! # Ce que ça coûte
//!
//! Une différence seconde par échantillon, sur le thread de capture — jamais
//! dans le callback audio. Même ordre que le calcul de RMS déjà fait à côté.
//!
//! # Ce que ça ne fait pas
//!
//! Ça ne corrige rien et n'accuse personne : une rugosité au bord peut venir du
//! pilote, de l'interface ou de nous. C'est un FAIT daté, à confronter au reste.

/// Distance au bord (en frames) en deçà de laquelle un pic compte comme « au
/// bord ». Trois, parce que le défaut mesuré s'étale sur quelques échantillons
/// de part et d'autre, pas sur un seul.
const TOLERANCE: usize = 3;

/// Concentration de la rugosité au bord des blocs, accumulée sur une fenêtre.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EdgeStats {
    /// Blocs examinés.
    pub blocks: u64,
    /// Blocs dont le pic de rugosité tombe près du bord.
    pub peak_at_edge: u64,
    /// Part attendue par pur hasard, en pourcentage — elle dépend de la taille
    /// du bloc, donc on la transporte avec la mesure plutôt que de la supposer.
    pub chance_pct: f32,
}

impl EdgeStats {
    /// Rapport au hasard : 1 = rien à signaler, 2 et plus = la rugosité se
    /// groupe au bord. `None` si rien n'a été examiné — jamais un chiffre
    /// rassurant quand on n'a rien regardé.
    pub fn edge_peak_ratio(&self) -> Option<f32> {
        if self.blocks == 0 || self.chance_pct <= 0.0 {
            return None;
        }
        let observed = 100.0 * self.peak_at_edge as f32 / self.blocks as f32;
        Some(observed / self.chance_pct)
    }
}

/// Observe la rugosité au bord des blocs d'un flux capté.
#[derive(Debug, Default)]
pub struct EdgeContinuity {
    /// Deux derniers échantillons du bloc précédent : la différence seconde qui
    /// enjambe le bord en a besoin.
    tail: Option<(f32, f32)>,
    blocks: u64,
    peak_at_edge: u64,
    chance_pct: f32,
}

impl EdgeContinuity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Examine un bloc INTERLEAVÉ de `channels` canaux. Seul le premier canal
    /// est suivi : une rugosité de transport les touche tous.
    pub fn observe(&mut self, block: &[f32], channels: usize) {
        if channels == 0 {
            return;
        }
        let frames = block.len() / channels;
        // Il faut de quoi placer un pic ailleurs qu'au bord pour que la mesure
        // ait un sens.
        if frames < 4 * TOLERANCE {
            return;
        }
        let at = |f: usize| block[f * channels];

        if let Some((a, b)) = self.tail {
            // Rugosité en chaque frame : |x[k-1] − 2·x[k] + x[k+1]|. Les deux
            // premières enjambent le bord grâce à la queue du bloc précédent.
            let mut best = f32::NEG_INFINITY;
            let mut best_k = 0usize;
            for k in 0..frames - 1 {
                let (p, c, n) = match k {
                    0 => (a, b, at(0)),
                    1 => (b, at(0), at(1)),
                    _ => (at(k - 2), at(k - 1), at(k)),
                };
                let r = (p - 2.0 * c + n).abs();
                if r > best {
                    best = r;
                    best_k = k;
                }
            }
            // `best_k` compte depuis le bord : 0 = juste au bord. La fin du bloc
            // est le bord du SUIVANT, donc elle compte aussi.
            let d = best_k.min(frames.saturating_sub(best_k));
            if d <= TOLERANCE {
                self.peak_at_edge += 1;
            }
            self.blocks += 1;
            self.chance_pct = 100.0 * (2 * TOLERANCE + 1) as f32 / frames as f32;
        }
        self.tail = Some((at(frames - 2), at(frames - 1)));
    }

    pub fn stats(&self) -> EdgeStats {
        EdgeStats {
            blocks: self.blocks,
            peak_at_edge: self.peak_at_edge,
            chance_pct: self.chance_pct,
        }
    }

    /// Rend la fenêtre écoulée et repart à zéro — la continuité du signal, elle,
    /// n'est PAS rompue (on garde la queue du bloc précédent).
    pub fn drain(&mut self) -> EdgeStats {
        let s = self.stats();
        self.blocks = 0;
        self.peak_at_edge = 0;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bloc_sinus(n: usize, phase: f32) -> Vec<f32> {
        (0..n).map(|i| ((i as f32 + phase) * 0.05).sin() * 0.3).collect()
    }

    #[test]
    fn un_signal_sain_reste_au_niveau_du_hasard() {
        let mut e = EdgeContinuity::new();
        for b in 0..400 {
            e.observe(&bloc_sinus(64, (b * 64) as f32), 1);
        }
        let r = e.stats().edge_peak_ratio().unwrap();
        assert!(r < 2.0, "signal continu : rapport au hasard {r}, attendu ~1");
    }

    #[test]
    fn une_impulsion_au_bord_de_chaque_bloc_est_vue() {
        let mut e = EdgeContinuity::new();
        for b in 0..400 {
            let mut v = bloc_sinus(64, (b * 64) as f32);
            v[0] += 0.5; // une impulsion pile au bord, à chaque bloc
            e.observe(&v, 1);
        }
        let r = e.stats().edge_peak_ratio().unwrap();
        assert!(r > 5.0, "rugosité groupée au bord : rapport {r}, attendu très grand");
    }

    #[test]
    fn une_impulsion_au_milieu_ne_declenche_rien() {
        // Le contrôle qui compte : un signal accidenté, mais pas au bord.
        let mut e = EdgeContinuity::new();
        for b in 0..400 {
            let mut v = bloc_sinus(64, (b * 64) as f32);
            v[32] += 0.5;
            e.observe(&v, 1);
        }
        let r = e.stats().edge_peak_ratio().unwrap();
        assert!(r < 1.0, "pic au milieu : rapport {r}, ne doit PAS accuser le bord");
    }

    #[test]
    fn sans_mesure_aucun_chiffre_invente() {
        let e = EdgeContinuity::new();
        assert_eq!(e.stats().edge_peak_ratio(), None);
        // Un bloc trop court pour qu'un pic puisse tomber ailleurs qu'au bord
        // ne produit aucune mesure plutôt qu'une mesure trompeuse.
        let mut e2 = EdgeContinuity::new();
        e2.observe(&[0.1; 8], 1);
        e2.observe(&[0.1; 8], 1);
        assert_eq!(e2.stats().blocks, 0);
    }

    #[test]
    fn le_hasard_depend_de_la_taille_du_bloc() {
        let mut petit = EdgeContinuity::new();
        for b in 0..50 {
            petit.observe(&bloc_sinus(64, (b * 64) as f32), 1);
        }
        let mut grand = EdgeContinuity::new();
        for b in 0..50 {
            grand.observe(&bloc_sinus(512, (b * 512) as f32), 1);
        }
        assert!(
            petit.stats().chance_pct > grand.stats().chance_pct,
            "un bloc plus long rend le hasard plus rare — la mesure doit le savoir"
        );
    }

    #[test]
    fn multicanal_le_premier_canal_est_suivi() {
        let mut e = EdgeContinuity::new();
        for b in 0..200 {
            let mono = bloc_sinus(64, (b * 64) as f32);
            let mut inter = Vec::with_capacity(64 * 4);
            for (i, s) in mono.iter().enumerate() {
                inter.push(*s);
                for c in 1..4 {
                    inter.push(if (i + c) % 2 == 0 { 0.9 } else { -0.9 });
                }
            }
            e.observe(&inter, 4);
        }
        let r = e.stats().edge_peak_ratio().unwrap();
        assert!(r < 2.0, "le bruit des autres canaux ne doit pas compter : {r}");
    }

    #[test]
    fn drain_rend_la_fenetre_sans_casser_la_continuite() {
        let mut e = EdgeContinuity::new();
        for b in 0..100 {
            e.observe(&bloc_sinus(64, (b * 64) as f32), 1);
        }
        let w = e.drain();
        assert_eq!(w.blocks, 99);
        assert_eq!(e.stats().blocks, 0);
        e.observe(&bloc_sinus(64, 6400.0), 1);
        assert_eq!(e.stats().blocks, 1, "la continuité n'est pas rompue par le drain");
    }
}
