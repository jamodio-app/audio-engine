use ringbuf::{HeapRb, traits::{Consumer, Observer, Producer, Split}};

/// Adaptive jitter buffer for one remote audio stream.
pub struct JitterBuffer {
    producer: ringbuf::HeapProd<f32>,
    consumer: ringbuf::HeapCons<f32>,
    target_samples: usize,
    underruns: u64,
    last_adapt: std::time::Instant,
    /// Pre-fill gate : on n'autorise le playout qu'une fois `target_samples`
    /// accumulés. Évite le silence au démarrage (CPAL tire avant que le 1er
    /// paquet RTP n'arrive) et la rafale d'underruns après un burst de jitter.
    /// Repasse à false sur underrun → ré-attente d'un buffer plein.
    primed: bool,
}

const SAMPLE_RATE: usize = 48000;
const CHANNELS: usize = 2;
const MIN_TARGET_MS: usize = 5;
const MAX_TARGET_MS: usize = 40;
const INITIAL_TARGET_MS: usize = 10;
const CAPACITY_MS: usize = 100;

impl JitterBuffer {
    pub fn new() -> Self {
        let capacity = CAPACITY_MS * SAMPLE_RATE * CHANNELS / 1000;
        let rb = HeapRb::<f32>::new(capacity);
        let (producer, consumer) = rb.split();

        Self {
            producer,
            consumer,
            target_samples: INITIAL_TARGET_MS * SAMPLE_RATE * CHANNELS / 1000,
            underruns: 0,
            last_adapt: std::time::Instant::now(),
            primed: false,
        }
    }

    /// Push decoded PCM samples (interleaved stereo f32).
    pub fn push(&mut self, samples: &[f32]) -> usize {
        self.producer.push_slice(samples)
    }

    /// Pull samples for playback.
    /// If not enough data, fills remainder with silence and counts an underrun.
    ///
    /// Pre-fill gate : avant de jouer, on attend que le buffer ait accumulé
    /// au moins `target_samples` (état `primed`). Sans ça le callback CPAL
    /// démarre immédiatement à l'init (avant le 1er paquet RTP) et chaque
    /// pull retourne du silence → silence permanent au démarrage. Sur
    /// underrun on repasse à false pour ré-attendre un buffer plein avant
    /// de reprendre le playout.
    pub fn pull(&mut self, output: &mut [f32]) -> usize {
        let available = self.consumer.occupied_len();

        if !self.primed {
            if available >= self.target_samples {
                self.primed = true;
            } else {
                output.fill(0.0);
                return 0;
            }
        }

        let needed = output.len();
        if available >= needed {
            self.consumer.pop_slice(&mut output[..needed]);
            self.adapt_down();
            needed
        } else {
            if available > 0 {
                self.consumer.pop_slice(&mut output[..available]);
            }
            output[available..].fill(0.0);
            self.underruns += 1;
            self.adapt_up();
            self.primed = false;
            available
        }
    }

    pub fn buffered(&self) -> usize {
        self.consumer.occupied_len()
    }

    pub fn target_ms(&self) -> usize {
        self.target_samples * 1000 / (SAMPLE_RATE * CHANNELS)
    }

    /// Override la cible du buffer (utilisé par le handler SetBuffer côté UI).
    /// Clamp dans [MIN_TARGET_MS, MAX_TARGET_MS]. Repasse en `unprimed` pour
    /// que le pull attende le nouveau target avant de reprendre le playout.
    pub fn set_target_ms(&mut self, target_ms: usize) {
        let clamped = target_ms.clamp(MIN_TARGET_MS, MAX_TARGET_MS);
        self.target_samples = clamped * SAMPLE_RATE * CHANNELS / 1000;
        self.last_adapt = std::time::Instant::now();
        self.primed = false;
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    fn adapt_up(&mut self) {
        let grow = 5 * SAMPLE_RATE * CHANNELS / 1000;
        let max = MAX_TARGET_MS * SAMPLE_RATE * CHANNELS / 1000;
        self.target_samples = (self.target_samples + grow).min(max);
        self.last_adapt = std::time::Instant::now();
    }

    fn adapt_down(&mut self) {
        if self.last_adapt.elapsed().as_secs() >= 5 {
            let shrink = 2 * SAMPLE_RATE * CHANNELS / 1000 + SAMPLE_RATE * CHANNELS / 2000;
            let min = MIN_TARGET_MS * SAMPLE_RATE * CHANNELS / 1000;
            self.target_samples = self.target_samples.saturating_sub(shrink).max(min);
            self.last_adapt = std::time::Instant::now();
        }
    }
}
