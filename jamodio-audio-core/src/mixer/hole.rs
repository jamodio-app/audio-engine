//! Dire POURQUOI un trou a été rendu — Lot 1-A du plan « ms en trop sur PC »
//! (`PLAN-LOT1-MS-PC-2026-09.md`, 23/09/2026).
//!
//! # Le problème
//!
//! Sur PC, 1,4 trou/min font monter le plancher du tampon à 17 ms, alors que la
//! gigue mesurée est normale. Mais cette gigue est un écart p95 − p10 sur ~1,3 s
//! (`sync::jitter`) : elle est aveugle, par construction, à un événement qui
//! touche 0,006 % des paquets. Et le remplissage minimal relevé aux arrivées
//! (`fillMinMs`) tombe à ~0 dans toute fenêtre qui contient un trou : il décrit
//! le régime, pas le trou. Aucun compteur existant ne dit ce qui s'est passé
//! AU trou — d'où ce module.
//!
//! # Ce qu'il fait
//!
//! Il reçoit les faits relevés autour d'un trou et rend une cause, sans état,
//! pour être testé aux bords sans réseau ni carte son. Les faits bruts sont
//! journalisés à côté de la cause : si le classement se révèle mal posé, la
//! session reste relisable.
//!
//! **Rien de tout ceci ne tourne dans le callback audio** : le tirage ne fait
//! que relever `HoleAtPull` (`ring_buffer`), le thread de décodage fait le reste.

/// Au-delà de combien de blocs de sortie une consommation plus rapide que le
/// temps écoulé est anormale.
///
/// Un tirage prend un bloc d'un coup, et un pilote ASIO rappelle par à-coups
/// (~945 / ~2 300 µs pour 1 333 µs nominaux, mesuré le 05/09) : entre deux
/// instants, la sortie peut avoir pris jusqu'à ~un bloc d'avance sur le temps.
/// Deux blocs laissent une marge d'un bloc au-dessus de ce régime sain. CONSTANTE
/// DE CLASSEMENT, pas de réglage audio : les chiffres bruts sont journalisés avec
/// chaque trou pour pouvoir la reprendre.
pub const CONSUMPTION_TOLERANCE_BLOCKS: f64 = 2.0;

/// Taille de bloc supposée tant que la sortie n'a pas mesuré la sienne : une
/// trame Opus (cf. `conceal::FRAME_MS`).
const UNKNOWN_BLOCK_MS: f64 = super::conceal::FRAME_MS;

/// Pourquoi le tampon n'avait pas de quoi servir le tirage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoleCause {
    /// Le paquet suivant n'était pas encore arrivé (horodaté) à l'instant du
    /// trou. Réseau, OU fil de réception pas ordonnancé avant l'horodatage : ce
    /// relevé ne sépare pas les deux (Lot 1-D).
    Arrival,
    /// Le paquet suivant était arrivé avant le trou, mais pas encore dans le
    /// tampon : le thread de décodage était en retard.
    Decode,
    /// Les paquets étaient là à temps : la sortie a tiré plus que le temps
    /// écoulé (rafale de callbacks).
    Consumption,
    /// Un paquet est arrivé mais a été écarté (en retard après un masquage,
    /// doublon, saut de numérotation).
    Sequence,
    /// Faits manquants ou incohérents : on le dit, on ne devine pas.
    Unclassified,
}

impl HoleCause {
    /// Nom stable, écrit dans le journal.
    pub fn as_str(self) -> &'static str {
        match self {
            HoleCause::Arrival => "arrival",
            HoleCause::Decode => "decode",
            HoleCause::Consumption => "consumption",
            HoleCause::Sequence => "sequence",
            HoleCause::Unclassified => "unclassified",
        }
    }
}

/// Faits relevés autour d'un trou. Tous les instants viennent de la même
/// horloge monotone ; les durées sont en ms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoleFacts {
    /// Remplissage juste après le dernier `push` avant le trou.
    pub fill_at_last_push_ms: f64,
    /// Temps écoulé entre ce `push` et le trou.
    pub since_last_push_ms: f64,
    /// Ce que la sortie a RÉELLEMENT consommé entre les deux (index de lecture).
    pub consumed_since_push_ms: f64,
    /// Bloc de sortie mesuré ; `0` s'il n'est pas encore connu.
    pub output_block_ms: f64,
    /// Le paquet dont le `push` a rendu le trou était-il déjà arrivé à
    /// l'instant du trou ? `None` : aucun paquet n'a encore suivi le trou (le
    /// `push` était une trame de masquage).
    pub next_received_before_hole: Option<bool>,
    /// Paquets écartés (tardifs, doublons, sauts) entre le dernier `push` et
    /// celui qui a rendu le trou.
    pub discarded_since_push: u64,
}

