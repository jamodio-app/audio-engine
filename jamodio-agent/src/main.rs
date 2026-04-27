//! Jamodio Desktop Audio Agent

mod audio;
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

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            eprintln!("[Jamodio] Audio Engine v0.1.2");

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
                eprintln!("[TRAY] No tray icon found");
            }


            // ─── Deep link handler (jamodio://) ────────────
            app.deep_link().on_open_url(|_event| {});

            // ─── Enable auto-start ──────────────────────────
            let autostart = app.autolaunch();
            if !autostart.is_enabled().unwrap_or(false) {
                let _ = autostart.enable();
                eprintln!("[Jamodio] Autostart enabled");
            }

            // ─── Spawn WS server (audio pipeline) ───────────
            let mixer = Arc::new(Mutex::new(AudioMixer::new()));
            let pipeline = Arc::new(tokio::sync::Mutex::new(PipelineState::new(mixer)));

            tauri::async_runtime::spawn(async move {
                ws_server::start(pipeline).await;
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
