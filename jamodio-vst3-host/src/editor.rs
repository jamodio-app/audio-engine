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
        int32, kInvalidArgument, kResultOk, kResultTrue, tresult, FUnknown, IBStream, IBStreamTrait,
        IBStream_::IStreamSeekMode_, IPluginBaseTrait, IPluginFactoryTrait, TUID,
        Vst::{
            IComponent, IComponentHandler, IComponentHandlerTrait, IComponentTrait,
            IConnectionPoint, IConnectionPointTrait, IEditController, IEditControllerTrait,
            IEditController_iid, IHostApplication, ParamID, ParamValue,
        },
        IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewContentScaleSupport,
        IPlugViewContentScaleSupportTrait, IPlugViewTrait, ViewRect,
    },
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    UI::HiDpi::GetDpiForSystem,
    UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW,
        IsIconic, RegisterClassExW, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow,
        GWL_EXSTYLE, GWL_STYLE, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SW_RESTORE, SW_SHOW, WMSZ_BOTTOMLEFT, WMSZ_LEFT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
        WM_DESTROY, WM_SIZE, WM_SIZING, WNDCLASSEXW, WS_CAPTION, WS_EX_DLGMODALFRAME, WS_MINIMIZEBOX,
        WS_OVERLAPPEDWINDOW, WS_SYSMENU, WS_VISIBLE,
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
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        // kLatencyChanged (review 11/06) : un plugin qui change sa latence
        // en session (toggle oversampling/lookahead) fausse la compensation
        // calculée au load. La propagation complète (re-read getLatencySamples
        // + notification wire au browser) = backlog post-beta ; en attendant,
        // on rend l'événement VISIBLE dans les logs au lieu de l'avaler.
        if flags & vst3::Steinberg::Vst::RestartFlags_::kLatencyChanged != 0 {
            tracing::warn!(
                target: "jamodio::vst3",
                flags,
                "plugin a signalé kLatencyChanged — latence de compensation figée au load (re-sync = backlog)"
            );
        }
        kResultOk
    }
}

// ---------- IPlugFrame ----------

/// `IPlugFrame` est le callback par lequel un plugin demande à l'hôte de
/// redimensionner SA fenêtre (UI scalable, bascule de skin, bouton « agrandir »…).
/// Le no-op précédent ignorait ces demandes → la fenêtre ne suivait jamais le
/// plugin. On garde le HWND parent (set après `CreateWindowExW`) pour pouvoir
/// le redimensionner. `AtomicPtr` car COM peut théoriquement appeler depuis
/// n'importe quel thread — en pratique toujours vst3-main.
struct PlugFrame {
    hwnd: AtomicPtr<c_void>,
}

impl PlugFrame {
    fn new() -> Self {
        Self {
            hwnd: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    fn set_hwnd(&self, hwnd: HWND) {
        self.hwnd.store(hwnd, Ordering::SeqCst);
    }
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    /// Le plugin réclame une nouvelle taille de vue. On redimensionne la HWND
    /// parente pour que sa zone CLIENT vaille exactement `new_size` (calcul
    /// exact via `AdjustWindowRectEx`, pas de marge magique). `SetWindowPos`
    /// émet `WM_SIZE` de façon synchrone → `editor_wnd_proc` rappelle
    /// `view.onSize()` : c'est la source unique qui confirme la taille au
    /// plugin, donc pas de double `onSize` ici.
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        let hwnd: HWND = self.hwnd.load(Ordering::SeqCst);
        if hwnd.is_null() || new_size.is_null() {
            return kInvalidArgument;
        }
        let rect = &*new_size;
        let client_w = rect.right - rect.left;
        let client_h = rect.bottom - rect.top;
        if client_w <= 0 || client_h <= 0 {
            return kInvalidArgument;
        }
        let (outer_w, outer_h) = outer_size_for_client(hwnd, client_w, client_h);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            outer_w,
            outer_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        kResultOk
    }
}

/// Taille EXTÉRIEURE (fenêtre) nécessaire pour obtenir une zone client de
/// `client_w × client_h`, d'après le style RÉEL de la fenêtre (bordure + barre
/// de titre, DPI/thème inclus). Remplace les marges codées en dur `+16/+40`.
fn outer_size_for_client(hwnd: HWND, client_w: i32, client_h: i32) -> (i32, i32) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        outer_size_for_style(client_w, client_h, style, exstyle)
    }
}

