use ringbuf::{HeapRb, traits::{Consumer, Observer, Producer, Split}};

/// Adaptive jitter buffer for one remote audio stream.
pub struct JitterBuffer {
    producer: ringbuf::HeapProd<f32>,
    consumer: ringbuf::HeapCons<f32>,
    /// Cible EFFECTIVE de remplissage (samples) = `clamp(MIN, floor + reactive_extra, cap)`.
    /// Valeur dérivée, recalculée par `recompute_target()` à chaque changement de
    /// `floor_samples` ou `reactive_extra_samples`. Lue par `pull` (pre-fill +
    /// seuil de drift-drain).
    target_samples: usize,
    /// Plancher PRÉDICTIF (samples). En mode auto : piloté par la gigue réseau
    /// mesurée (`observe_jitter`) ≈ `k·gigue + headroom`. En mode manuel / local :
    /// valeur figée par `set_target_ms`. Init = `INITIAL_TARGET_MS` (sûr avant la
    /// 1re mesure de gigue fiable).
    floor_samples: usize,
    /// Marge RÉACTIVE temporaire (samples) ajoutée au plancher : +5 ms à chaque
    /// underrun (`adapt_up`), décroît vers 0 au calme (`adapt_down`). C'est le
    /// FILET de sécurité — il garantit qu'on n'est jamais durablement moins
    /// bufferisé que le comportement réactif historique, quelle que soit la
    /// justesse de l'estimation de gigue.
    reactive_extra_samples: usize,
    /// `true` : le plancher suit la gigue mesurée (réseau). `false` : plancher
    /// figé par l'utilisateur (slider UI) ou le self-monitor — `observe_jitter`
    /// devient alors un no-op (on respecte l'override).
    jitter_auto: bool,
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
    /// Chantier C (v0.4.14) — mode « self-monitor local ».
    ///
    /// Le self-monitor n'a PAS de gigue réseau, mais subit la gigue de
    /// TRAITEMENT (un plugin CPU-lourd comme AmpliTube produit des blocs de
    /// 8–22 ms par à-coups). En mode local, sur underrun on fait un fondu de
    /// sortie + un fondu d'entrée à la reprise (`conceal`) → le trou devient un
    /// bref creux lissé, ZÉRO clic ; et l'adaptation reste bornée à
    /// `LOCAL_MAX_TARGET_MS` (latence plafonnée) puis redescend vers 5 ms dès
    /// le calme. Hors mode local (streams réseau) : comportement inchangé.
    local_mode: bool,
    /// Nombre de samples de fondu d'ENTRÉE restant à appliquer à la reprise
    /// après un trou (concealment). 0 = pas de fondu en cours.
    conceal_fade_in_remaining: usize,
}

const SAMPLE_RATE: usize = 48000;
const CHANNELS: usize = 2;
const MIN_TARGET_MS: usize = 5;
const MAX_TARGET_MS: usize = 40;
/// Plancher auto avant que la gigue mesurée soit fiable (warmup JitterEstimator).
/// Valeur sûre et conservatrice ; `observe_jitter` la fait ensuite descendre/monter.
const INITIAL_TARGET_MS: usize = 10;
/// Phase B — calibration du plancher prédictif : `floor = k·gigue + headroom`.
/// `k = 3` sur la gigue EWMA (RFC 3550, ≈ déviation absolue moyenne) couvre
/// largement la queue de distribution (≈ 3,7σ) ; `headroom` ajoute une marge
/// fixe pour la granularité de mesure. Calibré sur réseaux réels (gigue ethernet
/// mesurée ~0,7–1 ms → plancher ~5 ms ; gigue 3 ms → ~11 ms). Le filet réactif
/// (`reactive_extra`) couvre ce que l'EWMA sous-estime (rafales Wi-Fi).
const JITTER_TARGET_K: f64 = 3.0;
const JITTER_HEADROOM_MS: f64 = 2.5;
/// Capacité du ring buffer, en ms d'audio stéréo. Marge confortable au-dessus
/// de MAX_TARGET_MS (40) pour absorber les bursts SFU sans truncation
/// même quand le buffer est proche de sa cible haute. Coût RAM : ~115 KB / stream.
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
/// Chantier C — plafond d'adaptation du self-monitor en mode local. La latence
/// de monitoring ne dépasse JAMAIS cette valeur (priorité latence absolue).
/// 15 ms = compromis : absorbe la plupart des spikes plugin tout en restant
/// jouable à la guitare. Revient à 5 ms dès le calme.
const LOCAL_MAX_TARGET_MS: usize = 15;
/// Mode local : hold avant de réduire la cible (plus long que le réseau pour
/// éviter d'osciller entre deux spikes plugin espacés).
const LOCAL_ADAPT_DOWN_SECS: u64 = 8;
/// Durée du fondu de concealment (entrée/sortie) autour d'un trou self-monitor.
/// ~2 ms = assez pour tuer le clic, assez court pour rester transparent.
const CONCEAL_FADE_MS: usize = 2;
const CONCEAL_FADE_SAMPLES: usize = CONCEAL_FADE_MS * SAMPLE_RATE * CHANNELS / 1000;

