//! `session-bench` — le banc « N musiciens » en ligne de commande.
//!
//! ```text
//! session-bench devices                     # périphériques vus par l'Audio Engine
//! session-bench run                         # 2 → 9 musiciens, 5 min par palier, flux réguliers
//! session-bench run --profile ethernet --to 6 --step-secs 120
//! session-bench run --profiles regular,wifi --peer-voice bursts --send-voice 1
//! session-bench run --scenario mon-test.json
//! session-bench run --named 9-reseaux-mixtes   # scénario de la bibliothèque
//! session-bench scenarios                   # la bibliothèque
//! session-bench scenario > mon-test.json    # scénario par défaut, à modifier
//! session-bench selftest 8 30               # précision du banc seul, avant une campagne
//! ```
//!
//! L'Audio Engine doit tourner (pré-version ≥ 0.6.6-1 pour la cause des trous) ;
//! le studio ouvert dans le navigateur sera déconnecté pendant le banc.

use jamodio_bench::driver::AgentLink;
use jamodio_bench::profile::{Link, PeerProfile, Speech};
use jamodio_bench::scenario::Scenario;
use std::path::PathBuf;

const USAGE: &str = "\
session-bench — banc « N musiciens » contre l'Audio Engine installé

Campagne de version (lancée depuis le dépôt de l'Audio Engine) :
  session-bench configurer                     (une fois par machine : carte son, émetteur distant, nom)
  session-bench conseil [DEPUIS] [JUSQUA] [--publique]
                                               (faut-il une campagne ? DEPUIS = version de référence)
  session-bench version [rapide|complete|bruit] (la campagne ; verdict ouvert à la fin)
  session-bench reference [DOSSIER]            (la dernière campagne devient la référence de la machine)
  session-bench verdict [DOSSIER]              (recalcule le verdict avec la référence et les tolérances actuelles)
  session-bench importer --type importee|bruit --machine NOM --note TEXTE DOSSIER…
                                               (range des campagnes lancées à la main)
  session-bench reanalyser DOSSIER             (refait metrics.json depuis les CSV)
  session-bench archiver DOSSIER --resumes DIR [--bruts DIR]

Scénarios et réglages fins :

  session-bench devices
  session-bench plugins                        (plugins connus de l'Audio Engine)
  session-bench relay [--listen IP] [--port N] (SECONDE machine : relais du mode réseau)
  session-bench remote [--listen IP] [--port N] (SECONDE machine : émetteur distant —
                                               y fabrique les flux simulés, cf. --remote)
  session-bench scenarios                      (la bibliothèque de scénarios nommés)
  session-bench scenario [NOM]                 (écrit le scénario par défaut, ou NOM, en JSON)
  session-bench selftest [FLUX] [SECONDES]     (précision du banc seul, sans Audio Engine ; défaut 8 flux, 30 s)
  session-bench run [options]

Options de run :
  --named NOM             part d'un scénario de la bibliothèque (cf. scenarios)
  --scenario FICHIER      part d'un scénario JSON (les options suivantes le modifient)
  --from N / --to N       premier / dernier palier, toi compris (défaut 2 → 9)
  --step-secs S           durée d'un palier (défaut 300)
  --warmup-secs S         installation exclue de l'analyse (défaut 30)
  --profile P             même profil pour tous : regular | ethernet | wifi | fibre |
                          adsl | wifi-charge | 4g (les quatre derniers : à calibrer)
  --profiles P1,P2,…      profils attribués dans l'ordre d'arrivée, en boucle
  --loss PCT              pertes simulées (%), pour tous les profils
  --peer-voice V          talkback des musiciens simulés : none | bursts | always
  --send-voice CANAL      l'Audio Engine envoie aussi son talkback (canal 1, 2…)
  --input ID / --output ID  périphériques, identifiant EXACT « idx:nom » (cf. devices),
                          ex. --input \"1:UMC ASIO Driver\"
  --channel CANAL         canal de l'instrument (1, 2…)
  --plugin NOM            charge ce plugin sur l'instrument, dans l'Audio Engine
                          (nom exact, cf. plugins) — la charge réelle du musicien
  --seed N                graine (même graine = mêmes retards et pertes)
  --agent URL             WebSocket de l'Audio Engine (défaut ws://127.0.0.1:9876)
  --relay IP:PORT         mode réseau : les flux passent par le relais lancé sur
                          une seconde machine (session-bench relay)
  --remote IP:PORT        émetteur distant : les flux simulés sont fabriqués et
                          envoyés par une seconde machine (session-bench remote) —
                          la précision du banc ne dépend plus de cette machine
  --out DOSSIER           où écrire les résultats (défaut bench-results/<date>)
  --save-scenario FICHIER écrit le scénario final avant de lancer
  --no-mmcss              Windows : fils du banc en priorité TIME_CRITICAL, sans MMCSS
                          (à coupler avec « no-mmcss = 1 » dans le bench-flags de
                          l'Audio Engine) ; noté dans le résumé
";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("devices") => devices(&Scenario::default().agent_url).await,
        Some("plugins") => plugins(&Scenario::default().agent_url).await,
        Some("relay") => relay(&args[1..]),
        Some("remote") => remote(&args[1..]),
        Some("scenarios") => {
            for (name, what) in jamodio_bench::library::NAMED {
                println!("  {name:<24} {what}");
            }
            Ok(())
        }
        Some("scenario") => {
            let scenario = match args.get(1) {
                Some(name) => named(name),
                None => Ok(Scenario::default()),
            };
            scenario.and_then(|s| serde_json::to_string_pretty(&s).map_err(|e| e.to_string())).map(|j| println!("{j}"))
        }
        Some("run") => run(&args[1..]).await,
        Some("configurer") => jamodio_bench::tools::configure(&results()).await,
        Some("version") => version(&args[1..]).await,
        Some("reference") => jamodio_bench::tools::set_reference(&results(), args.get(1).map(String::as_str)),
        Some("verdict") => jamodio_bench::campaign::recompute_verdict(&results(), args.get(1).map(String::as_str)).map(|p| {
            jamodio_bench::campaign::open_page(&p);
        }),
        Some("conseil") => advice(&args[1..]),
        Some("importer") => import(&args[1..]),
        Some("reanalyser") => match args.get(1) {
            Some(d) => jamodio_bench::campaign::reanalyze(std::path::Path::new(d)).map(|m| {
                println!("metrics.json réécrit ({} flux, {} palier(s)).", m.streams.len(), m.steps.len())
            }),
            None => Err("reanalyser DOSSIER".into()),
        },
        Some("archiver") => archive(&args[1..]),
        Some("selftest") => {
            let streams = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(8);
            let secs = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(30);
            println!("Précision du banc seul : {streams} flux, {secs} s (aucun Audio Engine sollicité).");
            match tokio::task::spawn_blocking(move || jamodio_bench::run::selftest(streams, secs)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(e.to_string()),
            }
        }
        _ => {
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = code {
        eprintln!("\nERREUR : {e}");
        std::process::exit(1);
    }
}

/// Dossier des résultats : `bench-results/` là où le banc est lancé.
fn results() -> PathBuf {
    PathBuf::from("bench-results")
}

async fn version(args: &[String]) -> Result<(), String> {
    use jamodio_bench::campaign::CampaignKind;
    let kind = match args.first().map(String::as_str) {
        None | Some("rapide") => CampaignKind::Rapide,
        Some("complete") | Some("complète") => CampaignKind::Complete,
        Some("bruit") => CampaignKind::Bruit,
        Some(other) => return Err(format!("version : rapide | complete | bruit, reçu « {other} »")),
    };
    jamodio_bench::campaign::version(&results(), kind).await.map(|_| ())
}

fn advice(args: &[String]) -> Result<(), String> {
    use jamodio_bench::campaign::{CampaignInfo, MachineConfig, References};
    let public = args.iter().any(|a| a == "--publique");
    let mut revs = args.iter().filter(|a| !a.starts_with("--"));
    let from = match revs.next() {
        Some(r) => r.clone(),
        None => {
            // La version de référence de cette machine : son tag « v… ».
            let base = results();
            let machine = MachineConfig::load(&base)?.machine;
            let rel = References::load(&base)?
                .current(&machine)
                .map(str::to_string)
                .ok_or(format!("{machine} n'a pas de référence : préciser DEPUIS (ex. v0.6.6-15)"))?;
            format!("v{}", CampaignInfo::load(&base.join(rel))?.audio_engine)
        }
    };
    let to = revs.next().cloned().unwrap_or_else(|| "HEAD".into());
    let files = jamodio_bench::advice::changed_files(&from, &to)?;
    print!("{}", jamodio_bench::advice::render(&from, &to, &files, public));
    Ok(())
}

fn import(args: &[String]) -> Result<(), String> {
    use jamodio_bench::campaign::CampaignKind;
    let (mut kind, mut machine, mut note, mut dirs) = (None, None, String::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--type" => {
                kind = Some(match it.next().map(String::as_str) {
                    Some("importee") => CampaignKind::Importee,
                    Some("bruit") => CampaignKind::Bruit,
                    other => return Err(format!("--type importee | bruit, reçu {other:?}")),
                })
            }
            "--machine" => machine = it.next().cloned(),
            "--note" => note = it.next().cloned().unwrap_or_default(),
            d => dirs.push(PathBuf::from(d)),
        }
    }
    let machine = machine.ok_or("--machine NOM (celui de session-bench configurer)")?;
    let dir = jamodio_bench::campaign::import(&results(), kind.ok_or("--type importee | bruit")?, &machine, &note, &dirs)?;
    println!("Campagne rangée : {}", dir.display());
    Ok(())
}

fn archive(args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    let dir = it.next().ok_or("archiver DOSSIER --resumes DIR [--bruts DIR]")?.clone();
    let (mut summaries, mut raw) = (None, None);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--resumes" => summaries = it.next().map(PathBuf::from),
            "--bruts" => raw = it.next().map(PathBuf::from),
            other => return Err(format!("option inconnue : {other}")),
        }
    }
    let summaries = summaries.ok_or("--resumes DIR")?;
    jamodio_bench::tools::archive(&results(), &dir, &summaries, raw.as_deref())
}

