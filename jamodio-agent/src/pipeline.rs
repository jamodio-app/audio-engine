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
    /// Sprint S5 — état "bypass auto suite à overload détecté". Flag activé
    /// quand `perfstats.plugin_latency.p99 > 4 ms` (cf. ws_server perfstats_task).
    /// Set en même temps que `instrument_plugin_bypass = true` + émission
    /// d'un message `InstrumentPluginOverload` au browser.
    /// Reset à false sur :
    ///   - `SetInstrumentPluginBypass { bypass: false }` (= user clique Réactiver)
    ///   - `LoadInstrumentPlugin` (= nouveau plugin → fresh start)
    ///   - `UnloadInstrumentPlugin`
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
    pub capture_drops: Arc<std::sync::atomic::AtomicU64>,
    pub drift_ppm_by_producer: Arc<Mutex<HashMap<String, f64>>>,
    /// Chantier C (v0.4.14) — pic ABSOLU de la sortie post-plugin (pré-soft-clip)
    /// sur la fenêtre courante, en bits f32 (≥ 0 → ordre des bits monotone, OK
    /// pour `fetch_max`). Lu+reset par perfstats_task 1 Hz → indicateur CLIP UI.
    /// Permet de signaler à l'utilisateur quand son plugin sort > 0 dBFS.
    pub output_peak: Arc<std::sync::atomic::AtomicU32>,
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
            capture_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            drift_ppm_by_producer: Arc::new(Mutex::new(HashMap::new())),
            output_peak: Arc::new(std::sync::atomic::AtomicU32::new(0)),
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
                    sample_rx, rtp_tx, stop_rx, ssrc, payload_type, input_rms,
                    channels_in, native_sr, effective_channel, mixer_for_encoder, input_cut_for_encoder,
                    perfstats_for_encoder, output_device_name_for_encoder,
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

/// Sprint S3 — Orchestrateur des 3 stages audio (capture, process, encode).
///
/// Architecture (cf. PLAN-EXECUTION-AGENT-STABILITE.md §S3) :
///
/// ```text
/// CPAL callback ─sample_rx─►  capture_stage  ─►ringbuf 32─►  process_stage  ─►ringbuf 32─►  encode_stage  ─►rtp_tx─► UDP task
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
    output_device_name: Option<String>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_host: Arc<Mutex<PluginHostImpl>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_handle: Arc<Mutex<Option<PluginHandle>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] plugin_bypass: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] midi_event_rx: Option<Receiver<MidiEvent>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] input_source: Arc<Mutex<InputSource>>,
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
                rtp_tx,
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

