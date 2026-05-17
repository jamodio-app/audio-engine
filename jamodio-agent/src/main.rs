//! Jamodio Desktop Audio Agent

// Pas de console CMD au démarrage en release Windows. En dev (debug_assertions),
// la console reste pour faciliter le diag (eprintln, panics visibles).
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod audio;
mod logging;
mod pipeline;
mod ws_server;

use jamodio_audio_core::mixer::mixer::AudioMixer;
use parking_lot::Mutex;
use pipeline::PipelineState;
use std::sync::Arc;
use tauri::{
    menu::{Menu, MenuItem},
    Manager,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_updater::UpdaterExt;
use ws_server::WsServerHandle;

/// Détection minimale de la langue d'interface (FR/EN) sans dépendance externe.
/// Lit LC_ALL → LC_MESSAGES → LANG (POSIX, Linux/macOS). Sur Windows, ces vars
/// ne sont pas posées par défaut → on retombe sur 'en'. Suffisant pour les 2
/// labels du menu Tray ; pas besoin d'un système i18n complet ici.
fn detect_lang() -> &'static str {
    let raw = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    let prefix = raw.split(['_', '.', '@', '-']).next().unwrap_or("").to_lowercase();
    if prefix == "fr" { "fr" } else { "en" }
}

#[tauri::command]
fn open_log_dir() -> Result<String, String> {
    let dir = logging::log_dir();
    opener::open(&dir).map_err(|e| format!("Cannot open log dir: {e}"))?;
    Ok(dir.to_string_lossy().into_owned())
}

#[tauri::command]
fn get_log_dir() -> String {
    logging::log_dir().to_string_lossy().into_owned()
}

/// Retourne la version du binaire (CARGO_PKG_VERSION figé au build).
/// L'UI l'appelle au boot pour remplir le label `.version` — évite la
/// duplication entre Cargo.toml et l'HTML statique.
#[tauri::command]
fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Vérifie une éventuelle update via l'endpoint configuré dans
/// tauri.conf.json (`updater` bloc), télécharge + installe + restart si
/// dispo. Diffuse `Shutdown{reason:"update"}` à tous les clients WS AVANT
/// `app.restart()` pour que le browser puisse afficher un toast et
/// préparer un fallback gracieux (au lieu de voir un TCP close brutal
/// + watchdog timeout 3 s).
async fn check_for_update(app: tauri::AppHandle, ws_handle: WsServerHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(target: "jamodio::updater", error = %e, "updater unavailable");
            return;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            tracing::info!(
                target: "jamodio::updater",
                current = env!("CARGO_PKG_VERSION"),
                available = %update.version,
                "update available — downloading & installing"
            );
            let mut downloaded: u64 = 0;
            let download_result = update
                .download_and_install(
                    |chunk_length, content_length| {
                        downloaded += chunk_length as u64;
                        if let Some(total) = content_length {
                            tracing::debug!(
                                target: "jamodio::updater",
                                progress = format!("{}/{}", downloaded, total),
                                "download progress"
                            );
                        }
                    },
                    || tracing::info!(target: "jamodio::updater", "download finished, installing"),
                )
                .await;

            match download_result {
                Ok(_) => {
                    tracing::info!(target: "jamodio::updater", "update installed — broadcasting Shutdown then restart");
                    // Broadcast aux clients WS connectés AVANT restart.
                    // ws_server::handle_connection sleep 200ms après l'envoi
                    // pour laisser le browser recevoir + traiter.
                    ws_handle.broadcast_shutdown("update");
                    // Petit délai supplémentaire ici aussi pour la marge
                    // (les broadcasts tokio sont async, le restart aussi).
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    app.restart();
                }
                Err(e) => tracing::error!(
                    target: "jamodio::updater",
                    error = %e,
                    "download/install failed"
                ),
            }
        }
        Ok(None) => {
            tracing::info!(
                target: "jamodio::updater",
                version = env!("CARGO_PKG_VERSION"),
                "already on latest version"
            );
        }
        Err(e) => tracing::warn!(
            target: "jamodio::updater",
            error = %e,
            "update check failed (offline ? endpoint down ?)"
        ),
    }
}

