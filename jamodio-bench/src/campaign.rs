//! La campagne de version (lot R5, PLAN-BANC-REALISTE-2026-10).
//!
//! Une commande, lancée par Ben à chaque pré-version qui touche le chemin audio
//! (cf. `session-bench conseil`) ou version publique : elle enchaîne les
//! scénarios, compare chaque flux à la RÉFÉRENCE de la machine (la dernière
//! version validée) dans les tolérances mesurées (bruit du banc), et écrit un
//! verdict en tête d'une page qui s'ouvre toute seule. Chaque campagne est
//! rangée pour être comparée plus tard :
//!
//! ```text
//! bench-results/
//!   machine.json        la configuration de cette machine (session-bench configurer)
//!   index.csv           une ligne par campagne
//!   references.json     la référence de chaque machine, et l'historique
//!   tolerances/<machine>.json
//!   campagnes/<machine>/<date>-<version>-<type>/
//!       campagne.json   qui, quoi, où ; verdict.md, verdict.html
//!       <scénario>/     resume.md, metrics.json, peers.csv, machine.csv, scenario.json
//! ```

use crate::analysis::{self, Metrics, Precision};
use crate::driver::AgentLink;
use crate::report::Verdict;
use crate::scenario::Scenario;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Série rapide (~30 min, pré-version) : la référence régulière, les pics, un
/// mauvais réseau parmi de bons, les réseaux mêlés.
pub const RAPIDE: [&str; 4] = ["regulier-9", "pics-seuls", "un-wifi-charge-parmi-8", "9-reseaux-mixtes"];

/// Passages de la mesure du bruit (même série, même version, entrelacés).
pub const NOISE_RUNS: u32 = 3;

/// Format des fichiers de campagne (pour les relire dans un an).
pub const CAMPAIGN_FORMAT: u32 = 1;

// ─── Configuration de la machine ─────────────────────────────────────────────

/// Ce qui est propre à une machine, saisi une fois (`session-bench configurer`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    /// Nom de la machine dans les résultats (ex. « NUC », « MacBook »).
    pub machine: String,
    /// Périphériques, au format strict `{idx}:{name}`.
    pub input_device: String,
    pub output_device: String,
    #[serde(default)]
    pub channel_index: Option<u8>,
    /// Émetteur distant `IP:PORT` (recommandé sur PC), ou flux fabriqués ici.
    #[serde(default)]
    pub remote: Option<String>,
    /// Plugin inséré pendant les campagnes (nom exact), si la machine en a un.
    #[serde(default)]
    pub plugin: Option<String>,
}

impl MachineConfig {
    pub fn path(base: &Path) -> PathBuf {
        base.join("machine.json")
    }

    pub fn load(base: &Path) -> Result<Self, String> {
        let path = Self::path(base);
        let text = std::fs::read_to_string(&path)
            .map_err(|_| format!("{} absent : lancer d'abord « session-bench configurer »", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{} : {e}", path.display()))
    }

    pub fn save(&self, base: &Path) -> Result<(), String> {
        std::fs::create_dir_all(base).map_err(|e| e.to_string())?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(Self::path(base), json).map_err(|e| e.to_string())
    }

    /// Le scénario, réglé pour cette machine.
    pub fn apply(&self, mut s: Scenario) -> Scenario {
        s.input_device = Some(self.input_device.clone());
        s.output_device = Some(self.output_device.clone());
        s.channel_index = self.channel_index;
        s.remote = self.remote.clone();
        s.plugin = self.plugin.clone();
        s
    }
}

// ─── Campagne ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CampaignKind {
    /// ~30 min : pré-version qui touche le chemin audio.
    Rapide,
    /// ~1 h 30 : les 8 scénarios, avant une version publique.
    Complete,
    /// La série rapide 3 fois : l'écart naturel entre passages → tolérances.
    Bruit,
    /// Campagne importée d'avant la commande (ex. R1 du 01/10/2026).
    Importee,
}

impl CampaignKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rapide => "rapide",
            Self::Complete => "complete",
            Self::Bruit => "bruit",
            Self::Importee => "importee",
        }
    }

    /// (sous-dossier, scénario) dans l'ordre de passage. Le bruit entrelace
    /// ses passages (1, 2, 3 de chaque scénario, à des heures différentes) :
    /// l'écart mesuré comprend la dérive de la machine dans le temps.
    pub fn entries(self) -> Vec<(String, String)> {
        match self {
            Self::Rapide => RAPIDE.iter().map(|n| (n.to_string(), n.to_string())).collect(),
            Self::Complete => crate::library::NAMED.iter().map(|(n, _)| (n.to_string(), n.to_string())).collect(),
            Self::Bruit => (1..=NOISE_RUNS)
                .flat_map(|i| RAPIDE.iter().map(move |n| (format!("{i}-{n}"), n.to_string())))
                .collect(),
            Self::Importee => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioEntry {
    /// Sous-dossier dans la campagne.
    pub dir: String,
    pub scenario: String,
    pub fingerprint: String,
    /// « complète », ou ce qui l'a arrêtée.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CampaignInfo {
    pub format: u32,
    pub machine: String,
    pub kind: CampaignKind,
    pub started_utc: String,
    pub audio_engine: String,
    pub bench_commit: String,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub remote: Option<String>,
    pub plugin: Option<String>,
    pub scenarios: Vec<ScenarioEntry>,
    /// Origine d'une campagne importée, ou remarque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl CampaignInfo {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let p = dir.join("campagne.json");
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{} : {e}", p.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{} : {e}", p.display()))
    }

    fn save(&self, dir: &Path) -> Result<(), String> {
        write(dir, "campagne.json", &serde_json::to_string_pretty(self).map_err(|e| e.to_string())?)
    }

    /// Les mesures d'un scénario de la campagne (le premier passage de ce nom).
    pub fn metrics(&self, dir: &Path, scenario: &str) -> Option<Metrics> {
        let e = self.scenarios.iter().find(|e| e.scenario == scenario)?;
        read_metrics(&dir.join(&e.dir)).ok()
    }
}

pub fn read_metrics(dir: &Path) -> Result<Metrics, String> {
    let p = dir.join("metrics.json");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{} : {e}", p.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} : {e}", p.display()))
}

