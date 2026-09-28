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
        }))
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
        while !stop.load(Ordering::Relaxed) {
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
}

/// Mesures du faux serveur sur une fenêtre, vidées chaque seconde.
#[derive(Debug, Default, Clone)]
pub struct SenderWindow {
    /// Retard d'envoi sur l'heure prévue (µs), un par paquet.
    pub late_us: Vec<u32>,
    pub sent: u64,
    pub errors: u64,
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
    pub fn add(&mut self, link: Arc<Downlink>, profile: &PeerProfile, seed: u64) {
        let schedule: Box<dyn Iterator<Item = Frame> + Send> = match (link.kind, profile.voice) {
            (Kind::Instrument, _) => Box::new(InstrumentSchedule::new(profile, seed)),
            (Kind::Voice, Some(speech)) => Box::new(VoiceSchedule::new(profile, speech, seed)),
            (Kind::Voice, None) => Box::new(VoiceSchedule::new(profile, Speech::Always, seed)),
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
                Ok((link, schedule)) => active.push(Active { link, schedule, next: None, t0: None }),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
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
                let ok = a
                    .link
                    .packet(f, payload)
                    .and_then(|p| a.link.socket.send_to(&p, addr).map_err(|e| e.to_string()))
                    .is_ok();
                let late = now.saturating_duration_since(due).as_micros().min(u32::MAX as u128) as u32;
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
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

/// Mesures d'un flux MONTANT (ce que l'agent envoie) sur une fenêtre.
#[derive(Debug, Default, Clone)]
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
                if gap > 10_000 {
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
    #[tokio::test(flavor = "multi_thread")]
    async fn le_recepteur_de_l_agent_recoit_les_flux_du_faux_serveur() {
        let link = Downlink::bind("127.0.0.1", "p1".into(), Kind::Instrument, 1).unwrap();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        // Côté agent : ses clés en local, celles du serveur en distant.
        let agent_ctx = Arc::new(SrtpContext::new(&agent_keys, &link.server_keys).unwrap());
        let receiver = RtpReceiver::new(agent_ctx).await.unwrap();
        link.set_agent_keys(&agent_keys).unwrap();

        let mut sender = SenderLoop::start(Payloads::encode(220.0).unwrap(), Payloads::encode(440.0).unwrap());
        sender.add(link.clone(), &PeerProfile::preset("regular").unwrap(), 1);
        let sfu: SocketAddr = format!("127.0.0.1:{}", link.port()).parse().unwrap();
        receiver.punch(sfu).await.unwrap();

        let mut buf = Vec::with_capacity(2048);
        let mut seqs = Vec::new();
        let t = Instant::now();
        while seqs.len() < 200 && t.elapsed() < Duration::from_secs(5) {
            let (len, _) = tokio::time::timeout(Duration::from_secs(2), receiver.recv(&mut buf))
                .await
                .expect("le flux arrive")
                .unwrap();
            assert!(len > 12, "paquet déchiffré par l'agent");
            seqs.push(rtp::parse_header(&buf).unwrap().0.sequence);
        }
        assert_eq!(seqs.len(), 200);
        assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)), "numérotation continue");
        // ~0,5 s pour 200 trames de 2,5 ms (tolérance large : machine de CI).
        assert!(t.elapsed() >= Duration::from_millis(450), "cadence tenue, pas de rafale");
        let w = sender.take_window();
        assert!(w.sent >= 200 && w.errors == 0, "{w:?}");
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
        assert_eq!(w.packets, 49);
        assert_eq!(w.seq_missing, 1);
        assert!(w.gaps_over_10ms >= 1 && w.max_gap_us >= 20_000, "{w:?}");
        assert_eq!(w.undecryptable, 0);
    }
}
