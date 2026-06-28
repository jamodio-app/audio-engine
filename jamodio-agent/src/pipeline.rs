//! Audio pipeline orchestration.
//!
//! Capture: CPAL input → accumulate 240 samples (2.5ms stéréo) → Opus encode → RTP → UDP send
//! Receive: UDP recv → RTP parse → Opus decode → JitterBuffer → AudioMixer → CPAL output

use crossbeam_channel::{bounded, Receiver, Sender};
use jamodio_audio_core::codec::decoder::MusicDecoder;
use jamodio_audio_core::codec::encoder::MusicEncoder;
use jamodio_audio_core::mixer::mixer::AudioMixer;
use jamodio_audio_core::net::rtp::{self, RtpHeader};
use jamodio_audio_core::net::srtp::{SrtpContext, SrtpParameters};
use jamodio_audio_core::net::udp::{RtpReceiver, RtpSender};
use jamodio_audio_core::perfstats::Histogram;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use jamodio_audio_core::plugin_host::{MidiEvent, PluginHandle, PluginHost, PluginInfo, PluginRef};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::audio::midi::CapturedMidiEvent;
use jamodio_audio_core::protocol::AgentState;
use jamodio_audio_core::record::{RecordedFile, RecorderHandle, StemSpec};
use jamodio_audio_core::sync::drift::DriftEstimator;
use jamodio_audio_core::sync::jitter::JitterEstimator;
#[cfg(target_os = "macos")]
use jamodio_au_host::AuHost;
#[cfg(target_os = "windows")]
use jamodio_vst3_host::Vst3Host;

/// Type alias for the active plugin host implementation. Macos = AuHost,
/// Windows = Vst3Host. Both implement `jamodio_audio_core::PluginHost`.
/// Mode autres OS (linux test) : aucun host plugin défini (`cfg`).
#[cfg(target_os = "macos")]
pub type PluginHostImpl = AuHost;
#[cfg(target_os = "windows")]
pub type PluginHostImpl = Vst3Host;
use parking_lot::Mutex;
// Trait `Resampler` requis pour appeler output_frames_max() / process_into_buffer()
// sur le `SincFixedIn<f32>` du resampler 44.1k → 48k Windows.
use rubato::Resampler as _;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Wrapper to make cpal::Stream Send — we only hold it alive (RAII), never use across threads.
struct SendStream(#[allow(dead_code)] cpal::Stream);
// SAFETY: on ne fait que le garder en vie (RAII) et le dropper. Sur Windows,
// l'OUVERTURE et la FERMETURE du stream ASIO se font sur le thread COM-STA
// dédié (cf. `com_exec` + `close_stream_on_com`) — l'apartment STA qui a créé
// le driver est donc aussi celui qui le détruit. On ne fait que transférer le
// handle entre ce thread et la task tokio qui le stocke (jamais d'appel de
// méthode hors-thread ; le callback temps réel tourne sur le thread du driver).
unsafe impl Send for SendStream {}

/// Résultat de l'ouverture du stream d'entrée (résolution + build) effectuée
/// atomiquement sur le thread COM-STA — le `cpal::Device`/`cpal::Stream` !Send
/// ne quitte jamais ce thread, seules ces données `Send` reviennent.
struct BuiltInput {
    stream: SendStream,
    name: String,
    resolved_id: String,
    channels: u16,
    native_sr: u32,
    input_buf: Option<u32>,
}

/// Résultat de l'ouverture d'un stream de sortie sur le thread COM-STA.
enum OutputOpen {
    Opened { stream: SendStream, buffer: Option<u32>, name: String },
    NotFound,
    BuildFailed(String),
    /// 0.5.3-4 — un stream de playback tournait déjà (re-StartCapture « à chaud »)
    /// → on ne reconstruit/redémarre PAS la sortie. Pas de cold-start à éviter.
    Skipped,
}

/// 0.5.3-4 (Volet B) — résultat du passage COM-STA unique qui construit
/// l'ENTRÉE et la SORTIE puis les démarre (sortie d'abord). Regrouper build+play
/// des deux streams dans un seul passage évite de recréer les buffers ASIO de la
/// sortie APRÈS un `play()` d'entrée (cold-start full-duplex muet, bug PC 28/06).
struct BuiltDuplex {
    /// Entrée — déjà démarrée (`play()` appelé dans la closure, après la sortie).
    input: BuiltInput,
    /// Sortie — déjà démarrée si `Opened` ; `Skipped` si un playback existait.
    output: OutputOpen,
}

/// Résout le device de sortie + ouvre le stream playback **sur le thread
/// COM-STA** (Windows) / inline (macOS). Voir `com_exec` pour le pourquoi :
/// ASIO charge le driver via CoCreateInstance, qui exige COM initialisé et
/// le même apartment pour toute la vie de l'objet. Résolution et build sont
/// atomiques (le `cpal::Device` !Send ne traverse pas les threads).
fn open_output_on_com(
    output_id: Option<String>,
    mixer: Arc<Mutex<AudioMixer>>,
    output_callbacks: Arc<std::sync::atomic::AtomicU64>,
) -> OutputOpen {
    crate::audio::com_exec::run(move || {
        use cpal::traits::{DeviceTrait, StreamTrait};
        let device_opt = match output_id.as_deref() {
            Some(id) => crate::audio::device::get_output_device(id),
            None => crate::audio::device::default_output_device().map(|(d, _)| d),
        };
        let Some(device) = device_opt else {
            return OutputOpen::NotFound;
        };
        let name = device.name().unwrap_or_default();
        // Volet B : build (sans play) puis play, sur le thread COM-STA. Sur ce
        // chemin (add_stream / sortie seule, capture déjà chaude) il n'y a pas
        // de cold-start full-duplex à éviter, donc build+play immédiat suffit.
        match crate::audio::playback::build_playback_stream(&device, mixer, output_callbacks) {
            Ok((stream, buffer)) => match stream.play() {
                Ok(()) => OutputOpen::Opened { stream: SendStream(stream), buffer, name },
                Err(e) => OutputOpen::BuildFailed(format!("play: {}", e)),
            },
            Err(e) => OutputOpen::BuildFailed(format!("{}", e)),
        }
    })
}

/// Ferme (drop) un `cpal::Stream` **sur le thread COM-STA** (Windows) / inline
/// (macOS). Indispensable pour ASIO : `IASIO::stop`/`Release` doivent tourner
/// sur l'apartment qui a créé le driver, sinon corruption COM.
fn close_stream_on_com(stream: Option<SendStream>) {
    if let Some(s) = stream {
        crate::audio::com_exec::run(move || drop(s));
    }
}

/// Erreur typée renvoyée par `start_capture`. Permet à `ws_server` de
/// différencier un device introuvable (= `CaptureError` côté wire) d'une
/// erreur technique générique (= `Error` côté wire).
#[derive(Debug)]
pub enum CaptureStartError {
    /// Le device demandé (ou le default si aucun id) n'a pas été trouvé.
    /// Le `requested` est l'id transmis par le browser (None si aucun).
    InputDeviceNotFound { requested: Option<String> },
    OutputDeviceNotFound { requested: Option<String> },
    /// 0.5.3-4 — démarrage ASIO « à froid » raté de façon répétée : les streams
    /// se construisent (`build_*_stream` OK) mais leurs callbacks ne s'engagent
    /// pas (capture/sortie muettes), et le watchdog n'a pas réussi à réparer
    /// après `attempts` relances auto. On remonte une erreur CLAIRE au browser
    /// plutôt qu'un studio muet (jamais de fallback silencieux). `attempts` =
    /// nombre de tentatives `start_capture` effectuées.
    ColdStartFailed { attempts: u32 },
    /// Erreur technique : SFU, UDP, encoder, etc.
    Other(String),
}

impl std::fmt::Display for CaptureStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputDeviceNotFound { requested } => {
                write!(f, "input device introuvable : {:?}", requested)
            }
            Self::OutputDeviceNotFound { requested } => {
                write!(f, "output device introuvable : {:?}", requested)
            }
            Self::ColdStartFailed { attempts } => {
                write!(f, "démarrage audio ASIO muet après {} tentatives (cold-start)", attempts)
            }
            Self::Other(s) => write!(f, "{}", s),
        }
    }
}

/// Détails d'un device input ouvert avec succès — renvoyés au browser via
/// `CaptureStarted` pour confirmation explicite.
pub struct CaptureStartedInfo {
    pub device_id: String,
    pub device_name: String,
    pub channels: u16,
    /// Sample rate natif du device (Hz). Si ≠ 48 000, le resampler Rubato
    /// est actif → ~29 ms de latence cachée. Le browser surface un badge
    /// rouge UI sur la base de cette valeur.
    pub native_sample_rate: u32,
}

/// Holds all active pipeline components. Shared between WS handler and audio threads.
pub struct PipelineState {
    pub mixer: Arc<Mutex<AudioMixer>>,
    /// CPAL streams must be kept alive — dropping them stops audio.
    ///
    /// En mode MIDI (mac/win, plugin instrument INSERT chargé), CPAL reste
    /// ouvert mais ses samples sont **forcés à 0 en software** côté
    /// `process_stage` (cf. ligne `stereo.fill(0.0)` quand
    /// `input_source = InputSource::Midi(_)`). Le coût est minime (1
    /// callback CPAL + 1 fill par bloc audio = ~0,01 % CPU) et l'avantage
    /// est décisif : **aucun swap de source pendant la bascule MIDI↔AUDIO**,
    /// donc aucun risque de craquement à la frontière des buffers audio.
    ///
    /// La tentative d'optimisation v0.4.18 (Variante A : ticker silencieux
    /// remplace CPAL en mode MIDI) a été rollback en v0.4.21 suite à des
    /// craquements numériques reproductibles sur les swaps successifs —
    /// le drain + Chantier C fade ne suffisait pas en pratique.
    capture_stream: Option<SendStream>,
    playback_stream: Option<SendStream>,
    /// Handle to stop the encoder thread.
    encoder_stop: Option<Sender<()>>,
    /// Handles to stop per-stream receive I/O tasks (async tokio).
    pub recv_stops: HashMap<String, tokio::sync::oneshot::Sender<()>>,
    /// 0.5.3-2 — thread de décodage RT UNIQUE partagé par tous les pairs.
    /// Lazy-start au 1er `add_stream`, arrêté au `stop_all` (Shutdown + join).
    /// `None` = pas de stream reçu en cours.
    decode_thread: Option<DecodeThread>,
    /// 0.5.3-2 — compteur de génération des io tasks de réception. Incrémenté à
    /// chaque `add_stream` ; permet au thread de décodage de distinguer un Remove
    /// d'une ancienne connexion d'un re-add du même `producer_id` (race évitée).
    recv_epoch: u64,
    /// Selected devices : ids stricts au format `"{idx}:{name}"` produits par
    /// `device::list_inputs/outputs`. Le browser stocke et renvoie EXACTEMENT
    /// l'id reçu — on n'accepte aucune autre forme (cf. `device::get_input_device`).
    input_device_id: Option<String>,
    output_device_id: Option<String>,
    /// State
    pub state: AgentState,
    /// Buffer CPAL côté CAPTURE, en samples (mono), set au start_capture.
    /// `Some(N)` si `BufferSize::Fixed(N)` accepté par le driver,
    /// `None` si fallback `BufferSize::Default` (= driver auto, taille
    /// non connue côté agent). Lu par ws_server pour le wire
    /// `Stats.inputBufferMs`. Reset à `None` quand pas en capture.
    pub input_buffer_samples: Option<u32>,
    /// Buffer CPAL côté PLAYBACK, en samples (mono), set au start_playback.
    /// Sémantique identique à `input_buffer_samples`. Peut DIVERGER de
    /// l'input sur Windows WASAPI shared où un côté tombe sur Fixed et
    /// l'autre sur Default. Reset à `None` quand pas en playback.
    pub output_buffer_samples: Option<u32>,
    /// Input RMS for VU meter
    pub input_rms: Arc<std::sync::atomic::AtomicU32>,
    /// Talkback auto-mute (Sprint B) — true tant qu'au moins un MIDI Note ON
    /// a été reçu dans les ~200 dernières ms. Reset par la boucle process_stage
    /// quand le délai est dépassé sans nouvel event. Lu par ws_server.rs au
    /// push 100 ms des StreamLevels pour piloter l'auto-mute talkback côté
    /// browser quand l'utilisateur joue en MIDI (clavier USB ou virtuel).
    pub midi_active: Arc<std::sync::atomic::AtomicBool>,
    /// Timestamp ms (depuis epoch process) du dernier MIDI Note ON détecté.
    /// Utilisé en interne par process_stage pour le timeout de midi_active.
    pub midi_last_note_on_ms: Arc<std::sync::atomic::AtomicU64>,
    /// REC-3 : handle vers le thread record actif. `None` quand pas
    /// d'enregistrement. Le `tx` du handle est aussi posé dans le mixer
    /// via `set_record_tx` pour activer les tap sites.
    recorder: Option<RecorderHandle>,
    /// Si true, l'encoder_thread remplit les samples capturés avec des zéros
    /// avant remap/encode → équivalent à un mute hardware côté agent. Permet
    /// au browser d'implémenter le bouton « ENTRÉE OFF » en mode agent (en
    /// mode browser c'était `musicStream.getAudioTracks().enabled = false`,
    /// qui n'a pas d'équivalent ici puisque le flux part en PlainTransport
    /// piloté par CPAL).
    pub input_cut: Arc<std::sync::atomic::AtomicBool>,
    /// INSERT plugin — hôte commun (AU sur macOS, VST3 sur Windows) + handle
    /// du plugin actif sur la tranche instrument self. `handle = None` ⇒
    /// chain bypass total (no-op dans l'encoder_thread). `bypass = true` ⇒
    /// plugin chargé mais court-circuité (toggle UI A/B).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub plugin_host: Arc<Mutex<PluginHostImpl>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub instrument_plugin_handle: Arc<Mutex<Option<PluginHandle>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub instrument_plugin_bypass: Arc<std::sync::atomic::AtomicBool>,
    /// Sprint S5 — état "bypass auto suite à overload détecté". Flag activé
    /// quand `perfstats.plugin_latency.p99 > 4 ms` (cf. ws_server perfstats_task).
    /// Set en même temps que `instrument_plugin_bypass = true` + émission
    /// d'un message `InstrumentPluginOverload` au browser.
    /// Reset à false sur :
    ///   - `SetInstrumentPluginBypass { bypass: false }` (= user clique Réactiver)
    ///   - `LoadInstrumentPlugin` (= nouveau plugin → fresh start)
    ///   - `UnloadInstrumentPlugin`
    ///
    /// Permet au perfstats_task de ne PAS re-émettre un message d'overload
    /// tant que le cycle précédent n'a pas été acté par l'utilisateur.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub plugin_auto_bypass_active: Arc<std::sync::atomic::AtomicBool>,
    /// Cache du scan plugin. Le scan complet (mac : AU ~122ms-13s ; win :
    /// VST3 instancie chaque plugin pour lire latence/bus, ~5-15s) tourne UNE
    /// fois en background au boot et stocke le résultat ici. `ListPlugins`
    /// côté WS lit ce cache → réponse instantanée.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub plugin_scan_cache: Arc<Mutex<PluginScanCache>>,
    /// S1.5 — snapshot du plugin actuellement chargé (None si aucun). Lu au
    /// connect WS pour push l'état au browser → l'UI se resynchronise même
    /// après reload de page (le plugin reste actif côté agent).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub instrument_plugin_info: Arc<Mutex<Option<LoadedPluginInfo>>>,
    /// S2 — source d'entrée actuelle. Audio = CPAL classique. Midi(device_id)
    /// = ouvre un MIDI input via midir, force le signal audio à zéro (le mic
    /// reste ouvert pour la cadence d'horloge 48k/128) et passe les events
    /// MIDI au plugin instrument chargé. Le plugin produit alors le son.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub input_source: Arc<Mutex<InputSource>>,
    /// S2 — MIDI input physique ouvert (RAII : le Drop ferme le port). None
    /// si source = Audio OU si l'utilisateur a choisi le port virtuel (mac).
    /// Le callback midir push dans le channel `midi_event_rx`.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    midi_input: Option<crate::audio::midi::MidiInput>,
    /// S2 — Receiver des events MIDI cumulés depuis le dernier bloc audio.
    /// Drainé par l'encoder_thread juste avant `process_stereo`.
    ///
    /// Encapsulé dans `Arc<Mutex<Option<…>>>` pour que `set_input_source` puisse
    /// swapper le receiver **en cours de session** sans avoir à redémarrer la
    /// capture. L'encoder thread reçoit un clone du `Arc` au start_capture et
    /// lit l'Option intérieure à chaque bloc audio → suit automatiquement les
    /// bascules MIDI→AUDIO→MIDI sans redémarrage CPAL.
    ///
    /// Avant ce fix (≤ v0.4.16), l'encoder gardait un clone du receiver figé
    /// au moment du start_capture → toute re-bascule en MIDI créait un nouveau
    /// channel côté pipeline mais l'encoder continuait à lire l'ancien (vide),
    /// résultat : MIDI physique muet alors que le clavier HTML (= chemin
    /// PlayMidiNote → dispatch_midi_only) restait fonctionnel.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    midi_event_rx: Arc<Mutex<Option<Receiver<CapturedMidiEvent>>>>,
    /// S2.7 — Port virtuel "Jamodio Virtual MIDI" créé au boot agent et tenu
    /// vivant toute la durée d'exécution. Apparaît dans CoreMIDI = destination
    /// visible dans toutes les apps MIDI macOS (Logic, Ableton, GarageBand…).
    /// macOS only — Windows aura son équivalent en S2.5 via teVirtualMIDI.
    #[cfg(target_os = "macos")]
    virtual_midi_keepalive: Option<crate::audio::midi::MidiInput>,
    /// S2.7 — Receiver du port virtuel macOS, persistant et clonable.
    #[cfg(target_os = "macos")]
    virtual_midi_rx: Option<Receiver<CapturedMidiEvent>>,
    /// Sprint S1 — Métriques perf. Histogrammes capacités 512 (couvre 1.3 s à
    /// 375 obs/s, marge confortable pour le flush 1 Hz côté ws_server).
    ///
    /// `plugin_latency` est observé uniquement quand `process_stereo` tourne
    /// (plugin actif ET non-bypass) — vide en mode pass-through.
    /// `pipeline_latency` est observé à chaque tour de l'`encoder_thread`
    /// (= une mesure capture→send par bloc Opus).
    /// `capture_drops` est incrémenté depuis le callback CPAL (cf. capture.rs)
    /// quand le `sample_tx` est plein — signal direct de saturation encoder.
    /// `net_stats_by_producer` est mis à jour par les recv tasks après chaque
    /// paquet (drift + gigue réseau). Lecture côté ws_server au flush 1 Hz.
    pub perfstats: PerfHandles,
}