/// Lance la campagne de version `kind` sur cette machine.
pub async fn version(base: &Path, kind: CampaignKind) -> Result<PathBuf, String> {
    let cfg = MachineConfig::load(base)?;
    // Une première connexion : l'Audio Engine répond, et sa version nomme le dossier.
    let (audio_engine, os) = {
        let agent = AgentLink::connect(&Scenario::default().agent_url).await?;
        let v = agent.hello["agentVersion"].as_str().unwrap_or("?").to_string();
        let os = format!("{}/{}", agent.hello["os"].as_str().unwrap_or("?"), agent.hello["arch"].as_str().unwrap_or("?"));
        let _ = agent.stop();
        (v, os)
    };
    let (stamp, started_utc) = utc_now();
    let dir = base.join("campagnes").join(&cfg.machine).join(format!("{stamp}-{audio_engine}-{}", kind.label()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} : {e}", dir.display()))?;
    let entries = kind.entries();
    println!(
        "Campagne {} — {} — Audio Engine {audio_engine} ({os}) — {} scénarios → {}",
        kind.label(),
        cfg.machine,
        entries.len(),
        dir.display()
    );
    let mut info = CampaignInfo {
        format: CAMPAIGN_FORMAT,
        machine: cfg.machine.clone(),
        kind,
        started_utc,
        audio_engine: audio_engine.clone(),
        bench_commit: bench_commit(),
        input_device: Some(cfg.input_device.clone()),
        output_device: Some(cfg.output_device.clone()),
        remote: cfg.remote.clone(),
        plugin: cfg.plugin.clone(),
        scenarios: Vec::new(),
        note: None,
    };
    for (k, (sub, name)) in entries.iter().enumerate() {
        println!("\n═══ {}/{} : {sub}", k + 1, entries.len());
        let scenario = cfg.apply(crate::library::named(name).ok_or(format!("scénario {name} absent de la bibliothèque"))?);
        let fingerprint = scenario.fingerprint();
        let out = dir.join(sub);
        let status = match crate::run::run(scenario, &out).await {
            Ok(_) if read_metrics(&out).is_ok_and(|m| m.complete) => "complète".to_string(),
            Ok(_) => "interrompue".to_string(),
            Err(e) => e,
        };
        let complete = status == "complète";
        info.scenarios.push(ScenarioEntry { dir: sub.clone(), scenario: name.clone(), fingerprint, status });
        info.save(&dir)?;
        if !complete {
            // Une campagne incomplète ne juge rien : on s'arrête, et on le dit.
            println!("⚠ Scénario {sub} non complet : la campagne s'arrête ici.");
            break;
        }
    }
    let page = finish(base, &dir, &info)?;
    open_page(&page);
    Ok(page)
}

/// Verdict, pages et index d'une campagne écrite dans `dir`.
pub fn finish(base: &Path, dir: &Path, info: &CampaignInfo) -> Result<PathBuf, String> {
    let outcome = write_verdict(base, dir, info)?;
    append_index(base, info, outcome, dir)?;
    Ok(dir.join("verdict.html"))
}

/// `session-bench verdict [DOSSIER]` : recalcule le verdict d'une campagne
/// (la dernière de cette machine par défaut) avec la référence et les
/// tolérances ACTUELLES — après une nouvelle référence, une mesure du bruit,
/// ou un renommage. L'index n'est pas touché.
pub fn recompute_verdict(base: &Path, dir: Option<&str>) -> Result<PathBuf, String> {
    let rel = match dir {
        Some(d) => d.trim_end_matches(['/', '\\']).replace('\\', "/"),
        None => {
            let machine = MachineConfig::load(base)?.machine;
            latest_campaign(base, &machine).ok_or(format!("aucune campagne pour {machine} dans index.csv"))?
        }
    };
    let dir = base.join(&rel);
    let info = CampaignInfo::load(&dir)?;
    write_verdict(base, &dir, &info)?;
    Ok(dir.join("verdict.html"))
}

/// Écrit verdict.md et verdict.html (et les tolérances pour une campagne de
/// bruit) ; rend le verdict.
fn write_verdict(base: &Path, dir: &Path, info: &CampaignInfo) -> Result<Outcome, String> {
    let (md, html, outcome) = if info.kind == CampaignKind::Bruit {
        let tol = noise_tolerances(dir, info)?;
        let path = Tolerances::path(base, &info.machine);
        std::fs::create_dir_all(path.parent().expect("dossier")).map_err(|e| e.to_string())?;
        std::fs::write(&path, serde_json::to_string_pretty(&tol).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let (md, html) = render_noise(info, &tol);
        (md, html, Outcome::Measured)
    } else {
        let reference = References::load(base)?.current(&info.machine).map(|p| base.join(p));
        let reference = match reference {
            Some(r) if r != dir => Some((CampaignInfo::load(&r)?, r)),
            _ => None,
        };
        let tolerances = Tolerances::load(base, &info.machine);
        let verdict = compare_campaign(dir, info, reference.as_ref().map(|(i, p)| (i, p.as_path())), tolerances.as_ref());
        let (md, html) = render_verdict(info, &verdict);
        (md, html, verdict.outcome)
    };
    write(dir, "verdict.md", &md)?;
    write(dir, "verdict.html", &html)?;
    println!("\n{md}");
    Ok(outcome)
}

// ─── Comparaison et tolérances ───────────────────────────────────────────────

/// Écart toléré pour un flux d'un scénario (au-delà : régression).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tol {
    pub target_ms: f64,
    pub holes_per_min: f64,
}

/// Planchers : la cible est publiée en ms ENTIÈRES par l'agent (±1 ms
/// d'arrondi) ; un demi-trou par minute sur 5 min ne se distingue pas du hasard.
pub const TOL_FLOOR: Tol = Tol { target_ms: 1.0, holes_per_min: 0.5 };

/// Marge appliquée à l'écart observé entre passages (bruit) : deux fois le pire
/// écart vu sur trois passages. Choix d'ingénierie, à revoir après quelques
/// campagnes de version.
pub const NOISE_MARGIN: f64 = 2.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tolerances {
    /// Campagne de bruit d'où elles viennent.
    pub source: String,
    /// scénario → flux → tolérance.
    pub scenarios: BTreeMap<String, BTreeMap<String, Tol>>,
}

impl Tolerances {
    pub fn path(base: &Path, machine: &str) -> PathBuf {
        base.join("tolerances").join(format!("{machine}.json"))
    }

    pub fn load(base: &Path, machine: &str) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(base, machine)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn get(&self, scenario: &str, stream: &str) -> Option<Tol> {
        self.scenarios.get(scenario)?.get(stream).copied()
    }
}

/// Tolérances tirées de passages répétés : pour chaque flux, la marge fois le
/// plus grand écart observé (max − min), jamais sous le plancher.
pub fn tolerances_from_runs(runs: &BTreeMap<String, Vec<Metrics>>, source: &str) -> Tolerances {
    let mut scenarios = BTreeMap::new();
    for (scenario, metrics) in runs {
        let mut streams: BTreeMap<String, Tol> = BTreeMap::new();
        let names: Vec<String> = metrics.iter().flat_map(|m| m.streams.iter().map(|s| s.stream.clone())).collect();
        for name in names {
            let values = |f: fn(&analysis::StreamMetrics) -> Option<f64>| -> Vec<f64> {
                metrics.iter().filter_map(|m| m.streams.iter().find(|s| s.stream == name).and_then(f)).collect()
            };
            let spread = |v: Vec<f64>| v.iter().copied().fold(f64::MIN, f64::max) - v.iter().copied().fold(f64::MAX, f64::min);
            let target = values(|s| s.target_median_ms);
            let holes = values(|s| s.holes_per_min);
            let tol = |v: Vec<f64>, floor: f64| if v.len() < 2 { floor } else { (NOISE_MARGIN * spread(v)).max(floor) };
            streams.insert(
                name.clone(),
                Tol { target_ms: tol(target, TOL_FLOOR.target_ms), holes_per_min: tol(holes, TOL_FLOOR.holes_per_min) },
            );
        }
        scenarios.insert(scenario.clone(), streams);
    }
    Tolerances { source: source.to_string(), scenarios }
}

fn noise_tolerances(dir: &Path, info: &CampaignInfo) -> Result<Tolerances, String> {
    let mut runs: BTreeMap<String, Vec<Metrics>> = BTreeMap::new();
    for e in info.scenarios.iter().filter(|e| e.status == "complète") {
        runs.entry(e.scenario.clone()).or_default().push(read_metrics(&dir.join(&e.dir))?);
    }
    Ok(tolerances_from_runs(&runs, &dir.display().to_string()))
}

/// Verdict d'un scénario, ou de la campagne.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    NoRegression,
    Regression,
    /// Rien à juger, ou pas jugeable (pas de référence, scénario changé, banc
    /// imprécis, campagne incomplète) — dit pourquoi.
    Inconclusive,
    /// Campagne de bruit : elle mesure, elle ne juge pas.
    Measured,
}

