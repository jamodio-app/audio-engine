use cpal::traits::DeviceTrait;
use cpal::{Device, SampleFormat, SampleRate, StreamConfig, BufferSize};
use jamodio_audio_core::mixer::mixer::AudioMixer;
use parking_lot::Mutex;
use std::sync::Arc;

const TARGET_SR: u32 = 48000;
const TARGET_CHANNELS: u16 = 2;
const TARGET_BUFFER: u32 = 128;

/// Vérifie si le device OUTPUT expose une `BufferSize::Range` qui contient
/// `target_buf` pour le couple `(channels, sr)` demandé. Symétrique de
/// `capture::device_supports_fixed_buffer`. Permet de choisir
/// `Fixed(target_buf)` (low-latency) si supporté, sinon de tomber sur
/// `Default` au lieu d'échouer.
///
/// Comportement par OS / type de device : cf. doc de
/// `super::buffer_size::configs_support_fixed_buffer`.
fn device_supports_fixed_buffer(device: &Device, channels: u16, sr: u32, target_buf: u32) -> bool {
    let Ok(supported) = device.supported_output_configs() else {
        return false;
    };
    super::buffer_size::configs_support_fixed_buffer(supported, channels, sr, target_buf)
}

/// Error-callback CPAL (identique pour tous les formats de sortie).
fn on_playback_err(err: cpal::StreamError) {
    tracing::error!(target: "jamodio::playback", error = %err, "CPAL playback error");
}

