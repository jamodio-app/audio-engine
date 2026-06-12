//! Thread "vst3-main" — l'unique thread main VST3 de l'agent.
//!
//! # Pourquoi un thread unique
//!
//! La spec VST3 (et tous les DAW réels) impose que TOUS les appels non-RT
//! (module load, factory, createInstance, initialize, connect, createView,
//! attached…) viennent du même thread "main" qui pompe des messages Win32.
//!
//! Diagnostic v0.4.24→26 (Surge XT + Valhalla = plugins JUCE) : le wrapper
//! JUCE lie son MessageManager au thread qui construit la `JucePluginFactory`
//! (= le thread qui appelle `GetPluginFactory` après dlopen). Ensuite,
//! `IPlugView::attached()` → `addToDesktop` → constructeur `HWNDComponentPeer`
//! → `callFunctionIfNotLocked` → si le thread appelant n'est PAS ce message
//! thread, JUCE marshale la création de fenêtre vers lui via
//! `callFunctionOnMessageThread` et BLOQUE en attendant la réponse. Notre
//! ancien découpage (load sur le thread WS tokio, éditeur sur un thread STA
//! par fenêtre) plaçait le message thread JUCE sur un thread qui ne pompe
//! jamais → hang infini dans `attached()` (fenêtre blanche "NOT RESPONDING").
//!
//! Fix : ce module fournit LE thread main persistant. Il fait `CoInitializeEx`
//! STA une fois, crée une fenêtre message-only cachée, puis boucle sur
//! `GetMessageW`. Les jobs (closures) postés par les autres threads sont
//! drainés via un message `WM_APP_JOB` posté à la fenêtre cachée — passer par
//! une fenêtre (et pas `PostThreadMessageW`) garantit que les jobs survivent
//! aux boucles modales internes de Win32 (drag de fenêtre, menus…) qui ne
//! délivrent que les messages adressés à une HWND.
//!
//! Les fenêtres d'éditeur plugin sont créées SUR ce thread : leurs messages
//! sont dispatchés par la même pump, et le message thread JUCE == ce thread
//! → `attached()` crée le peer directement, sans marshaling.
//!
//! # Invariant anti-deadlock
//!
//! Les jobs ne doivent JAMAIS prendre le lock `plugin_host` (les callers de
//! `run()` le tiennent déjà). Un job qui relockerait pendant qu'un caller
//! attend = deadlock croisé.

#![cfg(target_os = "windows")]

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
    System::LibraryLoader::GetModuleHandleW,
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW,
        RegisterClassExW, TranslateMessage, HWND_MESSAGE, MSG, WM_APP, WNDCLASSEXW,
    },
};

/// Message posté à la fenêtre cachée pour signaler "des jobs attendent".
const WM_APP_JOB: u32 = WM_APP + 1;

type Job = Box<dyn FnOnce() + Send + 'static>;

struct MainThread {
    /// HWND de la fenêtre message-only — cible des PostMessageW(WM_APP_JOB).
    /// Win32 HWND utilisable depuis n'importe quel thread.
    hidden_hwnd: usize,
    thread_id: u32,
    jobs: Mutex<VecDeque<Job>>,
}

// SAFETY: hidden_hwnd est une HWND (handle opaque, valide cross-thread pour
// PostMessageW), jobs est derrière Mutex.
unsafe impl Send for MainThread {}
unsafe impl Sync for MainThread {}

static MAIN: OnceLock<MainThread> = OnceLock::new();

fn instance() -> &'static MainThread {
    MAIN.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<(usize, u32)>();
        std::thread::Builder::new()
            .name("vst3-main".into())
            .spawn(move || thread_main(tx))
            .expect("spawn vst3-main");
        let (hidden_hwnd, thread_id) = rx.recv().expect("vst3-main init");
        MainThread {
            hidden_hwnd,
            thread_id,
            jobs: Mutex::new(VecDeque::new()),
        }
    })
}

/// `true` si on est déjà sur le thread vst3-main.
pub fn on_main_thread() -> bool {
    MAIN.get()
        .map(|m| m.thread_id == unsafe { GetCurrentThreadId() })
        .unwrap_or(false)
}

