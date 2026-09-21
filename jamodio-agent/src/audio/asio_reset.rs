//! Honorer `kAsioResetRequest` — le handshake de reset que cpal 0.15 omet.
//!
//! # Cause racine (bug PC 28/06, cas dur)
//!
//! Le protocole ASIO impose qu'un driver qui doit se réinitialiser (resync
//! horloge/buffer USB, changement interne) envoie le message `kAsioResetRequest`
//! à l'hôte. Le contrat est strict : l'hôte répond « 1 » (« j'accepte, je gère »)
//! PUIS exécute lui-même, en différé sur un thread non temps-réel, la séquence
//! `ASIOStop → ASIODisposeBuffers → ASIOExit → ASIOInit → ASIOCreateBuffers →
//! ASIOStart`. C'est ce que fait tout DAW.
//!
//! Or `cpal 0.15.3` **n'enregistre AUCUN callback de message ASIO**. Conséquence
//! avec `asio-sys 0.2` : son handler interne `asio_message` répond bien « 1 » au
//! driver (« host gère le reset ») mais, le registre de callbacks utilisateur
//! étant vide, **n'exécute rien**. Le driver Focusrite croit donc que l'hôte va
//! le réinitialiser, arrête ses callbacks et attend… indéfiniment. Du point de
//! vue utilisateur : studio muet jusqu'à un débranchement/rebranchement physique
//! de l'interface USB (la seule façon de forcer le driver à repartir de zéro).
//!
//! # Ce que fait ce module
//!
//! L'hôte ASIO single-owner (`audio::asio_host::AsioDuplexHost`, seul chemin
//! ASIO sous Windows) enregistre lui-même un callback de message sur le driver
//! qu'il possède (`Driver::add_message_callback`, registre global d'`asio-sys`).
//! Ce module fournit le canal entre ce callback et le superviseur.
//!
//! Le callback tourne sur le thread du driver (à traiter comme temps-réel) : il
//! ne fait donc QUE signaler (incrément atomique + `Notify`). Le reset réel
//! (séquence ASIO complète) est exécuté en différé, sur le thread COM-STA, par
//! `ws_server::audio_liveness_supervisor` dès réception du signal — au moment où
//! le driver le demande, et non 1,5 s plus tard via le sondage de liveness.
//!
//! macOS/Linux : pas d'ASIO → tout est no-op (la `ResetSignal` n'est jamais
//! signalée, `register` rend un garde vide).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

// ═══════════════════════════════════════════════════════════════════════════
// Les AUTRES signaux du pilote — ceux qu'on ne voyait pas
// ═══════════════════════════════════════════════════════════════════════════
//
// `kAsioResetRequest` n'est pas la seule chose qu'un pilote sait dire. Il peut
// aussi annoncer qu'il a PERDU des données (`kAsioResyncRequest`), que ses
// latences ont changé, qu'il a décroché, ou que le sample rate a bougé. Le crate
// `asio-sys` publié interceptait les trois premiers sans les transmettre et
// envoyait le quatrième dans un `eprintln!` vers une sortie que personne ne lit :
// après deux épisodes de son dégradé (18/09/2026), impossible de savoir si le
// Focusrite avait crié. Notre copie patchée (`vendor/asio-sys`) les compte.
//
// Comptés DANS `asio-sys`, en atomiques statiques (`asio_sys::driver_message_counts`,
// `asio_sys::sample_rate_change_report`) : le pilote nous appelle depuis son
// thread, qui ne doit subir ni verrou ni allocation — un incrément, rien d'autre.
// La lecture et la journalisation sont à 1 Hz, dans le superviseur de liveness,
// hors temps-réel. Aucune décision ne s'y appuie : ce sont des FAITS pour le
// rapport de bug, pas un verdict.

/// Ce que le(s) pilote(s) ASIO ont signalé depuis le démarrage de l'agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriverNotices {
    /// « J'ai perdu des données » — le signal le plus intéressant pour un son sale.
    pub resync: u64,
    /// Les latences déclarées à l'ouverture ne sont plus les bonnes.
    pub latencies_changed: u64,
    /// Le pilote dit qu'il a décroché.
    pub overload: u64,
    /// Changements de sample rate annoncés, et dernier rate annoncé (Hz).
    pub sample_rate_changes: u64,
    pub last_reported_rate_hz: u32,
}

impl DriverNotices {
    /// `true` si le pilote n'a jamais rien signalé — le cas nominal, pour lequel
    /// le superviseur n'écrit aucune ligne.
    pub fn is_quiet(&self) -> bool {
        *self == Self::default()
    }
}

