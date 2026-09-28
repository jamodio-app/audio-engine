//! Un scénario de banc : TOUT ce qui décide de ce que le banc envoie.
//!
//! Il s'écrit en JSON (`--save-scenario`) et se relit (`--scenario`) : une
//! campagne se rejoue à l'identique, et une future interface n'aura qu'à
//! produire ce fichier pour lancer un test précis. Le scénario complet est
//! recopié en tête des résultats — on sait toujours ce qui a tourné.

use crate::profile::{PeerProfile, Speech};
use serde::{Deserialize, Serialize};

/// Borne haute du nombre de musiciens : au-delà, le banc n'a pas été pensé
/// (capacité du faux serveur, lisibilité du résumé). Demandé : jusqu'à 9.
pub const MAX_MUSICIANS: u32 = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Nom libre, repris dans les résultats.
    pub name: String,
    /// Nombre de musiciens au premier et au dernier palier, TOI compris : à 2,
    /// l'agent reçoit 1 flux ; à 9, il en reçoit 8.
    pub from_musicians: u32,
    pub to_musicians: u32,
    /// Durée d'un palier, et sa part d'installation exclue de l'analyse (le
    /// tampon apprend le lien).
    pub step_secs: u64,
    pub warmup_secs: u64,
    /// Profils des musiciens simulés, attribués dans l'ordre d'arrivée et
    /// réutilisés en boucle s'il y en a moins que de musiciens.
    pub peers: Vec<PeerProfile>,
    /// Canal d'entrée du talkback de l'agent (0 = canal 1) ; `None` = l'agent
    /// n'envoie pas de talkback.
    #[serde(default)]
    pub send_voice_channel: Option<u8>,
    /// Périphériques de l'agent, au format strict `{idx}:{name}` ; `None` = ceux
    /// par défaut du système.
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default)]
    pub output_device: Option<String>,
    /// Canal de l'instrument (0 = canal 1) ; `None` = le choix de l'agent.
    #[serde(default)]
    pub channel_index: Option<u8>,
    /// Plugin inséré sur l'instrument, chargé DANS l'Audio Engine comme en
    /// session (nom exact, tel que `session-bench plugins` l'affiche). C'est la
    /// charge réelle d'un musicien (ex. AmpliTube) ; `None` = aucun.
    #[serde(default)]
    pub plugin: Option<String>,
    /// Graine : la même graine rejoue exactement les mêmes retards et pertes.
    pub seed: u64,
    /// Adresse du faux serveur telle que l'agent doit la joindre. `"auto"` =
    /// l'adresse réseau de CETTE machine (192.168.x.x…). Jamais 127.0.0.1 :
    /// l'Audio Engine livré refuse d'envoyer le son vers le bouclage (aucun vrai
    /// serveur n'y est — protection contre le détournement du micro, revue du
    /// 12/07/2026) ; les paquets vers l'adresse réseau de la machine restent sur
    /// la machine, ce qui garde le mode local.
    pub server_ip: String,
    /// WebSocket de l'Audio Engine.
    pub agent_url: String,
    /// Mode « réseau local » (B0.2) : contrôle du relais `IP:PORT` lancé sur une
    /// SECONDE machine (`session-bench relay`). Les flux passent alors par le
    /// vrai réseau et la carte réseau de la machine mesurée. `None` = local.
    #[serde(default)]
    pub relay: Option<String>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            name: "montee-2-a-9".into(),
            from_musicians: 2,
            to_musicians: 9,
            step_secs: 300,
            warmup_secs: 30,
            peers: vec![PeerProfile::preset("regular").expect("préréglage")],
            send_voice_channel: None,
            input_device: None,
            output_device: None,
            channel_index: None,
            plugin: None,
            seed: 1,
            server_ip: "auto".into(),
            agent_url: "ws://127.0.0.1:9876".into(),
            relay: None,
        }
    }
}

impl Scenario {
    /// Vérifie le scénario AVANT de toucher à l'agent : une erreur ici coûte une
    /// seconde, la même découverte au palier 7 coûterait une demi-heure.
    pub fn validate(&self) -> Result<(), String> {
        if self.from_musicians < 2 {
            return Err("il faut au moins 2 musiciens (toi + 1)".into());
        }
        if self.to_musicians < self.from_musicians || self.to_musicians > MAX_MUSICIANS {
            return Err(format!("dernier palier entre {} et {MAX_MUSICIANS} musiciens", self.from_musicians));
        }
        if self.warmup_secs >= self.step_secs {
            return Err("l'installation doit être plus courte que le palier".into());
        }
        if self.step_secs < 30 {
            return Err("palier de 30 s au moins (sinon rien de mesurable)".into());
        }
        if self.server_ip != "auto" {
            match self.server_ip.parse::<std::net::IpAddr>() {
                Ok(ip) if ip.is_loopback() || ip.is_unspecified() => {
                    return Err(format!(
                        "server_ip {ip} : l'Audio Engine refuse le bouclage — mettre \"auto\" ou l'adresse réseau de la machine"
                    ))
                }
                Ok(_) => {}
                Err(_) => return Err(format!("server_ip illisible : {}", self.server_ip)),
            }
        }
        if let Some(r) = &self.relay {
            r.parse::<std::net::SocketAddr>()
                .map_err(|e| format!("relay « {r} » : {e} (attendu IP:PORT)"))?;
        }
        if self.peers.is_empty() {
            return Err("au moins un profil de musicien".into());
        }
        for p in &self.peers {
            if !(0.0..=100.0).contains(&p.loss_pct) {
                return Err(format!("{} : perte hors de 0-100 %", p.name));
            }
            if let crate::profile::Jitter::Exponential { mean_ms } = p.jitter {
                if !(mean_ms > 0.0 && mean_ms <= 100.0) {
                    return Err(format!("{} : gigue moyenne hors de ]0, 100] ms", p.name));
                }
            }
            if let Some(Speech::Bursts { talk_mean_s, silence_mean_s }) = p.voice {
                if !(talk_mean_s > 0.0 && silence_mean_s > 0.0) {
                    return Err(format!("{} : durées de parole/silence > 0", p.name));
                }
            }
        }
        Ok(())
    }

