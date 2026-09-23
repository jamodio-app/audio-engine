//! Sonde — horodatage NOYAU de la réception UDP sous Windows.
//!
//! Lot 1-D1 du plan « ms en trop sur PC » (dépôt web :
//! `internal-docs/plans/PLAN-LOT1-MS-PC-2026-09.md`). Sur le NUC, le flux reçu
//! montre 5 à 7,8 ms de gigue quand l'autre PC en voit 2,3 pour le même type de
//! flux. L'agent horodate l'arrivée sur un fil tokio ordinaire : on ne sait donc
//! pas si ce retard est né AVANT la machine (réseau) ou DEDANS (le fil qui lit la
//! socket servi trop tard). Windows sait dater l'arrivée d'un paquet dans sa pile
//! réseau (`SIO_TIMESTAMPING` + `SO_TIMESTAMP`) — si la carte et le pilote le
//! permettent. Cette sonde répond à deux questions, dans cet ordre :
//!
//! 1. **Est-ce disponible sur cette machine ?** Refus de `SIO_TIMESTAMPING`, ou
//!    aucun horodatage joint aux paquets → la mesure 1-D2 ne pourra pas se faire
//!    ainsi, et on le sait avant d'écrire une ligne dans l'agent.
//! 2. **Combien de temps un paquet attend-il entre la pile réseau et le
//!    programme ?** (`app - noyau`, en µs.) Et, en comparant les écarts entre
//!    arrivées vus par le noyau et vus par le programme : la gigue naît-elle
//!    avant la machine ou dedans ?
//!
//! Elle ne touche pas à l'agent : c'est un programme à part, qui écoute son
//! propre port. `--mmcss` promeut le fil de réception en « Pro Audio », comme le
//! décodage de l'agent : comparer avec et sans dit ce qu'un fil prioritaire
//! (Lot 1-D3) gagnerait sur cette machine.
//!
//! ⚠ L'horodatage n'est comparable à l'horloge du programme que s'il est
//! LOGICIEL (valeur QPC). Un horodatage MATÉRIEL est dans l'horloge de la carte :
//! la sonde le signale (écarts négatifs ou absurdes) au lieu de publier des
//! chiffres faux.
//!
//! Sortie en ASCII sans accents : lisible dans n'importe quelle console Windows.
//!
//! Usage — le NUC reçoit, une autre machine émet (même réseau que les sessions) :
//!
//! ```text
//! NUC   : cargo run --release -p jamodio-agent --example recv_timestamp_probe -- recv 50999
//! NUC   : cargo run --release -p jamodio-agent --example recv_timestamp_probe -- recv 50999 --mmcss
//! Mac   : cargo run --release -p jamodio-agent --example recv_timestamp_probe -- send 192.168.1.20:50999
//! seul  : cargo run --release -p jamodio-agent --example recv_timestamp_probe -- loopback
//! ```
//!
//! `--seconds N` (défaut 60) règle la durée. L'émetteur envoie un paquet toutes
//! les 2,5 ms, comme un flux Jamodio. Le pare-feu Windows peut demander
//! d'autoriser la sonde à la première réception. Lancer les mesures avec la
//! charge habituelle (AmpliTube ouvert, etc.) : c'est elle qu'on cherche.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// Cadence d'un flux Jamodio : une trame Opus toutes les 2,5 ms.
const PERIOD: Duration = Duration::from_micros(2_500);
/// Taille d'un paquet Jamodio typique (RTP + Opus 2,5 ms + tag SRTP), octets.
const PACKET_BYTES: usize = 160;

struct Args {
    mode: String,
    target: Option<String>,
    mmcss: bool,
    seconds: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mode = it.next().ok_or("mode manquant : recv <port> | send <ip:port> | loopback")?;
    let mut args = Args { mode, target: None, mmcss: false, seconds: 60 };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mmcss" => args.mmcss = true,
            "--seconds" => {
                args.seconds = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--seconds attend un nombre")?;
            }
            other if args.target.is_none() => args.target = Some(other.to_string()),
            other => return Err(format!("argument inattendu : {other}")),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("recv_timestamp_probe : {e}");
            std::process::exit(2);
        }
    };
    let run = Duration::from_secs(args.seconds);
    let code = match args.mode.as_str() {
        "send" => match args.target.as_deref() {
            Some(t) => send(t, run),
            None => {
                eprintln!("send <ip:port>");
                2
            }
        },
        "recv" => match args.target.as_deref().and_then(|p| p.parse::<u16>().ok()) {
            Some(port) => recv(port, args.mmcss, run),
            None => {
                eprintln!("recv <port>");
                2
            }
        },
        "loopback" => {
            // Port éphémère fixe pour la sonde ; l'émetteur tourne dans ce processus.
            let port = 50_998;
            let target = format!("127.0.0.1:{port}");
            std::thread::spawn(move || {
                // Laisse le récepteur ouvrir sa socket d'abord.
                std::thread::sleep(Duration::from_millis(300));
                send(&target, run);
            });
            recv(port, args.mmcss, run + Duration::from_millis(500))
        }
        other => {
            eprintln!("mode inconnu : {other}");
            2
        }
    };
    std::process::exit(code);
}

