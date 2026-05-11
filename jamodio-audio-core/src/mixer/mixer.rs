use super::ring_buffer::JitterBuffer;
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
}

struct StreamState {
    jitter: JitterBuffer,
    volume: f32,
    rms: f32,
    /// Snapshot du `overflow_drops` du jitter au précédent push, pour ne
    /// loguer que sur événement (rate-limited via puissance de 2).
    last_overflow_drops: u64,
    buffer_full_count: u64,
    /// Idem pour le drift drain (pull-side).
    last_drift_drops: u64,
    drift_drain_count: u64,
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            temp_buf: Vec::new(),
            default_target_ms: None,
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
            rms: 0.0,
            last_overflow_drops: 0,
            buffer_full_count: 0,
            last_drift_drops: 0,
            drift_drain_count: 0,
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
        self.streams.insert(SELF_MONITOR_ID.to_string(), StreamState {
            jitter,
            volume: 0.0,
            rms: 0.0,
            last_overflow_drops: 0,
            buffer_full_count: 0,
            last_drift_drops: 0,
            drift_drain_count: 0,
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

    /// Push decoded samples into a stream's jitter buffer.
    ///
    /// Le jitter buffer applique drop-oldest sur overflow (cf. `JitterBuffer::push`).
    /// Ici on rate-limit le warn sur l'INCRÉMENT de `overflow_drops` pour ne
    /// pas spammer (1 stream * 400 pkt/s en burst peut générer beaucoup
    /// d'événements).
    pub fn push_samples(&mut self, producer_id: &str, samples: &[f32]) {
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
                if stream.drift_drain_count == 1 || stream.drift_drain_count.is_power_of_two() {
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
            for (out, &sample) in output.iter_mut().zip(self.temp_buf.iter()) {
                *out += sample * vol;
            }
        }

        // Log mixed output RMS every ~20 seconds (48000*2 / 256 ≈ 375 calls/s)
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if c % 7500 == 0 && !self.streams.is_empty() {
            let rms: f32 = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
            tracing::debug!(target: "jamodio::mixer", streams = self.streams.len(), rms, "mix_into heartbeat");
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
