//! L'émetteur distant (lot R1-bis du banc réaliste).
//!
//! # Pourquoi
//!
//! Le banc doit être plus précis que ce qu'il mesure. Sur le NUC (01/10/2026),
//! il envoyait jusqu'à 2 ms en retard, 17 à 186 secondes sur 330, selon ce qui
//! tournait à côté (programmes de fond, pilote de sortie) — ancien banc compris :
//! la précision dépendait de l'état de la MACHINE MESURÉE. Le Mac, lui, tient
//! 0,15 ms. Et en mode relais, la machine mesurée portait le faux serveur ET
//! l'Audio Engine : environ deux fois les paquets d'un vrai musicien.
//!
//! # Comment
//!
//! `session-bench remote` tourne sur une SECONDE machine (en Ethernet). Il y
//! fabrique et envoie les flux des musiciens simulés (mêmes calendriers, mêmes
//! graines qu'en local : mêmes paquets), et y reçoit ce que l'Audio Engine
//! envoie. La machine mesurée ne garde que le pilotage (le WebSocket de l'agent
//! n'écoute qu'elle) et ne traite plus que les paquets d'un vrai musicien. Le
//! vrai réseau est dans la boucle, dans les deux sens, comme en session.
//!
//! Le pilote parle à l'émetteur par un canal de commande TCP (une ligne JSON par
//! commande et par réponse), comme au relais. Les clés SRTP y passent : clés de
//! test, tirées au hasard à chaque campagne, sur le réseau local.
//!
//! **Un fil d'envoi par musicien.** Avec un seul fil pour tous, la salve qu'un
//! lien relâche d'un coup (pic, gigue à queue lourde) retardait les paquets des
//! AUTRES musiciens le temps de l'envoyer — jusqu'à 2,6 ms, NUC du 01/10/2026,
//! dès le passage d'un seul musicien en Wi-Fi chargé. En vrai, leurs réseaux
//! sont indépendants : le banc ne doit pas les lier. L'émetteur est une machine
//! dédiée, il a les cœurs pour. (Le mode local garde un seul fil : sur la
//! machine mesurée, un fil par musicien la chargerait.)

use crate::profile::PeerProfile;
use crate::server::{Downlink, Kind, Senders, SenderWindow, Uplink, UplinkWindow};
use jamodio_audio_core::net::srtp::SrtpParameters;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Port de contrôle par défaut (le relais a 51 900).
pub const DEFAULT_PORT: u16 = 51_901;

/// Une commande du pilote à l'émetteur (une ligne JSON).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Command {
    /// Qui es-tu ? (nom de la machine, pour l'en-tête du résumé)
    Hello,
    /// Ouvrir un transport de réception de l'agent (instrument ou talkback).
    OpenUp { voice: bool },
    /// Clés de l'agent pour ce transport : la réception commence.
    UpKeys { id: u32, keys: SrtpParameters },
    /// Ouvrir le transport d'un musicien simulé.
    OpenDown { producer_id: String, voice: bool, seed: u64 },
    /// Clés de l'agent reçues : le flux part, selon ce profil.
    StartDown { id: u32, keys: SrtpParameters, profile: Box<PeerProfile>, seed: u64, offset_us: u64 },
    /// Le musicien part : plus aucun paquet de ce flux.
    Retire { id: u32 },
    /// Mesures depuis la dernière demande.
    Stats,
}

/// Réponse de l'émetteur (une ligne JSON). Seuls les champs utiles à la
/// commande sont remplis.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Version du banc de l'émetteur (commit compilé) : un émetteur resté sur
    /// un ancien binaire se voit dans l'en-tête du résumé (01/10/2026 : la
    /// précision « insuffisante » venait d'un émetteur non mis à jour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<SrtpParameters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<SenderWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_instrument: Option<UplinkWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_voice: Option<UplinkWindow>,
    /// Priorité obtenue par le fil d'envoi de l'émetteur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

/// Lance l'émetteur : écoute le contrôle sur `listen:port`, une campagne à la
/// fois (la suivante attend la fin de la précédente).
pub fn serve(listen: IpAddr, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind((listen, port)).map_err(|e| format!("contrôle {listen}:{port} : {e}"))?;
    println!("Émetteur distant prêt : contrôle sur {listen}:{port}.");
    println!("Côté machine mesurée : session-bench run … --remote {listen}:{port}");
    println!("(Ctrl-C pour arrêter.)");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                println!("Campagne connectée depuis {peer}");
                if let Err(e) = session(stream, listen) {
                    eprintln!("campagne terminée : {e}");
                }
                println!("Campagne terminée — flux arrêtés.");
            }
            Err(e) => eprintln!("connexion refusée : {e}"),
        }
    }
    Ok(())
}

