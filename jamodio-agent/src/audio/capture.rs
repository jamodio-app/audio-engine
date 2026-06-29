use cpal::traits::DeviceTrait;
use cpal::{Device, SampleFormat, SampleRate, StreamConfig, BufferSize};
use crossbeam_channel::{Sender, TrySendError};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

/// Vérifie si le device INPUT expose une `BufferSize::Range` qui contient
/// `target_buf` pour le couple `(channels, sr)` demandé. Permet de choisir
/// `Fixed(target_buf)` (low-latency) si supporté, sinon de tomber sur
/// `Default` (= laisse le backend choisir, ~10ms WASAPI shared typique).
///
/// Comportement par OS / type de device : cf. doc de
/// `super::buffer_size::configs_support_fixed_buffer` (logique partagée
/// avec le côté output dans `playback.rs`).
fn device_supports_fixed_buffer(device: &Device, channels: u16, sr: u32, target_buf: u32) -> bool {
    let Ok(supported) = device.supported_input_configs() else {
        return false;
    };
    super::buffer_size::configs_support_fixed_buffer(supported, channels, sr, target_buf)
}

/// Error-callback CPAL (identique pour tous les formats d'entrée).
fn on_capture_err(err: cpal::StreamError) {
    tracing::error!(target: "jamodio::capture", error = %err, "CPAL capture error");
}

/// 0.5.4-3 — logue la plage de buffer ASIO exposée par le driver (min/max) pour
/// le couple `(channels, sr)`. Diagnostic du gel Focusrite : permet de comparer
/// notre ancien `Fixed(128)` à la grille native du driver, et de connaître la
/// taille préférée effectivement retenue (croisé avec `log_first_callback`).
/// Appelé uniquement sur le chemin ASIO. No-op si le device ne renseigne pas de
/// `Range`.
fn log_asio_buffer_range(device: &Device, channels: u16, sr: u32) {
    use cpal::SupportedBufferSize;
    let Ok(configs) = device.supported_input_configs() else {
        return;
    };
    let target_sr = SampleRate(sr);
    for cfg in configs {
        if cfg.channels() == channels
            && cfg.min_sample_rate() <= target_sr
            && cfg.max_sample_rate() >= target_sr
        {
            if let SupportedBufferSize::Range { min, max } = cfg.buffer_size() {
                tracing::info!(
                    target: "jamodio::capture",
                    asio_buffer_min = *min,
                    asio_buffer_max = *max,
                    channels,
                    sr,
                    "plage de buffer ASIO du driver — on défère à sa taille préférée (plus de Fixed(128) forcé)"
                );
            }
            return;
        }
    }
}

