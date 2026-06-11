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

/// List all available audio input devices.
pub fn list_inputs() -> Vec<AudioDevice> {
    let host = super::host::active();
    let default = host.default_input_device().and_then(|d| d.name().ok());

    let Ok(devices) = host.input_devices() else { return vec![] };
    devices
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
        .collect()
}

/// List all available audio output devices.
pub fn list_outputs() -> Vec<AudioDevice> {
    let host = super::host::active();
    let default = host.default_output_device().and_then(|d| d.name().ok());

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
                is_default: Some(&name) == default.as_ref(),
                channels,
                native_sample_rate,
            })
        })
        .collect()
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
