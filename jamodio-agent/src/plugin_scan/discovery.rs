//! Découverte des items à scanner — IN-PROCESS, données seules.
//!
//! Ne charge AUCUN code plugin : liste de fichiers (Windows) ou lecture du
//! registre AudioComponent (macOS). C'est le seul travail de scan qui reste
//! dans le process agent ; l'instanciation (le risque) part dans le worker.

/// Items à faire scanner par le worker (format protocole : path `.vst3`
/// Windows, `au:type/subtype/manuf` macOS).
#[cfg(target_os = "windows")]
pub fn discover_items() -> Vec<String> {
    let mut items = Vec::new();
    for dir in jamodio_vst3_host::discovery::system_paths() {
        for path in jamodio_vst3_host::discovery::scan_directory(&dir) {
            items.push(path.to_string_lossy().into_owned());
        }
    }
    items
}

#[cfg(target_os = "macos")]
pub fn discover_items() -> Vec<String> {
    use super::protocol::AuItem;
    jamodio_au_host::enumerate_components()
        .into_iter()
        .map(|c| {
            AuItem {
                au_type: c.au_type,
                subtype: c.subtype,
                manufacturer: c.manufacturer,
            }
            .encode()
        })
        .collect()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn discover_items() -> Vec<String> {
    Vec::new()
}
