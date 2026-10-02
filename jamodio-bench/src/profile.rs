//! Profils des musiciens simulés : leur lien réseau, leur horloge, et le
//! calendrier d'envoi qu'ils produisent.
//!
//! Tout est DÉTERMINISTE : une graine donne toujours la même suite de retards,
//! de pertes, de pics et de prises de parole. Deux campagnes avec la même graine
//! envoient exactement la même chose — c'est ce qui permet de comparer un avant
//! et un après.
//!
//! Lot R1 (PLAN-BANC-REALISTE-2026-10, dépôt du site) : un lien n'est plus
//! seulement une gigue et des pertes isolées. Il peut aussi perdre en rafales,
//! retenir les paquets puis les relâcher d'un coup (pics), en livrer certains
//! après le suivant (désordre), avoir un délai de base qui saute en cours de
//! route ; le musicien a sa propre horloge (dérive en ppm), peut changer de lien
//! et s'absenter. Chaque nouveau réglage a une valeur NEUTRE par défaut : un
//! scénario d'avant R1 envoie exactement les mêmes paquets (test d'empreinte).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Cadence d'un flux Jamodio : une trame Opus de 2,5 ms.
pub const FRAME_US: u64 = 2_500;
/// Échantillons par trame à 48 kHz (horodatage RTP).
pub const FRAME_SAMPLES: u32 = 120;

/// Bornes de la dérive simulée. Une carte son réelle dérive de quelques dizaines
/// de ppm ; l'asservissement du tampon de l'agent en rattrape ±5 000
/// (`RESAMPLE_MAX_ADJ`, `ring_buffer.rs`). ±1 000 couvre largement le réel.
pub const MAX_DRIFT_PPM: f64 = 1_000.0;

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

/// Chaque modèle tire dans SA propre suite (graine du musicien mêlée à une
/// constante) : activer le désordre ne change pas la suite des retards, et un
/// avant / après ne compare qu'un effet à la fois.
const SEED_BURST: u64 = 0xB0B5_7A11_0000_0001;
const SEED_SPIKES: u64 = 0x5B1C_E5A1_0000_0002;
const SEED_REORDER: u64 = 0x0DD0_4DE4_0000_0003;

/// Retard réseau ajouté à chaque paquet.
///
/// Les lois se lisent par leur QUEUE p95 − p10 : c'est la mesure de gigue de
/// l'agent (`sync::jitter`) et ce qui dimensionne son tampon.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Jitter {
    /// Aucun retard : chaque trame part pile à son heure.
    None,
    /// Retard exponentiel de moyenne `mean_ms` : beaucoup de paquets à l'heure,
    /// une queue de retardataires. Queue p95 − p10 = `mean × (ln 20 − ln(1/0,9))`
    /// ≈ 2,89 × mean — d'où les moyennes des préréglages.
    Exponential { mean_ms: f64 },
    /// Loi à queue LOURDE (Pareto de seconde espèce) : de rares retards bien
    /// plus longs que l'exponentielle n'en produit jamais — une box chargée, un
    /// Wi-Fi encombré. Réglée par sa queue p95 − p10 (`tail_ms`) et sa forme
    /// (`shape` : plus elle est petite, plus la queue est lourde), plafonnée à
    /// `max_ms` (au-delà, un vrai lien perd le paquet plutôt que de le livrer).
    Pareto { tail_ms: f64, shape: f64, max_ms: f64 },
}

impl Jitter {
    pub fn sample_us(&self, rng: &mut Rng) -> u64 {
        match *self {
            Jitter::None => 0,
            Jitter::Exponential { mean_ms } => (rng.exp(mean_ms * 1000.0)).round() as u64,
            Jitter::Pareto { tail_ms, shape, max_ms } => {
                // Quantile de la loi : x(p) = λ·((1 − p)^(−1/α) − 1).
                let scale = tail_ms / pareto_tail_factor(shape);
                let x = scale * ((1.0 - rng.unit()).powf(-1.0 / shape) - 1.0);
                (x.min(max_ms) * 1000.0).round() as u64
            }
        }
    }

    fn validate(&self, who: &str) -> Result<(), String> {
        match *self {
            Jitter::None => Ok(()),
            Jitter::Exponential { mean_ms } if mean_ms > 0.0 && mean_ms <= 100.0 => Ok(()),
            Jitter::Exponential { .. } => Err(format!("{who} : gigue moyenne hors de ]0, 100] ms")),
            Jitter::Pareto { tail_ms, shape, max_ms } => {
                if !(tail_ms > 0.0 && tail_ms <= 100.0) {
                    return Err(format!("{who} : queue de gigue hors de ]0, 100] ms"));
                }
                if !(1.1..=10.0).contains(&shape) {
                    return Err(format!("{who} : forme de la loi à queue lourde hors de [1,1 ; 10]"));
                }
                if !(max_ms >= tail_ms && max_ms <= 1000.0) {
                    return Err(format!("{who} : plafond de gigue entre la queue ({tail_ms} ms) et 1000 ms"));
                }
                Ok(())
            }
        }
    }
}

/// Rapport entre la queue p95 − p10 et l'échelle λ de la loi de Pareto.
fn pareto_tail_factor(shape: f64) -> f64 {
    20f64.powf(1.0 / shape) - (1.0 / 0.9f64).powf(1.0 / shape)
}

/// Pertes en rafales : modèle à deux états. Dans l'état « mauvais », tout est
/// perdu ; on en sort en moyenne après `mean_packets` paquets ; on y entre assez
/// souvent pour perdre `rate_pct` % des paquets au total.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstLoss {
    pub rate_pct: f64,
    pub mean_packets: f64,
    /// Chaque rafale fait EXACTEMENT `mean_packets` paquets (au lieu d'une
    /// longueur tirée au hasard autour de cette moyenne). Sert à situer un
    /// seuil — chantier P1 : l'Audio Engine masque au plus 3 trames d'affilée
    /// (`mixer/conceal.rs`), une rafale de 4 devient-elle un trou ?
    #[serde(default, skip_serializing_if = "is_false")]
    pub fixed: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl BurstLoss {
    /// Longueur d'une rafale fixe (en paquets).
    fn fixed_len(&self) -> u32 {
        self.mean_packets.round() as u32
    }

    /// Rafale fixe : probabilité, par paquet transmis, d'en commencer une, pour
    /// perdre `rate_pct` % au total (L perdus pour 1/q + L paquets en moyenne).
    fn fixed_start(&self) -> f64 {
        let rate = self.rate_pct / 100.0;
        rate / (f64::from(self.fixed_len()) * (1.0 - rate))
    }

    /// Probabilités par paquet : (bon → mauvais, mauvais → bon).
    fn transitions(&self) -> (f64, f64) {
        let rate = self.rate_pct / 100.0;
        let leave_bad = 1.0 / self.mean_packets;
        (rate * leave_bad / (1.0 - rate), leave_bad)
    }
}