/// Convertit une durée en ms (f64) vers un nombre de samples interleaved stéréo.
fn ms_f64_to_samples(ms: f64) -> usize {
    (ms * (SAMPLE_RATE * CHANNELS) as f64 / 1000.0) as usize
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        let capacity = CAPACITY_MS * SAMPLE_RATE * CHANNELS / 1000;
        let rb = HeapRb::<f32>::new(capacity);
        let (producer, consumer) = rb.split();

        let initial = INITIAL_TARGET_MS * SAMPLE_RATE * CHANNELS / 1000;
        Self {
            producer,
            consumer,
            target_samples: initial,
            floor_samples: initial,
            reactive_extra_samples: 0,
            jitter_auto: true,
            underruns: 0,
            last_adapt: std::time::Instant::now(),
            primed: false,
            overflow_drops: 0,
            drift_drops: 0,
            crossfade_tail: Vec::with_capacity(CROSSFADE_SAMPLES),
            crossfade_pos: 0,
            local_mode: false,
            conceal_fade_in_remaining: 0,
        }
    }

    /// Chantier C — active le mode self-monitor local (concealment des trous +
    /// adaptation bornée à `LOCAL_MAX_TARGET_MS`). Appelé par `add_local_stream`.
    pub fn set_local_mode(&mut self, on: bool) {
        self.local_mode = on;
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
            // Chantier C — mode local : au lieu d'une coupure sèche (clic), on
            // fond la fin du réel vers le silence et on armera un fondu
            // d'entrée à la reprise → le trou (spike plugin) devient un bref
            // creux lissé, ZÉRO craquement. La latence reste inchangée.
            if self.local_mode {
                let n = CONCEAL_FADE_SAMPLES.min(available);
                let start = available - n;
                for (i, s) in output[start..available].iter_mut().enumerate() {
                    *s *= 1.0 - (i as f32 + 1.0) / n.max(1) as f32;
                }
                self.conceal_fade_in_remaining = CONCEAL_FADE_SAMPLES;
            }
            output[available..].fill(0.0);
            self.underruns += 1;
            self.adapt_up();
            self.primed = false;
            available
        };

        // Chantier C — fondu d'ENTRÉE à la reprise après un trou (mode local) :
        // rampe 0→1 sur les premiers samples RÉELS poppés → pas de clic au bord
        // de reprise. On l'applique UNIQUEMENT sur un pull plein (= vraie
        // reprise), jamais sur le pull d'underrun lui-même (dont la tête est
        // l'audio d'AVANT le trou, déjà fondu en sortie). S'étale sur plusieurs
        // pulls si needed < fondu restant.
        if self.local_mode && self.conceal_fade_in_remaining > 0 && pulled == needed {
            let total = CONCEAL_FADE_SAMPLES;
            let n = self.conceal_fade_in_remaining.min(pulled);
            for (i, s) in output[..n].iter_mut().enumerate() {
                let done = total - self.conceal_fade_in_remaining;
                let g = ((done + i) as f32 + 1.0) / total as f32;
                *s *= g.min(1.0);
            }
            self.conceal_fade_in_remaining -= n;
        }

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

    /// Override la cible du buffer (utilisé par le handler SetBuffer côté UI :
    /// slider de tuning manuel du jitter buffer). Clamp dans
    /// [MIN_TARGET_MS, MAX_TARGET_MS] — mêmes bornes que l'adaptation auto.
    /// Repasse en `unprimed` pour que le pull attende le nouveau target
    /// avant de reprendre le playout.
    pub fn set_target_ms(&mut self, target_ms: usize) {
        let clamped = target_ms.clamp(MIN_TARGET_MS, MAX_TARGET_MS);
        // Override manuel (slider UI) ou pin du self-monitor : on fige le
        // plancher sur cette valeur et on COUPE le pilotage par la gigue
        // (`observe_jitter` devient no-op). Le filet réactif reste actif.
        self.jitter_auto = false;
        self.floor_samples = clamped * SAMPLE_RATE * CHANNELS / 1000;
        self.reactive_extra_samples = 0;
        self.recompute_target();
        self.last_adapt = std::time::Instant::now();
        self.primed = false;
    }

    /// Phase B — alimente le plancher prédictif avec la gigue réseau mesurée
    /// (RFC 3550, ms). No-op si la cible est en override manuel (`jitter_auto`
    /// = false) ou si l'estimation n'est pas encore fiable (appelant garde
    /// `JitterEstimator::is_warm()`). Le plancher = `clamp(MIN, k·gigue +
    /// headroom, MAX)` ; le filet réactif s'ajoute par-dessus.
    pub fn observe_jitter(&mut self, jitter_ms: f64) {
        if !self.jitter_auto {
            return;
        }
        let floor_ms =
            (JITTER_TARGET_K * jitter_ms + JITTER_HEADROOM_MS).clamp(MIN_TARGET_MS as f64, MAX_TARGET_MS as f64);
        self.floor_samples = ms_f64_to_samples(floor_ms);
        self.recompute_target();
    }

    /// Recalcule la cible effective = `clamp(MIN, floor + reactive_extra, cap)`.
    /// `cap` = `LOCAL_MAX_TARGET_MS` en mode self-monitor, sinon `MAX_TARGET_MS`.
    fn recompute_target(&mut self) {
        let cap_ms = if self.local_mode { LOCAL_MAX_TARGET_MS } else { MAX_TARGET_MS };
        let min_s = MIN_TARGET_MS * SAMPLE_RATE * CHANNELS / 1000;
        let cap_s = cap_ms * SAMPLE_RATE * CHANNELS / 1000;
        self.target_samples = (self.floor_samples + self.reactive_extra_samples).clamp(min_s, cap_s);
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
        // Chantier C — en mode local la cible est plafonnée à LOCAL_MAX_TARGET_MS
        // (latence de monitoring bornée, priorité absolue). Les streams réseau
        // gardent MAX_TARGET_MS (40 ms).
        let cap_ms = if self.local_mode { LOCAL_MAX_TARGET_MS } else { MAX_TARGET_MS };
        let cap_s = cap_ms * SAMPLE_RATE * CHANNELS / 1000;
        // Borne le filet pour que `floor + extra` ne dépasse jamais le cap :
        // sinon une accumulation sans effet rendrait la redescente lente.
        let max_extra = cap_s.saturating_sub(self.floor_samples);
        self.reactive_extra_samples = (self.reactive_extra_samples + grow).min(max_extra);
        self.recompute_target();
        self.last_adapt = std::time::Instant::now();
    }

    fn adapt_down(&mut self) {
        // Mode local : hold plus long (évite d'osciller entre deux spikes
        // plugin) — mais on redescend bien vers le plancher dès le calme installé.
        let hold = if self.local_mode { LOCAL_ADAPT_DOWN_SECS } else { 5 };
        if self.last_adapt.elapsed().as_secs() >= hold {
            let shrink = 2 * SAMPLE_RATE * CHANNELS / 1000 + SAMPLE_RATE * CHANNELS / 2000;
            // Décroît le filet réactif vers 0 (le plancher prédictif fournit la base).
            self.reactive_extra_samples = self.reactive_extra_samples.saturating_sub(shrink);
            self.recompute_target();
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

    // ─── Chantier C — self-monitor local (concealment + adaptation bornée) ───

    #[test]
    fn local_mode_conceals_underrun_no_click() {
        // En mode local, un underrun ne doit PAS produire de coupure sèche : la
        // fin du signal réel est fondue vers le silence (pas de clic au bord).
        let mut jb = JitterBuffer::new();
        jb.set_local_mode(true);
        jb.set_target_ms(5);
        let t = 5 * SAMPLE_RATE * CHANNELS / 1000;
        // Amorce avec un plein régime +1.0.
        jb.push(&vec![1.0_f32; t]);
        // Tire bien plus que disponible → underrun + concealment.
        let mut out = vec![0.0_f32; t + 4800];
        let pulled = jb.pull(&mut out);
        assert!(pulled > 0 && pulled < out.len(), "underrun partiel attendu");
        // Le dernier sample réel (avant la zone silence) est fondu ≈ 0 → la
        // transition vers le silence est lisse (pas de marche 1.0 → 0).
        assert!(
            out[pulled - 1].abs() < 0.15,
            "fin du réel fondue vers 0, got {}",
            out[pulled - 1]
        );
        assert_eq!(jb.underruns(), 1);
    }

    #[test]
    fn local_mode_fades_in_on_resume() {
        // Après un trou, la reprise est fondue (rampe 0→1) → pas de clic au bord
        // de reprise.
        let mut jb = JitterBuffer::new();
        jb.set_local_mode(true);
        jb.set_target_ms(5);
        let t = 5 * SAMPLE_RATE * CHANNELS / 1000;
        jb.push(&vec![1.0_f32; t]);
        let mut out1 = vec![0.0_f32; t + 4800];
        jb.pull(&mut out1); // underrun → arme le fondu d'entrée + re-prime
        // Reprise : on re-amorce LARGEMENT (l'underrun a fait grandir la cible
        // via adapt_up ; il faut dépasser la nouvelle cible pour re-primer).
        jb.push(&vec![1.0_f32; 4 * t]);
        let mut out2 = vec![0.0_f32; t];
        jb.pull(&mut out2);
        // Le tout premier sample réel est fondu (proche de 0), pas un saut sec.
        assert!(out2[0].abs() < 0.5, "1er sample de reprise fondu, got {}", out2[0]);
        // Un peu plus loin, le signal a retrouvé son niveau plein.
        let later = (CONCEAL_FADE_SAMPLES + 64).min(out2.len() - 1);
        assert!(out2[later].abs() > 0.9, "niveau plein retrouvé après le fondu");
    }

    #[test]
    fn local_mode_adapt_capped_at_local_max() {
        // En mode local, l'adaptation auto est plafonnée à LOCAL_MAX_TARGET_MS
        // (latence de monitoring bornée). Plusieurs cycles prime→underrun ne
        // doivent jamais dépasser ce plafond.
        let mut jb = JitterBuffer::new();
        jb.set_local_mode(true);
        jb.set_target_ms(5);
        for _ in 0..12 {
            let t = jb.target_ms() * SAMPLE_RATE * CHANNELS / 1000;
            jb.push(&vec![0.1_f32; t.max(1)]); // amorce à la cible courante
            let mut big = vec![0.0_f32; t + 9600]; // tire bien plus → underrun
            jb.pull(&mut big);
        }
        assert!(
            jb.target_ms() <= LOCAL_MAX_TARGET_MS,
            "cap local respecté: {} ms",
            jb.target_ms()
        );
        assert!(
            jb.target_ms() > 5,
            "la cible a bien grandi sous underruns répétés: {} ms",
            jb.target_ms()
        );
    }

    // ── Phase B — cible pilotée par la gigue mesurée ───────────────────────

    #[test]
    fn observe_jitter_low_gives_low_target() {
        let mut jb = JitterBuffer::new();
        // gigue 0,7 ms → floor = 3·0,7 + 2,5 = 4,6 → clamp MIN = 5 ms.
        jb.observe_jitter(0.7);
        assert_eq!(jb.target_ms(), 5);
    }

    #[test]
    fn observe_jitter_high_gives_proportional_target() {
        let mut jb = JitterBuffer::new();
        // gigue 5 ms → floor = 3·5 + 2,5 = 17,5 ms.
        jb.observe_jitter(5.0);
        let t = jb.target_ms();
        assert!((16..=18).contains(&t), "target attendu ~17 ms, obtenu {t}");
    }

    #[test]
    fn observe_jitter_clamps_to_max() {
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(100.0); // énorme → borné à MAX_TARGET_MS.
        assert_eq!(jb.target_ms(), MAX_TARGET_MS);
    }

    #[test]
    fn manual_override_disables_jitter_targeting() {
        let mut jb = JitterBuffer::new();
        jb.set_target_ms(20); // slider UI : override manuel.
        jb.observe_jitter(0.5); // doit être ignoré.
        assert_eq!(jb.target_ms(), 20);
    }

    #[test]
    fn underrun_raises_target_above_jitter_floor_then_floor_holds() {
        // Garantie anti-régression : même avec un plancher gigue bas (5 ms),
        // le filet réactif remonte la cible à l'underrun (jamais moins sûr que
        // l'historique).
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(0.7); // plancher → 5 ms.
        assert_eq!(jb.target_ms(), 5);

        let five_ms = 5 * SAMPLE_RATE * CHANNELS / 1000;
        // Prime puis vide → un pull à vide déclenche l'underrun + adapt_up.
        jb.push(&vec![0.1_f32; five_ms]);
        let mut out = vec![0.0_f32; five_ms];
        jb.pull(&mut out); // prime + consomme tout.
        jb.pull(&mut out); // buffer vide → underrun → +5 ms réactif.
        assert!(
            jb.target_ms() > 5,
            "le filet réactif doit remonter la cible: {} ms",
            jb.target_ms()
        );
    }
}
