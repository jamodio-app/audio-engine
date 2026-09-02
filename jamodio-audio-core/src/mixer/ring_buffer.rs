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
    /// Phase C — compensation de drift continue (streams RÉSEAU uniquement).
    /// `false` en mode local (self-monitor : pas de drift réseau).
    resample_enabled: bool,
    /// Vitesse de lecture du flux entrant : input frames consommées par output
    /// frame. ≈ 1,0, piloté par un servo sur le remplissage : > 1 si le buffer
    /// est trop plein (sender plus rapide) → on produit MOINS de samples → le
    /// buffer draine en douceur ; < 1 sinon. Borné à ±`RESAMPLE_MAX_ADJ`
    /// (pitch inaudible). Remplace les drift-drains discrets par un ajustement continu.
    rs_speed: f64,
    /// Position fractionnaire de lecture, continuité inter-push. ∈ [0, 1).
    rs_frac: f64,
    /// Frame d'entrée précédente, pour l'interpolation linéaire (continuité).
    rs_prev: [f32; CHANNELS],
    /// `false` tant que `rs_prev` n'est pas amorcée (1er push, ou reprise après
    /// un trou de playout → on ré-amorce pour éviter une interpolation sur une
    /// frame périmée).
    rs_has_prev: bool,
    /// Buffer de sortie réutilisé du resampler (zéro-alloc en régime).
    rs_scratch: Vec<f32>,
    /// C1 — pression d'underrun en fuite (leaky bucket) pilotant la RÉCUPÉRATION
    /// du filet réactif en mode réseau. Sans horloge murale → déterministe.
    /// Cf. constantes `UNDERRUN_PRESSURE_*` / `REACTIVE_RECOVER_SAMPLES_PER_PULL`.
    underrun_pressure: f32,
    /// C1 — accumulateur fractionnaire de réduction du filet (samples), pour une
    /// descente lisse à `REACTIVE_RECOVER_SAMPLES_PER_PULL` par pull.
    shrink_accum: f32,
    /// P0 (2026-09) — plancher PERSISTANT piloté par le TAUX DE GLITCH (réseau).
    /// Monte à chaque underrun (petit pas), redescend TRÈS lentement au calme
    /// (grow-fast/shrink-slow). Capte les micro-à-coups LOCAUX que le plancher
    /// tail-aware (gigue réseau) ne voit pas → convergence vers le plancher
    /// minimal SANS glitch. `0` = inerte (comportement identique à avant).
    /// Inactif en `local_mode` (self-monitor a son propre chemin borné).
    glitch_floor_samples: usize,
    /// P0 — compteur de pulls calmes consécutifs, pour la décroissance très lente
    /// de `glitch_floor_samples`.
    glitch_calm_pulls: u32,
}

const SAMPLE_RATE: usize = 48000;
const CHANNELS: usize = 2;
const MIN_TARGET_MS: usize = 5;
const MAX_TARGET_MS: usize = 40;
/// Plancher auto avant que la gigue mesurée soit fiable (warmup JitterEstimator).
/// Valeur sûre et conservatrice ; `observe_jitter` la fait ensuite descendre/monter.
const INITIAL_TARGET_MS: usize = 10;
/// Phase B — calibration du plancher prédictif : `floor = k·gigue + headroom`.
/// Chantier #1 (28/06) — l'ancien plancher `k=3·gigue_MOYENNE + 2,5` (Phase B)
/// sous-estimait la queue sur réseau bursty (Wi-Fi/internet) → underrun-puis-réagit.
/// Remplacé par un plancher piloté par la QUEUE de gigue (`jitter_tail_ms`) au lieu
/// de la MOYENNE. La queue étant déjà le pire-cas récent, le multiplicateur est
/// proche de 1 (vs k=3 sur la moyenne) + un petit headroom pour les arrivées
/// tardives consécutives. `floor = clamp(MIN, K_TAIL·queue + TAIL_HEADROOM, MAX)`.
/// Effet : sur lien bursty le plancher couvre la queue PROACTIVEMENT → moins
/// d'underruns, buffer stable plus bas que l'oscillation réactive. Sur lien propre
/// (queue ≈ 0) → plancher ≈ MIN, identique à l'historique (zéro régression).
/// CONSTANTES DE CALIBRATION (à affiner sur lien réel — cf. PLAN-CHANTIER-1-JITTER).
const K_TAIL: f64 = 1.0;
const TAIL_HEADROOM_MS: f64 = 3.0;
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
/// A-lite (2026-08) — plancher au calme du self-monitor local, PLUS BAS que le
/// plancher réseau `MIN_TARGET_MS` (5 ms) : le signal local n'a pas de gigue
/// réseau, seulement la gigue d'ordonnancement des 2 hops de threads (petite).
/// 3 ms = ~2 ms gagnés sur le retour casque vs les 5 ms historiques. L'adaptation
/// vers `LOCAL_MAX_TARGET_MS` reste le filet sur les spikes plugin, avec retour à
/// ce plancher au calme ; un underrun devient un fondu de concealment inaudible
/// (Chantier C), jamais un clic. RÉSEAU intact (le min réseau reste `MIN_TARGET_MS`).
/// CONSTANTE DE CALIBRATION (remonter à 4 si trop de concealments au chant).
const LOCAL_MIN_TARGET_MS: usize = 3;
/// Mode local : hold avant de réduire la cible (plus long que le réseau pour
/// éviter d'osciller entre deux spikes plugin espacés).
const LOCAL_ADAPT_DOWN_SECS: u64 = 8;
/// Durée du fondu de concealment (entrée/sortie) autour d'un trou self-monitor.
/// ~2 ms = assez pour tuer le clic, assez court pour rester transparent.
const CONCEAL_FADE_MS: usize = 2;
const CONCEAL_FADE_SAMPLES: usize = CONCEAL_FADE_MS * SAMPLE_RATE * CHANNELS / 1000;
/// Phase C — gain proportionnel du servo de drift (erreur de remplissage relative
/// → écart de vitesse). Un simple P suffit : la correction stationnaire requise
/// (~7 ppm de drift) est minuscule, l'erreur résiduelle de remplissage est < 1 frame.
const RESAMPLE_KP: f64 = 0.01;
/// Borne dure du ratio : ±0,5 % = ~8 cents en transitoire extrême, inaudible.
/// Le drift réel (~7 ppm = 0,0007 %) laisse une marge de sécurité énorme ;
/// le clamp protège contre un emballement du servo sur un burst.
const RESAMPLE_MAX_ADJ: f64 = 0.005;
/// Slew-rate du ratio par push (~400/s) : 2e-5/push ⇒ ~0,008/s. Le ratio bouge
/// lentement (faible bande passante, façon DLL) → aucun wobble de hauteur audible.
const RESAMPLE_SLEW_PER_PUSH: f64 = 0.00002;

