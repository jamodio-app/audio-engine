//! Le relais « réseau local » (B0.2 du plan du banc).
//!
//! # Pourquoi
//!
//! En mode local, les paquets du faux serveur ne quittent pas la machine : ils
//! ne traversent ni la carte réseau ni son pilote — là où se logent les blocages
//! de pilotes Windows (Intel MEI sur le cœur 0, trouvé le 21/09/2026). Le NUC a
//! tenu 9 musiciens sans un trou en local (28/09) ; restait à savoir si le vrai
//! chemin réseau change la donne. Le WebSocket de l'Audio Engine n'écoute que
//! la machine locale : le banc reste donc sur la machine mesurée, et un RELAIS
//! sur une seconde machine fait passer chaque flux par le réseau.
//!
//! # Comment
//!
//! `session-bench relay` (seconde machine) écoute un port de CONTRÔLE (TCP). Le
//! banc y demande, pour chacun de ses transports, un port UDP de relais : il
//! donne ce port à l'agent à la place du sien. Chaque paquet reçu sur le port
//! de relais est renvoyé :
//! - venant du banc → vers l'agent (appris à son premier paquet : le perçage) ;
//! - venant de l'agent → vers le banc.
//!
//! Aucun contenu n'est lu ni modifié (le chiffrement reste de bout en bout
//! entre le banc et l'agent). Le relais MESURE son propre délai (heure de
//! réception par le système → réexpédition) : s'il ajoute de la gigue, le
//! résumé du banc le dit au lieu de l'imputer au réseau ou à l'agent.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Port de contrôle par défaut.
pub const DEFAULT_PORT: u16 = 51_900;

/// Une commande du banc au relais (une ligne JSON).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    /// Ouvrir un port de relais pour ce transport du banc.
    Open { bench: SocketAddr },
    /// Mesures du relais depuis la dernière demande.
    Stats,
}

/// Réponse du relais (une ligne JSON).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Paquets relayés sur la fenêtre.
    #[serde(default)]
    pub forwarded: u64,
    /// Délai du relais (réception par le système → réexpédition), p99 et max,
    /// en µs. Sans horodatage du système : lecture → réexpédition.
    #[serde(default)]
    pub delay_p99_us: u32,
    #[serde(default)]
    pub delay_max_us: u32,
}

/// Mesures partagées par les ports de relais d'une session de contrôle.
#[derive(Default)]
struct Window {
    delays_us: Vec<u32>,
}

/// Lance le relais : écoute le contrôle sur `listen:port`, une session de banc
/// à la fois (la suivante attend la fin de la précédente).
pub fn serve(listen: IpAddr, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind((listen, port)).map_err(|e| format!("contrôle {listen}:{port} : {e}"))?;
    println!("Relais prêt : contrôle sur {listen}:{port}. Côté banc : --relay {listen}:{port}");
    println!("(Ctrl-C pour arrêter.)");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                println!("Banc connecté depuis {peer}");
                if let Err(e) = session(stream, listen) {
                    eprintln!("session terminée : {e}");
                }
                println!("Banc déconnecté — ports de relais fermés.");
            }
            Err(e) => eprintln!("connexion refusée : {e}"),
        }
    }
    Ok(())
}

/// Une session de contrôle : ouvre des ports de relais à la demande, rend les
/// mesures, ferme tout quand le banc se déconnecte.
fn session(stream: TcpStream, listen: IpAddr) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let window = Arc::new(Mutex::new(Window::default()));
    let mut threads = Vec::new();
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let reader = BufReader::new(stream);
    let result = (|| -> Result<(), String> {
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            let reply = match serde_json::from_str::<Command>(&line) {
                Ok(Command::Open { bench }) => match open_pair(listen, bench, stop.clone(), window.clone()) {
                    Ok((port, t)) => {
                        threads.push(t);
                        Reply { port: Some(port), ..Reply::default() }
                    }
                    Err(e) => Reply { error: Some(e), ..Reply::default() },
                },
                Ok(Command::Stats) => take_stats(&window),
                Err(e) => Reply { error: Some(format!("commande illisible : {e}")), ..Reply::default() },
            };
            let json = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
            writeln!(writer, "{json}").map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    result
}

fn take_stats(window: &Mutex<Window>) -> Reply {
    let mut d = std::mem::take(&mut window.lock().unwrap().delays_us);
    d.sort_unstable();
    let pct = |p: f64| d.get(((d.len().max(1) - 1) as f64 * p).round() as usize).copied().unwrap_or(0);
    Reply { forwarded: d.len() as u64, delay_p99_us: pct(0.99), delay_max_us: pct(1.0), ..Reply::default() }
}

/// Ouvre un port de relais pour le transport `bench` et lance son fil.
fn open_pair(
    listen: IpAddr,
    bench: SocketAddr,
    stop: Arc<AtomicBool>,
    window: Arc<Mutex<Window>>,
) -> Result<(u16, std::thread::JoinHandle<()>), String> {
    let socket = UdpSocket::bind((listen, 0)).map_err(|e| format!("port de relais : {e}"))?;
    socket.set_read_timeout(Some(Duration::from_millis(200))).map_err(|e| e.to_string())?;
    let port = socket.local_addr().map_err(|e| e.to_string())?.port();
    let t = std::thread::Builder::new()
        .name(format!("relay-{port}"))
        .spawn(move || forward(socket, bench, stop, window))
        .map_err(|e| e.to_string())?;
    Ok((port, t))
}

/// Relaie les paquets d'un port jusqu'à `stop`.
fn forward(socket: UdpSocket, bench: SocketAddr, stop: Arc<AtomicBool>, window: Arc<Mutex<Window>>) {
    // Le relais doit ajouter moins de gigue que ce qu'on mesure : même priorité
    // que les fils du banc (temps réel macOS, MMCSS Windows).
    let _ = crate::rt::promote_current_thread();
    let stamped = jamodio_audio_core::net::rx_timestamp::enable(raw(&socket)).is_ok();
    let mut agent: Option<SocketAddr> = None;
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        let r = if stamped {
            jamodio_audio_core::net::rx_timestamp::recv(raw(&socket), &mut buf)
        } else {
            socket.recv_from(&mut buf).map(|(len, from)| jamodio_audio_core::net::rx_timestamp::Received {
                len,
                from,
                stack_delay: None,
            })
        };
        let read_at = Instant::now();
        let r = match r {
            Ok(r) => r,
            // Délai de lecture écoulé sans paquet : on revérifie `stop`.
            Err(_) => continue,
        };
        let to = if r.from == bench {
            match agent {
                Some(a) => a,
                None => continue, // l'agent n'a pas encore percé
            }
        } else {
            agent = Some(r.from);
            bench
        };
        if socket.send_to(&buf[..r.len], to).is_ok() {
            let delay = read_at.elapsed() + r.stack_delay.unwrap_or_default();
            window.lock().unwrap().delays_us.push(delay.as_micros().min(u32::MAX as u128) as u32);
        }
    }
}

