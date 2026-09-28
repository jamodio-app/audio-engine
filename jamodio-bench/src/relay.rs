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
//!
//! Il mesure aussi la RÉGULARITÉ de ce qui lui arrive, sens par sens (écarts
//! entre paquets d'un même flux, datés par le système quand il le permet).
//! Banc et agent tournent sur la machine mesurée : tout ce qui arrive au relais
//! a fait le trajet ALLER (machine mesurée → relais). Banc du 28/09 (NUC →
//! Mac en Ethernet) : blocages de 10 à 20 ms sur tous les flux à la fois, sans
//! savoir s'ils naissent à l'aller ou au retour — des écarts déjà présents à
//! l'arrivée au relais les placent à l'aller (envoi de la machine mesurée,
//! câble, réception du relais), leur absence au retour.

use crate::server::CUT_GAP_US;
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
    /// Régularité des arrivées, port par port (un port = un transport du banc).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortWindow>,
}

/// Nature d'un transport du banc, pour lire les mesures du relais : « Down » =
/// flux que le banc envoie à l'agent (un musicien simulé), « Up » = flux que
/// l'agent envoie au banc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    DownInstrument,
    DownVoice,
    UpInstrument,
    UpVoice,
}

/// Arrivées d'un sens sur la fenêtre : paquets, plus grand écart entre deux
/// paquets consécutifs, écarts de plus de `CUT_GAP_US`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapWindow {
    pub packets: u64,
    pub max_gap_us: u64,
    pub gaps_over_10ms: u64,
}

/// Les deux sens d'un port de relais : ce qui arrive du banc (flux « reçus »
/// par l'agent) et ce qui arrive de l'agent (son instrument, son talkback, ses
/// perçages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortWindow {
    /// Transport du banc servi par ce port (tel qu'il l'a demandé).
    pub bench: SocketAddr,
    pub from_bench: GapWindow,
    pub from_agent: GapWindow,
}

/// Écarts entre arrivées successives d'UN sens d'UN port. Fonction pure du
/// temps donné : testée sans réseau.
#[derive(Default)]
struct GapTracker {
    last: Option<Instant>,
}

impl GapTracker {
    fn observe(&mut self, at: Instant, w: &mut GapWindow) {
        w.packets += 1;
        if let Some(prev) = self.last {
            let gap = at.saturating_duration_since(prev).as_micros().min(u64::MAX as u128) as u64;
            w.max_gap_us = w.max_gap_us.max(gap);
            if gap > CUT_GAP_US {
                w.gaps_over_10ms += 1;
            }
        }
        self.last = Some(at);
    }
}

/// Mesures partagées par les ports de relais d'une session de contrôle.
#[derive(Default)]
struct Window {
    delays_us: Vec<u32>,
    ports: std::collections::HashMap<SocketAddr, PortWindow>,
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
    let (mut d, ports) = {
        let mut w = window.lock().unwrap();
        (std::mem::take(&mut w.delays_us), std::mem::take(&mut w.ports))
    };
    d.sort_unstable();
    let pct = |p: f64| d.get(((d.len().max(1) - 1) as f64 * p).round() as usize).copied().unwrap_or(0);
    Reply {
        forwarded: d.len() as u64,
        delay_p99_us: pct(0.99),
        delay_max_us: pct(1.0),
        ports: ports.into_values().collect(),
        ..Reply::default()
    }
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
    let (mut from_bench, mut from_agent) = (GapTracker::default(), GapTracker::default());
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
        // Arrivée datée par le système quand il le permet, sinon à la lecture.
        let arrived_at = r.stack_delay.and_then(|d| read_at.checked_sub(d)).unwrap_or(read_at);
        let is_bench = r.from == bench;
        let to = if is_bench {
            match agent {
                Some(a) => a,
                None => continue, // l'agent n'a pas encore percé
            }
        } else {
            agent = Some(r.from);
            bench
        };
        let sent = socket.send_to(&buf[..r.len], to).is_ok();
        let delay = read_at.elapsed() + r.stack_delay.unwrap_or_default();
        let mut w = window.lock().unwrap();
        let port = w.ports.entry(bench).or_insert(PortWindow {
            bench,
            from_bench: GapWindow::default(),
            from_agent: GapWindow::default(),
        });
        if is_bench {
            from_bench.observe(arrived_at, &mut port.from_bench);
        } else {
            from_agent.observe(arrived_at, &mut port.from_agent);
        }
        if sent {
            w.delays_us.push(delay.as_micros().min(u32::MAX as u128) as u32);
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
        // Chaque sens est compté sur le port du transport du banc.
        assert_eq!(st.ports.len(), 1, "{st:?}");
        let p = st.ports[0];
        assert_eq!(p.bench, bench.local_addr().unwrap());
        assert_eq!((p.from_bench.packets, p.from_agent.packets), (1, 1));

        // Un blocage de l'envoi du banc (≥ 30 ms : un sommeil n'est jamais plus
        // court que demandé) se voit à l'ARRIVÉE au relais.
        bench.send_to(b"a", relay).unwrap();
        agent.recv_from(&mut buf).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        bench.send_to(b"b", relay).unwrap();
        agent.recv_from(&mut buf).unwrap();
        let st = client.stats().await.unwrap();
        let p = st.ports[0];
        assert_eq!(p.from_bench.packets, 2);
        // (≥ 1 : l'écart avec le paquet précédent, avant le relevé, dépend de
        // la vitesse de la machine de test — on ne suppose aucune durée.)
        assert!(p.from_bench.gaps_over_10ms >= 1 && p.from_bench.max_gap_us >= 30_000, "{p:?}");
        assert_eq!(p.from_agent, GapWindow::default(), "rien n'est venu de l'agent : fenêtre vide");
    }

    #[test]
    fn un_ecart_se_mesure_entre_deux_arrivees_du_meme_sens() {
        let t0 = Instant::now();
        let at = |us: u64| t0 + Duration::from_micros(us);
        let (mut g, mut w) = (GapTracker::default(), GapWindow::default());
        // Flux régulier (2,5 ms), puis un blocage de 15 ms, puis régulier.
        for us in [0, 2_500, 5_000, 20_000, 22_500] {
            g.observe(at(us), &mut w);
        }
        assert_eq!(w, GapWindow { packets: 5, max_gap_us: 15_000, gaps_over_10ms: 1 });
        // Exactement 10 ms n'est pas une coupure (même seuil que le banc).
        let (mut g, mut w) = (GapTracker::default(), GapWindow::default());
        g.observe(at(0), &mut w);
        g.observe(at(10_000), &mut w);
        assert_eq!(w.gaps_over_10ms, 0);
    }

    #[test]
    fn une_reponse_d_un_ancien_relais_se_lit_sans_ports() {
        let r: Reply = serde_json::from_str(r#"{"forwarded":3,"delay_p99_us":10,"delay_max_us":12}"#).unwrap();
        assert!(r.ports.is_empty());
    }

    #[test]
    fn une_commande_se_lit_comme_elle_s_ecrit() {
        let c = Command::Open { bench: "192.168.1.82:5000".parse().unwrap() };
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"cmd":"open","bench":"192.168.1.82:5000"}"#);
        assert!(matches!(serde_json::from_str::<Command>(r#"{"cmd":"stats"}"#).unwrap(), Command::Stats));
    }
}
