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
///
/// Sérialisation wire (cohérente avec le reste du protocole — camelCase) :
/// ```json
/// { "format": "au", "auType": "aufx", "subtype": "mrev", "manufacturer": "appl" }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "lowercase")]
pub enum PluginRef {
    /// Audio Unit (macOS). type/subtype/manufacturer encodés en 4-CC ASCII.
    Au {
        #[serde(rename = "auType")]
        au_type: String,
        subtype: String,
        manufacturer: String,
    },
    /// VST3 (phase 2 Windows). À spécifier le moment venu.
    #[allow(dead_code)]
    Vst3 { path: String, uid: String },
}

/// Métadonnées d'un plugin tel que présenté au browser.
/// `incompatible: true` = latence intrinsèque au-delà du budget live
/// (cf. [`latency_exceeds_live_budget`]) → l'UI l'affiche grisé, non chargeable,
/// avec un tooltip explicatif (cf. mémoire vision).
/// `has_input_bus = false` (= synthé MIDI pur) signale au browser qu'il faut
/// auto-switcher la source d'entrée en MIDI à l'activation (S2).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub name: String,
    pub manufacturer: String,
    pub plugin_ref: PluginRef,
    pub latency_samples: u32,
    pub has_editor: bool,
    pub incompatible: bool,
    /// True si le plugin a au moins un bus audio in (= effets aufx + aumu
    /// hybrides type AmpliTube). False = pur instrument MIDI, nécessite une
    /// source d'entrée MIDI pour produire du son.
    #[serde(default = "default_true")]
    pub has_input_bus: bool,
    /// Classification AUTORITAIRE instrument (synthé/sampler) vs effet, calculée
    /// côté agent : AU = composant `aumu`, VST3 = sous-catégorie `"Instrument|…"`.
    /// Le browser s'en sert pour basculer la source en MIDI au chargement. Un
    /// instrument peut avoir `has_input_bus = true` (sidechain audio, ex. Surge
    /// XT / BFD) : c'est CE champ qui tranche, pas le bus d'entrée.
    /// `#[serde(default)]` = false → rétro-compat browser/agents pré-0.5.0.
    #[serde(default)]
    pub is_instrument: bool,
}

fn default_true() -> bool {
    true
}

/// Évènement MIDI dispatché à un plugin instrument (AU/VST3).
///
/// `frame_offset` est sample-accurate : c'est l'index du sample (relatif au
/// début du sous-bloc passé à `process_stereo`) auquel l'event doit s'aligner.
/// Calculé côté agent à partir du timestamp de capture midir
/// (`CapturedMidiEvent::captured_at` dans `crate::audio::midi`) — cf.
/// `pipeline.rs::process_stage` pour la conversion.
///
/// Précision : bornée par le sample 48 kHz (~20 µs). Garantit un timing
/// DAW-grade pour les pads/batterie/drums, vs ±1,33 ms RMS de l'ancien
/// dispatch block-quantized (frame_offset toujours 0).
#[derive(Clone, Debug)]
pub struct MidiEvent {
    pub frame_offset: u32,
    pub data: [u8; 3],
}

/// Budget de latence intrinsèque (PDC) qu'un plugin INSERT peut ajouter tout en
/// restant chargeable en live. **Sans rapport avec la taille de buffer** : c'est
/// un retard fixe interne au plugin (typiquement son suréchantillonnage) qui
/// s'AJOUTE au chemin note→oreille et note→réseau, quel que soit le bloc audio.
/// Le stage plugin traite déjà par sous-blocs de 128 samples (`PLUGIN_BLOCK`,
/// cf. `pipeline.rs`) et n'applique AUCUNE compensation de délai → la latence
/// intrinsèque est purement additive, bornée ici par pur choix de budget live.
///
/// 128 samples = 2,67 ms @ 48 kHz : couvre les amp-sims faible latence (p. ex.
/// Neural DSP ≈ 84 samples) et rejette le lookahead lourd / linéaire-phase.
///
/// Évolution possible (non implémentée) : troquer le blocage dur contre un
/// avertissement doux laissant l'utilisateur juge — techniquement sûr, l'absence
/// de PDC dans le pipeline garantissant qu'une latence > 128 ne casse rien.
pub const MAX_PLUGIN_LATENCY_SAMPLES: u32 = 128;

