//! Ce que le banc relève chaque seconde, et le résumé par palier.
//!
//! Deux fichiers CSV (flux, machine) gardent TOUT, seconde par seconde : c'est
//! la matière brute, relisable avec n'importe quel outil. Le résumé Markdown en
//! tire un tableau par nombre de musiciens et l'état des critères validés le
//! 28/09/2026 (`PLAN-BANC-N-MUSICIENS-2026-09.md` §3).
//!
//! Tout est calculé ici, sans réseau ni carte son : testable aux bords.

use crate::relay::{PortWindow, Transport};
use crate::server::{SenderWindow, UplinkWindow};
use std::net::SocketAddr;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;

/// Champs relevés pour chaque flux reçu, dans `perf-stats.peers[]`. Un champ
/// absent (agent plus ancien) est noté `NaN` : on ne l'invente pas à zéro.
pub const PEER_FIELDS: &[&str] = &[
    "underruns",
    "holesArrival",
    "holesReception",
    "holesDecode",
    "holesConsumption",
    "holesSequence",
    "holesUnclassified",
    "holesAfterBufferHolds",
    "bufferTargetMs",
    "targetJitterMs",
    "targetGlitchMs",
    // 0.6.6-20 : marge apprise des paquets remplacés à l'échéance (absente avant :
    // colonne vide, relue comme inconnue).
    "targetLateMs",
    "targetReactiveMs",
    "fillMinMs",
    "fillP50Ms",
    "jitterTailMs",
    "concealedUnderrunFrames",
    "concealedPrematureFrames",
    "packetsLost",
    "packetsLate",
    "zeroFilledMs",
    // Lot R1 (réseaux réalistes) : ce que l'agent mesure déjà et que le banc ne
    // lisait pas — la dérive qu'il estime, la gigue moyenne, ses rattrapages.
    "driftPpm",
    "jitterMs",
    "driftDrops",
    "overflowMs",
    "packetsExpected",
    "packetsDuplicate",
    "packetsJump",
];

fn field(name: &str) -> usize {
    PEER_FIELDS.iter().position(|f| *f == name).expect("champ connu")
}

/// Une seconde d'un flux reçu par l'agent.
#[derive(Debug, Clone)]
pub struct PeerRow {
    pub t_s: f64,
    pub musicians: u32,
    /// Nom lisible du flux (musicien simulé + nature).
    pub stream: String,
    pub voice: bool,
    /// Numéro du musicien simulé (2, 3…).
    pub musician: u32,
    /// Lien simulé en vigueur cette seconde-là, et dérive simulée (ppm).
    pub link: String,
    pub sim_ppm: f64,
    pub values: Vec<f64>,
}

/// Ce que le banc sait d'un flux qu'il envoie, pour lire `perf-stats`.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub name: String,
    pub voice: bool,
    pub musician: u32,
    /// Lien en vigueur (mis à jour par le déroulé à chaque changement).
    pub link: String,
    pub sim_ppm: f64,
}

impl PeerRow {
    pub fn get(&self, name: &str) -> f64 {
        self.values[field(name)]
    }
}

/// Une seconde de la machine et du faux serveur.
#[derive(Debug, Clone, Default)]
pub struct MachineRow {
    pub t_s: f64,
    pub musicians: u32,
    pub cpu_pct: f64,
    pub callback_deficit_out: f64,
    pub output_block_frames: f64,
    pub sender_sent: u64,
    pub sender_errors: u64,
    pub sender_late_p99_ms: f64,
    pub sender_late_max_ms: f64,
    pub up_instrument: UplinkWindow,
    pub up_voice: Option<UplinkWindow>,
    /// Mode relais : délai ajouté par le relais (p99, max), en ms ; `NaN` en local.
    pub relay_delay_p99_ms: f64,
    pub relay_delay_max_ms: f64,
    /// Mode relais : régularité de ce qui arrive AU relais (trajet aller).
    pub relay_in: Option<RelayArrivals>,
}

/// Mode relais, une seconde : ce qui arrive au relais depuis la machine
/// mesurée (trajet ALLER), flux instrument seulement — la voix se tait par
/// nature, ses écarts ne disent rien du réseau.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RelayArrivals {
    /// Instruments que le banc envoie à l'agent (les « musiciens simulés ») :
    /// paquets arrivés au relais (0 = rien de mesuré, jamais « aucune coupure »).
    pub down_packets: u64,
    pub down_max_gap_ms: f64,
    pub down_gaps_over_10ms: u64,
    /// Combien de ces flux ont eu au moins une coupure dans la seconde : un
    /// blocage commun les touche tous à la fois.
    pub down_flows_with_gap: u32,
    /// Instrument que l'agent envoie.
    pub up_packets: u64,
    pub up_max_gap_ms: f64,
    pub up_gaps_over_10ms: u64,
}

/// Ce que dit le relais d'une seconde, rapporté aux transports du banc
/// (`roles` : transport du banc → nature). Un port inconnu est ignoré.
pub fn relay_arrivals(ports: &[PortWindow], roles: &HashMap<SocketAddr, Transport>) -> RelayArrivals {
    let mut a = RelayArrivals::default();
    let ms = |us: u64| us as f64 / 1000.0;
    for p in ports {
        match roles.get(&p.bench) {
            Some(Transport::DownInstrument) => {
                a.down_packets += p.from_bench.packets;
                a.down_max_gap_ms = a.down_max_gap_ms.max(ms(p.from_bench.max_gap_us));
                a.down_gaps_over_10ms += p.from_bench.gaps_over_10ms;
                if p.from_bench.gaps_over_10ms > 0 {
                    a.down_flows_with_gap += 1;
                }
            }
            Some(Transport::UpInstrument) => {
                a.up_packets += p.from_agent.packets;
                a.up_max_gap_ms = a.up_max_gap_ms.max(ms(p.from_agent.max_gap_us));
                a.up_gaps_over_10ms += p.from_agent.gaps_over_10ms;
            }
            Some(Transport::DownVoice | Transport::UpVoice) | None => {}
        }
    }
    a
}

/// Mode relais : le relevé d'une seconde (délai propre du relais, en ms, et
/// arrivées au relais).
#[derive(Debug, Clone, Copy)]
pub struct RelayWindow {
    pub delay_p99_ms: f64,
    pub delay_max_ms: f64,
    pub arrivals: RelayArrivals,
}

/// Lignes « flux » d'un message `perf-stats`. `streams` : identifiant du flux
/// → ce que le banc en sait. Un flux inconnu du banc est ignoré.
pub fn peer_rows(perf: &Value, t_s: f64, musicians: u32, streams: &HashMap<String, StreamInfo>) -> Vec<PeerRow> {
    let Some(peers) = perf["peers"].as_array() else { return Vec::new() };
    peers
        .iter()
        .filter_map(|p| {
            let info = streams.get(p["producerId"].as_str()?)?;
            Some(PeerRow {
                t_s,
                musicians,
                stream: info.name.clone(),
                voice: info.voice,
                musician: info.musician,
                link: info.link.clone(),
                sim_ppm: info.sim_ppm,
                values: PEER_FIELDS.iter().map(|f| p[*f].as_f64().unwrap_or(f64::NAN)).collect(),
            })
        })
        .collect()
}

