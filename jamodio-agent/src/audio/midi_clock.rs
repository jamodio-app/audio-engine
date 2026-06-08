//! Ticker d'horloge silencieux pour le mode MIDI (Variante A du Chantier #2).
//!
//! ## Contexte
//!
//! Quand `input_source = InputSource::Midi(_)` et qu'un plugin instrument
//! est chargé en INSERT (ex : BFD Player, Kontakt, AUSampler), la capture
//! audio CPAL n'a aucune utilité musicale :
//!
//! - Le plugin reçoit les events MIDI via le channel `midi_event_rx`
//! - Il **génère lui-même** l'audio de sortie (voix samplées / synthèse)
//! - L'input audio capturé est ignoré (= dead path mémoire + CPU + risque
//!   de fuite : un device de routing externe type "Pro Tools Audio Bridge"
//!   ou "BlackHole" peut injecter un signal parasite qui se mélangerait
//!   accidentellement au son INSERT)
//!
//! Avant ce module, l'agent gardait CPAL ouvert et **forçait `samples = 0`**
//! côté process_stage. Coût : 1 callback CPAL par bloc + 1 lecture device +
//! 1 copy + 1 fill(0). Pour rien.
//!
//! ## Solution
//!
//! `MidiSilenceClock` spawne un thread dédié qui pousse des blocs de
//! silence dans `sample_tx` au rythme audio bloc (128 samples × 2 ch
//! = 256 samples interleaved @ 48 kHz = **2,667 ms** par bloc).
//!
//! L'encoder thread aval reste **strictement identique** : il consomme
//! `sample_rx` sans se soucier de la source. Le swap CPAL ↔ ticker en
//! amont est invisible pour le pipeline downstream.
//!
//! ## Précision timing
//!
//! Cible : ±100 µs sur le tick 2,667 ms (= ~0,04 % drift max).
//!
//! Stratégie cross-platform :
//! - Promotion RT du thread (workgroup CoreAudio macOS / MMCSS Windows /
//!   `thread-priority` Linux) — même chemin que l'encoder thread, cohérence
//!   garantie.
//! - Sleep absolu sur deadline = `start + n_blocks × block_duration` (pas
//!   d'accumulation d'erreur sur les sleeps relatifs).
//! - Sleep jusqu'à ~300 µs avant la deadline, **busy-spin** sur les
//!   derniers µs (`std::hint::spin_loop`) → précision sample.
//! - Windows : `timeBeginPeriod(1)` activé pendant la vie du clock pour
//!   amener la granularité sleep de 15 ms (par défaut) à 1 ms.
//!
//! ## Drop
//!
//! `Drop` signale l'arrêt via `AtomicBool::Release` et `join()` le thread.
//! Le thread vérifie le flag entre chaque bloc → arrêt sous ~2,7 ms max.

