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
//! Le filtre par défaut (`DEFAULT_DIRECTIVES`) met NOS crates et NOS cibles
//! `jamodio::*` en `debug`, et tout le reste en `info` ; override possible via la
//! variable d'environnement `RUST_LOG`. Le MÊME filtre s'applique à stderr et au
//! fichier : un fichier plus bavard que la console ne sert personne (cf. la note
//! sur le bruit tiers ci-dessous).
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

/// Filtre par défaut, appliqué À LA FOIS à stderr et au fichier.
///
/// # Pourquoi le fichier n'est plus en `debug` global (05/09/2026)
///
/// Il l'était, et ça a fini par détruire notre capacité de diagnostic : depuis
/// l'arrivée du filtre antibruit, `tract` (le moteur d'inférence) journalise en
/// `debug` À CHAQUE TRAME de voix — mesuré sur un rapport de bug réel : **124
/// lignes par seconde**, soit 99,4 % d'un export de 9 Mo. Le plafond de l'export
/// était atteint par du bruit tiers, et les lignes Jamodio de l'incident qu'on
/// cherchait avaient été évincées. Un log qui chasse l'information qu'il est
/// censé porter ne vaut rien.
///
/// `info` global suffit à museler tract (son bavardage est en `debug`) sans
/// perdre ce que les crates tierces disent d'important. `jamodio` (sans suffixe)
/// couvre les cibles explicites `jamodio::pipeline`, `jamodio::mixer`… que les
/// directives par nom de crate ne matchent pas.
const DEFAULT_DIRECTIVES: &str = "info,jamodio=debug,jamodio_agent=debug,jamodio_audio_core=debug,jamodio_au_host=debug,jamodio_vst3_host=debug";

/// Construit le filtre : `RUST_LOG` s'il est posé (choix explicite du
/// développeur, jamais contredit), sinon `DEFAULT_DIRECTIVES`.
fn build_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_DIRECTIVES))
}

/// Nombre de fichiers journaliers `agent.log.*` conservés sur disque.
/// `rolling::daily` ne purge jamais : sans ça les fichiers s'accumulent
/// indéfiniment (constaté : ~150 MB / 60 fichiers en prod). L'export support
/// n'en lit que `DEFAULT_LOG_ARCHIVE_DAYS` (3) ; on garde une marge confortable
/// pour le diagnostic manuel tout en bornant l'espace disque.
pub const LOG_RETENTION_FILES: usize = 14;

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

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_level(true)
        .with_ansi(true)
        .compact()
        .with_filter(build_filter());

    // Fichier : pas d'ANSI, niveaux + targets explicites pour faciliter le grep.
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_filter(build_filter());

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

    // Purge des vieux fichiers journaliers (rétention bornée). Une fois au boot :
    // le fichier du jour est toujours dans les plus récents, jamais supprimé.
    prune_old_logs(&dir, LOG_RETENTION_FILES);

    guard
}

/// Supprime les fichiers `agent.log.*` les plus anciens en ne gardant que les
/// `keep` plus récents. Le nommage `agent.log.YYYY-MM-DD` trie
/// chronologiquement par ordre lexical, donc pas de parsing de date fragile.
/// Best-effort : toute erreur d'I/O est ignorée (ne doit jamais bloquer le boot).
fn prune_old_logs(dir: &std::path::Path, keep: usize) {
    let mut names: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent.log."))
            })
            .collect(),
        Err(_) => return,
    };
    if names.len() <= keep {
        return;
    }
    names.sort(); // ordre lexical == chronologique (YYYY-MM-DD)
    let to_remove = names.len() - keep;
    let mut removed = 0usize;
    for path in names.into_iter().take(to_remove) {
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(
            target: "jamodio::support",
            removed,
            kept = keep,
            "purge des anciens fichiers de logs"
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// Writer de test : capture ce que le subscriber écrirait DANS LE FICHIER.
    /// On teste le filtre par son effet observable (des lignes écrites ou non),
    /// pas par la forme de la chaîne de directives — c'est l'effet qui protège
    /// l'export support.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Émet un jeu d'événements représentatif sous le filtre par défaut et rend
    /// ce qui a été écrit.
    fn sortie_sous_filtre_par_defaut() -> String {
        let cap = Capture::default();
        let sub = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(DEFAULT_DIRECTIVES))
            .with_writer(cap.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || {
            // Bruit tiers : le moteur d'inférence du filtre antibruit.
            tracing::debug!(target: "tract_core::plan", "BRUIT_PLAN");
            tracing::debug!(target: "tract_core::optim::change_axes", "BRUIT_AXES");
            tracing::debug!(target: "tract_pulse::model", "BRUIT_PULSE");
            // Tiers utile.
            tracing::info!(target: "tract_onnx::model", "TIERS_INFO");
            tracing::warn!(target: "cpal::host::wasapi", "TIERS_WARN");
            // Nos cibles explicites + nos crates.
            tracing::debug!(target: "jamodio::pipeline", "NOTRE_PIPELINE");
            tracing::debug!(target: "jamodio::perfstats", "NOTRE_PERFSTATS");
            tracing::debug!(target: "jamodio::voice_capture", "NOTRE_VOIX");
            tracing::debug!(target: "jamodio_audio_core::mixer", "NOTRE_CORE");
        });
        let bytes = cap.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// Régression réelle (05/09/2026) : `tract` journalisait en `debug` à chaque
    /// trame de voix — 124 lignes/s, 99,4 % d'un export support de 9 Mo, et les
    /// lignes Jamodio de l'incident cherché avaient été évincées par le plafond.
    #[test]
    fn le_bavardage_tiers_en_debug_n_atteint_plus_les_logs() {
        let out = sortie_sous_filtre_par_defaut();
        for marqueur in ["BRUIT_PLAN", "BRUIT_AXES", "BRUIT_PULSE"] {
            assert!(!out.contains(marqueur), "{marqueur} ne doit pas être journalisé");
        }
    }

    /// …sans museler ce que les crates tierces disent d'important.
    #[test]
    fn le_tiers_reste_audible_a_partir_de_info() {
        let out = sortie_sous_filtre_par_defaut();
        assert!(out.contains("TIERS_INFO"));
        assert!(out.contains("TIERS_WARN"));
    }

    /// Nos crates ET nos cibles explicites `jamodio::*` gardent le niveau debug :
    /// c'est le contenu utile de l'export support.
    #[test]
    fn nos_cibles_restent_en_debug() {
        let out = sortie_sous_filtre_par_defaut();
        for marqueur in ["NOTRE_PIPELINE", "NOTRE_PERFSTATS", "NOTRE_VOIX", "NOTRE_CORE"] {
            assert!(out.contains(marqueur), "{marqueur} doit rester journalisé");
        }
    }
}