/// Variante utilisable AVANT que la fenêtre existe : le style est fourni
/// explicitement (au moment du `CreateWindowExW`).
fn outer_size_for_style(client_w: i32, client_h: i32, style: u32, exstyle: u32) -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: client_w,
        bottom: client_h,
    };
    // bmenu = 0 : pas de menu. Modifie `r` pour englober le non-client.
    unsafe { AdjustWindowRectEx(&mut r, style, 0, exstyle) };
    (r.right - r.left, r.bottom - r.top)
}

// ---------- Registry des fenêtres ouvertes (thread-local vst3-main) ----------

/// Wiring `IConnectionPoint` actif entre component et controller. Conservé
/// pour pouvoir `disconnect()` proprement à la fermeture — indispensable :
/// le component JUCE garde le pointeur du controller tant qu'on ne le
/// déconnecte pas (`disconnect` → `juceVST3EditController = {}`), et IGNORE
/// tout handshake d'un nouveau controller (`if juceVST3EditController ==
/// nullptr` dans son notify). Sans disconnect, la 2e ouverture donnait un
/// `createView` null.
struct ConnState {
    comp_cp: ComPtr<IConnectionPoint>,
    ctrl_cp: ComPtr<IConnectionPoint>,
    proxy_for_comp: ComWrapper<ConnectionProxy>,
    proxy_for_ctrl: ComWrapper<ConnectionProxy>,
}

/// Keep-alives d'une fenêtre d'éditeur ouverte. Teardown dans le wnd_proc
/// sur WM_DESTROY (cf. `editor_wnd_proc`) : `view.removed()` → release
/// view/frame/handler → disconnect → terminate du controller (si séparé).
/// Le plugin cache les pointeurs des proxies / du handler → les release
/// trop tôt = use-after-free.
struct EditorState {
    frame: ComPtr<IPlugFrame>,
    view: ComPtr<IPlugView>,
    handler: ComPtr<IComponentHandler>,
    conn: Option<ConnState>,
    controller: ComPtr<IEditController>,
    /// `true` si le controller est une instance séparée (createInstance +
    /// initialize par nous) → on doit le terminate() à la fermeture. `false`
    /// = même objet que le component (single-component plugin) → le
    /// terminate appartient au cycle de vie de l'Instance, pas de l'éditeur.
    controller_separate: bool,
    host_app: ComPtr<IHostApplication>,
    component: ComPtr<IComponent>,
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

    /// Ramène la fenêtre éditeur EXISTANTE au premier plan. Appelé quand
    /// l'utilisateur re-clique sur le nom du plugin alors que la fenêtre est
    /// déjà ouverte mais cachée/minimisée (bug PC : `open_editor` retournait
    /// Ok sans rien montrer → il fallait passer par l'icône barre des tâches).
    /// Restaure si minimisée puis applique le même bring-to-front fiable que
    /// l'ouverture (toggle TOPMOST + SetForegroundWindow). No-op si HWND pas
    /// encore créée (setup en cours) ou déjà détruite. Posté sur vst3-main.
    pub fn focus(&self) {
        let shared = self.shared.clone();
        main_thread::post(move || {
            let hwnd = shared.hwnd.load(Ordering::SeqCst);
            if hwnd.is_null() {
                return;
            }
            let hwnd = hwnd as HWND;
            unsafe {
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                } else {
                    ShowWindow(hwnd, SW_SHOW);
                }
                SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                let _ = SetForegroundWindow(hwnd);
            }
            tracing::debug!(target: "jamodio::vst3::editor", "editor re-focus (bring to front)");
        });
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

/// Clone le `IPlugView` associé à une HWND SANS garder le borrow de la
/// registry pendant l'appel COM qui suit (le plugin pourrait ré-entrer via
/// `resizeView` → `borrow_mut` → panic). Le clone = un simple AddRef.
fn view_for_hwnd(hwnd: HWND) -> Option<ComPtr<IPlugView>> {
    OPEN_EDITORS.with(|m| m.borrow().get(&(hwnd as isize)).map(|s| s.view.clone()))
}

unsafe extern "system" fn editor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // L'utilisateur (ou le plugin via resizeView/SetWindowPos) a redimensionné
    // la fenêtre → propage la nouvelle zone client au plugin. Sans ça, la vue
    // gardait sa taille d'origine et se faisait rogner (bug fenêtre éditeur).
    // À la création, WM_SIZE arrive AVANT l'insertion en registry → ignoré
    // (la taille initiale est posée explicitement après `attached`).
    if msg == WM_SIZE {
        let client_w = (lparam & 0xFFFF) as i32;
        let client_h = ((lparam >> 16) & 0xFFFF) as i32;
        // Minimisation (client 0×0) → ne pas infliger une taille dégénérée au
        // plugin ; sa taille sera re-posée au restore (nouveau WM_SIZE).
        if client_w > 0 && client_h > 0 {
            if let Some(view) = view_for_hwnd(hwnd) {
                let mut rect = ViewRect {
                    left: 0,
                    top: 0,
                    right: client_w,
                    bottom: client_h,
                };
                let _ = view.onSize(&mut rect);
            }
        }
        return 0;
    }

