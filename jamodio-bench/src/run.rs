//! Le déroulé d'une campagne : connexion à l'agent, capture, musiciens simulés
//! ajoutés palier par palier, relevé chaque seconde, arrêt propre, résultats.
//!
//! Les résultats sont écrits dans TOUS les cas — fin normale, Ctrl-C, erreur de
//! l'agent — et un arrêt avant la fin est dit en tête du résumé : une campagne
//! interrompue garde ce qu'elle a mesuré, sans se faire passer pour complète.

use crate::driver::{AgentLink, CaptureParams, StreamParams, VoiceParams};
use crate::report::{self, MachineRow, PeerRow, StepSummary};
use crate::scenario::Scenario;
use crate::relay::RelayClient;
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
    let server_ip = match scenario.server_ip.as_str() {
        "auto" => crate::scenario::primary_local_ip()?.to_string(),
        ip => ip.to_string(),
    };
    let ip = server_ip.as_str();
    println!("Faux serveur sur {ip} (cette machine)");
    // Mode relais : l'agent joint le RELAIS (seconde machine), qui renvoie au
    // banc ; sinon il joint directement le banc.
    let mut relay = match scenario.relay.as_deref() {
        Some(addr) => {
            let r = RelayClient::connect(addr).await?;
            println!("Relais {addr} : les flux passent par le réseau.");
            Some(r)
        }
        None => None,
    };
    let agent_ip = relay.as_ref().map_or(server_ip.clone(), |r| r.ip.to_string());
    let up_instrument = Uplink::bind(ip)?;
    let up_port = agent_port(&mut relay, ip, up_instrument.port()).await?;
    let keys = agent
        .start_capture(&CaptureParams {
            ssrc: 0x4A4D_0001,
            server_ip: &agent_ip,
            server_port: up_port,
            input_device: Some(&input),
            channel_index: scenario.channel_index,
            server_keys: &up_instrument.server_keys,
        })
        .await?;
    up_instrument.set_agent_keys(&keys)?;
    listeners.push(spawn_listen(up_instrument.clone(), stop.clone()));

    // Plugin inséré : la charge réelle du musicien, dans l'Audio Engine.
    let plugin_line = match scenario.plugin.as_deref() {
        None => "aucun".to_string(),
        Some(name) => {
            let items = agent.plugins().await?;
            let plugin_ref = pick_plugin(&items, name)?;
            let loaded = agent.load_plugin(&plugin_ref).await?;
            let line = format!(
                "{} (latence déclarée {} échantillons)",
                loaded["name"].as_str().unwrap_or(name),
                loaded["latencySamples"]
            );
            println!("Plugin chargé : {line}");
            line
        }
    };

    let up_voice = match scenario.send_voice_channel {
        Some(channel) => {
            let up = Uplink::bind(ip)?;
            let port = agent_port(&mut relay, ip, up.port()).await?;
            let keys = agent
                .start_voice(&VoiceParams {
                    ssrc: 0x4A4D_0002,
                    server_ip: &agent_ip,
                    server_port: port,
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
    let mut warned_no_holes = false;

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
                let port = agent_port(&mut relay, ip, link.port()).await?;
                let keys = agent
                    .add_stream(&StreamParams {
                        producer_id: &pid,
                        peer_id: &format!("bench-{m}"),
                        server_ip: &agent_ip,
                        server_port: port,
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
                let rows = report::peer_rows(p, t, musicians, &names);
                if !warned_no_holes && rows.iter().any(|r| r.get("holesArrival").is_nan()) {
                    // Le numéro de version ne suffit pas à le savoir (la 0.6.6-2
                    // est plus récente que la -1 sans en avoir les mesures).
                    println!("⚠ Cet Audio Engine ne mesure pas la cause des trous : installer la pré-version 0.6.6-5 ou plus récente. Le banc continue, ces colonnes resteront vides.");
                    warned_no_holes = true;
                }
                peer_rows.extend(rows);
            }
            let relay_delay = match relay.as_mut() {
                Some(r) => {
                    let st = r.stats().await?;
                    Some((f64::from(st.delay_p99_us) / 1000.0, f64::from(st.delay_max_us) / 1000.0))
                }
                None => None,
            };
            let row = report::machine_row(
                last_perf.as_ref(),
                t,
                musicians,
                &sender.take_window(),
                up_instrument.take_window(),
                up_voice.as_ref().map(|u| u.take_window()),
                relay_delay,
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
    for l in listeners {
        let _ = l.join();
    }

    // Résumé : seuls les paliers mesurés au-delà de leur installation.
    let summaries: Vec<StepSummary> = steps
        .iter()
        .filter(|s| s.end_s - s.start_s > scenario.warmup_secs as f64)
        .map(|s| report::summarize(&peer_rows, &machine_rows, s.musicians, s.start_s + scenario.warmup_secs as f64, s.end_s))
        .collect();
    let criteria = report::criteria(&summaries, scenario.is_regular(), scenario.send_voice_channel.is_some());
    let status = match &end {
        End::Complete => "complète".to_string(),
        End::Interrupted => "INTERROMPUE (Ctrl-C) — paliers partiels".to_string(),
        End::AgentError(e) => format!("ARRÊTÉE PAR L'AUDIO ENGINE : {e}"),
    };
    let (precision, _) = report::precision(&machine_rows.iter().map(|r| r.sender_late_max_ms).collect::<Vec<_>>());
    let priority = match sender.priority.get() {
        Some(Ok(p)) => p.to_string(),
        Some(Err(e)) => format!("NORMALE — promotion refusée : {e}"),
        None => "inconnue".into(),
    };
    let relay_precision = match &scenario.relay {
        Some(addr) => {
            let (p, _) = report::precision(&machine_rows.iter().map(|r| r.relay_delay_max_ms).collect::<Vec<_>>());
            format!("{addr} — {p}")
        }
        None => "aucun (mode local)".into(),
    };
    let header = vec![
        ("Campagne".to_string(), status),
        ("Précision du banc".to_string(), precision),
        ("Relais (réseau)".to_string(), relay_precision),
        ("Priorité des fils du banc".to_string(), priority),
        ("Scénario".to_string(), scenario.name.clone()),
        ("Audio Engine".to_string(), format!("{agent_version} ({agent_os})")),
        ("Machine".to_string(), machine_name()),
        ("Entrée / sortie".to_string(), format!("{input} / {output}")),
        (
            "Mode".to_string(),
            format!(
                "{}, {}",
                if scenario.relay.is_some() { "réseau (relais)" } else { "local" },
                if scenario.is_regular() {
                    "flux réguliers".to_string()
                } else {
                    format!("gigue/pertes simulées ; profils : {}", profile_list(&scenario))
                }
            ),
        ),
        ("Plugin inséré".to_string(), plugin_line),
        ("Talkback envoyé".to_string(), scenario.send_voice_channel.map_or("non".into(), |c| format!("canal {}", c + 1))),
        ("Fichiers".to_string(), "peers.csv (flux, 1 ligne/s), machine.csv (machine et faux serveur), scenario.json".into()),
        ("Journal de l'Audio Engine".to_string(), "à joindre (lignes TROU, perfstats)".into()),
    ];
    // Le dossier n'est créé qu'ici : une campagne qui n'a jamais démarré ne
    // laisse pas de dossier vide qu'on prendrait pour un résultat.
    std::fs::create_dir_all(out_dir).map_err(|e| format!("dossier {} : {e}", out_dir.display()))?;
    write(out_dir, "peers.csv", &report::peers_csv(&peer_rows))?;
    write(out_dir, "machine.csv", &report::machine_csv(&machine_rows))?;
    write(out_dir, "scenario.json", &serde_json::to_string_pretty(&scenario).map_err(|e| e.to_string())?)?;
    drop(sender);
    let summary = report::markdown(&header, &summaries, &criteria);
    let path = out_dir.join("resume.md");
    write(out_dir, "resume.md", &summary)?;
    println!("\n{summary}");
    match end {
        End::AgentError(e) => Err(format!("campagne arrêtée par l'Audio Engine : {e} (résultats partiels dans {})", out_dir.display())),
        _ => Ok(path),
    }
}

/// Le port que l'agent doit joindre pour ce transport du banc : celui du relais
/// en mode réseau, le sien sinon.
async fn agent_port(relay: &mut Option<RelayClient>, bind_ip: &str, bench_port: u16) -> Result<u16, String> {
    match relay {
        Some(r) => {
            let bench: std::net::SocketAddr = format!("{bind_ip}:{bench_port}").parse().map_err(|e| format!("{e}"))?;
            r.open(bench).await
        }
        None => Ok(bench_port),
    }
}

fn spawn_listen(up: Arc<Uplink>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || up.listen(stop))
}

/// Le périphérique demandé, s'il figure EXACTEMENT dans la liste de l'agent
/// (format strict `idx:nom`), sinon celui par défaut. Aucun défaut, ou un
/// identifiant inconnu → erreur qui cite les identifiants valides : pas de
/// choix au hasard, et pas d'aller-retour avec l'agent pour un identifiant faux
/// (NUC, 28/09 : « 1 » au lieu de « 1:UMC ASIO Driver »).
fn pick_device(list: &Value, wanted: Option<&str>, what: &str) -> Result<String, String> {
    if let Some(w) = wanted {
        let ids: Vec<&str> = list.as_array().into_iter().flatten().filter_map(|d| d["id"].as_str()).collect();
        if ids.contains(&w) {
            return Ok(w.to_string());
        }
        return Err(format!("{what} « {w} » inconnue. Identifiants exacts : {}", ids.join(" | ")));
    }
    list.as_array()
        .and_then(|l| l.iter().find(|d| d["isDefault"].as_bool() == Some(true)))
        .and_then(|d| d["id"].as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("aucun périphérique de {what} par défaut : le préciser (session-bench devices)"))
}

/// Le plugin nommé, par son nom EXACT (sans tenir compte des majuscules).
/// Absent ou ambigu : erreur qui cite les plugins connus — pas d'à-peu-près.
fn pick_plugin(items: &[Value], name: &str) -> Result<Value, String> {
    let found: Vec<&Value> = items
        .iter()
        .filter(|p| p["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .collect();
    match found.as_slice() {
        [one] => Ok(one["pluginRef"].clone()),
        [] => {
            let names: Vec<&str> = items.iter().filter_map(|p| p["name"].as_str()).collect();
            Err(format!("plugin « {name} » introuvable. Connus : {}", names.join(", ")))
        }
        _ => Err(format!("plusieurs plugins s'appellent « {name} » : préciser (session-bench plugins)")),
    }
}

fn profile_list(s: &Scenario) -> String {
    s.peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// Nom de la machine (`hostname` existe sous macOS comme sous Windows).
fn machine_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "inconnue".into())
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

/// Mesure la précision du BANC SEUL, sans Audio Engine : `streams` flux
/// réguliers envoyés sur la machine à des récepteurs du banc, pendant `secs`
/// secondes. À lancer avant une campagne : si le banc n'est pas assez précis
/// sur cette machine, la campagne ne pourra pas conclure.
pub fn selftest(streams: u32, secs: u64) -> Result<bool, String> {
    use crate::profile::PeerProfile;
    use jamodio_audio_core::net::srtp::SrtpParameters;
    use std::net::UdpSocket;

    let mut sender = SenderLoop::start(Payloads::encode(220.0)?, Payloads::encode(330.0)?);
    let stop = Arc::new(AtomicBool::new(false));
    let gaps: Arc<std::sync::Mutex<Vec<f64>>> = Arc::default();
    let mut receivers = Vec::new();
    for i in 0..streams {
        let link = Downlink::bind("127.0.0.1", format!("selftest-{i}"), Kind::Instrument, u64::from(i))?;
        link.set_agent_keys(&SrtpParameters::generate_aead_aes_256_gcm())?;
        let rx = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        rx.set_read_timeout(Some(Duration::from_millis(200))).map_err(|e| e.to_string())?;
        // Le « perçage » : un en-tête RTP suffit au faux serveur pour apprendre l'adresse.
        let mut punch = [0u8; 12];
        punch[0] = 0x80;
        punch[1] = crate::server::PAYLOAD_TYPE;
        rx.send_to(&punch, ("127.0.0.1", link.port())).map_err(|e| e.to_string())?;
        sender.add(link, &PeerProfile::preset("regular").expect("préréglage"), u64::from(i));
        let (stop, gaps) = (stop.clone(), gaps.clone());
        receivers.push(std::thread::spawn(move || {
            let _ = crate::rt::promote_current_thread();
            let mut buf = [0u8; 2048];
            let mut last: Option<Instant> = None;
            let mut worst = 0.0f64;
            while !stop.load(Ordering::Relaxed) {
                if rx.recv_from(&mut buf).is_ok() {
                    let now = Instant::now();
                    if let Some(l) = last {
                        worst = worst.max(now.duration_since(l).as_secs_f64() * 1000.0);
                    }
                    last = Some(now);
                }
            }
            gaps.lock().unwrap().push(worst);
        }));
    }
    let mut maxima = Vec::new();
    for s in 1..=secs {
        std::thread::sleep(Duration::from_secs(1));
        let w = sender.take_window();
        let mut late = w.late_us.clone();
        late.sort_unstable();
        let pct = |p: f64| late.get(((late.len().max(1) - 1) as f64 * p).round() as usize).map_or(f64::NAN, |&v| v as f64 / 1000.0);
        let max = pct(1.0);
        maxima.push(max);
        println!("[{s:>3} s] {} paquets | retard d'envoi p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms", w.sent, pct(0.5), pct(0.99), max);
    }
    stop.store(true, Ordering::Relaxed);
    for r in receivers {
        let _ = r.join();
    }
    let priority = match sender.priority.get() {
        Some(Ok(p)) => p.to_string(),
        Some(Err(e)) => format!("NORMALE — promotion refusée : {e}"),
        None => "inconnue".into(),
    };
    let (verdict, ok) = report::precision(&maxima);
    let worst_gap = gaps.lock().unwrap().iter().copied().fold(0.0, f64::max);
    println!("\nPriorité des fils du banc : {priority}");
    println!("Plus grand écart entre deux paquets reçus (2,5 ms attendus) : {worst_gap:.2} ms");
    println!("Précision du banc : {verdict}");
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn liste() -> Vec<Value> {
        vec![
            json!({ "name": "AmpliTube 5", "pluginRef": { "format": "vst3", "path": "a", "uid": "1" } }),
            json!({ "name": "Reverb", "pluginRef": { "format": "vst3", "path": "b", "uid": "2" } }),
            json!({ "name": "Reverb", "pluginRef": { "format": "vst3", "path": "c", "uid": "3" } }),
        ]
    }

    #[test]
    fn un_peripherique_se_designe_par_son_identifiant_exact() {
        let l = json!([
            { "id": "0:ASIO4ALL v2", "isDefault": false },
            { "id": "1:UMC ASIO Driver", "isDefault": true },
        ]);
        assert_eq!(pick_device(&l, None, "entrée").unwrap(), "1:UMC ASIO Driver");
        assert_eq!(pick_device(&l, Some("0:ASIO4ALL v2"), "entrée").unwrap(), "0:ASIO4ALL v2");
        let e = pick_device(&l, Some("1"), "entrée").unwrap_err();
        assert!(e.contains("« 1 » inconnue") && e.contains("1:UMC ASIO Driver"), "{e}");
    }

    #[test]
    fn un_plugin_se_choisit_par_son_nom_exact() {
        assert_eq!(pick_plugin(&liste(), "amplitube 5").unwrap()["uid"], "1");
        let e = pick_plugin(&liste(), "AmpliTube").unwrap_err();
        assert!(e.contains("introuvable") && e.contains("AmpliTube 5"), "{e}");
        assert!(pick_plugin(&liste(), "Reverb").unwrap_err().contains("plusieurs"));
    }
}
