use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig, BufferSize, SupportedBufferSize};
use crossbeam_channel::{Sender, TrySendError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Backoffs entre les essais de `build_input_stream` quand un driver
/// audio est lent à se libérer (typiquement après un restart de process —
/// auto-update agent, kill manuel, install nouvelle release). Observé sur
/// Scarlett Solo 4th Gen mais générique : c'est le cycle libération/réacquisition
/// CoreAudio (kAudioDevicePropertyNominalSampleRate ack tardif) qui timeoute.
///
/// 3 backoffs → 4 essais total → pire-cas ~1,7 s avant fallback WebRTC.
/// Mieux que le fallback silencieux actuel (qui demandait à l'user de
/// sortir + re-rentrer dans le studio).
const BUILD_STREAM_BACKOFFS: &[Duration] = &[
    Duration::from_millis(200),
    Duration::from_millis(500),
    Duration::from_millis(1000),
];

/// Retry générique avec backoff configurable. Le slice `backoffs` fixe à
/// la fois le NOMBRE de retries (`backoffs.len()`) et la durée entre chaque
/// retry. Total d'essais = `backoffs.len() + 1`.
///
/// Découplé du `sleep` (durée fournie en paramètre) → testable sans I/O
/// en passant `&[Duration::ZERO; N]`.
///
/// `is_retryable` permet de NE retry que les erreurs transitoires (timing,
/// I/O) et de fail-fast sur les erreurs structurelles (config invalide).
fn retry_with_backoff<F, T, E, R>(
    backoffs: &[Duration],
    mut op: F,
    is_retryable: R,
) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
    R: Fn(&E) -> bool,
{
    let mut idx = 0usize;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !is_retryable(&e) || idx >= backoffs.len() {
                    return Err(e);
                }
                std::thread::sleep(backoffs[idx]);
                idx += 1;
            }
        }
    }
}

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
    // Sprint S1 — incrémenté à chaque drop "sample channel full". Le compteur
    // statique précédent (`FULLS`) servait uniquement au throttle de logs ;
    // celui-ci est lu+reset par ws_server au flush 1 Hz pour publier le
    // dropsPerSec dans `PerfStats.pipelineLatencyMs.dropsPerSec`.
    capture_drops: Arc<AtomicU64>,
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

    // Callback CPAL : capturé par valeur (Move) via Arc clone — `build_input_stream`
    // peut être appelé plusieurs fois (retry), il faut donc qu'à chaque appel
    // on re-fabrique une closure indépendante. `sample_tx` et `capture_drops`
    // sont des Sender/Arc, clone-safe.
    let attempts = std::cell::Cell::new(0usize);
    let build_one = || {
        let attempt = attempts.get() + 1;
        attempts.set(attempt);
        let sample_tx = sample_tx.clone();
        let capture_drops = capture_drops.clone();
        let result = device.build_input_stream(
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
                        // Sprint S1 — métrique partagée (lue+reset 1 Hz par ws_server)
                        capture_drops.fetch_add(1, Ordering::Relaxed);
                        // Compteur statique inchangé : sert au throttle de logs
                        // (un warn par puissance de 2) — indépendant de la métrique.
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
            None, // No timeout côté callback CPAL (la stratégie retry concerne l'init, pas la run).
        );
        if let Err(ref e) = result {
            // Logué à chaque essai pour tracer la séquence de retry dans agent.log.
            // Le warn final (essai épuisé) reste émis par le caller via ws_server.
            tracing::warn!(
                target: "jamodio::capture",
                attempt, error = %e,
                "build_input_stream failed"
            );
        }
        result
    };

    let stream = retry_with_backoff(BUILD_STREAM_BACKOFFS, build_one, |err| {
        // Fail-fast sur config invalide (= retry inutile, le device ne supporte
        // pas cette combinaison channels/SR/buffer). Toutes les autres erreurs
        // (timeout sample-rate, DeviceNotAvailable transitoire, BackendSpecific)
        // sont supposées transitoires → on retry avec backoff.
        !matches!(err, cpal::BuildStreamError::StreamConfigNotSupported)
    })?;

    let final_attempts = attempts.get();
    if final_attempts > 1 {
        tracing::info!(
            target: "jamodio::capture",
            attempts = final_attempts,
            "build_input_stream succeeded after retry"
        );
    }

    stream.play().map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    Ok((stream, channels, native_sr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Helper : slice de Durations à zéro pour tester sans dormir.
    const NO_SLEEP: &[Duration] = &[Duration::ZERO, Duration::ZERO, Duration::ZERO];

    #[test]
    fn retry_succeeds_on_first_attempt() {
        let calls = Cell::new(0usize);
        let result: Result<i32, &str> = retry_with_backoff(
            NO_SLEEP,
            || {
                calls.set(calls.get() + 1);
                Ok(42)
            },
            |_| true,
        );
        assert_eq!(result, Ok(42));
        assert_eq!(calls.get(), 1, "no retry needed on direct success");
    }

    #[test]
    fn retry_succeeds_after_transient_failures() {
        let calls = Cell::new(0usize);
        let result: Result<&str, &str> = retry_with_backoff(
            NO_SLEEP,
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err("transient")
                } else {
                    Ok("ok")
                }
            },
            |_| true,
        );
        assert_eq!(result, Ok("ok"));
        assert_eq!(calls.get(), 3, "two failures then success on third attempt");
    }

    #[test]
    fn retry_returns_last_error_when_all_attempts_fail() {
        let calls = Cell::new(0usize);
        let result: Result<(), &str> = retry_with_backoff(
            NO_SLEEP,
            || {
                calls.set(calls.get() + 1);
                Err("always-fails")
            },
            |_| true,
        );
        assert_eq!(result, Err("always-fails"));
        assert_eq!(calls.get(), NO_SLEEP.len() + 1, "all attempts exhausted");
    }

    #[test]
    fn retry_fails_fast_on_non_retryable_error() {
        let calls = Cell::new(0usize);
        let result: Result<(), &str> = retry_with_backoff(
            NO_SLEEP,
            || {
                calls.set(calls.get() + 1);
                Err("structural")
            },
            |e| *e != "structural",
        );
        assert_eq!(result, Err("structural"));
        assert_eq!(calls.get(), 1, "no retry when is_retryable returns false");
    }
}
