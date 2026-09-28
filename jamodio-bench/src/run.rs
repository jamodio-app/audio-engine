//! Le déroulé d'une campagne : connexion à l'agent, capture, musiciens simulés
//! ajoutés palier par palier, relevé chaque seconde, arrêt propre, résultats.
//!
//! Les résultats sont écrits dans TOUS les cas — fin normale, Ctrl-C, erreur de
//! l'agent — et un arrêt avant la fin est dit en tête du résumé : une campagne
//! interrompue garde ce qu'elle a mesuré, sans se faire passer pour complète.

use crate::driver::{AgentLink, CaptureParams, StreamParams, VoiceParams};
use crate::report::{self, MachineRow, PeerRow, StepSummary};
use crate::scenario::Scenario;
use crate::server::{Downlink, Kind, Payloads, SenderLoop, Uplink};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Un palier : ses bornes dans le temps de la campagne.
#[derive(Debug, Clone, Copy)]
struct Step {
    musicians: u32,
    start_s: f64,
    end_s: f64,
}

/// Comment la campagne s'est terminée.
enum End {
    Complete,
    Interrupted,
    AgentError(String),
}

pub async fn run(scenario: Scenario, out_dir: &Path) -> Result<PathBuf, String> {
    scenario.validate()?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("dossier {} : {e}", out_dir.display()))?;

    println!("Préparation des trames Opus…");
    let payloads = (Payloads::encode(220.0)?, Payloads::encode(330.0)?);
    let mut sender = SenderLoop::start(payloads.0, payloads.1);
    let stop = Arc::new(AtomicBool::new(false));
    let mut listeners = Vec::new();

    println!("Connexion à l'Audio Engine ({})…", scenario.agent_url);
    let mut agent = AgentLink::connect(&scenario.agent_url).await?;
    let agent_version = agent.hello["agentVersion"].as_str().unwrap_or("?").to_string();
    let agent_os = format!(
        "{}/{}",
        agent.hello["os"].as_str().unwrap_or("?"),
        agent.hello["arch"].as_str().unwrap_or("?")
    );
    println!("Audio Engine {agent_version} ({agent_os}) — un studio ouvert vient d'être déconnecté.");

    // Périphériques : ceux du scénario, sinon ceux par défaut — dit à l'écran.
    let devices = agent.devices().await?;
    let input = pick_device(&devices["inputs"], scenario.input_device.as_deref(), "entrée")?;
    let output = pick_device(&devices["outputs"], scenario.output_device.as_deref(), "sortie")?;
    agent.select_devices(Some(&input), Some(&output))?;
    println!("Entrée : {input} — sortie : {output}");

    // Capture instrument, envoyée au transport montant du banc.
    let ip = scenario.server_ip.as_str();
    let up_instrument = Uplink::bind(ip)?;
    let keys = agent
        .start_capture(&CaptureParams {
            ssrc: 0x4A4D_0001,
            server_ip: ip,
            server_port: up_instrument.port(),
            input_device: Some(&input),
            channel_index: scenario.channel_index,
            server_keys: &up_instrument.server_keys,
        })
        .await?;
    up_instrument.set_agent_keys(&keys)?;
    listeners.push(spawn_listen(up_instrument.clone(), stop.clone()));

    let up_voice = match scenario.send_voice_channel {
        Some(channel) => {
            let up = Uplink::bind(ip)?;
            let keys = agent
                .start_voice(&VoiceParams {
                    ssrc: 0x4A4D_0002,
                    server_ip: ip,
                    server_port: up.port(),
                    channel_index: channel,
                    server_keys: &up.server_keys,
                })
                .await?;
            up.set_agent_keys(&keys)?;
            listeners.push(spawn_listen(up.clone(), stop.clone()));
            Some(up)
        }
        None => None,
    };

    // Ctrl-C : on arrête proprement ET on écrit ce qu'on a.
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let interrupted = interrupted.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                interrupted.store(true, Ordering::Relaxed);
            }
        });
    }

    let t0 = Instant::now();
    let mut names: HashMap<String, (String, bool)> = HashMap::new();
    let mut producers: Vec<String> = Vec::new();
    let mut peer_rows: Vec<PeerRow> = Vec::new();
    let mut machine_rows: Vec<MachineRow> = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut end = End::Complete;

    'campaign: for musicians in scenario.from_musicians..=scenario.to_musicians {
        // Musiciens à ajouter pour atteindre ce palier (tous au premier).
        let first_new = if musicians == scenario.from_musicians { 2 } else { musicians };
        for m in first_new..=musicians {
            let profile = scenario.peer(m).clone();
            let mut kinds = vec![Kind::Instrument];
            if profile.voice.is_some() {
                kinds.push(Kind::Voice);
            }
            for kind in kinds {
                let voice = kind == Kind::Voice;
                let pid = format!("bench-m{m}{}", if voice { "-voix" } else { "" });
                let link = Downlink::bind(ip, pid.clone(), kind, scenario.seed.wrapping_add(u64::from(m)))?;
                let keys = agent
                    .add_stream(&StreamParams {
                        producer_id: &pid,
                        peer_id: &format!("bench-{m}"),
                        server_ip: ip,
                        server_port: link.port(),
                        voice,
                        server_keys: &link.server_keys,
                    })
                    .await?;
                link.set_agent_keys(&keys)?;
                sender.add(link, &profile, scenario.seed.wrapping_add(u64::from(m)));
                names.insert(pid.clone(), (format!("m{m}-{}{}", profile.name, if voice { "-voix" } else { "" }), voice));
                producers.push(pid);
            }
        }
        let start_s = t0.elapsed().as_secs_f64();
        steps.push(Step { musicians, start_s, end_s: start_s });
        println!("── {musicians} musiciens ({} flux reçus)", producers.len());

        let step_end = Instant::now() + Duration::from_secs(scenario.step_secs);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await;
        while Instant::now() < step_end {
            tick.tick().await;
            let t = t0.elapsed().as_secs_f64();
            let mut last_perf: Option<Value> = None;
            while let Ok(p) = agent.perf.try_recv() {
                last_perf = Some(p);
            }
            if let Some(p) = &last_perf {
                peer_rows.extend(report::peer_rows(p, t, musicians, &names));
            }
            let row = report::machine_row(
                last_perf.as_ref(),
                t,
                musicians,
                &sender.take_window(),
                up_instrument.take_window(),
                up_voice.as_ref().map(|u| u.take_window()),
            );
            progress(musicians, t - start_s, &row, &peer_rows);
            machine_rows.push(row);
            if let Some(last) = steps.last_mut() {
                last.end_s = t;
            }
            if let Ok(e) = agent.errors.try_recv() {
                end = End::AgentError(e);
                break 'campaign;
            }
            if interrupted.load(Ordering::Relaxed) {
                end = End::Interrupted;
                break 'campaign;
            }
        }
    }

    // Arrêt : flux retirés, capture arrêtée, fils du banc rendus.
    for pid in &producers {
        let _ = agent.remove_stream(pid);
    }
    let _ = agent.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.store(true, Ordering::Relaxed);
    drop(sender);
    for l in listeners {
        let _ = l.join();
    }

    // Résumé : seuls les paliers mesurés au-delà de leur installation.
    let summaries: Vec<StepSummary> = steps
        .iter()
        .filter(|s| s.end_s - s.start_s > scenario.warmup_secs as f64)
        .map(|s| report::summarize(&peer_rows, &machine_rows, s.musicians, s.start_s + scenario.warmup_secs as f64, s.end_s))
        .collect();
    let criteria = report::criteria(&summaries, scenario.is_local_regular(), scenario.send_voice_channel.is_some());
    let status = match &end {
        End::Complete => "complète".to_string(),
        End::Interrupted => "INTERROMPUE (Ctrl-C) — paliers partiels".to_string(),
        End::AgentError(e) => format!("ARRÊTÉE PAR L'AUDIO ENGINE : {e}"),
    };
    let header = vec![
        ("Campagne".to_string(), status),
        ("Scénario".to_string(), scenario.name.clone()),
        ("Audio Engine".to_string(), format!("{agent_version} ({agent_os})")),
        ("Machine".to_string(), machine_name()),
        ("Entrée / sortie".to_string(), format!("{input} / {output}")),
        (
            "Mode".to_string(),
            if scenario.is_local_regular() {
                "local, flux réguliers : tout trou est de cause locale".into()
            } else {
                format!("serveur {} ; profils : {}", scenario.server_ip, profile_list(&scenario))
            },
        ),
        ("Talkback envoyé".to_string(), scenario.send_voice_channel.map_or("non".into(), |c| format!("canal {}", c + 1))),
        ("Fichiers".to_string(), "peers.csv (flux, 1 ligne/s), machine.csv (machine et faux serveur), scenario.json".into()),
        ("Journal de l'Audio Engine".to_string(), "à joindre (lignes TROU, perfstats)".into()),
    ];
    write(out_dir, "peers.csv", &report::peers_csv(&peer_rows))?;
    write(out_dir, "machine.csv", &report::machine_csv(&machine_rows))?;
    write(out_dir, "scenario.json", &serde_json::to_string_pretty(&scenario).map_err(|e| e.to_string())?)?;
    let summary = report::markdown(&header, &summaries, &criteria);
    let path = out_dir.join("resume.md");
    write(out_dir, "resume.md", &summary)?;
    println!("\n{summary}");
    match end {
        End::AgentError(e) => Err(format!("campagne arrêtée par l'Audio Engine : {e} (résultats partiels dans {})", out_dir.display())),
        _ => Ok(path),
    }
}