/// Construit le stream de playback SANS le démarrer (`build_output_stream` mais
/// PAS `play()`). Pulls mixed audio from the shared AudioMixer.
/// Returns `(stream NON démarré, fixed_buffer)` — le stream doit rester vivant
/// (RAII) et être `play()`-é par le caller (sur le thread COM-STA pour ASIO).
/// `fixed_buffer` = `Some(N)` si on a appliqué `BufferSize::Fixed(N)`, `None`
/// si fallback `BufferSize::Default` (driver auto). Sert à la télémétrie
/// `outputBufferMs` côté wire (cf. `protocol::Stats`).
///
/// 0.5.3-4 (Volet B) — `play()` délégué au caller pour construire entrée+sortie
/// AVANT de démarrer l'une ou l'autre (évite le recreate de buffers ASIO en
/// cours de route = cold-start muet). Cf. `capture::build_capture_stream`.
pub fn build_playback_stream(
    device: &Device,
    mixer: Arc<Mutex<AudioMixer>>,
    // 0.5.3-4 — liveness : +1 par callback de sortie (cf. watchdog cold-start).
    output_callbacks: Arc<std::sync::atomic::AtomicU64>,
) -> Result<(cpal::Stream, Option<u32>), cpal::BuildStreamError> {
    // Diagnostic SR : on force CPAL en 48 kHz mais si le device préfère un
    // autre rate (Mac casque jack 44.1, BlackHole 2ch, etc.), CoreAudio fait
    // un resampling implicite de qualité variable → potentielles distortions.
    // Warn explicite pour aider le diag user (recommander un device 48k natif
    // dans Sound Settings : Scarlett, Focusrite, MOTU, BlackHole 16ch…).
    // Format natif du driver. CRITIQUE sur ASIO : cpal n'y fait AUCUNE
    // conversion (cf. capture.rs). Les sorties ASIO (Focusrite…) sont en Int32
    // → on ouvre au format natif et on convertit le mix f32 → type natif dans
    // le callback. CoreAudio/WASAPI : f32 natif → branche F32, inchangé.
    let sample_format = match device.default_output_config() {
        Ok(default_cfg) => {
            let native_sr = default_cfg.sample_rate().0;
            if native_sr != TARGET_SR {
                let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
                tracing::warn!(
                    target: "jamodio::playback",
                    device = %device_name,
                    native_sr,
                    target_sr = TARGET_SR,
                    "device sample rate ≠ 48 kHz — CoreAudio fera un resampling implicite (recommander à l'user un device 48k natif si glitches)"
                );
            }
            default_cfg.sample_format()
        }
        Err(_) => SampleFormat::F32,
    };

    // Buffer size : symétrique de capture.rs. On essaye Fixed(128) (= ~2.7 ms
    // low-latency) si le device output l'expose dans son SupportedBufferSize::Range
    // (CoreAudio mac + ASIO Windows + souvent WASAPI exclusive Win 11). Sinon
    // fallback BufferSize::Default — sans ce filet, `build_output_stream`
    // échouait avec StreamConfigNotSupported sur les sorties Windows shared
    // qui imposent leur propre buffer (jack onboard Realtek, HDMI typique →
    // Range { min: 480, max: 480 } ou Unknown). Symétrie complète avec la
    // logique input de capture.rs.
    let (buffer_size, fixed_buffer) = if device_supports_fixed_buffer(device, TARGET_CHANNELS, TARGET_SR, TARGET_BUFFER) {
        (BufferSize::Fixed(TARGET_BUFFER), Some(TARGET_BUFFER))
    } else {
        let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
        tracing::info!(
            target: "jamodio::playback",
            device = %device_name,
            "device n'expose pas Fixed(128) — fallback BufferSize::Default (WASAPI shared ~10ms)"
        );
        (BufferSize::Default, None)
    };

    let config = StreamConfig {
        channels: TARGET_CHANNELS,
        sample_rate: SampleRate(TARGET_SR),
        buffer_size,
    };

    // Le mixer produit du f32 ; pour les formats entiers (ASIO Int32/Int16) on
    // mixe dans un scratch f32 puis on convertit. Le scratch est capturé par le
    // callback (pas d'alloc par bloc après warmup).
    // 0.5.3-4 — liveness : +1 par callback de sortie effectivement appelé par le
    // driver. Si la sortie ne pull pas (cold-start ASIO muet), ce compteur reste
    // figé → le watchdog (ws_server) le détecte et relance. Un `fetch_add(Relaxed)`
    // par callback = négligeable sur le hot-path RT.
    use std::sync::atomic::Ordering;
    let stream = match sample_format {
        SampleFormat::F32 => {
            let output_callbacks = output_callbacks.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    output_callbacks.fetch_add(1, Ordering::Relaxed);
                    mixer.lock().mix_into(data);
                },
                on_playback_err,
                None,
            )?
        }
        SampleFormat::I32 => {
            let output_callbacks = output_callbacks.clone();
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                &config,
                move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                    output_callbacks.fetch_add(1, Ordering::Relaxed);
                    scratch.clear();
                    scratch.resize(data.len(), 0.0);
                    mixer.lock().mix_into(&mut scratch);
                    for (o, s) in data.iter_mut().zip(scratch.iter()) {
                        *o = (s.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32; // 2^31 - 1
                    }
                },
                on_playback_err,
                None,
            )?
        }
        SampleFormat::I16 => {
            let output_callbacks = output_callbacks.clone();
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    output_callbacks.fetch_add(1, Ordering::Relaxed);
                    scratch.clear();
                    scratch.resize(data.len(), 0.0);
                    mixer.lock().mix_into(&mut scratch);
                    for (o, s) in data.iter_mut().zip(scratch.iter()) {
                        *o = (s.clamp(-1.0, 1.0) * 32_767.0) as i16; // 2^15 - 1
                    }
                },
                on_playback_err,
                None,
            )?
        }
        other => {
            tracing::warn!(
                target: "jamodio::playback",
                ?other,
                "format de sortie non supporté (attendu f32/i32/i16)"
            );
            return Err(cpal::BuildStreamError::StreamConfigNotSupported);
        }
    };

    // Volet B : on NE démarre PAS ici (cf. doc de fonction). Le caller `play()`
    // la sortie puis l'entrée, sur le thread COM-STA.
    Ok((stream, fixed_buffer))
}
