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
use crate::plugin_host::{PluginInfo, PluginRef};
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
        /// Canal de départ d'une PAIRE stéréo (L = ch[N], R = ch[N+1]). Si
        /// `None`, paire 1+2 par défaut (comportement historique). Mutuellement
        /// exclusif avec `channel_index` (mono). `default` = rétro-compatible
        /// avec les browsers qui n'envoient pas encore ce champ.
        #[serde(rename = "stereoStart", default)]
        stereo_start: Option<u8>,
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
    /// Balance L/R par stream. `producer_id` = id du flux dans le mixer
    /// agent ; convention spéciale "self" pour le self-monitor (= la
    /// tranche "moi" côté browser). Pan range [-1.0, 1.0], constant-power
    /// applied in `mix_into`. Clamp défensif côté agent.
    SetPan {
        #[serde(rename = "producerId")]
        producer_id: String,
        pan: f32,
    },
    /// DIM factor — atténuation temporaire des instruments quand l'utilisateur
    /// active DIM côté UI (= pour entendre la conversation talkback clairement).
    /// Plage [0.0, 1.0], typiquement 0.25 (-12dB) ou 1.0 (off). Appliqué dans
    /// `mix_into` après la somme des streams et AVANT le master_gain. Le tap
    /// REC est avant dim/master, donc le record n'est pas affecté (= ce qu'un
    /// peer entendrait, indépendant de mon écoute locale dim/master).
    SetDim {
        factor: f32,
    },
    /// Sprint INSERT (S1) — liste les plugins natifs installés sur la machine.
    /// L'agent répond avec `PluginList` qui contient le snapshot du cache de
    /// scan. Si le scan tourne encore (au démarrage de l'agent), `scanning =
    /// true` et `items` peut être vide → le browser repolle.
    ListPlugins,
    /// Sprint INSERT (S1) — charge un plugin sur la tranche instrument self.
    /// Réponse : `InstrumentPluginLoaded` ou `InstrumentPluginError`.
    /// Charge UN seul plugin à la fois côté MVP (1 slot) — un appel quand un
    /// plugin est déjà chargé décharge l'ancien d'abord.
    LoadInstrumentPlugin {
        #[serde(rename = "pluginRef")]
        plugin_ref: PluginRef,
    },
    /// Sprint INSERT (S1) — décharge le plugin courant. No-op si rien chargé.
    UnloadInstrumentPlugin,
    /// Sprint INSERT (S1) — toggle bypass du plugin actif (= court-circuite
    /// process_stereo dans l'encoder_thread). Pas de réponse.
    SetInstrumentPluginBypass {
        bypass: bool,
    },
    /// Sprint INSERT (S1) — ouvre la fenêtre native macOS du plugin (= GUI
    /// AmpliTube etc.). No-op silencieux si aucun plugin chargé. La fenêtre
    /// est ouverte par dispatch_async sur le main thread agent.
    OpenInstrumentPluginEditor,
    /// Sprint INSERT (S1) — ferme la fenêtre native si ouverte.
    CloseInstrumentPluginEditor,
    /// Sprint INSERT instruments (S2) — liste les MIDI devices détectés sur
    /// la machine (claviers USB, virtual MIDI ports). L'agent répond avec
    /// `MidiDeviceList`. Pas de cache : les devices USB peuvent être hot-plug.
    ListMidiDevices,
    /// Sprint INSERT instruments (S2) — change la source d'entrée de la
    /// tranche instrument self. `source` = "audio" (= CPAL classique) ou
    /// "midi" (clavier MIDI). `midiDeviceId` est requis si source=midi
    /// (format `"{idx}:{name}"` retourné par MidiDeviceList).
    SetInputSource {
        /// "audio" | "midi"
        source: String,
        #[serde(rename = "midiDeviceId", default)]
        midi_device_id: Option<String>,
    },
    /// Sprint S2.9 — Envoie un event MIDI brut (3 bytes) au plugin instrument
    /// actuellement chargé. Utilisé par le clavier virtuel HTML intégré dans
    /// la tranche SELF : click sur une touche → NoteOn (0x9N), relâcher → NoteOff
    /// (0x8N). No-op silencieux si aucun plugin chargé.
    PlayMidiNote {
        /// Status byte MIDI : 0x80-0x8F (NoteOff) / 0x90-0x9F (NoteOn) /
        /// 0xB0-0xBF (CC) / 0xE0-0xEF (PitchBend) / etc.
        status: u8,
        data1: u8,
        data2: u8,
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
    /// Demande explicite de redémarrage de l'agent, déclenchée par le bouton
    /// « Relancer mon agent » du banner d'update browser (agent-version-check.js).
    /// L'agent relance le flux d'auto-update (download+install si une version
    /// est disponible sur l'endpoint configuré), broadcaste `Shutdown` aux
    /// clients connectés, puis `app.restart()`. Pas de réponse directe :
    /// le browser voit la WS tomber (Shutdown puis close) et bascule en
    /// fallback, puis reconnecte sur l'agent relancé/à jour.
    Restart,
    /// Redémarrage IMMÉDIAT de l'agent, SANS passer par le flux d'update.
    /// Déclenché par le bouton « Redémarrer l'agent » du badge WASAPI
    /// (Réglages audio) : un boot frais re-sonde le host CPAL → ASIO est
    /// détecté si l'interface a été branchée APRÈS le démarrage de l'agent
    /// (cas fréquent avec l'autostart au login). Contrairement à `Restart` —
    /// qui passe par `check_for_update` et ne relance QUE si une mise à jour
    /// existe — celui-ci relance toujours. Broadcaste
    /// `Shutdown{reason:"relaunch"}` puis `app.restart()`. Pas de réponse
    /// directe (la WS tombe puis reconnecte sur l'agent relancé).
    RelaunchNow,
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
        /// Backend audio actif côté agent : `"asio"` | `"wasapi"` |
        /// `"coreaudio"`. Décidé une fois au boot (cf. `audio::host`).
        /// Le browser s'en sert pour informer l'utilisateur : WASAPI sur
        /// Windows = latence non optimale → badge orange + lien support.
        /// Optionnel pour rétro-compat (vieux agents ne l'envoient pas).
        #[serde(rename = "audioHost", skip_serializing_if = "Option::is_none")]
        audio_host: Option<String>,
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
        /// Buffer CPAL en ms — sémantique côté INPUT (capture) pour
        /// rétrocompat avec les browsers pré-Q3. Égal à `input_buffer_ms`
        /// quand celui-ci est connu, sinon estimation conservatrice
        /// (10 ms = Default Win shared standard). À NE PLUS utiliser
        /// pour de nouveaux affichages : préférer `inputBufferMs` /
        /// `outputBufferMs` qui sont précis (ou absents si fallback Default).
        #[serde(rename = "bufferMs")]
        buffer_ms: f32,
        /// Buffer CPAL côté CAPTURE en ms. `None` (= champ absent du JSON)
        /// si le driver a appliqué `BufferSize::Default` (= taille non
        /// connue côté agent sans instrumenter le callback — cas WASAPI
        /// shared mic onboard Windows). Aligné sur `bufferMs` côté wire
        /// si présent.
        #[serde(rename = "inputBufferMs", skip_serializing_if = "Option::is_none")]
        input_buffer_ms: Option<f32>,
        /// Buffer CPAL côté PLAYBACK en ms. Peut DIVERGER de l'input sur
        /// Windows WASAPI shared où un côté tombe sur Fixed et l'autre
        /// sur Default (asymétrie introduite par les drivers shared).
        /// Même sémantique de `None` que `inputBufferMs`.
        #[serde(rename = "outputBufferMs", skip_serializing_if = "Option::is_none")]
        output_buffer_ms: Option<f32>,
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
        /// Sample rate natif du device (Hz). Si ≠ 48 000, le resampler
        /// Rubato (cf. `pipeline.rs:capture_stage_loop`) est actif et ajoute
        /// ~29 ms de latence cachée (1024-sample accumulateur + sinc 256).
        /// Le browser utilise cette valeur pour afficher un badge rouge UI
        /// et un toast au join, conseillant au user de configurer son
        /// interface en 48 kHz natif (cas typique Windows Sound Properties
        /// par défaut en 44 100 Hz sur Realtek onboard).
        #[serde(rename = "nativeSampleRate")]
        native_sample_rate: u32,
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
    ///
    /// Sprint B talkback auto-mute : payload étendu avec 2 champs optionnels
    /// pour piloter le détecteur d'activité instrument côté browser :
    /// - `input_rms` (linéaire 0..1) : RMS instrument self post-plugin
    /// - `midi_active` : true si une Note ON a été reçue dans les ~200 dernières ms
    ///
    /// Les 2 champs sont absents si l'agent ne capture pas (back-compat browser).
    StreamLevels {
        levels: Vec<StreamLevel>,
        #[serde(rename = "inputRms", skip_serializing_if = "Option::is_none")]
        input_rms: Option<f32>,
        #[serde(rename = "midiActive", skip_serializing_if = "Option::is_none")]
        midi_active: Option<bool>,
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
    /// Sprint INSERT (S1) — réponse à `ListPlugins`. Snapshot du cache.
    /// `scanning = true` ⇒ le scan tourne encore (peut être vide). Le browser
    /// peut repoll quelques secondes plus tard pour avoir la liste finale.
    PluginList {
        items: Vec<PluginInfo>,
        scanning: bool,
    },
    /// Sprint INSERT (S1) — réponse à `LoadInstrumentPlugin` ET push
    /// automatique au connect WS si un plugin est déjà chargé (sync state
    /// au reconnect, S1.5). `latencySamples` est la latence intrinsèque
    /// rapportée par l'AU ; le browser doit refuser d'activer le plugin si
    /// elle dépasse 64 samples (= au-delà du budget live). `pluginRef` permet
    /// au browser de matcher avec son localStorage. `bypass` reflète l'état
    /// courant (utile au reconnect pour resynchroniser le bouton bypass UI).
    InstrumentPluginLoaded {
        name: String,
        #[serde(rename = "pluginRef")]
        plugin_ref: PluginRef,
        #[serde(rename = "latencySamples")]
        latency_samples: u32,
        #[serde(rename = "hasEditor")]
        has_editor: bool,
        bypass: bool,
    },
    /// Sprint INSERT (S1) — ack du UnloadInstrumentPlugin.
    InstrumentPluginUnloaded,
    /// Sprint INSERT (S1) — erreur typée (load failed, plugin not found, etc).
    InstrumentPluginError {
        message: String,
    },
    /// Sprint S5 — Alerte d'overload CPU détectée sur le plugin INSERT actif.
    /// Émis automatiquement par l'agent quand `process_stereo` p99 dépasse le
    /// budget RT (`p99 > 4 ms` sur fenêtre 1 s avec au moins 100 mesures).
    /// L'agent a déjà mis le plugin en bypass auto (= signal dry remplacé)
    /// → cohérent UX : l'utilisateur n'entend plus le plugin mais entend
    /// quand même son signal direct.
    ///
    /// Côté browser, déclenche un toast persistant "{name} CPU-saturé,
    /// bypass auto activé" + bouton "Réactiver" qui send
    /// `SetInstrumentPluginBypass { bypass: false }`.
    ///
    /// Émis **une seule fois** par cycle d'overload (= jusqu'à ce que l'user
    /// reset via SetInstrumentPluginBypass false, ou load un nouveau plugin).
    InstrumentPluginOverload {
        /// Nom du plugin tel que rapporté côté agent (= `LoadedPluginInfo.name`).
        /// Le browser l'injecte dans son template de toast — pas hardcodé.
        name: String,
        /// 99e percentile observé sur la fenêtre de détection (ms).
        #[serde(rename = "p99Ms")]
        p99_ms: f32,
        /// Max observé sur la fenêtre (ms). Utile pour différencier
        /// "constamment lent" vs "spike isolé énorme".
        #[serde(rename = "maxMs")]
        max_ms: f32,
        /// Nombre de mesures dans la fenêtre. Permet au browser de vérifier
        /// la fiabilité statistique (= count élevé = trigger justifié).
        count: usize,
    },
    /// Sprint v0.4.9 — Alerte d'overload PIPELINE (= saturation globale
    /// agent, distincte d'un plugin lourd). Émis quand `capture_drops_per_sec`
    /// dépasse 100 sur la fenêtre 1 s — = le CPAL callback ne peut plus pousser
    /// ses samples dans le sample_rx car l'encoder thread est complètement bloqué.
    ///
    /// Cas typique : un plugin sampler (BFD Player, Kontakt…) qui charge
    /// brutalement un sample depuis disque pendant le hot path, ou un
    /// process tiers qui mange tout le CPU.
    ///
    /// Différent de `InstrumentPluginOverload` :
    /// - Aucun bypass auto (= on ne touche pas au plugin, qui peut être OK)
    /// - Toast d'INFO côté browser, pas d'action user requise
    /// - Anti-spam : émis au max 1× toutes les 10 s pour éviter le flood en
    ///   cas de saturation continue (ex : sample-load BFD massif).
    AgentPipelineOverload {
        /// Nombre de drops CPAL accumulés sur la fenêtre 1 s. > 100 = sévère.
        #[serde(rename = "dropsPerSec")]
        drops_per_sec: u64,
        /// p99 pipeline_latency_ms observé sur la même fenêtre (= temps
        /// end-to-end CPAL→RTP). Inflated si stages stallés.
        #[serde(rename = "pipelineP99Ms")]
        pipeline_p99_ms: f32,
        /// Nom du plugin actuellement chargé (= contexte diagnostic, le
        /// plugin n'est PAS forcément le coupable). Vide si aucun.
        #[serde(rename = "pluginName")]
        plugin_name: String,
    },
    /// Sprint S6 — Alerte de peer instable : un producer distant a accumulé
    /// trop de `drift_drain` sur fenêtre 30 s (> 16 events = ~1 par 2 s).
    /// Indique que le peer envoie par bursts (encoder stalls, Opus DTX
    /// pause/resume, CPU saturé chez lui, etc.) → on draine périodiquement
    /// son jitter buffer côté nous → micro-discontinuités audibles.
    ///
    /// **Pas une action critique** : le peer continue d'être audible
    /// (avec drains crossfade). Le but est d'INFORMER l'utilisateur côté
    /// browser (badge ⚠ sur la tranche du peer concerné) pour qu'il puisse
    /// inviter le peer à fermer ses apps gourmandes ou changer de carte
    /// audio.
    ///
    /// Anti-spam : émis au max 1× par 30 s par producer_id (= si le peer
    /// reste instable, l'agent renvoie périodiquement pour signaler la
    /// situation continue ; si la situation s'améliore, le badge UI
    /// disparaît après 60 s sans nouveau message).
    PeerUnstable {
        #[serde(rename = "producerId")]
        producer_id: String,
        /// Nombre de drift drains observés sur la fenêtre 30 s.
        #[serde(rename = "driftDrainsWindow")]
        drift_drains_window: u64,
        /// Cumul depuis le début du stream (= contexte diagnostic).
        #[serde(rename = "driftDrainsTotal")]
        drift_drains_total: u64,
        /// Dernier `drift_ppm` connu pour ce producer (= contexte, non-zéro
        /// signifie clock skew sender↔receiver détecté).
        #[serde(rename = "driftPpm")]
        drift_ppm: f64,
    },
    /// Sprint INSERT instruments (S2) — réponse à `ListMidiDevices`.
    MidiDeviceList {
        devices: Vec<MidiDeviceWire>,
    },
    /// Sprint INSERT instruments (S2) — ack du SetInputSource (ou erreur si
    /// le MIDI device demandé est introuvable). Le browser miroite cette info
    /// dans son UI (badge "MIDI" sur la tranche, retour Audio si erreur).
    InputSourceChanged {
        /// "audio" | "midi"
        source: String,
        #[serde(rename = "midiDeviceId", skip_serializing_if = "Option::is_none")]
        midi_device_id: Option<String>,
        #[serde(rename = "midiDeviceName", skip_serializing_if = "Option::is_none")]
        midi_device_name: Option<String>,
    },
    InputSourceError {
        message: String,
    },
    /// Sprint S1 — Snapshot périodique (1 Hz) des métriques perf agent.
    /// Permet au browser de logger (debug only en S1, UI en S5) et au support
    /// d'avoir des données chiffrées dans le bug-report sans avoir à reconstruire
    /// depuis le log brut.
    ///
    /// `timestampMs` est le `Instant`-since-process-start côté agent — utile
    /// pour ordonner les snapshots dans le bundle, pas pour comparer avec
    /// l'horloge browser (qui est sur un autre référentiel).
    ///
    /// `plugin = None` quand aucun INSERT plugin actif sur la fenêtre.
    /// `peers` vide quand pas de remote stream actif. `pipelineLatencyMs.count = 0`
    /// quand l'encoder est idle (= pas de StartCapture).
    PerfStats {
        #[serde(rename = "timestampMs")]
        timestamp_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin: Option<PluginPerf>,
        #[serde(rename = "pipelineLatencyMs")]
        pipeline_latency_ms: PipelineLatency,
        peers: Vec<PeerPerf>,
        /// Chantier C (v0.4.14) — pic ABSOLU de la sortie post-plugin sur la
        /// fenêtre 1 s. Diagnostic (un transitoire peut dépasser 1.0 sans être
        /// audible une fois soft-clippé — c'est `output_clip_pct` qui pilote le
        /// voyant, pas ce pic).
        #[serde(rename = "outputPeak")]
        output_peak: f32,
        /// Chantier C (v0.4.15) — TAUX de saturation soutenue : % de samples
        /// ayant dépassé la pleine-échelle (|x| > 1.0) sur la fenêtre. Pilote le
        /// voyant CLIP : reste ~0 sur les transitoires (batterie/piano, écrêtage
        /// inaudible), monte seulement sur un overdrive SOUTENU réel.
        #[serde(rename = "outputClipPct")]
        output_clip_pct: f32,
        /// Chantier C — latence courante du buffer self-monitor (ms) et nombre
        /// d'underruns cumulés. Diagnostic : la latence grandit transitoirement
        /// quand un plugin spike (absorbe la gigue), revient à ~5 ms au calme.
        #[serde(rename = "monitorBufferMs")]
        monitor_buffer_ms: usize,
        #[serde(rename = "monitorUnderruns")]
        monitor_underruns: u64,
    },
}

/// Sprint S1 — Métriques d'un INSERT plugin actif (process_stereo wall-clock).
/// `count = 0` n'est jamais sérialisé (cf. skip_serializing_if).
#[derive(Debug, Serialize)]
pub struct PluginPerf {
    pub name: String,
    pub count: usize,
    #[serde(rename = "meanMs")]
    pub mean_ms: f32,
    #[serde(rename = "p50Ms")]
    pub p50_ms: f32,
    #[serde(rename = "p99Ms")]
    pub p99_ms: f32,
    #[serde(rename = "maxMs")]
    pub max_ms: f32,
}

/// Sprint S1 — Latence interne de l'encoder pipeline (capture → RTP send).
/// `count = 0` indique encoder idle ; les autres champs sont 0.0 dans ce cas.
/// `dropsPerSec` = "sample channel full" sur la fenêtre, indicateur direct de
/// saturation côté capture.rs.
#[derive(Debug, Serialize)]
pub struct PipelineLatency {
    pub count: usize,
    #[serde(rename = "p50Ms")]
    pub p50_ms: f32,
    #[serde(rename = "p99Ms")]
    pub p99_ms: f32,
    #[serde(rename = "maxMs")]
    pub max_ms: f32,
    #[serde(rename = "meanMs")]
    pub mean_ms: f32,
    #[serde(rename = "dropsPerSec")]
    pub drops_per_sec: u64,
}

/// Sprint S1 — Métriques par peer remote (drift + jitter buffer + underruns).
/// `driftPpm` = dernière estimation du `DriftEstimator` (0.0 pendant les ~5
/// premières secondes de warmup). `bufferTargetMs` = cible adaptative du
/// jitter buffer pour ce stream. `driftDrops` = cumul des samples drainés
/// depuis le début de la session (pas seulement la fenêtre 1 s — c'est un
/// compteur monotone, comme `underruns`).
#[derive(Debug, Serialize)]
pub struct PeerPerf {
    #[serde(rename = "producerId")]
    pub producer_id: String,
    #[serde(rename = "driftPpm")]
    pub drift_ppm: f64,
    /// Gigue réseau MOYENNE mesurée (RFC 3550, EWMA), en ms.
    #[serde(rename = "jitterMs")]
    pub jitter_ms: f64,
    /// Chantier #1 — gigue de QUEUE (pire-cas récent), en ms. C'est elle qui
    /// dimensionne désormais `bufferTargetMs` ; exposée pour la calibration.
    #[serde(rename = "jitterTailMs")]
    pub jitter_tail_ms: f64,
    #[serde(rename = "bufferTargetMs")]
    pub buffer_target_ms: usize,
    pub underruns: u64,
    #[serde(rename = "driftDrops")]
    pub drift_drops: u64,
}

/// Wire format pour un MIDI device (cf. `audio::midi::MidiDeviceInfo` côté agent).
#[derive(Debug, Serialize)]
pub struct MidiDeviceWire {
    pub id: String,
    pub name: String,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
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
    /// Niveau global (mono) — VU peers (1 valeur sur 2 barres).
    pub rms: f32,
    /// Niveaux par canal L/R — VU self stéréo (2 barres indépendantes).
    /// Omis du JSON si absent (back-compat browser ancien : ignore ces champs).
    #[serde(rename = "rmsL", skip_serializing_if = "Option::is_none")]
    pub rms_l: Option<f32>,
    #[serde(rename = "rmsR", skip_serializing_if = "Option::is_none")]
    pub rms_r: Option<f32>,
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
    /// Sample rate natif du device (Hz) tel qu'exposé par CPAL
    /// `default_input_config` / `default_output_config`. Permet au browser
    /// d'afficher un badge UI dès la page Paramètres audio (Mes Studios)
    /// AVANT que la capture démarre, alertant l'utilisateur si le format
    /// n'est pas 48 000 Hz (= resampler Rubato actif, ~29 ms de latence
    /// cachée). 0 si la probe échoue.
    #[serde(rename = "nativeSampleRate")]
    pub native_sample_rate: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Capturing,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Contrat wire avec le browser (groupe.js / studio-settings-modal.js) :
    // `restart` et `relaunch-now` doivent rester stables (kebab-case du tag
    // `type`). Ne JAMAIS renommer sans migration côté web.
    #[test]
    fn restart_and_relaunch_now_parse_from_wire() {
        assert!(matches!(
            serde_json::from_str::<BrowserMessage>(r#"{"type":"restart"}"#).unwrap(),
            BrowserMessage::Restart
        ));
        assert!(matches!(
            serde_json::from_str::<BrowserMessage>(r#"{"type":"relaunch-now"}"#).unwrap(),
            BrowserMessage::RelaunchNow
        ));
    }
}
