//! Éditeur natif Win32 — fenêtre HWND hébergée sur le thread vst3-main.
//!
//! # Architecture (v3 — single main thread)
//!
//! TOUT le cycle de vie de l'éditeur (resolve controller, connect, createView,
//! HWND, setFrame, attached, removed) s'exécute sur le thread **vst3-main**
//! (cf. `main_thread.rs`) — le MÊME thread qui a chargé le module et créé le
//! component. C'est la règle "single main thread" de la spec VST3, et c'est
//! ce qu'attendent les plugins JUCE : leur MessageManager est lié au thread
//! qui a construit la factory ; un `attached()` venant d'un autre thread
//! marshale la création de fenêtre vers ce thread via
//! `callFunctionOnMessageThread` et bloque pour toujours si personne n'y
//! pompe (= le hang v0.4.24).
//!
//! Plus de thread par fenêtre : les messages des fenêtres d'éditeur sont
//! dispatchés par la pump centrale de vst3-main. L'état de chaque fenêtre
//! (ComPtrs keep-alive) vit dans une registry thread-local indexée par HWND,
//! nettoyée dans le wnd_proc sur WM_DESTROY (→ `view.removed()` + release
//! dans l'ordre correct, AVANT tout terminate du component).
//!
//! # Séquence du job d'ouverture (sur vst3-main)
//! 1. Crée `IHostApplication` (avec IMessage/IAttributeList, cf. host_app.rs)
//! 2. `resolve_controller` (cast IComponent → IEditController, ou createInstance
//!    de la classe controller séparée + initialize avec hostContext)
//! 3. `IConnectionPoint::connect` via ConnectionProxy (le handshake JUCE
//!    passe par sendIntMessage → host_app.createInstance(IMessage))
//! 4. `IComponent::getState` → `IEditController::setComponentState` (toléré E_NOTIMPL)
//! 5. `controller.setComponentHandler()` (IComponentHandler minimal)
//! 6. `controller.createView("editor")`
//! 7. `view.isPlatformTypeSupported("HWND")`, `view.getSize()`
//! 8. Crée la HWND parent Win32
//! 9. `view.setFrame()`, `view.attached(hwnd, "HWND")`, `view.onSize()`
//! 10. Enregistre l'état dans la registry — la pump centrale prend le relais

#![cfg(target_os = "windows")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use std::sync::Arc;

use vst3::{
    Class, ComPtr, ComWrapper,
    Steinberg::{
        int32, kResultOk, tresult, FUnknown, IBStream, IBStreamTrait,
        IBStream_::IStreamSeekMode_, IPluginBaseTrait, IPluginFactoryTrait, TUID,
        Vst::{
            IComponent, IComponentHandler, IComponentHandlerTrait, IComponentTrait,
            IConnectionPoint, IConnectionPointTrait, IEditController, IEditControllerTrait,
            IEditController_iid, IHostApplication, ParamID, ParamValue,
        },
        IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, ViewRect,
    },
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SetWindowTextW,
        ShowWindow, SW_SHOW, WM_DESTROY, WNDCLASSEXW, WS_EX_DLGMODALFRAME,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    },
};

use crate::conn_proxy::ConnectionProxy;
use crate::host::Instance;
use crate::host_app::MinimalHost;
use crate::loader::LoadedModule;
use crate::main_thread;
use crate::state::MemoryStream;

const HOST_NAMESPACE: &str = "JAMOEDITOR";
const PLATFORM_HWND: &[u8] = b"HWND\0";

/// États de `EditorShared::state`.
const STATE_PENDING: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_CLOSED: u8 = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------- IComponentHandler minimal ----------

struct MinimalHandler;

impl Class for MinimalHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for MinimalHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, _id: ParamID, _value_normalized: ParamValue) -> tresult {
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

// ---------- IPlugFrame minimal ----------

struct PlugFrame;

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, _new_size: *mut ViewRect) -> tresult {
        kResultOk
    }
}

// ---------- Registry des fenêtres ouvertes (thread-local vst3-main) ----------

