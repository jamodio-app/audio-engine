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
//! `cpal` expose `Device::as_inner() → DeviceInner::Asio(_)` dont le champ
//! `driver: Arc<asio_sys::Driver>` est `pub`. `asio_sys` étant le MÊME crate que
//! celui lié par cpal (version unifiée par Cargo), on se branche sur le registre
//! global de callbacks via `Driver::add_message_callback` — sans forker cpal.
//!
//! Le callback tourne sur le thread du driver (potentiellement temps-réel) : il
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
    /// Appelable depuis un callback de message ASIO enregistré directement via
    /// `asio-sys` (host single-owner), sans passer par un `cpal::Device`. Sûr sur le
    /// thread du driver : aucune allocation, aucun verrou bloquant.
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

/// Garde RAII de l'enregistrement du callback de message ASIO.
///
/// À la destruction, retire le callback du registre global d'`asio-sys` pour ne
/// pas accumuler de closures périmées (qui se déclencheraient à chaque reset des
/// drivers suivants). Volontairement détenu via un `Weak` : tenir une référence
/// forte sur le `Driver` empêcherait l'`ASIOExit` au drop des streams (last ref)
/// — exactement la ré-initialisation qu'on cherche à provoquer. Le garde doit
/// donc être droppé AVANT les streams (tant que cpal tient encore le driver
/// vivant), ce que garantit l'ordre de fermeture dans `pipeline.rs`.
#[cfg(windows)]
pub struct ResetCallbackGuard {
    weak_driver: Option<std::sync::Weak<asio_sys::Driver>>,
    cb_id: Option<asio_sys::MessageCallbackId>,
}

#[cfg(windows)]
impl Drop for ResetCallbackGuard {
    fn drop(&mut self) {
        if let (Some(weak), Some(id)) = (self.weak_driver.take(), self.cb_id.take()) {
            // `upgrade()` réussit tant que cpal tient encore le driver (streams
            // pas encore droppés). `remove_message_callback` ne touche que le
            // registre global (le `&self` n'est pas utilisé) — la ref forte
            // temporaire est relâchée aussitôt, sans empêcher l'`ASIOExit`.
            if let Some(driver) = weak.upgrade() {
                driver.remove_message_callback(id);
            }
        }
    }
}

/// Variante no-op hors Windows (pas d'ASIO).
#[cfg(not(windows))]
pub struct ResetCallbackGuard;

/// Enregistre le callback `kAsioResetRequest` sur le driver ASIO sous-jacent au
/// `cpal::Device` fourni, si — et seulement si — le host actif est ASIO.
///
/// À appeler sur le thread COM-STA (le device ASIO y est résolu/ouvert). Le
/// garde rendu doit être conservé tant que le stream vit, et droppé avant lui.
#[cfg(windows)]
pub fn register(device: &cpal::Device, signal: &ResetSignal) -> ResetCallbackGuard {
    use cpal::platform::DeviceInner;
    // WASAPI (ou tout host non-ASIO) ne souffre pas du wedge de reset → no-op.
    if let DeviceInner::Asio(asio_dev) = device.as_inner() {
        let requests = signal.requests.clone();
        let notify = signal.notify.clone();
        let cb_id = asio_dev.driver.add_message_callback(move |selector| {
            // Thread du driver, potentiellement temps-réel : aucune allocation,
            // aucun verrou bloquant, aucun appel ASIO ré-entrant. On signale.
            if matches!(selector, asio_sys::AsioMessageSelectors::kAsioResetRequest) {
                requests.fetch_add(1, Ordering::Relaxed);
                notify.notify_one();
            }
        });
        tracing::info!(
            target: "jamodio::audio",
            "callback de reset ASIO enregistré (kAsioResetRequest honoré)"
        );
        ResetCallbackGuard {
            weak_driver: Some(Arc::downgrade(&asio_dev.driver)),
            cb_id: Some(cb_id),
        }
    } else {
        ResetCallbackGuard {
            weak_driver: None,
            cb_id: None,
        }
    }
}

/// Hors Windows : aucun ASIO, garde vide.
#[cfg(not(windows))]
pub fn register(_device: &cpal::Device, _signal: &ResetSignal) -> ResetCallbackGuard {
    ResetCallbackGuard
}
