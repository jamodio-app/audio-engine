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

// Énumération ASIO : voir `super::com_exec` pour le « pourquoi » (asio-sys
// charge les drivers via CoCreateInstance sans initialiser COM → l'énumération
// DOIT tourner sur un thread STA). On passe par le thread COM-STA persistant
// partagé avec l'ouverture/fermeture des streams (`pipeline.rs`) : un seul
// apartment pour tout l'ASIO. macOS (CoreAudio) : exécution inline.

/// List all available audio input devices.
pub fn list_inputs() -> Vec<AudioDevice> {
    super::com_exec::run(list_inputs_inner)
}

fn list_inputs_inner() -> Vec<AudioDevice> {
    // Stream ASIO actif → ne PAS recharger le driver mono-client : sert le cache.
    if asio_stream_active() {
        if let Some(cached) = INPUT_CACHE.lock().unwrap().clone() {
            tracing::debug!(target: "jamodio::devices", "stream ASIO actif — inputs servis depuis le cache (pas de rechargement du driver)");
            return cached;
        }
        tracing::warn!(target: "jamodio::devices", "stream ASIO actif sans cache d'inputs — renvoi vide (évite le rechargement du driver mono-client)");
        return vec![];
    }

    let host = super::host::active();
    let default = host.default_input_device().and_then(|d| d.name().ok());

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
                name: name.clone(),
                is_default: Some(&name) == default.as_ref(),
                channels,
                native_sample_rate,
            })
        })
        .collect();
    // Mémorise pour servir pendant une session (quand le driver sera tenu).
    *INPUT_CACHE.lock().unwrap() = Some(list.clone());
    list
}

/// List all available audio output devices.
pub fn list_outputs() -> Vec<AudioDevice> {
    super::com_exec::run(list_outputs_inner)
}

fn list_outputs_inner() -> Vec<AudioDevice> {
    // Stream ASIO actif → ne PAS recharger le driver mono-client : sert le cache.
    if asio_stream_active() {
        if let Some(cached) = OUTPUT_CACHE.lock().unwrap().clone() {
            tracing::debug!(target: "jamodio::devices", "stream ASIO actif — outputs servis depuis le cache (pas de rechargement du driver)");
            return cached;
        }
        tracing::warn!(target: "jamodio::devices", "stream ASIO actif sans cache d'outputs — renvoi vide (évite le rechargement du driver mono-client)");
        return vec![];
    }

    let host = super::host::active();
    let default = host.default_output_device().and_then(|d| d.name().ok());
    let list = enumerate_outputs(&host, default.as_deref());
    *OUTPUT_CACHE.lock().unwrap() = Some(list.clone());
    list
}

