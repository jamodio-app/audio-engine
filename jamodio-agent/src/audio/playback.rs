use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig, BufferSize};
use jamodio_audio_core::mixer::mixer::AudioMixer;
use parking_lot::Mutex;
use std::sync::Arc;

/// Start audio playback on the given device.
/// Pulls mixed audio from the shared AudioMixer.
/// Returns the CPAL stream (must be kept alive).
pub fn start_playback(
    device: &Device,
    mixer: Arc<Mutex<AudioMixer>>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let config = StreamConfig {
        channels: 2,
        sample_rate: SampleRate(48000),
        buffer_size: BufferSize::Fixed(128), // ~2.7ms at 48kHz
    };

    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            let mut mx = mixer.lock();
            mx.mix_into(data);
        },
        |err| {
            eprintln!("[PLAYBACK] Error: {}", err);
        },
        None,
    )?;

    stream.play().map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    Ok(stream)
}
