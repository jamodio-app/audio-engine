//! Sélection du host CPAL — source de vérité UNIQUE pour tout l'agent.
//!
//! # Pourquoi ce module (chantier ASIO, 11/06/2026)
//!
//! Historiquement chaque fonction de `device.rs` appelait
//! `cpal::default_host()` — soit WASAPI **shared** sur Windows, toujours,
//! alors même que l'agent est compilé avec la feature cpal `asio`.
//! Conséquence : +10-20 ms de latence évitable pour tout utilisateur
//! équipé d'un driver ASIO (constructeur ou FlexASIO/ASIO4ALL), sans
//! qu'il en soit informé. Cf. `internal-docs/plans/PLAN-ASIO-WINDOWS.md`.
//!
//! # Politique de sélection
//!
//! Décidée UNE fois au premier appel (OnceLock), puis immuable pour la vie
//! du process — un changement de host à chaud invaliderait les device ids
//! stricts `{idx}:{name}` déjà persistés côté browser et le format wire.
//! (Installer un driver ASIO → redémarrer l'agent, comme un DAW.)
//!
//! - **Windows** : ASIO si le host s'initialise ET expose ≥ 1 device
//!   d'entrée, sinon WASAPI. Le choix et sa raison sont loggés, et exposés
//!   au browser via `Devices.audioHost` pour que l'UI informe l'utilisateur
//!   (badge vert ASIO / badge orange WASAPI + lien support).
//! - **macOS** : CoreAudio (default host), inchangé.
//!
//! Conformément à la règle « pas de fallback silencieux » : la bascule
//! ASIO→WASAPI n'arrive qu'au BOOT (avant toute capture), est loggée, et
//! l'utilisateur la voit dans l'UI. Jamais en cours de session.

use std::sync::OnceLock;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HostKind {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Asio,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Wasapi,
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    CoreAudio,
}

impl HostKind {
    /// Nom wire (champ `audioHost` du message `Devices`).
    pub fn wire_name(self) -> &'static str {
        match self {
            HostKind::Asio => "asio",
            HostKind::Wasapi => "wasapi",
            HostKind::CoreAudio => "coreaudio",
        }
    }
}

static KIND: OnceLock<HostKind> = OnceLock::new();

/// Type de host actif — décidé au premier appel, immuable ensuite.
pub fn kind() -> HostKind {
    *KIND.get_or_init(probe)
}

/// Host CPAL actif. `cpal::Host` n'étant pas partageable entre threads de
/// façon garantie, on le (re)construit à partir du `kind()` mémorisé —
/// même coût que l'ancien `cpal::default_host()` par appel, mais désormais
/// UN seul point de décision.
pub fn active() -> cpal::Host {
    match kind() {
        #[cfg(target_os = "windows")]
        HostKind::Asio => cpal::host_from_id(cpal::HostId::Asio).unwrap_or_else(|e| {
            // Théoriquement impossible (le probe a réussi au boot) ; si un
            // driver disparaît à chaud, on le dit haut et fort plutôt que de
            // mentir sur la latence.
            tracing::error!(
                target: "jamodio::audio",
                error = %e,
                "host ASIO sélectionné au boot mais indisponible maintenant — retombe sur WASAPI (redémarrer l'agent)"
            );
            cpal::default_host()
        }),
        _ => cpal::default_host(),
    }
}

#[cfg(target_os = "windows")]
fn probe() -> HostKind {
    use cpal::traits::HostTrait;
    match cpal::host_from_id(cpal::HostId::Asio) {
        Ok(h) => {
            let n_in = h.input_devices().map(|it| it.count()).unwrap_or(0);
            let n_out = h.output_devices().map(|it| it.count()).unwrap_or(0);
            if n_in > 0 {
                tracing::info!(
                    target: "jamodio::audio",
                    inputs = n_in,
                    outputs = n_out,
                    "host audio = ASIO (latence optimale)"
                );
                HostKind::Asio
            } else {
                tracing::info!(
                    target: "jamodio::audio",
                    outputs = n_out,
                    "host audio = WASAPI (ASIO présent mais aucun device d'entrée)"
                );
                HostKind::Wasapi
            }
        }
        Err(e) => {
            tracing::info!(
                target: "jamodio::audio",
                error = %e,
                "host audio = WASAPI (pas de driver ASIO — installer le driver constructeur ou FlexASIO pour une latence optimale)"
            );
            HostKind::Wasapi
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn probe() -> HostKind {
    HostKind::CoreAudio
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_are_stable() {
        // Contrat wire avec le browser (agent-input-status.js) — ne JAMAIS
        // renommer sans migration côté web.
        assert_eq!(HostKind::Asio.wire_name(), "asio");
        assert_eq!(HostKind::Wasapi.wire_name(), "wasapi");
        assert_eq!(HostKind::CoreAudio.wire_name(), "coreaudio");
    }

    #[test]
    fn kind_is_stable_across_calls() {
        assert_eq!(kind(), kind());
    }
}