    // Pendant le drag d'un bord (plugins redimensionnables uniquement : les
    // plugins fixes n'ont pas WS_THICKFRAME, ce message n'arrive jamais). On
    // demande au plugin de contraindre la taille (min/max/ratio) via
    // `checkSizeConstraint`, puis on réécrit le rectangle proposé en gardant
    // ancré le bord qui n'est PAS tiré.
    if msg == WM_SIZING {
        if let Some(view) = view_for_hwnd(hwnd) {
            let proposed = &mut *(lparam as *mut RECT);
            // Marge non-client (bord + titre) = taille fenêtre pour un client nul.
            let (nc_w, nc_h) = outer_size_for_client(hwnd, 0, 0);
            let client_w = (proposed.right - proposed.left - nc_w).max(1);
            let client_h = (proposed.bottom - proposed.top - nc_h).max(1);
            let mut want = ViewRect {
                left: 0,
                top: 0,
                right: client_w,
                bottom: client_h,
            };
            // `want` contient la taille acceptée par le plugin (inchangée s'il
            // accepte telle quelle, corrigée sinon) — on l'utilise dans tous les cas.
            let _ = view.checkSizeConstraint(&mut want);
            let new_w = (want.right - want.left) + nc_w;
            let new_h = (want.bottom - want.top) + nc_h;
            let edge = wparam as u32;
            if edge == WMSZ_LEFT || edge == WMSZ_TOPLEFT || edge == WMSZ_BOTTOMLEFT {
                proposed.left = proposed.right - new_w;
            } else {
                proposed.right = proposed.left + new_w;
            }
            if edge == WMSZ_TOP || edge == WMSZ_TOPLEFT || edge == WMSZ_TOPRIGHT {
                proposed.top = proposed.bottom - new_h;
            } else {
                proposed.bottom = proposed.top + new_h;
            }
        }
        return 1; // TRUE — rectangle traité.
    }

