//! Le faux serveur : il tient la place du SFU auprès de l'Audio Engine.
//!
//! Pour chaque musicien simulé, un « transport » UDP comme ceux du vrai serveur :
//! l'agent y envoie son perçage (comedia), le faux serveur apprend son adresse et
//! lui envoie le flux, chiffré avec le code SRTP de l'agent lui-même. Pour ce que
//! l'agent ENVOIE (instrument, talkback), un transport de réception qui mesure la
//! régularité d'arrivée — ce que les autres musiciens entendraient.
//!
//! Le faux serveur mesure aussi SA PROPRE précision d'envoi : si c'est lui qui
//! envoie en retard, la gigue vient du banc et non de l'agent. Les résultats le
//! disent au lieu de le cacher.

use crate::profile::{Frame, InstrumentSchedule, PeerProfile, Speech, VoiceSchedule, FRAME_SAMPLES, FRAME_US};
use jamodio_audio_core::codec::encoder::{MusicEncoder, MAX_PACKET_SIZE};
use jamodio_audio_core::net::rtp::{self, RtpHeader};
use jamodio_audio_core::net::srtp::{SrtpContext, SrtpParameters};
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Type de charge utile Opus, comme le studio.
pub const PAYLOAD_TYPE: u8 = 111;

/// Nature d'un flux simulé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Instrument,
    Voice,
}

/// Trames Opus pré-encodées, rejouées en boucle.
///
/// Encoder à la volée coûterait au banc 400 encodages par seconde et par flux —
/// sur le même PC en mode local, ce serait le banc qui chargerait la machine.
/// Le DÉCODAGE, lui, reste entier côté agent : c'est lui qu'on mesure.
#[derive(Clone)]
pub struct Payloads {
    frames: Vec<Vec<u8>>,
}

impl Payloads {
    /// Une seconde de son (sinus + souffle léger), encodée avec l'encodeur de
    /// l'agent (même débit, même mode, même taille de paquet qu'en vrai).
    pub fn encode(freq_hz: f32) -> Result<Self, String> {
        let enc = MusicEncoder::new().map_err(|e| format!("encodeur Opus : {e:?}"))?;
        let n = enc.frame_size();
        let mut rng = crate::profile::Rng::new(freq_hz.to_bits() as u64);
        let mut frames = Vec::with_capacity(400);
        let mut phase = 0.0f32;
        let step = 2.0 * std::f32::consts::PI * freq_hz / 48_000.0;
        for _ in 0..400 {
            let mut pcm = vec![0.0f32; n * 2];
            for s in 0..n {
                let v = 0.3 * phase.sin() + 0.01 * (rng.unit() as f32 - 0.5);
                phase = (phase + step) % (2.0 * std::f32::consts::PI);
                pcm[2 * s] = v;
                pcm[2 * s + 1] = v;
            }
            let mut out = vec![0u8; MAX_PACKET_SIZE];
            let len = enc.encode(&pcm, &mut out).map_err(|e| format!("encodage Opus : {e:?}"))?;
            out.truncate(len);
            frames.push(out);
        }
        Ok(Self { frames })
    }

    fn get(&self, index: u64) -> &[u8] {
        &self.frames[(index % self.frames.len() as u64) as usize]
    }
}

/// Un transport de descente : ce qu'un musicien simulé envoie à l'agent.
pub struct Downlink {
    pub producer_id: String,
    pub kind: Kind,
    socket: UdpSocket,
    /// Clés du faux serveur, données à l'agent dans `add-stream`.
    pub server_keys: SrtpParameters,
    /// Contexte SRTP, posé quand l'agent a rendu SES clés (`local-port`).
    ctx: OnceLock<SrtpContext>,
    /// Adresse de l'agent, apprise à son premier perçage.
    agent_addr: Mutex<Option<SocketAddr>>,
    ssrc: u32,
    seq_base: u16,
    ts_base: u32,
    /// Flux retiré (le musicien est parti) : la boucle d'envoi l'abandonne.
    retired: AtomicBool,
}

