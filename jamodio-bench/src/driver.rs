//! Le pilote : parle à l'Audio Engine comme le fait l'onglet du studio.
//!
//! Mêmes messages, mêmes champs (vérifié par les tests contre le type
//! `BrowserMessage` que l'agent désérialise). L'agent n'a qu'UN propriétaire :
//! un onglet de studio ouvert est déconnecté quand le banc se présente.
//!
//! Aucune erreur n'est avalée : un refus de l'agent arrête le banc avec son
//! message, au lieu de mesurer une session qui ne tourne pas.

use futures::{SinkExt, StreamExt};
use jamodio_audio_core::net::srtp::SrtpParameters;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// Origine annoncée : celle du studio en production. L'agent n'accepte que ses
/// origines connues ; un programme local qui s'annonce ainsi n'obtient rien de
/// plus que ce qu'il pourrait déjà faire sur la machine.
const ORIGIN: &str = "https://jamodio.com";
/// Cadence du battement de cœur du studio (`agent-bridge.js`) : l'agent coupe un
/// client promu silencieux depuis 5 s.
const HEARTBEAT: Duration = Duration::from_millis(1500);
/// Délai maximal d'une réponse de l'agent (ouverture d'une carte ASIO comprise).
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

/// Connexion au WebSocket de l'agent.
pub struct AgentLink {
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    /// Messages `perf-stats`, dans l'ordre d'arrivée.
    pub perf: mpsc::UnboundedReceiver<Value>,
    /// Erreurs de l'agent sans demande en attente (ex. arrêt de capture en
    /// cours de session) : le banc les rapporte.
    pub errors: mpsc::UnboundedReceiver<String>,
    /// `hello` de l'agent : version, OS.
    pub hello: Value,
}

/// Clé d'attente d'un `local-port` : `""` = capture instrument, `"voice"` =
/// capture voix, sinon l'identifiant du flux reçu.
fn port_key(producer_id: &str) -> String {
    format!("local-port:{producer_id}")
}

