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
use jamodio_audio_core::protocol::AgentState;
use jamodio_audio_core::record::{RecordedFile, RecorderHandle, StemSpec};
use jamodio_audio_core::sync::drift::DriftEstimator;
use parking_lot::Mutex;
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
        }
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
        let (stream, channels_in) = crate::audio::capture::start_capture(&device, sample_tx)
            .map_err(|e| CaptureStartError::Other(format!("CPAL input: {}", e)))?;
        self.capture_stream = Some(SendStream(stream));
        tracing::info!(target: "jamodio::pipeline", channels_in, ?channel_index, "input config");

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
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                encoder_thread(sample_rx, rtp_tx, stop_rx, ssrc, payload_type, input_rms, channels_in, effective_channel, mixer_for_encoder);
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
        tokio::spawn(async move {
            recv_decode_task(receiver, sfu_addr, producer_id, mixer, stop_rx).await;
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
    channel_index: Option<u8>,
    mixer: Arc<Mutex<AudioMixer>>,
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
    let mut opus_buf = vec![0u8; 4000];
    let mut sequence: u16 = 0;
    let mut timestamp: u32 = 0;

    loop {
        // Check stop signal (non-blocking)
        if stop_rx.try_recv().is_ok() {
            break;
        }

        // Receive audio chunks from CPAL
        match sample_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(samples) => {
                // RMS calculé sur le canal qui part réellement sur le réseau
                // (après remap) → le VU-mètre reflète le son transmis, pas la somme brute.
                let stereo = remap_to_stereo(&samples, channels_in, channel_index);
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

}
