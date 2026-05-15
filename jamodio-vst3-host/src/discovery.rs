//! Discovery des plugins VST3 installés sur le système.

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};

/// Chemins système standards où Windows installe les plugins VST3.
///
/// Convention Steinberg :
/// - `%CommonProgramFiles%\VST3\` (= `C:\Program Files\Common Files\VST3\`)
/// - `%LOCALAPPDATA%\Programs\Common\VST3\` (= installs per-user)
///
/// On scanne récursivement (un seul niveau de sous-dossier) — certains éditeurs
/// (Native Instruments, iZotope) regroupent leurs plugins dans un sous-dossier.
pub fn system_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(pf) = std::env::var_os("CommonProgramFiles") {
        paths.push(PathBuf::from(pf).join("VST3"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        paths.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Common")
                .join("VST3"),
        );
    }
    paths
}

/// Walk un répertoire et retourne tous les chemins `.vst3` (bundles ou DLL).
///
/// VST3 distribution formats supportés sur Windows :
/// - Bundle moderne : `<name>.vst3/Contents/x86_64-win/<name>.vst3` (depuis SDK 3.6.10)
/// - Legacy DLL : `<name>.vst3` direct (fichier unique)
///
/// On retourne le chemin du `.vst3` root (le dossier bundle OU le fichier DLL).
/// La résolution du binaire interne est faite par `loader::resolve_binary()`.
///
/// On scanne 1 niveau de sous-dossiers (Native Instruments / iZotope regroupent).
pub fn scan_directory(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_one_level(dir, &mut out);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.extension().and_then(|s| s.to_str()) != Some("vst3") {
            walk_one_level(&path, &mut out);
        }
    }
    out
}

fn walk_one_level(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("vst3") {
            out.push(path);
        }
    }
}