/// Ligne « machine » d'une seconde.
pub fn machine_row(
    perf: Option<&Value>,
    t_s: f64,
    musicians: u32,
    sender: &SenderWindow,
    up_instrument: UplinkWindow,
    up_voice: Option<UplinkWindow>,
    relay: Option<RelayWindow>,
) -> MachineRow {
    let num = |k: &str| perf.and_then(|p| p[k].as_f64()).unwrap_or(f64::NAN);
    let mut late = sender.late_us.clone();
    late.sort_unstable();
    let pct = |p: f64| -> f64 {
        if late.is_empty() {
            f64::NAN
        } else {
            late[((late.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0
        }
    };
    MachineRow {
        t_s,
        musicians,
        cpu_pct: num("cpuPct"),
        callback_deficit_out: num("callbackDeficitOut"),
        output_block_frames: num("outputBlockFrames"),
        sender_sent: sender.sent,
        sender_errors: sender.errors,
        sender_late_p99_ms: pct(0.99),
        sender_late_max_ms: pct(1.0),
        up_instrument,
        up_voice,
        relay_delay_p99_ms: relay.map_or(f64::NAN, |r| r.delay_p99_ms),
        relay_delay_max_ms: relay.map_or(f64::NAN, |r| r.delay_max_ms),
        relay_in: relay.map(|r| r.arrivals),
    }
}

fn fmt(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.3}")
    } else {
        String::new()
    }
}

pub fn peers_csv(rows: &[PeerRow]) -> String {
    let mut s = String::from("t_s,musicians,stream,voice,musician,link,sim_ppm");
    for f in PEER_FIELDS {
        s.push(',');
        s.push_str(f);
    }
    s.push('\n');
    for r in rows {
        let _ = write!(s, "{:.1},{},{},{},{},{},{}", r.t_s, r.musicians, r.stream, r.voice, r.musician, r.link, fmt(r.sim_ppm));
        for v in &r.values {
            s.push(',');
            s.push_str(&fmt(*v));
        }
        s.push('\n');
    }
    s
}

pub fn machine_csv(rows: &[MachineRow]) -> String {
    let mut s = String::from(
        "t_s,musicians,cpu_pct,callback_deficit_out,output_block_frames,sender_sent,sender_errors,\
sender_late_p99_ms,sender_late_max_ms,up_instr_packets,up_instr_max_gap_ms,up_instr_gaps_over_10ms,\
up_instr_seq_missing,up_voice_packets,up_voice_max_gap_ms,up_voice_gaps_over_10ms,up_voice_seq_missing,\
relay_delay_p99_ms,relay_delay_max_ms,relay_in_down_packets,relay_in_down_max_gap_ms,relay_in_down_gaps_over_10ms,\
relay_in_down_flows_with_gap,relay_in_up_packets,relay_in_up_max_gap_ms,relay_in_up_gaps_over_10ms\n",
    );
    let relay_in = |a: Option<&RelayArrivals>| match a {
        Some(a) => format!(
            "{},{},{},{},{},{},{}",
            a.down_packets,
            fmt(a.down_max_gap_ms),
            a.down_gaps_over_10ms,
            a.down_flows_with_gap,
            a.up_packets,
            fmt(a.up_max_gap_ms),
            a.up_gaps_over_10ms
        ),
        None => ",,,,,,".into(),
    };
    let up = |w: Option<&UplinkWindow>| match w {
        Some(w) => format!(
            "{},{},{},{}",
            w.packets,
            fmt(w.max_gap_us as f64 / 1000.0),
            w.gaps_over_10ms,
            w.seq_missing
        ),
        None => ",,,".into(),
    };
    for r in rows {
        let _ = writeln!(
            s,
            "{:.1},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.t_s,
            r.musicians,
            fmt(r.cpu_pct),
            fmt(r.callback_deficit_out),
            fmt(r.output_block_frames),
            r.sender_sent,
            r.sender_errors,
            fmt(r.sender_late_p99_ms),
            fmt(r.sender_late_max_ms),
            up(Some(&r.up_instrument)),
            up(r.up_voice.as_ref()),
            fmt(r.relay_delay_p99_ms),
            fmt(r.relay_delay_max_ms),
            relay_in(r.relay_in.as_ref()),
        );
    }
    s
}

/// Au-delà de ce retard d'envoi, le FAUX SERVEUR crée lui-même de la gigue du
/// même ordre que ce qu'on mesure : c'est le délai de grâce minimal du masquage
/// (`conceal::GRACE_MIN_MS`) et la moitié de la queue de gigue Ethernet simulée.
pub const BENCH_LATE_OK_MS: f64 = 1.0;

/// Précision du banc sur une campagne, à partir du pire retard de chaque seconde.
pub fn precision(max_late_per_second_ms: &[f64]) -> (String, bool) {
    let v: Vec<f64> = max_late_per_second_ms.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return ("aucun envoi mesuré".into(), false);
    }
    let bad = v.iter().filter(|&&x| x > BENCH_LATE_OK_MS).count();
    let worst = v.iter().copied().fold(0.0, f64::max);
    let ok = bad == 0;
    let verdict = if ok {
        "SUFFISANTE"
    } else if bad * 100 <= v.len() {
        "À SURVEILLER"
    } else {
        "INSUFFISANTE — les trous « arrivée » et « séquence » peuvent venir du banc"
    };
    (
        format!(
            "{verdict} ({bad} seconde(s) sur {} avec un envoi en retard de plus de {BENCH_LATE_OK_MS} ms ; pire {worst:.2} ms)",
            v.len()
        ),
        ok,
    )
}

/// Résumé d'un palier (N musiciens), sur sa fenêtre de mesure.
#[derive(Debug, Clone, PartialEq)]
pub struct StepSummary {
    pub musicians: u32,
    pub seconds: f64,
    /// Trous par minute sur l'ensemble des instruments reçus : total, puis par
    /// cause (arrivée, réception, décodage, consommation, séquence, non classé),
    /// puis ceux qui ont suivi un « le tampon tient ». `NaN` si l'agent ne les
    /// mesure pas (« réception » : agent ≥ 0.6.6-6).
    pub underruns_per_min: f64,
    pub holes_per_min: [f64; 7],
    /// Cible du tampon des instruments (ms) : médiane et p95 sur tous les flux.
    pub target_median_ms: f64,
    pub target_p95_ms: f64,
    /// Parts médianes de la cible : gigue, plancher anti-trou, filet réactif.
    pub target_parts_ms: [f64; 3],
    pub cpu_median: f64,
    pub cpu_max: f64,
    /// Secondes où la sortie a manqué ≥ 1 % de ses callbacks.
    pub late_callback_seconds: u32,
    pub sender_late_max_ms: f64,
    pub up_instrument_gaps: u64,
    pub up_voice_gaps: Option<u64>,
    /// Mode relais : coupures déjà présentes à l'ARRIVÉE au relais (trajet
    /// aller) — instruments envoyés par le banc, instrument envoyé par l'agent.
    /// `None` pour un sens où aucun paquet n'a été mesuré : jamais un faux 0.
    pub relay_in_gaps: Option<(Option<u64>, Option<u64>)>,
}

/// Seuil d'une seconde « en retard » pour la sortie : 1 % des callbacks
/// attendus. Le relevé à 1 Hz a une imprécision d'un à deux callbacks (bord de
/// fenêtre) ; 1 % vaut 7 à 8 callbacks à 64 frames, soit ~10 ms de sortie non
/// servie dans la seconde. CONSTANTE DE CLASSEMENT : la valeur brute est au CSV.
const LATE_CALLBACK_FRACTION: f64 = 0.01;

const HOLE_FIELDS: [&str; 7] = [
    "holesArrival",
    "holesReception",
    "holesDecode",
    "holesConsumption",
    "holesSequence",
    "holesUnclassified",
    "holesAfterBufferHolds",
];

/// Accroissement d'un compteur cumulé au fil des relevés d'un flux. Un compteur
/// qui recule a été remis à zéro (flux recréé au retour d'une absence) : on
/// compte depuis la remise à zéro, et ce qui précédait reste compté — le
/// premier et le dernier relevé ne suffisent pas. Inconnu (`NaN`) si un relevé
/// l'est.
fn increase(rows: &[&PeerRow], name: &str) -> f64 {
    let mut total = 0.0;
    for w in rows.windows(2) {
        let (a, b) = (w[0].get(name), w[1].get(name));
        if !a.is_finite() || !b.is_finite() {
            return f64::NAN;
        }
        total += if b >= a { b - a } else { b };
    }
    if rows.iter().any(|r| !r.get(name).is_finite()) {
        return f64::NAN;
    }
    total
}