impl Downlink {
    pub fn bind(ip: &str, producer_id: String, kind: Kind, seed: u64) -> Result<Arc<Self>, String> {
        let socket = UdpSocket::bind((ip, 0)).map_err(|e| format!("bind {ip} : {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| e.to_string())?;
        let mut rng = crate::profile::Rng::new(seed ^ 0x5EED);
        Ok(Arc::new(Self {
            producer_id,
            kind,
            socket,
            server_keys: SrtpParameters::generate_aead_aes_256_gcm(),
            ctx: OnceLock::new(),
            agent_addr: Mutex::new(None),
            ssrc: rng.next_u64() as u32,
            seq_base: rng.next_u64() as u16,
            ts_base: rng.next_u64() as u32,
            retired: AtomicBool::new(false),
        }))
    }

    /// Le musicien part : plus aucun paquet de ce flux. Sans cela, la boucle
    /// enverrait vers un port que l'agent a fermé (erreurs d'envoi comptées,
    /// calcul perdu) jusqu'à la fin de la campagne.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }

    pub fn port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Clés de l'agent reçues : on peut chiffrer pour lui.
    pub fn set_agent_keys(&self, agent_keys: &SrtpParameters) -> Result<(), String> {
        let ctx = SrtpContext::new(&self.server_keys, agent_keys)?;
        self.ctx.set(ctx).map_err(|_| "clés de l'agent déjà reçues pour ce flux".to_string())
    }

    fn ready(&self) -> Option<SocketAddr> {
        self.ctx.get()?;
        *self.agent_addr.lock().unwrap()
    }

    /// Écoute le perçage de l'agent (comedia) jusqu'à `stop`.
    fn listen_punch(self: Arc<Self>, stop: Arc<AtomicBool>) {
        let mut buf = [0u8; 2048];
        while !stop.load(Ordering::Relaxed) && !self.is_retired() {
            if let Ok((len, from)) = self.socket.recv_from(&mut buf) {
                // Un paquet RTP (pas un rapport RTCP) : c'est le perçage.
                if len >= 12 && buf[1] & 0x7f == PAYLOAD_TYPE {
                    let mut addr = self.agent_addr.lock().unwrap();
                    if addr.is_none() {
                        *addr = Some(from);
                    }
                }
            }
        }
    }

    /// Paquet SRTP de la trame `f`.
    fn packet(&self, f: Frame, payload: &[u8]) -> Result<Vec<u8>, String> {
        let header = RtpHeader {
            payload_type: PAYLOAD_TYPE,
            sequence: self.seq_base.wrapping_add(f.index as u16),
            timestamp: self.ts_base.wrapping_add((f.index as u32).wrapping_mul(FRAME_SAMPLES)),
            ssrc: self.ssrc,
            marker: false,
        };
        let mut pkt = rtp::build_packet(&header, payload);
        self.ctx.get().ok_or("pas de clés")?.protect(&mut pkt)?;
        Ok(pkt)
    }
}

/// Un flux actif dans la boucle d'envoi.
struct Active {
    link: Arc<Downlink>,
    schedule: Box<dyn Iterator<Item = Frame> + Send>,
    next: Option<Frame>,
    /// Origine du calendrier : posée quand le flux devient prêt.
    t0: Option<Instant>,
    /// Fin de l'envoi du paquet précédent de CE flux (cf. [`lateness`]).
    free_at: Option<Instant>,
}

/// Retard d'envoi imputable au BANC : depuis que le paquet pouvait partir,
/// c'est-à-dire à son heure ET une fois le paquet précédent de son propre flux
/// parti. Attendre derrière sa propre salve (pic relâché d'un coup, gigue à
/// queue lourde) est le comportement d'un vrai lien, simulé exprès : ce n'est
/// pas une erreur du banc. Attendre derrière la salve d'un AUTRE musicien en
/// est une (leurs réseaux sont indépendants) : elle reste comptée. Avant
/// (01/10/2026), le retard se mesurait depuis l'heure seule, et le verdict
/// tombait à « insuffisant » dès qu'un lien relâchait des salves.
pub fn lateness(start: Instant, due: Instant, free_at: Option<Instant>) -> Duration {
    let could_leave = free_at.map_or(due, |f| f.max(due));
    start.saturating_duration_since(could_leave)
}

/// Mesures du faux serveur sur une fenêtre, vidées chaque seconde (transmises
/// telles quelles par l'émetteur distant).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SenderWindow {
    /// Retard d'envoi sur l'heure prévue (µs), un par paquet.
    pub late_us: Vec<u32>,
    pub sent: u64,
    pub errors: u64,
}

impl SenderWindow {
    /// Ajoute les mesures d'un autre fil d'envoi sur la même fenêtre.
    pub fn absorb(&mut self, other: SenderWindow) {
        self.late_us.extend(other.late_us);
        self.sent += other.sent;
        self.errors += other.errors;
    }
}

/// La boucle d'envoi de TOUS les flux de descente, sur un fil.
pub struct SenderLoop {
    add_tx: Sender<(Arc<Downlink>, Box<dyn Iterator<Item = Frame> + Send>)>,
    pub window: Arc<Mutex<SenderWindow>>,
    /// Priorité obtenue par le fil d'envoi (cf. `rt`), pour le résumé.
    pub priority: Arc<OnceLock<Result<&'static str, String>>>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    listeners: Vec<std::thread::JoinHandle<()>>,
}

impl SenderLoop {
    pub fn start(instrument: Payloads, voice: Payloads) -> Self {
        let (add_tx, add_rx) = std::sync::mpsc::channel();
        let window = Arc::new(Mutex::new(SenderWindow::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let priority = Arc::new(OnceLock::new());
        let join = {
            let (window, stop, priority) = (window.clone(), stop.clone(), priority.clone());
            std::thread::Builder::new()
                .name("bench-send".into())
                .spawn(move || {
                    let _ = priority.set(crate::rt::promote_current_thread());
                    send_loop(add_rx, instrument, voice, window, stop)
                })
                .expect("fil d'envoi du banc")
        };
        Self { add_tx, window, priority, stop, join: Some(join), listeners: Vec::new() }
    }

    /// Ajoute un flux : il part dès que l'agent a percé ET rendu ses clés.
    /// `offset_us` : place du flux dans la frise du musicien (0 à son arrivée,
    /// la durée écoulée pour le flux recréé au retour d'une absence).
    pub fn add(&mut self, link: Arc<Downlink>, profile: &PeerProfile, seed: u64, offset_us: u64) {
        let speech = profile.voice.unwrap_or(Speech::Always);
        let schedule: Box<dyn Iterator<Item = Frame> + Send> = match link.kind {
            Kind::Instrument => Box::new(InstrumentSchedule::resumed(profile, seed, offset_us)),
            Kind::Voice => Box::new(VoiceSchedule::resumed(profile, speech, seed, offset_us)),
        };
        let l = link.clone();
        let stop = self.stop.clone();
        self.listeners.push(std::thread::spawn(move || l.listen_punch(stop)));
        // Le fil d'envoi ne s'arrête qu'avec `stop` : l'envoi ne peut pas échouer.
        let _ = self.add_tx.send((link, schedule));
    }

    pub fn take_window(&self) -> SenderWindow {
        std::mem::take(&mut *self.window.lock().unwrap())
    }
}

impl Drop for SenderLoop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        for l in self.listeners.drain(..) {
            let _ = l.join();
        }
    }
}

fn send_loop(
    add_rx: Receiver<(Arc<Downlink>, Box<dyn Iterator<Item = Frame> + Send>)>,
    instrument: Payloads,
    voice: Payloads,
    window: Arc<Mutex<SenderWindow>>,
    stop: Arc<AtomicBool>,
) {
    // `thread::sleep` prend une minuterie haute résolution, Windows compris
    // (~0,4 ms de dépassement médian, sonde du 19/09/2026) : pas de réglage de
    // minuterie à faire ici. Le retard réel est de toute façon MESURÉ (`late_us`).
    let mut active: Vec<Active> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        loop {
            match add_rx.try_recv() {
                Ok((link, schedule)) => active.push(Active { link, schedule, next: None, t0: None, free_at: None }),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        active.retain(|a| !a.link.is_retired());
        let now = Instant::now();
        let mut soonest: Option<Instant> = None;
        for a in active.iter_mut() {
            let Some(addr) = a.link.ready() else { continue };
            let t0 = *a.t0.get_or_insert(now);
            loop {
                if a.next.is_none() {
                    a.next = a.schedule.next();
                }
                let Some(f) = a.next else { break };
                let due = t0 + Duration::from_micros(f.send_at_us);
                if due > now {
                    soonest = Some(soonest.map_or(due, |s| s.min(due)));
                    break;
                }
                let payload = match a.link.kind {
                    Kind::Instrument => instrument.get(f.index),
                    Kind::Voice => voice.get(f.index),
                };
                let start = Instant::now();
                let late = lateness(start, due, a.free_at).as_micros().min(u32::MAX as u128) as u32;
                let ok = a
                    .link
                    .packet(f, payload)
                    .and_then(|p| a.link.socket.send_to(&p, addr).map_err(|e| e.to_string()))
                    .is_ok();
                a.free_at = Some(Instant::now());
                let mut w = window.lock().unwrap();
                if ok {
                    w.sent += 1;
                    w.late_us.push(late);
                } else {
                    w.errors += 1;
                }
                a.next = None;
            }
        }
        // Dormir jusqu'au prochain paquet dû, borné pour voir les nouveaux flux.
        let wait = soonest
            .map(|s| s.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_micros(FRAME_US))
            .min(Duration::from_millis(5));
        wait_precisely(Instant::now() + wait);
    }
}

/// Les fils d'envoi d'une campagne : un par flux, ou un seul pour tous.
///
/// Un seul fil lie les musiciens entre eux : la salve qu'un lien relâche d'un
/// coup retarde les paquets des AUTRES le temps de l'envoyer (jusqu'à 2,6 ms,
/// NUC du 01/10/2026), alors que leurs réseaux sont indépendants. Un fil par
/// flux les délie. Sous Windows, chaque fil tient sa fin d'attente en tournant
/// (`SPIN_MARGIN`) : un fil par flux chargerait la machine mesurée — d'où un
/// seul fil en mode local Windows (sur PC, l'émetteur distant est le mode
/// recommandé), et un par flux partout ailleurs.
pub struct Senders {
    payloads: (Payloads, Payloads),
    shared: Option<SenderLoop>,
    own: std::collections::HashMap<u32, SenderLoop>,
    /// Mesures des fils arrêtés (retraits) depuis le dernier relevé.
    retired: SenderWindow,
}

/// Un fil par flux sur la machine mesurée (mode local) : partout sauf Windows.
pub const PER_STREAM_LOCAL: bool = !cfg!(windows);

impl Senders {
    pub fn new(per_stream: bool) -> Result<Self, String> {
        let payloads = (Payloads::encode(220.0)?, Payloads::encode(330.0)?);
        let shared = (!per_stream).then(|| SenderLoop::start(payloads.0.clone(), payloads.1.clone()));
        Ok(Self { payloads, shared, own: std::collections::HashMap::new(), retired: SenderWindow::default() })
    }

    /// Le flux `id` part selon `profile`.
    pub fn add(&mut self, id: u32, link: Arc<Downlink>, profile: &PeerProfile, seed: u64, offset_us: u64) {
        match self.shared.as_mut() {
            Some(s) => s.add(link, profile, seed, offset_us),
            None => {
                let mut s = SenderLoop::start(self.payloads.0.clone(), self.payloads.1.clone());
                s.add(link, profile, seed, offset_us);
                self.own.insert(id, s);
            }
        }
    }

    /// Le flux `id` est retiré (`Downlink::retire` fait par l'appelant) : son
    /// fil s'arrête, ses dernières mesures sont gardées.
    pub fn retire(&mut self, id: u32) {
        if let Some(s) = self.own.remove(&id) {
            self.retired.absorb(s.take_window());
        }
    }

    pub fn take_window(&mut self) -> SenderWindow {
        let mut w = std::mem::take(&mut self.retired);
        for s in self.shared.iter().chain(self.own.values()) {
            w.absorb(s.take_window());
        }
        w
    }

    /// Nombre de fils d'envoi en marche.
    pub fn threads(&self) -> usize {
        usize::from(self.shared.is_some()) + self.own.len()
    }

    /// Priorité obtenue (tous les fils sont promus de la même façon).
    pub fn priority(&self) -> String {
        match self.shared.iter().chain(self.own.values()).next() {
            Some(s) => crate::run::priority_label(s),
            None => "aucun flux en cours".into(),
        }
    }
}

/// Marge de fin d'attente tenue ACTIVEMENT (Windows seulement).
///
/// Le réveil d'un `sleep` Windows déborde, même en MMCSS : 0,4 ms médian et
/// jusqu'à ~1 ms (sonde du 19/09/2026), et le `selftest` du NUC (28/09/2026)
/// a vu le banc envoyer jusqu'à 2,2 ms en retard presque chaque seconde. Le
/// banc dort donc jusqu'à `SPIN_MARGIN` avant l'échéance, puis la tient en
/// tournant. Coût : jusqu'à `SPIN_MARGIN` de calcul par échéance (une toutes les
/// 2,5 ms avec des flux alignés) — le prix d'un banc plus précis que ce qu'il
/// mesure ; `selftest` en donne le résultat. macOS n'en a pas besoin (réveil à
/// ~0,1 ms en priorité temps réel) : attente simple.
#[cfg(windows)]
const SPIN_MARGIN: Duration = Duration::from_micros(1_200);

fn wait_precisely(until: Instant) {
    #[cfg(windows)]
    {
        let now = Instant::now();
        if until > now + SPIN_MARGIN {
            std::thread::sleep(until - now - SPIN_MARGIN);
        }
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }
    #[cfg(not(windows))]
    {
        let now = Instant::now();
        if until > now {
            std::thread::sleep(until - now);
        }
    }
}

/// Mesures d'un flux MONTANT (ce que l'agent envoie) sur une fenêtre.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct UplinkWindow {
    pub packets: u64,
    /// Plus grand écart entre deux paquets reçus (µs).
    pub max_gap_us: u64,
    /// Écarts de plus de 10 ms : ce que les autres entendraient comme une coupure.
    pub gaps_over_10ms: u64,
    /// Numéros de séquence manquants.
    pub seq_missing: u64,
    /// Paquets indéchiffrables (clés, ou pas un paquet de l'agent).
    pub undecryptable: u64,
}

/// Écart entre deux paquets d'un flux régulier au-delà duquel on compte une
/// coupure : ce que les autres entendraient (4 trames manquantes). Même seuil
/// pour ce que reçoit le banc et ce qui arrive au relais.
pub const CUT_GAP_US: u64 = 10_000;

/// Un transport MONTANT : reçoit l'instrument ou le talkback de l'agent.
pub struct Uplink {
    socket: UdpSocket,
    pub server_keys: SrtpParameters,
    ctx: OnceLock<SrtpContext>,
    pub window: Mutex<UplinkWindow>,
}

impl Uplink {
    pub fn bind(ip: &str) -> Result<Arc<Self>, String> {
        let socket = UdpSocket::bind((ip, 0)).map_err(|e| format!("bind {ip} : {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            socket,
            server_keys: SrtpParameters::generate_aead_aes_256_gcm(),
            ctx: OnceLock::new(),
            window: Mutex::new(UplinkWindow::default()),
        }))
    }

