use super::ring_buffer::JitterBuffer;
use crate::record::RecordCmd;
use crossbeam_channel::Sender;
use std::collections::HashMap;

/// Id réservé du stream de self-monitor (capture locale rebouclée en sortie
/// pour que l'utilisateur s'entende dans son casque sans passer par la chaîne
/// browser à 25 ms). Mixé comme un stream normal mais exclu des stats remote
/// (stream_count, total_underruns, mean_target_ms) pour ne pas polluer l'UI.
pub const SELF_MONITOR_ID: &str = "self";

/// Cible jitter buffer du self-monitor (ms). 5 = MIN_TARGET_MS du ring buffer ;
/// le signal vient du même process que la capture, donc pas de gigue réseau,
/// on prend le minimum stable.
const SELF_MONITOR_TARGET_MS: usize = 5;

/// Mixes N remote audio streams into a single stereo output.
/// Each stream has its own jitter buffer and volume control.
pub struct AudioMixer {
    streams: HashMap<String, StreamState>,
    /// Buffer de travail réutilisé par mix_into — évite ~400 alloc/s
    /// dans le callback CPAL temps-réel.
    temp_buf: Vec<f32>,
    /// Cible jitter buffer par défaut (ms) — appliquée aux nouveaux streams.
    /// Si `None`, JitterBuffer utilise sa cible initiale par défaut. Override
    /// via `set_target_ms_all` (handler SetBuffer côté UI).
    default_target_ms: Option<usize>,
    /// REC-3 : si `Some(tx)`, un enregistrement est en cours et les push_*
    /// (self, peer) + mix_into envoient leurs samples au thread record via
    /// `try_send` non-bloquant. Si `None`, les tap sites sont no-op (1 if).
    record_tx: Option<Sender<RecordCmd>>,
    /// Master gain global appliqué dans `mix_into` après le mix des streams.
    /// Plage [0.0, 1.5] (cohérent avec les faders peer/self côté UI).
    /// Default 1.0 (unity).
    master_gain: f32,
    /// DIM factor — atténuation temporaire des instruments quand l'utilisateur
    /// active DIM côté UI (pour entendre la conversation talkback clairement).
    /// Plage [0.0, 1.0], typiquement 0.25 (-12dB) ou 1.0 (off). Appliqué
    /// dans `mix_into` après la somme des streams et avant le master_gain.
    /// **Le tap REC-3 push_mix est positionné AVANT dim et master** pour que
    /// le fichier MIX enregistré soit le mix post-fader des instruments
    /// SEUL (= ce qui irait à un peer théorique), indépendant des réglages
    /// d'écoute locaux dim/master. Cohérent avec le tap browser sur
    /// `instrumentMixBus` qui est aussi pre-dim/pre-master.
    dim_factor: f32,
}