/// Ce que l'émetteur tient pour une campagne.
struct Session {
    listen: String,
    /// Un fil d'envoi par flux démarré.
    senders: Senders,
    downs: HashMap<u32, Arc<Downlink>>,
    ups: HashMap<u32, (Arc<Uplink>, bool)>,
    stop: Arc<AtomicBool>,
    listeners: Vec<std::thread::JoinHandle<()>>,
    next_id: u32,
}

impl Session {
    fn new(listen: IpAddr) -> Result<Self, String> {
        Ok(Self {
            listen: listen.to_string(),
            senders: Senders::new(true)?,
            downs: HashMap::new(),
            ups: HashMap::new(),
            stop: Arc::new(AtomicBool::new(false)),
            listeners: Vec::new(),
            next_id: 1,
        })
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id - 1
    }

    fn handle(&mut self, cmd: Command) -> Result<Reply, String> {
        Ok(match cmd {
            Command::Hello => Reply {
                host: Some(crate::run::machine_name()),
                bench: Some(crate::campaign::bench_commit()),
                ..Reply::default()
            },
            Command::OpenUp { voice } => {
                let up = Uplink::bind(&self.listen)?;
                let id = self.id();
                let reply = Reply { id: Some(id), port: Some(up.port()), keys: Some(up.server_keys.clone()), ..Reply::default() };
                self.ups.insert(id, (up, voice));
                reply
            }
            Command::UpKeys { id, keys } => {
                let (up, _) = self.ups.get(&id).ok_or(format!("transport de réception {id} inconnu"))?;
                up.set_agent_keys(&keys)?;
                let (up, stop) = (up.clone(), self.stop.clone());
                self.listeners.push(std::thread::spawn(move || up.listen(stop)));
                Reply::default()
            }
            Command::OpenDown { producer_id, voice, seed } => {
                let kind = if voice { Kind::Voice } else { Kind::Instrument };
                let link = Downlink::bind(&self.listen, producer_id, kind, seed)?;
                let id = self.id();
                let reply = Reply { id: Some(id), port: Some(link.port()), keys: Some(link.server_keys.clone()), ..Reply::default() };
                self.downs.insert(id, link);
                reply
            }
            Command::StartDown { id, keys, profile, seed, offset_us } => {
                profile.validate()?;
                let link = self.downs.get(&id).ok_or(format!("flux {id} inconnu"))?.clone();
                link.set_agent_keys(&keys)?;
                self.senders.add(id, link, &profile, seed, offset_us);
                Reply::default()
            }
            Command::Retire { id } => {
                self.downs.remove(&id).ok_or(format!("flux {id} inconnu"))?.retire();
                self.senders.retire(id);
                Reply::default()
            }
            Command::Stats => {
                let take = |voice: bool| self.ups.values().find(|(_, v)| *v == voice).map(|(u, _)| u.take_window());
                let priority = Some(self.senders.priority());
                Reply {
                    sender: Some(self.senders.take_window()),
                    up_instrument: take(false),
                    up_voice: take(true),
                    priority,
                    ..Reply::default()
                }
            }
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for l in self.listeners.drain(..) {
            let _ = l.join();
        }
        // Chaque fil d'envoi s'arrête à son tour (`Drop` de `SenderLoop`).
    }
}

fn session(stream: TcpStream, listen: IpAddr) -> Result<(), String> {
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let reader = BufReader::new(stream);
    let mut session = Session::new(listen)?;
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let reply = match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => session.handle(cmd).unwrap_or_else(|e| Reply { error: Some(e), ..Reply::default() }),
            Err(e) => Reply { error: Some(format!("commande illisible : {e}")), ..Reply::default() },
        };
        let json = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
        writeln!(writer, "{json}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Côté machine mesurée : le canal de commande vers l'émetteur.
pub struct RemoteClient {
    reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    /// Adresse de l'émetteur : celle que l'agent doit joindre.
    pub ip: IpAddr,
    pub addr: SocketAddr,
}

impl RemoteClient {
    pub async fn connect(addr: &str) -> Result<Self, String> {
        let sock: SocketAddr = addr.parse().map_err(|e| format!("--remote {addr} : {e} (attendu IP:PORT)"))?;
        let stream = tokio::net::TcpStream::connect(sock)
            .await
            .map_err(|e| format!("émetteur distant injoignable sur {sock} : {e} (lancé ? pare-feu ?)"))?;
        // Une commande par seconde au moins : pas d'attente de regroupement.
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        let (r, w) = stream.into_split();
        Ok(Self { reader: tokio::io::BufReader::new(r), writer: w, ip: sock.ip(), addr: sock })
    }

    pub async fn call(&mut self, cmd: &Command) -> Result<Reply, String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let line = serde_json::to_string(cmd).map_err(|e| e.to_string())? + "\n";
        self.writer.write_all(line.as_bytes()).await.map_err(|e| format!("émetteur distant : {e}"))?;
        let mut reply = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut reply))
            .await
            .map_err(|_| "émetteur distant : pas de réponse".to_string())?
            .map_err(|e| format!("émetteur distant : {e}"))?;
        if n == 0 {
            return Err("émetteur distant : connexion fermée".into());
        }
        let r: Reply = serde_json::from_str(reply.trim()).map_err(|e| format!("émetteur distant : réponse illisible ({e})"))?;
        match r.error {
            Some(e) => Err(format!("émetteur distant : {e}")),
            None => Ok(r),
        }
    }