/// Instantané cumulé (depuis le démarrage de l'agent), pour le superviseur.
/// Lecture sans verrou et non destructive des compteurs d'`asio-sys`.
#[cfg(windows)]
pub fn driver_notices() -> DriverNotices {
    let counts = asio_sys::driver_message_counts();
    let (sample_rate_changes, last_reported_rate_hz) = asio_sys::sample_rate_change_report();
    DriverNotices {
        resync: counts.resync_requests,
        latencies_changed: counts.latencies_changed,
        overload: counts.overloads,
        sample_rate_changes,
        last_reported_rate_hz,
    }
}

/// Hors Windows : pas d'ASIO, donc rien à signaler — jamais une valeur inventée.
#[cfg(not(windows))]
pub fn driver_notices() -> DriverNotices {
    DriverNotices::default()
}

/// Canal de signalisation entre le callback de message ASIO (thread du driver)
/// et le superviseur de liveness. Clonable : une extrémité dans le callback,
/// l'autre dans le superviseur.
#[derive(Clone)]
pub struct ResetSignal {
    /// Cumul des `kAsioResetRequest` reçus. Le superviseur compare un delta pour
    /// savoir si un nouveau reset a été demandé depuis sa dernière observation.
    requests: Arc<AtomicU64>,
    /// Réveille le superviseur immédiatement, sans attendre son tick périodique.
    notify: Arc<Notify>,
}

impl ResetSignal {
    pub fn new() -> Self {
        Self {
            requests: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Nombre cumulé de resets demandés par le(s) driver(s) ASIO depuis le boot.
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Signale un `kAsioResetRequest` (incrément atomique + réveil du superviseur).
    /// Appelé par le callback de message qu'`AsioDuplexHost` enregistre sur son
    /// driver. Sur le thread du driver, le coût est : un incrément atomique, et
    /// `Notify::notify_one` (quelques opérations atomiques, sans allocation ; il
    /// peut prendre brièvement le verrou interne de `Notify` si une tâche est en
    /// attente — rare : un par reset demandé, jamais par bloc audio).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))] // appelé uniquement côté ASIO (Windows)
    pub fn signal(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_one();
    }

    /// Handle pour `select!`/`.notified().await` côté superviseur.
    pub fn notify_handle(&self) -> Arc<Notify> {
        self.notify.clone()
    }
}

impl Default for ResetSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Garde du callback de reset posé sur le chemin cpal — aujourd'hui toujours vide.
///
/// Sous Windows, ASIO passe EXCLUSIVEMENT par `AsioDuplexHost` (cf.
/// `pipeline::asio_host_enabled` : dès que l'hôte actif est ASIO, la branche cpal
/// n'est jamais atteinte), et ce host enregistre lui-même son callback de message.
/// Le chemin cpal ne sert donc qu'à CoreAudio et WASAPI, qui n'ont pas de
/// handshake de reset : il n'y a rien à enregistrer. Le type et `register`
/// subsistent uniquement parce que `pipeline.rs` les tient encore dans
/// `BuiltDuplex::Cpal` ; les retirer demande de toucher ce fichier.
pub struct ResetCallbackGuard;

/// No-op : voir `ResetCallbackGuard` — le chemin cpal n'est jamais ASIO.
pub fn register(_device: &cpal::Device, _signal: &ResetSignal) -> ResetCallbackGuard {
    ResetCallbackGuard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_pilote_qui_na_rien_signale_reste_silencieux() {
        // Le superviseur ne doit écrire AUCUNE ligne tant que le pilote n'a rien
        // dit : c'est ce que garantit `is_quiet` — une session saine ne remplit
        // pas le journal, et chaque ligne présente désigne un vrai incident.
        assert!(DriverNotices::default().is_quiet());
    }

    #[test]
    fn un_seul_signal_suffit_a_rompre_le_silence() {
        for notices in [
            DriverNotices {
                resync: 1,
                ..Default::default()
            },
            DriverNotices {
                latencies_changed: 1,
                ..Default::default()
            },
            DriverNotices {
                overload: 1,
                ..Default::default()
            },
            DriverNotices {
                sample_rate_changes: 1,
                last_reported_rate_hz: 44_100,
                ..Default::default()
            },
        ] {
            assert!(!notices.is_quiet(), "{notices:?} devrait être journalisé");
        }
    }

    #[test]
    fn hors_windows_aucun_signal_nest_inventé() {
        // Pas d'ASIO hors Windows : l'instantané doit rester vide, jamais une
        // valeur par défaut qui ressemblerait à une mesure.
        #[cfg(not(windows))]
        assert!(driver_notices().is_quiet());
    }
}