#[cfg(unix)]
fn raw(s: &UdpSocket) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;
    s.as_raw_fd()
}

#[cfg(windows)]
fn raw(s: &UdpSocket) -> usize {
    use std::os::windows::io::AsRawSocket;
    s.as_raw_socket() as usize
}

/// Le côté banc : la connexion de contrôle au relais.
pub struct RelayClient {
    reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    /// Adresse du relais telle que l'agent doit la joindre.
    pub ip: IpAddr,
}

impl RelayClient {
    pub async fn connect(addr: &str) -> Result<Self, String> {
        let sock: SocketAddr = addr.parse().map_err(|e| format!("--relay {addr} : {e} (attendu IP:PORT)"))?;
        let stream = tokio::net::TcpStream::connect(sock)
            .await
            .map_err(|e| format!("relais injoignable sur {sock} : {e} (lancé ? pare-feu ?)"))?;
        let (r, w) = stream.into_split();
        Ok(Self { reader: tokio::io::BufReader::new(r), writer: w, ip: sock.ip() })
    }

    async fn call(&mut self, cmd: &Command) -> Result<Reply, String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let line = serde_json::to_string(cmd).map_err(|e| e.to_string())? + "\n";
        self.writer.write_all(line.as_bytes()).await.map_err(|e| format!("relais : {e}"))?;
        let mut reply = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut reply))
            .await
            .map_err(|_| "relais : pas de réponse".to_string())?
            .map_err(|e| format!("relais : {e}"))?;
        if n == 0 {
            return Err("relais : connexion fermée".into());
        }
        let r: Reply = serde_json::from_str(reply.trim()).map_err(|e| format!("relais : réponse illisible ({e})"))?;
        match r.error {
            Some(e) => Err(format!("relais : {e}")),
            None => Ok(r),
        }
    }

    /// Port de relais pour le transport du banc `bench`.
    pub async fn open(&mut self, bench: SocketAddr) -> Result<u16, String> {
        self.call(&Command::Open { bench })
            .await?
            .port
            .ok_or_else(|| "relais : aucun port rendu".into())
    }

    pub async fn stats(&mut self) -> Result<Reply, String> {
        self.call(&Command::Stats).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// De bout en bout sur la machine : un « banc » et un « agent » échangent
    /// par le relais, dans les deux sens, et le relais compte ce qu'il relaie.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_relais_fait_passer_les_paquets_dans_les_deux_sens() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ctl = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let _ = session(s, "127.0.0.1".parse().unwrap());
        });
        let mut client = RelayClient::connect(&ctl.to_string()).await.unwrap();

        let bench = UdpSocket::bind("127.0.0.1:0").unwrap();
        bench.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let agent = UdpSocket::bind("127.0.0.1:0").unwrap();
        agent.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = client.open(bench.local_addr().unwrap()).await.unwrap();
        let relay: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let mut buf = [0u8; 64];
        // L'agent perce : le banc le reçoit, venant du relais.
        agent.send_to(b"perce", relay).unwrap();
        let (n, from) = bench.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&b"perce"[..], relay));
        // Le banc répond au relais : l'agent le reçoit.
        bench.send_to(b"son", relay).unwrap();
        let (n, from) = agent.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&b"son"[..], relay));

        let st = client.stats().await.unwrap();
        assert_eq!(st.forwarded, 2);
        assert!(st.delay_max_us < 100_000, "{st:?}");
    }

    #[test]
    fn une_commande_se_lit_comme_elle_s_ecrit() {
        let c = Command::Open { bench: "192.168.1.82:5000".parse().unwrap() };
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"cmd":"open","bench":"192.168.1.82:5000"}"#);
        assert!(matches!(serde_json::from_str::<Command>(r#"{"cmd":"stats"}"#).unwrap(), Command::Stats));
    }
}