async fn devices(url: &str) -> Result<(), String> {
    let agent = AgentLink::connect(url).await?;
    let d = agent.devices().await?;
    for (title, key) in [("Entrées", "inputs"), ("Sorties", "outputs")] {
        println!("{title} :");
        for dev in d[key].as_array().into_iter().flatten() {
            println!(
                "  {}{}  ({} canaux)",
                dev["id"].as_str().unwrap_or("?"),
                if dev["isDefault"].as_bool() == Some(true) { "  [par défaut]" } else { "" },
                dev["channels"]
            );
        }
    }
    Ok(())
}

async fn plugins(url: &str) -> Result<(), String> {
    let agent = AgentLink::connect(url).await?;
    for p in agent.plugins().await? {
        println!(
            "  {}  ({}){}",
            p["name"].as_str().unwrap_or("?"),
            p["manufacturer"].as_str().unwrap_or("?"),
            if p["incompatible"].as_bool() == Some(true) { "  [latence trop grande pour le direct]" } else { "" }
        );
    }
    Ok(())
}

/// L'émetteur distant : fabrique et envoie les flux simulés depuis une seconde
/// machine, reçoit ce que l'agent envoie (lot R1-bis).
fn remote(args: &[String]) -> Result<(), String> {
    let (listen, port) = listen_options(args, jamodio_bench::remote::DEFAULT_PORT)?;
    jamodio_bench::remote::serve(listen, port)
}