    pub fn port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn set_agent_keys(&self, agent_keys: &SrtpParameters) -> Result<(), String> {
        let ctx = SrtpContext::new(&self.server_keys, agent_keys)?;
        self.ctx.set(ctx).map_err(|_| "clés de l'agent déjà reçues pour ce flux".to_string())
    }

    pub fn take_window(&self) -> UplinkWindow {
        std::mem::take(&mut *self.window.lock().unwrap())
    }

    /// Reçoit et mesure jusqu'à `stop`. Les rapports RTCP sont ignorés. Le fil
    /// est promu comme celui d'envoi : un réveil tardif du banc se lirait
    /// sinon comme une coupure de l'agent.
    pub fn listen(self: Arc<Self>, stop: Arc<AtomicBool>) {
        let _ = crate::rt::promote_current_thread();
        let mut last: Option<(Instant, u16)> = None;
        let mut buf = vec![0u8; 2048];
        while !stop.load(Ordering::Relaxed) {
            buf.resize(2048, 0);
            let Ok((len, _)) = self.socket.recv_from(&mut buf) else { continue };
            let at = Instant::now();
            buf.truncate(len);
            if len < 2 || (200..=204).contains(&buf[1]) {
                continue;
            }
            let Some(ctx) = self.ctx.get() else { continue };
            let mut w = self.window.lock().unwrap();
            if ctx.unprotect(&mut buf).is_err() {
                w.undecryptable += 1;
                continue;
            }
            let Some((h, _)) = rtp::parse_header(&buf) else {
                w.undecryptable += 1;
                continue;
            };
            w.packets += 1;
            if let Some((prev_at, prev_seq)) = last {
                let gap = at.duration_since(prev_at).as_micros() as u64;
                w.max_gap_us = w.max_gap_us.max(gap);
                if gap > CUT_GAP_US {
                    w.gaps_over_10ms += 1;
                }
                let step = h.sequence.wrapping_sub(prev_seq);
                if (2..1000).contains(&step) {
                    w.seq_missing += u64::from(step - 1);
                }
            }
            last = Some((at, h.sequence));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::net::udp::{RtpReceiver, RtpSender};

    /// Le faux serveur parle au RÉCEPTEUR DE L'AGENT (même code) : perçage,
    /// chiffrement croisé, cadence tenue.
    #[test]
    fn le_recepteur_de_l_agent_recoit_les_flux_du_faux_serveur() {
        let link = Downlink::bind("127.0.0.1", "p1".into(), Kind::Instrument, 1).unwrap();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        // Côté agent : ses clés en local, celles du serveur en distant.
        let agent_ctx = Arc::new(SrtpContext::new(&agent_keys, &link.server_keys).unwrap());
        let receiver = RtpReceiver::new(agent_ctx).unwrap();
        link.set_agent_keys(&agent_keys).unwrap();

        let mut sender = SenderLoop::start(Payloads::encode(220.0).unwrap(), Payloads::encode(440.0).unwrap());
        sender.add(link.clone(), &PeerProfile::preset("regular").unwrap(), 1, 0);
        let sfu: SocketAddr = format!("127.0.0.1:{}", link.port()).parse().unwrap();
        receiver.punch(sfu).unwrap();

        // Lecture non bloquante, comme le fil de réception de l'agent (sans son
        // attente `mio` : une courte pause quand la socket est vide).
        let mut buf = Vec::with_capacity(2048);
        let mut seqs = Vec::new();
        let t = Instant::now();
        while seqs.len() < 200 && t.elapsed() < Duration::from_secs(5) {
            match receiver.read(&mut buf) {
                Ok(r) => {
                    assert!(r.len > 12, "paquet déchiffré par l'agent");
                    // Lot 1-D2 : sous macOS le système horodate toujours. Sous
                    // Windows cela dépend de la machine (la VM de CI n'horodate
                    // pas la boucle locale, 28/09/2026) : l'agent le dit au
                    // journal, le test ne l'exige pas.
                    if cfg!(target_os = "macos") {
                        assert!(r.stack_delay.is_some(), "attente système → lecture mesurée");
                    }
                    seqs.push(rtp::parse_header(&buf).unwrap().0.sequence);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_micros(200))
                }
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(seqs.len(), 200);
        assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)), "numérotation continue");
        // ~0,5 s pour 200 trames de 2,5 ms (tolérance large : machine de CI).
        assert!(t.elapsed() >= Duration::from_millis(450), "cadence tenue, pas de rafale");
        let w = sender.take_window();
        assert!(w.sent >= 200 && w.errors == 0, "{w:?}");
    }