/// Exécute `f` sur le thread vst3-main et BLOQUE jusqu'au résultat.
/// Si on est déjà sur vst3-main, exécute inline (pas de deadlock de nesting).
pub fn run<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    if on_main_thread() {
        return f();
    }
    let (tx, rx) = mpsc::channel();
    post(move || {
        let _ = tx.send(f());
    });
    rx.recv().expect("vst3-main job result (thread mort ?)")
}

/// Poste `f` sur le thread vst3-main SANS attendre. Les jobs sont exécutés
/// en FIFO, dans l'ordre de post — y compris relativement aux messages de
/// fenêtre postés avant/après (même queue de thread).
pub fn post(f: impl FnOnce() + Send + 'static) {
    let m = instance();
    if on_main_thread() {
        // Inline : on est déjà sur le thread, exécuter maintenant garde
        // l'ordre intuitif pour le caller (close avant drop, etc.).
        f();
        return;
    }
    m.jobs.lock().expect("jobs lock").push_back(Box::new(f));
    unsafe {
        PostMessageW(m.hidden_hwnd as HWND, WM_APP_JOB, 0, 0);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

thread_local! {
    /// Garde anti-réentrance du drain de jobs. Uniquement touché sur
    /// vst3-main → un `Cell` thread-local suffit (pas d'atomique).
    static DRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe extern "system" fn hidden_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_JOB {
        // CRITIQUE (review 11/06) : interdire le drain RÉENTRANT. Un job
        // comme `open_editor` appelle `IPlugView::attached()`, et beaucoup de
        // plugins y font tourner une pompe de messages imbriquée (splash,
        // dialog de licence). Cette pompe re-livre WM_APP_JOB → sans cette
        // garde, on exécuterait un job `unload` IMBRIQUÉ dans attached() →
        // `component.terminate()` pendant qu'attached() est sur la pile =
        // use-after-terminate. On re-poste et on sort : les jobs en attente
        // seront drainés quand le job courant aura rendu la main.
        if DRAINING.with(|d| d.get()) {
            PostMessageW(hwnd, WM_APP_JOB, 0, 0);
            return 0;
        }
        DRAINING.with(|d| d.set(true));
        // Draine TOUS les jobs en attente (plusieurs posts peuvent coalescer).
        loop {
            let job = match MAIN.get() {
                Some(m) => m.jobs.lock().expect("jobs lock").pop_front(),
                None => None,
            };
            match job {
                Some(j) => {
                    // catch_unwind : un panic qui traverserait la frontière
                    // extern "system" du wnd_proc = abort du process entier.
                    // Un plugin defectueux au scan/load ne doit pas tuer l'agent.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(j));
                    if r.is_err() {
                        tracing::error!(
                            target: "jamodio::vst3",
                            "panic dans un job vst3-main (plugin defectueux ?) — job abandonné"
                        );
                    }
                }
                None => break,
            }
        }
        DRAINING.with(|d| d.set(false));
        return 0;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn thread_main(ready: mpsc::Sender<(usize, u32)>) {
    // STA une fois pour toute la vie du process — requis par les plugins qui
    // utilisent OLE (drag&drop) et conforme à ce que font les DAW.
    let com_init = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    tracing::info!(
        target: "jamodio::vst3",
        hresult = format!("0x{com_init:08X}"),
        "vst3-main thread starting (CoInitializeEx STA)"
    );

    let class_name = wide("JamodioVst3MainHidden");
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(hidden_wnd_proc),
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
        RegisterClassExW(&wc);
    }
    let hidden = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE, // message-only window : invisible, pas dans la taskbar
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hidden.is_null() {
        tracing::error!(target: "jamodio::vst3", "vst3-main: CreateWindowExW(message-only) failed");
        return;
    }
    let thread_id = unsafe { GetCurrentThreadId() };
    let _ = ready.send((hidden as usize, thread_id));

    // Pump éternelle — le thread vit aussi longtemps que le process. Les
    // fenêtres d'éditeur plugin créées sur ce thread sont dispatchées ici.
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
    }
    tracing::warn!(target: "jamodio::vst3", "vst3-main thread exiting (WM_QUIT inattendu)");
}
