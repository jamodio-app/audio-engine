use super::reference::{Figure, MetroSound, OutputAnchor, ReferenceSource};
use super::ring_buffer::JitterBuffer;
use crate::protocol::StreamKind;
use crate::record::RecordCmd;
use crate::sync::clock::mono_now_ms;
use crossbeam_channel::Sender;
use std::collections::HashMap;

/// Id réservé du stream de self-monitor (capture locale rebouclée en sortie
/// pour que l'utilisateur s'entende dans son casque sans passer par la chaîne
/// browser à 25 ms). Mixé comme un stream normal mais exclu des stats remote
/// (stream_count, total_underruns, mean_target_ms) pour ne pas polluer l'UI.
pub const SELF_MONITOR_ID: &str = "self";

/// Id réservé de la source « référence » (métronome/backing via l'agent —
/// Option B). Le browser pilote son volume/pan via `SetVolume`/`SetPan` avec ce
/// producer_id (comme "self" pour le self-monitor). La source n'est PAS une
/// entrée de la map `streams` (cf. [`AudioMixer::reference`]).
pub const REFERENCE_ID: &str = "reference";

/// Id réservé de la sous-source backing (B4). Volume/pan pilotés par le browser
/// via `SetVolume`/`SetPan` avec ce producer_id (tranche backing de la mixette).
pub const BACKING_ID: &str = "backing";

/// Id réservé de la sous-source preview (Lot D — aperçu Library en studio).
/// Volume/pan pilotés par le browser via `SetVolume`/`SetPan` avec ce producer_id.
pub const PREVIEW_ID: &str = "preview";

/// Cible jitter buffer du self-monitor (ms). A-lite : 3 = `LOCAL_MIN_TARGET_MS`
/// du ring buffer (plancher local dédié, sous le plancher réseau). Le signal
/// vient du même process que la capture (pas de gigue réseau, seulement la gigue
/// d'ordonnancement des hops), donc on prend le minimum local stable → ~2 ms
/// gagnés sur le retour casque vs l'historique 5 ms. L'adaptation bornée (retour
/// à ce plancher, concealment sur spikes) reste le filet.
const SELF_MONITOR_TARGET_MS: usize = 3;

/// Loi de balance stéréo LINÉAIRE (0 dB au centre), source unique partagée par
/// `mix_into` (le rendu audio) et `stream_rms` (les niveaux VU post-pan). Sans
/// ce partage, le VU pourrait diverger de ce que l'utilisateur entend.
///   `gain_l = min(1, 1−pan)`, `gain_r = min(1, 1+pan)`.
/// Centre (`pan ≈ 0`) → `(1.0, 1.0)` ; extrêmes → `(1.0, 0.0)` / `(0.0, 1.0)`.
fn pan_gains(pan: f32) -> (f32, f32) {
    if pan.abs() < f32::EPSILON {
        (1.0, 1.0)
    } else {
        ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0))
    }
}

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
    /// REC-3 : si `Some(tx)`, un enregistrement est en cours et les push_*
    /// (self, peer) + mix_into envoient leurs samples au thread record via
    /// `try_send` non-bloquant. Si `None`, les tap sites sont no-op (1 if).
    record_tx: Option<Sender<RecordCmd>>,
    /// Master gain global appliqué dans `mix_into` après le mix des streams.
    /// Plage [0.0, 1.5] (cohérent avec les faders peer/self côté UI).
    /// Default 1.0 (unity).
    master_gain: f32,
    /// DIM factor — atténuation temporaire des instruments quand l'utilisateur
    /// active DIM côté UI (pour entendre la conversation talkback clairement).
    /// Plage [0.0, 1.0], typiquement 0.25 (-12dB) ou 1.0 (off). Appliqué
    /// dans `mix_into` après la somme des streams et avant le master_gain.
    /// **Le tap REC-3 push_mix est positionné AVANT dim et master** pour que
    /// le fichier MIX enregistré soit le mix post-fader des instruments
    /// SEUL (= ce qui irait à un peer théorique), indépendant des réglages
    /// d'écoute locaux dim/master. Cohérent avec le tap browser sur
    /// `instrumentMixBus` qui est aussi pre-dim/pre-master.
    dim_factor: f32,
    /// Source « référence » (métronome via l'agent — Option B). Mixée dans
    /// `mix_into` à un point DÉDIÉ (après le tap record, après le DIM, avant le
    /// master), donc HORS de la map `streams`. Toujours présente ; inerte tant
    /// qu'elle n'est pas configurée (`set_reference_config`). Cf.
    /// `mixer/reference.rs` + `PLAN-OPTION-B-B0-DESIGN.md`.
    reference: ReferenceSource,
    /// Point 3 (Lot 2) — RMS L/R du VRAI mix, mesuré dans `mix_into` sur la
    /// sortie réelle → le VU MASTER/MIX du browser reflète pan + faders (le
    /// browser ne pouvait que proxymer via un max mono). `master_*` = sortie
    /// finale (post dim/master/clamp) ; `mix_*` = tap MIX post-fader
    /// (pré-dim/master, parité browser `instrumentMixBus`). Lus par le
    /// `stream-levels` sender (100 ms) via `master_mix_rms()`.
    master_rms_l: f32,
    master_rms_r: f32,
    mix_rms_l: f32,
    mix_rms_r: f32,
    /// Pic échantillon L/R du MASTER (sortie finale) et du MIX (tap post-fader),
    /// mesurés dans `mix_into` en parallèle des RMS ci-dessus. Alimentent le
    /// peak-mètre DAW MASTER/MIX du browser. Lus via `master_mix_peak()`.
    master_peak_l: f32,
    master_peak_r: f32,
    mix_peak_l: f32,
    mix_peak_r: f32,
    /// Point 4 — accumulateur du bus MIX REC : somme des instruments **armés**
    /// uniquement (post-fader/pan), mesuré en parallèle de `output` dans la
    /// passe 1 de `mix_into`. Source du tap record (`PushMix`) et des
    /// `mix_rms/mix_peak`. Distinct de `output` (= monitoring/MASTER, tous les
    /// instruments). Réutilisé bloc à bloc → zéro alloc RT.
    mix_buf: Vec<f32>,
    /// Lot C (0.5.10-4) — accumulateur de la VOIX des pairs (talkback entrant).
    /// Les streams `StreamKind::Voice` sont sommés ici (passe 1), puis ajoutés à
    /// `output` dans `mix_into` APRÈS le tap RECORD et le DIM (comme la référence)
    /// → jamais enregistrée, jamais duckée (parité `voiceBus` navigateur).
    /// Réutilisé bloc à bloc (comme `temp_buf`) → zéro alloc RT.
    voice_buf: Vec<f32>,
    /// Gain/pan du BUS voix (une seule tranche, parité `voiceGain`/`voicePanNode`
    /// navigateur). Le mute est porté par le gain (le web envoie 0.0). Défaut 1.0/0.0.
    voice_gain: f32,
    voice_pan: f32,
    /// RMS (mono) de la voix des pairs effectivement mixée (post gain de bus) —
    /// remonté via `stream-levels` pour le VU voix navigateur en mode agent.
    inbound_voice_rms: f32,
}

/// RMS par canal d'un buffer stéréo entrelacé (L,R,L,R…). `(0,0)` si vide.
fn stereo_rms(buf: &[f32]) -> (f32, f32) {
    let frames = buf.len() / 2;
    if frames == 0 {
        return (0.0, 0.0);
    }
    let (mut sq_l, mut sq_r) = (0.0f32, 0.0f32);
    for pair in buf.chunks_exact(2) {
        sq_l += pair[0] * pair[0];
        sq_r += pair[1] * pair[1];
    }
    ((sq_l / frames as f32).sqrt(), (sq_r / frames as f32).sqrt())
}

/// Pic échantillon (|max|) par canal d'un buffer stéréo entrelacé. `(0,0)` si
/// vide. Alimente le VRAI peak-mètre DAW côté browser (mètres instrument/mix/
/// master) — capte les transitoires que le RMS lisse.
fn stereo_peak(buf: &[f32]) -> (f32, f32) {
    let (mut pk_l, mut pk_r) = (0.0f32, 0.0f32);
    for pair in buf.chunks_exact(2) {
        pk_l = pk_l.max(pair[0].abs());
        pk_r = pk_r.max(pair[1].abs());
    }
    (pk_l, pk_r)
}