// ── C1 (2026-08) — RÉCUPÉRATION du filet réactif (streams RÉSEAU) ───────────
// Le filet réactif (`adapt_up`, +5 ms/underrun) protège contre les trous ; il
// reste INCHANGÉ. Mais l'ancienne récupération (`adapt_down`, palier 5 s ré-armé
// par CHAQUE underrun, −2,5 ms/5 s) était trop lente ET bloquée par un underrun
// isolé → sur lien jittery (WiFi) le buffer restait coincé haut (mesuré 17–40 ms
// alors que la gigue réelle justifiait 6–9 ms — cf.
// `studies/ETUDE-LATENCE-MONITOR-EMISSION-2026-08.md` §9ter).
//
// C1 remplace ce gating horloge (RÉSEAU uniquement) par une PRESSION D'UNDERRUN
// EN FUITE (leaky bucket, sans horloge murale → déterministe et testable) : tant
// que la pression dépasse le seuil, on tient le filet (protection identique) ;
// sous le seuil (calme), on draine le filet vers 0 à vitesse bornée → retour au
// plancher tail-aware. Un underrun ISOLÉ ne bloque plus la récupération (la
// pression retombe seule), une CADENCE d'underruns la maintient (buffer tenu).
// Le self-monitor local (`local_mode`) garde son adaptation bornée historique.
//
/// Pression ajoutée par underrun.
const UNDERRUN_PRESSURE_STEP: f32 = 1.0;
/// Fuite de la pression par pull « plein ». **P1 (calibration réelle 0.5.12-1) :**
/// la première valeur (0.990, demi-vie ~90 ms) récupérait TROP vite sur WiFi — le
/// buffer retombait au plancher puis se faisait cueillir par un pic WiFi → hausse
/// des underruns (mesuré Mac 116 / PC 342 sur une session). On ralentit la fuite
/// pour donner une MÉMOIRE d'underrun de plusieurs secondes (grâce implicite) : à
/// ~750 pull/s (buffer 64), 0.9995 ⇒ demi-vie ~1,85 s ; un underrun isolé retombe
/// sous le seuil en ~1,8 s, une rafale de 4 en ~5,5 s. On ne récupère donc qu'après
/// un calme RÉELLEMENT installé. CONSTANTE DE CALIBRATION (à re-mesurer WiFi/Ethernet).
const UNDERRUN_PRESSURE_LEAK: f32 = 0.9995;
/// Seuil « calme » : sous cette pression, on récupère (draine le filet réactif).
const UNDERRUN_PRESSURE_CALM: f32 = 0.5;
/// Plafond de pression : borne la MÉMOIRE d'underrun. Sans lui, une longue passe
/// jittery ferait grimper la pression sans limite → le buffer resterait tenu très
/// longtemps après le retour au calme. À 4,0 (avec `UNDERRUN_PRESSURE_LEAK`), la
/// récupération démarre au plus tard ~5,5 s après le DERNIER underrun, même après
/// une grosse rafale. CONSTANTE DE CALIBRATION.
const UNDERRUN_PRESSURE_MAX: f32 = 4.0;
/// Vitesse de récupération du filet réactif au calme (samples interleaved/pull).
/// **P1 :** abaissé de 1,0 à 0,5 (drainage plus doux) — ~375 samples/s à 750 pull/s
/// ≈ 3,9 ms/s → un overshoot de ~30 ms revient au plancher en ~8 s. Descente
/// graduelle, jamais de saut sec. CONSTANTE DE CALIBRATION.
const REACTIVE_RECOVER_SAMPLES_PER_PULL: f32 = 0.5;

