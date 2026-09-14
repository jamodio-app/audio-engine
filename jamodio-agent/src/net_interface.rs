//! Type de l'interface réseau qui porte le trafic vers le SFU (Ethernet, Wi-Fi,
//! cellulaire), tel que le système le déclare.
//!
//! Répond à « suis-je en Wi-Fi ? » par un fait, pour le diagnostic du lien (dépôt
//! web : `internal-docs/plans/PLAN-INFOBULLE-LATENCE-2026-09.md`). Relevé hors du
//! thread temps réel (tâche perf-stats), sans envoyer de paquet :
//!   1. le système choisit l'adresse locale d'un socket UDP « connecté » au SFU
//!      (simple consultation de la table de routage) ;
//!   2. on retrouve l'interface qui porte cette adresse ;
//!   3. on lit son type : SystemConfiguration sur macOS (au niveau bas, le Wi-Fi s'y
//!      déclare comme de l'Ethernet), `GetAdaptersAddresses` sous Windows.
//!
//! Une adresse qu'aucune interface connue ne porte (VPN, pont) donne `Other` ; une
//! route introuvable ou la boucle locale donne `None`. Jamais de type deviné.

use jamodio_audio_core::protocol::NetInterface;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

/// Relevé en TÂCHE DE FOND pour la tâche perf-stats. La première consultation de
/// SystemConfiguration coûte près d'une seconde (mesuré : 0,9 s à froid, ~2 ms
/// ensuite) : elle ne doit jamais retenir une tâche du runtime asynchrone. La tâche
/// perf-stats lance un relevé et lit le dernier résultat connu, sans attendre.
#[derive(Debug, Default)]
pub struct Watcher {
    latest: Arc<AtomicU8>,
    in_flight: Arc<AtomicBool>,
}

impl Watcher {
    /// Lance un relevé vers `sfu` (hors du runtime async) s'il n'y en a pas déjà un en
    /// cours. Sans session, efface aussitôt le dernier résultat.
    pub fn refresh(&self, sfu: Option<SocketAddr>) {
        let Some(sfu) = sfu else {
            self.latest.store(encode(None), Ordering::Relaxed);
            return;
        };
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let latest = self.latest.clone();
        let in_flight = self.in_flight.clone();
        tokio::task::spawn_blocking(move || {
            latest.store(encode(toward(sfu)), Ordering::Relaxed);
            in_flight.store(false, Ordering::Release);
        });
    }

    /// Dernier type d'interface relevé.
    pub fn latest(&self) -> Option<NetInterface> {
        decode(self.latest.load(Ordering::Relaxed))
    }
}

fn encode(kind: Option<NetInterface>) -> u8 {
    match kind {
        None => 0,
        Some(NetInterface::Ethernet) => 1,
        Some(NetInterface::Wifi) => 2,
        Some(NetInterface::Cellular) => 3,
        Some(NetInterface::Other) => 4,
    }
}

fn decode(value: u8) -> Option<NetInterface> {
    match value {
        1 => Some(NetInterface::Ethernet),
        2 => Some(NetInterface::Wifi),
        3 => Some(NetInterface::Cellular),
        4 => Some(NetInterface::Other),
        _ => None,
    }
}

/// Type de l'interface par laquelle le système joint `sfu`.
pub fn toward(sfu: SocketAddr) -> Option<NetInterface> {
    let local = local_ip_toward(sfu)?;
    if local.is_loopback() {
        return None;
    }
    platform::interface_type(local)
}

