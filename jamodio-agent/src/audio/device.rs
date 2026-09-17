//! Énumération + résolution des devices CPAL — strict, déterministe, sans fallback.
//!
//! ## Identité d'un device
//!
//! CPAL n'expose pas d'ID stable côté plateforme (pas de DeviceUID CoreAudio,
//! pas de Endpoint ID WASAPI), juste un nom. Pour disambiguer deux cartes au
//! même nom (cas réel : deux dongles USB génériques "USB Audio CODEC"), on
//! génère un id composite `"{index}:{name}"` où `index` = position dans
//! `host.input_devices()` au moment de l'énumération.
//!
//! L'id est rendu au browser via `GetDevices`. Le browser le stocke tel
//! quel et le renvoie via `SelectDevices` / `StartCapture`. À la résolution,
//! on parse l'index, on récupère le device à cet index, on vérifie que son
//! nom correspond. Si quoi que ce soit ne match pas (index hors borne, nom
//! changé, énumération vide) → on renvoie `None`.
//!
//! Aucun fuzzy match. Aucun fallback sur le default. Pas d'approximation.
//! L'utilisateur sélectionne X, il a X — ou il a une erreur claire.

use cpal::traits::{DeviceTrait, HostTrait};
use jamodio_audio_core::protocol::AudioDevice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────────────
// Cache d'énumération ASIO — évite de RECHARGER le driver mono-client pendant
// qu'un stream tourne (cause racine du gel Focusrite, prouvée 2026-07-02).
//
// `host.input_devices()` (cpal ASIO) appelle asio-sys `load_driver`→`ASIOInit`
// sur le driver ASIO, GLOBAL au process et MONO-CLIENT. Si un stream est déjà
// actif dessus, cette ré-init se fait SOUS LES PIEDS du stream → ses callbacks
// gèlent en silence (aucun `kAsioResetRequest`). Le browser déclenche ça via
// `GetDevices` (list_inputs/list_outputs) pendant une session.
//
// Correctif : tant qu'un stream ASIO est actif (`ASIO_STREAM_ACTIVE`), on NE
// ré-énumère PAS — on sert le dernier cache connu (rempli avant l'ouverture,
// quand le browser choisit son device). Le flag est posé/levé par le pipeline
// SUR LE THREAD com_exec (sérialisé avec l'énumération → pas de course). Hors
// Windows/ASIO, le pipeline ne pose jamais le flag → énumération fraîche à chaque
// fois, comportement historique strictement inchangé.
// ─────────────────────────────────────────────────────────────────────────────

static ASIO_STREAM_ACTIVE: AtomicBool = AtomicBool::new(false);
static INPUT_CACHE: Mutex<Option<Vec<AudioDevice>>> = Mutex::new(None);
static OUTPUT_CACHE: Mutex<Option<Vec<AudioDevice>>> = Mutex::new(None);

/// Signale au module d'énumération qu'un stream ASIO est ouvert (`true`) ou que
/// le driver a été relâché (`false`). Appelé par le pipeline, TOUJOURS sur le
/// thread com_exec (contrat de sérialisation ASIO). Tant que `true`, sur ASIO,
/// `list_inputs`/`list_outputs` servent le cache au lieu de recharger le driver.
///
/// N'est appelé avec `true` QUE sur le host ASIO (Windows). Sur macOS/WASAPI le
/// flag reste `false` → énumération inchangée.
///
/// Invariant volontaire : pendant une reconstruction (`repair_audio_streams`) ou
/// un park (driver ASIO gardé chaud), le flag reste `true` alors même que les
/// streams sont momentanément fermés. C'est SÛR (ça supprime tout rechargement
/// concurrent pendant la fenêtre vulnérable) et auto-résorbé — `close_audio_driver`
/// le lève à la fermeture RÉELLE (stop / grâce de park expirée). Pire cas : un
/// `GetDevices` dans cette fenêtre reçoit une liste de devices légèrement périmée.
pub fn set_asio_stream_active(active: bool) {
    ASIO_STREAM_ACTIVE.store(active, Ordering::SeqCst);
}

/// `true` si un stream ASIO est actuellement ouvert (driver mono-client tenu).
fn asio_stream_active() -> bool {
    ASIO_STREAM_ACTIVE.load(Ordering::SeqCst)
}

/// Le matériel de ce périphérique est-il branché ? `None` quand on ne peut pas
/// savoir (macOS : l'énumération retire déjà les débranchés ; pilote enveloppe).
/// Un pilote ASIO reste installé — et listé — interface débranchée : sans ça, la
/// liste proposait de choisir une interface incapable de fonctionner (17/09/2026).
fn availability(name: &str, endpoints: &[String]) -> Option<bool> {
    match super::hardware_presence::presence_from_names(name, endpoints) {
        super::hardware_presence::Presence::Present => Some(true),
        super::hardware_presence::Presence::Absent => Some(false),
        super::hardware_presence::Presence::Unknown => None,
    }
}

/// Applique la règle de prudence à une liste : sans AUCUNE interface reconnue
/// présente, on n'affirme aucune absence (cf. `hardware_presence::corroborate`).
fn corroborated(mut list: Vec<AudioDevice>) -> Vec<AudioDevice> {
    let mut availabilities: Vec<Option<bool>> = list.iter().map(|d| d.available).collect();
    super::hardware_presence::corroborate(&mut availabilities);
    for (d, a) in list.iter_mut().zip(availabilities) {
        d.available = a;
    }
    list
}