/// Relevés de chaque flux instrument dans une fenêtre, dans l'ordre du temps,
/// par nom de flux (un flux recréé garde son nom).
fn instrument_rows(peers: &[PeerRow], musicians: u32, from_s: f64, to_s: f64) -> Vec<(&str, Vec<&PeerRow>)> {
    let mut by: Vec<(&str, Vec<&PeerRow>)> = Vec::new();
    for r in peers.iter().filter(|r| !r.voice && r.musicians == musicians && r.t_s >= from_s && r.t_s <= to_s) {
        match by.iter_mut().find(|(n, _)| *n == r.stream) {
            Some((_, v)) => v.push(r),
            None => by.push((r.stream.as_str(), vec![r])),
        }
    }
    by
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// Résume un palier sur `[from_s, to_s]`.
pub fn summarize(peers: &[PeerRow], machine: &[MachineRow], musicians: u32, from_s: f64, to_s: f64) -> StepSummary {
    let in_window = |t: f64| t >= from_s && t <= to_s;
    let minutes = ((to_s - from_s) / 60.0).max(1e-9);
    let per_stream = instrument_rows(peers, musicians, from_s, to_s);
    let mut targets = Vec::new();
    let mut parts: [Vec<f64>; 3] = Default::default();
    for r in per_stream.iter().flat_map(|(_, v)| v.iter()) {
        targets.push(r.get("bufferTargetMs"));
        for (i, f) in ["targetJitterMs", "targetGlitchMs", "targetReactiveMs"].iter().enumerate() {
            parts[i].push(r.get(f));
        }
    }
    let sum_delta = |name: &str| -> f64 { per_stream.iter().map(|(_, v)| increase(v, name)).sum::<f64>() / minutes };
    let mut holes_per_min = [0.0; 7];
    for (i, f) in HOLE_FIELDS.iter().enumerate() {
        holes_per_min[i] = sum_delta(f);
    }
    let m: Vec<&MachineRow> = machine.iter().filter(|r| r.musicians == musicians && in_window(r.t_s)).collect();
    let late_callback_seconds = m
        .iter()
        .filter(|r| {
            let expected = 48_000.0 / r.output_block_frames;
            r.callback_deficit_out.is_finite() && expected.is_finite() && r.callback_deficit_out >= expected * LATE_CALLBACK_FRACTION
        })
        .count() as u32;
    let voice_gaps = m.iter().filter_map(|r| r.up_voice.as_ref().map(|w| w.gaps_over_10ms)).reduce(|a, b| a + b);
    let relay_in: Vec<RelayArrivals> = m.iter().filter_map(|r| r.relay_in).collect();
    let measured = |packets: fn(&RelayArrivals) -> u64, gaps: fn(&RelayArrivals) -> u64| {
        (relay_in.iter().map(packets).sum::<u64>() > 0).then(|| relay_in.iter().map(gaps).sum::<u64>())
    };
    let relay_in_gaps = (!relay_in.is_empty()).then(|| {
        (
            measured(|a| a.down_packets, |a| a.down_gaps_over_10ms),
            measured(|a| a.up_packets, |a| a.up_gaps_over_10ms),
        )
    });
    StepSummary {
        musicians,
        seconds: to_s - from_s,
        underruns_per_min: sum_delta("underruns"),
        holes_per_min,
        target_median_ms: percentile(targets.clone(), 0.5),
        target_p95_ms: percentile(targets, 0.95),
        target_parts_ms: [
            percentile(parts[0].clone(), 0.5),
            percentile(parts[1].clone(), 0.5),
            percentile(parts[2].clone(), 0.5),
        ],
        cpu_median: percentile(m.iter().map(|r| r.cpu_pct).collect(), 0.5),
        cpu_max: percentile(m.iter().map(|r| r.cpu_pct).collect(), 1.0),
        late_callback_seconds,
        sender_late_max_ms: percentile(m.iter().map(|r| r.sender_late_max_ms).collect(), 1.0),
        up_instrument_gaps: m.iter().map(|r| r.up_instrument.gaps_over_10ms).sum(),
        up_voice_gaps: voice_gaps,
        relay_in_gaps,
    }
}

/// État d'un critère.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Verdict {
    #[serde(rename = "tenu")]
    Holds,
    #[serde(rename = "non-tenu")]
    Fails,
    /// Le scénario ne permet pas d'en juger (dit pourquoi dans le détail).
    #[serde(rename = "sans-objet")]
    NotApplicable,
}