/// Adresse locale que le système choisit pour joindre `sfu`. `connect` sur un socket
/// UDP n'émet rien : il fixe la route.
fn local_ip_toward(sfu: SocketAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if sfu.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(sfu).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// Type d'interface IANA (`ifType`, celui que rend Windows) → type publié.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn classify_if_type(if_type: u32) -> NetInterface {
    match if_type {
        6 => NetInterface::Ethernet,         // ethernetCsmacd
        71 => NetInterface::Wifi,            // ieee80211
        243 | 244 => NetInterface::Cellular, // wwanPP, wwanPP2
        _ => NetInterface::Other,
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
    use core_foundation_sys::base::{CFEqual, CFRelease, CFTypeRef};
    use core_foundation_sys::string::CFStringRef;
    use jamodio_audio_core::protocol::NetInterface;
    use std::ffi::CStr;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[link(name = "SystemConfiguration", kind = "framework")]
    extern "C" {
        fn SCNetworkInterfaceCopyAll() -> CFArrayRef;
        fn SCNetworkInterfaceGetBSDName(interface: CFTypeRef) -> CFStringRef;
        fn SCNetworkInterfaceGetInterfaceType(interface: CFTypeRef) -> CFStringRef;
        static kSCNetworkInterfaceTypeEthernet: CFStringRef;
        static kSCNetworkInterfaceTypeIEEE80211: CFStringRef;
        static kSCNetworkInterfaceTypeWWAN: CFStringRef;
    }

    /// Adresse IP d'un `sockaddr` IPv4 ou IPv6.
    ///
    /// # Safety
    /// `sa` est nul ou pointe un `sockaddr` valide de la famille qu'il annonce.
    unsafe fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
        if sa.is_null() {
            return None;
        }
        match i32::from((*sa).sa_family) {
            libc::AF_INET => {
                let sin = &*(sa as *const libc::sockaddr_in);
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    sin.sin_addr.s_addr,
                ))))
            }
            libc::AF_INET6 => {
                let sin6 = &*(sa as *const libc::sockaddr_in6);
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }

    /// Nom BSD (`en0`…) de l'interface qui porte `ip`.
    fn bsd_name_of(ip: IpAddr) -> Option<String> {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY : `head` reçoit une liste allouée par le système, libérée plus bas.
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return None;
        }
        let mut found = None;
        let mut cur = head;
        while !cur.is_null() {
            // SAFETY : nœud valide de la liste rendue par getifaddrs.
            let ifa = unsafe { &*cur };
            // SAFETY : `ifa_addr` est nul ou un sockaddr valide ; `ifa_name` une C-string.
            if unsafe { sockaddr_ip(ifa.ifa_addr) } == Some(ip) {
                found = unsafe { CStr::from_ptr(ifa.ifa_name) }
                    .to_str()
                    .ok()
                    .map(str::to_owned);
                break;
            }
            cur = ifa.ifa_next;
        }
        // SAFETY : libère la liste rendue par getifaddrs, plus aucune référence dessus.
        unsafe { libc::freeifaddrs(head) };
        found
    }

    pub(super) fn interface_type(ip: IpAddr) -> Option<NetInterface> {
        let bsd = bsd_name_of(ip)?;
        // SAFETY : `CopyAll` rend un tableau possédé (libéré en fin de bloc) ; ses
        // éléments et les chaînes `Get*` ne sont qu'empruntés ; les constantes
        // `kSCNetworkInterfaceType*` sont des CFString du framework.
        unsafe {
            let all = SCNetworkInterfaceCopyAll();
            if all.is_null() {
                return None;
            }
            // Porte l'adresse sans être une interface déclarée (VPN, pont) → Other.
            let mut kind = NetInterface::Other;
            for i in 0..CFArrayGetCount(all) {
                let iface = CFArrayGetValueAtIndex(all, i) as CFTypeRef;
                let name = crate::cf_string::to_string(SCNetworkInterfaceGetBSDName(iface));
                if name.as_deref() != Some(bsd.as_str()) {
                    continue;
                }
                let ty = SCNetworkInterfaceGetInterfaceType(iface) as CFTypeRef;
                if !ty.is_null() {
                    let is = |constant: CFStringRef| CFEqual(ty, constant as CFTypeRef) != 0;
                    kind = if is(kSCNetworkInterfaceTypeIEEE80211) {
                        NetInterface::Wifi
                    } else if is(kSCNetworkInterfaceTypeEthernet) {
                        NetInterface::Ethernet
                    } else if is(kSCNetworkInterfaceTypeWWAN) {
                        NetInterface::Cellular
                    } else {
                        NetInterface::Other
                    };
                }
                break;
            }
            CFRelease(all as CFTypeRef);
            Some(kind)
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::classify_if_type;
    use jamodio_audio_core::protocol::NetInterface;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    };

    const ERROR_SUCCESS: u32 = 0;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;

    /// Adresse IP d'un `SOCKADDR` IPv4 ou IPv6.
    ///
    /// # Safety
    /// `sa` est nul ou pointe un `SOCKADDR` valide de la famille qu'il annonce.
    unsafe fn sockaddr_ip(sa: *const SOCKADDR) -> Option<IpAddr> {
        if sa.is_null() {
            return None;
        }
        match (*sa).sa_family {
            AF_INET => {
                let sin = &*(sa as *const SOCKADDR_IN);
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    sin.sin_addr.S_un.S_addr,
                ))))
            }
            AF_INET6 => {
                let sin6 = &*(sa as *const SOCKADDR_IN6);
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.u.Byte)))
            }
            _ => None,
        }
    }

    pub(super) fn interface_type(ip: IpAddr) -> Option<NetInterface> {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 16 * 1024;
        // Tampon en `u64` : la liste rendue contient des structures alignées sur 8
        // octets ; un tampon d'octets ne garantirait pas cet alignement.
        let mut buf: Vec<u64> = Vec::new();
        let mut ok = false;
        for _ in 0..3 {
            buf = vec![0u64; (size as usize).div_ceil(8)];
            // SAFETY : `buf` offre au moins `size` octets alignés ; `size` reçoit la
            // taille requise si elle est insuffisante.
            let rc = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC),
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                    &mut size,
                )
            };
            match rc {
                ERROR_SUCCESS => {
                    ok = true;
                    break;
                }
                ERROR_BUFFER_OVERFLOW => continue,
                _ => return None,
            }
        }
        if !ok {
            return None;
        }
        let mut adapter = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !adapter.is_null() {
            // SAFETY : nœud valide de la liste écrite dans `buf`, qui vit jusqu'au retour.
            let a = unsafe { &*adapter };
            let mut unicast = a.FirstUnicastAddress;
            while !unicast.is_null() {
                // SAFETY : adresse unicast valide de cet adaptateur.
                let u = unsafe { &*unicast };
                // SAFETY : `lpSockaddr` pointe un SOCKADDR de la famille annoncée.
                if unsafe { sockaddr_ip(u.Address.lpSockaddr) } == Some(ip) {
                    return Some(classify_if_type(a.IfType));
                }
                unicast = u.Next;
            }
            adapter = a.Next;
        }
        Some(NetInterface::Other)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use jamodio_audio_core::protocol::NetInterface;
    use std::net::IpAddr;

    pub(super) fn interface_type(_ip: IpAddr) -> Option<NetInterface> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_iana_windows() {
        assert_eq!(classify_if_type(6), NetInterface::Ethernet);
        assert_eq!(classify_if_type(71), NetInterface::Wifi);
        assert_eq!(classify_if_type(243), NetInterface::Cellular);
        assert_eq!(classify_if_type(244), NetInterface::Cellular);
        assert_eq!(classify_if_type(24), NetInterface::Other); // boucle locale
    }

    #[test]
    fn encodage_du_dernier_releve_sans_perte() {
        for kind in [
            None,
            Some(NetInterface::Ethernet),
            Some(NetInterface::Wifi),
            Some(NetInterface::Cellular),
            Some(NetInterface::Other),
        ] {
            assert_eq!(decode(encode(kind)), kind);
        }
    }

    #[test]
    fn sans_session_le_dernier_releve_est_efface() {
        let watcher = Watcher::default();
        watcher
            .latest
            .store(encode(Some(NetInterface::Wifi)), Ordering::Relaxed);
        watcher.refresh(None);
        assert_eq!(watcher.latest(), None);
    }

    #[test]
    fn boucle_locale_non_publiee() {
        assert_eq!(toward("127.0.0.1:9".parse().unwrap()), None);
    }

    #[test]
    fn adresse_locale_vers_la_boucle() {
        let ip = local_ip_toward("127.0.0.1:9".parse().unwrap());
        assert!(ip.is_some_and(|ip| ip.is_loopback()));
    }

    /// Relevé réel vers une adresse publique (ignoré : dépend du réseau de la machine).
    /// `cargo test -p jamodio-agent net_interface -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn releve_de_l_interface() {
        let target: SocketAddr = "1.1.1.1:3478".parse().unwrap();
        let started = std::time::Instant::now();
        let local = local_ip_toward(target);
        println!("adresse locale : {local:?} en {:?}", started.elapsed());
        for essai in 1..=3 {
            let started = std::time::Instant::now();
            let kind = toward(target);
            println!("essai {essai} : {kind:?} en {:?}", started.elapsed());
        }
    }
}
