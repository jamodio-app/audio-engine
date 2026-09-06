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
/// tract 0.21.4 — la version à laquelle nous sommes épinglés, parce que `deep_filter`
/// l'exige et que tract a cassé son API dans un patch 0.21.x — active l'**AMX d'Apple**
/// sans condition sur macOS (`has_amx()` renvoie `true`, en dur). Ce bloc d'instructions
/// non documenté **n'existe pas dans une machine virtuelle** : la première inférence tue
/// le PROCESSUS (SIGILL). Ce n'est pas une erreur qu'on rattrape — l'agent meurt.
/// Constaté le 05/09/2026 sur les runners GitHub macOS, qui sont des VM.
///
/// Ce n'est PAS un problème de génération de puce : sur une machine physique, l'AMX
/// fonctionne de M1 à M4, et les utilisateurs sur Mac réel ne sont pas concernés. tract
/// a corrigé exactement ce point dans une version ultérieure (test « (Virtual) » sur
/// `machdep.cpu.brand_string`) ; tant qu'on ne peut pas la prendre, on refait ce test
/// nous-mêmes — AVANT de charger le moindre modèle.
///
/// Mis en cache : `sysctl` n'a pas à être appelé à chaque ouverture de talkback, et la
/// réponse ne change pas en cours d'exécution.
pub(crate) fn macos_virtualise() -> bool {
    #[cfg(target_os = "macos")]
    {
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| {
            let virtualise = std::process::Command::new("sysctl")
                .args(["-n", "machdep.cpu.brand_string"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("(Virtual)"))
                .unwrap_or(false);
            if virtualise {
                tracing::warn!(
                    target: "jamodio::voice_isolation",
                    "macOS virtualisé — filtre antibruit REFUSÉ (l'AMX d'Apple, utilisé par \
                     le moteur d'inférence, n'existe pas en machine virtuelle et tuerait le \
                     processus). Le talkback continue en voix brute."
                );
            }
            virtualise
        })
    }
    #[cfg(not(target_os = "macos"))]
    false
}

/// Message rendu à l'utilisateur quand la machine ne peut pas faire tourner le filtre.
/// Il NOMME la cause : un « indisponible » sans raison ne s'explique pas au support.
pub(crate) const VM_NON_SUPPORTEE: &str =
    "macOS virtualisé : le moteur d'inférence a besoin de l'AMX d'Apple, absent en \
     machine virtuelle. Filtre antibruit indisponible sur cette machine.";

/// Les tests qui chargent un modèle sont écartés sur ces machines — et le DISENT dans
/// la sortie de test : un vert ne doit jamais pouvoir se lire comme « l'isolation de
/// voix a été vérifiée ici ».
#[cfg(test)]
pub(crate) fn inference_impossible_ici() -> bool {
    let virtualise = macos_virtualise();
    if virtualise {
        eprintln!(
            "\n⚠️  macOS VIRTUALISÉ : tests d'isolation de voix ÉCARTÉS. Ce vert ne dit \
             RIEN de l'isolation de voix — elle n'a pas été exercée ici.\n"
        );
    }
    virtualise
}

#[cfg(test)]
mod garde_vm_tests {
    use super::*;

    /// Le refus doit NOMMER sa cause. Un « filtre antibruit indisponible » sans
    /// raison ne s'explique pas au support, et pousse à chercher un défaut audio
    /// là où il n'y en a pas — c'est exactement ce qui nous est arrivé.
    #[test]
    fn le_message_de_refus_dit_pourquoi() {
        let m = VM_NON_SUPPORTEE.to_lowercase();
        assert!(m.contains("virtualis"), "le message doit dire que la machine est virtualisée");
        assert!(m.contains("amx"), "le message doit nommer l'instruction manquante");
    }

    /// Sur une machine physique, la garde ne doit RIEN bloquer — sinon on priverait
    /// du filtre antibruit tous les utilisateurs pour protéger un cas rare.
    /// (Sur une VM, ce test ne peut pas conclure : il ne prétend rien.)
    #[test]
    fn sur_machine_physique_la_garde_laisse_passer() {
        if macos_virtualise() {
            return;
        }
        assert!(
            Denoiser::new().is_ok(),
            "le débruiteur doit se charger normalement sur une machine physique"
        );
    }
}
