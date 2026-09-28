//! Horodatage NOYAU de la réception — Lot 1-D2 du plan « ms en trop sur PC ».
//!
//! # Pourquoi
//!
//! L'agent date l'arrivée d'un paquet quand SA tâche de réception le lit. Au banc
//! du 28/09/2026 (Mac de Ben, faux serveur précis à 0,23 ms près), l'agent n'a
//! rien lu pendant 21 à 27 ms, sur deux flux à la fois. Le paquet était-il arrivé
//! dans la machine sans être lu (cause LOCALE : la tâche de réception servie trop
//! tard), ou n'était-il pas arrivé (cause en amont) ? Seule l'heure à laquelle le
//! SYSTÈME a reçu le paquet le dit.
//!
//! # Ce que fait le module
//!
//! - [`enable`] demande au système d'horodater chaque paquet reçu sur la socket ;
//! - [`recv`] lit UN paquet avec cet horodatage et rend l'attente entre sa
//!   réception par le système et sa lecture par nous ([`Received::stack_delay`]).
//!
//! macOS : `SO_TIMESTAMP` + `recvmsg` (heure murale, µs). Windows :
//! `SIO_TIMESTAMPING` + `WSARecvMsg` (compteur QPC) — disponible selon la carte et
//! son pilote ; le banc de la sonde 1-D1 le vérifie machine par machine.
//!
//! MESURE SEULE : l'instant d'arrivée utilisé par la réception ne change pas. Le
//! coût est celui d'un `recvmsg` au lieu d'un `recvfrom` (même appel système,
//! quelques octets de contrôle en plus), sur la tâche de réception — jamais dans
//! le callback audio.

use std::net::SocketAddr;
use std::time::Duration;

/// Ce qu'une lecture a rendu.
#[derive(Debug, Clone, Copy)]
pub struct Received {
    pub len: usize,
    pub from: SocketAddr,
    /// Attente entre la réception par le système et notre lecture. `None` : pas
    /// d'horodatage sur ce paquet (non pris en charge, ou horloge incohérente).
    pub stack_delay: Option<Duration>,
}

/// Borne de cohérence : un horodatage qui dirait qu'un paquet attend depuis plus
/// longtemps que ça n'est pas une attente, c'est une horloge différente (heure
/// matérielle de la carte, heure murale ajustée) — on ne le rend pas.
const MAX_PLAUSIBLE: Duration = Duration::from_secs(1);

#[cfg(target_os = "macos")]
pub use unix::{enable, recv};
#[cfg(windows)]
pub use win::{enable, recv};

#[cfg(target_os = "macos")]
mod unix {
    use super::{Received, MAX_PLAUSIBLE};
    use std::io;
    use std::mem::{size_of, MaybeUninit};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::os::fd::RawFd;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Demande l'horodatage de réception sur la socket.
    pub fn enable(fd: RawFd) -> io::Result<()> {
        let on: libc::c_int = 1;
        // SAFETY : `on` vit pendant l'appel, sa taille est passée exactement.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMP,
                (&on as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Lit un datagramme (socket non bloquante : `WouldBlock` s'il n'y a rien).
    pub fn recv(fd: RawFd, buf: &mut [u8]) -> io::Result<Received> {
        let mut name = MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        // Aligné pour `cmsghdr` ; largement assez pour un `timeval`.
        let mut control = [0u64; 16];
        // SAFETY : `msghdr` est une structure C sans invariant ; tous ses champs
        // sont posés ci-dessous.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = name.as_mut_ptr().cast();
        msg.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of::<[u64; 16]>() as libc::socklen_t;
        // SAFETY : tous les pointeurs de `msg` désignent des tampons vivants
        // pendant l'appel, avec leurs tailles exactes.
        let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
        let read_at = SystemTime::now();
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY : le système a rempli `name` (famille lue avant tout usage).
        let from = unsafe { sockaddr_to_std(name.assume_init_ref()) }
            .ok_or_else(|| io::Error::other("adresse d'origine non IPv4"))?;
        let mut stamp = None;
        // SAFETY : parcours des en-têtes de contrôle rendus par `recvmsg`, avec
        // les macros du système sur le `msg` rempli.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_TIMESTAMP {
                    let tv: libc::timeval = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast());
                    stamp = Some(UNIX_EPOCH + Duration::new(tv.tv_sec as u64, (tv.tv_usec as u32) * 1000));
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        let stack_delay = stamp
            .and_then(|s| read_at.duration_since(s).ok())
            .filter(|d| *d < MAX_PLAUSIBLE);
        Ok(Received { len: n as usize, from, stack_delay })
    }

    unsafe fn sockaddr_to_std(s: &libc::sockaddr_storage) -> Option<SocketAddr> {
        if i32::from(s.ss_family) != libc::AF_INET {
            return None;
        }
        let a = &*(s as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>();
        let ip = Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
        Some(SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(a.sin_port))))
    }
}