    /// Ce que le récepteur de l'agent a lu d'un flux du faux serveur : numéro de
    /// trame (déduit de la séquence), horodatage RTP, instant de lecture.
    struct Received {
        index: u64,
        timestamp: u32,
        at: Instant,
    }

    /// Envoie `profile` par le faux serveur au RÉCEPTEUR DE L'AGENT (même code :
    /// perçage, SRTP, lecture non bloquante) jusqu'à `count` paquets ou `max`.
    /// Fil promu comme ceux du banc : la date de lecture est la mesure.
    fn through_agent_receiver(profile: &PeerProfile, seed: u64, count: usize, max: Duration) -> (Vec<Received>, Arc<Downlink>) {
        let _ = crate::rt::promote_current_thread();
        let link = Downlink::bind("127.0.0.1", "p1".into(), Kind::Instrument, seed).unwrap();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        let receiver = RtpReceiver::new(Arc::new(SrtpContext::new(&agent_keys, &link.server_keys).unwrap())).unwrap();
        link.set_agent_keys(&agent_keys).unwrap();
        let mut sender = SenderLoop::start(Payloads::encode(220.0).unwrap(), Payloads::encode(440.0).unwrap());
        sender.add(link.clone(), profile, seed, 0);
        receiver.punch(format!("127.0.0.1:{}", link.port()).parse().unwrap()).unwrap();
        let mut buf = Vec::with_capacity(2048);
        let mut got = Vec::with_capacity(count.min(200_000));
        let t = Instant::now();
        while got.len() < count && t.elapsed() < max {
            match receiver.read(&mut buf) {
                Ok(_) => {
                    let at = Instant::now();
                    let (h, _) = rtp::parse_header(&buf).unwrap();
                    got.push(Received { index: u64::from(h.sequence.wrapping_sub(link.seq_base)), timestamp: h.timestamp, at });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_micros(200)),
                Err(e) => panic!("{e}"),
            }
        }
        (got, link)
    }

