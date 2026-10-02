//! L'analyse d'une campagne, et ses mesures lisibles par une machine.
//!
//! La même fonction sert pendant la campagne (relevés en mémoire) et APRÈS,
//! sur des CSV archivés (`session-bench reanalyser`) : un échantillon garde sa
//! valeur quand l'analyse évolue (lot R5, demande du 01/10/2026 : « avoir les
//! échantillons plus tard à titre de comparaison »). `metrics.json` porte les
//! chiffres que la campagne de version compare à la référence.

use crate::profile::PeerProfile;
use crate::report::{
    self, Criterion, DriftCheck, Event, EventKind, EventSummary, MachineRow, MusicianSummary, PeerRow, RelayArrivals,
    StepSummary, Verdict, PEER_FIELDS,
};
use crate::scenario::Scenario;
use crate::server::UplinkWindow;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Tout ce que le résumé dit d'une campagne.
pub struct Analysis {
    pub summaries: Vec<StepSummary>,
    pub musicians: Vec<MusicianSummary>,
    pub events: Vec<EventSummary>,
    pub drift: Option<DriftCheck>,
    pub criteria: Vec<Criterion>,
    /// Écart d'horloge émetteur distant ↔ machine mesurée (0 sinon).
    pub clock_offset_ppm: f64,
    pub precision: Precision,
    pub precision_text: String,
}

/// Précision du banc sur la campagne.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Precision {
    Sufficient,
    Watch,
    Insufficient,
    Unmeasured,
}

/// Les événements d'un musicien arrivé à `arrival_s` (s de campagne).
pub fn musician_events(profile: &PeerProfile, m: u32, arrival_s: f64) -> Vec<Event> {
    let mut out: Vec<Event> = profile
        .absences
        .iter()
        .map(|a| Event { t_s: arrival_s + a.at_s, musician: m, kind: EventKind::Absence { for_s: a.for_s } })
        .collect();
    let mut from = profile.name.clone();
    for c in &profile.changes {
        out.push(Event {
            t_s: arrival_s + c.at_s,
            musician: m,
            kind: EventKind::Change { from: std::mem::replace(&mut from, c.link.name.clone()), to: c.link.name.clone() },
        });
    }
    out
}

/// Analyse une campagne. `steps` : (musiciens, début, fin) de chaque palier,
/// installation COMPRISE (elle est retirée ici).
pub fn analyze(
    scenario: &Scenario,
    peers: &[PeerRow],
    machine: &[MachineRow],
    steps: &[(u32, f64, f64)],
    events: &[Event],
) -> Analysis {
    let warmup = scenario.warmup_secs as f64;
    let measured: Vec<(u32, f64, f64)> = steps
        .iter()
        .filter(|(_, s, e)| e - s > warmup)
        .map(|&(n, s, e)| (n, s + warmup, e))
        .collect();
    let summaries: Vec<StepSummary> =
        measured.iter().map(|&(n, from, to)| report::summarize(peers, machine, n, from, to)).collect();
    let musicians = measured.last().map_or_else(Vec::new, |&(n, from, to)| report::musician_summaries(peers, n, from, to));
    let event_table = report::event_summaries(peers, events, &measured);
    let clock_offset_ppm = if scenario.remote.is_some() { report::clock_offset_ppm(&musicians) } else { 0.0 };
    let drift = scenario.is_drift_only().then(|| report::drift_check(peers, &measured, clock_offset_ppm));
    let mut criteria = report::criteria(&summaries, scenario.is_regular(), scenario.send_voice_channel.is_some());
    criteria.extend(report::network_criteria(&summaries, !scenario.is_regular(), &event_table, drift.as_ref()));
    let late: Vec<f64> = machine.iter().map(|r| r.sender_late_max_ms).collect();
    let (precision_text, _) = report::precision(&late);
    Analysis {
        summaries,
        musicians,
        events: event_table,
        drift,
        criteria,
        clock_offset_ppm,
        precision: precision_class(&late),
        precision_text,
    }
}

/// Même règle que `report::precision`, en classe.
pub fn precision_class(max_late_per_second_ms: &[f64]) -> Precision {
    let v: Vec<f64> = max_late_per_second_ms.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return Precision::Unmeasured;
    }
    let bad = v.iter().filter(|&&x| x > report::BENCH_LATE_OK_MS).count();
    if bad == 0 {
        Precision::Sufficient
    } else if bad * 100 <= v.len() {
        Precision::Watch
    } else {
        Precision::Insufficient
    }
}

