//! Éditeur natif Win32 — fenêtre HWND + msg pump dédié pour héberger
//! l'`IPlugView` du plugin VST3 (analogue d'`AUGenericView` + NSWindow côté Mac).
//!
//! Architecture :
//! - `EditorWindow::open()` spawn un thread "ui-vst3-{title}" qui :
//!   1. crée une `HWND` Win32 via `CreateWindowExW`
//!   2. récupère un `IEditController` (cast IComponent → fallback createInstance)
//!   3. appelle `IPlugView::attached(hwnd, "HWND")`
//!   4. lance un msg pump `GetMessageW` → `DispatchMessageW`
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
    ComPtr,
    Steinberg::{
        IPluginBaseTrait, IPluginFactoryTrait, TUID,
        Vst::{IComponentTrait, IEditController, IEditControllerTrait, IEditController_iid},
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
use crate::loader::LoadedModule;

const HOST_NAMESPACE: &str = "JAMOEDITOR";
const PLATFORM_HWND: &[u8] = b"HWND\0";

/// Wrapper UTF-16 nul-terminé pour les APIs Windows.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Données passées au thread éditeur.
///
/// `hwnd_slot` est un `AtomicPtr<c_void>` (= équivalent `AtomicPtr<HWND>`)
/// pour rester `Send` sans Mutex. `HWND = *mut c_void` dans windows-sys 0.59,
/// les pointeurs raw ne sont pas Send par défaut.
struct EditorThreadData {
    title: String,
    controller: ComPtr<IEditController>,
    /// Tenu vivant pendant que la window existe — ComPtr.Clone() pour ne pas
    /// invalider l'`IComponent` du Vst3Host quand le thread sort.
    #[allow(dead_code)]
    component_keepalive: ComPtr<vst3::Steinberg::Vst::IComponent>,
    /// Idem pour le module — la DLL doit rester chargée tant que la window
    /// référence ses callbacks.
    #[allow(dead_code)]
    module_keepalive: Arc<LoadedModule>,
    /// Shared HWND raw pointer : main thread peut PostMessage(WM_CLOSE).
    /// Null = window pas encore créée OU déjà fermée.
    hwnd_slot: Arc<AtomicPtr<c_void>>,
}

/// État d'un éditeur ouvert. Détient le handle thread + le pointeur HWND
/// atomique pour permettre au main thread de poster un WM_CLOSE.
pub struct EditorWindow {
    join: Option<JoinHandle<()>>,
    hwnd_slot: Arc<AtomicPtr<c_void>>,
}

impl EditorWindow {
    /// Ouvre l'éditeur du plugin. Retourne une erreur si le plugin n'expose
    /// pas d'`IEditController` accessible.
    pub fn open(
        instance: &Instance,
        module: Arc<LoadedModule>,
        title: &str,
    ) -> Result<Self, String> {
        let controller = resolve_controller(instance, &module)?;

        let hwnd_slot: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
        let data = EditorThreadData {
            title: title.to_string(),
            controller,
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

    /// Ferme la fenêtre (poste WM_CLOSE) et attend la fin du thread.
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

/// Récupère un `IEditController` pour l'instance.
///
/// Pattern VST3 :
/// 1. Tentative cast : beaucoup de plugins "single component" exposent
///    `IComponent` + `IEditController` sur la même instance COM.
/// 2. Sinon : `getControllerClassId()` → `factory.createInstance(cid, IEditController_iid)`.
///
/// Pas d'`IConnectionPoint` pour le MVP — fonctionne tant que le plugin n'a
/// pas besoin de sync state component↔controller (= la majorité des effets).
fn resolve_controller(
    instance: &Instance,
    module: &LoadedModule,
) -> Result<ComPtr<IEditController>, String> {
    if let Some(c) = instance.component.cast::<IEditController>() {
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
    let init_ok = unsafe { controller.initialize(std::ptr::null_mut()) };
    if init_ok != 0 {
        return Err(format!("controller.initialize tresult={init_ok}"));
    }
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
            // Idempotent : si déjà registered (relance après drop), pas d'erreur fatale.
            let _ = RegisterClassExW(&wc);
        }
    });
}

fn editor_thread_main(data: EditorThreadData) {
    ensure_window_class();
    let title_w = wide(&data.title);
    let class_w = wide(HOST_NAMESPACE);

    // 1. createView("editor") — l'IPlugView est ce qu'on attache à la HWND.
    let view_ptr = unsafe {
        let cstr = b"editor\0";
        data.controller.createView(cstr.as_ptr() as *const i8)
    };
    let Some(view) = (unsafe { ComPtr::<IPlugView>::from_raw(view_ptr) }) else {
        tracing::warn!(target: "jamodio::vst3::editor", "createView('editor') retourne null");
        return;
    };

    // 2. Vérifier que le platform "HWND" est supporté.
    let plat_ok = unsafe { view.isPlatformTypeSupported(PLATFORM_HWND.as_ptr() as *const i8) };
    if plat_ok != 0 {
        tracing::warn!(
            target: "jamodio::vst3::editor",
            tresult = plat_ok,
            "plugin ne supporte pas la plateforme HWND"
        );
        return;
    }

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
            // x, y = use default placement
            -2_147_483_648, // CW_USEDEFAULT
            -2_147_483_648,
            width + 16, // add some chrome budget
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

    // 5. IPlugView::attached(hwnd, "HWND")
    let att_ok = unsafe { view.attached(hwnd as *mut c_void, PLATFORM_HWND.as_ptr() as *const i8) };
    if att_ok != 0 {
        tracing::warn!(
            target: "jamodio::vst3::editor",
            tresult = att_ok,
            "IPlugView::attached failed"
        );
    } else {
        // Ajuste la HWND à la taille demandée par le plugin.
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
}
