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

pub mod denoise;
pub mod gate;
pub mod isolator;
pub mod resample;
pub mod vad;

pub use denoise::{DenoiseParams, Denoiser};
pub use gate::{GateParams, VoiceGate};
pub use isolator::{IsolationConfig, VoiceIsolator, VoiceState};
pub use vad::Vad;

/// Erreurs de l'isolation de voix. **Aucun fallback silencieux** : si un modèle
/// ne charge pas / échoue, l'appelant reçoit une erreur explicite et bascule en
/// talkback brut en l'indiquant à l'UI (décision Ben, plan §4).
#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    /// Chargement ou inférence du denoise (DeepFilterNet) en échec.
    #[error("denoise (DeepFilterNet) indisponible : {0}")]
    Denoise(String),
    /// Chargement ou inférence du VAD (Silero) en échec.
    #[error("VAD (Silero) indisponible : {0}")]
    Vad(String),
}

/// macOS **virtualisé** : le moteur d'inférence y exécute une instruction illégale.
///
/// tract 0.21.4 — la version à laquelle nous sommes épinglés, parce que
/// `deep_filter` l'exige et que tract a cassé son API dans un patch 0.21.x — active
/// l'**AMX d'Apple** sans condition sur macOS (`has_amx()` renvoie `true`, en dur).
/// Ce bloc d'instructions non documenté **n'est pas disponible dans une machine
/// virtuelle** : la première inférence tue le processus (SIGILL). Constaté le
/// 05/09/2026 sur les runners GitHub macOS, qui sont des VM.
///
/// Ce n'est PAS un problème de génération de puce : sur une machine physique, AMX
/// fonctionne de M1 à M4. Les utilisateurs sur Mac réel ne sont pas concernés.
/// tract a d'ailleurs corrigé exactement ce point plus tard (test « (Virtual) » sur
/// `machdep.cpu.brand_string`) ; nous ne pouvons pas encore prendre cette version.
///
/// Sert à écarter EXPLICITEMENT les tests qui chargent un modèle sur ces machines —
/// et à le DIRE dans la sortie de test, pour qu'un vert ne se lise jamais comme
/// « l'isolation de voix a été vérifiée ici ».
#[cfg(test)]
pub(crate) fn inference_impossible_ici() -> bool {
    #[cfg(target_os = "macos")]
    {
        let virtualise = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("(Virtual)"))
            .unwrap_or(false);
        if virtualise {
            eprintln!(
                "\n⚠️  macOS VIRTUALISÉ : tests d'isolation de voix ÉCARTÉS (tract 0.21.4 \
                 exécute l'AMX d'Apple, indisponible en VM → SIGILL). Ce vert ne dit RIEN \
                 de l'isolation de voix — elle n'a pas été exercée ici.\n"
            );
        }
        virtualise
    }
    #[cfg(not(target_os = "macos"))]
    false
}
