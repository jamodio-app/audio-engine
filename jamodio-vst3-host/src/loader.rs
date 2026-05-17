//! Chargement dynamique d'un .vst3 + résolution du `GetPluginFactory`.

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};

use libloading::{Library, Symbol};
use vst3::{ComPtr, Steinberg::IPluginFactory};

/// Résout le binaire .vst3 à charger réellement dans `dlopen`.
///
/// Sur Windows, un `.vst3` peut être :
/// - un fichier DLL direct (`<name>.vst3` = fichier) — schéma legacy
/// - un bundle (`<name>.vst3/Contents/x86_64-win/<name>.vst3` = dossier)
pub fn resolve_binary(plugin_path: &Path) -> Result<PathBuf, String> {
    if plugin_path.is_file() {
        return Ok(plugin_path.to_path_buf());
    }
    let name = plugin_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("chemin .vst3 invalide : {}", plugin_path.display()))?;
    let contents = plugin_path.join("Contents");
    if !contents.is_dir() {
        return Err(format!(
            "ni fichier ni bundle : {}",
            plugin_path.display()
        ));
    }
    let candidates = [
        contents.join("x86_64-win").join(format!("{name}.vst3")),
        contents.join("x86-win").join(format!("{name}.vst3")),
    ];
    for c in candidates.iter() {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "aucun binaire VST3 trouvé dans {}",
        plugin_path.display()
    ))
}

/// Module .vst3 chargé : la `Library` (handle dlopen) + un `ComPtr` sur la
/// `IPluginFactory` exposée par le module.
///
/// **Drop order critique** : le champ `factory` doit être déclaré (et donc
/// droppé) AVANT le champ `lib`. Rust drop les champs dans l'ordre de
/// déclaration → la factory est released pendant que la DLL est encore
/// chargée, sinon use-after-unload garanti (l'impl COM de la factory vit
/// dans la DLL).
pub struct LoadedModule {
    /// Chemin original (= `.vst3` choisi par l'user). Gardé pour le diag log.
    #[allow(dead_code)]
    pub plugin_path: PathBuf,
    /// Chemin binaire interne au bundle / DLL flat. Gardé pour le diag log.
    #[allow(dead_code)]
    pub binary_path: PathBuf,
    factory: ComPtr<IPluginFactory>,
    #[allow(dead_code)] // kept alive for the lifetime of the factory
    lib: Library,
}

impl LoadedModule {
    pub fn load(plugin_path: &Path) -> Result<Self, String> {
        let binary_path = resolve_binary(plugin_path)?;
        let lib = unsafe { Library::new(&binary_path) }
            .map_err(|e| format!("dlopen failed for {} : {e}", binary_path.display()))?;

        // Per-OS init (optional but standard).
        unsafe {
            if let Ok(init) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0") {
                if !init() {
                    return Err(format!(
                        "InitDll() returned false for {}",
                        binary_path.display()
                    ));
                }
            }
        }

        // GetPluginFactory entry symbol — mandatoire en VST3.
        let factory_raw = unsafe {
            let sym: Symbol<unsafe extern "system" fn() -> *mut IPluginFactory> = lib
                .get(b"GetPluginFactory\0")
                .map_err(|_| "plugin sans symbole GetPluginFactory".to_string())?;
            sym()
        };
        let factory = unsafe { ComPtr::from_raw(factory_raw) }
            .ok_or_else(|| "GetPluginFactory retourne null".to_string())?;

        Ok(Self {
            plugin_path: plugin_path.to_path_buf(),
            binary_path,
            factory,
            lib,
        })
    }

    pub fn factory(&self) -> &ComPtr<IPluginFactory> {
        &self.factory
    }
}

// SAFETY: le module est tenu sur le main/scan thread (load) ou un thread WS.
// L'accès est sérialisé par le `parking_lot::Mutex` du Vst3Host.
// `Sync` requis pour que `Arc<LoadedModule>` soit Send (= partagé entre l'host
// thread et l'editor thread). La factory COM gère sa propre concurrence en
// interne, et `Library` (libloading) est Sync.
unsafe impl Send for LoadedModule {}
unsafe impl Sync for LoadedModule {}