/// Réévalue le champ `available` d'une liste déjà établie, SANS toucher aux
/// pilotes : la présence vient du système (WASAPI), jamais de l'ASIO. Sert au
/// cache servi pendant une session (driver mono-client tenu) — sinon la liste
/// resterait figée sur l'état du branchement au moment de l'énumération.
fn with_fresh_availability(mut list: Vec<AudioDevice>, endpoints: &[String]) -> Vec<AudioDevice> {
    if endpoints.is_empty() {
        return list; // rien à dire (macOS, ou énumération indisponible)
    }
    for d in &mut list {
        d.available = availability(&d.name, endpoints);
    }
    corroborated(list)
}

/// Format de l'id : `"{index}:{name}"`. Le `:` au plus tôt sépare index/nom.
fn make_id(index: usize, name: &str) -> String {
    format!("{}:{}", index, name)
}

/// Parse un id au format `"{index}:{name}"`. Retourne `(index, name)`.
/// Tolérant aux formats anciens (nom seul) → renvoie `None` plutôt que de
/// deviner. Le browser doit migrer ses settings au prochain `GetDevices`.
fn parse_id(id: &str) -> Option<(usize, &str)> {
    let (idx_str, name) = id.split_once(':')?;
    let idx = idx_str.parse::<usize>().ok()?;
    Some((idx, name))
}

/// Id d'un périphérique du canal **VOIX** : `"{host}:{index}:{name}"`.
///
/// # Pourquoi un préfixe (chantier micro talkback séparé, 09/2026)
///
/// L'index d'un id `{idx}:{name}` est relatif à l'énumération d'UN host. Sur
/// Windows, la liste voix vient de WASAPI alors que la liste instrument vient
/// d'ASIO : sans préfixe, **deux périphériques physiquement différents
/// porteraient le même id**. Le préfixe rend l'id auto-descriptif et permet de
/// REFUSER explicitement un id qui n'appartient pas au host voix courant.
///
/// Les ids instrument restent **non préfixés** → les réglages déjà persistés
/// côté navigateur gardent exactement leur sens (aucune migration).
fn make_voice_id(host: super::host::HostKind, index: usize, name: &str) -> String {
    format!("{}:{}:{}", host.wire_name(), index, name)
}

/// Parse un id voix `"{host}:{index}:{name}"`. **Aucune tolérance** : un id
/// instrument (non préfixé) ou un préfixe inconnu renvoie `None` — c'est une
/// erreur explicite, jamais un repli sur un périphérique voisin.
fn parse_voice_id(id: &str) -> Option<(super::host::HostKind, usize, &str)> {
    use super::host::HostKind;
    let (host_str, rest) = id.split_once(':')?;
    let host = match host_str {
        "asio" => HostKind::Asio,
        "wasapi" => HostKind::Wasapi,
        "coreaudio" => HostKind::CoreAudio,
        _ => return None,
    };
    let (idx_str, name) = rest.split_once(':')?;
    let idx = idx_str.parse::<usize>().ok()?;
    Some((host, idx, name))
}

/// Périphériques d'entrée disponibles pour le **talkback**, énumérés sur le host
/// voix (cf. [`super::host::voice_kind`]).
///
/// Volontairement énuméré **en ligne**, PAS via `com_exec` : ce thread STA
/// persistant est réservé à ASIO (mono-client, `CoCreateInstance`). Le host voix
/// est WASAPI/CoreAudio, il n'a rien à y faire — et l'y envoyer mélangerait deux
/// apartments COM sans raison. Corollaire : l'énumération voix reste possible
/// **pendant** qu'un stream ASIO est ouvert, contrairement à `list_inputs`.
pub fn list_voice_inputs() -> Vec<AudioDevice> {
    let host = super::host::voice();
    let host_kind = super::host::voice_kind();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let Ok(devices) = host.input_devices() else { return vec![] };
    devices
        .enumerate()
        .filter_map(|(idx, d)| {
            let name = d.name().ok()?;
            let cfg = d.default_input_config().ok();
            Some(AudioDevice {
                id: make_voice_id(host_kind, idx, &name),
                is_default: Some(&name) == default_name.as_ref(),
                channels: cfg.as_ref().map(|c| c.channels()).unwrap_or(0),
                // Peut légitimement valoir 44 100 ou 16 000 (micro-casque, micro
                // interne) : le chemin voix rééchantillonne, contrairement au
                // chemin instrument où R2 impose 48 kHz natif.
                native_sample_rate: cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0),
                name,
                // Liste voix = énumération du système lui-même : ce qu'elle contient
                // est branché, par construction.
                available: None,
            })
        })
        .collect()
}

/// Résolution stricte d'un périphérique voix. Même doctrine que
/// [`get_input_device`] : **pas de fuzzy match, pas de repli sur le défaut**.
/// Refuse aussi un id dont le host ne correspond pas au host voix courant.
pub fn get_voice_input_device(id: &str) -> Option<cpal::Device> {
    let (host_kind, idx, expected_name) = parse_voice_id(id)?;
    if host_kind != super::host::voice_kind() {
        tracing::warn!(
            target: "jamodio::devices",
            kind = "voice-input",
            requested_id = %id,
            "id voix d'un autre host que le host voix courant → refus"
        );
        return None;
    }
    let devices: Vec<cpal::Device> = super::host::voice().input_devices().ok()?.collect();
    resolve_among(devices, "voice-input", id, idx, expected_name, |_| true)
}

// Énumération ASIO : voir `super::com_exec` pour le « pourquoi » (asio-sys
// charge les drivers via CoCreateInstance sans initialiser COM → l'énumération
// DOIT tourner sur un thread STA). On passe par le thread COM-STA persistant
// partagé avec l'ouverture/fermeture des streams (`pipeline.rs`) : un seul
// apartment pour tout l'ASIO. macOS (CoreAudio) : exécution inline.