impl Verdict {
    /// En toutes lettres : jamais une couleur seule.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Holds => "TENU",
            Verdict::Fails => "NON TENU",
            Verdict::NotApplicable => "sans objet",
        }
    }

    /// Une forme par état, en plus du texte (lisible sans distinguer les
    /// couleurs, et en noir et blanc).
    pub fn symbol(self) -> &'static str {
        match self {
            Verdict::Holds => "✔",
            Verdict::Fails => "✖",
            Verdict::NotApplicable => "○",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Criterion {
    pub id: u8,
    pub text: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

const NOT_MEASURED: &str = "aucun palier mesuré (campagne arrêtée avant la fin de l'installation)";

/// Trous de cause locale (réception, décodage, consommation) sur la campagne.
/// `+ 0.0` : une somme vide vaut −0, qui s'écrirait « -0 ».
fn local_holes(steps: &[StepSummary]) -> f64 {
    steps
        .iter()
        .map(|s| (s.holes_per_min[1] + s.holes_per_min[2] + s.holes_per_min[3]) * s.seconds / 60.0)
        .sum::<f64>()
        + 0.0
}

/// Les quatre critères validés le 28/09/2026.
///
/// - `regular` : flux simulés sans gigue ni perte (critère 1 : sans gigue
///   simulée, une cause locale ne peut pas être confondue avec le réseau).
/// - `voice_sent` : le talkback de l'agent était envoyé (critère 3).
pub fn criteria(steps: &[StepSummary], regular: bool, voice_sent: bool) -> Vec<Criterion> {
    let mut out = Vec::new();

    // 1. Zéro trou de cause locale (réception, décodage, consommation). La
    // réception n'est mesurée qu'à partir de l'agent 0.6.6-6 : avant, elle
    // reste inconnue (NaN) et le critère ne juge pas.
    // Aucun palier mesuré (campagne arrêtée pendant l'installation) : aucun
    // critère ne peut se dire tenu — une absence de mesure n'est pas un succès.
    let measured = !steps.is_empty();
    let local = local_holes(steps);
    out.push(Criterion {
        id: 1,
        text: "zéro trou de cause locale (réception, décodage, consommation), flux réguliers",
        verdict: if !measured || !regular || !local.is_finite() {
            Verdict::NotApplicable
        } else if local == 0.0 {
            Verdict::Holds
        } else {
            Verdict::Fails
        },
        detail: if !measured {
            NOT_MEASURED.into()
        } else if !regular {
            "scénario avec gigue ou pertes simulées : critère réservé aux flux réguliers".into()
        } else if !local.is_finite() {
            "l'Audio Engine ne mesure pas toutes les causes locales (réception : 0.6.6-6 et plus)".into()
        } else {
            format!("{local:.0} trou(s) réception + décodage + consommation sur la campagne")
        },
    });

    // 2. La cible ne croît pas avec N (+1 ms au plus par rapport au 1er palier).
    // Un seul palier : rien à comparer — « tenu » y serait une fausse assurance.
    let base = steps.first().map(|s| s.target_median_ms).unwrap_or(f64::NAN);
    let worst = steps
        .iter()
        .skip(1)
        .map(|s| (s.musicians, s.target_median_ms - base))
        .filter(|(_, d)| d.is_finite())
        .max_by(|a, b| a.1.total_cmp(&b.1));
    out.push(Criterion {
        id: 2,
        text: "la cible du tampon ne croît pas avec le nombre de musiciens (≤ +1 ms)",
        verdict: match worst {
            None => Verdict::NotApplicable,
            Some((_, d)) if d <= 1.0 => Verdict::Holds,
            Some(_) => Verdict::Fails,
        },
        detail: match worst {
            None if steps.len() < 2 => "un seul palier : aucune montée à comparer".into(),
            None => "aucune mesure de cible".into(),
            Some((n, d)) => format!("médiane de départ {base:.1} ms ; pire écart {d:+.1} ms à {n} musiciens"),
        },
    });

    // 3. Voix envoyée sans coupure > 10 ms.
    let gaps: Option<u64> = steps.iter().filter_map(|s| s.up_voice_gaps).reduce(|a, b| a + b);
    out.push(Criterion {
        id: 3,
        text: "talkback envoyé sans coupure de plus de 10 ms",
        verdict: match (voice_sent, gaps) {
            (false, _) | (true, None) => Verdict::NotApplicable,
            (true, Some(0)) => Verdict::Holds,
            (true, Some(_)) => Verdict::Fails,
        },
        detail: match (voice_sent, gaps) {
            (false, _) => "talkback non envoyé dans ce scénario".into(),
            (true, None) => "aucun paquet de voix reçu (voix silencieuse ? cf. la notice)".into(),
            (true, Some(g)) => format!("{g} coupure(s) > 10 ms vue(s) par le faux serveur"),
        },
    });

    // 4. Sortie sans retard.
    let late: u32 = steps.iter().map(|s| s.late_callback_seconds).sum();
    out.push(Criterion {
        id: 4,
        text: "sortie audio sans retard (callbacks manquants < 1 % chaque seconde)",
        verdict: match (measured, late) {
            (false, _) => Verdict::NotApplicable,
            (true, 0) => Verdict::Holds,
            (true, _) => Verdict::Fails,
        },
        detail: if measured { format!("{late} seconde(s) en retard") } else { NOT_MEASURED.into() },
    });
    out
}

/// Un musicien simulé sur un palier : ce que son lien lui a fait.
#[derive(Debug, Clone, PartialEq)]
pub struct MusicianSummary {
    pub stream: String,
    /// Liens successifs sur la fenêtre (« ethernet → wifi-charge »).
    pub links: String,
    pub sim_ppm: f64,
    /// Dernière dérive estimée par l'agent dans la fenêtre (estimation cumulée
    /// depuis le début du flux : la dernière est la plus précise).
    pub read_ppm: f64,
    pub holes_per_min: f64,
    /// Trous/min : arrivée, réception, décodage, consommation, séquence.
    pub causes_per_min: [f64; 5],
    pub target_median_ms: f64,
    pub target_p95_ms: f64,
    /// Trames de son INVENTÉ à l'échéance (masquage d'un paquet absent à son
    /// heure) par minute — ce que l'oreille entend comme « corrigé » (écoute de
    /// Ben du 03/10/2026 : invisible dans les trous et la cible).
    pub invented_per_min: f64,
    /// Paquets écartés en retard (désordre compris), perdus, et audio rattrapé
    /// d'un coup (vidage du tampon trop plein), sur la fenêtre.
    pub late: f64,
    pub lost: f64,
    pub drift_drops: f64,
}

/// Table par musicien, instruments seulement, sur `[from_s, to_s]`.
pub fn musician_summaries(peers: &[PeerRow], musicians: u32, from_s: f64, to_s: f64) -> Vec<MusicianSummary> {
    let minutes = ((to_s - from_s) / 60.0).max(1e-9);
    let mut out: Vec<MusicianSummary> = instrument_rows(peers, musicians, from_s, to_s)
        .into_iter()
        .map(|(stream, rows)| {
            let mut links: Vec<&str> = Vec::new();
            for r in &rows {
                if links.last() != Some(&r.link.as_str()) {
                    links.push(&r.link);
                }
            }
            let per_min = |f: &str| increase(&rows, f) / minutes;
            let targets: Vec<f64> = rows.iter().map(|r| r.get("bufferTargetMs")).collect();
            MusicianSummary {
                stream: stream.to_string(),
                links: links.join(" → "),
                sim_ppm: rows[0].sim_ppm,
                read_ppm: rows.iter().rev().map(|r| r.get("driftPpm")).find(|v| v.is_finite()).unwrap_or(f64::NAN),
                holes_per_min: per_min("underruns"),
                causes_per_min: [
                    per_min("holesArrival"),
                    per_min("holesReception"),
                    per_min("holesDecode"),
                    per_min("holesConsumption"),
                    per_min("holesSequence"),
                ],
                target_median_ms: percentile(targets.clone(), 0.5),
                target_p95_ms: percentile(targets, 0.95),
                invented_per_min: per_min("concealedUnderrunFrames"),
                late: increase(&rows, "packetsLate"),
                lost: increase(&rows, "packetsLost"),
                drift_drops: increase(&rows, "driftDrops"),
            }
        })
        .collect();
    out.sort_by_key(|m| rows_musician(peers, &m.stream));
    out
}

fn rows_musician(peers: &[PeerRow], stream: &str) -> u32 {
    peers.iter().find(|r| r.stream == stream).map_or(0, |r| r.musician)
}

/// Un événement de la campagne, à l'instant où il a eu lieu.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub t_s: f64,
    pub musician: u32,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// Changement de lien (de, vers).
    Change { from: String, to: String },
    /// Départ, pour `for_s` secondes.
    Absence { for_s: f64 },
}

impl Event {
    pub fn describe(&self) -> String {
        match &self.kind {
            EventKind::Change { from, to } => format!("m{} passe de « {from} » à « {to} »", self.musician),
            EventKind::Absence { for_s } => format!("m{} part {for_s:.0} s puis revient", self.musician),
        }
    }

    /// Instant à partir duquel l'événement est « passé » (retour, pour une absence).
    fn settled_s(&self) -> f64 {
        match self.kind {
            EventKind::Change { .. } => self.t_s,
            EventKind::Absence { for_s } => self.t_s + for_s,
        }
    }
}

/// Avant un événement : les 120 s qui le précèdent ; après : de 30 s après lui
/// (le tampon s'adapte) jusqu'à l'événement suivant ou la fin du palier.
/// Définitions validées le 01/10/2026 (critère 6).
pub const EVENT_BEFORE_S: f64 = 120.0;
pub const EVENT_SETTLE_S: f64 = 30.0;

/// Cible d'un flux avant et après un événement.
#[derive(Debug, Clone, PartialEq)]
pub struct EventStream {
    pub stream: String,
    /// Le flux du musicien concerné par l'événement.
    pub concerned: bool,
    pub before_ms: f64,
    pub after_ms: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventSummary {
    pub event: Event,
    pub streams: Vec<EventStream>,
}

impl EventSummary {
    /// Plus grand écart de cible (en valeur absolue) parmi les AUTRES flux.
    pub fn worst_other_shift(&self) -> Option<(String, f64)> {
        self.streams
            .iter()
            .filter(|s| !s.concerned)
            .map(|s| (s.stream.clone(), s.after_ms - s.before_ms))
            .filter(|(_, d)| d.is_finite())
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
    }
}

/// Avant / après de chaque événement. `steps` : (musiciens, début mesuré, fin)
/// de chaque palier — les fenêtres ne débordent pas de leur palier.
pub fn event_summaries(peers: &[PeerRow], events: &[Event], steps: &[(u32, f64, f64)]) -> Vec<EventSummary> {
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));
    sorted
        .iter()
        .enumerate()
        .filter_map(|(k, e)| {
            let &(musicians, start, end) = steps.iter().find(|(_, s, en)| e.t_s >= *s && e.t_s <= *en)?;
            let before = ((e.t_s - EVENT_BEFORE_S).max(start), e.t_s);
            let next = sorted.get(k + 1).map_or(end, |n| n.t_s.min(end));
            let after = (e.settled_s() + EVENT_SETTLE_S, next);
            let median = |rows: &[&PeerRow], (a, b): (f64, f64)| {
                percentile(rows.iter().filter(|r| r.t_s >= a && r.t_s < b).map(|r| r.get("bufferTargetMs")).collect(), 0.5)
            };
            let streams = instrument_rows(peers, musicians, start, end)
                .into_iter()
                .map(|(stream, rows)| EventStream {
                    stream: stream.to_string(),
                    concerned: rows[0].musician == e.musician,
                    before_ms: median(&rows, before),
                    after_ms: median(&rows, after),
                })
                .collect();
            Some(EventSummary { event: (*e).clone(), streams })
        })
        .collect()
}

/// Ce que la dérive a fait aux flux, sur les paliers d'au moins 10 min mesurées.
#[derive(Debug, Clone, PartialEq)]
pub struct DriftCheck {
    /// Secondes mesurées sur les paliers jugés (0 = aucun assez long).
    pub seconds: f64,
    /// Trous, toutes causes, tous instruments.
    pub holes: f64,
    /// Pire écart de cible médiane entre les 5 dernières et les 5 premières
    /// minutes, et son flux.
    pub worst_shift: Option<(String, f64)>,
    /// Pire écart entre la dérive lue par l'agent et la dérive simulée (ppm).
    pub worst_read_error_ppm: f64,
    /// Audio rattrapé d'un coup par l'agent (échantillons) : à surveiller.
    pub drift_drops: f64,
}

/// Fenêtre de comparaison du critère 7 : 5 min au début, 5 min à la fin.
pub const DRIFT_EDGE_S: f64 = 300.0;

/// Écart d'horloge entre l'émetteur distant et la machine mesurée (ppm) :
/// commun à tous les flux, il s'ajoute à la dérive que l'agent estime. Médiane
/// de (lue − simulée) sur les musiciens : robuste à un flux mal mesuré, juste
/// que les dérives simulées soient nulles ou symétriques. `NaN` sans mesure.
pub fn clock_offset_ppm(musicians: &[MusicianSummary]) -> f64 {
    percentile(musicians.iter().map(|m| m.read_ppm - m.sim_ppm).collect(), 0.5)
}