use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bloc audio cible : 128 frames par tick (aligné sur CPAL `Fixed(128)` de
/// `capture.rs` et `playback.rs`). Le ticker pousse donc des blocs de taille
/// identique à ceux qu'aurait produit CPAL en mode AUDIO → l'encoder
/// thread ne voit aucune différence de format à la bascule MIDI↔AUDIO.
const BLOCK_FRAMES: usize = 128;

/// Marge avant deadline en-dessous de laquelle on busy-spin au lieu de
/// `std::thread::sleep`. 300 µs = compromis : assez large pour absorber
/// le jitter de wake-up scheduler (mesure pratique macOS workgroup ~100 µs,
/// Windows MMCSS ~150 µs, Linux nice +95 ~250 µs), assez court pour ne pas
/// brûler de CPU inutilement.
const SPIN_MARGIN_NS: u64 = 300_000;

/// RAII handle du ticker MIDI. Drop signale l'arrêt et join le thread.
///
/// Cycle de vie typique :
/// 1. `set_input_source(Midi(_))` en cours de capture → construit un clock,
///    le swap atomique avec l'ancien `CaptureMode::Audio` (cf. pipeline.rs)
/// 2. Le thread tourne, pousse des blocs silencieux à `sample_tx` au rythme
///    audio bloc, jusqu'au prochain `set_input_source(Audio)` ou
///    `stop_capture`.
/// 3. Drop : `stop.store(true)` puis `handle.join()`. Délai d'arrêt
///    borné à ~2,7 ms (= 1 bloc).
pub struct MidiSilenceClock {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MidiSilenceClock {
    /// Démarre le ticker au format `(channels, sample_rate)` fixé par
    /// `start_capture`. Le thread est spawné immédiatement et tourne
    /// jusqu'au `Drop`.
    ///
    /// Le ticker pousse des blocs de `BLOCK_FRAMES × channels` samples
    /// interleaved (silence) à la cadence `BLOCK_FRAMES / sample_rate`
    /// secondes. Format identique à ce qu'aurait produit CPAL en mode
    /// AUDIO → l'encoder thread aval est agnostique à la source.
    ///
    /// Erreurs :
    /// - `std::io::Error` si `thread::Builder::spawn` échoue (cas extrême :
    ///   OOM, ulimit). Le caller traite ça comme `CaptureStartError::Other`.
    /// - Panic si `channels == 0` ou `sample_rate == 0` (contrat caller :
    ///   format toujours valide).
    pub fn start(
        sample_tx: Sender<Vec<f32>>,
        channels: u16,
        sample_rate: u32,
    ) -> std::io::Result<Self> {
        assert!(channels > 0, "channels must be > 0");
        assert!(sample_rate > 0, "sample_rate must be > 0");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let block_samples_interleaved = BLOCK_FRAMES * channels as usize;
        let block_duration_ns =
            (BLOCK_FRAMES as u64) * 1_000_000_000 / (sample_rate as u64);

        let handle = std::thread::Builder::new()
            .name("midi-silence-clock".into())
            .spawn(move || {
                run(
                    stop_for_thread,
                    sample_tx,
                    block_samples_interleaved,
                    block_duration_ns,
                )
            })?;

        tracing::info!(
            target: "jamodio::midi_clock",
            block_frames = BLOCK_FRAMES,
            channels,
            sample_rate,
            block_ms = block_duration_ns as f32 / 1_000_000.0,
            "MIDI silence clock started"
        );

        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for MidiSilenceClock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // join() ne devrait jamais paniquer (le thread ne fait que push
            // des silences + sleep). En cas extrême, on log et on continue.
            if let Err(e) = handle.join() {
                tracing::warn!(
                    target: "jamodio::midi_clock",
                    error = ?e,
                    "MIDI silence clock thread join panicked"
                );
            } else {
                tracing::info!(target: "jamodio::midi_clock", "MIDI silence clock stopped");
            }
        }
    }
}

/// Boucle principale du thread ticker.
///
/// Architecture : deadline absolue (`start + n × block_duration`) pour
/// éviter l'accumulation d'erreur des sleeps relatifs. Sleep coarse puis
/// busy-spin pour les derniers µs.
fn run(
    stop: Arc<AtomicBool>,
    sample_tx: Sender<Vec<f32>>,
    block_samples_interleaved: usize,
    block_duration_ns: u64,
) {
    // 1. Promotion RT du thread. Best-effort : si la promotion échoue (CI
    //    Linux sans CAP_SYS_NICE, etc.), on continue en priorité normale —
    //    le thread restera fonctionnel mais avec plus de jitter.
    let _rt = crate::audio::rt_priority::promote_thread_for_audio(None);

    // 2. Windows : augmente la résolution du timer système à 1 ms le temps
    //    de vie du clock. Sans ça, std::thread::sleep est précis à ~15 ms
    //    seulement (granularité par défaut), ce qui ferait dériver le tick
    //    de plusieurs blocs. `_hires` est drop à la fin de la fonction →
    //    appel symétrique timeEndPeriod via Drop.
    #[cfg(target_os = "windows")]
    let _hires = HighResolutionTimer::activate();

    let silence: Vec<f32> = vec![0.0; block_samples_interleaved];
    let start = Instant::now();
    let mut block_count: u64 = 0;
    let block_duration = Duration::from_nanos(block_duration_ns);
    let spin_margin = Duration::from_nanos(SPIN_MARGIN_NS);

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }

        // 3. Calcule la deadline ABSOLUE du prochain tick. Pas d'accumulation
        //    d'erreur car on additionne toujours depuis `start`.
        let deadline = start + block_duration.saturating_mul(block_count as u32 + 1);

        // 4. Sleep coarse jusqu'à ~SPIN_MARGIN avant la deadline.
        let now = Instant::now();
        if let Some(sleep_total) = deadline.checked_duration_since(now) {
            if let Some(coarse_sleep) = sleep_total.checked_sub(spin_margin) {
                if !coarse_sleep.is_zero() {
                    std::thread::sleep(coarse_sleep);
                }
            }
        }

        // 5. Busy-spin pour les derniers µs. Vérification stop dans la boucle
        //    pour ne pas bloquer le shutdown sur un thread qui sleep mal.
        while Instant::now() < deadline {
            if stop.load(Ordering::Acquire) {
                return;
            }
            std::hint::spin_loop();
        }

        // 6. Push le bloc silencieux. `try_send` non-bloquant : si le
        //    sample_tx est plein (= encoder en retard, anomalie), on droppe
        //    silencieusement. Le compteur capture_drops n'est PAS incrémenté
        //    ici (il sert au monitoring CPAL ; en mode MIDI un drop ticker
        //    est sans gravité car le contenu est de toute façon du silence).
        //    Si l'encoder thread a terminé (channel Disconnected = stop
        //    capture en cours), on quitte la boucle.
        match sample_tx.try_send(silence.clone()) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
        }

        block_count += 1;
    }
}

