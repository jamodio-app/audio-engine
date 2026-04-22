use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use tokio::net::UdpSocket;

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
pub struct RtpSender {
    socket: UdpSocket,
    target: SocketAddr,
}

impl RtpSender {
    pub async fn new(target: SocketAddr) -> std::io::Result<Self> {
        let socket = bind_udp_dscp_ef()?;
        Ok(Self { socket, target })
    }

    pub async fn send(&self, packet: &[u8]) -> std::io::Result<usize> {
        self.socket.send_to(packet, self.target).await
    }

    /// Local address (for NAT hole-punching info).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// Receive RTP packets from the SFU PlainTransport.
/// Sends a "punch" packet first to establish NAT pinhole (comedia mode).
pub struct RtpReceiver {
    socket: UdpSocket,
}

impl RtpReceiver {
    pub async fn new() -> std::io::Result<Self> {
        let socket = bind_udp_dscp_ef()?;
        Ok(Self { socket })
    }

    /// Send a UDP hole-punch packet to the SFU so it discovers our address (comedia).
    pub async fn punch(&self, sfu_addr: SocketAddr) -> std::io::Result<()> {
        // Send a minimal RTP-like packet (just the header, no payload)
        let punch = [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        self.socket.send_to(&punch, sfu_addr).await?;
        Ok(())
    }

    /// Receive a packet. Returns (data_length, sender_address).
    pub async fn recv(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