/// `clock_offset_ppm` : écart d'horloge à retirer avant de comparer la dérive
/// lue à la simulée (0 quand les flux partent de la machine mesurée).
pub fn drift_check(peers: &[PeerRow], steps: &[(u32, f64, f64)], clock_offset_ppm: f64) -> DriftCheck {
    let mut c = DriftCheck { seconds: 0.0, holes: 0.0, worst_shift: None, worst_read_error_ppm: f64::NAN, drift_drops: 0.0 };
    for &(musicians, start, end) in steps.iter().filter(|(_, s, e)| e - s >= 2.0 * DRIFT_EDGE_S) {
        c.seconds += end - start;
        for (stream, rows) in instrument_rows(peers, musicians, start, end) {
            c.holes += increase(&rows, "underruns");
            c.drift_drops += increase(&rows, "driftDrops");
            let median = |a: f64, b: f64| {
                percentile(rows.iter().filter(|r| r.t_s >= a && r.t_s < b).map(|r| r.get("bufferTargetMs")).collect(), 0.5)
            };
            let shift = median(end - DRIFT_EDGE_S, end + 1.0) - median(start, start + DRIFT_EDGE_S);
            if shift.is_finite() && c.worst_shift.as_ref().is_none_or(|(_, w)| shift.abs() > w.abs()) {
                c.worst_shift = Some((stream.to_string(), shift));
            }
            if let Some(last) = rows.iter().rev().find(|r| r.get("driftPpm").is_finite()) {
                let offset = if clock_offset_ppm.is_finite() { clock_offset_ppm } else { 0.0 };
                let err = (last.get("driftPpm") - offset - last.sim_ppm).abs();
                c.worst_read_error_ppm = if c.worst_read_error_ppm.is_finite() { c.worst_read_error_ppm.max(err) } else { err };
            }
        }
    }
    c
}

/// Critères 5 à 7, validés le 01/10/2026 (PLAN-BANC-REALISTE-2026-10 § 3).
///
/// - `network`  : le scénario simule un réseau (gigue, pertes, pics, désordre,
///   dérive ou événements) — sinon le critère 1 juge déjà les causes locales ;
/// - `events`   : avant / après des événements ;
/// - `drift`    : `Some` si seule la dérive est simulée (critère 7).
pub fn network_criteria(steps: &[StepSummary], network: bool, events: &[EventSummary], drift: Option<&DriftCheck>) -> Vec<Criterion> {
    let mut out = Vec::new();

    // 5. Le réseau ne fait pas tomber la machine : aucun trou de cause locale.
    // « Réception » comprise (décision du 01/10/2026) : elle est locale par
    // définition, comme dans le critère 1.
    let local = local_holes(steps);
    out.push(Criterion {
        id: 5,
        text: "le réseau ne fait pas tomber la machine : aucun trou de cause locale (réception, décodage, consommation) sous réseau simulé",
        verdict: if steps.is_empty() || !network || !local.is_finite() {
            Verdict::NotApplicable
        } else if local == 0.0 {
            Verdict::Holds
        } else {
            Verdict::Fails
        },
        detail: if steps.is_empty() {
            NOT_MEASURED.into()
        } else if !network {
            "aucun réseau simulé : cf. critère 1".into()
        } else if !local.is_finite() {
            "l'Audio Engine ne mesure pas toutes les causes locales (réception : 0.6.6-6 et plus)".into()
        } else {
            // `+ 0.0` : une somme vide vaut −0, qui s'écrirait « -0 ».
            let network_holes: f64 = steps.iter().map(|s| (s.holes_per_min[0] + s.holes_per_min[4]) * s.seconds / 60.0).sum::<f64>() + 0.0;
            format!("{local:.0} trou(s) de cause locale ; trous dus au réseau (arrivée + séquence), comptés : {network_holes:.0}")
        },
    });

    // 6. Un mauvais réseau ne pénalise que lui.
    let changes: Vec<&EventSummary> = events.iter().filter(|e| matches!(e.event.kind, EventKind::Change { .. })).collect();
    let worst = changes
        .iter()
        .filter_map(|e| e.worst_other_shift().map(|w| (e, w)))
        .max_by(|a, b| a.1 .1.abs().total_cmp(&b.1 .1.abs()));
    out.push(Criterion {
        id: 6,
        text: "un mauvais réseau ne pénalise que lui : la cible des autres flux ne bouge pas de plus de 1 ms quand un lien change",
        verdict: match (changes.is_empty(), &worst) {
            (true, _) | (false, None) => Verdict::NotApplicable,
            (false, Some((_, (_, d)))) if d.abs() <= 1.0 => Verdict::Holds,
            _ => Verdict::Fails,
        },
        detail: match (changes.is_empty(), &worst) {
            (true, _) => "aucun changement de lien dans ce scénario".into(),
            (false, None) => "fenêtres avant / après sans mesure (palier trop court ?)".into(),
            (false, Some((e, (stream, d)))) => format!(
                "pire écart {d:+.1} ms ({stream}, quand {}) — médianes des {EVENT_BEFORE_S:.0} s avant / de {EVENT_SETTLE_S:.0} s après jusqu'à la suite ; cible entière (tronquée) : ±1 ms d'arrondi",
                e.event.describe()
            ),
        },
    });

    // 7. La dérive est absorbée.
    out.push(Criterion {
        id: 7,
        text: "la dérive est absorbée : aucun trou, et la cible stable (≤ 1 ms entre les 5 premières et les 5 dernières minutes)",
        verdict: match drift {
            None => Verdict::NotApplicable,
            Some(d) if d.seconds == 0.0 || !d.holes.is_finite() => Verdict::NotApplicable,
            Some(d) => match &d.worst_shift {
                Some((_, s)) if d.holes == 0.0 && s.abs() <= 1.0 => Verdict::Holds,
                None => Verdict::NotApplicable,
                _ => Verdict::Fails,
            },
        },
        detail: match drift {
            None => "réservé aux scénarios où seule la dérive est simulée".into(),
            Some(d) if d.seconds == 0.0 => format!("aucun palier d'au moins {:.0} min mesurées", 2.0 * DRIFT_EDGE_S / 60.0),
            Some(d) if !d.holes.is_finite() => "l'Audio Engine ne compte pas les trous".into(),
            Some(d) => format!(
                "{:.0} trou(s) sur {:.0} min ; pire écart de cible {} ; dérive lue par l'Audio Engine à {} ppm près de la simulée ; audio rattrapé d'un coup (à surveiller) : {:.0} échantillons",
                d.holes,
                d.seconds / 60.0,
                d.worst_shift.as_ref().map_or("—".into(), |(s, v)| format!("{v:+.1} ms ({s})")),
                cell(d.worst_read_error_ppm),
                d.drift_drops
            ),
        },
    });
    out
}

fn cell(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.1}")
    } else {
        "—".into()
    }
}

/// Un compte entier, ou « — » s'il est inconnu.
fn count(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.0}")
    } else {
        "—".into()
    }
}

