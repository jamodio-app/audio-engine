use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use base64::Engine;
use jamodio_audio_core::protocol::{
    AgentMessage, AgentState, BrowserMessage, PeerPerf, PipelineLatency, PluginPerf,
    RecordStemSpec, RecordedFileWire, StreamLevel, PROTOCOL_VERSION,
};
use std::sync::OnceLock;
use jamodio_audio_core::record::StemSpec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc as tokio_mpsc};

use crate::audio::device;
use crate::pipeline::PipelineState;

/// Timeout sur les locks `pipeline.lock().await` dans les handlers heartbeat.
/// Si dépassé, on répond `Error{overloaded}` au browser au lieu de bloquer
/// le watchdog browser (qui kill la WS à 3.47 s). Permet de survivre à un
/// pic CPU local sans perdre la session.
const LOCK_TIMEOUT_MS: u64 = 200;

/// Vérifie l'origin de la requête WS upgrade. On accepte uniquement :
///   - https://jamodio.com (prod)
///   - https://*.vercel.app (preview branches)
///   - http://localhost:* / http://127.0.0.1:* (dev local + browser-side dev)
///   - tauri://localhost ou http://tauri.localhost (UI WEBVIEW INTERNE
///     de l'agent — Tauri 2 sert sa webview sous ces schemes selon l'OS).
///     Cf. is_internal_client() qui bypass la single-client policy pour ces
///     origins, car la webview interne est un client légitime en plus du
///     browser jamodio.com (lecture-seule des stats, pas de race possible).
///   - file:// (cas webview embedded historique)
///   - Origin absent (clients "raw" comme tests CLI) — toléré, log warn
///
/// Empêche une page web random sur localhost:1234 de piloter l'agent
/// silencieusement.
fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        // Origin absent : clients non-browser (tests). On tolère mais on log.
        return true;
    };
    origin == "https://jamodio.com"
        || origin == "https://www.jamodio.com"
        || origin.ends_with(".vercel.app")
        || origin.starts_with("http://localhost:")
        || origin.starts_with("http://127.0.0.1:")
        || is_internal_client_origin(origin)
        || origin == "file://"
}

/// Vrai si l'origin correspond à la webview interne de l'agent Tauri.
/// Ces clients sont en LECTURE SEULE (UI dashboard) et bypass la
/// single-client policy : la webview reste connectée même si le browser
/// jamodio.com l'est aussi. Tauri 2 utilise des schemes différents selon
/// l'OS, on accepte les variantes courantes.
fn is_internal_client_origin(origin: &str) -> bool {
    origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
        || origin.starts_with("tauri://")
        || origin.starts_with("http://tauri.localhost")
}

/// Construit un `AgentMessage::Status` avec la version + OS + arch de l'agent.
/// Centralisé ici pour ne pas dupliquer ces 3 champs à chaque site.
fn make_status(state: AgentState) -> AgentMessage {
    AgentMessage::Status {
        state,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
    }
}

/// Construit le `Hello` envoyé en premier à chaque WS upgrade.
fn make_hello() -> AgentMessage {
    AgentMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        capabilities: vec![],
    }
}

/// Handle partagé pour le serveur WS. Permet :
///   - Single-client policy avec **kick automatique** du précédent (v0.4.3) :
///     `client_active` reste un AtomicBool de monitoring, mais le slot est
///     géré via `active_client_killer` qui permet d'évincer le client zombie
///     au lieu de rejeter le nouveau. Résout le pattern "agent déjà utilisé"
///     observé en prod 27/05 (WS browser half-open après tab close brutal,
///     slot agent jamais libéré jusqu'au quit+relance manuel).
///   - Broadcast d'événements globaux (Shutdown sur auto-update) à tous les
///     clients connectés via `shutdown_tx` (tokio broadcast channel).
#[derive(Clone)]
pub struct WsServerHandle {
    pipeline: Arc<tokio::sync::Mutex<PipelineState>>,
    /// True quand une WS browser EXTERNE est connectée et tient le slot.
    /// Sert au monitoring (pas à la décision d'admission). Cf. `active_client_killer`.
    client_active: Arc<AtomicBool>,
    /// v0.4.3 — Sender oneshot du client externe actuellement actif. Le nouveau
    /// client envoie via ce channel pour "kick" le précédent, qui voit sa
    /// receive loop break (cf. `tokio::select!` ws_rx + killer_rx). Une fois
    /// le slot libéré (cleanup terminé), le nouveau le reprend.
    /// `parking_lot::Mutex` car contention minimale (au plus 1×/connexion).
    active_client_killer:
        Arc<parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    /// Broadcast channel pour notifier tous les clients connectés (1 seul en
    /// pratique avec single-client) qu'un shutdown est imminent (auto-update).
    /// Capacité 4 : largement suffisant pour les ~quelques events de cycle de vie.
    shutdown_tx: broadcast::Sender<&'static str>,
}

impl WsServerHandle {
    pub fn new(pipeline: Arc<tokio::sync::Mutex<PipelineState>>) -> Self {
        let (shutdown_tx, _rx) = broadcast::channel::<&'static str>(4);
        Self {
            pipeline,
            client_active: Arc::new(AtomicBool::new(false)),
            active_client_killer: Arc::new(parking_lot::Mutex::new(None)),
            shutdown_tx,
        }
    }

    /// Diffuse un message Shutdown à tous les clients WS actuellement connectés.
    /// À appeler AVANT `app.restart()` (auto-update) pour donner au browser
    /// le temps de basculer en mode "agent restart imminent".
    pub fn broadcast_shutdown(&self, reason: &'static str) {
        // send() retourne Err si aucun receiver — pas grave, on s'en fiche.
        let _ = self.shutdown_tx.send(reason);
    }
}

/// Start the localhost WebSocket server on port 9876.
pub async fn start(handle: WsServerHandle) {
    let app = Router::new().route(
        "/",
        get(move |ws: WebSocketUpgrade, headers: HeaderMap, uri: Uri| {
            let handle = handle.clone();
            async move {
                // Origin check : rejet HTTP 403 si page web non-whitelisée
                let origin = headers
                    .get("origin")
                    .and_then(|h| h.to_str().ok());
                if !origin_allowed(origin) {
                    tracing::warn!(
                        target: "jamodio::ws",
                        origin = ?origin,
                        "WS upgrade rejected — origin not whitelisted"
                    );
                    return axum::http::Response::builder()
                        .status(403)
                        .body(axum::body::Body::from("forbidden origin"))
                        .unwrap();
                }
                if origin.is_none() {
                    tracing::warn!(
                        target: "jamodio::ws",
                        "WS upgrade with no Origin header (raw client?)"
                    );
                }
                // Le flag is_internal détermine si la connexion bypass la
                // single-client policy (UI Tauri webview = lecture seule).
                let is_internal = origin
                    .map(is_internal_client_origin)
                    .unwrap_or(false);
                // Read-only opt-in : `?op=logs` => connexion éphémère qui
                // sert uniquement GetLogsArchive, ne prend PAS le slot
                // single-client, ne déclenche PAS de cleanup (stop_all)
                // à la fermeture. Utilisé par le module Support browser
                // pour ne pas casser la WS du studio actif quand l'user
                // génère un bug-report. Cf. `handle_logs_connection`.
                let is_logs_only = uri
                    .query()
                    .map(|q| q.split('&').any(|kv| kv == "op=logs"))
                    .unwrap_or(false);
                if is_logs_only {
                    return ws
                        .on_upgrade(move |socket| handle_logs_connection(socket, handle))
                        .into_response();
                }
                ws.on_upgrade(move |socket| handle_connection(socket, handle, is_internal))
                    .into_response()
            }
        }),
    );

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:9876").await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "jamodio::ws", error = %e, "port 9876 already in use — another instance running?");
            return;
        }
    };

    tracing::info!(target: "jamodio::ws", addr = "ws://localhost:9876", "listening");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(target: "jamodio::ws", error = %e, "axum serve terminated");
    }
}

