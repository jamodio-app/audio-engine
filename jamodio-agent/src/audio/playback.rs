use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig, BufferSize};
use jamodio_audio_core::mixer::mixer::AudioMixer;
use parking_lot::Mutex;
use std::sync::Arc;

const TARGET_SR: u32 = 48000;

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

    let config = StreamConfig {
        channels: 2,
        sample_rate: SampleRate(TARGET_SR),
        buffer_size: BufferSize::Fixed(128), // ~2.7ms at 48kHz
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