    /// Profil du musicien numéro `musician` (2 = le premier simulé).
    pub fn peer(&self, musician: u32) -> &PeerProfile {
        &self.peers[(musician as usize - 2) % self.peers.len()]
    }

    /// Flux simulés sans gigue ni perte. Les causes réception, décodage et
    /// consommation sont locales par définition, en mode local comme à travers
    /// le relais : le critère 1 peut juger dès que les flux sont réguliers.
    pub fn is_regular(&self) -> bool {
        self.peers
            .iter()
            .all(|p| p.jitter == crate::profile::Jitter::None && p.loss_pct == 0.0)
    }

    /// Durée totale de la campagne.
    pub fn total_secs(&self) -> u64 {
        u64::from(self.to_musicians - self.from_musicians + 1) * self.step_secs
    }
}

/// Adresse réseau de cette machine : celle par laquelle elle sortirait vers
/// internet. Aucun paquet n'est envoyé (un `connect` UDP ne fait que choisir la
/// route).
pub fn primary_local_ip() -> Result<std::net::IpAddr, String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    // TEST-NET-1 (RFC 5737) : jamais routé, seulement consulté pour la route.
    sock.connect("192.0.2.1:9")
        .map_err(|e| format!("aucune route réseau : brancher le réseau ({e})"))?;
    let ip = sock.local_addr().map_err(|e| e.to_string())?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return Err("aucune adresse réseau locale : brancher le réseau (Wi-Fi ou câble)".into());
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_scenario_par_defaut_monte_de_2_a_9_et_se_valide() {
        let s = Scenario::default();
        s.validate().unwrap();
        assert_eq!((s.from_musicians, s.to_musicians), (2, 9));
        assert!(s.is_regular());
        assert_eq!(s.total_secs(), 8 * 300);
    }

    #[test]
    fn un_scenario_se_rejoue_a_l_identique_depuis_son_fichier() {
        let mut s = Scenario::default();
        s.peers.push(PeerProfile {
            name: "wifi-bavard".into(),
            voice: Some(Speech::Bursts { talk_mean_s: 2.0, silence_mean_s: 4.0 }),
            ..PeerProfile::preset("wifi").unwrap()
        });
        s.send_voice_channel = Some(1);
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Scenario = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn une_faute_de_frappe_dans_le_fichier_est_refusee() {
        let mut v = serde_json::to_value(Scenario::default()).unwrap();
        v["to_musicans"] = 9.into();
        assert!(serde_json::from_value::<Scenario>(v).is_err());
    }

    #[test]
    fn les_scenarios_absurdes_sont_refuses_avant_de_toucher_a_l_agent() {
        let bad = [
            Scenario { from_musicians: 1, ..Scenario::default() },
            Scenario { to_musicians: 1, ..Scenario::default() },
            Scenario { to_musicians: 40, ..Scenario::default() },
            Scenario { warmup_secs: 300, ..Scenario::default() },
            Scenario { peers: vec![], ..Scenario::default() },
            Scenario { server_ip: "127.0.0.1".into(), ..Scenario::default() },
            Scenario { server_ip: "pas-une-ip".into(), ..Scenario::default() },
            Scenario { relay: Some("10.0.0.2".into()), ..Scenario::default() },
            Scenario {
                peers: vec![PeerProfile { loss_pct: 150.0, ..PeerProfile::preset("regular").unwrap() }],
                ..Scenario::default()
            },
        ];
        for s in bad {
            assert!(s.validate().is_err(), "{s:?}");
        }
    }

    #[test]
    fn les_profils_se_reutilisent_en_boucle() {
        let s = Scenario {
            peers: vec![PeerProfile::preset("ethernet").unwrap(), PeerProfile::preset("wifi").unwrap()],
            ..Scenario::default()
        };
        assert_eq!(s.peer(2).name, "ethernet");
        assert_eq!(s.peer(3).name, "wifi");
        assert_eq!(s.peer(4).name, "ethernet");
        assert!(!s.is_regular(), "de la gigue simulée : pas « réguliers »");
    }
}