/// `--listen IP` (défaut : l'adresse réseau de cette machine) et `--port N`.
fn listen_options(args: &[String], default_port: u16) -> Result<(std::net::IpAddr, u16), String> {
    let mut listen = None;
    let mut port = default_port;
    let mut it = args.iter();
    while let Some(opt) = it.next() {
        let v = it.next().ok_or(format!("{opt} attend une valeur"))?;
        match opt.as_str() {
            "--listen" => listen = Some(v.parse().map_err(|e| format!("--listen {v} : {e}"))?),
            "--port" => port = v.parse().map_err(|e| format!("--port {v} : {e}"))?,
            other => return Err(format!("option inconnue : {other}")),
        }
    }
    let listen = match listen {
        Some(ip) => ip,
        None => jamodio_bench::scenario::primary_local_ip()?,
    };
    Ok((listen, port))
}

/// La seconde machine du mode réseau : relaie les flux entre le banc et
/// l'agent de la machine mesurée.
fn relay(args: &[String]) -> Result<(), String> {
    let (listen, port) = listen_options(args, jamodio_bench::relay::DEFAULT_PORT)?;
    jamodio_bench::relay::serve(listen, port)
}

async fn run(args: &[String]) -> Result<(), String> {
    let (scenario, out, save) = parse_run(args)?;
    scenario.validate()?;
    if let Some(path) = save {
        let json = serde_json::to_string_pretty(&scenario).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("{} : {e}", path.display()))?;
    }
    let out = out.unwrap_or_else(|| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        PathBuf::from("bench-results").join(format!("{}-{secs}", scenario.name))
    });
    println!(
        "Campagne « {} » : {} → {} musiciens, {} s par palier (~{} min).",
        scenario.name,
        scenario.from_musicians,
        scenario.to_musicians,
        scenario.step_secs,
        scenario.total_secs().div_ceil(60)
    );
    let summary = jamodio_bench::run::run(scenario, &out).await?;
    println!("Résultats : {}", summary.parent().unwrap_or(&out).display());
    Ok(())
}

