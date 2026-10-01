//! Où vivent les flux du banc : sur la machine mesurée (mode local, ou à
//! travers le relais), ou sur l'émetteur distant (lot R1-bis). Le déroulé d'une
//! campagne parle aux deux de la même façon ; seuls les paquets changent de
//! machine.

use crate::profile::PeerProfile;
use crate::relay::{RelayClient, Transport};
use crate::remote::{Command, RemoteClient};
use crate::report::{self, RelayWindow};
use crate::server::{Downlink, Kind, Payloads, SenderLoop, SenderWindow, Uplink, UplinkWindow};
use jamodio_audio_core::net::srtp::SrtpParameters;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Un transport ouvert : son identifiant, le port que l'agent doit joindre, et
/// les clés du banc à lui donner.
pub struct Opened {
    pub id: u32,
    pub port: u16,
    pub keys: SrtpParameters,
}

/// Les mesures d'une seconde.
pub struct Windows {
    pub sender: SenderWindow,
    pub up_instrument: UplinkWindow,
    pub up_voice: Option<UplinkWindow>,
    pub relay: Option<RelayWindow>,
}

pub enum Endpoint {
    Local(Box<Local>),
    Remote(Box<Remote>),
}

/// Flux fabriqués sur la machine mesurée.
pub struct Local {
    /// Adresse du banc sur cette machine.
    ip: String,
    sender: SenderLoop,
    relay: Option<RelayClient>,
    /// Mode relais : nature de chaque transport, pour lire ce qu'il mesure.
    roles: HashMap<SocketAddr, Transport>,
    ups: HashMap<u32, (Arc<Uplink>, bool)>,
    downs: HashMap<u32, Arc<Downlink>>,
    stop: Arc<AtomicBool>,
    listeners: Vec<std::thread::JoinHandle<()>>,
    next_id: u32,
}

/// Flux fabriqués sur l'émetteur distant.
pub struct Remote {
    client: RemoteClient,
    pub host: String,
    priority: String,
}

impl Endpoint {
    /// Mode local (`relay` : `None`) ou à travers le relais.
    pub async fn local(ip: String, relay: Option<&str>) -> Result<Self, String> {
        let relay = match relay {
            Some(addr) => {
                let r = RelayClient::connect(addr).await?;
                println!("Relais {addr} : les flux passent par le réseau.");
                Some(r)
            }
            None => None,
        };
        Ok(Self::Local(Box::new(Local {
            ip,
            sender: SenderLoop::start(Payloads::encode(220.0)?, Payloads::encode(330.0)?),
            relay,
            roles: HashMap::new(),
            ups: HashMap::new(),
            downs: HashMap::new(),
            stop: Arc::new(AtomicBool::new(false)),
            listeners: Vec::new(),
            next_id: 1,
        })))
    }

    pub async fn remote(addr: &str) -> Result<Self, String> {
        let mut client = RemoteClient::connect(addr).await?;
        let host = client.call(&Command::Hello).await?.host.unwrap_or_else(|| "?".into());
        println!("Émetteur distant {host} ({addr}) : les flux simulés partent de là-bas.");
        Ok(Self::Remote(Box::new(Remote { client, host, priority: "inconnue".into() })))
    }

    /// Adresse que l'agent doit joindre : relais, émetteur distant, ou banc.
    pub fn agent_ip(&self) -> String {
        match self {
            Self::Local(l) => l.relay.as_ref().map_or(l.ip.clone(), |r| r.ip.to_string()),
            Self::Remote(r) => r.client.ip.to_string(),
        }
    }

    /// Ouvre un transport de réception de ce que l'agent envoie.
    pub async fn open_uplink(&mut self, voice: bool) -> Result<Opened, String> {
        match self {
            Self::Local(l) => {
                let up = Uplink::bind(&l.ip)?;
                let role = if voice { Transport::UpVoice } else { Transport::UpInstrument };
                let port = l.agent_port(role, up.port()).await?;
                let id = l.id();
                let keys = up.server_keys.clone();
                l.ups.insert(id, (up, voice));
                Ok(Opened { id, port, keys })
            }
            Self::Remote(r) => {
                let (id, port, keys) = r.client.open(&Command::OpenUp { voice }).await?;
                Ok(Opened { id, port, keys })
            }
        }
    }

    /// Clés de l'agent pour ce transport de réception : la mesure commence.
    pub async fn uplink_keys(&mut self, id: u32, keys: &SrtpParameters) -> Result<(), String> {
        match self {
            Self::Local(l) => {
                let (up, _) = l.ups.get(&id).ok_or(format!("transport de réception {id} inconnu"))?;
                up.set_agent_keys(keys)?;
                let (up, stop) = (up.clone(), l.stop.clone());
                l.listeners.push(std::thread::spawn(move || up.listen(stop)));
                Ok(())
            }
            Self::Remote(r) => r.client.call(&Command::UpKeys { id, keys: keys.clone() }).await.map(|_| ()),
        }
    }