/// Keep-alives d'une fenêtre d'éditeur ouverte. Tout est droppé dans le
/// wnd_proc sur WM_DESTROY, dans l'ordre de déclaration : frame → view →
/// handler → proxies → controller → host_app → component → module. Le plugin
/// cache les pointeurs des proxies / du handler → les release trop tôt =
/// use-after-free.
struct EditorState {
    #[allow(dead_code)]
    frame: ComPtr<IPlugFrame>,
    view: ComPtr<IPlugView>,
    #[allow(dead_code)]
    handler: ComPtr<IComponentHandler>,
    #[allow(dead_code)]
    proxies: Option<(ComWrapper<ConnectionProxy>, ComWrapper<ConnectionProxy>)>,
    #[allow(dead_code)]
    controller: ComPtr<IEditController>,
    #[allow(dead_code)]
    host_app: ComPtr<IHostApplication>,
    #[allow(dead_code)]
    component: ComPtr<IComponent>,
    #[allow(dead_code)]
    module: Arc<LoadedModule>,
    shared: Arc<EditorShared>,
}

thread_local! {
    /// HWND (as isize) → état. Uniquement touché sur vst3-main.
    static OPEN_EDITORS: RefCell<HashMap<isize, EditorState>> = RefCell::new(HashMap::new());
}

// ---------- EditorWindow ----------

/// État partagé entre le handle public (côté Vst3Host) et vst3-main.
struct EditorShared {
    /// HWND de la fenêtre — null tant que le setup n'a pas abouti, re-null
    /// après destruction.
    hwnd: AtomicPtr<c_void>,
    /// STATE_PENDING → STATE_OPEN → STATE_CLOSED (ou PENDING → CLOSED si le
    /// setup échoue).
    state: AtomicU8,
}

/// Handle public exposé au `Vst3Host`. L'ouverture est asynchrone (job posté
/// sur vst3-main) pour ne pas bloquer le caller (qui tient le lock
/// plugin_host pendant que l'encoder thread en a besoin).
pub struct EditorWindow {
    shared: Arc<EditorShared>,
}

impl EditorWindow {
    /// Poste le job d'ouverture sur vst3-main — retourne immédiatement.
    /// Les erreurs du setup sont loggées sur vst3-main (le caller WS ne peut
    /// rien en faire de toute façon).
    pub fn open(
        instance: &Instance,
        module: Arc<LoadedModule>,
        title: &str,
    ) -> Result<Self, String> {
        let shared = Arc::new(EditorShared {
            hwnd: AtomicPtr::new(std::ptr::null_mut()),
            state: AtomicU8::new(STATE_PENDING),
        });
        let shared_job = shared.clone();
        let component = instance.component.clone();
        let title = title.to_string();

        main_thread::post(move || {
            if let Err(e) = open_editor_on_main_thread(component, module, &title, &shared_job) {
                tracing::error!(target: "jamodio::vst3::editor", error = %e, "editor setup failed");
                shared_job.state.store(STATE_CLOSED, Ordering::SeqCst);
            }
        });

        Ok(Self { shared })
    }

    /// `true` si la fenêtre a été fermée (X utilisateur, échec de setup,
    /// close explicite) — permet au Vst3Host d'autoriser une réouverture.
    pub fn is_closed(&self) -> bool {
        self.shared.state.load(Ordering::SeqCst) == STATE_CLOSED
    }