fn main() {
    // Init tracing AVANT tout le reste : tous les eprintln! ont été migrés
    // vers tracing::{info,warn,error,debug,trace}, et on veut capturer même
    // les events pendant le setup Tauri. Le guard doit rester vivant : on le
    // bind à _log_guard au scope de main() (drop = fin du process = OK).
    let _log_guard = logging::init();

    tauri::Builder::default()
        // ─── Single-instance lock ─────────────────────────────
        // Si un 2e process est lancé (clic répété sur "Lancer", deep link
        // jamodio://launch alors que l'agent tourne déjà, double-clic DMG…),
        // ce hook est appelé dans le 1er process avec les args du 2e, puis
        // le 2e exit immédiatement. On (re-)montre + focus la fenêtre
        // principale pour que l'utilisateur voie qu'elle existe déjà.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!(target: "jamodio::lifecycle", "2nd instance detected — focusing existing window");
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![open_log_dir, get_log_dir, get_version])
        .setup(|app| {
            tracing::info!(target: "jamodio::lifecycle", version = env!("CARGO_PKG_VERSION"), "setup phase");

            // ─── Dump devices CPAL au démarrage (diagnostic) ─────
            audio::device::log_devices();

            // ─── Attach menu to config-based tray icon ──────
            // i18n minimal : libellés FR si la locale OS est FR, EN sinon
            // (couvre la première impression utilisateur EN — cf. ToDo audit 17/05).
            let (show_label, quit_label) = if detect_lang() == "fr" {
                ("Afficher la fenêtre", "Quitter")
            } else {
                ("Show window", "Quit")
            };
            let show = MenuItem::with_id(app, "show", show_label, true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", quit_label, true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            // ─── Activate app so first tray click works ─────
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.hide();
            }

            if let Some(tray) = app.tray_by_id("main") {
                let _ = tray.set_menu(Some(menu));
                tray.on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    _ => {}
                });
            } else {
                tracing::warn!(target: "jamodio::tray", "no tray icon found");
            }

            // ─── Deep link handler (jamodio://) ────────────
            // Quand l'user clique "Lancer" dans le browser jamodio.com et que
            // l'agent tourne déjà, l'URL `jamodio://launch` arrive ici.
            // On focus la fenêtre principale au lieu de l'ignorer.
            let app_handle_dl = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                let urls: Vec<String> = event.urls().iter().map(|u| u.to_string()).collect();
                tracing::info!(
                    target: "jamodio::lifecycle",
                    urls = ?urls,
                    "deep-link received — focusing main window"
                );
                if let Some(win) = app_handle_dl.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.unminimize();
                    let _ = win.set_focus();
                }
            });

            // ─── Enable auto-start ──────────────────────────
            let autostart = app.autolaunch();
            if !autostart.is_enabled().unwrap_or(false) {
                let _ = autostart.enable();
                tracing::info!(target: "jamodio::lifecycle", "autostart enabled");
            }

            // ─── Spawn WS server (audio pipeline) ───────────
            let mixer = Arc::new(Mutex::new(AudioMixer::new()));
            let mut pipeline = PipelineState::new(mixer);
            // Sprint INSERT (S1.3) — lance le scan AU en background dès le
            // boot. Le scan complet prend ~13s, mais l'utilisateur n'ouvre
            // pas le menu FX avant plusieurs secondes → cache prêt à temps.
            pipeline.spawn_plugin_scan();
            // Sprint S2.7 — Crée un port MIDI virtuel "Jamodio Virtual MIDI"
            // dans CoreMIDI. Apparaît comme destination dans toutes les apps
            // MIDI macOS (Logic, Ableton, GarageBand…). Évite à l'user
            // d'avoir à configurer manuellement l'IAC Driver pour utiliser
            // les plugins instruments sans clavier USB physique.
            pipeline.spawn_virtual_midi();
            let pipeline = Arc::new(tokio::sync::Mutex::new(pipeline));
            let ws_handle = WsServerHandle::new(pipeline);
            let ws_handle_for_server = ws_handle.clone();

            tauri::async_runtime::spawn(async move {
                ws_server::start(ws_handle_for_server).await;
            });

            // ─── Vérification d'update au boot ──────────────
            // Endpoint + pubkey configurés dans tauri.conf.json (`updater` bloc).
            // Délai de 5 s pour laisser le démarrage se finir avant de hit
            // GitHub releases. Fire-and-forget : si l'install échoue ou si
            // l'user n'a pas le réseau, on log et on n'embête pas l'utilisateur.
            // Passe le ws_handle pour pouvoir broadcaster Shutdown avant restart.
            let app_handle = app.handle().clone();
            let ws_handle_for_update = ws_handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                check_for_update(app_handle, ws_handle_for_update).await;
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("Failed to run Jamodio Audio Engine");
}