impl AgentLink {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let mut req = url.into_client_request().map_err(|e| format!("adresse {url} : {e}"))?;
        req.headers_mut().insert("Origin", HeaderValue::from_static(ORIGIN));
        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|e| format!("Audio Engine injoignable sur {url} : {e} (est-il lancé ?)"))?;
        let (mut sink, mut stream) = ws.split();

        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (perf_tx, perf) = mpsc::unbounded_channel();
        let (err_tx, errors) = mpsc::unbounded_channel();
        let (hello_tx, hello_rx) = oneshot::channel();
        {
            let (pending, out) = (pending.clone(), out.clone());
            let mut hello_tx = Some(hello_tx);
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let Message::Text(text) = msg else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    dispatch(v, &pending, &out, &perf_tx, &err_tx, &mut hello_tx);
                }
                // Connexion perdue : toute attente échoue explicitement.
                for (_, tx) in pending.lock().unwrap().drain() {
                    let _ = tx.send(Err("connexion à l'Audio Engine perdue".into()));
                }
                let _ = err_tx.send("connexion à l'Audio Engine perdue".into());
            });
        }
        let hello = tokio::time::timeout(REPLY_TIMEOUT, hello_rx)
            .await
            .map_err(|_| "l'Audio Engine n'a pas dit bonjour".to_string())?
            .map_err(|_| "connexion fermée avant le bonjour".to_string())?;

        // Battement de cœur, comme le studio.
        {
            let out = out.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(HEARTBEAT);
                loop {
                    tick.tick().await;
                    if out.send(Message::Text(msg_get_stats().to_string())).is_err() {
                        break;
                    }
                }
            });
        }
        Ok(Self { out, pending, perf, errors, hello })
    }

    fn send(&self, v: Value) -> Result<(), String> {
        self.out
            .send(Message::Text(v.to_string()))
            .map_err(|_| "connexion à l'Audio Engine fermée".to_string())
    }

    async fn request(&self, v: Value, key: String) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key.clone(), tx);
        self.send(v)?;
        tokio::time::timeout(REPLY_TIMEOUT, rx)
            .await
            .map_err(|_| format!("pas de réponse de l'Audio Engine ({key})"))?
            .map_err(|_| "connexion fermée".to_string())?
    }

    pub async fn devices(&self) -> Result<Value, String> {
        self.request(json!({ "type": "get-devices" }), "devices".into()).await
    }

    pub fn select_devices(&self, input: Option<&str>, output: Option<&str>) -> Result<(), String> {
        self.send(msg_select_devices(input, output))
    }

    /// Lance la capture instrument, envoyée au transport montant du banc.
    /// Rend les clés de l'agent.
    pub async fn start_capture(&self, p: &CaptureParams<'_>) -> Result<SrtpParameters, String> {
        let started = {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert("capture-started".into(), tx);
            rx
        };
        let port = self.request(msg_start_capture(p), port_key("")).await?;
        tokio::time::timeout(REPLY_TIMEOUT, started)
            .await
            .map_err(|_| "capture non démarrée (pas de capture-started)".to_string())?
            .map_err(|_| "connexion fermée".to_string())??;
        keys_of(&port)
    }

    pub async fn start_voice(&self, p: &VoiceParams<'_>) -> Result<SrtpParameters, String> {
        let port = self.request(msg_start_voice(p), port_key("voice")).await?;
        keys_of(&port)
    }

    pub async fn add_stream(&self, p: &StreamParams<'_>) -> Result<SrtpParameters, String> {
        let port = self.request(msg_add_stream(p), port_key(p.producer_id)).await?;
        keys_of(&port)
    }

    /// Liste des plugins connus de l'agent. Un inventaire en cours rend une
    /// liste partielle : on attend qu'il finisse (2 min au plus), plutôt que de
    /// conclure « absent » sur une liste incomplète.
    pub async fn plugins(&self) -> Result<Vec<Value>, String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let list = self.request(json!({ "type": "list-plugins" }), "plugin-list".into()).await?;
            let items = list["items"].as_array().cloned().unwrap_or_default();
            if list["scanning"].as_bool() != Some(true) {
                return Ok(items);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("inventaire des plugins toujours en cours après 2 min".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Charge un plugin inséré sur l'instrument, comme le studio. Rend la
    /// confirmation de l'agent (nom, latence) ; un refus arrête le banc.
    pub async fn load_plugin(&self, plugin_ref: &Value) -> Result<Value, String> {
        self.request(
            json!({ "type": "load-instrument-plugin", "pluginRef": plugin_ref }),
            "instrument-plugin".into(),
        )
        .await
    }

    /// Retire le plugin inséré, comme le ✕ du studio. L'Audio Engine garde un
    /// plugin d'une connexion à l'autre (voulu : un rechargement de la page ne
    /// coupe pas le son) — le banc doit donc rendre ce qu'il a chargé, sinon le
    /// musicien joue ensuite à travers sans l'avoir choisi (vécu le 29/09).
    pub async fn unload_plugin(&self) -> Result<(), String> {
        self.request(json!({ "type": "unload-instrument-plugin" }), "instrument-plugin-unload".into())
            .await
            .map(|_| ())
    }

    pub fn remove_stream(&self, producer_id: &str) -> Result<(), String> {
        self.send(json!({ "type": "remove-stream", "producerId": producer_id }))
    }

    pub fn stop(&self) -> Result<(), String> {
        self.send(json!({ "type": "stop" }))
    }
}

fn keys_of(local_port: &Value) -> Result<SrtpParameters, String> {
    serde_json::from_value(local_port["srtpParameters"].clone())
        .map_err(|e| format!("clés de l'agent illisibles : {e}"))
}

/// Aiguille un message de l'agent vers l'attente qui lui correspond.
fn dispatch(
    v: Value,
    pending: &Pending,
    out: &mpsc::UnboundedSender<Message>,
    perf_tx: &mpsc::UnboundedSender<Value>,
    err_tx: &mpsc::UnboundedSender<String>,
    hello_tx: &mut Option<oneshot::Sender<Value>>,
) {
    let resolve = |key: String, r: Result<Value, String>| -> bool {
        match pending.lock().unwrap().remove(&key) {
            Some(tx) => {
                let _ = tx.send(r);
                true
            }
            None => false,
        }
    };
    match v["type"].as_str().unwrap_or("") {
        "hello" => {
            // Le studio répond aussitôt : c'est ce qui fait du banc le client promu.
            let ack = json!({
                "type": "hello-ack",
                "protocolVersion": v["protocolVersion"],
                "sessionId": "session-bench",
            });
            let _ = out.send(Message::Text(ack.to_string()));
            if let Some(tx) = hello_tx.take() {
                let _ = tx.send(v);
            }
        }
        "perf-stats" => {
            let _ = perf_tx.send(v);
        }
        "local-port" => {
            let key = port_key(v["producerId"].as_str().unwrap_or(""));
            resolve(key, Ok(v));
        }
        "capture-started" => {
            resolve("capture-started".into(), Ok(v));
        }
        "devices" => {
            resolve("devices".into(), Ok(v));
        }
        "plugin-list" => {
            resolve("plugin-list".into(), Ok(v));
        }
        "instrument-plugin-loaded" => {
            resolve("instrument-plugin".into(), Ok(v));
        }
        "instrument-plugin-unloaded" => {
            resolve("instrument-plugin-unload".into(), Ok(v));
        }
        "instrument-plugin-error" => {
            let msg = format!("plugin refusé : {}", v["message"].as_str().unwrap_or("?"));
            if !resolve("instrument-plugin".into(), Err(msg.clone())) {
                let _ = err_tx.send(msg);
            }
        }
        "rejected" => {
            let _ = err_tx.send(format!(
                "l'Audio Engine refuse le banc : {}",
                v["reason"].as_str().unwrap_or("?")
            ));
        }
        "capture-error" => {
            let msg = format!(
                "capture refusée : {} {}",
                v["reason"].as_str().unwrap_or("?"),
                v["detail"].as_str().unwrap_or("")
            );
            if !resolve(port_key(""), Err(msg.clone())) {
                let _ = err_tx.send(msg);
            }
        }
        "error" => {
            let msg = v["message"].as_str().unwrap_or("erreur").to_string();
            let answered = match v["key"].as_str() {
                Some(k) => resolve(port_key(k), Err(format!("{k:?} : {msg}"))),
                None => false,
            };
            if !answered {
                let _ = err_tx.send(msg);
            }
        }
        _ => {}
    }
}

/// Paramètres de `start-capture`.
pub struct CaptureParams<'a> {
    pub ssrc: u32,
    pub server_ip: &'a str,
    pub server_port: u16,
    pub input_device: Option<&'a str>,
    pub channel_index: Option<u8>,
    pub server_keys: &'a SrtpParameters,
}