/// Émet un paquet toutes les 2,5 ms vers `target`, numéroté. La régularité de
/// l'émetteur ne fausse pas la mesure : on compare deux horloges du RÉCEPTEUR.
fn send(target: &str, run: Duration) -> i32 {
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("send: bind impossible : {e}");
            return 1;
        }
    };
    let mut buf = [0u8; PACKET_BYTES];
    let start = Instant::now();
    let mut next = start;
    let mut seq: u32 = 0;
    println!("send: {target}, un paquet de {PACKET_BYTES} octets toutes les 2,5 ms, {} s", run.as_secs());
    while start.elapsed() < run {
        buf[..4].copy_from_slice(&seq.to_be_bytes());
        if let Err(e) = sock.send_to(&buf, target) {
            eprintln!("send: erreur d'envoi : {e}");
            return 1;
        }
        seq = seq.wrapping_add(1);
        next += PERIOD;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    println!("send: {seq} paquets envoyes");
    0
}

#[cfg(not(windows))]
fn recv(_port: u16, _mmcss: bool, _run: Duration) -> i32 {
    eprintln!("recv : sonde Windows uniquement (SIO_TIMESTAMPING). Sur cette machine, seul `send` sert.");
    2
}

#[cfg(windows)]
fn recv(port: u16, mmcss: bool, run: Duration) -> i32 {
    win::recv(port, mmcss, run)
}

#[cfg(windows)]
mod win {
    use super::PACKET_BYTES;
    use std::ffi::c_void;
    use std::net::UdpSocket;
    use std::os::windows::io::AsRawSocket;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Networking::WinSock::{
        WSAGetLastError, WSAIoctl, CMSGHDR, SIO_GET_EXTENSION_FUNCTION_POINTER, SIO_TIMESTAMPING,
        SOCKET, SOL_SOCKET, SO_TIMESTAMP, TIMESTAMPING_CONFIG, TIMESTAMPING_FLAG_RX, WSABUF,
        WSAID_WSARECVMSG, WSAMSG,
    };
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;

    /// Fenêtre des lignes de mesure intermédiaires.
    const WINDOW: Duration = Duration::from_secs(5);

    /// Statistiques d'une série de mesures (µs).
    #[derive(Default)]
    struct Series(Vec<f64>);

    impl Series {
        fn push(&mut self, v: f64) {
            self.0.push(v);
        }
        fn pct(&self, p: f64) -> f64 {
            if self.0.is_empty() {
                return f64::NAN;
            }
            let mut v = self.0.clone();
            v.sort_by(f64::total_cmp);
            v[((v.len() - 1) as f64 * p).round() as usize]
        }
        fn max(&self) -> f64 {
            self.0.iter().copied().fold(f64::NAN, f64::max)
        }
    }

    /// `WSARecvMsg`, obtenue à l'exécution (extension Winsock). Les deux derniers
    /// paramètres (OVERLAPPED, routine) restent nuls : réception bloquante.
    type RecvMsgFn = unsafe extern "system" fn(SOCKET, *mut WSAMSG, *mut u32, *mut c_void, *mut c_void) -> i32;

    /// Alignement des en-têtes et données de `WSACMSGHDR` sur x64.
    const CMSG_ALIGN: usize = 8;

    fn align(n: usize) -> usize {
        (n + CMSG_ALIGN - 1) & !(CMSG_ALIGN - 1)
    }

    fn qpc() -> i64 {
        let mut v = 0i64;
        // SAFETY: pointeur valide sur la pile ; ne peut pas échouer depuis XP.
        unsafe { QueryPerformanceCounter(&mut v) };
        v
    }