/// List all available audio input devices.
pub fn list_inputs() -> Vec<AudioDevice> {
    // La présence du matériel est lue AVANT d'entrer sur le thread COM-STA : ce
    // thread est réservé à ASIO (apartment unique pour tous les objets du driver),
    // et l'énumération WASAPI n'a rien à y faire — même raison que la liste voix,
    // énumérée en ligne. Revue du 17/09/2026.
    let endpoints = super::hardware_presence::system_endpoint_names();
    super::com_exec::run(move || list_inputs_inner(&endpoints))
}

fn list_inputs_inner(endpoints: &[String]) -> Vec<AudioDevice> {
    // Stream ASIO actif → ne PAS recharger le driver mono-client : sert le cache.
    if asio_stream_active() {
        if let Some(cached) = INPUT_CACHE.lock().unwrap().clone() {
            tracing::debug!(target: "jamodio::devices", "stream ASIO actif — inputs servis depuis le cache (pas de rechargement du driver)");
            // La liste date, mais le BRANCHEMENT, lui, est relu (via le système) :
            // une interface débranchée en session est vue comme telle.
            return with_fresh_availability(cached, endpoints);
        }
        tracing::warn!(target: "jamodio::devices", "stream ASIO actif sans cache d'inputs — renvoi vide (évite le rechargement du driver mono-client)");
        return vec![];
    }

    let host = super::host::active();
    // Décision 04/08 : `is_default` reflète le défaut PRÉFÉRÉ (natif > wrapper),
    // cohérent avec ce que l'agent ouvre réellement (`default_input_id`).
    let default = preferred_default_input_name(&host);

    let Ok(devices) = host.input_devices() else { return vec![] };
    let list: Vec<AudioDevice> = devices
        .enumerate()
        .filter_map(|(idx, d)| {
            let name = d.name().ok()?;
            // Q3 garde-fou 48 kHz : un seul appel à `default_input_config`
            // pour récupérer channels ET sample rate natif (évite la double
            // probe + cohérence des deux infos).
            let cfg = d.default_input_config().ok();
            let channels = cfg.as_ref().map(|c| c.channels()).unwrap_or(0);
            let native_sample_rate = cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0);
            Some(AudioDevice {
                id: make_id(idx, &name),
                available: availability(&name, endpoints),
                name: name.clone(),
                is_default: Some(&name) == default.as_ref(),
                channels,
                native_sample_rate,
            })
        })
        .collect();
    let list = corroborated(list);
    // Mémorise pour servir pendant une session (quand le driver sera tenu).
    *INPUT_CACHE.lock().unwrap() = Some(list.clone());
    list
}

/// List all available audio output devices.
pub fn list_outputs() -> Vec<AudioDevice> {
    let endpoints = super::hardware_presence::system_endpoint_names();
    super::com_exec::run(move || list_outputs_inner(&endpoints))
}

fn list_outputs_inner(endpoints: &[String]) -> Vec<AudioDevice> {
    // Stream ASIO actif → ne PAS recharger le driver mono-client : sert le cache.
    if asio_stream_active() {
        if let Some(cached) = OUTPUT_CACHE.lock().unwrap().clone() {
            tracing::debug!(target: "jamodio::devices", "stream ASIO actif — outputs servis depuis le cache (pas de rechargement du driver)");
            return with_fresh_availability(cached, endpoints);
        }
        tracing::warn!(target: "jamodio::devices", "stream ASIO actif sans cache d'outputs — renvoi vide (évite le rechargement du driver mono-client)");
        return vec![];
    }

    let host = super::host::active();
    let default = host.default_output_device().and_then(|d| d.name().ok());
    let list = corroborated(enumerate_outputs(&host, default.as_deref(), endpoints));
    *OUTPUT_CACHE.lock().unwrap() = Some(list.clone());
    list
}

/// Énumération des sorties — **Windows (WASAPI/ASIO)** : strictement inchangée,
/// `host.output_devices()` (le pré-filtre CPAL). On NE probe PAS sur Windows :
/// ouvrir un driver ASIO mono-client pour tester l'ouvrabilité le rechargerait
/// (cause racine du gel Focusrite). L'index de l'id = position dans
/// `host.output_devices()` (cohérent avec `get_output_device` non-macOS).
#[cfg(not(target_os = "macos"))]
fn enumerate_outputs(host: &cpal::Host, default: Option<&str>, endpoints: &[String]) -> Vec<AudioDevice> {
    let Ok(devices) = host.output_devices() else { return vec![] };
    devices
        .enumerate()
        .filter_map(|(idx, d)| {
            let name = d.name().ok()?;
            let cfg = d.default_output_config().ok();
            let channels = cfg.as_ref().map(|c| c.channels()).unwrap_or(0);
            let native_sample_rate = cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0);
            Some(AudioDevice {
                id: make_id(idx, &name),
                available: availability(&name, endpoints),
                name: name.clone(),
                is_default: Some(name.as_str()) == default,
                channels,
                native_sample_rate,
            })
        })
        .collect()
}

