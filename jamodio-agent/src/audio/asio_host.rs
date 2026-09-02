//! Host ASIO duplex **single-owner** (Windows) — remplace les 2 streams cpal sur le
//! chemin ASIO. Applique les invariants du contrat ASIO (single-owner), pour une
//! robustesse **toutes interfaces** (pas seulement Focusrite) :
//!
//! 1. **Une seule** instance `asio_sys::Asio` → **un seul** `ASIOInit` (l'agent via
//!    cpal en créait deux — une pour l'entrée, une pour la sortie — d'où un 2ᵉ
//!    `ASIOInit` sur le driver mono-client pendant que l'entrée tournait).
//! 2. **Taille de buffer snappée** à la grille légale du driver (min/max/granularité),
//!    jamais une taille forcée hors-grille (asio-sys ne valide que `<= max`).
//! 3. **Priming** (create dummy → start → ~120 ms → stop → dispose) : arme l'ADC des
//!    interfaces récalcitrantes qui ne délivrent rien au
//!    1ᵉʳ start à froid (le wedge « entrée figée au réveil de veille »).
//! 4. **Un seul** `ASIOCreateBuffers(in+out)` + **un seul** `ASIOStart` (pas de churn).
//! 5. **Start-timeout** : on attend le 1ᵉʳ callback ; s'il n'arrive pas, on le signale.
//! 6. Conversions **tous formats** natifs ASIO : Int16 / Int24 / Int32 / Float32 (LSB).
//!
//! macOS / WASAPI : ce module n'est jamais instancié (chemin cpal inchangé).
//!
//! Le seam est identique à `capture.rs` / `playback.rs` : l'entrée est poussée en
//! `f32` entrelacé vers `sample_tx` ; la sortie est tirée de `mixer.mix_into(&mut
//! [f32])` (stéréo). Le host **possède** le driver ; son `Drop` fait
//! `stop → dispose → ASIOExit`.

#![cfg(target_os = "windows")]

use crate::audio::asio_reset::ResetSignal;
use crate::audio::output_pair::clamp_output_pair;
use asio_sys as sys;
use crossbeam_channel::{Sender, TrySendError};
use jamodio_audio_core::mixer::mixer::AudioMixer;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// `ASIOGetBufferSize` n'est pas wrappé par asio-sys mais est compilé dans le même
// objet (asio.cpp). Symbole C++ mangled MSVC (`long __cdecl f(long*, …)`).
extern "C" {
    #[link_name = "?ASIOGetBufferSize@@YAJPEAJ000@Z"]
    fn ASIOGetBufferSize(min: *mut i32, max: *mut i32, pref: *mut i32, gran: *mut i32) -> i32;
}

/// Durée de la réchauffe (priming) : create → start → PRIME_MS → stop → dispose.
const PRIME_MS: u64 = 120;
/// Délai max d'attente du 1ᵉʳ callback après `ASIOStart`.
const START_TIMEOUT: Duration = Duration::from_secs(2);
/// Lot B — settle entre deux essais de `set_sample_rate` (un driver qui vient de
/// changer d'horloge — ASE_NoClock transitoire — a besoin de quelques dizaines de
/// ms pour verrouiller la PLL avant d'accepter le nouveau rate).
const RATE_SETTLE_MS: u64 = 60;

/// Format d'échantillon natif ASIO géré par le host (variantes little-endian, seules
/// rencontrées sur les interfaces Windows réelles).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fmt {
    I16,
    I24,
    I32,
    F32,
}

impl Fmt {
    fn from_asio(t: &sys::AsioSampleType) -> Option<Self> {
        use sys::AsioSampleType::*;
        Some(match t {
            ASIOSTInt16LSB => Fmt::I16,
            ASIOSTInt24LSB => Fmt::I24,
            ASIOSTInt32LSB => Fmt::I32,
            ASIOSTFloat32LSB => Fmt::F32,
            _ => return None,
        })
    }

