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
    AgentMessage, AgentState, BrowserMessage, RecordStemSpec, RecordedFileWire, StreamLevel,
    PROTOCOL_VERSION,
};
use jamodio_audio_core::record::StemSpec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
///   - Single-client policy via `client_active` (AtomicBool)
///   - Broadcast d'événements globaux (Shutdown sur auto-update) à tous les
///     clients connectés via `shutdown_tx` (tokio broadcast channel).
#[derive(Clone)]
pub struct WsServerHandle {
    pipeline: Arc<tokio::sync::Mutex<PipelineState>>,
    /// True quand une WS browser est connectée et active. Les WS suivantes
    /// sont rejetées (single-client policy) pour éviter les races sur le
    /// shared `PipelineState`.
    client_active: Arc<AtomicBool>,
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

async fn handle_connection(socket: WebSocket, handle: WsServerHandle, is_internal: bool) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Single-client policy : si une WS NON-INTERNE est déjà active, rejeter
    // celle-ci. Évite les races sur le shared PipelineState (2 onglets browser
    // qui se battent pour StartCapture). Les clients internes (UI Tauri webview)
    // sont autorisés en parallèle car ils sont lecture-seule (get-stats).
    //
    // compare_exchange : atomique, pas de race entre 2 connexions concurrentes.
    if !is_internal
        && handle
            .client_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
    {
        tracing::warn!(
            target: "jamodio::ws",
            "rejecting concurrent WS connection — another external client already active"
        );
        let rejected = AgentMessage::Rejected {
            reason: "another client is already connected to this agent".to_string(),
        };
        let _ = ws_tx
            .send(Message::Text(serde_json::to_string(&rejected).unwrap()))
            .await;
        let _ = ws_tx.close().await;
        return;
    }

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
            drop(pl);
            if !rms_data.is_empty() {
                let levels: Vec<StreamLevel> = rms_data
                    .into_iter()
                    .map(|(producer_id, rms)| StreamLevel { producer_id, rms })
                    .collect();
                if levels_tx.send(AgentMessage::StreamLevels { levels }).await.is_err() {
                    break;
                }
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

    // Message receive loop
    while let Some(Ok(msg)) = ws_rx.next().await {
        let Message::Text(text) = msg else { continue };

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
                continue;
            }
        };

        let responses = handle_message(browser_msg, &handle.pipeline).await;
        for resp in responses {
            if out_tx.send(resp).await.is_err() {
                break;
            }
        }
    }

    tracing::info!(target: "jamodio::ws", is_internal, "client disconnected — cleanup");

    levels_task.abort();
    send_task.abort();
    shutdown_task.abort();

    // Cleanup pipeline UNIQUEMENT pour les clients externes (jamodio.com).
    // Les clients internes (UI Tauri webview) ne pilotent pas le pipeline,
    // donc leur close ne doit PAS stop_all (sinon le browser actif perd
    // sa session quand on ferme la fenêtre Tauri par erreur).
    if !is_internal {
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
        // Libère le single-client slot uniquement si on l'avait pris (pas
        // pour les clients internes qui n'ont jamais consommé le slot).
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

        BrowserMessage::SetPeerDelay { producer_id, delay_ms } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            // Le mixer clampe à MAX_ALIGN_TARGET_MS (200) en interne via
            // ring_buffer::set_target_ms. Pas de log info ici : le broadcast
            // arrive toutes les 2 s, on logue uniquement les changements
            // significatifs côté mixer (debug + hystérèse interne).
            pl.mixer.lock().set_peer_delay(&producer_id, delay_ms);
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

            // Real latency from CPAL buffer: samples / 48000 * 1000
            let buf_ms = if is_capturing {
                pl.buffer_samples as f32 / 48.0 // 128 samples @ 48kHz = 2.67ms
            } else {
                0.0
            };
            let opus_ms: f32 = 2.5; // Opus frame 120 samples @ 48kHz

            let mixer = pl.mixer.lock();
            let underruns = mixer.total_underruns();
            let jitter_target_ms = mixer.mean_target_ms();
            drop(mixer);

            let total_latency_ms = if is_capturing {
                buf_ms + opus_ms + opus_ms + jitter_target_ms + buf_ms
            } else {
                0.0
            };

            vec![
                make_status(pl.state.clone()),
                AgentMessage::Stats {
                    device: device_name,
                    capture_latency_ms: if is_capturing { buf_ms + opus_ms } else { 0.0 },
                    playback_latency_ms: if is_capturing { buf_ms } else { 0.0 },
                    buffer_ms: if is_capturing { buf_ms } else { 0.0 },
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
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![AgentMessage::InstrumentPluginError {
                        message: "agent overloaded".into(),
                    }];
                };
                match pl.load_instrument_plugin(&plugin_ref) {
                    Ok((name, latency_samples, has_editor)) => {
                        vec![AgentMessage::InstrumentPluginLoaded {
                            name,
                            plugin_ref: plugin_ref.clone(),
                            latency_samples,
                            has_editor,
                            // Reset au load — l'agent met bypass à false dans
                            // load_instrument_plugin, on miroite pour le wire.
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
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                pl.unload_instrument_plugin();
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
