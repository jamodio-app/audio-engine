//! Éditeur natif Win32 — fenêtre HWND + msg pump dédié pour héberger
//! l'`IPlugView` du plugin VST3.
//!
//! # Architecture COM consolidée (v2)
//!
//! TOUS les appels COM liés à l'éditeur s'exécutent sur le **thread STA dédié**
//! `ui-vst3-{title}`. C'est essentiel parce que beaucoup de plugins marquent
//! leurs objets COM thread-affined à leur thread d'origine (= STA). Si le host
//! crée le controller sur le thread WS tokio (qui n'est dans aucun apartment
//! COM) et appelle `attached` depuis le thread STA, le plugin essaie de
//! notifier ses callbacks à travers les threads, ce qui marshalize vers le
//! thread d'origine — sans msg pump là-bas, deadlock garanti.
//!
//! En consolidant tout (createInstance du controller, setComponentHandler,
//! IConnectionPoint::connect, createView, setFrame, attached) sur un unique
//! thread STA, on garantit que toutes les références sont thread-affined au
//! même apartment → pas de marshaling → pas de deadlock.
//!
//! # Séquence du thread éditeur
//! 1. `CoInitializeEx(STA)`
//! 2. Crée `IHostApplication` minimal
//! 3. `resolve_controller` (cast IComponent → IEditController, ou createInstance
//!    de la classe controller séparée + initialize avec hostContext)
//! 4. `IConnectionPoint::connect` bidirectionnel component↔controller
//! 5. `IComponent::getState` → `IEditController::setComponentState` (toléré E_NOTIMPL)
//! 6. Crée `IComponentHandler` minimal, `controller.setComponentHandler()`
//! 7. `controller.createView("editor")`
//! 8. `view.isPlatformTypeSupported("HWND")`, `view.getSize()`
//! 9. Crée la HWND parent Win32 (cachée, WS_OVERLAPPEDWINDOW)
//! 10. Crée `IPlugFrame` minimal, `view.setFrame()`
//! 11. `pump_pending_messages()` (drain WM_CREATE etc.)
//! 12. `view.attached(hwnd, "HWND")` ← le moment de vérité
//! 13. `view.onSize()`
//! 14. `ShowWindow(SW_SHOW)`
//! 15. Msg pump `GetMessageW` jusqu'à WM_DESTROY
//! 16. `view.removed()`, drop des ComPtrs locaux, `CoUninitialize`

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

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
    System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PeekMessageW,
        PostMessageW, RegisterClassExW, SetWindowTextW, ShowWindow, TranslateMessage, MSG,
        PM_REMOVE, SW_SHOW, WM_CLOSE, WM_DESTROY, WNDCLASSEXW, WS_EX_DLGMODALFRAME,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    },
};

use crate::host::Instance;
use crate::host_app::MinimalHost;
use crate::loader::LoadedModule;
use crate::state::MemoryStream;

const HOST_NAMESPACE: &str = "JAMOEDITOR";
const PLATFORM_HWND: &[u8] = b"HWND\0";

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

// ---------- EditorWindow ----------

/// Handle public exposé au `Vst3Host`. Détient le thread + un slot atomique
/// vers la HWND (pour PostMessage WM_CLOSE depuis l'extérieur).
pub struct EditorWindow {
    join: Option<JoinHandle<()>>,
    hwnd_slot: Arc<AtomicPtr<c_void>>,
}

impl EditorWindow {
    /// Spawn le thread éditeur — retourne immédiatement, le thread fait tout
    /// le setup COM en interne. Les erreurs du setup sont loggées dans le
    /// thread (pas remontées au caller) parce que le caller (WS handler) ne
    /// peut rien en faire de toute façon.
    pub fn open(
        instance: &Instance,
        module: Arc<LoadedModule>,
        title: &str,
    ) -> Result<Self, String> {
        let hwnd_slot: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
        let hwnd_slot_thread = hwnd_slot.clone();
        let component = instance.component.clone();
        let title = title.to_string();

        let join = std::thread::Builder::new()
            .name(format!("ui-vst3-{title}"))
            .spawn(move || {
                editor_thread_main(component, module, title, hwnd_slot_thread);
            })
            .map_err(|e| format!("spawn editor thread: {e}"))?;

        Ok(Self {
            join: Some(join),
            hwnd_slot,
        })
    }