/// Classe un trou. L'ordre des questions compte :
///
/// 1. **Faits inexploitables** → `Unclassified`.
/// 2. **La sortie a-t-elle tiré plus que le temps ?** Si oui, même un paquet en
///    retard n'y est pour rien : à consommation normale, le tampon aurait tenu
///    plus longtemps. C'est la seule cause qui ne dépend pas de l'arrivée.
/// 3. **Un paquet a-t-il été écarté ?** Il était là, on l'a jeté.
/// 4. **Le paquet suivant était-il arrivé ?** Oui → le décodage était en retard ;
///    non → il n'était pas arrivé.
pub fn classify(f: &HoleFacts) -> HoleCause {
    let finite = f.fill_at_last_push_ms.is_finite()
        && f.since_last_push_ms.is_finite()
        && f.consumed_since_push_ms.is_finite();
    if !finite || f.since_last_push_ms < 0.0 || f.consumed_since_push_ms < 0.0 {
        return HoleCause::Unclassified;
    }
    let block = if f.output_block_ms.is_finite() && f.output_block_ms > 0.0 {
        f.output_block_ms
    } else {
        UNKNOWN_BLOCK_MS
    };
    if f.consumed_since_push_ms - f.since_last_push_ms > CONSUMPTION_TOLERANCE_BLOCKS * block {
        return HoleCause::Consumption;
    }
    if f.discarded_since_push > 0 {
        return HoleCause::Sequence;
    }
    match f.next_received_before_hole {
        Some(true) => HoleCause::Decode,
        Some(false) | None => HoleCause::Arrival,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bloc ASIO / CoreAudio mesuré sur nos deux plateformes : 64 frames.
    const BLOC: f64 = 64.0 * 1000.0 / 48_000.0;

    /// Un trou « ordinaire » : 12 ms au dernier push, 12 ms plus tard la
    /// sortie a tout pris au rythme du temps, et rien n'est arrivé.
    fn faits() -> HoleFacts {
        HoleFacts {
            fill_at_last_push_ms: 12.0,
            since_last_push_ms: 12.0,
            consumed_since_push_ms: 11.5,
            output_block_ms: BLOC,
            next_received_before_hole: Some(false),
            discarded_since_push: 0,
        }
    }

    #[test]
    fn un_paquet_pas_encore_arrive_est_une_cause_d_arrivee() {
        assert_eq!(classify(&faits()), HoleCause::Arrival);
    }

    #[test]
    fn sans_paquet_depuis_le_trou_c_est_aussi_l_arrivee() {
        let f = HoleFacts { next_received_before_hole: None, ..faits() };
        assert_eq!(classify(&f), HoleCause::Arrival);
    }

    #[test]
    fn un_paquet_arrive_avant_le_trou_mais_pas_pousse_accuse_le_decodage() {
        let f = HoleFacts { next_received_before_hole: Some(true), ..faits() };
        assert_eq!(classify(&f), HoleCause::Decode);
    }

    #[test]
    fn un_paquet_ecarte_passe_avant_l_arrivee_et_le_decodage() {
        let f = HoleFacts { discarded_since_push: 1, ..faits() };
        assert_eq!(classify(&f), HoleCause::Sequence);
        let f = HoleFacts { discarded_since_push: 1, next_received_before_hole: Some(true), ..faits() };
        assert_eq!(classify(&f), HoleCause::Sequence);
    }

    /// La sortie a pris 12 ms en 6 ms : aucun paquet n'aurait suffi.
    #[test]
    fn une_sortie_qui_tire_plus_que_le_temps_est_une_cause_de_consommation() {
        let f = HoleFacts { since_last_push_ms: 6.0, consumed_since_push_ms: 12.0, ..faits() };
        assert_eq!(classify(&f), HoleCause::Consumption);
        // Même avec un paquet écarté : c'est la consommation qui a vidé le tampon.
        let f = HoleFacts { discarded_since_push: 3, ..f };
        assert_eq!(classify(&f), HoleCause::Consumption);
    }

    /// Le régime sain d'un pilote par à-coups (un bloc d'avance) ne compte pas.
    #[test]
    fn un_bloc_d_avance_est_le_regime_normal_pas_une_rafale() {
        let f = HoleFacts { since_last_push_ms: 10.0, consumed_since_push_ms: 10.0 + BLOC, ..faits() };
        assert_eq!(classify(&f), HoleCause::Arrival);
        // Pile à la tolérance : pas encore une rafale.
        let f = HoleFacts {
            since_last_push_ms: 10.0,
            consumed_since_push_ms: 10.0 + CONSUMPTION_TOLERANCE_BLOCKS * BLOC,
            ..faits()
        };
        assert_eq!(classify(&f), HoleCause::Arrival);
    }

    /// Bloc inconnu : on juge contre une trame, pas contre zéro.
    #[test]
    fn sans_bloc_mesure_la_tolerance_vaut_une_trame_par_bloc() {
        let f = HoleFacts {
            output_block_ms: 0.0,
            since_last_push_ms: 10.0,
            consumed_since_push_ms: 14.0,
            ..faits()
        };
        assert_eq!(classify(&f), HoleCause::Arrival, "4 ms < 2 × 2,5 ms");
    }

    #[test]
    fn des_faits_absurdes_ne_sont_pas_classes() {
        for f in [
            HoleFacts { since_last_push_ms: f64::NAN, ..faits() },
            HoleFacts { consumed_since_push_ms: f64::INFINITY, ..faits() },
            HoleFacts { fill_at_last_push_ms: f64::NAN, ..faits() },
            HoleFacts { since_last_push_ms: -1.0, ..faits() },
        ] {
            assert_eq!(classify(&f), HoleCause::Unclassified, "{f:?}");
        }
    }
}
