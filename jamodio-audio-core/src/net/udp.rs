use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;

use super::rtcp::SendActivity;
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
        tracing::warn!(target: "jamodio::udp", error = %e, "set_tos(EF) non appliqué — trafic en best-effort");
    }
    #[cfg(windows)]
    disable_udp_conn_reset(&sock);
    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock)
}

/// N13 (chantier tampon) — Windows : ne plus faire échouer un `recv` UDP parce
/// qu'un ENVOI précédent a reçu un ICMP « port unreachable ».
///
/// Winsock remonte alors `WSAECONNRESET` (10054) sur la RÉCEPTION, alors que la
/// socket est saine et que les paquets suivants arriveront normalement. C'est un
/// comportement hérité, propre à Windows, que tout récepteur UDP temps réel
/// désactive : `SIO_UDP_CONNRESET = FALSE`. Sans ça, chaque ICMP tardif (le SFU
/// qui recycle un port, un pare-feu) coûtait une erreur de réception — donc une
/// attente — pour rien : 21 occurrences en une session de recette (18/09/2026).
///
/// Best-effort assumé : si l'ioctl échoue, on le DIT et on continue avec le
/// comportement d'avant (la boucle de réception sait encaisser l'erreur), plutôt
/// que de refuser d'ouvrir la socket — aucune session ne doit tomber pour ça.
#[cfg(windows)]
fn disable_udp_conn_reset(sock: &Socket) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{SIO_UDP_CONNRESET, SOCKET, WSAIoctl};

    // FALSE : « ne me remonte plus WSAECONNRESET sur cette socket ».
    let mut disable: u32 = 0;
    let mut returned: u32 = 0;
    // SAFETY : `sock` vit pendant tout l'appel (emprunt), donc son handle est
    // valide ; `disable` et `returned` sont deux u32 locaux dont on passe les
    // adresses avec leur taille exacte ; l'ioctl est SYNCHRONE (OVERLAPPED nul et
    // routine de complétion nulle), donc l'appelé ne conserve aucun pointeur.
    let rc = unsafe {
        WSAIoctl(
            sock.as_raw_socket() as SOCKET,
            SIO_UDP_CONNRESET,
            std::ptr::addr_of_mut!(disable).cast::<core::ffi::c_void>(),
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            std::ptr::addr_of_mut!(returned),
            std::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 {
        tracing::warn!(
            target: "jamodio::udp",
            error = %std::io::Error::last_os_error(),
            "SIO_UDP_CONNRESET non désactivé — les ICMP tardifs continueront de faire échouer des recv"
        );
    }
}

/// Send RTP packets to the SFU PlainTransport.
/// Tous les paquets sont chiffrés en place avec le contexte SRTP (AEAD AES-256-GCM)
/// — agent ↔ SFU n'accepte que SRTP côté Phase 1 production.
pub struct RtpSender {
    socket: UdpSocket,
    target: SocketAddr,
    srtp: Arc<SrtpContext>,
    activity: SendActivity,
}

impl RtpSender {
    pub async fn new(target: SocketAddr, srtp: Arc<SrtpContext>) -> std::io::Result<Self> {
        let socket = bind_udp_dscp_ef()?;
        Ok(Self {
            socket,
            target,
            srtp,
            activity: SendActivity::new(),
        })
    }

    /// Activité d'envoi du flux : écrite par le thread d'encodage après chaque
    /// paquet parti, lue pour le Sender Report (cf. `net::rtcp`).
    pub fn activity(&self) -> &SendActivity {
        &self.activity
    }

    /// Envoie un paquet RTCP DÉJÀ chiffré (cf. `SrtcpContext`) par le socket du
    /// flux : avec rtcpMux + comedia, le SFU n'accepte le RTCP que de l'adresse
    /// d'où part le RTP. Appelé par la tâche RTCP, jamais par le thread audio.
    pub async fn send_rtcp(&self, packet: &[u8]) -> std::io::Result<()> {
        self.socket.send_to(packet, self.target).await.map(|_| ())
    }

    /// Attend le prochain datagramme reçu sur le socket d'envoi. Le SFU n'y
    /// envoie que ses rapports RTCP (Receiver Reports, chiffrés). Rend `false`,
    /// buffer vidé, pour un datagramme venu d'une autre adresse que le SFU.
    pub async fn recv_from_sfu(&self, buf: &mut Vec<u8>) -> std::io::Result<bool> {
        let cap = buf.capacity();
        buf.resize(cap, 0);
        let (len, from) = self.socket.recv_from(buf).await?;
        if from != self.target {
            buf.clear();
            return Ok(false);
        }
        buf.truncate(len);
        Ok(true)
    }

    /// Chiffre SRTP puis envoie en **NON-BLOQUANT** — conçu pour être appelé
    /// directement depuis le thread d'encode RT (pas de hop tokio → pas de gigue
    /// d'égression sous charge). Le socket est en non-blocking : `try_send_to`
    /// rend immédiatement. `Err(WouldBlock)` (buffer noyau d'envoi plein,
    /// rarissime en UDP) signale au caller de **dropper** la frame (concealée par
    /// le PLC récepteur) plutôt que de staller le thread RT. Retourne `Ok(0)` si
    /// le chiffrement échoue. `packet` doit avoir la capacité pour l'auth tag
    /// SRTP (~16 octets ajoutés).
    ///
    /// NB : `try_send_to` ne nécessite PAS d'être appelé dans le runtime tokio
    /// (c'est un syscall non-bloquant via la registration du socket, créée au
    /// `new()`) — valide tant que le runtime du process est vivant (toute la vie
    /// de l'agent côté Tauri).
    pub fn send_blocking(&self, packet: Vec<u8>) -> std::io::Result<usize> {
        let mut buf = packet;
        if let Err(e) = self.srtp.protect(&mut buf) {
            tracing::error!(target: "jamodio::srtp", role = "sender", error = ?e, "SRTP protect failed");
            return Ok(0);
        }
        self.socket.try_send_to(&buf, self.target)
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
        getrandom::getrandom(&mut seed)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
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
            tracing::error!(target: "jamodio::srtp", role = "receiver", op = "punch", error = ?e, "SRTP protect failed");
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
        // SRTCP (PT 200..=204 au 2e octet) : le SFU en ENVOIE bien aux agents — des
        // Sender Reports sur ce transport de réception, des Receiver Reports sur le
        // transport d'envoi (cf. worker mediasoup `Transport::SendRtcp`). L'agent ne
        // parle pas encore RTCP : on les ignore ici, sans les déchiffrer (voie B du
        // plan « infobulle latence » côté web).
        // Paquets RTP : déchiffrés en place.
        if len >= 2 && buf[1] >= 200 && buf[1] <= 204 {
            return Ok((0, addr));
        }
        if let Err(e) = self.srtp.unprotect(buf) {
            tracing::warn!(target: "jamodio::srtp", role = "receiver", error = ?e, "SRTP unprotect failed");
            return Ok((0, addr));
        }
        Ok((buf.len(), addr))
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