/// Pousse un bloc de samples f32 entrelacés vers le thread encoder. Factorisé
/// pour être réutilisé par chaque callback typé (f32/i32/i16) — seule la
/// conversion vers f32 diffère, la comptabilité des drops est commune.
#[inline]
fn forward_samples(
    samples: Vec<f32>,
    sample_tx: &Sender<Vec<f32>>,
    capture_drops: &AtomicU64,
    capture_callbacks: &AtomicU64,
) {
    // 0.5.3-4 — liveness : +1 par callback CPAL d'entrée effectivement délivré
    // par le driver. Lu par le watchdog cold-start ASIO (cf. ws_server). Un seul
    // load+store Relaxed = négligeable sur le hot-path RT.
    capture_callbacks.fetch_add(1, Ordering::Relaxed);
    let n = samples.len();
    match sample_tx.try_send(samples) {
        Ok(_) => {}
        Err(TrySendError::Full(_)) => {
            // Sprint S1 — métrique partagée (lue+reset 1 Hz par ws_server).
            capture_drops.fetch_add(1, Ordering::Relaxed);
            static FULLS: AtomicU64 = AtomicU64::new(0);
            let c = FULLS.fetch_add(1, Ordering::Relaxed);
            if c == 0 || c.is_power_of_two() {
                tracing::warn!(
                    target: "jamodio::capture",
                    drop_count = c + 1,
                    samples_dropped = n,
                    "sample channel full — encoder thread saturé (CPU overload?)"
                );
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            static DISCONNECTS: AtomicU64 = AtomicU64::new(0);
            let c = DISCONNECTS.fetch_add(1, Ordering::Relaxed);
            if c == 0 {
                tracing::debug!(
                    target: "jamodio::capture",
                    "sample channel disconnected — CPAL still pushing post stop_capture (will stop soon)"
                );
            }
        }
    }
}

/// 0.5.3 — log one-shot de la taille RÉELLE du premier callback livré par le
/// driver (frames/canal). C'est le diagnostic direct de la rafale d'émission :
/// révèle si le backend ASIO a honoré notre `Fixed(128)` (≈128 → ~1 frame Opus,
/// flux régulier) ou s'il délivre la taille de son propre control panel
/// (ex. 512 → 4 frames émises d'affilée → grappe côté récepteur). Complète la
/// métrique `emit_burst` (frames/bloc à encode_stage) côté pipeline.
/// Coût hot-path : un load `Relaxed` par callback (quasi nul), un seul log/stream.
#[inline]
fn log_first_callback(
    logged: &std::sync::atomic::AtomicBool,
    input_frames: &AtomicU32,
    samples_interleaved: usize,
    channels: u16,
) {
    use std::sync::atomic::Ordering;
    // 0.5.4-4 — publie la taille RÉELLE du buffer (frames/canal) pour la
    // télémétrie de latence honnête. Stocké à chaque callback (store Relaxed ≈ 0
    // coût) → robuste à un éventuel changement de taille ; le log, lui, reste
    // one-shot via le flag `logged`.
    let frames = samples_interleaved / (channels.max(1) as usize);
    input_frames.store(frames as u32, Ordering::Relaxed);
    if logged.load(Ordering::Relaxed) || logged.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::info!(
        target: "jamodio::capture",
        frames_per_callback = frames,
        interleaved_len = samples_interleaved,
        channels,
        "taille du 1er callback capture livrée par le driver (granularité de la rafale d'émission)"
    );
}

/// Start capturing audio from the given device.
/// Returns `(stream, channels_captured, native_sample_rate, fixed_buffer)`.
/// `fixed_buffer` = `Some(N)` si on a appliqué `BufferSize::Fixed(N)`, `None`
/// si on a fallback sur `BufferSize::Default` (= le driver choisit, valeur
/// non connue côté agent sans instrumenter le callback). Sert à la
/// télémétrie `inputBufferMs` côté wire (cf. `protocol::Stats`). Le SR natif est
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
/// 0.5.3-4 (Volet B) — construit le stream d'entrée SANS le démarrer
/// (`build_input_stream` mais PAS `play()`). Le `play()` est délégué au caller
/// pour permettre de construire l'ENTRÉE **et** la SORTIE avant de démarrer
/// l'une ou l'autre : sur un device ASIO full-duplex, appeler
/// `build_output_stream` (ASIOCreateBuffers) APRÈS un `play()` d'entrée
/// (ASIOStart) recrée les buffers en cours de route → les 2 callbacks restent
/// muets (cold-start raté, bug PC 28/06). En créant tous les buffers d'abord,
/// puis en démarrant, on évite ce recreate. Cf. `pipeline::start_capture`.
///
/// Retourne `(stream NON démarré, channels, native_sr, fixed_buffer)`. Le
/// stream doit être `play()`-é par le caller (sur le thread COM-STA pour ASIO).
pub fn build_capture_stream(
    device: &Device,
    sample_tx: Sender<Vec<f32>>,
    // Sprint S1 — incrémenté à chaque drop "sample channel full". Le compteur
    // statique précédent (`FULLS`) servait uniquement au throttle de logs ;
    // celui-ci est lu+reset par ws_server au flush 1 Hz pour publier le
    // dropsPerSec dans `PerfStats.pipelineLatencyMs.dropsPerSec`.
    capture_drops: Arc<AtomicU64>,
    // 0.5.3-4 — liveness : +1 par callback effectif (cf. `forward_samples`).
    capture_callbacks: Arc<AtomicU64>,
    // 0.5.4-4 — taille réelle du callback (frames/canal), publiée au 1er callback.
    input_frames: Arc<AtomicU32>,
) -> Result<(cpal::Stream, u16, u32, Option<u32>), cpal::BuildStreamError> {
    // Interroger la config par défaut pour connaître le nombre réel de canaux
    // physiques + le sample rate natif (cf. doc fonction).
    let default_cfg = device
        .default_input_config()
        .map_err(|_| cpal::BuildStreamError::StreamConfigNotSupported)?;
    let channels = default_cfg.channels().max(1);
    let native_sr = default_cfg.sample_rate().0;
    // Format natif du driver. CRITIQUE sur ASIO : cpal n'y fait AUCUNE
    // conversion de format — il exige que le type demandé == type natif du
    // driver (cf. cpal `host/asio/stream.rs` : `if sample_format !=
    // expected_sample_format { return StreamConfigNotSupported }`). La plupart
    // des interfaces ASIO (Focusrite, Scarlett, MOTU…) sont en Int32 → ouvrir
    // un callback `f32` échouait avec « stream configuration not supported ».
    // On ouvre donc au format natif et on convertit en f32 nous-mêmes.
    // (CoreAudio/WASAPI : f32 natif → la branche F32 est prise, inchangé.)
    let sample_format = default_cfg.sample_format();

    // Buffer size — stratégie PAR HOST (cause racine du gel ASIO Focusrite,
    // sessions du 29/06) :
    //
    // - **ASIO (Windows)** : on DÉFÈRE à la taille PRÉFÉRÉE du driver
    //   (`BufferSize::Default` → asio-sys utilise `ASIOGetBufferSize().pref`),
    //   exactement comme un DAW et comme JUCE/RtAudio/Jamulus. Forcer `Fixed(128)`
    //   était fautif : asio-sys ne valide QUE `demandé ≤ max` (il IGNORE le min,
    //   la taille préférée ET la granularité du driver). Sur le Focusrite USB,
    //   128 hors-grille était accepté par `ASIOCreateBuffers` mais faisait HALTER
    //   les callbacks après ~75 s de streaming → studio injouable. La taille
    //   réellement retenue par le driver est révélée par `log_first_callback`.
    //   La basse latence (128 = 2,7 ms) n'était qu'un choix : ni Opus (encodeur à
    //   accumulateur → frames 120) ni les plugins (process_stage re-bloque en
    //   sous-blocs) ne dépendent de la taille de capture.
    // - **CoreAudio / WASAPI** : INCHANGÉ — Fixed(128) low-latency si exposé,
    //   sinon Default. (CoreAudio ignore de toute façon la valeur et prend la
    //   taille native du device ; WASAPI shared impose la sienne.)
    let on_asio = crate::audio::host::kind() == crate::audio::host::HostKind::Asio;
    let (buffer_size, fixed_buffer) = if on_asio {
        log_asio_buffer_range(device, channels, native_sr);
        (BufferSize::Default, None)
    } else if device_supports_fixed_buffer(device, channels, native_sr, 128) {
        (BufferSize::Fixed(128), Some(128u32))
    } else {
        tracing::info!(
            target: "jamodio::capture",
            channels, native_sr,
            "device n'expose pas Fixed(128) — fallback BufferSize::Default (WASAPI shared ~10ms)"
        );
        (BufferSize::Default, None)
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
    // 0.5.3 — partagé entre toutes les tentatives de build : on logue la taille
    // du tout premier callback effectif (diagnostic rafale, cf. log_first_callback).
    let first_block_logged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let build_one = || {
        let attempt = attempts.get() + 1;
        attempts.set(attempt);
        let sample_tx = sample_tx.clone();
        let capture_drops = capture_drops.clone();
        let capture_callbacks = capture_callbacks.clone();
        let input_frames = input_frames.clone();
        let first_logged = first_block_logged.clone();
        // `None` = pas de timeout côté callback CPAL (le retry concerne l'init).
        // On ouvre le stream au format NATIF du driver puis on convertit chaque
        // bloc en f32 normalisé [-1,1] (ce que la pipeline encoder attend).
        let result = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    log_first_callback(&first_logged, &input_frames, data.len(), channels);
                    forward_samples(data.to_vec(), &sample_tx, &capture_drops, &capture_callbacks);
                },
                on_capture_err,
                None,
            ),
            SampleFormat::I32 => device.build_input_stream(
                &config,
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    log_first_callback(&first_logged, &input_frames, data.len(), channels);
                    const SCALE: f32 = 1.0 / 2_147_483_648.0; // 1 / 2^31
                    let f: Vec<f32> = data.iter().map(|&s| s as f32 * SCALE).collect();
                    forward_samples(f, &sample_tx, &capture_drops, &capture_callbacks);
                },
                on_capture_err,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    log_first_callback(&first_logged, &input_frames, data.len(), channels);
                    const SCALE: f32 = 1.0 / 32_768.0; // 1 / 2^15
                    let f: Vec<f32> = data.iter().map(|&s| s as f32 * SCALE).collect();
                    forward_samples(f, &sample_tx, &capture_drops, &capture_callbacks);
                },
                on_capture_err,
                None,
            ),
            other => {
                tracing::warn!(
                    target: "jamodio::capture",
                    ?other,
                    "format d'entrée non supporté (attendu f32/i32/i16)"
                );
                return Err(cpal::BuildStreamError::StreamConfigNotSupported);
            }
        };
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

    // Volet B : on NE démarre PAS ici. Le caller (`pipeline::start_capture`)
    // construit aussi la sortie avant de `play()` les deux streams, sur le
    // thread COM-STA (ASIO). Cf. doc de fonction.
    Ok((stream, channels, native_sr, fixed_buffer))
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