/// Énumération des sorties — **macOS (CoreAudio)** : TOLÉRANTE. CPAL
/// `output_devices()` sous-liste : son pré-filtre écarte toute sortie dont
/// `supported_output_configs()` échoue (« Invalid property value ») — ce qui
/// arrive à des sorties RÉELLES et OUVRABLES (port jack intégré inactif
/// « Écouteurs externes », HP sous un agrégat, etc. — prouvé 2026-07-25). On
/// énumère donc `host.devices()` et on inclut un device s'il est réellement
/// ouvrable en sortie : soit sa config est queryable (rapide), soit un
/// build-probe 48 kHz/2ch réussit (SANS `play()` → aucun son). L'index de l'id
/// = position dans `host.devices()` (cohérent avec `get_output_device` macOS).
/// Résultat mis en cache par l'appelant → le probe ne tourne qu'au rebuild.
#[cfg(target_os = "macos")]
fn enumerate_outputs(host: &cpal::Host, default: Option<&str>, _endpoints: &[String]) -> Vec<AudioDevice> {
    let Ok(devices) = host.devices() else { return vec![] };
    let mut recovered = 0usize;
    let list: Vec<AudioDevice> = devices
        .enumerate()
        .filter_map(|(idx, d)| {
            let name = d.name().ok()?;
            // Chemin rapide : config de sortie queryable → vraie sortie, vrais canaux/SR.
            if let Ok(cfg) = d.default_output_config() {
                return Some(AudioDevice {
                    id: make_id(idx, &name),
                    available: None, // CoreAudio retire déjà les débranchés
                    name: name.clone(),
                    is_default: Some(name.as_str()) == default,
                    channels: cfg.channels(),
                    native_sample_rate: cfg.sample_rate().0,
                });
            }
            // Chemin lent : config non queryable (port inactif/agrégat). Le device
            // est-il quand même OUVRABLE en sortie ? Build-probe 48k/2ch sans play.
            // Les entrées (micros) échouent ici → exclues. Canaux/SR assumés au
            // standard agent (48k/2ch) faute de query — la lecture force 48k de toute façon.
            if probe_output_openable(&d) {
                recovered += 1;
                return Some(AudioDevice {
                    id: make_id(idx, &name),
                    available: None, // CoreAudio retire déjà les débranchés
                    name: name.clone(),
                    is_default: Some(name.as_str()) == default,
                    channels: 2,
                    native_sample_rate: 48_000,
                });
            }
            None // ni queryable ni ouvrable → pas une sortie utilisable
        })
        .collect();
    if recovered > 0 {
        tracing::info!(
            target: "jamodio::devices",
            recovered,
            total = list.len(),
            "énumération sortie tolérante : {recovered} sortie(s) récupérée(s) par build-probe (config CoreAudio non queryable mais ouvrable)"
        );
    }
    list
}

/// macOS — teste si un device est ouvrable en SORTIE en forçant 48 kHz/2ch
/// (build du stream SANS `play()` → aucun son émis, drop immédiat). Discrimine
/// les vraies sorties (ouvrent) des entrées/devices morts (échouent). Ne tourne
/// que pour les devices dont la config n'est pas queryable (chemin lent).
#[cfg(target_os = "macos")]
fn probe_output_openable(device: &cpal::Device) -> bool {
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(48_000),
        buffer_size: cpal::BufferSize::Default,
    };
    device
        .build_output_stream(
            &config,
            |data: &mut [f32], _| data.fill(0.0), // silence — jamais play()é
            |_err| {},
            None,
        )
        .is_ok()
}

/// Return the default input device id (au format `"{idx}:{name}"`).
/// Utilisé uniquement quand le browser n'a JAMAIS sélectionné de device
/// (premier lancement). Une fois une sélection persistée côté browser,
/// elle est l'unique source de vérité.
/// Décision 04/08 (48k/ASIO-only) — vrai si le pilote est un WRAPPER logiciel
/// (ASIO4ALL / FlexASIO) qui enveloppe le matériel via WASAPI au lieu d'un pilote
/// ASIO NATIF d'interface. Un wrapper NE FORCE PAS le matériel en 48 kHz (il suit
/// l'endpoint et se fait refuser hors 48), et peut rééchantillonner en silence.
/// Détection par nom (seul signal disponible). Inerte hors Windows (aucun wrapper).
pub fn is_wrapper_asio(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("asio4all") || n.contains("flexasio")
}

/// Nom du device d'entrée par défaut PRÉFÉRÉ. Décision 04/08 : si le défaut OS est
/// un wrapper (ASIO4ALL/FlexASIO) ET qu'un pilote ASIO NATIF existe, on préfère le
/// natif (il bascule le matériel en 48 kHz via `set_sample_rate` → « ça juste
/// marche » ; le wrapper serait refusé hors 48). Sinon on garde le défaut OS.
/// Inerte hors ASIO (aucun wrapper détecté → renvoie le défaut OS inchangé).
fn preferred_default_input_name(host: &cpal::Host) -> Option<String> {
    let os_default = host.default_input_device().and_then(|d| d.name().ok());
    if !os_default.as_deref().map(is_wrapper_asio).unwrap_or(false) {
        return os_default; // défaut OS non-wrapper (ou absent) → inchangé
    }
    // Défaut = wrapper : chercher le 1er pilote NATIF (non-wrapper).
    let native = host
        .input_devices()
        .ok()
        .and_then(|mut it| it.find_map(|d| d.name().ok().filter(|n| !is_wrapper_asio(n))));
    // Si aucun natif → on gardera le wrapper (défaut OS) faute de mieux.
    if let Some(n) = &native {
        tracing::info!(
            target: "jamodio::devices",
            wrapper = %os_default.as_deref().unwrap_or("?"),
            native = %n,
            "défaut d'entrée : pilote natif préféré au wrapper ASIO (48 kHz forcé)"
        );
    }
    native.or(os_default)
}

