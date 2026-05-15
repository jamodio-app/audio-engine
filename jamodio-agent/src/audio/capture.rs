use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig, BufferSize};
use crossbeam_channel::{Sender, TrySendError};

/// Start capturing audio from the given device.
/// Returns `(stream, channels_captured, native_sample_rate)`. Le SR natif est
/// **respecté tel quel** (pas forcé à 48000) pour rester compatible avec les
/// devices Windows WASAPI shared mode (qui imposent le mix format Windows :
/// souvent 44100 sur les chipsets Realtek onboard) — l'`encoder_thread`
/// resample ensuite vers 48000 via `rubato` avant Opus encode.
///
/// Sur macOS CoreAudio fait un resampling implicite si on demande 48000 sur
/// un device 44100 → ça marchait silencieusement. Sur Windows WASAPI shared
/// le device REFUSE toute config qui diffère du mix format → erreur explicite
/// `StreamConfigNotSupported`. D'où la stratégie "ouvrir au natif puis
/// resampler côté Rust".
///
/// Le nombre de canaux retourné est la valeur hardware native (pas forcément 2)
/// pour permettre l'extraction d'un canal mono précis sur les interfaces
/// multi-canaux (Scarlett, Motu, etc.). Les samples envoyés sont en f32
/// entrelacés sur `channels_captured` canaux, au sample rate natif.
pub fn start_capture(
    device: &Device,
    sample_tx: Sender<Vec<f32>>,
) -> Result<(cpal::Stream, u16, u32), cpal::BuildStreamError> {
    // Interroger la config par défaut pour connaître le nombre réel de canaux
    // physiques + le sample rate natif (cf. doc fonction).
    let default_cfg = device
        .default_input_config()
        .map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    let channels = default_cfg.channels().max(1);
    let native_sr = default_cfg.sample_rate().0;

    // Buffer size : Fixed(128) sur mac (CoreAudio + ASIO Windows acceptent
    // les petits buffers = ~2.7ms latence). Default sur Windows WASAPI shared
    // mode qui impose 10-20ms minimum (refus silencieux sinon = même symptôme
    // que le SR forcé). Sur Windows ASIO le code passera quand même par cette
    // branche Default mais ASIO ignorera et utilisera son propre buffer
    // configurable côté ASIO control panel.
    let buffer_size = if cfg!(windows) {
        BufferSize::Default
    } else {
        BufferSize::Fixed(128)
    };

    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(native_sr),
        buffer_size,
    };

    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _info: &cpal::InputCallbackInfo| {
            // Send a copy of the audio samples to the encoder thread.
            // Deux cas d'erreur distincts à ne PAS confondre :
            // - Full       : encoder saturé (CPU/IO surchargé) → vrai signal
            //                d'overload qu'on veut voir → warn power-of-2.
            // - Disconnected : l'encoder thread a quitté (stop_capture) →
            //                attendu, mais le callback CPAL peut continuer à
            //                pousser pendant quelques centaines de ms (drop
            //                cpal::Stream est asynchrone côté CoreAudio) →
            //                debug only, pas de pollution dans les logs.
            match sample_tx.try_send(data.to_vec()) {
                Ok(_) => {}
                Err(TrySendError::Full(_)) => {
                    static FULLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let n = FULLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n == 0 || n.is_power_of_two() {
                        tracing::warn!(
                            target: "jamodio::capture",
                            drop_count = n + 1,
                            samples_dropped = data.len(),
                            "sample channel full — encoder thread saturé (CPU overload?)"
                        );
                    }
                }
                Err(TrySendError::Disconnected(_)) => {
                    static DISCONNECTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let n = DISCONNECTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n == 0 {
                        tracing::debug!(
                            target: "jamodio::capture",
                            "sample channel disconnected — CPAL still pushing post stop_capture (will stop soon)"
                        );
                    }
                }
            }
        },
        |err| {
            tracing::error!(target: "jamodio::capture", error = %err, "CPAL capture error");
        },
        None, // No timeout
    )?;

    stream.play().map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    Ok((stream, channels, native_sr))
}
