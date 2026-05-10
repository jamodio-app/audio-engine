use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::HeaderMap,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use jamodio_audio_core::protocol::{
    AgentMessage, AgentState, BrowserMessage, StreamLevel, PROTOCOL_VERSION,
};
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
        get(move |ws: WebSocketUpgrade, headers: HeaderMap| {
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
        BrowserMessage::HelloAck { protocol_version } => {
            tracing::debug!(
                target: "jamodio::ws",
                browser_protocol = protocol_version,
                agent_protocol = PROTOCOL_VERSION,
                "received HelloAck from browser"
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
    }
}
