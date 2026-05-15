use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig, BufferSize, SupportedBufferSize};
use crossbeam_channel::{Sender, TrySendError};

/// Vérifie si le device input expose une `BufferSize::Range` qui contient
/// `target_buf` pour le couple `(channels, sr)` demandé. Permet de choisir
/// `Fixed(target_buf)` (low-latency) si supporté, sinon de tomber sur
/// `Default` (= laisse le backend choisir, ~10ms WASAPI shared typique).
///
/// Sur Windows :
/// - ASIO expose typiquement `Range { min: 16, max: 4096 }` → Fixed(128) OK.
/// - WASAPI shared mode expose `Range { min: 480, max: 480 }` (10ms à 48k)
///   ou `BufferSize::Unknown` → Fixed(128) refusé.
/// - WASAPI exclusive expose un range plus large mais nécessite un device pas
///   utilisé par d'autres apps.
///
/// Sur macOS CoreAudio expose presque toujours un range qui contient 128.
fn device_supports_fixed_buffer(device: &Device, channels: u16, sr: u32, target_buf: u32) -> bool {
    let Ok(supported) = device.supported_input_configs() else {
        return false;
    };
    let target_sr = SampleRate(sr);
    for cfg in supported {
        if cfg.channels() != channels {
            continue;
        }
        if cfg.min_sample_rate() > target_sr || cfg.max_sample_rate() < target_sr {
            continue;
        }
        if let SupportedBufferSize::Range { min, max } = cfg.buffer_size() {
            if target_buf >= *min && target_buf <= *max {
                return true;
            }
        }
    }
    false
}

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

    // Buffer size : on essaye Fixed(128) (= ~2.7ms low-latency, accepté par
    // CoreAudio mac + ASIO Windows + parfois WASAPI exclusive Win 11) et on
    // fallback sur Default si le device n'expose pas ce range (= WASAPI
    // shared sur mic onboard typique, qui impose ~10ms min). Le fallback
    // évite l'erreur `StreamConfigNotSupported` qui bloquait v0.3.0 sur PC.
    let buffer_size = if device_supports_fixed_buffer(device, channels, native_sr, 128) {
        BufferSize::Fixed(128)
    } else {
        tracing::info!(
            target: "jamodio::capture",
            channels, native_sr,
            "device n'expose pas Fixed(128) — fallback BufferSize::Default (WASAPI shared ~10ms)"
        );
        BufferSize::Default
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
