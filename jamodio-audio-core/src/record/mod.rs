//! REC-3 : enregistrement multi-stems côté agent (Ogg/Opus).
//!
//! Architecture :
//!  - `OpusOggRecorder` (opus_ogg.rs) : un recorder par stream — accumule des
//!    samples PCM stéréo f32 48kHz, les encode en Opus (20ms par packet),
//!    et écrit incrémentalement un container Ogg en mémoire.
//!  - `Recorder` (ce module) : N OpusOggRecorder indexés par stream id.
//!  - `RecorderHandle` (ce module) : Recorder + thread dédié + crossbeam channel.
//!    Les tap sites envoient des `RecordCmd::PushXxx` en `try_send` non-bloquant :
//!    encode Opus et écriture Ogg se font dans le thread record, hors path RT.
//!  - Quand pas d'enregistrement : `record_tx == None` côté mixer → 0 alloc,
//!    1 if check. Strict pas d'impact latence.

pub mod ogg;
pub mod opus_ogg;

use crossbeam_channel::{bounded, Receiver, Sender};
use opus_ogg::OpusOggRecorder;
use std::collections::HashMap;
use std::thread::JoinHandle;

/// Identifiant + métadonnées d'un stem à enregistrer. Le browser fournit
/// la liste dans `start-recording` ; l'agent crée un `OpusOggRecorder`
/// par entrée et stocke ces métadonnées pour les retourner à la fin.
#[derive(Debug, Clone)]
pub struct StemSpec {
    /// "stem-self" | "stem-peer" | "mix" — convention partagée avec le browser.
    pub role: String,
    /// Pour stem-peer : producer_id du peer (clé du mixer). Pour stem-self :
    /// optionnel (myUserId côté browser). Pour mix : None.
    pub peer_id: Option<String>,
    /// Nom lisible pour le nom de fichier final côté browser.
    pub peer_name: Option<String>,
}

/// Un fichier prêt à être renvoyé au browser après finalize.
pub struct RecordedFile {
    pub spec: StemSpec,
    pub data: Vec<u8>,
}

/// Multi-stems recorder — orchestré depuis `recording.rs` côté agent.
/// Wrappable dans `Arc<Mutex<Option<Recorder>>>` pour partage entre tap sites.
pub struct Recorder {
    /// `self_recorder` activé si un stem `stem-self` a été armé.
    self_recorder: Option<(StemSpec, OpusOggRecorder)>,
    /// Recorders par peer producer_id (clé du mixer).
    peer_recorders: HashMap<String, (StemSpec, OpusOggRecorder)>,
    /// `mix_recorder` activé si un stem `mix` a été armé.
    mix_recorder: Option<(StemSpec, OpusOggRecorder)>,
}

impl Recorder {
    /// Construit un Recorder à partir de la liste de stems armés par le browser.
    /// Échoue si l'init d'un OpusEncoder échoue (rare — config interne stable).
    pub fn new(stems: &[StemSpec]) -> Result<Self, audiopus::Error> {
        let mut self_recorder = None;
        let mut peer_recorders = HashMap::new();
        let mut mix_recorder = None;
        for s in stems {
            match s.role.as_str() {
                "stem-self" => {
                    self_recorder = Some((s.clone(), OpusOggRecorder::new()?));
                }
                "stem-peer" => {
                    let Some(peer_id) = s.peer_id.clone() else { continue };
                    peer_recorders.insert(peer_id, (s.clone(), OpusOggRecorder::new()?));
                }
                "mix" => {
                    mix_recorder = Some((s.clone(), OpusOggRecorder::new()?));
                }
                _ => {
                    tracing::warn!(target: "jamodio::record", role = %s.role, "unknown stem role, ignored");
                }
            }
        }
        Ok(Self { self_recorder, peer_recorders, mix_recorder })
    }

    /// Liste des specs effectivement armés (pour l'ack `recording-started`).
    pub fn armed_specs(&self) -> Vec<StemSpec> {
        let mut out = Vec::new();
        if let Some((s, _)) = &self.self_recorder { out.push(s.clone()); }
        for (s, _) in self.peer_recorders.values() { out.push(s.clone()); }
        if let Some((s, _)) = &self.mix_recorder { out.push(s.clone()); }
        out
    }

    pub fn push_self(&mut self, pcm_stereo: &[f32]) {
        if let Some((_, r)) = self.self_recorder.as_mut() {
            r.push_samples(pcm_stereo);
        }
    }

    pub fn push_peer(&mut self, producer_id: &str, pcm_stereo: &[f32]) {
        if let Some((_, r)) = self.peer_recorders.get_mut(producer_id) {
            r.push_samples(pcm_stereo);
        }
    }

    pub fn push_mix(&mut self, pcm_stereo: &[f32]) {
        if let Some((_, r)) = self.mix_recorder.as_mut() {
            r.push_samples(pcm_stereo);
        }
    }

    /// Finalise tous les recorders : écrit la dernière page Ogg avec EOS
    /// flag, retourne la liste des fichiers prêts à être envoyés.
    pub fn finalize(mut self) -> Vec<RecordedFile> {
        let mut out = Vec::new();
        if let Some((spec, r)) = self.self_recorder.take() {
            out.push(RecordedFile { spec, data: r.finalize() });
        }
        for (_, (spec, r)) in self.peer_recorders.drain() {
            out.push(RecordedFile { spec, data: r.finalize() });
        }
        if let Some((spec, r)) = self.mix_recorder.take() {
            out.push(RecordedFile { spec, data: r.finalize() });
        }
        out
    }
}

