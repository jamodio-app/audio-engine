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
    "targetReactiveMs",
    "fillMinMs",
    "fillP50Ms",
    "jitterTailMs",
    "concealedUnderrunFrames",
    "concealedPrematureFrames",
    "packetsLost",
    "packetsLate",
    "zeroFilledMs",
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
    pub values: Vec<f64>,
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
    /// Instruments que le banc envoie à l'agent (les « musiciens simulés »).
    pub down_max_gap_ms: f64,
    pub down_gaps_over_10ms: u64,
    /// Combien de ces flux ont eu au moins une coupure dans la seconde : un
    /// blocage commun les touche tous à la fois.
    pub down_flows_with_gap: u32,
    /// Instrument que l'agent envoie.
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
                a.down_max_gap_ms = a.down_max_gap_ms.max(ms(p.from_bench.max_gap_us));
                a.down_gaps_over_10ms += p.from_bench.gaps_over_10ms;
                if p.from_bench.gaps_over_10ms > 0 {
                    a.down_flows_with_gap += 1;
                }
            }
            Some(Transport::UpInstrument) => {
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

/// Lignes « flux » d'un message `perf-stats`. `names` : identifiant du flux →
/// (nom lisible, voix ?). Un flux inconnu du banc est ignoré.
pub fn peer_rows(perf: &Value, t_s: f64, musicians: u32, names: &HashMap<String, (String, bool)>) -> Vec<PeerRow> {
    let Some(peers) = perf["peers"].as_array() else { return Vec::new() };
    peers
        .iter()
        .filter_map(|p| {
            let (name, voice) = names.get(p["producerId"].as_str()?)?;
            Some(PeerRow {
                t_s,
                musicians,
                stream: name.clone(),
                voice: *voice,
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
    let mut s = String::from("t_s,musicians,stream,voice");
    for f in PEER_FIELDS {
        s.push(',');
        s.push_str(f);
    }
    s.push('\n');
    for r in rows {
        let _ = write!(s, "{:.1},{},{},{}", r.t_s, r.musicians, r.stream, r.voice);
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
relay_delay_p99_ms,relay_delay_max_ms,relay_in_down_max_gap_ms,relay_in_down_gaps_over_10ms,\
relay_in_down_flows_with_gap,relay_in_up_max_gap_ms,relay_in_up_gaps_over_10ms\n",
    );
    let relay_in = |a: Option<&RelayArrivals>| match a {
        Some(a) => format!(
            "{},{},{},{},{}",
            fmt(a.down_max_gap_ms),
            a.down_gaps_over_10ms,
            a.down_flows_with_gap,
            fmt(a.up_max_gap_ms),
            a.up_gaps_over_10ms
        ),
        None => ",,,,".into(),
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
    pub relay_in_gaps: Option<(u64, u64)>,
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

/// Accroissement d'un compteur cumulé sur la fenêtre. Un compteur qui recule a
/// été remis à zéro (flux recréé) : on compte depuis la remise à zéro.
fn delta(first: f64, last: f64) -> f64 {
    if !first.is_finite() || !last.is_finite() {
        return f64::NAN;
    }
    if last >= first {
        last - first
    } else {
        last
    }
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
    // Compteurs : premier et dernier relevé de chaque flux instrument.
    let mut per_stream: HashMap<&str, (&PeerRow, &PeerRow)> = HashMap::new();
    let mut targets = Vec::new();
    let mut parts: [Vec<f64>; 3] = Default::default();
    for r in peers.iter().filter(|r| !r.voice && r.musicians == musicians && in_window(r.t_s)) {
        per_stream
            .entry(r.stream.as_str())
            .and_modify(|e| e.1 = r)
            .or_insert((r, r));
        targets.push(r.get("bufferTargetMs"));
        for (i, f) in ["targetJitterMs", "targetGlitchMs", "targetReactiveMs"].iter().enumerate() {
            parts[i].push(r.get(f));
        }
    }
    let sum_delta = |name: &str| -> f64 {
        per_stream.values().map(|(a, b)| delta(a.get(name), b.get(name))).sum::<f64>() / minutes
    };
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
    let relay_in_gaps = m
        .iter()
        .filter_map(|r| r.relay_in.map(|a| (a.down_gaps_over_10ms, a.up_gaps_over_10ms)))
        .reduce(|x, y| (x.0 + y.0, x.1 + y.1));
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Holds,
    Fails,
    /// Le scénario ne permet pas d'en juger (dit pourquoi dans le détail).
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct Criterion {
    pub id: u8,
    pub text: &'static str,
    pub verdict: Verdict,
    pub detail: String,
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
    let local: f64 = steps
        .iter()
        .map(|s| (s.holes_per_min[1] + s.holes_per_min[2] + s.holes_per_min[3]) * s.seconds / 60.0)
        .sum();
    out.push(Criterion {
        id: 1,
        text: "zéro trou de cause locale (réception, décodage, consommation), flux réguliers",
        verdict: if !regular || !local.is_finite() {
            Verdict::NotApplicable
        } else if local == 0.0 {
            Verdict::Holds
        } else {
            Verdict::Fails
        },
        detail: if !regular {
            "scénario avec gigue ou pertes simulées : critère réservé aux flux réguliers".into()
        } else if !local.is_finite() {
            "l'Audio Engine ne mesure pas toutes les causes locales (réception : 0.6.6-6 et plus)".into()
        } else {
            format!("{local:.0} trou(s) réception + décodage + consommation sur la campagne")
        },
    });

    // 2. La cible ne croît pas avec N (+1 ms au plus par rapport au 1er palier).
    let base = steps.first().map(|s| s.target_median_ms).unwrap_or(f64::NAN);
    let worst = steps
        .iter()
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
        verdict: if late == 0 { Verdict::Holds } else { Verdict::Fails },
        detail: format!("{late} seconde(s) en retard"),
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

/// Le résumé lisible : en-tête (quoi, où, quelle version), tableau par palier,
/// critères.
pub fn markdown(header: &[(String, String)], steps: &[StepSummary], criteria: &[Criterion]) -> String {
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
            st.relay_in_gaps.map_or("—".into(), |(d, u)| format!("{d} / {u}")),
        );
    }
    s.push_str("\n## Critères (validés le 28/09/2026)\n\n");
    for c in criteria {
        let _ = writeln!(s, "{}. **{}** — {} ({})", c.id, c.verdict.label(), c.text, c.detail);
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
        PeerRow { t_s: t, musicians: n, stream: stream.into(), voice, values }
    }

    #[test]
    fn les_champs_des_flux_sont_lus_et_un_champ_absent_reste_inconnu() {
        let perf = json!({ "peers": [
            { "producerId": "bench-p2", "underruns": 3, "bufferTargetMs": 11, "holesArrival": 2 },
            { "producerId": "inconnu", "underruns": 9 },
        ]});
        let names = HashMap::from([("bench-p2".to_string(), ("m2".to_string(), false))]);
        let rows = peer_rows(&perf, 1.0, 2, &names);
        assert_eq!(rows.len(), 1, "un flux inconnu du banc est ignoré");
        assert_eq!(rows[0].get("underruns"), 3.0);
        assert_eq!(rows[0].get("holesArrival"), 2.0);
        assert!(rows[0].get("holesDecode").is_nan(), "absent ≠ zéro");
    }

    #[test]
    fn un_compteur_remis_a_zero_compte_depuis_la_remise() {
        assert_eq!(delta(5.0, 8.0), 3.0);
        assert_eq!(delta(15.0, 4.0), 4.0);
        assert!(delta(f64::NAN, 4.0).is_nan());
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

    /// Avec de la gigue simulée, un trou n'est pas forcément local : le critère
    /// 1 ne juge pas. Sans mesure de cause (vieil agent), non plus.
    #[test]
    fn le_critere_1_ne_juge_que_ce_qu_il_peut_prouver() {
        let c = criteria(&[step(2, 5.0, 1.0)], false, false);
        assert_eq!(c[0].verdict, Verdict::NotApplicable);
        let c = criteria(&[step(2, 5.0, f64::NAN)], true, false);
        assert_eq!(c[0].verdict, Verdict::NotApplicable);
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
                down_max_gap_ms: 17.5,
                down_gaps_over_10ms: 3,
                down_flows_with_gap: 2,
                up_max_gap_ms: 12.0,
                up_gaps_over_10ms: 1
            }
        );
    }

    #[test]
    fn le_resume_ne_parle_du_relais_qu_en_mode_relais() {
        let local = markdown(&[], &[step(2, 5.0, 0.0)], &[]);
        assert!(local.contains("| — |") && !local.contains("« Coupures à l'arrivée au relais » :"));
        let mut st = step(2, 5.0, 0.0);
        st.relay_in_gaps = Some((12, 3));
        let relais = markdown(&[], &[st], &[]);
        assert!(relais.contains("| 12 / 3 |") && relais.contains("naissent À L'ALLER"), "{relais}");
    }

    #[test]
    fn le_csv_machine_a_autant_de_colonnes_en_local_qu_en_relais() {
        let local = MachineRow::default();
        let relais = MachineRow {
            relay_in: Some(RelayArrivals { down_max_gap_ms: 16.0, down_gaps_over_10ms: 2, down_flows_with_gap: 2, up_max_gap_ms: 3.0, up_gaps_over_10ms: 0 }),
            ..Default::default()
        };
        let csv = machine_csv(&[local, relais]);
        let n: Vec<usize> = csv.lines().map(|l| l.split(',').count()).collect();
        assert!(n.iter().all(|&c| c == n[0]), "{csv}");
        assert!(csv.lines().nth(2).unwrap().ends_with(",16.000,2,2,3.000,0"), "{csv}");
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