// ── P0 (2026-09) — plancher piloté par le TAUX DE GLITCH (réseau) ───────────
// Le plancher tail-aware ne couvre que la gigue RÉSEAU. Sur lien propre, il
// tombe à MIN (5 ms) et underrunne sur des micro-à-coups LOCAUX (recv_path 2 ms,
// ordonnancement) — mesuré ~2/min sur Ethernet en 0.5.12-5. Le glitch_floor
// ajoute un headroom PERSISTANT piloté par les underruns réels : grow-fast à
// chaque glitch, shrink-slow au calme → converge vers le plancher minimal qui
// tient ZÉRO glitch. Additif (glitch-free ⇒ inerte).
/// Pas de croissance par underrun.
const GLITCH_FLOOR_STEP_MS: usize = 1;
/// Plafond (bien sous MAX 40 ms : headroom suffisant sans exploser la latence).
const GLITCH_FLOOR_MAX_MS: usize = 20;
/// Pulls calmes consécutifs avant de décrémenter le plancher d'UN sample. Très
/// lent : à ~750 pull/s, 500 ⇒ ~5 ms récupérés en ~5 min de calme TOTAL (un
/// underrun remet le compteur à zéro). CONSTANTE DE CALIBRATION.
const GLITCH_FLOOR_DECAY_CALM_PULLS: u32 = 500;

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
            resample_enabled: true,
            rs_speed: 1.0,
            rs_frac: 0.0,
            rs_prev: [0.0; CHANNELS],
            rs_has_prev: false,
            rs_scratch: Vec::with_capacity(2048),
            underrun_pressure: 0.0,
            shrink_accum: 0.0,
            glitch_floor_samples: 0,
            glitch_calm_pulls: 0,
        }
    }

    /// P1 (01/07) — repart PROPRE après un rétablissement audio (reset ASIO).
    /// Pendant le gel de sortie, le décodage continue de pousser → le ring se
    /// remplit jusqu'à `CAPACITY_MS` (300 ms) de samples PÉRIMÉS. À la reprise, le
    /// drift-drain finit par les évacuer, mais on force ici un départ net et
    /// déterministe : on VIDE le périmé et on re-prime à la cible de démarrage. La
    /// continuité audio est de toute façon rompue par le trou du reset → aucun
    /// artefact ajouté, et on évite de rejouer jusqu'à 300 ms de retard.
    ///
    /// Ne touche NI au mode (`local_mode`/`jitter_auto`) NI aux compteurs cumulés
    /// (`underruns`/`overflow_drops`/`drift_drops` = télémétrie de session). Réseau
    /// ET self-monitor : sûr pour les deux (le self-monitor re-prime sur la capture
    /// qui vient de repartir).
    pub fn reset_for_recovery(&mut self) {
        // Vide les samples périmés accumulés pendant le gel de sortie.
        let occupied = self.consumer.occupied_len();
        self.consumer.skip(occupied);
        // Re-prime propre + cible de démarrage (le filet réactif repart de 0).
        let initial = INITIAL_TARGET_MS * SAMPLE_RATE * CHANNELS / 1000;
        self.primed = false;
        self.reactive_extra_samples = 0;
        self.underrun_pressure = 0.0;
        self.shrink_accum = 0.0;
        self.glitch_floor_samples = 0;
        self.glitch_calm_pulls = 0;
        self.floor_samples = initial;
        self.target_samples = initial;
        self.last_adapt = std::time::Instant::now();
        // Continuité du resampler (Phase C) et du crossfade : repart à neuf.
        self.rs_speed = 1.0;
        self.rs_frac = 0.0;
        self.rs_has_prev = false;
        self.crossfade_tail.clear();
        self.crossfade_pos = 0;
        self.conceal_fade_in_remaining = 0;
    }

    /// Chantier C — active le mode self-monitor local (concealment des trous +
    /// adaptation bornée à `LOCAL_MAX_TARGET_MS`). Appelé par `add_local_stream`.
    pub fn set_local_mode(&mut self, on: bool) {
        self.local_mode = on;
        // Le self-monitor local n'a pas de drift réseau → pas de resampling Phase C.
        self.resample_enabled = !on;
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
        // Phase C — en régime établi (primed) et pour les streams réseau, on
        // resample le flux entrant en continu pour tenir le remplissage sur la
        // cible (compensation de drift d'horloge sender↔nous). Hors régime
        // (pre-fill / reprise) ou self-monitor : push direct, chemin historique.
        if self.resample_enabled && self.primed {
            self.update_resample_speed();
            // `scratch` sorti du `self` (mem::take préserve la capacité) pour
            // satisfaire l'emprunteur : resample_into emprunte `self` en mut.
            let mut scratch = std::mem::take(&mut self.rs_scratch);
            scratch.clear();
            self.resample_into(samples, &mut scratch);
            self.push_to_ring(&scratch);
            self.rs_scratch = scratch; // restitué (capacité conservée → zéro-alloc).
        } else {
            self.push_to_ring(samples);
        }
    }

    /// Écrit `data` dans le ring avec politique drop-oldest sur overflow.
    fn push_to_ring(&mut self, data: &[f32]) {
        let needed = data.len();
        let vacant = self.producer.vacant_len();
        if vacant < needed {
            let to_drop = needed - vacant;
            let dropped = self.consumer.skip(to_drop);
            self.overflow_drops += dropped as u64;
        }
        self.producer.push_slice(data);
    }

    /// Servo de drift (Phase C) : ajuste `rs_speed` pour ramener le remplissage
    /// vers `target_samples`. Proportionnel + slew-rate limité + clamp dur.
    fn update_resample_speed(&mut self) {
        let fill = self.consumer.occupied_len() as f64;
        let target = self.target_samples.max(1) as f64;
        let err = (fill - target) / target; // > 0 : trop plein → accélérer (drainer).
        let speed_target =
            (1.0 + RESAMPLE_KP * err).clamp(1.0 - RESAMPLE_MAX_ADJ, 1.0 + RESAMPLE_MAX_ADJ);
        let delta = (speed_target - self.rs_speed).clamp(-RESAMPLE_SLEW_PER_PUSH, RESAMPLE_SLEW_PER_PUSH);
        self.rs_speed = (self.rs_speed + delta).clamp(1.0 - RESAMPLE_MAX_ADJ, 1.0 + RESAMPLE_MAX_ADJ);
    }

    /// Resampler linéaire streaming : lit `input` (stéréo interleaved) à la
    /// vitesse `rs_speed` et écrit ~`in_frames / rs_speed` frames dans `out`.
    /// État (`rs_prev`, `rs_frac`, `rs_has_prev`) assure la continuité entre
    /// pushes. Interpolation linéaire = transparente à un ratio ≈ 1 (≤ 0,5 %).
    fn resample_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let in_frames = input.len() / CHANNELS;
        if in_frames == 0 {
            return;
        }
        let speed = self.rs_speed;
        let mut i = 0; // prochaine frame d'entrée à charger comme "cur".
        if !self.rs_has_prev {
            // Amorçage : la 1re frame devient `prev`, on interpole à partir d'elle.
            self.rs_prev.copy_from_slice(&input[0..CHANNELS]);
            self.rs_has_prev = true;
            self.rs_frac = 0.0;
            i = 1;
        }
        while i < in_frames {
            let base = i * CHANNELS;
            // Produit des outputs tant que la position fractionnaire est entre
            // `prev` (frac 0) et la frame courante `input[i]` (frac 1).
            while self.rs_frac < 1.0 {
                let f = self.rs_frac as f32;
                for c in 0..CHANNELS {
                    let prev = self.rs_prev[c];
                    let cur = input[base + c];
                    out.push(prev + (cur - prev) * f);
                }
                self.rs_frac += speed;
            }
            self.rs_frac -= 1.0;
            self.rs_prev.copy_from_slice(&input[base..base + CHANNELS]);
            i += 1;
        }
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
            // C1 — audio qui coule normalement : fuite de la pression + éventuelle
            // récupération du filet réactif (réseau) ; self-monitor local inchangé.
            self.recover_after_full_pull();
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
            // C1 — un underrun pousse la pression (bornée) : tant qu'elle reste
            // au-dessus du seuil, la récupération du filet est suspendue (buffer tenu).
            self.underrun_pressure =
                (self.underrun_pressure + UNDERRUN_PRESSURE_STEP).min(UNDERRUN_PRESSURE_MAX);
            self.primed = false;
            // Phase C — un trou de playout casse la continuité d'entrée : on
            // ré-amorce le resampler (sinon interpolation sur une frame périmée).
            self.rs_has_prev = false;
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
        // A-lite : le self-monitor local peut descendre sous le plancher réseau
        // (`MIN_TARGET_MS`) jusqu'à `LOCAL_MIN_TARGET_MS`. Appelé APRÈS
        // `set_local_mode` pour le self-monitor (cf. `add_local_stream`).
        let min_ms = if self.local_mode { LOCAL_MIN_TARGET_MS } else { MIN_TARGET_MS };
        let clamped = target_ms.clamp(min_ms, MAX_TARGET_MS);
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

    /// Chantier #1 (ex-Phase B) — alimente le plancher prédictif avec la **QUEUE**
    /// de gigue réseau mesurée (`jitter_tail_ms`, pire-cas récent) au lieu de la
    /// moyenne. No-op si override manuel (`jitter_auto = false`) ou si l'estimation
    /// n'est pas fiable (appelant garde `JitterEstimator::is_warm()`). Plancher =
    /// `clamp(MIN, K_TAIL·queue + TAIL_HEADROOM, MAX)` ; le filet réactif s'ajoute
    /// par-dessus (backstop CONSERVÉ). Réseau uniquement — le self-monitor local
    /// (`local_mode`) n'appelle pas ce chemin (cible pilotée par `set_target_ms`).
    pub fn observe_jitter(&mut self, jitter_tail_ms: f64) {
        if !self.jitter_auto {
            return;
        }
        let floor_ms = (K_TAIL * jitter_tail_ms + TAIL_HEADROOM_MS)
            .clamp(MIN_TARGET_MS as f64, MAX_TARGET_MS as f64);
        self.floor_samples = ms_f64_to_samples(floor_ms);
        self.recompute_target();
    }

    /// Recalcule la cible effective = `clamp(MIN, floor + reactive_extra, cap)`.
    /// `cap` = `LOCAL_MAX_TARGET_MS` en mode self-monitor, sinon `MAX_TARGET_MS`.
    fn recompute_target(&mut self) {
        let cap_ms = if self.local_mode { LOCAL_MAX_TARGET_MS } else { MAX_TARGET_MS };
        let min_ms = if self.local_mode { LOCAL_MIN_TARGET_MS } else { MIN_TARGET_MS };
        let min_s = min_ms * SAMPLE_RATE * CHANNELS / 1000;
        let cap_s = cap_ms * SAMPLE_RATE * CHANNELS / 1000;
        // P0 — `glitch_floor_samples` = headroom persistant piloté par le glitch
        // (0 en local_mode / glitch-free → identique à avant).
        self.target_samples = (self.floor_samples + self.glitch_floor_samples + self.reactive_extra_samples)
            .clamp(min_s, cap_s);
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

    /// Phase C — vitesse de resampling courante (≈ 1,0). L'écart à 1,0 reflète
    /// la dérive d'horloge compensée en continu. `(speed - 1)·1e6` ≈ ppm corrigés.
    pub fn resample_speed(&self) -> f64 {
        self.rs_speed
    }

    fn adapt_up(&mut self) {
        let grow = 5 * SAMPLE_RATE * CHANNELS / 1000;
        // Chantier C — en mode local la cible est plafonnée à LOCAL_MAX_TARGET_MS
        // (latence de monitoring bornée, priorité absolue). Les streams réseau
        // gardent MAX_TARGET_MS (40 ms).
        let cap_ms = if self.local_mode { LOCAL_MAX_TARGET_MS } else { MAX_TARGET_MS };
        let cap_s = cap_ms * SAMPLE_RATE * CHANNELS / 1000;
        // P0 — chaque underrun remonte le plancher PERSISTANT (réseau uniquement),
        // borné, et casse le calme. Capte les micro-à-coups locaux invisibles au
        // plancher tail-aware. Le self-monitor local n'est pas concerné.
        if !self.local_mode {
            let min_s = MIN_TARGET_MS * SAMPLE_RATE * CHANNELS / 1000;
            let gf_cap = (GLITCH_FLOOR_MAX_MS * SAMPLE_RATE * CHANNELS / 1000)
                .min(cap_s.saturating_sub(min_s));
            let step = GLITCH_FLOOR_STEP_MS * SAMPLE_RATE * CHANNELS / 1000;
            self.glitch_floor_samples = (self.glitch_floor_samples + step).min(gf_cap);
            self.glitch_calm_pulls = 0;
        }
        // Borne le filet pour que `floor + glitch_floor + extra` ne dépasse jamais
        // le cap : sinon une accumulation sans effet rendrait la redescente lente.
        let max_extra = cap_s.saturating_sub(self.floor_samples + self.glitch_floor_samples);
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

    /// C1 — appelé sur chaque pull PLEIN (audio qui coule normalement). Fait
    /// fuir la pression d'underrun, puis — en mode RÉSEAU et si c'est calme —
    /// draine le filet réactif vers le plancher tail-aware à vitesse bornée.
    ///
    /// Additif + backstop : la CROISSANCE (`adapt_up`) est inchangée ; on ne fait
    /// qu'améliorer la DESCENTE. Pire cas (pression toujours au-dessus du seuil) =
    /// aucune récupération = comportement d'avant. Le self-monitor local
    /// (`local_mode`) garde son adaptation temporelle historique (`adapt_down`)
    /// — INTOUCHÉ (scope réseau uniquement).
    fn recover_after_full_pull(&mut self) {
        self.underrun_pressure *= UNDERRUN_PRESSURE_LEAK;
        if self.local_mode {
            self.adapt_down();
            return;
        }
        // Encore des underruns récents → on tient le filet (protection).
        if self.underrun_pressure >= UNDERRUN_PRESSURE_CALM {
            return;
        }
        // P0 — calme installé : décroissance TRÈS lente du plancher de glitch
        // (grow-fast/shrink-slow). Se fait MÊME si le filet réactif est déjà à 0
        // (c'est justement là qu'on veut relâcher le headroom persistant).
        if self.glitch_floor_samples > 0 {
            self.glitch_calm_pulls = self.glitch_calm_pulls.saturating_add(1);
            if self.glitch_calm_pulls >= GLITCH_FLOOR_DECAY_CALM_PULLS {
                self.glitch_floor_samples = self.glitch_floor_samples.saturating_sub(1);
                self.glitch_calm_pulls = 0;
                self.recompute_target();
            }
        }
        // Déjà au plancher : rien à drainer, on repart d'un accumulateur propre.
        if self.reactive_extra_samples == 0 {
            self.shrink_accum = 0.0;
            return;
        }
        // Calme installé : draine le filet réactif (descente lisse < 1 sample/pull
        // possible via l'accumulateur). Le plancher tail-aware reste le minimum
        // (on ne touche jamais `floor_samples` ici).
        self.shrink_accum += REACTIVE_RECOVER_SAMPLES_PER_PULL;
        if self.shrink_accum >= 1.0 {
            let dec = self.shrink_accum as usize;
            self.reactive_extra_samples = self.reactive_extra_samples.saturating_sub(dec);
            self.shrink_accum -= dec as f32;
            self.recompute_target();
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

    #[test]
    fn reset_for_recovery_flushes_and_reprimes() {
        let mut jb = JitterBuffer::new();
        // Simule le gel de sortie : cible gonflée + ring rempli de périmé.
        jb.observe_jitter(30.0); // floor ~33 ms
        let big = vec![0.2_f32; 40 * SAMPLE_RATE * CHANNELS / 1000]; // ~40 ms
        jb.push(&big);
        // Rétablissement.
        jb.reset_for_recovery();
        // Cible revenue au démarrage (filet réactif purgé).
        assert_eq!(jb.target_ms(), INITIAL_TARGET_MS, "cible ré-initialisée");
        // Ring vidé + non primé : un pull rend du silence tant que la cible n'est
        // pas ré-accumulée (pas de rejeu du retard périmé).
        let mut out = vec![1.0_f32; 256];
        let n = jb.pull(&mut out);
        assert_eq!(n, 0, "non primé après reset → silence");
        assert!(out.iter().all(|&s| s == 0.0), "sortie silence après reset");
    }

    // ── Chantier #1 — cible pilotée par la QUEUE de gigue (tail-aware) ─────
    // `observe_jitter` reçoit désormais la gigue de QUEUE (jitter_tail_ms).
    // floor = clamp(MIN, K_TAIL·queue + TAIL_HEADROOM, MAX) = clamp(5, queue+3, 40).

    #[test]
    fn observe_jitter_low_gives_low_target() {
        let mut jb = JitterBuffer::new();
        // queue 0,7 ms → floor = 1·0,7 + 3 = 3,7 → clamp MIN = 5 ms.
        jb.observe_jitter(0.7);
        assert_eq!(jb.target_ms(), 5);
    }

    #[test]
    fn observe_jitter_high_gives_proportional_target() {
        let mut jb = JitterBuffer::new();
        // queue 12 ms (rafale) → floor = 1·12 + 3 = 15 ms → couvre la queue.
        jb.observe_jitter(12.0);
        let t = jb.target_ms();
        assert!((14..=16).contains(&t), "target attendu ~15 ms, obtenu {t}");
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

    // ── Phase C — compensation de drift continue (resampler + servo) ───────

    #[test]
    fn resample_speed_one_is_near_identity() {
        let mut jb = JitterBuffer::new();
        let input: Vec<f32> = (0..240).map(|i| i as f32).collect(); // 120 frames.
        let mut out = Vec::new();
        jb.resample_into(&input, &mut out); // rs_speed = 1.0 par défaut.
        let out_frames = out.len() / CHANNELS;
        assert!((118..=121).contains(&out_frames), "out_frames={out_frames}");
    }

    #[test]
    fn servo_speeds_up_when_overfull_and_slows_when_underfull() {
        let mut jb = JitterBuffer::new();
        jb.set_target_ms(10);
        let target = jb.target_samples;
        // Trop plein (3× la cible) → le servo doit accélérer (drainer).
        jb.push_to_ring(&vec![0.05_f32; target * 3]);
        for _ in 0..1000 {
            jb.update_resample_speed();
        }
        assert!(jb.resample_speed() > 1.0, "trop plein → speed>1: {}", jb.resample_speed());
        // Vide bien en dessous de la cible → le servo doit ralentir (remplir).
        let mut sink = vec![0.0_f32; target * 3];
        let _ = jb.consumer.pop_slice(&mut sink);
        for _ in 0..2000 {
            jb.update_resample_speed();
        }
        assert!(jb.resample_speed() < 1.0, "sous-rempli → speed<1: {}", jb.resample_speed());
    }

    #[test]
    fn servo_speed_is_hard_clamped() {
        let mut jb = JitterBuffer::new();
        jb.set_target_ms(5);
        jb.push_to_ring(&vec![0.05_f32; jb.target_samples * 10]); // excès énorme.
        for _ in 0..100_000 {
            jb.update_resample_speed();
        }
        assert!(jb.resample_speed() <= 1.0 + RESAMPLE_MAX_ADJ + 1e-9);
        assert!(jb.resample_speed() >= 1.0 - RESAMPLE_MAX_ADJ - 1e-9);
    }

    #[test]
    fn resampler_output_count_tracks_speed() {
        // À speed > 1, on produit MOINS de frames qu'on en reçoit → le buffer
        // draine en douceur (compensation d'un sender plus rapide).
        let mut jb = JitterBuffer::new();
        jb.rs_speed = 1.0 + RESAMPLE_MAX_ADJ; // +0,5 %.
        let input = vec![0.1_f32; 2000 * CHANNELS];
        let mut out = Vec::new();
        for _ in 0..10 {
            jb.resample_into(&input, &mut out);
        }
        let out_frames = out.len() / CHANNELS;
        let in_frames = 10 * 2000;
        assert!(out_frames < in_frames, "speed>1 doit réduire: {out_frames}/{in_frames}");
        let ratio = out_frames as f64 / in_frames as f64;
        assert!(
            (ratio - 1.0 / (1.0 + RESAMPLE_MAX_ADJ)).abs() < 0.001,
            "ratio sortie/entrée = {ratio}"
        );
    }

    // ─── P0 — plancher piloté par le taux de glitch ─────────────────────────

    #[test]
    fn glitch_floor_stays_zero_without_underrun() {
        // Additif : sans underrun, le plancher de glitch reste 0 → cible = plancher
        // tail (comportement STRICTEMENT identique à avant P0).
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(0.7); // plancher tail = 5 ms
        drive_full_pulls(&mut jb, 240, 3000); // calme, jamais d'underrun
        assert_eq!(jb.glitch_floor_samples, 0, "aucun glitch → plancher inerte");
        assert_eq!(jb.target_ms(), 5, "cible = plancher tail (comportement d'avant)");
    }

    #[test]
    fn glitch_floor_grows_on_underruns_and_persists_after_reactive_recovery() {
        // P0 : des underruns montent le plancher PERSISTANT ; après que le filet
        // réactif (C1) soit drainé au calme, la cible reste AU-DESSUS du plancher
        // tail — c'est ce qui tue les micro-à-coups locaux (contrairement au pur C1).
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(0.7); // plancher tail = 5 ms
        for _ in 0..4 {
            network_underrun_once(&mut jb);      // +1 ms glitch_floor + filet réactif
            drive_full_pulls(&mut jb, 240, 2000); // calme : draine le réactif, PAS le glitch_floor
        }
        assert!(jb.glitch_floor_samples > 0, "plancher de glitch relevé");
        assert!(jb.target_ms() > 5, "P0 : cible tenue au-dessus du plancher tail: {} ms", jb.target_ms());
    }

    #[test]
    fn glitch_floor_inert_in_local_mode() {
        // Le self-monitor local (local_mode) garde son chemin borné : le plancher
        // de glitch reste inerte même sous underrun.
        let mut jb = JitterBuffer::new();
        jb.set_local_mode(true);
        jb.set_target_ms(3);
        let three = 3 * SAMPLE_RATE * CHANNELS / 1000;
        jb.push(&vec![0.1_f32; three]);
        jb.pull(&mut vec![0.0_f32; three]);
        jb.pull(&mut vec![0.0_f32; three * 4]); // underrun
        assert_eq!(jb.glitch_floor_samples, 0, "local : glitch_floor inerte");
    }

    #[test]
    fn local_mode_disables_resampling() {
        let mut jb = JitterBuffer::new();
        assert!(jb.resample_enabled, "réseau : resampling actif par défaut");
        jb.set_local_mode(true);
        assert!(!jb.resample_enabled, "self-monitor : resampling désactivé");
    }

    // ─── C1 — récupération du filet réactif (réseau) ─────────────────────────

    /// Amène un stream réseau à un underrun (le filet réactif remonte la cible
    /// au-dessus du plancher). Retourne la cible (ms) juste après l'underrun.
    fn network_underrun_once(jb: &mut JitterBuffer) -> usize {
        let five_ms = 5 * SAMPLE_RATE * CHANNELS / 1000;
        jb.push(&vec![0.1_f32; five_ms]);
        let mut out = vec![0.0_f32; five_ms];
        jb.pull(&mut out); // prime + consomme tout
        jb.pull(&mut out); // buffer vide → underrun → adapt_up + pression
        jb.target_ms()
    }

    /// Fait `n` pulls PLEINS de `len` samples en gardant le ring alimenté (pousse
    /// un peu plus qu'on ne tire ; le drift-drain borne le haut). Aucun underrun.
    fn drive_full_pulls(jb: &mut JitterBuffer, len: usize, n: usize) {
        let mut out = vec![0.0_f32; len];
        let feed = vec![0.1_f32; len + len / 4];
        for _ in 0..n {
            jb.push(&feed);
            jb.pull(&mut out);
        }
    }

    #[test]
    fn network_reactive_recovers_to_floor_when_calm() {
        // Cœur de C1 : après un underrun (filet réactif remonté), une période
        // CALME (pulls pleins, plus d'underrun) doit ramener la cible au plancher
        // tail-aware — au lieu de rester coincée haut comme avant.
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(0.7); // plancher réseau = 5 ms
        let t_after = network_underrun_once(&mut jb);
        assert!(t_after > 5, "le filet doit remonter la cible: {t_after} ms");

        // Calme prolongé : on tire jusqu'au retour au plancher (borné, robuste à
        // la calibration des constantes de récupération).
        let mut guard = 0;
        while jb.target_ms() > 5 && guard < 400_000 {
            drive_full_pulls(&mut jb, 240, 500);
            guard += 500;
        }
        assert_eq!(jb.target_ms(), 5, "revenu au plancher après le calme: {} ms", jb.target_ms());
    }

    #[test]
    fn network_reactive_held_right_after_underrun() {
        // Backstop : un underrun TRÈS récent (pression au-dessus du seuil) NE doit
        // PAS déclencher de récupération prématurée — le filet tient (protection).
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(0.7);
        let t_after = network_underrun_once(&mut jb);

        // Un seul pull plein juste après : pression ≈ 0.99 > seuil → cible inchangée.
        jb.push(&vec![0.1_f32; 60 * SAMPLE_RATE * CHANNELS / 1000]); // re-prime large
        let mut out = vec![0.0_f32; 240];
        jb.pull(&mut out);
        assert_eq!(jb.target_ms(), t_after, "pas de récupération sous pression: {} ms", jb.target_ms());
    }

    #[test]
    fn network_reactive_recovery_is_bounded_per_pull() {
        // La descente est BORNÉE (≤ quelques samples/pull) : pas de saut sec de
        // cible. La récupération est graduelle, pilotée par
        // `REACTIVE_RECOVER_SAMPLES_PER_PULL` — contraste avec un reset brutal.
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(0.7);
        let floor_samples = 5 * SAMPLE_RATE * CHANNELS / 1000;
        // Pompe le filet par plusieurs underruns francs.
        for _ in 0..4 {
            jb.push(&vec![0.1_f32; jb.target_samples + 1]);
            let t = jb.target_samples;
            jb.pull(&mut vec![0.0_f32; t]);     // consomme (plein)
            jb.pull(&mut vec![0.0_f32; t * 4]); // underrun franc
        }
        let peak = jb.target_samples;
        assert!(peak > floor_samples, "filet pompé au-dessus du plancher");

        // Tire jusqu'à ce que la récupération DÉMARRE (target < peak), borné —
        // robuste à la calibration (fuite de pression plus ou moins lente).
        let mut guard = 0;
        while jb.target_samples >= peak && guard < 400_000 {
            drive_full_pulls(&mut jb, 240, 500);
            guard += 500;
        }
        let before = jb.target_samples;
        assert!(before < peak, "la récupération a bien commencé (before<peak)");

        // Un SEUL pull de plus : la cible ne chute que d'un pas borné.
        drive_full_pulls(&mut jb, 240, 1);
        let after = jb.target_samples;
        assert!(after <= before, "monotone décroissant");
        assert!(before - after <= 4, "descente bornée par pull: {} samples", before - after);
        assert!(after >= floor_samples, "jamais sous le plancher tail-aware");
    }

    #[test]
    fn local_mode_floor_below_network_min() {
        // A-lite : en mode local, le plancher descend à LOCAL_MIN_TARGET_MS (3),
        // SOUS le plancher réseau MIN_TARGET_MS (5). Le réseau, lui, reste borné à 5.
        let mut jb = JitterBuffer::new();
        jb.set_local_mode(true);
        jb.set_target_ms(3);
        assert_eq!(jb.target_ms(), LOCAL_MIN_TARGET_MS, "local : plancher 3 ms");
        assert_eq!(LOCAL_MIN_TARGET_MS, 3);

        let mut net = JitterBuffer::new(); // réseau (local_mode = false)
        net.set_target_ms(3);
        assert_eq!(net.target_ms(), MIN_TARGET_MS, "réseau : plancher reste 5 ms (intact)");
    }

    #[test]
    fn underrun_pressure_is_capped() {
        // La mémoire d'underrun est bornée : même une longue rafale ne fait pas
        // grimper la pression sans limite (sinon buffer tenu trop longtemps après).
        let mut jb = JitterBuffer::new();
        jb.observe_jitter(0.7);
        for _ in 0..50 {
            jb.push(&vec![0.1_f32; jb.target_samples + 1]);
            let t = jb.target_samples;
            jb.pull(&mut vec![0.0_f32; t]);     // consomme (plein)
            jb.pull(&mut vec![0.0_f32; t * 4]); // underrun
        }
        assert!(
            jb.underrun_pressure <= UNDERRUN_PRESSURE_MAX + 1e-6,
            "pression plafonnée: {}",
            jb.underrun_pressure
        );
    }

    #[test]
    fn local_mode_recovery_path_unchanged() {
        // Garantie « self-monitor intouché » : en mode local, la récupération
        // rapide C1 ne s'applique PAS — c'est `adapt_down` (palier temporel) qui
        // gouverne. Sans 8 s écoulées, la cible NE redescend pas via des pulls
        // calmes (contraste avec le réseau ci-dessus).
        let mut jb = JitterBuffer::new();
        jb.set_local_mode(true);
        jb.set_target_ms(5);
        let five_ms = 5 * SAMPLE_RATE * CHANNELS / 1000;
        jb.push(&vec![0.1_f32; five_ms]);
        let mut out = vec![0.0_f32; five_ms];
        jb.pull(&mut out); // prime + consomme
        jb.pull(&mut vec![0.0_f32; five_ms * 3]); // underrun → adapt_up (borné LOCAL_MAX)
        let t_after = jb.target_ms();
        assert!(t_after > 5, "local : le filet a bien remonté: {t_after} ms");

        drive_full_pulls(&mut jb, 240, 8000); // beaucoup de pulls calmes, mais < 8 s
        assert_eq!(
            jb.target_ms(), t_after,
            "local : pas de récupération rapide C1 (chemin adapt_down intact): {} ms",
            jb.target_ms()
        );
    }
}