pub struct VoiceParams<'a> {
    pub ssrc: u32,
    pub server_ip: &'a str,
    pub server_port: u16,
    pub channel_index: u8,
    pub server_keys: &'a SrtpParameters,
}

pub struct StreamParams<'a> {
    pub producer_id: &'a str,
    pub peer_id: &'a str,
    pub server_ip: &'a str,
    pub server_port: u16,
    pub voice: bool,
    pub server_keys: &'a SrtpParameters,
}

fn msg_get_stats() -> Value {
    json!({ "type": "get-stats" })
}

fn msg_select_devices(input: Option<&str>, output: Option<&str>) -> Value {
    json!({ "type": "select-devices", "inputId": input, "outputId": output })
}

fn msg_start_capture(p: &CaptureParams) -> Value {
    json!({
        "type": "start-capture",
        "ssrc": p.ssrc,
        "sfuIp": p.server_ip,
        "sfuPort": p.server_port,
        "payloadType": crate::server::PAYLOAD_TYPE,
        "inputDevice": p.input_device,
        "channelIndex": p.channel_index,
        "srtpParameters": p.server_keys,
        "sessionContinues": false,
    })
}

fn msg_start_voice(p: &VoiceParams) -> Value {
    json!({
        "type": "start-voice-capture",
        "ssrc": p.ssrc,
        "sfuIp": p.server_ip,
        "sfuPort": p.server_port,
        "payloadType": crate::server::PAYLOAD_TYPE,
        "channelIndex": p.channel_index,
        "srtpParameters": p.server_keys,
    })
}

