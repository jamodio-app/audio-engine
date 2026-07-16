use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use base64::Engine;
use jamodio_audio_core::protocol::{
    AgentMessage, AgentState, BrowserMessage, PeerPerf, PipelineLatency, PluginPerf,
    RecordStemSpec, RecordedFileWire, StreamLevel, PROTOCOL_VERSION,
};
use std::sync::OnceLock;
use jamodio_audio_core::record::StemSpec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc as tokio_mpsc};

use crate::audio::device;
use crate::pipeline::{PipelineState, ProducerNetStats};

/// Timeout sur les locks `pipeline.lock().await` dans les handlers heartbeat.
/// Si dépassé, on répond `Error{overloaded}` au browser au lieu de bloquer
/// le watchdog browser (qui kill la WS à 3.47 s). Permet de survivre à un
/// pic CPU local sans perdre la session.
const LOCK_TIMEOUT_MS: u64 = 200;

/// Suffixe du scope Vercel de l'équipe Jamodio (previews). Seul ce compte peut
/// déployer sous ce suffixe → un projet tiers `jamodio-*.vercel.app` d'un autre
/// scope est rejeté. ⚠️ Doit rester synchro avec le SFU (server/sfu.js
/// VERCEL_PREVIEW_RE). Cf. review pré-BETA 2026-07-12 (C5).
const VERCEL_TEAM_SUFFIX: &str = "-bengo82-9540s-projects.vercel.app";

/// Vérifie l'origin de la requête WS upgrade. On accepte uniquement :
///   - https://jamodio.com (prod)
///   - https://jamodio-<hash|branch>-<scope>.vercel.app (previews DU scope Jamodio)
///   - http://localhost:* / http://127.0.0.1:* (dev local + browser-side dev)
///   - tauri://localhost ou http://tauri.localhost (UI WEBVIEW INTERNE
///     de l'agent — Tauri 2 sert sa webview sous ces schemes selon l'OS).
///     Cf. is_internal_client() qui bypass la single-client policy pour ces
///     origins, car la webview interne est un client légitime en plus du
///     browser jamodio.com (lecture-seule des stats, pas de race possible).
///   - file:// (cas webview embedded historique)
///   - Origin absent : toléré en DEBUG (tests CLI), REFUSÉ en release (C5)
///
/// Empêche une page web random sur localhost:1234 de piloter l'agent
/// silencieusement.
fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        // Origin absent : un navigateur envoie TOUJOURS un en-tête Origin ;
        // seul un client non-browser (test CLI) ou un process natif local peut
        // l'omettre. En RELEASE on REFUSE (sinon un malware local se ferait
        // admettre sans condition en omettant l'Origin) ; en debug on tolère
        // pour les tests. Cf. review pré-BETA 2026-07-12 (C5).
        return cfg!(debug_assertions);
    };
    // Origins de PRODUCTION (build release ET debug).
    if origin == "https://jamodio.com"
        || origin == "https://www.jamodio.com"
        // Previews Vercel : on épingle le SCOPE de l'équipe Jamodio. Un
        // `ends_with(".vercel.app")` — ou même `starts_with("https://jamodio")`
        // — laisserait n'importe qui enregistrer `jamodio-x.vercel.app` (gratuit)
        // et piloter l'agent en drive-by. Les URLs de preview sont
        // `jamodio-<hash|git-branch>-<scope>.vercel.app` ; seul VERCEL_TEAM_SUFFIX
        // (le scope de l'équipe) n'est pas usurpable. ⚠️ Doit rester synchro avec
        // le SFU (server/sfu.js VERCEL_PREVIEW_RE).
        || (origin.starts_with("https://jamodio-") && origin.ends_with(VERCEL_TEAM_SUFFIX))
        || is_internal_client_origin(origin)
        || origin == "file://"
    {
        return true;
    }
    // Origins de DEV uniquement (serveur web local) : EXCLUS du build release.
    // En prod, une page sur http://localhost:* (autre app locale, malware,
    // iframe vers un serveur local) pourrait sinon piloter l'agent
    // (StartCapture, GetLogsArchive, LoadInstrumentPlugin). Les beta-testeurs
    // passent par jamodio.com → aucun impact.
    #[cfg(debug_assertions)]
    if origin.starts_with("http://localhost:") || origin.starts_with("http://127.0.0.1:") {
        return true;
    }
    false
}

/// Vrai si l'origin correspond à la webview interne de l'agent Tauri.
/// Ces clients sont en LECTURE SEULE (UI dashboard) et bypass la
/// single-client policy : la webview reste connectée même si le browser
/// jamodio.com l'est aussi. Tauri 2 utilise des schemes différents selon
/// l'OS (macOS WKWebView `tauri://localhost`, Windows WebView2
/// `http://tauri.localhost`). Comparaisons EXACTES : un `starts_with`
/// laisserait passer `tauri://localhost.attacker` / `http://tauri.localhost.evil`
/// → bypass de la single-client policy par un client local forgeant l'Origin.
fn is_internal_client_origin(origin: &str) -> bool {
    origin == "tauri://localhost" || origin == "http://tauri.localhost"
}

/// Construit un `AgentMessage::Status` avec la version + OS + arch de l'agent.
/// Centralisé ici pour ne pas dupliquer ces 3 champs à chaque site.
fn make_status(state: AgentState) -> AgentMessage {
    AgentMessage::Status {
        state,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
    }
}

/// Construit le `Hello` envoyé en premier à chaque WS upgrade.
fn make_hello() -> AgentMessage {
    AgentMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        capabilities: vec![],
    }
}

/// Handle partagé pour le serveur WS. Permet :
///   - Single-client policy avec **kick automatique** du précédent (v0.4.3) :
///     `client_active` reste un AtomicBool de monitoring, mais le slot est
///     géré via `active_client_killer` qui permet d'évincer le client zombie
///     au lieu de rejeter le nouveau. Résout le pattern "agent déjà utilisé"
///     observé en prod 27/05 (WS browser half-open après tab close brutal,
///     slot agent jamais libéré jusqu'au quit+relance manuel).
///   - Broadcast d'événements globaux (Shutdown sur auto-update) à tous les
///     clients connectés via `shutdown_tx` (tokio broadcast channel).
#[derive(Clone)]
pub struct WsServerHandle {
    pipeline: Arc<tokio::sync::Mutex<PipelineState>>,
    /// True quand une WS browser EXTERNE est connectée et tient le slot.
    /// Sert au monitoring (pas à la décision d'admission). Cf. `active_client_killer`.
    client_active: Arc<AtomicBool>,
    /// v0.4.3 — Sender oneshot du client externe actuellement actif. Le nouveau
    /// client envoie via ce channel pour "kick" le précédent, qui voit sa
    /// receive loop break (cf. `tokio::select!` ws_rx + killer_rx). Une fois
    /// le slot libéré (cleanup terminé), le nouveau le reprend.
    /// `parking_lot::Mutex` car contention minimale (au plus 1×/connexion).
    active_client_killer:
        Arc<parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    /// Broadcast channel pour notifier tous les clients connectés (1 seul en
    /// pratique avec single-client) qu'un shutdown est imminent (auto-update).
    /// Capacité 4 : largement suffisant pour les ~quelques events de cycle de vie.
    shutdown_tx: broadcast::Sender<&'static str>,
    /// Handle Tauri — permet de déclencher le flux d'auto-update + restart à la
    /// demande (message browser `Restart`, bouton « Relancer mon agent »).
    /// `OnceLock` car le handle n'est connu qu'au `setup()` (après `new`), mais
    /// on veut garder `new` testable sans environnement Tauri.
    app: Arc<OnceLock<tauri::AppHandle>>,
}

impl WsServerHandle {
    pub fn new(pipeline: Arc<tokio::sync::Mutex<PipelineState>>) -> Self {
        let (shutdown_tx, _rx) = broadcast::channel::<&'static str>(4);
        Self {
            pipeline,
            client_active: Arc::new(AtomicBool::new(false)),
            active_client_killer: Arc::new(parking_lot::Mutex::new(None)),
            shutdown_tx,
            app: Arc::new(OnceLock::new()),
        }
    }

    /// Injecte le `AppHandle` Tauri (appelé une fois au `setup()`). Idempotent :
    /// un 2e appel est ignoré (OnceLock). Requis avant `trigger_restart`.
    pub fn set_app_handle(&self, app: tauri::AppHandle) {
        let _ = self.app.set(app);
    }

    /// Diffuse un message Shutdown à tous les clients WS actuellement connectés.
    /// À appeler AVANT `app.restart()` (auto-update) pour donner au browser
    /// le temps de basculer en mode "agent restart imminent".
    pub fn broadcast_shutdown(&self, reason: &'static str) {
        // send() retourne Err si aucun receiver — pas grave, on s'en fiche.
        let _ = self.shutdown_tx.send(reason);
    }

    /// Déclenche le redémarrage de l'agent à la demande du browser (bouton
    /// « Relancer mon agent »). Réutilise exactement le flux d'auto-update du
    /// boot : `check_for_update` télécharge + installe la version dispo,
    /// broadcaste `Shutdown`, puis `app.restart()`. Fire-and-forget (spawn) pour
    /// ne pas bloquer la receive loop WS pendant le download. No-op + warn si le
    /// `AppHandle` n'a pas été injecté (ne devrait pas arriver en prod).
    pub fn trigger_restart(&self) {
        let Some(app) = self.app.get().cloned() else {
            tracing::warn!(target: "jamodio::ws", "Restart requested but AppHandle not set — ignoring");
            return;
        };
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            crate::check_for_update(app, me).await;
        });
    }

    /// Redémarrage IMMÉDIAT de l'agent, SANS flux d'update (message browser
    /// `RelaunchNow`, bouton « Redémarrer l'agent » du badge WASAPI). Un boot
    /// frais re-sonde le host CPAL → ASIO détecté si l'interface a été branchée
    /// après le démarrage. Contrairement à `trigger_restart` (qui passe par
    /// `check_for_update` et ne relance QUE si une update existe), celui-ci
    /// relance toujours. Broadcaste `Shutdown` aux browsers AVANT de couper,
    /// avec un court délai (le temps que la frame WS parte), puis `app.restart()`.
    pub fn trigger_relaunch(&self) {
        let Some(app) = self.app.get().cloned() else {
            tracing::warn!(target: "jamodio::ws", "Relaunch requested but AppHandle not set — ignoring");
            return;
        };
        tracing::info!(target: "jamodio::ws", "browser requested agent relaunch (WASAPI→ASIO refresh)");
        self.broadcast_shutdown("relaunch");
        let exe = std::env::current_exe();
        tauri::async_runtime::spawn(async move {
            // Laisse partir la frame Shutdown vers les browsers.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;

            // On N'UTILISE PAS `app.restart()` : il relance le nouveau process
            // PENDANT que l'ancien tient encore le verrou tauri-plugin-single-
            // instance → le nouveau est vu comme « 2e instance » et se tue
            // aussitôt → plus AUCUN agent (symptôme Windows confirmé par logs
            // 2026-06-26 : pastille en boucle, WS jamais de retour). À la place
            // on spawne un relanceur DÉTACHÉ marqué `--awaited-relaunch` : il
            // attend (cf. main.rs) que CE process meure — donc que le verrou
            // single-instance ET le port 9876 soient libérés — avant de démarrer.
            match exe {
                Ok(path) => {
                    if let Err(e) = spawn_awaited_relaunch(&path) {
                        tracing::error!(target: "jamodio::ws", error = %e, "spawn du relanceur échoué");
                    } else {
                        tracing::info!(target: "jamodio::ws", "relanceur détaché spawné — sortie du process courant");
                    }
                }
                Err(e) => tracing::error!(
                    target: "jamodio::ws", error = %e,
                    "current_exe() indisponible — relance impossible"
                ),
            }
            // Laisse le relanceur démarrer (il dort), puis on quitte proprement
            // pour libérer le verrou + le port.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            app.exit(0);
        });
    }
}

/// Spawne une nouvelle instance de l'agent en mode « relance attendue ». Le
/// nouveau process est DÉTACHÉ (survit à la mort du parent) et reçoit l'argument
/// `--awaited-relaunch` : au démarrage il attend ~2 s (cf. `main()`) que le
/// process courant meure et libère le verrou single-instance + le port WS 9876,
/// AVANT d'initialiser Tauri — sinon le plugin single-instance le tuerait comme
/// « 2e instance » (race classique de `app.restart()`).
fn spawn_awaited_relaunch(exe: &std::path::Path) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--awaited-relaunch");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS : pas de console héritée. CREATE_NEW_PROCESS_GROUP :
        // le child ne reçoit pas les signaux du parent qui s'éteint.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map(|_child| ())
}

