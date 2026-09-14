//! Classement des paquets RTP reçus selon leur numéro de séquence (RFC 3550 §A.1).
//!
//! Le récepteur joue le son dans l'ordre et masque (PLC) les paquets absents. Un
//! paquet qui arrive APRÈS que sa place a été dépassée ne doit donc plus être joué :
//! le décoder à ce moment le ferait entendre hors de sa place, et le paquet suivant
//! verrait un faux trou (masquage de trop, buffer allongé). Ce module dit, pour
//! chaque paquet, s'il faut le jouer et combien de paquets manquent juste avant.
//!
//! Il tient aussi les compteurs cumulés du flux, au sens du RFC 3550 : un paquet en
//! retard compte comme reçu (le réseau l'a livré), et comme `late` (trop tard pour
//! être joué).
//!
//! Calcul pur, O(1), sans allocation : appelé depuis le thread de décodage RT.

/// Au-delà de cet écart en avant, le saut n'est pas une perte mais une reprise du
/// flux (émetteur redémarré) : il faut deux paquets consécutifs pour la confirmer.
/// 3000 paquets = 7,5 s de son à 2,5 ms par paquet.
pub const MAX_DROPOUT: u16 = 3000;

/// Écart en arrière encore reconnu comme un paquet en retard ou en double.
/// 100 paquets = 250 ms. Au-delà, c'est un saut (cf. [`MAX_DROPOUT`]).
pub const MAX_MISORDER: u16 = 100;

/// Nombre de paquets récents dont on se souvient (bit à 1 = reçu). Couvre
/// [`MAX_MISORDER`] pour distinguer exactement retard et double.
const HISTORY_BITS: u16 = 128;

/// Ce que le récepteur doit faire du paquet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// Premier paquet du flux, ou reprise confirmée après un saut : à jouer, sans
    /// perte déduite (l'écart d'une reprise ne dit rien du réseau).
    Start,
    /// Dans l'ordre : à jouer. `missing` paquets manquent juste avant lui.
    Next { missing: u16 },
    /// Arrivé après que sa place a été dépassée : à ne PAS jouer.
    Late,
    /// Déjà reçu : à ne pas jouer.
    Duplicate,
    /// Saut trop grand, en attente de confirmation par le paquet suivant : à ne pas
    /// jouer.
    Jump,
}

impl Arrival {
    /// Le paquet doit-il être décodé et joué ?
    pub fn is_playable(self) -> bool {
        matches!(self, Arrival::Start | Arrival::Next { .. })
    }
}

/// Compteurs cumulés d'un flux reçu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeqCounters {
    /// Paquets attendus : places couvertes par les numéros de séquence, reprises
    /// exclues.
    pub expected: u64,
    /// Paquets distincts reçus, en retard compris.
    pub received: u64,
    /// Paquets reçus trop tard pour être joués.
    pub late: u64,
}

impl SeqCounters {
    /// Paquets jamais arrivés (attendus − reçus).
    pub fn lost(&self) -> u64 {
        self.expected.saturating_sub(self.received)
    }
}

/// Suivi des numéros de séquence d'un flux reçu.
#[derive(Debug, Default)]
pub struct SeqTracker {
    /// Plus haut numéro de séquence reçu (au sens circulaire).
    highest: Option<u16>,
    /// Bit `i` = paquet `highest - i` reçu.
    history: u128,
    /// Numéro qui confirmerait la reprise après un saut (`Jump`).
    resync_seq: Option<u16>,
    counters: SeqCounters,
}

