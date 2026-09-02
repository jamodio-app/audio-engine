//! Jamodio Desktop Audio Agent

// Pas de console CMD au démarrage en release Windows. En dev (debug_assertions),
// la console reste pour faciliter le diag (eprintln, panics visibles).
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod audio;
mod logging;
mod pipeline;
mod plugin_scan;
#[cfg(target_os = "windows")]
mod tray_promote;
mod ws_server;

use jamodio_audio_core::mixer::mixer::AudioMixer;
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

/// État courant de l'autostart (lu depuis l'OS : plist LaunchAgent sur macOS,
/// clé Run du registre sur Windows). Alimente la case à cocher de la fenêtre
/// agent à l'ouverture. `false` en cas d'erreur de lecture (fail-safe visuel).
#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Active/désactive l'autostart selon le choix explicite de l'utilisateur
/// (toggle fenêtre agent). Renvoie l'état RÉEL relu après l'opération pour que
/// l'UI reflète la vérité OS (jamais un faux « activé » si l'enregistrement a
/// échoué). Erreur explicite propagée au front — pas de fallback silencieux.
#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let al = app.autolaunch();
    let res = if enabled { al.enable() } else { al.disable() };
    res.map_err(|e| format!("autostart toggle failed: {e}"))?;
    let now = al.is_enabled().unwrap_or(enabled);
    tracing::info!(target: "jamodio::lifecycle", requested = enabled, effective = now, "autostart set by user");
    Ok(now)
}

/// Décision d'autostart au boot : appliquer le défaut (ON) une seule fois, ou
/// respecter l'état existant. Isolé en fonction pure (I/O = simple `exists()`)
/// pour être testable sans Tauri.
#[derive(Debug, PartialEq, Eq)]
enum AutostartBoot {
    /// Premier lancement (marqueur absent) → appliquer le défaut ON.
    EnableDefault,
    /// Déjà initialisé → NE PAS re-forcer, respecter le choix utilisateur / OS.
    Respect,
}

fn autostart_boot_action(marker: &std::path::Path) -> AutostartBoot {
    if marker.exists() {
        AutostartBoot::Respect
    } else {
        AutostartBoot::EnableDefault
    }
}

/// Chemin du marqueur « défaut autostart déjà appliqué » dans le dossier de
/// config de l'app. Sa présence garantit qu'on ne ré-impose plus jamais
/// `enable()` au boot (on respecte le choix utilisateur). `None` si le dossier
/// de config est indisponible (cas dégradé → on ne force rien).
fn first_run_marker_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("autostart-initialized"))
}

/// macOS (0.5.11-10) — opt-out de la « réouverture des apps à la reconnexion ».
/// macOS relance au login toute app Regular (Dock) qui tournait au reboot,
/// INDÉPENDAMMENT du LaunchAgent → décocher notre autostart n'empêchait pas le
/// redémarrage sur Mac (le LaunchAgent était bien retiré, mais loginwindow
/// restaurait l'app). `[NSApp disableRelaunchOnLogin]` désactive cette
/// restauration pour la session courante → le LaunchAgent (= notre toggle)
/// redevient l'UNIQUE mécanisme de démarrage. Appelé à chaque boot (l'effet vaut
/// pour le prochain login). No-op silencieux si l'app AppKit n'est pas prête.
#[cfg(target_os = "macos")]
fn disable_macos_relaunch_on_login() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    // SÉCURITÉ : un seul message Objective-C sans argument ni retour. `NSApp`
    // (sharedApplication) est instancié dès l'init AppKit du process Tauri.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            let _: () = msg_send![app, disableRelaunchOnLogin];
            tracing::info!(
                target: "jamodio::lifecycle",
                "macOS: disableRelaunchOnLogin — restauration de session désactivée (autostart = LaunchAgent seul)"
            );
        }
    }
}