struct StreamState {
    jitter: JitterBuffer,
    volume: f32,
    /// Pan range [-1.0, 1.0]. -1 = full left, 0 = center, +1 = full right.
    /// Applique constant-power panning dans `mix_into` (gain_L = cos(θ),
    /// gain_R = sin(θ) avec θ = (pan+1)·π/4). Le DAW classique pour un
    /// signal stéréo entrant = balance L/R. Default 0.0 (centré).
    pan: f32,
    rms: f32,
    /// Snapshot du `overflow_drops` du jitter au précédent push, pour ne
    /// loguer que sur événement (rate-limited via puissance de 2).
    last_overflow_drops: u64,
    buffer_full_count: u64,
    /// Idem pour le drift drain (pull-side).
    last_drift_drops: u64,
    drift_drain_count: u64,
    /// Sprint S6 — timestamps des drift drains observés sur la fenêtre
    /// glissante `UNSTABLE_WINDOW_SECS`. Purgé à chaque ajout (= cold path
    /// car ~1 drain max par 2 s). Sert à détecter un peer "instable" qui
    /// envoie en bursts (encoder stalls côté lui, Opus DTX, CPU saturé, etc.).
    /// `VecDeque` pré-alloué cap 32 = couvre une fenêtre 30 s sans
    /// réallocation tant que le burst rate < 1 drain/s.
    drift_drain_history: std::collections::VecDeque<std::time::Instant>,
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            temp_buf: Vec::new(),
            default_target_ms: None,
            record_tx: None,
            master_gain: 1.0,
            dim_factor: 1.0,
        }
    }

    /// DIM factor (= ducking des instruments quand le user veut entendre
    /// la voix talkback clairement). Plage [0.0, 1.0], typiquement 0.25
    /// (-12dB) ou 1.0 (off). Clamp défensif côté agent.
    pub fn set_dim(&mut self, factor: f32) {
        self.dim_factor = if factor.is_finite() { factor.clamp(0.0, 1.0) } else { 1.0 };
    }

    /// REC-3 : armer/désarmer l'enregistrement. Quand `Some(tx)`, les tap
    /// sites (push_self_samples, push_samples remote, mix_into) envoient
    /// leurs samples au thread record via `try_send` non-bloquant. Quand
    /// `None`, les taps sont no-op (1 if check). Appelé depuis le pipeline
    /// au start_recording / stop_recording.
    pub fn set_record_tx(&mut self, tx: Option<Sender<RecordCmd>>) {
        self.record_tx = tx;
    }

    /// Master gain global appliqué dans `mix_into`. Clamp défensif dans
    /// [0.0, 1.5] (NaN devient 1.0 — unity). Le tap record push_mix
    /// reçoit l'output APRÈS application, donc le fichier MIX reflète
    /// le réglage master fader courant.
    pub fn set_master_gain(&mut self, gain: f32) {
        self.master_gain = if gain.is_finite() { gain.clamp(0.0, 1.5) } else { 1.0 };
    }

    /// Helper interne : try_send vers le record thread sans bloquer.
    /// Drop silencieux si le channel est plein (thread en retard) — le warn
    /// est émis côté thread record qui surveille sa queue length.
    fn record_send(&self, cmd: RecordCmd) {
        if let Some(tx) = &self.record_tx {
            let _ = tx.try_send(cmd);
        }
    }

    /// Add a new remote stream.
    pub fn add_stream(&mut self, producer_id: &str) {
        let mut jitter = JitterBuffer::new();
        if let Some(ms) = self.default_target_ms {
            jitter.set_target_ms(ms);
        }
        self.streams.insert(producer_id.to_string(), StreamState {
            jitter,
            volume: 1.0,
            pan: 0.0,
            rms: 0.0,
            last_overflow_drops: 0,
            buffer_full_count: 0,
            last_drift_drops: 0,
            drift_drain_count: 0,
            drift_drain_history: std::collections::VecDeque::with_capacity(32),
        });
    }

    /// Remove a stream.
    pub fn remove_stream(&mut self, producer_id: &str) {
        self.streams.remove(producer_id);
    }

    /// Crée le stream de self-monitor (boucle locale capture → mixer → playback).
    ///
    /// Volume initial = 0.0 (silencieux) : l'utilisateur doit explicitement
    /// ouvrir le fader « moi » côté UI via `SetSelfMonitorVolume`. Sans ça,
    /// risque de larsen au démarrage si micro ouvert près d'un haut-parleur.
    ///
    /// Jitter target = `SELF_MONITOR_TARGET_MS` (5 ms) : signal local sans
    /// gigue réseau, on prend le minimum stable. Latence ear-to-ear self
    /// résultante ≈ 5.4 ms (capture 2.7 + playback 2.7) + 5 ms target ≈ 10 ms.
    pub fn add_local_stream(&mut self) {
        let mut jitter = JitterBuffer::new();
        jitter.set_target_ms(SELF_MONITOR_TARGET_MS);
        // Chantier C — mode local : concealment des trous (pas de clic sur les
        // spikes plugin) + adaptation bornée (latence plafonnée, retour 5 ms).
        jitter.set_local_mode(true);
        self.streams.insert(SELF_MONITOR_ID.to_string(), StreamState {
            jitter,
            volume: 0.0,
            pan: 0.0,
            rms: 0.0,
            last_overflow_drops: 0,
            buffer_full_count: 0,
            last_drift_drops: 0,
            drift_drain_count: 0,
            drift_drain_history: std::collections::VecDeque::with_capacity(32),
        });
    }

    /// Supprime le stream self-monitor (appelé depuis `stop_capture`).
    pub fn remove_local_stream(&mut self) {
        self.streams.remove(SELF_MONITOR_ID);
    }

    /// Override le volume du self-monitor (0.0 = silence, 1.0 = unity, 1.5 = max).
    /// Appelé par le handler `SetSelfMonitorVolume` côté ws_server.
    pub fn set_self_monitor_volume(&mut self, volume: f32) {
        self.set_volume(SELF_MONITOR_ID, volume);
    }

    /// Push capture samples dans le stream self-monitor (depuis l'encoder
    /// thread, en parallèle de l'encodage Opus pour les pairs).
    /// No-op si `add_local_stream()` n'a pas été appelé (capture pas démarrée).
    pub fn push_self_samples(&mut self, samples: &[f32]) {
        if self.streams.contains_key(SELF_MONITOR_ID) {
            self.push_samples(SELF_MONITOR_ID, samples);
        }
        // REC-3 : tap stem-self. Pré-fader, post channel-split.
        // Fait APRÈS push_samples pour ne pas dépendre de l'existence du
        // stream self-monitor : on enregistre l'instrument même si le user
        // a coupé son monitor browser (mode agent typique selfMuteGain=0).
        if self.record_tx.is_some() && !samples.is_empty() {
            self.record_send(RecordCmd::PushSelf(samples.to_vec()));
        }
    }

    /// Set per-stream volume (0.0 to 1.5).
    pub fn set_volume(&mut self, producer_id: &str, volume: f32) {
        if let Some(stream) = self.streams.get_mut(producer_id) {
            stream.volume = volume.clamp(0.0, 1.5);
        }
    }

    /// Set per-stream volume by producer_id (alias for set_volume).
    pub fn set_stream_volume(&mut self, producer_id: &str, volume: f32) {
        self.set_volume(producer_id, volume);
    }

    /// Set per-stream pan, range [-1.0, 1.0]. -1=full left, 0=center, +1=full right.
    /// Applique constant-power panning dans `mix_into`. Pour SELF_MONITOR_ID,
    /// fonctionne pareil — le browser envoie producer_id="self".
    /// No-op si le stream n'existe pas (peer parti, race).
    pub fn set_pan(&mut self, producer_id: &str, pan: f32) {
        let p = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
        if let Some(stream) = self.streams.get_mut(producer_id) {
            stream.pan = p;
        }
    }

    /// Push decoded samples into a stream's jitter buffer.
    ///
    /// Le jitter buffer applique drop-oldest sur overflow (cf. `JitterBuffer::push`).
    /// Ici on rate-limit le warn sur l'INCRÉMENT de `overflow_drops` pour ne
    /// pas spammer (1 stream * 400 pkt/s en burst peut générer beaucoup
    /// d'événements).
    pub fn push_samples(&mut self, producer_id: &str, samples: &[f32]) {
        // REC-3 : tap stem-peer. Pre-fader (avant `vol *` dans mix_into),
        // post Opus decode. On filtre SELF_MONITOR_ID car push_self_samples
        // émet déjà son propre PushSelf (sinon double tap pour self).
        if self.record_tx.is_some() && producer_id != SELF_MONITOR_ID && !samples.is_empty() {
            self.record_send(RecordCmd::PushPeer(producer_id.to_string(), samples.to_vec()));
        }

        if let Some(stream) = self.streams.get_mut(producer_id) {
            // Compute RMS of pushed samples
            if !samples.is_empty() {
                let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
                stream.rms = (sum_sq / samples.len() as f32).sqrt();
            }

            stream.jitter.push(samples);

            let new_drops = stream.jitter.overflow_drops();
            if new_drops > stream.last_overflow_drops {
                stream.buffer_full_count += 1;
                if stream.buffer_full_count == 1 || stream.buffer_full_count.is_power_of_two() {
                    tracing::warn!(
                        target: "jamodio::mixer",
                        producer = &producer_id[..8.min(producer_id.len())],
                        events = stream.buffer_full_count,
                        oldest_dropped_total = new_drops,
                        "jitter buffer overflow — oldest samples dropped (burst SFU?)"
                    );
                }
                stream.last_overflow_drops = new_drops;
            }
        } else {
            tracing::warn!(
                target: "jamodio::mixer",
                producer = &producer_id[..8.min(producer_id.len())],
                "push_samples on unknown stream"
            );
        }
    }

    /// Surveille les drift-drains (samples jetés côté pull pour borner la
    /// latence) après un mix. Appelé depuis `mix_into` à la fin du tour pour
    /// ne pas faire de logging coûteux dans le hot path callback CPAL.
    fn report_drift_drops(&mut self) {
        for (producer_id, stream) in self.streams.iter_mut() {
            let new_drops = stream.jitter.drift_drops();
            if new_drops > stream.last_drift_drops {
                stream.drift_drain_count += 1;
                // Sprint S6 — track ce drain dans la fenêtre glissante 30 s
                // pour la détection peer instable. Push timestamp.
                // (Purge à la lecture côté `stream_unstable_events`.)
                stream.drift_drain_history.push_back(std::time::Instant::now());
                // Garde-fou anti-mémoire : si jamais la fenêtre n'est pas
                // purgée (= caller oublie de call stream_unstable_events),
                // on cap à 256 entrées (= ~1 min de drains à 4 Hz, suffisant
                // pour signaler une instabilité massive).
                while stream.drift_drain_history.len() > 256 {
                    stream.drift_drain_history.pop_front();
                }
                // Bug D : on logue uniquement les drains sévères (events > 4).
                // Les small drifts (1-4) sont normaux et bruyaient le log
                // sans signal. Combiné avec is_power_of_two, on logue à
                // events = 8, 16, 32, 64… → réduction ~70 % du spam.
                if stream.drift_drain_count > 4
                    && stream.drift_drain_count.is_power_of_two()
                {
                    tracing::warn!(
                        target: "jamodio::mixer",
                        producer = &producer_id[..8.min(producer_id.len())],
                        events = stream.drift_drain_count,
                        drained_total = new_drops,
                        target_ms = stream.jitter.target_ms(),
                        "jitter buffer drift drain — latence excessive ramenée à target"
                    );
                }
                stream.last_drift_drops = new_drops;
            }
        }
    }

    /// Mix all streams into the output buffer.
    /// Called from the CPAL playback callback.
    /// Output is interleaved stereo f32.
    pub fn mix_into(&mut self, output: &mut [f32]) {
        output.fill(0.0);

        // Resize uniquement si la taille du callback change (typiquement jamais
        // après le 1er appel : CPAL livre des blocs de taille fixe).
        if self.temp_buf.len() != output.len() {
            self.temp_buf.resize(output.len(), 0.0);
        }

        for stream in self.streams.values_mut() {
            stream.jitter.pull(&mut self.temp_buf);

            let vol = stream.volume;
            // Constant-power panning : pour pan ≈ 0, fast path sans cos/sin
            // (skip un test). Sinon angle = (pan+1) · π/4 ∈ [0, π/2],
            // gain_L = cos(angle), gain_R = sin(angle) — total power constant.
            // Le temp_buf est stéréo interleaved (L, R, L, R, ...) ; on
            // applique gain_L sur les samples pairs et gain_R sur les impairs.
            if stream.pan.abs() < f32::EPSILON {
                for (out, &sample) in output.iter_mut().zip(self.temp_buf.iter()) {
                    *out += sample * vol;
                }
            } else {
                let angle = (stream.pan + 1.0) * std::f32::consts::FRAC_PI_4;
                let gain_l = vol * angle.cos();
                let gain_r = vol * angle.sin();
                let mut i = 0;
                while i + 1 < self.temp_buf.len() && i + 1 < output.len() {
                    output[i]   += self.temp_buf[i]   * gain_l;
                    output[i+1] += self.temp_buf[i+1] * gain_r;
                    i += 2;
                }
            }
        }

        // Log mixed output RMS every ~20 seconds (48000*2 / 256 ≈ 375 calls/s)
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if c % 7500 == 0 && !self.streams.is_empty() {
            let rms: f32 = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
            tracing::debug!(target: "jamodio::mixer", streams = self.streams.len(), rms, "mix_into heartbeat");
        }

        // REC-3 : tap MIX positionné ICI = APRÈS la somme des streams post-fader/pan
        // mais AVANT dim_factor + master_gain + clamp. Sémantique : le fichier MIX
        // enregistré reflète "le mix post-fader des instruments seul", pas mes
        // réglages d'écoute locaux (dim/master). Cohérent avec le tap browser sur
        // `instrumentMixBus` qui est PRE-dim/PRE-master côté Web Audio.
        if self.record_tx.is_some() {
            self.record_send(RecordCmd::PushMix(output.to_vec()));
        }

        // DIM factor — atténue les instruments quand l'user veut entendre le
        // talkback clairement. Skip si == 1.0 (cas par défaut majoritaire).
        if (self.dim_factor - 1.0).abs() > f32::EPSILON {
            let d = self.dim_factor;
            for sample in output.iter_mut() {
                *sample *= d;
            }
        }

        // Master gain global (fader MASTER côté UI). Appliqué AVANT le clamp
        // pour qu'un master à 0.5 atténue proprement un mix qui aurait dépassé
        // 1.0 (le clamp final ramène quand même dans [-1, 1] pour le DAC).
        // Skip multiplication si gain == 1.0 (cas par défaut, économise N muls
        // sur le hot path callback CPAL).
        if (self.master_gain - 1.0).abs() > f32::EPSILON {
            let g = self.master_gain;
            for sample in output.iter_mut() {
                *sample *= g;
            }
        }

        // Soft clamp to prevent distortion
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        // Report drift drains (rate-limité à puissances de 2). Coût formatage
        // négligeable hors événement (1 if + un getter atomic-free par stream).
        self.report_drift_drops();
    }

    /// RMS level per stream (for VU meters sent to browser).
    pub fn stream_rms(&self) -> Vec<(String, f32)> {
        self.streams.iter().map(|(id, stream)| {
            (id.clone(), stream.rms)
        }).collect()
    }

    /// Sprint S6 — purge la fenêtre glissante de drift drains et retourne
    /// les peers REMOTE dont le compte d'events sur la fenêtre dépasse
    /// `threshold`. Self-monitor exclu (= ses drains reflètent overload
    /// agent local, pas un peer distant instable — cf. AgentPipelineOverload).
    ///
    /// Retourne `(producer_id, drift_drains_window, drift_drains_total)`.
    pub fn stream_unstable_events(
        &mut self,
        window: std::time::Duration,
        threshold: usize,
    ) -> Vec<(String, usize, u64)> {
        let now = std::time::Instant::now();
        let cutoff = now.checked_sub(window).unwrap_or(now);
        let mut out = Vec::new();
        for (producer_id, stream) in self.streams.iter_mut() {
            if producer_id.as_str() == SELF_MONITOR_ID {
                continue;
            }
            // Purge les timestamps hors fenêtre (= plus anciens que cutoff).
            while let Some(&front) = stream.drift_drain_history.front() {
                if front < cutoff {
                    stream.drift_drain_history.pop_front();
                } else {
                    break;
                }
            }
            let events_window = stream.drift_drain_history.len();
            if events_window > threshold {
                out.push((
                    producer_id.clone(),
                    events_window,
                    stream.drift_drain_count,
                ));
            }
        }
        out
    }

    /// Sprint S1 — snapshot perf par stream remote (self-monitor exclu).
    /// Retourne (producer_id, underruns_cumul, drift_drops_cumul, target_ms_courant).
    /// Counters monotones depuis la création du stream — le browser fait la
    /// différence entre 2 snapshots s'il veut une cadence par seconde.
    pub fn stream_perf_stats(&self) -> Vec<(String, u64, u64, usize)> {
        self.streams
            .iter()
            .filter(|(k, _)| k.as_str() != SELF_MONITOR_ID)
            .map(|(id, s)| {
                (
                    id.clone(),
                    s.jitter.underruns(),
                    s.jitter.drift_drops(),
                    s.jitter.target_ms(),
                )
            })
            .collect()
    }

    /// Chantier C (v0.4.14) — stats du self-monitor pour diagnostic : latence
    /// courante du buffer (ms) + underruns cumulés. Permet de visualiser que
    /// la latence monitoring grandit transitoirement sous les spikes plugin
    /// (≤ LOCAL_MAX_TARGET_MS) et revient à ~5 ms au calme. (0, 0) si le
    /// self-monitor n'est pas actif (pas de capture en cours).
    pub fn self_monitor_stats(&self) -> (usize, u64) {
        match self.streams.get(SELF_MONITOR_ID) {
            Some(s) => (s.jitter.target_ms(), s.jitter.underruns()),
            None => (0, 0),
        }
    }

    /// Number of active REMOTE streams (self-monitor exclu).
    pub fn stream_count(&self) -> usize {
        self.streams.keys().filter(|k| k.as_str() != SELF_MONITOR_ID).count()
    }

    /// Total underruns aggregated across REMOTE per-stream jitter buffers.
    /// Self-monitor exclu : son ring est alimenté en local (pas de gigue
    /// réseau) → ses éventuels underruns refléteraient un overload CPU
    /// agent, pas un problème côté réseau. À surfacer séparément si besoin.
    pub fn total_underruns(&self) -> u64 {
        self.streams.iter()
            .filter(|(k, _)| k.as_str() != SELF_MONITOR_ID)
            .map(|(_, s)| s.jitter.underruns())
            .sum()
    }

    /// Cible jitter buffer moyenne (ms) sur les streams REMOTE actifs.
    /// Self-monitor exclu (target = 5 ms fixe, fausserait la moyenne).
    /// 0 si pas de stream remote — utilisé comme indicateur de tuning dans l'UI.
    pub fn mean_target_ms(&self) -> f32 {
        let targets: Vec<usize> = self.streams.iter()
            .filter(|(k, _)| k.as_str() != SELF_MONITOR_ID)
            .map(|(_, s)| s.jitter.target_ms())
            .collect();
        if targets.is_empty() {
            return 0.0;
        }
        let sum: usize = targets.iter().sum();
        sum as f32 / targets.len() as f32
    }

    /// Override la target_ms de tous les streams existants ET stocke la valeur
    /// comme défaut pour les futurs streams. Appelé par le handler SetBuffer.
    pub fn set_target_ms_all(&mut self, target_ms: usize) {
        self.default_target_ms = Some(target_ms);
        for stream in self.streams.values_mut() {
            stream.jitter.set_target_ms(target_ms);
        }
    }
}
