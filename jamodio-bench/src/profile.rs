//! Profils des musiciens simulés, et le calendrier d'envoi qu'ils produisent.
//!
//! Tout est DÉTERMINISTE : une graine donne toujours la même suite de retards,
//! de pertes et de prises de parole. Deux campagnes avec la même graine envoient
//! exactement la même chose — c'est ce qui permet de comparer un avant et un
//! après.

use serde::{Deserialize, Serialize};

/// Cadence d'un flux Jamodio : une trame Opus de 2,5 ms.
pub const FRAME_US: u64 = 2_500;
/// Échantillons par trame à 48 kHz (horodatage RTP).
pub const FRAME_SAMPLES: u32 = 120;

/// Générateur pseudo-aléatoire SplitMix64 : minuscule, rapide, et surtout
/// reproductible sur toutes les plateformes (aucune dépendance à une version de
/// crate qui changerait la suite).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniforme dans [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Loi exponentielle de moyenne `mean`.
    pub fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.unit()).ln()
    }
}

/// Retard réseau ajouté à chaque paquet.
///
/// Loi exponentielle : beaucoup de paquets à l'heure, une queue de retardataires.
/// Sa queue p95 − p10 (la mesure de gigue de l'agent, `sync::jitter`) vaut
/// `mean × (ln 20 − ln(1/0,9)) ≈ 2,89 × mean` — d'où les moyennes des préréglages.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "kebab-case")]
pub enum Jitter {
    /// Aucun retard : chaque trame part pile à son heure.
    None,
    /// Retard exponentiel de moyenne `mean_ms`.
    Exponential { mean_ms: f64 },
}

impl Jitter {
    pub fn sample_us(&self, rng: &mut Rng) -> u64 {
        match *self {
            Jitter::None => 0,
            Jitter::Exponential { mean_ms } => (rng.exp(mean_ms * 1000.0)).round() as u64,
        }
    }
}

/// Qui parle quand, pour un flux de talkback.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "kebab-case")]
pub enum Speech {
    /// Parle en permanence (cas le plus chargé).
    Always,
    /// Prises de parole et silences de durées exponentielles.
    Bursts { talk_mean_s: f64, silence_mean_s: f64 },
}

/// Un musicien simulé : son lien, et s'il envoie aussi du talkback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerProfile {
    /// Nom lisible dans les résultats.
    pub name: String,
    /// Retard réseau de SON instrument.
    pub jitter: Jitter,
    /// Paquets perdus, en % (0 à 100).
    #[serde(default)]
    pub loss_pct: f64,
    /// Talkback envoyé par ce musicien : `None` = il ne parle jamais.
    #[serde(default)]
    pub voice: Option<Speech>,
}

impl PeerProfile {
    /// Préréglages mesurés en vrai.
    ///
    /// - `regular` : aucun retard (le cas idéal : tout trou est alors local) ;
    /// - `ethernet` : queue de gigue ~2 ms (liens des 22 et 26/09) ;
    /// - `wifi` : queue ~16 ms (lien `bc79f1f7` du 26/09).
    pub fn preset(name: &str) -> Option<Self> {
        let jitter = match name {
            "regular" => Jitter::None,
            "ethernet" => Jitter::Exponential { mean_ms: 0.7 },
            "wifi" => Jitter::Exponential { mean_ms: 5.5 },
            _ => return None,
        };
        Some(Self { name: name.to_string(), jitter, loss_pct: 0.0, voice: None })
    }
}

/// Une trame à envoyer : quand, et laquelle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// Instant d'envoi, en µs depuis le début du flux.
    pub send_at_us: u64,
    /// Numéro de trame (0, 1, 2…) : donne le numéro de séquence et l'horodatage.
    pub index: u64,
}

/// Calendrier d'envoi d'un flux instrument : une trame toutes les 2,5 ms,
/// retardée selon le profil, jamais DÉPASSÉE par la suivante (un lien réseau
/// garde l'ordre : un paquet retardé retarde ceux qui le suivent dans la file),
/// et parfois perdue (la trame disparaît, son numéro aussi : le récepteur voit
/// le trou de numérotation, comme en vrai).
pub struct InstrumentSchedule {
    jitter: Jitter,
    loss: f64,
    rng: Rng,
    next_index: u64,
    last_send_us: u64,
}

impl InstrumentSchedule {
    pub fn new(profile: &PeerProfile, seed: u64) -> Self {
        Self {
            jitter: profile.jitter,
            loss: (profile.loss_pct / 100.0).clamp(0.0, 1.0),
            rng: Rng::new(seed),
            next_index: 0,
            last_send_us: 0,
        }
    }
}

impl Iterator for InstrumentSchedule {
    type Item = Frame;

    fn next(&mut self) -> Option<Frame> {
        loop {
            let index = self.next_index;
            self.next_index += 1;
            // Les deux tirages ont lieu même pour une trame perdue : la suite des
            // retards ne dépend pas du taux de perte (comparaisons à graine égale).
            let lost = self.rng.unit() < self.loss;
            let delay = self.jitter.sample_us(&mut self.rng);
            if lost {
                continue;
            }
            let send_at = (index * FRAME_US + delay).max(self.last_send_us);
            self.last_send_us = send_at;
            return Some(Frame { send_at_us: send_at, index });
        }
    }
}

/// Calendrier d'un flux de talkback : les trames d'un instrument, mais seulement
/// pendant les prises de parole.
pub struct VoiceSchedule {
    inner: InstrumentSchedule,
    speech: Speech,
    rng: Rng,
    /// Fin de la période courante (µs) et si c'est une prise de parole.
    period_end_us: u64,
    talking: bool,
}

