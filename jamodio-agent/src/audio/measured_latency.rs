//! Latences matérielles MESURÉES par Jamodio, pour les périphériques dont la
//! déclaration du système est connue pour être fausse.
//!
//! La latence déclarée par le pilote reste la règle (`declared_latency`). Quand le
//! banc de latence (dépôt web : `internal-docs/plans/PROTOCOLE-BANC-LATENCE-2026-09.md`)
//! prouve qu'un périphérique déclare faux, sa mesure entre dans cette table avec la
//! façon de le reconnaître : par sa nature telle que le système la déclare, jamais
//! par son nom (traduit selon la langue du système, renommable).
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use super::declared_latency::Scope;
use jamodio_audio_core::protocol::HwDeviceKind;

/// Identité CoreAudio d'un périphérique ouvert, telle que le système la déclare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreAudioIdentity {
    pub scope: Scope,
    /// Transport `kAudioDeviceTransportTypeBuiltIn`.
    pub built_in: bool,
    /// Source de données du sens ouvert (`kAudioDevicePropertyDataSource`), code à
    /// quatre caractères ; `None` si le périphérique n'en déclare pas.
    pub data_source: Option<u32>,
}

/// Une mesure du banc : la latence au-delà du buffer, en millisecondes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measured {
    pub kind: HwDeviceKind,
    pub hw_ms: f32,
}

struct CoreAudioEntry {
    scope: Scope,
    built_in: bool,
    data_source: u32,
    measured: Measured,
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const COREAUDIO: &[CoreAudioEntry] = &[
    // Micro intégré des Mac (source `imic`). CoreAudio déclare 51,02 ms au-delà du
    // buffer (appareil 0 + marge 50 + flux 2399 trames) ; le banc du 15/09/2026 en
    // mesure ≈ 30 : chaîne complète Mac → PC mesurée à 61,5 ms au micro (29,8 ms une
    // fois les autres étapes retirées), et boucles datées par les horodatages du
    // système, à sortie identique, 30,3 ms de plus que le micro d'un casque jack.
    // Relevé sur MacBook Pro (Mac15,11) ; cohérent avec les relevés publics des Mac
    // depuis 2016.
    CoreAudioEntry {
        scope: Scope::Input,
        built_in: true,
        data_source: fourcc(b"imic"),
        measured: Measured {
            kind: HwDeviceKind::AppleBuiltInMic,
            hw_ms: 30.0,
        },
    },
];

/// La mesure Jamodio du périphérique CoreAudio ouvert, s'il est dans la table.
pub fn coreaudio(identity: CoreAudioIdentity) -> Option<Measured> {
    COREAUDIO
        .iter()
        .find(|e| {
            e.scope == identity.scope
                && e.built_in == identity.built_in
                && identity.data_source == Some(e.data_source)
        })
        .map(|e| e.measured)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(scope: Scope, built_in: bool, data_source: Option<&[u8; 4]>) -> CoreAudioIdentity {
        CoreAudioIdentity {
            scope,
            built_in,
            data_source: data_source.map(fourcc),
        }
    }

    #[test]
    fn micro_integre_du_mac_reconnu_a_30_ms() {
        let m = coreaudio(identity(Scope::Input, true, Some(b"imic"))).expect("micro intégré");
        assert_eq!(m.kind, HwDeviceKind::AppleBuiltInMic);
        assert_eq!(m.hw_ms, 30.0);
    }

    #[test]
    fn micro_du_casque_jack_garde_sa_declaration() {
        // Même périphérique intégré, source « micro externe » : pas dans la table.
        assert_eq!(coreaudio(identity(Scope::Input, true, Some(b"emic"))), None);
    }

    #[test]
    fn source_imic_hors_appareil_integre_ou_en_sortie_rien() {
        assert_eq!(coreaudio(identity(Scope::Input, false, Some(b"imic"))), None);
        assert_eq!(coreaudio(identity(Scope::Output, true, Some(b"imic"))), None);
    }

    #[test]
    fn sans_source_declaree_rien() {
        assert_eq!(coreaudio(identity(Scope::Input, true, None)), None);
    }

    #[test]
    fn code_a_quatre_caracteres_gros_boutiste() {
        // Valeur de `kAudioDevicePropertyDataSource` rendue par CoreAudio pour « imic ».
        assert_eq!(fourcc(b"imic"), 0x696d_6963);
    }
}