/// Pics : en moyenne toutes les `every_mean_s` secondes (instants tirés au
/// hasard), le lien retient TOUT pendant `hold_ms`, puis relâche les paquets
/// d'un coup — un Wi-Fi qui balaie les canaux, une file d'attente qui se vide.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spikes {
    pub every_mean_s: f64,
    pub hold_ms: f64,
}

/// Désordre : `pct` % des paquets arrivent après les 1 à `max_depth` paquets
/// qui les suivent (profondeur tirée au hasard), sans retarder ceux-ci.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reorder {
    pub pct: f64,
    pub max_depth: u32,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// Un lien réseau complet. C'est aussi ce qu'un changement de lien en cours de
/// session installe (`Change`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    /// Nom lisible dans les résultats.
    pub name: String,
    /// D'où viennent les valeurs : « mesuré : … » ou « hypothèse — à calibrer
    /// en R2 ». Recopié dans le résumé : un lien inventé ne passe pas pour une
    /// mesure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub jitter: Jitter,
    /// Paquets perdus un à un, en % (0 à 100).
    #[serde(default)]
    pub loss_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst_loss: Option<BurstLoss>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spikes: Option<Spikes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reorder: Option<Reorder>,
    /// Délai constant (ms). Seul, il ne change rien au tampon (qui ne voit que
    /// les VARIATIONS d'arrivée) ; c'est son SAUT, d'un lien à l'autre, qui
    /// compte (changement de route).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub base_delay_ms: f64,
}

/// Origine des préréglages qui ne viennent pas d'une vraie session.
pub const UNCALIBRATED: &str = "hypothèse — à calibrer en R2";

impl Link {
    fn plain(name: &str, origin: &str, jitter: Jitter) -> Self {
        Self {
            name: name.into(),
            origin: Some(origin.into()),
            jitter,
            loss_pct: 0.0,
            burst_loss: None,
            spikes: None,
            reorder: None,
            base_delay_ms: 0.0,
        }
    }

    /// Préréglages.
    ///
    /// Mesurés : `regular` (aucun retard : tout trou est alors local),
    /// `ethernet` (queue ~2 ms, liens des 22 et 26/09), `wifi` (queue ~16 ms,
    /// lien `bc79f1f7` du 26/09).
    /// Non calibrés (R1, valeurs d'ordre de grandeur, à remplacer en R2 par des
    /// liens tirés de vraies sessions) : `fibre` (accès fibre, poste en Wi-Fi
    /// 5 GHz), `adsl`, `wifi-charge`, `4g`.
    pub fn preset(name: &str) -> Option<Self> {
        let pareto = |tail_ms, max_ms| Jitter::Pareto { tail_ms, shape: 2.0, max_ms };
        Some(match name {
            "regular" => Self::plain(name, "flux parfaitement régulier", Jitter::None),
            "ethernet" => Self::plain(name, "mesuré : liens des 22 et 26/09/2026", Jitter::Exponential { mean_ms: 0.7 }),
            "wifi" => Self::plain(name, "mesuré : lien bc79f1f7 du 26/09/2026", Jitter::Exponential { mean_ms: 5.5 }),
            "fibre" => Self {
                loss_pct: 0.02,
                spikes: Some(Spikes { every_mean_s: 30.0, hold_ms: 20.0 }),
                ..Self::plain(name, UNCALIBRATED, Jitter::Exponential { mean_ms: 1.5 })
            },
            "adsl" => Self {
                loss_pct: 0.05,
                burst_loss: Some(BurstLoss { rate_pct: 0.2, mean_packets: 3.0, fixed: false }),
                spikes: Some(Spikes { every_mean_s: 60.0, hold_ms: 40.0 }),
                ..Self::plain(name, UNCALIBRATED, pareto(6.0, 60.0))
            },
            "wifi-charge" => Self {
                loss_pct: 0.1,
                burst_loss: Some(BurstLoss { rate_pct: 0.5, mean_packets: 4.0, fixed: false }),
                spikes: Some(Spikes { every_mean_s: 8.0, hold_ms: 60.0 }),
                reorder: Some(Reorder { pct: 0.2, max_depth: 2 }),
                ..Self::plain(name, UNCALIBRATED, pareto(20.0, 120.0))
            },
            "4g" => Self {
                loss_pct: 0.1,
                burst_loss: Some(BurstLoss { rate_pct: 1.0, mean_packets: 6.0, fixed: false }),
                spikes: Some(Spikes { every_mean_s: 15.0, hold_ms: 100.0 }),
                reorder: Some(Reorder { pct: 0.5, max_depth: 3 }),
                ..Self::plain(name, UNCALIBRATED, pareto(25.0, 200.0))
            },
            _ => return None,
        })
    }