impl SeqTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn counters(&self) -> SeqCounters {
        self.counters
    }

    /// Classe le paquet `seq` et met à jour les compteurs.
    pub fn on_packet(&mut self, seq: u16) -> Arrival {
        let Some(highest) = self.highest else {
            return self.start(seq);
        };
        let ahead = seq.wrapping_sub(highest);
        if ahead == 0 {
            return Arrival::Duplicate;
        }
        if ahead < MAX_DROPOUT {
            self.history = if ahead >= HISTORY_BITS {
                1
            } else {
                (self.history << ahead) | 1
            };
            self.highest = Some(seq);
            self.resync_seq = None;
            self.counters.expected += u64::from(ahead);
            self.counters.received += 1;
            return Arrival::Next { missing: ahead - 1 };
        }
        let behind = highest.wrapping_sub(seq);
        if behind < MAX_MISORDER {
            let bit = 1u128 << behind;
            if self.history & bit != 0 {
                return Arrival::Duplicate;
            }
            self.history |= bit;
            self.counters.received += 1;
            self.counters.late += 1;
            return Arrival::Late;
        }
        if self.resync_seq == Some(seq) {
            return self.start(seq);
        }
        self.resync_seq = Some(seq.wrapping_add(1));
        Arrival::Jump
    }

    fn start(&mut self, seq: u16) -> Arrival {
        self.highest = Some(seq);
        self.history = 1;
        self.resync_seq = None;
        self.counters.expected += 1;
        self.counters.received += 1;
        Arrival::Start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tracker: &mut SeqTracker, seqs: &[u16]) -> Vec<Arrival> {
        seqs.iter().map(|&s| tracker.on_packet(s)).collect()
    }

    #[test]
    fn flux_dans_l_ordre() {
        let mut t = SeqTracker::new();
        let arrivals = feed(&mut t, &[10, 11, 12]);
        assert_eq!(
            arrivals,
            vec![
                Arrival::Start,
                Arrival::Next { missing: 0 },
                Arrival::Next { missing: 0 }
            ]
        );
        assert_eq!(
            t.counters(),
            SeqCounters {
                expected: 3,
                received: 3,
                late: 0
            }
        );
        assert_eq!(t.counters().lost(), 0);
    }

    #[test]
    fn un_trou_est_une_perte() {
        let mut t = SeqTracker::new();
        let arrivals = feed(&mut t, &[10, 13]);
        assert_eq!(arrivals[1], Arrival::Next { missing: 2 });
        assert_eq!(t.counters().expected, 4);
        assert_eq!(t.counters().lost(), 2);
    }

    #[test]
    fn un_paquet_en_retard_n_est_pas_joue_et_ne_cree_pas_de_faux_trou() {
        // 101 arrive après 102 : il ne doit être ni joué, ni suivi d'un faux trou
        // au paquet 103 (le bug corrigé).
        let mut t = SeqTracker::new();
        let arrivals = feed(&mut t, &[100, 102, 101, 103]);
        assert_eq!(
            arrivals,
            vec![
                Arrival::Start,
                Arrival::Next { missing: 1 },
                Arrival::Late,
                Arrival::Next { missing: 0 },
            ]
        );
        assert!(!Arrival::Late.is_playable());
        // Le réseau a tout livré : aucune perte, un retard.
        assert_eq!(
            t.counters(),
            SeqCounters {
                expected: 4,
                received: 4,
                late: 1
            }
        );
        assert_eq!(t.counters().lost(), 0);
    }

    #[test]
    fn un_double_est_ignore() {
        let mut t = SeqTracker::new();
        let arrivals = feed(&mut t, &[5, 6, 6, 5]);
        assert_eq!(arrivals[2], Arrival::Duplicate);
        assert_eq!(arrivals[3], Arrival::Duplicate);
        assert_eq!(
            t.counters(),
            SeqCounters {
                expected: 2,
                received: 2,
                late: 0
            }
        );
    }

    #[test]
    fn un_retard_deja_rattrape_devient_un_double() {
        let mut t = SeqTracker::new();
        let arrivals = feed(&mut t, &[1, 3, 2, 2]);
        assert_eq!(arrivals[2], Arrival::Late);
        assert_eq!(arrivals[3], Arrival::Duplicate);
        assert_eq!(t.counters().late, 1);
    }

    #[test]
    fn passage_par_zero() {
        let mut t = SeqTracker::new();
        let arrivals = feed(&mut t, &[65534, 65535, 0, 2, 1]);
        assert_eq!(
            arrivals,
            vec![
                Arrival::Start,
                Arrival::Next { missing: 0 },
                Arrival::Next { missing: 0 },
                Arrival::Next { missing: 1 },
                Arrival::Late,
            ]
        );
        assert_eq!(t.counters().lost(), 0);
    }

    #[test]
    fn retard_au_bord_de_la_fenetre() {
        let mut t = SeqTracker::new();
        t.on_packet(0);
        t.on_packet(MAX_MISORDER);
        // Place 1 : `MAX_MISORDER - 1` en arrière, encore reconnue.
        assert_eq!(t.on_packet(1), Arrival::Late);
    }

    #[test]
    fn reprise_confirmee_par_deux_paquets_consecutifs() {
        // L'émetteur redémarre sa numérotation : le premier paquet est mis de côté,
        // le suivant confirme la reprise ; l'écart n'est pas compté comme perte.
        let mut t = SeqTracker::new();
        feed(&mut t, &[40_000, 40_001]);
        assert_eq!(t.on_packet(0), Arrival::Jump);
        assert_eq!(t.on_packet(1), Arrival::Start);
        assert_eq!(t.on_packet(2), Arrival::Next { missing: 0 });
        assert_eq!(
            t.counters(),
            SeqCounters {
                expected: 4,
                received: 4,
                late: 0
            }
        );
    }

    #[test]
    fn un_saut_isole_n_ecrase_pas_le_flux() {
        let mut t = SeqTracker::new();
        feed(&mut t, &[10, 11]);
        assert_eq!(t.on_packet(30_000), Arrival::Jump);
        assert_eq!(t.on_packet(12), Arrival::Next { missing: 0 });
        // Le saut isolé n'arme plus rien : un nouveau 30_001 est à son tour un saut.
        assert_eq!(t.on_packet(30_001), Arrival::Jump);
    }

    #[test]
    fn grand_trou_sous_le_seuil_de_saut() {
        let mut t = SeqTracker::new();
        t.on_packet(0);
        assert_eq!(
            t.on_packet(MAX_DROPOUT - 1),
            Arrival::Next {
                missing: MAX_DROPOUT - 2
            }
        );
        // L'historique est reparti : l'ancien paquet 0 est hors fenêtre → saut.
        assert_eq!(t.on_packet(0), Arrival::Jump);
    }
}
