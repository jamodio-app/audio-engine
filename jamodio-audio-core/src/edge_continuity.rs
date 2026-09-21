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
//! période saine 11-14 %, hasard 10,9 %. Chiffres obtenus avec la première
//! version du calcul, qui décalait les positions d'une frame et comptait 6
//! positions de bord pour un hasard annoncé de 7 (rapport ≈ 0,87 sur un signal
//! sain au lieu de 1) ; corrigé le 21/09/2026, le hasard vaut désormais
//! exactement `2·TOLERANCE / n` (9,4 % à 64 frames). Seuil d'alerte à
//! recalibrer sur une prise réelle.
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

/// Rugosité en deçà de laquelle un bloc n'est pas jugé : sous ~−100 dBFS, il
/// n'y a que du silence numérique ou le dernier bit du convertisseur. Sur de
/// tels blocs, le maximum est une ÉGALITÉ entre dizaines de positions, et la
/// première gagnait — c'est-à-dire le bord : le silence lisait 9,1 fois le
/// hasard, un bruit de ±1 LSB 24 bits 2,2 fois (revue du 21/09/2026), assez
/// pour déclencher l'alerte sans rien d'abîmé.
const ROUGHNESS_FLOOR: f32 = 1e-5;

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

    /// Examine le canal `channel` d'un bloc INTERLEAVÉ de `channels` canaux —
    /// le canal que le musicien a choisi, pas le premier venu : sur une
    /// interface à plusieurs entrées, le premier canal peut être vide.
    ///
    /// # Où tombent les positions
    ///
    /// La rugosité en `c` vaut |x[c−1] − 2·x[c] + x[c+1]|. Dans un bloc de `n`
    /// frames, on la calcule pour `c = −1 … n−2` : `c = −1` est la DERNIÈRE
    /// frame du bloc précédent, qu'on ne pouvait pas juger sans celui-ci ; la
    /// dernière frame du bloc courant le sera au bloc suivant. Chaque position
    /// est donc jugée exactement une fois, et le bord (entre `n−1` et `0`) est
    /// au milieu de la fenêtre, symétrique : `TOLERANCE` positions de chaque
    /// côté. Le hasard vaut `2·TOLERANCE / n`.
    pub fn observe(&mut self, block: &[f32], channels: usize, channel: usize) {
        if channels == 0 || channel >= channels {
            return;
        }
        let frames = block.len() / channels;
        let at = |f: usize| block[f * channels + channel];
        // Il faut de quoi placer un pic ailleurs qu'au bord pour que la mesure
        // ait un sens. Un bloc trop court coupe la continuité : sans ça, le bloc
        // suivant enjamberait un raccord fictif et compterait un pic au bord.
        if frames < 4 * TOLERANCE {
            self.tail = None;
            return;
        }

        if let Some((a, b)) = self.tail {
            let mut best = f32::NEG_INFINITY;
            let mut best_r = 0usize;
            let mut tie = false;
            // `r` = position modulo le bloc : n−1 pour la frame d'avant le bord.
            for c in -1i64..=(frames as i64 - 2) {
                let (p, x, n) = match c {
                    -1 => (a, b, at(0)),
                    0 => (b, at(0), at(1)),
                    _ => {
                        let c = c as usize;
                        (at(c - 1), at(c), at(c + 1))
                    }
                };
                let v = (p - 2.0 * x + n).abs();
                let r = if c < 0 { frames - 1 } else { c as usize };
                if v > best {
                    best = v;
                    best_r = r;
                    tie = false;
                } else if v == best {
                    tie = true;
                }
            }
            // Un bloc sans relief, ou dont le maximum est partagé, ne dit rien
            // de l'endroit où la rugosité se groupe : on ne le juge pas.
            if best.is_finite() && best >= ROUGHNESS_FLOOR && !tie {
                // Distance au bord en positions : 1 pour les deux frames qui le
                // touchent (r = n−1 et r = 0), 2 pour les suivantes…
                let d = (best_r + 1).min(frames - best_r);
                if d <= TOLERANCE {
                    self.peak_at_edge += 1;
                }
                self.blocks += 1;
                self.chance_pct = 100.0 * (2 * TOLERANCE) as f32 / frames as f32;
            }
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
            e.observe(&bloc_sinus(64, (b * 64) as f32), 1, 0);
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
            e.observe(&v, 1, 0);
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
            e.observe(&v, 1, 0);
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
        e2.observe(&[0.1; 8], 1, 0);
        e2.observe(&[0.1; 8], 1, 0);
        assert_eq!(e2.stats().blocks, 0);
    }

    #[test]
    fn le_hasard_depend_de_la_taille_du_bloc() {
        let mut petit = EdgeContinuity::new();
        for b in 0..50 {
            petit.observe(&bloc_sinus(64, (b * 64) as f32), 1, 0);
        }
        let mut grand = EdgeContinuity::new();
        for b in 0..50 {
            grand.observe(&bloc_sinus(512, (b * 512) as f32), 1, 0);
        }
        assert!(
            petit.stats().chance_pct > grand.stats().chance_pct,
            "un bloc plus long rend le hasard plus rare — la mesure doit le savoir"
        );
    }

    #[test]
    fn multicanal_seul_le_canal_choisi_est_suivi() {
        // Instrument sur l'entrée 3 d'une interface à 4 canaux ; les autres
        // portent une impulsion au bord de chaque bloc. Seul le canal choisi
        // doit compter — avant le 21/09, on suivait toujours le canal 0.
        let mut choisi = EdgeContinuity::new();
        let mut premier = EdgeContinuity::new();
        for b in 0..200 {
            let mono = bloc_sinus(64, (b * 64) as f32);
            let mut inter = Vec::with_capacity(64 * 4);
            for (i, s) in mono.iter().enumerate() {
                for c in 0..4 {
                    let parasite = if i == 0 { 0.5 } else { 0.01 * ((i * 7 + c) % 5) as f32 };
                    inter.push(if c == 2 { *s } else { parasite });
                }
            }
            choisi.observe(&inter, 4, 2);
            premier.observe(&inter, 4, 0);
        }
        let r = choisi.stats().edge_peak_ratio().unwrap();
        assert!(r < 2.0, "le canal choisi est sain : {r}");
        let r0 = premier.stats().edge_peak_ratio().unwrap();
        assert!(r0 > 5.0, "et le canal 0 abîmé est bien vu quand c'est lui qu'on suit : {r0}");
    }

    #[test]
    fn un_canal_hors_plage_ne_mesure_rien() {
        let mut e = EdgeContinuity::new();
        for b in 0..10 {
            e.observe(&bloc_sinus(128, (b * 64) as f32), 2, 2);
        }
        assert_eq!(e.stats().edge_peak_ratio(), None);
    }

    /// Revue du 21/09/2026 : le silence numérique lisait 9,1 fois le hasard.
    #[test]
    fn le_silence_ne_crie_pas_au_loup() {
        let mut e = EdgeContinuity::new();
        for _ in 0..500 {
            e.observe(&[0.0; 64], 1, 0);
        }
        assert_eq!(e.stats().edge_peak_ratio(), None, "rien à juger, aucun chiffre");
    }

    /// … et un bruit de ±1 LSB 24 bits, 2,2 fois.
    #[test]
    fn le_dernier_bit_du_convertisseur_ne_crie_pas_au_loup() {
        const LSB: f32 = 1.0 / 8_388_608.0;
        let mut graine = 12345u32;
        let mut e = EdgeContinuity::new();
        for _ in 0..500 {
            let bloc: Vec<f32> = (0..64)
                .map(|_| {
                    graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((graine >> 30) as f32 - 1.0) * LSB
                })
                .collect();
            e.observe(&bloc, 1, 0);
        }
        assert_eq!(e.stats().edge_peak_ratio(), None);
    }

    /// La base : sur un bruit franc, le pic tombe n'importe où — rapport ≈ 1,
    /// et non plus ≈ 0,87 comme avec le décalage d'une frame.
    #[test]
    fn un_bruit_franc_lit_le_hasard() {
        let mut graine = 987_654_321u32;
        let mut e = EdgeContinuity::new();
        for _ in 0..20_000 {
            let bloc: Vec<f32> = (0..64)
                .map(|_| {
                    graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (graine as f32 / u32::MAX as f32 - 0.5) * 0.2
                })
                .collect();
            e.observe(&bloc, 1, 0);
        }
        let r = e.stats().edge_peak_ratio().unwrap();
        assert!((0.9..1.1).contains(&r), "bruit blanc : rapport {r}, attendu ≈ 1");
    }

    #[test]
    fn un_bloc_trop_court_coupe_la_continuite() {
        let mut e = EdgeContinuity::new();
        e.observe(&bloc_sinus(64, 0.0), 1, 0);
        e.observe(&[0.9; 8], 1, 0); // bloc trop court : ignoré…
        // … et le suivant ne doit pas être raccordé au bloc d'avant.
        e.observe(&bloc_sinus(64, 64.0), 1, 0);
        assert_eq!(e.stats().blocks, 0, "pas de raccord fictif après un bloc ignoré");
    }

    #[test]
    fn drain_rend_la_fenetre_sans_casser_la_continuite() {
        let mut e = EdgeContinuity::new();
        for b in 0..100 {
            e.observe(&bloc_sinus(64, (b * 64) as f32), 1, 0);
        }
        let w = e.drain();
        // 99 raccords, moins les rares blocs d'une sinusoïde lisse dont le
        // maximum est une égalité exacte (non jugés, par construction).
        assert!((95..=99).contains(&w.blocks), "{}", w.blocks);
        assert_eq!(e.stats().blocks, 0);
        // Un bloc au maximum sans ambiguïté : il n'est jugé que si la queue du
        // bloc d'avant le drain a survécu.
        let mut v = bloc_sinus(64, 6400.0);
        v[32] += 0.5;
        e.observe(&v, 1, 0);
        assert_eq!(e.stats().blocks, 1, "la continuité n'est pas rompue par le drain");
    }
}