    pub fn close(&mut self) {
        let hwnd = self.hwnd_slot.load(Ordering::SeqCst);
        if !hwnd.is_null() {
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------- Thread main ----------

/// Pump tous les messages Win32 en attente dans la queue du thread courant.
fn pump_pending_messages() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
            lpfnWndProc: Some(wnd_proc),
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

fn editor_thread_main(
    component: ComPtr<IComponent>,
    module: Arc<LoadedModule>,
    title: String,
    hwnd_slot: Arc<AtomicPtr<c_void>>,
) {
    tracing::info!(
        target: "jamodio::vst3::editor",
        title = %title,
        "editor thread starting"
    );

    // 1. COM apartment STA OBLIGATOIRE pour le thread UI VST3.
    let com_init =
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    tracing::info!(
        target: "jamodio::vst3::editor",
        hresult = format!("0x{com_init:08X}"),
        "CoInitializeEx STA"
    );

    // RAII : CoUninitialize sera appelé au scope exit même si on early-return.
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _com_guard = ComGuard;

    // 2. IHostApplication minimal — context du controller.
    let host_app_wrapper = ComWrapper::new(MinimalHost);
    let host_app: ComPtr<IHostApplication> = match host_app_wrapper.to_com_ptr::<IHostApplication>() {
        Some(h) => h,
        None => {
            tracing::error!(target: "jamodio::vst3::editor", "ComWrapper::to_com_ptr<IHostApplication> a échoué");
            return;
        }
    };

    // 3. Resolve le controller (sur CE thread = STA, donc le plugin va le
    //    thread-affiner ici).
    let controller = match resolve_controller(&component, &module, &host_app) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(target: "jamodio::vst3::editor", error = %e, "resolve_controller failed");
            return;
        }
    };

    // 4. IConnectionPoint bidirectionnel.
    connect_component_to_controller(&component, &controller);

    // 5. State sync via stream (toléré E_NOTIMPL).
    sync_component_state(&component, &controller);

    // 6. IComponentHandler.
    let handler_wrapper = ComWrapper::new(MinimalHandler);
    let handler: ComPtr<IComponentHandler> = match handler_wrapper.to_com_ptr::<IComponentHandler>() {
        Some(h) => h,
        None => {
            tracing::error!(target: "jamodio::vst3::editor", "ComWrapper::to_com_ptr<IComponentHandler> a échoué");
            return;
        }
    };
    let set_ok = unsafe { controller.setComponentHandler(handler.as_ptr()) };
    tracing::info!(
        target: "jamodio::vst3::editor",
        tresult = set_ok,
        "setComponentHandler"
    );

    // 7. createView (sur ce thread = STA).
    let view_ptr = unsafe {
        let cstr = b"editor\0";
        controller.createView(cstr.as_ptr() as *const i8)
    };
    if view_ptr.is_null() {
        tracing::error!(
            target: "jamodio::vst3::editor",
            "createView('editor') retourne null — le plugin refuse de fournir une UI"
        );
        return;
    }
    let view = match unsafe { ComPtr::<IPlugView>::from_raw(view_ptr) } {
        Some(v) => v,
        None => {
            tracing::error!(target: "jamodio::vst3::editor", "ComPtr::from_raw(view) NULL");
            return;
        }
    };
    tracing::info!(target: "jamodio::vst3::editor", "createView ok");

    // 8. Platform check + size.
    let plat_ok = unsafe { view.isPlatformTypeSupported(PLATFORM_HWND.as_ptr() as *const i8) };
    if plat_ok != kResultOk {
        tracing::error!(
            target: "jamodio::vst3::editor",
            tresult = plat_ok,
            "plugin ne supporte pas la plateforme HWND"
        );
        return;
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

    // 9. HWND parent — visible directement avec WS_VISIBLE. Tests précédents :
    //    cacher la window pour attached() ne change rien au hang, on revient
    //    à WS_VISIBLE pour voir la window immédiatement et confirmer qu'elle
    //    apparaît avant attached().
    ensure_window_class();
    let title_w = wide(&title);
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
        tracing::error!(target: "jamodio::vst3::editor", "CreateWindowExW returned null");
        return;
    }
    unsafe {
        SetWindowTextW(hwnd, title_w.as_ptr());
        ShowWindow(hwnd, SW_SHOW);
    }
    hwnd_slot.store(hwnd, Ordering::SeqCst);
    tracing::info!(target: "jamodio::vst3::editor", "HWND créée et affichée");

    // 10. IPlugFrame.
    let frame_wrapper = ComWrapper::new(PlugFrame);
    let frame: ComPtr<IPlugFrame> = match frame_wrapper.to_com_ptr::<IPlugFrame>() {
        Some(f) => f,
        None => {
            tracing::error!(target: "jamodio::vst3::editor", "ComWrapper::to_com_ptr<IPlugFrame> a échoué");
            return;
        }
    };
    let set_frame_ok = unsafe { view.setFrame(frame.as_ptr()) };
    tracing::info!(target: "jamodio::vst3::editor", tresult = set_frame_ok, "setFrame");

    // 11. Drain les messages WM_CREATE initiaux.
    pump_pending_messages();

    // 12. attached — le moment de vérité.
    tracing::info!(target: "jamodio::vst3::editor", "calling attached…");
    let att_ok = unsafe { view.attached(hwnd as *mut c_void, PLATFORM_HWND.as_ptr() as *const i8) };
    if att_ok != kResultOk {
        tracing::error!(
            target: "jamodio::vst3::editor",
            tresult = att_ok,
            "IPlugView::attached failed"
        );
        return;
    }
    tracing::info!(target: "jamodio::vst3::editor", "IPlugView::attached ok");

    // 13. onSize après attach.
    let _ = unsafe {
        view.onSize(&ViewRect {
            left: 0, top: 0, right: width, bottom: height,
        } as *const _ as *mut ViewRect)
    };

    // 14. Msg pump.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if msg.message == WM_DESTROY {
            break;
        }
    }

    // 15. Cleanup côté plugin.
    let _ = unsafe { view.removed() };
    hwnd_slot.store(std::ptr::null_mut(), Ordering::SeqCst);

    // Keepalives droppent en sortie de scope (frame → view → handler → controller
    // → host_app → component → module). CoUninitialize via ComGuard::drop.
    let _ = (frame, view, handler, controller, host_app, component, module);
    tracing::info!(target: "jamodio::vst3::editor", "editor thread exiting");
}

// ---------- Helpers (tournent sur le thread éditeur) ----------

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

/// Connecte composant et controller via `IConnectionPoint` bidirectionnel.
fn connect_component_to_controller(component: &ComPtr<IComponent>, controller: &ComPtr<IEditController>) {
    let comp_cp = match component.cast::<IConnectionPoint>() {
        Some(c) => c,
        None => return,
    };
    let ctrl_cp = match controller.cast::<IConnectionPoint>() {
        Some(c) => c,
        None => return,
    };
    let r1 = unsafe { comp_cp.connect(ctrl_cp.as_ptr()) };
    let r2 = unsafe { ctrl_cp.connect(comp_cp.as_ptr()) };
    if r1 == kResultOk && r2 == kResultOk {
        tracing::info!(target: "jamodio::vst3::editor", "IConnectionPoint connect component↔controller ok");
    } else {
        tracing::warn!(target: "jamodio::vst3::editor", r1, r2, "IConnectionPoint connect partiel");
    }
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
        "controller = separate class instance, initialized on STA thread"
    );
    Ok(controller)
}
