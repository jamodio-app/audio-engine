use super::reference::{Figure, MetroSound, OutputAnchor, ReferenceSource};
use super::ring_buffer::JitterBuffer;
use crate::protocol::StreamKind;
use crate::record::RecordCmd;
use crate::sync::clock::mono_now_ms;
use crossbeam_channel::Sender;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

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

/// Sentinelle « pas de cible par défaut imposée » pour `default_target_ms`
/// (stocké en `AtomicUsize`, lu sans lock à l'ajout d'un flux). Si la valeur
/// vaut `NO_DEFAULT_TARGET`, un nouveau JitterBuffer garde sa cible initiale ;
/// sinon on force `set_target_ms` (override UI SetBuffer).
const NO_DEFAULT_TARGET: usize = usize::MAX;

/// f32 partagé lock-free (bits dans un `AtomicU32`). Ordonnancement `Relaxed` :
/// ce sont des scalaires indépendants (volume/pan/VU), sans invariant croisé à
/// protéger — la synchro des données du jitter buffer est portée par les
/// verrous par flux, pas par ces atomiques.
#[derive(Debug)]
struct AtomicF32(AtomicU32);

impl AtomicF32 {
    fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    #[inline]
    fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

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

/// C2.1 — cellule d'un flux : le `JitterBuffer` sous son PROPRE `Mutex` (verrou
/// COURT, par flux) + params/VU/compteurs en atomiques (posés/lus SANS lock). La
/// map les partage via `Arc<StreamCell>` : le callback (mix) et le décodage
/// (push) ne se croisent plus que sur le MÊME flux, le temps d'un push/pull.
struct StreamCell {
    /// Id du producer — dupliqué ici (immuable) pour que les itérations sur un
    /// snapshot de cellules (télémétrie, logs) n'aient pas à re-consulter la map.
    id: String,
    /// Lot C — nature du flux : `Instrument` (sommé dans le mix enregistré/duckable)
    /// ou `Voice` (talkback pair, sommé post-tap/post-DIM via `voice_buf`).
    /// Immuable après l'ajout du flux.
    kind: StreamKind,
    /// Verrou COURT par flux — pris juste le temps d'un `push` (décodage) ou d'un
    /// `pull` (callback). Jamais tenu en même temps que le RwLock de la map.
    jitter: Mutex<JitterBuffer>,
    volume: AtomicF32,
    /// Pan range [-1.0, 1.0]. -1 = full left, 0 = center, +1 = full right.
    /// Loi de BALANCE stéréo linéaire dans `mix_into` (0 dB au centre :
    /// gain_L = min(1, 1−pan), gain_R = min(1, 1+pan)) — la loi standard
    /// DAW pour un signal stéréo entrant. Default 0.0 (centré).
    pan: AtomicF32,
    /// Point 4 — armé pour le bus MIX REC. `true` = ce flux est sommé dans
    /// `mix_buf` (tap fichier enregistré + VU MIX REC) ; `false` = exclu du MIX
    /// mais TOUJOURS dans le monitoring (`output`/MASTER). Défaut `false`.
    mix_armed: AtomicBool,
    /// VU par flux (pré-pan) — ÉCRITS par `push_samples` (thread décode), LUS par
    /// `stream_levels` (thread WS). Atomiques → lock-free des deux côtés.
    rms: AtomicF32,
    rms_l: AtomicF32,
    rms_r: AtomicF32,
    peak_l: AtomicF32,
    peak_r: AtomicF32,
    /// Compteurs de logging overflow (côté push, écrivain unique = thread décode).
    last_overflow_drops: AtomicU64,
    buffer_full_count: AtomicU64,
    /// Compteurs de drift drain (côté pull, écrivain unique = callback via
    /// `report_drift_drops`).
    last_drift_drops: AtomicU64,
    drift_drain_count: AtomicU64,
    /// Sprint S6 — timestamps des drift drains sur la fenêtre glissante
    /// `UNSTABLE_WINDOW_SECS`. Poussé par le callback (`report_drift_drops`),
    /// purgé/lu par le thread WS (`stream_unstable_events`) → sous son propre
    /// Mutex court (distinct du jitter).
    drift_drain_history: Mutex<std::collections::VecDeque<std::time::Instant>>,
}

impl StreamCell {
    /// Cellule d'un flux réseau (pair). `default_target_ms` = valeur imposée par
    /// SetBuffer (ou `NO_DEFAULT_TARGET` pour garder la cible initiale du buffer).
    fn new_peer(id: &str, kind: StreamKind, default_target_ms: usize) -> Self {
        let mut jitter = JitterBuffer::new();
        if default_target_ms != NO_DEFAULT_TARGET {
            jitter.set_target_ms(default_target_ms);
        }
        Self::from_jitter(id, kind, jitter, 1.0)
    }

    /// Cellule du self-monitor (boucle locale). `volume` initial paramétrable
    /// (0.0 à la création, préservé lors d'un `reset_local_stream`).
    ///
    /// A-lite : `set_local_mode` AVANT `set_target_ms` pour que le clamp du
    /// plancher utilise `LOCAL_MIN_TARGET_MS` (3 ms) et non `MIN_TARGET_MS` (5).
    /// Chantier C — mode local : concealment des trous (pas de clic sur les
    /// spikes plugin) + adaptation bornée (latence plafonnée, retour au plancher).
    fn new_local(volume: f32) -> Self {
        let mut jitter = JitterBuffer::new();
        jitter.set_local_mode(true);
        jitter.set_target_ms(SELF_MONITOR_TARGET_MS);
        // self-monitor = instrument (enregistré/duckable).
        Self::from_jitter(SELF_MONITOR_ID, StreamKind::Instrument, jitter, volume)
    }

