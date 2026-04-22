use cpal::traits::{DeviceTrait, HostTrait};
use jamodio_audio_core::protocol::AudioDevice;

/// List all available audio input devices.
pub fn list_inputs() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.name().ok());

    host.input_devices()
        .map(|devices| {
            devices
                .filter_map(|d| {
                    let name = d.name().ok()?;
                    let channels = d.default_input_config().map(|c| c.channels()).unwrap_or(0);
                    Some(AudioDevice {
                        id: name.clone(),
                        name: name.clone(),
                        is_default: Some(&name) == default.as_ref(),
                        channels,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// List all available audio output devices.
pub fn list_outputs() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let default = host.default_output_device().and_then(|d| d.name().ok());

    host.output_devices()
        .map(|devices| {
            devices
                .filter_map(|d| {
                    let name = d.name().ok()?;
                    let channels = d.default_output_config().map(|c| c.channels()).unwrap_or(0);
                    Some(AudioDevice {
                        id: name.clone(),
                        name: name.clone(),
                        is_default: Some(&name) == default.as_ref(),
                        channels,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Return the default input device name.
pub fn default_input_name() -> Option<String> {
    let host = cpal::default_host();
    host.default_input_device().and_then(|d| d.name().ok())
}

/// Dump tous les devices CPAL (appelé une fois au démarrage) : nom exact, canaux,
/// sample rate par défaut, flag default. Aide le debug des cas où le nom d'un device
/// est surprenant (aggregate device, virtuel, UID numérique CoreAudio, etc.).
pub fn log_devices() {
    let host = cpal::default_host();
    let def_in = host.default_input_device().and_then(|d| d.name().ok()).unwrap_or_default();
    let def_out = host.default_output_device().and_then(|d| d.name().ok()).unwrap_or_default();
    eprintln!("[Jamodio] ── CPAL devices ──────────────────────");
    eprintln!("[Jamodio] Default input  : '{}'", def_in);
    eprintln!("[Jamodio] Default output : '{}'", def_out);
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            let name = d.name().unwrap_or_else(|_| "<err>".into());
            let cfg = d.default_input_config().ok();
            let ch = cfg.as_ref().map(|c| c.channels()).unwrap_or(0);
            let sr = cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0);
            let mark = if name == def_in { " [default]" } else { "" };
            eprintln!("[Jamodio]   IN  '{}' — {}ch @ {}Hz{}", name, ch, sr, mark);
        }
    }
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            let name = d.name().unwrap_or_else(|_| "<err>".into());
            let cfg = d.default_output_config().ok();
            let ch = cfg.as_ref().map(|c| c.channels()).unwrap_or(0);
            let sr = cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0);
            let mark = if name == def_out { " [default]" } else { "" };
            eprintln!("[Jamodio]   OUT '{}' — {}ch @ {}Hz{}", name, ch, sr, mark);
        }
    }
    eprintln!("[Jamodio] ──────────────────────────────────────");
}

/// Comparaison "fuzzy" : on normalise (lowercase + retrait d'espaces/ponctuation)
/// et on regarde si l'une contient l'autre. Absorbe les variations type
/// "Scarlett Solo (3rd Gen)" vs "Scarlett Solo 3rd Gen" ou "MacBook Pro Microphone"
/// vs "MacBook Pro - Microphone".
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn fuzzy_match(a: &str, b: &str) -> bool {
    let na = normalize(a);
    let nb = normalize(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb || na.contains(&nb) || nb.contains(&na)
}

/// Find an input device by name, or return default.
/// Falls back to default device if named device is not found.
/// 1er essai : égalité stricte. 2e essai : matching fuzzy (normalisation + contains)
/// pour absorber les petites variations de formatage entre CPAL et l'OS.
pub fn get_input_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(n) = name {
        let devices: Vec<cpal::Device> = host.input_devices().ok()?.collect();
        if let Some(dev) = devices.iter().find(|d| d.name().ok().as_deref() == Some(n)) {
            return Some(dev.clone());
        }
        if let Some(dev) = devices.iter().find(|d| {
            d.name().ok().as_deref().map_or(false, |dn| fuzzy_match(dn, n))
        }) {
            eprintln!("[DEVICE] Input '{}' matched fuzzy → '{}'", n, dev.name().unwrap_or_default());
            return Some(dev.clone());
        }
        eprintln!("[DEVICE] Input '{}' not found, using default", n);
    }
    host.default_input_device()
}

/// Find an output device by name, or return default.
/// Falls back to default device if named device is not found.
pub fn get_output_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(n) = name {
        let devices: Vec<cpal::Device> = host.output_devices().ok()?.collect();
        if let Some(dev) = devices.iter().find(|d| d.name().ok().as_deref() == Some(n)) {
            return Some(dev.clone());
        }
        if let Some(dev) = devices.iter().find(|d| {
            d.name().ok().as_deref().map_or(false, |dn| fuzzy_match(dn, n))
        }) {
            eprintln!("[DEVICE] Output '{}' matched fuzzy → '{}'", n, dev.name().unwrap_or_default());
            return Some(dev.clone());
        }
        eprintln!("[DEVICE] Output '{}' not found, using default", n);
    }
    host.default_output_device()
}
