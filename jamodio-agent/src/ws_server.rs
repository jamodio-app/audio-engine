use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use jamodio_audio_core::protocol::{AgentMessage, AgentState, BrowserMessage, StreamLevel};
use std::sync::Arc;
use tokio::sync::mpsc as tokio_mpsc;

use crate::audio::device;
use crate::pipeline::PipelineState;

/// Construit un `AgentMessage::Status` avec la version + OS + arch de l'agent.
/// Centralisé ici pour ne pas dupliquer ces 3 champs à chaque site (init WS,
/// reset après stop, retour de SelectDevices, etc.). Le browser lit ces champs
/// pour afficher un banner "agent obsolète" si la version est en retard.
fn make_status(state: AgentState) -> AgentMessage {
    AgentMessage::Status {
        state,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
    }
}

/// Start the localhost WebSocket server on port 9876.
pub async fn start(pipeline: Arc<tokio::sync::Mutex<PipelineState>>) {
    let app = Router::new().route(
        "/",
        get(move |ws: WebSocketUpgrade| {
            let pipeline = pipeline.clone();
            async move { ws.on_upgrade(move |socket| handle_connection(socket, pipeline)) }
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

async fn handle_connection(socket: WebSocket, pipeline: Arc<tokio::sync::Mutex<PipelineState>>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Send initial status (avec version/os/arch — détection obsolescence côté browser)
    let status = make_status(AgentState::Idle);
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&status).unwrap()))
        .await;

    // Channel for outgoing messages (from message handler + periodic tasks)
    let (out_tx, mut out_rx) = tokio_mpsc::channel::<AgentMessage>(64);

    // Spawn periodic StreamLevels sender (every 100ms)
    let levels_pipeline = pipeline.clone();
    let levels_tx = out_tx.clone();
    let levels_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            let pl = levels_pipeline.lock().await;
            let rms_data = pl.mixer.lock().stream_rms();
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

        let Ok(browser_msg) = serde_json::from_str::<BrowserMessage>(&text) else {
            let err = AgentMessage::Error {
                message: format!("Invalid message: {}", &text[..text.len().min(100)]),
            };
            let _ = out_tx.send(err).await;
            continue;
        };

        let responses = handle_message(browser_msg, &pipeline).await;
        for resp in responses {
            if out_tx.send(resp).await.is_err() {
                break;
            }
        }
    }

    levels_task.abort();
    send_task.abort();
    pipeline.lock().await.stop_all();
}

async fn handle_message(
    msg: BrowserMessage,
    pipeline: &Arc<tokio::sync::Mutex<PipelineState>>,
) -> Vec<AgentMessage> {
    match msg {
        BrowserMessage::GetDevices => {
            let inputs = device::list_inputs();
            let outputs = device::list_outputs();
            vec![AgentMessage::Devices { inputs, outputs }]
        }

        BrowserMessage::SelectDevices { input_id, output_id } => {
            pipeline.lock().await.select_devices(input_id, output_id);
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
            let mut pl = pipeline.lock().await;
            // Le browser peut passer le device directement dans start-capture
            // (le plus fiable — select-devices pouvait ne jamais arriver).
            // ⚠ Bug fix : utiliser set_input_device() au lieu de select_devices(_, None)
            //   qui écrasait à None l'output_device_name précédemment configuré
            //   → fallback sur device par défaut système (Mac speakers).
            if input_device.is_some() {
                pl.set_input_device(input_device);
            }
            match pl.start_capture(ssrc, sfu_ip.clone(), sfu_port, 111, channel_index, srtp_parameters).await {
                Ok((local_port, agent_srtp)) => {
                    vec![AgentMessage::LocalPort {
                        producer_id: String::new(),
                        port: local_port,
                        srtp_parameters: agent_srtp,
                    }]
                }
                Err(e) => vec![AgentMessage::Error { message: e }],
            }
        }

        BrowserMessage::AddStream { producer_id, sfu_ip, sfu_port, payload_type: _, srtp_parameters, .. } => {
            tracing::info!(
                target: "jamodio::ws",
                producer = &producer_id[..8.min(producer_id.len())],
                sfu = format!("{}:{}", sfu_ip, sfu_port),
                "AddStream"
            );
            let mut pl = pipeline.lock().await;
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
            pipeline.lock().await.remove_stream(&producer_id);
            vec![]
        }

        BrowserMessage::SetVolume { producer_id, volume } => {
            pipeline.lock().await.mixer.lock().set_volume(&producer_id, volume);
            vec![]
        }

        BrowserMessage::SetBuffer { target_ms } => {
            let pl = pipeline.lock().await;
            pl.mixer.lock().set_target_ms_all(target_ms as usize);
            tracing::info!(target: "jamodio::ws", target_ms, "SetBuffer");
            vec![]
        }

        BrowserMessage::GetStats => {
            let pl = pipeline.lock().await;
            let is_capturing = matches!(pl.state, AgentState::Capturing);
            let stream_count = pl.recv_stops.len();
            let device_name = pl.selected_input_name();

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

            // Latence end-to-end estimée : capture + encode + decode + jitter + playback.
            // Calculée ici (côté agent) pour éviter les double-comptages côté UI :
            // les champs capture_latency_ms / playback_latency_ms / buffer_ms sont
            // exposés séparément pour debug mais NE DOIVENT PAS être additionnés naïvement.
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
            pipeline.lock().await.stop_all();
            vec![make_status(AgentState::Idle)]
        }
    }
}