    /// Le retard imputable au banc part de l'heure du paquet, ou de la fin du
    /// paquet précédent de son flux s'il partait encore : la file d'un lien
    /// n'est pas comptée, l'attente derrière un autre flux l'est.
    #[test]
    fn le_retard_du_banc_ne_compte_pas_la_file_du_lien_lui_meme() {
        let t = Instant::now();
        let ms = |v: u64| Duration::from_millis(v);
        // À l'heure, flux libre : aucun retard.
        assert_eq!(lateness(t + ms(5), t + ms(5), None), Duration::ZERO);
        // Parti 2 ms après son heure, flux libre depuis longtemps : 2 ms (banc).
        assert_eq!(lateness(t + ms(7), t + ms(5), Some(t)), ms(2));
        // Dû à 5 ms mais derrière sa propre salve jusqu'à 9 ms, parti à 9 ms :
        // la file du lien, pas le banc.
        assert_eq!(lateness(t + ms(9), t + ms(5), Some(t + ms(9))), Duration::ZERO);
        // Derrière sa salve jusqu'à 9 ms, parti à 10 ms : 1 ms imputable au banc.
        assert_eq!(lateness(t + ms(10), t + ms(5), Some(t + ms(9))), ms(1));
    }

    /// Un flux retiré (musicien parti) n'envoie plus rien.
    #[test]
    fn un_flux_retire_n_envoie_plus_rien() {
        let link = Downlink::bind("127.0.0.1", "p1".into(), Kind::Instrument, 2).unwrap();
        link.set_agent_keys(&SrtpParameters::generate_aead_aes_256_gcm()).unwrap();
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut punch = [0u8; 12];
        punch[0] = 0x80;
        punch[1] = PAYLOAD_TYPE;
        rx.send_to(&punch, ("127.0.0.1", link.port())).unwrap();
        let mut sender = SenderLoop::start(Payloads::encode(220.0).unwrap(), Payloads::encode(440.0).unwrap());
        sender.add(link.clone(), &PeerProfile::preset("regular").unwrap(), 2, 0);
        let mut buf = [0u8; 2048];
        let t = Instant::now();
        let mut before = 0;
        while before < 40 && t.elapsed() < Duration::from_secs(5) {
            before += usize::from(rx.recv_from(&mut buf).is_ok());
        }
        assert_eq!(before, 40, "le flux part");
        link.retire();
        std::thread::sleep(Duration::from_millis(30));
        while rx.recv_from(&mut buf).is_ok() {} // ce qui était en route
        let quiet = Instant::now();
        let mut after = 0;
        while quiet.elapsed() < Duration::from_millis(200) {
            after += usize::from(rx.recv_from(&mut buf).is_ok());
        }
        assert_eq!(after, 0, "plus rien après le retrait");
    }

