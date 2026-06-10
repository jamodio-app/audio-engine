use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig, BufferSize};
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

/// Start audio playback on the given device.
/// Pulls mixed audio from the shared AudioMixer.
/// Returns the CPAL stream (must be kept alive).
pub fn start_playback(
    device: &Device,
    mixer: Arc<Mutex<AudioMixer>>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    // Diagnostic SR : on force CPAL en 48 kHz mais si le device préfère un
    // autre rate (Mac casque jack 44.1, BlackHole 2ch, etc.), CoreAudio fait
    // un resampling implicite de qualité variable → potentielles distortions.
    // Warn explicite pour aider le diag user (recommander un device 48k natif
    // dans Sound Settings : Scarlett, Focusrite, MOTU, BlackHole 16ch…).
    if let Ok(default_cfg) = device.default_output_config() {
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
    }

    // Buffer size : symétrique de capture.rs. On essaye Fixed(128) (= ~2.7 ms
    // low-latency) si le device output l'expose dans son SupportedBufferSize::Range
    // (CoreAudio mac + ASIO Windows + souvent WASAPI exclusive Win 11). Sinon
    // fallback BufferSize::Default — sans ce filet, `build_output_stream`
    // échouait avec StreamConfigNotSupported sur les sorties Windows shared
    // qui imposent leur propre buffer (jack onboard Realtek, HDMI typique →
    // Range { min: 480, max: 480 } ou Unknown). Symétrie complète avec la
    // logique input de capture.rs.
    let buffer_size = if device_supports_fixed_buffer(device, TARGET_CHANNELS, TARGET_SR, TARGET_BUFFER) {
        BufferSize::Fixed(TARGET_BUFFER)
    } else {
        let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
        tracing::info!(
            target: "jamodio::playback",
            device = %device_name,
            "device n'expose pas Fixed(128) — fallback BufferSize::Default (WASAPI shared ~10ms)"
        );
        BufferSize::Default
    };

    let config = StreamConfig {
        channels: TARGET_CHANNELS,
        sample_rate: SampleRate(TARGET_SR),
        buffer_size,
    };

    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            let mut mx = mixer.lock();
            mx.mix_into(data);
        },
        |err| {
            tracing::error!(target: "jamodio::playback", error = %err, "CPAL playback error");
        },
        None,
    )?;

    stream.play().map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    Ok(stream)
}