// ─── Windows : timer haute résolution (timeBeginPeriod / timeEndPeriod) ───
//
// Augmente la précision de `std::thread::sleep` de ~15 ms (par défaut) à
// ~1 ms le temps de vie du `HighResolutionTimer`. RAII : Drop appelle
// timeEndPeriod avec la même valeur pour libérer la ressource système.
//
// La doc Microsoft recommande timeBeginPeriod(1) au plus juste — c'est ce
// qu'on utilise. Limite : impacte TOUS les sleeps du process. Coût
// énergétique négligeable pour notre usage (1 thread audio).
//
// Binding via `extern "system"` direct contre `winmm.dll` (lib statique
// `winmm.lib` côté MSVC) — évite la dépendance aux paths internes de
// `windows-sys` qui peuvent changer entre features (`Win32_Media` vs
// `Win32_Media_Multimedia`). La signature MSDN officielle est :
//
//   MMRESULT timeBeginPeriod(UINT uPeriod);
//   MMRESULT timeEndPeriod(UINT uPeriod);
//
// `MMRESULT` est un `u32` (0 = TIMERR_NOERROR, 97 = TIMERR_NOCANDO).

#[cfg(target_os = "windows")]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(uperiod: u32) -> u32;
    fn timeEndPeriod(uperiod: u32) -> u32;
}

#[cfg(target_os = "windows")]
struct HighResolutionTimer;

#[cfg(target_os = "windows")]
impl HighResolutionTimer {
    fn activate() -> Self {
        // SAFETY: timeBeginPeriod prend une période en ms et retourne
        // TIMERR_NOERROR (0) ou TIMERR_NOCANDO (97) si non supporté. On
        // ignore le résultat — si l'OS refuse, le ticker fonctionne quand
        // même (juste avec plus de jitter scheduler).
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}

#[cfg(target_os = "windows")]
impl Drop for HighResolutionTimer {
    fn drop(&mut self) {
        // SAFETY: appel symétrique à activate(). Doit être appelé exactement
        // une fois pour libérer la ressource système (refcount interne OS).
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    /// Sanity : le ticker pousse des blocs à un rythme approximatif au
    /// format canonique stéréo 48 kHz. On tolère ±50 % sur 1 sec de mesure
    /// (= ~375 blocs attendus, 190-560 acceptés) pour absorber le jitter
    /// scheduler CI (notamment sur GH Actions sans privilèges RT).
    #[test]
    fn clock_pushes_blocks_at_approximate_rate() {
        let (tx, rx) = bounded::<Vec<f32>>(1024);
        let clock = MidiSilenceClock::start(tx, 2, 48_000).expect("clock start");
        std::thread::sleep(Duration::from_millis(1000));
        drop(clock);

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        // Cible nominale = 1000 / 2.667 ≈ 375 blocs/sec. Tolérance large.
        assert!(
            (190..=560).contains(&count),
            "expected ~375 blocks/s, got {count} (tolerated 190-560)"
        );
    }

    /// Drop bloque jusqu'à l'arrêt effectif du thread. Après drop, plus
    /// aucun bloc ne doit arriver. On vérifie en lisant le channel
    /// quelques ms après le drop.
    #[test]
    fn drop_stops_the_thread() {
        let (tx, rx) = bounded::<Vec<f32>>(128);
        let clock = MidiSilenceClock::start(tx, 2, 48_000).expect("clock start");
        std::thread::sleep(Duration::from_millis(30));
        drop(clock);
        // Vide tout ce qui était déjà en queue.
        while rx.try_recv().is_ok() {}
        // Attend largement plus qu'un bloc — aucun nouveau bloc ne doit
        // arriver.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            rx.try_recv().is_err(),
            "no block expected after drop, but channel still alive"
        );
    }