    fn from_jitter(id: &str, kind: StreamKind, jitter: JitterBuffer, volume: f32) -> Self {
        Self {
            id: id.to_string(),
            kind,
            jitter: Mutex::new(jitter),
            volume: AtomicF32::new(volume),
            pan: AtomicF32::new(0.0),
            mix_armed: AtomicBool::new(false),
            rms: AtomicF32::new(0.0),
            rms_l: AtomicF32::new(0.0),
            rms_r: AtomicF32::new(0.0),
            peak_l: AtomicF32::new(0.0),
            peak_r: AtomicF32::new(0.0),
            last_overflow_drops: AtomicU64::new(0),
            buffer_full_count: AtomicU64::new(0),
            last_drift_drops: AtomicU64::new(0),
            drift_drain_count: AtomicU64::new(0),
            drift_drain_history: Mutex::new(std::collections::VecDeque::with_capacity(32)),
        }
    }
}

/// Buffers de travail du callback (mix_into) — TOUCHÉS UNIQUEMENT par le thread
/// audio de sortie. Sous un `Mutex` verrouillé une seule fois par bloc :
/// NON contendu par le décodage (qui ne touche jamais ces buffers) → coût nul en
/// régime, et le callback ne bloque jamais un producteur en les tenant.
struct MixScratch {
    /// Réutilisé bloc à bloc pour le `pull` de chaque flux — évite ~400 alloc/s.
    temp_buf: Vec<f32>,
    /// Lot C — accumulateur de la VOIX des pairs (talkback), sommé post-tap/post-DIM.
    voice_buf: Vec<f32>,
    /// Point 4 — accumulateur du bus MIX REC : somme des instruments **armés**.
    mix_buf: Vec<f32>,
    /// C2.1 — snapshot réutilisé des `Arc<StreamCell>` cloné sous le RwLock
    /// lecture puis relâché : le callback mixe les cellules SANS tenir le verrou
    /// de la map. `clear` + `extend` → zéro alloc en régime établi.
    snapshot: Vec<Arc<StreamCell>>,
}

impl MixScratch {
    fn new() -> Self {
        Self {
            temp_buf: Vec::new(),
            voice_buf: Vec::new(),
            mix_buf: Vec::new(),
            snapshot: Vec::new(),
        }
    }
}

/// Mixes N remote audio streams into a single stereo output.
/// Each stream has its own jitter buffer and volume control.
///
/// C2.1 (verrouillage FIN par flux) — l'état interne est protégé par des verrous
/// FINS (interior mutability), donc toutes les méthodes sont `&self` : le mixer
/// est partagé via un simple `Arc<AudioMixer>` SANS `Mutex` externe. Le callback
/// de sortie ne tient JAMAIS un verrou couvrant tous les flux : il clone les
/// `Arc<StreamCell>` sous le RwLock lecture (µs), relâche, puis verrouille chaque
/// cellule une par une (pull COURT). Le décodage (`push_samples`) et le callback
/// ne se croisent plus que sur le MÊME flux → fenêtre de contention ms → µs.
pub struct AudioMixer {
    /// Registre des flux. RwLock : LU par le callback (clone des Arc) ET le
    /// décodage (clone de l'Arc cible) ; ÉCRIT seulement à l'add/remove/reset
    /// (rare — join/leave peer, capture start/stop).
    streams: RwLock<HashMap<String, Arc<StreamCell>>>,
    /// Buffers de travail du callback (voir [`MixScratch`]).
    scratch: Mutex<MixScratch>,
    /// Cible jitter buffer par défaut (ms) — appliquée aux nouveaux streams.
    /// `NO_DEFAULT_TARGET` = pas d'override (JitterBuffer garde sa cible initiale).
    /// Piloté par `set_target_ms_all` (handler SetBuffer côté UI). Atomique → lu
    /// sans lock à l'ajout d'un flux.
    default_target_ms: AtomicUsize,
    /// REC-3 : si `Some(tx)`, un enregistrement est en cours et les push_*
    /// (self, peer) + mix_into envoient leurs samples au thread record via
    /// `try_send` non-bloquant. `RwLock` : posé rarement (start/stop record),
    /// lu en concurrence par décodage + callback (lecteurs multiples, zéro
    /// contention entre eux).
    record_tx: RwLock<Option<Sender<RecordCmd>>>,
    /// Gate rapide (lock-free) miroir de `record_tx.is_some()` — testé en 1er sur
    /// le hot path pour court-circuiter le lookup/clone hors enregistrement.
    record_active: AtomicBool,
    /// Master gain global appliqué dans `mix_into` après le mix des streams.
    /// Plage [0.0, 1.5] (cohérent avec les faders peer/self côté UI). Default 1.0.
    master_gain: AtomicF32,
    /// DIM factor — atténuation temporaire des instruments quand l'utilisateur
    /// active DIM côté UI (pour entendre la conversation talkback clairement).
    /// Plage [0.0, 1.0], typiquement 0.25 (-12dB) ou 1.0 (off). Appliqué dans
    /// `mix_into` après la somme des streams et avant le master_gain.
    /// **Le tap REC-3 push_mix est positionné AVANT dim et master** pour que
    /// le fichier MIX enregistré soit le mix post-fader des instruments SEUL.
    dim_factor: AtomicF32,
    /// Source « référence » (métronome via l'agent — Option B). Mixée dans
    /// `mix_into` à un point DÉDIÉ (après le tap record, après le DIM, avant le
    /// master), donc HORS de la map `streams`. Sous son propre `Mutex` : accédée
    /// par le callback (`advance_and_generate`) et le thread WS (config/backing/
    /// preview) — pas par le décodage, donc hors du chemin de contention corrigé.
    reference: Mutex<ReferenceSource>,
    /// Point 3 (Lot 2) — RMS L/R du VRAI mix, mesuré dans `mix_into` sur la
    /// sortie réelle → le VU MASTER/MIX du browser reflète pan + faders. Écrits
    /// par le callback, lus par le sender `stream-levels` (100 ms) → atomiques.
    /// `master_*` = sortie finale (post dim/master/clamp) ; `mix_*` = tap MIX
    /// post-fader (pré-dim/master, parité browser `instrumentMixBus`).
    master_rms_l: AtomicF32,
    master_rms_r: AtomicF32,
    mix_rms_l: AtomicF32,
    mix_rms_r: AtomicF32,
    /// Pic échantillon L/R du MASTER (sortie finale) et du MIX (tap post-fader).
    master_peak_l: AtomicF32,
    master_peak_r: AtomicF32,
    mix_peak_l: AtomicF32,
    mix_peak_r: AtomicF32,
    /// Gain/pan du BUS voix (une seule tranche, parité `voiceGain`/`voicePanNode`
    /// navigateur). Le mute est porté par le gain (le web envoie 0.0). Défaut 1.0/0.0.
    voice_gain: AtomicF32,
    voice_pan: AtomicF32,
    /// RMS (mono) de la voix des pairs effectivement mixée (post gain de bus) —
    /// remonté via `stream-levels` pour le VU voix navigateur en mode agent.
    inbound_voice_rms: AtomicF32,
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
            scratch: Mutex::new(MixScratch::new()),
            default_target_ms: AtomicUsize::new(NO_DEFAULT_TARGET),
            record_tx: RwLock::new(None),
            record_active: AtomicBool::new(false),
            master_gain: AtomicF32::new(1.0),
            dim_factor: AtomicF32::new(1.0),
            reference: Mutex::new(ReferenceSource::new()),
            master_rms_l: AtomicF32::new(0.0),
            master_rms_r: AtomicF32::new(0.0),
            mix_rms_l: AtomicF32::new(0.0),
            mix_rms_r: AtomicF32::new(0.0),
            master_peak_l: AtomicF32::new(0.0),
            master_peak_r: AtomicF32::new(0.0),
            mix_peak_l: AtomicF32::new(0.0),
            mix_peak_r: AtomicF32::new(0.0),
            voice_gain: AtomicF32::new(1.0),
            voice_pan: AtomicF32::new(0.0),
            inbound_voice_rms: AtomicF32::new(0.0),
        }
    }

    /// Point 3 (Lot 2) — RMS L/R du MASTER (sortie finale) et du MIX (tap
    /// post-fader), mesurés au dernier `mix_into`. `(master_l, master_r,
    /// mix_l, mix_r)`. Lus par le sender `stream-levels` → VU MASTER/MIX
    /// stéréo réels côté browser (pan + faders visibles).
    pub fn master_mix_rms(&self) -> (f32, f32, f32, f32) {
        (
            self.master_rms_l.load(),
            self.master_rms_r.load(),
            self.mix_rms_l.load(),
            self.mix_rms_r.load(),
        )
    }

    /// Pic échantillon L/R du MASTER et du MIX (parité `master_mix_rms`, pour le
    /// peak-mètre DAW). `(master_peak_l, master_peak_r, mix_peak_l, mix_peak_r)`.
    pub fn master_mix_peak(&self) -> (f32, f32, f32, f32) {
        (
            self.master_peak_l.load(),
            self.master_peak_r.load(),
            self.mix_peak_l.load(),
            self.mix_peak_r.load(),
        )
    }

    /// DIM factor (= ducking des instruments quand le user veut entendre
    /// la voix talkback clairement). Plage [0.0, 1.0], typiquement 0.25
    /// (-12dB) ou 1.0 (off). Clamp défensif côté agent.
    pub fn set_dim(&self, factor: f32) {
        self.dim_factor
            .store(if factor.is_finite() { factor.clamp(0.0, 1.0) } else { 1.0 });
    }

    /// REC-3 : armer/désarmer l'enregistrement. Quand `Some(tx)`, les tap
    /// sites (push_self_samples, push_samples remote, mix_into) envoient
    /// leurs samples au thread record via `try_send` non-bloquant. Quand
    /// `None`, les taps sont no-op (gate `record_active`). Appelé depuis le
    /// pipeline au start_recording / stop_recording.
    pub fn set_record_tx(&self, tx: Option<Sender<RecordCmd>>) {
        let active = tx.is_some();
        *self.record_tx.write() = tx;
        // Le gate rapide est mis à jour APRÈS le tx : un décodage qui voit
        // `record_active=true` trouvera toujours un tx valide.
        self.record_active.store(active, Ordering::Relaxed);
    }

    /// Point 4 — applique l'armement MIX REC en SNAPSHOT (état complet poussé
    /// par le web à chaque mutation). Ne somme dans le bus MIX (fichier + VU)
    /// que le self-monitor si `self_armed` et les pairs dont le producer_id est
    /// dans `armed_peers`. Tous les autres instruments passent à `mix_armed =
    /// false`. N'affecte JAMAIS le monitoring/MASTER (`output`).
    pub fn set_record_arm(&self, self_armed: bool, armed_peers: &[String]) {
        let map = self.streams.read();
        for (id, cell) in map.iter() {
            let armed = if id == SELF_MONITOR_ID {
                self_armed
            } else {
                armed_peers.iter().any(|p| p == id)
            };
            cell.mix_armed.store(armed, Ordering::Relaxed);
        }
    }

    /// Master gain global appliqué dans `mix_into`. Clamp défensif dans
    /// [0.0, 1.5] (NaN devient 1.0 — unity).
    pub fn set_master_gain(&self, gain: f32) {
        self.master_gain
            .store(if gain.is_finite() { gain.clamp(0.0, 1.5) } else { 1.0 });
    }

    /// Lot C — gain du BUS voix (talkback pairs). Le web envoie le gain EFFECTIF
    /// (valeur du fader, ou 0.0 pour le mute « M »). Clamp [0.0, 1.5] (NaN → 1.0).
    pub fn set_peer_voice_gain(&self, gain: f32) {
        self.voice_gain
            .store(if gain.is_finite() { gain.clamp(0.0, 1.5) } else { 1.0 });
    }

    /// Lot C — balance du BUS voix, [-1.0, 1.0] (parité `voicePanNode`). NaN → 0.0.
    pub fn set_peer_voice_pan(&self, pan: f32) {
        self.voice_pan
            .store(if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 });
    }

    /// Lot C — RMS mono de la voix des pairs effectivement mixée (post gain de
    /// bus), pour le VU voix navigateur en mode agent. `0.0` si aucune voix.
    pub fn inbound_voice_rms(&self) -> f32 {
        self.inbound_voice_rms.load()
    }

    /// Helper interne : try_send vers le record thread sans bloquer.
    /// Drop silencieux si le channel est plein (thread en retard) — le warn
    /// est émis côté thread record qui surveille sa queue length.
    fn record_send(&self, cmd: RecordCmd) {
        if let Some(tx) = &*self.record_tx.read() {
            let _ = tx.try_send(cmd);
        }
    }

    /// Add a new remote stream.
    /// Ajoute un stream entrant. `kind` (Lot C) route son mixage : `Instrument`
    /// = mix enregistré/duckable ; `Voice` = talkback pair, sommé post-tap/post-DIM.
    pub fn add_stream(&self, producer_id: &str, kind: StreamKind) {
        let default_target = self.default_target_ms.load(Ordering::Relaxed);
        let cell = Arc::new(StreamCell::new_peer(producer_id, kind, default_target));
        self.streams.write().insert(producer_id.to_string(), cell);
    }

    /// Remove a stream.
    pub fn remove_stream(&self, producer_id: &str) {
        self.streams.write().remove(producer_id);
    }

    /// Crée le stream de self-monitor (boucle locale capture → mixer → playback).
    ///
    /// Volume initial = 0.0 (silencieux) : l'utilisateur doit explicitement
    /// ouvrir le fader « moi » côté UI via `SetSelfMonitorVolume`. Sans ça,
    /// risque de larsen au démarrage si micro ouvert près d'un haut-parleur.
    ///
    /// Jitter target = `SELF_MONITOR_TARGET_MS` (A-lite : 3 ms). Latence ear-to-ear
    /// self résultante ≈ buffers device (in+out) + 3 ms target ≈ ~5-6 ms à 64.
    pub fn add_local_stream(&self) {
        let cell = Arc::new(StreamCell::new_local(0.0));
        self.streams.write().insert(SELF_MONITOR_ID.to_string(), cell);
    }

    /// Supprime le stream self-monitor (appelé depuis `stop_capture`).
    pub fn remove_local_stream(&self) {
        self.streams.write().remove(SELF_MONITOR_ID);
    }

    /// 0.5.4-18 — réinitialise le JitterBuffer du self-monitor en préservant le
    /// volume. À appeler après une discontinuité d'horloge de capture (re-init ASIO
    /// mid-session sur réveil de veille PC) : le buffer de gigue repart propre, son
    /// estimateur de drift n'est plus faussé par le trou → plus de distorsion
    /// persistante au casque. No-op si le self-monitor n'existe pas.
    ///
    /// C2.1 — remplacement ATOMIQUE sous le write lock (lecture de l'ancien volume
    /// puis insertion de la cellule neuve dans la même section critique) → aucune
    /// fenêtre où le self-monitor serait absent.
    pub fn reset_local_stream(&self) {
        let mut map = self.streams.write();
        let Some(old) = map.get(SELF_MONITOR_ID) else {
            return;
        };
        let volume = old.volume.load();
        map.insert(
            SELF_MONITOR_ID.to_string(),
            Arc::new(StreamCell::new_local(volume)),
        );
    }

    /// Override le volume du self-monitor (0.0 = silence, 1.0 = unity, 1.5 = max).
    /// Appelé par le handler `SetSelfMonitorVolume` côté ws_server.
    pub fn set_self_monitor_volume(&self, volume: f32) {
        self.set_volume(SELF_MONITOR_ID, volume);
    }

    /// Push capture samples dans le stream self-monitor (depuis l'encoder
    /// thread, en parallèle de l'encodage Opus pour les pairs).
    /// No-op si `add_local_stream()` n'a pas été appelé (capture pas démarrée).
    pub fn push_self_samples(&self, samples: &[f32]) {
        if self.streams.read().contains_key(SELF_MONITOR_ID) {
            self.push_samples(SELF_MONITOR_ID, samples);
        }
        // REC-3 : tap stem-self. Pré-fader, post channel-split.
        // Fait APRÈS push_samples pour ne pas dépendre de l'existence du
        // stream self-monitor : on enregistre l'instrument même si le user
        // a coupé son monitor browser (mode agent typique selfMuteGain=0).
        if self.record_active.load(Ordering::Relaxed) && !samples.is_empty() {
            self.record_send(RecordCmd::PushSelf(samples.to_vec()));
        }
    }

    /// Set per-stream volume (0.0 to 1.5).
    pub fn set_volume(&self, producer_id: &str, volume: f32) {
        // La référence (métronome) n'est pas dans `streams` : route explicite.
        if producer_id == REFERENCE_ID {
            self.reference.lock().set_volume(volume);
            return;
        }
        if producer_id == BACKING_ID {
            self.reference.lock().set_backing_volume(volume);
            return;
        }
        if producer_id == PREVIEW_ID {
            self.reference.lock().set_preview_volume(volume);
            return;
        }
        // Garde NaN alignée sur set_pan/set_dim/set_master_gain :
        // NaN.clamp() = NaN → silence définitif du stream sinon.
        let v = if volume.is_finite() { volume.clamp(0.0, 1.5) } else { 1.0 };
        if let Some(cell) = self.streams.read().get(producer_id) {
            cell.volume.store(v);
        }
    }

    /// Set per-stream volume by producer_id (alias for set_volume).
    pub fn set_stream_volume(&self, producer_id: &str, volume: f32) {
        self.set_volume(producer_id, volume);
    }

    /// Set per-stream pan, range [-1.0, 1.0]. -1=full left, 0=center, +1=full right.
    /// Applique une loi de balance stéréo linéaire (0 dB au centre) dans
    /// `mix_into`. Pour SELF_MONITOR_ID, fonctionne pareil.
    /// No-op si le stream n'existe pas (peer parti, race).
    pub fn set_pan(&self, producer_id: &str, pan: f32) {
        if producer_id == REFERENCE_ID {
            self.reference.lock().set_pan(pan);
            return;
        }
        if producer_id == BACKING_ID {
            self.reference.lock().set_backing_pan(pan);
            return;
        }
        if producer_id == PREVIEW_ID {
            self.reference.lock().set_preview_pan(pan);
            return;
        }
        let p = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
        if let Some(cell) = self.streams.read().get(producer_id) {
            cell.pan.store(p);
        }
    }

    /// Configure la source référence (métronome via l'agent — Option B).
    /// Handler `reference-config`. Les `String` wire (`sound`/`figure`) sont
    /// parsées côté ws_server en `MetroSound`/`Figure` pour garder cette API typée.
    #[allow(clippy::too_many_arguments)]
    pub fn set_reference_config(
        &self,
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
        self.reference.lock().set_config(
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
    pub fn set_reference_grid(&self, anchor_beat_frame: f64, anchor_beat_index: u64) {
        self.reference.lock().set_grid(anchor_beat_frame, anchor_beat_index);
    }

    /// Arrête la référence et coupe ses voix — handler `reference-stop`.
    pub fn reference_stop(&self) {
        self.reference.lock().stop();
    }

    /// Ancre échantillon↔mural courante (frame de sortie ↔ instant monotone
    /// agent) pour construire `reference-clock-pong`.
    pub fn output_anchor(&self) -> OutputAnchor {
        self.reference.lock().anchor()
    }

    // ─── Backing (B4) — délégué à la source référence ─────────────────────────
    pub fn backing_begin(&self, total_frames: usize) {
        self.reference.lock().backing_begin(total_frames);
    }
    pub fn backing_push(&self, samples: &[f32]) {
        self.reference.lock().backing_push(samples);
    }
    pub fn backing_end(&self) {
        self.reference.lock().backing_end();
    }
    pub fn backing_unload(&self) {
        self.reference.lock().backing_unload();
    }
    pub fn backing_play(&self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.lock().backing_play(anchor_backing_frame, anchor_output_frame);
    }
    pub fn backing_pause(&self) {
        self.reference.lock().backing_pause();
    }
    pub fn backing_seek(&self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.lock().backing_seek(anchor_backing_frame, anchor_output_frame);
    }
    pub fn backing_sync(&self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.lock().backing_sync(anchor_backing_frame, anchor_output_frame);
    }

    // ─── Preview (Lot D) — délégué à la source référence (buffer séparé) ───────
    pub fn preview_begin(&self, total_frames: usize) {
        self.reference.lock().preview_begin(total_frames);
    }
    pub fn preview_push(&self, samples: &[f32]) {
        self.reference.lock().preview_push(samples);
    }
    pub fn preview_end(&self) {
        self.reference.lock().preview_end();
    }
    pub fn preview_unload(&self) {
        self.reference.lock().preview_unload();
    }
    pub fn preview_play(&self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.lock().preview_play(anchor_backing_frame, anchor_output_frame);
    }
    pub fn preview_pause(&self) {
        self.reference.lock().preview_pause();
    }
    pub fn preview_seek(&self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.lock().preview_seek(anchor_backing_frame, anchor_output_frame);
    }
    pub fn preview_sync(&self, anchor_backing_frame: f64, anchor_output_frame: f64) {
        self.reference.lock().preview_sync(anchor_backing_frame, anchor_output_frame);
    }

    /// Phase B — transmet la gigue réseau mesurée (RFC 3550, ms) au jitter
    /// buffer du stream pour piloter sa cible prédictive. No-op si le stream
    /// n'existe pas (peer parti) ou s'il est en override manuel. Appelé par la
    /// recv task à cadence réduite. Clone l'Arc sous le RwLock lecture puis
    /// relâche avant de verrouiller la cellule (jamais les deux à la fois).
    pub fn observe_jitter(&self, producer_id: &str, jitter_tail_ms: f64) {
        let cell = self.streams.read().get(producer_id).cloned();
        if let Some(cell) = cell {
            cell.jitter.lock().observe_jitter(jitter_tail_ms);
        }
    }

    /// P1 (01/07) — repart propre après un rétablissement audio (reset ASIO).
    /// Vide les jitter buffers du périmé accumulé pendant le gel de sortie et les
    /// re-prime à la cible de démarrage. Appelé par la pipeline juste après une
    /// reconstruction réussie des streams. Tous les streams (pairs + self-monitor).
    pub fn reset_streams_for_recovery(&self) {
        // Clone des Arc (cold path — reconstruction rare) puis relâche la map
        // avant de verrouiller chaque cellule une par une.
        let cells: Vec<Arc<StreamCell>> = self.streams.read().values().cloned().collect();
        for cell in cells {
            cell.jitter.lock().reset_for_recovery();
        }
    }

    /// Push decoded samples into a stream's jitter buffer.
    ///
    /// Le jitter buffer applique drop-oldest sur overflow (cf. `JitterBuffer::push`).
    /// On rate-limit le warn sur l'INCRÉMENT de `overflow_drops`.
    ///
    /// C2.1 — clone l'Arc du flux cible sous le RwLock lecture, relâche, puis
    /// verrouille SA cellule (verrou court). Ne croise le callback que sur ce flux.
    pub fn push_samples(&self, producer_id: &str, samples: &[f32]) {
        let cell = self.streams.read().get(producer_id).cloned();

        // REC-3 : tap stem-peer. Pre-fader (avant `vol *` dans mix_into),
        // post Opus decode. On filtre SELF_MONITOR_ID (push_self_samples émet
        // déjà son propre PushSelf). Lot C : on filtre aussi les flux VOIX — le
        // talkback des pairs n'est jamais enregistré. Le gate `record_active`
        // court-circuite tout hors enregistrement. Défaut SÛR : un flux INCONNU
        // (pas encore `add_stream`) n'est PAS enregistré (`is_some_and`).
        if self.record_active.load(Ordering::Relaxed)
            && producer_id != SELF_MONITOR_ID
            && !samples.is_empty()
            && cell.as_ref().is_some_and(|c| c.kind != StreamKind::Voice)
        {
            self.record_send(RecordCmd::PushPeer(producer_id.to_string(), samples.to_vec()));
        }

        let Some(cell) = cell else {
            tracing::warn!(
                target: "jamodio::mixer",
                producer = &producer_id[..8.min(producer_id.len())],
                "push_samples on unknown stream"
            );
            return;
        };

        // Compute RMS of pushed samples (global + par canal L/R) — hors lock
        // (ne lit que `samples`). Posé dans les atomiques de la cellule.
        if !samples.is_empty() {
            let n = samples.len();
            let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
            let rms = (sum_sq / n as f32).sqrt();
            cell.rms.store(rms);
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
                cell.rms_l.store((sq_l / half).sqrt());
                cell.rms_r.store((sq_r / half).sqrt());
                cell.peak_l.store(pk_l);
                cell.peak_r.store(pk_r);
            } else {
                cell.rms_l.store(rms);
                cell.rms_r.store(rms);
                cell.peak_l.store(peak);
                cell.peak_r.store(peak);
            }
        }

        // Verrou COURT de la cellule : push + lecture du compteur d'overflow.
        let new_drops = {
            let mut jitter = cell.jitter.lock();
            jitter.push(samples);
            jitter.overflow_drops()
        };

        // Logging rate-limité (écrivain unique = ce thread décode pour ce flux).
        if new_drops > cell.last_overflow_drops.load(Ordering::Relaxed) {
            let count = cell.buffer_full_count.load(Ordering::Relaxed) + 1;
            cell.buffer_full_count.store(count, Ordering::Relaxed);
            if count == 1 || count.is_power_of_two() {
                tracing::warn!(
                    target: "jamodio::mixer",
                    producer = &producer_id[..8.min(producer_id.len())],
                    events = count,
                    oldest_dropped_total = new_drops,
                    "jitter buffer overflow — oldest samples dropped (burst SFU?)"
                );
            }
            cell.last_overflow_drops.store(new_drops, Ordering::Relaxed);
        }
    }

    /// Mix all streams into the output buffer.
    /// Called from the CPAL playback callback.
    /// Output is interleaved stereo f32.
    ///
    /// C2.1 — le callback verrouille son scratch privé (non contendu), clone les
    /// `Arc<StreamCell>` sous le RwLock lecture (relâché juste après), puis mixe
    /// chaque flux en verrouillant SA cellule le temps d'un `pull`. Il ne tient
    /// JAMAIS deux verrous de cellule à la fois, ni un verrou couvrant tous les flux.
    pub fn mix_into(&self, output: &mut [f32]) {
        // Scratch privé du callback (un seul thread audio de sortie) — lock
        // NON contendu par le décodage.
        let mut scratch = self.scratch.lock();
        let MixScratch {
            temp_buf,
            voice_buf,
            mix_buf,
            snapshot,
        } = &mut *scratch;

        output.fill(0.0);

        // Resize uniquement si la taille du callback change (typiquement jamais
        // après le 1er appel : CPAL livre des blocs de taille fixe).
        if temp_buf.len() != output.len() {
            temp_buf.resize(output.len(), 0.0);
        }
        // Lot C — accumulateur voix (talkback pairs), sommé plus bas post-tap/post-DIM.
        if voice_buf.len() != output.len() {
            voice_buf.resize(output.len(), 0.0);
        }
        voice_buf.fill(0.0);
        let mut any_voice = false;
        // Point 4 — accumulateur MIX REC (instruments ARMÉS), rempli en passe 1
        // en parallèle de `output`. Remis à zéro à chaque bloc (comme voice_buf).
        if mix_buf.len() != output.len() {
            mix_buf.resize(output.len(), 0.0);
        }
        mix_buf.fill(0.0);

        // Snapshot des cellules sous RwLock LECTURE (clone des Arc), relâché
        // AUSSITÔT : le mixage se fait ensuite sans tenir le verrou de la map.
        snapshot.clear();
        {
            let map = self.streams.read();
            snapshot.extend(map.values().cloned());
        }

        // PASSE 1 — chaque flux est pull UNE fois (verrou court de SA cellule) :
        //   - INSTRUMENT/self → sommé (fader+balance) dans `output` ;
        //   - VOICE (talkback pair) → accumulé BRUT dans `voice_buf` (pas de
        //     fader/pan par pair : tranche unique) — gain/pan de bus après le DIM.
        for cell in snapshot.iter() {
            {
                let mut jitter = cell.jitter.lock();
                jitter.pull(temp_buf);
            }

            if cell.kind == StreamKind::Voice {
                any_voice = true;
                for (v, &sample) in voice_buf.iter_mut().zip(temp_buf.iter()) {
                    *v += sample;
                }
                continue;
            }

            let vol = cell.volume.load();
            // Point 4 — un instrument armé alimente AUSSI le bus MIX REC
            // (`mix_buf`) avec exactement le même échantillon post-fader/pan que
            // le monitoring. Non armé → seul `output` (monitoring/MASTER) reçoit.
            let armed = cell.mix_armed.load(Ordering::Relaxed);
            let pan = cell.pan.load();
            // Balance stéréo — loi LINÉAIRE 0 dB au centre. Les streams sont
            // STÉRÉO interleaved (L,R,L,R…) : ce contrôle est un *balance*,
            // pas un pan mono. Centre = 1.0/1.0 (fast-path), extrêmes = 1.0/0.0.
            if pan.abs() < f32::EPSILON {
                let n = temp_buf.len().min(output.len());
                if armed {
                    // `output` (monitoring) ET `mix_buf` (MIX REC) reçoivent le
                    // même échantillon post-fader — calculé une fois.
                    for ((o, mb), &t) in output[..n]
                        .iter_mut()
                        .zip(mix_buf[..n].iter_mut())
                        .zip(temp_buf[..n].iter())
                    {
                        let s = t * vol;
                        *o += s;
                        *mb += s;
                    }
                } else {
                    for (o, &t) in output[..n].iter_mut().zip(temp_buf[..n].iter()) {
                        *o += t * vol;
                    }
                }
            } else {
                let (gl, gr) = pan_gains(pan);
                let gain_l = vol * gl;
                let gain_r = vol * gr;
                let mut i = 0;
                while i + 1 < temp_buf.len() && i + 1 < output.len() {
                    let l = temp_buf[i] * gain_l;
                    let r = temp_buf[i + 1] * gain_r;
                    output[i] += l;
                    output[i + 1] += r;
                    if armed {
                        mix_buf[i] += l;
                        mix_buf[i + 1] += r;
                    }
                    i += 2;
                }
            }
        }

        // Log mixed output RMS every ~20 seconds (48000*2 / 256 ≈ 375 calls/s)
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        if c.is_multiple_of(7500) && !snapshot.is_empty() {
            let rms: f32 = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
            tracing::debug!(target: "jamodio::mixer", streams = snapshot.len(), rms, "mix_into heartbeat");
        }

        // REC-3 / Point 4 : tap MIX = `mix_buf` (instruments ARMÉS uniquement),
        // pré-dim/master (parité browser `instrumentMixBus` post-`armGain`).
        // Rien armé → `mix_buf` silencieux → fichier MIX silencieux.
        if self.record_active.load(Ordering::Relaxed) {
            self.record_send(RecordCmd::PushMix(mix_buf.clone()));
        }

        // Point 3/4 — RMS + pic L/R du MIX REC (instruments ARMÉS post-fader,
        // pré-dim/master) pour le VU MIX REC stéréo.
        let (mix_l, mix_r) = stereo_rms(mix_buf);
        self.mix_rms_l.store(mix_l);
        self.mix_rms_r.store(mix_r);
        let (mix_pk_l, mix_pk_r) = stereo_peak(mix_buf);
        self.mix_peak_l.store(mix_pk_l);
        self.mix_peak_r.store(mix_pk_r);

        // DIM factor — atténue les instruments quand l'user veut entendre le
        // talkback clairement. Skip si == 1.0 (cas par défaut majoritaire).
        let dim_factor = self.dim_factor.load();
        if (dim_factor - 1.0).abs() > f32::EPSILON {
            for sample in output.iter_mut() {
                *sample *= dim_factor;
            }
        }

        // Lot C — VOIX des pairs (talkback). Sommée ICI, exactement comme la
        // référence : APRÈS le tap RECORD (⇒ jamais enregistrée) et APRÈS le DIM
        // (⇒ jamais duckée), AVANT le master. Gain/pan de BUS unique (parité
        // voiceGain/voicePanNode navigateur). Le VU voix lit `inbound_voice_rms`.
        let voice_gain = self.voice_gain.load();
        if any_voice && voice_gain > f32::EPSILON {
            let (gl, gr) = pan_gains(self.voice_pan.load());
            let gain_l = voice_gain * gl;
            let gain_r = voice_gain * gr;
            let mut i = 0;
            while i + 1 < voice_buf.len() && i + 1 < output.len() {
                output[i] += voice_buf[i] * gain_l;
                output[i + 1] += voice_buf[i + 1] * gain_r;
                i += 2;
            }
            // RMS scalaire agrégé sur L+R, post-gain de bus (avant pan), pour le VU
            // voix navigateur.
            let frames = voice_buf.len() / 2;
            if frames > 0 {
                let sum_sq: f32 = voice_buf.iter().map(|s| s * s).sum();
                self.inbound_voice_rms
                    .store((sum_sq / voice_buf.len() as f32).sqrt() * voice_gain);
            } else {
                self.inbound_voice_rms.store(0.0);
            }
        } else {
            self.inbound_voice_rms.store(0.0);
        }

        // Référence (métronome via l'agent — Option B). Ajoutée ICI, à un point
        // DÉDIÉ hors de la boucle streams :
        //   - APRÈS le tap record push_mix ⇒ EXCLUE du MIX enregistré ;
        //   - APRÈS le DIM ⇒ le clic n'est PAS ducké par le talkback ;
        //   - AVANT le master + le clamp ⇒ suit le fader master et reste borné.
        // Appelée à CHAQUE bloc (même métro coupé) pour tenir à jour l'ancre
        // échantillon↔mural exposée au browser (`output_anchor`).
        self.reference.lock().advance_and_generate(output, mono_now_ms());

        // Master gain global (fader MASTER côté UI). Appliqué AVANT le clamp.
        // Skip multiplication si gain == 1.0 (cas par défaut).
        let master_gain = self.master_gain.load();
        if (master_gain - 1.0).abs() > f32::EPSILON {
            for sample in output.iter_mut() {
                *sample *= master_gain;
            }
        }

        // Soft clamp to prevent distortion
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        // Point 3 — RMS L/R du MASTER (sortie finale = ce que l'utilisateur
        // entend, post dim/master/clamp) pour le VU MASTER stéréo.
        let (master_l, master_r) = stereo_rms(output);
        self.master_rms_l.store(master_l);
        self.master_rms_r.store(master_r);
        let (master_pk_l, master_pk_r) = stereo_peak(output);
        self.master_peak_l.store(master_pk_l);
        self.master_peak_r.store(master_pk_r);

        // Report drift drains (rate-limité à puissances de 2). Coût formatage
        // négligeable hors événement. Itère le snapshot déjà cloné (pas de re-lock
        // de la map) ; chaque cellule est verrouillée brièvement une par une.
        report_drift_drops(snapshot);
    }

    /// Niveaux par stream pour les VU du browser.
    /// Retourne `(producer_id, rms_global, rms_l, rms_r, peak_l, peak_r)` par
    /// stream (RMS + pic échantillon, tous POST-pan). Lot C : les flux VOIX sont
    /// EXCLUS (agrégat unique via `inbound_voice_rms()`).
    pub fn stream_levels(&self) -> Vec<(String, f32, f32, f32, f32, f32)> {
        let map = self.streams.read();
        map.values()
            .filter(|cell| cell.kind != StreamKind::Voice)
            .map(|cell| {
                // VU POST-pan : on applique la MÊME loi de balance que `mix_into`
                // aux niveaux L/R (stockés pré-pan) → le VU reflète le placement
                // stéréo exactement comme le rendu.
                let (gl, gr) = pan_gains(cell.pan.load());
                (
                    cell.id.clone(),
                    cell.rms.load(),
                    cell.rms_l.load() * gl,
                    cell.rms_r.load() * gr,
                    cell.peak_l.load() * gl,
                    cell.peak_r.load() * gr,
                )
            })
            .collect()
    }

    /// Sprint S6 — purge la fenêtre glissante de drift drains et retourne
    /// les peers REMOTE dont le compte d'events sur la fenêtre dépasse
    /// `threshold`. Self-monitor exclu (drains = overload agent local, pas un
    /// peer distant instable).
    ///
    /// Retourne `(producer_id, drift_drains_window, drift_drains_total)`.
    pub fn stream_unstable_events(
        &self,
        window: std::time::Duration,
        threshold: usize,
    ) -> Vec<(String, usize, u64)> {
        let now = std::time::Instant::now();
        let cutoff = now.checked_sub(window).unwrap_or(now);
        let mut out = Vec::new();
        let map = self.streams.read();
        for cell in map.values() {
            if cell.id == SELF_MONITOR_ID {
                continue;
            }
            let events_window = {
                let mut hist = cell.drift_drain_history.lock();
                // Purge les timestamps hors fenêtre (= plus anciens que cutoff).
                while let Some(&front) = hist.front() {
                    if front < cutoff {
                        hist.pop_front();
                    } else {
                        break;
                    }
                }
                hist.len()
            };
            if events_window > threshold {
                out.push((
                    cell.id.clone(),
                    events_window,
                    cell.drift_drain_count.load(Ordering::Relaxed),
                ));
            }
        }
        out
    }

    /// Sprint S1 — snapshot perf par stream remote (self-monitor exclu).
    /// Retourne (producer_id, underruns_cumul, drift_drops_cumul, target_ms_courant).
    pub fn stream_perf_stats(&self) -> Vec<(String, u64, u64, usize)> {
        let map = self.streams.read();
        map.values()
            .filter(|cell| cell.id != SELF_MONITOR_ID)
            .map(|cell| {
                let jitter = cell.jitter.lock();
                (
                    cell.id.clone(),
                    jitter.underruns(),
                    jitter.drift_drops(),
                    jitter.target_ms(),
                )
            })
            .collect()
    }

    /// Chantier C (v0.4.14) — stats du self-monitor : latence courante du buffer
    /// (ms) + underruns cumulés. (0, 0) si le self-monitor n'est pas actif.
    pub fn self_monitor_stats(&self) -> (usize, u64) {
        match self.streams.read().get(SELF_MONITOR_ID) {
            Some(cell) => {
                let jitter = cell.jitter.lock();
                (jitter.target_ms(), jitter.underruns())
            }
            None => (0, 0),
        }
    }

    /// Number of active REMOTE streams (self-monitor exclu).
    pub fn stream_count(&self) -> usize {
        self.streams
            .read()
            .keys()
            .filter(|k| k.as_str() != SELF_MONITOR_ID)
            .count()
    }

    /// Total underruns aggregated across REMOTE per-stream jitter buffers.
    /// Self-monitor exclu (ring alimenté en local → underruns = overload CPU
    /// agent, pas un problème réseau).
    pub fn total_underruns(&self) -> u64 {
        let map = self.streams.read();
        map.iter()
            .filter(|(k, _)| k.as_str() != SELF_MONITOR_ID)
            .map(|(_, cell)| cell.jitter.lock().underruns())
            .sum()
    }

    /// Cible jitter buffer moyenne (ms) sur les streams REMOTE actifs.
    /// Self-monitor exclu. 0 si pas de stream remote.
    pub fn mean_target_ms(&self) -> f32 {
        let map = self.streams.read();
        let targets: Vec<usize> = map
            .iter()
            .filter(|(k, _)| k.as_str() != SELF_MONITOR_ID)
            .map(|(_, cell)| cell.jitter.lock().target_ms())
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
    pub fn set_target_ms_all(&self, target_ms: usize) {
        self.default_target_ms.store(target_ms, Ordering::Relaxed);
        let map = self.streams.read();
        for (id, cell) in map.iter() {
            if id == SELF_MONITOR_ID {
                continue;
            }
            cell.jitter.lock().set_target_ms(target_ms);
        }
    }
}

/// Surveille les drift-drains (samples jetés côté pull pour borner la latence)
/// après un mix. Appelé depuis `mix_into` (fin du tour) sur le snapshot déjà
/// cloné — pas de re-lock de la map. Chaque cellule est verrouillée brièvement.
fn report_drift_drops(snapshot: &[Arc<StreamCell>]) {
    for cell in snapshot {
        let (new_drops, target_ms) = {
            let jitter = cell.jitter.lock();
            (jitter.drift_drops(), jitter.target_ms())
        };
        if new_drops > cell.last_drift_drops.load(Ordering::Relaxed) {
            let count = cell.drift_drain_count.load(Ordering::Relaxed) + 1;
            cell.drift_drain_count.store(count, Ordering::Relaxed);
            // Sprint S6 — track ce drain dans la fenêtre glissante 30 s pour la
            // détection peer instable. Garde-fou anti-mémoire : cap 256 entrées.
            {
                let mut hist = cell.drift_drain_history.lock();
                hist.push_back(std::time::Instant::now());
                while hist.len() > 256 {
                    hist.pop_front();
                }
            }
            // Bug D : on logue uniquement les drains sévères (events > 4), à
            // events = 8, 16, 32… (is_power_of_two) → réduction ~70 % du spam.
            if count > 4 && count.is_power_of_two() {
                tracing::warn!(
                    target: "jamodio::mixer",
                    producer = &cell.id[..8.min(cell.id.len())],
                    events = count,
                    drained_total = new_drops,
                    target_ms,
                    "jitter buffer drift drain — latence excessive ramenée à target"
                );
            }
            cell.last_drift_drops.store(new_drops, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_par_canal_l_r_independants() {
        let m = AudioMixer::new();
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
        let m = AudioMixer::new();
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
    /// sur un signal à transitoires, et suivre le pan comme le RMS.
    #[test]
    fn stream_peak_capte_le_transitoire_et_le_pan() {
        let m = AudioMixer::new();
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
        let m = AudioMixer::new();
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
        let m = AudioMixer::new();
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
        let m = AudioMixer::new();
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

    /// Extrêmes de la loi de balance : côté plein = unity, côté opposé = 0.
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
    /// toucher le self-monitor : son buffer est local (3 ms).
    #[test]
    fn set_target_ms_all_excludes_self_monitor() {
        let m = AudioMixer::new();
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
            let m = AudioMixer::new();
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
    /// (instruments coupés), la voix reste pleinement audible en sortie.
    #[test]
    fn voice_is_never_ducked_by_dim() {
        let m = AudioMixer::new();
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
    /// stem par pair (`PushPeer`), ni dans le MIX (`PushMix`).
    #[test]
    fn voice_is_never_recorded() {
        use crate::record::RecordCmd;
        let (tx, rx) = crossbeam_channel::unbounded();
        let m = AudioMixer::new();
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
    /// comme le backing/la référence).
    #[test]
    fn preview_is_never_ducked_by_dim() {
        let m = AudioMixer::new();
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
    /// tap, comme le backing).
    #[test]
    fn preview_is_never_recorded() {
        use crate::record::RecordCmd;
        let (tx, rx) = crossbeam_channel::unbounded();
        let m = AudioMixer::new();
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
    /// monitoring (MASTER).
    #[test]
    fn record_arm_gates_mix_not_master() {
        let m = AudioMixer::new();
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
    #[test]
    fn record_arm_targets_self_monitor() {
        let m = AudioMixer::new();
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

    // ─── C2.1 — concurrence : push (décode) pendant mix_into (callback) ────────

    /// Un thread pousse en continu pendant qu'un autre mixe : le verrouillage fin
    /// (RwLock map + Mutex par flux) doit rester SÛR (aucun panic, données finies
    /// et bornées) et sans deadlock. Preuve empirique que le callback et le
    /// décodage ne se bloquent plus mutuellement de façon dangereuse.
    #[test]
    fn concurrent_push_and_mix_is_safe() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mixer = Arc::new(AudioMixer::new());
        mixer.add_stream("p1", StreamKind::Instrument);
        mixer.add_stream("p2", StreamKind::Instrument);
        mixer.set_record_arm(false, &["p1".to_string(), "p2".to_string()]);

        let stop = Arc::new(AtomicBool::new(false));

        // Producteur 1 (décode p1).
        let m1 = mixer.clone();
        let s1 = stop.clone();
        let t1 = std::thread::spawn(move || {
            let block = vec![0.5f32; 960]; // 10 ms stéréo @ 48 kHz
            while !s1.load(Ordering::Relaxed) {
                m1.push_samples("p1", &block);
            }
        });
        // Producteur 2 (décode p2) — mute des params en parallèle.
        let m2 = mixer.clone();
        let s2 = stop.clone();
        let t2 = std::thread::spawn(move || {
            let block = vec![0.25f32; 960];
            let mut pan = -1.0f32;
            while !s2.load(Ordering::Relaxed) {
                m2.push_samples("p2", &block);
                m2.set_pan("p2", pan);
                m2.set_volume("p2", 0.8);
                pan = -pan;
            }
        });

        // Consommateur (callback) — mixe des milliers de blocs.
        let mut out = vec![0.0f32; 512];
        for _ in 0..20_000 {
            mixer.mix_into(&mut out);
            for &s in &out {
                assert!(s.is_finite(), "sortie finie");
                assert!((-1.0..=1.0).contains(&s), "sortie clampée dans [-1,1]");
            }
        }

        stop.store(true, Ordering::Relaxed);
        t1.join().unwrap();
        t2.join().unwrap();

        // Les VU par flux doivent être lisibles sans lock et cohérents (finis).
        for (_, rms, l, r, pl, pr) in mixer.stream_levels() {
            for v in [rms, l, r, pl, pr] {
                assert!(v.is_finite(), "VU fini après concurrence");
            }
        }
    }

    /// C2.1 — add/remove de flux en concurrence d'un mix continu : l'écriture de
    /// la map (write lock) et sa lecture par le callback (clone des Arc) ne
    /// doivent jamais paniquer ni laisser un flux à demi-inséré.
    #[test]
    fn concurrent_add_remove_during_mix_is_safe() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mixer = Arc::new(AudioMixer::new());
        let stop = Arc::new(AtomicBool::new(false));

        let m1 = mixer.clone();
        let s1 = stop.clone();
        let churn = std::thread::spawn(move || {
            let block = vec![0.5f32; 480];
            let mut n = 0u64;
            while !s1.load(Ordering::Relaxed) {
                let id = format!("peer{}", n % 4);
                m1.add_stream(&id, StreamKind::Instrument);
                m1.push_samples(&id, &block);
                m1.remove_stream(&id);
                n += 1;
            }
        });

        let mut out = vec![0.0f32; 256];
        for _ in 0..20_000 {
            mixer.mix_into(&mut out);
            for &s in &out {
                assert!(s.is_finite());
            }
        }

        stop.store(true, Ordering::Relaxed);
        churn.join().unwrap();
    }
}
