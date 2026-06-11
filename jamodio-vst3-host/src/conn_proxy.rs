//! `ConnectionProxy` — wrapper `IConnectionPoint` qui bloque uniquement les
//! notify() venant du thread audio RT.
//!
//! Adapté du pattern Steinberg SDK (`public.sdk/source/vst/hosting/connectionproxy.cpp`),
//! que les vrais hosts (Cubase…) insèrent systématiquement entre component et
//! controller. Conséquence connue : les plugins JUCE ne peuvent plus faire
//! leur cast direct privé à travers la connexion et basculent sur un
//! handshake par message (`sendIntMessage`) — qui exige que le host
//! implémente `IHostApplication::createInstance(IMessage)` (cf. host_app.rs).
//! C'était la VRAIE cause du `createView() == null` en v0.4.25/26 — pas le
//! filtre de threads.
//!
//! Notre filtre (plus permissif que celui du SDK, qui n'autorise que le
//! thread créateur) :
//! - Notify depuis le thread audio RT (= encoder_thread, marqué via
//!   `register_audio_thread()`) → drop (kResultFalse) : un plugin qui
//!   notifie depuis `process()` ne doit pas toucher des objets UI.
//! - Notify depuis n'importe quel autre thread → forward vers le peer.
//!   Depuis le refactor vst3-main (v0.4.27), component ET controller vivent
//!   sur le même thread qui pompe — le forward est donc toujours direct.
//!
//! Coût : on perd les notify component → controller émis pendant `process()`
//! (param changes générés par l'audio thread, rare en pratique : l'utilisateur
//! tourne les knobs depuis l'UI, pas l'inverse). Pas un problème pour le live.

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use vst3::{
    Class, ComPtr,
    Steinberg::{
        kResultFalse, kResultOk, tresult,
        Vst::{IConnectionPoint, IConnectionPointTrait, IMessage},
    },
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

/// ID du thread audio RT (= encoder_thread Jamodio). Renseigné une fois au
/// démarrage du thread via `register_audio_thread()`. `0` = pas encore set
/// (cas du scan plugin au boot, avant que le studio démarre une capture).
static AUDIO_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Doit être appelé UNE fois au démarrage du thread audio (encoder_thread)
/// pour permettre au ConnectionProxy de filtrer les notify() venant de ce
/// thread. Pas grave si appelé plusieurs fois (= overwrite atomic), mais
/// l'idée est : un seul thread RT vivant à la fois.
pub fn register_audio_thread() {
    let id = unsafe { GetCurrentThreadId() };
    AUDIO_THREAD_ID.store(id, Ordering::SeqCst);
    tracing::info!(
        target: "jamodio::vst3",
        thread_id = id,
        "audio thread registered for ConnectionProxy filter"
    );
}

#[inline]
fn is_audio_thread() -> bool {
    let id = AUDIO_THREAD_ID.load(Ordering::Relaxed);
    id != 0 && id == unsafe { GetCurrentThreadId() }
}

/// Proxy IConnectionPoint qui drop les notify() venant du thread audio RT.
pub struct ConnectionProxy {
    /// Destination réelle des `notify()` (= le peer côté plugin que le proxy
    /// adresse). Set par `set_dst()` après création, juste avant d'installer
    /// le proxy via `comp_cp.connect(proxy_ptr)`.
    dst: Mutex<Option<ComPtr<IConnectionPoint>>>,
}

impl ConnectionProxy {
    pub fn new() -> Self {
        Self {
            dst: Mutex::new(None),
        }
    }

    /// Installe la destination du proxy. Doit être appelé AVANT que le plugin
    /// fasse un `notify()` à travers le proxy.
    pub fn set_dst(&self, dst: ComPtr<IConnectionPoint>) {
        if let Ok(mut g) = self.dst.lock() {
            *g = Some(dst);
        }
    }
}

impl Default for ConnectionProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for ConnectionProxy {
    type Interfaces = (IConnectionPoint,);
}

impl IConnectionPointTrait for ConnectionProxy {
    /// No-op : le wiring est fait côté host externe (cf. editor.rs
    /// `connect_component_to_controller_via_proxy`), pas via l'API connect()
    /// du proxy. Cette méthode existe pour respecter le trait IConnectionPoint
    /// mais le plugin n'a aucune raison de l'appeler.
    unsafe fn connect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultOk
    }

    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultOk
    }

    /// Forward le notify au peer SAUF si on est sur le thread audio RT
    /// (= drop pour éviter le marshaling deadlock vers editor STA pendant
    /// attached()). Tous les autres threads (WS load, editor STA, etc.)
    /// peuvent émettre — c'est nécessaire pour l'initialisation du
    /// controller (state initial component → controller) et pour les
    /// param changes UI → component.
    unsafe fn notify(&self, message: *mut IMessage) -> tresult {
        if is_audio_thread() {
            return kResultFalse;
        }
        let dst = match self.dst.lock() {
            Ok(g) => g,
            Err(_) => return kResultFalse,
        };
        match dst.as_ref() {
            Some(d) => d.notify(message),
            None => kResultFalse,
        }
    }
}