/// Start the localhost WebSocket server on port 9876.
pub async fn start(handle: WsServerHandle) {
    let app = Router::new().route(
        "/",
        get(move |ws: WebSocketUpgrade, headers: HeaderMap, uri: Uri| {
            let handle = handle.clone();
            async move {
                // Origin check : rejet HTTP 403 si page web non-whitelisée
                let origin = headers
                    .get("origin")
                    .and_then(|h| h.to_str().ok());
                if !origin_allowed(origin) {
                    tracing::warn!(
                        target: "jamodio::ws",
                        origin = ?origin,
                        "WS upgrade rejected — origin not whitelisted"
                    );
                    return axum::http::Response::builder()
                        .status(403)
                        .body(axum::body::Body::from("forbidden origin"))
                        .unwrap();
                }
                if origin.is_none() {
                    tracing::warn!(
                        target: "jamodio::ws",
                        "WS upgrade with no Origin header (raw client?)"
                    );
                }
                // Le flag is_internal détermine si la connexion bypass la
                // single-client policy (UI Tauri webview = lecture seule).
                let is_internal = origin
                    .map(is_internal_client_origin)
                    .unwrap_or(false);
                // Read-only opt-in : `?op=logs` => connexion éphémère qui
                // sert uniquement GetLogsArchive, ne prend PAS le slot
                // single-client, ne déclenche PAS de cleanup (stop_all)
                // à la fermeture. Utilisé par le module Support browser
                // pour ne pas casser la WS du studio actif quand l'user
                // génère un bug-report. Cf. `handle_logs_connection`.
                let is_logs_only = uri
                    .query()
                    .map(|q| q.split('&').any(|kv| kv == "op=logs"))
                    .unwrap_or(false);
                if is_logs_only {
                    return ws
                        .on_upgrade(move |socket| handle_logs_connection(socket, handle))
                        .into_response();
                }
                ws.on_upgrade(move |socket| handle_connection(socket, handle, is_internal))
                    .into_response()
            }
        }),
    );

    // Bind du port 9876 avec SO_REUSEADDR. Raison (Windows, redémarrage agent
    // 26/06, confirmé par logs) : après la mort de l'ancien process, sa
    // connexion WS avec le browser reste ~30 s en TIME_WAIT → un bind classique
    // échoue avec WSAEADDRINUSE (os error 10048) tant que ce TIME_WAIT n'est pas
    // purgé. `tokio::TcpListener::bind` ne pose PAS SO_REUSEADDR sur Windows, on
    // passe donc par socket2. SO_REUSEADDR autorise le bind malgré le TIME_WAIT
    // → reconnexion immédiate après « Redémarrer l'agent ». Petit retry en filet
    // pour une race brève si l'ancien listener n'est pas encore tout à fait
    // fermé (≠ TIME_WAIT, que SO_REUSEADDR couvre déjà).
    let listener = {
        const MAX_ATTEMPTS: u32 = 5;
        let mut attempt: u32 = 0;
        loop {
            match bind_ws_listener() {
                Ok(l) => break l,
                Err(e) => {
                    attempt += 1;
                    if attempt >= MAX_ATTEMPTS {
                        tracing::error!(
                            target: "jamodio::ws",
                            error = %e,
                            attempts = attempt,
                            "bind 9876 impossible — abandon (autre instance ?)"
                        );
                        return;
                    }
                    tracing::warn!(
                        target: "jamodio::ws",
                        error = %e,
                        attempt,
                        "bind 9876 échoué — retry dans 300ms"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
        }
    };

    tracing::info!(target: "jamodio::ws", addr = "ws://localhost:9876", "listening");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(target: "jamodio::ws", error = %e, "axum serve terminated");
    }
}

/// Crée le listener TCP du serveur WS (127.0.0.1:9876) avec `SO_REUSEADDR`.
/// Indispensable pour re-binder immédiatement après un redémarrage de l'agent
/// malgré la connexion précédente encore en TIME_WAIT (sinon WSAEADDRINUSE sur
/// Windows). Passe par `socket2` car tokio ne pose pas SO_REUSEADDR sur Windows.
fn bind_ws_listener() -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let addr: std::net::SocketAddr = std::net::SocketAddr::from(([127, 0, 0, 1], 9876));
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?; // SO_REUSEADDR
    socket.set_nonblocking(true)?; // requis par TcpListener::from_std
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// v0.4.3 — Helper extrait pour traiter un Message WS unique. Retourne
/// `true` si on doit continuer la receive loop, `false` si on doit la
/// quitter (envoi sortant cassé). Partagé entre la branche `is_internal`
/// et la branche externe (qui ajoute en plus killer/watchdog).
async fn handle_one_message(
    msg: Message,
    handle: &WsServerHandle,
    out_tx: &tokio_mpsc::Sender<AgentMessage>,
) -> bool {
    let Message::Text(text) = msg else { return true };

    let browser_msg = match serde_json::from_str::<BrowserMessage>(&text) {
        Ok(m) => m,
        Err(e) => {
            // Tronque par CHARS, pas par octets : `&text[..120]` paniquerait
            // si l'octet 120 tombe au milieu d'un char multioctet (accents) —
            // ce panic tuerait la future handle_connection AVANT son cleanup
            // (slot/pipeline zombie).
            let truncated: String = text.chars().take(120).collect();
            tracing::warn!(
                target: "jamodio::ws",
                error = %e,
                payload = truncated,
                "invalid browser message"
            );
            let err = AgentMessage::error(
                format!("Invalid message: {} (parse error: {})", truncated, e),
            );
            let _ = out_tx.send(err).await;
            return true;
        }
    };

    // Restart : intercepté ici (pas dans handle_message) car il a besoin du
    // AppHandle porté par `handle`, pas seulement du pipeline. Fire-and-forget
    // côté trigger_restart → on rend la main immédiatement (la WS tombera quand
    // le Shutdown sera broadcasté puis app.restart()).
    if matches!(browser_msg, BrowserMessage::Restart) {
        tracing::info!(target: "jamodio::ws", "browser requested agent restart (update banner)");
        handle.trigger_restart();
        return true;
    }

    // RelaunchNow : redémarrage immédiat sans flux d'update (badge WASAPI →
    // bouton « Redémarrer l'agent »). Même raison que Restart : besoin de
    // l'AppHandle, fire-and-forget.
    if matches!(browser_msg, BrowserMessage::RelaunchNow) {
        handle.trigger_relaunch();
        return true;
    }

    let responses = handle_message(browser_msg, &handle.pipeline).await;
    for resp in responses {
        if out_tx.send(resp).await.is_err() {
            return false;
        }
    }
    true
}

async fn handle_connection(socket: WebSocket, handle: WsServerHandle, is_internal: bool) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // v0.4.3 — Single-client policy avec **kick du précédent à la promotion**
    // au lieu d'un rejet sec. Critique : on NE PROMEUT PAS la connexion au
    // moment du `on_upgrade` (= aucun kick sur simple open WS), seulement
    // à la réception du PREMIER BrowserMessage (typiquement HelloAck).
    // Justification :
    //   - agent-status.js émet des probes périodiques : ouvre WS, lit Hello,
    //     ferme. Aucun BrowserMessage envoyé. Si on kickait au connect, chaque
    //     probe killerait la session active du même browser !
    //   - groupe.js detectAgent() envoie HelloAck juste après réception Hello.
    //     C'est ce HelloAck qui sert de "preuve de session réelle" et
    //     déclenche la promotion + kick du précédent.
    //
    // Le slot `client_active` + `active_client_killer` sont donc pris en
    // charge dans le receive loop ci-dessous (cherche `// PROMOTION`).
    //
    // Les clients internes (UI Tauri webview) bypass tout slot management.
    let mut killer_rx: Option<tokio::sync::oneshot::Receiver<&'static str>> = None;
    let mut slot_taken = false;

    tracing::info!(target: "jamodio::ws", is_internal, "client connected");

    // Premier message : Hello (annonce le protocole + version).
    // Suivi du Status initial (legacy compat pour browsers v0.1.x qui
    // ignorent Hello et lisent uniquement Status).
    let hello = make_hello();
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&hello).unwrap()))
        .await;
    let status = make_status(AgentState::Idle);
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&status).unwrap()))
        .await;

    // S1.5 — Sync state au reconnect : si un plugin INSERT est déjà chargé
    // côté agent (= browser a fait reload de page mais l'agent vivait), on
    // push l'état pour que l'UI affiche directement [● bypass][nom][✕] au
    // lieu de "+ FX" trompeur. Le browser reçoit le même message que pour
    // un load fresh, plus rien à modifier côté handler.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let pl = handle.pipeline.lock().await;
        if let Some((info, bypass)) = pl.get_instrument_plugin_snapshot() {
            drop(pl);
            let resync = AgentMessage::InstrumentPluginLoaded {
                name: info.name,
                plugin_ref: info.plugin_ref,
                latency_samples: info.latency_samples,
                has_editor: info.has_editor,
                bypass,
            };
            let _ = ws_tx
                .send(Message::Text(serde_json::to_string(&resync).unwrap()))
                .await;
            tracing::info!(target: "jamodio::ws", "pushed instrument plugin state on connect");
        }
    }

    // Sync la SOURCE d'entrée (audio / MIDI) au (re)connect, comme on push déjà
    // l'état du plugin ci-dessus. Sans ça, le browser assume 'audio' alors que
    // l'agent peut être en MIDI (multi-onglet, rejoin « Basculer », reload page)
    // → le clavier MIDI (physique OU virtuel) restait indisponible jusqu'à un
    // re-toggle audio↔midi manuel (symptôme Ben 14/07). On envoie le même
    // message InputSourceChanged que pour un set explicite → aucun handler
    // browser à modifier.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let src = { handle.pipeline.lock().await.current_input_source() };
        let (src_str, dev_id, dev_name) = match &src {
            crate::pipeline::InputSource::Audio => ("audio".to_string(), None, None),
            crate::pipeline::InputSource::Midi(id) => {
                let name = crate::audio::midi::list_devices()
                    .into_iter()
                    .find(|d| &d.id == id)
                    .map(|d| d.name);
                ("midi".to_string(), Some(id.clone()), name)
            }
        };
        let src_msg = AgentMessage::InputSourceChanged {
            source: src_str,
            midi_device_id: dev_id,
            midi_device_name: dev_name,
        };
        let _ = ws_tx
            .send(Message::Text(serde_json::to_string(&src_msg).unwrap()))
            .await;
        tracing::info!(target: "jamodio::ws", "pushed input source state on connect");
    }

    // Channel for outgoing messages (from message handler + periodic tasks)
    let (out_tx, mut out_rx) = tokio_mpsc::channel::<AgentMessage>(64);

    // Subscribe au broadcast shutdown (auto-update).
    let mut shutdown_rx = handle.shutdown_tx.subscribe();
    let shutdown_out = out_tx.clone();
    let shutdown_task = tokio::spawn(async move {
        if let Ok(reason) = shutdown_rx.recv().await {
            tracing::info!(target: "jamodio::ws", reason, "broadcasting Shutdown to client");
            let _ = shutdown_out
                .send(AgentMessage::Shutdown {
                    reason: reason.to_string(),
                })
                .await;
            // Petit délai pour laisser le temps au browser de recevoir + handle
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });

    // Spawn periodic StreamLevels sender (every 100ms)
    let levels_pipeline = handle.pipeline.clone();
    let levels_tx = out_tx.clone();
    let levels_task = tokio::spawn(async move {
        // SEUL le client externe (jamodio.com) reçoit les StreamLevels (VU
        // mètres). La webview interne n'a pas de VU → inutile pour elle. Son
        // dashboard est nourri par GetStats (pull, non-destructif), pas par
        // cette task. Gate sur !is_internal pour ne pas dupliquer le travail.
        if is_internal {
            return;
        }
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            let pl = levels_pipeline.lock().await;
            let rms_data = pl.mixer.lock().stream_rms();
            // Sprint B talkback auto-mute : lit input_rms (instrument self post-plugin)
            // et midi_active (Note ON dans les ~200 dernières ms) pour piloter le
            // détecteur d'activité côté browser. Ces 2 valeurs sont reset entre les
            // captures (Pipeline::new), donc Some(...) toujours valides côté agent
            // — le serializer écrira `null`/absent uniquement si l'utilisateur veut
            // un payload minimaliste (back-compat).
            // 0.5.4-18 — pendant un re-init long-settle du driver ASIO, l'alim
            // encodeur est coupée (`capture_feeding=false`) et les streams fermés :
            // on force le VU d'entrée à 0 pour toute la durée du settle. Sinon le
            // browser afficherait le pic RAILÉ figé du 1er open à froid (VU « à fond »
            // ~6 s). Garantie au niveau AFFICHAGE → insensible à toute course sur
            // `input_rms` côté thread encodeur.
            let feeding = pl.perfstats.capture_feeding.load(std::sync::atomic::Ordering::Relaxed);
            let input_rms = if feeding {
                f32::from_bits(pl.input_rms.load(std::sync::atomic::Ordering::Relaxed))
            } else {
                0.0
            };
            let midi_active = pl.midi_active.load(std::sync::atomic::Ordering::Relaxed);
            drop(pl);
            // Push si on a soit des niveaux peers, soit un signal self (RMS > 0
            // ou MIDI actif). En idle complet, on saute le push.
            let has_self_signal = input_rms > 0.0 || midi_active;
            if !rms_data.is_empty() || has_self_signal {
                let levels: Vec<StreamLevel> = rms_data
                    .into_iter()
                    .map(|(producer_id, rms, rms_l, rms_r)| StreamLevel {
                        producer_id,
                        rms,
                        rms_l: Some(rms_l),
                        rms_r: Some(rms_r),
                    })
                    .collect();
                let msg = AgentMessage::StreamLevels {
                    levels,
                    input_rms: Some(input_rms),
                    midi_active: Some(midi_active),
                };
                if levels_tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
    });

    // Sprint S1 — periodic PerfStats sender (1 Hz).
    //
    // Flush des histogrammes (capture→send + plugin process_stereo) +
    // snapshot du compteur drops capture + snapshot drift_ppm par peer +
    // stats mixer (underruns, drift_drops, target_ms). Construit
    // `AgentMessage::PerfStats` et l'envoie. Skip l'émission si encoder
    // idle ET aucun peer actif (= rien d'intéressant à reporter).
    // 0.5.3-5 — superviseur de liveness des callbacks audio (recovery ASIO).
    // Gaté sur le client externe (comme perfstats) → une seule instance par
    // agent ; la webview interne ne pilote pas de capture.
    let liveness_pipeline = handle.pipeline.clone();
    let liveness_tx = out_tx.clone();
    let liveness_task = tokio::spawn(async move {
        if is_internal {
            return;
        }
        audio_liveness_supervisor(liveness_pipeline, liveness_tx).await;
    });

    let perfstats_pipeline = handle.pipeline.clone();
    let perfstats_tx = out_tx.clone();
    let perfstats_start = Instant::now();
    let perfstats_task = tokio::spawn(async move {
        // CRITIQUE (review 11/06) : un seul flusher de perfstats par agent.
        // Le flush des histogrammes + swap(0) des atomics est DESTRUCTIF :
        // deux tasks (webview interne + browser externe) se partageraient les
        // données → overload detection sur demi-fenêtres → bypass plugin auto
        // faussé/silencieux. La webview interne ne consomme PAS « perf-stats »
        // (son dashboard utilise GetStats, pull non-destructif), donc gater
        // sur !is_internal ne change rien à son affichage et garantit un seul
        // flusher pendant les sessions (toujours pilotées par le client externe).
        if is_internal {
            return;
        }
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        // Skip le 1er tick immédiat (sinon flush vide juste après connect).
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        // Sprint S6 — anti-spam PeerUnstable par producer_id. Map<id, Instant>
        // contenant le dernier instant d'émission. Émis au max 1× / 30 s par
        // peer (= si le peer reste instable, l'agent renvoie périodiquement
        // pour signaler la situation continue, mais sans flooder).
        let mut last_peer_unstable_emit: std::collections::HashMap<
            String,
            Instant,
        > = std::collections::HashMap::new();
        const PEER_UNSTABLE_COOLDOWN: std::time::Duration =
            std::time::Duration::from_secs(30);
        const PEER_UNSTABLE_WINDOW: std::time::Duration =
            std::time::Duration::from_secs(30);
        const PEER_UNSTABLE_THRESHOLD: usize = 16;
        // 0.5.3-4 — valeurs cumulées au tick précédent pour calculer le DÉBIT de
        // callbacks CPAL par seconde (liveness ASIO). Avancent ≈370/s en session
        // saine ; figés à 0 = cold-start muet (le watchdog l'aura déjà réparé).
        let mut prev_capture_callbacks: u64 = 0;
        let mut prev_output_callbacks: u64 = 0;
        // 0.5.4-17 — détecteur de backoff de buffer (cf. `audio::buffer_policy`).
        // `buffer_low_pressure` = leaky bucket (robuste aux pics isolés) ;
        // `prev_monitor_underruns` = underruns self-monitor cumulés au tick
        // précédent (pour le delta par fenêtre ; `saturating_sub` gère le reset
        // du compteur quand le self-monitor est recréé à un nouveau start).
        let mut buffer_low_pressure: u32 = 0;
        let mut prev_monitor_underruns: u64 = 0;
        loop {
            interval.tick().await;
            let pl = perfstats_pipeline.lock().await;
            // Flush histograms (acquièrent le lock parking_lot une fois chacun)
            let pipeline_snap = pl.perfstats.pipeline_latency.lock().flush();
            let plugin_snap = pl.perfstats.plugin_latency.lock().flush();
            // v0.4.8 — 3 histogrammes par stage pour discriminer "spike
            // traitement" vs "spike file en queue ringbuf".
            let capture_snap = pl.perfstats.capture_latency.lock().flush();
            let process_snap = pl.perfstats.process_latency.lock().flush();
            let encode_snap = pl.perfstats.encode_latency.lock().flush();
            let send_path_snap = pl.perfstats.send_path_latency.lock().flush();
            // 0.5.3 — rafale d'émission (frames Opus/bloc à encode_stage).
            let emit_burst_snap = pl.perfstats.emit_burst.lock().flush();
            // 0.5.3-2 — latence du chemin de réception (arrivée → avant push mixer).
            let recv_path_snap = pl.perfstats.recv_path.lock().flush();
            // 0.5.3-4 — débit de callbacks CPAL sur la fenêtre 1 s (liveness ASIO).
            // Compteurs cumulés → on logue le delta. 0 en session active = sortie
            // ou entrée muette (cold-start), sinon ≈370/s.
            let capture_callbacks_total = pl.perfstats.capture_callbacks.load(Ordering::Relaxed);
            let output_callbacks_total = pl.perfstats.output_callbacks.load(Ordering::Relaxed);
            let capture_cb_per_sec = capture_callbacks_total.saturating_sub(prev_capture_callbacks);
            let output_cb_per_sec = output_callbacks_total.saturating_sub(prev_output_callbacks);
            prev_capture_callbacks = capture_callbacks_total;
            prev_output_callbacks = output_callbacks_total;
            // Reset+swap atomic des drops capture
            let capture_drops_window = pl
                .perfstats
                .capture_drops
                .swap(0, Ordering::Relaxed);
            // Chantier C — pic de sortie post-plugin (reset à 0 = +0.0 f32) +
            // taux de saturation soutenue (% samples > pleine-échelle).
            let output_peak =
                f32::from_bits(pl.perfstats.output_peak.swap(0, Ordering::Relaxed));
            let clip_samples = pl.perfstats.output_clip_samples.swap(0, Ordering::Relaxed);
            let total_samples = pl.perfstats.output_total_samples.swap(0, Ordering::Relaxed);
            let output_clip_pct = if total_samples > 0 {
                100.0 * clip_samples as f32 / total_samples as f32
            } else {
                0.0
            };
            // Snapshot des stats réseau par peer (drift + gigue) — clone du
            // hashmap, cheap car ≤4 peers.
            let net_stats_map: std::collections::HashMap<String, ProducerNetStats> =
                pl.perfstats.net_stats_by_producer.lock().clone();
            // Snapshot mixer stats (underruns + drift_drops cumul + target_ms)
            // + Chantier C : stats du self-monitor (latence courante + underruns).
            let (mixer_stats, monitor_buffer_ms, monitor_underruns) = {
                let m = pl.mixer.lock();
                let stats = m.stream_perf_stats();
                let (mt, mu) = m.self_monitor_stats();
                (stats, mt, mu)
            };

            // ── Adaptive buffer : backoff auto 64 → 128 sous charge soutenue ────
            // À la cible basse (64), si des drops capture OU des underruns
            // self-monitor PERSISTENT (leaky bucket → ~ESCALATE_AT s de charge
            // réelle, insensible aux 1-2 pics isolés), on remonte UNE fois à 128
            // et on demande une reconstruction seamless (exécutée par le
            // superviseur de liveness, même chemin éprouvé que la recovery). One-
            // way : jamais de retour auto à 64 (anti-oscillation / anti-glitch
            // répété) ; un 64 frais est re-tenté au prochain démarrage de l'agent.
            // Filet RARE : à 64 le callback a ~100× de marge (mesuré), un drop
            // réel suppose une machine/charge vraiment limite. Cf. `buffer_policy`.
            {
                use crate::audio::buffer_policy;
                const ESCALATE_AT: u32 = 4; // ~4 s de charge soutenue
                const DROP_BAD_PER_SEC: u64 = 10; // > 10 drops/s = vraie saturation
                let underruns_delta = monitor_underruns.saturating_sub(prev_monitor_underruns);
                prev_monitor_underruns = monitor_underruns;
                let capturing = matches!(pl.state, AgentState::Capturing);
                if capturing && buffer_policy::target() == buffer_policy::LOW {
                    let bad = capture_drops_window > DROP_BAD_PER_SEC || underruns_delta > 0;
                    buffer_low_pressure = if bad {
                        buffer_low_pressure.saturating_add(1)
                    } else {
                        buffer_low_pressure.saturating_sub(1)
                    };
                    if buffer_low_pressure >= ESCALATE_AT && buffer_policy::escalate_to_safe() {
                        buffer_policy::request_rebuild();
                        buffer_low_pressure = 0;
                        tracing::warn!(
                            target: "jamodio::ws",
                            from = buffer_policy::LOW,
                            to = buffer_policy::SAFE,
                            drops_per_sec = capture_drops_window,
                            underruns_delta,
                            "buffer bas insuffisant sous charge — passage automatique 64 → 128 (reconstruction seamless, une seule fois)"
                        );
                    }
                } else {
                    buffer_low_pressure = 0;
                }
            }
            // Sprint S6 — récupère les peers REMOTE instables (= > 16 drift
            // drains sur fenêtre 30 s). Le mixer purge ses VecDeque internes
            // au passage. Retour : (producer_id, events_window, drains_total).
            let unstable_peers: Vec<(String, usize, u64)> = pl
                .mixer
                .lock()
                .stream_unstable_events(
                    PEER_UNSTABLE_WINDOW,
                    PEER_UNSTABLE_THRESHOLD,
                );
            // Nom du plugin actif (si chargé) pour `PluginPerf.name` — sinon
            // on omet PluginPerf entièrement.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let plugin_name: Option<String> = pl
                .instrument_plugin_info
                .lock()
                .as_ref()
                .map(|info| info.name.clone());
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let plugin_name: Option<String> = None;

            // v0.4.9 — détection saturation pipeline globale (= capture_drops
            // burst, distinct du plugin overload S5). Cas typique : sample-load
            // brutal d'un plugin sampler (BFD Player, Kontakt) qui bloque
            // l'encoder thread alors que `plugin_latency` reste sous le seuil
            // (les blocs droppés ne traversent pas le plugin → pas mesurés).
            //
            // Anti-spam : on n'émet qu'une fois toutes les 10 s pour éviter
            // le flood en cas de saturation prolongée (BFD chargeant plusieurs
            // samples consécutifs).
            const PIPELINE_OVERLOAD_DROP_THRESHOLD: u64 = 100;
            const PIPELINE_OVERLOAD_COOLDOWN_MS: u128 = 10_000;
            static LAST_PIPELINE_OVERLOAD_EMIT_MS: OnceLock<
                std::sync::atomic::AtomicU64,
            > = OnceLock::new();
            let last_emit_atomic = LAST_PIPELINE_OVERLOAD_EMIT_MS
                .get_or_init(|| std::sync::atomic::AtomicU64::new(0));
            let now_ms = perfstats_start.elapsed().as_millis() as u64;
            let last_emit = last_emit_atomic.load(Ordering::Relaxed);
            let pipeline_overload_msg: Option<AgentMessage> =
                if capture_drops_window > PIPELINE_OVERLOAD_DROP_THRESHOLD
                    && (now_ms as u128)
                        .saturating_sub(last_emit as u128)
                        > PIPELINE_OVERLOAD_COOLDOWN_MS
                {
                    last_emit_atomic.store(now_ms, Ordering::Relaxed);
                    let plugin_name_owned =
                        plugin_name.clone().unwrap_or_default();
                    tracing::warn!(
                        target: "jamodio::ws",
                        drops_per_sec = capture_drops_window,
                        pipeline_p99_ms = pipeline_snap.p99_ms,
                        plugin = %plugin_name_owned,
                        "agent pipeline overload — encoder thread bloqué (sample-load plugin ou CPU tiers)"
                    );
                    Some(AgentMessage::AgentPipelineOverload {
                        drops_per_sec: capture_drops_window,
                        pipeline_p99_ms: pipeline_snap.p99_ms,
                        plugin_name: plugin_name_owned,
                    })
                } else {
                    None
                };

            // Sprint S5 (révisé v0.4.11) — bypass auto plugin SEULEMENT s'il
            // cause des DROPS RÉELS.
            //
            // L'ancienne logique (p99 plugin > 4 ms seul) était trop agressive
            // et hardware-dépendante : sur la session 28/05 (Mac Mini M1),
            // AmpliTube 5 et BFD Player tournaient à p99 4-6 ms AVEC
            // drops_per_sec=0 (= le ringbuf S3 absorbait, audio nickel) mais
            // étaient bypassés à tort → l'utilisateur entendait le DRY
            // ("comme si bypass") + silence total en mode MIDI.
            //
            // Le SEUL signal fiable de "le plugin sature vraiment" =
            // `capture_drops` > 0 (= le CPAL callback ne peut plus pousser ses
            // samples car l'encoder est durablement bloqué). Hardware-agnostic :
            // on bypasse quand ça coupe RÉELLEMENT, pas quand un plugin lourd
            // mais viable prend 5 ms par bloc.
            //
            // Conditions cumulatives :
            //   1. capture_drops_window > seuil (= vraies coupures, pas 1-2 isolés)
            //   2. plugin actif (= candidat) avec p99 notable (= il consomme)
            //   3. pas déjà bypassé (anti-spam — flag reset par l'user/load)
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let overload_msg: Option<AgentMessage> = {
                // Seuil drops : > 20/s = saturation soutenue (≈ 5 % des blocs
                // CPAL droppés). En-dessous, c'est tolérable / transitoire.
                const OVERLOAD_DROPS_THRESHOLD: u64 = 20;
                // p99 minimal pour incriminer le plugin (= il consomme du temps
                // significatif ; sinon les drops viennent d'ailleurs → couvert
                // par AgentPipelineOverload).
                const OVERLOAD_PLUGIN_P99_MIN_MS: f32 = 3.0;
                let plugin_is_culprit = plugin_snap.count >= 50
                    && plugin_snap.p99_ms > OVERLOAD_PLUGIN_P99_MIN_MS;
                if capture_drops_window > OVERLOAD_DROPS_THRESHOLD
                    && plugin_is_culprit
                    && !pl
                        .plugin_auto_bypass_active
                        .load(Ordering::SeqCst)
                {
                    // Trigger : on flag bypass auto + auto_bypass_active = true.
                    pl.instrument_plugin_bypass.store(true, Ordering::SeqCst);
                    pl.plugin_auto_bypass_active
                        .store(true, Ordering::SeqCst);
                    let name = plugin_name
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    tracing::warn!(
                        target: "jamodio::plugin",
                        plugin = %name,
                        p99_ms = plugin_snap.p99_ms,
                        max_ms = plugin_snap.max_ms,
                        drops_per_sec = capture_drops_window,
                        count = plugin_snap.count,
                        "plugin overload détecté (drops réels) — bypass auto activé"
                    );
                    Some(AgentMessage::InstrumentPluginOverload {
                        name,
                        p99_ms: plugin_snap.p99_ms,
                        max_ms: plugin_snap.max_ms,
                        count: plugin_snap.count,
                    })
                } else {
                    None
                }
            };
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let overload_msg: Option<AgentMessage> = None;

            drop(pl);

            // Plugin perf : seulement si on a observé ET qu'un plugin est
            // chargé. Le nom peut être absent si la course load↔flush se
            // termine entre les deux locks — dans ce cas on rapporte
            // l'observation avec "unknown" pour ne pas perdre la donnée.
            let plugin_perf = if plugin_snap.count > 0 {
                Some(PluginPerf {
                    name: plugin_name.unwrap_or_else(|| "unknown".to_string()),
                    count: plugin_snap.count,
                    mean_ms: plugin_snap.mean_ms,
                    p50_ms: plugin_snap.p50_ms,
                    p99_ms: plugin_snap.p99_ms,
                    max_ms: plugin_snap.max_ms,
                })
            } else {
                None
            };

            // Construction de la couche pipeline_latency (toujours présente,
            // count=0 indique encoder idle).
            let pipeline_latency_ms = PipelineLatency {
                count: pipeline_snap.count,
                p50_ms: pipeline_snap.p50_ms,
                p99_ms: pipeline_snap.p99_ms,
                max_ms: pipeline_snap.max_ms,
                mean_ms: pipeline_snap.mean_ms,
                // Inclut les drops "RTP channel full" agrégés par l'histogramme
                // (record_drop côté encoder) + les drops capture côté CPAL.
                // Les deux sont des indicateurs de saturation à reporter ensemble.
                drops_per_sec: pipeline_snap.drops + capture_drops_window,
            };

            // Construction des peers : on dérive de mixer_stats + net_stats_map.
            // Si un producer est dans mixer mais pas dans net_stats_map (warmup),
            // les métriques réseau valent 0.0 (cf. drift.rs / jitter.rs).
            let peers: Vec<PeerPerf> = mixer_stats
                .into_iter()
                .map(|(producer_id, underruns, drift_drops, target_ms)| {
                    let net = net_stats_map.get(&producer_id).copied().unwrap_or_default();
                    PeerPerf {
                        producer_id,
                        drift_ppm: net.drift_ppm,
                        jitter_ms: net.jitter_ms,
                        jitter_tail_ms: net.jitter_tail_ms,
                        buffer_target_ms: target_ms,
                        underruns,
                        drift_drops,
                    }
                })
                .collect();

            // Phase A — observabilité : log par peer de la gigue mesurée vs la
            // cible courante du buffer (calibration des Phases B/C). 1 Hz, debug.
            for p in &peers {
                tracing::debug!(
                    target: "jamodio::netstats",
                    producer = &p.producer_id[..8.min(p.producer_id.len())],
                    jitter_ms = p.jitter_ms,
                    jitter_tail_ms = p.jitter_tail_ms,
                    drift_ppm = p.drift_ppm,
                    buffer_target_ms = p.buffer_target_ms,
                    underruns = p.underruns,
                    "peer net stats"
                );
            }

            // Skip si rien à reporter (encoder idle + pas de peer + pas de plugin)
            if plugin_perf.is_none()
                && pipeline_latency_ms.count == 0
                && pipeline_latency_ms.drops_per_sec == 0
                && peers.is_empty()
            {
                continue;
            }

            // Log structuré tracing pour qu'il finisse dans agent.log :
            // permet au support de lire les perfstats même sans bundle browser.
            // v0.4.8 — ajout des 3 mesures par stage (capture/process/encode).
            // Sémantique : pipeline_* = end-to-end (= traitement + temps en
            // file dans les ringbufs). capture_*/process_*/encode_* = temps
            // de traitement PUR par stage. Permet de discriminer un spike
            // "vraie charge plugin" vs "stall en queue".
            tracing::info!(
                target: "jamodio::perfstats",
                pipeline_p50_ms = pipeline_latency_ms.p50_ms,
                pipeline_p99_ms = pipeline_latency_ms.p99_ms,
                pipeline_max_ms = pipeline_latency_ms.max_ms,
                pipeline_count = pipeline_latency_ms.count,
                drops_per_sec = pipeline_latency_ms.drops_per_sec,
                plugin_name = plugin_perf.as_ref().map(|p| p.name.as_str()).unwrap_or(""),
                plugin_p99_ms = plugin_perf.as_ref().map(|p| p.p99_ms).unwrap_or(0.0),
                plugin_max_ms = plugin_perf.as_ref().map(|p| p.max_ms).unwrap_or(0.0),
                capture_p99_ms = capture_snap.p99_ms,
                capture_max_ms = capture_snap.max_ms,
                process_p99_ms = process_snap.p99_ms,
                process_max_ms = process_snap.max_ms,
                encode_p99_ms = encode_snap.p99_ms,
                encode_max_ms = encode_snap.max_ms,
                send_path_p50_ms = send_path_snap.p50_ms,
                send_path_p99_ms = send_path_snap.p99_ms,
                send_path_max_ms = send_path_snap.max_ms,
                // 0.5.3-2 — latence réception (arrivée→push). p99 qui grimpe =
                // décodage préempté (le bug Windows que le thread RT corrige).
                recv_path_p50_ms = recv_path_snap.p50_ms,
                recv_path_p99_ms = recv_path_snap.p99_ms,
                recv_path_max_ms = recv_path_snap.max_ms,
                // 0.5.3 — rafale d'émission : frames Opus émises par bloc d'entrée.
                // ≈1 = flux régulier (pas de rafale) ; ≫1 = callback gros (ASIO non
                // honoré). À 48 k natif : emit_burst_mean ≈ taille_callback / 120.
                emit_burst_p50 = emit_burst_snap.p50_ms,
                emit_burst_max = emit_burst_snap.max_ms,
                emit_burst_mean = emit_burst_snap.mean_ms,
                // 0.5.3-4 — liveness callbacks CPAL (par seconde). 0 en session
                // active = cold-start muet (watchdog). ≈370/s = sain.
                capture_cb_per_sec,
                output_cb_per_sec,
                peers = peers.len(),
                output_peak,
                output_clip_pct,
                monitor_buffer_ms,
                monitor_underruns,
                "perfstats snapshot"
            );

            let msg = AgentMessage::PerfStats {
                timestamp_ms: perfstats_start.elapsed().as_millis() as u64,
                plugin: plugin_perf,
                pipeline_latency_ms,
                peers,
                output_peak,
                output_clip_pct,
                monitor_buffer_ms,
                monitor_underruns,
            };
            if perfstats_tx.send(msg).await.is_err() {
                break;
            }

            // Sprint S5 — émet le message d'overload APRÈS le PerfStats (le
            // browser voit d'abord les chiffres "vérité" qui ont déclenché
            // le trigger, puis le toast UI). 1 seul message par cycle
            // d'overload — protégé par `plugin_auto_bypass_active`.
            let plugin_overload_fired = overload_msg.is_some();
            if let Some(msg) = overload_msg {
                if perfstats_tx.send(msg).await.is_err() {
                    break;
                }
            }

            // v0.4.9 — émet le message AgentPipelineOverload (distinct du
            // plugin overload S5 ; ne déclenche PAS de bypass plugin).
            // v0.4.11 — si un bypass plugin vient de partir sur la même
            // fenêtre de drops, on supprime ce toast générique : le bypass
            // plugin EST la cause/remédiation spécifique, deux toasts pour
            // le même évènement = bruit UX.
            if let Some(msg) = pipeline_overload_msg {
                if !plugin_overload_fired && perfstats_tx.send(msg).await.is_err() {
                    break;
                }
            }

            // Sprint S6 — émet AgentMessage::PeerUnstable pour chaque peer
            // au-dessus du seuil. Anti-spam : 1× / 30 s par producer_id.
            // Le browser maintient le badge "⚠ X envoie par à-coups" pendant
            // 60 s après le dernier message (= si message renvoyé toutes les
            // 30 s, badge reste affiché ; sinon il disparaît).
            let now = Instant::now();
            for (producer_id, events_window, drains_total) in unstable_peers {
                let should_emit = match last_peer_unstable_emit.get(&producer_id) {
                    Some(last) => now.duration_since(*last) >= PEER_UNSTABLE_COOLDOWN,
                    None => true,
                };
                if !should_emit {
                    continue;
                }
                let drift_ppm = net_stats_map
                    .get(&producer_id)
                    .map(|n| n.drift_ppm)
                    .unwrap_or(0.0);
                tracing::warn!(
                    target: "jamodio::mixer",
                    producer = &producer_id[..8.min(producer_id.len())],
                    events_window,
                    drains_total,
                    drift_ppm,
                    "peer instable détecté — envoie en bursts"
                );
                if perfstats_tx
                    .send(AgentMessage::PeerUnstable {
                        producer_id: producer_id.clone(),
                        drift_drains_window: events_window as u64,
                        drift_drains_total: drains_total,
                        drift_ppm,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                last_peer_unstable_emit.insert(producer_id, now);
            }
        }
    });

    // Spawn task to forward outgoing messages to WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if ws_tx.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    // v0.4.3 — Receive loop unifié.
    //
    // Critères d'arrêt :
    //   1. WS fermée proprement (close frame du browser, erreur réseau)
    //   2. Eviction par un nouveau client (killer_rx — uniquement post-promotion)
    //   3. Watchdog (uniquement post-promotion, voir détail ci-dessous)
    //
    // Phases :
    //   - PRE-PROMOTION : on attend un BrowserMessage. Le timeout est
    //     généreux (60 s) car un probe peut juste lire Hello et fermer,
    //     mais on veut pouvoir détecter une session "abandonnée" qui ne
    //     fait que rester ouverte sans rien envoyer. Pas de slot pris,
    //     pas de killer armé.
    //   - POST-PROMOTION (déclenchée par 1er BrowserMessage côté externe) :
    //     on prend le slot, on kick le précédent client, on arme le killer.
    //     Le watchdog devient 5 s (heartbeat browser = 1.5 s × 3 = 4.5 s).
    //
    // Les clients internes (UI Tauri webview) restent en phase pre-promotion :
    // pas de slot, pas de killer, pas de watchdog agressif.
    const PRE_PROMOTION_IDLE_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(60);
    const POST_PROMOTION_WATCHDOG: std::time::Duration =
        std::time::Duration::from_secs(5);

    let mut exit_reason: &'static str = "ws-closed-normally";

    loop {
        let current_timeout = if slot_taken {
            POST_PROMOTION_WATCHDOG
        } else {
            PRE_PROMOTION_IDLE_TIMEOUT
        };

        // Branche killer : `pending()` tant qu'on n'a pas armé (pre-promotion).
        let killer_fut = async {
            match killer_rx.as_mut() {
                Some(rx) => rx.await.ok(),
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased; // priorité au killer pour libérer le slot vite
            kicked = killer_fut => {
                exit_reason = kicked.unwrap_or("displaced");
                break;
            }
            ws_msg = tokio::time::timeout(current_timeout, ws_rx.next()) => {
                match ws_msg {
                    Err(_) => {
                        if slot_taken {
                            // Heartbeat 1.5 s côté browser aurait dû arriver.
                            // 5 s sans message = WS half-open (tab killed,
                            // kernel buffer saturé, etc.). Libère le slot.
                            tracing::warn!(
                                target: "jamodio::ws",
                                timeout_secs = POST_PROMOTION_WATCHDOG.as_secs(),
                                "watchdog timeout post-promotion — releasing slot"
                            );
                            exit_reason = "watchdog-timeout";
                        } else {
                            // Pre-promotion idle : probe qui n'a pas fermé,
                            // ou client mort avant HelloAck. On ferme.
                            tracing::debug!(
                                target: "jamodio::ws",
                                "pre-promotion idle timeout — closing"
                            );
                            exit_reason = "pre-promotion-idle";
                        }
                        break;
                    }
                    Ok(None) => break, // WS closed by browser side
                    Ok(Some(Err(e))) => {
                        tracing::warn!(
                            target: "jamodio::ws",
                            error = %e,
                            "ws receive error — closing connection"
                        );
                        exit_reason = "ws-error";
                        break;
                    }
                    Ok(Some(Ok(msg))) => {
                        // v0.4.4 — PROMOTION uniquement sur Message::Text
                        // qui se parse en BrowserMessage. Les Close/Ping/Pong/
                        // Binary ne déclenchent PAS promotion : c'est ce qui
                        // créait le log spam observé en v0.4.3 (probes
                        // agent-status.js qui ferment leur WS génèrent un
                        // Close frame, traité à tort comme "1er message").
                        let is_real_browser_msg = match &msg {
                            Message::Text(text) => serde_json::from_str::<
                                BrowserMessage,
                            >(text)
                            .is_ok(),
                            _ => false,
                        };

                        if is_real_browser_msg && !slot_taken && !is_internal {
                            slot_taken = true;
                            let (new_killer_tx, new_killer_rx) =
                                tokio::sync::oneshot::channel();
                            let previous = handle
                                .active_client_killer
                                .lock()
                                .replace(new_killer_tx);
                            if let Some(prev) = previous {
                                // v0.4.4 — log "displacing" UNIQUEMENT si on
                                // a réellement kické quelqu'un. send() retourne
                                // Err si le receiver a déjà été drop (= ancien
                                // client déjà cleanup, Sender stale dans le
                                // Mutex). Dans ce cas, no-op silencieux : pas
                                // de kick effectif, pas de log spam.
                                match prev.send("displaced-by-new-client") {
                                    Ok(()) => {
                                        tracing::warn!(
                                            target: "jamodio::ws",
                                            "displacing previous external client (stale slot — likely half-open WS or rapid reconnect)"
                                        );
                                        // Pause courte pour laisser le cleanup
                                        // ancien finir (stop_all peut prendre
                                        // quelques ms).
                                        tokio::time::sleep(
                                            std::time::Duration::from_millis(50),
                                        ).await;
                                    }
                                    Err(_) => {
                                        // Sender stale — ancien client déjà parti.
                                        // Pas de kick effectif, pas de pause.
                                    }
                                }
                            }
                            handle.client_active.store(true, Ordering::SeqCst);
                            killer_rx = Some(new_killer_rx);
                            tracing::info!(
                                target: "jamodio::ws",
                                "external client promoted (first BrowserMessage received) — watchdog armed"
                            );
                        }

                        if !handle_one_message(msg, &handle, &out_tx).await {
                            break;
                        }
                    }
                }
            }
        }
    }

    tracing::info!(
        target: "jamodio::ws",
        is_internal,
        promoted = slot_taken,
        reason = exit_reason,
        "client disconnected — cleanup"
    );

    levels_task.abort();
    perfstats_task.abort();
    liveness_task.abort();
    send_task.abort();
    shutdown_task.abort();

    // v0.4.3 — Cleanup pipeline UNIQUEMENT pour les clients externes
    // qui ont été PROMUS (= ont envoyé au moins un BrowserMessage et donc
    // pris le slot). Un client externe non promu (= probe qui a juste lu
    // Hello et fermé) ne touche pas au pipeline et ne libère pas de slot.
    //
    // Les clients internes (UI Tauri webview) ne pilotent jamais le pipeline,
    // donc leur close ne doit PAS stop_all (sinon le browser actif perd sa
    // session quand on ferme la fenêtre Tauri par erreur).
    if !is_internal && slot_taken {
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            handle.pipeline.lock(),
        )
        .await
        {
            // 0.5.4-5 — déconnexion WS = sortie de studio : on PARK (driver ASIO
            // gardé chaud pour un rejoin rapide, relâché après grâce) au lieu de
            // tout fermer. Sur macOS/WASAPI, `leave_session` retombe sur stop_all.
            Ok(mut pl) => pl.leave_session(),
            Err(_) => tracing::warn!(
                target: "jamodio::ws",
                "pipeline lock timeout during cleanup — leave_session skipped (next client will see stale state)"
            ),
        }
        // Libère le slot. Note : on ne `take()` PAS notre Sender dans
        // `active_client_killer` car s'il a été displaced, c'est déjà le
        // Sender d'un autre client qui occupe le slot. Le mutex contient
        // potentiellement un sender stale ; le prochain `replace()` retournera
        // un Err silencieux sur `send()` (receiver drop) — OK, idempotent.
        handle.client_active.store(false, Ordering::SeqCst);
    }
}

/// Handler dédié aux connexions read-only `?op=logs`. Ne prend PAS le
/// slot single-client, ne pilote PAS le pipeline, ne fait PAS de cleanup
/// → la WS persistante du studio (s'il y en a une) reste intacte.
///
/// Sert uniquement `GetLogsArchive` puis ferme. Toute autre commande est
/// rejetée explicitement (Error) — pas de surface d'attaque additionnelle.
/// Timeout de sécurité 10 s pour éviter une connexion qui pendrait.
async fn handle_logs_connection(socket: WebSocket, handle: WsServerHandle) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    tracing::info!(target: "jamodio::ws", "logs-only client connected");

    // Hello envoyé pour cohérence avec le handler principal (le browser
    // vérifie peut-être qu'il reçoit un Hello avant d'envoyer sa requête).
    let hello = make_hello();
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&hello).unwrap()))
        .await;

    // Une seule itération attendue : GetLogsArchive → réponse → close.
    let res = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            let Message::Text(text) = msg else { continue };
            let browser_msg = match serde_json::from_str::<BrowserMessage>(&text) {
                Ok(m) => m,
                Err(_) => continue, // ignore HelloAck éventuel + autres bruits
            };
            match browser_msg {
                BrowserMessage::GetLogsArchive { max_days, max_bytes } => {
                    let days = max_days.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_DAYS);
                    let bytes = max_bytes.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_BYTES);
                    let collected = tokio::task::spawn_blocking(move || {
                        let (content, files, truncated) =
                            crate::logging::collect_recent_logs(days, bytes);
                        let log_dir = crate::logging::log_dir().to_string_lossy().into_owned();
                        (content, files, truncated, log_dir)
                    })
                    .await;
                    let payload = match collected {
                        Ok((content, files, truncated, log_dir)) => {
                            tracing::info!(
                                target: "jamodio::support",
                                files = files.len(),
                                bytes = content.len(),
                                truncated,
                                conn = "logs-only",
                                "GetLogsArchive served"
                            );
                            AgentMessage::LogsArchive { content, files, truncated, log_dir }
                        }
                        Err(e) => AgentMessage::error(
                            format!("logs archive task failed: {e}"),
                        ),
                    };
                    let _ = ws_tx
                        .send(Message::Text(serde_json::to_string(&payload).unwrap()))
                        .await;
                    return;
                }
                BrowserMessage::HelloAck { .. } => continue,
                #[allow(unreachable_patterns)]
                _ => {
                    // Tout autre message est refusé : ce canal est read-only.
                    let err = AgentMessage::error(
                        "logs-only connection: only get-logs-archive is allowed",
                    );
                    let _ = ws_tx
                        .send(Message::Text(serde_json::to_string(&err).unwrap()))
                        .await;
                    return;
                }
            }
        }
    })
    .await;
    if res.is_err() {
        tracing::warn!(target: "jamodio::ws", "logs-only connection timed out (10s)");
    }
    // Petite latence pour laisser le frame WS partir avant le close TCP.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _ = ws_tx.close().await;
    // Le `_ = handle` empêche le warning unused — on ne touche PAS au pipeline
    // ni au flag client_active depuis ici, c'est tout l'intérêt.
    let _ = handle;
    tracing::info!(target: "jamodio::ws", "logs-only client disconnected (no cleanup)");
}