    pub const PRESETS: [&'static str; 7] = ["regular", "ethernet", "wifi", "fibre", "adsl", "wifi-charge", "4g"];

    /// Aucun aléa : ni gigue, ni perte, ni pic, ni désordre (un délai constant
    /// reste régulier).
    pub fn is_regular(&self) -> bool {
        self.jitter == Jitter::None
            && self.loss_pct == 0.0
            && self.burst_loss.is_none()
            && self.spikes.is_none()
            && self.reorder.is_none()
    }

    pub fn validate(&self) -> Result<(), String> {
        let who = if self.name.is_empty() { "lien sans nom" } else { self.name.as_str() };
        if self.name.is_empty() {
            return Err("un lien doit avoir un nom".into());
        }
        if !(0.0..=100.0).contains(&self.loss_pct) {
            return Err(format!("{who} : perte hors de 0-100 %"));
        }
        self.jitter.validate(who)?;
        if let Some(b) = self.burst_loss {
            if !(b.rate_pct > 0.0 && b.rate_pct <= 50.0) {
                return Err(format!("{who} : pertes en rafales hors de ]0, 50] %"));
            }
            if !(1.0..=1000.0).contains(&b.mean_packets) {
                return Err(format!("{who} : longueur moyenne des rafales hors de [1, 1000] paquets"));
            }
            if b.transitions().0 > 1.0 {
                return Err(format!("{who} : taux de rafales impossible avec des rafales aussi courtes"));
            }
            if b.fixed && (b.mean_packets.fract() != 0.0 || b.fixed_start() > 1.0) {
                return Err(format!("{who} : rafale fixe = un nombre entier de paquets, compatible avec le taux"));
            }
        }
        if let Some(s) = self.spikes {
            if !(0.5..=3600.0).contains(&s.every_mean_s) {
                return Err(format!("{who} : intervalle moyen des pics hors de [0,5 ; 3600] s"));
            }
            if !(s.hold_ms > 0.0 && s.hold_ms <= 2000.0 && s.hold_ms < s.every_mean_s * 1000.0) {
                return Err(format!("{who} : durée d'un pic hors de ]0, 2000] ms ou plus longue que leur intervalle"));
            }
        }
        if let Some(r) = self.reorder {
            if !(r.pct > 0.0 && r.pct <= 50.0) {
                return Err(format!("{who} : désordre hors de ]0, 50] %"));
            }
            // Au-delà de 100 paquets en arrière, l'agent ne voit plus un retard
            // mais un saut de numérotation (`net::seq::MAX_MISORDER`).
            if !(1..=50).contains(&r.max_depth) {
                return Err(format!("{who} : profondeur du désordre hors de [1, 50] paquets"));
            }
        }
        if !(0.0..=1000.0).contains(&self.base_delay_ms) {
            return Err(format!("{who} : délai de base hors de [0, 1000] ms"));
        }
        Ok(())
    }
}

/// Qui parle quand, pour un flux de talkback.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Speech {
    /// Parle en permanence (cas le plus chargé).
    Always,
    /// Prises de parole et silences de durées exponentielles.
    Bursts { talk_mean_s: f64, silence_mean_s: f64 },
}

/// Un changement de lien, `at_s` secondes après l'arrivée du musicien.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub at_s: f64,
    pub link: Link,
}

/// Une absence : le musicien part `at_s` secondes après son arrivée et revient
/// `for_s` secondes plus tard, comme un vrai musicien (nouveau flux).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Absence {
    pub at_s: f64,
    pub for_s: f64,
}

/// Un musicien simulé : son lien de départ (champs à plat, comme avant R1 : les
/// anciens scénarios se relisent tels quels), son horloge, ses changements de
/// lien, ses absences, et s'il envoie aussi du talkback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerProfile {
    /// Nom lisible dans les résultats (celui du lien de départ).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Retard réseau de SON instrument.
    pub jitter: Jitter,
    /// Paquets perdus un à un, en % (0 à 100).
    #[serde(default)]
    pub loss_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst_loss: Option<BurstLoss>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spikes: Option<Spikes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reorder: Option<Reorder>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub base_delay_ms: f64,
    /// Horloge du musicien par rapport à celle du banc, en ppm (> 0 : il envoie
    /// plus vite). Sa cadence d'envoi ET son horodatage la suivent, comme une
    /// vraie carte son.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub drift_ppm: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absences: Vec<Absence>,
    /// Talkback envoyé par ce musicien : `None` = il ne parle jamais.
    #[serde(default)]
    pub voice: Option<Speech>,
}

impl PeerProfile {
    pub fn from_link(link: Link) -> Self {
        Self {
            name: link.name,
            origin: link.origin,
            jitter: link.jitter,
            loss_pct: link.loss_pct,
            burst_loss: link.burst_loss,
            spikes: link.spikes,
            reorder: link.reorder,
            base_delay_ms: link.base_delay_ms,
            drift_ppm: 0.0,
            changes: Vec::new(),
            absences: Vec::new(),
            voice: None,
        }
    }

    /// Préréglage de lien (cf. [`Link::preset`]), sans dérive ni événement.
    pub fn preset(name: &str) -> Option<Self> {
        Link::preset(name).map(Self::from_link)
    }

    /// Le lien de départ.
    pub fn link(&self) -> Link {
        Link {
            name: self.name.clone(),
            origin: self.origin.clone(),
            jitter: self.jitter,
            loss_pct: self.loss_pct,
            burst_loss: self.burst_loss,
            spikes: self.spikes,
            reorder: self.reorder,
            base_delay_ms: self.base_delay_ms,
        }
    }

    /// Les liens successifs : (début en µs depuis l'arrivée, lien).
    pub fn timeline(&self) -> Vec<(u64, Link)> {
        std::iter::once((0, self.link()))
            .chain(self.changes.iter().map(|c| ((c.at_s * 1e6).round() as u64, c.link.clone())))
            .collect()
    }

    /// Nom du lien en vigueur `t_s` secondes après l'arrivée.
    pub fn link_name_at(&self, t_s: f64) -> &str {
        self.changes
            .iter()
            .rev()
            .find(|c| t_s >= c.at_s)
            .map_or(self.name.as_str(), |c| c.link.name.as_str())
    }

    /// Flux parfaitement régulier : aucun aléa, aucun événement, aucune dérive.
    pub fn is_regular(&self) -> bool {
        self.link().is_regular() && self.changes.is_empty() && self.absences.is_empty() && self.drift_ppm == 0.0
    }

