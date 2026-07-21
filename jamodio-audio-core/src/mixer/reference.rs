//! Source « référence » de l'agent (Option B — chantier synchro métro/backing).
//!
//! La *référence* = le chef d'orchestre partagé (métronome, et plus tard le
//! backing track) que TOUS les peers doivent entendre au même instant. En mode
//! agent, elle sort par le chemin ASIO/CoreAudio de l'agent, à latence CONNUE —
//! contrairement au navigateur qui, sur Windows/WASAPI, ignore quand son son
//! sort réellement (cf. `internal-docs/plans/PLAN-OPTION-B-B0-DESIGN.md`).
//!
//! ## Rôle de ce module (B1 = métronome, M1)
//!
//! Le browser reste **maître de la grille** : il calcule, pour chaque beat,
//! l'indice d'échantillon de sortie de l'agent où le clic doit ÉMERGER (via
//! l'offset d'horloge serveur + l'ancre exposée par l'agent), et pilote cette
//! source. Ici on ne fait donc AUCUN calcul de grille temporelle : on reçoit un
//! ancrage « le beat #index émerge au frame #frame » + un tempo, et on
//! **synthétise le clic à l'échantillon près** dans le flux de sortie.
//!
//! Un re-ancrage périodique (`set_grid`) absorbe la lente dérive du quartz de
//! sortie vs l'horloge serveur (= l'équivalent de la DLL d'Option A).
//!
//! ## Extensibilité (décision Ben)
//!
//! La synthèse est conçue extensible dès le départ (choix du son, figures
//! rythmiques croche/triolet/doubles via `Figure::offsets`) — mais B1 ne câble
//! qu'UN preset (`MetroSound::Click`, figure noire) : priorité à la robustesse
//! et à la justesse de la synchro. Réf. design : l'ancien `metro-engine.js`
//! (SOUNDS / FIGURES / subdivisions), commit de suppression `4402711`.

/// Sample rate de sortie de l'agent (Hz). Verrouillé à 48 kHz (cf. `playback.rs`).
const SR: f64 = 48_000.0;

/// Enveloppe du grain de clic : attaque linéaire très courte (anti-pop) puis
/// décroissance exponentielle. Durée totale bornée (le grain est coupé au-delà).
const ATTACK_S: f32 = 0.0005; // 0.5 ms
const DECAY_TAU_S: f32 = 0.030; // 30 ms
const DURATION_S: f32 = 0.120; // 120 ms (queue inaudible ensuite)
const DURATION_FRAMES: u64 = (DURATION_S as f64 * SR) as u64;

// ─── Backing (B4) ─────────────────────────────────────────────────────────
/// Gain proportionnel du servo varispeed du backing (erreur en frames → écart de
/// vitesse). Réglé pour que l'erreur juste sous le seuil de snap sature la borne.
const BACKING_SERVO_GAIN: f64 = 5.0e-6;
/// Borne de varispeed (±) : 1 % max → pitch inaudible, dérive inter-peers (~50 ppm)
/// largement couverte.
const BACKING_SERVO_CLAMP: f64 = 0.01;
/// Au-delà de cette erreur d'alignement (frames), on SNAP (seek) au lieu de servo.
/// 2400 frames = 50 ms @48k.
const BACKING_SNAP_FRAMES: f64 = 2400.0;

/// Rôle rythmique d'un onset — pilote timbre/niveau du grain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// 1er temps de la mesure (accent fort).
    Accent,
    /// Accent secondaire (tête de groupe d'une métrique composée/irrégulière).
    Medium,
    /// Temps principal (non accentué).
    Main,
    /// Subdivision (croche/double/triolet) — plus discret. Futur (figures).
    Sub,
}

/// Longueur max d'un pattern d'accents (borne le tableau fixe embarqué dans
/// `Metro` → aucune allocation sur le chemin audio). 16 couvre largement les
/// chiffrages supportés (max actuel 7 pulses).
const MAX_BEATS_PER_BAR: usize = 16;

/// Timbre du métronome. Extensible : une nouvelle variante + son mapping
/// `params()` suffit. B1 ne produit que `Click`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MetroSound {
    /// Clic synthétique (2 partiels + décroissance rapide) — le défaut.
    #[default]
    Click,
}

impl MetroSound {
    /// Depuis le nom wire (`reference-config.sound`). Inconnu → défaut (jamais
    /// d'échec : un preset non supporté par un vieil agent retombe sur Click).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "click" => MetroSound::Click,
            _ => MetroSound::default(),
        }
    }

    /// Fréquence (Hz) et amplitude (0..1) du grain selon le rôle.
    fn params(self, role: Role) -> (f32, f32) {
        match self {
            MetroSound::Click => match role {
                Role::Accent => (1800.0, 0.90),
                Role::Medium => (1500.0, 0.72),
                Role::Main => (1200.0, 0.55),
                Role::Sub => (1200.0, 0.30),
            },
        }
    }
}

/// Figure rythmique = onsets par temps, en fractions de temps (0.0 = sur le
/// temps). B1 : la noire (`[0.0]`). Futur : croches `[0, 0.5]`, doubles
/// `[0, 0.25, 0.5, 0.75]`, triolet `[0, 0.333, 0.667]`…
#[derive(Clone, Copy, Debug)]
pub struct Figure {
    pub offsets: &'static [f32],
}