/// Chantier A (v0.4.12) — sérialise les opérations plugin LENTES (load/unload
/// natif AU/VST3, 0,4–4 s). Tenu HORS du lock `PipelineState` et du chemin
/// audio → ne gèle rien. Garantit qu'on n'exécute jamais deux init/teardown
/// natifs concurrents (course handle ↔ instance) même si le browser spamme.
#[cfg(any(target_os = "macos", target_os = "windows"))]
static PLUGIN_OPS_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn plugin_ops_lock() -> &'static tokio::sync::Mutex<()> {
    PLUGIN_OPS_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Tente d'acquérir le lock pipeline avec un timeout court. Si dépassé,
/// retourne None et le caller répond Error{overloaded} au lieu de bloquer.
///
/// Réservé au **hot-path idempotent** (SetVolume/Pan/Dim, GetStats, Reference*,
/// éditeur plugin…) : ces messages sont fréquents et/ou rejoués, donc en
/// dropper un sur contention est sans conséquence. Pour le SETUP CRITIQUE
/// (StartCapture, AddStream, SelectDevices, SetInputSource, Load/Unload plugin,
/// ListPlugins, Start/StopRecording, Stop) → `lock_pipeline_wait` (on ATTEND).
async fn try_lock_pipeline(
    pipeline: &Arc<tokio::sync::Mutex<PipelineState>>,
) -> Option<tokio::sync::MutexGuard<'_, PipelineState>> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(LOCK_TIMEOUT_MS),
        pipeline.lock(),
    )
    .await
    {
        Ok(guard) => Some(guard),
        Err(_) => {
            tracing::warn!(
                target: "jamodio::ws",
                timeout_ms = LOCK_TIMEOUT_MS,
                "pipeline.lock() timeout — agent overloaded, skipping handler"
            );
            None
        }
    }
}

