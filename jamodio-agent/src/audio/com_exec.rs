//! Exécuteur COM-STA pour les opérations CPAL/ASIO (Windows).
//!
//! # Pourquoi ce module (bug Windows/ASIO, 23/06/2026)
//!
//! Sur Windows, `asio-sys` charge chaque driver ASIO via `CoCreateInstance`
//! (ASIO SDK `loadAsioDriver`) **sans initialiser COM lui-même** : il compte
//! sur le thread appelant. Deux conséquences :
//!
//!  1. Toute opération qui touche un driver ASIO — énumération, résolution
//!     d'un device, ouverture ET fermeture d'un stream — doit tourner sur un
//!     thread où COM est initialisé. Les workers tokio (work-stealing, COM non
//!     initialisé) renvoient `CO_E_NOTINITIALIZED` → cpal saute le device
//!     silencieusement (`host/asio/device.rs` : `Err(_) => continue`) → device
//!     « introuvable » alors qu'il est bien là.
//!  2. Un objet COM STA est lié à SON apartment : il doit être créé, utilisé
//!     ET détruit sur le **même** thread STA. D'où un thread STA *persistant*
//!     (pas un thread jetable par appel) qui possède la durée de vie des
//!     objets driver tant qu'un `cpal::Stream` ASIO est vivant.
//!
//! Ce module fournit donc UN thread STA persistant qui exécute des closures à
//! la demande. Tous les call-sites CPAL côté Windows passent par `run()` :
//! l'énumération (`device.rs`) comme l'ouverture/fermeture des streams
//! (`pipeline.rs`). ASIO étant mono-client, l'unicité du thread garantit aussi
//! une sérialisation naturelle des accès au driver.
//!
//! macOS (CoreAudio, pas de COM) : `run()` exécute la closure **inline**,
//! aucun thread créé, comportement strictement inchangé.

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    /// Travail à exécuter sur le thread STA : une closure auto-contenue qui
    /// renvoie son résultat via son propre canal capturé.
    type Job = Box<dyn FnOnce() + Send>;

    /// Worker STA courant : son canal d'envoi + son handle (pour le `join` au
    /// recyclage). Encapsulés ensemble pour rester cohérents.
    struct Worker {
        tx: Sender<Job>,
        handle: JoinHandle<()>,
    }

    /// Worker courant, REMPLAÇABLE (≠ `OnceLock`) pour autoriser le RECYCLAGE de
    /// l'apartment COM (cf. `recycle`). Motif : certains drivers ASIO (Focusrite
    /// USB) reviennent « callbacks vivants mais MUETS » (ni VU ni son) après une
    /// réouverture À FROID dans le MÊME apartment COM — typiquement après une
    /// interface restée idle plusieurs minutes puis relâchée par la grâce. Seul un
    /// `CoUninitialize`/`CoInitialize` frais les débloque (ce qu'un redémarrage de
    /// process faisait implicitement). `None` = pas encore de worker (créé au 1er
    /// appel `run`).
    static WORKER: Mutex<Option<Worker>> = Mutex::new(None);

    /// Démarre un thread STA neuf (COM initialisé pour sa durée de vie) et renvoie
    /// son `Worker`. Le thread termine — et rend l'apartment via `CoUninitialize`
    /// — dès que son `Sender` est droppé (fin de process ou `recycle`).
    fn spawn_worker() -> Worker {
        let (tx, rx) = channel::<Job>();
        let handle = std::thread::Builder::new()
            .name("audio-com-sta".into())
            .spawn(move || {
                use windows_sys::Win32::System::Com::{
                    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
                };
                // SAFETY : thread neuf et dédié → pas de COM préexistant, l'init
                // STA réussit (jamais de RPC_E_CHANGED_MODE).
                let _ = unsafe {
                    CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32)
                };
                while let Ok(job) = rx.recv() {
                    // Défense en profondeur : un panic dans une closure driver
                    // (ASIO tiers, unwrap interne cpal, Drop de stream/host) ne
                    // doit JAMAIS tuer ce thread. S'il mourait, TOUTE opération
                    // audio Windows ultérieure (énumération, open/close stream)
                    // échouerait pour la vie du worker. Même leçon que
                    // main_thread.rs:169 côté VST3. Cf. review pré-BETA (C7).
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }
                // Sender droppé (recycle ou arrêt) : on rend l'apartment proprement
                // (équilibre le `CoInitializeEx` ci-dessus) AVANT de terminer, pour
                // que le prochain worker reparte d'un COM strictement neuf.
                // SAFETY : appelé sur le thread qui a fait l'init, exactement une
                // fois, sans objet COM survivant (contrat de `recycle`).
                unsafe { CoUninitialize() };
            })
            .expect("spawn audio-com-sta thread");
        Worker { tx, handle }
    }

    /// Exécute `f` sur le thread STA courant (créé paresseusement au 1er appel) et
    /// renvoie son résultat (bloquant).
    ///
    /// Si `f` panique, le thread STA SURVIT (catch_unwind) et le panic est
    /// re-propagé au thread appelant — pour les handlers ws il est absorbé en
    /// amont par `spawn_blocking`/`JoinError`. L'audio Windows reste opérationnel
    /// pour les appels suivants (contrairement à l'ancien `.expect` qui figeait
    /// tout après le premier panic). Cf. review pré-BETA 2026-07-12 (C7).
    pub fn run<R, F>(f: F) -> R
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (rtx, rrx) = channel::<std::thread::Result<R>>();
        let job: Job = Box::new(move || {
            // f est exécutée sous catch_unwind : un panic est capturé et renvoyé
            // au caller au lieu de dérouler le thread STA. Si le caller a
            // abandonné (rrx droppé), l'envoi échoue : on ignore.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            let _ = rtx.send(result);
        });
        // Envoi sous le lock (crée le worker au besoin) ; l'attente du résultat se
        // fait HORS lock → un `recycle` concurrent (jamais en pratique : verrou
        // pipeline tenu) ne peut pas interbloquer avec une job en vol.
        {
            let mut guard = WORKER.lock().expect("com-sta worker mutex empoisonné");
            if guard.is_none() {
                *guard = Some(spawn_worker());
            }
            guard
                .as_ref()
                .expect("worker présent")
                .tx
                .send(job)
                .expect("audio-com-sta thread vivant");
        }
        match rrx.recv() {
            Ok(Ok(val)) => val,
            // La closure a paniqué : thread STA préservé, on re-panique le caller.
            Ok(Err(panic)) => std::panic::resume_unwind(panic),
            // Thread STA mort : ne devrait plus arriver (catch_unwind ci-dessus).
            Err(_) => panic!("audio-com-sta thread mort"),
        }
    }

    /// Recycle le thread STA : termine l'apartment COM courant (`CoUninitialize`
    /// au drop du thread) et en recrée un neuf (`CoInitialize` frais). Sert à
    /// débloquer un driver ASIO revenu « vivant mais muet » après une réouverture
    /// à froid (cf. `WORKER`).
    ///
    /// # Contrat d'appel (IMPORTANT)
    /// À n'appeler QUE lorsqu'AUCUN objet ASIO/COM n'est vivant (tous les streams
    /// et hosts fermés) — sinon leur `Drop`, qui DOIT tourner sur l'apartment
    /// créateur, s'exécuterait sur un thread mort. En pratique : uniquement juste
    /// avant une réouverture À FROID, après `close_audio_driver()`.
    pub fn recycle() {
        let mut guard = WORKER.lock().expect("com-sta worker mutex empoisonné");
        if let Some(w) = guard.take() {
            // Drop du Sender → `rx.recv()` renvoie `Err` → le worker sort de sa
            // boucle, exécute `CoUninitialize`, puis termine. On ATTEND sa fin
            // (join) pour garantir que l'apartment est bien rendu avant d'en
            // recréer un — sinon deux apartments coexisteraient brièvement.
            drop(w.tx);
            let _ = w.handle.join();
        }
        // Recrée immédiatement un worker neuf : les appels `run` suivants repartent
        // d'un COM strictement propre.
        *guard = Some(spawn_worker());
    }
}

/// Exécute `f` sur le thread COM-STA dédié (Windows) et renvoie son résultat.
#[cfg(target_os = "windows")]
pub fn run<R, F>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    imp::run(f)
}

/// Recycle l'apartment COM-STA (Windows) : `CoUninitialize`/`CoInitialize` frais.
/// À n'appeler qu'AUCUN objet ASIO vivant (cf. `imp::recycle`). No-op ailleurs.
#[cfg(target_os = "windows")]
#[inline]
pub fn recycle() {
    imp::recycle()
}

/// macOS (et autres non-Windows) : pas de COM → exécution inline, aucun thread.
/// Les bornes `Send`/`'static` ne sont volontairement PAS exigées ici, pour ne
/// rien contraindre sur le chemin CoreAudio existant.
#[cfg(not(target_os = "windows"))]
#[inline]
pub fn run<R, F>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

/// macOS (et autres non-Windows) : pas de COM → rien à recycler. No-op.
#[cfg(not(target_os = "windows"))]
#[inline]
pub fn recycle() {}