/// Quitte proprement l'agent : informe les browsers connectés
/// (`Shutdown { reason }`) puis termine le process après un court délai
/// (le temps que la frame WS parte). Utilisé par le bouton « Quitter
/// l'agent » de la fenêtre ET par le menu tray — un seul chemin de sortie.
fn graceful_quit(app: &tauri::AppHandle, reason: &'static str) {
    tracing::info!(target: "jamodio::lifecycle", reason, "quit requested");
    if let Some(ws) = app.try_state::<WsServerHandle>() {
        ws.broadcast_shutdown(reason);
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        app.exit(0);
    });
}

/// Bouton « Quitter l'agent » de la fenêtre agent (ui/index.html).
/// Filet indispensable sur Windows quand l'icône tray est masquée par
/// l'OS (overflow Win11) — cf. tray_promote.rs pour la promotion d'icône.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    graceful_quit(&app, "quit");
}

/// Vérifie une éventuelle update via l'endpoint configuré dans
/// tauri.conf.json (`updater` bloc), télécharge + installe + restart si
/// dispo. Diffuse `Shutdown{reason:"update"}` à tous les clients WS AVANT
/// `app.restart()` pour que le browser puisse afficher un toast et
/// préparer un fallback gracieux (au lieu de voir un TCP close brutal
/// + watchdog timeout 3 s).
pub(crate) async fn check_for_update(app: tauri::AppHandle, ws_handle: WsServerHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(target: "jamodio::updater", error = %e, "updater unavailable");
            // Le gate d'entrée attend un dénouement : pas de fallback silencieux.
            ws_handle.broadcast_update_progress(ws_server::UpdateProgressEvent {
                phase: "error",
                downloaded: None,
                total: None,
                message: Some("updater-unavailable".to_string()),
            });
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
            let mut last_emit: u64 = 0;
            let download_result = update
                .download_and_install(
                    |chunk_length, content_length| {
                        downloaded += chunk_length as u64;
                        // Throttle ~256 Ko : le callback fire par chunk (des
                        // centaines) — on borne le débit WS, la barre web rattrape.
                        if downloaded - last_emit >= 256 * 1024 {
                            last_emit = downloaded;
                            ws_handle.broadcast_update_progress(ws_server::UpdateProgressEvent {
                                phase: "downloading",
                                downloaded: Some(downloaded),
                                total: content_length,
                                message: None,
                            });
                        }
                        if let Some(total) = content_length {
                            tracing::debug!(
                                target: "jamodio::updater",
                                progress = format!("{}/{}", downloaded, total),
                                "download progress"
                            );
                        }
                    },
                    || {
                        tracing::info!(target: "jamodio::updater", "download finished, installing");
                        ws_handle.broadcast_update_progress(ws_server::UpdateProgressEvent {
                            phase: "installing",
                            downloaded: None,
                            total: None,
                            message: None,
                        });
                    },
                )
                .await;

            match download_result {
                Ok(_) => {
                    tracing::info!(target: "jamodio::updater", "update installed — broadcasting Shutdown then restart");
                    // Phase finale : la barre web passe à « redémarrage » avant
                    // que la WS ne tombe.
                    ws_handle.broadcast_update_progress(ws_server::UpdateProgressEvent {
                        phase: "restarting",
                        downloaded: None,
                        total: None,
                        message: None,
                    });
                    // Broadcast aux clients WS connectés AVANT restart.
                    // ws_server::handle_connection sleep 200ms après l'envoi
                    // pour laisser le browser recevoir + traiter.
                    ws_handle.broadcast_shutdown("update");
                    // Petit délai supplémentaire ici aussi pour la marge
                    // (les broadcasts tokio sont async, le restart aussi).
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    app.restart();
                }
                Err(e) => {
                    tracing::error!(
                        target: "jamodio::updater",
                        error = %e,
                        "download/install failed"
                    );
                    // Pas de fallback silencieux : le web sait que la MàJ a échoué
                    // (la modale d'entrée propose de réessayer / DL manuel).
                    ws_handle.broadcast_update_progress(ws_server::UpdateProgressEvent {
                        phase: "error",
                        downloaded: None,
                        total: None,
                        message: Some(format!("{e}")),
                    });
                }
            }
        }
        Ok(None) => {
            tracing::info!(
                target: "jamodio::updater",
                version = env!("CARGO_PKG_VERSION"),
                "already on latest version"
            );
            // L'updater ne voit pas de MàJ (endpoint `latest.json`). Si le gate
            // avait déclenché ceci, on le débloque (la modale affiche « aucune
            // MàJ trouvée » plutôt que de tourner indéfiniment).
            ws_handle.broadcast_update_progress(ws_server::UpdateProgressEvent {
                phase: "error",
                downloaded: None,
                total: None,
                message: Some("no-update".to_string()),
            });
        }
        Err(e) => {
            tracing::warn!(
                target: "jamodio::updater",
                error = %e,
                "update check failed (offline ? endpoint down ?)"
            );
            ws_handle.broadcast_update_progress(ws_server::UpdateProgressEvent {
                phase: "error",
                downloaded: None,
                total: None,
                message: Some("check-failed".to_string()),
            });
        }
    }
}

