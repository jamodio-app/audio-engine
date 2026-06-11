//! `ConnectionProxy` — wrapper `IConnectionPoint` thread-safe pour briser le
//! deadlock entre component (RT audio thread) et controller (UI thread STA).
//!
//! Pattern Steinberg SDK (`public.sdk/source/vst/hosting/connectionproxy.cpp`) :
//! sans ce proxy, brancher directement les 2 `IConnectionPoint` du plugin
//! (component ↔ controller) permet au plugin de marshalize des `notify()`
//! cross-thread → l'éditeur STA appelle attached() qui peut bloquer sur un
//! notify renvoyé depuis l'audio thread vers l'UI thread (qui est nous,
//! occupé dans attached()) → deadlock circulaire.
//!
//! Le ThreadChecker stocke le `thread_id` de création du proxy. À chaque
//! `notify()`, on vérifie que `GetCurrentThreadId() == thread_id`, sinon on
//! drop le message (kResultFalse). Asymétrique : seul le thread créateur
//! (= UI thread STA = celui qui ouvre l'éditeur) peut envoyer des notify.
//! Les notify depuis l'audio thread sont silencieusement éliminés — c'est
//! exactement ce que fait le SDK.
//!
//! Coût : on perd potentiellement des messages component → controller depuis
//! le thread audio (= rare, surtout pour les param changes de l'utilisateur
//! qui partent toujours du UI thread). Pas un problème pour le live.

#![cfg(target_os = "windows")]

use std::sync::Mutex;

use vst3::{
    Class, ComPtr,
    Steinberg::{
        kResultFalse, kResultOk, tresult,
        Vst::{IConnectionPoint, IConnectionPointTrait, IMessage},
    },
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

/// Proxy IConnectionPoint qui filtre les notify() pour autoriser seulement
/// le thread qui a créé le proxy.
pub struct ConnectionProxy {
    /// Destination réelle des `notify()` (= le peer côté plugin que le proxy
    /// adresse). Set par `set_dst()` après création, juste avant d'installer
    /// le proxy via `comp_cp.connect(proxy_ptr)`.
    dst: Mutex<Option<ComPtr<IConnectionPoint>>>,
    /// Thread ID au moment de la création — c'est ce thread (UI STA = editor)
    /// qui sera autorisé à envoyer des notify à travers ce proxy.
    owner_thread_id: u32,
}

impl ConnectionProxy {
    pub fn new() -> Self {
        Self {
            dst: Mutex::new(None),
            owner_thread_id: unsafe { GetCurrentThreadId() },
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

    /// Forward le notify au peer SI ET SEULEMENT SI on est sur le thread
    /// créateur du proxy (= UI thread STA). Sinon drop silencieusement
    /// (kResultFalse) pour éviter un marshaling cross-thread qui causerait
    /// un deadlock pendant `IPlugView::attached()`.
    unsafe fn notify(&self, message: *mut IMessage) -> tresult {
        if GetCurrentThreadId() != self.owner_thread_id {
            // Drop : pas sur le thread autorisé. C'est exactement ce que fait
            // le SDK Steinberg (cf. threadchecker_win32.cpp).
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