    /// Taille en octets d'un échantillon (le *stride* dans le buffer dé-entrelacé).
    fn bytes(self) -> usize {
        match self {
            Fmt::I16 => 2,
            Fmt::I24 => 3,
            Fmt::I32 | Fmt::F32 => 4,
        }
    }

    /// Lit l'échantillon `frame` (base = buffer du canal) et le normalise en `f32`.
    ///
    /// # Safety
    /// `base` doit pointer sur au moins `(frame + 1) * self.bytes()` octets valides.
    #[inline]
    unsafe fn read(self, base: *const u8, frame: usize) -> f32 {
        let p = base.add(frame * self.bytes());
        match self {
            Fmt::I16 => (p as *const i16).read_unaligned() as f32 / 32_768.0,
            Fmt::I32 => (p as *const i32).read_unaligned() as f32 / 2_147_483_648.0,
            Fmt::F32 => (p as *const f32).read_unaligned(),
            Fmt::I24 => {
                let b0 = *p as i32;
                let b1 = *p.add(1) as i32;
                let b2 = *p.add(2) as i32;
                let mut v = b0 | (b1 << 8) | (b2 << 16);
                if v & 0x0080_0000 != 0 {
                    v |= !0x00FF_FFFF; // extension de signe 24 → 32 bits
                }
                v as f32 / 8_388_608.0 // 2^23
            }
        }
    }

    /// Écrit `v` (clampé [-1,1]) dans l'échantillon `frame` au format natif.
    ///
    /// # Safety
    /// `base` doit pointer sur au moins `(frame + 1) * self.bytes()` octets valides.
    #[inline]
    unsafe fn write(self, base: *mut u8, frame: usize, v: f32) {
        let p = base.add(frame * self.bytes());
        let c = v.clamp(-1.0, 1.0);
        match self {
            Fmt::I16 => (p as *mut i16).write_unaligned((c * 32_767.0) as i16),
            Fmt::I32 => (p as *mut i32).write_unaligned((c * 2_147_483_647.0) as i32),
            Fmt::F32 => (p as *mut f32).write_unaligned(c),
            Fmt::I24 => {
                let iv = (c * 8_388_607.0) as i32;
                *p = (iv & 0xFF) as u8;
                *p.add(1) = ((iv >> 8) & 0xFF) as u8;
                *p.add(2) = ((iv >> 16) & 0xFF) as u8;
            }
        }
    }
}

/// Lit `ASIOGetBufferSize(min,max,pref,granularity)` sur le driver global chargé.
fn asio_buffer_sizes() -> Option<(i32, i32, i32, i32)> {
    let (mut mn, mut mx, mut pf, mut gr) = (0i32, 0i32, 0i32, 0i32);
    let rc = unsafe { ASIOGetBufferSize(&mut mn, &mut mx, &mut pf, &mut gr) };
    (rc == 0).then_some((mn, mx, pf, gr))
}

/// Snappe une taille de buffer désirée à une taille **légale** du driver (min/max/
/// granularité). Ne renvoie JAMAIS une taille hors grille : hors `[min,max]` →
/// préférée ; granularité `-1` → puissance de 2 la plus proche ; `<= 0` → toute taille
/// (on garde le désir) ; sinon → multiple de `gran` le plus proche.
fn snap_buffer_size(desired: i32, min: i32, max: i32, pref: i32, gran: i32) -> i32 {
    if min <= 0 || max < min {
        return pref.max(1);
    }
    if desired < min || desired > max {
        return pref;
    }
    if gran == -1 {
        let mut s = 1i32;
        while s < min {
            s = s.saturating_mul(2);
        }
        let mut best = s;
        while s <= max {
            if (s - desired).abs() < (best - desired).abs() {
                best = s;
            }
            s = s.saturating_mul(2);
        }
        best
    } else if gran <= 0 {
        desired
    } else {
        let k = ((desired - min) as f64 / gran as f64).round() as i32;
        (min + k * gran).clamp(min, max)
    }
}

