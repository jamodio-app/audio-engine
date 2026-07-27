use super::output_pair::clamp_output_pair;
use cpal::traits::DeviceTrait;
use cpal::{Device, SampleFormat, SampleRate, StreamConfig, BufferSize};
use jamodio_audio_core::mixer::mixer::AudioMixer;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TARGET_SR: u32 = 48000;
// La taille de buffer cible n'est plus une constante figée : elle est pilotée
// par `crate::audio::buffer_policy` (64 par défaut, 128 après backoff auto).

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

/// Écrit le mix STÉRÉO (`scratch`, `frames*2` entrelacé) dans le buffer de sortie
/// `data` (`frames*n_out` entrelacé) : canal L de la paire → canal `ps`, R →
/// `ps+1`, ZÉROS sur tous les autres canaux (device multicanal — Lot B/CoreAudio).
/// `conv` convertit un échantillon f32 → type natif du device (f32/i32/i16).
/// `n_out == 2` (device stéréo) → écriture directe des 2 canaux (cas historique).
fn scatter_pair<T: Copy>(data: &mut [T], scratch: &[f32], n_out: usize, ps: usize, conv: impl Fn(f32) -> T) {
    if n_out == 0 {
        return;
    }
    let frames = data.len() / n_out;
    for f in 0..frames {
        for c in 0..n_out {
            let v = if c == ps {
                scratch[f * 2]
            } else if c == ps + 1 {
                scratch[f * 2 + 1]
            } else {
                0.0
            };
            data[f * n_out + c] = conv(v);
        }
    }
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
    // 0.5.4-4 — taille réelle du callback de sortie (frames/canal), publiée à
    // chaque callback pour la télémétrie de latence honnête.
    output_frames: Arc<std::sync::atomic::AtomicU32>,
    // Lot B / extension CoreAudio — index de départ (0-based) de la PAIRE de
    // canaux de sortie. Partagé avec la pipeline (swap live, aucune réouverture).
    // Sur un device stéréo (n_out=2) il vaut 0 → écriture directe des 2 canaux.
    output_pair_start: Arc<AtomicUsize>,
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
    // On lit la config par défaut UNE fois : format natif + nombre de canaux de
    // sortie. `n_out` = nb réel de canaux du device (Lot B/CoreAudio) : le mix
    // stéréo est écrit dans la paire choisie, zéros ailleurs. Stéréo → n_out=2
    // (comportement historique). Min 1 (garde-fou ; un device 0-canal est rejeté
    // au build par cpal).
    let (sample_format, n_out) = match device.default_output_config() {
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
            (default_cfg.sample_format(), default_cfg.channels().max(1))
        }
        Err(_) => (SampleFormat::F32, 2),
    };

    // Buffer size : cible basse latence UNIFIÉE, pilotée par `buffer_policy` —
    // strictement symétrique de capture.rs (cf. sa doc). Entrée et sortie lisent
    // la MÊME cible → en duplex ASIO le `ASIOCreateBuffers` partagé reste cohérent.
    // `Fixed(cible)` si le device l'expose, sinon `Default` (WASAPI shared ~10ms).
    let target_buf = crate::audio::buffer_policy::target();
    let (buffer_size, fixed_buffer) =
        if device_supports_fixed_buffer(device, n_out, TARGET_SR, target_buf) {
            (BufferSize::Fixed(target_buf), Some(target_buf))
        } else {
            let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
            tracing::info!(
                target: "jamodio::playback",
                device = %device_name,
                target_buf,
                "device n'expose pas Fixed(cible) — fallback BufferSize::Default (WASAPI shared ~10ms)"
            );
            (BufferSize::Default, None)
        };

    let config = StreamConfig {
        channels: n_out,
        sample_rate: SampleRate(TARGET_SR),
        buffer_size,
    };
    let n_out = n_out as usize; // pour l'indexation dans le callback

    // Le mixer produit du f32 ; pour les formats entiers (ASIO Int32/Int16) on
    // mixe dans un scratch f32 puis on convertit. Le scratch est capturé par le
    // callback (pas d'alloc par bloc après warmup).
    // 0.5.3-4 — liveness : +1 par callback de sortie effectivement appelé par le
    // driver. Si la sortie ne pull pas (cold-start ASIO muet), ce compteur reste
    // figé → le watchdog (ws_server) le détecte et relance. Un `fetch_add(Relaxed)`
    // par callback = négligeable sur le hot-path RT.
    // Le mix est TOUJOURS produit dans un scratch STÉRÉO (frames*2), puis « scatteré »
    // dans la paire de canaux choisie du device (zéros ailleurs), quel que soit le
    // format natif. Sur un device stéréo (n_out=2, ps=0) → écriture directe des 2
    // canaux (identique à l'historique). Le scratch est capturé par le callback
    // (pas d'alloc par bloc après warm-up).
    let stream = match sample_format {
        SampleFormat::F32 => {
            let output_callbacks = output_callbacks.clone();
            let output_frames = output_frames.clone();
            let output_pair_start = output_pair_start.clone();
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    output_callbacks.fetch_add(1, Ordering::Relaxed);
                    let frames = data.len() / n_out;
                    output_frames.store(frames as u32, Ordering::Relaxed);
                    scratch.clear();
                    scratch.resize(frames * 2, 0.0);
                    mixer.lock().mix_into(&mut scratch);
                    let ps = clamp_output_pair(output_pair_start.load(Ordering::Relaxed), n_out);
                    scatter_pair(data, &scratch, n_out, ps, |v| v);
                },
                on_playback_err,
                None,
            )?
        }
        SampleFormat::I32 => {
            let output_callbacks = output_callbacks.clone();
            let output_frames = output_frames.clone();
            let output_pair_start = output_pair_start.clone();
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                &config,
                move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                    output_callbacks.fetch_add(1, Ordering::Relaxed);
                    let frames = data.len() / n_out;
                    output_frames.store(frames as u32, Ordering::Relaxed);
                    scratch.clear();
                    scratch.resize(frames * 2, 0.0);
                    mixer.lock().mix_into(&mut scratch);
                    let ps = clamp_output_pair(output_pair_start.load(Ordering::Relaxed), n_out);
                    scatter_pair(data, &scratch, n_out, ps, |v| (v.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32); // 2^31 - 1
                },
                on_playback_err,
                None,
            )?
        }
        SampleFormat::I16 => {
            let output_callbacks = output_callbacks.clone();
            let output_frames = output_frames.clone();
            let output_pair_start = output_pair_start.clone();
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    output_callbacks.fetch_add(1, Ordering::Relaxed);
                    let frames = data.len() / n_out;
                    output_frames.store(frames as u32, Ordering::Relaxed);
                    scratch.clear();
                    scratch.resize(frames * 2, 0.0);
                    mixer.lock().mix_into(&mut scratch);
                    let ps = clamp_output_pair(output_pair_start.load(Ordering::Relaxed), n_out);
                    scatter_pair(data, &scratch, n_out, ps, |v| (v.clamp(-1.0, 1.0) * 32_767.0) as i16); // 2^15 - 1
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