/// Métriques de timing réseau mesurées par stream entrant, alimentées par les
/// le thread de décodage (`decode_rt_loop`) et lues à 1 Hz par le perfstats_task.
/// Struct extensible : le chantier jitter buffer adaptatif y ajoutera la cible
/// mesurée et le ratio de resampling de drift (Phases B/C).
#[derive(Clone, Copy, Default)]
pub struct ProducerNetStats {
    /// Dérive d'horloge sender↔nous, en ppm. Cf. [`sync::drift::DriftEstimator`].
    pub drift_ppm: f64,
    /// Gigue réseau lissée, en ms (RFC 3550). Cf. [`sync::jitter::JitterEstimator`].
    pub jitter_ms: f64,
}

/// Sprint S1 — Handles perf partagés entre `PipelineState`, `encoder_thread`,
/// `recv_task`, et le CPAL capture callback. `Clone` cheap (Arc).
#[derive(Clone)]
pub struct PerfHandles {
    pub plugin_latency: Arc<Mutex<Histogram>>,
    /// End-to-end CAPTURE_in → ENCODE_send. Inclut le temps en file dans les
    /// ringbufs entre stages (S3) — c'est la VRAIE latence pipeline ressentie.
    pub pipeline_latency: Arc<Mutex<Histogram>>,
    /// v0.4.8 — temps de traitement PUR du capture_stage (remap + resample),
    /// du `recv_timeout` (= pop sample_rx) à juste avant le `send` sortant.
    /// Sum(capture+process+encode) << pipeline_latency ⇒ stages bien découplés.
    /// Sum ≈ pipeline_latency ⇒ au moins un stage stall en queue.
    pub capture_latency: Arc<Mutex<Histogram>>,
    /// v0.4.8 — temps de traitement PUR du process_stage (plugin + RMS +
    /// self-monitor). Contient `plugin_latency` comme sous-ensemble par sous-bloc.
    pub process_latency: Arc<Mutex<Histogram>>,
    /// v0.4.8 — temps de traitement PUR du encode_stage (Opus + RTP build + send).
    pub encode_latency: Arc<Mutex<Histogram>>,
    /// Délai d'ÉMISSION (ms) : de la production du paquet (sortie encodeur) à son
    /// envoi réel sur le socket = attente dans la file RTP + sommeil du pacer.
    /// C'était l'angle mort de la latence d'émission ; on le mesure désormais
    /// pour juger le pacing sur des chiffres déterministes (pas l'acoustique).
    pub send_path_latency: Arc<Mutex<Histogram>>,
    /// 0.5.3 — RAFALE d'émission : nombre de frames Opus émises par bloc d'entrée
    /// à `encode_stage` (= nombre d'itérations du `while accumulator >= frame_len`).
    /// C'est LA mesure déterministe de la rafale Windows/ASIO : un callback qui
    /// livre N×120 samples d'un coup → N frames `try_send` d'affilée → le pair
    /// récepteur reçoit une grappe (son `buffer_target_ms` monte). À 48 k natif
    /// (resampler bypassé) ce chiffre ≈ taille_callback / 120 → révèle SANS
    /// inférence si le driver ASIO a honoré `Fixed(128)` (≈1) ou délivre sa
    /// taille de control panel (≈4 pour 512). Cible après fix : ≈1.
    /// (Unité « ms » du Histogram réutilisée pour un comptage de frames.)
    pub emit_burst: Arc<Mutex<Histogram>>,
    /// 0.5.3-2 — latence du chemin de RÉCEPTION : de l'arrivée réseau (horodatée
    /// dans `recv_io_task`) à juste avant `push_samples` (file MPSC + parse +
    /// décode Opus). Miroir de `send_path_latency`. Doit lire ~0,1-0,5 ms si le
    /// thread de décodage RT tient ; un p99 qui grimpe = décodage préempté (le
    /// bug Windows que ce thread RT corrige).
    pub recv_path: Arc<Mutex<Histogram>>,
    pub capture_drops: Arc<std::sync::atomic::AtomicU64>,
    /// 0.5.3-4 — LIVENESS du callback CPAL d'ENTRÉE : incrémenté d'1 à chaque
    /// callback de capture (cf. `capture::forward_samples`). Sert au watchdog
    /// cold-start ASIO (`ws_server` handler `StartCapture`) : si ce compteur ne
    /// bouge pas ~700 ms après un `start_capture` réussi → le callback ASIO ne
    /// s'est PAS engagé (démarrage à froid muet, bug PC 28/06) → relance auto.
    /// Cumulatif (jamais reset) : le watchdog mesure un DELTA. Coût hot-path :
    /// un `fetch_add(Relaxed)` par callback = négligeable.
    pub capture_callbacks: Arc<std::sync::atomic::AtomicU64>,
    /// 0.5.3-4 — LIVENESS du callback CPAL de SORTIE : incrémenté d'1 à chaque
    /// callback de playback (cf. `playback`/`mix_into`). Pendant du précédent
    /// pour la sortie : si la sortie ne pull pas (jitter buffer overflow en
    /// cascade), ce compteur reste figé → le watchdog le détecte.
    pub output_callbacks: Arc<std::sync::atomic::AtomicU64>,
    pub net_stats_by_producer: Arc<Mutex<HashMap<String, ProducerNetStats>>>,
    /// Chantier C (v0.4.14) — pic ABSOLU de la sortie post-plugin (pré-soft-clip)
    /// sur la fenêtre courante, en bits f32 (≥ 0 → ordre des bits monotone, OK
    /// pour `fetch_max`). Lu+reset par perfstats_task 1 Hz. Diagnostic.
    pub output_peak: Arc<std::sync::atomic::AtomicU32>,
    /// Chantier C (v0.4.15) — nombre de samples ayant dépassé la pleine-échelle
    /// (|x| > 1.0) sur la fenêtre = vrais écrêtages rattrapés par le soft-clip.
    /// `output_clip_samples / output_total_samples` = taux de saturation soutenu
    /// → c'est CE taux (pas le pic transitoire) qui allume le voyant CLIP, pour
    /// ne pas crier au loup sur les transitoires inaudibles (batterie/piano).
    pub output_clip_samples: Arc<std::sync::atomic::AtomicU64>,
    pub output_total_samples: Arc<std::sync::atomic::AtomicU64>,
}