/// P0 (0.5.6) — acquisition du lock pour les handlers de **SETUP CRITIQUE**.
///
/// Contrairement au hot-path, on ne DROPPE PAS ces handlers sur contention :
/// dropper un `AddStream`/`StartCapture`/swap-device laisse un flux muet ou une
/// tranche figée jusqu'au relaunch (symptômes A/B). On ATTEND donc le lock avec
/// un timeout LONG. Comme la receive loop est sérielle, le seul concurrent
/// possible est une tâche de fond (superviseur de liveness pendant un reset ASIO,
/// borné ~2 s) : l'attente aboutit quasi toujours.
///
/// Le timeout reste sous le watchdog browser (5 s) : si l'agent est réellement
/// bloqué au-delà de `LOCK_WAIT_CRITICAL_MS`, on rend une `Error` (idéalement
/// corrélée) que le browser peut retenter, plutôt que de pendre indéfiniment.
const LOCK_WAIT_CRITICAL_MS: u64 = 3000;

async fn lock_pipeline_wait(
    pipeline: &Arc<tokio::sync::Mutex<PipelineState>>,
) -> Option<tokio::sync::MutexGuard<'_, PipelineState>> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(LOCK_WAIT_CRITICAL_MS),
        pipeline.lock(),
    )
    .await
    {
        Ok(guard) => Some(guard),
        Err(_) => {
            tracing::warn!(
                target: "jamodio::ws",
                timeout_ms = LOCK_WAIT_CRITICAL_MS,
                "pipeline.lock() timeout on CRITICAL handler — still contended after wait"
            );
            None
        }
    }
}