fn main() {
    // Mode worker de scan plugins (0.5.9-2, PLAN-PLUGIN-SCAN-OOP) : process
    // enfant JETABLE spawné par le coordinateur pour instancier les plugins
    // hors du process agent. Court-circuit AVANT tout le reste — pas de
    // logging fichier, pas de lock single-instance, pas de Tauri, pas de
    // port 9876, pas de tray. Ne retourne jamais.
    if std::env::args().any(|a| a == "--plugin-scan-worker") {
        plugin_scan::worker::run();
    }

    // Relance « attendue » (bouton « Redémarrer l'agent » → ws_server::
    // spawn_awaited_relaunch). On a été spawné DÉTACHÉ par l'ancien process
    // pendant qu'il s'éteignait. On attend qu'il soit mort — donc que le verrou
    // tauri-plugin-single-instance ET le port WS 9876 soient libérés — AVANT
    // d'initialiser Tauri. Sans ce délai, le plugin single-instance nous
    // tuerait comme « 2e instance » et il ne resterait AUCUN agent (race
    // classique de app.restart(), à l'origine du bug Windows du 26/06). Doit
    // impérativement précéder `tauri::Builder`.
    if std::env::args().any(|a| a == "--awaited-relaunch") {
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }

    // Init tracing AVANT tout le reste : tous les eprintln! ont été migrés
    // vers tracing::{info,warn,error,debug,trace}, et on veut capturer même
    // les events pendant le setup Tauri. Le guard doit rester vivant : on le
    // bind à _log_guard au scope de main() (drop = fin du process = OK).
    let _log_guard = logging::init();

    // Filet de diagnostic crash (0.5.11) : un panic Rust part par DÉFAUT sur
    // stderr — jeté sur une app GUI Windows → invisible dans `agent.log` (donc
    // absent du bug-report support). On installe un hook qui route le panic
    // (message + localisation + backtrace) vers `tracing` → `agent.log` → bundle.
    // On CHAÎNE le hook par défaut : on ne change pas la stratégie d'unwind.
    //
    // Portée : ceci capture les panics RUST uniquement. Les crashs NATIFS
    // (corruption de tas / access-violation SEH, ex. driver ASIO) passent à côté
    // et relèvent d'un `SetUnhandledExceptionFilter`/minidump — à ajouter en
    // session Windows (cf. internal-docs/plans/PLAN-ASIO-OUTPUT-PAIR-2026-07.md,
    // §« Capture de crash natif »).
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<inconnue>".to_string());
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<payload non-string>".to_string());
            let backtrace = std::backtrace::Backtrace::force_capture();
            tracing::error!(
                target: "jamodio::panic",
                location = %location,
                message = %message,
                backtrace = %backtrace,
                "PANIC Rust — thread en cours d'unwind"
            );
            default_hook(info);
        }));
    }

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
        .invoke_handler(tauri::generate_handler![
            open_log_dir,
            get_log_dir,
            get_version,
            get_autostart,
            set_autostart,
            quit_app
        ])
        .setup(|app| {
            tracing::info!(target: "jamodio::lifecycle", version = env!("CARGO_PKG_VERSION"), "setup phase");
            // Marqueur de build (robustesse ASIO) — permet d'identifier SANS
            // AMBIGUÏTÉ quel binaire tourne, la version numérique seule ne
            // distinguant pas debug/patché de la release du même numéro. Grep :
            // « build ASIO robustesse ». Mettre à jour la liste des lots au fil.
            tracing::info!(
                target: "jamodio::lifecycle",
                policy = "48kHz-only + ASIO-only(win) + hard-stop, no-resampler",
                profile = if cfg!(debug_assertions) { "debug" } else { "release" },
                "build ASIO robustesse : 48 kHz natif obligatoire (décision 04/08)"
            );

            // ─── Dump devices CPAL au démarrage (diagnostic) ─────
            audio::device::log_devices();

            // ─── P2.0 — spike host ASIO duplex (Windows, opt-in JAMODIO_ASIO_PROBE) ───
            // 0.5.4-15 : retour à l'opt-in par var d'env après le BSOD (la 0.5.4-14
            // écrivait dans les buffers ASIO → crash kernel). No-op sans la var.
            // Diagnostic isolé (comptage seul), jamais câblé au pipeline.
            #[cfg(target_os = "windows")]
            audio::asio_probe::spawn_probe_at_startup();

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
                // tray.png est désormais un BADGE COULEUR (disque jaune marque +
                // logo sombre, cf. icons/src/jamodio-tray.svg), visible sur Mac
                // ET Windows. On NE le remplace plus par 32x32.png au runtime sur
                // Windows : cet override datait du tray template monochrome
                // (invisible sur barre sombre) et provoquait désormais un flash
                // (badge jaune chargé par la config → remplacé par la tuile
                // noire). Une seule icône de marque partout, pas de flash.
                let _ = tray.set_menu(Some(menu));
                tray.on_menu_event(|app, event| match event.id.as_ref() {
                    // Même chemin de sortie que le bouton de la fenêtre :
                    // broadcast Shutdown aux browsers AVANT de quitter
                    // (l'ancien app.exit(0) direct coupait la WS sans prévenir).
                    "quit" => graceful_quit(app, "quit"),
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

            // Windows 11 masque les nouvelles icônes tray dans l'overflow
            // « ^ » — on promeut la nôtre (IsPromoted=1) au premier run,
            // sans jamais écraser un choix utilisateur. Cf. tray_promote.rs.
            #[cfg(target_os = "windows")]
            tray_promote::spawn_promotion();

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

            // ─── macOS : autostart = LaunchAgent SEUL (pas la restauration OS) ──
            #[cfg(target_os = "macos")]
            disable_macos_relaunch_on_login();

            // ─── Autostart : défaut ON au 1ER LANCEMENT, puis RESPECTÉ ──────
            // Racine (0.5.11-10) : on NE ré-force PLUS `enable()` à chaque boot.
            // L'ancien ré-enable inconditionnel écrasait tout choix utilisateur
            // (désactivation via le toggle de cette fenêtre ou via l'OS) — viol
            // direct de « si l'utilisateur choisit X, il a X ». On applique le
            // défaut (ON) UNE seule fois, tracé par un marqueur dans
            // app_config_dir ; ensuite l'état OS fait foi. Le toggle
            // `set_autostart` (fenêtre agent) pilote le reste.
            match first_run_marker_path(app.handle()) {
                Some(marker) => match autostart_boot_action(&marker) {
                    AutostartBoot::EnableDefault => {
                        match app.autolaunch().enable() {
                            Ok(()) => tracing::info!(
                                target: "jamodio::lifecycle",
                                "autostart enabled (défaut 1er lancement)"
                            ),
                            Err(e) => tracing::warn!(
                                target: "jamodio::lifecycle", error = %e,
                                "autostart: enable initial a échoué"
                            ),
                        }
                        // Marqueur écrit MÊME si enable a échoué : on ne re-tente
                        // pas à chaque boot (ce serait re-forcer, précisément ce
                        // qu'on supprime). L'utilisateur garde le toggle.
                        if let Some(parent) = marker.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if let Err(e) = std::fs::write(&marker, b"1") {
                            tracing::warn!(
                                target: "jamodio::lifecycle", error = %e,
                                "autostart: écriture du marqueur 1er lancement impossible"
                            );
                        }
                    }
                    AutostartBoot::Respect => tracing::info!(
                        target: "jamodio::lifecycle",
                        enabled = app.autolaunch().is_enabled().unwrap_or(false),
                        "autostart: état existant respecté (pas le 1er lancement)"
                    ),
                },
                None => tracing::warn!(
                    target: "jamodio::lifecycle",
                    "autostart: app_config_dir indisponible — marqueur ignoré, pas de ré-enable forcé"
                ),
            }

            // ─── Spawn WS server (audio pipeline) ───────────
            let mixer = Arc::new(AudioMixer::new());
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
            // Injecte le AppHandle pour que le message browser `Restart`
            // (bouton « Relancer mon agent ») puisse déclencher check_for_update
            // + app.restart() depuis la receive loop WS.
            ws_handle.set_app_handle(app.handle().clone());
            // State managé : permet à graceful_quit (commande quit_app +
            // menu tray) de broadcaster Shutdown aux browsers connectés.
            app.manage(ws_handle.clone());
            let ws_handle_for_server = ws_handle.clone();

            tauri::async_runtime::spawn(async move {
                ws_server::start(ws_handle_for_server).await;
            });

            // ─── PAS de MàJ automatique au boot (Lot 2, 0.5.11-9) ───────────
            // Retrait volontaire de l'install auto au démarrage : elle tournait
            // en douce ~5 s après le boot (autostart au login), pouvait afficher
            // une fenêtre d'installeur PENDANT le chargement des drivers ASIO, et
            // n'informait pas l'utilisateur. La MàJ est désormais OBLIGATOIRE mais
            // AU MOMENT DE L'USAGE : le web bloque l'entrée en studio si l'agent
            // est en retard et déclenche `check_for_update` via le message
            // `restart` (barre de progression + garde-fou session active). Voir
            // `trigger_restart` (ws_server) et la modale d'entrée côté web.

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        // `build` + `run` (plutôt que `run(generate_context!())`) pour pouvoir
        // intercepter les RunEvent — notamment `Reopen` (macOS).
        .build(tauri::generate_context!())
        .expect("Failed to run Jamodio Audio Engine")
        .run(|app_handle, event| {
            // macOS : clic sur l'icône Dock → `applicationShouldHandleReopen`.
            // Option B : la fenêtre principale est `visible:false` + masquée à
            // la fermeture ; sans ce handler, cliquer l'icône Dock ne faisait
            // RIEN (porte d'entrée morte). On (re)montre + focus la fenêtre
            // d'infos de l'Agent — entrée fiable, jamais masquée par l'encoche.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = &event {
                if let Some(win) = app_handle.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.unminimize();
                    let _ = win.set_focus();
                }
            }
            // Évite les warnings unused sur les autres OS / variantes.
            let _ = (app_handle, &event);
        });
}

#[cfg(test)]
mod autostart_tests {
    use super::{autostart_boot_action, AutostartBoot};

    /// Le défaut ON ne s'applique QU'au premier lancement : marqueur absent →
    /// EnableDefault ; une fois posé → Respect (on ne re-force plus jamais,
    /// donc un utilisateur qui a désactivé garde son choix au reboot).
    #[test]
    fn default_on_first_run_then_respect() {
        let dir = std::env::temp_dir().join(format!("jamodio-autostart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("autostart-initialized");

        // 1er lancement : marqueur absent → on applique le défaut.
        assert_eq!(autostart_boot_action(&marker), AutostartBoot::EnableDefault);

        // Après pose du marqueur : on respecte l'état existant.
        std::fs::write(&marker, b"1").unwrap();
        assert_eq!(autostart_boot_action(&marker), AutostartBoot::Respect);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
