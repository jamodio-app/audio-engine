use ringbuf::{HeapRb, traits::{Consumer, Observer, Producer, Split}};

/// Adaptive jitter buffer for one remote audio stream.
pub struct JitterBuffer {
    producer: ringbuf::HeapProd<f32>,
    consumer: ringbuf::HeapCons<f32>,
    target_samples: usize,
    underruns: u64,
    last_adapt: std::time::Instant,
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
        }
    }

    /// Push decoded PCM samples (interleaved stereo f32).
    pub fn push(&mut self, samples: &[f32]) -> usize {
        self.producer.push_slice(samples)
    }

    /// Pull samples for playback.
    /// If not enough data, fills remainder with silence and counts an underrun.
    pub fn pull(&mut self, output: &mut [f32]) -> usize {
        let available = self.consumer.occupied_len();
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
            available
        }
    }

    pub fn buffered(&self) -> usize {
        self.consumer.occupied_len()
    }

    pub fn target_ms(&self) -> usize {
        self.target_samples * 1000 / (SAMPLE_RATE * CHANNELS)
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