/// v0.4.3 — Helper extrait pour traiter un Message WS unique. Retourne
/// `true` si on doit continuer la receive loop, `false` si on doit la
/// quitter (envoi sortant cassé). Partagé entre la branche `is_internal`
/// et la branche externe (qui ajoute en plus killer/watchdog).
async fn handle_one_message(
    msg: Message,
    handle: &WsServerHandle,
    out_tx: &tokio_mpsc::Sender<AgentMessage>,
) -> bool {
    let Message::Text(text) = msg else { return true };

    let browser_msg = match serde_json::from_str::<BrowserMessage>(&text) {
        Ok(m) => m,
        Err(e) => {
            let truncated = &text[..text.len().min(120)];
            tracing::warn!(
                target: "jamodio::ws",
                error = %e,
                payload = truncated,
                "invalid browser message"
            );
            let err = AgentMessage::Error {
                message: format!("Invalid message: {} (parse error: {})", truncated, e),
            };
            let _ = out_tx.send(err).await;
            return true;
        }
    };

    let responses = handle_message(browser_msg, &handle.pipeline).await;
    for resp in responses {
        if out_tx.send(resp).await.is_err() {
            return false;
        }
    }
    true
}

async fn handle_connection(socket: WebSocket, handle: WsServerHandle, is_internal: bool) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // v0.4.3 — Single-client policy avec **kick du précédent à la promotion**
    // au lieu d'un rejet sec. Critique : on NE PROMEUT PAS la connexion au
    // moment du `on_upgrade` (= aucun kick sur simple open WS), seulement
    // à la réception du PREMIER BrowserMessage (typiquement HelloAck).
    // Justification :
    //   - agent-status.js émet des probes périodiques : ouvre WS, lit Hello,
    //     ferme. Aucun BrowserMessage envoyé. Si on kickait au connect, chaque
    //     probe killerait la session active du même browser !
    //   - groupe.js detectAgent() envoie HelloAck juste après réception Hello.
    //     C'est ce HelloAck qui sert de "preuve de session réelle" et
    //     déclenche la promotion + kick du précédent.
    //
    // Le slot `client_active` + `active_client_killer` sont donc pris en
    // charge dans le receive loop ci-dessous (cherche `// PROMOTION`).
    //
    // Les clients internes (UI Tauri webview) bypass tout slot management.
    let mut killer_rx: Option<tokio::sync::oneshot::Receiver<&'static str>> = None;
    let mut slot_taken = false;

    tracing::info!(target: "jamodio::ws", is_internal, "client connected");

    // Premier message : Hello (annonce le protocole + version).
    // Suivi du Status initial (legacy compat pour browsers v0.1.x qui
    // ignorent Hello et lisent uniquement Status).
    let hello = make_hello();
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&hello).unwrap()))
        .await;
    let status = make_status(AgentState::Idle);
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&status).unwrap()))
        .await;

    // S1.5 — Sync state au reconnect : si un plugin INSERT est déjà chargé
    // côté agent (= browser a fait reload de page mais l'agent vivait), on
    // push l'état pour que l'UI affiche directement [● bypass][nom][✕] au
    // lieu de "+ FX" trompeur. Le browser reçoit le même message que pour
    // un load fresh, plus rien à modifier côté handler.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let pl = handle.pipeline.lock().await;
        if let Some((info, bypass)) = pl.get_instrument_plugin_snapshot() {
            drop(pl);
            let resync = AgentMessage::InstrumentPluginLoaded {
                name: info.name,
                plugin_ref: info.plugin_ref,
                latency_samples: info.latency_samples,
                has_editor: info.has_editor,
                bypass,
            };
            let _ = ws_tx
                .send(Message::Text(serde_json::to_string(&resync).unwrap()))
                .await;
            tracing::info!(target: "jamodio::ws", "pushed instrument plugin state on connect");
        }
    }

    // Channel for outgoing messages (from message handler + periodic tasks)
    let (out_tx, mut out_rx) = tokio_mpsc::channel::<AgentMessage>(64);

    // Subscribe au broadcast shutdown (auto-update).
    let mut shutdown_rx = handle.shutdown_tx.subscribe();
    let shutdown_out = out_tx.clone();
    let shutdown_task = tokio::spawn(async move {
        if let Ok(reason) = shutdown_rx.recv().await {
            tracing::info!(target: "jamodio::ws", reason, "broadcasting Shutdown to client");
            let _ = shutdown_out
                .send(AgentMessage::Shutdown {
                    reason: reason.to_string(),
                })
                .await;
            // Petit délai pour laisser le temps au browser de recevoir + handle
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });

    // Spawn periodic StreamLevels sender (every 100ms)
    let levels_pipeline = handle.pipeline.clone();
    let levels_tx = out_tx.clone();
    let levels_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            let pl = levels_pipeline.lock().await;
            let rms_data = pl.mixer.lock().stream_rms();
            // Sprint B talkback auto-mute : lit input_rms (instrument self post-plugin)
            // et midi_active (Note ON dans les ~200 dernières ms) pour piloter le
            // détecteur d'activité côté browser. Ces 2 valeurs sont reset entre les
            // captures (Pipeline::new), donc Some(...) toujours valides côté agent
            // — le serializer écrira `null`/absent uniquement si l'utilisateur veut
            // un payload minimaliste (back-compat).
            let input_rms = f32::from_bits(
                pl.input_rms.load(std::sync::atomic::Ordering::Relaxed),
            );
            let midi_active = pl.midi_active.load(std::sync::atomic::Ordering::Relaxed);
            drop(pl);
            // Push si on a soit des niveaux peers, soit un signal self (RMS > 0
            // ou MIDI actif). En idle complet, on saute le push.
            let has_self_signal = input_rms > 0.0 || midi_active;
            if !rms_data.is_empty() || has_self_signal {
                let levels: Vec<StreamLevel> = rms_data
                    .into_iter()
                    .map(|(producer_id, rms)| StreamLevel { producer_id, rms })
                    .collect();
                let msg = AgentMessage::StreamLevels {
                    levels,
                    input_rms: Some(input_rms),
                    midi_active: Some(midi_active),
                };
                if levels_tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
    });

    // Sprint S1 — periodic PerfStats sender (1 Hz).
    //
    // Flush des histogrammes (capture→send + plugin process_stereo) +
    // snapshot du compteur drops capture + snapshot drift_ppm par peer +
    // stats mixer (underruns, drift_drops, target_ms). Construit
    // `AgentMessage::PerfStats` et l'envoie. Skip l'émission si encoder
    // idle ET aucun peer actif (= rien d'intéressant à reporter).
    let perfstats_pipeline = handle.pipeline.clone();
    let perfstats_tx = out_tx.clone();
    let perfstats_start = Instant::now();
    let perfstats_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        // Skip le 1er tick immédiat (sinon flush vide juste après connect).
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        // Sprint S6 — anti-spam PeerUnstable par producer_id. Map<id, Instant>
        // contenant le dernier instant d'émission. Émis au max 1× / 30 s par
        // peer (= si le peer reste instable, l'agent renvoie périodiquement
        // pour signaler la situation continue, mais sans flooder).
        let mut last_peer_unstable_emit: std::collections::HashMap<
            String,
            Instant,
        > = std::collections::HashMap::new();
        const PEER_UNSTABLE_COOLDOWN: std::time::Duration =
            std::time::Duration::from_secs(30);
        const PEER_UNSTABLE_WINDOW: std::time::Duration =
            std::time::Duration::from_secs(30);
        const PEER_UNSTABLE_THRESHOLD: usize = 16;
        loop {
            interval.tick().await;
            let pl = perfstats_pipeline.lock().await;
            // Flush histograms (acquièrent le lock parking_lot une fois chacun)
            let pipeline_snap = pl.perfstats.pipeline_latency.lock().flush();
            let plugin_snap = pl.perfstats.plugin_latency.lock().flush();
            // v0.4.8 — 3 histogrammes par stage pour discriminer "spike
            // traitement" vs "spike file en queue ringbuf".
            let capture_snap = pl.perfstats.capture_latency.lock().flush();
            let process_snap = pl.perfstats.process_latency.lock().flush();
            let encode_snap = pl.perfstats.encode_latency.lock().flush();
            // Reset+swap atomic des drops capture
            let capture_drops_window = pl
                .perfstats
                .capture_drops
                .swap(0, Ordering::Relaxed);
            // Chantier C — pic de sortie post-plugin (reset à 0 = +0.0 f32) +
            // taux de saturation soutenue (% samples > pleine-échelle).
            let output_peak =
                f32::from_bits(pl.perfstats.output_peak.swap(0, Ordering::Relaxed));
            let clip_samples = pl.perfstats.output_clip_samples.swap(0, Ordering::Relaxed);
            let total_samples = pl.perfstats.output_total_samples.swap(0, Ordering::Relaxed);
            let output_clip_pct = if total_samples > 0 {
                100.0 * clip_samples as f32 / total_samples as f32
            } else {
                0.0
            };
            // Snapshot drift_ppm par peer (clone du hashmap, cheap car ≤4 peers)
            let drift_map: std::collections::HashMap<String, f64> =
                pl.perfstats.drift_ppm_by_producer.lock().clone();
            // Snapshot mixer stats (underruns + drift_drops cumul + target_ms)
            // + Chantier C : stats du self-monitor (latence courante + underruns).
            let (mixer_stats, monitor_buffer_ms, monitor_underruns) = {
                let m = pl.mixer.lock();
                let stats = m.stream_perf_stats();
                let (mt, mu) = m.self_monitor_stats();
                (stats, mt, mu)
            };
            // Sprint S6 — récupère les peers REMOTE instables (= > 16 drift
            // drains sur fenêtre 30 s). Le mixer purge ses VecDeque internes
            // au passage. Retour : (producer_id, events_window, drains_total).
            let unstable_peers: Vec<(String, usize, u64)> = pl
                .mixer
                .lock()
                .stream_unstable_events(
                    PEER_UNSTABLE_WINDOW,
                    PEER_UNSTABLE_THRESHOLD,
                );
            // Nom du plugin actif (si chargé) pour `PluginPerf.name` — sinon
            // on omet PluginPerf entièrement.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let plugin_name: Option<String> = pl
                .instrument_plugin_info
                .lock()
                .as_ref()
                .map(|info| info.name.clone());
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let plugin_name: Option<String> = None;

            // v0.4.9 — détection saturation pipeline globale (= capture_drops
            // burst, distinct du plugin overload S5). Cas typique : sample-load
            // brutal d'un plugin sampler (BFD Player, Kontakt) qui bloque
            // l'encoder thread alors que `plugin_latency` reste sous le seuil
            // (les blocs droppés ne traversent pas le plugin → pas mesurés).
            //
            // Anti-spam : on n'émet qu'une fois toutes les 10 s pour éviter
            // le flood en cas de saturation prolongée (BFD chargeant plusieurs
            // samples consécutifs).
            const PIPELINE_OVERLOAD_DROP_THRESHOLD: u64 = 100;
            const PIPELINE_OVERLOAD_COOLDOWN_MS: u128 = 10_000;
            static LAST_PIPELINE_OVERLOAD_EMIT_MS: OnceLock<
                std::sync::atomic::AtomicU64,
            > = OnceLock::new();
            let last_emit_atomic = LAST_PIPELINE_OVERLOAD_EMIT_MS
                .get_or_init(|| std::sync::atomic::AtomicU64::new(0));
            let now_ms = perfstats_start.elapsed().as_millis() as u64;
            let last_emit = last_emit_atomic.load(Ordering::Relaxed);
            let pipeline_overload_msg: Option<AgentMessage> =
                if capture_drops_window > PIPELINE_OVERLOAD_DROP_THRESHOLD
                    && (now_ms as u128)
                        .saturating_sub(last_emit as u128)
                        > PIPELINE_OVERLOAD_COOLDOWN_MS
                {
                    last_emit_atomic.store(now_ms, Ordering::Relaxed);
                    let plugin_name_owned =
                        plugin_name.clone().unwrap_or_default();
                    tracing::warn!(
                        target: "jamodio::ws",
                        drops_per_sec = capture_drops_window,
                        pipeline_p99_ms = pipeline_snap.p99_ms,
                        plugin = %plugin_name_owned,
                        "agent pipeline overload — encoder thread bloqué (sample-load plugin ou CPU tiers)"
                    );
                    Some(AgentMessage::AgentPipelineOverload {
                        drops_per_sec: capture_drops_window,
                        pipeline_p99_ms: pipeline_snap.p99_ms,
                        plugin_name: plugin_name_owned,
                    })
                } else {
                    None
                };

            // Sprint S5 (révisé v0.4.11) — bypass auto plugin SEULEMENT s'il
            // cause des DROPS RÉELS.
            //
            // L'ancienne logique (p99 plugin > 4 ms seul) était trop agressive
            // et hardware-dépendante : sur la session 28/05 (Mac Mini M1),
            // AmpliTube 5 et BFD Player tournaient à p99 4-6 ms AVEC
            // drops_per_sec=0 (= le ringbuf S3 absorbait, audio nickel) mais
            // étaient bypassés à tort → l'utilisateur entendait le DRY
            // ("comme si bypass") + silence total en mode MIDI.
            //
            // Le SEUL signal fiable de "le plugin sature vraiment" =
            // `capture_drops` > 0 (= le CPAL callback ne peut plus pousser ses
            // samples car l'encoder est durablement bloqué). Hardware-agnostic :
            // on bypasse quand ça coupe RÉELLEMENT, pas quand un plugin lourd
            // mais viable prend 5 ms par bloc.
            //
            // Conditions cumulatives :
            //   1. capture_drops_window > seuil (= vraies coupures, pas 1-2 isolés)
            //   2. plugin actif (= candidat) avec p99 notable (= il consomme)
            //   3. pas déjà bypassé (anti-spam — flag reset par l'user/load)
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let overload_msg: Option<AgentMessage> = {
                // Seuil drops : > 20/s = saturation soutenue (≈ 5 % des blocs
                // CPAL droppés). En-dessous, c'est tolérable / transitoire.
                const OVERLOAD_DROPS_THRESHOLD: u64 = 20;
                // p99 minimal pour incriminer le plugin (= il consomme du temps
                // significatif ; sinon les drops viennent d'ailleurs → couvert
                // par AgentPipelineOverload).
                const OVERLOAD_PLUGIN_P99_MIN_MS: f32 = 3.0;
                let plugin_is_culprit = plugin_snap.count >= 50
                    && plugin_snap.p99_ms > OVERLOAD_PLUGIN_P99_MIN_MS;
                if capture_drops_window > OVERLOAD_DROPS_THRESHOLD
                    && plugin_is_culprit
                    && !pl
                        .plugin_auto_bypass_active
                        .load(Ordering::SeqCst)
                {
                    // Trigger : on flag bypass auto + auto_bypass_active = true.
                    pl.instrument_plugin_bypass.store(true, Ordering::SeqCst);
                    pl.plugin_auto_bypass_active
                        .store(true, Ordering::SeqCst);
                    let name = plugin_name
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    tracing::warn!(
                        target: "jamodio::plugin",
                        plugin = %name,
                        p99_ms = plugin_snap.p99_ms,
                        max_ms = plugin_snap.max_ms,
                        drops_per_sec = capture_drops_window,
                        count = plugin_snap.count,
                        "plugin overload détecté (drops réels) — bypass auto activé"
                    );
                    Some(AgentMessage::InstrumentPluginOverload {
                        name,
                        p99_ms: plugin_snap.p99_ms,
                        max_ms: plugin_snap.max_ms,
                        count: plugin_snap.count,
                    })
                } else {
                    None
                }
            };
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let overload_msg: Option<AgentMessage> = None;

            drop(pl);

            // Plugin perf : seulement si on a observé ET qu'un plugin est
            // chargé. Le nom peut être absent si la course load↔flush se
            // termine entre les deux locks — dans ce cas on rapporte
            // l'observation avec "unknown" pour ne pas perdre la donnée.
            let plugin_perf = if plugin_snap.count > 0 {
                Some(PluginPerf {
                    name: plugin_name.unwrap_or_else(|| "unknown".to_string()),
                    count: plugin_snap.count,
                    mean_ms: plugin_snap.mean_ms,
                    p50_ms: plugin_snap.p50_ms,
                    p99_ms: plugin_snap.p99_ms,
                    max_ms: plugin_snap.max_ms,
                })
            } else {
                None
            };

            // Construction de la couche pipeline_latency (toujours présente,
            // count=0 indique encoder idle).
            let pipeline_latency_ms = PipelineLatency {
                count: pipeline_snap.count,
                p50_ms: pipeline_snap.p50_ms,
                p99_ms: pipeline_snap.p99_ms,
                max_ms: pipeline_snap.max_ms,
                mean_ms: pipeline_snap.mean_ms,
                // Inclut les drops "RTP channel full" agrégés par l'histogramme
                // (record_drop côté encoder) + les drops capture côté CPAL.
                // Les deux sont des indicateurs de saturation à reporter ensemble.
                drops_per_sec: pipeline_snap.drops + capture_drops_window,
            };

            // Construction des peers : on dérive de mixer_stats + drift_map.
            // Si un producer est dans mixer mais pas dans drift_map (warmup),
            // ppm = 0.0 (cf. drift.rs).
            let peers: Vec<PeerPerf> = mixer_stats
                .into_iter()
                .map(|(producer_id, underruns, drift_drops, target_ms)| {
                    let drift_ppm = drift_map.get(&producer_id).copied().unwrap_or(0.0);
                    PeerPerf {
                        producer_id,
                        drift_ppm,
                        buffer_target_ms: target_ms,
                        underruns,
                        drift_drops,
                    }
                })
                .collect();

            // Skip si rien à reporter (encoder idle + pas de peer + pas de plugin)
            if plugin_perf.is_none()
                && pipeline_latency_ms.count == 0
                && pipeline_latency_ms.drops_per_sec == 0
                && peers.is_empty()
            {
                continue;
            }

            // Log structuré tracing pour qu'il finisse dans agent.log :
            // permet au support de lire les perfstats même sans bundle browser.
            // v0.4.8 — ajout des 3 mesures par stage (capture/process/encode).
            // Sémantique : pipeline_* = end-to-end (= traitement + temps en
            // file dans les ringbufs). capture_*/process_*/encode_* = temps
            // de traitement PUR par stage. Permet de discriminer un spike
            // "vraie charge plugin" vs "stall en queue".
            tracing::info!(
                target: "jamodio::perfstats",
                pipeline_p50_ms = pipeline_latency_ms.p50_ms,
                pipeline_p99_ms = pipeline_latency_ms.p99_ms,
                pipeline_max_ms = pipeline_latency_ms.max_ms,
                pipeline_count = pipeline_latency_ms.count,
                drops_per_sec = pipeline_latency_ms.drops_per_sec,
                plugin_name = plugin_perf.as_ref().map(|p| p.name.as_str()).unwrap_or(""),
                plugin_p99_ms = plugin_perf.as_ref().map(|p| p.p99_ms).unwrap_or(0.0),
                plugin_max_ms = plugin_perf.as_ref().map(|p| p.max_ms).unwrap_or(0.0),
                capture_p99_ms = capture_snap.p99_ms,
                capture_max_ms = capture_snap.max_ms,
                process_p99_ms = process_snap.p99_ms,
                process_max_ms = process_snap.max_ms,
                encode_p99_ms = encode_snap.p99_ms,
                encode_max_ms = encode_snap.max_ms,
                peers = peers.len(),
                output_peak,
                output_clip_pct,
                monitor_buffer_ms,
                monitor_underruns,
                "perfstats snapshot"
            );

            let msg = AgentMessage::PerfStats {
                timestamp_ms: perfstats_start.elapsed().as_millis() as u64,
                plugin: plugin_perf,
                pipeline_latency_ms,
                peers,
                output_peak,
                output_clip_pct,
                monitor_buffer_ms,
                monitor_underruns,
            };
            if perfstats_tx.send(msg).await.is_err() {
                break;
            }

            // Sprint S5 — émet le message d'overload APRÈS le PerfStats (le
            // browser voit d'abord les chiffres "vérité" qui ont déclenché
            // le trigger, puis le toast UI). 1 seul message par cycle
            // d'overload — protégé par `plugin_auto_bypass_active`.
            let plugin_overload_fired = overload_msg.is_some();
            if let Some(msg) = overload_msg {
                if perfstats_tx.send(msg).await.is_err() {
                    break;
                }
            }

            // v0.4.9 — émet le message AgentPipelineOverload (distinct du
            // plugin overload S5 ; ne déclenche PAS de bypass plugin).
            // v0.4.11 — si un bypass plugin vient de partir sur la même
            // fenêtre de drops, on supprime ce toast générique : le bypass
            // plugin EST la cause/remédiation spécifique, deux toasts pour
            // le même évènement = bruit UX.
            if let Some(msg) = pipeline_overload_msg {
                if !plugin_overload_fired && perfstats_tx.send(msg).await.is_err() {
                    break;
                }
            }

            // Sprint S6 — émet AgentMessage::PeerUnstable pour chaque peer
            // au-dessus du seuil. Anti-spam : 1× / 30 s par producer_id.
            // Le browser maintient le badge "⚠ X envoie par à-coups" pendant
            // 60 s après le dernier message (= si message renvoyé toutes les
            // 30 s, badge reste affiché ; sinon il disparaît).
            let now = Instant::now();
            for (producer_id, events_window, drains_total) in unstable_peers {
                let should_emit = match last_peer_unstable_emit.get(&producer_id) {
                    Some(last) => now.duration_since(*last) >= PEER_UNSTABLE_COOLDOWN,
                    None => true,
                };
                if !should_emit {
                    continue;
                }
                let drift_ppm = drift_map.get(&producer_id).copied().unwrap_or(0.0);
                tracing::warn!(
                    target: "jamodio::mixer",
                    producer = &producer_id[..8.min(producer_id.len())],
                    events_window,
                    drains_total,
                    drift_ppm,
                    "peer instable détecté — envoie en bursts"
                );
                if perfstats_tx
                    .send(AgentMessage::PeerUnstable {
                        producer_id: producer_id.clone(),
                        drift_drains_window: events_window as u64,
                        drift_drains_total: drains_total,
                        drift_ppm,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                last_peer_unstable_emit.insert(producer_id, now);
            }
        }
    });

    // Spawn task to forward outgoing messages to WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if ws_tx.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    // v0.4.3 — Receive loop unifié.
    //
    // Critères d'arrêt :
    //   1. WS fermée proprement (close frame du browser, erreur réseau)
    //   2. Eviction par un nouveau client (killer_rx — uniquement post-promotion)
    //   3. Watchdog (uniquement post-promotion, voir détail ci-dessous)
    //
    // Phases :
    //   - PRE-PROMOTION : on attend un BrowserMessage. Le timeout est
    //     généreux (60 s) car un probe peut juste lire Hello et fermer,
    //     mais on veut pouvoir détecter une session "abandonnée" qui ne
    //     fait que rester ouverte sans rien envoyer. Pas de slot pris,
    //     pas de killer armé.
    //   - POST-PROMOTION (déclenchée par 1er BrowserMessage côté externe) :
    //     on prend le slot, on kick le précédent client, on arme le killer.
    //     Le watchdog devient 5 s (heartbeat browser = 1.5 s × 3 = 4.5 s).
    //
    // Les clients internes (UI Tauri webview) restent en phase pre-promotion :
    // pas de slot, pas de killer, pas de watchdog agressif.
    const PRE_PROMOTION_IDLE_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(60);
    const POST_PROMOTION_WATCHDOG: std::time::Duration =
        std::time::Duration::from_secs(5);

    let mut exit_reason: &'static str = "ws-closed-normally";

    loop {
        let current_timeout = if slot_taken {
            POST_PROMOTION_WATCHDOG
        } else {
            PRE_PROMOTION_IDLE_TIMEOUT
        };

        // Branche killer : `pending()` tant qu'on n'a pas armé (pre-promotion).
        let killer_fut = async {
            match killer_rx.as_mut() {
                Some(rx) => rx.await.ok(),
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased; // priorité au killer pour libérer le slot vite
            kicked = killer_fut => {
                exit_reason = kicked.unwrap_or("displaced");
                break;
            }
            ws_msg = tokio::time::timeout(current_timeout, ws_rx.next()) => {
                match ws_msg {
                    Err(_) => {
                        if slot_taken {
                            // Heartbeat 1.5 s côté browser aurait dû arriver.
                            // 5 s sans message = WS half-open (tab killed,
                            // kernel buffer saturé, etc.). Libère le slot.
                            tracing::warn!(
                                target: "jamodio::ws",
                                timeout_secs = POST_PROMOTION_WATCHDOG.as_secs(),
                                "watchdog timeout post-promotion — releasing slot"
                            );
                            exit_reason = "watchdog-timeout";
                        } else {
                            // Pre-promotion idle : probe qui n'a pas fermé,
                            // ou client mort avant HelloAck. On ferme.
                            tracing::debug!(
                                target: "jamodio::ws",
                                "pre-promotion idle timeout — closing"
                            );
                            exit_reason = "pre-promotion-idle";
                        }
                        break;
                    }
                    Ok(None) => break, // WS closed by browser side
                    Ok(Some(Err(e))) => {
                        tracing::warn!(
                            target: "jamodio::ws",
                            error = %e,
                            "ws receive error — closing connection"
                        );
                        exit_reason = "ws-error";
                        break;
                    }
                    Ok(Some(Ok(msg))) => {
                        // v0.4.4 — PROMOTION uniquement sur Message::Text
                        // qui se parse en BrowserMessage. Les Close/Ping/Pong/
                        // Binary ne déclenchent PAS promotion : c'est ce qui
                        // créait le log spam observé en v0.4.3 (probes
                        // agent-status.js qui ferment leur WS génèrent un
                        // Close frame, traité à tort comme "1er message").
                        let is_real_browser_msg = match &msg {
                            Message::Text(text) => serde_json::from_str::<
                                BrowserMessage,
                            >(text)
                            .is_ok(),
                            _ => false,
                        };

                        if is_real_browser_msg && !slot_taken && !is_internal {
                            slot_taken = true;
                            let (new_killer_tx, new_killer_rx) =
                                tokio::sync::oneshot::channel();
                            let previous = handle
                                .active_client_killer
                                .lock()
                                .replace(new_killer_tx);
                            if let Some(prev) = previous {
                                // v0.4.4 — log "displacing" UNIQUEMENT si on
                                // a réellement kické quelqu'un. send() retourne
                                // Err si le receiver a déjà été drop (= ancien
                                // client déjà cleanup, Sender stale dans le
                                // Mutex). Dans ce cas, no-op silencieux : pas
                                // de kick effectif, pas de log spam.
                                match prev.send("displaced-by-new-client") {
                                    Ok(()) => {
                                        tracing::warn!(
                                            target: "jamodio::ws",
                                            "displacing previous external client (stale slot — likely half-open WS or rapid reconnect)"
                                        );
                                        // Pause courte pour laisser le cleanup
                                        // ancien finir (stop_all peut prendre
                                        // quelques ms).
                                        tokio::time::sleep(
                                            std::time::Duration::from_millis(50),
                                        ).await;
                                    }
                                    Err(_) => {
                                        // Sender stale — ancien client déjà parti.
                                        // Pas de kick effectif, pas de pause.
                                    }
                                }
                            }
                            handle.client_active.store(true, Ordering::SeqCst);
                            killer_rx = Some(new_killer_rx);
                            tracing::info!(
                                target: "jamodio::ws",
                                "external client promoted (first BrowserMessage received) — watchdog armed"
                            );
                        }

                        if !handle_one_message(msg, &handle, &out_tx).await {
                            break;
                        }
                    }
                }
            }
        }
    }

    tracing::info!(
        target: "jamodio::ws",
        is_internal,
        promoted = slot_taken,
        reason = exit_reason,
        "client disconnected — cleanup"
    );

    levels_task.abort();
    perfstats_task.abort();
    send_task.abort();
    shutdown_task.abort();

    // v0.4.3 — Cleanup pipeline UNIQUEMENT pour les clients externes
    // qui ont été PROMUS (= ont envoyé au moins un BrowserMessage et donc
    // pris le slot). Un client externe non promu (= probe qui a juste lu
    // Hello et fermé) ne touche pas au pipeline et ne libère pas de slot.
    //
    // Les clients internes (UI Tauri webview) ne pilotent jamais le pipeline,
    // donc leur close ne doit PAS stop_all (sinon le browser actif perd sa
    // session quand on ferme la fenêtre Tauri par erreur).
    if !is_internal && slot_taken {
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            handle.pipeline.lock(),
        )
        .await
        {
            Ok(mut pl) => pl.stop_all(),
            Err(_) => tracing::warn!(
                target: "jamodio::ws",
                "pipeline lock timeout during cleanup — stop_all skipped (next client will see stale state)"
            ),
        }
        // Libère le slot. Note : on ne `take()` PAS notre Sender dans
        // `active_client_killer` car s'il a été displaced, c'est déjà le
        // Sender d'un autre client qui occupe le slot. Le mutex contient
        // potentiellement un sender stale ; le prochain `replace()` retournera
        // un Err silencieux sur `send()` (receiver drop) — OK, idempotent.
        handle.client_active.store(false, Ordering::SeqCst);
    }
}