impl VoiceSchedule {
    pub fn new(profile: &PeerProfile, speech: Speech, seed: u64) -> Self {
        let mut rng = Rng::new(seed ^ 0xA5A5_A5A5_A5A5_A5A5);
        let (talking, period_end_us) = match speech {
            Speech::Always => (true, u64::MAX),
            // On commence par un silence : personne ne parle à l'arrivée.
            Speech::Bursts { silence_mean_s, .. } => (false, (rng.exp(silence_mean_s) * 1e6) as u64),
        };
        Self {
            inner: InstrumentSchedule::new(profile, seed),
            speech,
            rng,
            period_end_us,
            talking,
        }
    }
}

impl Iterator for VoiceSchedule {
    type Item = Frame;

    fn next(&mut self) -> Option<Frame> {
        loop {
            let f = self.inner.next()?;
            let nominal = f.index * FRAME_US;
            while nominal >= self.period_end_us {
                if let Speech::Bursts { talk_mean_s, silence_mean_s } = self.speech {
                    self.talking = !self.talking;
                    let mean = if self.talking { talk_mean_s } else { silence_mean_s };
                    // Au moins une trame, sinon une période nulle bouclerait.
                    let len = ((self.rng.exp(mean) * 1e6) as u64).max(FRAME_US);
                    self.period_end_us += len;
                }
            }
            if self.talking {
                return Some(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(jitter: Jitter, loss_pct: f64) -> PeerProfile {
        PeerProfile { name: "t".into(), jitter, loss_pct, voice: None }
    }

    #[test]
    fn la_meme_graine_donne_la_meme_suite() {
        let p = profile(Jitter::Exponential { mean_ms: 2.0 }, 1.0);
        let a: Vec<_> = InstrumentSchedule::new(&p, 42).take(2000).collect();
        let b: Vec<_> = InstrumentSchedule::new(&p, 42).take(2000).collect();
        assert_eq!(a, b);
        let c: Vec<_> = InstrumentSchedule::new(&p, 43).take(2000).collect();
        assert_ne!(a, c, "une autre graine, une autre suite");
    }

    #[test]
    fn sans_gigue_une_trame_part_toutes_les_2_5_ms() {
        let frames: Vec<_> = InstrumentSchedule::new(&profile(Jitter::None, 0.0), 1).take(400).collect();
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.index, i as u64);
            assert_eq!(f.send_at_us, i as u64 * FRAME_US);
        }
    }

    /// Un lien garde l'ordre : jamais un paquet ne part avant le précédent.
    #[test]
    fn l_ordre_est_garde_meme_avec_une_forte_gigue() {
        let frames: Vec<_> = InstrumentSchedule::new(&profile(Jitter::Exponential { mean_ms: 10.0 }, 0.0), 7)
            .take(10_000)
            .collect();
        assert!(frames.windows(2).all(|w| w[1].send_at_us >= w[0].send_at_us));
        assert!(frames.iter().all(|f| f.send_at_us >= f.index * FRAME_US), "jamais en avance");
    }

    /// La queue de gigue produite est celle que le préréglage annonce, mesurée
    /// comme l'agent la mesure (p95 − p10 du retard).
    #[test]
    fn les_prereglages_donnent_la_queue_de_gigue_annoncee() {
        for (name, tail_ms) in [("ethernet", 2.0), ("wifi", 16.0)] {
            let p = PeerProfile::preset(name).unwrap();
            let mut rng = Rng::new(3);
            let mut d: Vec<f64> = (0..50_000).map(|_| p.jitter.sample_us(&mut rng) as f64 / 1000.0).collect();
            d.sort_by(f64::total_cmp);
            let tail = d[d.len() * 95 / 100] - d[d.len() / 10];
            assert!((tail - tail_ms).abs() < tail_ms * 0.1, "{name}: queue {tail:.2} ms");
        }
        assert!(PeerProfile::preset("inconnu").is_none());
    }

    #[test]
    fn le_taux_de_perte_est_tenu_et_se_voit_dans_la_numerotation() {
        let frames: Vec<_> = InstrumentSchedule::new(&profile(Jitter::None, 1.0), 9).take(100_000).collect();
        let sent = frames.len() as f64;
        let expected = frames.last().unwrap().index as f64 + 1.0;
        let loss = 1.0 - sent / expected;
        assert!((loss - 0.01).abs() < 0.002, "perte mesurée {loss}");
        assert!(frames.windows(2).any(|w| w[1].index > w[0].index + 1), "des numéros manquent");
    }

    #[test]
    fn un_talkback_permanent_suit_la_cadence_de_l_instrument() {
        let p = profile(Jitter::None, 0.0);
        let v: Vec<_> = VoiceSchedule::new(&p, Speech::Always, 5).take(400).collect();
        let i: Vec<_> = InstrumentSchedule::new(&p, 5).take(400).collect();
        assert_eq!(v, i);
    }

    #[test]
    fn un_talkback_par_salves_alterne_paroles_et_silences() {
        let p = profile(Jitter::None, 0.0);
        let speech = Speech::Bursts { talk_mean_s: 2.0, silence_mean_s: 4.0 };
        let v: Vec<_> = VoiceSchedule::new(&p, speech, 11)
            .take_while(|f| f.send_at_us < 600_000_000)
            .collect();
        // Sur 10 min, environ un tiers du temps en parole.
        let talking = v.len() as f64 * FRAME_US as f64 / 600e6;
        assert!((0.2..0.5).contains(&talking), "part de parole {talking}");
        let silences = v.windows(2).filter(|w| w[1].index > w[0].index + 1).count();
        assert!(silences > 50, "des silences, {silences}");
        // Déterministe aussi.
        let w: Vec<_> = VoiceSchedule::new(&p, speech, 11).take(v.len()).collect();
        assert_eq!(v, w);
    }
}