impl PerfHandles {
    /// Histogrammes capacité 512 = ~1.36 s à cadence Opus 48k/120 (≈400 blocs/s),
    /// marge confortable pour le flush 1 Hz côté ws_server (10 % slack).
    fn new() -> Self {
        const HISTOGRAM_CAPACITY: usize = 512;
        Self {
            plugin_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            pipeline_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            capture_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            process_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            encode_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            send_path_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            emit_burst: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            recv_path: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            capture_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            capture_callbacks: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            output_callbacks: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            net_stats_by_producer: Arc::new(Mutex::new(HashMap::new())),
            output_peak: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            output_clip_samples: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            output_total_samples: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

/// Source d'entrée de l'instrument self (sprint S2). Mutuellement exclusif :
/// l'utilisateur choisit Audio OU MIDI, pas les deux. Audio+MIDI simultané
/// = sprint futur sur demande (cf. mémoire vision INSERT plugins).
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Clone)]
pub enum InputSource {
    Audio,
    Midi(String), // device id format `"{idx}:{name}"` (cf. midi::list_devices)
}

/// État du scan plugin en background. Stocké dans `PipelineState`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Clone)]
pub enum PluginScanCache {
    /// Scan en cours — le browser doit repoller dans quelques secondes.
    Scanning,
    /// Scan terminé, liste prête.
    Ready(Vec<PluginInfo>),
    /// Scan échoué (rarissime).
    #[allow(dead_code)]
    Failed(String),
}

/// Snapshot complet du plugin chargé sur l'instrument self (S1.5). Utilisé
/// pour push l'état au browser au reconnect WS — sans ça, après reload de
/// la page browser, l'UI ignorait qu'un plugin tournait encore côté agent.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Clone)]
pub struct LoadedPluginInfo {
    pub plugin_ref: PluginRef,
    pub name: String,
    pub latency_samples: u32,
    pub has_editor: bool,
}

/// Chantier A (v0.4.12) — bundle d'`Arc` strictement nécessaire pour charger /
/// décharger un plugin SANS tenir le lock `PipelineState`.
///
/// Pourquoi : l'init / teardown natif d'un plugin (AU/VST3) prend 0,4 à 4 s sur
/// les gros plugins (AmpliTube, BFD, Kontakt…). Avant, ce travail tournait en
/// tenant le lock `PipelineState` (→ perfstats_task bloqué) ET `plugin_host`
/// (→ thread audio bloqué → drops → glitch). Désormais le handler WS clone ce
/// bundle (cheap), relâche le lock `PipelineState`, puis exécute l'opération
/// lente sur `spawn_blocking`. Le thread audio voit `handle = None` → dry
/// passthrough (cf. `try_lock` dans le process stage) et ne bloque jamais.
///
/// Clone = simples `Arc::clone` (pas de copie de données).
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Clone)]
pub struct PluginControl {
    plugin_host: Arc<Mutex<PluginHostImpl>>,
    instrument_plugin_handle: Arc<Mutex<Option<PluginHandle>>>,
    instrument_plugin_bypass: Arc<std::sync::atomic::AtomicBool>,
    plugin_auto_bypass_active: Arc<std::sync::atomic::AtomicBool>,
    plugin_scan_cache: Arc<Mutex<PluginScanCache>>,
    instrument_plugin_info: Arc<Mutex<Option<LoadedPluginInfo>>>,
    plugin_latency: Arc<Mutex<Histogram>>,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl PluginControl {
    /// Décharge le plugin courant (no-op si aucun). Pose `handle = None` AVANT
    /// le teardown natif → le thread audio passe en dry immédiatement, puis le
    /// teardown lent s'exécute sans bloquer l'audio (try_lock côté process).
    /// À appeler hors du lock `PipelineState` (idéalement `spawn_blocking`).
    pub fn unload(&self) {
        // Détache d'abord le handle (dry instantané), PUIS teardown natif.
        let old = self.instrument_plugin_handle.lock().take();
        if let Some(handle) = old {
            let _ = self.plugin_host.lock().unload(handle);
            // S1.5 — clear le snapshot AVEC le handle pour cohérence.
            *self.instrument_plugin_info.lock() = None;
            self.instrument_plugin_bypass
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // S5 — reset flag overload (cohérent avec load).
            self.plugin_auto_bypass_active
                .store(false, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(target: "jamodio::plugin", "instrument plugin unloaded");
        }
    }

    /// Charge `plugin_ref` sur l'instrument self. Décharge l'éventuel précédent
    /// (single slot MVP). `max_frames = 128` (cf. PLUGIN_BLOCK). Le thread audio
    /// reste en dry (`handle = None`) toute la durée du load natif, puis bascule
    /// wet une fois le handle posé. Retourne (name, latency_samples, has_editor)
    /// pour l'ack browser. À appeler hors du lock `PipelineState`.
    pub fn load(&self, plugin_ref: &PluginRef) -> Result<(String, u32, bool), String> {
        // SÉCURITÉ (audit pré-beta) : n'accepter QUE des plugins présents dans
        // le cache de scan. Le `path` d'un `PluginRef::Vst3` vient du browser et
        // arrive jusqu'à `LoadLibrary` (exécution de code natif au chargement).
        // Sans cette garde, une page autorisée pourrait faire charger une DLL
        // arbitraire (RCE Windows). Le browser ne propose que des plugins issus
        // du scan → cette validation est transparente en usage normal. Refus
        // AVANT unload pour ne pas décharger le plugin courant sur un refus.
        {
            let scan = self.plugin_scan_cache.lock();
            let known = matches!(&*scan, PluginScanCache::Ready(items)
                if items.iter().any(|p| p.plugin_ref == *plugin_ref));
            if !known {
                return Err(
                    "plugin inconnu (absent du scan) — chargement refusé".to_string(),
                );
            }
        }

        // Décharger d'abord (pose handle=None → dry immédiat).
        self.unload();

        // Load natif (lent). handle reste None → thread audio en dry.
        let mut host = self.plugin_host.lock();
        let handle = host.load(plugin_ref, 128).map_err(|e| format!("{e}"))?;
        let latency = host.latency_samples(handle);
        drop(host);

        // Retrouver name + has_editor depuis le cache pour l'ack browser. Si le
        // scan tourne encore (cas limite), valeurs par défaut — le browser a de
        // toute façon déjà le name dans sa liste.
        let (name, has_editor) = {
            let scan = self.plugin_scan_cache.lock();
            if let PluginScanCache::Ready(items) = &*scan {
                items
                    .iter()
                    .find(|p| p.plugin_ref == *plugin_ref)
                    .map(|p| (p.name.clone(), p.has_editor))
                    .unwrap_or_else(|| ("Unknown plugin".to_string(), false))
            } else {
                ("Unknown plugin".to_string(), false)
            }
        };

        // Bascule wet : pose le handle (le thread audio le récupère au prochain
        // bloc via try_lock → traitement plugin actif).
        *self.instrument_plugin_handle.lock() = Some(handle);
        self.instrument_plugin_bypass
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // S5 — reset le flag overload + flush l'histogramme plugin_latency pour
        // ne pas mélanger les mesures de l'ancien plugin avec le nouveau.
        self.plugin_auto_bypass_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = self.plugin_latency.lock().flush();
        // S1.5 — snapshot complet pour resync au reconnect.
        *self.instrument_plugin_info.lock() = Some(LoadedPluginInfo {
            plugin_ref: plugin_ref.clone(),
            name: name.clone(),
            latency_samples: latency,
            has_editor,
        });
        tracing::info!(
            target: "jamodio::plugin",
            name = %name,
            latency_samples = latency,
            "instrument plugin loaded"
        );
        Ok((name, latency, has_editor))
    }
}

const CHANNELS: usize = 2;

impl PipelineState {
    pub fn new(mixer: Arc<Mutex<AudioMixer>>) -> Self {
        Self {
            mixer,
            capture_stream: None,
            playback_stream: None,
            encoder_stop: None,
            recv_stops: HashMap::new(),
            decode_thread: None,
            recv_epoch: 0,
            input_device_id: None,
            output_device_id: None,
            state: AgentState::Idle,
            input_buffer_samples: None,
            output_buffer_samples: None,
            input_rms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            midi_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            midi_last_note_on_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            recorder: None,
            input_cut: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(target_os = "macos")]
            plugin_host: Arc::new(Mutex::new(AuHost::new())),
            #[cfg(target_os = "windows")]
            plugin_host: Arc::new(Mutex::new(Vst3Host::new())),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            instrument_plugin_handle: Arc::new(Mutex::new(None)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            instrument_plugin_bypass: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            plugin_auto_bypass_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            plugin_scan_cache: Arc::new(Mutex::new(PluginScanCache::Scanning)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            instrument_plugin_info: Arc::new(Mutex::new(None)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            input_source: Arc::new(Mutex::new(InputSource::Audio)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            midi_input: None,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            midi_event_rx: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "macos")]
            virtual_midi_keepalive: None,
            #[cfg(target_os = "macos")]
            virtual_midi_rx: None,
            perfstats: PerfHandles::new(),
        }
    }

    /// S2.7 — Crée le port virtuel "Jamodio Virtual MIDI" au boot agent.
    /// Best-effort : si la création échoue (rare — droits CoreMIDI), on
    /// continue sans virtual port. Les ports physiques restent utilisables.
    /// Appelée une fois après `new()` par `main.rs`.
    #[cfg(target_os = "macos")]
    pub fn spawn_virtual_midi(&mut self) {
        let (tx, rx) = bounded::<CapturedMidiEvent>(512);
        match crate::audio::midi::create_virtual_input(tx) {
            Ok(mi) => {
                self.virtual_midi_keepalive = Some(mi);
                self.virtual_midi_rx = Some(rx);
            }
            Err(e) => {
                tracing::warn!(
                    target: "jamodio::midi",
                    error = %e,
                    "virtual MIDI port creation failed — physical ports only"
                );
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    pub fn spawn_virtual_midi(&mut self) {}

    /// Change la source d'entrée. Appelé par le WS handler quand le browser
    /// bascule entre Audio et MIDI. En mode MIDI, ouvre un MidiInput via
    /// midir et stocke son receiver pour drainage dans encoder_thread.
    /// L'ouverture du device peut échouer si introuvable → erreur retournée
    /// au caller (qui fait un toast browser).
    ///
    /// **Pas de swap de la capture audio** : CPAL reste ouvert dans les deux
    /// modes. En mode MIDI, ses samples sont écrasés par 0 côté
    /// `process_stage` (le plugin instrument génère l'audio depuis les
    /// events MIDI). Cette stratégie élimine tout risque de craquement à la
    /// frontière des buffers audio pendant la bascule.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn set_input_source(&mut self, source: InputSource) -> Result<(), String> {
        match &source {
            InputSource::Audio => {
                // Ferme le MIDI input physique s'il y en avait un. Le port
                // virtuel macOS reste vivant (= virtual_midi_keepalive intact).
                self.midi_input = None;
                // Set l'Option intérieure du Arc — l'encoder thread (qui détient
                // un clone du Arc) verra `None` au prochain lock et basculera
                // en mode audio passthrough sans devoir être redémarré.
                *self.midi_event_rx.lock() = None;
            }
            InputSource::Midi(device_id) => {
                let is_virtual = device_id
                    .starts_with(crate::audio::midi::VIRTUAL_PORT_ID_PREFIX);
                #[cfg(target_os = "macos")]
                if is_virtual {
                    // Port virtuel macOS : on réutilise le receiver persistant
                    // créé au boot. Pas d'ouverture nouvelle.
                    let rx = self.virtual_midi_rx.clone().ok_or_else(|| {
                        "virtual MIDI port not available (creation failed at boot)".to_string()
                    })?;
                    self.midi_input = None;
                    *self.midi_event_rx.lock() = Some(rx);
                    *self.input_source.lock() = source;
                    return Ok(());
                }
                #[cfg(target_os = "windows")]
                if is_virtual {
                    // Sur Windows, le pseudo "Jamodio Virtual MIDI" n'a pas
                    // (encore) de vrai port CoreMIDI-équivalent (= S2.5 via
                    // teVirtualMIDI). En attendant, accepter la sélection
                    // permet de basculer source=MIDI et d'utiliser le
                    // clavier HTML intégré qui dispatch directement au
                    // plugin via PlayMidiNote → Vst3Host::dispatch_midi_only.
                    self.midi_input = None;
                    *self.midi_event_rx.lock() = None;
                    *self.input_source.lock() = source;
                    return Ok(());
                }
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                let _ = is_virtual;

                let (tx, rx) = bounded::<CapturedMidiEvent>(256);
                let midi = crate::audio::midi::MidiInput::open(device_id, tx)?;
                self.midi_input = Some(midi);
                // Set l'Option intérieure du Arc → l'encoder thread RT lit le
                // NOUVEAU receiver au prochain bloc audio (sans restart).
                *self.midi_event_rx.lock() = Some(rx);
            }
        }
        *self.input_source.lock() = source;
        Ok(())
    }

    /// Symétrique de `set_input_source`. Pas utilisé en prod (le browser
    /// pousse l'état via `InputSourceChanged`), gardé pour le diag log et
    /// la cohérence d'API.
    #[allow(dead_code)]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn current_input_source(&self) -> InputSource {
        self.input_source.lock().clone()
    }

    /// Lance le scan plugin en background. Appelé une fois après `new()` par
    /// `main.rs`. Le thread tourne typiquement 100ms à 15s puis stocke le
    /// résultat dans le cache. Méthode no-op sur les OS sans host plugin.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn spawn_plugin_scan(&self) {
        let host = self.plugin_host.clone();
        let cache = self.plugin_scan_cache.clone();
        let kind = if cfg!(target_os = "macos") { "AU" } else { "VST3" };
        std::thread::Builder::new()
            .name("plugin-scan".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                tracing::info!(target: "jamodio::plugin", kind, "plugin scan starting in background");
                let plugins = host.lock().scan();
                let elapsed_ms = t0.elapsed().as_millis();
                tracing::info!(
                    target: "jamodio::plugin",
                    kind,
                    count = plugins.len(),
                    elapsed_ms,
                    "plugin scan finished"
                );
                *cache.lock() = PluginScanCache::Ready(plugins);
            })
            .expect("spawn plugin-scan thread");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn spawn_plugin_scan(&self) {
        // No-op : pas d'host plugin Linux pour l'instant.
    }

    /// Helpers INSERT — appelés par les handlers WS dans `ws_server.rs`.
    /// Chacun renvoie un Result avec message d'erreur lisible pour wire.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn list_instrument_plugins(&self) -> (Vec<PluginInfo>, bool) {
        match &*self.plugin_scan_cache.lock() {
            PluginScanCache::Scanning => (Vec::new(), true),
            PluginScanCache::Ready(items) => (items.clone(), false),
            PluginScanCache::Failed(_) => (Vec::new(), false),
        }
    }

    /// Chantier A — clone le bundle d'`Arc` plugin pour exécuter load/unload
    /// HORS du lock `PipelineState` (cf. `PluginControl`). Cheap (Arc::clone).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn plugin_control(&self) -> PluginControl {
        PluginControl {
            plugin_host: self.plugin_host.clone(),
            instrument_plugin_handle: self.instrument_plugin_handle.clone(),
            instrument_plugin_bypass: self.instrument_plugin_bypass.clone(),
            plugin_auto_bypass_active: self.plugin_auto_bypass_active.clone(),
            plugin_scan_cache: self.plugin_scan_cache.clone(),
            instrument_plugin_info: self.instrument_plugin_info.clone(),
            plugin_latency: self.perfstats.plugin_latency.clone(),
        }
    }


    /// S1.5 — Snapshot pour resync au reconnect WS. Retourne None si aucun
    /// plugin actuellement chargé. Le bypass est dans le AtomicBool dédié.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn get_instrument_plugin_snapshot(&self) -> Option<(LoadedPluginInfo, bool)> {
        let info = self.instrument_plugin_info.lock().clone()?;
        let bypass = self
            .instrument_plugin_bypass
            .load(std::sync::atomic::Ordering::Relaxed);
        Some((info, bypass))
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn set_instrument_plugin_bypass(&self, bypass: bool) {
        self.instrument_plugin_bypass
            .store(bypass, std::sync::atomic::Ordering::Relaxed);
        // S5 — reset flag overload : un toggle manuel (= action user
        // explicite, via UI "Réactiver" ou bypass A/B) signifie que
        // l'user a pris connaissance et acte. Le perfstats_task peut
        // à nouveau émettre un overload si le plugin re-spike après.
        // On reset DANS LES DEUX SENS (bypass=true et bypass=false) car
        // un toggle vers true = pas un overload-detection automatique
        // (= l'user a choisi de muter manuellement, il n'a pas besoin
        // du toast d'alerte).
        self.plugin_auto_bypass_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn open_instrument_plugin_editor(&self) -> Result<(), String> {
        let handle = self
            .instrument_plugin_handle
            .lock()
            .ok_or_else(|| "no plugin loaded".to_string())?;
        self.plugin_host
            .lock()
            .open_editor(handle)
            .map_err(|e| format!("{e}"))
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn close_instrument_plugin_editor(&self) -> Result<(), String> {
        let handle = self
            .instrument_plugin_handle
            .lock()
            .ok_or_else(|| "no plugin loaded".to_string())?;
        self.plugin_host
            .lock()
            .close_editor(handle)
            .map_err(|e| format!("{e}"))
    }

    /// Active/désactive le mute hardware côté capture (bouton ENTRÉE OFF
    /// browser). Le flag est lu sans lock dans l'encoder_thread à chaque
    /// frame capturée — coût Relaxed négligeable face à une frame 2.5ms.
    pub fn set_input_cut(&mut self, cut: bool) {
        self.input_cut.store(cut, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(target: "jamodio::pipeline", cut, "input_cut updated");
    }

    /// REC-3 : démarre un enregistrement multi-stems. Crée un `RecorderHandle`
    /// (thread record + Recorder), poste son `tx` au mixer pour activer les
    /// tap sites. Échoue si un enregistrement est déjà en cours OU si l'init
    /// d'un OpusEncoder échoue.
    /// Retourne les specs vraiment armés (peut différer des demandés si
    /// un peer_id n'existe pas — non vérifié ici, juste 1:1 pour l'instant).
    pub fn start_recording(&mut self, stems: Vec<StemSpec>) -> Result<Vec<StemSpec>, String> {
        if self.recorder.is_some() {
            return Err("recording already in progress".into());
        }
        let handle = RecorderHandle::start(stems)?;
        // Active les tap sites côté mixer en clonant le sender.
        self.mixer.lock().set_record_tx(Some(handle.tx.clone()));
        let armed = handle.armed_specs.clone();
        self.recorder = Some(handle);
        tracing::info!(target: "jamodio::pipeline", stems = armed.len(), "recording started");
        Ok(armed)
    }

    /// Détache l'enregistreur et retourne son handle SANS finaliser (rapide).
    /// Le caller appelle ensuite `handle.stop()` HORS du lock pipeline — le
    /// finalize peut prendre jusqu'à 30s et ne doit pas geler le pipeline
    /// (sinon tous les autres handlers voient "overloaded" pendant ce temps).
    pub fn take_recorder(&mut self) -> Option<RecorderHandle> {
        // Détache le tx du mixer d'abord — les tap sites deviennent no-op
        // immédiatement, plus aucune nouvelle commande n'arrive au thread.
        self.mixer.lock().set_record_tx(None);
        self.recorder.take()
    }

    /// REC-3 : stop l'enregistrement et retourne les fichiers Ogg/Opus.
    /// Bloque jusqu'à finalize (timeout 30s côté handle). Tient le lock
    /// pipeline pendant le finalize → réservé aux callers internes déjà
    /// dans un contexte de teardown (ex. `stop_capture`). Le handler WS
    /// StopRecording utilise `take_recorder` + `stop()` hors lock.
    pub fn stop_recording(&mut self) -> Vec<RecordedFile> {
        let Some(handle) = self.take_recorder() else {
            return Vec::new();
        };
        let files = handle.stop();
        tracing::info!(target: "jamodio::pipeline", files = files.len(), "recording stopped");
        files
    }

    /// Set the input device id (format `"{idx}:{name}"`). `None` = utiliser
    /// le default système au prochain start_capture (uniquement si le browser
    /// n'a jamais sélectionné).
    pub fn set_input_device(&mut self, input: Option<String>) {
        self.input_device_id = input;
    }

    pub fn select_devices(&mut self, input: Option<String>, output: Option<String>) {
        // Sprint 3.1 — restart live du stream playback si l'output change.
        // Sans ça, modifier la sortie audio dans les settings n'a aucun effet
        // tant qu'on ne quitte/rejoint pas la session (le stream CPAL est
        // démarré une fois dans start_capture et jamais touché ensuite).
        let output_changed = self.output_device_id != output;
        self.input_device_id = input;
        self.output_device_id = output;
        if output_changed && self.playback_stream.is_some() {
            self.restart_playback();
        }
    }

    /// Sprint 3.1 — Recrée le CPAL output stream avec le device courant.
    /// Le mixer est conservé (Arc partagé), aucun audio en cours n'est perdu :
    /// le ring buffer continue d'accumuler côté décodeur pendant la transition.
    fn restart_playback(&mut self) {
        // ASIO mono-client : sur Windows, impossible d'ouvrir un 2e stream sur
        // le driver tant que l'ancien le tient → on FERME l'ancien (sur le
        // thread COM-STA) AVANT d'ouvrir le nouveau. Le ring buffer décodeur
        // côté mixer couvre le court gap (changement de sortie = action rare).
        // Résolution + ouverture atomiques sur le thread COM-STA (cf. com_exec).
        close_stream_on_com(self.playback_stream.take());
        match open_output_on_com(
            self.output_device_id.clone(),
            self.mixer.clone(),
            self.perfstats.output_callbacks.clone(),
        ) {
            OutputOpen::Opened { stream, buffer, name } => {
                self.playback_stream = Some(stream);
                self.output_buffer_samples = buffer;
                tracing::info!(target: "jamodio::pipeline", device = %name, "output device switched");
            }
            OutputOpen::NotFound => {
                tracing::warn!(
                    target: "jamodio::pipeline",
                    requested = ?self.output_device_id,
                    "output device introuvable — playback désactivé jusqu'à nouvelle sélection"
                );
                self.output_buffer_samples = None;
            }
            OutputOpen::BuildFailed(e) => {
                tracing::error!(
                    target: "jamodio::pipeline",
                    error = %e,
                    "restart_playback échoué — playback désactivé jusqu'à nouvelle sélection"
                );
                self.output_buffer_samples = None;
            }
            // `open_output_on_com` ne renvoie jamais Skipped (réservé au passage
            // duplex de start_capture).
            OutputOpen::Skipped => unreachable!("open_output_on_com ne renvoie pas Skipped"),
        }
    }

    /// Renvoie l'id du device sélectionné par le browser (s'il y en a un),
    /// sinon l'id du default système. Utilisé uniquement pour les Stats UI
    /// (pas un point de résolution de capture — le start_capture fait sa
    /// propre résolution stricte).
    pub fn selected_input_id(&self) -> Option<String> {
        self.input_device_id.clone().or_else(crate::audio::device::default_input_id)
    }

    /// Start the capture pipeline: CPAL → accumulator → Opus → RTP → UDP.
    /// `channel_index` : si `Some(i)`, extrait le canal physique i et duplique
    /// L=R=canal[i] avant encodage Opus (mode mono propre, centré à la lecture).
    /// Si `None`, capture stéréo standard (canaux 1+2 du device).
    /// `sfu_srtp` : clés SRTP du SFU (chiffrement RTP entrant côté agent).
    /// Returns `(local_port, agent_srtp)` — le browser relaie `agent_srtp`
    /// au SFU via `connect-plain-transport`.
    pub async fn start_capture(
        &mut self,
        ssrc: u32,
        sfu_ip: String,
        sfu_port: u16,
        payload_type: u8,
        channel_index: Option<u8>,
        sfu_srtp: SrtpParameters,
    ) -> Result<(u16, SrtpParameters, CaptureStartedInfo), CaptureStartError> {
        // 1. STOP toute capture en cours D'ABORD. ASIO est mono-client :
        // impossible d'ouvrir un second stream tant que l'ancien tient le
        // driver ; et la fermeture doit se faire sur le thread COM-STA (cf.
        // stop_capture → close_stream_on_com). Sur macOS, sans effet de bord.
        // (Le build du nouveau stream, plus bas, EST la validation du device :
        // on n'alloue le socket UDP qu'ensuite mais un échec retombe proprement
        // via `?`, RAII relâche tout.)
        self.stop_capture();

        let input_id = self.input_device_id.clone();

        let sfu_addr: SocketAddr = format!("{}:{}", sfu_ip, sfu_port)
            .parse()
            .map_err(|e| CaptureStartError::Other(format!("Bad SFU address: {}", e)))?;

        // 2. Create SRTP context: nos clés (TX, à transmettre au SFU) + clés SFU (RX).
        let agent_srtp = SrtpParameters::generate_aead_aes_256_gcm();
        let srtp_ctx = Arc::new(SrtpContext::new(&agent_srtp, &sfu_srtp).map_err(CaptureStartError::Other)?);

        // 3. Create UDP sender (chiffre via le contexte SRTP).
        let sender = RtpSender::new(sfu_addr, srtp_ctx)
            .await
            .map_err(|e| CaptureStartError::Other(format!("UDP bind: {}", e)))?;
        let local_port = sender.local_addr().map_err(|e| CaptureStartError::Other(format!("{}", e)))?.port();

        // Pas de punch ici : le 1er paquet audio chiffré (sous 10 ms) sert de punch
        // pour comedia. Un punch en clair serait rejeté par le SFU (enableSrtp:true).

        // 4. Channels
        let (sample_tx, sample_rx) = bounded::<Vec<f32>>(64);
        let input_rms = self.input_rms.clone();
        // 0.5.3-3 — ÉMISSION RT : plus de channel ni de tâche UDP tokio. Le thread
        // d'encode (RT/MMCSS) chiffre + envoie en non-bloquant DIRECTEMENT (cf.
        // `encode_stage_loop` → `RtpSender::send_blocking`). Supprime le hop tokio
        // normal-priorité = supprime la gigue d'égression sous charge Windows.
        let sender = Arc::new(sender);
        let (stop_tx, stop_rx) = bounded::<()>(1);
        self.encoder_stop = Some(stop_tx);

        // 5. RÉSOLUTION + OUVERTURE DU STREAM CPAL, atomiquement sur le thread
        //    COM-STA (cf. `com_exec`). Le `cpal::Device` est !Send et ne doit
        //    pas quitter ce thread : on résout l'id, on ouvre le stream, et on
        //    ne renvoie que des données `Send` (+ le handle `SendStream`).
        //    Résolution STRICTE : l'id du browser DOIT pointer sur un device
        //    courant (pas de fallback default, sauf AUCUN id sélectionné =
        //    premier lancement). Une erreur de build (driver, sample-rate) est
        //    technique pure, pas une erreur de sélection user.
        //
        //    CPAL est ouvert dans TOUS les modes (AUDIO et MIDI) : en MIDI ses
        //    samples sont écrasés par 0 côté `process_stage` (le plugin
        //    instrument génère l'audio depuis les events MIDI) → aucun swap de
        //    source pendant les bascules MIDI↔AUDIO, donc aucun craquement.
        //
        //    Sprint S1 — `capture_drops` partagé avec le callback CPAL :
        //    incrémenté quand `sample_tx` est plein, lu+reset par ws_server au
        //    flush 1 Hz pour publier `dropsPerSec` dans PerfStats.
        //
        //    Volet B (0.5.3-4) — on construit l'ENTRÉE **et** la SORTIE puis on
        //    les démarre (sortie d'abord, entrée ensuite) dans CE seul passage
        //    COM-STA. Sur un device ASIO full-duplex, appeler `build_output`
        //    (ASIOCreateBuffers) APRÈS un `play()` d'entrée recréait les buffers
        //    en cours → callbacks muets (cold-start raté). En créant tous les
        //    buffers avant de démarrer, on évite ce recreate. La sortie n'est
        //    (re)construite que si aucun playback ne tourne déjà (cold path).
        let capture_drops_for_callback = self.perfstats.capture_drops.clone();
        let capture_callbacks_for_callback = self.perfstats.capture_callbacks.clone();
        let output_callbacks_for_callback = self.perfstats.output_callbacks.clone();
        let need_output = self.playback_stream.is_none();
        let output_id = self.output_device_id.clone();
        let mixer_for_output = self.mixer.clone();
        let built = crate::audio::com_exec::run(move || -> Result<BuiltDuplex, CaptureStartError> {
            use cpal::traits::{DeviceTrait, StreamTrait};
            // --- ENTRÉE : résolution + build (SANS play) ---
            let device = match input_id.as_deref() {
                Some(id) => crate::audio::device::get_input_device(id),
                None => crate::audio::device::default_input_id()
                    .as_deref()
                    .and_then(crate::audio::device::get_input_device),
            };
            let Some(device) = device else {
                return Err(CaptureStartError::InputDeviceNotFound { requested: input_id });
            };
            let name = device.name().unwrap_or_default();
            // Id rapporté : celui demandé, sinon le default résolu ({idx}:{name}).
            let resolved_id = input_id
                .unwrap_or_else(|| crate::audio::device::default_input_id().unwrap_or_else(|| name.clone()));
            let (in_stream, channels, native_sr, input_buf) =
                crate::audio::capture::build_capture_stream(
                    &device,
                    sample_tx,
                    capture_drops_for_callback,
                    capture_callbacks_for_callback,
                )
                .map_err(|e| CaptureStartError::Other(format!("CPAL input: {}", e)))?;

            // --- SORTIE : résolution + build (SANS play) + play, si nécessaire ---
            // Tous les drops ci-dessous (in_stream, out_stream) se font SUR le
            // thread COM-STA car ils ont lieu DANS cette closure → fermeture ASIO
            // sur l'apartment qui a créé le driver (contrat `SendStream`).
            let output = if need_output {
                let dev = match output_id.as_deref() {
                    Some(id) => crate::audio::device::get_output_device(id),
                    None => crate::audio::device::default_output_device().map(|(d, _)| d),
                };
                match dev {
                    None => {
                        // Sortie introuvable = fatal. `in_stream` (non démarré)
                        // est droppé ici, sur le thread COM-STA. Erreur claire.
                        return Err(CaptureStartError::OutputDeviceNotFound { requested: output_id });
                    }
                    Some(d) => {
                        let out_name = d.name().unwrap_or_default();
                        match crate::audio::playback::build_playback_stream(
                            &d,
                            mixer_for_output,
                            output_callbacks_for_callback,
                        ) {
                            // Démarre la SORTIE d'abord (buffers tous créés).
                            Ok((out_stream, buffer)) => match out_stream.play() {
                                Ok(()) => OutputOpen::Opened {
                                    stream: SendStream(out_stream),
                                    buffer,
                                    name: out_name,
                                },
                                Err(e) => OutputOpen::BuildFailed(format!("play: {}", e)),
                            },
                            Err(e) => OutputOpen::BuildFailed(format!("{}", e)),
                        }
                    }
                }
            } else {
                OutputOpen::Skipped
            };

            // --- Démarre l'ENTRÉE, après la sortie ---
            // En cas d'échec ici, in_stream ET la sortie démarrée (`output`)
            // sont droppés sur ce thread COM-STA (fermeture ASIO correcte).
            in_stream
                .play()
                .map_err(|e| CaptureStartError::Other(format!("CPAL input play: {}", e)))?;

            Ok(BuiltDuplex {
                input: BuiltInput {
                    stream: SendStream(in_stream),
                    name,
                    resolved_id,
                    channels,
                    native_sr,
                    input_buf,
                },
                output,
            })
        })?;
        let BuiltDuplex { input: built_input, output: built_output } = built;
        let in_name = built_input.name;
        let resolved_input_id = built_input.resolved_id;
        let channels_in = built_input.channels;
        let native_sr = built_input.native_sr;
        let input_buf = built_input.input_buf;
        tracing::info!(target: "jamodio::pipeline", device = %in_name, "input device opened");
        self.capture_stream = Some(built_input.stream);
        tracing::info!(
            target: "jamodio::pipeline",
            channels_in, native_sr, ?channel_index,
            needs_resample = native_sr != 48000,
            "input config"
        );

        // Valider que le canal mono demandé existe bien sur le device
        let effective_channel = channel_index.and_then(|idx| {
            if (idx as u16) < channels_in { Some(idx) } else {
                tracing::warn!(
                    target: "jamodio::pipeline",
                    requested_channel = idx,
                    available_channels = channels_in,
                    "channel_index hors plage — fallback stéréo"
                );
                None
            }
        });

        // 6. Spawn encoder thread (std thread, not tokio — real-time audio)
        //
        // SELF-MONITOR : on enregistre un stream local dans le mixer AVANT de
        // spawn l'encoder. Le thread va y pousser les samples capturés en
        // parallèle de l'encodage Opus → l'utilisateur s'entend dans son
        // casque sans passer par la chaîne browser à 25 ms. Volume initial 0
        // (silencieux) → le browser ouvre le fader via SetSelfMonitorVolume.
        self.mixer.lock().add_local_stream();
        let mixer_for_encoder = self.mixer.clone();
        let input_cut_for_encoder = self.input_cut.clone();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let plugin_host_for_encoder = self.plugin_host.clone();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let plugin_handle_for_encoder = self.instrument_plugin_handle.clone();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let plugin_bypass_for_encoder = self.instrument_plugin_bypass.clone();
        // S2 — Arc partagé du receiver MIDI. L'encoder thread lit l'Option
        // intérieure à chaque bloc et suit donc automatiquement les
        // bascules MIDI ↔ AUDIO ↔ MIDI faites via set_input_source, sans
        // restart de capture (= fix bug v0.4.16 → unreleased : MIDI physique
        // muet après bascule AUDIO→MIDI).
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let midi_event_rx_for_encoder = self.midi_event_rx.clone();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let input_source_for_encoder = self.input_source.clone();
        let perfstats_for_encoder = self.perfstats.clone();
        // Sprint B talkback auto-mute — clones pour le thread encoder.
        let midi_active_for_encoder = self.midi_active.clone();
        let midi_last_note_on_ms_for_encoder = self.midi_last_note_on_ms.clone();
        // Sprint S2 — nom du device output (extrait du format "{idx}:{name}")
        // passé à rt_priority pour matcher le workgroup CoreAudio HAL. Si
        // aucun device explicite (None ⇒ default OS), on passera None et
        // le workgroup utilisera le default output.
        let output_device_name_for_encoder = self
            .output_device_id
            .as_deref()
            .and_then(|id| id.split_once(':').map(|(_, name)| name.to_string()));
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                encoder_thread(
                    sample_rx, sender, stop_rx, ssrc, payload_type, input_rms,
                    channels_in, native_sr, effective_channel, mixer_for_encoder, input_cut_for_encoder,
                    perfstats_for_encoder, output_device_name_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_host_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_handle_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_bypass_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] midi_event_rx_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] input_source_for_encoder,
                    midi_active_for_encoder,
                    midi_last_note_on_ms_for_encoder,
                );
            })
            .map_err(|e| CaptureStartError::Other(format!("Spawn encoder: {}", e)))?;

        // 7. (Émission RT — cf. section 4 : le thread d'encode chiffre + envoie
        //    directement, il n'y a plus de tâche UDP tokio.)

        // 8. Stream de sortie (playback) — déjà CONSTRUIT + DÉMARRÉ dans le
        //    passage COM-STA ci-dessus (Volet B : build entrée+sortie avant de
        //    démarrer, sortie d'abord). Ici on ne fait que router le résultat :
        //    - Opened → on conserve le handle (RAII) + le buffer télémétrie ;
        //    - NotFound → DÉJÀ remonté en erreur depuis la closure (fatal) ;
        //    - BuildFailed → NON FATAL : la capture (entrée) prime, le playback/
        //      self-monitor sera indisponible jusqu'à nouvelle sélection ;
        //    - Skipped → un playback tournait déjà, conservé tel quel.
        match built_output {
            OutputOpen::Opened { stream, buffer, name } => {
                tracing::info!(target: "jamodio::pipeline", device = %name, "output device opened");
                self.playback_stream = Some(stream);
                self.output_buffer_samples = buffer;
            }
            OutputOpen::BuildFailed(e) => tracing::warn!(
                target: "jamodio::pipeline",
                error = %e,
                "ouverture du stream de sortie échouée — playback désactivé (capture active)"
            ),
            OutputOpen::Skipped => {}
            OutputOpen::NotFound => unreachable!("OutputDeviceNotFound déjà renvoyé par la closure"),
        }

        self.state = AgentState::Capturing;
        // Buffer CPAL effectif des deux côtés (cf. champs doc). `input_buf` est
        // toujours connu ici (la branche capture vient de réussir). Pour
        // l'output, soit on vient d'ouvrir un stream (= `output_buffer_samples`
        // mis à jour juste au-dessus), soit un stream playback existait déjà
        // (= valeur conservée du précédent start_playback).
        self.input_buffer_samples = input_buf;
        tracing::info!(
            target: "jamodio::pipeline",
            sfu = format!("{}:{}", sfu_ip, sfu_port),
            local_port,
            device = %in_name,
            channels = channels_in,
            "capture started (SRTP)"
        );
        let info = CaptureStartedInfo {
            device_id: resolved_input_id,
            device_name: in_name,
            channels: channels_in,
            native_sample_rate: native_sr,
        };
        Ok((local_port, agent_srtp, info))
    }