    /// Format paramétré : la taille du bloc s'adapte à `channels`, et le
    /// rythme s'adapte à `sample_rate`. On vérifie sur un format atypique
    /// (4 canaux, 44 100 Hz) que les blocs ont bien la taille attendue.
    ///
    /// Test déterministe : on attend le PREMIER bloc via `recv_timeout`
    /// (max 2 s), puis on vérifie son format. Évite la flakiness des
    /// tests basés sur `sleep + try_recv` quand le scheduler CI charge
    /// plusieurs threads en parallèle.
    #[test]
    fn block_format_respects_channels_and_rate() {
        let (tx, rx) = bounded::<Vec<f32>>(64);
        let channels: u16 = 4;
        let sr: u32 = 44_100;
        let clock = MidiSilenceClock::start(tx, channels, sr).expect("clock start");

        let first = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("at least one block within 2 s");
        let expected_size = BLOCK_FRAMES * channels as usize;
        assert_eq!(
            first.len(),
            expected_size,
            "block format = BLOCK_FRAMES × channels"
        );
        assert!(first.iter().all(|&s| s == 0.0), "block entièrement silence");

        drop(clock);
    }

    /// Channel plein (consumer absent) ne doit PAS faire paniquer le thread
    /// ni bloquer le drop. try_send Full = drop silencieux.
    #[test]
    fn full_channel_does_not_panic() {
        let (tx, _rx) = bounded::<Vec<f32>>(2); // très petite capacité
        let clock = MidiSilenceClock::start(tx, 2, 48_000).expect("clock start");
        // Ne lit jamais le channel → après 2 blocs il sature, le ticker
        // doit continuer à tourner sans panic.
        std::thread::sleep(Duration::from_millis(50));
        drop(clock); // doit retourner sans hang
    }

    /// Channel déconnecté côté receiver → le thread quitte proprement.
    /// Évite un thread orphelin si l'encoder thread meurt avant le drop.
    #[test]
    fn disconnected_channel_terminates_thread() {
        let (tx, rx) = bounded::<Vec<f32>>(8);
        let clock = MidiSilenceClock::start(tx, 2, 48_000).expect("clock start");
        // Drop le receiver → le tx try_send va retourner Disconnected.
        drop(rx);
        // Le thread doit terminer dans les ~2,7 ms (1 bloc), on attend
        // largement pour ne pas être flaky.
        std::thread::sleep(Duration::from_millis(20));
        // Drop du clock = join sur le thread déjà terminé → instantané,
        // pas de hang.
        drop(clock);
    }

    #[test]
    #[should_panic(expected = "channels must be > 0")]
    fn zero_channels_panics() {
        let (tx, _rx) = bounded::<Vec<f32>>(1);
        let _ = MidiSilenceClock::start(tx, 0, 48_000);
    }

    #[test]
    #[should_panic(expected = "sample_rate must be > 0")]
    fn zero_sample_rate_panics() {
        let (tx, _rx) = bounded::<Vec<f32>>(1);
        let _ = MidiSilenceClock::start(tx, 2, 0);
    }
}
