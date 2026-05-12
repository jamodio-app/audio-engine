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
    /// Tail conservé au moment d'un drift drain : les `CROSSFADE_SAMPLES`
    /// derniers samples drainés (= ce qui aurait été joué juste avant le
    /// saut). Sur les pulls suivants, on fait un crossfade entre ce tail et
    /// les premiers samples poppés → la discontinuité brutale du drain
    /// devient une rampe douce sur 5 ms (inaudible). Vide hors drain.
    crossfade_tail: Vec<f32>,
    /// Position courante (en samples interleaved) dans `crossfade_tail`.
    /// `crossfade_pos < crossfade_tail.len()` ⇒ un crossfade est en cours.
    crossfade_pos: usize,
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
/// Durée du crossfade appliqué au moment d'un drift drain. 5 ms à 48 kHz
/// stéréo interleaved = 240 frames × 2 canaux = 480 samples. Suffisant pour
/// masquer la discontinuité du drain sans introduire de smear audible sur
/// transients (standard DAW splice point). N'ajoute AUCUNE latence : le
/// crossfade s'applique sur les samples qu'on poppait déjà — la cible du
/// buffer reste `target_samples`.
const CROSSFADE_MS: usize = 5;
const CROSSFADE_SAMPLES: usize = CROSSFADE_MS * SAMPLE_RATE * CHANNELS / 1000;

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
            crossfade_tail: Vec::with_capacity(CROSSFADE_SAMPLES),
            crossfade_pos: 0,
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
        //
        // Crossfade ~5 ms : au lieu de drop sec tous les samples excédentaires
        // (= clic audible), on garde les CROSSFADE_SAMPLES derniers dans
        // `crossfade_tail` et on les fade-out contre le fade-in des nouveaux
        // samples poppés ci-dessous. Pas de latence ajoutée — la cible du
        // buffer reste target_samples après l'opération.
        let drain_threshold = DRIFT_DRAIN_FACTOR * self.target_samples;
        let available = if available > drain_threshold {
            let to_drop = available - self.target_samples;
            let tail_len = CROSSFADE_SAMPLES.min(to_drop);
            let pre_drop = to_drop - tail_len;
            let dropped_pre = self.consumer.skip(pre_drop);
            self.crossfade_tail.resize(tail_len, 0.0);
            let popped_tail = self.consumer.pop_slice(&mut self.crossfade_tail[..]);
            self.crossfade_tail.truncate(popped_tail);
            self.crossfade_pos = 0;
            self.drift_drops += (dropped_pre + popped_tail) as u64;
            self.consumer.occupied_len()
        } else {
            available
        };

        let needed = output.len();
        let pulled = if available >= needed {
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
        };

        // Applique le crossfade en cours sur les premiers samples poppés.
        // Le fade s'étale sur plusieurs pulls si output.len() < tail_len.
        if self.crossfade_pos < self.crossfade_tail.len() {
            let fade_len = self.crossfade_tail.len();
            let remaining = fade_len - self.crossfade_pos;
            let n = remaining.min(output.len());
            let start = self.crossfade_pos;
            let inv_fade = 1.0 / fade_len as f32;
            for (i, (out, &tail)) in output[..n]
                .iter_mut()
                .zip(&self.crossfade_tail[start..start + n])
                .enumerate()
            {
                let t = (start + i) as f32 * inv_fade;
                *out = tail * (1.0 - t) + *out * t;
            }
            self.crossfade_pos += n;
            if self.crossfade_pos >= fade_len {
                self.crossfade_tail.clear();
                self.crossfade_pos = 0;
            }
        }

        pulled
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Plus grand écart entre 2 samples interleaved consécutifs d'un même
    /// canal (= dérivée discrète par canal). Sur un signal continu cette
    /// valeur est bornée par la slope du signal ; une discontinuité brutale
    /// la fait exploser. Mesure aussi le saut entre `prev_tail` (dernier
    /// sample joué juste avant `buf`) et le 1er sample de `buf` pour
    /// détecter une coupure au bord du pull.
    fn max_step_per_channel(buf: &[f32], prev_tail: Option<&[f32]>) -> f32 {
        let mut m = 0.0_f32;
        for ch in 0..CHANNELS {
            let mut prev = if let Some(t) = prev_tail {
                t[t.len() - CHANNELS + ch]
            } else {
                buf[ch]
            };
            let start_frame = if prev_tail.is_some() { 0 } else { 1 };
            for frame_idx in start_frame..(buf.len() / CHANNELS) {
                let s = buf[frame_idx * CHANNELS + ch];
                m = m.max((s - prev).abs());
                prev = s;
            }
        }
        m
    }

    #[test]
    fn drift_drain_no_audible_discontinuity() {
        // Pour qu'un drain SEC produise une discontinuité observable, on
        // push un échelon : grand segment à +1.0 puis segment à -1.0. Le
        // drain va jeter une partie du +1.0 → sans crossfade le pull
        // suivant verra une marche directe +1.0 → −1.0 (step = 2.0). Avec
        // crossfade sur 480 samples interleaved, la transition est lissée
        // (~2.0 / 240 ≈ 0.008 par frame).
        let target_ms = 10;
        let target_samples_local = target_ms * SAMPLE_RATE * CHANNELS / 1000;

        let mut jb = JitterBuffer::new();
        jb.set_target_ms(target_ms);

        // Pré-fill amorce : 1 chunk de +1.0 → primed sur +1.0.
        jb.push(&vec![1.0_f32; target_samples_local]);

        // 1er pull : consomme tout le chunk. Pas de drain (occupied = target).
        let mut warmup = vec![0.0_f32; target_samples_local];
        jb.pull(&mut warmup);
        assert!(warmup.iter().all(|&v| (v - 1.0).abs() < 1e-6));
        assert_eq!(jb.drift_drops(), 0, "pas de drain au pré-pull");

        // Construit un buffer dont la frontière +1/−1 tombe pile entre le
        // tail conservé pour le crossfade et le new_head poppé après :
        //   • pre_drop = 4320 samples skipped dans la zone +1.0
        //   • tail (480 samples) = fin de la zone +1.0
        //   • new_head poppé = début de la zone −1.0 (≥ target)
        // Calcul : pour avoir pre_drop = (5×target − tail_len) avec tail_len
        // = CROSSFADE_SAMPLES = 480, il faut occupied = 5×target + target
        // = 6×target. La zone +1.0 doit faire 5×target pour que tail
        // s'arrête exactement à la frontière.
        jb.push(&vec![1.0_f32; 5 * target_samples_local]);
        jb.push(&vec![-1.0_f32; target_samples_local]);

        // Pull de la taille exacte du crossfade pour rester dans la zone
        // alimentée (target = 960 samples post-drain — un pull plus grand
        // déclencherait un underrun et faussserait la mesure).
        let mut out = vec![0.0_f32; CROSSFADE_SAMPLES];
        jb.pull(&mut out);
        assert!(jb.drift_drops() > 0, "drift drain attendu");

        // Step max entre la fin du warmup et le 2e pull. Sans crossfade
        // ≈ 2.0 (saut +1.0 → −1.0). Avec crossfade ≈ 0.008.
        let max_step = max_step_per_channel(&out, Some(&warmup));
        assert!(
            max_step < 0.20,
            "discontinuité résiduelle trop forte: max_step={max_step}"
        );
    }

    #[test]
    fn drift_drain_counts_all_dropped_samples() {
        // Le crossfade ne doit pas perdre la trace des samples consommés :
        // drift_drops doit refléter exactement (occupied_initial − target),
        // tail conservé pour le fade inclus (consumé pour de bon, pas joué
        // tel quel — mixé en fade-out avec le new_head).
        let target_ms = 10;
        let target_samples_local = target_ms * SAMPLE_RATE * CHANNELS / 1000;

        let mut jb = JitterBuffer::new();
        jb.set_target_ms(target_ms);

        let burst_len = 5 * target_samples_local; // > 3× target ⇒ déclenche
        jb.push(&vec![0.5_f32; burst_len]);

        let mut out = vec![0.0_f32; 256];
        jb.pull(&mut out);

        let expected_drained = burst_len - target_samples_local;
        assert_eq!(
            jb.drift_drops(),
            expected_drained as u64,
            "drift_drops doit compter pre_drop + tail conservé pour le crossfade"
        );
    }
}