/// Priming : arme l'ADC des interfaces récalcitrantes. Crée des buffers, démarre
/// brièvement, attend, arrête, dispose. Aucun callback : on ne fait que faire tourner
/// l'horloge/DMA le temps de la réchauffe. Best-effort (les erreurs sont ignorées :
/// l'ouverture réelle qui suit reste tentée).
fn prime(driver: &sys::Driver, n_in: usize, n_out: usize, size: i32) {
    if let Ok(streams) = driver
        .prepare_input_stream(None, n_in, Some(size))
        .and_then(|s| driver.prepare_output_stream(s.input, n_out, Some(size)))
    {
        let _ = driver.start();
        std::thread::sleep(Duration::from_millis(PRIME_MS));
        let _ = driver.stop();
        let _ = driver.dispose_buffers();
        drop(streams);
    }
}

/// Host ASIO duplex single-owner. Possède le driver ; le `Drop` fait le cleanup ASIO.
pub struct AsioDuplexHost {
    driver: sys::Driver,
    streams: Arc<Mutex<Option<sys::AsioStreams>>>,
    cb_id: sys::CallbackId,
    msg_id: sys::MessageCallbackId,
    /// Canaux d'entrée réellement ouverts (le seam envoie ce nombre de canaux entrelacés).
    pub channels_in: u16,
    /// Sample rate natif du driver (après `set_sample_rate`).
    pub native_sr: u32,
    /// Taille de buffer réellement retenue (frames/canal).
    pub buffer_size: u32,
}

// Le handle est déplacé entre le thread appelant et le thread COM-STA (`com_exec`),
// comme les `cpal::Stream` de l'agent (wrappés en `SendStream`). Le driver n'est
// manipulé que sur le thread COM-STA (open/drop) ; les callbacks tournent sur le
// thread du driver et ne référencent que des `Arc` clonés (déjà `Send`).
unsafe impl Send for AsioDuplexHost {}

