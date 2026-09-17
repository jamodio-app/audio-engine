//! L'interface est-elle VRAIMENT branchée ? — vérité indépendante du pilote ASIO.
//!
//! # Pourquoi (recette PC du 17/09/2026)
//!
//! Interface Focusrite débranchée EN SESSION : son pilote ASIO reste chargeable.
//! Il s'ouvre, annonce ses latences et sa taille de buffer… et ne délivre plus un
//! seul callback. L'agent croyait donc avoir reconstruit ses flux (rebuild `Ok`)
//! et recommençait toutes les 2 s — 24 fois d'affilée dans le rapport, sans jamais
//! rien dire au musicien, en saturant son propre verrou (« agent overloaded »,
//! commandes du navigateur ignorées, changement d'interface impossible).
//!
//! On ne peut donc pas demander au pilote ASIO si SON matériel est là : il répond
//! oui. Sur Windows, le même matériel est AUSSI exposé par WASAPI, qui, lui, suit
//! le branchement USB réel. C'est la source de vérité utilisée ici.
//!
//! **Lot 0 : ce module ne décide RIEN.** Il observe et journalise, pour que le
//! rapport de bug tranche « matériel absent » / « matériel là mais pilote muet »
//! — les deux cas qu'on ne sait pas distinguer aujourd'hui. Les décisions
//! (déclarer l'interface indisponible, espacer les tentatives, prévenir le
//! navigateur) viendront au lot suivant.
//!
//! Hors thread audio, jamais dans un callback : aucune latence ajoutée.

/// Ce que dit le matériel, indépendamment du pilote ASIO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Un point d'entrée/sortie du système porte le nom de cette interface.
    Present,
    /// Le système ne connaît plus aucun point de cette interface : débranchée.
    Absent,
    /// Rien à conclure : pilote enveloppe (ASIO4ALL/FlexASIO, qui n'est pas une
    /// interface), énumération indisponible, ou plateforme sans seconde vue
    /// (macOS : CoreAudio est déjà la vérité, cf. `device.rs`).
    Unknown,
}

impl Presence {
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Present => "present",
            Presence::Absent => "absent",
            Presence::Unknown => "unknown",
        }
    }
}

/// Mots trop communs pour identifier une interface : ils apparaissent dans les
/// noms de pilotes ET dans ceux des points du système, sur des matériels
/// différents. Un rapprochement sur ces mots-là ne prouverait rien.
const GENERIC: &[&str] = &[
    "asio", "usb", "audio", "driver", "device", "sound", "son", "carte", "card", "interface",
    "in", "out", "input", "output", "inputs", "outputs", "line", "ligne", "mic", "micro",
    "microphone", "speaker", "speakers", "haut", "parleur", "parleurs", "casque", "headphones",
    "analog", "analogue", "digital", "stereo", "mono", "main", "master", "playback", "capture",
    "front", "rear", "realtek", "windows", "default", "defaut", "systeme", "system", "v2", "v1",
];

/// Mots distinctifs d'un nom de périphérique : minuscules, sans accents, sans les
/// mots communs ci-dessus, sans les nombres seuls (« 2- Scarlett » : le « 2- » est
/// un numéro d'instance Windows, pas une marque).
fn distinctive_tokens(name: &str) -> Vec<String> {
    name.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            match c {
                'à' | 'â' | 'ä' => 'a',
                'é' | 'è' | 'ê' | 'ë' => 'e',
                'î' | 'ï' => 'i',
                'ô' | 'ö' => 'o',
                'ù' | 'û' | 'ü' => 'u',
                c if c.is_ascii_alphanumeric() => c,
                _ => ' ',
            }
        })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| w.len() > 1 && !w.chars().all(|c| c.is_ascii_digit()) && !GENERIC.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Le matériel du pilote `driver` est-il visible parmi les points `endpoints` du
/// système ? PURE. Un mot distinctif partagé suffit : le nom d'un pilote
/// (« Focusrite USB ASIO ») et celui d'un point système (« Ligne (Focusrite USB
/// Audio) ») ne sont jamais identiques.
///
/// Ce rapprochement ne SÉLECTIONNE aucun périphérique — la doctrine « device id
/// strict » reste entière : il ne sert qu'à savoir si le matériel est branché.
pub fn presence_from_names(driver: &str, endpoints: &[String]) -> Presence {
    if crate::audio::device::is_wrapper_asio(driver) {
        return Presence::Unknown; // enveloppe logicielle : elle survit à tout débranchement
    }
    let wanted = distinctive_tokens(driver);
    if wanted.is_empty() || endpoints.is_empty() {
        return Presence::Unknown;
    }
    let found = endpoints
        .iter()
        .any(|e| distinctive_tokens(e).iter().any(|t| wanted.contains(t)));
    if found { Presence::Present } else { Presence::Absent }
}

