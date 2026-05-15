//! MIDI input — discovery + ouverture d'un MIDI device.
//!
//! Sprint S2 INSERT plugins instruments. Permet de router des events MIDI
//! (note on/off, CC, pitch bend…) depuis un clavier USB ou un port virtuel
//! vers un plugin AU instrument (AUSampler, AUMIDISynth, batteries virtuelles,
//! synthés). Le plugin produit l'audio qui est ensuite encodé Opus + RTP
//! comme une capture audio normale.
//!
//! Architecture :
//!   1. `list_devices()` retourne tous les MIDI input ports détectés.
//!   2. `MidiInput::open(device_id, tx)` ouvre un port et push chaque event
//!      dans un crossbeam channel. Le `encoder_thread` drain ce channel à
//!      chaque bloc audio (= ~2.7 ms) et passe les events au plugin via
//!      `PluginHost::process_stereo(handle, audio, midi_events)`.
//!   3. Mode "omni" : tous les channels MIDI sont acceptés (= comportement
//!      par défaut des plugins, choix MVP). Filtrage par channel = futur.
//!
//! Le crate `midir` (mature, 0.10) abstrait CoreMIDI (macOS) / ALSA (Linux) /
//! Windows MIDI.

use crossbeam_channel::Sender;
use jamodio_audio_core::plugin_host::MidiEvent;
use midir::{MidiInput as MidirInput, MidiInputConnection, MidiInputPort};
// Port virtuel MIDI = Unix-only (CoreMIDI macOS, ALSA Linux). Windows MIDI
// (winmm) ne supporte pas nativement la création de ports virtuels — pas de
// `midir::os::windows::VirtualInput`. Sur Windows, l'utilisateur passe
// directement par un clavier MIDI USB physique (toujours supporté).
#[cfg(unix)]
use midir::os::unix::VirtualInput;
use serde::Serialize;

/// Nom du port virtuel MIDI créé au boot agent. Exposé dans CoreMIDI sous
/// ce nom — les autres apps macOS (Logic, Ableton, GarageBand, MainStage,
/// LMMS, MuseScore…) le voient comme destination MIDI et peuvent y envoyer.
/// Modifié = breaking change pour les users qui ont sauvegardé l'id.
pub const VIRTUAL_PORT_NAME: &str = "Jamodio Virtual MIDI";
/// Préfixe d'id distingué pour le port virtuel — `set_input_source` détecte
/// ce préfixe pour réutiliser le port existant au lieu d'ouvrir un physique.
pub const VIRTUAL_PORT_ID_PREFIX: &str = "virtual:";

/// Métadonnées d'un MIDI device exposées au browser via WS.
#[derive(Debug, Clone, Serialize)]
pub struct MidiDeviceInfo {
    /// Identifiant stable (= index dans la liste + nom). Format `"{idx}:{name}"`,
    /// même convention que les audio devices (cf. `device::list_inputs`).
    pub id: String,
    pub name: String,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

/// Liste tous les MIDI input ports disponibles sur le système. Appelée
/// chaque fois que le browser ouvre la section "Source d'entrée" dans
/// Mes Paramètres (pas de cache — les devices USB peuvent être hot-plug).
///
/// Le port virtuel "Jamodio Virtual MIDI" créé au boot agent est ajouté
/// EN TÊTE de liste (= default). Permet à l'user d'utiliser Jamodio comme
/// destination MIDI depuis n'importe quelle app macOS sans configurer
/// l'IAC Driver.
pub fn list_devices() -> Vec<MidiDeviceInfo> {
    let input = match MidirInput::new("Jamodio MIDI Scanner") {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(target: "jamodio::midi", error = %e, "MidirInput::new failed");
            return Vec::new();
        }
    };
    let ports = input.ports();

    // Port virtuel toujours en tête de liste (= default UI). Son id
    // commence par "virtual:" → `set_input_source` détecte ce préfixe
    // pour réutiliser le receiver persistant au lieu d'ouvrir un physique.
    // Unix-only — Windows ne sait pas créer de port MIDI virtuel.
    let mut out = Vec::with_capacity(ports.len() + 1);
    #[cfg(unix)]
    out.push(MidiDeviceInfo {
        id: format!("{VIRTUAL_PORT_ID_PREFIX}{VIRTUAL_PORT_NAME}"),
        name: VIRTUAL_PORT_NAME.to_string(),
        is_default: true,
    });

    for (idx, port) in ports.iter().enumerate() {
        let name = input.port_name(port).unwrap_or_else(|_| "Unknown".into());
        // Le port virtuel apparaît parfois dans midir.ports() (CoreMIDI
        // se voit lui-même). On dédoublonne par nom.
        #[cfg(unix)]
        if name == VIRTUAL_PORT_NAME {
            continue;
        }
        out.push(MidiDeviceInfo {
            id: format!("{idx}:{name}"),
            name,
            is_default: idx == 0 && cfg!(not(unix)),
        });
    }
    out
}

