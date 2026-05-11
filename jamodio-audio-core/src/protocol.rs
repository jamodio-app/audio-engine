//! JSON protocol types for browser ↔ agent communication via localhost WebSocket.
//!
//! ## Versioning
//!
//! `PROTOCOL_VERSION` est embarqué dans le `Hello` envoyé à chaque connexion.
//! Le browser peut comparer pour adapter son comportement (compat ascendante :
//! le browser tolère une version plus ancienne, mode legacy).
//!
//! Historique :
//!  - v1 (v0.2.0+) : ajout de `Hello`, `HelloAck`, `Shutdown`. Single-client policy.
//!  - v0 implicite (v0.1.x) : pas de `Hello`. Browser detecte ça via timeout 1.5 s.
//!
//! Convention : tout ajout de champ obligatoire = bump majeur. Ajouts optionnels OK.

use crate::net::srtp::SrtpParameters;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Version du protocole. À bumper sur tout breaking change wire-format.
pub const PROTOCOL_VERSION: u32 = 1;

// ─── Browser → Agent ───────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BrowserMessage {
    /// Acknowledgement du `Hello` agent. Optionnel (le browser peut ne pas
    /// répondre, l'agent continue quand même). Sert au futur tracking +
    /// confirmation que le browser a bien parsé le Hello.
    /// `session_id` (UUID v4 généré côté browser, persisté `sessionStorage`)
    /// est logué côté agent pour permettre au support de croiser les logs
    /// browser et agent à partir de l'identifiant qui apparaît dans les
    /// 2 fichiers (header logger.js d'un côté, log line agent de l'autre).
    HelloAck {
        #[serde(rename = "protocolVersion")]
        protocol_version: u32,
        #[serde(rename = "sessionId", default)]
        session_id: Option<String>,
    },
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
        /// Clés SRTP du SFU (chiffrement des paquets SFU → agent).
        /// Le browser les a reçues dans `plain-transport-created`.
        #[serde(rename = "srtpParameters")]
        srtp_parameters: SrtpParameters,
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
        /// Clés SRTP du SFU pour ce flux (reçues dans `plain-consumer-created`).
        #[serde(rename = "srtpParameters")]
        srtp_parameters: SrtpParameters,
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
    /// Volume du stream de self-monitor (capture locale rebouclée en sortie
    /// agent → casque). 0.0 = silence par défaut, 1.0 = unity. Sans ce
    /// message après StartCapture, l'utilisateur ne s'entend pas via l'agent
    /// (fail-safe anti-larsen au démarrage).
    SetSelfMonitorVolume {
        volume: f32,
    },
    /// Sprint B — delay d'alignement de latence pour un peer remote.
    /// Le serveur SFU calcule `delay = maxHalfRtt − peerHalfRtt` (cf.
    /// server/latency-equalizer.js) et le browser le relaie ici. L'agent
    /// retarde le playout du stream concerné en augmentant la cible du
    /// jitter buffer de `delay_ms`. En mode browser, la même valeur est
    /// appliquée via `consumer.rtpReceiver.playoutDelayHint` côté Chrome ;
    /// en mode agent (ce message), c'est l'agent qui aligne les peers.
    SetPeerDelay {
        #[serde(rename = "producerId")]
        producer_id: String,
        #[serde(rename = "delayMs")]
        delay_ms: u32,
    },
    GetStats,
    Stop,
    /// Demande à l'agent les N derniers jours de logs concaténés en plain
    /// text. Utilisé par le module Support browser pour packager un
    /// bug-report avec les logs des 2 côtés en un seul fichier .txt.
    /// Les logs sont coupés à `max_bytes` (les plus anciens tronqués en
    /// premier) pour rester sous la limite d'attachment Resend (25 MB).
    GetLogsArchive {
        /// Nombre max de fichiers journaliers à inclure (défaut 3).
        #[serde(rename = "maxDays")]
        max_days: Option<u32>,
        /// Taille max totale en bytes après concaténation (défaut 5_000_000).
        #[serde(rename = "maxBytes")]
        max_bytes: Option<u64>,
    },
    /// Coupe (true) ou ouvre (false) l'entrée capture côté agent. Quand
    /// coupée, l'encoder_thread remplit les samples capturés par des zéros
    /// avant tout traitement (RMS, self-monitor, record self stem, mix,
    /// envoi RTP). Équivalent du bouton « ENTRÉE OFF » du browser ; en
    /// mode browser sans agent, c'était `track.enabled = false` côté WebRTC.
    SetInputCut {
        cut: bool,
    },
    /// Volume du fader MASTER (sortie globale agent → casque). Appliqué
    /// dans `AudioMixer::mix_into` après la somme des streams et avant le
    /// clamp soft. En mode browser sans agent, cette commande est sans
    /// effet (le master Web Audio est piloté localement via masterGain).
    /// Clamp défensif côté agent dans [0.0, 1.5].
    SetMasterVolume {
        volume: f32,
    },
    /// REC-2/REC-3 — démarre l'enregistrement multi-stems côté agent.
    /// Le browser fournit la liste des stems armés (self + peers + mix).
    /// L'agent active les tap sites du mixer et démarre un thread record
    /// avec un OpusOggRecorder par stem.
    StartRecording {
        stems: Vec<RecordStemSpec>,
    },
    /// REC-2/REC-3 — stop l'enregistrement courant. L'agent répond avec
    /// `RecordingDone` contenant les fichiers Ogg/Opus en base64.
    StopRecording,
}