/// Règle unique de compatibilité live, partagée par les hôtes AU et VST3.
/// `true` ⇒ latence intrinsèque au-delà de [`MAX_PLUGIN_LATENCY_SAMPLES`] :
/// le plugin est marqué `incompatible` et présenté non chargeable par l'UI.
pub const fn latency_exceeds_live_budget(latency_samples: u32) -> bool {
    latency_samples > MAX_PLUGIN_LATENCY_SAMPLES
}

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
    /// `midi_events` est passé au plugin AVANT le render (S2 : utilisé par les AU instrument
    /// pour générer leur son à partir du MIDI). Passer `&[]` pour les AU effects qui ne
    /// consomment pas de MIDI.
    /// Appelé depuis le thread audio RT — DOIT être lock-free et alloc-free.
    fn process_stereo(
        &mut self,
        handle: PluginHandle,
        left: &mut [f32],
        right: &mut [f32],
        midi_events: &[MidiEvent],
    ) -> Result<(), PluginError>;

    /// Latence intrinsèque rapportée par le plugin. Stable après load.
    fn latency_samples(&self, handle: PluginHandle) -> u32;

    /// Ouvre la fenêtre éditeur du plugin (NSWindow native sur macOS).
    /// L'ouverture est dispatchée sur le main thread du process.
    fn open_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError>;

    /// Ferme la fenêtre éditeur si ouverte. No-op sinon.
    fn close_editor(&mut self, handle: PluginHandle) -> Result<(), PluginError>;
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    #[test]
    fn plugin_ref_au_serializes_camel_case() {
        let pr = PluginRef::Au {
            au_type: "aufx".into(),
            subtype: "mrev".into(),
            manufacturer: "appl".into(),
        };
        let json = serde_json::to_string(&pr).unwrap();
        assert!(json.contains(r#""format":"au""#), "json was {json}");
        assert!(json.contains(r#""auType":"aufx""#), "json was {json}");
        assert!(json.contains(r#""subtype":"mrev""#), "json was {json}");
        assert!(json.contains(r#""manufacturer":"appl""#), "json was {json}");
    }

    #[test]
    fn plugin_ref_au_round_trip() {
        let raw = r#"{"format":"au","auType":"aufx","subtype":"mrev","manufacturer":"appl"}"#;
        let pr: PluginRef = serde_json::from_str(raw).unwrap();
        match pr {
            PluginRef::Au { au_type, subtype, manufacturer } => {
                assert_eq!(au_type, "aufx");
                assert_eq!(subtype, "mrev");
                assert_eq!(manufacturer, "appl");
            }
            _ => panic!("expected Au variant"),
        }
    }

    #[test]
    fn plugin_handle_zero_is_invalid() {
        assert!(!PluginHandle::INVALID.is_valid());
        assert!(PluginHandle(1).is_valid());
    }

    /// v0.2.23 — Vérifie que PluginInfo sérialise en camelCase (cohérent avec
    /// le reste du protocole wire). Avant v0.2.23, on avait l'inconsistance
    /// snake_case dans PluginList vs camelCase dans InstrumentPluginLoaded
    /// → bug Yannick avec `p.pluginRef` undefined côté Chrome console.
    #[test]
    fn plugin_info_serializes_camel_case() {
        let info = PluginInfo {
            name: "AUMatrixReverb".into(),
            manufacturer: "Apple".into(),
            plugin_ref: PluginRef::Au {
                au_type: "aufx".into(),
                subtype: "mrev".into(),
                manufacturer: "appl".into(),
            },
            latency_samples: 0,
            has_editor: true,
            incompatible: false,
            has_input_bus: true,
            is_instrument: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains(r#""pluginRef":"#), "json was {json}");
        assert!(json.contains(r#""latencySamples":0"#), "json was {json}");
        assert!(json.contains(r#""hasEditor":true"#), "json was {json}");
        assert!(json.contains(r#""hasInputBus":true"#), "json was {json}");
        assert!(json.contains(r#""isInstrument":false"#), "json was {json}");
        // Sanity : aucun champ snake_case ne doit fuir.
        assert!(!json.contains("plugin_ref"), "snake_case leaked: {json}");
        assert!(!json.contains("latency_samples"), "snake_case leaked: {json}");
        assert!(!json.contains("has_editor"), "snake_case leaked: {json}");
        assert!(!json.contains("has_input_bus"), "snake_case leaked: {json}");
        assert!(!json.contains("is_instrument"), "snake_case leaked: {json}");
    }

    #[test]
    fn latency_budget_boundary() {
        // Limite incluse = compatible ; strictement au-delà = rejeté.
        assert!(!latency_exceeds_live_budget(0));
        assert!(!latency_exceeds_live_budget(84)); // Neural DSP Darkglass : désormais OK
        assert!(!latency_exceeds_live_budget(MAX_PLUGIN_LATENCY_SAMPLES));
        assert!(latency_exceeds_live_budget(MAX_PLUGIN_LATENCY_SAMPLES + 1));
        assert!(latency_exceeds_live_budget(256));
    }
}