    /// Ouvre le transport d'un musicien simulé (`seed` : identité du flux).
    pub async fn open_downlink(&mut self, producer_id: &str, voice: bool, seed: u64) -> Result<Opened, String> {
        match self {
            Self::Local(l) => {
                let kind = if voice { Kind::Voice } else { Kind::Instrument };
                let link = Downlink::bind(&l.ip, producer_id.to_string(), kind, seed)?;
                let role = if voice { Transport::DownVoice } else { Transport::DownInstrument };
                let port = l.agent_port(role, link.port()).await?;
                let id = l.id();
                let keys = link.server_keys.clone();
                l.downs.insert(id, link);
                Ok(Opened { id, port, keys })
            }
            Self::Remote(r) => {
                let cmd = Command::OpenDown { producer_id: producer_id.to_string(), voice, seed };
                let (id, port, keys) = r.client.open(&cmd).await?;
                Ok(Opened { id, port, keys })
            }
        }
    }

    /// Clés de l'agent reçues : le flux part selon `profile` (`seed` : son
    /// calendrier ; `offset_us` : sa place dans la frise du musicien).
    pub async fn start_downlink(
        &mut self,
        id: u32,
        keys: &SrtpParameters,
        profile: &PeerProfile,
        seed: u64,
        offset_us: u64,
    ) -> Result<(), String> {
        match self {
            Self::Local(l) => {
                let link = l.downs.get(&id).ok_or(format!("flux {id} inconnu"))?.clone();
                link.set_agent_keys(keys)?;
                l.sender.add(link, profile, seed, offset_us);
                Ok(())
            }
            Self::Remote(r) => {
                let cmd = Command::StartDown { id, keys: keys.clone(), profile: Box::new(profile.clone()), seed, offset_us };
                r.client.call(&cmd).await.map(|_| ())
            }
        }
    }

    /// Le musicien part : plus aucun paquet de ce flux.
    pub async fn retire(&mut self, id: u32) -> Result<(), String> {
        match self {
            Self::Local(l) => {
                l.downs.remove(&id).ok_or(format!("flux {id} inconnu"))?.retire();
                Ok(())
            }
            Self::Remote(r) => r.client.call(&Command::Retire { id }).await.map(|_| ()),
        }
    }

    /// Les mesures de la seconde écoulée.
    pub async fn windows(&mut self) -> Result<Windows, String> {
        match self {
            Self::Local(l) => {
                let Local { relay, roles, ups, sender, .. } = &mut **l;
                let relay = match relay.as_mut() {
                    Some(r) => {
                        let st = r.stats().await?;
                        Some(RelayWindow {
                            delay_p99_ms: f64::from(st.delay_p99_us) / 1000.0,
                            delay_max_ms: f64::from(st.delay_max_us) / 1000.0,
                            arrivals: report::relay_arrivals(&st.ports, roles),
                        })
                    }
                    None => None,
                };
                let take = |voice: bool| ups.values().find(|(_, v)| *v == voice).map(|(u, _)| u.take_window());
                Ok(Windows {
                    sender: sender.take_window(),
                    up_instrument: take(false).unwrap_or_default(),
                    up_voice: take(true),
                    relay,
                })
            }
            Self::Remote(r) => {
                let st = r.client.call(&Command::Stats).await?;
                if let Some(p) = st.priority {
                    r.priority = p;
                }
                Ok(Windows {
                    sender: st.sender.unwrap_or_default(),
                    up_instrument: st.up_instrument.unwrap_or_default(),
                    up_voice: st.up_voice,
                    relay: None,
                })
            }
        }
    }

    /// Priorité obtenue par le fil d'envoi (là où il tourne).
    pub fn priority(&self) -> String {
        match self {
            Self::Local(l) => crate::run::priority_label(&l.sender),
            Self::Remote(r) => format!("{} (émetteur distant {})", r.priority, r.host),
        }
    }

    /// Arrête la réception (fils du banc rendus). L'émetteur distant arrête ses
    /// flux quand le canal de commande se ferme, c'est-à-dire ici.
    pub fn shutdown(self) {
        if let Self::Local(mut l) = self {
            l.stop.store(true, Ordering::Relaxed);
            for t in l.listeners.drain(..) {
                let _ = t.join();
            }
        }
    }
}

impl Local {
    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id - 1
    }

    /// Le port que l'agent doit joindre pour ce transport du banc : celui du
    /// relais en mode réseau, le sien sinon.
    async fn agent_port(&mut self, role: Transport, bench_port: u16) -> Result<u16, String> {
        let Local { relay, roles, ip, .. } = self;
        match relay.as_mut() {
            Some(r) => {
                let bench: SocketAddr = format!("{ip}:{bench_port}").parse().map_err(|e| format!("{e}"))?;
                roles.insert(bench, role);
                r.open(bench).await
            }
            None => Ok(bench_port),
        }
    }
}