/// Spec d'un stem à enregistrer, transmise par le browser au start.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RecordStemSpec {
    /// "stem-self" | "stem-peer" | "mix"
    pub role: String,
    /// producer_id du peer (clé du mixer agent). null pour `mix`, myUserId
    /// pour `stem-self` (informatif côté agent, le tap ne dépend pas de la
    /// valeur — il route via le role).
    #[serde(rename = "peerId", default)]
    pub peer_id: Option<String>,
    /// Nom lisible (pour fichier final côté browser).
    #[serde(rename = "peerName", default)]
    pub peer_name: Option<String>,
}

// ─── Agent → Browser ───────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AgentMessage {
    /// Premier message envoyé au browser dès l'open WebSocket. Contient la
    /// version du protocole + métadonnées agent. Le browser utilise ce message
    /// pour transitionner sa state machine `HANDSHAKING → CONNECTED`.
    /// Si le browser ne reçoit PAS ce message en 1.5 s, il assume un agent
    /// legacy (≤ v0.1.7) et passe en mode permissif (legacyMode = true).
    Hello {
        #[serde(rename = "protocolVersion")]
        protocol_version: u32,
        /// Version du binaire (CARGO_PKG_VERSION).
        #[serde(rename = "agentVersion")]
        agent_version: String,
        os: String,
        arch: String,
        /// Capabilities optionnelles (futur extensible). Vide en v1.
        #[serde(default)]
        capabilities: Vec<String>,
    },
    /// Notification de shutdown imminent (auto-update, quit user, etc.).
    /// Le browser doit considérer la WS comme partant et préparer un fallback.
    Shutdown {
        reason: String,
    },
    /// Notification de rejet d'une connexion (single-client policy).
    /// Envoyé immédiatement avant fermeture de la 2e WS si une 1re est déjà active.
    Rejected {
        reason: String,
    },
    Devices {
        inputs: Vec<AudioDevice>,
        outputs: Vec<AudioDevice>,
    },
    Status {
        state: AgentState,
        /// Version du binaire (CARGO_PKG_VERSION). Utilisée par le browser pour
        /// détecter si l'agent est obsolète et afficher un banner "Mise à jour
        /// disponible". Optionnel pour rester rétro-compatible avec d'éventuels
        /// codes browser anciens qui ignorent les champs inconnus.
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// `os` / `arch` côté agent — utiles pour debug à distance et pour le
        /// banner browser (ex : suggérer le bon installer macOS ARM/Intel).
        #[serde(skip_serializing_if = "Option::is_none")]
        os: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        arch: Option<String>,
    },
    Stats {
        device: Option<String>,
        /// Latence d'encodage côté capture : buffer CPAL + frame Opus.
        #[serde(rename = "captureLatencyMs")]
        capture_latency_ms: f32,
        /// Latence playback : buffer CPAL output uniquement.
        #[serde(rename = "playbackLatencyMs")]
        playback_latency_ms: f32,
        /// Buffer CPAL I/O en ms (capture/playback). Identique côté in/out
        /// car on utilise BufferSize::Fixed(128) des deux côtés.
        #[serde(rename = "bufferMs")]
        buffer_ms: f32,
        /// Cible adaptative du jitter buffer (moyenne des streams actifs, ms).
        /// 0 si aucun stream actif. C'est le levier principal de tuning latence
        /// vs robustesse au jitter — affiché dans l'UI agent.
        #[serde(rename = "jitterTargetMs")]
        jitter_target_ms: f32,
        /// Latence end-to-end agent estimée (capture + encode + decode + jitter + playback).
        /// Pré-calculée côté agent pour éviter les double-comptages côté UI.
        #[serde(rename = "totalLatencyMs")]
        total_latency_ms: f32,
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
    /// Confirmation explicite de la capture démarrée. Permet au browser de
    /// vérifier que le device ouvert correspond bien à celui demandé.
    /// Pas de fallback silencieux : si on est ici, le device demandé EST
    /// celui ouvert (cf. CaptureError pour le cas inverse).
    CaptureStarted {
        /// Id complet du device tel que renvoyé par GetDevices (`{idx}:{name}`).
        #[serde(rename = "deviceId")]
        device_id: String,
        /// Nom lisible du device (= la part après `:` de l'id).
        #[serde(rename = "deviceName")]
        device_name: String,
        /// Nombre de canaux physiques effectivement ouverts.
        channels: u16,
    },
    /// Erreur explicite quand la capture ne peut pas démarrer parce que le
    /// device demandé n'est pas trouvé. Plus de silent fallback to default :
    /// le browser doit afficher un toast et forcer l'ouverture de Settings.
    CaptureError {
        /// "device-not-found" | "io-error" | autre (extensible).
        reason: String,
        /// Id ou nom demandé par le browser (pour message UI).
        #[serde(rename = "requestedDevice")]
        requested_device: Option<String>,
        /// Détail technique facultatif (logs/debug).
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Agent reports the local UDP port + SRTP keys.
    /// Le browser doit relayer `srtpParameters` au SFU via `connect-plain-transport`
    /// (clés agent → SFU pour le déchiffrement côté SFU).
    LocalPort {
        #[serde(rename = "producerId")]
        producer_id: String,
        port: u16,
        #[serde(rename = "srtpParameters")]
        srtp_parameters: SrtpParameters,
    },
    /// Per-stream RMS levels for VU meters.
    StreamLevels {
        levels: Vec<StreamLevel>,
    },
    /// Réponse à `GetLogsArchive`. Contient les logs agent concaténés en
    /// plain text (UTF-8), avec entêtes par fichier `====== agent.log.YYYY-MM-DD ======`.
    /// `truncated = true` indique que le plus ancien fichier a été coupé
    /// pour respecter `max_bytes`.
    LogsArchive {
        content: String,
        /// Nom des fichiers inclus, du plus ancien au plus récent.
        files: Vec<String>,
        truncated: bool,
        /// Chemin absolu du dossier de logs sur disque (pour aide UI :
        /// "tu peux aussi ouvrir directement ce dossier").
        #[serde(rename = "logDir")]
        log_dir: String,
    },
    /// REC-2/REC-3 — Ack du StartRecording. Liste les stems vraiment armés
    /// (peut différer de la requête : un peer_id inconnu côté mixer est
    /// silencieusement ignoré).
    RecordingStarted {
        stems: Vec<RecordStemSpec>,
    },
    /// REC-2/REC-3 — Réponse au StopRecording. Pour chaque stem armé, un
    /// fichier Ogg/Opus encodé en base64. Le browser décode et stocke en
    /// IndexedDB avec un sessionTag commun.
    RecordingDone {
        files: Vec<RecordedFileWire>,
    },
    /// REC-2/REC-3 — Erreur pendant l'enregistrement (init encoder, etc.).
    RecordingError {
        message: String,
    },
}

/// Fichier enregistré encodé pour le wire (base64 du Ogg/Opus complet).
#[derive(Debug, Serialize)]
pub struct RecordedFileWire {
    pub role: String,
    #[serde(rename = "peerId", skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(rename = "peerName", skip_serializing_if = "Option::is_none")]
    pub peer_name: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub extension: String,
    /// Contenu Ogg/Opus encodé en base64 standard (RFC 4648).
    #[serde(rename = "dataB64")]
    pub data_b64: String,
}

#[derive(Debug, Serialize)]
pub struct StreamLevel {
    #[serde(rename = "producerId")]
    pub producer_id: String,
    pub rms: f32,
}

#[derive(Debug, Serialize)]
pub struct AudioDevice {
    /// Identifiant stable au sein d'une enumeration : `"{index}:{name}"`.
    /// L'index disambigue les devices à nom identique (deux cartes USB
    /// génériques étiquetées pareil). Le browser stocke et renvoie
    /// EXACTEMENT cet id dans StartCapture / SelectDevices — l'agent
    /// vérifie au moment du resolve que l'index pointe toujours sur un
    /// device au même nom (sinon → CaptureError, JAMAIS de fallback
    /// silencieux sur un autre device).
    pub id: String,
    /// Nom lisible affiché à l'UI (déduit aussi de l'id si besoin).
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