#[cfg(windows)]
mod win {
    use super::{Received, MAX_PLAUSIBLE};
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::sync::OnceLock;
    use std::time::Duration;
    use windows_sys::Win32::Networking::WinSock::{
        WSAGetLastError, WSAIoctl, AF_INET, CMSGHDR, SIO_GET_EXTENSION_FUNCTION_POINTER,
        SIO_TIMESTAMPING, SOCKADDR_IN, SOCKADDR_STORAGE, SOCKET, SOL_SOCKET, SO_TIMESTAMP,
        TIMESTAMPING_CONFIG, TIMESTAMPING_FLAG_RX, WSABUF, WSAEWOULDBLOCK, WSAID_WSARECVMSG, WSAMSG,
    };
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    /// `WSARecvMsg`, obtenue à l'exécution (extension Winsock). OVERLAPPED et
    /// routine nuls : appel synchrone sur socket non bloquante.
    type RecvMsgFn = unsafe extern "system" fn(SOCKET, *mut WSAMSG, *mut u32, *mut c_void, *mut c_void) -> i32;

    /// Même pointeur pour toutes les sockets UDP du fournisseur par défaut.
    static RECV_MSG: OnceLock<Option<RecvMsgFn>> = OnceLock::new();

    /// Alignement de `WSACMSGHDR` et de ses données sur x64.
    const CMSG_ALIGN: usize = 8;