/// Handler dédié aux connexions read-only `?op=logs`. Ne prend PAS le
/// slot single-client, ne pilote PAS le pipeline, ne fait PAS de cleanup
/// → la WS persistante du studio (s'il y en a une) reste intacte.
///
/// Sert uniquement `GetLogsArchive` puis ferme. Toute autre commande est
/// rejetée explicitement (Error) — pas de surface d'attaque additionnelle.
/// Timeout de sécurité 10 s pour éviter une connexion qui pendrait.
async fn handle_logs_connection(socket: WebSocket, handle: WsServerHandle) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    tracing::info!(target: "jamodio::ws", "logs-only client connected");

    // Hello envoyé pour cohérence avec le handler principal (le browser
    // vérifie peut-être qu'il reçoit un Hello avant d'envoyer sa requête).
    let hello = make_hello();
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&hello).unwrap()))
        .await;

    // Une seule itération attendue : GetLogsArchive → réponse → close.
    let res = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            let Message::Text(text) = msg else { continue };
            let browser_msg = match serde_json::from_str::<BrowserMessage>(&text) {
                Ok(m) => m,
                Err(_) => continue, // ignore HelloAck éventuel + autres bruits
            };
            match browser_msg {
                BrowserMessage::GetLogsArchive { max_days, max_bytes } => {
                    let days = max_days.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_DAYS);
                    let bytes = max_bytes.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_BYTES);
                    let collected = tokio::task::spawn_blocking(move || {
                        let (content, files, truncated) =
                            crate::logging::collect_recent_logs(days, bytes);
                        let log_dir = crate::logging::log_dir().to_string_lossy().into_owned();
                        (content, files, truncated, log_dir)
                    })
                    .await;
                    let payload = match collected {
                        Ok((content, files, truncated, log_dir)) => {
                            tracing::info!(
                                target: "jamodio::support",
                                files = files.len(),
                                bytes = content.len(),
                                truncated,
                                conn = "logs-only",
                                "GetLogsArchive served"
                            );
                            AgentMessage::LogsArchive { content, files, truncated, log_dir }
                        }
                        Err(e) => AgentMessage::Error {
                            message: format!("logs archive task failed: {e}"),
                        },
                    };
                    let _ = ws_tx
                        .send(Message::Text(serde_json::to_string(&payload).unwrap()))
                        .await;
                    return;
                }
                BrowserMessage::HelloAck { .. } => continue,
                #[allow(unreachable_patterns)]
                _ => {
                    // Tout autre message est refusé : ce canal est read-only.
                    let err = AgentMessage::Error {
                        message: "logs-only connection: only get-logs-archive is allowed".into(),
                    };
                    let _ = ws_tx
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
                    return;
                }
            }
        }
    })
    .await;
    if res.is_err() {
        tracing::warn!(target: "jamodio::ws", "logs-only connection timed out (10s)");
    }
    // Petite latence pour laisser le frame WS partir avant le close TCP.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _ = ws_tx.close().await;
    // Le `_ = handle` empêche le warning unused — on ne touche PAS au pipeline
    // ni au flag client_active depuis ici, c'est tout l'intérêt.
    let _ = handle;
    tracing::info!(target: "jamodio::ws", "logs-only client disconnected (no cleanup)");
}