    /// La dérive lue par l'ESTIMATEUR DE L'AGENT (`sync::drift`), calcul pur :
    /// les instants du calendrier tiennent lieu d'arrivées. +50 ppm est lu +50.
    #[test]
    fn la_derive_simulee_est_celle_que_lit_l_estimateur_de_l_agent() {
        use jamodio_audio_core::sync::drift::DriftEstimator;
        for ppm in [50.0, -50.0, 100.0] {
            let p = PeerProfile { drift_ppm: ppm, ..PeerProfile::preset("regular").unwrap() };
            let mut est = DriftEstimator::new("banc");
            let t0 = Instant::now();
            for f in InstrumentSchedule::new(&p, 1).take_while(|f| f.send_at_us <= 60_000_000) {
                est.observe((f.index as u32).wrapping_mul(FRAME_SAMPLES), t0 + Duration::from_micros(f.send_at_us));
            }
            assert!((est.drift_ppm() - ppm).abs() < 0.1, "{ppm} ppm lus {}", est.drift_ppm());
        }
    }

    /// Échange réel, court (CI) : une forte dérive traverse tout le chemin
    /// (cadence, horodatage, SRTP, réception) et l'estimateur de l'agent la lit.
    /// La précision n'est pas l'objet ici (machine de CI) : cf. le test long.
    #[test]
    fn la_derive_traverse_le_recepteur_de_l_agent() {
        drift_through_agent(1_000.0, Duration::from_secs(10), 250.0);
    }