    if msg == WM_DESTROY {
        // Teardown complet, dans l'ordre du plugprovider SDK :
        // 1. view.removed() pendant que la HWND existe encore
        // 2. release view/frame/handler
        // 3. disconnect component↔controller (sinon le component garde le
        //    pointeur de CE controller et ignorera le handshake du suivant
        //    → createView null à la réouverture)
        // 4. terminate + release du controller (si instance séparée)
        let state = OPEN_EDITORS.with(|m| m.borrow_mut().remove(&(hwnd as isize)));
        if let Some(st) = state {
            let _ = st.view.removed();
            let EditorState {
                frame,
                view,
                handler,
                conn,
                controller,
                controller_separate,
                host_app,
                component,
                module,
                shared,
            } = st;
            drop(frame);
            drop(view);
            drop(handler);
            if let Some(c) = conn {
                let p_comp = c.proxy_for_comp.to_com_ptr::<IConnectionPoint>();
                let p_ctrl = c.proxy_for_ctrl.to_com_ptr::<IConnectionPoint>();
                if let (Some(p1), Some(p2)) = (p_comp, p_ctrl) {
                    let _ = c.comp_cp.disconnect(p1.as_ptr());
                    let _ = c.ctrl_cp.disconnect(p2.as_ptr());
                }
                drop(c);
            }
            if controller_separate {
                let _ = controller.terminate();
            }
            drop(controller);
            drop((host_app, component, module));
            shared.hwnd.store(std::ptr::null_mut(), Ordering::SeqCst);
            shared.state.store(STATE_CLOSED, Ordering::SeqCst);
            tracing::info!(
                target: "jamodio::vst3::editor",
                "editor window destroyed — view removed, connections disconnected, controller released"
            );
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
    let (controller, controller_separate) = resolve_controller(&component, &module, &host_app)?;

    // 3. IConnectionPoint bidirectionnel via proxies thread-checked (pattern
    //    SDK Steinberg, cf. conn_proxy.rs). Les notify du thread audio RT
    //    sont droppés, tout le reste est forwardé. Le handshake JUCE
    //    controller→component (sendIntMessage) passe ici, pendant connect().
    let conn = connect_component_to_controller_via_proxies(&component, &controller);

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
        teardown_partial(&conn, &controller, controller_separate);
        return Err("createView('editor') retourne null — le plugin refuse de fournir une UI".into());
    }
    let view = match unsafe { ComPtr::<IPlugView>::from_raw(view_ptr) } {
        Some(v) => v,
        None => {
            teardown_partial(&conn, &controller, controller_separate);
            return Err("ComPtr::from_raw(view) NULL".into());
        }
    };
    tracing::info!(target: "jamodio::vst3::editor", "createView ok");

    // 7. Platform check + size.
    let plat_ok = unsafe { view.isPlatformTypeSupported(PLATFORM_HWND.as_ptr() as *const i8) };
    if plat_ok != kResultOk {
        teardown_partial(&conn, &controller, controller_separate);
        return Err(format!(
            "plugin ne supporte pas la plateforme HWND (tresult={plat_ok})"
        ));
    }
    // 7bis. Facteur d'échelle DPI (fix 0.5.8 — GUI VST3 rognée sur écran > 100 %).
    // Sur un écran à 150 % (ex. 2560×1440 @1.5×), un plugin JUCE (Neural DSP…)
    // rend son UI à l'échelle système et déborde d'une fenêtre créée à la taille
    // « 100 % » → GUI rognée + fenêtre non redimensionnable (canResize=false).
    // Le contrat VST3 veut que l'hôte informe la vue AVANT getSize via
    // IPlugViewContentScaleSupport : la vue renvoie alors la taille physique
    // correcte. GetDpiForSystem() = DPI du moniteur principal (process Tauri
    // per-monitor-aware). Un plugin qui détecte ensuite un autre moniteur
    // corrige via IPlugFrame::resizeView (déjà géré). Sans cette interface, on
    // laisse la taille native (le plugin ne scale pas, donc pas de rognage).
    let scale = (unsafe { GetDpiForSystem() } as f32) / 96.0;
    if scale > 0.0 && (scale - 1.0).abs() > 0.01 {
        match view.cast::<IPlugViewContentScaleSupport>() {
            Some(css) => {
                // setContentScaleFactor attend un ScaleFactor (= f32) : `scale`
                // est déjà f32, pas de cast (éviterait clippy::unnecessary_cast).
                let sc_ok = unsafe { css.setContentScaleFactor(scale) };
                tracing::info!(
                    target: "jamodio::vst3::editor",
                    scale, tresult = sc_ok,
                    "setContentScaleFactor (DPI) appliqué"
                );
            }
            None => tracing::info!(
                target: "jamodio::vst3::editor",
                scale,
                "plugin sans IPlugViewContentScaleSupport — taille native conservée"
            ),
        }
    }

    let mut size = ViewRect { left: 0, top: 0, right: 800, bottom: 600 };
    let _ = unsafe { view.getSize(&mut size) };
    let width = (size.right - size.left).max(100);
    let height = (size.bottom - size.top).max(100);

    // `canResize` décide si l'UTILISATEUR peut redimensionner la fenêtre. La
    // grande majorité des plugins (Valhalla, etc.) ont une UI à taille fixe :
    // leur offrir un cadre redimensionnable ne fait que permettre de rogner le
    // GUI (le bug). On ne met WS_THICKFRAME/WS_MAXIMIZEBOX (via
    // WS_OVERLAPPEDWINDOW) que si le plugin le déclare redimensionnable ;
    // sinon fenêtre fixe (caption + system menu + minimize), qui épouse
    // exactement la vue.
    let resizable = unsafe { view.canResize() } == kResultTrue;
    let exstyle = WS_EX_DLGMODALFRAME;
    let style = WS_VISIBLE
        | if resizable {
            WS_OVERLAPPEDWINDOW
        } else {
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX
        };
    let (outer_w, outer_h) = outer_size_for_style(width, height, style, exstyle);
    tracing::info!(
        target: "jamodio::vst3::editor",
        width, height, resizable,
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
            exstyle,
            class_w.as_ptr(),
            title_w.as_ptr(),
            style,
            -2_147_483_648,
            -2_147_483_648,
            outer_w,
            outer_h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        teardown_partial(&conn, &controller, controller_separate);
        return Err("CreateWindowExW returned null".into());
    }
    unsafe {
        SetWindowTextW(hwnd, title_w.as_ptr());
        ShowWindow(hwnd, SW_SHOW);
        // Bring-to-front fiable : l'agent est un process background, donc
        // SetForegroundWindow seul est souvent refusé par Windows (la fenêtre
        // apparaissait DERRIÈRE le browser, de façon non systématique). Le
        // toggle TOPMOST→NOTOPMOST place la fenêtre au-dessus des fenêtres
        // normales sans voler le focus clavier ; SetForegroundWindow ensuite
        // en best-effort (réussit si l'user vient de cliquer dans Jamodio).
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
    }
    tracing::info!(target: "jamodio::vst3::editor", "HWND créée et affichée (top)");

    // 9. IPlugFrame + attached. Sur CE thread (= message thread JUCE), la
    //    création du peer JUCE est directe — pas de marshaling cross-thread.
    let frame_wrapper = ComWrapper::new(PlugFrame::new());
    frame_wrapper.set_hwnd(hwnd);
    let frame: ComPtr<IPlugFrame> = match frame_wrapper.to_com_ptr::<IPlugFrame>() {
        Some(f) => f,
        None => {
            unsafe { DestroyWindow(hwnd) };
            teardown_partial(&conn, &controller, controller_separate);
            return Err("ComWrapper::to_com_ptr<IPlugFrame> a échoué".into());
        }
    };
    let set_frame_ok = unsafe { view.setFrame(frame.as_ptr()) };
    tracing::info!(target: "jamodio::vst3::editor", tresult = set_frame_ok, "setFrame");

    tracing::info!(target: "jamodio::vst3::editor", "calling attached…");
    let att_ok = unsafe { view.attached(hwnd, PLATFORM_HWND.as_ptr() as *const i8) };
    if att_ok != kResultOk {
        unsafe { DestroyWindow(hwnd) };
        teardown_partial(&conn, &controller, controller_separate);
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
    shared.hwnd.store(hwnd, Ordering::SeqCst);
    shared.state.store(STATE_OPEN, Ordering::SeqCst);
    OPEN_EDITORS.with(|m| {
        m.borrow_mut().insert(
            hwnd as isize,
            EditorState {
                frame,
                view,
                handler,
                conn,
                controller,
                controller_separate,
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

/// Annule un wiring partiel quand l'ouverture échoue APRÈS le `connect`
/// (createView null, attached échoué…). Sans ça : le component reste branché
/// à l'ancien proxy → la réouverture suivante redonne `createView == null`
/// (le bug corrigé en v0.4.28 réapparaîtrait sur ce chemin) + leak du
/// controller initialisé. Miroir du teardown WM_DESTROY.
fn teardown_partial(
    conn: &Option<ConnState>,
    controller: &ComPtr<IEditController>,
    controller_separate: bool,
) {
    if let Some(c) = conn {
        if let (Some(p1), Some(p2)) = (
            c.proxy_for_comp.to_com_ptr::<IConnectionPoint>(),
            c.proxy_for_ctrl.to_com_ptr::<IConnectionPoint>(),
        ) {
            unsafe {
                let _ = c.comp_cp.disconnect(p1.as_ptr());
                let _ = c.ctrl_cp.disconnect(p2.as_ptr());
            }
        }
    }
    if controller_separate {
        unsafe {
            let _ = controller.terminate();
        }
    }
}

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
        stream.seek(0, IStreamSeekMode_::kIBSeekSet, &mut dummy)
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
/// Retourne le wiring complet (`ConnState`) pour le garder vivant dans
/// `EditorState.conn` (le plugin cache les pointeurs des proxies —
/// refcount à 0 = use-after-free) et pouvoir `disconnect()` au teardown.
fn connect_component_to_controller_via_proxies(
    component: &ComPtr<IComponent>,
    controller: &ComPtr<IEditController>,
) -> Option<ConnState> {
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
    Some(ConnState {
        comp_cp,
        ctrl_cp,
        proxy_for_comp,
        proxy_for_ctrl,
    })
}

/// Récupère un `IEditController` pour le composant. Si le composant l'expose
/// directement (plugin "single-component"), on le partage. Sinon on crée une
/// instance séparée via la factory et on l'initialise avec le host context.
///
/// Retourne `(controller, separate)` — `separate = true` si l'instance a été
/// créée (et donc initialisée) par nous : c'est alors à l'éditeur de la
/// terminate() au teardown.
fn resolve_controller(
    component: &ComPtr<IComponent>,
    module: &LoadedModule,
    host_app: &ComPtr<IHostApplication>,
) -> Result<(ComPtr<IEditController>, bool), String> {
    if let Some(c) = component.cast::<IEditController>() {
        tracing::info!(
            target: "jamodio::vst3::editor",
            "controller = same instance as component (single-component plugin)"
        );
        return Ok((c, false));
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
            cid.as_ptr(),
            IEditController_iid.as_ptr(),
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
    Ok((controller, true))
}