    pub fn validate(&self) -> Result<(), String> {
        self.link().validate()?;
        let who = &self.name;
        if self.drift_ppm.is_nan() || self.drift_ppm.abs() > MAX_DRIFT_PPM {
            return Err(format!("{who} : dérive hors de ±{MAX_DRIFT_PPM} ppm"));
        }
        let mut prev = 0.0;
        for c in &self.changes {
            if !(c.at_s.is_finite() && c.at_s > prev) {
                return Err(format!("{who} : changements de lien à des instants > 0 et croissants"));
            }
            c.link.validate()?;
            prev = c.at_s;
        }
        let mut free_from = 0.0;
        for a in &self.absences {
            if !(a.at_s.is_finite() && a.at_s > free_from) {
                return Err(format!("{who} : absences à des instants > 0, croissants, sans chevauchement"));
            }
            if !(a.for_s >= 1.0 && a.for_s.is_finite()) {
                return Err(format!("{who} : absence d'au moins 1 s"));
            }
            free_from = a.at_s + a.for_s;
        }
        if let Some(Speech::Bursts { talk_mean_s, silence_mean_s }) = self.voice {
            if !(talk_mean_s > 0.0 && silence_mean_s > 0.0) {
                return Err(format!("{who} : durées de parole/silence > 0"));
            }
        }
        Ok(())
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

/// Instant où la trame `index` est produite par un musicien dont l'horloge
/// dérive de `ppm` : à +100 ppm, il produit 48 004,8 échantillons par seconde du
/// banc, donc une trame toutes les 2,5 ms ÷ 1,0001. Calculé depuis l'index, sans
/// cumul d'erreur, aussi longue que soit la session.
pub fn nominal_us(index: u64, ppm: f64) -> u64 {
    if ppm == 0.0 {
        index * FRAME_US
    } else {
        ((index * FRAME_US) as f64 * 1e6 / (1e6 + ppm)).round() as u64
    }
}

/// Marge en arrière gardée sur les pics passés : un paquet peut être daté avant
/// le précédent (gigue plafonnée à 1 s, délai de base qui baisse d'au plus 1 s).
const SPIKE_LOOKBACK_US: u64 = 3_000_000;

/// Les pics d'un musicien, dans le temps de SON lien (depuis son arrivée).
/// Tirés de la graine du musicien : instrument et talkback, et le flux recréé
/// après une absence, vivent les mêmes pics.
struct SpikeTrain {
    rng: Rng,
    /// Fenêtres [début, fin) déjà tirées, dans l'ordre.
    windows: VecDeque<(u64, u64)>,
    /// Instant à partir duquel tirer la prochaine ; `None` = plus aucun pic.
    cursor: Option<u64>,
}

impl SpikeTrain {
    fn new(seed: u64) -> Self {
        Self { rng: Rng::new(seed ^ SEED_SPIKES), windows: VecDeque::new(), cursor: Some(0) }
    }

    /// Tire les pics jusqu'à couvrir l'instant `t`.
    fn extend_to(&mut self, t: u64, timeline: &[(u64, Link)]) {
        while let Some(cursor) = self.cursor {
            if cursor > t {
                return;
            }
            let seg = segment_at(timeline, cursor);
            let next_change = timeline.get(seg + 1).map(|(at, _)| *at);
            match timeline[seg].1.spikes {
                // Pas de pic sur ce lien : on reprend au changement suivant.
                None => self.cursor = next_change,
                Some(s) => {
                    let start = cursor + (self.rng.exp(s.every_mean_s) * 1e6).round() as u64;
                    match next_change {
                        // Le lien change avant ce pic : processus sans mémoire, on
                        // repart du changement avec les réglages du lien suivant.
                        Some(at) if start >= at => self.cursor = Some(at),
                        _ => {
                            let end = start + (s.hold_ms * 1000.0).round() as u64;
                            self.windows.push_back((start, end));
                            self.cursor = Some(end);
                        }
                    }
                }
            }
        }
    }

    /// Si l'instant `t` tombe dans un pic, l'instant où le lien relâche.
    fn release(&mut self, t: u64, timeline: &[(u64, Link)]) -> Option<u64> {
        self.extend_to(t, timeline);
        while self.windows.front().is_some_and(|w| w.1 + SPIKE_LOOKBACK_US < t) {
            self.windows.pop_front();
        }
        self.windows.iter().find(|w| (w.0..w.1).contains(&t)).map(|w| w.1)
    }
}

/// Index du lien en vigueur à l'instant `t` de la frise.
fn segment_at(timeline: &[(u64, Link)], t: u64) -> usize {
    timeline.iter().rposition(|(at, _)| *at <= t).unwrap_or(0)
}

/// Calendrier d'envoi d'un flux : une trame toutes les 2,5 ms de l'horloge du
/// musicien, puis, dans cet ordre, les pertes (isolées, en rafales), le retard
/// (délai de base, gigue, pics), l'ordre de la file (un paquet retardé retarde
/// ceux qui le suivent) et enfin le désordre (un paquet passe après les
/// suivants, sans les retarder). Une trame perdue disparaît avec son numéro :
/// le récepteur voit le trou de numérotation, comme en vrai. Les trames sortent
/// dans l'ordre d'envoi.
pub struct InstrumentSchedule {
    timeline: Vec<(u64, Link)>,
    seg: usize,
    drift_ppm: f64,
    /// Place de ce flux dans la frise du musicien (µs) : 0 à l'arrivée, la durée
    /// écoulée pour un flux recréé au retour d'une absence.
    offset_us: u64,
    /// Pertes isolées puis gigue : l'ordre de tirage d'avant R1 (empreinte).
    rng: Rng,
    burst_rng: Rng,
    burst_bad: bool,
    /// Rafale fixe en cours : paquets encore à perdre.
    burst_left: u32,
    /// Une rafale fixe vient de finir : le paquet suivant passe, sans tirage
    /// — sinon deux rafales de L se colleraient en une de 2L et brouilleraient
    /// le seuil cherché.
    burst_just_ended: bool,
    spikes: SpikeTrain,
    reorder_rng: Rng,
    next_index: u64,
    last_send_us: u64,
    out: VecDeque<Frame>,
    /// Trames désordonnées : elles sortent juste après la trame `.0`.
    deferred: Vec<(u64, u64)>,
}

impl InstrumentSchedule {
    pub fn new(profile: &PeerProfile, seed: u64) -> Self {
        Self::resumed(profile, seed, 0)
    }

    /// Flux recréé `offset_us` après l'arrivée du musicien (retour d'absence) :
    /// numérotation à zéro, mais le lien en vigueur et les pics sont ceux de cet
    /// instant-là.
    pub fn resumed(profile: &PeerProfile, seed: u64, offset_us: u64) -> Self {
        Self {
            timeline: profile.timeline(),
            seg: 0,
            drift_ppm: profile.drift_ppm,
            offset_us,
            rng: Rng::new(seed),
            burst_rng: Rng::new(seed ^ SEED_BURST),
            burst_bad: false,
            burst_left: 0,
            burst_just_ended: false,
            spikes: SpikeTrain::new(seed),
            reorder_rng: Rng::new(seed ^ SEED_REORDER),
            next_index: 0,
            last_send_us: 0,
            out: VecDeque::new(),
            deferred: Vec::new(),
        }
    }

    /// Produit la trame suivante de l'horloge du musicien (0 ou plusieurs
    /// trames rejoignent la file de sortie).
    fn produce(&mut self) {
        let index = self.next_index;
        self.next_index += 1;
        let nominal = nominal_us(index, self.drift_ppm);
        let at = self.offset_us + nominal;
        while self.timeline.get(self.seg + 1).is_some_and(|(t, _)| *t <= at) {
            self.seg += 1;
        }
        let link = &self.timeline[self.seg].1;
        // Les tirages ont lieu même pour une trame perdue : la suite des retards
        // ne dépend pas du taux de perte (comparaisons à graine égale).
        let lost = self.rng.unit() < (link.loss_pct / 100.0).clamp(0.0, 1.0);
        let delay = link.jitter.sample_us(&mut self.rng);
        let burst_lost = match link.burst_loss {
            // Longueur fixe : un tirage par paquet transmis, aucun pendant la rafale.
            Some(b) if b.fixed => {
                if self.burst_left > 0 {
                    self.burst_left -= 1;
                    self.burst_just_ended = self.burst_left == 0;
                    true
                } else if std::mem::take(&mut self.burst_just_ended) {
                    false
                } else if self.burst_rng.unit() < b.fixed_start() {
                    self.burst_left = b.fixed_len() - 1;
                    self.burst_just_ended = self.burst_left == 0;
                    true
                } else {
                    false
                }
            }
            Some(b) => {
                let (enter_bad, leave_bad) = b.transitions();
                let u = self.burst_rng.unit();
                self.burst_bad = if self.burst_bad { u >= leave_bad } else { u < enter_bad };
                self.burst_bad
            }
            None => {
                self.burst_bad = false;
                self.burst_left = 0;
                self.burst_just_ended = false;
                false
            }
        };
        let reorder_depth = link.reorder.map(|r| {
            let hit = self.reorder_rng.unit() < r.pct / 100.0;
            let depth = 1 + self.reorder_rng.next_u64() % u64::from(r.max_depth);
            (hit, depth)
        });
        let base = (link.base_delay_ms * 1000.0).round() as u64;
        if lost || burst_lost {
            return;
        }
        let mut arrival = nominal + base + delay;
        if let Some(release) = self.spikes.release(self.offset_us + arrival, &self.timeline) {
            arrival = release - self.offset_us;
        }
        let send = arrival.max(self.last_send_us);
        if let Some((true, depth)) = reorder_depth {
            self.deferred.push((index + depth, index));
            return;
        }
        self.last_send_us = send;
        self.out.push_back(Frame { send_at_us: send, index });
        // Les trames désordonnées dont le tour est venu partent juste derrière.
        let mut due: Vec<u64> = Vec::new();
        self.deferred.retain(|&(after, i)| {
            if after <= index {
                due.push(i);
                false
            } else {
                true
            }
        });
        due.sort_unstable();
        self.out.extend(due.into_iter().map(|i| Frame { send_at_us: send, index: i }));
    }
}

impl Iterator for InstrumentSchedule {
    type Item = Frame;

    fn next(&mut self) -> Option<Frame> {
        while self.out.is_empty() {
            self.produce();
        }
        self.out.pop_front()
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
        Self::resumed(profile, speech, seed, 0)
    }

    pub fn resumed(profile: &PeerProfile, speech: Speech, seed: u64, offset_us: u64) -> Self {
        let mut rng = Rng::new(seed ^ 0xA5A5_A5A5_A5A5_A5A5);
        let (talking, period_end_us) = match speech {
            Speech::Always => (true, u64::MAX),
            // On commence par un silence : personne ne parle à l'arrivée.
            Speech::Bursts { silence_mean_s, .. } => (false, (rng.exp(silence_mean_s) * 1e6) as u64),
        };
        Self {
            inner: InstrumentSchedule::resumed(profile, seed, offset_us),
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
        PeerProfile { loss_pct, ..PeerProfile::from_link(Link::plain("t", "test", jitter)) }
    }

    fn regular() -> PeerProfile {
        PeerProfile::preset("regular").unwrap()
    }

    /// Empreinte FNV-1a des 20 000 premières trames (instant, numéro).
    fn fingerprint(it: impl Iterator<Item = Frame>) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for f in it.take(20_000) {
            for v in [f.send_at_us, f.index] {
                for b in v.to_le_bytes() {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x100_0000_01b3);
                }
            }
        }
        h
    }

    /// Les scénarios d'avant R1 envoient EXACTEMENT les mêmes paquets : empreintes
    /// relevées sur le code d'avant R1 (`f871bdd`, 01/10/2026). Si ce test casse,
    /// les campagnes passées ne sont plus comparables aux nouvelles.
    #[test]
    fn les_profils_d_avant_r1_envoient_les_memes_paquets() {
        let eth = PeerProfile { loss_pct: 1.0, ..PeerProfile::preset("ethernet").unwrap() };
        let wifi = PeerProfile::preset("wifi").unwrap();
        assert_eq!(fingerprint(InstrumentSchedule::new(&eth, 42)), 0xbcc6_21bc_53d3_a4d5);
        assert_eq!(fingerprint(InstrumentSchedule::new(&wifi, 7)), 0xff47_ff54_05c7_15be);
        assert_eq!(fingerprint(InstrumentSchedule::new(&regular(), 1)), 0x3926_7cd6_3f4f_1eb9);
        let speech = Speech::Bursts { talk_mean_s: 3.0, silence_mean_s: 6.0 };
        assert_eq!(fingerprint(VoiceSchedule::new(&wifi, speech, 11)), 0x4205_a507_588f_b8a3);
    }

    #[test]
    fn la_meme_graine_donne_la_meme_suite() {
        for name in Link::PRESETS {
            let p = PeerProfile { drift_ppm: 37.0, ..PeerProfile::preset(name).unwrap() };
            let a: Vec<_> = InstrumentSchedule::new(&p, 42).take(20_000).collect();
            let b: Vec<_> = InstrumentSchedule::new(&p, 42).take(20_000).collect();
            assert_eq!(a, b, "{name}");
            if name != "regular" {
                let c: Vec<_> = InstrumentSchedule::new(&p, 43).take(20_000).collect();
                assert_ne!(a, c, "{name} : une autre graine, une autre suite");
            }
        }
    }

    #[test]
    fn sans_gigue_une_trame_part_toutes_les_2_5_ms() {
        let frames: Vec<_> = InstrumentSchedule::new(&regular(), 1).take(400).collect();
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.index, i as u64);
            assert_eq!(f.send_at_us, i as u64 * FRAME_US);
        }
    }

    /// Sans désordre, un lien garde l'ordre : jamais un paquet ne part avant le
    /// précédent, ni avant son heure. Avec, les instants restent croissants (la
    /// file de sortie est dans l'ordre d'envoi).
    #[test]
    fn l_ordre_est_garde_meme_avec_une_forte_gigue() {
        for name in Link::PRESETS {
            let p = PeerProfile::preset(name).unwrap();
            let frames: Vec<_> = InstrumentSchedule::new(&p, 7).take(40_000).collect();
            assert!(frames.windows(2).all(|w| w[1].send_at_us >= w[0].send_at_us), "{name}");
            assert!(frames.iter().all(|f| f.send_at_us >= f.index * FRAME_US), "{name} : jamais en avance");
            if p.reorder.is_none() {
                assert!(frames.windows(2).all(|w| w[1].index > w[0].index), "{name}");
            }
        }
    }

    fn tail_ms(jitter: Jitter) -> (f64, Vec<f64>) {
        let mut rng = Rng::new(3);
        let mut d: Vec<f64> = (0..200_000).map(|_| jitter.sample_us(&mut rng) as f64 / 1000.0).collect();
        d.sort_by(f64::total_cmp);
        (d[d.len() * 95 / 100] - d[d.len() / 10], d)
    }

    /// La queue de gigue produite est celle que le préréglage annonce, mesurée
    /// comme l'agent la mesure (p95 − p10 du retard).
    #[test]
    fn les_prereglages_donnent_la_queue_de_gigue_annoncee() {
        for (name, expected) in [("ethernet", 2.0), ("wifi", 16.0), ("fibre", 4.3), ("adsl", 6.0), ("wifi-charge", 20.0), ("4g", 25.0)] {
            let (tail, _) = tail_ms(Link::preset(name).unwrap().jitter);
            assert!((tail - expected).abs() < expected * 0.1, "{name}: queue {tail:.2} ms");
        }
        assert!(Link::preset("inconnu").is_none());
    }

    /// À queue égale, la loi de Pareto a des retards extrêmes bien plus longs que
    /// l'exponentielle — c'est sa raison d'être — et ne dépasse jamais son plafond.
    #[test]
    fn la_loi_a_queue_lourde_a_des_extremes_et_un_plafond() {
        let (_, pareto) = tail_ms(Jitter::Pareto { tail_ms: 10.0, shape: 2.0, max_ms: 80.0 });
        let (_, expo) = tail_ms(Jitter::Exponential { mean_ms: 10.0 / 2.89 });
        let p999 = |d: &[f64]| d[d.len() * 999 / 1000];
        assert!(p999(&pareto) > 1.5 * p999(&expo), "p99,9 {} contre {}", p999(&pareto), p999(&expo));
        assert!(pareto.last().copied().unwrap() <= 80.0);
        assert!(pareto.iter().filter(|&&x| x == 80.0).count() > 0, "le plafond est atteint");
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

    /// Rafales : taux global et longueur moyenne tenus (±10 %).
    #[test]
    fn les_pertes_en_rafales_ont_le_taux_et_la_longueur_annonces() {
        let p = PeerProfile { burst_loss: Some(BurstLoss { rate_pct: 1.0, mean_packets: 5.0, fixed: false }), ..regular() };
        let frames: Vec<_> = InstrumentSchedule::new(&p, 21).take(400_000).collect();
        let span = frames.last().unwrap().index + 1;
        let lost = span - frames.len() as u64;
        let bursts: Vec<u64> = frames.windows(2).map(|w| w[1].index - w[0].index - 1).filter(|&g| g > 0).collect();
        let rate = lost as f64 / span as f64;
        let mean = lost as f64 / bursts.len() as f64;
        assert!((rate - 0.01).abs() < 0.001, "taux {rate}");
        assert!((mean - 5.0).abs() < 0.5, "longueur moyenne {mean}");
    }

    /// Un modèle ajouté tire dans sa propre suite : les pertes isolées tombent
    /// sur les mêmes trames avec ou sans désordre et pics, et sans gigue, une
    /// trame non désordonnée part à la même heure.
    #[test]
    fn un_modele_ajoute_ne_change_pas_les_tirages_des_autres() {
        let base = PeerProfile { loss_pct: 1.0, ..PeerProfile::preset("wifi").unwrap() };
        let more = PeerProfile {
            reorder: Some(Reorder { pct: 1.0, max_depth: 2 }),
            spikes: Some(Spikes { every_mean_s: 1.0, hold_ms: 30.0 }),
            ..base.clone()
        };
        let present = |p: &PeerProfile| -> Vec<u64> {
            let mut v: Vec<u64> = InstrumentSchedule::new(p, 5).take(40_000).map(|f| f.index).collect();
            v.sort_unstable();
            v.truncate(39_000);
            v
        };
        assert_eq!(present(&base), present(&more));
        let reordered = PeerProfile { reorder: Some(Reorder { pct: 1.0, max_depth: 1 }), ..regular() };
        let frames: Vec<_> = InstrumentSchedule::new(&reordered, 5).take(20_000).collect();
        let mut highest = 0;
        let mut kept = 0;
        for f in &frames {
            if f.index >= highest {
                assert_eq!(f.send_at_us, f.index * FRAME_US, "trame {}", f.index);
                kept += 1;
            }
            highest = highest.max(f.index);
        }
        assert!(kept > 19_000 && kept < 20_000, "{kept}");
    }

    /// Rafales fixes : chaque rafale fait exactement L paquets, au taux annoncé.
    #[test]
    fn une_rafale_fixe_fait_exactement_sa_longueur() {
        for len in 1..=6u64 {
            let p = PeerProfile {
                burst_loss: Some(BurstLoss { rate_pct: 0.5, mean_packets: len as f64, fixed: true }),
                ..regular()
            };
            p.validate().unwrap();
            let frames: Vec<_> = InstrumentSchedule::new(&p, 31).take(400_000).collect();
            // Deux rafales ne se collent jamais : après une rafale, un paquet passe.
            let gaps: Vec<u64> = frames.windows(2).map(|w| w[1].index - w[0].index - 1).filter(|&g| g > 0).collect();
            assert!(gaps.iter().all(|&g| g == len), "L={len} : {:?}", &gaps[..gaps.len().min(10)]);
            let span = frames.last().unwrap().index + 1;
            let rate = (span - frames.len() as u64) as f64 / span as f64;
            assert!((rate - 0.005).abs() < 0.0005, "L={len} : taux {rate}");
        }
        let bad = PeerProfile { burst_loss: Some(BurstLoss { rate_pct: 0.5, mean_packets: 2.5, fixed: true }), ..regular() };
        assert!(bad.validate().is_err(), "longueur fixe non entière");
    }

    /// Désordre : la part annoncée (±10 %), chaque trame déplacée part après 1 à
    /// `max_depth` trames qui la suivaient, au même instant que celle qu'elle suit.
    #[test]
    fn le_desordre_a_la_part_et_la_profondeur_annoncees() {
        let p = PeerProfile { reorder: Some(Reorder { pct: 2.0, max_depth: 3 }), ..regular() };
        let frames: Vec<_> = InstrumentSchedule::new(&p, 13).take(200_000).collect();
        let mut late = 0;
        let mut depths = [0u32; 4];
        let mut highest = 0u64;
        for (k, f) in frames.iter().enumerate() {
            if k > 0 && f.index < highest {
                late += 1;
                let overtaken = frames[..k].iter().rev().take(10).filter(|g| g.index > f.index).count();
                assert!((1..=3).contains(&overtaken), "profondeur {overtaken}");
                depths[overtaken] += 1;
                assert_eq!(f.send_at_us, frames[k - 1].send_at_us, "part juste derrière");
                assert!(f.send_at_us > f.index * FRAME_US);
            }
            highest = highest.max(f.index);
        }
        let share = late as f64 / frames.len() as f64;
        assert!((share - 0.02).abs() < 0.002, "part {share}");
        assert!(depths[1] > 0 && depths[2] > 0 && depths[3] > 0, "{depths:?}");
        // Aucune trame n'est perdue ni doublée par le désordre.
        let mut idx: Vec<u64> = frames.iter().map(|f| f.index).collect();
        idx.sort_unstable();
        assert!(idx.iter().enumerate().take(idx.len() - 3).all(|(k, &i)| i == k as u64));
    }

    /// Pics : fréquence et durée annoncées ; les paquets retenus partent ensemble
    /// à la fin du pic, et l'écart vu par le récepteur vaut la durée du pic.
    #[test]
    fn les_pics_retiennent_puis_relachent_d_un_coup() {
        let p = PeerProfile { spikes: Some(Spikes { every_mean_s: 2.0, hold_ms: 40.0 }), ..regular() };
        let frames: Vec<_> = InstrumentSchedule::new(&p, 17).take_while(|f| f.send_at_us < 1_200_000_000).collect();
        // Un pic = un écart d'envoi de plus de 30 ms suivi d'une salve.
        let gaps: Vec<usize> = frames
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[1].send_at_us - w[0].send_at_us > 30_000)
            .map(|(k, _)| k + 1)
            .collect();
        // Un pic toutes les 2,04 s en moyenne (intervalle + durée) : ~29,4/min.
        let per_min = gaps.len() as f64 / 20.0;
        assert!((per_min - 29.4).abs() < 3.0, "{per_min} pics/min");
        // Deux pics tirés presque coup sur coup se confondent (~2 %) : les autres
        // ont exactement la forme annoncée.
        let conform = gaps
            .iter()
            .filter(|&&k| {
                let gap = frames[k].send_at_us - frames[k - 1].send_at_us;
                // ~16 trames retenues, relâchées au même instant.
                let burst = frames[k..].iter().take_while(|f| f.send_at_us == frames[k].send_at_us).count();
                (40_000..=42_500).contains(&gap) && (15..=17).contains(&burst)
            })
            .count();
        assert!(conform as f64 >= 0.95 * gaps.len() as f64, "{conform} conformes sur {}", gaps.len());
    }

    /// Dérive : sur 60 s à +100 ppm, le musicien produit 24 002,4 trames et
    /// chacune part à n × 2,5 ms ÷ 1,0001 ; l'horodatage reste n × 120.
    #[test]
    fn la_derive_suit_l_horloge_du_musicien() {
        let p = PeerProfile { drift_ppm: 100.0, ..regular() };
        let frames: Vec<_> = InstrumentSchedule::new(&p, 1).take_while(|f| f.send_at_us < 60_000_000).collect();
        assert_eq!(frames.len(), 24_003, "trames 0 à 24 002");
        let last = frames.last().unwrap();
        assert_eq!(last.send_at_us, (24_002.0 * 2_500.0 / 1.0001f64).round() as u64);
        assert!(frames.iter().enumerate().all(|(i, f)| f.index == i as u64));
        let slow = PeerProfile { drift_ppm: -100.0, ..regular() };
        assert_eq!(InstrumentSchedule::new(&slow, 1).take_while(|f| f.send_at_us < 60_000_000).count(), 23_998);
    }

    /// Saut de route 12 → 30 ms : un trou de 18 ms dans les envois, puis la
    /// cadence. Retour 30 → 12 : les paquets en avance s'empilent derrière le
    /// dernier (une salve), jamais d'inversion.
    #[test]
    fn un_saut_de_route_retarde_puis_rattrape_dans_l_ordre() {
        let route = |ms: f64| Link { base_delay_ms: ms, ..Link::preset("regular").unwrap() };
        let p = PeerProfile {
            base_delay_ms: 12.0,
            changes: vec![Change { at_s: 1.0, link: route(30.0) }, Change { at_s: 2.0, link: route(12.0) }],
            ..regular()
        };
        let frames: Vec<_> = InstrumentSchedule::new(&p, 1).take(1_200).collect();
        assert_eq!(frames[0].send_at_us, 12_000);
        assert_eq!(frames[399].send_at_us, 399 * 2_500 + 12_000);
        assert_eq!(frames[400].send_at_us - frames[399].send_at_us, 2_500 + 18_000, "le trou du saut");
        assert_eq!(frames[401].send_at_us - frames[400].send_at_us, 2_500);
        // Retour : les trames 800.. auraient pu partir 18 ms plus tôt.
        let at_799 = frames[799].send_at_us;
        assert!(frames[800..807].iter().all(|f| f.send_at_us == at_799), "salve au retour");
        assert!(frames.windows(2).all(|w| w[1].index == w[0].index + 1 && w[1].send_at_us >= w[0].send_at_us));
        assert_eq!(frames[1_100].send_at_us, 1_100 * 2_500 + 12_000, "cadence retrouvée");
    }

    /// Un changement de lien s'applique aux trames produites après lui, et un
    /// flux recréé au retour d'une absence reprend le lien de ce moment-là.
    #[test]
    fn un_changement_de_lien_s_applique_a_son_heure_et_au_retour() {
        let p = PeerProfile { changes: vec![Change { at_s: 10.0, link: Link::preset("4g").unwrap() }], ..regular() };
        assert_eq!(p.link_name_at(9.9), "regular");
        assert_eq!(p.link_name_at(10.0), "4g");
        let frames: Vec<_> = InstrumentSchedule::new(&p, 3).take_while(|f| f.send_at_us < 20_000_000).collect();
        assert!(frames.iter().filter(|f| f.index < 4_000).all(|f| f.send_at_us == f.index * FRAME_US));
        assert!(frames.iter().filter(|f| f.index >= 4_000).any(|f| f.send_at_us > f.index * FRAME_US + 5_000));
        // Recréé à 12 s : déjà en 4G, numérotation repartie de zéro.
        let back: Vec<_> = InstrumentSchedule::resumed(&p, 3, 12_000_000).take(4_000).collect();
        assert!(back.iter().any(|f| f.send_at_us > f.index * FRAME_US + 5_000));
        assert!(back.iter().any(|f| f.index == 0) && back.iter().all(|f| f.index < 4_100));
    }

    /// Instrument et talkback d'un même musicien vivent les mêmes pics : ils
    /// partagent son lien.
    #[test]
    fn instrument_et_voix_partagent_les_pics_du_musicien() {
        let p = PeerProfile { spikes: Some(Spikes { every_mean_s: 1.0, hold_ms: 50.0 }), ..regular() };
        let gaps = |v: Vec<Frame>| -> Vec<u64> {
            v.windows(2).filter(|w| w[1].send_at_us - w[0].send_at_us > 30_000).map(|w| w[1].send_at_us).collect()
        };
        let i = gaps(InstrumentSchedule::new(&p, 8).take(8_000).collect());
        let v = gaps(VoiceSchedule::new(&p, Speech::Always, 8).take(8_000).collect());
        assert!(!i.is_empty());
        assert_eq!(i, v);
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

    #[test]
    fn les_prereglages_sont_valides_et_disent_leur_origine() {
        for name in Link::PRESETS {
            let l = Link::preset(name).unwrap();
            l.validate().unwrap();
            let origin = l.origin.clone().unwrap();
            let measured = ["regular", "ethernet", "wifi"].contains(&name);
            assert_eq!(origin == UNCALIBRATED, !measured, "{name} : {origin}");
        }
    }

    #[test]
    fn les_reglages_absurdes_sont_refuses() {
        let r = regular;
        let bad = [
            PeerProfile { drift_ppm: 1_500.0, ..r() },
            PeerProfile { jitter: Jitter::Pareto { tail_ms: 10.0, shape: 0.5, max_ms: 50.0 }, ..r() },
            PeerProfile { jitter: Jitter::Pareto { tail_ms: 10.0, shape: 2.0, max_ms: 5.0 }, ..r() },
            PeerProfile { burst_loss: Some(BurstLoss { rate_pct: 60.0, mean_packets: 3.0, fixed: false }), ..r() },
            PeerProfile { burst_loss: Some(BurstLoss { rate_pct: 1.0, mean_packets: 0.5, fixed: false }), ..r() },
            PeerProfile { spikes: Some(Spikes { every_mean_s: 1.0, hold_ms: 1_500.0 }), ..r() },
            PeerProfile { reorder: Some(Reorder { pct: 1.0, max_depth: 0 }), ..r() },
            PeerProfile { reorder: Some(Reorder { pct: 1.0, max_depth: 200 }), ..r() },
            PeerProfile { base_delay_ms: -1.0, ..r() },
            PeerProfile { changes: vec![Change { at_s: 0.0, link: Link::preset("wifi").unwrap() }], ..r() },
            PeerProfile {
                changes: vec![
                    Change { at_s: 20.0, link: Link::preset("wifi").unwrap() },
                    Change { at_s: 10.0, link: Link::preset("4g").unwrap() },
                ],
                ..r()
            },
            PeerProfile { absences: vec![Absence { at_s: 10.0, for_s: 0.5 }], ..r() },
            PeerProfile { absences: vec![Absence { at_s: 10.0, for_s: 20.0 }, Absence { at_s: 25.0, for_s: 5.0 }], ..r() },
        ];
        for p in bad {
            assert!(p.validate().is_err(), "{p:?}");
        }
    }

    /// Une faute de frappe dans un profil est refusée (avant R1 elle était
    /// ignorée sans rien dire : un réglage pensé actif ne l'était pas).
    #[test]
    fn une_faute_de_frappe_dans_un_profil_est_refusee() {
        let mut v = serde_json::to_value(PeerProfile { drift_ppm: 50.0, ..regular() }).unwrap();
        assert!(serde_json::from_value::<PeerProfile>(v.clone()).is_ok());
        v["drift_pmm"] = 50.into();
        assert!(serde_json::from_value::<PeerProfile>(v).is_err());
    }
}
