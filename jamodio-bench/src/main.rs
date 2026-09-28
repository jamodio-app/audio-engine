//! `session-bench` — le banc « N musiciens » en ligne de commande.
//!
//! ```text
//! session-bench devices                     # périphériques vus par l'Audio Engine
//! session-bench run                         # 2 → 9 musiciens, 5 min par palier, flux réguliers
//! session-bench run --profile ethernet --to 6 --step-secs 120
//! session-bench run --profiles regular,wifi --peer-voice bursts --send-voice 1
//! session-bench run --scenario mon-test.json
//! session-bench scenario > mon-test.json    # scénario par défaut, à modifier
//! session-bench selftest 8 30               # précision du banc seul, avant une campagne
//! ```
//!
//! L'Audio Engine doit tourner (pré-version ≥ 0.6.6-1 pour la cause des trous) ;
//! le studio ouvert dans le navigateur sera déconnecté pendant le banc.

use jamodio_bench::driver::AgentLink;
use jamodio_bench::profile::{PeerProfile, Speech};
use jamodio_bench::scenario::Scenario;
use std::path::PathBuf;

const USAGE: &str = "\
session-bench — banc « N musiciens » contre l'Audio Engine installé

  session-bench devices
  session-bench plugins                        (plugins connus de l'Audio Engine)
  session-bench relay [--listen IP] [--port N] (SECONDE machine : relais du mode réseau)
  session-bench scenario                       (écrit le scénario par défaut en JSON)
  session-bench selftest [FLUX] [SECONDES]     (précision du banc seul, sans Audio Engine ; défaut 8 flux, 30 s)
  session-bench run [options]

Options de run :
  --scenario FICHIER      part d'un scénario JSON (les options suivantes le modifient)
  --from N / --to N       premier / dernier palier, toi compris (défaut 2 → 9)
  --step-secs S           durée d'un palier (défaut 300)
  --warmup-secs S         installation exclue de l'analyse (défaut 30)
  --profile P             même profil pour tous : regular | ethernet | wifi
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
  --out DOSSIER           où écrire les résultats (défaut bench-results/<date>)
  --save-scenario FICHIER écrit le scénario final avant de lancer
";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("devices") => devices(&Scenario::default().agent_url).await,
        Some("plugins") => plugins(&Scenario::default().agent_url).await,
        Some("relay") => relay(&args[1..]),
        Some("scenario") => match serde_json::to_string_pretty(&Scenario::default()) {
            Ok(j) => {
                println!("{j}");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        Some("run") => run(&args[1..]).await,
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

/// La seconde machine du mode réseau : relaie les flux entre le banc et
/// l'agent de la machine mesurée.
fn relay(args: &[String]) -> Result<(), String> {
    let mut listen = None;
    let mut port = jamodio_bench::relay::DEFAULT_PORT;
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

/// Lit les options de `run` (sans dépendance : une vingtaine d'options suffit).
fn parse_run(args: &[String]) -> Result<(Scenario, Option<PathBuf>, Option<PathBuf>), String> {
    // Le scénario de départ d'abord : les autres options le modifient.
    let mut scenario = match args.iter().position(|a| a == "--scenario") {
        Some(i) => {
            let path = args.get(i + 1).ok_or("--scenario attend un fichier")?;
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path} : {e}"))?;
            serde_json::from_str(&text).map_err(|e| format!("{path} : {e}"))?
        }
        None => Scenario::default(),
    };
    let (mut out, mut save) = (None, None);
    let mut it = args.iter();
    while let Some(opt) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{opt} attend une valeur"));
        let num = |v: String| v.parse::<u64>().map_err(|_| format!("{opt} : nombre attendu, reçu {v}"));
        match opt.as_str() {
            "--scenario" => {
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
                    .map(|n| PeerProfile::preset(n.trim()).ok_or(format!("profil inconnu : {n} (regular | ethernet | wifi)")))
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
            "--out" => out = Some(PathBuf::from(val()?)),
            "--save-scenario" => save = Some(PathBuf::from(val()?)),
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
        s.validate().unwrap();
    }

    #[test]
    fn une_option_fausse_est_refusee_avec_son_nom() {
        assert!(parse_run(&args("--to")).unwrap_err().contains("--to"));
        assert!(parse_run(&args("--profile fibre")).unwrap_err().contains("fibre"));
        assert!(parse_run(&args("--send-voice 0")).is_err());
        assert!(parse_run(&args("--bidule")).unwrap_err().contains("--bidule"));
    }
}
