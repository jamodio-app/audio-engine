//! Jamodio Desktop Audio Agent

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
/// dispo. Tout est silencieux — pas de prompt, pas de friction. Si l'user
/// quitte avant la fin, l'update reprendra au prochain démarrage.
async fn check_for_update(app: tauri::AppHandle) {
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
                    tracing::info!(target: "jamodio::updater", "update installed — restarting");
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
            // Utile pour voir ce que CPAL expose réellement sur le poste :
            // nom exact, canaux par défaut, device par défaut. Aide à diagnostiquer
            // les cas où la sélection UI affiche "des chiffres" ou un nom inattendu.
            audio::device::log_devices();

            // ─── Attach menu to config-based tray icon ──────
            let show = MenuItem::with_id(app, "show", "Afficher la fenêtre", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
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
            app.deep_link().on_open_url(|_event| {});

            // ─── Enable auto-start ──────────────────────────
            let autostart = app.autolaunch();
            if !autostart.is_enabled().unwrap_or(false) {
                let _ = autostart.enable();
                tracing::info!(target: "jamodio::lifecycle", "autostart enabled");
            }

            // ─── Spawn WS server (audio pipeline) ───────────
            let mixer = Arc::new(Mutex::new(AudioMixer::new()));
            let pipeline = Arc::new(tokio::sync::Mutex::new(PipelineState::new(mixer)));

            tauri::async_runtime::spawn(async move {
                ws_server::start(pipeline).await;
            });

            // ─── Vérification d'update au boot ──────────────
            // Endpoint + pubkey configurés dans tauri.conf.json (`updater` bloc).
            // Délai de 5 s pour laisser le démarrage se finir avant de hit
            // GitHub releases. Fire-and-forget : si l'install échoue ou si
            // l'user n'a pas le réseau, on log et on n'embête pas l'utilisateur.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                check_for_update(app_handle).await;
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