impl AsioDuplexHost {
    /// Ouvre le duplex ASIO sur `driver_name`, prime l'interface, démarre.
    /// À exécuter sur le thread COM-STA (`com_exec`). `desired_buffer` = taille cible
    /// (elle sera snappée à la grille légale du driver).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        driver_name: &str,
        desired_buffer: i32,
        sample_tx: Sender<Vec<f32>>,
        capture_drops: Arc<AtomicU64>,
        capture_callbacks: Arc<AtomicU64>,
        input_frames: Arc<AtomicU32>,
        capture_feeding: Arc<AtomicBool>,
        mixer: Arc<AudioMixer>,
        output_callbacks: Arc<AtomicU64>,
        output_frames: Arc<AtomicU32>,
        // Lot B — index de départ (0-based) de la PAIRE de canaux de sortie ASIO.
        // Partagé avec la pipeline (persiste à travers les ré-ouvertures keep-warm) ;
        // lu à chaque bloc dans le callback → changement de paire = swap LIVE sans
        // réouverture driver.
        output_pair_start: Arc<AtomicUsize>,
        reset_signal: &ResetSignal,
    ) -> Result<Self, String> {
        // 1) UNE instance Asio, UN load_driver (1 ASIOInit).
        let asio = sys::Asio::new();
        let driver = asio
            .load_driver(driver_name)
            .map_err(|e| format!("load_driver({driver_name}): {e:?}"))?;

        // 2) Sample rate — Lot B robustesse ASIO : `set_sample_rate(48k)` NON
        //    SILENCIEUX. On capture le résultat ET on vérifie le rate EFFECTIF.
        //    Séquence, alignée sur ce que fait un DAW :
        //      a) 1er set + vérif ; si non honoré → settle bref + RE-SET (couvre le
        //         ASE_NoClock transitoire, la PLL a besoin de verrouiller) ;
        //      b) si TOUJOURS ≠ 48 k → on renvoie le rate RÉEL (native_sr le reflète)
        //         + WARN explicite. `start_capture` (garde R2, décision 48k/ASIO-only)
        //         REFUSERA alors la capture — JAMAIS de resampling caché. Un pilote
        //         ASIO natif bascule le matériel en 48 ici même (transparent) ; seuls
        //         les wrappers (ASIO4ALL) / le WASAPI partagé restent bloqués hors 48.
        let target = 48_000.0_f64;
        let set_res = driver.set_sample_rate(target);
        let mut sr = driver.sample_rate().unwrap_or(0.0);
        if set_res.is_err() || (sr - target).abs() > 1.0 {
            tracing::warn!(
                target: "jamodio::audio",
                set_ok = set_res.is_ok(),
                got_sr = sr,
                "set_sample_rate(48k) non honoré au 1er essai — settle + nouvel essai"
            );
            std::thread::sleep(Duration::from_millis(RATE_SETTLE_MS));
            let retry_res = driver.set_sample_rate(target);
            sr = driver.sample_rate().unwrap_or(sr);
            if retry_res.is_err() || (sr - target).abs() > 1.0 {
                tracing::warn!(
                    target: "jamodio::audio",
                    retry_ok = retry_res.is_ok(),
                    got_sr = sr,
                    "driver REFUSE 48 kHz — la capture sera REFUSÉE (R2, jamais de resampling caché) ; l'UI demandera de régler l'interface en 48 kHz"
                );
            } else {
                tracing::info!(target: "jamodio::audio", "set_sample_rate(48k) honoré au 2e essai (après settle)");
            }
        }
        // native_sr = rate DÉCLARÉ par le driver. ⚠️ CERTAINS DRIVERS MENTENT :
        // le Focusrite natif renvoie « OK » à set_sample_rate(48k) et rapporte
        // sample_rate()=48000, mais continue de DÉLIVRER à 44,1. On corrige ce
        // rate plus bas en MESURANT la cadence réelle des callbacks (seul signal
        // fiable) après le start — cf. bloc « vérification du rate RÉEL ».
        let mut native_sr = if sr > 0.0 { sr as u32 } else { target as u32 };

        let channels = driver.channels().map_err(|e| format!("channels: {e:?}"))?;
        let n_in = channels.ins.max(0) as usize;
        // Lot B (0.5.11) — n'ouvrir QUE les canaux de sortie nécessaires à la paire
        // COURANTE (lue à l'ouverture), pas TOUS les canaux.
        //
        // Pourquoi (RACINE d'un crash 0.5.10) : ouvrir les 32 sorties d'un WING
        // (`ASIOCreateBuffers` sur 32 in + 32 out = 64 buffers, puis 32 écritures
        // FFI par callback) déclenchait une CORRUPTION DE TAS côté asio-sys/driver
        // sur les interfaces à beaucoup de canaux (`STATUS_HEAP_CORRUPTION` /
        // access-violation Windows, cf. crash-loop grotchybrax 28/07). On revient au
        // régime éprouvé 0.5.9 : `paire+2` sorties (ex. paire 1-2 → 2 ; 3-4 → 4).
        //
        // `clamp_output_pair` garantit un `start` PAIR borné à `[0, total-2]`, donc
        // `+2 ∈ [2, total]`. Le callback écrit le mix sur `ps`/`ps+1` (toujours dans
        // la fenêtre ouverte) et des zéros ailleurs.
        //
        // LIMITE CONNUE (à corriger en session Windows, cf. PLAN-ASIO-OUTPUT-PAIR) :
        // si l'utilisateur choisit EN SESSION une paire PLUS HAUTE que la fenêtre
        // ouverte, le callback la clampe à la dernière paire ouverte (mauvais
        // routage, JAMAIS de crash) jusqu'à la prochaine réouverture. Le fix propre
        // = réouverture sur croissance de paire (reset borné) — non fait ici car
        // non testable hors machine Windows.
        let total_out = channels.outs.max(0) as usize;
        let n_out = if total_out < 2 {
            total_out
        } else {
            (clamp_output_pair(output_pair_start.load(Ordering::Relaxed), total_out) + 2)
                .min(total_out)
        };
        if n_in == 0 || n_out == 0 {
            return Err(format!("driver sans entrée/sortie (ins={}, outs={})", channels.ins, channels.outs));
        }
        let in_fmt = Fmt::from_asio(&driver.input_data_type().map_err(|e| format!("input type: {e:?}"))?)
            .ok_or("format d'entrée ASIO non supporté (attendu Int16/24/32 ou Float32 LSB)")?;
        let out_fmt = Fmt::from_asio(&driver.output_data_type().map_err(|e| format!("output type: {e:?}"))?)
            .ok_or("format de sortie ASIO non supporté")?;

        // 3) Snap de la taille à la grille légale.
        let size = match asio_buffer_sizes() {
            Some((mn, mx, pf, gr)) => snap_buffer_size(desired_buffer, mn, mx, pf, gr),
            None => desired_buffer,
        };

        // 4) PRIMING (arme l'ADC).
        prime(&driver, n_in, n_out, size);

        // 5) Ouverture réelle : UN seul ASIOCreateBuffers(in+out).
        let asio_streams = driver
            .prepare_input_stream(None, n_in, Some(size))
            .and_then(|s| driver.prepare_output_stream(s.input, n_out, Some(size)))
            .map_err(|e| format!("create_buffers(in+out): {e:?}"))?;
        let buffer_size = asio_streams
            .input
            .as_ref()
            .or(asio_streams.output.as_ref())
            .map(|s| s.buffer_size)
            .unwrap_or(size)
            .max(1) as u32;
        let streams = Arc::new(Mutex::new(Some(asio_streams)));

        // Vérification d'armement (télémétrie objective à chaque cold-start) : le
        // callback mesure la fraîcheur du 1er sample ch0 ; un thread logge un verdict
        // ~1,5 s après le start (ARMÉE / FIGÉE / SATURE).
        let arm_changes = Arc::new(AtomicU64::new(0));
        let arm_blocks = Arc::new(AtomicU64::new(0));
        let arm_absmax_ppm = Arc::new(AtomicI64::new(0));
        let arm_prev = Arc::new(AtomicI64::new(i64::MIN));

        // 6) Callback duplex : entrée → sample_tx ; mixer → sortie.
        let cb_id = {
            // Clones locaux : le closure `move` prend les clones, les originaux restent
            // disponibles (capture_callbacks est réutilisé par le start-timeout).
            let streams = streams.clone();
            let arm_changes = arm_changes.clone();
            let arm_blocks = arm_blocks.clone();
            let arm_absmax_ppm = arm_absmax_ppm.clone();
            let arm_prev = arm_prev.clone();
            let sample_tx = sample_tx.clone();
            let capture_drops = capture_drops.clone();
            let capture_callbacks = capture_callbacks.clone();
            let input_frames = input_frames.clone();
            let capture_feeding = capture_feeding.clone();
            let mixer = mixer.clone();
            let output_callbacks = output_callbacks.clone();
            let output_frames = output_frames.clone();
            let output_pair_start = output_pair_start.clone();
            let mut out_scratch: Vec<f32> = Vec::new();
            driver.add_callback(move |info| {
                let idx = info.buffer_index as usize;
                if idx > 1 {
                    return; // double-buffer ASIO : 0 ou 1
                }
                let guard = streams.lock();
                let Some(st) = guard.as_ref() else { return };

                // --- ENTRÉE : dé-entrelacé natif → f32 entrelacé → sample_tx ---
                if let Some(inp) = st.input.as_ref() {
                    let bufsz = inp.buffer_size as usize;
                    let nin = inp.buffer_infos.len();
                    input_frames.store(bufsz as u32, Ordering::Relaxed);
                    // Liveness : +1 par callback, TOUJOURS (même parké).
                    capture_callbacks.fetch_add(1, Ordering::Relaxed);
                    // Armement : fraîcheur du 1er sample ch0 (indépendant du feeding).
                    if bufsz > 0 && nin > 0 {
                        let base0 = inp.buffer_infos[0].buffers[idx] as *const u8;
                        if !base0.is_null() {
                            let s0 = unsafe { in_fmt.read(base0, 0) };
                            let key = s0.to_bits() as i64;
                            let prev = arm_prev.swap(key, Ordering::Relaxed);
                            if prev != i64::MIN && prev != key {
                                arm_changes.fetch_add(1, Ordering::Relaxed);
                            }
                            arm_blocks.fetch_add(1, Ordering::Relaxed);
                            arm_absmax_ppm.fetch_max(
                                (s0.abs().clamp(0.0, 4.0) * 1_000_000.0) as i64,
                                Ordering::Relaxed,
                            );
                        }
                    }
                    if capture_feeding.load(Ordering::Relaxed) && bufsz > 0 && nin > 0 {
                        let mut interleaved = vec![0.0f32; bufsz * nin];
                        for (c, bi) in inp.buffer_infos.iter().enumerate() {
                            let base = bi.buffers[idx] as *const u8;
                            if base.is_null() {
                                continue;
                            }
                            for f in 0..bufsz {
                                interleaved[f * nin + c] = unsafe { in_fmt.read(base, f) };
                            }
                        }
                        match sample_tx.try_send(interleaved) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                capture_drops.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(TrySendError::Disconnected(_)) => {}
                        }
                    }
                }

                // --- SORTIE : mixer stéréo f32 → natif dé-entrelacé ---
                if let Some(out) = st.output.as_ref() {
                    let bufsz = out.buffer_size as usize;
                    output_callbacks.fetch_add(1, Ordering::Relaxed);
                    output_frames.store(bufsz as u32, Ordering::Relaxed);
                    out_scratch.clear();
                    out_scratch.resize(bufsz * 2, 0.0);
                    mixer.mix_into(&mut out_scratch);
                    // Lot B — le mix stéréo va sur la PAIRE choisie (canaux `ps`, `ps+1`),
                    // zéros sur tous les autres canaux ouverts (nécessaire chaque bloc :
                    // double-buffer ASIO + la paire peut bouger en live). Lu ici = swap
                    // atomique sans réouverture.
                    let n_out = out.buffer_infos.len();
                    let ps = clamp_output_pair(output_pair_start.load(Ordering::Relaxed), n_out);
                    for (c, bi) in out.buffer_infos.iter().enumerate() {
                        let base = bi.buffers[idx] as *mut u8;
                        if base.is_null() {
                            continue;
                        }
                        // Canal L de la paire → out_scratch[.. + 0], R → [.. + 1], sinon 0.
                        let lane = if c == ps {
                            Some(0)
                        } else if c == ps + 1 {
                            Some(1)
                        } else {
                            None
                        };
                        for f in 0..bufsz {
                            let v = match lane {
                                Some(l) => out_scratch[f * 2 + l],
                                None => 0.0,
                            };
                            unsafe { out_fmt.write(base, f, v) };
                        }
                    }
                }
            })
        };

        // Message callback : honore kAsioResetRequest (signale le superviseur).
        let msg_id = {
            let signal = reset_signal.clone();
            driver.add_message_callback(move |sel| {
                if matches!(sel, sys::AsioMessageSelectors::kAsioResetRequest) {
                    signal.signal();
                }
            })
        };

        // 7) UN seul ASIOStart + start-timeout.
        let started_at = capture_callbacks.load(Ordering::Relaxed);
        driver.start().map_err(|e| {
            driver.remove_callback(cb_id);
            driver.remove_message_callback(msg_id);
            format!("ASIOStart: {e:?}")
        })?;
        let t0 = Instant::now();
        while capture_callbacks.load(Ordering::Relaxed) == started_at && t0.elapsed() < START_TIMEOUT {
            std::thread::sleep(Duration::from_millis(5));
        }
        if capture_callbacks.load(Ordering::Relaxed) == started_at {
            tracing::warn!(
                target: "jamodio::audio",
                "AsioDuplexHost : aucun callback en {START_TIMEOUT:?} après ASIOStart — driver muet (le superviseur de liveness prendra le relais)"
            );
        } else {
            // ── VÉRIFICATION DU RATE RÉEL (le driver peut MENTIR) ──────────────
            // On MESURE la cadence des callbacks sur une brève fenêtre : le rate
            // réel = cb_par_seconde × buffer_size. C'est le SEUL signal fiable —
            // `sample_rate()` peut rapporter 48 000 alors que le matériel tourne à
            // 44,1 (Focusrite natif : mensonge prouvé le 04/08, cb=689×64≈44 100).
            // Si le mesuré diverge nettement (> 3 %) du déclaré, on RETIENT LE
            // MESURÉ → la garde R2 (start_capture) refusera un vrai non-48. Coût :
            // ~500 ms à l'ouverture (hors thread RT, une fois au join).
            let c0 = capture_callbacks.load(Ordering::Relaxed);
            let m0 = Instant::now();
            std::thread::sleep(Duration::from_millis(500));
            let dt = m0.elapsed().as_secs_f64();
            let cb_delta = capture_callbacks.load(Ordering::Relaxed).saturating_sub(c0);
            if cb_delta > 0 && dt > 0.0 && buffer_size > 0 {
                let measured_sr = (cb_delta as f64 / dt) * buffer_size as f64;
                let declared = native_sr as f64;
                if (measured_sr - declared).abs() / declared > 0.03 {
                    tracing::warn!(
                        target: "jamodio::audio",
                        declared_sr = native_sr,
                        measured_sr = measured_sr as u32,
                        cb_per_sec = (cb_delta as f64 / dt) as u32,
                        buffer_size,
                        "driver MENT sur son rate (déclaré ≠ mesuré) — on retient le rate RÉEL mesuré (la capture sera refusée si ≠ 48 kHz)"
                    );
                    native_sr = measured_sr.round() as u32;
                    // Snap au rate standard le plus proche (le mesuré a ±~1 % de
                    // bruit) : évite qu'un 44 096 mesuré passe pour « ni 44,1 ni 48 ».
                    for std_sr in [44100u32, 48000, 88200, 96000, 176400, 192000, 32000, 22050, 11025] {
                        if (native_sr as i64 - std_sr as i64).abs() <= 400 {
                            native_sr = std_sr;
                            break;
                        }
                    }
                }
            }
            tracing::info!(
                target: "jamodio::audio",
                first_callback_ms = t0.elapsed().as_millis() as u64,
                buffer_size, native_sr, channels_in = n_in as u16,
                in_fmt = ?in_fmt, out_fmt = ?out_fmt,
                "AsioDuplexHost ouvert (single-owner : 1 ASIOInit, priming, 1 create(in+out), 1 start)"
            );
        }

        // Vérification d'armement, non bloquante : ~1,5 s après le start, un thread
        // logge un verdict OBJECTIF (l'entrée est-elle vivante, ou figée = wedge ?).
        // Fire-and-forget : lit les atomiques alimentés par le callback.
        {
            let (c, b, a) = (arm_changes.clone(), arm_blocks.clone(), arm_absmax_ppm.clone());
            let _ = std::thread::Builder::new()
                .name("asio-arm-check".into())
                .spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    let blocks = b.load(Ordering::Relaxed).max(1);
                    let changes = c.load(Ordering::Relaxed);
                    let absmax = a.load(Ordering::Relaxed) as f64 / 1_000_000.0;
                    let live_ratio = changes as f64 / blocks as f64;
                    if live_ratio < 0.05 {
                        tracing::warn!(
                            target: "jamodio::audio",
                            changes, blocks, absmax = format!("{absmax:.4}"),
                            "AsioDuplexHost : entrée FIGÉE au démarrage — WEDGE (priming insuffisant sur cette interface ?)"
                        );
                    } else if absmax > 0.95 {
                        tracing::warn!(
                            target: "jamodio::audio",
                            changes, blocks, absmax = format!("{absmax:.4}"),
                            "AsioDuplexHost : entrée VIVANTE mais SATURE (larsen/self-monitor ou jeu fort, PAS le wedge)"
                        );
                    } else {
                        tracing::info!(
                            target: "jamodio::audio",
                            changes, blocks, absmax = format!("{absmax:.4}"),
                            "AsioDuplexHost : entrée ARMÉE (vivante) au démarrage — cold-start sain"
                        );
                    }
                });
        }

        Ok(Self {
            driver,
            streams,
            cb_id,
            msg_id,
            channels_in: n_in as u16,
            native_sr,
            buffer_size,
        })
    }
}