// Tables d'offsets (fractions de PULSE) — MIROIR de `metro-config.js` FIGURES.
// Toute évolution doit rester synchronisée des deux côtés (browser + agent).
const FIGURE_QUARTER: Figure = Figure { offsets: &[0.0] };
const FIGURE_EIGHTH: Figure = Figure { offsets: &[0.0, 1.0 / 2.0] };
const FIGURE_EIGHTH_T: Figure = Figure { offsets: &[0.0, 1.0 / 3.0, 2.0 / 3.0] };
const FIGURE_SIXTEENTH: Figure = Figure {
    offsets: &[0.0, 1.0 / 4.0, 2.0 / 4.0, 3.0 / 4.0],
};
const FIGURE_SIXTEENTH_T: Figure = Figure {
    offsets: &[0.0, 1.0 / 6.0, 2.0 / 6.0, 3.0 / 6.0, 4.0 / 6.0, 5.0 / 6.0],
};
const FIGURE_THIRTYSECOND: Figure = Figure {
    offsets: &[
        0.0, 1.0 / 8.0, 2.0 / 8.0, 3.0 / 8.0, 4.0 / 8.0, 5.0 / 8.0, 6.0 / 8.0, 7.0 / 8.0,
    ],
};

impl Default for Figure {
    fn default() -> Self {
        FIGURE_QUARTER
    }
}

impl Figure {
    /// Depuis le nom wire (`reference-config.figure`). Inconnu → noire.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "q" | "quarter" => FIGURE_QUARTER,
            "8" => FIGURE_EIGHTH,
            "8t" => FIGURE_EIGHTH_T,
            "16" => FIGURE_SIXTEENTH,
            "16t" => FIGURE_SIXTEENTH_T,
            "32" => FIGURE_THIRTYSECOND,
            _ => Figure::default(),
        }
    }
}

/// Ancre échantillon↔mural exposée au browser : le frame de sortie `frame`
/// (indice absolu, per-canal) était en cours de rendu à l'instant monotone
/// agent `mono_ms`. Le browser en déduit l'émergence de tout frame `F` :
/// `E(F) = mono_ms + outMs + (F − frame)·1000/SR` (cf. B0 §3.1).
#[derive(Clone, Copy, Debug, Default)]
pub struct OutputAnchor {
    pub frame: u64,
    pub mono_ms: f64,
}

/// Un grain de clic en cours de sonorisation (peut chevaucher plusieurs blocs
/// de callback → on garde les voix actives entre les appels à `mix_into`).
#[derive(Clone, Copy)]
struct Voice {
    /// Frame absolu (per-canal) de l'onset — origine temporelle du grain.
    start_frame: u64,
    freq: f32,
    amp: f32,
}

/// État de grille du métronome, exprimé en **frames de sortie** (pas en temps) :
/// le beat `anchor_beat_index` émerge au frame `anchor_beat_frame`, les suivants
/// à `+ frames_per_beat`. Le browser fournit cet ancrage et le rafraîchit
/// (`set_grid`) pour suivre la dérive.
struct Metro {
    frames_per_beat: f64,
    beats_per_accent: u32,
    /// Nombre de pulses/mesure (modulo pour l'accent). 0 = utiliser le fallback
    /// `beats_per_accent` (accent sur le 1er temps uniquement).
    beats_per_bar: u32,
    /// Pattern d'accents par pulse (0 normal / 1 médium / 2 fort). Seuls les
    /// `beats_per_bar` premiers éléments sont significatifs. Tableau FIXE →
    /// aucune allocation sur le chemin audio.
    accent_pattern: [u8; MAX_BEATS_PER_BAR],
    anchor_beat_frame: f64,
    anchor_beat_index: u64,
    sound: MetroSound,
    figure: Figure,
    /// Clé (`beat*n_offsets + sub`) du dernier onset ÉMIS — garde-fou anti
    /// double-émission au « joint » d'un re-ancrage (les indices restent
    /// monotones car liés à l'indice de beat, pas au frame).
    last_onset_key: Option<u64>,
}

impl Metro {
    fn idle() -> Self {
        Self {
            frames_per_beat: 0.0,
            beats_per_accent: 4,
            beats_per_bar: 0,
            accent_pattern: [0; MAX_BEATS_PER_BAR],
            anchor_beat_frame: 0.0,
            anchor_beat_index: 0,
            sound: MetroSound::default(),
            figure: Figure::default(),
            last_onset_key: None,
        }
    }

    /// Rôle du temps (pulse) d'indice global `ju`. Utilise le `accent_pattern`
    /// fourni par le browser si disponible ; sinon retombe sur `beats_per_accent`
    /// (rétro-compat : accent sur les multiples, comme avant l'accent-pattern).
    fn beat_role(&self, ju: u64) -> Role {
        if self.beats_per_bar > 0 {
            let n = (self.beats_per_bar as usize).min(MAX_BEATS_PER_BAR);
            match self.accent_pattern[(ju % n as u64) as usize] {
                2 => Role::Accent,
                1 => Role::Medium,
                _ => Role::Main,
            }
        } else if ju.is_multiple_of(self.beats_per_accent.max(1) as u64) {
            Role::Accent
        } else {
            Role::Main
        }
    }
}