    /// Add a receive pipeline for one remote stream.
    /// `sfu_srtp` : clés du SFU pour ce flux (déchiffrement SFU → agent).
    /// Returns `(local_port, agent_srtp)` — clés agent pour ce transport,
    /// à transmettre au SFU via `connect-plain-transport`.
    pub async fn add_stream(
        &mut self,
        producer_id: String,
        sfu_ip: String,
        sfu_port: u16,
        sfu_srtp: SrtpParameters,
    ) -> Result<(u16, SrtpParameters), String> {
        // Remove existing if any
        self.remove_stream(&producer_id);

        let sfu_addr: SocketAddr = format!("{}:{}", sfu_ip, sfu_port)
            .parse()
            .map_err(|e| format!("Bad SFU address: {}", e))?;

        // SRTP context : clés agent (TX) générées localement + clés SFU (RX).
        let agent_srtp = SrtpParameters::generate_aead_aes_256_gcm();
        let srtp_ctx = Arc::new(SrtpContext::new(&agent_srtp, &sfu_srtp)?);

        // Create UDP receiver
        let receiver = RtpReceiver::new(srtp_ctx)
            .await
            .map_err(|e| format!("UDP bind: {}", e))?;
        let local_port = receiver.local_addr().map_err(|e| format!("{}", e))?.port();

        // Note : pas de punch synchrone ici. Le punch SRTP serait rejeté par le SFU
        // tant que celui-ci n'a pas reçu nos clés via connect-plain-transport
        // (qui n'est envoyé par le browser qu'après cette réponse). On punch en boucle
        // dans recv_io_task jusqu'au 1er paquet reçu (=> comedia activé côté SFU).

        // 0.5.3-2 — lazy-start du thread de décodage RT partagé (au 1er stream).
        // Le stream mixer N'EST PLUS créé ici : le thread de décodage le crée au
        // 1er paquet du pair (il est l'unique écrivain du mixer côté pairs → zéro
        // race add/remove/push).
        if self.decode_thread.is_none() {
            self.decode_thread = Some(
                spawn_decode_thread(
                    self.mixer.clone(),
                    self.perfstats.net_stats_by_producer.clone(),
                    self.perfstats.recv_path.clone(),
                )
                .map_err(|e| format!("spawn decode thread: {}", e))?,
            );
        }
        let decode = self
            .decode_thread
            .as_ref()
            .expect("decode thread démarré juste au-dessus");

        // Stop signal pour la tâche I/O de ce pair.
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        self.recv_stops.insert(producer_id.clone(), stop_tx);

        // Spawn la tâche I/O async (recv UDP + horodatage + punch + idle-timeout).
        // Elle forwarde les paquets bruts au thread de décodage RT via le MPSC.
        let tx = decode.tx.clone();
        let pool_rx = decode.pool_rx.clone();
        let pid: Arc<str> = Arc::from(producer_id.as_str());
        self.recv_epoch = self.recv_epoch.wrapping_add(1);
        let epoch = self.recv_epoch;
        tokio::spawn(async move {
            recv_io_task(receiver, sfu_addr, pid, epoch, tx, pool_rx, stop_rx).await;
        });

        // Start playback if not running. Résolution + ouverture sur le thread
        // COM-STA (cf. com_exec) ; pas de fallback silencieux sur le default si
        // un id explicite échoue.
        if self.playback_stream.is_none() {
            match open_output_on_com(
                self.output_device_id.clone(),
                self.mixer.clone(),
                self.perfstats.output_callbacks.clone(),
            ) {
                OutputOpen::Opened { stream, buffer, .. } => {
                    self.playback_stream = Some(stream);
                    self.output_buffer_samples = buffer;
                }
                OutputOpen::NotFound => return Err("output device introuvable".into()),
                OutputOpen::BuildFailed(e) => return Err(format!("CPAL output: {}", e)),
                OutputOpen::Skipped => unreachable!("open_output_on_com ne renvoie pas Skipped"),
            }
        }

        tracing::info!(
            target: "jamodio::pipeline",
            sfu = format!("{}:{}", sfu_ip, sfu_port),
            local_port,
            "stream added (SRTP)"
        );
        Ok((local_port, agent_srtp))
    }

    pub fn remove_stream(&mut self, producer_id: &str) {
        // On signale juste la tâche I/O ; à sa sortie elle envoie `Remove` au
        // thread de décodage qui retire l'état + le stream mixer + net_stats,
        // APRÈS le dernier paquet du pair (ordre garanti → zéro 'unknown stream').
        if let Some(stop) = self.recv_stops.remove(producer_id) {
            let _ = stop.send(());
        }
    }

    fn stop_capture(&mut self) {
        // Drop du stream CPAL sur le thread COM-STA (ASIO : stop/Release doivent
        // tourner sur l'apartment qui a créé le driver). macOS : inline.
        close_stream_on_com(self.capture_stream.take());
        if let Some(stop) = self.encoder_stop.take() {
            let _ = stop.send(());
        }
        // Retire le stream self-monitor (créé dans start_capture).
        // Sans ça, le stream subsisterait dans le mixer avec son volume courant
        // et continuerait à mixer son ring buffer résiduel jusqu'à underrun.
        self.mixer.lock().remove_local_stream();
        // Le buffer input n'a plus de sens hors capture (capture_stream droppé).
        // L'output reste actif (peut continuer à jouer les peers reçus), on
        // garde donc `output_buffer_samples` tel quel jusqu'au `stop_all`.
        self.input_buffer_samples = None;
    }

    pub fn stop_all(&mut self) {
        // Évite le bruit en idle : si rien ne tourne, on log en debug pour
        // pas spammer info à chaque WS disconnect du browser (le probe agent
        // ouvre/ferme une WS toutes les 30 s, ce qui appelait stop_all).
        let was_active = self.capture_stream.is_some()
            || self.playback_stream.is_some()
            || !self.recv_stops.is_empty()
            || self.recorder.is_some();
        // REC-3 : si un recording était en cours, on l'arrête proprement
        // (les fichiers sont produits mais perdus — l'utilisateur a quitté).
        // Évite de laisser un thread record orphelin tenir des ressources.
        if self.recorder.is_some() {
            tracing::warn!(target: "jamodio::pipeline", "stop_all during recording — files discarded");
            let _ = self.stop_recording();
        }
        self.stop_capture();
        let ids: Vec<String> = self.recv_stops.keys().cloned().collect();
        for id in ids {
            self.remove_stream(&id);
        }
        // 0.5.3-2 — arrête le thread de décodage RT partagé. Shutdown (il nettoie
        // les streams mixer + net_stats restants) puis join (sortie immédiate sur
        // le message). On drop le Sender pour fermer le MPSC côté principal ; les
        // io tasks restantes (oneshots déjà envoyés ci-dessus) verront Err à leur
        // prochain send et sortiront.
        if let Some(DecodeThread { tx, pool_rx: _, join }) = self.decode_thread.take() {
            let _ = tx.send(DecodeMsg::Shutdown);
            drop(tx);
            let _ = join.join();
        }
        close_stream_on_com(self.playback_stream.take()); // drop sur le thread COM-STA (ASIO)
        self.output_buffer_samples = None;
        self.state = AgentState::Idle;
        if was_active {
            tracing::info!(target: "jamodio::pipeline", "pipeline stopped");
        } else {
            tracing::debug!(target: "jamodio::pipeline", "stop_all on idle pipeline (no-op)");
        }
    }
}

// ─── Encoder thread (std::thread, real-time priority) ──────────────

/// Convertit un bloc PCM entrelacé N canaux vers stéréo entrelacé (L, R, L, R, …).
/// - `channel_index = Some(i)` : extraction pure du canal i, dupliqué L=R=ch[i]
///   (signal mono centré, parfait pour un instrument mono branché sur un seul
///   canal d'une interface multi-canaux).
/// - `channel_index = None` :
///     - si source mono (channels_in = 1) → L=R=sample (centrage)
///     - sinon → prend les 2 premiers canaux (ch0 = L, ch1 = R)
///
/// Sortie : un `Vec<f32>` de longueur `frames × 2` (interleaved stéréo).
fn remap_to_stereo(src: &[f32], channels_in: usize, channel_index: Option<u8>) -> Vec<f32> {
    if channels_in == 0 {
        return Vec::new();
    }
    let frames = src.len() / channels_in;
    let mut out = Vec::with_capacity(frames * 2);
    match channel_index {
        Some(idx) => {
            let i = idx as usize;
            for f in 0..frames {
                let s = src[f * channels_in + i];
                out.push(s);
                out.push(s);
            }
        }
        None => {
            if channels_in == 1 {
                for &s in src.iter().take(frames) {
                    out.push(s);
                    out.push(s);
                }
            } else {
                // Prend ch0 = L, ch1 = R (les canaux suivants sont ignorés)
                for f in 0..frames {
                    out.push(src[f * channels_in]);
                    out.push(src[f * channels_in + 1]);
                }
            }
        }
    }
    out
}

// Helper RT thread — chaque paramètre est un primitive différent et grouper
// dans un struct ne ferait qu'ajouter un nom intermédiaire sans clarté.
#[allow(clippy::too_many_arguments)]
/// Sprint S3 — bloc audio horodaté qui transite entre stages. Le timestamp
/// est apposé en début de `capture_stage_loop` (= entrée sample_rx) et lu
/// en fin de `encode_stage_loop` (= juste après try_send RTP). Permet de
/// conserver la sémantique exacte de `pipeline_latency` historique
/// (capture→send) malgré la séparation en 3 threads.
type TimedBlock = (std::time::Instant, Vec<f32>);

/// Sprint S3 — capacité des ringbufs entre stages. 32 chunks × ~5,3 ms
/// (240 samples stéréo @ 48k) = ~170 ms de marge. Un spike plugin de 22 ms
/// est absorbé sans saturer la queue.
const STAGE_CHANNEL_CAPACITY: usize = 32;