    /// Échange réel, long (5 min) : +50 ppm lus à ±2 ppm par l'estimateur de
    /// l'agent — la précision que l'agent annonce après quelques minutes
    /// (`sync/drift.rs`). Ce test prouve le banc ET la mesure de l'agent.
    /// `cargo test -p jamodio-bench --release -- --ignored derive_lue --nocapture`
    #[test]
    #[ignore = "5 minutes : à lancer à la main sur une machine calme"]
    fn derive_lue_a_2_ppm_par_le_recepteur_de_l_agent() {
        drift_through_agent(50.0, Duration::from_secs(300), 2.0);
    }

    fn drift_through_agent(ppm: f64, secs: Duration, tolerance_ppm: f64) {
        use jamodio_audio_core::sync::drift::DriftEstimator;
        let p = PeerProfile { drift_ppm: ppm, ..PeerProfile::preset("regular").unwrap() };
        let (got, _) = through_agent_receiver(&p, 1, usize::MAX, secs);
        let mut est = DriftEstimator::new("banc");
        for r in &got {
            est.observe(r.timestamp, r.at);
        }
        let read = est.drift_ppm();
        println!("dérive simulée {ppm} ppm, lue {read:.2} ppm ({} paquets)", got.len());
        assert!((read - ppm).abs() <= tolerance_ppm, "simulée {ppm}, lue {read:.2}");
    }