/// Validation de la destination SFU fournie par le browser (M-agent-2, review
/// pré-BETA 2026-07-12) : c'est la frontière contre une redirection du flux
/// micro. Refuse une IP non parseable, unspecified ou multicast ; en release
/// refuse aussi le loopback (jamais un vrai POP SFU depuis l'agent), toléré en
/// debug pour un SFU local. Partagé par `StartCapture` et `StartVoiceCapture`
/// (le flux voix mérite EXACTEMENT la même garde que l'instrument).
fn is_valid_sfu_dest(sfu_ip: &str) -> bool {
    match sfu_ip.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            let mut bogus = ip.is_unspecified() || ip.is_multicast();
            if cfg!(not(debug_assertions)) {
                bogus = bogus || ip.is_loopback();
            }
            !bogus
        }
        Err(_) => false,
    }
}

/// 0.5.4-2 — SUPERVISEUR de liveness + RESET COOPÉRATIF des callbacks audio ASIO.
///
/// # Cause racine (bug PC 28/06)
/// Sur certains drivers ASIO full-duplex (Focusrite), le driver émet un
/// `kAsioResetRequest` (resync horloge/buffer USB). Le protocole ASIO impose à
/// l'hôte d'honorer ce message : répondre « 1 » PUIS exécuter lui-même
/// `ASIOExit→ASIOInit→CreateBuffers→Start`. Or **cpal 0.15 n'enregistre aucun
/// callback de message ASIO** → asio-sys répond « 1 » au driver mais n'exécute
/// rien → le driver halte ses callbacks et attend un reset qui ne vient jamais
/// (wedge ; jusqu'ici seul un replug USB le débloquait).
///
/// # Deux niveaux de défense
/// 1. **Reset coopératif (cause racine)** : `audio::asio_reset` enregistre le
///    callback que cpal omet (via le `Driver` que cpal possède déjà). À chaque
///    `kAsioResetRequest`, le driver nous réveille IMMÉDIATEMENT (`Notify`) et on
///    exécute le reset propre — au moment où le driver le demande.
/// 2. **Filet de liveness (morts silencieuses)** : on observe les compteurs de
///    callbacks ; un flatline > ~1,5 s en capture active déclenche le même reset.
///    Couvre les drivers qui halteraient sans émettre de reset request.
///
/// # Robustesse du reset (cas dur)
/// Le reset est BORNÉ avec settle + backoff (`repair_audio_streams`) : le 1er
/// essai attend que le driver USB relâche après l'`ASIOExit` (cause des échecs
/// immédiats observés). Si tous les essais échouent (driver bloqué au niveau
/// matériel USB), on **n'arrête PAS la session** (pas de `stop_all` → pas de
/// cascade Shutdown/relaunch) : on passe en mode DÉGRADÉ et on relance lentement
/// en arrière-plan. Le rebranchement de l'interface relance alors l'audio SANS
/// re-handshake réseau ni redémarrage de l'agent.
///
/// macOS/Linux : pas d'ASIO, les callbacks ne meurent pas et le canal de reset
/// n'est jamais signalé → ce superviseur reste inerte (no-op).
async fn audio_liveness_supervisor(
    pipeline: Arc<tokio::sync::Mutex<PipelineState>>,
    // Conservé pour un éventuel feedback browser ; aucun message envoyé pour
    // l'instant (décision produit : on analyse via les logs avant toute UI).
    _out_tx: tokio_mpsc::Sender<AgentMessage>,
) {
    use std::sync::atomic::Ordering;
    // Cadence du filet de liveness (le reset coopératif, lui, réagit via Notify).
    // P1 (01/07) — 250 ms (vs 500) : réaction plus fine au gel de callbacks ASIO
    // (mort silencieuse du driver Focusrite), coût négligeable (4 locks brefs/s).
    const TICK_MS: u64 = 250;
    // Flatline confirmé si aucun callback pendant ce délai en capture active.
    // P1 (01/07) — 800 ms (vs 1500) : sur la mort silencieuse ASIO le gel dure ~2 s
    // avant détection à l'ancien seuil → ~1 s de trou audio en trop. 800 ms reste
    // ≫ période ASIO (2,7 ms) → un stream vivant produit ~290 callbacks/800 ms,
    // faux positif quasi impossible (aucun gap légitime de cet ordre observé).
    const FLATLINE_MS: u128 = 800;
    // Intervalle minimal entre deux séquences de réparation (anti-thrash). En
    // régime nominal le reset s'exécute une fois et réussit ; ce garde borne le
    // rythme si le driver re-demande des resets en rafale.
    const MIN_REPAIR_INTERVAL: Duration = Duration::from_secs(2);
    // En mode dégradé (driver dur-bloqué), on relance LENTEMENT en arrière-plan
    // jusqu'au replug — heartbeat de récupération, jamais une boucle serrée.
    const DEGRADED_RETRY_INTERVAL: Duration = Duration::from_secs(8);
    // 0.5.4-5 — délai de grâce avant de relâcher le driver ASIO gardé chaud
    // (parké, hors studio). Couvre les leave/rejoin rapides (anti-churn Focusrite)
    // puis libère l'interface pour un autre logiciel (DAW). Cf. `park`.
    const PARK_GRACE: Duration = Duration::from_secs(30);

    // Canal de signalisation kAsioResetRequest (stable pour la vie du pipeline).
    let reset_signal = { pipeline.lock().await.reset_signal() };
    let reset_notify = reset_signal.notify_handle();

    // 0.5.4-18 — écoute des réveils de veille Windows. Au resume système, le
    // driver ASIO peut revenir muet OU vivant-mais-figé (contenu railé) : la
    // seule réponse générique (tous modèles) est un re-init propre. On réutilise
    // exactement le chemin de reset borné (`repair_audio_streams`). No-op hors
    // Windows (le signal n'est jamais déclenché). Idempotent.
    let resume_signal = crate::audio::power_events::register();
    let resume_notify = resume_signal.notify_handle();
    let mut last_resume_seen = resume_signal.resume_count();

    let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // État inter-réveils.
    let mut session_active = false;
    let mut prev_cap = 0u64;
    let mut prev_out = 0u64;
    let mut last_progress = Instant::now();
    let mut last_reset_seen = reset_signal.request_count();
    let mut degraded = false;
    let mut last_repair: Option<Instant> = None;

    loop {
        // Réveil : tick périodique (filet de liveness) OU demande de reset
        // immédiate émise par le driver (chemin cause-racine). Les deux futures
        // sont cancel-safe ; la source de vérité reste `request_count()` + les
        // compteurs, donc une notification « manquée » est rattrapée au tick.
        tokio::select! {
            _ = interval.tick() => {}
            _ = reset_notify.notified() => {}
            _ = resume_notify.notified() => {}
        }

        // 0.5.4-5 — relâche le driver ASIO gardé chaud si la grâce de park est
        // expirée (≥ 30 s hors studio sans rejoin) → libère l'interface. No-op si
        // pas parké / hors ASIO.
        {
            let mut pl = pipeline.lock().await;
            pl.close_warm_if_grace_expired(PARK_GRACE);
        }

        // Observation atomique (lock bref).
        let (state_capturing, has_stream, cap, out) = {
            let pl = pipeline.lock().await;
            (
                matches!(pl.state, AgentState::Capturing),
                pl.has_active_capture_stream(),
                pl.perfstats.capture_callbacks.load(Ordering::Relaxed),
                pl.perfstats.output_callbacks.load(Ordering::Relaxed),
            )
        };

        // Hors session, ou transition Idle→Capturing : base propre, on oublie
        // tout état dégradé, on attend.
        if !state_capturing || !session_active {
            session_active = state_capturing;
            degraded = false;
            prev_cap = cap;
            prev_out = out;
            last_progress = Instant::now();
            last_reset_seen = reset_signal.request_count();
            // Un réveil survenu hors session est sans objet (le prochain start
            // rouvrira à froid) → on le consomme pour ne pas réparer à vide.
            last_resume_seen = resume_signal.resume_count();
            // Une demande de backoff arrivée hors session est caduque : le
            // prochain start rouvrira déjà à la cible courante. On la purge pour
            // éviter un rebuild parasite au démarrage suivant.
            crate::audio::buffer_policy::take_rebuild_request();
            continue;
        }

        // 0.5.4-17 — backoff de buffer demandé par le flush `perfstats` (64 → 128
        // insuffisant sous charge). On reconstruit les streams à la nouvelle taille
        // via le MÊME chemin seamless que la recovery (`repair_audio_streams` :
        // close→settle→reopen, session réseau maintenue, pas de redémarrage), MÊME
        // si les callbacks avancent — le driver est sain, on ne change que la
        // taille. Événement one-way et unique (cf. `buffer_policy`).
        if crate::audio::buffer_policy::take_rebuild_request() {
            tracing::info!(
                target: "jamodio::ws",
                "reconstruction des streams à la nouvelle taille de buffer (backoff auto)"
            );
            let _ = repair_audio_streams(&pipeline).await;
            last_reset_seen = reset_signal.request_count();
            last_resume_seen = resume_signal.resume_count();
            last_progress = Instant::now();
            last_repair = Some(Instant::now());
            {
                let pl = pipeline.lock().await;
                prev_cap = pl.perfstats.capture_callbacks.load(Ordering::Relaxed);
                prev_out = pl.perfstats.output_callbacks.load(Ordering::Relaxed);
            }
            continue;
        }

        // RE-INIT « long-settle » au RÉVEIL DE VEILLE PC (mid-session) : au réveil,
        // l'interface a pu se rendormir et livrer une entrée wedgée que seul un ASIOInit
        // frais, l'interface réveillée + stabilisée (~6 s : bias ADC / PLL USB), nettoie.
        // Séquence : MUTE (pas de préfixe railé routé → pas de larsen) → fermeture
        // (ASIOExit) → SETTLE → réouverture (le host single-owner re-prime) → RESET du
        // JitterBuffer self-monitor (le trou d'horloge fausserait sinon son drift →
        // distorsion persistante au casque) → démute. Délai réglable via
        // `JAMODIO_RESUME_SETTLE_MS` (défaut 6000). Windows/ASIO uniquement.
        let resumed = resume_signal.resume_count() != last_resume_seen;
        if resumed {
            let settle = crate::pipeline::resume_reinit_settle().as_millis() as u64;
            tracing::info!(
                target: "jamodio::ws",
                settle_ms = settle,
                "réveil de veille PC : re-init long-settle du driver ASIO (mute → fermeture → settle → réouverture → reset self-monitor)"
            );
            {
                let mut pl = pipeline.lock().await;
                pl.set_capture_feeding(false);
                pl.close_audio_streams_for_reset();
            }
            tokio::time::sleep(Duration::from_millis(settle)).await;
            let res = {
                let mut pl = pipeline.lock().await;
                let r = pl.rebuild_audio_streams();
                pl.reset_self_monitor(); // buffer de gigue propre sur la nouvelle horloge
                pl.set_capture_feeding(true);
                r
            };
            match res {
                Ok(()) => tracing::info!(
                    target: "jamodio::ws",
                    "réveil de veille PC : streams reconstruits"
                ),
                // Échec (mono-client pas encore relâché ?) : le filet de liveness
                // ci-dessous (streams tombés → flatline) relancera avec backoff.
                Err(e) => tracing::warn!(
                    target: "jamodio::ws",
                    error = %e,
                    "réveil de veille PC : reconstruction échouée (le filet de liveness relancera)"
                ),
            }
            last_reset_seen = reset_signal.request_count();
            last_resume_seen = resume_signal.resume_count();
            last_progress = Instant::now();
            last_repair = Some(Instant::now());
            {
                let pl = pipeline.lock().await;
                prev_cap = pl.perfstats.capture_callbacks.load(Ordering::Relaxed);
                prev_out = pl.perfstats.output_callbacks.load(Ordering::Relaxed);
            }
            continue;
        }

        // Un kAsioResetRequest est-il arrivé depuis la dernière observation ?
        let reqs = reset_signal.request_count();
        let reset_requested = reqs != last_reset_seen;

        // Les DEUX compteurs avancent ET aucun reset en attente ⇒ session saine.
        let advancing = cap > prev_cap && out > prev_out;
        prev_cap = cap;
        prev_out = out;
        if advancing && !reset_requested {
            last_progress = Instant::now();
            if degraded {
                tracing::info!(
                    target: "jamodio::ws",
                    "callbacks audio rétablis — moteur ASIO récupéré"
                );
                degraded = false;
            }
            continue;
        }

        // Réparation requise si : le driver l'a demandé, OU les streams sont tombés
        // (rebuild précédent échoué), OU flatline confirmé. (Le cold-start et le
        // réveil de veille PC sont déjà traités plus haut par le re-init long-settle.)
        let flatline = !advancing && last_progress.elapsed().as_millis() >= FLATLINE_MS;
        if !(reset_requested || !has_stream || flatline) {
            continue;
        }

        // Throttle : burst initial, puis relance lente en dégradé. Jamais de
        // boucle serrée.
        let min_gap = if degraded { DEGRADED_RETRY_INTERVAL } else { MIN_REPAIR_INTERVAL };
        if let Some(t) = last_repair {
            if t.elapsed() < min_gap {
                continue;
            }
        }
        last_repair = Some(Instant::now());

        if reset_requested {
            tracing::warn!(
                target: "jamodio::ws",
                "kAsioResetRequest reçu — reset coopératif du driver ASIO"
            );
        } else {
            tracing::warn!(
                target: "jamodio::ws",
                flatline_ms = last_progress.elapsed().as_millis() as u64,
                has_stream,
                "callbacks audio figés — reset du driver ASIO"
            );
        }

        // Reset borné avec settle + backoff (verrou pipeline relâché pendant les
        // attentes → les heartbeats browser restent servis).
        let repaired = repair_audio_streams(&pipeline).await;

        // Consomme la demande de reset/réveil traitée + fenêtre de grâce (les
        // callbacks recréés mettent quelques ms à démarrer) + re-baseline compteurs.
        last_reset_seen = reset_signal.request_count();
        last_resume_seen = resume_signal.resume_count();
        last_progress = Instant::now();
        {
            let pl = pipeline.lock().await;
            prev_cap = pl.perfstats.capture_callbacks.load(Ordering::Relaxed);
            prev_out = pl.perfstats.output_callbacks.load(Ordering::Relaxed);
        }

        match repaired {
            Ok(()) => {
                if degraded {
                    tracing::info!(
                        target: "jamodio::ws",
                        "moteur ASIO reconstruit après période dégradée"
                    );
                    degraded = false;
                }
            }
            Err(last_err) => {
                if !degraded {
                    // Transition → dégradé : un seul ERROR, actionnable (analyse).
                    tracing::error!(
                        target: "jamodio::ws",
                        asio_error = %last_err,
                        "reset ASIO en échec — driver probablement bloqué au niveau USB. \
                         Session maintenue, relance auto en arrière-plan (rebrancher \
                         l'interface relancera l'audio sans coupure réseau)."
                    );
                    degraded = true;
                } else {
                    tracing::debug!(
                        target: "jamodio::ws",
                        asio_error = %last_err,
                        "reset ASIO toujours en échec (dégradé)"
                    );
                }
            }
        }
    }
}