pub fn default_input_id() -> Option<String> {
    let host = super::host::active();
    let default_name = preferred_default_input_name(&host)?;
    let devices = host.input_devices().ok()?;
    for (idx, d) in devices.enumerate() {
        if d.name().ok().as_deref() == Some(&default_name) {
            return Some(make_id(idx, &default_name));
        }
    }
    None
}

/// Points d'entrée/sortie vus par le SYSTÈME au démarrage, pilote ASIO exclu
/// (Windows : WASAPI). Sert de repère dans un rapport de bug : si l'interface
/// manque ICI, elle n'était pas branchée — quoi qu'en dise son pilote ASIO.
/// Inerte sur macOS (CoreAudio est déjà la vérité).
fn log_system_endpoints() {
    let names = super::hardware_presence::system_endpoint_names();
    if names.is_empty() {
        return;
    }
    tracing::info!(
        target: "jamodio::devices",
        endpoints = %names.join(" | "),
        "points audio vus par le système (hors pilote ASIO)"
    );
}

/// Dump tous les devices CPAL (appelé une fois au démarrage) : nom exact, canaux,
/// sample rate par défaut, flag default. Aide le debug des cas où le nom d'un device
/// est surprenant (aggregate device, virtuel, UID numérique CoreAudio, etc.).
pub fn log_devices() {
    log_system_endpoints();
    let host = super::host::active();
    // Défaut PRÉFÉRÉ (natif > wrapper) — cohérent avec `default_input_id`/`list_inputs`.
    let def_in = preferred_default_input_name(&host).unwrap_or_default();
    let def_out = host.default_output_device().and_then(|d| d.name().ok()).unwrap_or_default();
    tracing::info!(target: "jamodio::devices", default_input = %def_in, default_output = %def_out, "CPAL devices");
    if let Ok(devices) = host.input_devices() {
        for (idx, d) in devices.enumerate() {
            let name = d.name().unwrap_or_else(|_| "<err>".into());
            let cfg = d.default_input_config().ok();
            let ch = cfg.as_ref().map(|c| c.channels()).unwrap_or(0);
            let sr = cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0);
            tracing::info!(
                target: "jamodio::devices",
                kind = "input",
                index = idx,
                name = %name,
                channels = ch,
                sample_rate = sr,
                is_default = name == def_in,
            );
        }
    }
    // Sortie : on logge l'ÉNUMÉRATION RÉELLE (`list_outputs`, celle qui produit les
    // ids `{idx}:{name}` envoyés au web). Sur macOS elle passe par `host.devices()`
    // (tolérante) → un `host.output_devices().enumerate()` donnerait un index NE
    // correspondant PAS à l'id. On logge donc l'`id` complet, pas un index brut.
    // (log_devices n'est appelé qu'au démarrage → le build-probe éventuel est hors session.)
    for d in list_outputs() {
        tracing::info!(
            target: "jamodio::devices",
            kind = "output",
            id = %d.id,
            name = %d.name,
            channels = d.channels,
            sample_rate = d.native_sample_rate,
            is_default = d.is_default,
        );
    }
}

/// Où se trouve, dans une énumération, le périphérique désigné par `{idx}:{name}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Located {
    /// À sa position, sous son nom : le cas normal.
    AtIndex(usize),
    /// Plus à sa position, mais présent sous le MÊME nom exact, une seule fois : sa
    /// position a glissé (un autre périphérique a été branché/débranché avant lui).
    Moved { from: usize, to: usize },
    /// Aucun périphérique ne porte ce nom : il est absent (débranché).
    Missing,
    /// Plusieurs périphériques portent ce nom et aucun n'est à la position
    /// demandée : impossible de savoir lequel est celui du musicien → refus.
    Ambiguous(usize),
}

/// Localise `{idx}:{expected}` dans `names` (noms de l'énumération, dans l'ordre ;
/// `None` = nom illisible). `eligible(i)` écarte un homonyme qui n'est pas du bon
/// type (macOS : une entrée seule portant le nom d'une sortie) ; il n'est consulté
/// QUE pour les homonymes, jamais pour le cas nominal.
///
/// # Pourquoi (recette D5, 17/09/2026)
///
/// L'index d'un id est la position dans l'énumération du système, et cette position
/// n'est PAS stable : débrancher un casque jack (CoreAudio), un pilote ASIO dont
/// l'interface est absente (cpal saute les pilotes qui ne chargent pas), un
/// périphérique virtuel qui apparaît (WASAPI)… décale tous les suivants. Rejeter sur
/// la seule position faisait déclarer « perdu » un périphérique toujours branché.
///
/// Ce n'est PAS un rapprochement approximatif : le nom doit être IDENTIQUE et
/// UNIQUE. Deux homonymes hors position (deux interfaces identiques) → refus
/// explicite, jamais un choix au hasard. Même règle que la page (`resolveAgentDeviceId`).
fn locate(
    idx: usize,
    expected: &str,
    names: &[Option<String>],
    eligible: impl Fn(usize) -> bool,
) -> Located {
    if names.get(idx).and_then(|n| n.as_deref()) == Some(expected) {
        return Located::AtIndex(idx);
    }
    match unique_by_name(expected, names, eligible) {
        Ok(to) => Located::Moved { from: idx, to },
        Err(0) => Located::Missing,
        Err(count) => Located::Ambiguous(count),
    }
}