    /// Échange réel : un paquet désordonné par le banc arrive APRÈS le suivant et
    /// le suivi de séquence de l'agent (`net::seq`) le classe « en retard » (non
    /// joué). Le banc ne fait pas que le dire : l'agent le voit.
    #[test]
    fn le_desordre_du_banc_est_vu_en_retard_par_l_agent() {
        use crate::profile::Reorder;
        use jamodio_audio_core::net::seq::{Arrival, SeqTracker};
        let p = PeerProfile { reorder: Some(Reorder { pct: 5.0, max_depth: 2 }), ..PeerProfile::preset("regular").unwrap() };
        let (got, _) = through_agent_receiver(&p, 4, 2_000, Duration::from_secs(10));
        assert!(got.len() >= 1_980, "{} paquets", got.len());
        // L'ordre reçu est celui du calendrier (la boucle locale ne perd presque
        // rien : on tolère un manque, jamais une inversion inventée).
        let planned: Vec<u64> = InstrumentSchedule::new(&p, 4).take(2_100).map(|f| f.index).collect();
        let mut it = planned.iter();
        assert!(got.iter().all(|r| it.any(|&i| i == r.index)), "ordre reçu = ordre envoyé");
        let mut seq = SeqTracker::new();
        let mut highest = 0;
        let mut overtaken = 0;
        let mut late = 0;
        for r in &got {
            if r.index < highest {
                overtaken += 1;
            }
            highest = highest.max(r.index);
            if matches!(seq.on_packet(r.index as u16), Arrival::Late { .. }) {
                late += 1;
            }
        }
        assert!(overtaken > 60, "~5 % de 2 000 : {overtaken}");
        assert_eq!(late, overtaken, "chaque paquet dépassé est « en retard » pour l'agent");
        assert_eq!(seq.counters().late, late);
    }

    /// Échange réel : les pertes en rafales du banc sont exactement les paquets
    /// que le suivi de séquence de l'agent compte perdus.
    #[test]
    fn les_rafales_du_banc_sont_les_pertes_que_compte_l_agent() {
        use crate::profile::BurstLoss;
        use jamodio_audio_core::net::seq::SeqTracker;
        let p = PeerProfile {
            burst_loss: Some(BurstLoss { rate_pct: 2.0, mean_packets: 4.0, fixed: false }),
            ..PeerProfile::preset("regular").unwrap()
        };
        let (got, _) = through_agent_receiver(&p, 6, 3_000, Duration::from_secs(15));
        let last = got.last().unwrap().index;
        let sent: std::collections::HashSet<u64> =
            InstrumentSchedule::new(&p, 6).take_while(|f| f.index <= last).map(|f| f.index).collect();
        let first = got[0].index;
        let planned_lost = (first..=last).filter(|i| !sent.contains(i)).count() as u64;
        let mut seq = SeqTracker::new();
        for r in &got {
            seq.on_packet(r.index as u16);
        }
        let lost = seq.counters().lost();
        assert!(planned_lost > 20, "{planned_lost}");
        // La boucle locale peut perdre un paquet de plus sur une machine chargée
        // (vu le 28/09/2026) ; jamais moins que ce que le banc a retiré.
        assert!(lost >= planned_lost && lost <= planned_lost + 3, "agent {lost}, banc {planned_lost}");
    }

    /// Ce que l'EXPÉDITEUR de l'agent envoie est reçu, déchiffré et mesuré.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_flux_montant_de_l_agent_est_mesure() {
        let up = Uplink::bind("127.0.0.1").unwrap();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        up.set_agent_keys(&agent_keys).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let listener = {
            let (up, stop) = (up.clone(), stop.clone());
            std::thread::spawn(move || up.listen(stop))
        };
        let target: SocketAddr = format!("127.0.0.1:{}", up.port()).parse().unwrap();
        let ctx = Arc::new(SrtpContext::new(&agent_keys, &up.server_keys).unwrap());
        let sender = RtpSender::new(target, ctx).await.unwrap();
        for i in 0u16..50 {
            // Un paquet manquant (25) et une pause de 20 ms (après 40).
            if i == 25 {
                continue;
            }
            if i == 40 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let h = RtpHeader { payload_type: PAYLOAD_TYPE, sequence: i, timestamp: 0, ssrc: 7, marker: false };
            // Envoi non bloquant de l'agent : un `WouldBlock` (tampon d'envoi du
            // système momentanément plein) ferait perdre la trame — l'agent la
            // laisse tomber, ce test doit rester déterministe : on réessaie.
            loop {
                match sender.send_blocking(rtp::build_packet(&h, &[0u8; 100])) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_micros(200)).await
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            tokio::time::sleep(Duration::from_micros(2_500)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop.store(true, Ordering::Relaxed);
        listener.join().unwrap();
        let w = up.take_window();
        // UDP sur une machine de CI chargée : un paquet peut se perdre en plus
        // de celui qu'on retire (vu le 28/09/2026, 48 au lieu de 49). Ce qui
        // doit tenir : chaque paquet manquant est vu comme tel, et la pause l'est.
        assert!(w.packets >= 45, "{w:?}");
        assert!(w.seq_missing >= 1, "le paquet retiré est vu manquant : {w:?}");
        assert!(w.packets + w.seq_missing <= 50, "rien n'est compté deux fois : {w:?}");
        assert!(w.gaps_over_10ms >= 1 && w.max_gap_us >= 20_000, "{w:?}");
        assert_eq!(w.undecryptable, 0);
    }
}