/// 0.5.4-2 — exécute un reset ASIO à chaud, BORNÉ. Ferme les streams (→ `ASIOExit`
/// au dernier drop = dé-init complète exigée par la spec) puis tente la
/// reconstruction avec un délai de settle initial + backoff. Relâche le verrou
/// pipeline pendant les attentes. Renvoie l'erreur ASIO du dernier essai si tous
/// échouent (le superviseur passe alors en mode dégradé sans couper la session).
async fn repair_audio_streams(pipeline: &Arc<tokio::sync::Mutex<PipelineState>>) -> Result<(), String> {
    // Délais AVANT chaque tentative de reconstruction. Le 1er (~350 ms) laisse le
    // driver USB relâcher après l'`ASIOExit` — c'est la cause des échecs immédiats
    // de reconstruction sur wedge dur (recréer sur un driver pas encore relâché
    // échoue). Les suivants montent pour absorber un blocage transitoire plus long.
    const BACKOFF_MS: [u64; 4] = [350, 600, 1200, 2500];

    // Phase 1 : fermeture (ASIOExit au dernier drop) — lock bref.
    {
        let mut pl = pipeline.lock().await;
        pl.close_audio_streams_for_reset();
    }

    let mut last_err = String::from("inconnu");
    for (i, delay) in BACKOFF_MS.iter().enumerate() {
        tokio::time::sleep(Duration::from_millis(*delay)).await;
        let res = {
            let mut pl = pipeline.lock().await;
            pl.rebuild_audio_streams()
        };
        match res {
            Ok(()) => {
                tracing::info!(
                    target: "jamodio::ws",
                    attempt = i + 1,
                    "reset ASIO : streams reconstruits"
                );
                return Ok(());
            }
            Err(e) => {
                last_err = e.to_string();
                tracing::warn!(
                    target: "jamodio::ws",
                    attempt = i + 1,
                    max = BACKOFF_MS.len(),
                    asio_error = %last_err,
                    "reset ASIO : reconstruction échouée, nouvel essai"
                );
            }
        }
    }
    Err(last_err)
}