    /// Ouvre un transport (réception ou musicien) : (identifiant, port, clés de l'émetteur).
    pub async fn open(&mut self, cmd: &Command) -> Result<(u32, u16, SrtpParameters), String> {
        let r = self.call(cmd).await?;
        match (r.id, r.port, r.keys) {
            (Some(id), Some(port), Some(keys)) => Ok((id, port, keys)),
            _ => Err("émetteur distant : transport ouvert sans identifiant, port ou clés".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::net::rtp::{self, RtpHeader};
    use jamodio_audio_core::net::srtp::SrtpContext;
    use jamodio_audio_core::net::udp::{RtpReceiver, RtpSender};
    use std::time::Instant;

    /// Un émetteur sur cette machine, dans un fil : son adresse de contrôle.
    fn spawn_remote() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let _ = session(conn, "127.0.0.1".parse().unwrap());
            }
        });
        addr
    }

    /// Un fil d'envoi par musicien : deux flux, deux fils ; un retrait arrête le
    /// sien, et les mesures des deux sont rendues ensemble (rien de perdu au
    /// retrait).
    #[test]
    fn chaque_musicien_a_son_propre_fil_d_envoi() {
        let mut s = Session::new("127.0.0.1".parse().unwrap()).unwrap();
        let mut ids = Vec::new();
        for m in 2..=3u64 {
            let r = s.handle(Command::OpenDown { producer_id: format!("bench-m{m}"), voice: false, seed: m }).unwrap();
            let (id, port) = (r.id.unwrap(), r.port.unwrap());
            // Le perçage de l'agent : un en-tête RTP suffit pour apprendre l'adresse.
            let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut punch = [0u8; 12];
            punch[0] = 0x80;
            punch[1] = crate::server::PAYLOAD_TYPE;
            rx.send_to(&punch, ("127.0.0.1", port)).unwrap();
            let profile = Box::new(PeerProfile::preset("regular").unwrap());
            let keys = SrtpParameters::generate_aead_aes_256_gcm();
            s.handle(Command::StartDown { id, keys, profile, seed: m, offset_us: 0 }).unwrap();
            ids.push((id, rx));
        }
        assert_eq!(s.senders.threads(), 2, "un fil par flux");
        std::thread::sleep(Duration::from_millis(400));
        s.handle(Command::Retire { id: ids[0].0 }).unwrap();
        assert_eq!(s.senders.threads(), 1, "le fil du musicien parti est arrêté");
        let sent = s.handle(Command::Stats).unwrap().sender.unwrap().sent;
        // ~160 trames par flux en 400 ms : celles du flux retiré sont comptées.
        assert!(sent > 200, "{sent}");
        assert!(s.handle(Command::Stats).unwrap().priority.is_some());
    }

    #[test]
    fn les_commandes_font_l_aller_retour_et_une_faute_est_refusee() {
        let cmd = Command::StartDown {
            id: 3,
            keys: SrtpParameters::generate_aead_aes_256_gcm(),
            profile: Box::new(PeerProfile { drift_ppm: -40.0, ..PeerProfile::preset("4g").unwrap() }),
            seed: 9,
            offset_us: 12_000_000,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        match serde_json::from_str::<Command>(&json).unwrap() {
            Command::StartDown { profile, offset_us, .. } => {
                assert_eq!((profile.name.as_str(), profile.drift_ppm, offset_us), ("4g", -40.0, 12_000_000));
            }
            other => panic!("{other:?}"),
        }
        assert!(serde_json::from_str::<Command>(r#"{"cmd":"retire","idd":3}"#).is_err());
    }

    /// Échange réel : le RÉCEPTEUR DE L'AGENT reçoit et déchiffre un flux
    /// fabriqué par l'émetteur distant (ici sur la même machine), à la cadence
    /// et dans l'ordre du calendrier ; retiré, le flux s'arrête.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_recepteur_de_l_agent_recoit_les_flux_de_l_emetteur_distant() {
        let mut remote = RemoteClient::connect(&spawn_remote().to_string()).await.unwrap();
        let hello = remote.call(&Command::Hello).await.unwrap();
        assert!(!hello.host.unwrap().is_empty());
        assert_eq!(hello.bench.as_deref(), Some(crate::campaign::bench_commit().as_str()));
        let (id, port, server_keys) = remote
            .open(&Command::OpenDown { producer_id: "bench-m2".into(), voice: false, seed: 2 })
            .await
            .unwrap();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        let receiver = RtpReceiver::new(Arc::new(SrtpContext::new(&agent_keys, &server_keys).unwrap())).unwrap();
        let profile = PeerProfile::preset("regular").unwrap();
        remote
            .call(&Command::StartDown { id, keys: agent_keys, profile: Box::new(profile), seed: 2, offset_us: 0 })
            .await
            .unwrap();
        receiver.punch(format!("127.0.0.1:{port}").parse().unwrap()).unwrap();

        let read = |n: usize, max: Duration| {
            let mut buf = Vec::with_capacity(2048);
            let mut seqs = Vec::new();
            let t = Instant::now();
            while seqs.len() < n && t.elapsed() < max {
                match receiver.read(&mut buf) {
                    Ok(_) => seqs.push(rtp::parse_header(&buf).unwrap().0.sequence),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_micros(200)),
                    Err(e) => panic!("{e}"),
                }
            }
            (seqs, t.elapsed())
        };
        let (seqs, took) = tokio::task::block_in_place(|| read(200, Duration::from_secs(5)));
        assert_eq!(seqs.len(), 200, "paquets déchiffrés par l'agent");
        assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)), "numérotation continue");
        assert!(took >= Duration::from_millis(450), "cadence tenue, pas de rafale");
        let stats = remote.call(&Command::Stats).await.unwrap();
        let sent = stats.sender.unwrap();
        assert!(sent.sent >= 200 && sent.errors == 0, "{sent:?}");
        assert!(stats.priority.is_some());

        remote.call(&Command::Retire { id }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::task::block_in_place(|| read(usize::MAX, Duration::from_millis(30))); // ce qui était en route
        let (after, _) = tokio::task::block_in_place(|| read(usize::MAX, Duration::from_millis(200)));
        assert!(after.is_empty(), "plus rien après le retrait : {}", after.len());
        assert!(remote.call(&Command::Retire { id }).await.unwrap_err().contains("inconnu"));
    }

    /// Échange réel : ce que l'EXPÉDITEUR de l'agent envoie est reçu et mesuré
    /// par l'émetteur distant, et rendu au pilote dans les mesures.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_flux_montant_de_l_agent_est_mesure_par_l_emetteur_distant() {
        let mut remote = RemoteClient::connect(&spawn_remote().to_string()).await.unwrap();
        let (id, port, server_keys) = remote.open(&Command::OpenUp { voice: false }).await.unwrap();
        let agent_keys = SrtpParameters::generate_aead_aes_256_gcm();
        remote.call(&Command::UpKeys { id, keys: agent_keys.clone() }).await.unwrap();
        let ctx = Arc::new(SrtpContext::new(&agent_keys, &server_keys).unwrap());
        let sender = RtpSender::new(format!("127.0.0.1:{port}").parse().unwrap(), ctx).await.unwrap();
        for i in 0u16..40 {
            let h = RtpHeader { payload_type: crate::server::PAYLOAD_TYPE, sequence: i, timestamp: 0, ssrc: 7, marker: false };
            loop {
                match sender.send_blocking(rtp::build_packet(&h, &[0u8; 100])) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => tokio::time::sleep(Duration::from_micros(200)).await,
                    Err(e) => panic!("{e}"),
                }
            }
            tokio::time::sleep(Duration::from_micros(2_500)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stats = remote.call(&Command::Stats).await.unwrap();
        let up = stats.up_instrument.unwrap();
        assert!(up.packets >= 36 && up.undecryptable == 0, "{up:?}");
        assert!(stats.up_voice.is_none(), "aucun talkback ouvert");
    }
}
