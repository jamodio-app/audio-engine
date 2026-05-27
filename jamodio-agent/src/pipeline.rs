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
use jamodio_audio_core::protocol::AgentState;
use jamodio_audio_core::record::{RecordedFile, RecorderHandle, StemSpec};
use jamodio_audio_core::sync::drift::DriftEstimator;
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
use tokio::sync::mpsc as tokio_mpsc;

/// Wrapper to make cpal::Stream Send — we only hold it alive (RAII), never use across threads.
struct SendStream(#[allow(dead_code)] cpal::Stream);
// SAFETY: cpal::Stream on CoreAudio/ASIO is effectively thread-safe for keep-alive.
// We never call methods on it from another thread, only drop it.
unsafe impl Send for SendStream {}

/// Erreur typée renvoyée par `start_capture`. Permet à `ws_server` de
/// différencier un device introuvable (= `CaptureError` côté wire) d'une
/// erreur technique générique (= `Error` côté wire).
#[derive(Debug)]
pub enum CaptureStartError {
    /// Le device demandé (ou le default si aucun id) n'a pas été trouvé.
    /// Le `requested` est l'id transmis par le browser (None si aucun).
    InputDeviceNotFound { requested: Option<String> },
    OutputDeviceNotFound { requested: Option<String> },
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
}

/// Holds all active pipeline components. Shared between WS handler and audio threads.
pub struct PipelineState {
    pub mixer: Arc<Mutex<AudioMixer>>,
    /// CPAL streams must be kept alive — dropping them stops audio.
    capture_stream: Option<SendStream>,
    playback_stream: Option<SendStream>,
    /// Handle to stop the encoder thread.
    encoder_stop: Option<Sender<()>>,
    /// Handles to stop per-stream receive tasks.
    pub recv_stops: HashMap<String, tokio::sync::oneshot::Sender<()>>,
    /// Selected devices : ids stricts au format `"{idx}:{name}"` produits par
    /// `device::list_inputs/outputs`. Le browser stocke et renvoie EXACTEMENT
    /// l'id reçu — on n'accepte aucune autre forme (cf. `device::get_input_device`).
    input_device_id: Option<String>,
    output_device_id: Option<String>,
    /// State
    pub state: AgentState,
    /// Buffer size in samples (set when capture starts)
    pub buffer_samples: u32,
    /// Input RMS for VU meter
    pub input_rms: Arc<std::sync::atomic::AtomicU32>,
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
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    midi_event_rx: Option<Receiver<MidiEvent>>,
    /// S2.7 — Port virtuel "Jamodio Virtual MIDI" créé au boot agent et tenu
    /// vivant toute la durée d'exécution. Apparaît dans CoreMIDI = destination
    /// visible dans toutes les apps MIDI macOS (Logic, Ableton, GarageBand…).
    /// macOS only — Windows aura son équivalent en S2.5 via teVirtualMIDI.
    #[cfg(target_os = "macos")]
    virtual_midi_keepalive: Option<crate::audio::midi::MidiInput>,
    /// S2.7 — Receiver du port virtuel macOS, persistant et clonable.
    #[cfg(target_os = "macos")]
    virtual_midi_rx: Option<Receiver<MidiEvent>>,
    /// Sprint S1 — Métriques perf. Histogrammes capacités 512 (couvre 1.3 s à
    /// 375 obs/s, marge confortable pour le flush 1 Hz côté ws_server).
    ///
    /// `plugin_latency` est observé uniquement quand `process_stereo` tourne
    /// (plugin actif ET non-bypass) — vide en mode pass-through.
    /// `pipeline_latency` est observé à chaque tour de l'`encoder_thread`
    /// (= une mesure capture→send par bloc Opus).
    /// `capture_drops` est incrémenté depuis le callback CPAL (cf. capture.rs)
    /// quand le `sample_tx` est plein — signal direct de saturation encoder.
    /// `drift_ppm_by_producer` est mis à jour par les recv tasks après chaque
    /// `DriftEstimator::observe()`. Lecture côté ws_server au flush 1 Hz.
    pub perfstats: PerfHandles,
}

/// Sprint S1 — Handles perf partagés entre `PipelineState`, `encoder_thread`,
/// `recv_task`, et le CPAL capture callback. `Clone` cheap (Arc).
#[derive(Clone)]
pub struct PerfHandles {
    pub plugin_latency: Arc<Mutex<Histogram>>,
    pub pipeline_latency: Arc<Mutex<Histogram>>,
    pub capture_drops: Arc<std::sync::atomic::AtomicU64>,
    pub drift_ppm_by_producer: Arc<Mutex<HashMap<String, f64>>>,
}

impl PerfHandles {
    /// Histogrammes capacité 512 = ~1.36 s à cadence Opus 48k/120 (≈400 blocs/s),
    /// marge confortable pour le flush 1 Hz côté ws_server (10 % slack).
    fn new() -> Self {
        const HISTOGRAM_CAPACITY: usize = 512;
        Self {
            plugin_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            pipeline_latency: Arc::new(Mutex::new(Histogram::new(HISTOGRAM_CAPACITY))),
            capture_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            drift_ppm_by_producer: Arc::new(Mutex::new(HashMap::new())),
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

const CHANNELS: usize = 2;

impl PipelineState {
    pub fn new(mixer: Arc<Mutex<AudioMixer>>) -> Self {
        Self {
            mixer,
            capture_stream: None,
            playback_stream: None,
            encoder_stop: None,
            recv_stops: HashMap::new(),
            input_device_id: None,
            output_device_id: None,
            state: AgentState::Idle,
            buffer_samples: 0,
            input_rms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
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
            plugin_scan_cache: Arc::new(Mutex::new(PluginScanCache::Scanning)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            instrument_plugin_info: Arc::new(Mutex::new(None)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            input_source: Arc::new(Mutex::new(InputSource::Audio)),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            midi_input: None,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            midi_event_rx: None,
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
        let (tx, rx) = bounded::<MidiEvent>(512);
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

    /// S2 — change la source d'entrée. Appelé par le WS handler quand le
    /// browser bascule entre Audio et MIDI. En mode MIDI, ouvre un MidiInput
    /// via midir et stocke son receiver pour drainage dans encoder_thread.
    /// L'ouverture du device peut échouer si introuvable → erreur retournée
    /// au caller (qui fait un toast browser).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn set_input_source(&mut self, source: InputSource) -> Result<(), String> {
        match &source {
            InputSource::Audio => {
                // Ferme le MIDI input physique s'il y en avait un. Le port
                // virtuel macOS reste vivant (= virtual_midi_keepalive intact).
                self.midi_input = None;
                self.midi_event_rx = None;
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
                    self.midi_event_rx = Some(rx);
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
                    // Pas d'ouverture midir (= pas de réception physique
                    // depuis d'autres apps OS pour l'instant).
                    self.midi_input = None;
                    self.midi_event_rx = None;
                    *self.input_source.lock() = source;
                    return Ok(());
                }
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                let _ = is_virtual;

                let (tx, rx) = bounded::<MidiEvent>(256);
                let midi = crate::audio::midi::MidiInput::open(device_id, tx)?;
                self.midi_input = Some(midi);
                self.midi_event_rx = Some(rx);
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

    /// Charge un plugin sur l'instrument self. Décharge l'éventuel précédent.
    /// `max_frames` = 128 (cf. PLUGIN_BLOCK dans encoder_thread). Retourne
    /// (name, latency_samples, has_editor) pour ack côté browser.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn load_instrument_plugin(
        &self,
        plugin_ref: &PluginRef,
    ) -> Result<(String, u32, bool), String> {
        // Décharger d'abord (single slot MVP).
        self.unload_instrument_plugin();

        let mut host = self.plugin_host.lock();
        let handle = host
            .load(plugin_ref, 128)
            .map_err(|e| format!("{e}"))?;
        let latency = host.latency_samples(handle);
        drop(host);

        // Retrouver name + has_editor depuis le cache pour l'ack côté browser.
        // Si le scan tourne encore (cas limite), on retourne des valeurs par
        // défaut — le browser a de toute façon déjà le name dans sa liste.
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

        *self.instrument_plugin_handle.lock() = Some(handle);
        self.instrument_plugin_bypass
            .store(false, std::sync::atomic::Ordering::Relaxed);
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

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn unload_instrument_plugin(&self) {
        let mut handle_guard = self.instrument_plugin_handle.lock();
        if let Some(handle) = handle_guard.take() {
            let _ = self.plugin_host.lock().unload(handle);
            // S1.5 — clear le snapshot AVEC le handle pour cohérence.
            *self.instrument_plugin_info.lock() = None;
            self.instrument_plugin_bypass
                .store(false, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(target: "jamodio::plugin", "instrument plugin unloaded");
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

    /// REC-3 : stop l'enregistrement et retourne les fichiers Ogg/Opus.
    /// Bloque jusqu'à finalize (timeout 30s côté handle).
    pub fn stop_recording(&mut self) -> Vec<RecordedFile> {
        // Détache le tx du mixer d'abord — les tap sites deviennent no-op
        // immédiatement, plus aucune nouvelle commande n'arrive au thread.
        self.mixer.lock().set_record_tx(None);
        let Some(handle) = self.recorder.take() else {
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
        use cpal::traits::DeviceTrait;
        // Output : si un id est sélectionné, on l'utilise strictement. Sinon
        // (browser n'a jamais sélectionné — flow normal vu qu'on délègue à
        // l'OS) on prend le default système. Pas de hybrid id-then-default :
        // un id explicite échoué = erreur claire, pas de silent fallback.
        let out_device_opt = match self.output_device_id.as_deref() {
            Some(id) => crate::audio::device::get_output_device(id),
            None => crate::audio::device::default_output_device().map(|(d, _)| d),
        };
        let Some(out_device) = out_device_opt else {
            tracing::warn!(
                target: "jamodio::pipeline",
                requested = ?self.output_device_id,
                "output device introuvable — playback désactivé jusqu'à nouvelle sélection"
            );
            self.playback_stream.take();
            return;
        };
        let resolved_name = out_device.name().unwrap_or_default();

        // mem::replace : crée le nouveau stream AVANT de drop l'ancien
        // → minimise le gap audio (sinon brève silence → jitter buffer
        // overflow → crackles au resume).
        match crate::audio::playback::start_playback(&out_device, self.mixer.clone()) {
            Ok(stream) => {
                // .replace() : crée le nouveau stream AVANT de drop l'ancien
                // → minimise le gap audio (sinon brève silence → jitter buffer
                // overflow → crackles au resume). L'Option<> retournée est
                // droppée en fin de scope = CPAL stoppe l'ancien stream.
                let _old = self.playback_stream.replace(SendStream(stream));
                tracing::info!(target: "jamodio::pipeline", device = %resolved_name, "output device switched");
            }
            Err(e) => tracing::error!(
                target: "jamodio::pipeline",
                error = %e,
                "restart_playback échoué — on garde l'ancien"
            ),
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
        use cpal::traits::DeviceTrait;

        // 1. RÉSOUDRE LE DEVICE D'ABORD — avant de toucher quoi que ce soit
        // d'autre. Si le device demandé n'existe pas, on échoue tout de suite,
        // proprement, sans avoir stoppé une capture en cours ni alloué un
        // socket UDP. Comportement strict : l'id du browser DOIT pointer sur
        // un device courant. Aucun fallback default.
        let input_id = self.input_device_id.clone();
        let input_device = match input_id.as_deref() {
            Some(id) => crate::audio::device::get_input_device(id),
            None => {
                // Premier lancement, browser n'a rien sélectionné : on prend
                // le default système (uniquement dans ce cas-là).
                crate::audio::device::default_input_id()
                    .as_deref()
                    .and_then(crate::audio::device::get_input_device)
            }
        };
        let Some(device) = input_device else {
            return Err(CaptureStartError::InputDeviceNotFound { requested: input_id });
        };
        let in_name = device.name().unwrap_or_default();
        // L'id qu'on rapporte est celui demandé (si présent) ou celui du
        // default résolu — toujours au format `{idx}:{name}` pour cohérence.
        let resolved_input_id = input_id.unwrap_or_else(|| {
            crate::audio::device::default_input_id().unwrap_or_else(|| in_name.clone())
        });

        // Stop any existing capture (only after device resolved successfully)
        self.stop_capture();

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
        let (rtp_tx, mut rtp_rx) = tokio_mpsc::channel::<Vec<u8>>(64);
        let input_rms = self.input_rms.clone();
        let (stop_tx, stop_rx) = bounded::<()>(1);
        self.encoder_stop = Some(stop_tx);

        // 5. Start CPAL input stream (le device est déjà résolu, ici on
        //    ouvre seulement le stream — toute erreur ici est technique
        //    pure (driver, sample-rate impossible, etc.), pas une erreur
        //    de sélection user).
        tracing::info!(target: "jamodio::pipeline", device = %in_name, "input device opened");
        // Sprint S1 — partage du compteur de drops avec le callback CPAL : il
        // incrémente quand `sample_tx` est plein, ws_server le lit + reset au
        // flush 1 Hz pour publier `dropsPerSec` dans PerfStats.
        let capture_drops_for_callback = self.perfstats.capture_drops.clone();
        let (stream, channels_in, native_sr) = crate::audio::capture::start_capture(
            &device,
            sample_tx,
            capture_drops_for_callback,
        )
            .map_err(|e| CaptureStartError::Other(format!("CPAL input: {}", e)))?;
        self.capture_stream = Some(SendStream(stream));
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
        // S2 — receiver MIDI cloné si on est en mode MIDI au moment du
        // start_capture. Si l'user switch en cours de session, restart
        // capture = sprint robustesse (= prochain).
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let midi_event_rx_for_encoder = self.midi_event_rx.clone();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let input_source_for_encoder = self.input_source.clone();
        let perfstats_for_encoder = self.perfstats.clone();
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                encoder_thread(
                    sample_rx, rtp_tx, stop_rx, ssrc, payload_type, input_rms,
                    channels_in, native_sr, effective_channel, mixer_for_encoder, input_cut_for_encoder,
                    perfstats_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_host_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_handle_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_bypass_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] midi_event_rx_for_encoder,
                    #[cfg(any(target_os = "macos", target_os = "windows"))] input_source_for_encoder,
                );
            })
            .map_err(|e| CaptureStartError::Other(format!("Spawn encoder: {}", e)))?;

        // 7. Spawn tokio task for UDP sending (chiffrement SRTP en place avant send_to)
        let sender = Arc::new(sender);
        tokio::spawn({
            let sender = sender.clone();
            async move {
                while let Some(packet) = rtp_rx.recv().await {
                    let _ = sender.send(packet).await;
                }
            }
        });

        // 8. Start CPAL output stream (playback) if not already running.
        //    Output : id explicite si défini, sinon default système (pas de
        //    fallback hybrid : un id explicite échoué est une erreur claire).
        if self.playback_stream.is_none() {
            let out_id = self.output_device_id.clone();
            let out_device_opt = match out_id.as_deref() {
                Some(id) => crate::audio::device::get_output_device(id),
                None => crate::audio::device::default_output_device().map(|(d, _)| d),
            };
            let Some(out_device) = out_device_opt else {
                return Err(CaptureStartError::OutputDeviceNotFound { requested: out_id });
            };
            let out_name = out_device.name().unwrap_or_default();
            tracing::info!(target: "jamodio::pipeline", device = %out_name, "output device opened");
            let out_stream = crate::audio::playback::start_playback(&out_device, self.mixer.clone())
                .map_err(|e| CaptureStartError::Other(format!("CPAL output: {}", e)))?;
            self.playback_stream = Some(SendStream(out_stream));
        }

        self.state = AgentState::Capturing;
        self.buffer_samples = 128; // matches capture.rs BufferSize::Fixed(128)
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
        // dans recv_decode_task jusqu'au 1er paquet reçu (=> comedia activé côté SFU).

        // Add stream to mixer
        self.mixer.lock().add_stream(&producer_id);

        // Stop signal
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        self.recv_stops.insert(producer_id.clone(), stop_tx);

        // Spawn receive + decode task (reçoit aussi sfu_addr pour le punch périodique)
        let mixer = self.mixer.clone();
        let drift_ppm_handle = self.perfstats.drift_ppm_by_producer.clone();
        tokio::spawn(async move {
            recv_decode_task(receiver, sfu_addr, producer_id, mixer, drift_ppm_handle, stop_rx).await;
        });

        // Start playback if not running. Output : id explicite si défini,
        // sinon default système. Pas de fallback silencieux sur le default
        // si un id explicite échoue.
        if self.playback_stream.is_none() {
            let out_device_opt = match self.output_device_id.as_deref() {
                Some(id) => crate::audio::device::get_output_device(id),
                None => crate::audio::device::default_output_device().map(|(d, _)| d),
            };
            let out_device = out_device_opt.ok_or("output device introuvable")?;
            let out_stream = crate::audio::playback::start_playback(&out_device, self.mixer.clone())
                .map_err(|e| format!("CPAL output: {}", e))?;
            self.playback_stream = Some(SendStream(out_stream));
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
        if let Some(stop) = self.recv_stops.remove(producer_id) {
            let _ = stop.send(());
        }
        self.mixer.lock().remove_stream(producer_id);
    }

    fn stop_capture(&mut self) {
        self.capture_stream.take(); // Drop stops CPAL stream
        if let Some(stop) = self.encoder_stop.take() {
            let _ = stop.send(());
        }
        // Retire le stream self-monitor (créé dans start_capture).
        // Sans ça, le stream subsisterait dans le mixer avec son volume courant
        // et continuerait à mixer son ring buffer résiduel jusqu'à underrun.
        self.mixer.lock().remove_local_stream();
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
        self.playback_stream.take();
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
fn encoder_thread(
    sample_rx: Receiver<Vec<f32>>,
    rtp_tx: tokio_mpsc::Sender<Vec<u8>>,
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
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_host: Arc<Mutex<PluginHostImpl>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_handle: Arc<Mutex<Option<PluginHandle>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_bypass: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] midi_event_rx: Option<Receiver<MidiEvent>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] input_source: Arc<Mutex<InputSource>>,
) {
    // Best-effort RT priority — sur Linux sans CAP_SYS_NICE c'est refusé,
    // dans ce cas on continue en priorité normale plutôt que de planter.
    let prio = thread_priority::ThreadPriority::Crossplatform(
        95u8.try_into().expect("0..=100"),
    );
    if let Err(e) = thread_priority::set_current_thread_priority(prio) {
        tracing::warn!(target: "jamodio::encoder", error = ?e, "RT priority refusée — fallback prio normale");
    }

    let encoder = match MusicEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(target: "jamodio::encoder", error = %e, "failed to create Opus encoder");
            return;
        }
    };

    let frame_size = encoder.frame_size(); // 120 samples/channel
    let frame_len = frame_size * CHANNELS; // 240 f32s (stereo interleaved, 2.5ms @ 48kHz)
    let channels_in = channels_in as usize;
    let mut accumulator: Vec<f32> = Vec::with_capacity(frame_len * 2);

    // Resampler natif → 48 kHz (mic Windows onboard typique = 44.1 kHz, mac
    // CoreAudio est généralement 48 kHz natif → bypass total). Rubato Sinc
    // est sync, ~50-150 µs par bloc 128 samples sur M1. Latence introduite
    // ≈ sinc_len / native_sr = 256 / 44100 ≈ 5.8 ms (acceptable, dominé par
    // le buffer WASAPI shared 10ms de toute façon sur ce path).
    let mut resampler: Option<rubato::SincFixedIn<f32>> = if native_sr != 48000 {
        let ratio = 48000.0 / native_sr as f64;
        let params = rubato::SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: rubato::SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        // chunk_size 1024 = absorbe les buffers WASAPI shared (~480 samples
        // @44.1k) sans réinit ; padding interne géré par Rubato.
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
                    "rubato init failed — capture continuera SANS resampling (audio sera désynchronisé)"
                );
                None
            }
        }
    } else {
        None
    };
    // Buffers de sortie Rubato réutilisés entre les itérations pour éviter alloc
    // dans le hot path. Resize au besoin (output_frames_max).
    let mut resample_out_l: Vec<f32> = Vec::with_capacity(2048);
    let mut resample_out_r: Vec<f32> = Vec::with_capacity(2048);
    // Accumulateur PRE-resample : Rubato impose un chunk_size FIXE en input
    // (1024). Les buffers CPAL arrivent à des tailles variables (128 sur
    // CoreAudio, ~480 sur WASAPI shared). On accumule jusqu'à atteindre
    // 1024 par canal avant de resampler.
    let mut pre_resample_l: Vec<f32> = Vec::with_capacity(2048);
    let mut pre_resample_r: Vec<f32> = Vec::with_capacity(2048);
    const RESAMPLE_CHUNK: usize = 1024;
    let mut opus_buf = vec![0u8; 4000];
    let mut sequence: u16 = 0;
    let mut timestamp: u32 = 0;

    // SPRINT INSERT (S1.2) — buffers L/R préalloués pour le passage à
    // travers le plugin (AU sur mac, VST3 sur win). Désentrelacer/ré-entrelacer
    // par sous-blocs de PLUGIN_BLOCK samples par canal. Capacité fixée à 128
    // (la frame Opus fait 120 stéréo, et les buffers CPAL typiques < 128 par canal).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    const PLUGIN_BLOCK: usize = 128;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut plugin_left: Vec<f32> = Vec::with_capacity(PLUGIN_BLOCK);
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let mut plugin_right: Vec<f32> = Vec::with_capacity(PLUGIN_BLOCK);

    loop {
        // Check stop signal (non-blocking)
        if stop_rx.try_recv().is_ok() {
            break;
        }

        // Receive audio chunks from CPAL
        match sample_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(samples) => {
                // Sprint S1 — chrono début du tour : depuis la sortie de
                // `sample_rx` jusqu'à l'envoi RTP final. Mesure la latence
                // INTERNE du pipeline (hors buffer CPAL en amont qui dépend
                // du HAL OS). Coût `Instant::now()` ≈ 30 ns sur Apple Silicon
                // — négligeable comparé au budget RT 2.7 ms par bloc.
                let t_block_start = std::time::Instant::now();
                // RMS calculé sur le canal qui part réellement sur le réseau
                // (après remap) → le VU-mètre reflète le son transmis, pas la somme brute.
                let mut stereo = remap_to_stereo(&samples, channels_in, channel_index);

                // RESAMPLE (Windows mic onboard 44.1k → 48k Opus). Bypass total
                // si native_sr == 48000 (mac CoreAudio + cartes pro). Rubato
                // SincFixedIn impose un chunk_size FIXE en input → on accumule
                // les buffers CPAL (taille variable selon WASAPI/ASIO/CoreAudio)
                // jusqu'à RESAMPLE_CHUNK puis on process. Output = variable
                // (~RESAMPLE_CHUNK * 48000/native_sr).
                if let Some(rs) = resampler.as_mut() {
                    // Désentrelace stereo entrelacé → 2 canaux séparés.
                    for chunk in stereo.chunks_exact(2) {
                        pre_resample_l.push(chunk[0]);
                        pre_resample_r.push(chunk[1]);
                    }
                    stereo.clear();
                    let out_max = rs.output_frames_max();
                    if resample_out_l.len() < out_max { resample_out_l.resize(out_max, 0.0); }
                    if resample_out_r.len() < out_max { resample_out_r.resize(out_max, 0.0); }
                    while pre_resample_l.len() >= RESAMPLE_CHUNK {
                        // Slices d'entrée sans alloc.
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
                                // Re-entrelace dans `stereo` (= ce que le reste du
                                // pipeline attend, comme avant).
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
                        // Drain le chunk consommé. SincFixedIn consomme TOUJOURS
                        // chunk_size en input (contrat FixedIn).
                        pre_resample_l.drain(..RESAMPLE_CHUNK);
                        pre_resample_r.drain(..RESAMPLE_CHUNK);
                    }
                    // Si pas encore assez de samples accumulés, `stereo` reste
                    // vide ce tour-ci → l'accumulator Opus n'avance pas, on
                    // attend le prochain buffer CPAL. Comportement attendu.
                    if stereo.is_empty() {
                        continue;
                    }
                }

                // ENTRÉE OFF (= SetInputCut) : remplace les samples capturés
                // par du silence avant tout traitement (RMS, self-monitor,
                // record self stem, mix, envoi RTP). Cohérent avec le mode
                // browser où on faisait `track.enabled = false` côté WebRTC.
                if input_cut.load(std::sync::atomic::Ordering::Relaxed) {
                    stereo.fill(0.0);
                }

                // S2 — Si source = MIDI, on FORCE les samples audio à zéro.
                // Le micro/carte audio continue d'être ouverte (= elle nous
                // donne le tick d'horloge 48k/128) mais on ignore son contenu.
                // Le plugin instrument recevra silence + les events MIDI
                // accumulés, et produira l'audio depuis les notes jouées.
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    let src = input_source.lock().clone();
                    if matches!(src, InputSource::Midi(_)) {
                        stereo.fill(0.0);
                    }
                }

                // INSERT plugin (S1.2) — applique le plugin chargé (AU sur
                // mac, VST3 sur win) sur la tranche instrument self entre
                // remap et self-monitor/encode. Le self-monitor entend donc
                // le son WET (cohérent avec l'expérience DAW : jouer dans
                // un ampli simulé en s'écoutant traité).
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                if !stereo.is_empty() && !plugin_bypass.load(std::sync::atomic::Ordering::Relaxed) {
                    let handle_opt = *plugin_handle.lock();
                    if let Some(handle) = handle_opt {
                        let mut host = plugin_host.lock();

                        // S2 — Drain les events MIDI accumulés depuis le
                        // dernier bloc. Le receiver est non-blocking : on
                        // collecte ce qui est dispo MAX BATCH events
                        // (limite défensive pour ne pas spinner sur un
                        // device qui flood).
                        let midi_events: Vec<MidiEvent> = if let Some(rx) = &midi_event_rx {
                            let mut batch = Vec::new();
                            while let Ok(ev) = rx.try_recv() {
                                batch.push(ev);
                                if batch.len() >= 64 { break; }
                            }
                            batch
                        } else {
                            Vec::new()
                        };

                        let n_pairs = stereo.len() / 2;
                        let mut idx = 0;
                        // Important : on dispatche TOUS les MIDI events au 1er
                        // sous-bloc seulement (le plugin AU recevra des notes
                        // ON/OFF au début du bloc). Pour les sous-blocs
                        // suivants, MIDI vide (le plugin tient l'état).
                        let mut first_subblock = true;
                        while idx < n_pairs {
                            let end = (idx + PLUGIN_BLOCK).min(n_pairs);
                            plugin_left.clear();
                            plugin_right.clear();
                            for i in idx..end {
                                plugin_left.push(stereo[i * 2]);
                                plugin_right.push(stereo[i * 2 + 1]);
                            }
                            let midi_for_block: &[MidiEvent] = if first_subblock {
                                &midi_events
                            } else {
                                &[]
                            };
                            first_subblock = false;
                            // Sprint S1 — wall-clock guard plugin INSERT. Mesure
                            // par sous-bloc PLUGIN_BLOCK (= 128 frames stéréo).
                            // Le seuil de bypass auto (S5) sera calculé sur cette
                            // distribution. Coût `Instant::now()` ≈ 30 ns × 2.
                            let t_plugin = std::time::Instant::now();
                            let plugin_result = host.process_stereo(
                                handle,
                                &mut plugin_left,
                                &mut plugin_right,
                                midi_for_block,
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
                                    // Bug 1 diagnostic (S1.9) — log throttled
                                    // pour ne pas spam (2.7ms par bloc).
                                    static FAILS: std::sync::atomic::AtomicU64 =
                                        std::sync::atomic::AtomicU64::new(0);
                                    let n = FAILS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if n == 0 || n.is_power_of_two() {
                                        tracing::warn!(
                                            target: "jamodio::plugin",
                                            handle = ?handle,
                                            count = n + 1,
                                            error = %e,
                                            "process_stereo failed in encoder_thread (signal passe DRY)"
                                        );
                                    }
                                }
                            }
                            idx = end;
                        }
                    }
                }

                if !stereo.is_empty() {
                    let sum_sq: f32 = stereo.iter().map(|s| s * s).sum();
                    let rms = (sum_sq / stereo.len() as f32).sqrt();
                    input_rms.store(rms.to_bits(), std::sync::atomic::Ordering::Relaxed);

                    // SELF-MONITOR FORK : push les samples capturés (post-remap)
                    // dans le mixer local → ils sortent sur le casque via le
                    // callback CPAL playback. Gated par le volume du stream
                    // « self » côté mixer (0.0 par défaut = silence). Lock
                    // parking_lot contended ≤ µs, négligeable pour un RT
                    // thread à frame 2.7 ms. Le push est no-op si l'id
                    // n'existe pas (cf. push_self_samples).
                    mixer.lock().push_self_samples(&stereo);
                }

                accumulator.extend_from_slice(&stereo);

                // Encode complete frames (240 f32 stéréo = 2.5ms)
                while accumulator.len() >= frame_len {
                    let frame: Vec<f32> = accumulator.drain(..frame_len).collect();

                    match encoder.encode(&frame, &mut opus_buf) {
                        Ok(encoded_len) => {
                            let header = RtpHeader {
                                payload_type,
                                sequence,
                                timestamp,
                                ssrc,
                                marker: sequence == 0,
                            };
                            let packet = rtp::build_packet(&header, &opus_buf[..encoded_len]);

                            // Non-blocking send to tokio. Distinguer Full vs
                            // Closed pour ne pas polluer les logs en shutdown :
                            // - Full   : task UDP saturée → vrai overload, warn.
                            // - Closed : task UDP terminée (stop_capture) →
                            //            attendu, debug only.
                            if let Err(e) = rtp_tx.try_send(packet) {
                                use tokio::sync::mpsc::error::TrySendError;
                                match e {
                                    TrySendError::Full(_) => {
                                        static FULLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                                        let n = FULLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        if n == 0 || n.is_power_of_two() {
                                            tracing::warn!(
                                                target: "jamodio::encoder",
                                                drop_count = n + 1,
                                                "RTP channel full — packet dropped (CPU/network overload?)"
                                            );
                                        }
                                    }
                                    TrySendError::Closed(_) => {
                                        static CLOSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                                        let n = CLOSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        if n == 0 {
                                            tracing::debug!(
                                                target: "jamodio::encoder",
                                                "RTP channel closed — UDP task gone (post stop_capture)"
                                            );
                                        }
                                    }
                                }
                            }

                            sequence = sequence.wrapping_add(1);
                            timestamp = timestamp.wrapping_add(frame_size as u32);
                        }
                        Err(e) => {
                            tracing::error!(target: "jamodio::encoder", error = %e, "Opus encode error");
                        }
                    }
                }

                // Sprint S1 — fin du tour : capture→send latency mesurée du
                // pop `sample_rx` jusqu'ici (juste après le dernier try_send
                // RTP). Inclut remap + resample + plugin + RMS + Opus +
                // RTP build + handoff au channel tokio (= côté pipeline pur,
                // hors UDP réseau qui est asynchrone via le tokio task).
                let block_elapsed_ms =
                    t_block_start.elapsed().as_secs_f32() * 1000.0;
                perfstats.pipeline_latency.lock().observe(block_elapsed_ms);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

}

// ─── Receive + decode task (tokio, one per remote stream) ──────────

async fn recv_decode_task(
    receiver: RtpReceiver,
    sfu_addr: SocketAddr,
    producer_id: String,
    mixer: Arc<Mutex<AudioMixer>>,
    drift_ppm_by_producer: Arc<Mutex<HashMap<String, f64>>>,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let mut decoder = match MusicDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(target: "jamodio::recv", producer = %producer_id, error = %e, "failed to create decoder");
            return;
        }
    };

    // T4.2a — DriftEstimator (mesure pure pour l'instant, log toutes les 30s)
    let drift_label = producer_id.chars().take(8).collect::<String>();
    let mut drift = DriftEstimator::new(drift_label);
    // Sprint S1 — pour ne pas écraser le hashmap à chaque paquet RTP (= 50/s
    // par stream), on snapshot le ppm périodiquement. Le DriftEstimator
    // recalcule à chaque observe() en interne, donc lire 1×/s côté ws_server
    // donne déjà une vue actuelle ; ici on push uniquement quand la valeur
    // a "bougé sensiblement" (> 1 ppm) pour minimiser la contention Mutex.
    let mut last_pushed_ppm: f64 = 0.0;

    // 4096 = MTU + marge auth tag SRTP (~16 octets) + en-tête RTP (12+).
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut last_seq: Option<u16> = None;
    let mut pkt_count: u64 = 0;
    let mut logged_large_jump = false;

    // Punch périodique pour comedia : 1er paquet SRTP valide reçu par le SFU
    // = src_addr enregistrée. On retry jusqu'au 1er paquet entrant côté agent.
    // 100 ms × 30 = 3 s : marge confortable pour que le browser pousse
    // connect-plain-transport au SFU avant qu'on stoppe.
    let mut punch_interval = tokio::time::interval(std::time::Duration::from_millis(100));
    punch_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut punch_remaining: u32 = 30;

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = punch_interval.tick(), if punch_remaining > 0 => {
                let _ = receiver.punch(sfu_addr).await;
                punch_remaining -= 1;
            }
            result = receiver.recv(&mut buf) => {
                match result {
                    Ok((len, _addr)) => {
                        // len == 0 : RTCP filtré ou échec SRTP unprotect (déjà loggé en amont)
                        if len == 0 { continue; }

                        // 1er paquet valide reçu : comedia activé, on stoppe les punches
                        if pkt_count == 0 { punch_remaining = 0; }

                        pkt_count += 1;
                        if pkt_count == 1 {
                            tracing::info!(
                                target: "jamodio::recv",
                                producer = &producer_id[..8.min(producer_id.len())],
                                bytes = len,
                                "first RTP packet received"
                            );
                        } else if pkt_count.is_multiple_of(5000) {
                            tracing::debug!(
                                target: "jamodio::recv",
                                producer = &producer_id[..8.min(producer_id.len())],
                                count = pkt_count,
                                "RTP packets received"
                            );
                        }

                        if let Some((_header, payload)) = rtp::parse_header(&buf[..len]) {
                            // T4.2a — alimente l'estimateur de dérive d'horloge
                            drift.observe(_header.timestamp, std::time::Instant::now());
                            // Sprint S1 — push le ppm courant dans le hashmap
                            // partagé si la valeur a bougé de > 1 ppm depuis
                            // la dernière push. Évite la contention Mutex à
                            // 50 Hz et garde le hashmap lisible pour ws_server
                            // (1 Hz). À warmup le ppm reste à 0.0 (cf. drift.rs).
                            let current_ppm = drift.drift_ppm();
                            if (current_ppm - last_pushed_ppm).abs() > 1.0 {
                                drift_ppm_by_producer
                                    .lock()
                                    .insert(producer_id.clone(), current_ppm);
                                last_pushed_ppm = current_ppm;
                            }
                            // Detect packet loss → PLC
                            if let Some(prev) = last_seq {
                                let expected = prev.wrapping_add(1);
                                if _header.sequence != expected {
                                    let gap = _header.sequence.wrapping_sub(expected);
                                    if gap <= 10 {
                                        for _ in 0..gap.min(3) {
                                            // PLC : copie obligatoire avant le push_samples car
                                            // decode_loss() rend une slice référencant un buffer
                                            // interne au decoder qui sera écrasé par le decode
                                            // suivant (cf. Sprint 3 BUG 7).
                                            let plc_owned: Option<Vec<f32>> = decoder.decode_loss().map(|s| s.to_vec());
                                            if let Some(plc) = plc_owned {
                                                // block_in_place : signale au scheduler tokio
                                                // qu'on prend un lock parking_lot bloquant
                                                // (le callback CPAL peut le tenir pendant
                                                // mix_into). Sans ça, le worker tokio peut
                                                // être bloqué → backpressure UDP recv.
                                                tokio::task::block_in_place(|| {
                                                    mixer.lock().push_samples(&producer_id, &plc);
                                                });
                                            }
                                        }
                                    } else if !logged_large_jump {
                                        tracing::warn!(
                                            target: "jamodio::recv",
                                            producer = &producer_id[..8.min(producer_id.len())],
                                            prev_seq = prev,
                                            got_seq = _header.sequence,
                                            gap,
                                            "large seq jump (skipping PLC)"
                                        );
                                        logged_large_jump = true;
                                    }
                                }
                            }
                            last_seq = Some(_header.sequence);

                            // Decode actual packet : on push directement la slice
                            // pendant qu'elle est valide (pas de re-emprunt de
                            // decoder avant la fin du push).
                            if let Some(pcm) = decoder.decode(payload) {
                                tokio::task::block_in_place(|| {
                                    mixer.lock().push_samples(&producer_id, pcm);
                                });
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "jamodio::recv",
                            producer = %producer_id,
                            error = %e,
                            "UDP recv error"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        }
    }

    // Sprint S1 — retire l'entrée drift_ppm de ce producer au shutdown du
    // task. Sans ça, un peer disparu laisse un ppm fantôme dans le hashmap
    // → le PerfStats publié continuerait à mentionner ce peer mort.
    drift_ppm_by_producer.lock().remove(&producer_id);
}