/// Le scénario nommé de la bibliothèque, ou une erreur qui les cite tous.
fn named(name: &str) -> Result<Scenario, String> {
    jamodio_bench::library::named(name).ok_or_else(|| {
        let names: Vec<&str> = jamodio_bench::library::NAMED.iter().map(|(n, _)| *n).collect();
        format!("scénario « {name} » inconnu. Connus : {}", names.join(", "))
    })
}

/// Lit les options de `run` (sans dépendance : une vingtaine d'options suffit).
fn parse_run(args: &[String]) -> Result<(Scenario, Option<PathBuf>, Option<PathBuf>), String> {
    // Le scénario de départ d'abord : les autres options le modifient.
    let file = args.iter().position(|a| a == "--scenario");
    let library = args.iter().position(|a| a == "--named");
    let mut scenario = match (file, library) {
        (Some(_), Some(_)) => return Err("--scenario ou --named, pas les deux".into()),
        (Some(i), None) => {
            let path = args.get(i + 1).ok_or("--scenario attend un fichier")?;
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path} : {e}"))?;
            serde_json::from_str(&text).map_err(|e| format!("{path} : {e}"))?
        }
        (None, Some(i)) => named(args.get(i + 1).ok_or("--named attend un nom (cf. scenarios)")?)?,
        (None, None) => Scenario::default(),
    };
    let (mut out, mut save) = (None, None);
    let mut it = args.iter();
    while let Some(opt) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{opt} attend une valeur"));
        let num = |v: String| v.parse::<u64>().map_err(|_| format!("{opt} : nombre attendu, reçu {v}"));
        match opt.as_str() {
            "--scenario" | "--named" => {
                val()?;
            }
            "--from" => scenario.from_musicians = num(val()?)? as u32,
            "--to" => scenario.to_musicians = num(val()?)? as u32,
            "--step-secs" => scenario.step_secs = num(val()?)?,
            "--warmup-secs" => scenario.warmup_secs = num(val()?)?,
            "--seed" => scenario.seed = num(val()?)?,
            "--profile" | "--profiles" => {
                let v = val()?;
                scenario.peers = v
                    .split(',')
                    .map(|n| {
                        PeerProfile::preset(n.trim()).ok_or(format!("profil inconnu : {n} ({})", Link::PRESETS.join(" | ")))
                    })
                    .collect::<Result<_, _>>()?;
            }
            "--loss" => {
                let v = val()?;
                let pct: f64 = v.parse().map_err(|_| format!("--loss : pourcentage attendu, reçu {v}"))?;
                scenario.peers.iter_mut().for_each(|p| p.loss_pct = pct);
            }
            "--peer-voice" => {
                let speech = match val()?.as_str() {
                    "none" => None,
                    "always" => Some(Speech::Always),
                    "bursts" => Some(Speech::Bursts { talk_mean_s: 3.0, silence_mean_s: 6.0 }),
                    other => return Err(format!("--peer-voice : none | bursts | always, reçu {other}")),
                };
                scenario.peers.iter_mut().for_each(|p| p.voice = speech);
            }
            "--send-voice" => scenario.send_voice_channel = Some(channel(&val()?)?),
            "--channel" => scenario.channel_index = Some(channel(&val()?)?),
            "--input" => scenario.input_device = Some(val()?),
            "--plugin" => scenario.plugin = Some(val()?),
            "--output" => scenario.output_device = Some(val()?),
            "--agent" => scenario.agent_url = val()?,
            "--relay" => scenario.relay = Some(val()?),
            "--remote" => scenario.remote = Some(val()?),
            "--out" => out = Some(PathBuf::from(val()?)),
            "--save-scenario" => save = Some(PathBuf::from(val()?)),
            "--no-mmcss" => scenario.no_mmcss = true,
            other => return Err(format!("option inconnue : {other}\n\n{USAGE}")),
        }
    }
    Ok((scenario, out, save))
}

