//! Le déroulé d'une campagne : connexion à l'agent, capture, musiciens simulés
//! ajoutés palier par palier, relevé chaque seconde, arrêt propre, résultats.
//!
//! Les résultats sont écrits dans TOUS les cas — fin normale, Ctrl-C, erreur de
//! l'agent — et un arrêt avant la fin est dit en tête du résumé : une campagne
//! interrompue garde ce qu'elle a mesuré, sans se faire passer pour complète.

use crate::driver::{AgentLink, CaptureParams, StreamParams, VoiceParams};
use crate::endpoint::Endpoint;
use crate::profile::PeerProfile;
use crate::report::{self, Event, EventKind, MachineRow, PeerRow, StreamInfo};
use crate::scenario::Scenario;
use crate::server::{Downlink, Kind, Payloads, SenderLoop};
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
    /// Erreur du banc lui-même pendant la campagne (ajout d'un flux, relais…) :
    /// la sortie propre (flux, plugin, capture) a lieu quand même.
    BenchError(String),
}

pub async fn run(scenario: Scenario, out_dir: &Path) -> Result<PathBuf, String> {
    scenario.validate()?;

    // Avant tout fil du banc : leur promotion en dépend.
    crate::rt::set_without_mmcss(scenario.no_mmcss);

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

    // Où vivent les flux : sur l'émetteur distant (seconde machine), ou sur
    // cette machine (en direct, ou à travers le relais).
    let mut endpoint = match scenario.remote.as_deref() {
        Some(addr) => Endpoint::remote(addr).await?,
        None => {
            let ip = match scenario.server_ip.as_str() {
                "auto" => crate::scenario::primary_local_ip()?.to_string(),
                ip => ip.to_string(),
            };
            println!("Faux serveur sur {ip} (cette machine) — préparation des trames Opus…");
            Endpoint::local(ip, scenario.relay.as_deref()).await?
        }
    };
    let agent_ip = endpoint.agent_ip();

    // Capture instrument, envoyée au transport montant du banc.
    let up = endpoint.open_uplink(false).await?;
    let keys = agent
        .start_capture(&CaptureParams {
            ssrc: 0x4A4D_0001,
            server_ip: &agent_ip,
            server_port: up.port,
            input_device: Some(&input),
            channel_index: scenario.channel_index,
            server_keys: &up.keys,
        })
        .await?;
    endpoint.uplink_keys(up.id, &keys).await?;

    if let Some(channel) = scenario.send_voice_channel {
        let up = endpoint.open_uplink(true).await?;
        let keys = agent
            .start_voice(&VoiceParams {
                ssrc: 0x4A4D_0002,
                server_ip: &agent_ip,
                server_port: up.port,
                channel_index: channel,
                server_keys: &up.keys,
            })
            .await?;
        endpoint.uplink_keys(up.id, &keys).await?;
    }

    // Plugin inséré : la charge réelle du musicien, dans l'Audio Engine.
    // Chargé en DERNIER avant la campagne : toute erreur de préparation qui
    // précède ne peut pas laisser un plugin inséré derrière le banc.
    let mut plugin_loaded = false;
    let mut plugin_line = match scenario.plugin.as_deref() {
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
            plugin_loaded = true;
            line
        }
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
    let mut streams: HashMap<String, StreamInfo> = HashMap::new();
    // Transport de chaque flux, pour couper ceux d'un musicien qui part.
    let mut links: HashMap<String, u32> = HashMap::new();
    let mut producers: Vec<String> = Vec::new();
    let mut peer_rows: Vec<PeerRow> = Vec::new();
    let mut machine_rows: Vec<MachineRow> = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    // Instant d'arrivée de chaque musicien (s de campagne) : ses changements de
    // lien et ses absences se comptent depuis là.
    let mut arrivals: HashMap<u32, f64> = HashMap::new();
    // Départs et retours à venir : (instant en s de campagne, musicien, départ ?).
    let mut pending: Vec<(f64, u32, bool)> = Vec::new();
    let mut warned_no_holes = false;
    let ctx = StreamCtx { scenario: &scenario, agent_ip: &agent_ip };

    // La campagne dans un bloc : une erreur du banc (`?`) en sort sans sauter
    // la sortie propre qui suit — flux retirés, plugin rendu, capture arrêtée.
    let campaign: Result<End, String> = async {
        let mut end = End::Complete;
        'campaign: for musicians in scenario.from_musicians..=scenario.to_musicians {
            // Musiciens à ajouter pour atteindre ce palier (tous au premier).
            let first_new = if musicians == scenario.from_musicians { 2 } else { musicians };
            for m in first_new..=musicians {
                let arrival = t0.elapsed().as_secs_f64();
                arrivals.insert(m, arrival);
                let profile = scenario.peer(m);
                for a in &profile.absences {
                    pending.push((arrival + a.at_s, m, true));
                    pending.push((arrival + a.at_s + a.for_s, m, false));
                }
                events.extend(crate::analysis::musician_events(profile, m, arrival));
                let pids = ctx.add_musician(&mut agent, &mut endpoint, m, 0, 0).await?;
                for (pid, info, id) in pids {
                    streams.insert(pid.clone(), info);
                    links.insert(pid.clone(), id);
                    producers.push(pid);
                }
            }
            pending.sort_by(|a, b| a.0.total_cmp(&b.0));
            let start_s = t0.elapsed().as_secs_f64();
            steps.push(Step { musicians, start_s, end_s: start_s });
            println!("── {musicians} musiciens ({} flux reçus)", producers.len());

            let step_end = Instant::now() + Duration::from_secs(scenario.step_secs);
            let mut next_sample = Instant::now() + Duration::from_secs(1);
            while Instant::now() < step_end {
                // Départs et retours à leur heure, entre deux relevés.
                let next_event = pending.first().map(|(t, _, _)| t0 + Duration::from_secs_f64(*t));
                let wake = next_event.map_or(next_sample, |e| e.min(next_sample));
                tokio::time::sleep_until(tokio::time::Instant::from_std(wake)).await;
                while pending.first().is_some_and(|(t, _, _)| t0 + Duration::from_secs_f64(*t) <= Instant::now()) {
                    let (t, m, leaving) = pending.remove(0);
                    let mine: Vec<String> = streams.iter().filter(|(_, i)| i.musician == m).map(|(p, _)| p.clone()).collect();
                    if leaving {
                        for pid in &mine {
                            agent.remove_stream(pid)?;
                            if let Some(id) = links.remove(pid) {
                                endpoint.retire(id).await?;
                            }
                            streams.remove(pid);
                            producers.retain(|p| p != pid);
                        }
                        println!("   m{m} part ({:.0} s)", t);
                    } else {
                        // Un retour = un nouveau flux (nouvel identifiant, numérotation
                        // à zéro), placé dans la frise du musicien à l'heure prévue.
                        let returns = events
                            .iter()
                            .filter(|e| e.musician == m && e.t_s < t && matches!(e.kind, EventKind::Absence { .. }))
                            .count() as u64;
                        let offset_us = ((t - arrivals[&m]) * 1e6).round() as u64;
                        let pids = ctx.add_musician(&mut agent, &mut endpoint, m, returns, offset_us).await?;
                        for (pid, info, id) in pids {
                            streams.insert(pid.clone(), info);
                            links.insert(pid.clone(), id);
                            producers.push(pid);
                        }
                        println!("   m{m} revient ({:.0} s)", t);
                    }
                }
                if Instant::now() < next_sample {
                    continue;
                }
                next_sample += Duration::from_secs(1);
                let t = t0.elapsed().as_secs_f64();
                // Lien en vigueur pour chaque flux cette seconde-là.
                for info in streams.values_mut() {
                    let since = t - arrivals.get(&info.musician).copied().unwrap_or(0.0);
                    info.link = scenario.peer(info.musician).link_name_at(since).to_string();
                }
                let mut last_perf: Option<Value> = None;
                while let Ok(p) = agent.perf.try_recv() {
                    last_perf = Some(p);
                }
                if let Some(p) = &last_perf {
                    let rows = report::peer_rows(p, t, musicians, &streams);
                    if !warned_no_holes && rows.iter().any(|r| r.get("holesArrival").is_nan()) {
                        // Le numéro de version ne suffit pas à le savoir (la 0.6.6-2
                        // est plus récente que la -1 sans en avoir les mesures).
                        println!("⚠ Cet Audio Engine ne mesure pas la cause des trous : installer la pré-version 0.6.6-5 ou plus récente. Le banc continue, ces colonnes resteront vides.");
                        warned_no_holes = true;
                    }
                    peer_rows.extend(rows);
                }
                let w = endpoint.windows().await?;
                let row = report::machine_row(last_perf.as_ref(), t, musicians, &w.sender, w.up_instrument, w.up_voice, w.relay);
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
        Ok(end)
    }
    .await;
    let end = campaign.unwrap_or_else(End::BenchError);

    // Arrêt : flux retirés, plugin rendu, capture arrêtée, fils du banc rendus.
    for pid in &producers {
        let _ = agent.remove_stream(pid);
    }
    if plugin_loaded {
        plugin_line = match agent.unload_plugin().await {
            Ok(()) => {
                println!("Plugin retiré de l'Audio Engine.");
                format!("{plugin_line} — retiré à la fin")
            }
            Err(e) => {
                println!("⚠ Plugin NON retiré ({e}) : le retirer dans le studio (✕) ou quitter l'Audio Engine.");
                format!("{plugin_line} — NON RETIRÉ à la fin ({e}) : le retirer dans le studio ou quitter l'Audio Engine")
            }
        };
    }
    let _ = agent.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let priority = endpoint.priority();
    let remote_host = match &endpoint {
        Endpoint::Remote(r) => Some(r.host.clone()),
        Endpoint::Local(_) => None,
    };
    endpoint.shutdown();

    // Résumé : la même analyse que celle d'un échantillon relu plus tard.
    let raw_steps: Vec<(u32, f64, f64)> = steps.iter().map(|s| (s.musicians, s.start_s, s.end_s)).collect();
    let analysis = crate::analysis::analyze(&scenario, &peer_rows, &machine_rows, &raw_steps, &events);
    let clock_offset = analysis.clock_offset_ppm;
    let status = match &end {
        End::Complete => "complète".to_string(),
        End::Interrupted => "INTERROMPUE (Ctrl-C) — paliers partiels".to_string(),
        End::AgentError(e) => format!("ARRÊTÉE PAR L'AUDIO ENGINE : {e}"),
        End::BenchError(e) => format!("ARRÊTÉE PAR UNE ERREUR DU BANC : {e}"),
    };
    let precision = analysis.precision_text.clone();
    let relay_precision = match (&scenario.relay, &scenario.remote) {
        (Some(addr), _) => {
            let (p, _) = report::precision(&machine_rows.iter().map(|r| r.relay_delay_max_ms).collect::<Vec<_>>());
            format!("{addr} — {p}")
        }
        (None, Some(_)) => "aucun (émetteur distant)".into(),
        (None, None) => "aucun (mode local)".into(),
    };

    let header = vec![
        ("Campagne".to_string(), status),
        ("Précision du banc".to_string(), precision),
        ("Relais (réseau)".to_string(), relay_precision),
        (
            "Émetteur distant".to_string(),
            match (&scenario.remote, &remote_host) {
                (Some(addr), Some(host)) => format!(
                    "{host} ({addr}) — les flux simulés partent de là-bas ; la précision ci-dessus y est mesurée ; écart d'horloge estimé {}",
                    if clock_offset.is_finite() { format!("{clock_offset:+.1} ppm (retiré avant de comparer les dérives)") } else { "inconnu".into() }
                ),
                _ => "aucun".into(),
            },
        ),
        ("Priorité des fils du banc".to_string(), priority),
        ("Freinage réseau Windows (registre)".to_string(), crate::rt::multimedia_profile()),
        ("Scénario".to_string(), scenario.name.clone()),
        ("Audio Engine".to_string(), format!("{agent_version} ({agent_os})")),
        ("Machine".to_string(), machine_name()),
        ("Entrée / sortie".to_string(), format!("{input} / {output}")),
        (
            "Mode".to_string(),
            format!(
                "{}, {}",
                match (&scenario.relay, &scenario.remote) {
                    (Some(_), _) => "réseau (relais)",
                    (None, Some(_)) => "réseau (émetteur distant)",
                    (None, None) => "local",
                },
                if scenario.is_regular() {
                    "flux réguliers".to_string()
                } else {
                    format!("réseau simulé (cf. « Réseau simulé ») ; profils : {}", profile_list(&scenario))
                }
            ),
        ),
        ("Réseau simulé".to_string(), network_line(&scenario)),
        ("Origine des liens".to_string(), origin_line(&scenario)),
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
    let metrics = crate::analysis::Metrics::from_analysis(&scenario, &analysis, matches!(end, End::Complete));
    write(out_dir, "metrics.json", &serde_json::to_string_pretty(&metrics).map_err(|e| e.to_string())?)?;
    let summary = report::markdown(&header, &analysis.summaries, &analysis.musicians, &analysis.events, &analysis.criteria);
    let path = out_dir.join("resume.md");
    write(out_dir, "resume.md", &summary)?;
    println!("\n{summary}");
    match end {
        End::AgentError(e) => Err(format!("campagne arrêtée par l'Audio Engine : {e} (résultats partiels dans {})", out_dir.display())),
        End::BenchError(e) => Err(format!("campagne arrêtée par une erreur du banc : {e} (résultats partiels dans {})", out_dir.display())),
        _ => Ok(path),
    }
}

/// Le périphérique demandé, au format strict `idx:nom`, sinon celui par
/// défaut. Même règle que l'agent (`audio/device.rs::locate`) et le studio :
/// l'index glisse au branchement d'une carte (01/10/2026 : la Focusrite
/// branchée, le micro interne passe de `0:` à `1:`) — si l'identifiant exact
/// n'existe plus, on retrouve le périphérique par son nom EXACT et UNIQUE ;
/// absent → perdu, homonymes → refus. Jamais d'à-peu-près, et le glissement
/// est dit à l'écran.
fn pick_device(list: &Value, wanted: Option<&str>, what: &str) -> Result<String, String> {
    let ids: Vec<&str> = list.as_array().into_iter().flatten().filter_map(|d| d["id"].as_str()).collect();
    if let Some(w) = wanted {
        if ids.contains(&w) {
            return Ok(w.to_string());
        }
        let name = w.split_once(':').map(|(_, n)| n).filter(|n| !n.is_empty());
        let same: Vec<&str> = match name {
            Some(n) => ids.iter().copied().filter(|id| id.split_once(':').is_some_and(|(_, m)| m == n)).collect(),
            None => Vec::new(),
        };
        return match same.as_slice() {
            [one] => {
                println!("{what} « {w} » : index glissé, retrouvée par son nom exact → « {one} »");
                Ok(one.to_string())
            }
            [] => Err(format!("{what} « {w} » introuvable (débranchée ?). Identifiants exacts : {}", ids.join(" | "))),
            _ => Err(format!("{what} « {w} » : plusieurs périphériques portent ce nom ({}) — préciser l'identifiant", same.join(" | "))),
        };
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

/// Ce qu'il faut pour ajouter les flux d'un musicien.
struct StreamCtx<'a> {
    scenario: &'a Scenario,
    /// Adresse que l'agent doit joindre (banc, relais ou émetteur distant).
    agent_ip: &'a str,
}

impl StreamCtx<'_> {
    /// Ajoute les flux du musicien `m` (instrument, et voix s'il parle) :
    /// transport, `add-stream`, clés, calendrier. `rejoin` : 0 à l'arrivée, k au
    /// k-ième retour (identifiant neuf, comme un vrai musicien qui revient) ;
    /// `offset_us` : sa place dans la frise du musicien. Rend, par flux, son
    /// identifiant d'agent, ce que le banc en sait et son transport.
    async fn add_musician(
        &self,
        agent: &mut AgentLink,
        endpoint: &mut Endpoint,
        m: u32,
        rejoin: u64,
        offset_us: u64,
    ) -> Result<Vec<(String, StreamInfo, u32)>, String> {
        let profile: &PeerProfile = self.scenario.peer(m);
        let seed = self.scenario.seed.wrapping_add(u64::from(m));
        let mut out = Vec::new();
        for voice in [false, true] {
            if voice && profile.voice.is_none() {
                continue;
            }
            let suffix = if rejoin == 0 { String::new() } else { format!("-r{rejoin}") };
            let pid = format!("bench-m{m}{}{suffix}", if voice { "-voix" } else { "" });
            let down = endpoint.open_downlink(&pid, voice, seed.wrapping_add(rejoin)).await?;
            let keys = agent
                .add_stream(&StreamParams {
                    producer_id: &pid,
                    peer_id: &format!("bench-{m}{suffix}"),
                    server_ip: self.agent_ip,
                    server_port: down.port,
                    voice,
                    server_keys: &down.keys,
                })
                .await?;
            endpoint.start_downlink(down.id, &keys, profile, seed, offset_us).await?;
            let info = StreamInfo {
                name: format!("m{m}-{}{}", profile.name, if voice { "-voix" } else { "" }),
                voice,
                musician: m,
                link: profile.link_name_at(offset_us as f64 / 1e6).to_string(),
                sim_ppm: profile.drift_ppm,
            };
            out.push((pid, info, down.id));
        }
        Ok(out)
    }
}

/// Le réseau de chaque musicien simulé, en une ligne : « m2 ethernet −80 ppm ;
/// m3 fibre +60 ppm, → wifi-charge à 300 s ; m5 absent 20 s à 330 s… ».
fn network_line(s: &Scenario) -> String {
    if s.is_regular() {
        return "aucun (flux parfaitement réguliers)".into();
    }
    (2..=s.to_musicians)
        .map(|m| {
            let p = s.peer(m);
            let mut line = format!("m{m} {}", p.name);
            if p.drift_ppm != 0.0 {
                let _ = std::fmt::Write::write_fmt(&mut line, format_args!(" {:+.0} ppm", p.drift_ppm));
            }
            for c in &p.changes {
                let _ = std::fmt::Write::write_fmt(&mut line, format_args!(", → {} à {:.0} s", c.link.name, c.at_s));
            }
            for a in &p.absences {
                let _ = std::fmt::Write::write_fmt(&mut line, format_args!(", absent {:.0} s à {:.0} s", a.for_s, a.at_s));
            }
            line
        })
        .collect::<Vec<_>>()
        .join(" ; ")
}

/// D'où viennent les liens utilisés : un lien inventé ne passe pas pour une
/// mesure.
fn origin_line(s: &Scenario) -> String {
    let mut seen: Vec<(String, String)> = Vec::new();
    for p in &s.peers {
        for (_, l) in p.timeline() {
            let origin = l.origin.clone().unwrap_or_else(|| "non précisée".into());
            if !seen.iter().any(|(n, _)| *n == l.name) {
                seen.push((l.name.clone(), origin));
            }
        }
    }
    seen.iter().map(|(n, o)| format!("{n} : {o}")).collect::<Vec<_>>().join(" ; ")
}

fn profile_list(s: &Scenario) -> String {
    s.peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// Nom de la machine (`hostname` existe sous macOS comme sous Windows).
pub fn machine_name() -> String {
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
    // Mode relais : coupures déjà présentes EN ARRIVANT au relais (aller).
    let relay = m.relay_in.map_or(String::new(), |a| {
        let side = |packets: u64, gaps: u64| if packets == 0 { "non mesuré".to_string() } else { gaps.to_string() };
        format!(
            " | coupures à l'arrivée au relais {} (reçus) / {} (envoyé)",
            side(a.down_packets, a.down_gaps_over_10ms),
            side(a.up_packets, a.up_gaps_over_10ms)
        )
    });
    println!(
        "[{musicians} mus. {:>4.0} s] trous cumulés {:>4.0} | cible max {:>5.1} ms | CPU {:>5.1} % | banc en retard max {:>5.2} ms{relay}",
        t_step, underruns, target, m.cpu_pct, m.sender_late_max_ms
    );
}

/// Priorité obtenue par le fil d'envoi du banc, en toutes lettres.
pub fn priority_label(sender: &SenderLoop) -> String {
    match sender.priority.get() {
        Some(Ok(p)) => p.to_string(),
        Some(Err(e)) => format!("NORMALE — promotion refusée : {e}"),
        None => "inconnue".into(),
    }
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
        sender.add(link, &PeerProfile::preset("regular").expect("préréglage"), u64::from(i), 0);
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
    let priority = priority_label(&sender);
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
        assert!(e.contains("« 1 » introuvable") && e.contains("1:UMC ASIO Driver"), "{e}");
    }

    /// L'index glisse au branchement d'une carte : retrouvé par son nom exact
    /// et unique ; absent ou homonyme → refus, jamais d'à-peu-près.
    #[test]
    fn un_index_glisse_se_retrouve_par_le_nom_exact_et_unique() {
        let l = json!([
            { "id": "0:Scarlett Solo 4th Gen", "isDefault": false },
            { "id": "1:Microphone MacBook Pro", "isDefault": true },
            { "id": "2:Teams", "isDefault": false },
            { "id": "3:Teams", "isDefault": false },
        ]);
        assert_eq!(pick_device(&l, Some("0:Microphone MacBook Pro"), "entrée").unwrap(), "1:Microphone MacBook Pro");
        assert!(pick_device(&l, Some("0:Microphone"), "entrée").unwrap_err().contains("introuvable"), "nom partiel refusé");
        assert!(pick_device(&l, Some("5:Teams"), "entrée").unwrap_err().contains("plusieurs"));
        assert!(pick_device(&l, Some("4:UMC"), "entrée").unwrap_err().contains("débranchée"));
    }

    #[test]
    fn un_plugin_se_choisit_par_son_nom_exact() {
        assert_eq!(pick_plugin(&liste(), "amplitube 5").unwrap()["uid"], "1");
        let e = pick_plugin(&liste(), "AmpliTube").unwrap_err();
        assert!(e.contains("introuvable") && e.contains("AmpliTube 5"), "{e}");
        assert!(pick_plugin(&liste(), "Reverb").unwrap_err().contains("plusieurs"));
    }
}
