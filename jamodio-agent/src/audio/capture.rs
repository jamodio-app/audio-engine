use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig, BufferSize};
use crossbeam_channel::Sender;

/// Start capturing audio from the given device.
/// Returns (stream, channels_captured). The number of channels is the hardware's
/// native value (pas forcément 2) pour permettre l'extraction d'un canal mono
/// précis sur les interfaces multi-canaux (Scarlett, Motu, etc.).
/// Les samples envoyés sont en f32 entrelacés sur `channels_captured` canaux.
pub fn start_capture(
    device: &Device,
    sample_tx: Sender<Vec<f32>>,
) -> Result<(cpal::Stream, u16), cpal::BuildStreamError> {
    // Interroger la config par défaut pour connaître le nombre réel de canaux
    // physiques exposés. Sur une Scarlett Solo = 2, une 4i4 = 4, un built-in = 1.
    let default_cfg = device
        .default_input_config()
        .map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    let channels = default_cfg.channels().max(1);

    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(48000),
        buffer_size: BufferSize::Fixed(128), // ~2.7ms at 48kHz
    };

    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _info: &cpal::InputCallbackInfo| {
            // Send a copy of the audio samples to the encoder thread
            let _ = sample_tx.try_send(data.to_vec());
        },
        |err| {
            eprintln!("[CAPTURE] Error: {}", err);
        },
        None, // No timeout
    )?;

    stream.play().map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    Ok((stream, channels))
}