/// Sous-source BACKING (B4) : le browser détient/décode le fichier et pousse son
/// PCM stéréo (48 kHz entrelacé) UNE FOIS (begin/chunk/end) ; l'agent le rejoue
/// aligné sur la grille de sortie, avec un **servo varispeed** anti-dérive
/// inter-peers (verrouille la tête de lecture sur une cible `frame backing ↔
/// frame de sortie` rafraîchie périodiquement par le browser). Le tap record
/// reste 100 % côté browser (non-négociable #2) — l'agent ne reçoit qu'une copie
/// monitoring.
struct Backing {
    /// PCM stéréo entrelacé @48 kHz. Rempli par begin/push_chunk/end.
    pcm: Vec<f32>,
    /// true une fois `end` reçu → lisible.
    ready: bool,
    playing: bool,
    volume: f32,
    pan: f32,
    /// Tête de lecture en FRAMES (fractionnaire → interpolation linéaire).
    play_head: f64,
    /// Vitesse de lecture (servo). ~1.0 ; ajustée pour verrouiller la grille.
    rs_speed: f64,
    /// Cible d'alignement : le frame backing `anchor_backing_frame` doit émerger
    /// au frame de sortie `anchor_output_frame`.
    anchor_backing_frame: f64,
    anchor_output_frame: f64,
    anchored: bool,
    /// Force un SNAP de la tête au prochain bloc (play / seek).
    snap_pending: bool,
}

impl Backing {
    fn new() -> Self {
        Self {
            pcm: Vec::new(),
            ready: false,
            playing: false,
            volume: 1.0,
            pan: 0.0,
            play_head: 0.0,
            rs_speed: 1.0,
            anchor_backing_frame: 0.0,
            anchor_output_frame: 0.0,
            anchored: false,
            snap_pending: false,
        }
    }

    fn begin(&mut self, total_frames: usize) {
        // Garde-fou anti-DoS (review pré-BETA) : total_frames vient du wire ; sans
        // borne, une valeur absurde (u64::MAX → usize) faisait reserve() → échec
        // d'alloc → abort du process EN UN SEUL message. On plafonne la
        // pré-réservation à 30 min @ 48kHz ; le Vec grandit à la demande si un
        // backing légitime dépasse (rare).
        const MAX_BACKING_FRAMES: usize = 48_000 * 60 * 30;
        self.pcm.clear();
        self.pcm.reserve(total_frames.min(MAX_BACKING_FRAMES).saturating_mul(2));
        self.ready = false;
        self.playing = false;
        self.anchored = false;
        self.play_head = 0.0;
        self.rs_speed = 1.0;
    }
    fn push_chunk(&mut self, samples: &[f32]) {
        self.pcm.extend_from_slice(samples);
    }
    fn end(&mut self) {
        self.ready = true;
    }
    fn unload(&mut self) {
        self.pcm.clear();
        self.ready = false;
        self.playing = false;
        self.anchored = false;
    }
    fn set_anchor(&mut self, abf: f64, aof: f64) {
        if abf.is_finite() && aof.is_finite() {
            self.anchor_backing_frame = abf;
            self.anchor_output_frame = aof;
            self.anchored = true;
        }
    }
    fn play(&mut self, abf: f64, aof: f64) {
        self.set_anchor(abf, aof);
        self.playing = true;
        self.snap_pending = true;
    }
    fn pause(&mut self) {
        self.playing = false;
    }
    fn seek(&mut self, abf: f64, aof: f64) {
        self.set_anchor(abf, aof);
        self.snap_pending = true;
    }
    fn sync(&mut self, abf: f64, aof: f64) {
        self.set_anchor(abf, aof);
    }
    fn set_volume(&mut self, v: f32) {
        self.volume = if v.is_finite() { v.clamp(0.0, 1.5) } else { 1.0 };
    }
    fn set_pan(&mut self, p: f32) {
        self.pan = if p.is_finite() { p.clamp(-1.0, 1.0) } else { 0.0 };
    }

