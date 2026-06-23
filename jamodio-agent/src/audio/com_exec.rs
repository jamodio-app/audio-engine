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
    use std::sync::OnceLock;

    /// Travail à exécuter sur le thread STA : une closure auto-contenue qui
    /// renvoie son résultat via son propre canal capturé.
    type Job = Box<dyn FnOnce() + Send>;

    static TX: OnceLock<Sender<Job>> = OnceLock::new();

    /// Sender vers le thread STA, créé paresseusement au premier appel.
    fn sender() -> &'static Sender<Job> {
        TX.get_or_init(|| {
            let (tx, rx) = channel::<Job>();
            std::thread::Builder::new()
                .name("audio-com-sta".into())
                .spawn(move || {
                    use windows_sys::Win32::System::Com::{
                        CoInitializeEx, COINIT_APARTMENTTHREADED,
                    };
                    // STA initialisé une seule fois, pour toute la vie du process.
                    // Pas de `CoUninitialize` : le thread ne se termine jamais
                    // (il vit autant que le process), donc l'apartment reste
                    // valide tant qu'un stream ASIO peut exister.
                    // SAFETY : thread neuf et dédié → pas de COM préexistant,
                    // l'init STA réussit (jamais de RPC_E_CHANGED_MODE).
                    let _ = unsafe {
                        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32)
                    };
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                })
                .expect("spawn audio-com-sta thread");
            tx
        })
    }

    /// Exécute `f` sur le thread STA et renvoie son résultat (bloquant).
    pub fn run<R, F>(f: F) -> R
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (rtx, rrx) = channel::<R>();
        let job: Job = Box::new(move || {
            // Si le caller a abandonné (rrx droppé), l'envoi échoue : on ignore.
            let _ = rtx.send(f());
        });
        sender()
            .send(job)
            .expect("audio-com-sta thread vivant");
        rrx.recv().expect("résultat du thread audio-com-sta")
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