/// Une valeur mesurée, ou `None` si elle est inconnue (`NaN` ne s'écrit pas en
/// JSON : on ne l'invente pas à zéro pour autant).
fn known(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepMetrics {
    pub musicians: u32,
    pub seconds: f64,
    pub holes_per_min: Option<f64>,
    /// Arrivée, réception, décodage, consommation, séquence, non classé, après « tampon tient ».
    pub causes_per_min: Vec<Option<f64>>,
    pub target_median_ms: Option<f64>,
    pub target_p95_ms: Option<f64>,
    pub cpu_median: Option<f64>,
    pub cpu_max: Option<f64>,
    pub late_callback_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamMetrics {
    pub stream: String,
    pub links: String,
    pub holes_per_min: Option<f64>,
    /// Arrivée, réception, décodage, consommation, séquence.
    pub causes_per_min: Vec<Option<f64>>,
    pub target_median_ms: Option<f64>,
    pub target_p95_ms: Option<f64>,
    pub late: Option<f64>,
    pub lost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriterionMetrics {
    pub id: u8,
    pub verdict: Verdict,
}

/// Les chiffres d'un scénario, ce que compare la campagne de version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Format du fichier (pour le relire dans un an).
    pub format: u32,
    pub scenario: String,
    /// Empreinte de ce qui décide des paquets envoyés (cf. `Scenario::fingerprint`).
    pub fingerprint: String,
    pub complete: bool,
    pub precision: Precision,
    pub steps: Vec<StepMetrics>,
    pub streams: Vec<StreamMetrics>,
    pub criteria: Vec<CriterionMetrics>,
}

pub const METRICS_FORMAT: u32 = 1;

impl Metrics {
    pub fn from_analysis(scenario: &Scenario, a: &Analysis, complete: bool) -> Self {
        Self {
            format: METRICS_FORMAT,
            scenario: scenario.name.clone(),
            fingerprint: scenario.fingerprint(),
            complete,
            precision: a.precision,
            steps: a
                .summaries
                .iter()
                .map(|s| StepMetrics {
                    musicians: s.musicians,
                    seconds: s.seconds,
                    holes_per_min: known(s.underruns_per_min),
                    causes_per_min: s.holes_per_min.iter().map(|&v| known(v)).collect(),
                    target_median_ms: known(s.target_median_ms),
                    target_p95_ms: known(s.target_p95_ms),
                    cpu_median: known(s.cpu_median),
                    cpu_max: known(s.cpu_max),
                    late_callback_seconds: s.late_callback_seconds,
                })
                .collect(),
            streams: a
                .musicians
                .iter()
                .map(|m| StreamMetrics {
                    stream: m.stream.clone(),
                    links: m.links.clone(),
                    holes_per_min: known(m.holes_per_min),
                    causes_per_min: m.causes_per_min.iter().map(|&v| known(v)).collect(),
                    target_median_ms: known(m.target_median_ms),
                    target_p95_ms: known(m.target_p95_ms),
                    late: known(m.late),
                    lost: known(m.lost),
                })
                .collect(),
            criteria: a.criteria.iter().map(|c| CriterionMetrics { id: c.id, verdict: c.verdict }).collect(),
        }
    }
}

/// Relit `peers.csv` (colonnes repérées par leur NOM : un CSV plus ancien,
/// avec moins de colonnes, se relit ; une colonne absente reste inconnue).
pub fn read_peers_csv(text: &str) -> Result<Vec<PeerRow>, String> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().ok_or("peers.csv vide")?.split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name);
    let (t, n, stream, voice) = (
        col("t_s").ok_or("peers.csv : colonne t_s absente")?,
        col("musicians").ok_or("peers.csv : colonne musicians absente")?,
        col("stream").ok_or("peers.csv : colonne stream absente")?,
        col("voice").ok_or("peers.csv : colonne voice absente")?,
    );
    let (musician, link, sim_ppm) = (col("musician"), col("link"), col("sim_ppm"));
    let fields: Vec<Option<usize>> = PEER_FIELDS.iter().map(|f| col(f)).collect();
    let num = |v: Option<&&str>| v.and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::NAN);
    lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let c: Vec<&str> = l.split(',').collect();
            let name = c.get(stream).copied().unwrap_or_default().to_string();
            // Avant R1, pas de colonne « musician » : « m3-ethernet » → 3.
            let from_name = name.strip_prefix('m').and_then(|r| r.split('-').next()).and_then(|d| d.parse().ok());
            Ok(PeerRow {
                t_s: num(c.get(t)),
                musicians: c.get(n).and_then(|v| v.parse().ok()).ok_or(format!("peers.csv : ligne illisible « {l} »"))?,
                voice: c.get(voice) == Some(&"true"),
                musician: musician.and_then(|i| c.get(i)).and_then(|v| v.parse().ok()).or(from_name).unwrap_or(0),
                link: link.and_then(|i| c.get(i)).map_or_else(String::new, |v| v.to_string()),
                sim_ppm: sim_ppm.map_or(0.0, |i| num(c.get(i))),
                values: fields.iter().map(|f| f.map_or(f64::NAN, |i| num(c.get(i)))).collect(),
                stream: name,
            })
        })
        .collect()
}