/// Position de l'unique périphérique nommé exactement `expected` (homonymes
/// départagés par `eligible`), ou `Err(nombre de candidats)` : 0 = absent, ≥ 2 = ambigu.
fn unique_by_name(
    expected: &str,
    names: &[Option<String>],
    eligible: impl Fn(usize) -> bool,
) -> Result<usize, usize> {
    let same_name: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.as_deref() == Some(expected))
        .map(|(i, _)| i)
        .collect();
    let candidates: Vec<usize> = if same_name.len() > 1 {
        same_name.into_iter().filter(|&i| eligible(i)).collect()
    } else {
        same_name
    };
    match candidates.as_slice() {
        [one] => Ok(*one),
        many => Err(many.len()),
    }
}

/// Résout `id` parmi `devices` selon [`locate`], journalise tout écart au cas
/// nominal et renvoie le périphérique, ou `None` (absent / ambigu).
fn resolve_among(
    devices: Vec<cpal::Device>,
    kind: &'static str,
    id: &str,
    idx: usize,
    expected: &str,
    eligible: impl Fn(&cpal::Device) -> bool,
) -> Option<cpal::Device> {
    let names: Vec<Option<String>> = devices.iter().map(|d| d.name().ok()).collect();
    let at = match locate(idx, expected, &names, |i| eligible(&devices[i])) {
        Located::AtIndex(i) => i,
        Located::Moved { from, to } => {
            tracing::info!(
                target: "jamodio::devices",
                kind,
                requested_id = %id,
                from,
                to,
                "position du périphérique décalée (branchement/débranchement d'un autre) → retrouvé par son nom exact"
            );
            to
        }
        Located::Missing => {
            tracing::info!(target: "jamodio::devices", kind, requested_id = %id, "périphérique absent (aucun périphérique de ce nom)");
            return None;
        }
        Located::Ambiguous(count) => {
            tracing::warn!(
                target: "jamodio::devices",
                kind,
                requested_id = %id,
                count,
                "plusieurs périphériques portent ce nom, aucun à la position demandée → refus (impossible de savoir lequel)"
            );
            return None;
        }
    };
    devices.into_iter().nth(at)
}

/// Résolution stricte input : parse l'id et retrouve le périphérique selon
/// [`locate`] (position + nom exact ; position glissée → nom exact unique).
/// **Pas de rapprochement approximatif. Pas de repli sur le défaut.** Sinon `None`.
///
/// Le caller (pipeline / ws_server) doit traiter `None` comme une erreur
/// utilisateur explicite (CaptureError côté wire).
pub fn get_input_device(id: &str) -> Option<cpal::Device> {
    let (idx, expected_name) = parse_id(id)?;
    let host = super::host::active();
    let devices: Vec<cpal::Device> = host.input_devices().ok()?.collect();
    resolve_among(devices, "input", id, idx, expected_name, |_| true)
}