impl Outcome {
    /// Symbole + mot : jamais une couleur seule.
    pub fn label(self) -> &'static str {
        match self {
            Self::NoRegression => "✔ AUCUNE RÉGRESSION",
            Self::Regression => "✖ RÉGRESSION",
            Self::Inconclusive => "○ NON CONCLUANT",
            Self::Measured => "◆ BRUIT MESURÉ",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioVerdict {
    pub scenario: String,
    pub outcome: Outcome,
    /// Pourquoi : régressions, améliorations, ou ce qui empêche de juger.
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CampaignVerdict {
    pub outcome: Outcome,
    /// Campagne de référence (dossier, version).
    pub reference: Option<(String, String)>,
    pub tolerances: String,
    pub scenarios: Vec<ScenarioVerdict>,
}

/// Compare un scénario à sa référence. `tol(flux)` : tolérance de ce flux.
pub fn compare_scenario(current: &Metrics, reference: Option<&Metrics>, tol: &dyn Fn(&str) -> Tol) -> ScenarioVerdict {
    let verdict = |outcome, lines| ScenarioVerdict { scenario: current.scenario.clone(), outcome, lines };
    let Some(r) = reference else {
        return verdict(Outcome::Inconclusive, vec!["aucune référence pour ce scénario sur cette machine".into()]);
    };
    if !current.complete {
        return verdict(Outcome::Inconclusive, vec!["scénario interrompu : rien à juger".into()]);
    }
    if current.fingerprint != r.fingerprint {
        return verdict(
            Outcome::Inconclusive,
            vec![format!(
                "scénario modifié depuis la référence (empreinte {} ≠ {}) : comparaison refusée — refaire une référence",
                current.fingerprint, r.fingerprint
            )],
        );
    }
    if current.precision == Precision::Insufficient || r.precision == Precision::Insufficient {
        return verdict(
            Outcome::Inconclusive,
            vec!["précision du banc insuffisante (cette campagne ou la référence) : les écarts peuvent venir du banc".into()],
        );
    }
    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    for s in &current.streams {
        let Some(rs) = r.streams.iter().find(|x| x.stream == s.stream) else {
            regressions.push(format!("{} : absent de la référence", s.stream));
            continue;
        };
        let t = tol(&s.stream);
        let mut check = |what: &str, unit: &str, cur: Option<f64>, refv: Option<f64>, limit: f64| match (cur, refv) {
            (Some(c), Some(rv)) if c - rv > limit => {
                regressions.push(format!("{} : {what} {rv:.1} → {c:.1} {unit} ({:+.1}, tolérance {limit:.1})", s.stream, c - rv))
            }
            (Some(c), Some(rv)) if rv - c > limit => {
                improvements.push(format!("{} : {what} {rv:.1} → {c:.1} {unit} ({:+.1})", s.stream, c - rv))
            }
            (None, Some(_)) => regressions.push(format!("{} : {what} non mesuré cette fois", s.stream)),
            _ => {}
        };
        check("cible médiane", "ms", s.target_median_ms, rs.target_median_ms, t.target_ms);
        check("trous", "/min", s.holes_per_min, rs.holes_per_min, t.holes_per_min);
    }
    for c in &current.criteria {
        if c.verdict == Verdict::Fails && r.criteria.iter().any(|x| x.id == c.id && x.verdict == Verdict::Holds) {
            regressions.push(format!("critère {} : tenu dans la référence, non tenu cette fois", c.id));
        }
    }
    let outcome = if regressions.is_empty() { Outcome::NoRegression } else { Outcome::Regression };
    let mut lines: Vec<String> = regressions.into_iter().map(|l| format!("✖ {l}")).collect();
    lines.extend(improvements.into_iter().map(|l| format!("▲ amélioration — {l}")));
    verdict(outcome, lines)
}

/// Compare chaque scénario de la campagne à la référence de la machine.
pub fn compare_campaign(
    dir: &Path,
    info: &CampaignInfo,
    reference: Option<(&CampaignInfo, &Path)>,
    tolerances: Option<&Tolerances>,
) -> CampaignVerdict {
    // Mêmes scénarios mais autre carte son, autre émetteur ou autre plugin :
    // ce ne sont plus les mêmes conditions (taille de bloc, chemin réseau,
    // charge). On ne compare pas, et on dit pourquoi.
    let setup_differs = reference.and_then(|(ri, _)| {
        let pairs = [
            ("entrée", &info.input_device, &ri.input_device),
            ("sortie", &info.output_device, &ri.output_device),
            ("émetteur distant", &info.remote, &ri.remote),
            ("plugin", &info.plugin, &ri.plugin),
        ];
        let diffs: Vec<String> = pairs
            .iter()
            .filter(|(_, a, b)| a != b)
            .map(|(k, a, b)| format!("{k} {} ≠ référence {}", a.as_deref().unwrap_or("aucun"), b.as_deref().unwrap_or("aucun")))
            .collect();
        (!diffs.is_empty()).then(|| format!("conditions différentes de la référence ({}) : comparaison refusée", diffs.join(" ; ")))
    });
    let mut scenarios: Vec<ScenarioVerdict> = info
        .scenarios
        .iter()
        .map(|e| match read_metrics(&dir.join(&e.dir)) {
            Err(err) => ScenarioVerdict {
                scenario: e.scenario.clone(),
                outcome: Outcome::Inconclusive,
                lines: vec![format!("{} — mesures illisibles : {err}", e.status)],
            },
            Ok(_) if setup_differs.is_some() => ScenarioVerdict {
                scenario: e.scenario.clone(),
                outcome: Outcome::Inconclusive,
                lines: vec![setup_differs.clone().unwrap_or_default()],
            },
            Ok(m) => {
                let r = reference.and_then(|(ri, rp)| ri.metrics(rp, &e.scenario));
                let tol = |stream: &str| tolerances.and_then(|t| t.get(&e.scenario, stream)).unwrap_or(TOL_FLOOR);
                compare_scenario(&m, r.as_ref(), &tol)
            }
        })
        .collect();
    // Les scénarios prévus qui n'ont pas tourné (campagne arrêtée) le disent.
    for (_, name) in info.kind.entries() {
        if !info.scenarios.iter().any(|e| e.scenario == name) && !scenarios.iter().any(|s| s.scenario == name) {
            scenarios.push(ScenarioVerdict {
                scenario: name,
                outcome: Outcome::Inconclusive,
                lines: vec!["non lancé : la campagne s'est arrêtée avant".into()],
            });
        }
    }
    let outcome = if scenarios.iter().any(|s| s.outcome == Outcome::Regression) {
        Outcome::Regression
    } else if scenarios.is_empty() || scenarios.iter().any(|s| s.outcome == Outcome::Inconclusive) {
        Outcome::Inconclusive
    } else {
        Outcome::NoRegression
    };
    CampaignVerdict {
        outcome,
        reference: reference.map(|(ri, rp)| (rp.display().to_string(), ri.audio_engine.clone())),
        tolerances: tolerances.map_or_else(
            || format!("par défaut (bruit non mesuré) : cible ±{} ms, trous ±{}/min", TOL_FLOOR.target_ms, TOL_FLOOR.holes_per_min),
            |t| format!("mesurées (bruit : {})", t.source),
        ),
        scenarios,
    }
}

// ─── Références et index ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineReferences {
    /// Campagne de référence actuelle (chemin relatif à `bench-results`).
    pub current: Option<String>,
    /// Toutes les références successives, jamais effacées.
    pub history: Vec<ReferenceEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceEntry {
    pub dir: String,
    pub audio_engine: String,
    pub since_utc: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct References {
    pub machines: BTreeMap<String, MachineReferences>,
}

impl References {
    fn path(base: &Path) -> PathBuf {
        base.join("references.json")
    }

    pub fn load(base: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(Self::path(base)) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| format!("references.json : {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("references.json : {e}")),
        }
    }

    pub fn current(&self, machine: &str) -> Option<&str> {
        self.machines.get(machine)?.current.as_deref()
    }

    /// `dir` (relatif à `bench-results`) devient la référence de sa machine.
    pub fn set(&mut self, machine: &str, dir: &str, audio_engine: &str, since_utc: &str) {
        let m = self.machines.entry(machine.to_string()).or_default();
        m.current = Some(dir.to_string());
        m.history.push(ReferenceEntry { dir: dir.into(), audio_engine: audio_engine.into(), since_utc: since_utc.into() });
    }

    pub fn save(&self, base: &Path) -> Result<(), String> {
        std::fs::write(Self::path(base), serde_json::to_string_pretty(self).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    }
}

/// `dir` relatif à `base`, avec des « / » (le même fichier sur Mac et PC).
pub fn relative(base: &Path, dir: &Path) -> String {
    dir.strip_prefix(base).unwrap_or(dir).components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

const INDEX_HEADER: &str = "date_utc,machine,audio_engine,type,verdict,dossier\n";

fn append_index(base: &Path, info: &CampaignInfo, outcome: Outcome, dir: &Path) -> Result<(), String> {
    use std::io::Write as _;
    let path = base.join("index.csv");
    let new = !path.exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("index.csv : {e}"))?;
    if new {
        f.write_all(INDEX_HEADER.as_bytes()).map_err(|e| e.to_string())?;
    }
    let verdict = serde_json::to_value(outcome).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    writeln!(f, "{},{},{},{},{},{}", info.started_utc, info.machine, info.audio_engine, info.kind.label(), verdict, relative(base, dir))
        .map_err(|e| e.to_string())
}

/// Dernière campagne jugeable (rapide ou complète) d'une machine, d'après l'index.
pub fn latest_campaign(base: &Path, machine: &str) -> Option<String> {
    let text = std::fs::read_to_string(base.join("index.csv")).ok()?;
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let c: Vec<&str> = l.split(',').collect();
            (c.len() == 6 && c[1] == machine && (c[3] == "rapide" || c[3] == "complete" || c[3] == "importee")).then(|| c[5].to_string())
        })
        .last()
}

// ─── Import d'une campagne déjà faite ────────────────────────────────────────

/// Range dans `campagnes/<machine>/` des dossiers de scénarios lancés à la main
/// (`session-bench run`) : mesures relues depuis leurs CSV (`metrics.json`),
/// campagne décrite (`campagne.json`), verdict, index. Les dossiers d'origine
/// sont COPIÉS, jamais déplacés ni effacés.
pub fn import(base: &Path, kind: CampaignKind, machine: &str, note: &str, sources: &[PathBuf]) -> Result<PathBuf, String> {
    if sources.is_empty() {
        return Err("aucun dossier à importer".into());
    }
    // Tout vérifier AVANT de créer quoi que ce soit : un import qui échoue en
    // route ne laisse pas une campagne à moitié copiée (vu le 01/10/2026).
    for src in sources {
        for f in ["scenario.json", "peers.csv", "machine.csv"] {
            if !src.join(f).is_file() {
                return Err(format!("{} : {f} absent — ce n'est pas un dossier de scénario du banc", src.display()));
            }
        }
        let text = std::fs::read_to_string(src.join("scenario.json")).map_err(|e| format!("{} : {e}", src.display()))?;
        serde_json::from_str::<Scenario>(&text).map_err(|e| format!("{}/scenario.json : {e}", src.display()))?;
    }
    let (audio_engine, input, output, remote) = header_facts(&sources[0].join("resume.md"));
    let (stamp, started_utc) = utc_now();
    let dir = base.join("campagnes").join(machine).join(format!("{stamp}-{audio_engine}-{}", kind.label()));
    if dir.exists() {
        return Err(format!("{} existe déjà", dir.display()));
    }
    let mut info = CampaignInfo {
        format: CAMPAIGN_FORMAT,
        machine: machine.to_string(),
        kind,
        started_utc,
        audio_engine,
        bench_commit: format!("importée (banc {})", bench_commit()),
        input_device: input,
        output_device: output,
        remote,
        plugin: None,
        scenarios: Vec::new(),
        note: Some(note.to_string()),
    };
    for src in sources {
        let text = std::fs::read_to_string(src.join("scenario.json")).map_err(|e| format!("{} : {e}", src.display()))?;
        let scenario: Scenario = serde_json::from_str(&text).map_err(|e| format!("{} : {e}", src.display()))?;
        let runs = info.scenarios.iter().filter(|e| e.scenario == scenario.name).count();
        let sub = if kind == CampaignKind::Bruit { format!("{}-{}", runs + 1, scenario.name) } else { scenario.name.clone() };
        let dst = dir.join(&sub);
        copy_dir(src, &dst)?;
        let metrics = reanalyze(&dst)?;
        info.scenarios.push(ScenarioEntry {
            dir: sub,
            scenario: scenario.name.clone(),
            fingerprint: scenario.fingerprint(),
            status: if metrics.complete { "complète".into() } else { "interrompue".into() },
        });
    }
    info.save(&dir)?;
    finish(base, &dir, &info)?;
    Ok(dir)
}

/// Relit les CSV d'un dossier de scénario et (ré)écrit son `metrics.json`.
/// Le `resume.md` d'origine n'est pas touché.
pub fn reanalyze(dir: &Path) -> Result<Metrics, String> {
    let read = |n: &str| std::fs::read_to_string(dir.join(n)).map_err(|e| format!("{} : {e}", dir.join(n).display()));
    let scenario: Scenario = serde_json::from_str(&read("scenario.json")?).map_err(|e| format!("scenario.json : {e}"))?;
    let peers = analysis::read_peers_csv(&read("peers.csv")?)?;
    let machine = analysis::read_machine_csv(&read("machine.csv")?)?;
    let steps = analysis::steps_from_rows(&machine);
    let events = analysis::events_from_steps(&scenario, &steps);
    let a = analysis::analyze(&scenario, &peers, &machine, &steps, &events);
    let complete = read("resume.md").map_or(true, |r| r.contains("**Campagne** : complète"));
    let metrics = Metrics::from_analysis(&scenario, &a, complete);
    write(dir, "metrics.json", &serde_json::to_string_pretty(&metrics).map_err(|e| e.to_string())?)?;
    Ok(metrics)
}

/// Version de l'Audio Engine, périphériques, émetteur : lus dans l'en-tête d'un résumé.
fn header_facts(resume: &Path) -> (String, Option<String>, Option<String>, Option<String>) {
    let text = std::fs::read_to_string(resume).unwrap_or_default();
    let field = |k: &str| {
        text.lines().find_map(|l| l.strip_prefix(&format!("- **{k}** : ")).map(str::to_string))
    };
    let version = field("Audio Engine").and_then(|v| v.split_whitespace().next().map(str::to_string)).unwrap_or_else(|| "?".into());
    let (input, output) = match field("Entrée / sortie").and_then(|v| v.split_once(" / ").map(|(a, b)| (a.to_string(), b.to_string()))) {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    };
    let remote = field("Émetteur distant")
        .filter(|v| v != "aucun")
        .and_then(|v| v.split('(').nth(1).and_then(|r| r.split(')').next()).map(str::to_string));
    (version, input, output, remote)
}

// ─── Pages ───────────────────────────────────────────────────────────────────

fn render_verdict(info: &CampaignInfo, v: &CampaignVerdict) -> (String, String) {
    let mut md = format!("# {} — campagne {} ({})\n\n", v.outcome.label(), info.kind.label(), info.machine);
    let facts = facts(info);
    for (k, val) in &facts {
        let _ = writeln!(md, "- **{k}** : {val}");
    }
    let reference = v.reference.as_ref().map_or("aucune — « session-bench reference » quand une version est validée".into(), |(d, ver)| format!("{ver} ({d})"));
    let _ = writeln!(md, "- **Référence** : {reference}");
    let _ = writeln!(md, "- **Tolérances** : {}", v.tolerances);
    md.push_str("\n## Par scénario\n\n");
    for s in &v.scenarios {
        let _ = writeln!(md, "### {} — {}\n", s.outcome.label(), s.scenario);
        if s.lines.is_empty() {
            md.push_str("Chaque flux dans la tolérance de la référence.\n\n");
        }
        for l in &s.lines {
            let _ = writeln!(md, "- {l}");
        }
        md.push('\n');
    }
    md.push_str(LEGEND);

    let mut body = format!(
        "<header class=\"verdict {}\"><p class=\"badge\">{}</p><h1>Campagne {} — {}</h1></header>\n<dl>",
        css_class(v.outcome),
        esc(v.outcome.label()),
        esc(info.kind.label()),
        esc(&info.machine)
    );
    for (k, val) in facts.iter().chain([("Référence".to_string(), reference), ("Tolérances".to_string(), v.tolerances.clone())].iter()) {
        let _ = write!(body, "<dt>{}</dt><dd>{}</dd>", esc(k), esc(val));
    }
    body.push_str("</dl>\n");
    for s in &v.scenarios {
        let _ = write!(body, "<section class=\"{}\"><h2><span class=\"badge\">{}</span> {}</h2><ul>", css_class(s.outcome), esc(s.outcome.label()), esc(&s.scenario));
        if s.lines.is_empty() {
            body.push_str("<li>Chaque flux dans la tolérance de la référence.</li>");
        }
        for l in &s.lines {
            let _ = write!(body, "<li>{}</li>", esc(l));
        }
        body.push_str("</ul></section>\n");
    }
    let _ = write!(body, "<p class=\"legend\">{}</p>", esc(LEGEND.trim()));
    (md, page(&format!("Verdict — {}", info.machine), &body))
}

fn render_noise(info: &CampaignInfo, t: &Tolerances) -> (String, String) {
    let mut md = format!("# {} — {} ({} passages par scénario)\n\n", Outcome::Measured.label(), info.machine, NOISE_RUNS);
    for (k, val) in facts(info) {
        let _ = writeln!(md, "- **{k}** : {val}");
    }
    let _ = writeln!(
        md,
        "\nTolérance d'un flux = {NOISE_MARGIN} × le plus grand écart vu entre passages, jamais sous {} ms (cible) ni {} trou/min.\n",
        TOL_FLOOR.target_ms, TOL_FLOOR.holes_per_min
    );
    let mut body = format!(
        "<header class=\"verdict measured\"><p class=\"badge\">{}</p><h1>Bruit du banc — {}</h1></header><p>{}</p>",
        esc(Outcome::Measured.label()),
        esc(&info.machine),
        esc(&format!("Tolérance d'un flux = {NOISE_MARGIN} × le plus grand écart vu entre {NOISE_RUNS} passages, jamais sous les planchers."))
    );
    for (scenario, streams) in &t.scenarios {
        let _ = writeln!(md, "## {scenario}\n\n| Flux | Tolérance cible (ms) | Tolérance trous (/min) |\n|---|---|---|");
        let _ = write!(body, "<h2>{}</h2><table><tr><th>Flux</th><th>Cible (ms)</th><th>Trous (/min)</th></tr>", esc(scenario));
        for (stream, tol) in streams {
            let _ = writeln!(md, "| {stream} | {:.1} | {:.1} |", tol.target_ms, tol.holes_per_min);
            let _ = write!(body, "<tr><td>{}</td><td>{:.1}</td><td>{:.1}</td></tr>", esc(stream), tol.target_ms, tol.holes_per_min);
        }
        md.push('\n');
        body.push_str("</table>");
    }
    (md, page(&format!("Bruit du banc — {}", info.machine), &body))
}

const LEGEND: &str = "Lecture : ✔ aucune régression · ✖ régression (au-delà de la tolérance) · ○ non concluant (dit pourquoi) · ▲ amélioration. Chaque flux est comparé à la référence de CETTE machine : cible médiane du tampon et trous par minute.\n";

fn facts(info: &CampaignInfo) -> Vec<(String, String)> {
    let mut v = vec![
        ("Audio Engine".to_string(), info.audio_engine.clone()),
        ("Machine".to_string(), info.machine.clone()),
        ("Début (UTC)".to_string(), info.started_utc.clone()),
        (
            "Entrée / sortie".to_string(),
            format!("{} / {}", info.input_device.as_deref().unwrap_or("?"), info.output_device.as_deref().unwrap_or("?")),
        ),
        ("Émetteur distant".to_string(), info.remote.clone().unwrap_or_else(|| "aucun (flux fabriqués sur la machine)".into())),
        ("Plugin".to_string(), info.plugin.clone().unwrap_or_else(|| "aucun".into())),
        ("Banc".to_string(), info.bench_commit.clone()),
    ];
    if let Some(n) = &info.note {
        v.push(("Note".to_string(), n.clone()));
    }
    v
}

fn css_class(o: Outcome) -> &'static str {
    match o {
        Outcome::NoRegression => "ok",
        Outcome::Regression => "ko",
        Outcome::Inconclusive => "na",
        Outcome::Measured => "measured",
    }
}

/// Échappement HTML : les noms et messages viennent de fichiers.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Page autonome (aucune ressource externe). Chaque état a sa FORME (trait
/// plein, double, pointillé) et son mot, en plus de sa teinte : lisible sans
/// distinguer les couleurs.
fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="fr"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title><style>
:root{{--bg:#fbfaf7;--fg:#1d1d1b;--muted:#5d5a52;--ok:#2f6b3a;--ko:#8a1c1c;--na:#5d5a52;--line:#d9d5cb}}
@media (prefers-color-scheme:dark){{:root{{--bg:#151513;--fg:#ecebe6;--muted:#a9a598;--ok:#8fd19e;--ko:#f2a3a3;--na:#a9a598;--line:#3a3833}}}}
body{{background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif;max-width:60rem;margin:0 auto;padding:1.5rem 1rem}}
.badge{{display:inline-block;font-weight:700;letter-spacing:.02em;padding:.2rem .6rem;border-radius:.3rem}}
header.verdict .badge{{font-size:1.6rem}}
.ok .badge{{color:var(--ok);border:3px solid var(--ok)}}
.ko .badge{{color:var(--ko);border:3px double var(--ko)}}
.na .badge,.measured .badge{{color:var(--na);border:3px dashed var(--na)}}
dl{{display:grid;grid-template-columns:max-content 1fr;gap:.2rem 1rem;color:var(--muted)}}dt{{font-weight:600}}dd{{margin:0;overflow-wrap:anywhere}}
section{{border-top:1px solid var(--line);margin-top:1rem}}h2{{font-size:1.1rem}}h2 .badge{{font-size:.85rem}}
table{{border-collapse:collapse;width:100%}}td,th{{border-bottom:1px solid var(--line);padding:.3rem .5rem;text-align:left}}
.legend{{color:var(--muted);font-size:.9rem;margin-top:2rem}}
</style></head><body>{}</body></html>"#,
        esc(title),
        body
    )
}

/// Ouvre la page dans le navigateur ; sinon, dit où elle est.
pub fn open_page(path: &Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(path).status();
    #[cfg(windows)]
    let r = std::process::Command::new("cmd").args(["/C", "start", ""]).arg(path).status();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(path).status();
    if !r.is_ok_and(|s| s.success()) {
        println!("Ouvrir le verdict à la main : {}", path.display());
    }
}

// ─── Petits outils ───────────────────────────────────────────────────────────

fn write(dir: &Path, name: &str, content: &str) -> Result<(), String> {
    std::fs::write(dir.join(name), content).map_err(|e| format!("écriture de {name} : {e}"))
}

pub fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("{} : {e}", dst.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("{} : {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let to = dst.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to).map_err(|e| format!("{} : {e}", to.display()))?;
        }
    }
    Ok(())
}

/// Version du banc : le commit d'où ce binaire a été compilé (`build.rs`),
/// « +modifié » s'il y avait des changements non commités. Un binaire copié
/// sur une autre machine garde la sienne.
pub fn bench_commit() -> String {
    env!("BENCH_COMMIT").to_string()
}

/// (« 20261001-1840 » pour un nom de dossier, « 2026-10-01T18:40:12Z »), en UTC.
pub fn utc_now() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    utc_parts(secs)
}

/// Date civile d'un instant Unix (algorithme des jours civils, sans dépendance).
pub fn utc_parts(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let (h, mi, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}"), format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{CriterionMetrics, StreamMetrics};

    fn stream(name: &str, target: f64, holes: f64) -> StreamMetrics {
        StreamMetrics {
            stream: name.into(),
            links: "ethernet".into(),
            holes_per_min: Some(holes),
            causes_per_min: vec![Some(holes), Some(0.0), Some(0.0), Some(0.0), Some(0.0)],
            target_median_ms: Some(target),
            target_p95_ms: Some(target),
            late: Some(0.0),
            lost: Some(0.0),
        }
    }

    fn metrics(streams: Vec<StreamMetrics>, c5: Verdict) -> Metrics {
        Metrics {
            format: 1,
            scenario: "9-reseaux-mixtes".into(),
            fingerprint: "abc".into(),
            complete: true,
            precision: Precision::Sufficient,
            steps: Vec::new(),
            streams,
            criteria: vec![CriterionMetrics { id: 5, verdict: c5 }],
        }
    }

    #[test]
    fn la_date_utc_est_juste() {
        assert_eq!(utc_parts(0), ("19700101-0000".into(), "1970-01-01T00:00:00Z".into()));
        // 01/10/2026 18:40:12 UTC.
        assert_eq!(utc_parts(1_790_880_012).1, "2026-10-01T18:40:12Z");
        assert_eq!(utc_parts(951_782_400).1, "2000-02-29T00:00:00Z", "année bissextile");
    }

    /// Dans la tolérance : aucune régression ; au-delà : régression qui nomme
    /// le flux, l'écart et la tolérance ; un mieux est dit comme tel.
    #[test]
    fn un_flux_hors_tolerance_est_une_regression_qui_dit_son_ecart() {
        let r = metrics(vec![stream("m2-ethernet", 5.0, 0.2), stream("m9-4g", 40.0, 150.0)], Verdict::Holds);
        let tol = |_: &str| TOL_FLOOR;
        let same = metrics(vec![stream("m2-ethernet", 6.0, 0.6), stream("m9-4g", 40.0, 150.4)], Verdict::Holds);
        assert_eq!(compare_scenario(&same, Some(&r), &tol).outcome, Outcome::NoRegression);
        let worse = metrics(vec![stream("m2-ethernet", 8.0, 0.2), stream("m9-4g", 40.0, 120.0)], Verdict::Holds);
        let v = compare_scenario(&worse, Some(&r), &tol);
        assert_eq!(v.outcome, Outcome::Regression);
        assert!(v.lines[0].contains("✖ m2-ethernet : cible médiane 5.0 → 8.0 ms (+3.0, tolérance 1.0)"), "{:?}", v.lines);
        assert!(v.lines.iter().any(|l| l.starts_with("▲ amélioration — m9-4g : trous")), "{:?}", v.lines);
        // Une tolérance mesurée plus large absorbe l'écart.
        let wide = |_: &str| Tol { target_ms: 4.0, holes_per_min: 50.0 };
        assert_eq!(compare_scenario(&worse, Some(&r), &wide).outcome, Outcome::NoRegression);
    }

    /// Critère tenu dans la référence et perdu : régression, même dans les tolérances.
    #[test]
    fn un_critere_perdu_est_une_regression() {
        let r = metrics(vec![stream("m2-ethernet", 5.0, 0.0)], Verdict::Holds);
        let c = metrics(vec![stream("m2-ethernet", 5.0, 0.0)], Verdict::Fails);
        let v = compare_scenario(&c, Some(&r), &|_| TOL_FLOOR);
        assert_eq!(v.outcome, Outcome::Regression);
        assert!(v.lines[0].contains("critère 5"));
    }

    /// Ce qui ne se compare pas est dit, jamais approximé.
    #[test]
    fn ce_qui_ne_se_compare_pas_est_non_concluant_et_dit_pourquoi() {
        let r = metrics(vec![stream("m2-ethernet", 5.0, 0.0)], Verdict::Holds);
        let tol = |_: &str| TOL_FLOOR;
        let cases = [
            (None, metrics(vec![stream("m2-ethernet", 9.0, 9.0)], Verdict::Holds), "aucune référence"),
            (Some(&r), Metrics { fingerprint: "autre".into(), ..metrics(vec![], Verdict::Holds) }, "scénario modifié"),
            (Some(&r), Metrics { precision: Precision::Insufficient, ..metrics(vec![], Verdict::Holds) }, "précision du banc"),
            (Some(&r), Metrics { complete: false, ..metrics(vec![], Verdict::Holds) }, "interrompu"),
        ];
        for (reference, current, why) in cases {
            let v = compare_scenario(&current, reference, &tol);
            assert_eq!(v.outcome, Outcome::Inconclusive, "{why}");
            assert!(v.lines[0].contains(why), "{why} : {:?}", v.lines);
        }
    }

    /// Bruit : la tolérance d'un flux suit l'écart entre passages, jamais sous
    /// les planchers.
    #[test]
    fn les_tolerances_suivent_le_bruit_mesure() {
        let run = |t: f64, h: f64| metrics(vec![stream("m2-ethernet", 5.0, 0.0), stream("m9-4g", t, h)], Verdict::Holds);
        let runs = BTreeMap::from([("9-reseaux-mixtes".to_string(), vec![run(38.0, 140.0), run(40.0, 150.0), run(39.0, 160.0)])]);
        let t = tolerances_from_runs(&runs, "bruit-x");
        let s = &t.scenarios["9-reseaux-mixtes"];
        assert_eq!(s["m2-ethernet"], TOL_FLOOR, "aucun écart : les planchers");
        assert_eq!(s["m9-4g"], Tol { target_ms: 4.0, holes_per_min: 40.0 });
        assert_eq!(t.get("9-reseaux-mixtes", "m9-4g").unwrap().target_ms, 4.0);
        assert!(t.get("autre", "m9-4g").is_none());
    }

    /// Une campagne complète : écrite, comparée à sa référence, indexée ; la
    /// page dit le verdict par symbole et mot, échappe ce qui vient des fichiers.
    #[test]
    fn une_campagne_se_range_se_compare_et_s_indexe() {
        let base = std::env::temp_dir().join(format!("banc-r5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mk = |name: &str, target: f64| -> (PathBuf, CampaignInfo) {
            let dir = base.join("campagnes").join("NUC").join(name);
            std::fs::create_dir_all(dir.join("9-reseaux-mixtes")).unwrap();
            let m = metrics(vec![stream("m2-<ethernet>", target, 0.0)], Verdict::Holds);
            std::fs::write(dir.join("9-reseaux-mixtes/metrics.json"), serde_json::to_string(&m).unwrap()).unwrap();
            let info = CampaignInfo {
                format: 1,
                machine: "NUC".into(),
                kind: CampaignKind::Rapide,
                started_utc: "2026-10-01T18:40:12Z".into(),
                audio_engine: "0.6.6-15".into(),
                bench_commit: "x".into(),
                input_device: Some("1:UMC ASIO Driver".into()),
                output_device: Some("1:UMC ASIO Driver".into()),
                remote: Some("192.168.1.49:51901".into()),
                plugin: None,
                scenarios: vec![ScenarioEntry {
                    dir: "9-reseaux-mixtes".into(),
                    scenario: "9-reseaux-mixtes".into(),
                    fingerprint: "abc".into(),
                    status: "complète".into(),
                }],
                note: None,
            };
            info.save(&dir).unwrap();
            (dir, info)
        };
        let (ref_dir, ref_info) = mk("ref", 5.0);
        finish(&base, &ref_dir, &ref_info).unwrap();
        assert_eq!(latest_campaign(&base, "NUC").as_deref(), Some("campagnes/NUC/ref"));
        let mut refs = References::load(&base).unwrap();
        refs.set("NUC", "campagnes/NUC/ref", "0.6.6-15", "2026-10-01T19:00:00Z");
        refs.save(&base).unwrap();
        let (dir, info) = mk("nouvelle", 9.0);
        let page = finish(&base, &dir, &info).unwrap();
        let md = std::fs::read_to_string(dir.join("verdict.md")).unwrap();
        assert!(md.starts_with("# ✖ RÉGRESSION"), "{md}");
        let html = std::fs::read_to_string(page).unwrap();
        assert!(html.contains("✖ RÉGRESSION") && html.contains("m2-&lt;ethernet&gt;") && !html.contains("m2-<ethernet>"));
        let index = std::fs::read_to_string(base.join("index.csv")).unwrap();
        assert_eq!(index.lines().count(), 3, "{index}");
        assert!(index.lines().nth(2).unwrap().ends_with(",regression,campagnes/NUC/nouvelle"), "{index}");
        let refs = References::load(&base).unwrap();
        assert_eq!(refs.machines["NUC"].history.len(), 1, "l'historique garde chaque référence");
        // Autre carte son que la référence : pas de comparaison, la raison est dite.
        let (dir, mut info) = mk("autre-carte", 9.0);
        info.output_device = Some("0:ASIO4ALL v2".into());
        let refinfo = CampaignInfo::load(&base.join("campagnes/NUC/ref")).unwrap();
        let v = compare_campaign(&dir, &info, Some((&refinfo, &base.join("campagnes/NUC/ref"))), None);
        assert_eq!(v.outcome, Outcome::Inconclusive);
        assert!(v.scenarios[0].lines[0].contains("sortie 0:ASIO4ALL v2 ≠ référence 1:UMC ASIO Driver"), "{:?}", v.scenarios[0].lines);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Un dossier qui n'est pas un scénario du banc fait échouer l'import
    /// AVANT toute copie : aucune campagne à moitié rangée.
    #[test]
    fn un_import_invalide_ne_laisse_rien_derriere_lui() {
        let base = std::env::temp_dir().join(format!("banc-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let e = import(&base, CampaignKind::Importee, "NUC", "essai", &[base.join("pas-un-scenario")]).unwrap_err();
        assert!(e.contains("scenario.json absent"), "{e}");
        assert!(!base.join("campagnes").exists(), "rien de créé");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn la_serie_de_bruit_entrelace_ses_passages() {
        let e = CampaignKind::Bruit.entries();
        assert_eq!(e.len(), 12);
        assert_eq!((e[0].0.as_str(), e[4].0.as_str()), ("1-regulier-9", "2-regulier-9"));
        assert_eq!(CampaignKind::Complete.entries().len(), 8);
        assert!(CampaignKind::Rapide.entries().iter().all(|(_, n)| crate::library::named(n).is_some()));
    }

    #[test]
    fn la_configuration_de_la_machine_s_applique_au_scenario_sans_changer_son_empreinte() {
        let cfg = MachineConfig {
            machine: "NUC".into(),
            input_device: "1:UMC ASIO Driver".into(),
            output_device: "1:UMC ASIO Driver".into(),
            channel_index: None,
            remote: Some("192.168.1.49:51901".into()),
            plugin: None,
        };
        let base = crate::library::named("pics-seuls").unwrap();
        let s = cfg.apply(base.clone());
        assert_eq!(s.remote.as_deref(), Some("192.168.1.49:51901"));
        assert_eq!(s.fingerprint(), base.fingerprint());
        s.validate().unwrap();
        assert!(serde_json::from_str::<MachineConfig>(r#"{"machine":"x","input_device":"a","output_device":"b","remot":"y"}"#).is_err());
    }
}
