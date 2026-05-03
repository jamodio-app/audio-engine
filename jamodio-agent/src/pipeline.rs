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
    /// Selected devices
    input_device_name: Option<String>,
    output_device_name: Option<String>,
    /// State
    pub state: AgentState,
    /// Buffer size in samples (set when capture starts)
    pub buffer_samples: u32,
    /// Input RMS for VU meter
    pub input_rms: Arc<std::sync::atomic::AtomicU32>,
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
            input_device_name: None,
            output_device_name: None,
            state: AgentState::Idle,
            buffer_samples: 0,
            input_rms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    /// Set ONLY the input device, preserving the current output_device_name.
    /// Évite le bug où start-capture appelait select_devices(input, None) et
    /// écrasait à None l'output précédemment configuré (= silent fallback
    /// sur le device par défaut système au lieu du device choisi par le user).
    pub fn set_input_device(&mut self, input: Option<String>) {
        self.input_device_name = input;
    }

    pub fn select_devices(&mut self, input: Option<String>, output: Option<String>) {
        // Sprint 3.1 — restart live du stream playback si l'output change.
        // Sans ça, modifier la sortie audio dans les settings n'a aucun effet
        // tant qu'on ne quitte/rejoint pas la session (le stream CPAL est
        // démarré une fois dans start_capture et jamais touché ensuite).
        let output_changed = self.output_device_name != output;
        self.input_device_name = input;
        self.output_device_name = output;
        if output_changed && self.playback_stream.is_some() {
            self.restart_playback();
        }
    }

    /// Sprint 3.1 — Recrée le CPAL output stream avec le device courant.
    /// Le mixer est conservé (Arc partagé), aucun audio en cours n'est perdu :
    /// le ring buffer continue d'accumuler côté décodeur pendant la transition.
    fn restart_playback(&mut self) {
        use cpal::traits::DeviceTrait;
        let Some(out_device) = crate::audio::device::get_output_device(self.output_device_name.as_deref()) else {
            eprintln!("[PIPELINE] Output device introuvable ({:?}), fallback default au prochain frame",
                self.output_device_name);
            self.playback_stream.take();
            return;
        };
        let resolved_name = out_device.name().unwrap_or_default();

        // mem::replace : crée le nouveau stream AVANT de drop l'ancien
        // → minimise le gap audio (sinon brève silence → jitter buffer
        // overflow → crackles au resume).
        match crate::audio::playback::start_playback(&out_device, self.mixer.clone()) {
            Ok(stream) => {
                let _old = std::mem::replace(&mut self.playback_stream, Some(SendStream(stream)));
                eprintln!("[Jamodio] ✓ Output device switched → '{}'", resolved_name);
                // _old drop ici (fin de scope) → CPAL stoppe l'ancien stream
            }
            Err(e) => eprintln!("[PIPELINE] ✗ restart_playback échoué ({}) — on garde l'ancien", e),
        }
    }

    /// Return the currently selected (or default) input device name.
    pub fn selected_input_name(&self) -> Option<String> {
        if let Some(ref name) = self.input_device_name {
            return Some(name.clone());
        }
        // Fallback to default device name
        crate::audio::device::default_input_name()
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
    ) -> Result<(u16, SrtpParameters), String> {
        // Stop any existing capture
        self.stop_capture();

        let sfu_addr: SocketAddr = format!("{}:{}", sfu_ip, sfu_port)
            .parse()
            .map_err(|e| format!("Bad SFU address: {}", e))?;

        // 1. Create SRTP context: nos clés (TX, à transmettre au SFU) + clés SFU (RX).
        let agent_srtp = SrtpParameters::generate_aead_aes_256_gcm();
        let srtp_ctx = Arc::new(SrtpContext::new(&agent_srtp, &sfu_srtp)?);

        // 2. Create UDP sender (chiffre via le contexte SRTP).
        let sender = RtpSender::new(sfu_addr, srtp_ctx)
            .await
            .map_err(|e| format!("UDP bind: {}", e))?;
        let local_port = sender.local_addr().map_err(|e| format!("{}", e))?.port();

        // Pas de punch ici : le 1er paquet audio chiffré (sous 10 ms) sert de punch
        // pour comedia. Un punch en clair serait rejeté par le SFU (enableSrtp:true).

        // 2. Channel: CPAL callback → accumulator thread
        let (sample_tx, sample_rx) = bounded::<Vec<f32>>(64);

        // 3. Channel: encoder thread → tokio UDP sender task
        let (rtp_tx, mut rtp_rx) = tokio_mpsc::channel::<Vec<u8>>(64);

        // 4. Input RMS tracking
        let input_rms = self.input_rms.clone();

        // 5. Encoder stop signal
        let (stop_tx, stop_rx) = bounded::<()>(1);
        self.encoder_stop = Some(stop_tx);

        // 6. Start CPAL input stream (avant le thread encodeur : on doit connaître
        //    le nombre réel de canaux physiques pour splitter correctement).
        let device = crate::audio::device::get_input_device(self.input_device_name.as_deref())
            .ok_or("No input device found")?;
        use cpal::traits::DeviceTrait;
        let in_name = device.name().unwrap_or_default();
        eprintln!("[Jamodio] Input device: '{}'", in_name);
        let (stream, channels_in) = crate::audio::capture::start_capture(&device, sample_tx)
            .map_err(|e| format!("CPAL input: {}", e))?;
        self.capture_stream = Some(SendStream(stream));
        eprintln!("[Jamodio] Input channels: {} — channel_index: {:?}", channels_in, channel_index);

        // Valider que le canal mono demandé existe bien sur le device
        let effective_channel = channel_index.and_then(|idx| {
            if (idx as u16) < channels_in { Some(idx) } else {
                eprintln!("[Jamodio] channel_index {} hors plage (device a {} canaux) — fallback stéréo", idx, channels_in);
                None
            }
        });

        // 7. Spawn encoder thread (std thread, not tokio — real-time audio)
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                encoder_thread(sample_rx, rtp_tx, stop_rx, ssrc, payload_type, input_rms, channels_in, effective_channel);
            })
            .map_err(|e| format!("Spawn encoder: {}", e))?;

        // 8. Spawn tokio task for UDP sending (chiffrement SRTP en place avant send_to)
        let sender = Arc::new(sender);
        tokio::spawn({
            let sender = sender.clone();
            async move {
                while let Some(packet) = rtp_rx.recv().await {
                    let _ = sender.send(packet).await;
                }
            }
        });

        // 9. Start CPAL output stream (playback) if not already running
        if self.playback_stream.is_none() {
            let out_device = crate::audio::device::get_output_device(self.output_device_name.as_deref())
                .ok_or("No output device found")?;
            use cpal::traits::DeviceTrait;
            let out_name = out_device.name().unwrap_or_default();
            eprintln!("[Jamodio] Output device: '{}'", out_name);
            let out_stream = crate::audio::playback::start_playback(&out_device, self.mixer.clone())
                .map_err(|e| format!("CPAL output: {}", e))?;
            self.playback_stream = Some(SendStream(out_stream));
        }

        self.state = AgentState::Capturing;
        self.buffer_samples = 128; // matches capture.rs BufferSize::Fixed(128)
        eprintln!("[Jamodio] Capture → {}:{} (UDP {}, SRTP)", sfu_ip, sfu_port, local_port);
        Ok((local_port, agent_srtp))
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

        // Start playback if not running
        if self.playback_stream.is_none() {
            let out_device = crate::audio::device::get_output_device(self.output_device_name.as_deref())
                .ok_or("No output device found")?;
            let out_stream = crate::audio::playback::start_playback(&out_device, self.mixer.clone())
                .map_err(|e| format!("CPAL output: {}", e))?;
            self.playback_stream = Some(SendStream(out_stream));
        }

        eprintln!("[Jamodio] Stream + {}:{} (UDP {}, SRTP)", sfu_ip, sfu_port, local_port);
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
    }

    pub fn stop_all(&mut self) {
        self.stop_capture();
        // Stop all receive tasks
        let ids: Vec<String> = self.recv_stops.keys().cloned().collect();
        for id in ids {
            self.remove_stream(&id);
        }
        self.playback_stream.take();
        self.state = AgentState::Idle;
        eprintln!("[Jamodio] Stopped");
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
                for f in 0..frames {
                    let s = src[f];
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

fn encoder_thread(
    sample_rx: Receiver<Vec<f32>>,
    rtp_tx: tokio_mpsc::Sender<Vec<u8>>,
    stop_rx: Receiver<()>,
    ssrc: u32,
    payload_type: u8,
    input_rms: Arc<std::sync::atomic::AtomicU32>,
    channels_in: u16,
    channel_index: Option<u8>,
) {
    // Best-effort RT priority — sur Linux sans CAP_SYS_NICE c'est refusé,
    // dans ce cas on continue en priorité normale plutôt que de planter.
    let prio = thread_priority::ThreadPriority::Crossplatform(
        95u8.try_into().expect("0..=100"),
    );
    if let Err(e) = thread_priority::set_current_thread_priority(prio) {
        eprintln!("[ENCODER] RT priority refusée ({:?}) — fallback prio normale", e);
    }

    let encoder = match MusicEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[ENCODER] Failed to create Opus encoder: {}", e);
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

                            // Non-blocking send to tokio
                            let _ = rtp_tx.try_send(packet);

                            sequence = sequence.wrapping_add(1);
                            timestamp = timestamp.wrapping_add(frame_size as u32);
                        }
                        Err(e) => {
                            eprintln!("[ENCODER] Opus error: {}", e);
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
            eprintln!("[RECV:{}] Failed to create decoder: {}", producer_id, e);
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
                            eprintln!("[Jamodio] Recv first RTP packet ({} bytes) for {}", len, &producer_id[..8.min(producer_id.len())]);
                        } else if pkt_count % 5000 == 0 {
                            eprintln!("[Jamodio] Recv {} packets for {}", pkt_count, &producer_id[..8.min(producer_id.len())]);
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
                                            if let Some(plc) = decoder.decode_loss() {
                                                mixer.lock().push_samples(&producer_id, &plc);
                                            }
                                        }
                                    } else if !logged_large_jump {
                                        eprintln!("[RECV] large seq jump: prev={} got={} gap={} (skipping PLC)", prev, _header.sequence, gap);
                                        logged_large_jump = true;
                                    }
                                }
                            }
                            last_seq = Some(_header.sequence);

                            // Decode actual packet
                            if let Some(pcm) = decoder.decode(payload) {
                                mixer.lock().push_samples(&producer_id, &pcm);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[RECV:{}] UDP error: {}", producer_id, e);
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        }
    }

}