    /// Cherche l'horodatage `SO_TIMESTAMP` dans les données de contrôle.
    fn find_timestamp(control: &[u8]) -> Option<u64> {
        let hdr_len = std::mem::size_of::<CMSGHDR>();
        let mut off = 0;
        while off + hdr_len <= control.len() {
            // SAFETY: lecture non alignée d'un en-tête entièrement dans la tranche.
            let hdr: CMSGHDR = unsafe { std::ptr::read_unaligned(control[off..].as_ptr().cast()) };
            if hdr.cmsg_len < hdr_len {
                return None;
            }
            let data = off + align(hdr_len);
            if hdr.cmsg_level == SOL_SOCKET && hdr.cmsg_type == SO_TIMESTAMP as i32 && data + 8 <= control.len() {
                let mut b = [0u8; 8];
                b.copy_from_slice(&control[data..data + 8]);
                return Some(u64::from_ne_bytes(b));
            }
            off += align(hdr.cmsg_len);
        }
        None
    }

    pub fn recv(port: u16, mmcss: bool, run: Duration) -> i32 {
        let sock = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("recv: bind du port {port} impossible : {e}");
                return 1;
            }
        };
        // Sans délai de lecture, un émetteur absent bloquerait la sonde à jamais.
        let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
        let s = sock.as_raw_socket() as SOCKET;

        if mmcss {
            let name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
            let mut idx = 0u32;
            // SAFETY: chaîne UTF-16 terminée, index sur la pile.
            let h = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut idx) };
            if h.is_null() {
                eprintln!("recv: MMCSS Pro Audio refuse (erreur {})", std::io::Error::last_os_error());
                return 1;
            }
            println!("recv: fil de reception promu MMCSS \"Pro Audio\"");
        } else {
            println!("recv: fil de reception en priorite NORMALE");
        }

        // 1. Demander l'horodatage de réception.
        let cfg = TIMESTAMPING_CONFIG { Flags: TIMESTAMPING_FLAG_RX, TxTimestampsBuffered: 0 };
        let mut ret = 0u32;
        // SAFETY: tampons d'entrée/sortie valides et dimensionnés.
        let rc = unsafe {
            WSAIoctl(
                s,
                SIO_TIMESTAMPING,
                (&cfg as *const TIMESTAMPING_CONFIG).cast(),
                std::mem::size_of::<TIMESTAMPING_CONFIG>() as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc != 0 {
            // SAFETY: lecture de l'erreur du fil courant.
            let err = unsafe { WSAGetLastError() };
            println!("RESULTAT: horodatage noyau INDISPONIBLE (SIO_TIMESTAMPING refuse, erreur Winsock {err})");
            println!("CSV;timestamping;refused;{err}");
            return 3;
        }

        // 2. Obtenir WSARecvMsg (seule voie qui rend les données de contrôle).
        let mut recv_msg: Option<RecvMsgFn> = None;
        let guid = WSAID_WSARECVMSG;
        // SAFETY: GUID en entrée, pointeur de fonction en sortie, tailles exactes.
        let rc = unsafe {
            WSAIoctl(
                s,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                (&guid as *const windows_sys::core::GUID).cast(),
                std::mem::size_of::<windows_sys::core::GUID>() as u32,
                (&mut recv_msg as *mut Option<RecvMsgFn>).cast(),
                std::mem::size_of::<Option<RecvMsgFn>>() as u32,
                &mut ret,
                std::ptr::null_mut(),
                None,
            )
        };
        let Some(recv_msg) = recv_msg.filter(|_| rc == 0) else {
            // SAFETY: lecture de l'erreur du fil courant.
            let err = unsafe { WSAGetLastError() };
            eprintln!("recv: WSARecvMsg introuvable (erreur Winsock {err})");
            return 1;
        };

        let mut freq = 0i64;
        // SAFETY: pointeur valide sur la pile.
        unsafe { QueryPerformanceFrequency(&mut freq) };
        let us_per_tick = 1e6 / freq as f64;
        println!("recv: port {port}, {} s, QPC {freq} Hz. En attente des paquets...", run.as_secs());

        let mut data = [0u8; 2048];
        let mut control = [0u64; 32]; // 256 octets, aligné 8
        let (mut total, mut with_ts, mut absurd) = (0u64, 0u64, 0u64);
        let mut all_delay = Series::default();
        let mut all_kgap = Series::default();
        let mut all_agap = Series::default();
        let mut win_delay = Series::default();
        let mut win_kgap = Series::default();
        let mut win_agap = Series::default();
        let mut prev: Option<(u64, i64)> = None; // (noyau, programme) du paquet précédent
        let mut started: Option<Instant> = None;
        let mut window_start = Instant::now();

        loop {
            if started.is_some_and(|t| t.elapsed() >= run) {
                break;
            }
            let mut buf = WSABUF { len: data.len() as u32, buf: data.as_mut_ptr() };
            let mut msg = WSAMSG {
                name: std::ptr::null_mut(),
                namelen: 0,
                lpBuffers: &mut buf,
                dwBufferCount: 1,
                Control: WSABUF {
                    len: std::mem::size_of_val(&control) as u32,
                    buf: control.as_mut_ptr().cast(),
                },
                dwFlags: 0,
            };
            let mut n = 0u32;
            // SAFETY: WSAMSG et tampons valides pendant l'appel ; réception bloquante.
            let rc = unsafe { recv_msg(s, &mut msg, &mut n, std::ptr::null_mut(), std::ptr::null_mut()) };
            let app = qpc();
            if rc != 0 {
                // SAFETY: lecture de l'erreur du fil courant.
                let err = unsafe { WSAGetLastError() };
                if started.is_none() {
                    // Délai de lecture sans émetteur : on attend encore.
                    continue;
                }
                eprintln!("recv: erreur de reception (Winsock {err}), arret");
                break;
            }
            if n as usize != PACKET_BYTES {
                continue; // pas un paquet de la sonde
            }
            started.get_or_insert_with(Instant::now);
            total += 1;
            let ctl_len = (msg.Control.len as usize).min(std::mem::size_of_val(&control));
            // SAFETY: `control` est un tableau de u64 vu en octets, longueur bornée.
            let ctl = unsafe { std::slice::from_raw_parts(control.as_ptr().cast::<u8>(), ctl_len) };
            if let Some(ts) = find_timestamp(ctl) {
                with_ts += 1;
                let delay = (app as f64 - ts as f64) * us_per_tick;
                // Un horodatage LOGICIEL précède la lecture de quelques µs à
                // quelques ms. Négatif ou > 1 s : ce n'est pas l'horloge QPC.
                if !(0.0..1e6).contains(&delay) {
                    absurd += 1;
                } else {
                    win_delay.push(delay);
                    all_delay.push(delay);
                    if let Some((pk, pa)) = prev {
                        let kgap = (ts as f64 - pk as f64) * us_per_tick;
                        let agap = (app - pa) as f64 * us_per_tick;
                        win_kgap.push(kgap);
                        win_agap.push(agap);
                        all_kgap.push(kgap);
                        all_agap.push(agap);
                    }
                    prev = Some((ts, app));
                }
            }
            if window_start.elapsed() >= WINDOW {
                print_line("5s", &win_delay, &win_kgap, &win_agap);
                win_delay = Series::default();
                win_kgap = Series::default();
                win_agap = Series::default();
                window_start = Instant::now();
            }
        }

        println!();
        println!("recv: {total} paquets, {with_ts} horodates par le noyau, {absurd} horodatages hors horloge QPC");
        if total == 0 {
            println!("RESULTAT: aucun paquet recu (emetteur lance ? pare-feu ?)");
            return 1;
        }
        if with_ts == 0 {
            println!("RESULTAT: horodatage noyau INDISPONIBLE (accepte, mais aucun paquet horodate : carte/pilote)");
            println!("CSV;timestamping;absent;{total}");
            return 3;
        }
        if absurd > with_ts / 2 {
            println!("RESULTAT: horodatages NON comparables (horloge materielle de la carte ?) : ne pas utiliser ces chiffres");
            println!("CSV;timestamping;hardware_clock;{absurd}");
            return 3;
        }
        println!("RESULTAT: horodatage noyau DISPONIBLE");
        print_line("TOTAL", &all_delay, &all_kgap, &all_agap);
        println!(
            "CSV;timestamping;ok;mmcss={};n={};delay_p50_us={:.0};delay_p99_us={:.0};delay_max_us={:.0};kgap_max_us={:.0};agap_max_us={:.0}",
            mmcss,
            all_delay.0.len(),
            all_delay.pct(0.5),
            all_delay.pct(0.99),
            all_delay.max(),
            all_kgap.max(),
            all_agap.max()
        );
        0
    }

    /// Une ligne : attente pile réseau → programme (p50/p99/max), et plus grand
    /// écart entre deux arrivées vu par le NOYAU puis par le PROGRAMME. Un écart
    /// programme ≫ écart noyau = le retard naît dans la machine.
    fn print_line(label: &str, delay: &Series, kgap: &Series, agap: &Series) {
        println!(
            "{label:>5} attente noyau->programme (us) p50 {:>6.0} p99 {:>6.0} max {:>6.0} | ecart max entre arrivees (us) noyau {:>6.0} programme {:>6.0} | n {}",
            delay.pct(0.5),
            delay.pct(0.99),
            delay.max(),
            kgap.max(),
            agap.max(),
            delay.0.len()
        );
    }
}