    /// Échantillon stéréo interpolé linéairement à la tête `head` (frames).
    fn sample_at(&self, head: f64) -> (f32, f32) {
        let n = self.pcm.len() / 2;
        if n == 0 {
            return (0.0, 0.0);
        }
        let i = head.floor().max(0.0) as usize;
        if i + 1 >= n {
            let li = 2 * (n - 1);
            return (self.pcm[li], self.pcm[li + 1]);
        }
        let frac = (head - i as f64) as f32;
        let l0 = self.pcm[2 * i];
        let r0 = self.pcm[2 * i + 1];
        let l1 = self.pcm[2 * i + 2];
        let r1 = self.pcm[2 * i + 3];
        (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
    }

    /// Additionne le backing dans `output` pour le bloc démarrant au frame de
    /// sortie `block_start`. Verrouille la tête sur la grille (snap ou servo).
    fn generate(&mut self, output: &mut [f32], block_start: u64) {
        if !self.ready || !self.playing || self.pcm.len() < 2 {
            return;
        }
        let pcm_frames = (self.pcm.len() / 2) as f64;

        if self.anchored {
            let target = self.anchor_backing_frame + (block_start as f64 - self.anchor_output_frame);
            let err = target - self.play_head;
            if self.snap_pending || err.abs() > BACKING_SNAP_FRAMES {
                self.play_head = target.clamp(0.0, pcm_frames);
                self.rs_speed = 1.0;
                self.snap_pending = false;
            } else {
                // Servo proportionnel : rapproche la tête de la cible sans clic.
                self.rs_speed = (1.0 + BACKING_SERVO_GAIN * err)
                    .clamp(1.0 - BACKING_SERVO_CLAMP, 1.0 + BACKING_SERVO_CLAMP);
            }
        }

        let gain_l = self.volume * (1.0 - self.pan).min(1.0);
        let gain_r = self.volume * (1.0 + self.pan).min(1.0);
        let frames = output.len() / 2;
        for i in 0..frames {
            if self.play_head >= pcm_frames - 1.0 {
                self.playing = false; // fin de piste
                break;
            }
            let (l, r) = self.sample_at(self.play_head);
            output[i * 2] += l * gain_l;
            output[i * 2 + 1] += r * gain_r;
            self.play_head += self.rs_speed;
        }
    }
}

/// Source référence : synthétise la grille métro dans le flux de sortie, tout en
/// maintenant le compteur de frames absolu + l'ancre exposée au browser.
///
/// **Placement dans `mix_into`** : ajoutée APRÈS le tap record (donc exclue du
/// MIX enregistré = parité browser) et APRÈS le DIM (le clic n'est pas ducké),
/// mais AVANT le master (elle suit le fader master) et le clamp. Elle n'est
/// donc PAS une entrée de la map `streams` (celles-ci sont sommées avant le tap).
pub struct ReferenceSource {
    enabled: bool,
    volume: f32,
    pan: f32,
    /// Compteur de frames de sortie absolu (per-canal), monotone sur la vie de
    /// la source. Avance de `frames` à chaque `advance_and_generate`.
    frames_rendered: u64,
    anchor: OutputAnchor,
    metro: Metro,
    voices: Vec<Voice>,
    /// Sous-source backing (B4) — rejoue le PCM poussé par le browser, aligné.
    backing: Backing,
}

impl Default for ReferenceSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceSource {
    pub fn new() -> Self {
        Self {
            enabled: false,
            volume: 0.6,
            pan: 0.0,
            frames_rendered: 0,
            anchor: OutputAnchor::default(),
            metro: Metro::idle(),
            // Cap ~8 : à 120 bpm un clic (120 ms) ne chevauche jamais plus de
            // 1-2 beats ; large marge pour les futures subdivisions.
            voices: Vec::with_capacity(8),
            backing: Backing::new(),
        }
    }

    /// Configure la grille métro + son + volume/pan (message `reference-config`).
    /// `enabled=false` NE coupe PAS les voix en cours (elles s'éteignent
    /// naturellement) mais stoppe les nouveaux onsets.
    #[allow(clippy::too_many_arguments)]
    pub fn set_config(
        &mut self,
        enabled: bool,
        volume: f32,
        pan: f32,
        bpm: f32,
        // Durée d'une pulse / noire : 1.0 noire, 0.5 croche, 1.5 noire pointée.
        pulse_ratio: f64,
        // Nb de pulses/mesure + pattern d'accents (0/1/2). `accent_pattern` vide
        // → fallback `beats_per_accent`.
        beats_per_bar: u32,
        accent_pattern: &[u8],
        beats_per_accent: u32,
        sound: MetroSound,
        figure: Figure,
        anchor_beat_frame: f64,
        anchor_beat_index: u64,
    ) {
        self.enabled = enabled;
        self.set_volume(volume);
        self.set_pan(pan);
        // La PULSE (le temps cliqué) = noire × pulse_ratio. En 4/4 (ratio 1.0) →
        // `SR*60/bpm`, identique à l'historique. Ratio non fini/≤0 → défaut 1.0.
        let ratio = if pulse_ratio.is_finite() && pulse_ratio > 0.0 { pulse_ratio } else { 1.0 };
        self.metro.frames_per_beat = if bpm.is_finite() && bpm > 0.0 {
            SR * 60.0 / bpm as f64 * ratio
        } else {
            0.0
        };
        self.metro.beats_per_accent = beats_per_accent.max(1);
        // Copie le pattern dans le tableau fixe (tronqué à MAX_BEATS_PER_BAR).
        // Longueur cohérente pattern/beats_per_bar sinon on ignore le pattern.
        self.metro.accent_pattern = [0; MAX_BEATS_PER_BAR];
        let bpb = beats_per_bar as usize;
        if bpb > 0 && bpb <= MAX_BEATS_PER_BAR && accent_pattern.len() >= bpb {
            self.metro.accent_pattern[..bpb].copy_from_slice(&accent_pattern[..bpb]);
            self.metro.beats_per_bar = beats_per_bar;
        } else {
            self.metro.beats_per_bar = 0; // fallback beats_per_accent
        }
        self.metro.sound = sound;
        self.metro.figure = figure;
        self.set_grid(anchor_beat_frame, anchor_beat_index);
    }