/// Énumération des sorties — **Windows (WASAPI/ASIO)** : strictement inchangée,
/// `host.output_devices()` (le pré-filtre CPAL). On NE probe PAS sur Windows :
/// ouvrir un driver ASIO mono-client pour tester l'ouvrabilité le rechargerait
/// (cause racine du gel Focusrite). L'index de l'id = position dans
/// `host.output_devices()` (cohérent avec `get_output_device` non-macOS).
#[cfg(not(target_os = "macos"))]
fn enumerate_outputs(host: &cpal::Host, default: Option<&str>) -> Vec<AudioDevice> {
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
fn enumerate_outputs(host: &cpal::Host, default: Option<&str>) -> Vec<AudioDevice> {
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
pub fn default_input_id() -> Option<String> {
    let host = super::host::active();
    let default_name = host.default_input_device().and_then(|d| d.name().ok())?;
    let devices = host.input_devices().ok()?;
    for (idx, d) in devices.enumerate() {
        if d.name().ok().as_deref() == Some(&default_name) {
            return Some(make_id(idx, &default_name));
        }
    }
    None
}

/// Dump tous les devices CPAL (appelé une fois au démarrage) : nom exact, canaux,
/// sample rate par défaut, flag default. Aide le debug des cas où le nom d'un device
/// est surprenant (aggregate device, virtuel, UID numérique CoreAudio, etc.).
pub fn log_devices() {
    let host = super::host::active();
    let def_in = host.default_input_device().and_then(|d| d.name().ok()).unwrap_or_default();
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
    if let Ok(devices) = host.output_devices() {
        for (idx, d) in devices.enumerate() {
            let name = d.name().unwrap_or_else(|_| "<err>".into());
            let cfg = d.default_output_config().ok();
            let ch = cfg.as_ref().map(|c| c.channels()).unwrap_or(0);
            let sr = cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0);
            tracing::info!(
                target: "jamodio::devices",
                kind = "output",
                index = idx,
                name = %name,
                channels = ch,
                sample_rate = sr,
                is_default = name == def_out,
            );
        }
    }
}

/// Résolution stricte input : parse l'id, vérifie que le device à cet index
/// existe et a toujours le même nom. **Pas de fuzzy match. Pas de fallback
/// sur le device par défaut.** Si quelque chose ne match pas → `None`.
///
/// Le caller (pipeline / ws_server) doit traiter `None` comme une erreur
/// utilisateur explicite (CaptureError côté wire).
pub fn get_input_device(id: &str) -> Option<cpal::Device> {
    let (idx, expected_name) = parse_id(id)?;
    let host = super::host::active();
    let devices: Vec<cpal::Device> = host.input_devices().ok()?.collect();
    let dev = devices.into_iter().nth(idx)?;
    let actual_name = dev.name().ok()?;
    if actual_name == expected_name {
        Some(dev)
    } else {
        tracing::warn!(
            target: "jamodio::devices",
            kind = "input",
            requested_id = %id,
            actual_name = %actual_name,
            "id resolved to a device with a different name (hot-plug ?) → reject"
        );
        None
    }
}

/// Résolution stricte output : même logique que `get_input_device`.
pub fn get_output_device(id: &str) -> Option<cpal::Device> {
    let (idx, expected_name) = parse_id(id)?;
    let host = super::host::active();
    // L'index de l'id doit indexer la MÊME énumération que `enumerate_outputs` :
    // macOS = `host.devices()` (tolérant, inclut les sorties ouvrables non
    // queryables) ; Windows = `host.output_devices()` (inchangé). Sinon l'index
    // glisse et la résolution stricte par nom rejette (→ repli non-fatal côté
    // pipeline). La vérification de nom ci-dessous reste le garde strict.
    #[cfg(target_os = "macos")]
    let devices: Vec<cpal::Device> = host.devices().ok()?.collect();
    #[cfg(not(target_os = "macos"))]
    let devices: Vec<cpal::Device> = host.output_devices().ok()?.collect();
    let dev = devices.into_iter().nth(idx)?;
    let actual_name = dev.name().ok()?;
    if actual_name == expected_name {
        Some(dev)
    } else {
        tracing::warn!(
            target: "jamodio::devices",
            kind = "output",
            requested_id = %id,
            actual_name = %actual_name,
            "id resolved to a device with a different name (hot-plug ?) → reject"
        );
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str) -> AudioDevice {
        AudioDevice {
            id: format!("0:{name}"),
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
        let ins = list_inputs_inner();
        let outs = list_outputs_inner();
        assert_eq!(ins.len(), 1, "inputs servis depuis le cache");
        assert_eq!(ins[0].name, "Focusrite USB ASIO");
        assert_eq!(outs.len(), 1, "outputs servis depuis le cache");

        // Stream actif MAIS cache vide → renvoi vide (jamais de rechargement driver).
        *INPUT_CACHE.lock().unwrap() = None;
        let ins_empty = list_inputs_inner();
        assert!(ins_empty.is_empty(), "actif sans cache ⇒ vide, pas de reload");

        // Nettoyage (statics globaux partagés avec les autres tests).
        set_asio_stream_active(false);
        *INPUT_CACHE.lock().unwrap() = None;
        *OUTPUT_CACHE.lock().unwrap() = None;
    }
}
