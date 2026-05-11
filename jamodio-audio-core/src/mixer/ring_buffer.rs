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
    /// Nombre cumulé de samples les plus anciens jetés côté `push` quand le
    /// ring est plein (burst SFU + drift d'horloge). Reporting via getter.
    overflow_drops: u64,
    /// Nombre cumulé de samples drainés côté `pull` quand le buffer s'est
    /// rempli durablement bien au-dessus de `target_samples` (drift drain
    /// pré-emptif pour borner la latence post-burst).
    drift_drops: u64,
}

const SAMPLE_RATE: usize = 48000;
const CHANNELS: usize = 2;
const MIN_TARGET_MS: usize = 5;
const MAX_TARGET_MS: usize = 40;
const INITIAL_TARGET_MS: usize = 10;
/// Sprint B — borne max du target quand fixé manuellement via `set_target_ms`
/// (latency-align mode agent : delay = maxHalfRtt − peerHalfRtt, peut atteindre
/// 100+ ms en internet WAN). L'adaptation automatique reste bornée à
/// MAX_TARGET_MS (40 ms) pour ne pas grimper sur underrun, mais le pilotage
/// externe (browser → SetPeerDelay) peut monter plus haut pour aligner les
/// peers sur le plus lent.
const MAX_ALIGN_TARGET_MS: usize = 200;
/// Capacité du ring buffer, en ms d'audio stéréo. Marge confortable au-dessus
/// de MAX_ALIGN_TARGET_MS (200) pour absorber les bursts SFU sans truncation
/// même avec un fort delay d'alignement. Coût RAM : ~115 KB / stream.
const CAPACITY_MS: usize = 300;
/// Seuil hystérèse de drift-drain : si le buffer dépasse `DRIFT_DRAIN_FACTOR
/// × target_samples`, on draine les plus anciens samples pour ramener à
/// target. Borne la latence après un burst (sinon le buffer reste à 80-90 ms
/// indéfiniment sous l'effet de la dérive d'horloge producer↔consumer).
const DRIFT_DRAIN_FACTOR: usize = 3;

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
            overflow_drops: 0,
            drift_drops: 0,
        }
    }

    /// Push decoded PCM samples (interleaved stereo f32).
    ///
    /// Politique d'overflow : si le ring est plein, on jette les samples
    /// LES PLUS ANCIENS (côté consumer) pour faire de la place — pas de
    /// truncation mid-paquet. Sans ça, `push_slice` partial-write coupait
    /// le paquet en deux côté producer → discontinuité PCM mid-paquet =
    /// click numérique audible (Max difference ~0.3 sur 2 samples f32
    /// détecté par ffmpeg astats).
    ///
    /// Le drop-oldest préserve l'audio le plus récent (=> latence minimale)
    /// et la discontinuité tombe entre 2 paquets côté pull, ce qui est
    /// audiblement moins violent qu'une coupure mid-paquet.
    pub fn push(&mut self, samples: &[f32]) {
        let needed = samples.len();
        let vacant = self.producer.vacant_len();
        if vacant < needed {
            let to_drop = needed - vacant;
            let dropped = self.consumer.skip(to_drop);
            self.overflow_drops += dropped as u64;
        }
        self.producer.push_slice(samples);
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
    ///
    /// Drift drain : si le buffer s'est rempli durablement bien au-dessus
    /// de `target_samples` (>= 3× target), on draine les plus anciens
    /// samples pour ramener à target_samples. Sans ça, post-burst SFU ou
    /// drift d'horloge producer→consumer, le buffer peut rester à 80-90 ms
    /// indéfiniment → latence silencieuse 9× la cible + push-overflows
    /// permanents au moindre nouveau jitter. Une seule discontinuité
    /// audible vaut mieux qu'un buffer dégradé en permanence.
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

        // Drift drain (uniquement quand primed → on n'interfère pas avec
        // le pre-fill au démarrage).
        let drain_threshold = DRIFT_DRAIN_FACTOR * self.target_samples;
        let available = if available > drain_threshold {
            let to_drop = available - self.target_samples;
            let dropped = self.consumer.skip(to_drop);
            self.drift_drops += dropped as u64;
            self.consumer.occupied_len()
        } else {
            available
        };

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

    /// Override la cible du buffer (utilisé par les handlers SetBuffer et
    /// SetPeerDelay côté UI). Clamp dans [MIN_TARGET_MS, MAX_ALIGN_TARGET_MS]
    /// — borne supérieure élargie depuis le sprint B pour permettre
    /// l'alignement de latence agent au peer le plus lent (delay 100+ ms en
    /// WAN). L'adaptation automatique reste bornée à MAX_TARGET_MS (40 ms)
    /// pour ne pas grimper sur underrun.
    /// Repasse en `unprimed` pour que le pull attende le nouveau target
    /// avant de reprendre le playout.
    pub fn set_target_ms(&mut self, target_ms: usize) {
        let clamped = target_ms.clamp(MIN_TARGET_MS, MAX_ALIGN_TARGET_MS);
        self.target_samples = clamped * SAMPLE_RATE * CHANNELS / 1000;
        self.last_adapt = std::time::Instant::now();
        self.primed = false;
    }

    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Cumul des samples plus-anciens jetés à `push` quand le ring était plein.
    pub fn overflow_drops(&self) -> u64 {
        self.overflow_drops
    }

    /// Cumul des samples drainés à `pull` quand le buffer dépassait 3× target
    /// (correction de drift / post-burst).
    pub fn drift_drops(&self) -> u64 {
        self.drift_drops
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
