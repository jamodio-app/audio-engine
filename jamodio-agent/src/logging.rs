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
