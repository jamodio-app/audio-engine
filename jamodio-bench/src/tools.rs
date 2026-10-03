//! Commandes d'accompagnement de la campagne de version (lot R5) :
//! `configurer` (une fois par machine), `reference` (après validation d'une
//! version), `archiver` (ranger une campagne hors de la machine).

use crate::campaign::{self, CampaignInfo, CampaignKind, MachineConfig, References};
use crate::driver::AgentLink;
use crate::scenario::Scenario;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Pose une question, rend la réponse sans espaces autour.
fn ask(question: &str) -> Result<String, String> {
    print!("{question} ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

/// Choix dans une liste numérotée (1, 2…), au format strict de l'agent.
fn choose(title: &str, ids: &[String]) -> Result<String, String> {
    println!("{title} :");
    for (i, id) in ids.iter().enumerate() {
        println!("  {}. {id}", i + 1);
    }
    let answer = ask("Numéro :")?;
    answer
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| ids.get(i))
        .cloned()
        .ok_or(format!("« {answer} » : un numéro de la liste, de 1 à {}", ids.len()))
}

/// `session-bench configurer` : la carte son, l'émetteur distant et le nom de
/// cette machine, une fois pour toutes (`bench-results/machine.json`).
pub async fn configure(base: &Path) -> Result<(), String> {
    if let Ok(current) = MachineConfig::load(base) {
        println!("Configuration actuelle :\n{}", serde_json::to_string_pretty(&current).map_err(|e| e.to_string())?);
        if ask("La remplacer ? (o/n)")? != "o" {
            return Ok(());
        }
    }
    println!("Connexion à l'Audio Engine (un studio ouvert sera déconnecté)…");
    let agent = AgentLink::connect(&Scenario::default().agent_url).await?;
    let devices = agent.devices().await?;
    let _ = agent.stop();
    let ids = |key: &str| -> Vec<String> {
        devices[key].as_array().into_iter().flatten().filter_map(|d| d["id"].as_str().map(str::to_string)).collect()
    };
    let input_device = choose("Entrée (ta carte son)", &ids("inputs"))?;
    let output_device = choose("Sortie", &ids("outputs"))?;
    let remote = ask("Émetteur distant IP:PORT (ex. 192.168.1.20:51901 ; Entrée = flux fabriqués sur cette machine) :")?;
    let remote = if remote.is_empty() {
        None
    } else {
        // Même règle que le scénario : adresse réseau, jamais le bouclage.
        Scenario { remote: Some(remote.clone()), ..Scenario::default() }.validate()?;
        Some(remote)
    };
    let plugin = ask("Plugin à insérer pendant les campagnes (nom exact ; Entrée = aucun) :")?;
    // Un nom déjà connu ici (campagnes, tolérances) plutôt que le nom réseau :
    // sinon la campagne ne retrouve ni ses tolérances ni sa référence (NUC,
    // 02/10/2026 : le nom réseau proposé au lieu du nom déjà utilisé).
    let known = known_machines(base);
    if !known.is_empty() {
        println!("Machines déjà connues ici : {}", known.join(", "));
    }
    let default_name = match known.as_slice() {
        [one] => one.clone(),
        _ => crate::run::machine_name(),
    };
    let machine = ask(&format!("Nom de cette machine dans les résultats [{default_name}] :"))?;
    let cfg = MachineConfig {
        machine: if machine.is_empty() { default_name } else { machine },
        input_device,
        output_device,
        channel_index: None,
        remote,
        plugin: (!plugin.is_empty()).then_some(plugin),
    };
    if cfg.machine.contains(['/', '\\', ',']) {
        return Err("le nom de la machine ne doit contenir ni « / », ni « \\ », ni « , »".into());
    }
    cfg.save(base)?;
    println!("Enregistré dans {} :\n{}", MachineConfig::path(base).display(), serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?);
    Ok(())
}

/// Noms de machine déjà présents dans `bench-results` (campagnes, tolérances).
pub fn known_machines(base: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(base.join("campagnes"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .chain(
            std::fs::read_dir(base.join("tolerances"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok()?.strip_suffix(".json").map(str::to_string)),
        )
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `session-bench reference [DOSSIER]` : la campagne (la dernière de cette
/// machine par défaut) devient sa référence, après confirmation. L'ancienne
/// reste dans l'historique ; rien n'est effacé.
pub fn set_reference(base: &Path, dir: Option<&str>) -> Result<(), String> {
    let cfg = MachineConfig::load(base)?;
    let rel = match dir {
        Some(d) => d.trim_end_matches(['/', '\\']).replace('\\', "/"),
        None => campaign::latest_campaign(base, &cfg.machine).ok_or(format!("aucune campagne pour {} dans index.csv", cfg.machine))?,
    };
    let path = base.join(&rel);
    let info = CampaignInfo::load(&path)?;
    if info.machine != cfg.machine {
        return Err(format!("cette campagne est celle de {}, pas de {} : une référence est propre à sa machine", info.machine, cfg.machine));
    }
    if info.kind == CampaignKind::Bruit {
        return Err("une campagne de bruit mesure les tolérances, elle ne sert pas de référence".into());
    }
    let verdict = std::fs::read_to_string(path.join("verdict.md")).unwrap_or_default();
    println!("{rel}\n  Audio Engine {} — {}", info.audio_engine, verdict.lines().next().unwrap_or("(pas de verdict)"));
    if ask(&format!("En faire la référence de {} ? (o/n)", cfg.machine))? != "o" {
        println!("Rien changé.");
        return Ok(());
    }
    let mut refs = References::load(base)?;
    refs.set(&cfg.machine, &rel, &info.audio_engine, &campaign::utc_now().1);
    refs.save(base)?;
    println!("Référence de {} : {rel} (Audio Engine {}).", cfg.machine, info.audio_engine);
    Ok(())
}

/// `session-bench archiver DOSSIER --resumes D [--bruts D]` : copie les résumés
/// (quelques Ko : campagne.json, verdict, resume.md, metrics.json,
/// scenario.json) sous `D/<machine>/<campagne>/`, et la campagne entière (CSV
/// compris) sous les bruts. Ajoute la campagne à `D/index.csv`. Copie
/// seulement : la campagne reste sur la machine.
pub fn archive(base: &Path, dir: &str, summaries: &Path, raw: Option<&Path>) -> Result<(), String> {
    let path = if Path::new(dir).is_absolute() { PathBuf::from(dir) } else { base.join(dir) };
    let info = CampaignInfo::load(&path)?;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or("nom de campagne illisible")?.to_string();
    let dst = summaries.join(&info.machine).join(&name);
    std::fs::create_dir_all(&dst).map_err(|e| format!("{} : {e}", dst.display()))?;
    for f in ["campagne.json", "verdict.md", "verdict.html"] {
        copy_if_present(&path.join(f), &dst.join(f))?;
    }
    for e in &info.scenarios {
        std::fs::create_dir_all(dst.join(&e.dir)).map_err(|err| err.to_string())?;
        for f in ["resume.md", "metrics.json", "scenario.json"] {
            copy_if_present(&path.join(&e.dir).join(f), &dst.join(&e.dir).join(f))?;
        }
    }
    // Index des résumés archivés : une ligne par campagne, sans doublon.
    let index = summaries.join("index.csv");
    let line = format!("{},{},{},{},{}/{}", info.started_utc, info.machine, info.audio_engine, info.kind.label(), info.machine, name);
    let current = std::fs::read_to_string(&index).unwrap_or_default();
    if !current.lines().any(|l| l == line) {
        let mut text = if current.is_empty() { "date_utc,machine,audio_engine,type,dossier\n".to_string() } else { current };
        text.push_str(&line);
        text.push('\n');
        std::fs::write(&index, text).map_err(|e| format!("{} : {e}", index.display()))?;
    }
    println!("Résumés : {}", dst.display());
    if let Some(raw) = raw {
        let dst = raw.join(&info.machine).join(&name);
        campaign::copy_dir(&path, &dst)?;
        println!("Campagne entière (CSV compris) : {}", dst.display());
    }
    Ok(())
}

fn copy_if_present(from: &Path, to: &Path) -> Result<(), String> {
    if from.exists() {
        std::fs::copy(from, to).map_err(|e| format!("{} : {e}", to.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'archive garde les résumés (pas les CSV), une ligne d'index par
    /// campagne même archivée deux fois, et la campagne entière côté bruts.
    #[test]
    fn les_machines_connues_viennent_des_campagnes_et_des_tolerances() {
        let base = std::env::temp_dir().join(format!("banc-noms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("campagnes/NUC")).unwrap();
        std::fs::create_dir_all(base.join("tolerances")).unwrap();
        std::fs::write(base.join("tolerances/NUC.json"), "{}").unwrap();
        std::fs::write(base.join("tolerances/MacBook.json"), "{}").unwrap();
        assert_eq!(known_machines(&base), vec!["MacBook".to_string(), "NUC".to_string()]);
        assert!(known_machines(&base.join("absent")).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn l_archive_garde_les_resumes_et_indexe_sans_doublon() {
        let root = std::env::temp_dir().join(format!("banc-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join("bench-results");
        let dir = base.join("campagnes/NUC/20261001-1840-0.6.6-15-rapide");
        std::fs::create_dir_all(dir.join("regulier-9")).unwrap();
        for f in ["resume.md", "metrics.json", "scenario.json", "peers.csv"] {
            std::fs::write(dir.join("regulier-9").join(f), "x").unwrap();
        }
        std::fs::write(dir.join("verdict.md"), "# ✔ AUCUNE RÉGRESSION").unwrap();
        let info = CampaignInfo {
            format: 1,
            machine: "NUC".into(),
            kind: CampaignKind::Rapide,
            started_utc: "2026-10-01T18:40:12Z".into(),
            audio_engine: "0.6.6-15".into(),
            bench_commit: "x".into(),
            input_device: None,
            output_device: None,
            remote: None,
            plugin: None,
            scenarios: vec![campaign::ScenarioEntry {
                dir: "regulier-9".into(),
                scenario: "regulier-9".into(),
                fingerprint: "f".into(),
                status: "complète".into(),
            }],
            note: None,
        };
        std::fs::write(dir.join("campagne.json"), serde_json::to_string(&info).unwrap()).unwrap();
        let (summaries, raw) = (root.join("resumes"), root.join("bruts"));
        for _ in 0..2 {
            archive(&base, "campagnes/NUC/20261001-1840-0.6.6-15-rapide", &summaries, Some(&raw)).unwrap();
        }
        let kept = summaries.join("NUC/20261001-1840-0.6.6-15-rapide");
        assert!(kept.join("verdict.md").exists() && kept.join("regulier-9/metrics.json").exists());
        assert!(!kept.join("regulier-9/peers.csv").exists(), "pas de CSV dans les résumés");
        assert!(raw.join("NUC/20261001-1840-0.6.6-15-rapide/regulier-9/peers.csv").exists(), "les CSV côté bruts");
        let index = std::fs::read_to_string(summaries.join("index.csv")).unwrap();
        assert_eq!(index.lines().count(), 2, "{index}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