impl Drop for AsioDuplexHost {
    fn drop(&mut self) {
        // Retire les callbacks du registre global AVANT d'arrêter/détruire.
        self.driver.remove_callback(self.cb_id);
        self.driver.remove_message_callback(self.msg_id);
        let _ = self.driver.stop();
        let _ = self.driver.dispose_buffers();
        // Vide les buffers (les pointeurs deviennent invalides après dispose).
        *self.streams.lock() = None;
        // Le dernier `Arc<DriverInner>` (celui-ci) droppé → ASIOExit + removeCurrentDriver.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (Invariants de `clamp_output_pair` → `audio::output_pair` tests, cross-platform.)
    #[test]
    fn snap_keeps_legal_size() {
        // Focusrite : min16 max1024 pref64 gran16 → 64 est légal, on le garde.
        assert_eq!(snap_buffer_size(64, 16, 1024, 64, 16), 64);
        // 100 (hors grille de 16) → multiple de 16 le plus proche = 96.
        assert_eq!(snap_buffer_size(100, 16, 1024, 64, 16), 96);
        // Hors plage → préférée.
        assert_eq!(snap_buffer_size(4, 16, 1024, 64, 16), 64);
        assert_eq!(snap_buffer_size(5000, 16, 1024, 64, 16), 64);
    }

    #[test]
    fn snap_power_of_two_driver() {
        // gran == -1 : puissances de 2. 64 déjà 2^6 → gardé ; 100 → 128 (le plus proche).
        assert_eq!(snap_buffer_size(64, 32, 2048, 256, -1), 64);
        assert_eq!(snap_buffer_size(100, 32, 2048, 256, -1), 128);
    }

    #[test]
    fn snap_any_size_driver() {
        // gran <= 0 (hors -1) : toute taille légale gardée telle quelle.
        assert_eq!(snap_buffer_size(77, 16, 1024, 128, 0), 77);
    }

    // Round-trip f32 → format natif → f32 (tolérance selon la profondeur de bits).
    unsafe fn roundtrip(fmt: Fmt, v: f32) -> f32 {
        let mut buf = [0u8; 8];
        fmt.write(buf.as_mut_ptr(), 0, v);
        fmt.read(buf.as_ptr(), 0)
    }

    #[test]
    fn format_roundtrip() {
        unsafe {
            for &v in &[0.0f32, 0.5, -0.5, 0.999, -0.999] {
                assert!((roundtrip(Fmt::F32, v) - v).abs() < 1e-9, "F32 {v}");
                assert!((roundtrip(Fmt::I32, v) - v).abs() < 1e-6, "I32 {v}");
                assert!((roundtrip(Fmt::I24, v) - v).abs() < 1e-3, "I24 {v}");
                assert!((roundtrip(Fmt::I16, v) - v).abs() < 1e-2, "I16 {v}");
            }
            // Clamp : au-delà de ±1 on sature sans déborder.
            assert!((roundtrip(Fmt::I32, 2.0) - 1.0).abs() < 1e-6);
            assert!((roundtrip(Fmt::I16, -3.0) - -1.0).abs() < 1e-2);
        }
    }

    #[test]
    fn fmt_bytes() {
        assert_eq!(Fmt::I16.bytes(), 2);
        assert_eq!(Fmt::I24.bytes(), 3);
        assert_eq!(Fmt::I32.bytes(), 4);
        assert_eq!(Fmt::F32.bytes(), 4);
    }
}