/// Chantier C (v0.4.14) — soft-clip de sécurité ZÉRO-latence, plugin-agnostic.
///
/// En dessous de `threshold` : identité (signal bit-identique, coût ≈ 1 abs +
/// 1 compare par sample). Au-dessus : genou `tanh` qui plafonne en douceur vers
/// ±1.0 — continu et de pente continue à `threshold` (tanh'(0)=1) → pas de
/// rupture. Asymptote à ±1.0 : la sortie ne dépasse JAMAIS 0 dBFS. Aucun
/// lookahead → aucune latence ajoutée. Protège le DAC (monitoring), le réseau
/// et l'enregistrement quand un plugin sort trop chaud (cf. AmpliTube +6 dB).
/// Retourne le pic ABSOLU d'ENTRÉE (pré-clip) → alimente l'indicateur CLIP UI.
fn soft_clip_block(samples: &mut [f32], threshold: f32) -> f32 {
    let mut peak = 0.0f32;
    let range = 1.0 - threshold;
    for s in samples.iter_mut() {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        if a > threshold {
            let over = (a - threshold) / range;
            *s = (threshold + range * over.tanh()).copysign(*s);
        }
    }
    peak
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
    #[cfg(any(target_os = "macos", target_os = "windows"))] midi_event_rx: Option<Receiver<MidiEvent>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))] input_source: Arc<Mutex<InputSource>>,
) {
    let _rt_priority_handle = crate::audio::rt_priority::promote_thread_for_audio(
        output_device_name.as_deref(),
    );

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

                // S2 — source MIDI : force samples = 0 (mic ouvert pour le tick
                // d'horloge mais on ignore son contenu, le plugin instrument
                // génère l'audio depuis les events MIDI).
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    let src = input_source.lock().clone();
                    if matches!(src, InputSource::Midi(_)) {
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
                            if let Some(rx) = &midi_event_rx {
                                while rx.try_recv().is_ok() {}
                            }
                            // Signal sec → continue vers RMS/self-monitor/encode.
                            break 'plugin_block;
                        };

                        // Drain les events MIDI accumulés depuis le dernier
                        // bloc. Max 64 events / bloc (limite défensive).
                        let midi_events: Vec<MidiEvent> = if let Some(rx) = &midi_event_rx {
                            let mut batch = Vec::new();
                            while let Ok(ev) = rx.try_recv() {
                                batch.push(ev);
                                if batch.len() >= 64 {
                                    break;
                                }
                            }
                            batch
                        } else {
                            Vec::new()
                        };

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

                        let n_pairs = stereo.len() / 2;
                        let mut idx = 0;
                        // MIDI dispatché au 1er sous-bloc seulement (notes
                        // ON/OFF au début du bloc) ; sous-blocs suivants
                        // reçoivent une slice vide.
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
                            // Wall-clock guard plugin INSERT (mesure par
                            // sous-bloc). Coût `Instant::now()` ≈ 30 ns × 2.
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
                    } else if let Some(rx) = &midi_event_rx {
                        // Pas de plugin actif (ou swap en cours, handle=None) →
                        // purge la file MIDI pour éviter un burst d'events
                        // périmés (note-on orphelins) au prochain plugin.
                        while rx.try_recv().is_ok() {}
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
                    const SOFT_CLIP_THRESHOLD: f32 = 0.94; // ≈ -0,5 dBFS
                    let peak = soft_clip_block(&mut stereo, SOFT_CLIP_THRESHOLD);
                    perfstats
                        .output_peak
                        .fetch_max(peak.to_bits(), std::sync::atomic::Ordering::Relaxed);
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
    rtp_tx: tokio_mpsc::Sender<Vec<u8>>,
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
                            let packet =
                                rtp::build_packet(&header, &opus_buf[..encoded_len]);

                            if let Err(e) = rtp_tx.try_send(packet) {
                                use tokio::sync::mpsc::error::TrySendError;
                                match e {
                                    TrySendError::Full(_) => {
                                        static FULLS: std::sync::atomic::AtomicU64 =
                                            std::sync::atomic::AtomicU64::new(0);
                                        let n = FULLS.fetch_add(
                                            1,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        if n == 0 || n.is_power_of_two() {
                                            tracing::warn!(
                                                target: "jamodio::encoder",
                                                drop_count = n + 1,
                                                "RTP channel full — packet dropped (CPU/network overload?)"
                                            );
                                        }
                                    }
                                    TrySendError::Closed(_) => {
                                        static CLOSED: std::sync::atomic::AtomicU64 =
                                            std::sync::atomic::AtomicU64::new(0);
                                        let n = CLOSED.fetch_add(
                                            1,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
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
                            tracing::error!(
                                target: "jamodio::encoder",
                                error = %e,
                                "Opus encode error"
                            );
                        }
                    }
                }

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
        let peak = soft_clip_block(&mut x, 0.94);
        assert_eq!(x, orig, "signal sous seuil inchangé");
        assert!((peak - 0.94).abs() < 1e-6, "peak = max abs d'entrée");
    }

    #[test]
    fn soft_clip_never_exceeds_full_scale() {
        // Un plugin hot (jusqu'à ±3.0) ne doit JAMAIS sortir > ±1.0 après clip.
        let mut x: Vec<f32> = vec![3.0, -2.0, 1.5, -1.2, 1.0, 0.97];
        let peak = soft_clip_block(&mut x, 0.94);
        assert!(peak >= 3.0 - 1e-6, "peak reflète l'entrée brute (3.0)");
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
        soft_clip_block(&mut just_above, t);
        assert!(
            (just_above[0] - t).abs() < 1e-3,
            "transition douce au seuil, got {}",
            just_above[0]
        );
        // Monotonie + signe préservés.
        let mut neg = vec![-2.0f32];
        soft_clip_block(&mut neg, t);
        assert!(neg[0] < 0.0, "signe préservé");
    }
}
