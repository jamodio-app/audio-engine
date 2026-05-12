//! Abstraction d'hôte plugin audio (AU sur macOS, VST3 sur Windows phase 2).
//!
//! Le trait `PluginHost` est implémenté par `jamodio-au-host` côté macOS.
//! Toute la chaîne capture-side (CPAL → plugin → Opus) parle à cette abstraction,
//! pas à AU directement, pour préparer l'arrivée de Windows/VST3.

use serde::{Deserialize, Serialize};

/// Identifiant opaque d'un plugin chargé dans un host.
/// La valeur 0 est réservée comme sentinelle "invalide".
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PluginHandle(pub u32);

impl PluginHandle {
    pub const INVALID: PluginHandle = PluginHandle(0);
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Référence persistable d'un plugin (= ce qui permet de le retrouver entre 2 sessions).
/// Côté AU : type/subtype/manufacturer en 4-CC. Côté VST3 : path .vst3 + UID.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "lowercase")]
pub enum PluginRef {
    /// Audio Unit (macOS). type/subtype/manufacturer encodés en 4-CC ASCII.
    Au {
        #[serde(rename = "type")]
        au_type: String,
        subtype: String,
        manufacturer: String,
    },
    /// VST3 (phase 2 Windows). À spécifier le moment venu.
    #[allow(dead_code)]
    Vst3 { path: String, uid: String },
}

/// Métadonnées d'un plugin tel que présenté au browser.
/// `incompatible: true` = latence intrinsèque trop haute (>64 samples) pour live.
/// L'UI affiche ces plugins en grisé avec tooltip explicatif (cf. mémoire vision).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub manufacturer: String,
    pub plugin_ref: PluginRef,
    pub latency_samples: u32,
    pub has_editor: bool,
    pub incompatible: bool,
}

/// Évènement MIDI pour les plugins instruments (S2). Pas utilisé en S1 mais réservé.
#[derive(Clone, Debug)]
pub struct MidiEvent {
    pub frame_offset: u32,
    pub data: [u8; 3],
}

/// Limite latence intrinsèque acceptable pour Jamodio (bloc CPAL cible = 64 samples).
pub const MAX_PLUGIN_LATENCY_SAMPLES: u32 = 64;

/// Erreurs PluginHost. Volontairement minimaliste — pas de hiérarchie complexe.
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin not found")]
    NotFound,
    #[error("plugin failed to initialize: {0}")]
    Init(String),
    #[error("plugin process error: {0}")]
    Process(String),
    #[error("invalid handle")]
    InvalidHandle,
}

/// Hôte plugin audio. Une instance vit pendant toute la durée du studio agent.
/// Les méthodes `scan`/`load`/`unload`/`open_editor` sont appelées depuis le main thread.
/// `process_stereo` est appelé depuis le thread audio RT (CPAL callback) — ne doit
/// jamais bloquer ni allouer.
pub trait PluginHost: Send {
    /// Liste tous les plugins installés du format supporté.
    fn scan(&self) -> Vec<PluginInfo>;

    /// Charge un plugin et retourne son handle. Format audio fixé : 48k stéréo f32.
    /// max_frames = bloc CPAL maximum garanti (typiquement 64).
    fn load(&mut self, plugin_ref: &PluginRef, max_frames: u32) -> Result<PluginHandle, PluginError>;

    /// Décharge un plugin et libère ses ressources. Ferme la window si ouverte.
    fn unload(&mut self, handle: PluginHandle) -> Result<(), PluginError>;

    /// Process un bloc audio stéréo non-interleaved IN-PLACE.
    /// `left`/`right` contiennent les samples d'entrée à l'appel, samples de sortie au retour.
    /// Appelé depuis le thread audio RT — DOIT être lock-free et alloc-free.
    fn process_stereo(
        &mut self,
        handle: PluginHandle,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), PluginError>;

    /// Latence intrinsèque rapportée par le plugin. Stable après load.
    fn latency_samples(&self, handle: PluginHandle) -> u32;

    /// Ouvre la fenêtre éditeur du plugin (NSWindow native sur macOS).
    /// L'ouverture est dispatchée sur le main thread du process.
    fn open_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError>;

    /// Ferme la fenêtre éditeur si ouverte. No-op sinon.
    fn close_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError>;
}
