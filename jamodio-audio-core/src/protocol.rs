//! JSON protocol types for browser ↔ agent communication via localhost WebSocket.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Browser → Agent ───────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BrowserMessage {
    GetDevices,
    SelectDevices {
        #[serde(rename = "inputId")]
        input_id: Option<String>,
        #[serde(rename = "outputId")]
        output_id: Option<String>,
    },
    StartCapture {
        ssrc: u32,
        #[serde(rename = "sfuIp")]
        sfu_ip: String,
        #[serde(rename = "sfuPort")]
        sfu_port: u16,
        #[serde(rename = "payloadType")]
        payload_type: u8,
        #[serde(rename = "inputDevice")]
        input_device: Option<String>,
        /// Canal mono à extraire (0..N-1). Si `None`, capture stéréo standard.
        #[serde(rename = "channelIndex", default)]
        channel_index: Option<u8>,
    },
    AddStream {
        #[serde(rename = "producerId")]
        producer_id: String,
        #[serde(rename = "producerPeerId")]
        producer_peer_id: Option<String>,
        #[serde(rename = "sfuIp")]
        sfu_ip: String,
        #[serde(rename = "sfuPort")]
        sfu_port: u16,
        #[serde(rename = "payloadType")]
        payload_type: u8,
    },
    RemoveStream {
        #[serde(rename = "producerId")]
        producer_id: String,
    },
    SetVolume {
        #[serde(rename = "producerId")]
        producer_id: String,
        volume: f32,
    },
    SetBuffer {
        #[serde(rename = "targetMs")]
        target_ms: u32,
    },
    GetStats,
    Stop,
}

// ─── Agent → Browser ───────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AgentMessage {
    Devices {
        inputs: Vec<AudioDevice>,
        outputs: Vec<AudioDevice>,
    },
    Status {
        state: AgentState,
    },
    Stats {
        device: Option<String>,
        #[serde(rename = "captureLatencyMs")]
        capture_latency_ms: f32,
        #[serde(rename = "playbackLatencyMs")]
        playback_latency_ms: f32,
        #[serde(rename = "bufferMs")]
        buffer_ms: f32,
        streams: usize,
        underruns: u64,
    },
    Vu {
        #[serde(rename = "inputRms")]
        input_rms: f32,
        streams: HashMap<String, f32>,
    },
    Error {
        message: String,
    },
    /// Agent reports the local UDP port it's receiving on (for SFU connect).
    LocalPort {
        #[serde(rename = "producerId")]
        producer_id: String,
        port: u16,
    },
    /// Per-stream RMS levels for VU meters.
    StreamLevels {
        levels: Vec<StreamLevel>,
    },
}

#[derive(Debug, Serialize)]
pub struct StreamLevel {
    #[serde(rename = "producerId")]
    pub producer_id: String,
    pub rms: f32,
}

#[derive(Debug, Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
    /// Nombre de canaux physiques exposés par le device (config par défaut CPAL).
    /// Permet au browser de restreindre le sélecteur "canal d'entrée" au vrai nombre
    /// de canaux disponibles (ex : 2 pour Scarlett Solo, 4 pour 4i4, 18 pour 18i20).
    pub channels: u16,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Capturing,
    Error,
}
