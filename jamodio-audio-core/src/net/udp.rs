use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;

use super::srtp::SrtpContext;

// DSCP EF (Expedited Forwarding, RFC 3246) pour le trafic audio temps réel.
// Valeur 6 bits = 46 (binaire 101110). Le byte ToS IP = DSCP << 2 = 0xB8.
// Les routeurs domestiques respectant WMM (Wi-Fi Multimedia) mappent EF
// vers la classe "Voice" et priorisent ces paquets sur la file d'attente.
// Best-effort : si le kernel refuse (non root), on log et on continue.
const DSCP_EF_TOS: u32 = 0xB8;

/// Bind un UdpSocket IPv4 sur `0.0.0.0:0` et applique DSCP EF marking.
/// Factorise la logique partagée entre RtpSender et RtpReceiver.
fn bind_udp_dscp_ef() -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_nonblocking(true)?;
    let local: SocketAddr = "0.0.0.0:0".parse().unwrap();
    sock.bind(&local.into())?;
    if let Err(e) = sock.set_tos(DSCP_EF_TOS) {
        eprintln!("[udp] set_tos(EF) non appliqué ({e}) — trafic en best-effort");
    }
    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock)
}

/// Send RTP packets to the SFU PlainTransport.
/// Tous les paquets sont chiffrés en place avec le contexte SRTP (AEAD AES-256-GCM)
/// — agent ↔ SFU n'accepte que SRTP côté Phase 1 production.
pub struct RtpSender {
    socket: UdpSocket,
    target: SocketAddr,
    srtp: Arc<SrtpContext>,
}

impl RtpSender {
    pub async fn new(target: SocketAddr, srtp: Arc<SrtpContext>) -> std::io::Result<Self> {
        let socket = bind_udp_dscp_ef()?;
        Ok(Self { socket, target, srtp })
    }

    /// Encrypt with SRTP then send. The `packet` buffer must have enough capacity
    /// for the auth tag (~16 bytes appended). Returns 0 if SRTP encryption fails.
    pub async fn send(&self, packet: Vec<u8>) -> std::io::Result<usize> {
        let mut buf = packet;
        if let Err(e) = self.srtp.protect(&mut buf) {
            eprintln!("[RtpSender] SRTP protect failed: {e}");
            return Ok(0);
        }
        self.socket.send_to(&buf, self.target).await
    }

    /// Local address (for NAT hole-punching info).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// Receive RTP packets from the SFU PlainTransport.
/// Comedia : la SFU n'identifie l'adresse de l'agent qu'au premier paquet SRTP
/// valide reçu — on envoie un punch SRTP-chiffré (RTP minimal vide) au démarrage.
///
/// Anti-replay AEAD : libsrtp **refuse** de réutiliser un nonce (SSRC, seq, ROC).
/// Chaque punch incrémente seq + timestamp pour générer un nonce unique.
pub struct RtpReceiver {
    socket: UdpSocket,
    srtp: Arc<SrtpContext>,
    punch_ssrc: u32,
    punch_seq: AtomicU16,
    punch_ts: AtomicU32,
}

impl RtpReceiver {
    pub async fn new(srtp: Arc<SrtpContext>) -> std::io::Result<Self> {
        let socket = bind_udp_dscp_ef()?;
        // SSRC, seq, ts initiaux aléatoires (RFC 3550 § 5.1) — évite les collisions
        // si le SFU réutilise un transport ou si plusieurs receivers cohabitent.
        let mut seed = [0u8; 10];
        getrandom::getrandom(&mut seed).map_err(std::io::Error::other)?;
        let ssrc = u32::from_be_bytes([seed[0], seed[1], seed[2], seed[3]]);
        let seq = u16::from_be_bytes([seed[4], seed[5]]);
        let ts = u32::from_be_bytes([seed[6], seed[7], seed[8], seed[9]]);
        Ok(Self {
            socket,
            srtp,
            punch_ssrc: ssrc,
            punch_seq: AtomicU16::new(seq),
            punch_ts: AtomicU32::new(ts),
        })
    }

    /// Send a UDP hole-punch packet to the SFU so it discovers our address (comedia).
    /// Le paquet doit être un RTP **valide chiffré** sinon mediasoup le rejette
    /// (avec enableSrtp:true, comedia ne lit que les paquets qui passent l'auth SRTP).
    /// Chaque appel = nouveau (seq, ts) → nonce SRTP unique, pas de REPLAY_FAIL.
    pub async fn punch(&self, sfu_addr: SocketAddr) -> std::io::Result<()> {
        let seq = self.punch_seq.fetch_add(1, Ordering::Relaxed);
        let ts = self.punch_ts.fetch_add(960, Ordering::Relaxed); // +20ms @ 48kHz
        let mut punch: Vec<u8> = Vec::with_capacity(64);
        punch.push(0x80);                           // V=2, no padding/ext/CC
        punch.push(0x6f);                           // PT=111 (Opus), marker=0
        punch.extend_from_slice(&seq.to_be_bytes());
        punch.extend_from_slice(&ts.to_be_bytes());
        punch.extend_from_slice(&self.punch_ssrc.to_be_bytes());
        if let Err(e) = self.srtp.protect(&mut punch) {
            eprintln!("[RtpReceiver] punch SRTP protect failed: {e}");
            return Ok(());
        }
        self.socket.send_to(&punch, sfu_addr).await?;
        Ok(())
    }

    /// Receive an SRTP packet, decrypt in place. Returns (data_length, sender_address).
    /// Si la décryption échoue, retourne (0, addr) — caller doit ignorer.
    pub async fn recv(&self, buf: &mut Vec<u8>) -> std::io::Result<(usize, SocketAddr)> {
        // Resize to capacity so recv_from can fill it.
        let cap = buf.capacity();
        buf.resize(cap, 0);
        let (len, addr) = self.socket.recv_from(buf).await?;
        buf.truncate(len);
        // SRTCP packets (PT 200..=204 in second byte) need unprotect_rtcp,
        // but for now mediasoup doesn't send SRTCP back to comedia agents — drop them.
        // RTP packets : decrypt in place.
        if len >= 2 && buf[1] >= 200 && buf[1] <= 204 {
            // RTCP : currently not handled (no encoder/decoder for SR/RR feedback)
            return Ok((0, addr));
        }
        if let Err(e) = self.srtp.unprotect(buf) {
            eprintln!("[RtpReceiver] SRTP unprotect failed: {e}");
            return Ok((0, addr));
        }
        Ok((buf.len(), addr))
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