/// Canal tel que le musicien le nomme (1, 2…) → index de l'agent (0, 1…).
fn channel(v: &str) -> Result<u8, String> {
    match v.parse::<u8>() {
        Ok(n) if n >= 1 => Ok(n - 1),
        _ => Err(format!("canal attendu (1, 2…), reçu {v}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn les_options_modifient_le_scenario() {
        let (s, out, _) = parse_run(&args(
            "--to 6 --step-secs 120 --profiles regular,wifi --loss 0.5 --peer-voice bursts --send-voice 2 --channel 1 --out r",
        ))
        .unwrap();
        assert_eq!(s.to_musicians, 6);
        assert_eq!(s.step_secs, 120);
        assert_eq!(s.peers.len(), 2);
        assert!(s.peers.iter().all(|p| p.loss_pct == 0.5 && p.voice.is_some()));
        assert_eq!(s.send_voice_channel, Some(1), "canal 2 → index 1");
        assert_eq!(s.channel_index, Some(0));
        assert_eq!(out, Some(PathBuf::from("r")));
        assert!(!s.no_mmcss, "MMCSS par défaut");
        s.validate().unwrap();
    }

    /// Un scénario de la bibliothèque se lance par son nom, et les options le
    /// modifient comme un fichier.
    #[test]
    fn un_scenario_nomme_se_lance_et_se_modifie() {
        let (s, _, _) = parse_run(&args("--named 9-reseaux-mixtes --seed 7")).unwrap();
        assert_eq!((s.name.as_str(), s.seed, s.to_musicians), ("9-reseaux-mixtes", 7, 9));
        assert_eq!(s.peer(9).name, "4g");
        let (s, _, _) = parse_run(&args("--profile wifi-charge")).unwrap();
        assert_eq!(s.peer(2).name, "wifi-charge");
    }

    #[test]
    fn l_emetteur_distant_se_choisit_et_exclut_le_relais() {
        let (s, _, _) = parse_run(&args("--named regulier-9 --remote 192.168.1.20:51901")).unwrap();
        assert_eq!(s.remote.as_deref(), Some("192.168.1.20:51901"));
        s.validate().unwrap();
        let (s, _, _) = parse_run(&args("--remote 192.168.1.20:51901 --relay 192.168.1.20:51900")).unwrap();
        assert!(s.validate().unwrap_err().contains("pas les deux"));
        assert!(listen_options(&args("--port 1 --listen pas-une-ip"), 2).unwrap_err().contains("--listen"));
        assert_eq!(listen_options(&args("--listen 10.0.0.2 --port 4000"), 2).unwrap(), ("10.0.0.2".parse().unwrap(), 4000));
    }

    #[test]
    fn no_mmcss_se_pose_sans_valeur() {
        let (s, _, _) = parse_run(&args("--no-mmcss --to 3")).unwrap();
        assert!(s.no_mmcss);
        assert_eq!(s.to_musicians, 3, "l'option suivante reste lue");
    }

    #[test]
    fn une_option_fausse_est_refusee_avec_son_nom() {
        assert!(parse_run(&args("--to")).unwrap_err().contains("--to"));
        assert!(parse_run(&args("--profile fibree")).unwrap_err().contains("wifi-charge"));
        assert!(parse_run(&args("--named inconnu")).unwrap_err().contains("9-reseaux-mixtes"));
        assert!(parse_run(&args("--named regulier-9 --scenario x.json")).unwrap_err().contains("pas les deux"));
        assert!(parse_run(&args("--send-voice 0")).is_err());
        assert!(parse_run(&args("--bidule")).unwrap_err().contains("--bidule"));
    }
}