// ─── Threading wrapper : RecorderHandle + RecordCmd ─────────────────

/// Commande envoyée au thread record via un crossbeam channel.
/// Les variantes contiennent leur propre `Vec<f32>` (ownership transféré)
/// pour découpler le caller du timing d'encodage. L'alloc côté caller est
/// brève (~200ns pour un Vec<f32; 240>) et acceptable face à une frame 2.5ms.
pub enum RecordCmd {
    PushSelf(Vec<f32>),
    PushPeer(String, Vec<f32>),
    PushMix(Vec<f32>),
    Finalize,
}

/// Handle externe vers le thread record. Les tap sites clonent `tx` et
/// envoient leurs samples en non-bloquant (`try_send`) — si le thread est
/// en retard, on drop le sample du record (jamais du jam temps-réel) avec
/// un warn rate-limité.
pub struct RecorderHandle {
    pub tx: Sender<RecordCmd>,
    finalize_rx: Receiver<Vec<RecordedFile>>,
    thread: Option<JoinHandle<()>>,
    pub armed_specs: Vec<StemSpec>,
}

impl RecorderHandle {
    /// Démarre le thread record + crée le Recorder pour les stems demandés.
    /// Erreur en String pour agréger audiopus::Error (init encoder) et
    /// std::io::Error (spawn thread) sans inventer un type d'erreur dédié.
    pub fn start(stems: Vec<StemSpec>) -> Result<Self, String> {
        let recorder = Recorder::new(&stems).map_err(|e| format!("opus encoder init: {}", e))?;
        let armed_specs = recorder.armed_specs();

        // Channel cmd : bounded 256. À 400 cmds/s (taux RT), ça représente
        // 0.6s de buffering — suffisant pour absorber un pic d'encodage de
        // quelques frames sans drop.
        let (tx, rx) = bounded::<RecordCmd>(256);
        // Réponse de finalize : capacity 1, le main thread l'attend.
        let (fin_tx, fin_rx) = bounded::<Vec<RecordedFile>>(1);

        let thread = std::thread::Builder::new()
            .name("record".into())
            .spawn(move || run_record_thread(recorder, rx, fin_tx))
            .map_err(|e| format!("spawn record thread: {}", e))?;

        Ok(Self {
            tx,
            finalize_rx: fin_rx,
            thread: Some(thread),
            armed_specs,
        })
    }

    /// Send Finalize au thread + attend les fichiers (timeout 30s).
    /// Consomme le handle.
    pub fn stop(mut self) -> Vec<RecordedFile> {
        let _ = self.tx.send(RecordCmd::Finalize);
        match self
            .finalize_rx
            .recv_timeout(std::time::Duration::from_secs(30))
        {
            Ok(files) => {
                // Finalize OK → le thread a fini son travail, on peut le join
                // sans risque de blocage.
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
                files
            }
            Err(e) => {
                // Timeout/disconnect : le thread record est probablement bloqué
                // (I/O disque, encodeur figé). On NE join PAS — sinon le thread
                // de contrôle de l'agent se bloquerait indéfiniment. On le laisse
                // détaché (il sera nettoyé à la fin du process). Mieux vaut une
                // fuite de thread bornée qu'un agent wedgé.
                tracing::error!(target: "jamodio::record", error = ?e, "finalize timeout/disconnect — thread record laissé détaché (pas de join)");
                let _ = self.thread.take(); // drop le JoinHandle sans join
                Vec::new()
            }
        }
    }
}

fn run_record_thread(
    mut recorder: Recorder,
    rx: Receiver<RecordCmd>,
    fin_tx: Sender<Vec<RecordedFile>>,
) {
    tracing::info!(target: "jamodio::record", stems = recorder.armed_specs().len(), "record thread started");
    let mut drops: u64 = 0;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            RecordCmd::PushSelf(s)         => recorder.push_self(&s),
            RecordCmd::PushPeer(id, s)     => recorder.push_peer(&id, &s),
            RecordCmd::PushMix(s)          => recorder.push_mix(&s),
            RecordCmd::Finalize            => {
                let files = recorder.finalize();
                if fin_tx.send(files).is_err() {
                    tracing::warn!(target: "jamodio::record", "finalize_rx dropped before recv");
                }
                tracing::info!(target: "jamodio::record", drops, "record thread done");
                return;
            }
        }
        // Heartbeat des drops : si le channel sender try_send a échoué côté
        // tap site, on n'a pas de visibilité ici. Mais on peut détecter une
        // accumulation : si plus de N cmds en file, c'est qu'on est en retard.
        if rx.len() > 200 {
            drops += 1;
            if drops == 1 || drops.is_power_of_two() {
                tracing::warn!(target: "jamodio::record", queue_len = rx.len(), events = drops, "record thread behind — risk of sample drops");
            }
        }
    }
    // Channel disconnected sans Finalize : on finalize quand même pour ne pas
    // perdre l'audio capturé.
    let files = recorder.finalize();
    let _ = fin_tx.send(files);
}
