//! Initialisation du logging structuré pour l'agent Jamodio.
//!
//! Convention (cf. CLAUDE.md) :
//! - Niveaux : `error/warn/info/debug/trace`.
//! - Format : `2026-05-06T10:22:14.123Z LEVEL [target] message field=value`
//! - Cibles (`target` automatique) : nom du module Rust (ex `jamodio_agent::pipeline`).
//! - Sorties : stderr (format compact) + fichier rolling journalier.
//!
//! Le fichier de logs est dans :
//! - macOS   : `~/Library/Logs/Jamodio/agent.log.YYYY-MM-DD`
//! - Windows : `%APPDATA%/Jamodio/logs/agent.log.YYYY-MM-DD`
//! - Linux   : `~/.local/state/jamodio/agent.log.YYYY-MM-DD`
//!
//! Le filtre par défaut est `info,jamodio_agent=debug,jamodio_audio_core=debug` ;
//! override possible via la variable d'environnement `RUST_LOG`.
//!
//! Le `WorkerGuard` retourné DOIT rester vivant pendant toute la durée de
//! l'exécution — sinon le worker async du file appender s'arrête et plus
//! aucun log n'est persisté sur disque.

use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Defaults pour `collect_recent_logs` (cf. handler `GetLogsArchive`).
/// 5 MB plafond pour rester très en-dessous de la limite Resend (25 MB)
/// après encoding base64 (~+33 %) côté browser.
pub const DEFAULT_LOG_ARCHIVE_DAYS: u32 = 3;
pub const DEFAULT_LOG_ARCHIVE_BYTES: u64 = 5_000_000;

/// Retourne le dossier où les logs sont écrits. Crée le dossier si absent.
pub fn log_dir() -> PathBuf {
    let dir = base_log_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(target_os = "macos")]
fn base_log_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Logs/Jamodio")
}

#[cfg(target_os = "windows")]
fn base_log_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| "C:\\Temp".into());
    PathBuf::from(appdata).join("Jamodio").join("logs")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn base_log_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/state/jamodio")
}

/// Initialise le subscriber global tracing.
/// À appeler UNE fois au tout début de `main()`.
pub fn init() -> WorkerGuard {
    let dir = log_dir();
    let file_appender = rolling::daily(&dir, "agent.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,jamodio_agent=debug,jamodio_audio_core=debug")
    });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_level(true)
        .with_ansi(true)
        .compact()
        .with_filter(env_filter);

    // Fichier : pas d'ANSI, niveaux + targets explicites pour faciliter le grep.
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_filter(EnvFilter::new("debug"));

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();

    tracing::info!(
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        log_dir = %dir.display(),
        version = env!("CARGO_PKG_VERSION"),
        "Jamodio Audio Engine starting"
    );

    guard
}

/// Récupère les `max_days` derniers fichiers de logs et les concatène en
/// plain text UTF-8, séparés par des entêtes lisibles. Si la concaténation
/// dépasse `max_bytes`, on tronque les fichiers les PLUS ANCIENS d'abord
/// (on préserve le contexte récent qui est généralement le plus utile pour
/// debugger un bug rapporté à l'instant).
///
/// Format de sortie :
/// ```text
/// ====== agent.log.2026-05-08 ======
/// <contenu>
/// ====== agent.log.2026-05-09 ======
/// <contenu>
/// ```
///
/// Retourne `(content, files_included, truncated)`.
pub fn collect_recent_logs(max_days: u32, max_bytes: u64) -> (String, Vec<String>, bool) {
    let dir = log_dir();
    // Liste les fichiers `agent.log.*` triés par nom (donc par date ASC).
    let mut entries: Vec<(String, PathBuf)> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let n = p.file_name()?.to_string_lossy().to_string();
                if n.starts_with("agent.log.") {
                    Some((n, p))
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => return (String::new(), Vec::new(), false),
    };
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Garde les `max_days` derniers (les plus récents).
    let take = (max_days as usize).max(1);
    if entries.len() > take {
        entries = entries.split_off(entries.len() - take);
    }

    // Lit chaque fichier ; assemble du plus récent au plus ancien dans un
    // Vec<String>, puis on inverse pour l'ordre chronologique d'affichage.
    let mut sections: Vec<String> = Vec::with_capacity(entries.len());
    let mut files: Vec<String> = Vec::with_capacity(entries.len());
    let mut total: u64 = 0;
    let mut truncated = false;

    for (name, path) in entries.iter().rev() {
        let body = std::fs::read_to_string(path).unwrap_or_else(|e| format!("(read error: {e})"));
        let header = format!("\n====== {name} ======\n");
        let section_size = (header.len() + body.len()) as u64;
        if total + section_size > max_bytes {
            // Première troncature : on coupe le fichier en cours pour rentrer.
            let remaining = max_bytes.saturating_sub(total + header.len() as u64) as usize;
            if remaining > 0 {
                // Garde la fin du fichier (tail) — la section en cours est
                // forcément la plus ancienne du paquet à ce stade.
                let start = body.len().saturating_sub(remaining);
                let tail = &body[start..];
                let tail_aligned = match tail.find('\n') {
                    Some(i) => &tail[i + 1..],
                    None => tail,
                };
                sections.push(format!(
                    "{header}(... older logs truncated to fit {max_bytes} bytes ...)\n{tail_aligned}"
                ));
                files.push(name.clone());
            }
            truncated = true;
            break;
        }
        sections.push(format!("{header}{body}"));
        files.push(name.clone());
        total += section_size;
    }

    sections.reverse();
    files.reverse();
    (sections.concat(), files, truncated)
}
