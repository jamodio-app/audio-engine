//! Isolation de voix talkback (agent-only) — enlève la repisse d'instrument du
//! canal talkback et coupe le canal quand personne ne parle.
//!
//! Chaîne cible (cf. `internal-docs/plans/PLAN-LOT2-INTEGRATION-ISOLATION-VOIX-2026-09.md`) :
//!
//! ```text
//! voix mono 48 kHz
//!    → denoise (DeepFilterNet, tract)      // enlève l'instrument, garde la voix
//!    → VAD (Silero) sur la voix NETTOYÉE   // décision « parole » fiable (instrument déjà retiré)
//!    → gate (attaque/hangover/relâche)     // silence total hors parole
//!    → × gain mute utilisateur
//! ```
//!
//! **Doctrine :** canal talkback UNIQUEMENT — jamais le monitoring instrument
//! temps-réel. **Zéro fallback silencieux** : si un modèle ne charge pas, l'appelant
//! reçoit une erreur explicite et bascule en talkback brut (voix non filtrée) en
//! l'indiquant à l'UI (décision Ben, plan §4).
//!
//! Construction incrémentale (Lot 2b) : `gate` (pur, ci-dessous) d'abord ; les
//! wrappers `denoise` (DeepFilterNet) et `vad` (Silero), puis le `resample` 48↔16 k
//! et l'orchestrateur `VoiceIsolator`, arrivent ensuite.

pub mod gate;

pub use gate::{GateParams, VoiceGate};
