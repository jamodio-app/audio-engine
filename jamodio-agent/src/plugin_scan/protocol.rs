//! Protocole IPC coordinateur ↔ worker de scan (NDJSON sur stdout du worker).
//!
//! Une ligne JSON par event, flush après chaque ligne — le flush immédiat est
//! CONTRACTUEL : c'est lui qui permet au coordinateur de désigner le coupable
//! (dernier `begin` sans `end`) quand le worker meurt d'un crash natif.
//!
//! # Items
//!
//! Un « item » est l'unité de scan/isolation, encodée en String :
//! - Windows : path absolu du `.vst3` (bundle ou DLL legacy) ;
//! - macOS   : `au:{type}/{subtype}/{manufacturer}` en 4-CC ASCII
//!   (ex. `au:aufx/mrev/appl`), cf. [`AuItem`].
//!
//! `PluginInfo` est réutilisé tel quel (Serialize camelCase, cf.
//! jamodio-audio-core/src/plugin_host.rs) — zéro nouveau format wire.

use jamodio_audio_core::plugin_host::PluginInfo;
use serde::{Deserialize, Serialize};

/// Event émis par le worker, une ligne NDJSON chacun.
///
/// Séquence par item : `Begin` → 0..n `Plugin` → `End`. Un fichier .vst3 peut
/// contenir plusieurs classes (d'où 0..n) ; un composant AU en produit 0 ou 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum WorkerEvent {
    /// Le worker VA charger cet item. Si le process meurt avant le `End`
    /// correspondant, cet item est le coupable.
    Begin { item: String },
    /// Un plugin extrait de l'item courant.
    Plugin { info: PluginInfo },
    /// L'item courant est terminé (avec ou sans plugins — un `.vst3` sans
    /// classe audio-effect produit `Begin`/`End` sans `Plugin`).
    End { item: String },
}

/// Item macOS : identité d'un composant AudioComponent en 4-CC.
///
/// L'énumération du registre (in-process, sûre) produit ces identités ; le
/// worker les re-résout via `AudioComponentFindNext` pour prober. Format
/// String stable : `au:{type}/{subtype}/{manufacturer}`.
///
/// Le préfixe `au:` est aussi utilisé hors macOS par le cache (qui traite les
/// items comme des String opaques) — d'où [`AuItemPrefix`], disponible partout.
pub struct AuItemPrefix;
impl AuItemPrefix {
    pub const VALUE: &'static str = "au:";
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuItem {
    pub au_type: String,
    pub subtype: String,
    pub manufacturer: String,
}

#[cfg(target_os = "macos")]
impl AuItem {
    pub const PREFIX: &'static str = AuItemPrefix::VALUE;

    pub fn encode(&self) -> String {
        format!(
            "{}{}/{}/{}",
            Self::PREFIX,
            self.au_type,
            self.subtype,
            self.manufacturer
        )
    }

    /// Parse `au:aufx/mrev/appl`. `None` si le format ne correspond pas —
    /// le worker log et ignore l'item plutôt que de paniquer (l'entrée vient
    /// du coordinateur, un mismatch = bug interne, pas une donnée externe).
    pub fn decode(item: &str) -> Option<Self> {
        let rest = item.strip_prefix(Self::PREFIX)?;
        let mut parts = rest.split('/');
        let (t, s, m) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || t.is_empty() || s.is_empty() || m.is_empty() {
            return None;
        }
        Some(Self {
            au_type: t.to_string(),
            subtype: s.to_string(),
            manufacturer: m.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::plugin_host::PluginRef;

    #[test]
    fn worker_event_wire_format() {
        // Le tag `event` + camelCase de PluginInfo sont contractuels (le
        // coordinateur parse ligne à ligne).
        let begin = WorkerEvent::Begin { item: "C:\\VST3\\Foo.vst3".into() };
        let json = serde_json::to_string(&begin).unwrap();
        assert!(json.contains(r#""event":"begin""#), "json was {json}");
        assert!(json.contains(r#""item":"#), "json was {json}");

        let plugin = WorkerEvent::Plugin {
            info: PluginInfo {
                name: "Foo".into(),
                manufacturer: "Bar".into(),
                plugin_ref: PluginRef::Vst3 { path: "C:\\VST3\\Foo.vst3".into(), uid: "AB".into() },
                latency_samples: 0,
                has_editor: true,
                incompatible: false,
                has_input_bus: true,
                is_instrument: false,
            },
        };
        let json = serde_json::to_string(&plugin).unwrap();
        assert!(json.contains(r#""event":"plugin""#), "json was {json}");
        assert!(json.contains(r#""latencySamples":0"#), "camelCase attendu: {json}");

        // Round-trip.
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plugin);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn au_item_round_trip() {
        let item = AuItem {
            au_type: "aufx".into(),
            subtype: "mrev".into(),
            manufacturer: "appl".into(),
        };
        assert_eq!(item.encode(), "au:aufx/mrev/appl");
        assert_eq!(AuItem::decode("au:aufx/mrev/appl"), Some(item));
        assert_eq!(AuItem::decode("au:aufx/mrev"), None);
        assert_eq!(AuItem::decode("au:a/b/c/d"), None);
        assert_eq!(AuItem::decode("C:\\VST3\\Foo.vst3"), None);
        assert_eq!(AuItem::decode("au://x"), None);
    }
}