    /// Re-ancrage périodique de la grille (message `reference-grid`) = la DLL :
    /// le browser recalcule l'ancrage avec l'offset d'horloge courant et le
    /// pousse ici. On réinitialise `last_onset_key` de façon cohérente pour ne
    /// re-jouer aucun onset déjà passé (clé liée à l'indice de beat).
    pub fn set_grid(&mut self, anchor_beat_frame: f64, anchor_beat_index: u64) {
        if anchor_beat_frame.is_finite() {
            self.metro.anchor_beat_frame = anchor_beat_frame;
            self.metro.anchor_beat_index = anchor_beat_index;
        }
    }

    /// Arrête le métronome et coupe net les voix en cours (message
    /// `reference-stop`).
    pub fn stop(&mut self) {
        self.enabled = false;
        self.voices.clear();
        self.metro.last_onset_key = None;
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_finite() {
            volume.clamp(0.0, 1.5)
        } else {
            0.6
        };
    }

    pub fn set_pan(&mut self, pan: f32) {
        self.pan = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
    }

    /// Ancre courante (frame de sortie ↔ instant monotone) pour le pong.
    pub fn anchor(&self) -> OutputAnchor {
        self.anchor
    }

    // ─── Backing (B4) — pilotage depuis les handlers wire ─────────────────────
    /// Démarre le chargement d'un nouveau backing (réserve `total_frames` frames).
    pub fn backing_begin(&mut self, total_frames: usize) {
        self.backing.begin(total_frames);
    }
    /// Ajoute un chunk de PCM stéréo entrelacé (déjà converti en f32 côté handler).
    pub fn backing_push(&mut self, samples: &[f32]) {
        self.backing.push_chunk(samples);
    }
    /// Fin de chargement → le backing devient lisible.
    pub fn backing_end(&mut self) {
        self.backing.end();
    }
    /// Décharge le backing (libère le PCM).
    pub fn backing_unload(&mut self) {
        self.backing.unload();
    }
    /// Lance la lecture : le frame backing `abf` doit émerger au frame de sortie `aof`.
    pub fn backing_play(&mut self, abf: f64, aof: f64) {
        self.backing.play(abf, aof);
    }
    pub fn backing_pause(&mut self) {
        self.backing.pause();
    }
    /// Repositionne (seek) : snap au prochain bloc.
    pub fn backing_seek(&mut self, abf: f64, aof: f64) {
        self.backing.seek(abf, aof);
    }
    /// Re-ancrage périodique (= DLL du backing) : ajuste la cible du servo.
    pub fn backing_sync(&mut self, abf: f64, aof: f64) {
        self.backing.sync(abf, aof);
    }
    pub fn set_backing_volume(&mut self, v: f32) {
        self.backing.set_volume(v);
    }
    pub fn set_backing_pan(&mut self, p: f32) {
        self.backing.set_pan(p);
    }

    /// Avance le compteur de frames, rafraîchit l'ancre (avec `mono_ms` fourni
    /// par le mixer), et additionne la synthèse métro dans `output` (stéréo
    /// entrelacé). Appelé à CHAQUE `mix_into`, même désactivé, pour que l'ancre
    /// reste fraîche (le browser ping AVANT le 1er beat pour placer la grille).
    pub fn advance_and_generate(&mut self, output: &mut [f32], mono_ms: f64) {
        let frames = (output.len() / 2) as u64;
        let block_start = self.frames_rendered;
        let block_end = block_start + frames;
        // Ancre = 1er frame du bloc courant ↔ maintenant (monotone agent).
        self.anchor = OutputAnchor { frame: block_start, mono_ms };

        if self.enabled {
            self.spawn_onsets(block_start, block_end);
        }
        if !self.voices.is_empty() {
            self.render(output, block_start, block_end);
        }
        // Backing (B4) — mixé au même point (donc mêmes garanties record/DIM/master).
        self.backing.generate(output, block_start);
        self.frames_rendered = block_end;
    }

    /// Programme les voix dont l'onset tombe dans `[block_start, block_end)`.
    fn spawn_onsets(&mut self, block_start: u64, block_end: u64) {
        let fpb = self.metro.frames_per_beat;
        if fpb <= 0.0 {
            return;
        }
        let aframe = self.metro.anchor_beat_frame;
        let aidx = self.metro.anchor_beat_index as f64;
        let offsets = self.metro.figure.offsets;
        let n_off = offsets.len().max(1) as u64;
        let sound = self.metro.sound;

        // Indices de beat susceptibles d'avoir un onset dans le bloc. On élargit
        // de ±1 beat car une subdivision (offset > 0) peut pousser un onset dans
        // le bloc voisin. Blocs courts (64 frames) ⇒ 0-1 beat concerné.
        let j_lo = (((block_start as f64 - aframe) / fpb) + aidx - 1.0).floor() as i64;
        let j_hi = (((block_end as f64 - aframe) / fpb) + aidx + 1.0).ceil() as i64;

        for j in j_lo..=j_hi {
            if j < 0 {
                continue;
            }
            let ju = j as u64;
            for (si, off) in offsets.iter().enumerate() {
                let onset = aframe + ((ju as f64 - aidx) + *off as f64) * fpb;
                if onset < block_start as f64 || onset >= block_end as f64 {
                    continue;
                }
                let key = ju * n_off + si as u64;
                if let Some(last) = self.metro.last_onset_key {
                    if key <= last {
                        continue; // déjà émis (ou joint de re-ancrage) → skip
                    }
                }
                let role = if si == 0 {
                    self.metro.beat_role(ju)
                } else {
                    Role::Sub
                };
                let (freq, amp) = sound.params(role);
                self.voices.push(Voice {
                    start_frame: onset.round() as u64,
                    freq,
                    amp,
                });
                self.metro.last_onset_key = Some(key);
            }
        }
    }