/// Chantier A (v0.4.12) — sérialise les opérations plugin LENTES (load/unload
/// natif AU/VST3, 0,4–4 s). Tenu HORS du lock `PipelineState` et du chemin
/// audio → ne gèle rien. Garantit qu'on n'exécute jamais deux init/teardown
/// natifs concurrents (course handle ↔ instance) même si le browser spamme.
#[cfg(any(target_os = "macos", target_os = "windows"))]
static PLUGIN_OPS_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn plugin_ops_lock() -> &'static tokio::sync::Mutex<()> {
    PLUGIN_OPS_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Tente d'acquérir le lock pipeline avec un timeout court. Si dépassé,
/// retourne None et le caller répond Error{overloaded} au lieu de bloquer.
async fn try_lock_pipeline(
    pipeline: &Arc<tokio::sync::Mutex<PipelineState>>,
) -> Option<tokio::sync::MutexGuard<'_, PipelineState>> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(LOCK_TIMEOUT_MS),
        pipeline.lock(),
    )
    .await
    {
        Ok(guard) => Some(guard),
        Err(_) => {
            tracing::warn!(
                target: "jamodio::ws",
                timeout_ms = LOCK_TIMEOUT_MS,
                "pipeline.lock() timeout — agent overloaded, skipping handler"
            );
            None
        }
    }
}