/// Points d'entrée/sortie que le SYSTÈME expose, pilote ASIO exclu.
///
/// Windows : WASAPI, qui suit le branchement USB réel. Appelé hors du thread
/// COM-STA réservé à ASIO (cf. `com_exec`) : cpal initialise COM sur le thread
/// appelant, et l'énumération WASAPI ne recharge aucun pilote ASIO.
#[cfg(target_os = "windows")]
pub fn system_endpoint_names() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = match cpal::host_from_id(cpal::HostId::Wasapi) {
        Ok(h) => h,
        Err(_) => return vec![],
    };
    let mut names = Vec::new();
    if let Ok(devices) = host.devices() {
        for d in devices {
            if let Ok(n) = d.name() {
                names.push(n);
            }
        }
    }
    names
}

/// macOS : CoreAudio est DÉJÀ la vérité (un débranchement retire le périphérique
/// de l'énumération, cf. `device.rs`). Pas de seconde vue à interroger.
#[cfg(not(target_os = "windows"))]
pub fn system_endpoint_names() -> Vec<String> {
    vec![]
}

/// Présence du matériel de `driver` selon le système, avec les noms observés
/// (journalisés tels quels : c'est eux qui tranchent dans un rapport de bug).
pub fn probe(driver: &str) -> (Presence, Vec<String>) {
    let endpoints = system_endpoint_names();
    (presence_from_names(driver, &endpoints), endpoints)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn interface_branchee_reconnue_malgre_des_noms_differents() {
        // Cas du rapport : pilote « Focusrite USB ASIO », points système en français.
        let endpoints = names(&[
            "Ligne (Focusrite USB Audio)",
            "Haut-parleurs (Focusrite USB Audio)",
            "Microphone (Realtek(R) Audio)",
        ]);
        assert_eq!(presence_from_names("Focusrite USB ASIO", &endpoints), Presence::Present);
    }

    #[test]
    fn interface_debranchee_reconnue_absente() {
        // Le pilote ASIO reste chargeable ; le matériel, lui, a disparu du système.
        let endpoints = names(&["Microphone (Realtek(R) Audio)", "Haut-parleurs (Realtek(R) Audio)"]);
        assert_eq!(presence_from_names("Focusrite USB ASIO", &endpoints), Presence::Absent);
    }

    #[test]
    fn autres_interfaces_courantes() {
        let endpoints = names(&["Ligne (2- Scarlett 2i2 USB)", "Speakers (Yamaha Steinberg USB Audio)"]);
        assert_eq!(presence_from_names("Focusrite USB ASIO", &endpoints), Presence::Absent);
        assert_eq!(presence_from_names("Scarlett 2i2 USB", &endpoints), Presence::Present);
        assert_eq!(presence_from_names("Yamaha Steinberg USB ASIO", &endpoints), Presence::Present);
    }

    #[test]
    fn pilote_enveloppe_sans_materiel_propre() {
        // ASIO4ALL / FlexASIO enveloppent le matériel des autres : leur nom ne
        // désigne aucune interface, on ne conclut rien.
        let endpoints = names(&["Ligne (Focusrite USB Audio)"]);
        assert_eq!(presence_from_names("ASIO4ALL v2", &endpoints), Presence::Unknown);
        assert_eq!(presence_from_names("FlexASIO", &endpoints), Presence::Unknown);
    }

    #[test]
    fn sans_enumeration_ou_sans_mot_distinctif_on_ne_conclut_rien() {
        assert_eq!(presence_from_names("Focusrite USB ASIO", &[]), Presence::Unknown);
        // Un nom entièrement générique ne prouverait rien : mieux vaut se taire.
        assert_eq!(
            presence_from_names("USB Audio Device", &names(&["Ligne (Focusrite USB Audio)"])),
            Presence::Unknown
        );
    }

    #[test]
    fn un_numero_d_instance_ne_fait_pas_une_identite() {
        // « 2- » (numérotation Windows) ne doit JAMAIS servir de preuve.
        let endpoints = names(&["Ligne (2- Behringer UMC)"]);
        assert_eq!(presence_from_names("2- Focusrite USB ASIO", &endpoints), Presence::Absent);
    }
}