    /// Additionne les voix actives dans `output` puis élague celles terminées.
    fn render(&mut self, output: &mut [f32], block_start: u64, block_end: u64) {
        let vol = self.volume;
        let pan = self.pan;
        // Loi de balance stéréo linéaire (0 dB au centre) — identique au mixer.
        let gain_l = vol * (1.0 - pan).min(1.0);
        let gain_r = vol * (1.0 + pan).min(1.0);

        for v in &self.voices {
            let first = block_start.max(v.start_frame);
            for f in first..block_end {
                let rel = (f - v.start_frame) as f32 / SR as f32;
                if rel > DURATION_S {
                    break;
                }
                let s = v.amp * envelope(rel) * tone(v.freq, rel);
                let idx = ((f - block_start) as usize) * 2;
                output[idx] += s * gain_l;
                output[idx + 1] += s * gain_r;
            }
        }

        // Élague les grains entièrement passés (dernier frame < début du prochain bloc).
        self.voices
            .retain(|v| v.start_frame + DURATION_FRAMES > block_end);
    }
}

/// Enveloppe d'amplitude du grain (0..1) à `t` secondes de l'onset.
fn envelope(t: f32) -> f32 {
    if t < 0.0 {
        0.0
    } else if t < ATTACK_S {
        t / ATTACK_S
    } else {
        (-(t - ATTACK_S) / DECAY_TAU_S).exp()
    }
}

/// Timbre du clic à `t` secondes : fondamentale + octave (partiel à 2f), amorti
/// pour rester dans [-1, 1] avant enveloppe.
fn tone(freq: f32, t: f32) -> f32 {
    use std::f32::consts::TAU;
    let phase = TAU * freq * t;
    (phase.sin() + 0.5 * (2.0 * phase).sin()) / 1.5
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bloc stéréo de `frames` frames (interleaved), zéro-initialisé.
    fn block(frames: usize) -> Vec<f32> {
        vec![0.0; frames * 2]
    }

    fn rms(buf: &[f32]) -> f32 {
        if buf.is_empty() {
            return 0.0;
        }
        (buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32).sqrt()
    }

    #[test]
    fn disabled_source_is_silent_but_advances_anchor() {
        let mut r = ReferenceSource::new();
        let mut out = block(64);
        r.advance_and_generate(&mut out, 10.0);
        assert_eq!(rms(&out), 0.0, "désactivé ⇒ aucun son");
        assert_eq!(r.anchor().frame, 0, "ancre = 1er frame du bloc");
        assert_eq!(r.anchor().mono_ms, 10.0);
        // 2e bloc : le compteur de frames a avancé de 64.
        r.advance_and_generate(&mut out, 11.0);
        assert_eq!(r.anchor().frame, 64, "frames_rendered avance de 64/bloc");
    }

    #[test]
    fn beat_emerges_at_the_scheduled_frame() {
        // 120 bpm ⇒ 24000 frames/beat. Beat 0 ancré au frame 100.
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 4, &[], 4, MetroSound::Click, Figure::default(), 100.0, 0);
        // Rends par blocs de 64 frames jusqu'à couvrir le frame 100.
        let mut fired_block = None;
        for b in 0..4 {
            let mut out = block(64);
            r.advance_and_generate(&mut out, b as f64);
            if rms(&out) > 0.0 && fired_block.is_none() {
                fired_block = Some(b);
            }
        }
        // Frame 100 ⇒ bloc index 1 (frames 64..128).
        assert_eq!(fired_block, Some(1), "le clic sonne dans le bloc contenant le frame 100");
    }

    #[test]
    fn no_double_fire_across_reanchor() {
        // Une grille, on rend le beat 0, puis un re-ancrage qui ne doit PAS
        // re-jouer le beat 0.
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 4, &[], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        // Beat 0 est à frame 0 → sonne au 1er bloc.
        let mut out = block(64);
        r.advance_and_generate(&mut out, 0.0);
        assert!(rms(&out) > 0.0, "beat 0 sonne");
        assert_eq!(r.metro.last_onset_key, Some(0));
        // Re-ancrage identique (beat 0 @ frame 0) : ne doit pas re-spawn beat 0.
        r.set_grid(0.0, 0);
        let voices_before = r.voices.len();
        r.advance_and_generate(&mut block(64), 1.0);
        // last_onset_key inchangé (0) ⇒ pas de nouvel onset pour beat 0.
        assert_eq!(r.metro.last_onset_key, Some(0), "beat 0 pas re-émis après re-ancrage");
        assert!(r.voices.len() <= voices_before, "pas de voix dupliquée");
    }