/// Convertit le `captured_at` d'un `CapturedMidiEvent` en `frame_offset`
/// sample-accurate (= index sample dans le bloc audio courant) pour le
/// dispatch au plugin instrument.
///
/// `block_start` = wall-clock juste avant le drain MIDI (= référence de
/// l'instant zéro du bloc audio en cours de traitement).
/// `max_offset` = dernier index sample valide du bloc (= `n_pairs - 1`).
///
/// Sémantique du clamp :
/// - `captured_at < block_start` → 0 (event arrivé pendant le queueing du
///   bloc précédent → snap au début, position la plus précoce jouable)
/// - `captured_at` au-delà du bloc → `max_offset` (extrêmement rare car
///   la prochaine itération aura un `block_start` plus récent ; safety net).
///
/// Précision : bornée par le sample 48 kHz (≈ 20 µs).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn midi_frame_offset(
    captured_at: std::time::Instant,
    block_start: std::time::Instant,
    max_offset: u32,
) -> u32 {
    captured_at
        .checked_duration_since(block_start)
        .map(|d| (d.as_micros() as u32) * 48 / 1000)
        .unwrap_or(0)
        .min(max_offset)
}

/// Filtre les events MIDI dont le `frame_offset` (absolu dans le bloc parent)
/// tombe dans `[sub_start, sub_end)`, et écrit dans `out` les events ré-exprimés
/// avec un offset relatif au début du sous-bloc.
///
/// `out` est cleared avant remplissage. Le caller passe son buffer réutilisable
/// (pré-alloué cap 64) pour éviter une allocation par sous-bloc dans le hot
/// path encoder thread.
///
/// Cas dégénéré (CPAL Fixed(128) = 1 sous-bloc = tout le bloc) : O(N) où
/// N = nombre d'events (<= 64), trivialement rapide.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn dispatch_subblock_midi(
    events: &[MidiEvent],
    sub_start: u32,
    sub_end: u32,
    out: &mut Vec<MidiEvent>,
) {
    out.clear();
    for ev in events {
        if ev.frame_offset >= sub_start && ev.frame_offset < sub_end {
            out.push(MidiEvent {
                frame_offset: ev.frame_offset - sub_start,
                data: ev.data,
            });
        }
    }
}

/// Sprint S3 — Orchestrateur des 3 stages audio (capture, process, encode).
///
/// Architecture (cf. PLAN-EXECUTION-AGENT-STABILITE.md §S3) :
///
/// ```text
/// CPAL callback ─sample_rx─►  capture_stage  ─►ringbuf 32─►  process_stage  ─►ringbuf 32─►  encode_stage  ─SRTP+send_to─► SFU
///                              (remap+resample)              (plugin+RMS+self-monitor)        (Opus+RTP)
/// ```
///
/// Chaque stage tourne dans son propre thread `std::thread` RT promu via
/// `crate::audio::rt_priority::promote_thread_for_audio` (= workgroup
/// CoreAudio macOS / MMCSS Windows / thread-priority Linux).
///
/// Cette fonction conserve la signature historique (= drop-in replacement
/// de l'ancien `encoder_thread` monolithique). À l'intérieur, elle :
/// 1. Crée les channels entre stages
/// 2. Spawn les 3 sub-threads
/// 3. Attend le `stop_rx` original (de `stop_capture`)
/// 4. Propage le signal stop via `stop_flag` atomique
/// 5. Joint les 3 threads (drainage naturel via Disconnected cascade)
#[allow(clippy::too_many_arguments)]
fn encoder_thread(
    sample_rx: Receiver<Vec<f32>>,
    sender: Arc<RtpSender>,
    stop_rx: Receiver<()>,
    ssrc: u32,
    payload_type: u8,
    input_rms: Arc<std::sync::atomic::AtomicU32>,
    channels_in: u16,
    native_sr: u32,
    channel_index: Option<u8>,
    mixer: Arc<Mutex<AudioMixer>>,
    input_cut: Arc<std::sync::atomic::AtomicBool>,
    perfstats: PerfHandles,
    output_device_name: Option<String>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_host: Arc<Mutex<PluginHostImpl>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_handle: Arc<Mutex<Option<PluginHandle>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_bypass: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] midi_event_rx: Arc<Mutex<Option<Receiver<CapturedMidiEvent>>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] input_source: Arc<Mutex<InputSource>>,
    // Sprint B talkback auto-mute : drapeau "MIDI Note ON dans les ~200 ms
    // précédentes". Set par process_stage à chaque Note ON, lu par ws_server.
    midi_active: Arc<std::sync::atomic::AtomicBool>,
    midi_last_note_on_ms: Arc<std::sync::atomic::AtomicU64>,
) {
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (cap_to_proc_tx, cap_to_proc_rx) =
        bounded::<TimedBlock>(STAGE_CHANNEL_CAPACITY);
    let (proc_to_enc_tx, proc_to_enc_rx) =
        bounded::<TimedBlock>(STAGE_CHANNEL_CAPACITY);

    // ─── Capture stage ────────────────────────────────────
    let stop_cap = stop_flag.clone();
    let out_name_cap = output_device_name.clone();
    let perfstats_cap = perfstats.clone();
    let h_cap = std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || {
            capture_stage_loop(
                sample_rx,
                cap_to_proc_tx,
                stop_cap,
                channels_in,
                native_sr,
                channel_index,
                perfstats_cap,
                out_name_cap,
            );
        })
        .expect("spawn audio-capture thread");

    // ─── Process stage ────────────────────────────────────
    let stop_proc = stop_flag.clone();
    let out_name_proc = output_device_name.clone();
    let mixer_proc = mixer.clone();
    let input_cut_proc = input_cut.clone();
    let input_rms_proc = input_rms.clone();
    let perfstats_proc = perfstats.clone();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let plugin_host_proc = plugin_host.clone();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let plugin_handle_proc = plugin_handle.clone();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let plugin_bypass_proc = plugin_bypass.clone();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let input_source_proc = input_source.clone();
    // Sprint B talkback auto-mute — clones pour le thread process.
    let midi_active_proc = midi_active.clone();
    let midi_last_note_on_ms_proc = midi_last_note_on_ms.clone();
    let h_proc = std::thread::Builder::new()
        .name("audio-process".into())
        .spawn(move || {
            process_stage_loop(
                cap_to_proc_rx,
                proc_to_enc_tx,
                stop_proc,
                mixer_proc,
                input_cut_proc,
                input_rms_proc,
                perfstats_proc,
                out_name_proc,
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                plugin_host_proc,
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                plugin_handle_proc,
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                plugin_bypass_proc,
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                midi_event_rx,
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                input_source_proc,
                midi_active_proc,
                midi_last_note_on_ms_proc,
            );
        })
        .expect("spawn audio-process thread");

    // ─── Encode stage ─────────────────────────────────────
    let stop_enc = stop_flag.clone();
    let out_name_enc = output_device_name;
    let perfstats_enc = perfstats.clone();
    let h_enc = std::thread::Builder::new()
        .name("audio-encode".into())
        .spawn(move || {
            encode_stage_loop(
                proc_to_enc_rx,
                sender,
                stop_enc,
                ssrc,
                payload_type,
                perfstats_enc,
                out_name_enc,
            );
        })
        .expect("spawn audio-encode thread");

    // ─── Attente stop + drainage cascade ──────────────────
    //
    // Le stop_rx vient de `start_capture` (Sender stocké dans `encoder_stop`,
    // déclenché par `stop_capture`). À la réception :
    // 1. On flag `stop_flag` → chaque stage break en début de prochaine
    //    iteration de sa boucle.
    // 2. On join `h_cap` → quand il return, `cap_to_proc_tx` est drop →
    //    `process_stage` voit `Disconnected` sur son recv → drain les
    //    samples restants en queue (max 32 × 5.3ms ≈ 170ms) → return.
    // 3. Idem pour `proc_to_enc_tx` → `encode_stage` drain et return.
    //
    // Ordonner les joins amont→aval garantit qu'aucun sample en queue
    // n'est perdu au stop. Coût pire-cas du stop : ~170 ms (vidange complète
    // des deux ringbufs) — négligeable pour un cycle de vie utilisateur.
    let _ = stop_rx.recv();
    stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = h_cap.join();
    let _ = h_proc.join();
    let _ = h_enc.join();
}

// ═══════════════════════════════════════════════════════════════════
// Sprint S3 — Capture stage
// ═══════════════════════════════════════════════════════════════════
//
// Responsabilités :
//   - Lire les chunks PCM bruts depuis CPAL via `sample_rx`
//   - Remapper le canal mono sélectionné en stéréo (= dupliquer ou
//     extraire le bon canal selon `channel_index`)
//   - Resampler si `native_sr != 48_000` (Rubato SincFixedIn) — pratique
//     uniquement sur Windows WASAPI shared 44.1k. Sur Mac CoreAudio
//     l'input est nativement 48k → bypass total (resampler = None).
//   - Apposer un `Instant::now()` à chaque bloc émis pour mesure
//     pipeline_latency end-to-end (lu par encode_stage).
//
// Coût observé en baseline : < 200 µs par bloc CPAL (= ~5% du budget 2.7 ms).
// L'isoler en thread propre prépare le terrain pour mesurer
// `capture_p99_ms` séparément en S4 si besoin.

