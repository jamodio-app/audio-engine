//! Éditeur natif Win32 — fenêtre HWND + msg pump dédié pour héberger
//! l'`IPlugView` du plugin VST3 (analogue d'`AUGenericView` + NSWindow côté Mac).
//!
//! Architecture :
//! - `EditorWindow::open()` spawn un thread "ui-vst3-{title}" qui :
//!   1. crée une `HWND` Win32 via `CreateWindowExW`
//!   2. récupère un `IEditController` (cast IComponent → fallback createInstance)
//!   3. lui attache un `IComponentHandler` minimal (= no-op qui retourne kResultOk
//!      sur beginEdit/performEdit/endEdit/restartComponent). **Indispensable** :
//!      les plugins pro (Valhalla, AmpliTube…) refusent silencieusement de
//!      créer leur view si le handler n'est pas set.
//!   4. appelle `IPlugView::attached(hwnd, "HWND")`
//!   5. lance un msg pump `GetMessageW` → `DispatchMessageW`
//! - `EditorWindow::close()` poste un `WM_CLOSE` sur la HWND → le thread sort.
//!
//! Threading : VST3 demande que `IEditController` + `IPlugView` soient appelés
//! sur un thread unique avec un msg pump (= "UI thread"). On crée un thread
//! dédié par éditeur ouvert plutôt qu'un thread UI global, plus simple à
//! cleanup et isolant les crashs plugin.

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
            IComponentHandler, IComponentHandlerTrait, IComponentTrait, IEditController,
            IEditControllerTrait, IEditController_iid, IHostApplication, ParamID, ParamValue,
        },
        IPlugView, IPlugViewTrait, ViewRect,
    },
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW,
        RegisterClassExW, SetWindowTextW, ShowWindow, TranslateMessage, MSG, SW_SHOW, WM_CLOSE,
        WM_DESTROY, WNDCLASSEXW, WS_EX_DLGMODALFRAME, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    },
};

use crate::host::Instance;
use crate::host_app::MinimalHost;
use crate::loader::LoadedModule;
use crate::state::MemoryStream;

const HOST_NAMESPACE: &str = "JAMOEDITOR";
const PLATFORM_HWND: &[u8] = b"HWND\0";

/// Wrapper UTF-16 nul-terminé pour les APIs Windows.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------- IComponentHandler minimal ----------
//
// Implémenté en Rust via `Class` (= la facilité de coupler-rs pour exposer une
// classe COM Rust côté plugin). Les 4 méthodes retournent `kResultOk` sans
// rien faire : on n'enregistre pas l'automation des params, ce qui est OK
// pour du live (l'user joue avec les knobs en direct, pas besoin de sync).

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

// ---------- Données thread + struct publique ----------

struct EditorThreadData {
    title: String,
    controller: ComPtr<IEditController>,
    /// Handler injecté dans le controller via `setComponentHandler`. Maintenu
    /// en vie pendant toute la durée du thread éditeur — sinon refcount tombe
    /// à 0 et le plugin crashera dès qu'il essaiera de notifier un edit.
    #[allow(dead_code)]
    handler: ComPtr<IComponentHandler>,
    /// Host context passé à `controller.initialize`. Le plugin garde son
    /// pointeur en cache et peut appeler `getName` à tout moment — donc on
    /// le garde vivant aussi longtemps que le controller.
    #[allow(dead_code)]
    host_app: ComPtr<IHostApplication>,
    #[allow(dead_code)]
    component_keepalive: ComPtr<vst3::Steinberg::Vst::IComponent>,
    #[allow(dead_code)]
    module_keepalive: Arc<LoadedModule>,
    hwnd_slot: Arc<AtomicPtr<c_void>>,
}

/// État d'un éditeur ouvert. Détient le handle thread + le pointeur HWND
/// atomique pour permettre au main thread de poster un WM_CLOSE.
pub struct EditorWindow {
    join: Option<JoinHandle<()>>,
    hwnd_slot: Arc<AtomicPtr<c_void>>,
}