struct StreamState {
    jitter: JitterBuffer,
    /// Lot C — nature du flux : `Instrument` (sommé dans le mix enregistré/duckable)
    /// ou `Voice` (talkback pair, sommé post-tap/post-DIM via `voice_buf`).
    kind: StreamKind,
    /// Point 4 — armé pour le bus MIX REC. `true` = ce stream est sommé dans
    /// `mix_buf` (tap fichier enregistré + VU MIX REC) ; `false` = exclu du MIX
    /// mais TOUJOURS dans le monitoring (`output`/MASTER). Défaut `false` (rien
    /// armé). Piloté par snapshot via `set_record_arm`.
    mix_armed: bool,
    volume: f32,
    /// Pan range [-1.0, 1.0]. -1 = full left, 0 = center, +1 = full right.
    /// Loi de BALANCE stéréo linéaire dans `mix_into` (0 dB au centre :
    /// gain_L = min(1, 1−pan), gain_R = min(1, 1+pan)) — la loi standard
    /// DAW pour un signal stéréo entrant. Default 0.0 (centré).
    pan: f32,
    rms: f32,
    /// RMS par canal (L = samples pairs, R = samples impairs de l'entrelacé
    /// stéréo). Utilisé pour le VU self stéréo côté browser (2 barres L/R
    /// indépendantes). `rms` reste le niveau global (back-compat peers).
    rms_l: f32,
    rms_r: f32,
    /// Pic échantillon par canal (|max| sur le dernier bloc poussé) — alimente
    /// le peak-mètre DAW du browser (barre = pic, pas RMS). Pré-pan comme rms_*.
    peak_l: f32,
    peak_r: f32,
    /// Snapshot du `overflow_drops` du jitter au précédent push, pour ne
    /// loguer que sur événement (rate-limited via puissance de 2).
    last_overflow_drops: u64,
    buffer_full_count: u64,
    /// Idem pour le drift drain (pull-side).
    last_drift_drops: u64,
    drift_drain_count: u64,
    /// Sprint S6 — timestamps des drift drains observés sur la fenêtre
    /// glissante `UNSTABLE_WINDOW_SECS`. Purgé à chaque ajout (= cold path
    /// car ~1 drain max par 2 s). Sert à détecter un peer "instable" qui
    /// envoie en bursts (encoder stalls côté lui, Opus DTX, CPU saturé, etc.).
    /// `VecDeque` pré-alloué cap 32 = couvre une fenêtre 30 s sans
    /// réallocation tant que le burst rate < 1 drain/s.
    drift_drain_history: std::collections::VecDeque<std::time::Instant>,
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            temp_buf: Vec::new(),
            default_target_ms: None,
            record_tx: None,
            master_gain: 1.0,
            dim_factor: 1.0,
            reference: ReferenceSource::new(),
            master_rms_l: 0.0,
            master_rms_r: 0.0,
            mix_rms_l: 0.0,
            mix_rms_r: 0.0,
            master_peak_l: 0.0,
            master_peak_r: 0.0,
            mix_peak_l: 0.0,
            mix_peak_r: 0.0,
            mix_buf: Vec::new(),
            voice_buf: Vec::new(),
            voice_gain: 1.0,
            voice_pan: 0.0,
            inbound_voice_rms: 0.0,
        }
    }

    /// Point 3 (Lot 2) — RMS L/R du MASTER (sortie finale) et du MIX (tap
    /// post-fader), mesurés au dernier `mix_into`. `(master_l, master_r,
    /// mix_l, mix_r)`. Lus par le sender `stream-levels` → VU MASTER/MIX
    /// stéréo réels côté browser (pan + faders visibles).
    pub fn master_mix_rms(&self) -> (f32, f32, f32, f32) {
        (self.master_rms_l, self.master_rms_r, self.mix_rms_l, self.mix_rms_r)
    }

    /// Pic échantillon L/R du MASTER et du MIX (parité `master_mix_rms`, pour le
    /// peak-mètre DAW). `(master_peak_l, master_peak_r, mix_peak_l, mix_peak_r)`.
    pub fn master_mix_peak(&self) -> (f32, f32, f32, f32) {
        (self.master_peak_l, self.master_peak_r, self.mix_peak_l, self.mix_peak_r)
    }

    /// DIM factor (= ducking des instruments quand le user veut entendre
    /// la voix talkback clairement). Plage [0.0, 1.0], typiquement 0.25
    /// (-12dB) ou 1.0 (off). Clamp défensif côté agent.
    pub fn set_dim(&mut self, factor: f32) {
        self.dim_factor = if factor.is_finite() { factor.clamp(0.0, 1.0) } else { 1.0 };
    }

    /// REC-3 : armer/désarmer l'enregistrement. Quand `Some(tx)`, les tap
    /// sites (push_self_samples, push_samples remote, mix_into) envoient
    /// leurs samples au thread record via `try_send` non-bloquant. Quand
    /// `None`, les taps sont no-op (1 if check). Appelé depuis le pipeline
    /// au start_recording / stop_recording.
    pub fn set_record_tx(&mut self, tx: Option<Sender<RecordCmd>>) {
        self.record_tx = tx;
    }

    /// Point 4 — applique l'armement MIX REC en SNAPSHOT (état complet poussé
    /// par le web à chaque mutation). Ne somme dans le bus MIX (fichier + VU)
    /// que le self-monitor si `self_armed` et les pairs dont le producer_id est
    /// dans `armed_peers`. Tous les autres instruments passent à `mix_armed =
    /// false` — donc un désarmement ou un peer retiré de la liste est appliqué
    /// sans état résiduel. N'affecte JAMAIS le monitoring/MASTER (`output`).
    pub fn set_record_arm(&mut self, self_armed: bool, armed_peers: &[String]) {
        for (id, stream) in self.streams.iter_mut() {
            stream.mix_armed = if id == SELF_MONITOR_ID {
                self_armed
            } else {
                armed_peers.iter().any(|p| p == id)
            };
        }
    }

    /// Master gain global appliqué dans `mix_into`. Clamp défensif dans
    /// [0.0, 1.5] (NaN devient 1.0 — unity). Le tap record push_mix
    /// reçoit l'output APRÈS application, donc le fichier MIX reflète
    /// le réglage master fader courant.
    pub fn set_master_gain(&mut self, gain: f32) {
        self.master_gain = if gain.is_finite() { gain.clamp(0.0, 1.5) } else { 1.0 };
    }

    /// Lot C — gain du BUS voix (talkback pairs). Tranche unique : le web envoie
    /// le gain EFFECTIF (valeur du fader, ou 0.0 pour le mute « M »). Clamp
    /// [0.0, 1.5] (parité faders ; NaN → 1.0).
    pub fn set_peer_voice_gain(&mut self, gain: f32) {
        self.voice_gain = if gain.is_finite() { gain.clamp(0.0, 1.5) } else { 1.0 };
    }

    /// Lot C — balance du BUS voix, [-1.0, 1.0] (parité `voicePanNode`). NaN → 0.0.
    pub fn set_peer_voice_pan(&mut self, pan: f32) {
        self.voice_pan = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
    }

    /// Lot C — RMS mono de la voix des pairs effectivement mixée (post gain de
    /// bus), pour le VU voix navigateur en mode agent. `0.0` si aucune voix.
    pub fn inbound_voice_rms(&self) -> f32 {
        self.inbound_voice_rms
    }

    /// Helper interne : try_send vers le record thread sans bloquer.
    /// Drop silencieux si le channel est plein (thread en retard) — le warn
    /// est émis côté thread record qui surveille sa queue length.
    fn record_send(&self, cmd: RecordCmd) {
        if let Some(tx) = &self.record_tx {
            let _ = tx.try_send(cmd);
        }
    }

    /// Add a new remote stream.
    /// Ajoute un stream entrant. `kind` (Lot C) route son mixage : `Instrument`
    /// = mix enregistré/duckable ; `Voice` = talkback pair, sommé post-tap/post-DIM.
    pub fn add_stream(&mut self, producer_id: &str, kind: StreamKind) {
        let mut jitter = JitterBuffer::new();
        if let Some(ms) = self.default_target_ms {
            jitter.set_target_ms(ms);
        }
        self.streams.insert(producer_id.to_string(), StreamState {
            jitter,
            kind,
            mix_armed: false,
            volume: 1.0,
            pan: 0.0,
            rms: 0.0,
            rms_l: 0.0,
            rms_r: 0.0,
            peak_l: 0.0,
            peak_r: 0.0,
            last_overflow_drops: 0,
            buffer_full_count: 0,
            last_drift_drops: 0,
            drift_drain_count: 0,
            drift_drain_history: std::collections::VecDeque::with_capacity(32),
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
    /// Jitter target = `SELF_MONITOR_TARGET_MS` (A-lite : 3 ms) : signal local
    /// sans gigue réseau, on prend le minimum LOCAL stable. Latence ear-to-ear
    /// self résultante ≈ buffers device (in+out) + 3 ms target ≈ ~5-6 ms à 64
    /// (vs ~7-8 ms avec l'ancien plancher 5 ms). L'adaptation bornée reste le filet.
    pub fn add_local_stream(&mut self) {
        let mut jitter = JitterBuffer::new();
        // A-lite : `set_local_mode` AVANT `set_target_ms` pour que le clamp du
        // plancher utilise `LOCAL_MIN_TARGET_MS` (3 ms) et non `MIN_TARGET_MS` (5).
        // Chantier C — mode local : concealment des trous (pas de clic sur les
        // spikes plugin) + adaptation bornée (latence plafonnée, retour au plancher).
        jitter.set_local_mode(true);
        jitter.set_target_ms(SELF_MONITOR_TARGET_MS);
        self.streams.insert(SELF_MONITOR_ID.to_string(), StreamState {
            jitter,
            kind: StreamKind::Instrument, // self-monitor = instrument (enregistré/duckable)
            mix_armed: false,
            volume: 0.0,
            pan: 0.0,
            rms: 0.0,
            rms_l: 0.0,
            rms_r: 0.0,
            peak_l: 0.0,
            peak_r: 0.0,
            last_overflow_drops: 0,
            buffer_full_count: 0,
            last_drift_drops: 0,
            drift_drain_count: 0,
            drift_drain_history: std::collections::VecDeque::with_capacity(32),
        });
    }

    /// Supprime le stream self-monitor (appelé depuis `stop_capture`).
    pub fn remove_local_stream(&mut self) {
        self.streams.remove(SELF_MONITOR_ID);
    }

    /// 0.5.4-18 — réinitialise le JitterBuffer du self-monitor en préservant le
    /// volume. À appeler après une discontinuité d'horloge de capture (re-init ASIO
    /// mid-session sur réveil de veille PC) : le buffer de gigue repart propre, son
    /// estimateur de drift n'est plus faussé par le trou → plus de distorsion
    /// persistante au casque. No-op si le self-monitor n'existe pas.
    pub fn reset_local_stream(&mut self) {
        if !self.streams.contains_key(SELF_MONITOR_ID) {
            return;
        }
        let volume = self
            .streams
            .get(SELF_MONITOR_ID)
            .map(|s| s.volume)
            .unwrap_or(0.0);
        self.remove_local_stream();
        self.add_local_stream();
        self.set_self_monitor_volume(volume);
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
        // REC-3 : tap stem-self. Pré-fader, post channel-split.
        // Fait APRÈS push_samples pour ne pas dépendre de l'existence du
        // stream self-monitor : on enregistre l'instrument même si le user
        // a coupé son monitor browser (mode agent typique selfMuteGain=0).
        if self.record_tx.is_some() && !samples.is_empty() {
            self.record_send(RecordCmd::PushSelf(samples.to_vec()));
        }
    }

    /// Set per-stream volume (0.0 to 1.5).
    pub fn set_volume(&mut self, producer_id: &str, volume: f32) {
        // La référence (métronome) n'est pas dans `streams` : route explicite.
        if producer_id == REFERENCE_ID {
            self.reference.set_volume(volume);
            return;
        }
        if producer_id == BACKING_ID {
            self.reference.set_backing_volume(volume);
            return;
        }
        if producer_id == PREVIEW_ID {
            self.reference.set_preview_volume(volume);
            return;
        }
        // Garde NaN alignée sur set_pan/set_dim/set_master_gain :
        // NaN.clamp() = NaN → silence définitif du stream sinon.
        let v = if volume.is_finite() { volume.clamp(0.0, 1.5) } else { 1.0 };
        if let Some(stream) = self.streams.get_mut(producer_id) {
            stream.volume = v;
        }
    }

    /// Set per-stream volume by producer_id (alias for set_volume).
    pub fn set_stream_volume(&mut self, producer_id: &str, volume: f32) {
        self.set_volume(producer_id, volume);
    }

    /// Set per-stream pan, range [-1.0, 1.0]. -1=full left, 0=center, +1=full right.
    /// Applique une loi de balance stéréo linéaire (0 dB au centre) dans
    /// `mix_into`. Pour SELF_MONITOR_ID, fonctionne pareil — le browser
    /// envoie producer_id="self".
    /// No-op si le stream n'existe pas (peer parti, race).
    pub fn set_pan(&mut self, producer_id: &str, pan: f32) {
        if producer_id == REFERENCE_ID {
            self.reference.set_pan(pan);
            return;
        }
        if producer_id == BACKING_ID {
            self.reference.set_backing_pan(pan);
            return;
        }
        if producer_id == PREVIEW_ID {
            self.reference.set_preview_pan(pan);
            return;
        }
        let p = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
        if let Some(stream) = self.streams.get_mut(producer_id) {
            stream.pan = p;
        }
    }

    /// Configure la source référence (métronome via l'agent — Option B).
    /// Handler `reference-config`. Les `String` wire (`sound`/`figure`) sont
    /// parsées côté ws_server en `MetroSound`/`Figure` pour garder cette API typée.
    #[allow(clippy::too_many_arguments)]
    pub fn set_reference_config(
        &mut self,
        enabled: bool,
        volume: f32,
        pan: f32,
        bpm: f32,
        pulse_ratio: f64,
        beats_per_bar: u32,
        accent_pattern: &[u8],
        beats_per_accent: u32,
        sound: MetroSound,
        figure: Figure,
        anchor_beat_frame: f64,
        anchor_beat_index: u64,
    ) {
        self.reference.set_config(
            enabled,
            volume,
            pan,
            bpm,
            pulse_ratio,
            beats_per_bar,
            accent_pattern,
            beats_per_accent,
            sound,
            figure,
            anchor_beat_frame,
            anchor_beat_index,
        );
    }

    /// Re-ancrage périodique de la grille référence (= DLL) — handler `reference-grid`.
    pub fn set_reference_grid(&mut self, anchor_beat_frame: f64, anchor_beat_index: u64) {
        self.reference.set_grid(anchor_beat_frame, anchor_beat_index);
    }

    /// Arrête la référence et coupe ses voix — handler `reference-stop`.
    pub fn reference_stop(&mut self) {
        self.reference.stop();
    }

    /// Ancre échantillon↔mural courante (frame de sortie ↔ instant monotone
    /// agent) pour construire `reference-clock-pong`.
    pub fn output_anchor(&self) -> OutputAnchor {
        self.reference.anchor()
    }

    // ─── Backing (B4) — délégué à la source référence ─────────────────────────
    pub fn backing_begin(&mut self, total_frames: usize) {
        self.reference.backing_begin(total_frames);
    }
    pub fn backing_push(&mut self, samples: &[f32]) {
        self.reference.backing_push(samples);
    }
    pub fn backing_end(&mut self) {
        self.reference.backing_end();
    }
    pub fn backing_unload(&mut self) {
        self.reference.backing_unload();
    }
    pub fn backing_play(&mut self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.backing_play(anchor_backing_frame, anchor_output_frame);
    }
    pub fn backing_pause(&mut self) {
        self.reference.backing_pause();
    }
    pub fn backing_seek(&mut self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.backing_seek(anchor_backing_frame, anchor_output_frame);
    }
    pub fn backing_sync(&mut self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.backing_sync(anchor_backing_frame, anchor_output_frame);
    }

    // ─── Preview (Lot D) — délégué à la source référence (buffer séparé) ───────
    pub fn preview_begin(&mut self, total_frames: usize) {
        self.reference.preview_begin(total_frames);
    }
    pub fn preview_push(&mut self, samples: &[f32]) {
        self.reference.preview_push(samples);
    }
    pub fn preview_end(&mut self) {
        self.reference.preview_end();
    }
    pub fn preview_unload(&mut self) {
        self.reference.preview_unload();
    }
    pub fn preview_play(&mut self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.preview_play(anchor_backing_frame, anchor_output_frame);
    }
    pub fn preview_pause(&mut self) {
        self.reference.preview_pause();
    }
    pub fn preview_seek(&mut self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.preview_seek(anchor_backing_frame, anchor_output_frame);
    }
    pub fn preview_sync(&mut self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.preview_sync(anchor_backing_frame, anchor_output_frame);
    }

    /// Phase B — transmet la gigue réseau mesurée (RFC 3550, ms) au jitter
    /// buffer du stream pour piloter sa cible prédictive. No-op si le stream
    /// n'existe pas (peer parti) ou s'il est en override manuel (cf.
    /// [`JitterBuffer::observe_jitter`]). Appelé par la recv task à cadence
    /// réduite (pas à chaque paquet) pour limiter la contention du lock mixer.
    pub fn observe_jitter(&mut self, producer_id: &str, jitter_tail_ms: f64) {
        if let Some(stream) = self.streams.get_mut(producer_id) {
            stream.jitter.observe_jitter(jitter_tail_ms);
        }
    }

    /// P1 (01/07) — repart propre après un rétablissement audio (reset ASIO).
    /// Vide les jitter buffers du périmé accumulé pendant le gel de sortie et les
    /// re-prime à la cible de démarrage (cf. [`JitterBuffer::reset_for_recovery`]).
    /// Appelé par la pipeline juste après une reconstruction réussie des streams.
    /// Tous les streams (pairs + self-monitor) : le trou du reset rompt déjà la
    /// continuité → aucun artefact ajouté.
    pub fn reset_streams_for_recovery(&mut self) {
        for stream in self.streams.values_mut() {
            stream.jitter.reset_for_recovery();
        }
    }

    /// Push decoded samples into a stream's jitter buffer.
    ///
    /// Le jitter buffer applique drop-oldest sur overflow (cf. `JitterBuffer::push`).
    /// Ici on rate-limit le warn sur l'INCRÉMENT de `overflow_drops` pour ne
    /// pas spammer (1 stream * 400 pkt/s en burst peut générer beaucoup
    /// d'événements).
    pub fn push_samples(&mut self, producer_id: &str, samples: &[f32]) {
        // REC-3 : tap stem-peer. Pre-fader (avant `vol *` dans mix_into),
        // post Opus decode. On filtre SELF_MONITOR_ID car push_self_samples
        // émet déjà son propre PushSelf (sinon double tap pour self). Lot C : on
        // filtre aussi les flux VOIX — le talkback des pairs n'est jamais
        // enregistré (parité `voiceBus` navigateur, hors `instrumentMixBus`). Le
        // lookup de `kind` est court-circuité hors enregistrement (record_tx None).
        // Défaut SÛR (defense-in-depth) : un stream INCONNU (pas encore `add_stream`)
        // n'est PAS enregistré → `is_some_and` (et non `is_none_or`). En pratique
        // `add_stream(kind)` précède toujours `push_samples`, mais on ne veut pas que
        // l'invariant « la voix n'est jamais enregistrée » dépende de cet ordre : si
        // un futur chemin poussait des samples avant l'enregistrement du stream, un
        // fragment de voix ne doit pas fuiter dans le stem.
        if self.record_tx.is_some()
            && producer_id != SELF_MONITOR_ID
            && !samples.is_empty()
            && self.streams.get(producer_id).is_some_and(|s| s.kind != StreamKind::Voice)
        {
            self.record_send(RecordCmd::PushPeer(producer_id.to_string(), samples.to_vec()));
        }

        if let Some(stream) = self.streams.get_mut(producer_id) {
            // Compute RMS of pushed samples (global + par canal L/R).
            if !samples.is_empty() {
                let n = samples.len();
                let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
                stream.rms = (sum_sq / n as f32).sqrt();
                // Pic global (|max|) — fallback pour le cas dégénéré mono.
                let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                // L = samples pairs, R = samples impairs (entrelacé stéréo). Si
                // longueur impaire (cas dégénéré), on retombe sur le global.
                if n >= 2 && n.is_multiple_of(2) {
                    let half = (n / 2) as f32;
                    let mut sq_l = 0.0f32;
                    let mut sq_r = 0.0f32;
                    let (mut pk_l, mut pk_r) = (0.0f32, 0.0f32);
                    for pair in samples.chunks_exact(2) {
                        sq_l += pair[0] * pair[0];
                        sq_r += pair[1] * pair[1];
                        pk_l = pk_l.max(pair[0].abs());
                        pk_r = pk_r.max(pair[1].abs());
                    }
                    stream.rms_l = (sq_l / half).sqrt();
                    stream.rms_r = (sq_r / half).sqrt();
                    stream.peak_l = pk_l;
                    stream.peak_r = pk_r;
                } else {
                    stream.rms_l = stream.rms;
                    stream.rms_r = stream.rms;
                    stream.peak_l = peak;
                    stream.peak_r = peak;
                }
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
                // Sprint S6 — track ce drain dans la fenêtre glissante 30 s
                // pour la détection peer instable. Push timestamp.
                // (Purge à la lecture côté `stream_unstable_events`.)
                stream.drift_drain_history.push_back(std::time::Instant::now());
                // Garde-fou anti-mémoire : si jamais la fenêtre n'est pas
                // purgée (= caller oublie de call stream_unstable_events),
                // on cap à 256 entrées (= ~1 min de drains à 4 Hz, suffisant
                // pour signaler une instabilité massive).
                while stream.drift_drain_history.len() > 256 {
                    stream.drift_drain_history.pop_front();
                }
                // Bug D : on logue uniquement les drains sévères (events > 4).
                // Les small drifts (1-4) sont normaux et bruyaient le log
                // sans signal. Combiné avec is_power_of_two, on logue à
                // events = 8, 16, 32, 64… → réduction ~70 % du spam.
                if stream.drift_drain_count > 4
                    && stream.drift_drain_count.is_power_of_two()
                {
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
        // Lot C — accumulateur voix (talkback pairs), sommé plus bas post-tap/post-DIM.
        if self.voice_buf.len() != output.len() {
            self.voice_buf.resize(output.len(), 0.0);
        }
        self.voice_buf.fill(0.0);
        let mut any_voice = false;
        // Point 4 — accumulateur MIX REC (instruments ARMÉS), rempli en passe 1
        // en parallèle de `output`. Remis à zéro à chaque bloc (comme voice_buf).
        if self.mix_buf.len() != output.len() {
            self.mix_buf.resize(output.len(), 0.0);
        }
        self.mix_buf.fill(0.0);

        // PASSE 1 — chaque stream est pull UNE fois (le jitter buffer se consomme) :
        //   - INSTRUMENT/self → sommé (fader+balance) dans `output` ;
        //   - VOICE (talkback pair) → accumulé BRUT dans `voice_buf` (pas de
        //     fader/pan par pair : tranche unique, cf. décision Lot C) — le
        //     gain/pan de bus s'applique après le DIM.
        for stream in self.streams.values_mut() {
            stream.jitter.pull(&mut self.temp_buf);

            if stream.kind == StreamKind::Voice {
                any_voice = true;
                for (v, &sample) in self.voice_buf.iter_mut().zip(self.temp_buf.iter()) {
                    *v += sample;
                }
                continue;
            }

            let vol = stream.volume;
            // Point 4 — un instrument armé alimente AUSSI le bus MIX REC
            // (`mix_buf`) avec exactement le même échantillon post-fader/pan que
            // le monitoring. Non armé → seul `output` (monitoring/MASTER) reçoit.
            let armed = stream.mix_armed;
            // Balance stéréo — loi LINÉAIRE 0 dB au centre. Les streams sont
            // STÉRÉO interleaved (L,R,L,R…) : ce contrôle est un *balance*,
            // pas un pan mono → la loi correcte atténue un canal sans toucher
            // l'autre (standard DAW pour pistes stéréo). Continue partout :
            // centre = 1.0/1.0 (identique au fast-path), extrêmes = 1.0/0.0.
            // L'ancienne constant-power (cos/sin de (pan+1)·π/4) n'était pas
            // normalisée au centre → saut de −3 dB sur les DEUX canaux dès
            // que le fader quittait pan=0 exact (bug audible, review 11/06).
            if stream.pan.abs() < f32::EPSILON {
                let n = self.temp_buf.len().min(output.len());
                if armed {
                    // `output` (monitoring) ET `mix_buf` (MIX REC) reçoivent le
                    // même échantillon post-fader — calculé une fois.
                    for ((o, mb), &t) in output[..n]
                        .iter_mut()
                        .zip(self.mix_buf[..n].iter_mut())
                        .zip(self.temp_buf[..n].iter())
                    {
                        let s = t * vol;
                        *o += s;
                        *mb += s;
                    }
                } else {
                    for (o, &t) in output[..n].iter_mut().zip(self.temp_buf[..n].iter()) {
                        *o += t * vol;
                    }
                }
            } else {
                let (gl, gr) = pan_gains(stream.pan);
                let gain_l = vol * gl;
                let gain_r = vol * gr;
                let mut i = 0;
                while i + 1 < self.temp_buf.len() && i + 1 < output.len() {
                    let l = self.temp_buf[i] * gain_l;
                    let r = self.temp_buf[i + 1] * gain_r;
                    output[i] += l;
                    output[i + 1] += r;
                    if armed {
                        self.mix_buf[i] += l;
                        self.mix_buf[i + 1] += r;
                    }
                    i += 2;
                }
            }
        }

        // Log mixed output RMS every ~20 seconds (48000*2 / 256 ≈ 375 calls/s)
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if c.is_multiple_of(7500) && !self.streams.is_empty() {
            let rms: f32 = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
            tracing::debug!(target: "jamodio::mixer", streams = self.streams.len(), rms, "mix_into heartbeat");
        }

        // REC-3 / Point 4 : tap MIX = `mix_buf` (instruments ARMÉS uniquement),
        // pré-dim/master (parité browser `instrumentMixBus` post-`armGain`).
        // Sémantique : le fichier MIX enregistré ne contient QUE les pistes
        // armées, post-fader, indépendamment de mon écoute locale (dim/master)
        // et du monitoring (`output` = tous les instruments). Rien armé →
        // `mix_buf` silencieux → fichier MIX silencieux.
        if self.record_tx.is_some() {
            self.record_send(RecordCmd::PushMix(self.mix_buf.clone()));
        }

        // Point 3/4 — RMS + pic L/R du MIX REC (instruments ARMÉS post-fader,
        // pré-dim/master) pour le VU MIX REC stéréo. Mesuré sur `mix_buf` → le VU
        // reflète EXACTEMENT ce qui sera enregistré (rien armé → VU au plancher,
        // distinct du MASTER).
        let (mix_l, mix_r) = stereo_rms(&self.mix_buf);
        self.mix_rms_l = mix_l;
        self.mix_rms_r = mix_r;
        let (mix_pk_l, mix_pk_r) = stereo_peak(&self.mix_buf);
        self.mix_peak_l = mix_pk_l;
        self.mix_peak_r = mix_pk_r;

        // DIM factor — atténue les instruments quand l'user veut entendre le
        // talkback clairement. Skip si == 1.0 (cas par défaut majoritaire).
        if (self.dim_factor - 1.0).abs() > f32::EPSILON {
            let d = self.dim_factor;
            for sample in output.iter_mut() {
                *sample *= d;
            }
        }

        // Lot C — VOIX des pairs (talkback). Sommée ICI, exactement comme la
        // référence : APRÈS le tap RECORD (⇒ jamais enregistrée) et APRÈS le DIM
        // (⇒ jamais duckée : le talkback reste clair pendant que les instruments
        // baissent), AVANT le master. Gain/pan de BUS unique (parité voiceGain/
        // voicePanNode navigateur ; le mute = gain 0 envoyé par le web). Le VU
        // voix lit `inbound_voice_rms` (RMS scalaire agrégé L+R, post-gain de bus).
        if any_voice && self.voice_gain > f32::EPSILON {
            let (gl, gr) = pan_gains(self.voice_pan);
            let gain_l = self.voice_gain * gl;
            let gain_r = self.voice_gain * gr;
            let mut i = 0;
            while i + 1 < self.voice_buf.len() && i + 1 < output.len() {
                output[i]     += self.voice_buf[i]     * gain_l;
                output[i + 1] += self.voice_buf[i + 1] * gain_r;
                i += 2;
            }
            // RMS scalaire agrégé sur L+R, post-gain de bus (avant pan), pour le VU
            // voix navigateur.
            let frames = self.voice_buf.len() / 2;
            if frames > 0 {
                let sum_sq: f32 = self.voice_buf.iter().map(|s| s * s).sum();
                self.inbound_voice_rms = (sum_sq / self.voice_buf.len() as f32).sqrt() * self.voice_gain;
            } else {
                self.inbound_voice_rms = 0.0;
            }
        } else {
            self.inbound_voice_rms = 0.0;
        }

        // Référence (métronome via l'agent — Option B). Ajoutée ICI, à un point
        // DÉDIÉ hors de la boucle streams :
        //   - APRÈS le tap record push_mix ⇒ EXCLUE du MIX enregistré (parité
        //     browser : métro/backing hors `instrumentMixBus`) ;
        //   - APRÈS le DIM ⇒ le clic n'est PAS ducké par le talkback (parité
        //     browser : métro→masterBus direct, pas via `instrumentDimGain`) ;
        //   - AVANT le master + le clamp ⇒ suit le fader master et reste borné.
        // Appelée à CHAQUE bloc (même métro coupé) pour tenir à jour l'ancre
        // échantillon↔mural exposée au browser (`output_anchor`).
        self.reference.advance_and_generate(output, mono_now_ms());

        // Master gain global (fader MASTER côté UI). Appliqué AVANT le clamp
        // pour qu'un master à 0.5 atténue proprement un mix qui aurait dépassé
        // 1.0 (le clamp final ramène quand même dans [-1, 1] pour le DAC).
        // Skip multiplication si gain == 1.0 (cas par défaut, économise N muls
        // sur le hot path callback CPAL).
        if (self.master_gain - 1.0).abs() > f32::EPSILON {
            let g = self.master_gain;
            for sample in output.iter_mut() {
                *sample *= g;
            }
        }

        // Soft clamp to prevent distortion
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        // Point 3 — RMS L/R du MASTER (sortie finale = ce que l'utilisateur
        // entend, post dim/master/clamp) pour le VU MASTER stéréo.
        let (master_l, master_r) = stereo_rms(output);
        self.master_rms_l = master_l;
        self.master_rms_r = master_r;
        let (master_pk_l, master_pk_r) = stereo_peak(output);
        self.master_peak_l = master_pk_l;
        self.master_peak_r = master_pk_r;

        // Report drift drains (rate-limité à puissances de 2). Coût formatage
        // négligeable hors événement (1 if + un getter atomic-free par stream).
        self.report_drift_drops();
    }

    /// Niveaux par stream pour les VU du browser.
    /// Retourne `(producer_id, rms_global, rms_l, rms_r, peak_l, peak_r)` par
    /// stream (RMS + pic échantillon, tous POST-pan). Le global RMS reste utilisé
    /// pour les VU peers mono ; L/R alimentent le VU stéréo ; les pics alimentent
    /// le peak-mètre DAW (barre = pic). Lot C : les flux VOIX sont EXCLUS (les VU
    /// pairs = instruments+self seuls) — la voix est un agrégat unique remonté par
    /// `inbound_voice_rms()` (tranche voix navigateur, RMS).
    pub fn stream_levels(&self) -> Vec<(String, f32, f32, f32, f32, f32)> {
        self.streams.iter()
            .filter(|(_, stream)| stream.kind != StreamKind::Voice)
            .map(|(id, stream)| {
                // VU POST-pan : on applique la MÊME loi de balance que `mix_into`
                // aux niveaux L/R (stockés pré-pan) → le VU reflète le placement
                // stéréo exactement comme le rendu (self + peers). Un flux mono a
                // `rms_l = rms_r = rms` (idem pic) en amont ; le pan crée l'asymétrie.
                let (gl, gr) = pan_gains(stream.pan);
                (
                    id.clone(),
                    stream.rms,
                    stream.rms_l * gl,
                    stream.rms_r * gr,
                    stream.peak_l * gl,
                    stream.peak_r * gr,
                )
            }).collect()
    }

    /// Sprint S6 — purge la fenêtre glissante de drift drains et retourne
    /// les peers REMOTE dont le compte d'events sur la fenêtre dépasse
    /// `threshold`. Self-monitor exclu (= ses drains reflètent overload
    /// agent local, pas un peer distant instable — cf. AgentPipelineOverload).
    ///
    /// Retourne `(producer_id, drift_drains_window, drift_drains_total)`.
    pub fn stream_unstable_events(
        &mut self,
        window: std::time::Duration,
        threshold: usize,
    ) -> Vec<(String, usize, u64)> {
        let now = std::time::Instant::now();
        let cutoff = now.checked_sub(window).unwrap_or(now);
        let mut out = Vec::new();
        for (producer_id, stream) in self.streams.iter_mut() {
            if producer_id.as_str() == SELF_MONITOR_ID {
                continue;
            }
            // Purge les timestamps hors fenêtre (= plus anciens que cutoff).
            while let Some(&front) = stream.drift_drain_history.front() {
                if front < cutoff {
                    stream.drift_drain_history.pop_front();
                } else {
                    break;
                }
            }
            let events_window = stream.drift_drain_history.len();
            if events_window > threshold {
                out.push((
                    producer_id.clone(),
                    events_window,
                    stream.drift_drain_count,
                ));
            }
        }
        out
    }

    /// Sprint S1 — snapshot perf par stream remote (self-monitor exclu).
    /// Retourne (producer_id, underruns_cumul, drift_drops_cumul, target_ms_courant).
    /// Counters monotones depuis la création du stream — le browser fait la
    /// différence entre 2 snapshots s'il veut une cadence par seconde.
    pub fn stream_perf_stats(&self) -> Vec<(String, u64, u64, usize)> {
        self.streams
            .iter()
            .filter(|(k, _)| k.as_str() != SELF_MONITOR_ID)
            .map(|(id, s)| {
                (
                    id.clone(),
                    s.jitter.underruns(),
                    s.jitter.drift_drops(),
                    s.jitter.target_ms(),
                )
            })
            .collect()
    }

    /// Chantier C (v0.4.14) — stats du self-monitor pour diagnostic : latence
    /// courante du buffer (ms) + underruns cumulés. Permet de visualiser que
    /// la latence monitoring grandit transitoirement sous les spikes plugin
    /// (≤ LOCAL_MAX_TARGET_MS) et revient à ~5 ms au calme. (0, 0) si le
    /// self-monitor n'est pas actif (pas de capture en cours).
    pub fn self_monitor_stats(&self) -> (usize, u64) {
        match self.streams.get(SELF_MONITOR_ID) {
            Some(s) => (s.jitter.target_ms(), s.jitter.underruns()),
            None => (0, 0),
        }
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

    /// Override la target_ms de tous les streams PEERS existants ET stocke la
    /// valeur comme défaut pour les futurs streams. Appelé par le handler
    /// SetBuffer.
    ///
    /// EXCLUT le self-monitor : son buffer est local (pas de réseau → pas de
    /// gigue) et plafonné bas (latence d'écoute de son propre instrument).
    /// L'inclure faisait passer le monitoring de ~5 ms à la target réseau
    /// (ex. 40 ms = 8×) + un trou audible au re-prime (review 11/06).
    pub fn set_target_ms_all(&mut self, target_ms: usize) {
        self.default_target_ms = Some(target_ms);
        for (id, stream) in self.streams.iter_mut() {
            if id == SELF_MONITOR_ID {
                continue;
            }
            stream.jitter.set_target_ms(target_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_par_canal_l_r_independants() {
        let mut m = AudioMixer::new();
        m.add_stream("p1", StreamKind::Instrument);
        // Entrelacé stéréo : L = 1.0 partout, R = 0.0 partout.
        let mut s = Vec::new();
        for _ in 0..100 { s.push(1.0); s.push(0.0); }
        m.push_samples("p1", &s);
        let (_, rms, rms_l, rms_r, _, _) = m.stream_levels().into_iter().find(|(id, ..)| id == "p1").unwrap();
        assert!((rms_l - 1.0).abs() < 1e-4, "rms_l ≈ 1 (canal gauche plein)");
        assert!(rms_r.abs() < 1e-4, "rms_r ≈ 0 (canal droit silencieux)");
        // rms global = sqrt(moyenne sur tous) = sqrt(0.5) ≈ 0.707.
        assert!((rms - 0.5f32.sqrt()).abs() < 1e-3, "rms global = sqrt(0.5)");
    }

    /// Point 3 (Lot 2) — le VU doit refléter le pan : un flux MONO (L=R en amont,
    /// comme l'instrument self) doit ressortir asymétrique dans `stream_rms` une
    /// fois pané (le rendu `mix_into` et le VU partagent `pan_gains`).
    #[test]
    fn vu_rms_reflete_le_pan_mono() {
        let mut m = AudioMixer::new();
        m.add_stream("p1", StreamKind::Instrument);
        // Source mono dupliquée : L = R = 1.0 (200 samples = 100 frames stéréo).
        let s = vec![1.0f32; 200];
        m.push_samples("p1", &s);
        let lr = |m: &AudioMixer| {
            let (_, _, l, r, _, _) = m.stream_levels().into_iter().find(|(id, ..)| id == "p1").unwrap();
            (l, r)
        };
        // Centre : L = R.
        let (l0, r0) = lr(&m);
        assert!((l0 - r0).abs() < 1e-4, "centre : rms_l = rms_r");
        // Full left : rms_r muet.
        m.set_pan("p1", -1.0);
        let (ll, rl) = lr(&m);
        assert!((ll - 1.0).abs() < 1e-4, "full left : rms_l ≈ 1, got {ll}");
        assert!(rl.abs() < 1e-4, "full left : rms_r ≈ 0, got {rl}");
        // Full right : symétrique.
        m.set_pan("p1", 1.0);
        let (lr_, rr) = lr(&m);
        assert!(lr_.abs() < 1e-4, "full right : rms_l ≈ 0, got {lr_}");
        assert!((rr - 1.0).abs() < 1e-4, "full right : rms_r ≈ 1, got {rr}");
    }

    /// Peak-mètre DAW — le PIC échantillon capté par stream doit dépasser le RMS
    /// sur un signal à transitoires, et suivre le pan comme le RMS. Ici : L plein
    /// (1.0) avec un transitoire à 1.0 → peak_l ≈ 1 > rms_l ; R quasi nul.
    #[test]
    fn stream_peak_capte_le_transitoire_et_le_pan() {
        let mut m = AudioMixer::new();
        m.add_stream("p1", StreamKind::Instrument);
        // Canal L : rampe faible (RMS bas) + 1 transitoire plein ; canal R : nul.
        let mut s = Vec::new();
        for i in 0..100 {
            s.push(if i == 50 { 1.0 } else { 0.05 }); // L : bruit faible + 1 pic
            s.push(0.0);                                // R : silence
        }
        m.push_samples("p1", &s);
        let (_, _, rms_l, _, peak_l, peak_r) =
            m.stream_levels().into_iter().find(|(id, ..)| id == "p1").unwrap();
        assert!((peak_l - 1.0).abs() < 1e-4, "peak_l capte le transitoire (≈1), got {peak_l}");
        assert!(peak_l > rms_l + 0.5, "peak_l ({peak_l}) >> rms_l ({rms_l}) — le pic voit le transitoire que le RMS lisse");
        assert!(peak_r.abs() < 1e-4, "peak_r ≈ 0 (canal droit muet), got {peak_r}");
        // Pané full-left → peak_r reste nul, peak_l conservé.
        m.set_pan("p1", -1.0);
        m.push_samples("p1", &s);
        let (_, _, _, _, pl, pr) = m.stream_levels().into_iter().find(|(id, ..)| id == "p1").unwrap();
        assert!((pl - 1.0).abs() < 1e-4, "full left : peak_l ≈ 1, got {pl}");
        assert!(pr.abs() < 1e-4, "full left : peak_r ≈ 0, got {pr}");
    }

    /// Peak-mètre DAW — MASTER/MIX : le pic de sortie doit dépasser le RMS sur un
    /// signal à transitoires, et être exposé par `master_mix_peak()`.
    #[test]
    fn master_mix_peak_expose_le_pic_de_sortie() {
        let mut m = AudioMixer::new();
        m.add_stream("p1", StreamKind::Instrument);
        m.set_record_arm(false, &["p1".to_string()]); // Point 4 : armer pour peupler le MIX
        let mut out = vec![0.0f32; 512];
        // Gros buffer (amorce le jitter) : faible (0.05) partout + transitoires
        // pleins réguliers sur L ET R → toute fenêtre de sortie en contient.
        let mut s = vec![0.05f32; 48_000];
        let mut i = 0;
        while i + 1 < s.len() { s[i] = 1.0; s[i + 1] = 1.0; i += 16; }
        m.push_samples("p1", &s);
        m.mix_into(&mut out);
        let (mpl, mpr, xpl, xpr) = m.master_mix_peak();
        let (mrl, _, xrl, _) = m.master_mix_rms();
        assert!(mpl > mrl + 0.3, "master : peak ({mpl}) >> rms ({mrl})");
        assert!(xpl > xrl + 0.3, "mix : peak ({xpl}) >> rms ({xrl})");
        assert!(mpr > 0.0 && xpr > 0.0, "canal R non nul (source centrée)");
    }

    /// Point 3 (Lot 2) — le VU MASTER/MIX doit refléter le pan du mix réel :
    /// un stream centré → master L ≈ R ; pané à fond à gauche → master R ≈ 0.
    #[test]
    fn master_rms_reflete_le_mix_pane() {
        let mut m = AudioMixer::new();
        m.add_stream("p1", StreamKind::Instrument);
        m.set_record_arm(false, &["p1".to_string()]); // Point 4 : armer pour peupler le MIX
        let ones = vec![1.0f32; 48_000];
        let mut out = vec![0.0f32; 512];
        // Centre.
        m.push_samples("p1", &ones);
        m.mix_into(&mut out);
        let (ml, mr, xl, xr) = m.master_mix_rms();
        assert!((ml - mr).abs() < 1e-3, "centre : master L ≈ R ({ml} vs {mr})");
        assert!((xl - xr).abs() < 1e-3, "centre : mix L ≈ R");
        // Full left : R muet côté master ET mix.
        m.set_pan("p1", -1.0);
        m.push_samples("p1", &ones);
        m.mix_into(&mut out);
        let (ml2, mr2, xl2, xr2) = m.master_mix_rms();
        assert!(ml2 > 0.1, "full left : master L présent, got {ml2}");
        assert!(mr2 < 1e-3, "full left : master R ≈ 0, got {mr2}");
        assert!(xl2 > 0.1 && xr2 < 1e-3, "full left : mix L présent / R muet");
    }

    /// Helper : pousse un signal constant 1.0 dans un stream et mixe un bloc,
    /// retourne (gain_L_effectif, gain_R_effectif) mesurés sur la sortie.
    fn measure_gains(pan: f32) -> (f32, f32) {
        let mut m = AudioMixer::new();
        m.add_stream("p1", StreamKind::Instrument);
        m.set_pan("p1", pan);
        // Remplit le jitter buffer au-delà de sa target pour que pull()
        // rende le signal (et pas du silence de prime).
        let ones = vec![1.0f32; 48_000]; // 500 ms stéréo @ 48 kHz
        m.push_samples("p1", &ones);
        let mut out = vec![0.0f32; 512];
        m.mix_into(&mut out);
        // Gain mesuré = valeur des samples (entrée constante à 1.0).
        (out[0], out[1])
    }

    /// LOT 3 (review 11/06) — la loi de balance doit être CONTINUE au centre :
    /// l'ancienne constant-power non normalisée sautait de 1.0 à ~0.707 sur
    /// les deux canaux dès pan = ±ε (−3 dB audibles d'un cran de fader).
    #[test]
    fn pan_law_continuous_at_center() {
        let (l0, r0) = measure_gains(0.0);
        let (le, re) = measure_gains(0.001);
        assert!((l0 - 1.0).abs() < 1e-3, "centre L = unity, got {l0}");
        assert!((r0 - 1.0).abs() < 1e-3, "centre R = unity, got {r0}");
        assert!((le - l0).abs() < 0.01, "L continu au centre : {l0} vs {le}");
        assert!((re - r0).abs() < 0.01, "R continu au centre : {r0} vs {re}");
    }

    /// Extrêmes de la loi de balance : côté plein = unity, côté opposé = 0
    /// (identique à l'ancienne loi aux extrêmes → pas de changement perçu).
    #[test]
    fn pan_law_endpoints() {
        let (l, r) = measure_gains(1.0); // full right
        assert!(l.abs() < 1e-3, "full right : L muet, got {l}");
        assert!((r - 1.0).abs() < 1e-3, "full right : R unity, got {r}");
        let (l, r) = measure_gains(-1.0); // full left
        assert!((l - 1.0).abs() < 1e-3, "full left : L unity, got {l}");
        assert!(r.abs() < 1e-3, "full left : R muet, got {r}");
    }

    /// LOT 3 (review 11/06) — SetBuffer (target jitter réseau) ne doit PAS
    /// toucher le self-monitor : son buffer est local (5 ms), l'inclure
    /// multipliait la latence de monitoring par 8 (ex. target 40 ms).
    #[test]
    fn set_target_ms_all_excludes_self_monitor() {
        let mut m = AudioMixer::new();
        m.add_local_stream();
        m.add_stream("peer", StreamKind::Instrument);
        m.set_target_ms_all(40);
        let (self_target, _) = m.self_monitor_stats();
        assert_eq!(self_target, SELF_MONITOR_TARGET_MS, "self-monitor préservé");
        let peers = m.stream_perf_stats();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].3, 40, "peer prend bien la nouvelle target");
    }

    /// Garde NaN sur set_volume (alignée sur set_pan) : NaN.clamp() = NaN
    /// aurait silencé le stream définitivement.
    #[test]
    fn set_volume_rejects_nan() {
        let (l, _r) = {
            let mut m = AudioMixer::new();
            m.add_stream("p1", StreamKind::Instrument);
            m.set_volume("p1", f32::NAN);
            let ones = vec![1.0f32; 48_000];
            m.push_samples("p1", &ones);
            let mut out = vec![0.0f32; 512];
            m.mix_into(&mut out);
            (out[0], out[1])
        };
        assert!(l.is_finite() && l > 0.5, "volume NaN → fallback 1.0, got {l}");
    }

    // ─── Lot C — invariants voix des pairs (0.5.10-4) ─────────────────────────

    /// INVARIANT : la voix des pairs n'est JAMAIS duckée par le DIM. Avec DIM=0
    /// (instruments coupés), la voix reste pleinement audible en sortie (parité
    /// `voiceBus` navigateur : talkback clair pendant que les instruments baissent).
    #[test]
    fn voice_is_never_ducked_by_dim() {
        let mut m = AudioMixer::new();
        m.add_stream("inst", StreamKind::Instrument);
        m.add_stream("voice", StreamKind::Voice);
        let ones = vec![1.0f32; 48_000];
        m.push_samples("inst", &ones);
        m.push_samples("voice", &ones);
        m.set_dim(0.0); // duck TOTAL des instruments
        let mut out = vec![0.0f32; 512];
        m.mix_into(&mut out);
        let rms: f32 = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!(rms > 0.5, "voix audible malgré DIM=0 (jamais duckée), rms={rms}");
        assert!(m.inbound_voice_rms() > 0.5, "RMS voix remonté pour le VU, got {}", m.inbound_voice_rms());
    }

    /// INVARIANT : la voix des pairs n'entre JAMAIS dans le RECORD — ni dans le
    /// stem par pair (`PushPeer`), ni dans le MIX (`PushMix`). Parité `voiceBus`
    /// navigateur (talkback hors `instrumentMixBus`).
    #[test]
    fn voice_is_never_recorded() {
        use crate::record::RecordCmd;
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut m = AudioMixer::new();
        m.set_record_tx(Some(tx));
        m.add_stream("inst", StreamKind::Instrument);
        m.add_stream("voice", StreamKind::Voice);
        m.set_record_arm(false, &["inst".to_string()]); // Point 4 : instrument armé dans le MIX
        let ones = vec![1.0f32; 48_000];
        m.push_samples("inst", &ones);   // → stem PushPeer("inst")
        m.push_samples("voice", &ones);  // → PAS de stem (voix)
        let mut out = vec![0.0f32; 512];
        m.mix_into(&mut out);            // → PushMix = instruments SEULS
        let mut peer_stems = Vec::new();
        let mut mix_rms = 0.0f32;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                RecordCmd::PushPeer(id, _) => peer_stems.push(id),
                RecordCmd::PushMix(buf) => {
                    mix_rms = (buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32).sqrt();
                }
                _ => {}
            }
        }
        assert!(peer_stems.iter().any(|id| id == "inst"), "stem instrument enregistré");
        assert!(!peer_stems.iter().any(|id| id == "voice"), "stem voix JAMAIS enregistré");
        // MIX enregistré = instrument seul (rms≈1). Si la voix avait fuité dans le
        // tap (pré-voix), le mix vaudrait ~2 (deux sources à 1.0).
        assert!((mix_rms - 1.0).abs() < 0.2, "MIX enregistré = instruments seuls, voix exclue (rms={mix_rms})");
    }

    // ─── Lot D — invariants aperçu Library (preview) ──────────────────────────

    /// INVARIANT : l'aperçu Library n'est JAMAIS ducké par le DIM (mixé post-DIM,
    /// comme le backing/la référence). DIM=0 (instruments coupés) → l'aperçu reste
    /// pleinement audible.
    #[test]
    fn preview_is_never_ducked_by_dim() {
        let mut m = AudioMixer::new();
        m.add_stream("inst", StreamKind::Instrument);
        m.push_samples("inst", &vec![1.0f32; 48_000]);
        // Aperçu chargé + en lecture (constante 0.8, buffer > bloc).
        m.preview_begin(2000);
        m.preview_push(&vec![0.8f32; 4000]);
        m.preview_end();
        m.preview_play(0.0, 0.0);
        m.set_dim(0.0); // duck TOTAL des instruments
        let mut out = vec![0.0f32; 512];
        m.mix_into(&mut out);
        // Instruments duckés à 0 → seul l'aperçu reste.
        let rms: f32 = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!(rms > 0.3, "aperçu audible malgré DIM=0 (jamais ducké), rms={rms}");
    }

    /// INVARIANT : l'aperçu Library n'entre JAMAIS dans le RECORD (mixé après le
    /// tap, comme le backing). Le MIX enregistré = instruments seuls ; aucun stem
    /// "preview".
    #[test]
    fn preview_is_never_recorded() {
        use crate::record::RecordCmd;
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut m = AudioMixer::new();
        m.set_record_tx(Some(tx));
        m.add_stream("inst", StreamKind::Instrument);
        m.set_record_arm(false, &["inst".to_string()]); // Point 4 : instrument armé dans le MIX
        m.push_samples("inst", &vec![1.0f32; 48_000]);
        m.preview_begin(2000);
        m.preview_push(&vec![0.8f32; 4000]);
        m.preview_end();
        m.preview_play(0.0, 0.0);
        let mut out = vec![0.0f32; 512];
        m.mix_into(&mut out); // → PushMix = instruments SEULS
        let mut peer_stems = Vec::new();
        let mut mix_rms = 0.0f32;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                RecordCmd::PushPeer(id, _) => peer_stems.push(id),
                RecordCmd::PushMix(buf) => {
                    mix_rms = (buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32).sqrt();
                }
                _ => {}
            }
        }
        assert!(!peer_stems.iter().any(|id| id == "preview"), "aperçu JAMAIS enregistré en stem");
        // MIX = instrument seul (~1.0). Si l'aperçu (0.8) avait fuité dans le tap,
        // le mix serait sensiblement > 1.0.
        assert!((mix_rms - 1.0).abs() < 0.2, "MIX enregistré = instruments seuls, aperçu exclu (rms={mix_rms})");
    }

    // ─── Point 4 — armement du bus MIX REC (armés-seulement) ──────────────────

    /// INVARIANT : l'armement gate le bus MIX REC (fichier + VU) SANS toucher le
    /// monitoring (MASTER). Non armé → absent du MIX, présent au MASTER ; armé →
    /// présent aux deux ; désarmé → de nouveau absent (snapshot sans résidu).
    #[test]
    fn record_arm_gates_mix_not_master() {
        let mut m = AudioMixer::new();
        m.add_stream("peer", StreamKind::Instrument);
        let ones = vec![1.0f32; 48_000];
        let mut out = vec![0.0f32; 512];

        // Défaut : rien armé → MIX silencieux, MASTER présent (monitoring).
        m.push_samples("peer", &ones);
        m.mix_into(&mut out);
        let (ml, _mr, xl, _xr) = m.master_mix_rms();
        assert!(ml > 0.1, "MASTER présent même sans armement (monitoring), got {ml}");
        assert!(xl < 1e-4, "MIX silencieux tant que rien n'est armé, got {xl}");

        // Peer armé → MIX présent, MASTER inchangé.
        m.set_record_arm(false, &["peer".to_string()]);
        m.push_samples("peer", &ones);
        m.mix_into(&mut out);
        let (ml2, _, xl2, _) = m.master_mix_rms();
        assert!((ml2 - ml).abs() < 1e-3, "MASTER inchangé par l'armement ({ml} vs {ml2})");
        assert!(xl2 > 0.1, "MIX présent une fois le peer armé, got {xl2}");

        // Désarmement (snapshot vide) → MIX de nouveau silencieux, sans résidu.
        m.set_record_arm(false, &[]);
        m.push_samples("peer", &ones);
        m.mix_into(&mut out);
        let (_, _, xl3, _) = m.master_mix_rms();
        assert!(xl3 < 1e-4, "désarmement → MIX de nouveau silencieux, got {xl3}");
    }

    /// INVARIANT : `self_armed` cible bien le self-monitor (clé SELF_MONITOR_ID).
    /// Fader moi ouvert : non armé → self hors MIX ; armé → self dans le MIX.
    #[test]
    fn record_arm_targets_self_monitor() {
        let mut m = AudioMixer::new();
        m.add_local_stream(); // self-monitor (SELF_MONITOR_ID = "self")
        m.set_self_monitor_volume(1.0); // ouvrir le fader moi (défaut 0.0)
        let ones = vec![1.0f32; 48_000];
        let mut out = vec![0.0f32; 512];

        // Self non armé → MIX silencieux.
        m.push_self_samples(&ones);
        m.mix_into(&mut out);
        let (_, _, xl, _) = m.master_mix_rms();
        assert!(xl < 1e-4, "self non armé → MIX silencieux, got {xl}");

        // Self armé → MIX présent.
        m.set_record_arm(true, &[]);
        m.push_self_samples(&ones);
        m.mix_into(&mut out);
        let (_, _, xl2, _) = m.master_mix_rms();
        assert!(xl2 > 0.1, "self armé → MIX présent, got {xl2}");
    }
}
