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
            eprintln!("[Jamodio] Port 9876 already in use — another instance running?");
            return;
        }
    };

    eprintln!("[Jamodio] Listening on ws://localhost:9876");
    axum::serve(listener, app).await.unwrap();
}

async fn handle_connection(socket: WebSocket, pipeline: Arc<tokio::sync::Mutex<PipelineState>>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Send initial status
    let status = AgentMessage::Status {
        state: AgentState::Idle,
    };
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
            vec![AgentMessage::Status {
                state: AgentState::Idle,
            }]
        }

        BrowserMessage::StartCapture { ssrc, sfu_ip, sfu_port, payload_type: _, input_device, channel_index, srtp_parameters } => {
            eprintln!("[Jamodio] StartCapture ssrc={} → {}:{} device={:?} channel={:?}",
                ssrc, sfu_ip, sfu_port, input_device, channel_index);
            let mut pl = pipeline.lock().await;
            // Le browser peut passer le device directement dans start-capture
            // (le plus fiable — select-devices pouvait ne jamais arriver).
            if input_device.is_some() {
                pl.select_devices(input_device, None);
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
            eprintln!("[Jamodio] AddStream {} → {}:{}", &producer_id[..8.min(producer_id.len())], sfu_ip, sfu_port);
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

        BrowserMessage::SetBuffer { .. } => {
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
            let opus_ms: f32 = 2.5; // Opus frame 120 samples @ 48kHz (Phase 2)

            vec![
                AgentMessage::Status {
                    state: pl.state.clone(),
                },
                AgentMessage::Stats {
                    device: device_name,
                    capture_latency_ms: if is_capturing { buf_ms + opus_ms } else { 0.0 },
                    playback_latency_ms: if is_capturing { buf_ms } else { 0.0 },
                    buffer_ms: if is_capturing { buf_ms } else { 0.0 },
                    streams: stream_count,
                    underruns: 0,
                },
            ]
        }

        BrowserMessage::Stop => {
            pipeline.lock().await.stop_all();
            vec![AgentMessage::Status {
                state: AgentState::Idle,
            }]
        }
    }
}