fn msg_add_stream(p: &StreamParams) -> Value {
    let mut v = json!({
        "type": "add-stream",
        "producerId": p.producer_id,
        "producerPeerId": p.peer_id,
        "sfuIp": p.server_ip,
        "sfuPort": p.server_port,
        "payloadType": crate::server::PAYLOAD_TYPE,
        "srtpParameters": p.server_keys,
    });
    // Comme le studio : pas d'étiquette pour un instrument (défaut de l'agent).
    if p.voice {
        v["mediaTag"] = json!("voice");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::protocol::{BrowserMessage, StreamKind};

    /// Chaque message du banc est lu par l'agent comme celui du studio : même
    /// type, mêmes champs. Si le protocole change, ce test casse avant le banc.
    fn as_agent_reads(v: Value) -> BrowserMessage {
        serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{v} : {e}"))
    }

    #[test]
    fn les_messages_du_banc_sont_ceux_que_l_agent_attend() {
        let keys = SrtpParameters::generate_aead_aes_256_gcm();
        assert!(matches!(as_agent_reads(msg_get_stats()), BrowserMessage::GetStats));
        assert!(matches!(
            as_agent_reads(msg_select_devices(Some("0:Focusrite USB ASIO"), None)),
            BrowserMessage::SelectDevices { input_id: Some(_), output_id: None }
        ));
        let cap = CaptureParams {
            ssrc: 7,
            server_ip: "127.0.0.1",
            server_port: 5000,
            input_device: Some("0:Focusrite USB ASIO"),
            channel_index: Some(1),
            server_keys: &keys,
        };
        match as_agent_reads(msg_start_capture(&cap)) {
            BrowserMessage::StartCapture { ssrc, sfu_ip, sfu_port, input_device, channel_index, session_continues, .. } => {
                assert_eq!((ssrc, sfu_ip.as_str(), sfu_port), (7, "127.0.0.1", 5000));
                assert_eq!(input_device.as_deref(), Some("0:Focusrite USB ASIO"));
                assert_eq!(channel_index, Some(1));
                assert!(!session_continues);
            }
            other => panic!("{other:?}"),
        }
        let voice = VoiceParams { ssrc: 8, server_ip: "127.0.0.1", server_port: 5001, channel_index: 1, server_keys: &keys };
        assert!(matches!(
            as_agent_reads(msg_start_voice(&voice)),
            BrowserMessage::StartVoiceCapture { channel_index: 1, .. }
        ));
        for (is_voice, kind) in [(false, StreamKind::Instrument), (true, StreamKind::Voice)] {
            let s = StreamParams {
                producer_id: "bench-p2",
                peer_id: "bench-2",
                server_ip: "127.0.0.1",
                server_port: 5002,
                voice: is_voice,
                server_keys: &keys,
            };
            match as_agent_reads(msg_add_stream(&s)) {
                BrowserMessage::AddStream { producer_id, media_tag, .. } => {
                    assert_eq!(producer_id, "bench-p2");
                    assert_eq!(media_tag, kind);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// Une erreur liée à une demande la fait échouer — jamais un silence.
    #[tokio::test]
    async fn une_erreur_de_l_agent_fait_echouer_la_demande_en_attente() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(port_key("bench-p2"), tx);
        let (out, _o) = mpsc::unbounded_channel();
        let (perf_tx, _p) = mpsc::unbounded_channel();
        let (err_tx, mut err_rx) = mpsc::unbounded_channel();
        let mut hello = None;
        dispatch(
            json!({ "type": "error", "message": "agent overloaded", "key": "bench-p2" }),
            &pending, &out, &perf_tx, &err_tx, &mut hello,
        );
        let r = rx.await.unwrap();
        assert!(r.unwrap_err().contains("agent overloaded"));
        // Une erreur sans demande en attente remonte au banc.
        dispatch(json!({ "type": "error", "message": "rate" }), &pending, &out, &perf_tx, &err_tx, &mut hello);
        assert_eq!(err_rx.recv().await.unwrap(), "rate");
    }

    #[test]
    fn le_chargement_d_un_plugin_est_celui_du_studio() {
        let r = json!({ "format": "vst3", "path": "C:/VST3/AmpliTube 5.vst3", "uid": "ABCD" });
        let v = json!({ "type": "load-instrument-plugin", "pluginRef": r });
        assert!(matches!(as_agent_reads(v), BrowserMessage::LoadInstrumentPlugin { .. }));
        assert!(matches!(as_agent_reads(json!({ "type": "list-plugins" })), BrowserMessage::ListPlugins));
    }

    /// Le retrait est celui du ✕ du studio, et sa confirmation débloque l'attente.
    #[tokio::test]
    async fn le_retrait_du_plugin_est_celui_du_studio_et_se_confirme() {
        assert!(matches!(
            as_agent_reads(json!({ "type": "unload-instrument-plugin" })),
            BrowserMessage::UnloadInstrumentPlugin
        ));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert("instrument-plugin-unload".into(), tx);
        let (out, _o) = mpsc::unbounded_channel();
        let (perf_tx, _p) = mpsc::unbounded_channel();
        let (err_tx, _e) = mpsc::unbounded_channel();
        let mut hello = None;
        dispatch(json!({ "type": "instrument-plugin-unloaded" }), &pending, &out, &perf_tx, &err_tx, &mut hello);
        assert!(rx.await.unwrap().is_ok());
    }

    /// Un plugin refusé fait échouer la demande — jamais un banc « chargé » à vide.
    #[tokio::test]
    async fn un_plugin_refuse_fait_echouer_la_demande() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert("instrument-plugin".into(), tx);
        let (out, _o) = mpsc::unbounded_channel();
        let (perf_tx, _p) = mpsc::unbounded_channel();
        let (err_tx, _e) = mpsc::unbounded_channel();
        let mut hello = None;
        dispatch(json!({ "type": "instrument-plugin-error", "message": "latence trop grande" }), &pending, &out, &perf_tx, &err_tx, &mut hello);
        assert!(rx.await.unwrap().unwrap_err().contains("latence trop grande"));
    }

    #[test]
    fn le_bonjour_recoit_l_accuse_du_studio() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (out, mut out_rx) = mpsc::unbounded_channel();
        let (perf_tx, _p) = mpsc::unbounded_channel();
        let (err_tx, _e) = mpsc::unbounded_channel();
        let (htx, mut hrx) = oneshot::channel();
        let mut hello = Some(htx);
        dispatch(json!({ "type": "hello", "protocolVersion": 1, "agentVersion": "0.6.6-5" }), &pending, &out, &perf_tx, &err_tx, &mut hello);
        let Message::Text(ack) = out_rx.try_recv().unwrap() else { panic!() };
        assert!(matches!(
            as_agent_reads(serde_json::from_str(&ack).unwrap()),
            BrowserMessage::HelloAck { protocol_version: 1, .. }
        ));
        assert_eq!(hrx.try_recv().unwrap()["agentVersion"], "0.6.6-5");
    }
}