async fn handle_message(
    msg: BrowserMessage,
    pipeline: &Arc<tokio::sync::Mutex<PipelineState>>,
) -> Vec<AgentMessage> {
    match msg {
        BrowserMessage::HelloAck { protocol_version, session_id } => {
            // Log au niveau INFO (pas debug) pour que `session_id` soit toujours
            // visible dans les logs agent même en niveau de prod par défaut.
            // C'est l'unique pivot pour croiser browser↔agent côté support.
            tracing::info!(
                target: "jamodio::ws",
                browser_protocol = protocol_version,
                agent_protocol = PROTOCOL_VERSION,
                session_id = session_id.as_deref().unwrap_or("?"),
                "browser session linked"
            );
            vec![]
        }

        BrowserMessage::GetDevices => {
            let inputs = device::list_inputs();
            let outputs = device::list_outputs();
            let audio_host = Some(crate::audio::host::kind().wire_name().to_string());
            vec![AgentMessage::Devices { inputs, outputs, audio_host }]
        }

        BrowserMessage::SelectDevices { input_id, output_id } => {
            // Setup critique (swap device) : on ATTEND le lock, jamais de drop.
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![AgentMessage::error("agent overloaded")];
            };
            pl.select_devices(input_id, output_id);
            vec![make_status(AgentState::Idle)]
        }

        BrowserMessage::StartCapture { ssrc, sfu_ip, sfu_port, payload_type: _, input_device, channel_index, stereo_start, srtp_parameters } => {
            tracing::info!(
                target: "jamodio::ws",
                ssrc,
                sfu = format!("{}:{}", sfu_ip, sfu_port),
                ?input_device,
                ?channel_index,
                ?stereo_start,
                "StartCapture"
            );
            // Validation de la destination (M-agent-2, review pré-BETA 2026-07-12).
            // Le browser fournit sfu_ip/sfu_port ; on refuse une IP invalide ou
            // manifestement bogue avant d'ouvrir le flux. En release on rejette
            // aussi loopback (jamais un vrai POP SFU depuis l'agent) ; en debug on
            // tolère (SFU local sur 127.0.0.1). L'auth d'origine (C5) reste la
            // barrière principale contre la redirection du flux micro.
            if !is_valid_sfu_dest(&sfu_ip) {
                tracing::warn!(target: "jamodio::ws", sfu = %sfu_ip, "StartCapture rejeté : destination SFU invalide");
                return vec![AgentMessage::error_keyed("invalid sfu destination", "")];
            }
            // Setup critique : on ATTEND le lock (jamais de drop → sinon tranche
            // figée jusqu'au relaunch). Erreur corrélée à la clé vide "" (comme
            // LocalPort/CaptureError) → pas de reject collatéral côté browser.
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![AgentMessage::error_keyed("agent overloaded", "")];
            };
            // Le browser passe l'id du device directement dans start-capture
            // (le plus fiable — select-devices pouvait ne jamais arriver).
            // L'id est strict ({idx}:{name}) — pas de fuzzy, pas de fallback.
            // ⚠ Bug fix : set_input_device() (pas select_devices) pour ne pas
            // écraser l'output_device_id précédemment configuré.
            if input_device.is_some() {
                pl.set_input_device(input_device.clone());
            }
            // La liveness des callbacks ASIO (cold-start ET mort en cours de
            // session) est surveillée en continu par `audio_liveness_supervisor`,
            // qui recrée les streams au besoin (cf. la fonction).
            match pl.start_capture(ssrc, sfu_ip.clone(), sfu_port, 111, channel_index, stereo_start, srtp_parameters).await {
                Ok((local_port, agent_srtp, info)) => {
                    // Deux messages : LocalPort (chaîne SRTP avec le SFU) +
                    // CaptureStarted (confirmation explicite côté browser
                    // que le device demandé est bien celui ouvert).
                    vec![
                        AgentMessage::LocalPort {
                            producer_id: String::new(),
                            port: local_port,
                            srtp_parameters: agent_srtp,
                        },
                        AgentMessage::CaptureStarted {
                            device_id: info.device_id,
                            device_name: info.device_name,
                            channels: info.channels,
                            native_sample_rate: info.native_sample_rate,
                        },
                    ]
                }
                Err(crate::pipeline::CaptureStartError::InputDeviceNotFound { requested }) => {
                    tracing::warn!(
                        target: "jamodio::ws",
                        ?requested,
                        "StartCapture rejected: input device not found"
                    );
                    vec![AgentMessage::CaptureError {
                        reason: "input-device-not-found".into(),
                        requested_device: requested,
                        detail: None,
                    }]
                }
                Err(crate::pipeline::CaptureStartError::OutputDeviceNotFound { requested }) => {
                    tracing::warn!(
                        target: "jamodio::ws",
                        ?requested,
                        "StartCapture rejected: output device not found"
                    );
                    vec![AgentMessage::CaptureError {
                        reason: "output-device-not-found".into(),
                        requested_device: requested,
                        detail: None,
                    }]
                }
                Err(crate::pipeline::CaptureStartError::Other(msg)) => {
                    // ASIO est mono-client : l'échec d'ouverture n°1 est un
                    // driver déjà tenu par une autre app (DAW). On le dit
                    // EXPLICITEMENT au browser (reason dédiée) au lieu d'un
                    // Error générique — jamais de fallback WASAPI silencieux
                    // qui mentirait sur la latence (cf. PLAN-ASIO-WINDOWS A5).
                    if crate::audio::host::kind() == crate::audio::host::HostKind::Asio {
                        tracing::warn!(
                            target: "jamodio::ws",
                            detail = %msg,
                            "StartCapture failed on ASIO host (driver occupé ?)"
                        );
                        vec![AgentMessage::CaptureError {
                            reason: "asio-open-failed".into(),
                            requested_device: input_device,
                            detail: Some(msg),
                        }]
                    } else {
                        // Corrélé à la clé vide "" (comme LocalPort/CaptureError)
                        // → le browser rejette seulement la requête StartCapture.
                        vec![AgentMessage::error_keyed(msg, "")]
                    }
                }
            }
        }

        // ─── Talkback (Lot 2, v0.5.7) — 2e producteur voix via l'agent ───
        BrowserMessage::StartVoiceCapture { ssrc, sfu_ip, sfu_port, payload_type: _, channel_index, srtp_parameters } => {
            tracing::info!(
                target: "jamodio::ws",
                ssrc, channel_index,
                sfu = format!("{}:{}", sfu_ip, sfu_port),
                "StartVoiceCapture"
            );
            // Même garde de destination que l'instrument. Erreurs corrélées à la
            // clé "voice" (comme le LocalPort renvoyé) → le browser ne rejette
            // QUE la requête voix en vol, jamais l'instrument.
            if !is_valid_sfu_dest(&sfu_ip) {
                tracing::warn!(target: "jamodio::ws", sfu = %sfu_ip, "StartVoiceCapture rejeté : destination SFU invalide");
                return vec![AgentMessage::error_keyed("invalid sfu destination", "voice")];
            }
            // Setup critique (greffe d'un flux) : on ATTEND le lock.
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![AgentMessage::error_keyed("agent overloaded", "voice")];
            };
            match pl.start_voice_capture(ssrc, sfu_ip, sfu_port, 111, channel_index, srtp_parameters).await {
                Ok((local_port, agent_srtp)) => vec![AgentMessage::LocalPort {
                    producer_id: "voice".into(),
                    port: local_port,
                    srtp_parameters: agent_srtp,
                }],
                Err(msg) => {
                    tracing::warn!(target: "jamodio::ws", detail = %msg, "StartVoiceCapture rejeté");
                    vec![AgentMessage::error_keyed(msg, "voice")]
                }
            }
        }

        BrowserMessage::StopVoiceCapture => {
            // Toggle talkback OFF / device-canal changé. Fiabilité > vitesse
            // (une voix restée ouverte est pire qu'une courte attente) → wait.
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![];
            };
            pl.stop_voice_capture();
            vec![]
        }

        BrowserMessage::SetVoiceGain { gain } => {
            // Hot-path idempotent (l'auto-mute le pilote à ~10 Hz) : try-lock,
            // skip OK si contention (la prochaine cible rattrapera). L'atomique
            // clampe/écrit sans toucher au thread RT voix.
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.set_voice_gain(gain);
            vec![]
        }

        BrowserMessage::AddStream { producer_id, sfu_ip, sfu_port, payload_type: _, srtp_parameters, .. } => {
            tracing::info!(
                target: "jamodio::ws",
                producer = &producer_id[..8.min(producer_id.len())],
                sfu = format!("{}:{}", sfu_ip, sfu_port),
                "AddStream"
            );
            // Setup critique du montage d'un flux entrant (join d'un peer) : on
            // ATTEND le lock, jamais de drop (sinon flux jamais monté → peer muet,
            // cf. symptôme A "ghost/orphan"). Erreurs corrélées au producer_id
            // → le browser rejette SEULEMENT cette requête (pas ses voisines).
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![AgentMessage::error_keyed("agent overloaded", producer_id)];
            };
            match pl.add_stream(producer_id.clone(), sfu_ip, sfu_port, srtp_parameters).await {
                Ok((local_port, agent_srtp)) => vec![AgentMessage::LocalPort {
                    producer_id,
                    port: local_port,
                    srtp_parameters: agent_srtp,
                }],
                Err(e) => vec![AgentMessage::error_keyed(e, producer_id)],
            }
        }

        BrowserMessage::RemoveStream { producer_id } => {
            // Setup critique (teardown d'un flux au leave d'un peer) : on ATTEND.
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![AgentMessage::error_keyed("agent overloaded", producer_id)];
            };
            pl.remove_stream(&producer_id);
            vec![]
        }

        BrowserMessage::SetVolume { producer_id, volume } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_volume(&producer_id, volume);
            vec![]
        }

        BrowserMessage::SetBuffer { target_ms } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_target_ms_all(target_ms as usize);
            tracing::info!(target: "jamodio::ws", target_ms, "SetBuffer");
            vec![]
        }

        BrowserMessage::SetSelfMonitorVolume { volume } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            // Clamp défensif côté agent (le mixer clampe déjà dans
            // [0, 1.5] mais on filtre les NaN ici). 0 = silence (défaut).
            let v = if volume.is_finite() { volume.max(0.0) } else { 0.0 };
            pl.mixer.lock().set_self_monitor_volume(v);
            tracing::info!(target: "jamodio::ws", volume = v, "SetSelfMonitorVolume");
            vec![]
        }

        BrowserMessage::GetStats => {
            // GetStats est appelé en heartbeat (toutes les 1.5 s). Si on ne peut
            // pas acquérir le lock dans 200 ms, on répond Error pour que le
            // browser sache que l'agent est saturé (au lieu de timeout watchdog 3 s).
            // Hot-path (heartbeat 1.5 s) : skip OK, mais erreur NON corrélée
            // (`error()`) → avec le fix P1 browser, elle ne rejette plus les
            // requêtes de setup en vol (plus d'amplificateur).
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![AgentMessage::error("agent overloaded")];
            };
            let is_capturing = matches!(pl.state, AgentState::Capturing);
            let stream_count = pl.recv_stops.len();
            // L'UI agent affiche le nom lisible (pas l'id complet `{idx}:{name}`).
            // On extrait la part nom de l'id sélectionné.
            let device_name = pl.selected_input_id().and_then(|id| {
                id.split_once(':').map(|(_, n)| n.to_string()).or(Some(id))
            });

            // Real latency from CPAL buffer: samples / 48000 * 1000.
            //
            // 0.5.4-4 — on privilégie la taille RÉELLE mesurée au 1er callback
            // (`perfstats.input_frames`/`output_frames`, frames/canal ; 0 = pas
            // encore mesuré). C'est la latence HONNÊTE : depuis qu'on défère à la
            // taille préférée du driver sur ASIO (`BufferSize::Default`), la valeur
            // DEMANDÉE est inconnue (`input_buffer_samples = None`) → seule la
            // mesure dit la vérité. Corrige aussi la sur-estimation Mac historique
            // (on demandait 128, CoreAudio servait 64). Ordre de priorité :
            //   1) taille mesurée au callback, 2) taille demandée (Fixed),
            //   3) fallback conservateur 10 ms (= 480/48, valeur WASAPI shared).
            // Les champs wire `inputBufferMs`/`outputBufferMs` reflètent désormais
            // la mesure dès qu'elle est dispo (présents même sur ASIO).
            const DEFAULT_BUF_MS_FALLBACK: f32 = 10.0;
            use std::sync::atomic::Ordering as AtomicOrdering;
            let measured_in = pl.perfstats.input_frames.load(AtomicOrdering::Relaxed);
            let measured_out = pl.perfstats.output_frames.load(AtomicOrdering::Relaxed);
            let input_buf_ms_opt: Option<f32> = if measured_in > 0 {
                Some(measured_in as f32 / 48.0)
            } else {
                pl.input_buffer_samples.map(|n| n as f32 / 48.0)
            };
            let output_buf_ms_opt: Option<f32> = if measured_out > 0 {
                Some(measured_out as f32 / 48.0)
            } else {
                pl.output_buffer_samples.map(|n| n as f32 / 48.0)
            };
            let input_buf_ms_est = input_buf_ms_opt.unwrap_or(DEFAULT_BUF_MS_FALLBACK);
            let output_buf_ms_est = output_buf_ms_opt.unwrap_or(DEFAULT_BUF_MS_FALLBACK);
            // Latence algorithmique Opus = lookahead encodeur. En mode
            // RESTRICTED_LOWDELAY (cf. MusicEncoder) le lookahead vaut
            // exactement une frame de 120 samples = 2,5 ms — invariant verrouillé
            // par le test `lowdelay_lookahead_vs_audio`. (En mode Audio il valait
            // 6,5 ms : la télémétrie d'avant sous-estimait donc la latence.)
            let opus_ms: f32 = 2.5;

            let mixer = pl.mixer.lock();
            let underruns = mixer.total_underruns();
            let jitter_target_ms = mixer.mean_target_ms();
            drop(mixer);

            // Total = input_buf + opus_enc + opus_dec + jitter + output_buf
            // (cf. doc `Stats::total_latency_ms`). Utilise les estimations
            // input/output séparées au lieu du double-buf hérité — corrige
            // le calcul faux sur Windows shared où in et out peuvent diverger.
            let total_latency_ms = if is_capturing {
                input_buf_ms_est + opus_ms + opus_ms + jitter_target_ms + output_buf_ms_est
            } else {
                0.0
            };

            vec![
                make_status(pl.state.clone()),
                AgentMessage::Stats {
                    device: device_name,
                    capture_latency_ms: if is_capturing { input_buf_ms_est + opus_ms } else { 0.0 },
                    playback_latency_ms: if is_capturing { output_buf_ms_est } else { 0.0 },
                    // bufferMs (rétrocompat) = input (sémantique historique
                    // utilisée par les browsers pré-Q3 qui le sommaient avec
                    // opus_ms pour estimer la capture).
                    buffer_ms: if is_capturing { input_buf_ms_est } else { 0.0 },
                    input_buffer_ms: input_buf_ms_opt,
                    output_buffer_ms: output_buf_ms_opt,
                    jitter_target_ms,
                    total_latency_ms,
                    streams: stream_count,
                    underruns,
                },
            ]
        }

        BrowserMessage::Stop => {
            // Setup critique (sortie de studio) : on ATTEND pour garantir un
            // leave_session propre (park ASIO) plutôt que de figer l'état.
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![AgentMessage::error("agent overloaded")];
            };
            // 0.5.4-5 — sortie de studio : PARK sur ASIO (driver gardé chaud →
            // rejoin instantané, anti-churn Focusrite), stop_all complet ailleurs.
            pl.leave_session();
            vec![make_status(AgentState::Idle)]
        }

        BrowserMessage::SetInputCut { cut } => {
            let Some(mut pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.set_input_cut(cut);
            vec![]
        }

        BrowserMessage::SetMasterVolume { volume } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_master_gain(volume);
            vec![]
        }

        BrowserMessage::SetPan { producer_id, pan } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_pan(&producer_id, pan);
            vec![]
        }

        BrowserMessage::SetDim { factor } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().set_dim(factor);
            vec![]
        }

        // ─── Option B — référence (métronome) via l'agent ─────────────────
        BrowserMessage::ReferenceClockPing { ping_id, client_send_ms } => {
            // Réponse IMMÉDIATE : l'ancre échantillon↔mural + l'horloge agent.
            // `outMs` = latence de sortie CONNUE (buffer CPAL mesuré) ; c'est ce
            // que Chrome ne sait pas sur WASAPI. Le browser gate déjà l'Option B
            // sur `audioHost ∈ {asio, coreaudio}` → sur WASAPI il ignore ce pong
            // (fallback Option A). Cf. B0 §3.4.
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            use std::sync::atomic::Ordering as AtomicOrdering;
            const DEFAULT_BUF_MS_FALLBACK: f32 = 10.0;
            let measured_out = pl.perfstats.output_frames.load(AtomicOrdering::Relaxed);
            let out_ms: f32 = if measured_out > 0 {
                measured_out as f32 / 48.0
            } else {
                pl.output_buffer_samples
                    .map(|n| n as f32 / 48.0)
                    .unwrap_or(DEFAULT_BUF_MS_FALLBACK)
            };
            let anchor = pl.mixer.lock().output_anchor();
            drop(pl);
            // Stampé au plus près de la réception (même epoch que `anchor.mono_ms`).
            let agent_mono_ms = jamodio_audio_core::sync::clock::mono_now_ms();
            vec![AgentMessage::ReferenceClockPong {
                ping_id,
                client_send_ms,
                agent_mono_ms,
                anchor_frame: anchor.frame,
                anchor_emerge_mono_ms: anchor.mono_ms + out_ms as f64,
                sample_rate: 48_000,
                out_ms,
            }]
        }

        BrowserMessage::ReferenceConfig {
            enabled,
            volume,
            pan,
            bpm,
            beats_per_accent,
            sound,
            figure,
            anchor_beat_frame,
            anchor_beat_index,
        } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            use jamodio_audio_core::mixer::reference::{Figure, MetroSound};
            pl.mixer.lock().set_reference_config(
                enabled,
                volume,
                pan,
                bpm,
                beats_per_accent,
                MetroSound::from_wire(&sound),
                Figure::from_wire(&figure),
                anchor_beat_frame,
                anchor_beat_index,
            );
            tracing::debug!(
                target: "jamodio::ws",
                enabled, bpm, beats_per_accent,
                anchor_beat_frame, anchor_beat_index,
                "ReferenceConfig"
            );
            vec![]
        }

        BrowserMessage::ReferenceGrid { anchor_beat_frame, anchor_beat_index } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer
                .lock()
                .set_reference_grid(anchor_beat_frame, anchor_beat_index);
            vec![]
        }

        BrowserMessage::ReferenceStop => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().reference_stop();
            tracing::debug!(target: "jamodio::ws", "ReferenceStop");
            vec![]
        }

        // ─── Option B / B4 — backing track via l'agent ────────────────────
        BrowserMessage::ReferenceBackingBegin { total_frames } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().backing_begin(total_frames as usize);
            tracing::debug!(target: "jamodio::ws", total_frames, "ReferenceBackingBegin");
            vec![]
        }

        BrowserMessage::ReferenceBackingChunk { data_b64 } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            // base64 → PCM int16 LE → f32 (stéréo entrelacé).
            let b64 = base64::engine::general_purpose::STANDARD;
            match b64.decode(data_b64.as_bytes()) {
                Ok(bytes) => {
                    let mut samples = Vec::with_capacity(bytes.len() / 2);
                    for pair in bytes.chunks_exact(2) {
                        let s = i16::from_le_bytes([pair[0], pair[1]]);
                        samples.push(s as f32 / 32768.0);
                    }
                    pl.mixer.lock().backing_push(&samples);
                }
                Err(e) => {
                    tracing::warn!(target: "jamodio::ws", error = %e, "ReferenceBackingChunk base64 invalide");
                }
            }
            vec![]
        }

        BrowserMessage::ReferenceBackingEnd => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().backing_end();
            tracing::debug!(target: "jamodio::ws", "ReferenceBackingEnd");
            vec![]
        }

        BrowserMessage::ReferenceBackingUnload => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().backing_unload();
            vec![]
        }

        BrowserMessage::ReferenceBackingPlay { anchor_backing_frame, anchor_output_frame } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().backing_play(anchor_backing_frame, anchor_output_frame);
            vec![]
        }

        BrowserMessage::ReferenceBackingPause => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().backing_pause();
            vec![]
        }

        BrowserMessage::ReferenceBackingSeek { anchor_backing_frame, anchor_output_frame } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().backing_seek(anchor_backing_frame, anchor_output_frame);
            vec![]
        }

        BrowserMessage::ReferenceBackingSync { anchor_backing_frame, anchor_output_frame } => {
            let Some(pl) = try_lock_pipeline(pipeline).await else {
                return vec![];
            };
            pl.mixer.lock().backing_sync(anchor_backing_frame, anchor_output_frame);
            vec![]
        }

        // Sprint INSERT — 6 handlers plugin (AU sur macOS, VST3 sur Windows).
        // Sur les OS sans host plugin (linux test), fallback "not supported".
        BrowserMessage::ListPlugins => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // P2 — ne JAMAIS laisser l'UI bloquée en « Scan… » : on ATTEND le
                // lock, et si l'agent est vraiment contendu on répond quand même
                // un PluginList `scanning:true` (au lieu de `vec![]` = aucun
                // message → l'UI restait figée). Le browser reçoit un signal
                // explicite « toujours en cours » et repolle.
                let Some(pl) = lock_pipeline_wait(pipeline).await else {
                    return vec![AgentMessage::PluginList { items: vec![], scanning: true }];
                };
                let (items, scanning) = pl.list_instrument_plugins();
                vec![AgentMessage::PluginList { items, scanning }]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![AgentMessage::PluginList { items: vec![], scanning: false }]
            }
        }

        BrowserMessage::LoadInstrumentPlugin { plugin_ref } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // Chantier A — on ne tient PAS le lock PipelineState pendant le
                // load natif (0,4–4 s) : on clone le bundle d'Arcs (cheap) puis
                // on relâche immédiatement. Le thread audio passe en dry
                // (handle=None + try_lock) et perfstats_task n'est pas bloqué.
                // Setup critique : on ATTEND le lock COURT (juste cloner le
                // bundle d'Arcs). Le load natif lent (0,4–4 s) se fait ensuite
                // HORS lock (spawn_blocking) — cf. plus bas.
                let ctrl = {
                    let Some(pl) = lock_pipeline_wait(pipeline).await else {
                        return vec![AgentMessage::InstrumentPluginError {
                            message: "agent overloaded".into(),
                        }];
                    };
                    pl.plugin_control()
                };
                // Sérialise vs un autre load/unload en cours, puis exécute le
                // load natif sur le pool blocking (ne bloque pas le runtime
                // tokio ni les autres handlers/tasks).
                let _ops = plugin_ops_lock().lock().await;
                let pref = plugin_ref.clone();
                let result =
                    tokio::task::spawn_blocking(move || ctrl.load(&pref)).await;
                // `spawn_blocking` ne panique que si la task panique : on traite
                // le JoinError comme une erreur de chargement plutôt que de
                // propager un panic dans le handler WS.
                let result = match result {
                    Ok(inner) => inner,
                    Err(join_err) => Err(format!("plugin load task failed: {join_err}")),
                };
                match result {
                    Ok((name, latency_samples, has_editor)) => {
                        vec![AgentMessage::InstrumentPluginLoaded {
                            name,
                            plugin_ref: plugin_ref.clone(),
                            latency_samples,
                            has_editor,
                            // Reset au load — l'agent met bypass à false dans
                            // PluginControl::load, on miroite pour le wire.
                            bypass: false,
                        }]
                    }
                    Err(message) => {
                        // v0.2.23 — Log enrichi avec l'identifiant plugin pour
                        // permettre le diag à distance sans Chrome console
                        // (cf. bug Yannick 2026-05-13). Format diffère AU/VST3.
                        let ident: String = match &plugin_ref {
                            jamodio_audio_core::plugin_host::PluginRef::Au {
                                au_type, subtype, manufacturer,
                            } => format!("AU {au_type}/{subtype}/{manufacturer}"),
                            jamodio_audio_core::plugin_host::PluginRef::Vst3 {
                                path, uid,
                            } => format!("VST3 {path} (uid={uid})"),
                        };
                        tracing::error!(
                            target: "jamodio::ws",
                            plugin = %ident,
                            error = %message,
                            "LoadInstrumentPlugin failed"
                        );
                        vec![AgentMessage::InstrumentPluginError { message }]
                    }
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = plugin_ref;
                vec![AgentMessage::InstrumentPluginError {
                    message: "INSERT plugins not supported on this platform".into(),
                }]
            }
        }

        BrowserMessage::UnloadInstrumentPlugin => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // Chantier A — même principe que le load : clone le bundle,
                // relâche le lock PipelineState, teardown natif sur le pool
                // blocking (le thread audio est déjà passé en dry dès que
                // PluginControl::unload pose handle=None).
                // Setup critique : on ATTEND le lock COURT (clone du bundle),
                // le teardown natif lent se fait ensuite HORS lock.
                let ctrl = {
                    let Some(pl) = lock_pipeline_wait(pipeline).await else {
                        return vec![];
                    };
                    pl.plugin_control()
                };
                let _ops = plugin_ops_lock().lock().await;
                let _ = tokio::task::spawn_blocking(move || ctrl.unload()).await;
                vec![AgentMessage::InstrumentPluginUnloaded]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![AgentMessage::InstrumentPluginUnloaded]
            }
        }

        BrowserMessage::SetInstrumentPluginBypass { bypass } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                pl.set_instrument_plugin_bypass(bypass);
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = bypass;
                vec![]
            }
        }

        BrowserMessage::OpenInstrumentPluginEditor => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                if let Err(message) = pl.open_instrument_plugin_editor() {
                    return vec![AgentMessage::InstrumentPluginError { message }];
                }
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![]
            }
        }

        BrowserMessage::CloseInstrumentPluginEditor => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                let _ = pl.close_instrument_plugin_editor();
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![]
            }
        }

        // Sprint INSERT instruments (S2) — discovery + selection MIDI input.
        BrowserMessage::ListMidiDevices => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let devices = crate::audio::midi::list_devices()
                    .into_iter()
                    .map(|d| jamodio_audio_core::protocol::MidiDeviceWire {
                        id: d.id,
                        name: d.name,
                        is_default: d.is_default,
                    })
                    .collect();
                vec![AgentMessage::MidiDeviceList { devices }]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                vec![AgentMessage::MidiDeviceList { devices: vec![] }]
            }
        }

        BrowserMessage::SetInputSource { source, midi_device_id } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                // Setup critique (bascule MIDI ↔ audio, symptôme B) : on ATTEND
                // le lock pour ne pas laisser la tranche figée jusqu'au relaunch.
                let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                    return vec![AgentMessage::InputSourceError {
                        message: "agent overloaded".into(),
                    }];
                };
                let new_source = match source.as_str() {
                    "audio" => crate::pipeline::InputSource::Audio,
                    "midi" => match midi_device_id {
                        Some(id) => crate::pipeline::InputSource::Midi(id),
                        None => {
                            return vec![AgentMessage::InputSourceError {
                                message: "midiDeviceId required when source=midi".into(),
                            }];
                        }
                    },
                    _ => {
                        return vec![AgentMessage::InputSourceError {
                            message: format!("unknown source: {source}"),
                        }];
                    }
                };

                match pl.set_input_source(new_source.clone()) {
                    Ok(()) => {
                        let (src_str, dev_id, dev_name) = match &new_source {
                            crate::pipeline::InputSource::Audio => {
                                ("audio".to_string(), None, None)
                            }
                            crate::pipeline::InputSource::Midi(id) => {
                                let name = crate::audio::midi::list_devices()
                                    .into_iter()
                                    .find(|d| &d.id == id)
                                    .map(|d| d.name);
                                ("midi".to_string(), Some(id.clone()), name)
                            }
                        };
                        tracing::info!(
                            target: "jamodio::midi",
                            source = %src_str,
                            device_id = ?dev_id,
                            "input source changed"
                        );
                        vec![AgentMessage::InputSourceChanged {
                            source: src_str,
                            midi_device_id: dev_id,
                            midi_device_name: dev_name,
                        }]
                    }
                    Err(message) => {
                        tracing::error!(
                            target: "jamodio::midi",
                            error = %message,
                            "set_input_source failed"
                        );
                        vec![AgentMessage::InputSourceError { message }]
                    }
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = (source, midi_device_id);
                vec![AgentMessage::InputSourceError {
                    message: "MIDI input not supported on this platform".into(),
                }]
            }
        }

        BrowserMessage::PlayMidiNote { status, data1, data2 } => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                let Some(pl) = try_lock_pipeline(pipeline).await else {
                    return vec![];
                };
                let handle_opt = *pl.instrument_plugin_handle.lock();
                if let Some(handle) = handle_opt {
                    // Clavier HTML virtuel (browser → WS → agent) : pas de
                    // source temporelle précise (mousedown/keydown soumis au
                    // lag UI + transit WebSocket). `frame_offset: 0` est
                    // l'approximation honnête — l'event est joué au tout
                    // début du prochain bloc rendu par dispatch_midi_only.
                    // Le path MIDI USB physique passe par `midi.rs` qui, lui,
                    // est sample-accurate via `CapturedMidiEvent::captured_at`.
                    let event = jamodio_audio_core::plugin_host::MidiEvent {
                        frame_offset: 0,
                        data: [status, data1, data2],
                    };
                    // Mac (AuHost) ET Win (Vst3Host) implémentent tous deux
                    // `dispatch_midi_only` avec la même signature → call
                    // OS-agnostic via le champ `plugin_host` aliasé.
                    // try_lock (pas lock) : si le plugin_host est tenu par un
                    // load/unload en cours (jusqu'à plusieurs secondes), on NE
                    // bloque PAS le worker tokio — on abandonne la note. Une
                    // note de clavier HTML perdue pendant un chargement de
                    // plugin est acceptable ; bloquer le runtime ne l'est pas.
                    if let Some(mut host) = pl.plugin_host.try_lock() {
                        let _ = host.dispatch_midi_only(handle, &[event]);
                    }
                }
                vec![]
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let _ = (status, data1, data2);
                vec![]
            }
        }

        BrowserMessage::StartRecording { stems } => {
            // Setup critique (armement REC) : on ATTEND, et on rend une
            // RecordingError (corrélée dans l'UI REC) plutôt qu'un Error générique.
            let Some(mut pl) = lock_pipeline_wait(pipeline).await else {
                return vec![AgentMessage::RecordingError { message: "agent overloaded".into() }];
            };
            // Convertit le wire StemSpec → record::StemSpec (même contenu,
            // module différent pour découpler le protocol du core record).
            let specs: Vec<StemSpec> = stems.iter().map(|s| StemSpec {
                role: s.role.clone(),
                peer_id: s.peer_id.clone(),
                peer_name: s.peer_name.clone(),
            }).collect();
            tracing::info!(target: "jamodio::ws", count = specs.len(), "StartRecording");
            match pl.start_recording(specs) {
                Ok(armed) => {
                    let wire: Vec<RecordStemSpec> = armed.iter().map(|s| RecordStemSpec {
                        role: s.role.clone(),
                        peer_id: s.peer_id.clone(),
                        peer_name: s.peer_name.clone(),
                    }).collect();
                    vec![AgentMessage::RecordingStarted { stems: wire }]
                }
                Err(msg) => {
                    tracing::error!(target: "jamodio::ws", error = %msg, "start_recording failed");
                    vec![AgentMessage::RecordingError { message: msg }]
                }
            }
        }

        BrowserMessage::StopRecording => {
            // stop_recording bloque jusqu'à finalize (timeout 30s côté handle).
            // On délègue à spawn_blocking pour ne pas bloquer le runtime tokio
            // pendant l'encodage final + lock pipeline.
            let pipeline = pipeline.clone();
            let files_opt = tokio::task::spawn_blocking(move || {
                // Lock COURT : on extrait juste le handle, puis on RELÂCHE le
                // lock pipeline avant le finalize (jusqu'à 30s). Sinon tous les
                // autres handlers (heartbeat GetStats…) voient "overloaded"
                // pendant tout le finalize. Le handle.stop() tourne hors lock.
                let handle = {
                    let mut pl = pipeline.blocking_lock();
                    pl.take_recorder()
                };
                match handle {
                    Some(h) => h.stop(),
                    None => Vec::new(),
                }
            })
            .await;
            let files = match files_opt {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(target: "jamodio::ws", error = %e, "stop_recording join error");
                    return vec![AgentMessage::RecordingError {
                        message: format!("stop task failed: {e}"),
                    }];
                }
            };
            tracing::info!(
                target: "jamodio::ws",
                count = files.len(),
                bytes_total = files.iter().map(|f| f.data.len()).sum::<usize>(),
                "StopRecording — encoding base64",
            );
            // Base64 encode chaque fichier. Un seul message à la fin évite
            // le chunking côté browser ; WS frame supporte les payloads MB.
            let b64 = base64::engine::general_purpose::STANDARD;
            let wire_files: Vec<RecordedFileWire> = files.into_iter().map(|f| RecordedFileWire {
                role: f.spec.role,
                peer_id: f.spec.peer_id,
                peer_name: f.spec.peer_name,
                mime_type: "audio/ogg".into(),
                extension: "opus".into(),
                data_b64: b64.encode(&f.data),
            }).collect();
            vec![AgentMessage::RecordingDone { files: wire_files }]
        }

        BrowserMessage::GetLogsArchive { max_days, max_bytes } => {
            // I/O disque dans une tâche bloquante pour ne pas geler le runtime
            // tokio (lecture de plusieurs MB peut prendre 50-200ms sur HDD).
            let days = max_days.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_DAYS);
            let bytes = max_bytes.unwrap_or(crate::logging::DEFAULT_LOG_ARCHIVE_BYTES);
            let res = tokio::task::spawn_blocking(move || {
                let (content, files, truncated) = crate::logging::collect_recent_logs(days, bytes);
                let log_dir = crate::logging::log_dir().to_string_lossy().into_owned();
                (content, files, truncated, log_dir)
            })
            .await;
            match res {
                Ok((content, files, truncated, log_dir)) => {
                    tracing::info!(
                        target: "jamodio::support",
                        files = files.len(),
                        bytes = content.len(),
                        truncated,
                        "GetLogsArchive served"
                    );
                    vec![AgentMessage::LogsArchive {
                        content,
                        files,
                        truncated,
                        log_dir,
                    }]
                }
                Err(e) => vec![AgentMessage::error(
                    format!("logs archive task failed: {e}"),
                )],
            }
        }

        // Restart et RelaunchNow sont interceptés en amont dans
        // handle_one_message (ils ont besoin du AppHandle, pas seulement du
        // pipeline) et n'atteignent jamais ce match. Bras défensifs pour
        // l'exhaustivité — no-op si jamais routés ici.
        BrowserMessage::Restart | BrowserMessage::RelaunchNow => vec![],
    }
}