/// Le résumé lisible : en-tête (quoi, où, quelle version), tableau par palier,
/// critères.
pub fn markdown(
    header: &[(String, String)],
    steps: &[StepSummary],
    musicians: &[MusicianSummary],
    events: &[EventSummary],
    criteria: &[Criterion],
) -> String {
    let mut s = String::from("# Banc « N musiciens » — résumé\n\n");
    for (k, v) in header {
        let _ = writeln!(s, "- **{k}** : {v}");
    }
    s.push_str(
        "\n## Par palier (instruments reçus)\n\n\
| Musiciens | Trous/min | arrivée | réception | décodage | consommation | séquence | non classé | après « tampon tient » \
| Cible médiane (ms) | Cible p95 (ms) | dont gigue / anti-trou / réactif (ms) | CPU méd. / max (%) \
| Sortie en retard (s) | Retard max du banc (ms) | Coupures instrument envoyé | Coupures voix envoyée \
| Coupures à l'arrivée au relais (reçus / envoyé) |\n\
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for st in steps {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} / {} / {} | {} / {} | {} | {} | {} | {} | {} |",
            st.musicians,
            cell(st.underruns_per_min),
            cell(st.holes_per_min[0]),
            cell(st.holes_per_min[1]),
            cell(st.holes_per_min[2]),
            cell(st.holes_per_min[3]),
            cell(st.holes_per_min[4]),
            cell(st.holes_per_min[5]),
            cell(st.holes_per_min[6]),
            cell(st.target_median_ms),
            cell(st.target_p95_ms),
            cell(st.target_parts_ms[0]),
            cell(st.target_parts_ms[1]),
            cell(st.target_parts_ms[2]),
            cell(st.cpu_median),
            cell(st.cpu_max),
            st.late_callback_seconds,
            cell(st.sender_late_max_ms),
            st.up_instrument_gaps,
            st.up_voice_gaps.map_or("—".into(), |g| g.to_string()),
            st.relay_in_gaps.map_or("—".into(), |(d, u)| {
                let side = |g: Option<u64>| g.map_or("non mesuré".into(), |g| g.to_string());
                format!("{} / {}", side(d), side(u))
            }),
        );
    }
    if !musicians.is_empty() {
        s.push_str(
            "\n## Par musicien simulé (dernier palier, instruments)\n\n\
| Flux | Lien | Dérive simulée / lue (ppm) | Trous/min | arrivée | réception | décodage | consommation | séquence \
| Cible médiane / p95 (ms) | Son inventé (trames/min) | Paquets en retard | Paquets perdus | Audio rattrapé (échantillons) |\n\
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        for m in musicians {
            let _ = writeln!(
                s,
                "| {} | {} | {} / {} | {} | {} | {} | {} | {} | {} | {} / {} | {} | {} | {} | {} |",
                m.stream,
                m.links,
                cell(m.sim_ppm),
                cell(m.read_ppm),
                cell(m.holes_per_min),
                cell(m.causes_per_min[0]),
                cell(m.causes_per_min[1]),
                cell(m.causes_per_min[2]),
                cell(m.causes_per_min[3]),
                cell(m.causes_per_min[4]),
                cell(m.target_median_ms),
                cell(m.target_p95_ms),
                cell(m.invented_per_min),
                count(m.late),
                count(m.lost),
                count(m.drift_drops),
            );
        }
    }
    if !events.is_empty() {
        s.push_str("\n## Événements (cible médiane des instruments, avant → après)\n\n");
        for e in events {
            let _ = writeln!(s, "- **à {:.0} s : {}**", e.event.t_s, e.event.describe());
            for st in &e.streams {
                let _ = writeln!(
                    s,
                    "  - {}{} : {} → {} ms",
                    st.stream,
                    if st.concerned { " (concerné)" } else { "" },
                    cell(st.before_ms),
                    cell(st.after_ms)
                );
            }
        }
    }
    s.push_str("\n## Critères (1-4 validés le 28/09/2026, 5-7 le 01/10/2026)\n\n");
    for c in criteria {
        let _ = writeln!(s, "{}. {} **{}** — {} ({})", c.id, c.verdict.symbol(), c.verdict.label(), c.text, c.detail);
    }
    s.push_str(
        "\n« Retard max du banc » : de combien le FAUX SERVEUR a envoyé en retard sur son \
calendrier. S'il approche la queue de gigue du profil, c'est le banc qui fait la gigue, \
pas le réseau simulé — à lire avant tout le reste.\n",
    );
    if steps.iter().any(|st| st.relay_in_gaps.is_some()) {
        s.push_str(
            "\n« Coupures à l'arrivée au relais » : écarts de plus de 10 ms entre deux paquets d'un même \
flux instrument, vus EN ARRIVANT au relais (datés par son système quand il le permet). Tout ce \
qui y arrive vient de la machine mesurée : présentes ici, les coupures naissent À L'ALLER \
(envoi de la machine mesurée, câble, réception du relais) ; absentes ici mais présentes dans les \
trous « arrivée » de l'agent, elles naissent AU RETOUR (envoi du relais, câble, réception de la \
machine mesurée).\n",
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(t: f64, n: u32, stream: &str, voice: bool, set: &[(&str, f64)]) -> PeerRow {
        let mut values = vec![0.0; PEER_FIELDS.len()];
        for (k, v) in set {
            values[field(k)] = *v;
        }
        PeerRow { t_s: t, musicians: n, stream: stream.into(), voice, musician: 0, link: "regular".into(), sim_ppm: 0.0, values }
    }

    #[test]
    fn les_champs_des_flux_sont_lus_et_un_champ_absent_reste_inconnu() {
        let perf = json!({ "peers": [
            { "producerId": "bench-p2", "underruns": 3, "bufferTargetMs": 11, "holesArrival": 2 },
            { "producerId": "inconnu", "underruns": 9 },
        ]});
        let info = StreamInfo { name: "m2".into(), voice: false, musician: 2, link: "wifi".into(), sim_ppm: -30.0 };
        let names = HashMap::from([("bench-p2".to_string(), info)]);
        let rows = peer_rows(&perf, 1.0, 2, &names);
        assert_eq!(rows.len(), 1, "un flux inconnu du banc est ignoré");
        assert_eq!(rows[0].get("underruns"), 3.0);
        assert_eq!(rows[0].get("holesArrival"), 2.0);
        assert!(rows[0].get("holesDecode").is_nan(), "absent ≠ zéro");
        assert_eq!((rows[0].musician, rows[0].link.as_str(), rows[0].sim_ppm), (2, "wifi", -30.0));
    }

    /// Un flux recréé (retour d'absence) repart de zéro : ce qu'il avait
    /// compté avant reste compté, ce qu'il compte après s'y ajoute.
    #[test]
    fn un_compteur_remis_a_zero_compte_avant_et_apres_la_remise() {
        let rows: Vec<PeerRow> = [5.0, 8.0, 1.0, 4.0].iter().map(|&u| row(0.0, 2, "m2", false, &[("underruns", u)])).collect();
        let refs: Vec<&PeerRow> = rows.iter().collect();
        assert_eq!(increase(&refs, "underruns"), 3.0 + 1.0 + 3.0);
        assert_eq!(increase(&refs[..1], "underruns"), 0.0);
        let unknown = row(0.0, 2, "m2", false, &[("underruns", f64::NAN)]);
        assert!(increase(&[refs[0], &unknown], "underruns").is_nan());
    }

    /// Deux flux, une minute : les trous se somment par cause, la fenêtre
    /// d'installation est exclue.
    #[test]
    fn le_resume_d_un_palier_somme_les_causes_et_exclut_l_installation() {
        let mut peers = Vec::new();
        for (s, arr) in [("m2", 2.0), ("m3", 1.0)] {
            // Pendant l'installation : un trou qui ne doit PAS compter.
            peers.push(row(0.0, 3, s, false, &[("underruns", 0.0), ("holesArrival", 0.0), ("bufferTargetMs", 10.0)]));
            peers.push(row(30.0, 3, s, false, &[("underruns", 1.0), ("holesArrival", 1.0), ("bufferTargetMs", 10.0)]));
            peers.push(row(90.0, 3, s, false, &[("underruns", 1.0 + arr), ("holesArrival", 1.0 + arr), ("bufferTargetMs", 12.0)]));
        }
        // Une voix, qui ne compte pas dans les instruments.
        peers.push(row(30.0, 3, "m2-voix", true, &[("underruns", 0.0)]));
        peers.push(row(90.0, 3, "m2-voix", true, &[("underruns", 50.0)]));
        let s = summarize(&peers, &[], 3, 30.0, 90.0);
        assert_eq!(s.underruns_per_min, 3.0);
        assert_eq!(s.holes_per_min[0], 3.0);
        assert_eq!(s.target_median_ms, 12.0);
    }

    #[test]
    fn une_seconde_de_sortie_en_retard_se_compte_a_partir_de_1_pour_cent() {
        let mk = |d: f64| MachineRow { t_s: 40.0, musicians: 2, callback_deficit_out: d, output_block_frames: 64.0, ..Default::default() };
        // 750 callbacks/s attendus à 64 frames : 1 % = 7,5.
        let s = summarize(&[], &[mk(1.0), mk(7.0), mk(8.0)], 2, 30.0, 90.0);
        assert_eq!(s.late_callback_seconds, 1);
    }

    fn step(n: u32, target: f64, decode: f64) -> StepSummary {
        StepSummary {
            musicians: n,
            seconds: 60.0,
            underruns_per_min: decode,
            holes_per_min: [0.0, 0.0, decode, 0.0, 0.0, 0.0, 0.0],
            target_median_ms: target,
            target_p95_ms: target,
            target_parts_ms: [target, 0.0, 0.0],
            cpu_median: 10.0,
            cpu_max: 20.0,
            late_callback_seconds: 0,
            sender_late_max_ms: 0.5,
            up_instrument_gaps: 0,
            up_voice_gaps: None,
            relay_in_gaps: None,
        }
    }

    #[test]
    fn les_criteres_disent_tenu_ou_non_tenu_en_toutes_lettres() {
        let ok = criteria(&[step(2, 5.0, 0.0), step(9, 5.8, 0.0)], true, false);
        assert_eq!(ok[0].verdict, Verdict::Holds);
        assert_eq!(ok[1].verdict, Verdict::Holds);
        assert_eq!(ok[2].verdict, Verdict::NotApplicable, "pas de voix envoyée");
        assert_eq!(ok[3].verdict, Verdict::Holds);

        let ko = criteria(&[step(2, 5.0, 0.0), step(9, 11.0, 1.0)], true, false);
        assert_eq!(ko[0].verdict, Verdict::Fails);
        assert_eq!(ko[1].verdict, Verdict::Fails);
        assert!(ko[1].detail.contains("+6.0 ms à 9"), "{}", ko[1].detail);
        assert_eq!(ko[1].verdict.label(), "NON TENU");
    }

    /// Campagne arrêtée avant tout palier mesuré : aucun critère n'est « tenu »
    /// (vu le 01/10/2026 : un studio rouvert avait repris l'Audio Engine au
    /// bout d'une seconde, et le résumé affichait « TENU »).
    #[test]
    fn sans_palier_mesure_aucun_critere_n_est_tenu() {
        let mut all = criteria(&[], true, false);
        all.extend(network_criteria(&[], true, &[], None));
        assert!(all.iter().all(|c| c.verdict == Verdict::NotApplicable), "{all:?}");
        assert!(all[0].detail.contains("aucun palier mesuré"));
        assert!(!markdown(&[], &[], &[], &[], &all).contains("-0"));
    }

    /// Un seul palier : le critère 2 n'a rien à comparer.
    #[test]
    fn le_critere_2_ne_juge_pas_un_palier_unique() {
        let c = criteria(&[step(9, 11.0, 0.0)], true, false);
        assert_eq!(c[1].verdict, Verdict::NotApplicable);
        assert!(c[1].detail.contains("un seul palier"));
    }

    /// Avec de la gigue simulée, un trou n'est pas forcément local : le critère
    /// 1 ne juge pas. Sans mesure de cause (vieil agent), non plus.
    #[test]
    fn le_critere_1_ne_juge_que_ce_qu_il_peut_prouver() {
        let c = criteria(&[step(2, 5.0, 1.0)], false, false);
        assert_eq!(c[0].verdict, Verdict::NotApplicable);
        let c = criteria(&[step(2, 5.0, f64::NAN)], true, false);
        assert_eq!(c[0].verdict, Verdict::NotApplicable);
    }

    /// Un flux par musicien : `m`, nom `m{m}`, une ligne par seconde de 0 à
    /// `secs`, cible donnée par `target(t)`, trous et dérive au choix.
    fn stream(m: u32, secs: u32, target: impl Fn(f64) -> f64, underruns_at: &[f64], sim_ppm: f64, read_ppm: f64) -> Vec<PeerRow> {
        (0..=secs)
            .map(|t| {
                let t = f64::from(t);
                let holes = underruns_at.iter().filter(|&&h| h <= t).count() as f64;
                let mut r = row(t, 9, &format!("m{m}"), false, &[
                    ("bufferTargetMs", target(t)),
                    ("underruns", holes),
                    ("holesArrival", holes),
                    ("driftPpm", read_ppm),
                    ("packetsLate", t),
                ]);
                r.musician = m;
                r.sim_ppm = sim_ppm;
                r.link = if t >= 300.0 && m == 9 { "wifi-charge".into() } else { "ethernet".into() };
                r
            })
            .collect()
    }

    #[test]
    fn la_table_par_musicien_dit_lien_derive_trous_et_cible() {
        let mut peers = stream(3, 600, |_| 5.0, &[], 40.0, 39.5);
        peers.extend(stream(9, 600, |t| if t < 300.0 { 5.0 } else { 14.0 }, &[400.0, 500.0], -20.0, -19.0));
        let t = musician_summaries(&peers, 9, 30.0, 600.0);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].stream.as_str(), t[1].stream.as_str()), ("m3", "m9"));
        assert_eq!(t[1].links, "ethernet → wifi-charge");
        assert_eq!((t[1].sim_ppm, t[1].read_ppm), (-20.0, -19.0));
        assert!((t[1].holes_per_min - 2.0 / 9.5).abs() < 1e-9);
        assert_eq!(t[1].causes_per_min[0], t[1].holes_per_min);
        assert_eq!(t[0].late, 570.0);
        let md = markdown(&[], &[], &t, &[], &[]);
        assert!(md.contains("| m9 | ethernet → wifi-charge | -20.0 / -19.0 |"), "{md}");
    }

    /// Critère 6 : m9 passe en Wi-Fi chargé ; sa cible monte, celle des autres
    /// non → tenu. Si un autre flux prend +2 ms → non tenu, avec son nom.
    #[test]
    fn le_critere_6_compare_les_autres_avant_et_apres_le_changement() {
        let change = Event { t_s: 300.0, musician: 9, kind: EventKind::Change { from: "ethernet".into(), to: "wifi-charge".into() } };
        let steps = [(9, 30.0, 600.0)];
        let mut peers = stream(3, 600, |_| 5.0, &[], 0.0, 0.0);
        peers.extend(stream(9, 600, |t| if t < 300.0 { 5.0 } else { 14.0 }, &[], 0.0, 0.0));
        let ev = event_summaries(&peers, std::slice::from_ref(&change), &steps);
        assert_eq!(ev.len(), 1);
        let m9 = ev[0].streams.iter().find(|s| s.stream == "m9").unwrap();
        assert!(m9.concerned && m9.before_ms == 5.0 && m9.after_ms == 14.0);
        let c = &network_criteria(&[], true, &ev, None)[1];
        assert_eq!((c.id, c.verdict), (6, Verdict::Holds), "{}", c.detail);

        // Un autre flux touché (m3 : +2 ms après 330 s).
        let mut bad = stream(3, 600, |t| if t < 320.0 { 5.0 } else { 7.0 }, &[], 0.0, 0.0);
        bad.extend(stream(9, 600, |t| if t < 300.0 { 5.0 } else { 14.0 }, &[], 0.0, 0.0));
        let ev = event_summaries(&bad, &[change], &steps);
        let c = &network_criteria(&[], true, &ev, None)[1];
        assert_eq!(c.verdict, Verdict::Fails);
        assert!(c.detail.contains("+2.0 ms (m3"), "{}", c.detail);

        // Sans changement de lien : sans objet.
        let absence = Event { t_s: 300.0, musician: 9, kind: EventKind::Absence { for_s: 20.0 } };
        let ev = event_summaries(&peers, &[absence], &steps);
        assert_eq!(network_criteria(&[], true, &ev, None)[1].verdict, Verdict::NotApplicable);
        assert!(markdown(&[], &[], &[], &ev, &[]).contains("m9 part 20 s puis revient"));
    }

    /// La fenêtre « après » d'une absence commence au RETOUR (+30 s), et une
    /// fenêtre s'arrête à l'événement suivant.
    #[test]
    fn les_fenetres_d_un_evenement_respectent_le_retour_et_la_suite() {
        let peers = stream(3, 600, |t| if (300.0..380.0).contains(&t) { 20.0 } else { 5.0 }, &[], 0.0, 0.0);
        let events = [
            Event { t_s: 300.0, musician: 3, kind: EventKind::Absence { for_s: 50.0 } },
            Event { t_s: 500.0, musician: 3, kind: EventKind::Change { from: "a".into(), to: "b".into() } },
        ];
        let ev = event_summaries(&peers, &events, &[(9, 30.0, 600.0)]);
        // Après = [380, 500) : la cible y vaut 5 (les 20 ms sont avant 380).
        assert_eq!(ev[0].streams[0].after_ms, 5.0);
    }

    /// Critère 7 : sans trou et cible stable → tenu ; un trou → non tenu ; un
    /// palier de moins de 10 min → sans objet ; pas de scénario de dérive → sans
    /// objet.
    #[test]
    fn le_critere_7_juge_trous_et_stabilite_sur_au_moins_10_minutes() {
        let steps = [(9, 30.0, 1830.0)];
        let mut ok = stream(2, 1830, |_| 5.0, &[], -100.0, -99.2);
        ok.extend(stream(3, 1830, |t| if t > 1500.0 { 6.0 } else { 5.0 }, &[], 100.0, 101.0));
        let d = drift_check(&ok, &steps, 0.0);
        assert_eq!((d.holes, d.seconds), (0.0, 1800.0));
        assert!((d.worst_read_error_ppm - 1.0).abs() < 1e-9);
        let c = &network_criteria(&[], true, &[], Some(&d))[2];
        assert_eq!((c.id, c.verdict), (7, Verdict::Holds), "{}", c.detail);
        assert!(c.detail.contains("+1.0 ms (m3)"), "{}", c.detail);

        let holed = stream(2, 1830, |_| 5.0, &[900.0], -100.0, -100.0);
        assert_eq!(network_criteria(&[], true, &[], Some(&drift_check(&holed, &steps, 0.0)))[2].verdict, Verdict::Fails);
        let drifting = stream(2, 1830, |t| 5.0 + t / 600.0, &[], -100.0, -100.0);
        assert_eq!(network_criteria(&[], true, &[], Some(&drift_check(&drifting, &steps, 0.0)))[2].verdict, Verdict::Fails);

        let short = drift_check(&ok, &[(9, 30.0, 400.0)], 0.0);
        let c = &network_criteria(&[], true, &[], Some(&short))[2];
        assert_eq!(c.verdict, Verdict::NotApplicable);
        assert!(c.detail.contains("10 min"), "{}", c.detail);
        assert_eq!(network_criteria(&[], true, &[], None)[2].verdict, Verdict::NotApplicable);
    }

    /// Émetteur distant : un écart d'horloge commun (+7 ppm) s'ajoute à chaque
    /// dérive lue ; il est estimé puis retiré avant de comparer.
    #[test]
    fn l_ecart_d_horloge_commun_est_estime_puis_retire() {
        let steps = [(9, 30.0, 1830.0)];
        let mut peers = stream(2, 1830, |_| 5.0, &[], -100.0, -93.0);
        peers.extend(stream(3, 1830, |_| 5.0, &[], 100.0, 107.5));
        peers.extend(stream(4, 1830, |_| 5.0, &[], 0.0, 7.0));
        let table = musician_summaries(&peers, 9, 30.0, 1830.0);
        let offset = clock_offset_ppm(&table);
        assert!((offset - 7.0).abs() < 1e-9, "{offset}");
        let d = drift_check(&peers, &steps, offset);
        assert!((d.worst_read_error_ppm - 0.5).abs() < 1e-9, "{}", d.worst_read_error_ppm);
        assert!((drift_check(&peers, &steps, 0.0).worst_read_error_ppm - 7.5).abs() < 1e-9);
        assert!(clock_offset_ppm(&[]).is_nan());
    }

    /// Critère 5 : sous réseau simulé, seules les causes locales le font tomber ;
    /// les trous dus au réseau sont comptés, pas reprochés.
    #[test]
    fn le_critere_5_ne_reproche_que_les_trous_de_cause_locale() {
        let mut net = step(9, 5.0, 0.0);
        net.holes_per_min = [12.0, 0.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        let c = &network_criteria(std::slice::from_ref(&net), true, &[], None)[0];
        assert_eq!((c.id, c.verdict), (5, Verdict::Holds));
        assert!(c.detail.contains("comptés : 15"), "{}", c.detail);
        net.holes_per_min[1] = 1.0; // réception : locale
        assert_eq!(network_criteria(std::slice::from_ref(&net), true, &[], None)[0].verdict, Verdict::Fails);
        assert_eq!(network_criteria(&[net], false, &[], None)[0].verdict, Verdict::NotApplicable);
    }

    /// Chaque état se lit par sa forme ET son texte, jamais par une couleur.
    #[test]
    fn un_critere_s_ecrit_avec_un_symbole_et_un_mot() {
        let c = criteria(&[step(2, 5.0, 0.0), step(9, 11.0, 1.0)], true, false);
        let md = markdown(&[], &[], &[], &[], &c);
        assert!(md.contains("1. ✖ **NON TENU**"), "{md}");
        assert!(md.contains("3. ○ **sans objet**"), "{md}");
        assert!(md.contains("4. ✔ **TENU**"), "{md}");
    }

    #[test]
    fn la_precision_du_banc_se_dit_en_toutes_lettres() {
        assert!(precision(&[0.2, 0.5, 0.9]).0.starts_with("SUFFISANTE"));
        let mut v = vec![0.3; 199];
        v.push(3.0);
        assert!(precision(&v).0.starts_with("À SURVEILLER"));
        assert!(precision(&[0.3, 2.0, 4.0]).0.starts_with("INSUFFISANTE"));
        assert!(!precision(&[]).1);
    }

    /// Seuls les flux instrument comptent, chacun dans son sens ; un blocage
    /// commun se voit au nombre de flux touchés dans la seconde.
    #[test]
    fn les_arrivees_au_relais_se_lisent_par_sens_et_par_nature() {
        use crate::relay::GapWindow;
        let addr = |p: u16| -> SocketAddr { format!("192.168.1.20:{p}").parse().unwrap() };
        let gw = |max_gap_us: u64, gaps: u64| GapWindow { packets: 400, max_gap_us, gaps_over_10ms: gaps };
        let port = |p: u16, from_bench: GapWindow, from_agent: GapWindow| PortWindow { bench: addr(p), from_bench, from_agent };
        let roles = HashMap::from([
            (addr(1), Transport::DownInstrument),
            (addr(2), Transport::DownInstrument),
            (addr(3), Transport::DownVoice),
            (addr(4), Transport::UpInstrument),
        ]);
        let ports = [
            port(1, gw(16_000, 1), gw(100_000, 1)), // perçages de l'agent : pas du trajet mesuré
            port(2, gw(17_500, 2), GapWindow::default()),
            port(3, gw(900_000, 5), GapWindow::default()), // voix : se tait par nature
            port(4, GapWindow::default(), gw(12_000, 1)),
            port(9, gw(50_000, 9), GapWindow::default()), // port inconnu
        ];
        let a = relay_arrivals(&ports, &roles);
        assert_eq!(
            a,
            RelayArrivals {
                down_packets: 800,
                down_max_gap_ms: 17.5,
                down_gaps_over_10ms: 3,
                down_flows_with_gap: 2,
                up_packets: 400,
                up_max_gap_ms: 12.0,
                up_gaps_over_10ms: 1
            }
        );
        // Aucun port reconnu : rien de mesuré, pas « aucune coupure ».
        assert_eq!(relay_arrivals(&ports, &HashMap::new()), RelayArrivals::default());
    }

    #[test]
    fn le_resume_ne_parle_du_relais_qu_en_mode_relais() {
        let local = markdown(&[], &[step(2, 5.0, 0.0)], &[], &[], &[]);
        assert!(local.contains("| — |") && !local.contains("« Coupures à l'arrivée au relais » :"));
        let mut st = step(2, 5.0, 0.0);
        st.relay_in_gaps = Some((Some(12), Some(3)));
        let relais = markdown(&[], &[st], &[], &[], &[]);
        assert!(relais.contains("| 12 / 3 |") && relais.contains("naissent À L'ALLER"), "{relais}");
    }

    /// Un sens où le relais n'a vu aucun paquet se dit « non mesuré » : un 0
    /// laisserait croire que ce trajet est propre.
    #[test]
    fn un_sens_sans_paquet_au_relais_se_dit_non_mesure() {
        let row = |a: RelayArrivals| MachineRow { t_s: 40.0, musicians: 2, relay_in: Some(a), ..Default::default() };
        let vide = summarize(&[], &[row(RelayArrivals::default())], 2, 30.0, 90.0);
        assert_eq!(vide.relay_in_gaps, Some((None, None)));
        let md = markdown(&[], &[vide], &[], &[], &[]);
        assert!(md.contains("| non mesuré / non mesuré |"), "{md}");
        let propre = RelayArrivals { down_packets: 3200, up_packets: 400, down_max_gap_ms: 2.9, up_max_gap_ms: 2.8, ..Default::default() };
        assert_eq!(summarize(&[], &[row(propre)], 2, 30.0, 90.0).relay_in_gaps, Some((Some(0), Some(0))));
    }

    #[test]
    fn le_csv_machine_a_autant_de_colonnes_en_local_qu_en_relais() {
        let local = MachineRow::default();
        let relais = MachineRow {
            relay_in: Some(RelayArrivals {
                down_packets: 3200,
                down_max_gap_ms: 16.0,
                down_gaps_over_10ms: 2,
                down_flows_with_gap: 2,
                up_packets: 400,
                up_max_gap_ms: 3.0,
                up_gaps_over_10ms: 0,
            }),
            ..Default::default()
        };
        let csv = machine_csv(&[local, relais]);
        let n: Vec<usize> = csv.lines().map(|l| l.split(',').count()).collect();
        assert!(n.iter().all(|&c| c == n[0]), "{csv}");
        assert!(csv.lines().nth(2).unwrap().ends_with(",3200,16.000,2,2,400,3.000,0"), "{csv}");
    }

    #[test]
    fn le_csv_garde_une_colonne_par_champ_et_laisse_vide_l_inconnu() {
        let mut r = row(1.0, 2, "m2", false, &[("underruns", 1.0)]);
        r.values[field("holesDecode")] = f64::NAN;
        let csv = peers_csv(&[r]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0].split(',').count(), lines[1].split(',').count());
        assert!(lines[1].contains(",1.000,"));
        assert!(lines[1].contains(",,"), "inconnu = vide, pas 0");
    }
}