/// Relit `machine.csv` (ce dont l'analyse a besoin).
pub fn read_machine_csv(text: &str) -> Result<Vec<MachineRow>, String> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().ok_or("machine.csv vide")?.split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name);
    let idx: HashMap<&str, Option<usize>> = [
        "t_s", "musicians", "cpu_pct", "callback_deficit_out", "output_block_frames", "sender_sent", "sender_errors",
        "sender_late_p99_ms", "sender_late_max_ms", "up_instr_packets", "up_instr_max_gap_ms", "up_instr_gaps_over_10ms",
        "up_instr_seq_missing", "up_voice_packets", "up_voice_max_gap_ms", "up_voice_gaps_over_10ms",
        "up_voice_seq_missing", "relay_delay_p99_ms", "relay_delay_max_ms", "relay_in_down_packets",
        "relay_in_down_max_gap_ms", "relay_in_down_gaps_over_10ms", "relay_in_down_flows_with_gap",
        "relay_in_up_packets", "relay_in_up_max_gap_ms", "relay_in_up_gaps_over_10ms",
    ]
    .into_iter()
    .map(|k| (k, col(k)))
    .collect();
    lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let c: Vec<&str> = l.split(',').collect();
            let raw = |k: &str| idx.get(k).copied().flatten().and_then(|i| c.get(i)).copied().filter(|v| !v.is_empty());
            let f = |k: &str| raw(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(f64::NAN);
            let u = |k: &str| raw(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let up = |p: &str| UplinkWindow {
                packets: u(&format!("{p}_packets")),
                max_gap_us: (f(&format!("{p}_max_gap_ms")) * 1000.0).max(0.0) as u64,
                gaps_over_10ms: u(&format!("{p}_gaps_over_10ms")),
                seq_missing: u(&format!("{p}_seq_missing")),
                undecryptable: 0,
            };
            Ok(MachineRow {
                t_s: f("t_s"),
                musicians: raw("musicians").and_then(|v| v.parse().ok()).ok_or(format!("machine.csv : ligne illisible « {l} »"))?,
                cpu_pct: f("cpu_pct"),
                callback_deficit_out: f("callback_deficit_out"),
                output_block_frames: f("output_block_frames"),
                sender_sent: u("sender_sent"),
                sender_errors: u("sender_errors"),
                sender_late_p99_ms: f("sender_late_p99_ms"),
                sender_late_max_ms: f("sender_late_max_ms"),
                up_instrument: up("up_instr"),
                up_voice: raw("up_voice_packets").map(|_| up("up_voice")),
                relay_delay_p99_ms: f("relay_delay_p99_ms"),
                relay_delay_max_ms: f("relay_delay_max_ms"),
                relay_in: raw("relay_in_down_packets").map(|_| RelayArrivals {
                    down_packets: u("relay_in_down_packets"),
                    down_max_gap_ms: f("relay_in_down_max_gap_ms"),
                    down_gaps_over_10ms: u("relay_in_down_gaps_over_10ms"),
                    down_flows_with_gap: u("relay_in_down_flows_with_gap") as u32,
                    up_packets: u("relay_in_up_packets"),
                    up_max_gap_ms: f("relay_in_up_max_gap_ms"),
                    up_gaps_over_10ms: u("relay_in_up_gaps_over_10ms"),
                }),
            })
        })
        .collect()
}

/// Paliers retrouvés dans les relevés de la machine : (musiciens, début, fin).
/// Le premier relevé d'un palier tombe 1 s après son début.
pub fn steps_from_rows(machine: &[MachineRow]) -> Vec<(u32, f64, f64)> {
    let mut steps: Vec<(u32, f64, f64)> = Vec::new();
    for r in machine {
        match steps.last_mut() {
            Some(s) if s.0 == r.musicians => s.2 = r.t_s,
            _ => steps.push((r.musicians, r.t_s - 1.0, r.t_s)),
        }
    }
    steps
}