impl EditorWindow {
    pub fn open(
        instance: &Instance,
        module: Arc<LoadedModule>,
        title: &str,
    ) -> Result<Self, String> {
        // 1. IHostApplication minimal — passé en context à controller.initialize.
        //    Indispensable pour les plugins commerciaux (Valhalla, FabFilter…)
        //    qui refusent leur UI si le hostContext est null. On garde le
        //    pointeur vivant en stockant le `ComPtr` dans `EditorThreadData`.
        let host_app_wrapper = ComWrapper::new(MinimalHost);
        let host_app: ComPtr<IHostApplication> = host_app_wrapper
            .to_com_ptr::<IHostApplication>()
            .ok_or_else(|| "ComWrapper::to_com_ptr::<IHostApplication> a échoué".to_string())?;

        let controller = resolve_controller(instance, &module, &host_app)?;

        // State sync component → controller : indispensable pour les plugins
        // en architecture "separate component+controller" (Valhalla, FabFilter,
        // NI…). Tolérant à l'échec (certains plugins retournent E_NOTIMPL).
        sync_component_state(instance, &controller);

        // Set le component handler avant tout createView (sinon plugins pros
        // refusent de créer leur UI).
        let handler_wrapper = ComWrapper::new(MinimalHandler);
        let handler: ComPtr<IComponentHandler> = handler_wrapper
            .to_com_ptr::<IComponentHandler>()
            .ok_or_else(|| "ComWrapper::to_com_ptr::<IComponentHandler> a échoué".to_string())?;
        let set_ok = unsafe { controller.setComponentHandler(handler.as_ptr()) };
        if set_ok != kResultOk {
            tracing::warn!(
                target: "jamodio::vst3::editor",
                tresult = set_ok,
                "setComponentHandler refusé — la view risque d'être non fonctionnelle"
            );
        }

        let hwnd_slot: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
        let data = EditorThreadData {
            title: title.to_string(),
            controller,
            handler,
            host_app,
            component_keepalive: instance.component.clone(),
            module_keepalive: module,
            hwnd_slot: hwnd_slot.clone(),
        };

        let join = std::thread::Builder::new()
            .name(format!("ui-vst3-{title}"))
            .spawn(move || editor_thread_main(data))
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

/// Synchronise l'état du `IComponent` vers le `IEditController` via un
/// `IBStream` mémoire éphémère.
///
/// Pattern VST3 standard pour activer la view d'un plugin "separate
/// component+controller" :
/// 1. `component.getState(stream)` — le composant audio écrit ses params
/// 2. `stream.seek(0, kIBSeekSet)` — rewind avant lecture
/// 3. `controller.setComponentState(stream)` — le controller charge
///
/// Tolérant à l'échec : si une étape rate, on log warn et on continue. Pour
/// les plugins simples ça suffit ; les plus pointilleux refuseront createView
/// derrière (= prochaine étape de diag).
fn sync_component_state(instance: &Instance, controller: &ComPtr<IEditController>) {
    let stream_wrapper = ComWrapper::new(MemoryStream::new());
    let stream: ComPtr<IBStream> = match stream_wrapper.to_com_ptr::<IBStream>() {
        Some(s) => s,
        None => {
            tracing::warn!(
                target: "jamodio::vst3::editor",
                "MemoryStream::to_com_ptr<IBStream> a échoué — state sync skipped"
            );
            return;
        }
    };

    let get_ok = unsafe { instance.component.getState(stream.as_ptr()) };
    if get_ok != kResultOk {
        tracing::warn!(
            target: "jamodio::vst3::editor",
            tresult = get_ok,
            "component.getState a échoué — state sync skipped"
        );
        return;
    }

    let mut dummy: i64 = 0;
    let seek_ok = unsafe {
        stream.seek(0, IStreamSeekMode_::kIBSeekSet as i32, &mut dummy)
    };
    if seek_ok != kResultOk {
        tracing::warn!(
            target: "jamodio::vst3::editor",
            tresult = seek_ok,
            "stream.seek(0) a échoué — state sync skipped"
        );
        return;
    }

    let set_ok = unsafe { controller.setComponentState(stream.as_ptr()) };
    if set_ok != kResultOk {
        tracing::warn!(
            target: "jamodio::vst3::editor",
            tresult = set_ok,
            "controller.setComponentState a échoué (le plugin tolère peut-être, on continue)"
        );
        return;
    }
    tracing::info!(target: "jamodio::vst3::editor", "state sync component→controller ok");
}

/// Récupère un `IEditController` pour l'instance.
///
/// Pattern VST3 :
/// 1. Tentative cast : beaucoup de plugins "single component" exposent
///    `IComponent` + `IEditController` sur la même instance COM.
/// 2. Sinon : `getControllerClassId()` → `factory.createInstance(cid, IEditController_iid)`.
fn resolve_controller(
    instance: &Instance,
    module: &LoadedModule,
    host_app: &ComPtr<IHostApplication>,
) -> Result<ComPtr<IEditController>, String> {
    if let Some(c) = instance.component.cast::<IEditController>() {
        tracing::info!(
            target: "jamodio::vst3::editor",
            "controller = same instance as component (single-component plugin)"
        );
        return Ok(c);
    }

    let mut cid: TUID = [0; 16];
    let ok = unsafe { instance.component.getControllerClassId(&mut cid as *mut TUID) };
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

    // hostContext = ptr sur notre MinimalHost (cast IHostApplication → FUnknown
    // via le layout vtable : IHostApplication inherits FUnknown).
    let host_ctx = host_app.as_ptr() as *mut FUnknown;
    let init_ok = unsafe { controller.initialize(host_ctx) };
    if init_ok != 0 {
        return Err(format!(
            "controller.initialize(IHostApplication) tresult={init_ok}"
        ));
    }
    tracing::info!(
        target: "jamodio::vst3::editor",
        "controller = separate class instance, initialized with IHostApplication"
    );
    Ok(controller)
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

fn editor_thread_main(data: EditorThreadData) {
    tracing::info!(
        target: "jamodio::vst3::editor",
        title = %data.title,
        "editor thread starting"
    );
    ensure_window_class();
    let title_w = wide(&data.title);
    let class_w = wide(HOST_NAMESPACE);

    // 1. createView("editor") — l'IPlugView est ce qu'on attache à la HWND.
    let view_ptr = unsafe {
        let cstr = b"editor\0";
        data.controller.createView(cstr.as_ptr() as *const i8)
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
            tracing::error!(
                target: "jamodio::vst3::editor",
                "ComPtr::from_raw(view) a refusé un pointeur non-null (improbable)"
            );
            return;
        }
    };
    tracing::info!(target: "jamodio::vst3::editor", "createView ok");

    // 2. Vérifier que le platform "HWND" est supporté.
    let plat_ok = unsafe { view.isPlatformTypeSupported(PLATFORM_HWND.as_ptr() as *const i8) };
    if plat_ok != kResultOk {
        tracing::error!(
            target: "jamodio::vst3::editor",
            tresult = plat_ok,
            "plugin ne supporte pas la plateforme HWND"
        );
        return;
    }
    tracing::info!(target: "jamodio::vst3::editor", "platform HWND supporté");

    // 3. Récupérer la taille préférée de la view (avant attach).
    let mut size = ViewRect {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let _ = unsafe { view.getSize(&mut size) };
    let width = (size.right - size.left).max(100);
    let height = (size.bottom - size.top).max(100);
    tracing::info!(
        target: "jamodio::vst3::editor",
        width,
        height,
        "size demandée par le plugin"
    );

    // 4. Créer la HWND parent.
    let hinst = unsafe {
        windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null())
    };
    let hwnd: HWND = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class_w.as_ptr(),
            title_w.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            -2_147_483_648, // CW_USEDEFAULT
            -2_147_483_648,
            width + 16,
            height + 40,
            std::ptr::null_mut(), // parent
            std::ptr::null_mut(), // menu
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
    data.hwnd_slot.store(hwnd, Ordering::SeqCst);
    tracing::info!(target: "jamodio::vst3::editor", "HWND créée et affichée");

    // 5. IPlugView::attached(hwnd, "HWND")
    let att_ok = unsafe { view.attached(hwnd as *mut c_void, PLATFORM_HWND.as_ptr() as *const i8) };
    if att_ok != kResultOk {
        tracing::error!(
            target: "jamodio::vst3::editor",
            tresult = att_ok,
            "IPlugView::attached failed"
        );
    } else {
        tracing::info!(target: "jamodio::vst3::editor", "IPlugView::attached ok");
        let _ = unsafe {
            view.onSize(&ViewRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            } as *const _ as *mut ViewRect)
        };
    }

    // 6. Msg pump jusqu'à WM_DESTROY.
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

    // 7. Cleanup côté plugin.
    let _ = unsafe { view.removed() };
    data.hwnd_slot.store(std::ptr::null_mut(), Ordering::SeqCst);
    tracing::info!(target: "jamodio::vst3::editor", "editor thread exiting");
}