    #[test]
    fn accent_is_louder_than_main() {
        let (fa, aa) = MetroSound::Click.params(Role::Accent);
        let (fm, am) = MetroSound::Click.params(Role::Main);
        assert!(aa > am, "accent plus fort que temps normal");
        assert!(fa > fm, "accent plus aigu");
    }

    #[test]
    fn stop_clears_voices() {
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 4, &[], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        r.advance_and_generate(&mut block(64), 0.0);
        assert!(!r.voices.is_empty());
        r.stop();
        assert!(r.voices.is_empty(), "stop coupe les voix");
        assert!(!r.enabled);
    }

    #[test]
    fn wire_parsers_fall_back_to_defaults() {
        assert_eq!(MetroSound::from_wire("click"), MetroSound::Click);
        assert_eq!(MetroSound::from_wire("inconnu"), MetroSound::Click);
        assert_eq!(Figure::from_wire("q").offsets, FIGURE_QUARTER.offsets);
        assert_eq!(Figure::from_wire("bizarre").offsets, FIGURE_QUARTER.offsets);
    }

    // ─── Sous-lot ① : pulse_ratio + accent_pattern ────────────────────────
    #[test]
    fn pulse_ratio_scales_beat_spacing() {
        // 120 bpm, ratio 1.0 (noire) → 24000 frames/beat ; ratio 0.5 (croche) → 12000.
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 4, &[2, 0, 0, 0], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        assert_eq!(r.metro.frames_per_beat, 24_000.0);
        r.set_config(true, 1.0, 0.0, 120.0, 0.5, 5, &[2, 0, 0, 1, 0], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        assert_eq!(r.metro.frames_per_beat, 12_000.0, "croche = moitié de la noire");
    }

    #[test]
    fn accent_pattern_drives_role_5_8() {
        // 5/8 (3+2) : fort sur 0, médium sur 3, normal ailleurs. Modulo 5 sur l'index.
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 0.5, 5, &[2, 0, 0, 1, 0], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        assert_eq!(r.metro.beat_role(0), Role::Accent);
        assert_eq!(r.metro.beat_role(3), Role::Medium);
        assert_eq!(r.metro.beat_role(1), Role::Main);
        assert_eq!(r.metro.beat_role(5), Role::Accent, "mesure suivante : 5 mod 5 = 0");
        assert_eq!(r.metro.beat_role(8), Role::Medium, "8 mod 5 = 3");
    }

    #[test]
    fn empty_accent_pattern_falls_back_to_beats_per_accent() {
        // Pattern vide (agent piloté par un browser ancien) → accent sur les
        // multiples de beats_per_accent, comportement historique.
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 0, &[], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        assert_eq!(r.metro.beats_per_bar, 0, "pas de pattern → fallback");
        assert_eq!(r.metro.beat_role(0), Role::Accent);
        assert_eq!(r.metro.beat_role(4), Role::Accent);
        assert_eq!(r.metro.beat_role(1), Role::Main);
    }

    #[test]
    fn medium_between_accent_and_main() {
        let (_, aa) = MetroSound::Click.params(Role::Accent);
        let (_, am) = MetroSound::Click.params(Role::Medium);
        let (_, an) = MetroSound::Click.params(Role::Main);
        assert!(aa > am && am > an, "accent > médium > normal");
    }

    // ─── Sous-lot ② : subdivisions (figures) ──────────────────────────────
    #[test]
    fn figure_parsers() {
        assert_eq!(Figure::from_wire("8").offsets.len(), 2);
        assert_eq!(Figure::from_wire("8t").offsets.len(), 3);
        assert_eq!(Figure::from_wire("16").offsets.len(), 4);
        assert_eq!(Figure::from_wire("16t").offsets.len(), 6);
        assert_eq!(Figure::from_wire("32").offsets.len(), 8);
        assert_eq!(Figure::from_wire("inconnu").offsets.len(), 1, "défaut noire");
    }

    #[test]
    fn eighth_figure_adds_subdivision_onset() {
        // 120 bpm, ratio 1 → 24000 frames/beat. Croche → onsets à 0 ET 12000.
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 4, &[2, 0, 0, 0], 4, MetroSound::Click, Figure::from_wire("8"), 0.0, 0);
        let mut out = block(24_000); // un temps entier
        r.advance_and_generate(&mut out, 0.0);
        // 2 onsets émis (si=0 puis si=1) → dernière clé = beat0*2 + 1 = 1.
        assert_eq!(r.metro.last_onset_key, Some(1), "croche : 2 onsets sur le temps");
    }