    fn wsa_err() -> io::Error {
        // SAFETY : lecture de l'erreur Winsock du fil courant.
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    fn recv_msg_fn(s: SOCKET) -> Option<RecvMsgFn> {
        *RECV_MSG.get_or_init(|| {
            let mut f: Option<RecvMsgFn> = None;
            let guid = WSAID_WSARECVMSG;
            let mut ret = 0u32;
            // SAFETY : GUID en entrée, pointeur de fonction en sortie, tailles exactes.
            let rc = unsafe {
                WSAIoctl(
                    s,
                    SIO_GET_EXTENSION_FUNCTION_POINTER,
                    (&guid as *const windows_sys::core::GUID).cast(),
                    size_of::<windows_sys::core::GUID>() as u32,
                    (&mut f as *mut Option<RecvMsgFn>).cast(),
                    size_of::<Option<RecvMsgFn>>() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                    None,
                )
            };
            if rc == 0 { f } else { None }
        })
    }

    /// Demande l'horodatage de réception. Un refus (carte ou pilote) est rendu
    /// tel quel : l'appelant le dit dans le journal et lit sans horodatage.
    pub fn enable(s: SOCKET) -> io::Result<()> {
        if recv_msg_fn(s).is_none() {
            return Err(io::Error::other("WSARecvMsg indisponible"));
        }
        let cfg = TIMESTAMPING_CONFIG { Flags: TIMESTAMPING_FLAG_RX, TxTimestampsBuffered: 0 };
        let mut ret = 0u32;
        // SAFETY : configuration en entrée, taille exacte ; appel synchrone.
        let rc = unsafe {
            WSAIoctl(
                s,
                SIO_TIMESTAMPING,
                (&cfg as *const TIMESTAMPING_CONFIG).cast(),
                size_of::<TIMESTAMPING_CONFIG>() as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc != 0 {
            return Err(wsa_err());
        }
        Ok(())
    }

    fn qpc() -> (i64, i64) {
        let (mut now, mut freq) = (0i64, 0i64);
        // SAFETY : pointeurs sur la pile ; ne peut pas échouer depuis XP.
        unsafe {
            QueryPerformanceCounter(&mut now);
            QueryPerformanceFrequency(&mut freq);
        }
        (now, freq)
    }

    /// Lit un datagramme (socket non bloquante : `WouldBlock` s'il n'y a rien).
    pub fn recv(s: SOCKET, buf: &mut [u8]) -> io::Result<Received> {
        let f = recv_msg_fn(s).ok_or_else(|| io::Error::other("WSARecvMsg indisponible"))?;
        // SAFETY : structures C remplies par le système ; zéro est une valeur valide.
        let mut name: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
        let mut data = WSABUF { len: buf.len() as u32, buf: buf.as_mut_ptr() };
        let mut control = [0u64; 16];
        let mut msg = WSAMSG {
            name: (&mut name as *mut SOCKADDR_STORAGE).cast(),
            namelen: size_of::<SOCKADDR_STORAGE>() as i32,
            lpBuffers: &mut data,
            dwBufferCount: 1,
            Control: WSABUF { len: size_of::<[u64; 16]>() as u32, buf: control.as_mut_ptr().cast() },
            dwFlags: 0,
        };
        let mut n = 0u32;
        // SAFETY : tous les pointeurs de `msg` désignent des tampons vivants
        // pendant l'appel ; appel synchrone (OVERLAPPED nul).
        let rc = unsafe { f(s, &mut msg, &mut n, std::ptr::null_mut(), std::ptr::null_mut()) };
        let (now, freq) = qpc();
        if rc != 0 {
            let e = wsa_err();
            return Err(if e.raw_os_error() == Some(WSAEWOULDBLOCK) {
                io::ErrorKind::WouldBlock.into()
            } else {
                e
            });
        }
        if name.ss_family != AF_INET {
            return Err(io::Error::other("adresse d'origine non IPv4"));
        }
        // SAFETY : famille vérifiée ci-dessus.
        let a = unsafe { &*(&name as *const SOCKADDR_STORAGE).cast::<SOCKADDR_IN>() };
        // SAFETY : champ d'union lu comme l'entier réseau qu'il est.
        let ip = Ipv4Addr::from(u32::from_be(unsafe { a.sin_addr.S_un.S_addr }));
        let from = SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(a.sin_port)));

        // Données de contrôle : cherche SO_TIMESTAMP (QPC du système).
        let ctl_len = (msg.Control.len as usize).min(size_of::<[u64; 16]>());
        // SAFETY : `control` vu comme octets, longueur bornée par sa taille.
        let ctl = unsafe { std::slice::from_raw_parts(control.as_ptr().cast::<u8>(), ctl_len) };
        let hdr_len = size_of::<CMSGHDR>();
        let mut off = 0;
        let mut stamp = None;
        while off + hdr_len <= ctl.len() {
            // SAFETY : en-tête entièrement dans la tranche, lecture non alignée.
            let h: CMSGHDR = unsafe { std::ptr::read_unaligned(ctl[off..].as_ptr().cast()) };
            if h.cmsg_len < hdr_len {
                break;
            }
            let data_at = off + hdr_len.div_ceil(CMSG_ALIGN) * CMSG_ALIGN;
            if h.cmsg_level == SOL_SOCKET && h.cmsg_type == SO_TIMESTAMP as i32 && data_at + 8 <= ctl.len() {
                let mut b = [0u8; 8];
                b.copy_from_slice(&ctl[data_at..data_at + 8]);
                stamp = Some(u64::from_ne_bytes(b) as i64);
            }
            off += h.cmsg_len.div_ceil(CMSG_ALIGN) * CMSG_ALIGN;
        }
        let stack_delay = stamp
            .filter(|&t| freq > 0 && now >= t)
            .map(|t| Duration::from_secs_f64((now - t) as f64 / freq as f64))
            .filter(|d| *d < MAX_PLAUSIBLE);
        Ok(Received { len: n as usize, from, stack_delay })
    }
}

/// Autres systèmes : pas d'horodatage noyau (l'agent n'y est pas livré).
#[cfg(not(any(target_os = "macos", windows)))]
pub fn enable<T>(_s: T) -> std::io::Result<()> {
    Err(std::io::Error::other("horodatage noyau non pris en charge sur ce système"))
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn recv<T>(_s: T, _buf: &mut [u8]) -> std::io::Result<Received> {
    Err(std::io::Error::other("horodatage noyau non pris en charge sur ce système"))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io;
    use std::os::fd::AsRawFd;

    /// Un paquet reçu porte son heure d'arrivée dans le système, et l'attente
    /// mesurée est celle qu'on lui a imposée avant de le lire.
    #[test]
    fn un_paquet_lu_en_retard_dit_combien_de_temps_il_a_attendu() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_nonblocking(true).unwrap();
        enable(rx.as_raw_fd()).unwrap();
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(b"jamodio", rx.local_addr().unwrap()).unwrap();
        // Le paquet attend 20 ms dans la file du système avant d'être lu.
        std::thread::sleep(Duration::from_millis(20));
        let mut buf = [0u8; 64];
        let r = recv(rx.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(&buf[..r.len], b"jamodio");
        assert_eq!(r.from, tx.local_addr().unwrap());
        let d = r.stack_delay.expect("horodatage présent");
        assert!(d >= Duration::from_millis(19) && d < Duration::from_millis(200), "{d:?}");
    }

    #[test]
    fn sans_paquet_la_lecture_rend_wouldblock() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_nonblocking(true).unwrap();
        enable(rx.as_raw_fd()).unwrap();
        let mut buf = [0u8; 64];
        let e = recv(rx.as_raw_fd(), &mut buf).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
    }
}