    /// Détruit la fenêtre. Posté en FIFO sur vst3-main : si le job
    /// d'ouverture est encore en queue, il s'exécute d'abord — pas de race.
    /// Depuis vst3-main, exécution inline (synchrone) — utilisé par unload
    /// pour garantir `view.removed()` AVANT `component.terminate()`.
    pub fn close(&mut self) {
        let shared = self.shared.clone();
        main_thread::post(move || {
            let hwnd = shared.hwnd.load(Ordering::SeqCst);
            if !hwnd.is_null() {
                unsafe {
                    DestroyWindow(hwnd as HWND);
                }
            }
        });
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------- wnd_proc + setup (vst3-main uniquement) ----------

unsafe extern "system" fn editor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DESTROY {
        // Retire l'état AVANT de dropper : view.removed() pendant que la
        // HWND existe encore, puis release des ComPtrs (ordre des champs).
        let state = OPEN_EDITORS.with(|m| m.borrow_mut().remove(&(hwnd as isize)));
        if let Some(st) = state {
            let _ = st.view.removed();
            st.shared.hwnd.store(std::ptr::null_mut(), Ordering::SeqCst);
            st.shared.state.store(STATE_CLOSED, Ordering::SeqCst);
            tracing::info!(target: "jamodio::vst3::editor", "editor window destroyed, plugin view removed");
            // drop(st) → release frame/view/handler/proxies/controller/…
        }
        return 0;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn ensure_window_class() {
    use std::sync::Once;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let class_name = wide(HOST_NAMESPACE);
        let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(editor_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        unsafe {
            let _ = RegisterClassExW(&wc);
        }
    });
}

/// Tout le setup COM + fenêtre — s'exécute sur vst3-main.
fn open_editor_on_main_thread(
    component: ComPtr<IComponent>,
    module: Arc<LoadedModule>,
    title: &str,
    shared: &Arc<EditorShared>,
) -> Result<(), String> {
    debug_assert!(main_thread::on_main_thread());
    tracing::info!(
        target: "jamodio::vst3::editor",
        title = %title,
        "opening editor on vst3-main"
    );

    // 1. IHostApplication — context du controller. Fournit IMessage et
    //    IAttributeList (handshake JUCE via ConnectionProxy, cf. host_app.rs).
    let host_app_wrapper = ComWrapper::new(MinimalHost);
    let host_app: ComPtr<IHostApplication> = host_app_wrapper
        .to_com_ptr::<IHostApplication>()
        .ok_or("ComWrapper::to_com_ptr<IHostApplication> a échoué")?;

    // 2. Resolve le controller sur CE thread (= le thread de la factory, le
    //    seul que les plugins considèrent comme "main").
    let controller = resolve_controller(&component, &module, &host_app)?;

    // 3. IConnectionPoint bidirectionnel via proxies thread-checked (pattern
    //    SDK Steinberg, cf. conn_proxy.rs). Les notify du thread audio RT
    //    sont droppés, tout le reste est forwardé. Le handshake JUCE
    //    controller→component (sendIntMessage) passe ici, pendant connect().
    let proxies = connect_component_to_controller_via_proxies(&component, &controller);

    // 4. State sync via stream (toléré E_NOTIMPL).
    sync_component_state(&component, &controller);

    // 5. IComponentHandler.
    let handler_wrapper = ComWrapper::new(MinimalHandler);
    let handler: ComPtr<IComponentHandler> = handler_wrapper
        .to_com_ptr::<IComponentHandler>()
        .ok_or("ComWrapper::to_com_ptr<IComponentHandler> a échoué")?;
    let set_ok = unsafe { controller.setComponentHandler(handler.as_ptr()) };
    tracing::info!(
        target: "jamodio::vst3::editor",
        tresult = set_ok,
        "setComponentHandler"
    );

    // 6. createView.
    let view_ptr = unsafe {
        let cstr = b"editor\0";
        controller.createView(cstr.as_ptr() as *const i8)
    };
    if view_ptr.is_null() {
        return Err("createView('editor') retourne null — le plugin refuse de fournir une UI".into());
    }
    let view = unsafe { ComPtr::<IPlugView>::from_raw(view_ptr) }
        .ok_or("ComPtr::from_raw(view) NULL")?;
    tracing::info!(target: "jamodio::vst3::editor", "createView ok");

    // 7. Platform check + size.
    let plat_ok = unsafe { view.isPlatformTypeSupported(PLATFORM_HWND.as_ptr() as *const i8) };
    if plat_ok != kResultOk {
        return Err(format!(
            "plugin ne supporte pas la plateforme HWND (tresult={plat_ok})"
        ));
    }
    let mut size = ViewRect { left: 0, top: 0, right: 800, bottom: 600 };
    let _ = unsafe { view.getSize(&mut size) };
    let width = (size.right - size.left).max(100);
    let height = (size.bottom - size.top).max(100);
    tracing::info!(
        target: "jamodio::vst3::editor",
        width, height,
        "size demandée par le plugin"
    );

    // 8. HWND parent.
    ensure_window_class();
    let title_w = wide(title);
    let class_w = wide(HOST_NAMESPACE);
    let hinst = unsafe {
        windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null())
    };
    let hwnd: HWND = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class_w.as_ptr(),
            title_w.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            -2_147_483_648,
            -2_147_483_648,
            width + 16,
            height + 40,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("CreateWindowExW returned null".into());
    }
    unsafe {
        SetWindowTextW(hwnd, title_w.as_ptr());
        ShowWindow(hwnd, SW_SHOW);
    }
    tracing::info!(target: "jamodio::vst3::editor", "HWND créée et affichée");

