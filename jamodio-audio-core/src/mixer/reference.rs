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

/// Rôle rythmique d'un onset — pilote timbre/niveau du grain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// 1er temps de la mesure (accent).
    Accent,
    /// Temps principal (non accentué).
    Main,
    /// Subdivision (croche/double/triolet) — plus discret. Futur (figures).
    Sub,
}

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

const FIGURE_QUARTER: Figure = Figure { offsets: &[0.0] };

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
            anchor_beat_frame: 0.0,
            anchor_beat_index: 0,
            sound: MetroSound::default(),
            figure: Figure::default(),
            last_onset_key: None,
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
        beats_per_accent: u32,
        sound: MetroSound,
        figure: Figure,
        anchor_beat_frame: f64,
        anchor_beat_index: u64,
    ) {
        self.enabled = enabled;
        self.set_volume(volume);
        self.set_pan(pan);
        self.metro.frames_per_beat = if bpm.is_finite() && bpm > 0.0 {
            SR * 60.0 / bpm as f64
        } else {
            0.0
        };
        self.metro.beats_per_accent = beats_per_accent.max(1);
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
        let bpa = self.metro.beats_per_accent.max(1) as u64;
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
                    if ju.is_multiple_of(bpa) {
                        Role::Accent
                    } else {
                        Role::Main
                    }
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
        r.set_config(true, 1.0, 0.0, 120.0, 4, MetroSound::Click, Figure::default(), 100.0, 0);
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
        r.set_config(true, 1.0, 0.0, 120.0, 4, MetroSound::Click, Figure::default(), 0.0, 0);
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
        r.set_config(true, 1.0, 0.0, 120.0, 4, MetroSound::Click, Figure::default(), 0.0, 0);
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

    #[test]
    fn pan_hard_left_silences_right() {
        let mut r = ReferenceSource::new();
        r.set_config(true, 1.0, -1.0, 120.0, 4, MetroSound::Click, Figure::default(), 0.0, 0);
        let mut out = block(64);
        r.advance_and_generate(&mut out, 0.0);
        let l: f32 = out.iter().step_by(2).map(|s| s.abs()).sum();
        let rr: f32 = out.iter().skip(1).step_by(2).map(|s| s.abs()).sum();
        assert!(l > 0.0, "canal gauche audible");
        assert!(rr < 1e-6, "pan full left ⇒ droite muette");
    }
}