/// Crée le port virtuel "Jamodio Virtual MIDI" et le tient ouvert tant que
/// le `MidiInput` retourné n'est pas dropped. Apparaît dans CoreMIDI = visible
/// dans toutes les apps MIDI macOS comme destination. Appelée UNE fois au
/// boot agent par `PipelineState::spawn_virtual_midi()` — qui est lui-même
/// gardé `cfg(target_os = "macos")`, donc cette fonction n'est appelée que
/// sur mac. Le `cfg(unix)` ici garde la définition cohérente avec l'API
/// `midir::os::unix::VirtualInput` qui n'existe que sur Unix.
#[cfg(unix)]
pub fn create_virtual_input(tx: Sender<MidiEvent>) -> Result<MidiInput, String> {
    let input = MidirInput::new("Jamodio Agent")
        .map_err(|e| format!("MidirInput virtual init: {e}"))?;

    let conn = input
        .create_virtual(
            VIRTUAL_PORT_NAME,
            move |_timestamp_us, message, _state| {
                if message.is_empty() || message.len() > 3 {
                    return;
                }
                let mut data = [0u8; 3];
                for (i, b) in message.iter().enumerate() {
                    data[i] = *b;
                }
                let _ = tx.try_send(MidiEvent {
                    frame_offset: 0,
                    data,
                });
            },
            (),
        )
        .map_err(|e| format!("create_virtual: {e}"))?;

    let device_id = format!("{VIRTUAL_PORT_ID_PREFIX}{VIRTUAL_PORT_NAME}");
    tracing::info!(
        target: "jamodio::midi",
        device_id = %device_id,
        port_name = %VIRTUAL_PORT_NAME,
        "virtual MIDI port created (visible to all macOS MIDI apps)"
    );
    Ok(MidiInput {
        _conn: conn,
        device_id,
    })
}

/// Connexion MIDI input ouverte. Tient la connexion vivante via RAII.
/// `Drop` ferme automatiquement le port.
pub struct MidiInput {
    _conn: MidiInputConnection<()>,
    device_id: String,
}

impl MidiInput {
    /// Ouvre un MIDI device par son `id` (format `"{idx}:{name}"`). Chaque
    /// event reçu est packé en `MidiEvent` et poussé dans `tx` (le receiver
    /// est drainé par `encoder_thread`). Le callback midir tourne sur son
    /// propre thread géré par CoreMIDI.
    ///
    /// Mode omni : tous les channels sont acceptés. Filtrage = futur.
    pub fn open(device_id: &str, tx: Sender<MidiEvent>) -> Result<Self, String> {
        let parsed_idx = device_id
            .split_once(':')
            .and_then(|(i, _)| i.parse::<usize>().ok())
            .ok_or_else(|| format!("malformed MIDI device id: {device_id}"))?;

        let input = MidirInput::new("Jamodio MIDI Input")
            .map_err(|e| format!("MidirInput init: {e}"))?;
        let ports = input.ports();
        let port: &MidiInputPort = ports
            .get(parsed_idx)
            .ok_or_else(|| format!("MIDI device index {parsed_idx} out of range ({} ports available)", ports.len()))?;

        let port_name = input.port_name(port).unwrap_or_default();
        let device_id_owned = device_id.to_string();

        let conn = input
            .connect(
                port,
                "jamodio-midi-in",
                move |_timestamp_us, message, _state| {
                    // `message` est l'event MIDI brut (1-3 bytes typiquement).
                    // Note On/Off + CC + Pitch bend tiennent en 3 bytes.
                    // SysEx (variable length) est ignoré au MVP — peu de plugins
                    // l'utilisent pour le live, et notre wire MidiEvent fait 3 bytes.
                    if message.is_empty() || message.len() > 3 {
                        return;
                    }
                    let mut data = [0u8; 3];
                    for (i, b) in message.iter().enumerate() {
                        data[i] = *b;
                    }
                    let event = MidiEvent {
                        frame_offset: 0, // S2 MVP : tous les events traités au début du bloc.
                                         // Sample-precise timing (= timestamp_us → frame_offset)
                                         // = sprint futur si jitter audible.
                        data,
                    };
                    // try_send non bloquant : si la queue est pleine (= encoder
                    // thread saturé), on droppe l'event. Préférable à un block
                    // qui ferait tourner le callback MIDI plus longtemps.
                    let _ = tx.try_send(event);
                },
                (),
            )
            .map_err(|e| format!("MIDI connect: {e}"))?;

        tracing::info!(
            target: "jamodio::midi",
            device_id = %device_id_owned,
            port_name = %port_name,
            "MIDI input opened"
        );
        Ok(Self {
            _conn: conn,
            device_id: device_id_owned,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}

impl Drop for MidiInput {
    fn drop(&mut self) {
        tracing::info!(target: "jamodio::midi", device_id = %self.device_id, "MIDI input closed");
        // _conn est dropped automatiquement → midir ferme le port + thread interne.
    }
}