    // 9. IPlugFrame + attached. Sur CE thread (= message thread JUCE), la
    //    création du peer JUCE est directe — pas de marshaling cross-thread.
    let frame_wrapper = ComWrapper::new(PlugFrame);
    let frame: ComPtr<IPlugFrame> = match frame_wrapper.to_com_ptr::<IPlugFrame>() {
        Some(f) => f,
        None => {
            unsafe { DestroyWindow(hwnd) };
            return Err("ComWrapper::to_com_ptr<IPlugFrame> a échoué".into());
        }
    };
    let set_frame_ok = unsafe { view.setFrame(frame.as_ptr()) };
    tracing::info!(target: "jamodio::vst3::editor", tresult = set_frame_ok, "setFrame");

    tracing::info!(target: "jamodio::vst3::editor", "calling attached…");
    let att_ok = unsafe { view.attached(hwnd as *mut c_void, PLATFORM_HWND.as_ptr() as *const i8) };
    if att_ok != kResultOk {
        unsafe { DestroyWindow(hwnd) };
        return Err(format!("IPlugView::attached failed (tresult={att_ok})"));
    }
    tracing::info!(target: "jamodio::vst3::editor", "IPlugView::attached ok");

    // 10. onSize après attach.
    let _ = unsafe {
        view.onSize(&ViewRect {
            left: 0, top: 0, right: width, bottom: height,
        } as *const _ as *mut ViewRect)
    };

    // 11. Publie l'état — la pump centrale de vst3-main dispatch désormais
    //     les messages de cette fenêtre ; cleanup sur WM_DESTROY.
    shared.hwnd.store(hwnd as *mut c_void, Ordering::SeqCst);
    shared.state.store(STATE_OPEN, Ordering::SeqCst);
    OPEN_EDITORS.with(|m| {
        m.borrow_mut().insert(
            hwnd as isize,
            EditorState {
                frame,
                view,
                handler,
                proxies,
                controller,
                host_app,
                component,
                module,
                shared: shared.clone(),
            },
        )
    });
    Ok(())
}

// ---------- Helpers (tournent sur vst3-main) ----------

/// Synchronise l'état du `IComponent` vers le `IEditController` via un
/// `IBStream` mémoire éphémère. Tolérant à E_NOTIMPL.
fn sync_component_state(component: &ComPtr<IComponent>, controller: &ComPtr<IEditController>) {
    let stream_wrapper = ComWrapper::new(MemoryStream::new());
    let stream: ComPtr<IBStream> = match stream_wrapper.to_com_ptr::<IBStream>() {
        Some(s) => s,
        None => {
            tracing::warn!(target: "jamodio::vst3::editor", "MemoryStream to_com_ptr échoué — state sync skipped");
            return;
        }
    };

    let get_ok = unsafe { component.getState(stream.as_ptr()) };
    if get_ok != kResultOk {
        tracing::warn!(target: "jamodio::vst3::editor", tresult = get_ok, "component.getState échoué");
        return;
    }
    let mut dummy: i64 = 0;
    let seek_ok = unsafe {
        stream.seek(0, IStreamSeekMode_::kIBSeekSet as i32, &mut dummy)
    };
    if seek_ok != kResultOk {
        return;
    }
    let set_ok = unsafe { controller.setComponentState(stream.as_ptr()) };
    if set_ok != kResultOk {
        tracing::warn!(
            target: "jamodio::vst3::editor",
            tresult = set_ok,
            "controller.setComponentState échoué (E_NOTIMPL = tolérable)"
        );
        return;
    }
    tracing::info!(target: "jamodio::vst3::editor", "state sync component→controller ok");
}