async fn handle_message(
    msg: BrowserMessage,
    pipeline: &Arc<tokio::sync::Mutex<PipelineState>>,
) -> Vec<AgentMessage> {
    match msg {
        BrowserMessage::HelloAck { protocol_version, session_id } => {
            // Log au niveau INFO (pas debug) pour que `session_id` soit toujours
            // visible dans les logs agent même en niveau de prod par défaut.
            // C'est l'unique pivot pour croiser browser↔agent côté support.
            tracing::info!(
                target: "jamodio::ws",
                browser_protocol = protocol_version,
                agent_protocol = PROTOCOL_VERSION,
                session_id = session_id.as_deref().unwrap_or("?"),
                "browser session linked"
            );
            vec![]
        }

        BrowserMessage::GetDevices => {
            let inputs = device::list_inputs();
            let outputs = device::list_outputs();
            vec![AgentMessage::Devices { inputs, outputs }]
        }

        BrowserMessage::SelectDevices { input_id, output_id } => {
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::Error { message: "agent overloaded".into() }];
            };
            pl.select_devices(input_id, output_id);
            vec![make_status(AgentState::Idle)]
        }

        BrowserMessage::StartCapture { ssrc, sfu_ip, sfu_port, payload_type: _, input_device, channel_index, srtp_parameters } => {
            tracing::info!(
                target: "jamodio::ws",
                ssrc,
                sfu = format!("{}:{}", sfu_ip, sfu_port),
                ?input_device,
                ?channel_index,
                "StartCapture"
            );
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::Error { message: "agent overloaded".into() }];
            };
            // Le browser passe l'id du device directement dans start-capture
            // (le plus fiable — select-devices pouvait ne jamais arriver).
            // L'id est strict ({idx}:{name}) — pas de fuzzy, pas de fallback.
            // ⚠ Bug fix : set_input_device() (pas select_devices) pour ne pas
            // écraser l'output_device_id précédemment configuré.
            if input_device.is_some() {
                pl.set_input_device(input_device.clone());
            }
            match pl.start_capture(ssrc, sfu_ip.clone(), sfu_port, 111, channel_index, srtp_parameters).await {
                Ok((local_port, agent_srtp, info)) => {
                    // Deux messages : LocalPort (chaîne SRTP avec le SFU) +
                    // CaptureStarted (confirmation explicite côté browser
                    // que le device demandé est bien celui ouvert).
                    vec![
                        AgentMessage::LocalPort {
                            producer_id: String::new(),
                            port: local_port,
                            srtp_parameters: agent_srtp,
                        },
                        AgentMessage::CaptureStarted {
                            device_id: info.device_id,
                            device_name: info.device_name,
                            channels: info.channels,
                            native_sample_rate: info.native_sample_rate,
                        },
                    ]
                }
                Err(crate::pipeline::CaptureStartError::InputDeviceNotFound { requested }) => {
                    tracing::warn!(
                        target: "jamodio::ws",
                        ?requested,
                        "StartCapture rejected: input device not found"
                    );
                    vec![AgentMessage::CaptureError {
                        reason: "input-device-not-found".into(),
                        requested_device: requested,
                        detail: None,
                    }]
                }
                Err(crate::pipeline::CaptureStartError::OutputDeviceNotFound { requested }) => {
                    tracing::warn!(
                        target: "jamodio::ws",
                        ?requested,
                        "StartCapture rejected: output device not found"
                    );
                    vec![AgentMessage::CaptureError {
                        reason: "output-device-not-found".into(),
                        requested_device: requested,
                        detail: None,
                    }]
                }
                Err(crate::pipeline::CaptureStartError::Other(msg)) => {
                    vec![AgentMessage::Error { message: msg }]
                }
            }
        }

        BrowserMessage::AddStream { producer_id, sfu_ip, sfu_port, payload_type: _, srtp_parameters, .. } => {
            tracing::info!(
                target: "jamodio::ws",
                producer = &producer_id[..8.min(producer_id.len())],
                sfu = format!("{}:{}", sfu_ip, sfu_port),
                "AddStream"
            );
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::Error { message: "agent overloaded".into() }];
            };
            match pl.add_stream(producer_id.clone(), sfu_ip, sfu_port, srtp_parameters).await {
                Ok((local_port, agent_srtp)) => vec![AgentMessage::LocalPort {
                    producer_id,
                    port: local_port,
                    srtp_parameters: agent_srtp,
                }],
                Err(e) => vec![AgentMessage::Error { message: e }],
            }
        }

        BrowserMessage::RemoveStream { producer_id } => {
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::Error { message: "agent overloaded".into() }];
            };
            pl.remove_stream(&producer_id);
            vec![]
        }

        BrowserMessage::SetVolume { producer_id, volume } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_volume(&producer_id, volume);
            vec![]
        }

        BrowserMessage::SetBuffer { target_ms } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_target_ms_all(target_ms as usize);
            tracing::info!(target: "jamodio::ws", target_ms, "SetBuffer");
            vec![]
        }

        BrowserMessage::SetSelfMonitorVolume { volume } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            // Clamp défensif côté agent (le mixer clampe déjà dans
            // [0, 1.5] mais on filtre les NaN ici). 0 = silence (défaut).
            let v = if volume.is_finite() { volume.max(0.0) } else { 0.0 };
            pl.mixer.lock().set_self_monitor_volume(v);
            tracing::info!(target: "jamodio::ws", volume = v, "SetSelfMonitorVolume");
            vec![]
        }

        BrowserMessage::GetStats => {
            // GetStats est appelé en heartbeat (toutes les 1.5 s). Si on ne peut
            // pas acquérir le lock dans 200 ms, on répond Error pour que le
            // browser sache que l'agent est saturé (au lieu de timeout watchdog 3 s).
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::Error { message: "agent overloaded".into() }];
            };
            let is_capturing = matches!(pl.state, AgentState::Capturing);
            let stream_count = pl.recv_stops.len();
            // L'UI agent affiche le nom lisible (pas l'id complet `{idx}:{name}`).
            // On extrait la part nom de l'id sélectionné.
            let device_name = pl.selected_input_id().and_then(|id| {
                id.split_once(':').map(|(_, n)| n.to_string()).or(Some(id))
            });

            // Real latency from CPAL buffer: samples / 48000 * 1000.
            // input/output sont des Option<u32> côté pipeline (Some si
            // BufferSize::Fixed appliqué, None si fallback Default — taille
            // réelle inconnue côté agent). Pour les composants de la latence
            // totale on est obligés d'estimer le None — on prend 10 ms
            // (= 480 samples / 48), la valeur conservatrice WASAPI shared
            // standard, alignée sur les recommandations FarPlay/Jamulus pour
            // ce mode. Les champs wire `inputBufferMs` / `outputBufferMs`
            // restent absents (= None) pour ne pas mentir.
            const DEFAULT_BUF_MS_FALLBACK: f32 = 10.0;
            let input_buf_ms_opt: Option<f32> = pl.input_buffer_samples.map(|n| n as f32 / 48.0);
            let output_buf_ms_opt: Option<f32> = pl.output_buffer_samples.map(|n| n as f32 / 48.0);
            let input_buf_ms_est = input_buf_ms_opt.unwrap_or(DEFAULT_BUF_MS_FALLBACK);
            let output_buf_ms_est = output_buf_ms_opt.unwrap_or(DEFAULT_BUF_MS_FALLBACK);
            let opus_ms: f32 = 2.5; // Opus frame 120 samples @ 48kHz

            let mixer = pl.mixer.lock();
            let underruns = mixer.total_underruns();
            let jitter_target_ms = mixer.mean_target_ms();
            drop(mixer);

            // Total = input_buf + opus_enc + opus_dec + jitter + output_buf
            // (cf. doc `Stats::total_latency_ms`). Utilise les estimations
            // input/output séparées au lieu du double-buf hérité — corrige
            // le calcul faux sur Windows shared où in et out peuvent diverger.
            let total_latency_ms = if is_capturing {
                input_buf_ms_est + opus_ms + opus_ms + jitter_target_ms + output_buf_ms_est
            } else {
                0.0
            };

            vec![
                make_status(pl.state.clone()),
                AgentMessage::Stats {
                    device: device_name,
                    capture_latency_ms: if is_capturing { input_buf_ms_est + opus_ms } else { 0.0 },
                    playback_latency_ms: if is_capturing { output_buf_ms_est } else { 0.0 },
                    // bufferMs (rétrocompat) = input (sémantique historique
                    // utilisée par les browsers pré-Q3 qui le sommaient avec
                    // opus_ms pour estimer la capture).
                    buffer_ms: if is_capturing { input_buf_ms_est } else { 0.0 },
                    input_buffer_ms: input_buf_ms_opt,
                    output_buffer_ms: output_buf_ms_opt,
                    jitter_target_ms,
                    total_latency_ms,
                    streams: stream_count,
                    underruns,
                },
            ]
        }

        BrowserMessage::Stop => {
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::Error { message: "agent overloaded".into() }];
            };
            pl.stop_all();
            vec![make_status(AgentState::Idle)]
        }

        BrowserMessage::SetInputCut { cut } => {
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.set_input_cut(cut);
            vec![]
        }

        BrowserMessage::SetMasterVolume { volume } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_master_gain(volume);
            vec![]
        }

        BrowserMessage::SetPan { producer_id, pan } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_pan(&producer_id, pan);
            vec![]
        }

        BrowserMessage::SetDim { factor } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_dim(factor);
            vec![]
        }

        // Sprint INSERT — 6 handlers plugin (AU sur macOS, VST3 sur Windows).
        // Sur les OS sans host plugin (linux test), fallback "not supported".
        BrowserMessage::ListPlugins => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                let (items, scanning) = pl.list_instrument_plugins();
                vec![AgentMessage::PluginList { items, scanning }]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![AgentMessage::PluginList { items: vec![], scanning: false }]
            }
        }

        BrowserMessage::LoadInstrumentPlugin { plugin_ref } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // Chantier A — on ne tient PAS le lock PipelineState pendant le
                // load natif (0,4–4 s) : on clone le bundle d'Arcs (cheap) puis
                // on relâche immédiatement. Le thread audio passe en dry
                // (handle=None + try_lock) et perfstats_task n'est pas bloqué.
                let ctrl = {
                    let Some(pl) = try_lock_pipeline(pipeline).await else {
                        return vec![AgentMessage::InstrumentPluginError {
                            message: "agent overloaded".into(),
                        }];
                    };
                    pl.plugin_control()
                };
                // Sérialise vs un autre load/unload en cours, puis exécute le
                // load natif sur le pool blocking (ne bloque pas le runtime
                // tokio ni les autres handlers/tasks).
                let _ops = plugin_ops_lock().lock().await;
                let pref = plugin_ref.clone();
                let result =
                    tokio::task::spawn_blocking(move || ctrl.load(&pref)).await;
                // `spawn_blocking` ne panique que si la task panique : on traite
                // le JoinError comme une erreur de chargement plutôt que de
                // propager un panic dans le handler WS.
                let result = match result {
                    Ok(inner) => inner,
                    Err(join_err) => Err(format!("plugin load task failed: {join_err}")),
                };
                match result {
                    Ok((name, latency_samples, has_editor)) => {
                        vec![AgentMessage::InstrumentPluginLoaded {
                            name,
                            plugin_ref: plugin_ref.clone(),
                            latency_samples,
                            has_editor,
                            // Reset au load — l'agent met bypass à false dans
                            // PluginControl::load, on miroite pour le wire.
                            bypass: false,
                        }]
                    }
                    Err(message) => {
                        // v0.2.23 — Log enrichi avec l'identifiant plugin pour
                        // permettre le diag à distance sans Chrome console
                        // (cf. bug Yannick 2026-05-13). Format diffère AU/VST3.
                        let ident: String = match &plugin_ref {
                            jamodio_audio_core::plugin_host::PluginRef::Au {
                                au_type, subtype, manufacturer,
                            } => format!("AU {au_type}/{subtype}/{manufacturer}"),
                            jamodio_audio_core::plugin_host::PluginRef::Vst3 {
                                path, uid,
                            } => format!("VST3 {path} (uid={uid})"),
                        };
                        tracing::error!(
                            target: "jamodio::ws",
                            plugin = %ident,
                            error = %message,
                            "LoadInstrumentPlugin failed"
                        );
                        vec![AgentMessage::InstrumentPluginError { message }]
                    }
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = plugin_ref;
                vec![AgentMessage::InstrumentPluginError {
                    message: "INSERT plugins not supported on this platform".into(),
                }]
            }
        }

        BrowserMessage::UnloadInstrumentPlugin => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // Chantier A — même principe que le load : clone le bundle,
                // relâche le lock PipelineState, teardown natif sur le pool
                // blocking (le thread audio est déjà passé en dry dès que
                // PluginControl::unload pose handle=None).
                let ctrl = {
                    let Some(pl) = try_lock_pipeline(pipeline).await else {
                        return vec![];
                    };
                    pl.plugin_control()
                };
                let _ops = plugin_ops_lock().lock().await;
                let _ = tokio::task::spawn_blocking(move || ctrl.unload()).await;
                vec![AgentMessage::InstrumentPluginUnloaded]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![AgentMessage::InstrumentPluginUnloaded]
            }
        }

        BrowserMessage::SetInstrumentPluginBypass { bypass } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                pl.set_instrument_plugin_bypass(bypass);
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = bypass;
                vec![]
            }
        }

        BrowserMessage::OpenInstrumentPluginEditor => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                if let Err(message) = pl.open_instrument_plugin_editor() {
                    return vec![AgentMessage::InstrumentPluginError { message }];
                }
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![]
            }
        }

        BrowserMessage::CloseInstrumentPluginEditor => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                let _ = pl.close_instrument_plugin_editor();
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![]
            }
        }

        // Sprint INSERT instruments (S2) — discovery + selection MIDI input.
        BrowserMessage::ListMidiDevices => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let devices = crate::audio::midi::list_devices()
                    .into_iter()
                    .map(|d| jamodio_audio_core::protocol::MidiDeviceWire {
                        id: d.id,
                        name: d.name,
                        is_default: d.is_default,
                    })
                    .collect();
                vec![AgentMessage::MidiDeviceList { devices }]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![AgentMessage::MidiDeviceList { devices: vec![] }]
            }
        }

        BrowserMessage::SetInputSource { source, midi_device_id } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                    return vec![AgentMessage::InputSourceError {
                        message: "agent overloaded".into(),
                    }];
                };
                let new_source = match source.as_str() {
                    "audio" => crate::pipeline::InputSource::Audio,
                    "midi" => match midi_device_id {
                        Some(id) => crate::pipeline::InputSource::Midi(id),
                        None => {
                            return vec![AgentMessage::InputSourceError {
                                message: "midiDeviceId required when source=midi".into(),
                            }];
                        }
                    },
                    _ => {
                        return vec![AgentMessage::InputSourceError {
                            message: format!("unknown source: {source}"),
                        }];
                    }
                };

                match pl.set_input_source(new_source.clone()) {
                    Ok(()) => {
                        let (src_str, dev_id, dev_name) = match &new_source {
                            crate::pipeline::InputSource::Audio => {
                                ("audio".to_string(), None, None)
                            }
                            crate::pipeline::InputSource::Midi(id) => {
                                let name = crate::audio::midi::list_devices()
                                    .into_iter()
                                    .find(|d| &d.id == id)
                                    .map(|d| d.name);
                                ("midi".to_string(), Some(id.clone()), name)
                            }
                        };
                        tracing::info!(
                            target: "jamodio::midi",
                            source = %src_str,
                            device_id = ?dev_id,
                            "input source changed"
                        );
                        vec![AgentMessage::InputSourceChanged {
                            source: src_str,
                            midi_device_id: dev_id,
                            midi_device_name: dev_name,
                        }]
                    }
                    Err(message) => {
                        tracing::error!(
                            target: "jamodio::midi",
                            error = %message,
                            "set_input_source failed"
                        );
                        vec![AgentMessage::InputSourceError { message }]
                    }
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = (source, midi_device_id);
                vec![AgentMessage::InputSourceError {
                    message: "MIDI input not supported on this platform".into(),
                }]
            }
        }

        BrowserMessage::PlayMidiNote { status, data1, data2 } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                let handle_opt = *pl.instrument_plugin_handle.lock();
                if let Some(handle) = handle_opt {
                    // Clavier HTML virtuel (browser → WS → agent) : pas de
                    // source temporelle précise (mousedown/keydown soumis au
                    // lag UI + transit WebSocket). `frame_offset: 0` est
                    // l'approximation honnête — l'event est joué au tout
                    // début du prochain bloc rendu par dispatch_midi_only.
                    // Le path MIDI USB physique passe par `midi.rs` qui, lui,
                    // est sample-accurate via `CapturedMidiEvent::captured_at`.
                    let event = jamodio_audio_core::plugin_host::MidiEvent {
                        frame_offset: 0,
                        data: [status, data1, data2],
                    };
                    // Mac (AuHost) ET Win (Vst3Host) implémentent tous deux
                    // `dispatch_midi_only` avec la même signature → call
                    // OS-agnostic via le champ `plugin_host` aliasé.
                    let _ = pl.plugin_host.lock().dispatch_midi_only(handle, &[event]);
                }
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = (status, data1, data2);
                vec![]
            }
        }

        BrowserMessage::StartRecording { stems } => {
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::Error { message: "agent overloaded".into() }];
            };
            // Convertit le wire StemSpec → record::StemSpec (même contenu,
            // module différent pour découpler le protocol du core record).
            let specs: Vec<StemSpec> = stems.iter().map(|s| StemSpec {
                role: s.role.clone(),
                peer_id: s.peer_id.clone(),
                peer_name: s.peer_name.clone(),
            }).collect();
            tracing::info!(target: "jamodio::ws", count = specs.len(), "StartRecording");
            match pl.start_recording(specs) {
                Ok(armed) => {
                    let wire: Vec<RecordStemSpec> = armed.iter().map(|s| RecordStemSpec {
                        role: s.role.clone(),
                        peer_id: s.peer_id.clone(),
                        peer_name: s.peer_name.clone(),
                    }).collect();
                    vec![AgentMessage::RecordingStarted { stems: wire }]
                }
                Err(msg) => {
                    tracing::error!(target: "jamodio::ws", error = %msg, "start_recording failed");
                    vec![AgentMessage::RecordingError { message: msg }]
                }
            }
        }

        BrowserMessage::StopRecording => {
            // stop_recording bloque jusqu'à finalize (timeout 30s côté handle).
            // On délègue à spawn_blocking pour ne pas bloquer le runtime tokio
            // pendant l'encodage final + lock pipeline.
            let pipeline = pipeline.clone();
            let files_opt = tokio::task::spawn_blocking(move || {
                // Le blocking thread prend le lock pipeline en sync via
                // blocking_lock (Tokio Mutex supporte blocking_lock).
                let mut pl = pipeline.blocking_lock();
                pl.stop_recording()
            })
            .await;
            let files = match files_opt {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(target: "jamodio::ws", error = %e, "stop_recording join error");
                    return vec![AgentMessage::RecordingError {
                        message: format!("stop task failed: {e}"),
                    }];
                }
            };
            tracing::info!(
                target: "jamodio::ws",
                count = files.len(),
                bytes_total = files.iter().map(|f| f.data.len()).sum::<usize>(),
                "StopRecording — encoding base64",
            );
            // Base64 encode chaque fichier. Un seul message à la fin évite
            // le chunking côté browser ; WS frame supporte les payloads MB.
            let b64 = base64::engine::general_purpose::STANDARD;
            let wire_files: Vec<RecordedFileWire> = files.into_iter().map(|f| RecordedFileWire {
                role: f.spec.role,
                peer_id: f.spec.peer_id,
                peer_name: f.spec.peer_name,
                mime_type: "audio/ogg".into(),
                extension: "opus".into(),
                data_b64: b64.encode(&f.data),
            }).collect();
            vec![AgentMessage::RecordingDone { files: wire_files }]
        }

        BrowserMessage::GetLogsArchive { max_days, max_bytes } => {
            // I/O disque dans une tâche bloquante pour ne pas geler le runtime
            // tokio (lecture de plusieurs MB peut prendre 50-200ms sur HDD).
            let days = max_days.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_DAYS);
            let bytes = max_bytes.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_BYTES);
            let res = tokio::task::spawn_blocking(move || {
                let (content, files, truncated) = crate::logging::collect_recent_logs(days, bytes);
                let log_dir = crate::logging::log_dir().to_string_lossy().into_owned();
                (content, files, truncated, log_dir)
            })
            .await;
            match res {
                Ok((content, files, truncated, log_dir)) => {
                    tracing::info!(
                        target: "jamodio::support",
                        files = files.len(),
                        bytes = content.len(),
                        truncated,
                        "GetLogsArchive served"
                    );
                    vec![AgentMessage::LogsArchive {
                        content,
                        files,
                        truncated,
                        log_dir,
                    }]
                }
                Err(e) => vec![AgentMessage::Error {
                    message: format!("logs archive task failed: {e}"),
                }],
            }
        }
    }
}