fn spawn_listen(up: Arc<Uplink>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || up.listen(stop))
}

/// Le périphérique demandé (tel quel, l'agent le vérifie strictement), sinon
/// celui par défaut. Aucun défaut → erreur explicite, pas de choix au hasard.
fn pick_device(list: &Value, wanted: Option<&str>, what: &str) -> Result<String, String> {
    if let Some(w) = wanted {
        return Ok(w.to_string());
    }
    list.as_array()
        .and_then(|l| l.iter().find(|d| d["isDefault"].as_bool() == Some(true)))
        .and_then(|d| d["id"].as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("aucun périphérique de {what} par défaut : le préciser (session-bench devices)"))
}

fn profile_list(s: &Scenario) -> String {
    s.peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
}

fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "machine".into())
}

fn write(dir: &Path, name: &str, content: &str) -> Result<(), String> {
    std::fs::write(dir.join(name), content).map_err(|e| format!("écriture de {name} : {e}"))
}

/// Une ligne par seconde à l'écran : de quoi voir la campagne vivre.
fn progress(musicians: u32, t_step: f64, m: &MachineRow, peers: &[PeerRow]) {
    // Le dernier relevé de chaque flux (même seconde).
    let last_t = peers.last().map_or(f64::NAN, |r| r.t_s);
    let latest = peers.iter().rev().take_while(|r| r.t_s == last_t);
    let (mut underruns, mut targets) = (0.0, Vec::new());
    for r in latest.filter(|r| !r.voice) {
        underruns += r.get("underruns").max(0.0);
        targets.push(r.get("bufferTargetMs"));
    }
    let target = targets.iter().copied().filter(|v| v.is_finite()).fold(f64::NAN, f64::max);
    println!(
        "[{musicians} mus. {:>4.0} s] trous cumulés {:>4.0} | cible max {:>5.1} ms | CPU {:>5.1} % | banc en retard max {:>5.2} ms",
        t_step, underruns, target, m.cpu_pct, m.sender_late_max_ms
    );
}