/// Connecte composant et controller via `IConnectionPoint` **avec des proxies
/// ThreadChecker** (= pattern Steinberg SDK `connectionproxy.cpp`).
///
/// Le proxy drop les notify() venant du thread audio RT (= le seul cas où un
/// notify pourrait surprendre un plugin pendant que vst3-main est occupé) et
/// forward tout le reste. Le handshake JUCE controller→component passe par
/// `host_app.createInstance(IMessage)` + notify à travers le proxy — les deux
/// côtés tournent sur vst3-main, donc le filtre laisse passer.
///
/// Retourne les 2 ComWrappers pour les garder vivants (= dans
/// `EditorState.proxies`) — sinon refcount à 0 = use-after-free dès que le
/// plugin tente un notify.
fn connect_component_to_controller_via_proxies(
    component: &ComPtr<IComponent>,
    controller: &ComPtr<IEditController>,
) -> Option<(ComWrapper<ConnectionProxy>, ComWrapper<ConnectionProxy>)> {
    let comp_cp = component.cast::<IConnectionPoint>()?;
    let ctrl_cp = controller.cast::<IConnectionPoint>()?;

    // Proxy côté component : peer du component sera ce proxy. Quand component
    // notifie son peer, le proxy filtre par thread puis forward vers ctrl_cp.
    let proxy_for_comp = ComWrapper::new(ConnectionProxy::new());
    proxy_for_comp.set_dst(ctrl_cp.clone());
    let proxy_for_comp_ptr = proxy_for_comp.to_com_ptr::<IConnectionPoint>()?;

    // Proxy côté controller : peer du controller sera ce proxy. Quand
    // controller notifie son peer (depuis vst3-main), le proxy forward vers
    // comp_cp.
    let proxy_for_ctrl = ComWrapper::new(ConnectionProxy::new());
    proxy_for_ctrl.set_dst(comp_cp.clone());
    let proxy_for_ctrl_ptr = proxy_for_ctrl.to_com_ptr::<IConnectionPoint>()?;

    let r1 = unsafe { comp_cp.connect(proxy_for_comp_ptr.as_ptr()) };
    let r2 = unsafe { ctrl_cp.connect(proxy_for_ctrl_ptr.as_ptr()) };
    if r1 == kResultOk && r2 == kResultOk {
        tracing::info!(
            target: "jamodio::vst3::editor",
            "IConnectionPoint proxies installed (thread-checked, mirror SDK pattern)"
        );
    } else {
        tracing::warn!(
            target: "jamodio::vst3::editor",
            r1,
            r2,
            "IConnectionPoint connect via proxy partiel"
        );
    }
    Some((proxy_for_comp, proxy_for_ctrl))
}

/// Récupère un `IEditController` pour le composant. Si le composant l'expose
/// directement (plugin "single-component"), on le partage. Sinon on crée une
/// instance séparée via la factory et on l'initialise avec le host context.
fn resolve_controller(
    component: &ComPtr<IComponent>,
    module: &LoadedModule,
    host_app: &ComPtr<IHostApplication>,
) -> Result<ComPtr<IEditController>, String> {
    if let Some(c) = component.cast::<IEditController>() {
        tracing::info!(
            target: "jamodio::vst3::editor",
            "controller = same instance as component (single-component plugin)"
        );
        return Ok(c);
    }

    let mut cid: TUID = [0; 16];
    let ok = unsafe { component.getControllerClassId(&mut cid as *mut TUID) };
    if ok != 0 {
        return Err(format!(
            "plugin n'expose ni IEditController inline ni getControllerClassId (tresult={ok})"
        ));
    }
    let mut raw: *mut c_void = std::ptr::null_mut();
    let cr_ok = unsafe {
        module.factory().createInstance(
            cid.as_ptr() as *const i8,
            IEditController_iid.as_ptr() as *const i8,
            &mut raw,
        )
    };
    if cr_ok != 0 || raw.is_null() {
        return Err(format!("createInstance(IEditController) tresult={cr_ok}"));
    }
    let controller = unsafe { ComPtr::<IEditController>::from_raw(raw as *mut IEditController) }
        .ok_or_else(|| "createInstance retourne null".to_string())?;

    let host_ctx = host_app.as_ptr() as *mut FUnknown;
    let init_ok = unsafe { controller.initialize(host_ctx) };
    if init_ok != 0 {
        return Err(format!(
            "controller.initialize(IHostApplication) tresult={init_ok}"
        ));
    }
    tracing::info!(
        target: "jamodio::vst3::editor",
        "controller = separate class instance, initialized on vst3-main"
    );
    Ok(controller)
}
