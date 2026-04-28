use super::ring_buffer::JitterBuffer;
use std::collections::HashMap;

/// Mixes N remote audio streams into a single stereo output.
/// Each stream has its own jitter buffer and volume control.
pub struct AudioMixer {
    streams: HashMap<String, StreamState>,
    /// Buffer de travail réutilisé par mix_into — évite ~400 alloc/s
    /// dans le callback CPAL temps-réel.
    temp_buf: Vec<f32>,
}

struct StreamState {
    jitter: JitterBuffer,
    volume: f32,
    rms: f32,
    buffer_full_count: u64,
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            temp_buf: Vec::new(),
        }
    }

    /// Add a new remote stream.
    pub fn add_stream(&mut self, producer_id: &str) {
        self.streams.insert(producer_id.to_string(), StreamState {
            jitter: JitterBuffer::new(),
            volume: 1.0,
            rms: 0.0,
            buffer_full_count: 0,
        });
    }

    /// Remove a stream.
    pub fn remove_stream(&mut self, producer_id: &str) {
        self.streams.remove(producer_id);
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
    pub fn push_samples(&mut self, producer_id: &str, samples: &[f32]) {
        if let Some(stream) = self.streams.get_mut(producer_id) {
            // Compute RMS of pushed samples
            if !samples.is_empty() {
                let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
                stream.rms = (sum_sq / samples.len() as f32).sqrt();
            }

            let pushed = stream.jitter.push(samples);
            if pushed < samples.len() {
                stream.buffer_full_count += 1;
                if stream.buffer_full_count == 1 || stream.buffer_full_count % 100 == 0 {
                    eprintln!("[MIXER] Buffer full for {} (#{}) — dropped {} samples",
                        &producer_id[..8.min(producer_id.len())], stream.buffer_full_count, samples.len() - pushed);
                }
            }
        } else {
            eprintln!("[MIXER] No stream found for {}", &producer_id[..8.min(producer_id.len())]);
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
            eprintln!("[MIXER] mix_into: {} streams, rms={:.6}", self.streams.len(), rms);
        }

        // Soft clamp to prevent distortion
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }

    /// RMS level per stream (for VU meters sent to browser).
    pub fn stream_rms(&self) -> Vec<(String, f32)> {
        self.streams.iter().map(|(id, stream)| {
            (id.clone(), stream.rms)
        }).collect()
    }

    /// Number of active streams.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }
}