#[allow(clippy::too_many_arguments)]
fn capture_stage_loop(
    sample_rx: Receiver<Vec<f32>>,
    out_tx: Sender<TimedBlock>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    channels_in: u16,
    native_sr: u32,
    channel_index: Option<u8>,
    perfstats: PerfHandles,
    output_device_name: Option<String>,
) {
    let _rt_priority_handle = crate::audio::rt_priority::promote_thread_for_audio(
        output_device_name.as_deref(),
    );

    // Sprint S3 — shadow channels_in en usize pour les indexations downstream
    // (remap_to_stereo, slice indexing, ...).
    let channels_in: usize = channels_in.into();

    // Resampler natif → 48 kHz (mic Windows onboard typique = 44.1 kHz, mac
    // CoreAudio est généralement 48 kHz natif → bypass total). Rubato Sinc
    // est sync, ~50-150 µs par bloc 128 samples sur M1. Latence introduite
    // ≈ sinc_len / native_sr = 256 / 44100 ≈ 5.8 ms (acceptable, dominé par
    // le buffer WASAPI shared 10 ms de toute façon sur ce path).
    let mut resampler: Option<rubato::SincFixedIn<f32>> = if native_sr != 48000 {
        let ratio = 48000.0 / native_sr as f64;
        let params = rubato::SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: rubato::SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        match rubato::SincFixedIn::<f32>::new(ratio, 1.0, params, 1024, CHANNELS) {
            Ok(r) => {
                tracing::info!(
                    target: "jamodio::encoder",
                    native_sr, target_sr = 48000u32, ratio,
                    "resampler enabled (native_sr ≠ 48k)"
                );
                Some(r)
            }
            Err(e) => {
                tracing::error!(
                    target: "jamodio::encoder",
                    error = %e,
                    "rubato init failed — capture continuera SANS resampling (audio désynchronisé)"
                );
                None
            }
        }
    } else {
        None
    };
    // Buffers de sortie Rubato réutilisés entre les itérations pour éviter
    // d'allouer dans le hot path. Resize au besoin (output_frames_max).
    let mut resample_out_l: Vec<f32> = Vec::with_capacity(2048);
    let mut resample_out_r: Vec<f32> = Vec::with_capacity(2048);
    // Accumulateur PRE-resample : Rubato impose un chunk_size FIXE en input
    // (1024). Les buffers CPAL arrivent à des tailles variables (128 sur
    // CoreAudio, ~480 sur WASAPI shared). On accumule jusqu'à atteindre
    // 1024 par canal avant de resampler.
    let mut pre_resample_l: Vec<f32> = Vec::with_capacity(2048);
    let mut pre_resample_r: Vec<f32> = Vec::with_capacity(2048);
    const RESAMPLE_CHUNK: usize = 1024;

    loop {
        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        match sample_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(samples) => {
                // Timestamp début pipeline : transporté avec le bloc à travers
                // tous les stages pour mesure pipeline_latency end-to-end.
                let t_block_start = std::time::Instant::now();
                // v0.4.8 — timer "traitement pur" de ce stage : démarre ici,
                // s'arrête juste avant out_tx.send (= AVANT l'entrée en file
                // dans le ringbuf du process_stage).
                let t_stage_start = t_block_start;
                let mut stereo = remap_to_stereo(&samples, channels_in, channel_index);

                // RESAMPLE (Windows 44.1 → 48k). Bypass total si natif = 48k.
                if let Some(rs) = resampler.as_mut() {
                    for chunk in stereo.chunks_exact(2) {
                        pre_resample_l.push(chunk[0]);
                        pre_resample_r.push(chunk[1]);
                    }
                    stereo.clear();
                    let out_max = rs.output_frames_max();
                    if resample_out_l.len() < out_max {
                        resample_out_l.resize(out_max, 0.0);
                    }
                    if resample_out_r.len() < out_max {
                        resample_out_r.resize(out_max, 0.0);
                    }
                    while pre_resample_l.len() >= RESAMPLE_CHUNK {
                        let waves_in: [&[f32]; 2] = [
                            &pre_resample_l[..RESAMPLE_CHUNK],
                            &pre_resample_r[..RESAMPLE_CHUNK],
                        ];
                        let mut waves_out: [&mut [f32]; 2] = [
                            &mut resample_out_l[..],
                            &mut resample_out_r[..],
                        ];
                        match rs.process_into_buffer(&waves_in, &mut waves_out, None) {
                            Ok((_in_used, out_frames)) => {
                                for i in 0..out_frames {
                                    stereo.push(resample_out_l[i]);
                                    stereo.push(resample_out_r[i]);
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    target: "jamodio::encoder",
                                    error = %e,
                                    "rubato process_into_buffer failed"
                                );
                            }
                        }
                        pre_resample_l.drain(..RESAMPLE_CHUNK);
                        pre_resample_r.drain(..RESAMPLE_CHUNK);
                    }
                    // Pas encore assez de samples accumulés → on attend le
                    // prochain buffer CPAL. Le bloc n'est pas émis ce tour-ci.
                    if stereo.is_empty() {
                        continue;
                    }
                }

                // v0.4.8 — observe le temps de traitement PUR du capture_stage
                // (= depuis sample_rx.recv jusqu'ici, avant entrée en file).
                let capture_elapsed_ms =
                    t_stage_start.elapsed().as_secs_f32() * 1000.0;
                perfstats.capture_latency.lock().observe(capture_elapsed_ms);

                // Émet vers process_stage. Si plein (Disconnected = stage en
                // shutdown), on continue sans bloquer le thread capture (le
                // bound 32 est large mais on protège contre un blocage de
                // process_stage pendant un shutdown ordonné).
                match out_tx.send((t_block_start, stereo)) {
                    Ok(()) => {}
                    Err(_) => {
                        // process_stage downstream a drop son receiver →
                        // shutdown en cascade. On termine.
                        break;
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Sprint S3 — Process stage
// ═══════════════════════════════════════════════════════════════════
//
// Responsabilités :
//   - Appliquer `input_cut` (silence forcé en entrée si toggle UI OFF)
//   - Appliquer le mode MIDI source (force samples=0, le plugin instrument
//     génère l'audio depuis les events MIDI)
//   - Appliquer le plugin INSERT (AU mac / VST3 win) via `process_stereo`
//     par sous-blocs de PLUGIN_BLOCK frames. Mesure wall-clock par
//     sous-bloc dans `perfstats.plugin_latency` (= signal pour S5 overload).
//   - Calculer le RMS post-plugin pour le VU-mètre
//   - Push self-monitor dans le mixer (= ce que l'utilisateur entend wet
//     dans son casque)
//   - Forward le bloc + timestamp original vers `encode_stage`
//
// C'est le SEUL stage qui peut spiker (plugin lourd). Le ringbuf en amont
// (depuis capture) absorbe ~170 ms de jitter sans drop CPAL.

/// Chantier C (v0.4.14, révisé v0.4.15) — soft-clip de sécurité ZÉRO-latence,
/// plugin-agnostic.
///
/// En dessous de `threshold` : identité (signal bit-identique, coût ≈ 1 abs +
/// 1 compare par sample). Au-dessus : genou `tanh` qui plafonne en douceur vers
/// ±1.0 — continu et de pente continue à `threshold` (tanh'(0)=1) → pas de
/// rupture. Asymptote à ±1.0 : la sortie ne dépasse JAMAIS 0 dBFS. Aucun
/// lookahead → aucune latence ajoutée. Protège le DAC (monitoring), le réseau
/// et l'enregistrement quel que soit le plugin (ou même sans plugin, un gain
/// d'entrée trop élevé). `threshold` ≈ 0.98 (-0,17 dBFS) → on ne touche QUE le
/// tout haut du signal (les pics ≥ pleine-échelle), pas le signal fort propre.
///
/// Retourne `(peak, overs)` :
///   - `peak` = pic ABSOLU d'ENTRÉE (pré-clip), diagnostic ;
///   - `overs` = nombre de samples qui DÉPASSAIENT la pleine-échelle (|x| > 1.0)
///     = vrais écrêtages que le soft-clip a rattrapés. Sur un transitoire isolé
///     (attaque batterie/piano) `overs` est minuscule (inaudible) ; un overdrive
///     SOUTENU produit un `overs` élevé → c'est ce signal-là (taux soutenu, pas
///     le pic instantané) qui doit allumer le voyant CLIP.
fn soft_clip_block(samples: &mut [f32], threshold: f32) -> (f32, u64) {
    let mut peak = 0.0f32;
    let mut overs = 0u64;
    let range = 1.0 - threshold;
    for s in samples.iter_mut() {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        if a > 1.0 {
            overs += 1;
        }
        if a > threshold {
            let over = (a - threshold) / range;
            *s = (threshold + range * over.tanh()).copysign(*s);
        }
    }
    (peak, overs)
}

/// Chantier B (v0.4.13) — fondu équal-power dry→wet IN-PLACE.
///
/// Mélange `wet` (sortie plugin, interleaved stéréo) avec `dry` (signal sec de
/// même longueur) sur la fenêtre de fondu restante. Gains `sin`/`cos` (équal-
/// power : `g_dry² + g_wet² = 1` → loudness perçue constante, pas de creux au
/// milieu du fondu). Le fondu démarre à dry pur (`g_wet=0`) et finit wet pur.
/// Retourne `fade_remaining` après consommation. Une fois à 0, l'audio est
/// 100 % wet et cette fonction n'est plus appelée.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn apply_dry_wet_fade(
    wet: &mut [f32],
    dry: &[f32],
    mut fade_remaining: usize,
    fade_total: usize,
) -> usize {
    debug_assert_eq!(wet.len(), dry.len(), "dry/wet doivent être alignés");
    let n_pairs = wet.len() / 2;
    for k in 0..n_pairs {
        if fade_remaining == 0 {
            break;
        }
        let pos = fade_total - fade_remaining; // 0 → fade_total-1
        let t = pos as f32 / fade_total as f32; // 0.0 → ~1.0
        let a = t * std::f32::consts::FRAC_PI_2;
        let (g_wet, g_dry) = (a.sin(), a.cos());
        wet[k * 2] = dry[k * 2] * g_dry + wet[k * 2] * g_wet;
        wet[k * 2 + 1] = dry[k * 2 + 1] * g_dry + wet[k * 2 + 1] * g_wet;
        fade_remaining -= 1;
    }
    fade_remaining
}

#[allow(clippy::too_many_arguments)]
fn process_stage_loop(
    in_rx: Receiver<TimedBlock>,
    out_tx: Sender<TimedBlock>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    mixer: Arc<Mutex<AudioMixer>>,
    input_cut: Arc<std::sync::atomic::AtomicBool>,
    input_rms: Arc<std::sync::atomic::AtomicU32>,
    perfstats: PerfHandles,
    output_device_name: Option<String>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_host: Arc<Mutex<PluginHostImpl>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_handle: Arc<Mutex<Option<PluginHandle>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_bypass: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] midi_event_rx: Arc<Mutex<Option<Receiver<CapturedMidiEvent>>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] input_source: Arc<Mutex<InputSource>>,
    // Sprint B talkback auto-mute : updated par cette boucle à chaque Note ON.
    midi_active: Arc<std::sync::atomic::AtomicBool>,
    midi_last_note_on_ms: Arc<std::sync::atomic::AtomicU64>,
) {
    let _rt_priority_handle = crate::audio::rt_priority::promote_thread_for_audio(
        output_device_name.as_deref(),
    );

    // Marque ce thread comme "audio RT" pour que le ConnectionProxy du
    // host VST3 drop les `IConnectionPoint::notify()` venant d'ici. Sans
    // ça, le plugin peut marshalize un notify cross-thread vers l'éditeur
    // STA en plein dans `attached()` → deadlock (cause confirmée du hang
    // éditeur en v0.4.0..v0.4.24, fixé en v0.4.26).
    #[cfg(target_os = "windows")]
    jamodio_vst3_host::register_audio_thread();

    // Buffers L/R préalloués pour passer le bloc à travers le plugin par
    // sous-blocs de PLUGIN_BLOCK samples. Capacité fixée à 128 (la frame
    // Opus stéréo fait 120, et les buffers post-resample ne dépassent
    // typiquement pas cette taille).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    const PLUGIN_BLOCK: usize = 128;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut plugin_left: Vec<f32> = Vec::with_capacity(PLUGIN_BLOCK);
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut plugin_right: Vec<f32> = Vec::with_capacity(PLUGIN_BLOCK);

    // Chantier B (v0.4.13) — état du crossfade dry→wet à l'activation plugin.
    // À chaque bascule "pas de plugin" → "plugin actif" (load terminé, un-bypass,
    // reprise après un swap), on amorce un fondu équal-power de FADE_SAMPLES
    // pour supprimer le clic de transition (le signal passe du sec au wet sans
    // discontinuité). En régime établi (wet stable), `fade_remaining == 0` →
    // coût nul (un seul test booléen par bloc, aucune copie, aucune latence).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    const FADE_SAMPLES: usize = 8 * 48; // 8 ms @ 48 kHz, par canal
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut wet_was_active = false;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut fade_remaining: usize = 0;
    // Copie du signal SEC du bloc, uniquement pendant un fondu (pré-alloué,
    // réutilisé → zéro alloc en régime établi).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut dry_scratch: Vec<f32> = Vec::with_capacity(PLUGIN_BLOCK * 2 * 4);

    loop {
        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        match in_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok((t_block_start, mut stereo)) => {
                // v0.4.8 — timer "traitement pur" du process_stage : démarre
                // ici (= après pop ringbuf), s'arrête juste avant le send.
                let t_stage_start = std::time::Instant::now();
                // input_cut (= SetInputCut toggle UI "ENTRÉE OFF")
                if input_cut.load(std::sync::atomic::Ordering::Relaxed) {
                    stereo.fill(0.0);
                }

                // En mode MIDI, on force samples = 0 : CPAL reste ouvert
                // (= aucun swap de source pendant la bascule MIDI↔AUDIO,
                // donc zéro risque de craquement à la frontière des buffers
                // audio), mais ses samples capturés sont ignorés — le plugin
                // instrument INSERT en aval génère l'audio depuis les events
                // MIDI. Coût : 1 fill(0) par bloc audio = négligeable.
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    // matches! sous le lock, sans clone : l'ancien
                    // `.lock().clone()` clonait la String du device MIDI à
                    // CHAQUE bloc audio en mode MIDI (alloc hot path,
                    // review 11/06).
                    let is_midi = matches!(&*input_source.lock(), InputSource::Midi(_));
                    if is_midi {
                        stereo.fill(0.0);
                    }
                }

                // INSERT plugin (AU mac / VST3 win) appliqué par sous-blocs
                // de PLUGIN_BLOCK frames. Le self-monitor entend le son WET.
                //
                // `'plugin_block` permet de SAUTER le traitement plugin ce bloc
                // (→ dry passthrough) sans court-circuiter le RMS/self-monitor/
                // encode qui suivent. Utilisé quand le lock plugin est tenu par
                // un (dé)chargement natif en cours (cf. try_lock plus bas).
                //
                // `wet_applied` : a-t-on réellement traité via le plugin ce bloc ?
                // Sert à détecter la bascule dry→wet pour le crossfade (cf. fin
                // du bloc). False par défaut (dry/bypass/swap) → mis à true sur
                // le chemin wet.
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                let mut wet_applied = false;
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                'plugin_block: {
                if !stereo.is_empty()
                    && !plugin_bypass.load(std::sync::atomic::Ordering::Relaxed)
                {
                    let handle_opt = *plugin_handle.lock();
                    if let Some(handle) = handle_opt {
                        // try_lock : ne JAMAIS bloquer le thread audio. Pendant
                        // un (dé)chargement plugin natif lent (load/unload), le
                        // lock est tenu ailleurs → on laisse passer le signal
                        // SEC ce bloc (dry passthrough) → zéro coupure au swap.
                        let Some(mut host) = plugin_host.try_lock() else {
                            // Purge la file MIDI : events périmés pour le plugin
                            // en cours de swap (sinon burst au bloc suivant).
                            // Lock court (parking_lot Mutex, non-contendu en
                            // régime établi) ; set_input_source est rare.
                            let guard = midi_event_rx.lock();
                            if let Some(rx) = guard.as_ref() {
                                while rx.try_recv().is_ok() {}
                            }
                            drop(guard);
                            // Signal sec → continue vers RMS/self-monitor/encode.
                            break 'plugin_block;
                        };

                        // Drain les events MIDI accumulés depuis le dernier
                        // bloc et convertit le `captured_at` (timestamp midir)
                        // en `frame_offset` sample-accurate (cf.
                        // `midi_frame_offset`). Cible : timing DAW-grade
                        // (~20 µs) sur pads/batterie, vs ±1,33 ms RMS du
                        // dispatch block-quantized antérieur.
                        //
                        // Max 64 events / bloc (limite défensive). Lock court :
                        // re-lookup de l'Option par bloc → suit auto les
                        // bascules MIDI/AUDIO de set_input_source.
                        let n_pairs = stereo.len() / 2;
                        let block_start = std::time::Instant::now();
                        let midi_events: Vec<MidiEvent> = {
                            let guard = midi_event_rx.lock();
                            match guard.as_ref() {
                                Some(rx) => {
                                    let mut batch = Vec::new();
                                    let max_offset = (n_pairs as u32).saturating_sub(1);
                                    while let Ok(cap) = rx.try_recv() {
                                        batch.push(MidiEvent {
                                            frame_offset: midi_frame_offset(
                                                cap.captured_at,
                                                block_start,
                                                max_offset,
                                            ),
                                            data: cap.data,
                                        });
                                        if batch.len() >= 64 {
                                            break;
                                        }
                                    }
                                    batch
                                }
                                None => Vec::new(),
                            }
                        };

                        // Sprint B talkback auto-mute — détecte Note ON
                        // (status 0x90..0x9F + velocity > 0) dans le batch. Set
                        // midi_active=true et stocke le timestamp pour le reset
                        // timeout 200 ms (vérifié à chaque bloc plus bas).
                        for ev in &midi_events {
                            let status = ev.data[0];
                            let velocity = ev.data[2];
                            if (status & 0xF0) == 0x90 && velocity > 0 {
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                midi_last_note_on_ms.store(now_ms, std::sync::atomic::Ordering::Relaxed);
                                midi_active.store(true, std::sync::atomic::Ordering::Relaxed);
                                break; // Un seul Note ON suffit pour le flag.
                            }
                        }
                        // Reset midi_active si timeout 200 ms sans Note ON (chaque bloc).
                        if midi_active.load(std::sync::atomic::Ordering::Relaxed) {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            let last_ms = midi_last_note_on_ms.load(std::sync::atomic::Ordering::Relaxed);
                            if now_ms.saturating_sub(last_ms) >= 200 {
                                midi_active.store(false, std::sync::atomic::Ordering::Relaxed);
                            }
                        }

                        // Chantier B — crossfade dry→wet : si le plugin vient de
                        // s'activer (bloc précédent en dry), amorce le fondu et
                        // sauvegarde le signal SEC AVANT que le plugin n'écrase
                        // `stereo` (le blend a lieu après le traitement).
                        if !wet_was_active {
                            fade_remaining = FADE_SAMPLES;
                        }
                        if fade_remaining > 0 {
                            dry_scratch.clear();
                            dry_scratch.extend_from_slice(&stereo);
                        }

                        let mut idx = 0;
                        // Dispatch sample-accurate par sous-bloc (cf.
                        // `dispatch_subblock_midi`). Le cas commun (CPAL
                        // Fixed(128) = 1 sous-bloc = tout le bloc) clone
                        // simplement les events sans ajustement d'offset.
                        let mut subblock_midi: Vec<MidiEvent> = Vec::with_capacity(64);
                        while idx < n_pairs {
                            let end = (idx + PLUGIN_BLOCK).min(n_pairs);
                            plugin_left.clear();
                            plugin_right.clear();
                            for i in idx..end {
                                plugin_left.push(stereo[i * 2]);
                                plugin_right.push(stereo[i * 2 + 1]);
                            }
                            dispatch_subblock_midi(
                                &midi_events,
                                idx as u32,
                                end as u32,
                                &mut subblock_midi,
                            );
                            // Wall-clock guard plugin INSERT (mesure par
                            // sous-bloc). Coût `Instant::now()` ≈ 30 ns × 2.
                            let t_plugin = std::time::Instant::now();
                            let plugin_result = host.process_stereo(
                                handle,
                                &mut plugin_left,
                                &mut plugin_right,
                                &subblock_midi,
                            );
                            let plugin_elapsed_ms =
                                t_plugin.elapsed().as_secs_f32() * 1000.0;
                            perfstats.plugin_latency.lock().observe(plugin_elapsed_ms);
                            match plugin_result {
                                Ok(()) => {
                                    for (k, j) in (idx..end).enumerate() {
                                        stereo[j * 2] = plugin_left[k];
                                        stereo[j * 2 + 1] = plugin_right[k];
                                    }
                                }
                                Err(e) => {
                                    static FAILS: std::sync::atomic::AtomicU64 =
                                        std::sync::atomic::AtomicU64::new(0);
                                    let n = FAILS.fetch_add(
                                        1,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    if n == 0 || n.is_power_of_two() {
                                        tracing::warn!(
                                            target: "jamodio::plugin",
                                            handle = ?handle,
                                            count = n + 1,
                                            error = %e,
                                            "process_stereo failed in process_stage (signal passe DRY)"
                                        );
                                    }
                                }
                            }
                            idx = end;
                        }

                        // Chantier B — applique le fondu équal-power dry→wet sur
                        // la fenêtre restante. `dry_scratch` (sec) et `stereo`
                        // (wet) sont alignés (même longueur, interleaved stéréo).
                        if fade_remaining > 0 {
                            fade_remaining = apply_dry_wet_fade(
                                &mut stereo,
                                &dry_scratch,
                                fade_remaining,
                                FADE_SAMPLES,
                            );
                        }
                        wet_applied = true;
                    } else {
                        // Pas de plugin actif (ou swap en cours, handle=None) →
                        // purge la file MIDI pour éviter un burst d'events
                        // périmés (note-on orphelins) au prochain plugin.
                        let guard = midi_event_rx.lock();
                        if let Some(rx) = guard.as_ref() {
                            while rx.try_recv().is_ok() {}
                        }
                        drop(guard);
                    }
                }
                } // 'plugin_block

                // Chantier B — mémorise l'état wet pour détecter la prochaine
                // bascule dry→wet (déclenche le crossfade). Hors plugin (dry,
                // bypass, swap, échec) → wet_applied reste false → le retour au
                // wet refera un fondu propre.
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    wet_was_active = wet_applied;
                }

                // Chantier C — soft-clip de sécurité sur la sortie post-plugin
                // (couvre self-monitor + encode + record, tous en aval). Zéro
                // latence. Remonte le pic d'entrée (pré-clip) → indicateur CLIP
                // si le plugin sort > 0 dBFS (l'user doit baisser sa sortie).
                if !stereo.is_empty() {
                    // 0.98 ≈ -0,17 dBFS : ne shape QUE le tout haut (vrais
                    // dépassements pleine-échelle), pas le signal fort propre.
                    const SOFT_CLIP_THRESHOLD: f32 = 0.98;
                    let (peak, overs) = soft_clip_block(&mut stereo, SOFT_CLIP_THRESHOLD);
                    use std::sync::atomic::Ordering::Relaxed;
                    perfstats.output_peak.fetch_max(peak.to_bits(), Relaxed);
                    perfstats.output_clip_samples.fetch_add(overs, Relaxed);
                    perfstats
                        .output_total_samples
                        .fetch_add(stereo.len() as u64, Relaxed);
                }

                // RMS + self-monitor (= ce que l'utilisateur entend wet
                // dans son casque via le callback CPAL playback).
                if !stereo.is_empty() {
                    let sum_sq: f32 = stereo.iter().map(|s| s * s).sum();
                    let rms = (sum_sq / stereo.len() as f32).sqrt();
                    input_rms
                        .store(rms.to_bits(), std::sync::atomic::Ordering::Relaxed);
                    mixer.lock().push_self_samples(&stereo);
                }

                // v0.4.8 — observe le temps de traitement PUR du process_stage.
                let process_elapsed_ms =
                    t_stage_start.elapsed().as_secs_f32() * 1000.0;
                perfstats.process_latency.lock().observe(process_elapsed_ms);

                // Forward vers encode_stage avec le timestamp original.
                if out_tx.send((t_block_start, stereo)).is_err() {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Sprint S3 — Encode stage
// ═══════════════════════════════════════════════════════════════════
//
// Responsabilités :
//   - Accumuler les samples reçus dans un buffer ; émettre des frames
//     Opus de 240 f32 stéréo (= 120 samples par canal, 2.5 ms @ 48 kHz)
//   - Encoder en Opus
//   - Construire le packet RTP
//   - try_send vers le channel tokio (= UDP task qui chiffre + send)
//   - Observer `pipeline_latency` end-to-end (= elapsed depuis le
//     `t_block_start` apposé par capture_stage) → conserve la sémantique
//     historique du `pipeline_latency_ms` mesuré côté browser.
//
// Coût observé : ~50-100 µs par frame (Opus encode est constant). Spike
// rarissime (Opus interne).

fn encode_stage_loop(
    in_rx: Receiver<TimedBlock>,
    sender: Arc<RtpSender>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    ssrc: u32,
    payload_type: u8,
    perfstats: PerfHandles,
    output_device_name: Option<String>,
) {
    let _rt_priority_handle = crate::audio::rt_priority::promote_thread_for_audio(
        output_device_name.as_deref(),
    );

    let encoder = match MusicEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(
                target: "jamodio::encoder",
                error = %e,
                "failed to create Opus encoder"
            );
            return;
        }
    };

    let frame_size = encoder.frame_size(); // 120 samples/channel
    let frame_len = frame_size * CHANNELS; // 240 f32s stéréo interleaved
    let mut accumulator: Vec<f32> = Vec::with_capacity(frame_len * 2);
    let mut opus_buf = vec![0u8; 4000];
    let mut sequence: u16 = 0;
    let mut timestamp: u32 = 0;

    loop {
        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        match in_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok((t_block_start, stereo)) => {
                // v0.4.8 — timer "traitement pur" du encode_stage : depuis le
                // pop ringbuf jusqu'au try_send RTP final. NB : `block_elapsed_ms`
                // (pipeline_latency end-to-end) reste mesuré depuis t_block_start.
                let t_stage_start = std::time::Instant::now();
                accumulator.extend_from_slice(&stereo);

                // 0.5.3 — comptage de la RAFALE : combien de frames Opus ce bloc
                // d'entrée fait-il partir d'affilée ? (cf. PerfHandles::emit_burst)
                let mut frames_this_block: u32 = 0;
                while accumulator.len() >= frame_len {
                    // Encode directement depuis le slice de l'accumulateur —
                    // l'ancien `drain(..).collect()` allouait un Vec par frame
                    // Opus (~400/s) sur le hot path (review 11/06). Le drain
                    // sans collect (après encode) ne réalloue pas.
                    match encoder.encode(&accumulator[..frame_len], &mut opus_buf) {
                        Ok(encoded_len) => {
                            let header = RtpHeader {
                                payload_type,
                                sequence,
                                timestamp,
                                ssrc,
                                marker: sequence == 0,
                            };
                            let packet =
                                rtp::build_packet(&header, &opus_buf[..encoded_len]);

                            // 0.5.3-3 — ÉMISSION RT : chiffrement SRTP + send_to
                            // NON-BLOQUANT directement ici (thread d'encode RT),
                            // plus de hop tokio (supprime la gigue d'égression sous
                            // charge Windows). `send_path` mesure désormais le coût
                            // protect+send_to (doit lire ~0). Sur WouldBlock (buffer
                            // noyau plein, rarissime) on DROP la frame (concealée par
                            // le PLC récepteur) au lieu de staller le thread RT.
                            let produced_at = std::time::Instant::now();
                            match sender.send_blocking(packet) {
                                Ok(_) => {
                                    let send_delay_ms = produced_at.elapsed().as_secs_f32() * 1000.0;
                                    perfstats.send_path_latency.lock().observe(send_delay_ms);
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    static WOULDBLOCK: std::sync::atomic::AtomicU64 =
                                        std::sync::atomic::AtomicU64::new(0);
                                    let n = WOULDBLOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if n == 0 || n.is_power_of_two() {
                                        tracing::warn!(
                                            target: "jamodio::encoder",
                                            drop_count = n + 1,
                                            "UDP send buffer full (WouldBlock) — frame dropped (réseau/CPU saturé ?)"
                                        );
                                    }
                                }
                                Err(e) => {
                                    static SENDERR: std::sync::atomic::AtomicU64 =
                                        std::sync::atomic::AtomicU64::new(0);
                                    let n = SENDERR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if n == 0 || n.is_power_of_two() {
                                        tracing::warn!(target: "jamodio::encoder", error = %e, drop_count = n + 1, "UDP send_to error");
                                    }
                                }
                            }

                            sequence = sequence.wrapping_add(1);
                            timestamp = timestamp.wrapping_add(frame_size as u32);
                            frames_this_block += 1;
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "jamodio::encoder",
                                error = %e,
                                "Opus encode error"
                            );
                        }
                    }
                    // Consomme la frame encodée (ou ratée — comportement
                    // identique à l'ancien drain-avant-encode) : drain pur,
                    // sans collect → pas d'allocation.
                    accumulator.drain(..frame_len);
                }

                // 0.5.3 — enregistre la rafale de CE bloc. On observe même 0
                // (bloc plus petit qu'une frame = sous-frame = AUCUNE rafale,
                // c'est le bon signe) : la moyenne reflète alors frames/bloc réel.
                perfstats.emit_burst.lock().observe(frames_this_block as f32);

                // Sprint S1/S3 — pipeline_latency end-to-end. Le timestamp
                // `t_block_start` est apposé par `capture_stage_loop` en
                // début de pipeline et nous arrive intact ici via les
                // channels. Le elapsed mesure donc EXACTEMENT le temps
                // CPAL-recv → RTP-send, comme l'ancien `encoder_thread`
                // monolithique, ce qui garantit la continuité de la
                // baseline v0.4.1.
                let block_elapsed_ms =
                    t_block_start.elapsed().as_secs_f32() * 1000.0;
                perfstats.pipeline_latency.lock().observe(block_elapsed_ms);

                // v0.4.8 — observe le temps de traitement PUR du encode_stage.
                // pipeline_latency - encode_latency - process_latency -
                // capture_latency = temps en file dans les ringbufs entre stages.
                let encode_elapsed_ms =
                    t_stage_start.elapsed().as_secs_f32() * 1000.0;
                perfstats.encode_latency.lock().observe(encode_elapsed_ms);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Réception : tâches I/O async (1/pair) + UN thread de décodage RT partagé
// ═══════════════════════════════════════════════════════════════════
//
// Pourquoi ce split (0.5.3-2, fix « injouable Windows ») :
// Le décodage Opus alimente le jitter buffer (`push_samples`) ; le callback de
// SORTIE le draine. Si le décodage tourne en priorité NORMALE (ancien
// `recv_decode_task` sur le pool tokio), Windows le préempte ~10-15 ms → le
// buffer n'est pas réalimenté → underrun → `adapt_up` colle la cible au plafond
// 40 ms → +25 ms de latence (injouable). L'émission, elle, est RT (MMCSS) → le
// self-monitor ne décroche jamais : asymétrie. macOS masque le trou (scheduler
// clément). Tous les concurrents (JackTrip/SonoBus/Jamulus) mettent la réception
// sur un thread RT — jamais normal.
//
// Design (validé en triple revue senior) :
//   - `recv_io_task` (async tokio, 1/pair) : recv UDP + horodatage d'arrivée +
//     comedia punch + idle-timeout fantôme. Forwarde le paquet brut via un MPSC.
//   - `decode_rt_loop` (UN seul std::thread, RT) : décode pour TOUS les pairs
//     (HashMap d'état). Promotion « event-driven » (MMCSS Windows / QoS macOS
//     SEUL, PAS le workgroup → pas de sur-population). Seul écrivain du mixer côté
//     pairs (add/remove/push tous depuis ce thread) → zéro race, zéro contention
//     mutex ×N (1 thread partagé, pas thread-par-stream).
// Décode-sur-push conservé (jitter buffer en PCM, Phases B/C inchangées).

/// Message d'une `recv_io_task` vers le thread de décodage RT partagé.
enum DecodeMsg {
    /// Paquet RTP déchiffré, horodaté à l'arrivée (avant tout parse/file).
    /// `epoch` = génération de l'io task émettrice (cf. re-add même producer).
    Packet {
        producer_id: Arc<str>,
        epoch: u64,
        recv_instant: std::time::Instant,
        buf: Vec<u8>,
    },
    /// Pair terminé (stop ou idle-timeout) : envoyé en DERNIER par l'io task →
    /// le thread retire l'état + le stream mixer APRÈS le dernier paquet du pair
    /// (ordre garanti : l'io task est l'unique émetteur de ce producteur).
    /// `epoch` : on n'honore le Remove que s'il matche la génération courante
    /// (sinon un Remove d'une ancienne connexion supprimerait un stream re-créé).
    Remove { producer_id: Arc<str>, epoch: u64 },
    /// Arrêt complet (stop_all).
    Shutdown,
}

/// État de décodage par pair — détenu UNIQUEMENT par le thread RT.
struct DecodeState {
    /// Génération de l'io task propriétaire (cf. epoch dans `DecodeMsg`).
    epoch: u64,
    decoder: MusicDecoder,
    drift: DriftEstimator,
    jitter: JitterEstimator,
    last_seq: Option<u16>,
    last_pushed: ProducerNetStats,
    pkt_count: u64,
    logged_large_jump: bool,
}

impl DecodeState {
    fn new(producer_id: &str, epoch: u64) -> Option<Self> {
        let decoder = match MusicDecoder::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(target: "jamodio::recv", producer = %producer_id, error = %e, "failed to create decoder");
                return None;
            }
        };
        let drift_label = producer_id.chars().take(8).collect::<String>();
        Some(Self {
            epoch,
            decoder,
            drift: DriftEstimator::new(drift_label),
            jitter: JitterEstimator::new(),
            last_seq: None,
            last_pushed: ProducerNetStats::default(),
            pkt_count: 0,
            logged_large_jump: false,
        })
    }
}

/// Handle du thread de décodage RT partagé, détenu par `PipelineState`.
struct DecodeThread {
    /// MPSC vers le thread (paquets + lifecycle). Cloné dans chaque io task.
    tx: Sender<DecodeMsg>,
    /// Pool de buffers recyclés. Cloné dans chaque io task (côté réception).
    pool_rx: Receiver<Vec<u8>>,
    join: std::thread::JoinHandle<()>,
}

/// Démarre le thread de décodage RT unique (lazy, au 1er stream). Faillible
/// (cohérent avec le spawn de l'encoder thread) : une erreur OS de création de
/// thread est propagée au lieu de paniquer.
fn spawn_decode_thread(
    mixer: Arc<Mutex<AudioMixer>>,
    net_stats_by_producer: Arc<Mutex<HashMap<String, ProducerNetStats>>>,
    recv_path: Arc<Mutex<Histogram>>,
) -> std::io::Result<DecodeThread> {
    // Data MPSC : N io tasks → 1 thread. 256 = large (décode ≫ arrivée).
    let (tx, rx) = bounded::<DecodeMsg>(256);
    // Pool : buffers MTU réutilisés → zéro alloc/dealloc sur le thread RT.
    let (pool_tx, pool_rx) = bounded::<Vec<u8>>(128);
    for _ in 0..128 {
        let _ = pool_tx.try_send(Vec::with_capacity(2048));
    }
    let join = std::thread::Builder::new()
        .name("audio-decode".into())
        .spawn(move || decode_rt_loop(rx, pool_tx, mixer, net_stats_by_producer, recv_path))?;
    Ok(DecodeThread { tx, pool_rx, join })
}

/// Boucle du thread de décodage RT. Promu en tête. Multiplexe tous les pairs.
fn decode_rt_loop(
    rx: Receiver<DecodeMsg>,
    pool_tx: Sender<Vec<u8>>,
    mixer: Arc<Mutex<AudioMixer>>,
    net_stats_by_producer: Arc<Mutex<HashMap<String, ProducerNetStats>>>,
    recv_path: Arc<Mutex<Histogram>>,
) {
    // Promotion « event-driven » : MMCSS « Pro Audio » (Windows) / QoS
    // USER_INTERACTIVE seul (macOS, PAS le workgroup) / thread-priority (Linux).
    let _rt = crate::audio::rt_priority::promote_thread_for_audio_recv();

    let mut states: HashMap<Arc<str>, DecodeState> = HashMap::new();

    while let Ok(msg) = rx.recv() {
        match msg {
            DecodeMsg::Shutdown => break,
            DecodeMsg::Remove { producer_id, epoch } => {
                // N'honore le Remove que pour la génération courante : un Remove
                // d'une ancienne connexion (re-add même producer) ne doit PAS
                // supprimer le stream re-créé par la nouvelle génération.
                if states.get(&producer_id).map(|st| st.epoch) == Some(epoch) {
                    states.remove(&producer_id);
                    mixer.lock().remove_stream(&producer_id);
                    // Sans ça, un peer disparu laisserait un ppm fantôme dans la
                    // map → PerfStats continuerait à mentionner ce peer mort.
                    net_stats_by_producer.lock().remove(&*producer_id);
                }
            }
            DecodeMsg::Packet { producer_id, epoch, recv_instant, buf } => {
                // (Re)création de l'état + du stream mixer selon la génération.
                let needs_create = match states.get(&producer_id) {
                    Some(st) if st.epoch == epoch => false,
                    // Paquet d'une génération PÉRIMÉE (ancienne connexion qui
                    // traîne après un re-add) → ignoré.
                    Some(st) if st.epoch > epoch => {
                        let _ = pool_tx.try_send(buf);
                        continue;
                    }
                    // Génération plus RÉCENTE que l'état présent → l'ancienne est
                    // supersédée : on retire son stream avant d'en recréer un.
                    Some(_) => {
                        mixer.lock().remove_stream(&producer_id);
                        true
                    }
                    None => true,
                };
                if needs_create {
                    match DecodeState::new(&producer_id, epoch) {
                        Some(st) => {
                            mixer.lock().add_stream(&producer_id);
                            states.insert(producer_id.clone(), st);
                        }
                        None => {
                            let _ = pool_tx.try_send(buf);
                            continue;
                        }
                    }
                }
                let st = states.get_mut(&producer_id).expect("état présent ou créé juste au-dessus");
                decode_one_packet(st, &producer_id, recv_instant, &buf, &mixer, &net_stats_by_producer, &recv_path);
                // Recycle le buffer (capacité conservée) → zéro alloc/dealloc RT.
                let _ = pool_tx.try_send(buf);
            }
        }
    }

    // Shutdown : nettoie les streams mixer + net_stats restants (Remove non
    // encore traités). Sépare les locks (jamais les deux en même temps).
    {
        let mut m = mixer.lock();
        for id in states.keys() {
            m.remove_stream(id);
        }
    }
    {
        let mut ns = net_stats_by_producer.lock();
        for id in states.keys() {
            ns.remove(&**id);
        }
    }
}

/// Décode UN paquet pour `st` et le pousse dans le jitter buffer. Tourne sur le
/// thread RT. `recv_instant` = arrivée réseau horodatée par `recv_io_task`
/// (JAMAIS un `Instant::now()` ici, sinon le délai de file polluerait la gigue).
#[allow(clippy::too_many_arguments)]
fn decode_one_packet(
    st: &mut DecodeState,
    producer_id: &str,
    recv_instant: std::time::Instant,
    buf: &[u8],
    mixer: &Arc<Mutex<AudioMixer>>,
    net_stats_by_producer: &Arc<Mutex<HashMap<String, ProducerNetStats>>>,
    recv_path: &Arc<Mutex<Histogram>>,
) {
    let short = &producer_id[..8.min(producer_id.len())];
    st.pkt_count += 1;
    if st.pkt_count == 1 {
        tracing::info!(target: "jamodio::recv", producer = short, bytes = buf.len(), "first RTP packet received");
    } else if st.pkt_count.is_multiple_of(5000) {
        tracing::debug!(target: "jamodio::recv", producer = short, count = st.pkt_count, "RTP packets received");
    }

    let Some((header, payload)) = rtp::parse_header(buf) else {
        return;
    };

    // Estimateurs de timing réseau (mesure pure). Un unique instant d'arrivée
    // (celui horodaté dans recv_io_task) pour drift ET gigue.
    st.drift.observe(header.timestamp, recv_instant);
    st.jitter.observe(header.timestamp, recv_instant);
    // Miroir paresseux dans la map partagée : on n'écrit que si drift > 1 ppm OU
    // gigue > 0,5 ms de variation depuis la dernière écriture (limite la
    // contention ; ws_server lit à 1 Hz).
    let current = ProducerNetStats {
        drift_ppm: st.drift.drift_ppm(),
        jitter_ms: st.jitter.jitter_ms(),
    };
    if (current.drift_ppm - st.last_pushed.drift_ppm).abs() > 1.0
        || (current.jitter_ms - st.last_pushed.jitter_ms).abs() > 0.5
    {
        net_stats_by_producer.lock().insert(producer_id.to_string(), current);
        st.last_pushed = current;
    }
    // Phase B — pilote la cible du jitter buffer avec la gigue mesurée, ~10×/s
    // (1 paquet sur 40) et seulement une fois l'estimateur fiable (warmup).
    if st.jitter.is_warm() && st.pkt_count.is_multiple_of(40) {
        let jitter_ms = st.jitter.jitter_ms();
        mixer.lock().observe_jitter(producer_id, jitter_ms);
    }
    // Détection de perte → PLC
    if let Some(prev) = st.last_seq {
        let expected = prev.wrapping_add(1);
        if header.sequence != expected {
            let gap = header.sequence.wrapping_sub(expected);
            if gap <= 10 {
                for _ in 0..gap.min(3) {
                    // Copie obligatoire avant push : decode_loss() rend une slice
                    // d'un buffer interne écrasé au decode suivant (Sprint 3 BUG 7).
                    let plc_owned: Option<Vec<f32>> = st.decoder.decode_loss().map(|s| s.to_vec());
                    if let Some(plc) = plc_owned {
                        mixer.lock().push_samples(producer_id, &plc);
                    }
                }
            } else if !st.logged_large_jump {
                tracing::warn!(target: "jamodio::recv", producer = short, prev_seq = prev, got_seq = header.sequence, gap, "large seq jump (skipping PLC)");
                st.logged_large_jump = true;
            }
        }
    }
    st.last_seq = Some(header.sequence);

    // Décode le paquet + push. recv_path = arrivée réseau → juste avant push
    // (file MPSC + parse + décode) : doit lire ~0,1-0,5 ms si le thread RT tient.
    if let Some(pcm) = st.decoder.decode(payload) {
        let recv_path_ms = recv_instant.elapsed().as_secs_f32() * 1000.0;
        recv_path.lock().observe(recv_path_ms);
        mixer.lock().push_samples(producer_id, pcm);
    }
}

/// Tâche I/O de réception (async tokio, 1 par pair). Recv UDP + horodatage +
/// comedia punch + idle-timeout. Ne décode RIEN : forwarde le paquet brut au
/// thread de décodage RT. Envoie un `Remove` terminal sur sortie (stop/idle).
#[allow(clippy::too_many_arguments)]
async fn recv_io_task(
    receiver: RtpReceiver,
    sfu_addr: SocketAddr,
    producer_id: Arc<str>,
    epoch: u64,
    tx: Sender<DecodeMsg>,
    pool_rx: Receiver<Vec<u8>>,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let short = &producer_id[..8.min(producer_id.len())];

    // Punch périodique pour comedia : on retry jusqu'au 1er paquet entrant.
    // 100 ms × 30 = 3 s (marge pour le connect-plain-transport du browser).
    let mut punch_interval = tokio::time::interval(std::time::Duration::from_millis(100));
    punch_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut punch_remaining: u32 = 30;

    // Idle-timeout : un pair vivant envoie ~400 pkt/s (Opus CBR + DTX OFF →
    // paquets continus MÊME en silence, cf. encoder.rs). 8 s sans paquet = flux
    // mort (reconnexion non signalée par le browser) → auto-terminaison pour
    // nettoyer le producteur fantôme.
    let idle_timeout = std::time::Duration::from_secs(8);
    let mut idle_check = tokio::time::interval(std::time::Duration::from_secs(2));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_packet = std::time::Instant::now();
    let mut got_first = false;

    // Buffer courant (recyclé via le pool). 2048 ≥ MTU + tag SRTP + en-tête RTP.
    let mut buf: Vec<u8> = pool_rx.try_recv().unwrap_or_else(|_| Vec::with_capacity(2048));

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = punch_interval.tick(), if punch_remaining > 0 => {
                let _ = receiver.punch(sfu_addr).await;
                punch_remaining -= 1;
            }
            _ = idle_check.tick() => {
                if got_first && last_packet.elapsed() >= idle_timeout {
                    tracing::warn!(target: "jamodio::recv", producer = short, "no packet for 8s — terminating (ghost/orphan stream)");
                    break;
                }
            }
            result = receiver.recv(&mut buf) => {
                match result {
                    Ok((len, _addr)) if len > 0 => {
                        // Horodatage d'arrivée — ICI, avant tout parse/file (load-bearing).
                        let recv_instant = std::time::Instant::now();
                        last_packet = recv_instant;
                        // 1er paquet valide : comedia activé → on stoppe les punches.
                        if !got_first {
                            got_first = true;
                            punch_remaining = 0;
                        }
                        // Échange le buffer plein contre un neuf (pool) et envoie
                        // le plein au thread de décodage.
                        let fresh = pool_rx.try_recv().unwrap_or_else(|_| Vec::with_capacity(2048));
                        let full = std::mem::replace(&mut buf, fresh);
                        if tx
                            .send(DecodeMsg::Packet {
                                producer_id: producer_id.clone(),
                                epoch,
                                recv_instant,
                                buf: full,
                            })
                            .is_err()
                        {
                            break; // thread de décodage parti (shutdown)
                        }
                    }
                    // len == 0 : RTCP filtré / échec SRTP (déjà loggé) → on réutilise buf.
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(target: "jamodio::recv", producer = %producer_id, error = %e, "UDP recv error");
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        }
    }

    // Message terminal : le thread retire l'état + le stream mixer + l'entrée
    // net_stats de ce pair, APRÈS notre dernier paquet (ordre garanti — émetteur
    // unique) → zéro 'unknown stream', zéro ppm fantôme.
    let _ = tx.send(DecodeMsg::Remove { producer_id, epoch });
}

// ═══════════════════════════════════════════════════════════════════
// Chantier A (v0.4.12) — tests PluginControl (load/unload non-bloquant)
// ═══════════════════════════════════════════════════════════════════
//
// macOS uniquement : on charge de vrais AudioUnits Apple natifs (présents
// sur toute machine macOS, load rapide). Sous `cargo test`, NSApp est nil
// donc l'hôte exécute load/unload inline (cf. jmo_run_on_main_sync). La
// parité Windows (VST3) sera couverte dans la session de validation Windows.
//
// Ce qu'on valide : la machine à états de PluginControl (handle / info /
// flags). La propriété « 0 drops pendant un load » (= le thread audio ne
// bloque pas) dépend du hardware audio → validée on-device.
#[cfg(all(test, target_os = "macos"))]
mod plugin_control_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn make_control() -> PluginControl {
        PluginControl {
            plugin_host: Arc::new(Mutex::new(PluginHostImpl::new())),
            instrument_plugin_handle: Arc::new(Mutex::new(None)),
            // bypass=true + overload=true au départ → on vérifie que load() les
            // remet à false (= fresh start).
            instrument_plugin_bypass: Arc::new(AtomicBool::new(true)),
            plugin_auto_bypass_active: Arc::new(AtomicBool::new(true)),
            plugin_scan_cache: Arc::new(Mutex::new(PluginScanCache::Scanning)),
            instrument_plugin_info: Arc::new(Mutex::new(None)),
            plugin_latency: Arc::new(Mutex::new(Histogram::new(64))),
        }
    }

    // AUNBandEQ — effet Apple natif, présent partout, load rapide & déterministe.
    fn eq_ref() -> PluginRef {
        PluginRef::Au {
            au_type: "aufx".into(),
            subtype: "nbeq".into(),
            manufacturer: "appl".into(),
        }
    }

    #[test]
    fn load_sets_handle_and_resets_flags() {
        let ctrl = make_control();
        ctrl.load(&eq_ref()).expect("load AUNBandEQ");
        assert!(
            ctrl.instrument_plugin_handle.lock().is_some(),
            "handle posé après load (→ thread audio passe en wet)"
        );
        assert!(
            ctrl.instrument_plugin_info.lock().is_some(),
            "snapshot info posé (resync reconnect)"
        );
        assert!(
            !ctrl.instrument_plugin_bypass.load(Ordering::Relaxed),
            "bypass reset à false au load"
        );
        assert!(
            !ctrl.plugin_auto_bypass_active.load(Ordering::SeqCst),
            "flag overload reset à false au load"
        );
        ctrl.unload();
    }

    #[test]
    fn unload_clears_handle_and_info() {
        let ctrl = make_control();
        ctrl.load(&eq_ref()).expect("load");
        ctrl.unload();
        assert!(
            ctrl.instrument_plugin_handle.lock().is_none(),
            "handle libéré au unload (→ thread audio en dry)"
        );
        assert!(
            ctrl.instrument_plugin_info.lock().is_none(),
            "info clear au unload"
        );
    }

    #[test]
    fn reload_swap_keeps_consistent_state() {
        // Swap A→B : pas de leak/panic, l'ancien est déchargé avant le nouveau.
        let ctrl = make_control();
        ctrl.load(&eq_ref()).expect("load 1");
        ctrl.load(&eq_ref()).expect("load 2 (swap, unload interne du 1er)");
        assert!(
            ctrl.instrument_plugin_handle.lock().is_some(),
            "handle posé après swap"
        );
        ctrl.unload();
        assert!(ctrl.instrument_plugin_handle.lock().is_none());
    }

    // ─── Chantier B — crossfade dry→wet ───────────────────────────────

    #[test]
    fn fade_starts_dry_ends_wet() {
        // dry = 1.0 partout, wet = -1.0 partout (signaux opposés → on voit
        // clairement le mélange). Fondu sur 4 frames (8 samples interleaved).
        let total = 4;
        let dry = vec![1.0f32; total * 2];
        let mut wet = vec![-1.0f32; total * 2];
        let rem = apply_dry_wet_fade(&mut wet, &dry, total, total);
        assert_eq!(rem, 0, "le fondu doit être entièrement consommé");
        // Premier frame : t=0 → dry pur (g_dry=1, g_wet=0) → +1.0.
        assert!((wet[0] - 1.0).abs() < 1e-5, "1er sample = dry pur, got {}", wet[0]);
        assert!((wet[1] - 1.0).abs() < 1e-5);
        // Dernier frame du fondu : t≈0.75 → majoritairement wet → négatif.
        assert!(wet[(total - 1) * 2] < 0.0, "dernier sample tend vers wet");
    }

    #[test]
    fn fade_is_equal_power() {
        // Invariant équal-power : à chaque step, g_dry² + g_wet² == 1
        // → pas de creux de loudness. On le vérifie en mixant dry=1/wet=0
        // (sortie = g_dry) et dry=0/wet=1 (sortie = g_wet) au même index.
        let total = 16;
        let mut a = vec![0.0f32; total * 2]; // wet=0 → sortie = g_dry
        let dry1 = vec![1.0f32; total * 2];
        apply_dry_wet_fade(&mut a, &dry1, total, total);
        let mut b = vec![1.0f32; total * 2]; // wet=1, dry=0 → sortie = g_wet
        let dry0 = vec![0.0f32; total * 2];
        apply_dry_wet_fade(&mut b, &dry0, total, total);
        for k in 0..total {
            let g_dry = a[k * 2];
            let g_wet = b[k * 2];
            let power = g_dry * g_dry + g_wet * g_wet;
            assert!((power - 1.0).abs() < 1e-4, "équal-power à k={k} : {power}");
        }
    }

    #[test]
    fn fade_partial_then_completes_across_blocks() {
        // Fondu de 8 frames étalé sur 2 blocs de 4 frames : le 1er bloc
        // consomme 4, le 2e les 4 derniers → rem=0, signal 100 % wet ensuite.
        let total = 8;
        let dry = vec![1.0f32; 4 * 2];
        let mut blk1 = vec![-1.0f32; 4 * 2];
        let rem1 = apply_dry_wet_fade(&mut blk1, &dry, total, total);
        assert_eq!(rem1, 4, "1er bloc consomme 4 frames");
        let mut blk2 = vec![-1.0f32; 4 * 2];
        let rem2 = apply_dry_wet_fade(&mut blk2, &dry, rem1, total);
        assert_eq!(rem2, 0, "2e bloc termine le fondu");
    }

    #[test]
    fn failed_load_leaves_handle_none() {
        // Plugin inexistant → Err. Invariant de sûreté : le handle reste None
        // (jamais de wet sur une instance fantôme) → le thread audio reste dry.
        let ctrl = make_control();
        let bogus = PluginRef::Au {
            au_type: "aufx".into(),
            subtype: "zzzz".into(),
            manufacturer: "zzzz".into(),
        };
        assert!(ctrl.load(&bogus).is_err(), "load d'un AU inexistant doit échouer");
        assert!(
            ctrl.instrument_plugin_handle.lock().is_none(),
            "handle reste None après échec de load (pas de wet fantôme)"
        );
        assert!(
            ctrl.instrument_plugin_info.lock().is_none(),
            "info reste None après échec de load"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════
// Chantier C (v0.4.14) — tests soft-clip de sécurité (cross-platform)
// ═══════════════════════════════════════════════════════════════════
#[cfg(test)]
mod dsp_tests {
    use super::soft_clip_block;

    #[test]
    fn soft_clip_identity_below_threshold() {
        // Tout signal sous le seuil reste bit-identique (aucune coloration).
        let orig: Vec<f32> = vec![-0.9, -0.5, 0.0, 0.3, 0.93, -0.94];
        let mut x = orig.clone();
        let (peak, overs) = soft_clip_block(&mut x, 0.94);
        assert_eq!(x, orig, "signal sous seuil inchangé");
        assert!((peak - 0.94).abs() < 1e-6, "peak = max abs d'entrée");
        assert_eq!(overs, 0, "aucun sample > pleine-échelle");
    }

    #[test]
    fn soft_clip_never_exceeds_full_scale() {
        // Un plugin hot (jusqu'à ±3.0) ne doit JAMAIS sortir > ±1.0 après clip.
        let mut x: Vec<f32> = vec![3.0, -2.0, 1.5, -1.2, 1.0, 0.97];
        let (peak, overs) = soft_clip_block(&mut x, 0.94);
        assert!(peak >= 3.0 - 1e-6, "peak reflète l'entrée brute (3.0)");
        // 4 samples dépassent STRICTEMENT 1.0 (3.0, 2.0, 1.5, 1.2) → vrais
        // écrêtages comptés ; 1.0 et 0.97 ne comptent pas.
        assert_eq!(overs, 4, "compte les samples > pleine-échelle");
        for &v in &x {
            // Asymptote = ±1.0 (0 dBFS, plein-échelle représentable, PAS un
            // clip) : jamais dépassé, atteint sur très gros dépassement (tanh
            // sature).
            assert!(v.abs() <= 1.0 + 1e-6, "sortie bornée à 0 dBFS, got {v}");
            assert!(v.abs() >= 0.94 - 1e-6, "au-dessus du seuil, reste >= seuil");
        }
    }

    #[test]
    fn soft_clip_continuous_at_threshold() {
        // Continuité : une valeur juste au-dessus du seuil ne saute pas (pas de
        // marche). On compare l'identité (seuil) au clip d'un poil au-dessus.
        let t = 0.94f32;
        let mut just_above = vec![t + 1e-4];
        let _ = soft_clip_block(&mut just_above, t);
        assert!(
            (just_above[0] - t).abs() < 1e-3,
            "transition douce au seuil, got {}",
            just_above[0]
        );
        // Monotonie + signe préservés.
        let mut neg = vec![-2.0f32];
        let _ = soft_clip_block(&mut neg, t);
        assert!(neg[0] < 0.0, "signe préservé");
    }

    #[test]
    fn soft_clip_transients_vs_sustained_overs() {
        // Le voyant CLIP doit distinguer un transitoire (peu d'overs) d'un
        // overdrive soutenu (beaucoup d'overs). Bloc de 4800 samples : un
        // transitoire = quelques samples > 1.0 ; un overdrive soutenu = la
        // majorité. C'est le TAUX (overs/total) qui compte, pas le pic.
        let mut transient = vec![0.3f32; 4800];
        for s in transient.iter_mut().take(8) {
            *s = 2.0; // 8 samples au-dessus de 1.0 sur 4800 = 0.17 %
        }
        let (_p, overs_t) = soft_clip_block(&mut transient, 0.98);
        assert_eq!(overs_t, 8);
        let pct_t = 100.0 * overs_t as f32 / 4800.0;
        assert!(pct_t < 1.0, "transitoire = taux faible ({pct_t} %)");

        let mut sustained = vec![1.5f32; 4800]; // tout dépasse 1.0
        let (_p2, overs_s) = soft_clip_block(&mut sustained, 0.98);
        let pct_s = 100.0 * overs_s as f32 / 4800.0;
        assert!(pct_s > 50.0, "overdrive soutenu = taux élevé ({pct_s} %)");
    }
}

// ─── MIDI sample-accurate dispatch (Chantier #1) ─────────────────────────

#[cfg(test)]
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod midi_dispatch_tests {
    use super::{dispatch_subblock_midi, midi_frame_offset};
    use jamodio_audio_core::plugin_host::MidiEvent;
    use std::time::{Duration, Instant};

    // ─── midi_frame_offset ───

    #[test]
    fn frame_offset_event_at_block_start_is_zero() {
        let t0 = Instant::now();
        assert_eq!(midi_frame_offset(t0, t0, 127), 0);
    }

    #[test]
    fn frame_offset_proportional_to_us_delay() {
        // 1 ms delay @ 48 kHz = 48 samples.
        let block_start = Instant::now();
        let captured = block_start + Duration::from_micros(1000);
        assert_eq!(midi_frame_offset(captured, block_start, 127), 48);
    }

    #[test]
    fn frame_offset_subsample_precision_truncates_below_sample() {
        // 10 µs delay @ 48 kHz = 0.48 sample → tronqué à 0 (= snap au début).
        let block_start = Instant::now();
        let captured = block_start + Duration::from_micros(10);
        assert_eq!(midi_frame_offset(captured, block_start, 127), 0);
    }

    #[test]
    fn frame_offset_event_before_block_start_snaps_to_zero() {
        // Event arrivé pendant le queueing du bloc précédent : captured_at
        // antérieur à block_start → 0 (position la plus précoce du bloc).
        let block_start = Instant::now();
        let captured = block_start - Duration::from_micros(500);
        assert_eq!(midi_frame_offset(captured, block_start, 127), 0);
    }

    #[test]
    fn frame_offset_clamps_to_max_offset() {
        // Event tardif (10 ms = 480 samples) avec bloc 128 → clampé à 127.
        let block_start = Instant::now();
        let captured = block_start + Duration::from_millis(10);
        assert_eq!(midi_frame_offset(captured, block_start, 127), 127);
    }

    #[test]
    fn frame_offset_zero_max_returns_zero() {
        // max_offset = 0 (bloc vide théorique) → tous events à 0.
        let block_start = Instant::now();
        let captured = block_start + Duration::from_millis(2);
        assert_eq!(midi_frame_offset(captured, block_start, 0), 0);
    }

    // ─── dispatch_subblock_midi ───

    fn ev(frame_offset: u32) -> MidiEvent {
        MidiEvent { frame_offset, data: [0x90, 60, 100] }
    }

    #[test]
    fn dispatch_single_subblock_keeps_all_events_unchanged() {
        // Cas commun CPAL Fixed(128) = 1 sous-bloc = tout le bloc :
        // les offsets restent identiques car sub_start = 0.
        let events = vec![ev(0), ev(32), ev(64), ev(127)];
        let mut out = Vec::with_capacity(64);
        dispatch_subblock_midi(&events, 0, 128, &mut out);
        assert_eq!(out.len(), 4);
        for (i, e) in events.iter().enumerate() {
            assert_eq!(out[i].frame_offset, e.frame_offset);
            assert_eq!(out[i].data, e.data);
        }
    }

    #[test]
    fn dispatch_multi_subblock_routes_events_to_correct_subblock() {
        // Bloc 256 samples = 2 sous-blocs de 128. Event à offset 130 doit
        // aller dans le 2e sous-bloc, avec offset relatif 130-128 = 2.
        let events = vec![ev(10), ev(127), ev(128), ev(130), ev(255)];
        let mut out = Vec::with_capacity(64);

        dispatch_subblock_midi(&events, 0, 128, &mut out);
        assert_eq!(out.len(), 2, "1er sous-bloc : offsets 10 et 127");
        assert_eq!(out[0].frame_offset, 10);
        assert_eq!(out[1].frame_offset, 127);

        dispatch_subblock_midi(&events, 128, 256, &mut out);
        assert_eq!(out.len(), 3, "2e sous-bloc : offsets 128, 130, 255");
        assert_eq!(out[0].frame_offset, 0, "128 - 128 = 0");
        assert_eq!(out[1].frame_offset, 2, "130 - 128 = 2");
        assert_eq!(out[2].frame_offset, 127, "255 - 128 = 127");
    }

    #[test]
    fn dispatch_clears_output_before_filling() {
        // Le buffer out est cleared à chaque appel : pas de fuite entre
        // appels successifs (= contrat avec le caller du hot path).
        let mut out = vec![ev(99), ev(99)];
        dispatch_subblock_midi(&[], 0, 128, &mut out);
        assert!(out.is_empty(), "empty events → out vide après dispatch");
    }

    #[test]
    fn dispatch_no_match_yields_empty() {
        // Event à 100, sous-bloc [128, 256) → aucun match.
        let events = vec![ev(100)];
        let mut out = Vec::with_capacity(64);
        dispatch_subblock_midi(&events, 128, 256, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn dispatch_boundary_exclusive_on_sub_end() {
        // Event à offset = sub_end → exclu (sub_end est exclusif).
        let events = vec![ev(128)];
        let mut out = Vec::with_capacity(64);
        dispatch_subblock_midi(&events, 0, 128, &mut out);
        assert!(out.is_empty(), "frame_offset == sub_end est exclu");

        dispatch_subblock_midi(&events, 128, 256, &mut out);
        assert_eq!(out.len(), 1, "frame_offset == sub_start est inclus");
        assert_eq!(out[0].frame_offset, 0);
    }
}