    #[test]
    fn quarter_figure_single_onset() {
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 4, &[2, 0, 0, 0], 4, MetroSound::Click, Figure::from_wire("q"), 0.0, 0);
        let mut out = block(24_000);
        r.advance_and_generate(&mut out, 0.0);
        assert_eq!(r.metro.last_onset_key, Some(0), "noire : 1 seul onset");
    }

    #[test]
    fn subdivision_uses_sub_role() {
        // La subdivision (si>0) doit être plus discrète que le temps → amplitude
        // Sub < Main (garantit le grain « léger » attendu).
        let (_, sub) = MetroSound::Click.params(Role::Sub);
        let (_, main) = MetroSound::Click.params(Role::Main);
        assert!(sub < main, "subdivision plus discrète que le temps");
    }

    // ─── Backing (B4) ─────────────────────────────────────────────────────
    /// Charge une rampe (valeur du frame = frame/frames) pour tester la position.
    fn load_ramp(r: &mut ReferenceSource, frames: usize) {
        r.backing_begin(frames);
        let mut pcm = Vec::with_capacity(frames * 2);
        for f in 0..frames {
            let v = f as f32 / frames as f32;
            pcm.push(v);
            pcm.push(v);
        }
        r.backing_push(&pcm);
        r.backing_end();
    }

    #[test]
    fn backing_silent_until_ready_then_playing() {
        let mut r = ReferenceSource::new();
        r.backing_begin(100);
        r.backing_push(&[0.5_f32; 200]);
        // Pas de end() → pas lisible.
        let mut out = block(64);
        r.backing_play(0.0, 0.0);
        r.advance_and_generate(&mut out, 0.0);
        assert_eq!(rms(&out), 0.0, "silencieux tant que non chargé (end)");
        // end + play → audible.
        r.backing_end();
        r.backing_play(0.0, r.anchor().frame as f64); // ancre au frame de sortie courant
        let mut out2 = block(64);
        r.advance_and_generate(&mut out2, 1.0);
        assert!(rms(&out2) > 0.3, "audible une fois chargé + play");
        assert!((out2[0] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn backing_pause_silences() {
        let mut r = ReferenceSource::new();
        r.backing_begin(1000);
        r.backing_push(&[0.5_f32; 2000]);
        r.backing_end();
        r.backing_play(0.0, 0.0);
        r.advance_and_generate(&mut block(64), 0.0);
        r.backing_pause();
        let mut out = block(64);
        r.advance_and_generate(&mut out, 64.0);
        assert_eq!(rms(&out), 0.0, "pause coupe le backing");
    }

    #[test]
    fn backing_reaches_end_and_stops() {
        let mut r = ReferenceSource::new();
        r.backing_begin(10);
        r.backing_push(&[0.5_f32; 20]); // 10 frames
        r.backing_end();
        r.backing_play(0.0, 0.0);
        r.advance_and_generate(&mut block(64), 0.0); // 64 > 10 → fin de piste
        let mut out = block(64);
        r.advance_and_generate(&mut out, 64.0);
        assert_eq!(rms(&out), 0.0, "arrêt en fin de piste");
    }

    #[test]
    fn backing_seek_snaps_to_position() {
        let mut r = ReferenceSource::new();
        load_ramp(&mut r, 48_000); // valeur = frame/48000
        r.backing_play(0.0, 0.0);
        r.advance_and_generate(&mut block(64), 0.0);
        r.backing_seek(24_000.0, 64.0); // milieu, ancré au bloc suivant (frame 64)
        let mut out = block(64);
        r.advance_and_generate(&mut out, 64.0);
        assert!((out[0] - 0.5).abs() < 0.01, "seek snappe à la position (got {})", out[0]);
    }

    #[test]
    fn backing_pan_hard_right_silences_left() {
        let mut r = ReferenceSource::new();
        r.backing_begin(1000);
        r.backing_push(&[0.5_f32; 2000]);
        r.backing_end();
        r.set_backing_pan(1.0);
        r.backing_play(0.0, 0.0);
        let mut out = block(64);
        r.advance_and_generate(&mut out, 0.0);
        let l: f32 = out.iter().step_by(2).map(|s| s.abs()).sum();
        let rr: f32 = out.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(rr > 0.0, "canal droit audible");
        assert!(l < 1e-6, "pan full right ⇒ gauche muette");
    }

    #[test]
    fn backing_unload_clears() {
        let mut r = ReferenceSource::new();
        r.backing_begin(1000);
        r.backing_push(&[0.5_f32; 2000]);
        r.backing_end();
        r.backing_play(0.0, 0.0);
        r.backing_unload();
        let mut out = block(64);
        r.advance_and_generate(&mut out, 0.0);
        assert_eq!(rms(&out), 0.0, "unload → silencieux");
    }

    #[test]
    fn metro_and_backing_coexist() {
        // Les deux sous-sources s'additionnent sans s'exclure.
        let mut r = ReferenceSource::new();
        load_ramp(&mut r, 48_000);
        r.backing_play(0.5, 0.0); // audible immédiatement (rampe ≈0.5 en milieu)
        r.set_config(true, 1.0, 0.0, 120.0, 1.0, 4, &[], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        let mut out = block(64);
        r.advance_and_generate(&mut out, 0.0);
        assert!(rms(&out) > 0.0, "métro + backing produisent du son");
    }

    #[test]
    fn pan_hard_left_silences_right() {
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, -1.0, 120.0, 1.0, 4, &[], 4, MetroSound::Click, Figure::default(), 0.0, 0);
        let mut out = block(64);
        r.advance_and_generate(&mut out, 0.0);
        let l: f32 = out.iter().step_by(2).map(|s| s.abs()).sum();
        let rr: f32 = out.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(l > 0.0, "canal gauche audible");
        assert!(rr < 1e-6, "pan full left ⇒ droite muette");
    }
}