/// Résolution stricte output : même règle que `get_input_device`.
pub fn get_output_device(id: &str) -> Option<cpal::Device> {
    let (idx, expected_name) = parse_id(id)?;
    let host = super::host::active();
    // L'index de l'id doit indexer la MÊME énumération que `enumerate_outputs` :
    // macOS = `host.devices()` (tolérant, inclut les sorties ouvrables non
    // queryables, mais aussi les entrées seules → un homonyme doit prouver qu'il
    // sort du son) ; Windows = `host.output_devices()` (que des sorties).
    #[cfg(target_os = "macos")]
    {
        let devices: Vec<cpal::Device> = host.devices().ok()?.collect();
        resolve_among(devices, "output", id, idx, expected_name, |d| {
            d.default_output_config().is_ok() || probe_output_openable(d)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let devices: Vec<cpal::Device> = host.output_devices().ok()?.collect();
        resolve_among(devices, "output", id, idx, expected_name, |_| true)
    }
}

/// Id `{idx}:{name}` de la sortie par défaut du système À CET INSTANT, dans
/// l'énumération de `get_output_device`. Sert au REPLI quand la sortie choisie a
/// disparu : on ouvre CE périphérique précis, pas « la sortie par défaut ».
///
/// Pourquoi : sur macOS, un flux ouvert sur la sortie par défaut la SUIT (cpal
/// prend l'unité CoreAudio `DefaultOutput`). Rebrancher un casque y envoyait le son
/// quelques secondes avant le retour sur la sortie choisie (recette D5), et le
/// message « le son passe par X » devenait faux. Ouvert par son id, le repli reste
/// là où on l'a annoncé, sur tous les systèmes. `None` : pas de sortie par défaut,
/// ou homonymes indépartageables.
///
/// Appelable depuis le thread COM (ne repasse pas par `com_exec`).
pub fn default_output_id() -> Option<String> {
    let host = super::host::active();
    let name = host.default_output_device()?.name().ok()?;
    #[cfg(target_os = "macos")]
    let devices: Vec<cpal::Device> = host.devices().ok()?.collect();
    #[cfg(not(target_os = "macos"))]
    let devices: Vec<cpal::Device> = host.output_devices().ok()?.collect();
    let names: Vec<Option<String>> = devices.iter().map(|d| d.name().ok()).collect();
    #[cfg(target_os = "macos")]
    let eligible = |i: usize| devices[i].default_output_config().is_ok() || probe_output_openable(&devices[i]);
    #[cfg(not(target_os = "macos"))]
    let eligible = |_: usize| true;
    match unique_by_name(&name, &names, eligible) {
        Ok(idx) => Some(make_id(idx, &name)),
        Err(count) => {
            tracing::warn!(target: "jamodio::devices", default = %name, count, "sortie par défaut introuvable ou homonyme dans l'énumération");
            None
        }
    }
}

/// Résout le default output device, sans demande explicite du browser.
/// Utilisé uniquement comme bootstrap pour l'output (le browser ne pilote
/// pas l'output dans le flow actuel — sortie déléguée à l'OS, cf. décision
/// audio_output_decision). Renvoie le device + son nom pour log.
pub fn default_output_device() -> Option<(cpal::Device, String)> {
    let host = super::host::active();
    let dev = host.default_output_device()?;
    let name = dev.name().ok()?;
    Some((dev, name))
}

/// Nom du device de sortie par DÉFAUT OS — lecture **COM-safe** (via `com_exec`,
/// contrat STA Windows). Utilisé par le superviseur qui fait suivre le « Défaut
/// système » au défaut OS en live (Lot A2) : il tourne hors du thread com_exec,
/// donc doit passer par lui pour interroger CoreAudio/WASAPI sans planter. `None`
/// si aucun défaut. N'est appelé QUE hors ASIO (cf. `output_follows_os_default`).
pub fn default_output_name() -> Option<String> {
    super::com_exec::run(|| {
        let host = super::host::active();
        host.default_output_device().and_then(|d| d.name().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str) -> AudioDevice {
        AudioDevice {
            id: format!("0:{name}"),
            available: None,
            name: name.into(),
            is_default: false,
            channels: 2,
            native_sample_rate: 48_000,
        }
    }

    /// Cœur du correctif 0.5.4-17 : quand un stream ASIO est actif, l'énumération
    /// NE recharge PAS le driver (elle sert le cache) — le chemin cache retourne
    /// AVANT tout appel à `host::active()`/cpal, donc ce test tourne sans matériel
    /// ni COM. Un seul test (les statics globaux interdisent l'exécution parallèle).
    #[test]
    fn asio_active_serves_cache_and_never_reloads() {
        // État de départ propre.
        set_asio_stream_active(false);
        *INPUT_CACHE.lock().unwrap() = None;
        *OUTPUT_CACHE.lock().unwrap() = None;

        // Cache pré-rempli (comme si le browser avait énuméré avant d'ouvrir).
        *INPUT_CACHE.lock().unwrap() = Some(vec![dev("Focusrite USB ASIO")]);
        *OUTPUT_CACHE.lock().unwrap() = Some(vec![dev("Focusrite USB ASIO Out")]);

        // Stream actif → sert le cache SANS toucher cpal (sinon ce test paniquerait
        // ou dépendrait du matériel sur une machine de CI).
        set_asio_stream_active(true);
        let ins = list_inputs_inner(&[]);
        let outs = list_outputs_inner(&[]);
        assert_eq!(ins.len(), 1, "inputs servis depuis le cache");
        assert_eq!(ins[0].name, "Focusrite USB ASIO");
        assert_eq!(outs.len(), 1, "outputs servis depuis le cache");

        // Stream actif MAIS cache vide → renvoi vide (jamais de rechargement driver).
        *INPUT_CACHE.lock().unwrap() = None;
        let ins_empty = list_inputs_inner(&[]);
        assert!(ins_empty.is_empty(), "actif sans cache ⇒ vide, pas de reload");

        // Nettoyage (statics globaux partagés avec les autres tests).
        set_asio_stream_active(false);
        *INPUT_CACHE.lock().unwrap() = None;
        *OUTPUT_CACHE.lock().unwrap() = None;
    }

    #[test]
    fn id_voix_aller_retour() {
        use super::super::host::HostKind;
        let id = make_voice_id(HostKind::Wasapi, 2, "Casque USB");
        assert_eq!(id, "wasapi:2:Casque USB");
        let (host, idx, name) = parse_voice_id(&id).expect("id voix valide");
        assert_eq!((host, idx, name), (HostKind::Wasapi, 2, "Casque USB"));
    }

    #[test]
    fn id_voix_accepte_un_nom_contenant_des_deux_points() {
        // Les noms de devices en contiennent (« Scarlett 2i2: Entrée 1 ») : seuls
        // les DEUX premiers `:` sont des séparateurs, le reste appartient au nom.
        use super::super::host::HostKind;
        let (host, idx, name) =
            parse_voice_id("coreaudio:0:Scarlett 2i2: Entrée 1").expect("id voix valide");
        assert_eq!((host, idx, name), (HostKind::CoreAudio, 0, "Scarlett 2i2: Entrée 1"));
    }

    #[test]
    fn id_voix_refuse_un_id_instrument_ou_malforme() {
        // Doctrine device id strict : on refuse, on ne devine pas. Un id
        // instrument (non préfixé) n'est PAS un id voix valide — sinon l'index
        // d'une énumération ASIO serait lu comme un index WASAPI.
        assert!(parse_voice_id("2:Casque USB").is_none(), "id instrument refusé");
        assert!(parse_voice_id("jack:2:Casque").is_none(), "host inconnu refusé");
        assert!(parse_voice_id("wasapi:x:Casque").is_none(), "index non numérique refusé");
        assert!(parse_voice_id("wasapi:2").is_none(), "nom manquant refusé");
        assert!(parse_voice_id("").is_none());
    }

    fn names(list: &[&str]) -> Vec<Option<String>> {
        list.iter().map(|n| Some((*n).to_string())).collect()
    }

    #[test]
    fn locate_nominal_a_sa_position() {
        let n = names(&["Microphone externe", "Microphone MacBook Pro"]);
        assert_eq!(locate(1, "Microphone MacBook Pro", &n, |_| true), Located::AtIndex(1));
    }

    #[test]
    fn locate_position_glissee_retrouve_par_nom_exact() {
        // Recette D5 : casque jack débranché → « Microphone externe » disparaît,
        // le micro intégré passe de 1 à 0 — il est toujours là.
        let n = names(&["Microphone MacBook Pro", "MJAudioRecorder"]);
        assert_eq!(
            locate(1, "Microphone MacBook Pro", &n, |_| true),
            Located::Moved { from: 1, to: 0 }
        );
        // ASIO : un pilote sans interface n'est pas listé, le suivant remonte.
        let n = names(&["Focusrite USB ASIO"]);
        assert_eq!(locate(1, "Focusrite USB ASIO", &n, |_| true), Located::Moved { from: 1, to: 0 });
        // Un périphérique branché avant décale vers le bas.
        let n = names(&["BlackHole 2ch", "Microsoft Teams Audio", "Haut-parleurs MacBook Pro"]);
        assert_eq!(
            locate(1, "Haut-parleurs MacBook Pro", &n, |_| true),
            Located::Moved { from: 1, to: 2 }
        );
    }

    #[test]
    fn locate_absent() {
        let n = names(&["Microphone MacBook Pro"]);
        assert_eq!(locate(0, "Microphone externe", &n, |_| true), Located::Missing);
        assert_eq!(locate(5, "Microphone externe", &[], |_| true), Located::Missing);
    }

    #[test]
    fn locate_jamais_de_nom_approchant() {
        // Windows renumérote « 2- USB Audio » : ce n'est PAS le même nom → absent.
        let n = names(&["Haut-parleurs (2- USB Audio CODEC)"]);
        assert_eq!(locate(0, "Haut-parleurs (USB Audio CODEC)", &n, |_| true), Located::Missing);
        let n = names(&["scarlett 2i2 usb"]);
        assert_eq!(locate(0, "Scarlett 2i2 USB", &n, |_| true), Located::Missing, "casse différente = autre nom");
    }

    #[test]
    fn locate_homonymes_hors_position_refus() {
        // Deux interfaces identiques, celle du musicien n'est plus à sa position :
        // impossible de savoir laquelle → refus, jamais un choix au hasard.
        let n = names(&["USB Audio CODEC", "Haut-parleurs", "USB Audio CODEC"]);
        assert_eq!(locate(3, "USB Audio CODEC", &n, |_| true), Located::Ambiguous(2));
    }

    #[test]
    fn locate_homonymes_departages_par_le_type() {
        // macOS : un casque USB expose une entrée seule et une sortie seule du même
        // nom ; en sortie, seul l'homonyme qui sort du son compte.
        let n = names(&["USB PnP Sound Device", "USB PnP Sound Device"]);
        let only_second_outputs = |i: usize| i == 1;
        assert_eq!(
            locate(4, "USB PnP Sound Device", &n, only_second_outputs),
            Located::Moved { from: 4, to: 1 }
        );
        // Le cas nominal ne consulte jamais le type.
        assert_eq!(locate(0, "USB PnP Sound Device", &n, |_| panic!("non consulté")), Located::AtIndex(0));
    }

    #[test]
    fn locate_ignore_les_noms_illisibles() {
        let n = vec![None, Some("Focusrite USB ASIO".to_string())];
        assert_eq!(locate(0, "Focusrite USB ASIO", &n, |_| true), Located::Moved { from: 0, to: 1 });
    }


    #[test]
    fn disponibilite_derivee_de_la_presence_materielle() {
        let endpoints = vec!["Ligne (Focusrite USB Audio)".to_string()];
        assert_eq!(availability("Focusrite USB ASIO", &endpoints), Some(true));
        assert_eq!(availability("Scarlett 2i2 USB", &endpoints), Some(false), "installée mais débranchée");
        assert_eq!(availability("ASIO4ALL v2", &endpoints), None, "pilote enveloppe : rien à conclure");
        assert_eq!(availability("Focusrite USB ASIO", &[]), None, "sans énumération système : rien à conclure");
    }

    #[test]
    fn le_cache_ne_fige_pas_le_branchement() {
        // Pendant une session ASIO, la liste vient du cache (le driver mono-client
        // ne doit pas être rechargé) — mais la présence, elle, est relue. Sans
        // énumération système (macOS, ou indisponible), la liste est rendue telle quelle.
        let cached = vec![dev("Focusrite USB ASIO")];
        assert_eq!(with_fresh_availability(cached.clone(), &[])[0].available, None);
    }

    #[test]
    fn la_liste_ne_declare_une_absence_qu_avec_une_preuve() {
        // PC en Bureau à distance : Windows ne montre que sa sortie distante, aucune
        // interface n'est reconnue → aucune mention de branchement (17/09/2026).
        let distant = vec!["Sortie audio de l\u{2019}ordinateur distant".to_string()];
        let mut liste = vec![dev("Focusrite USB ASIO"), dev("ASIO4ALL v2")];
        for d in &mut liste {
            d.available = availability(&d.name, &distant);
        }
        let liste = corroborated(liste);
        assert_eq!(liste[0].available, None, "branchée ou non : on n'en sait rien ici");
        assert_eq!(liste[1].available, None);

        // Session Windows locale : une interface est reconnue, le verdict des autres
        // devient exploitable.
        let local = vec![
            "Ligne (Focusrite USB Audio)".to_string(),
            "Haut-parleurs (Realtek(R) Audio)".to_string(),
        ];
        let mut liste = vec![dev("Focusrite USB ASIO"), dev("Scarlett 2i2 USB")];
        for d in &mut liste {
            d.available = availability(&d.name, &local);
        }
        let liste = corroborated(liste);
        assert_eq!(liste[0].available, Some(true));
        assert_eq!(liste[1].available, Some(false), "installée mais débranchée");
    }
}
