//! Un scénario de banc : TOUT ce qui décide de ce que le banc envoie.
//!
//! Il s'écrit en JSON (`--save-scenario`) et se relit (`--scenario`) : une
//! campagne se rejoue à l'identique, et une future interface n'aura qu'à
//! produire ce fichier pour lancer un test précis. Le scénario complet est
//! recopié en tête des résultats — on sait toujours ce qui a tourné.

use crate::profile::PeerProfile;
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
    /// Émetteur distant `IP:PORT` (`session-bench remote` sur une SECONDE
    /// machine, lot R1-bis) : les flux simulés y sont fabriqués et envoyés, et
    /// ce que l'agent envoie y est reçu. La précision du banc ne dépend plus de
    /// la machine mesurée. `None` = flux fabriqués ici.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Windows : les fils du banc prennent la priorité de fil la plus haute
    /// (`THREAD_PRIORITY_TIME_CRITICAL`) au lieu de MMCSS « Pro Audio ». Pendant
    /// de l'interrupteur `no-mmcss` de l'Audio Engine (Lot W1,
    /// PLAN-FREINAGE-RESEAU-WINDOWS-2026-09) : sans lui, les fils MMCSS du banc
    /// déclencheraient à eux seuls le freinage réseau qu'on veut mesurer.
    #[serde(default)]
    pub no_mmcss: bool,
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
            remote: None,
            no_mmcss: false,
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
        if let Some(r) = &self.remote {
            if self.relay.is_some() {
                return Err("relais ou émetteur distant, pas les deux (l'émetteur distant fait déjà passer les flux par le réseau)".into());
            }
            let addr = r
                .parse::<std::net::SocketAddr>()
                .map_err(|e| format!("remote « {r} » : {e} (attendu IP:PORT)"))?;
            if addr.ip().is_loopback() || addr.ip().is_unspecified() {
                return Err(format!("remote {addr} : l'Audio Engine refuse le bouclage — l'adresse réseau de la seconde machine"));
            }
        }
        if self.peers.is_empty() {
            return Err("au moins un profil de musicien".into());
        }
        for p in &self.peers {
            p.validate()?;
        }
        // Chaque événement doit tomber dans la campagne, pour CHAQUE musicien qui
        // porte ce profil (les profils se réutilisent en boucle) : un événement
        // qui ne se produirait jamais serait une promesse silencieusement non tenue.
        let total = self.total_secs() as f64;
        for m in 2..=self.to_musicians {
            let (p, left) = (self.peer(m), total - self.arrival_s(m));
            if let Some(c) = p.changes.iter().find(|c| c.at_s >= left) {
                return Err(format!(
                    "musicien {m} ({}) : changement de lien à {} s après son arrivée, mais il ne reste que {left} s de campagne",
                    p.name, c.at_s
                ));
            }
            if let Some(a) = p.absences.iter().find(|a| a.at_s + a.for_s >= left) {
                return Err(format!(
                    "musicien {m} ({}) : absence de {} s à {} s après son arrivée, mais il ne reste que {left} s de campagne (il doit revenir avant la fin)",
                    p.name, a.for_s, a.at_s
                ));
            }
        }
        Ok(())
    }

    /// Profil du musicien numéro `musician` (2 = le premier simulé).
    pub fn peer(&self, musician: u32) -> &PeerProfile {
        &self.peers[(musician as usize - 2) % self.peers.len()]
    }

    /// Instant d'arrivée du musicien `musician` dans la campagne (s) : tous au
    /// premier palier, puis un par palier.
    pub fn arrival_s(&self, musician: u32) -> f64 {
        (u64::from(musician.saturating_sub(self.from_musicians)) * self.step_secs) as f64
    }

    /// Flux simulés parfaitement réguliers : ni gigue, ni perte, ni pic, ni
    /// désordre, ni dérive, ni événement. Les causes réception, décodage et
    /// consommation sont locales par définition, en mode local comme à travers
    /// le relais : le critère 1 peut juger dès que les flux sont réguliers.
    pub fn is_regular(&self) -> bool {
        self.peers.iter().all(PeerProfile::is_regular)
    }

    /// Seule la dérive est simulée (liens réguliers, aucun événement) : tout trou
    /// vient alors de la dérive ou de la machine (critère 7).
    pub fn is_drift_only(&self) -> bool {
        self.peers.iter().any(|p| p.drift_ppm != 0.0)
            && self
                .peers
                .iter()
                .all(|p| p.link().is_regular() && p.changes.is_empty() && p.absences.is_empty())
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
    use crate::profile::{Absence, Change, Link, Speech};

    #[test]
    fn le_scenario_par_defaut_monte_de_2_a_9_et_se_valide() {
        let s = Scenario::default();
        s.validate().unwrap();
        assert_eq!((s.from_musicians, s.to_musicians), (2, 9));
        assert!(s.is_regular());
        assert_eq!(s.total_secs(), 8 * 300);
    }

    /// Un scénario écrit avant R1 (sortie de `session-bench scenario` modifiée,
    /// telle quelle) se relit, et ses musiciens sont ceux d'alors.
    #[test]
    fn un_scenario_d_avant_r1_se_relit_tel_quel() {
        let json = r#"{"name":"montee-2-a-9","from_musicians":2,"to_musicians":9,"step_secs":300,"warmup_secs":30,"peers":[{"name":"ethernet","jitter":{"model":"exponential","mean_ms":0.7},"loss_pct":1.0,"voice":null},{"name":"wifi","jitter":{"model":"exponential","mean_ms":5.5},"loss_pct":0.0,"voice":{"model":"always"}}],"send_voice_channel":1,"input_device":null,"output_device":null,"channel_index":null,"plugin":null,"seed":1,"server_ip":"auto","agent_url":"ws://127.0.0.1:9876","relay":null,"no_mmcss":false}"#;
        let s: Scenario = serde_json::from_str(json).unwrap();
        s.validate().unwrap();
        let eth = s.peer(2);
        assert_eq!((eth.name.as_str(), eth.loss_pct, eth.drift_ppm), ("ethernet", 1.0, 0.0));
        assert!(eth.changes.is_empty() && eth.absences.is_empty() && eth.burst_loss.is_none());
        assert_eq!(s.peer(3).voice, Some(Speech::Always));
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
            Scenario { remote: Some("10.0.0.2".into()), ..Scenario::default() },
            Scenario { remote: Some("127.0.0.1:51901".into()), ..Scenario::default() },
            Scenario { remote: Some("10.0.0.2:51901".into()), relay: Some("10.0.0.3:51900".into()), ..Scenario::default() },
            Scenario {
                peers: vec![PeerProfile { loss_pct: 150.0, ..PeerProfile::preset("regular").unwrap() }],
                ..Scenario::default()
            },
        ];
        for s in bad {
            assert!(s.validate().is_err(), "{s:?}");
        }
    }

    /// Un événement qui tomberait après la fin de la campagne est refusé, pour
    /// chaque musicien qui porte le profil (le dernier arrive le plus tard).
    #[test]
    fn un_evenement_hors_de_la_campagne_est_refuse() {
        let late = PeerProfile {
            absences: vec![Absence { at_s: 280.0, for_s: 30.0 }],
            ..PeerProfile::preset("regular").unwrap()
        };
        // Montée 2 → 3, paliers de 300 s : m2 arrive à 0 s (600 s devant lui,
        // retour à 310 s : possible), m3 à 300 s (300 s devant lui : il ne
        // reviendrait qu'après la fin).
        let s = Scenario { to_musicians: 3, peers: vec![late.clone()], ..Scenario::default() };
        assert!(s.validate().unwrap_err().contains("musicien 3"));
        let ok = Scenario { from_musicians: 3, to_musicians: 3, step_secs: 400, peers: vec![late], ..Scenario::default() };
        ok.validate().unwrap();
        assert_eq!(ok.arrival_s(3), 0.0);
        let change = PeerProfile {
            changes: vec![Change { at_s: 400.0, link: Link::preset("wifi").unwrap() }],
            ..PeerProfile::preset("regular").unwrap()
        };
        let s = Scenario { from_musicians: 9, peers: vec![change], ..Scenario::default() };
        assert!(s.validate().unwrap_err().contains("changement de lien"));
    }

    #[test]
    fn la_derive_seule_et_la_regularite_se_reconnaissent() {
        let drift = PeerProfile { drift_ppm: 100.0, ..PeerProfile::preset("regular").unwrap() };
        let s = Scenario { peers: vec![drift.clone(), PeerProfile::preset("regular").unwrap()], ..Scenario::default() };
        assert!(s.is_drift_only() && !s.is_regular());
        let s = Scenario { peers: vec![drift, PeerProfile::preset("ethernet").unwrap()], ..Scenario::default() };
        assert!(!s.is_drift_only());
        assert!(!Scenario::default().is_drift_only() && Scenario::default().is_regular());
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