/// Événements d'un scénario rejoué : chaque musicien arrive au début du palier
/// qui l'ajoute (tous au premier).
pub fn events_from_steps(scenario: &Scenario, steps: &[(u32, f64, f64)]) -> Vec<Event> {
    (2..=scenario.to_musicians)
        .filter_map(|m| {
            let step = steps.iter().find(|(n, _, _)| *n >= m.max(scenario.from_musicians))?;
            Some(musician_events(scenario.peer(m), m, step.1))
        })
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{MachineRow, PeerRow};

    fn rows() -> (Vec<PeerRow>, Vec<MachineRow>) {
        let mut peers = Vec::new();
        let mut machine = Vec::new();
        for t in 1..=90 {
            let t = f64::from(t);
            for m in 2..=3u32 {
                let mut values = vec![0.0; PEER_FIELDS.len()];
                values[PEER_FIELDS.iter().position(|f| *f == "bufferTargetMs").unwrap()] = 5.0;
                values[PEER_FIELDS.iter().position(|f| *f == "underruns").unwrap()] = if t > 60.0 { 1.0 } else { 0.0 };
                values[PEER_FIELDS.iter().position(|f| *f == "holesArrival").unwrap()] = f64::NAN;
                peers.push(PeerRow {
                    t_s: t,
                    musicians: 3,
                    stream: format!("m{m}-ethernet"),
                    voice: false,
                    musician: m,
                    link: "ethernet".into(),
                    sim_ppm: -20.0,
                    values,
                });
            }
            machine.push(MachineRow { t_s: t, musicians: 3, cpu_pct: 12.5, sender_late_max_ms: 0.3, output_block_frames: 64.0, ..Default::default() });
        }
        (peers, machine)
    }

    /// Une campagne écrite en CSV puis relue donne la MÊME analyse : un
    /// échantillon archivé se réanalyse sans perte.
    #[test]
    fn une_campagne_relue_depuis_ses_csv_donne_la_meme_analyse() {
        let (peers, machine) = rows();
        let s = Scenario { from_musicians: 3, to_musicians: 3, step_secs: 90, peers: vec![PeerProfile::preset("ethernet").unwrap()], ..Scenario::default() };
        let steps = steps_from_rows(&machine);
        assert_eq!(steps, vec![(3, 0.0, 90.0)]);
        let direct = Metrics::from_analysis(&s, &analyze(&s, &peers, &machine, &steps, &[]), true);
        let peers2 = read_peers_csv(&report::peers_csv(&peers)).unwrap();
        let machine2 = read_machine_csv(&report::machine_csv(&machine)).unwrap();
        let again = Metrics::from_analysis(&s, &analyze(&s, &peers2, &machine2, &steps_from_rows(&machine2), &[]), true);
        assert_eq!(direct, again);
        assert_eq!(direct.streams.len(), 2);
        assert_eq!(direct.streams[0].target_median_ms, Some(5.0));
        assert_eq!(direct.streams[0].causes_per_min[0], None, "inconnu reste inconnu, pas 0");
        let json = serde_json::to_string(&direct).unwrap();
        assert_eq!(serde_json::from_str::<Metrics>(&json).unwrap(), direct);
        assert_eq!(direct.precision, Precision::Sufficient);
    }

    /// Un CSV d'avant R1 (moins de colonnes, pas de « musician ») se relit.
    #[test]
    fn un_csv_d_avant_r1_se_relit() {
        let csv = "t_s,musicians,stream,voice,underruns,bufferTargetMs\n31.0,3,m3-wifi,false,2.000,11.000\n";
        let r = &read_peers_csv(csv).unwrap()[0];
        assert_eq!((r.musician, r.get("underruns"), r.get("bufferTargetMs")), (3, 2.0, 11.0));
        assert!(r.get("driftPpm").is_nan());
    }

    #[test]
    fn la_precision_se_classe_comme_le_resume_la_dit() {
        assert_eq!(precision_class(&[0.2, 0.5]), Precision::Sufficient);
        let mut v = vec![0.3; 199];
        v.push(3.0);
        assert_eq!(precision_class(&v), Precision::Watch);
        assert_eq!(precision_class(&[0.3, 2.0]), Precision::Insufficient);
        assert_eq!(precision_class(&[]), Precision::Unmeasured);
    }

    /// Les événements d'un scénario rejoué tombent à l'arrivée du musicien + l'instant prévu.
    #[test]
    fn les_evenements_se_retrouvent_depuis_le_scenario() {
        let s = crate::library::named("un-wifi-charge-parmi-8").unwrap();
        let ev = events_from_steps(&s, &[(9, 0.4, 600.0)]);
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].musician, ev[0].t_s), (9, 300.4));
    }
}
