//! Politique de taille de buffer audio — cible unique, basse latence, adaptative.
//!
//! # Objectif
//!
//! Latence la plus basse possible SANS sacrifier la qualité, **automatiquement**
//! (pas de slider utilisateur), et **cohérente Mac ↔ PC**.
//!
//! - Cible par défaut = [`LOW`] (64 samples = 1,33 ms/direction), demandée en
//!   `BufferSize::Fixed(LOW)` sur les chemins pro (CoreAudio, ASIO) quand le
//!   device l'expose dans sa plage. Sinon repli `BufferSize::Default` (le backend
//!   choisit) — cf. `capture.rs`/`playback.rs`. WASAPI shared impose sa période
//!   (~10 ms) et n'est pas concerné.
//! - Si la machine ne tient pas [`LOW`] sous charge réelle (drops/underruns
//!   soutenus, détectés par le flush `perfstats`), on remonte **une seule fois**
//!   à [`SAFE`] (128 samples) via une reconstruction seamless des streams (même
//!   chemin que la recovery de liveness — pas de redémarrage, pas de coupure
//!   réseau, ~quelques centaines de ms de trou audio auto-résorbé). C'est le
//!   « slider » des concurrents, rendu invisible et automatique.
//!
//! # Pourquoi one-way (LOW → SAFE, jamais l'inverse en cours de run)
//!
//! Re-sonder [`LOW`] en cours de session re-provoquerait un trou audio à chaque
//! tentative → « plus de son / l'user ne comprend pas ». On préfère la STABILITÉ :
//! une machine qui a prouvé ne pas tenir 64 reste à 128 pour tout le run. Un
//! [`LOW`] frais est re-tenté au prochain **démarrage** de l'agent (état global
//! au process, remis à [`LOW`] par nature à chaque lancement).
//!
//! # Portabilité
//!
//! Le module est agnostique de l'OS (simples atomiques). La cible s'applique via
//! cpal, donc CoreAudio et ASIO en bénéficient de la même façon (cohérence). Le
//! repli `Default` protège les devices qui n'exposent pas 64.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Cible basse latence par défaut (samples/canal). 64 @ 48 kHz = 1,33 ms.
pub const LOW: u32 = 64;
/// Repli sûr si [`LOW`] ne tient pas sous charge. 128 @ 48 kHz = 2,67 ms.
pub const SAFE: u32 = 128;

/// Cible courante. Démarre à [`LOW`] ; peut monter one-way à [`SAFE`].
static TARGET: AtomicU32 = AtomicU32::new(LOW);

/// Positionné quand une escalade vient d'avoir lieu : demande au superviseur de
/// liveness de reconstruire les streams à la nouvelle taille. Consommé (remis à
/// `false`) par le superviseur via [`take_rebuild_request`].
static REBUILD_PENDING: AtomicBool = AtomicBool::new(false);

/// Taille de buffer cible actuelle (samples/canal). Lue par `capture.rs` et
/// `playback.rs` au (re)build des streams — les deux côtés lisent la MÊME valeur,
/// donc l'entrée et la sortie d'un duplex ASIO restent cohérentes.
///
/// Cohérence entrée↔sortie garantie par un invariant de lock : l'ouverture
/// (`open_duplex_on_com`, qui lit la cible pour l'entrée PUIS la sortie) tourne
/// sous le lock `PipelineState`, et le seul escaladeur (le flush `perfstats`)
/// doit prendre CE MÊME lock avant de pouvoir appeler [`escalate_to_safe`]. Une
/// escalade ne peut donc jamais s'intercaler entre le build entrée et le build
/// sortie d'une même ouverture. ⚠️ Ne pas escalader hors de ce lock.
#[inline]
pub fn target() -> u32 {
    TARGET.load(Ordering::Acquire)
}

/// Choix de taille de buffer à l'ouverture d'un stream.
pub enum BufferChoice {
    /// Demander `BufferSize::Fixed(n)` si le device l'expose, sinon `Default`.
    Fixed(u32),
    /// Forcer `BufferSize::Default` = **taille PRÉFÉRÉE du driver** (souvent 128 sur
    /// Focusrite). C'est le comportement d'avant 0.5.4-17 ; `Fixed(64)` déclenche le
    /// wedge cold-start (ADC railé + DMA figé, bug 2026-07-03) que 128/préféré n'a pas.
    PreferDriver,
}

/// Choix de buffer courant, avec override d'environnement `JAMODIO_ASIO_BUFFER` :
///   - `default` (ou `pref`) → `PreferDriver` (taille préférée du driver)
///   - `<nombre>`            → `Fixed(nombre)`
///   - absent                → `Fixed(target())` (cible basse latence, 64 par défaut)
///
/// Permet de trancher au banc si c'est bien la taille `Fixed(64)` qui déclenche le
/// wedge cold-start Focusrite, sans rebuild.
pub fn choice() -> BufferChoice {
    match std::env::var("JAMODIO_ASIO_BUFFER").ok().as_deref() {
        Some("default") | Some("Default") | Some("pref") => BufferChoice::PreferDriver,
        Some(s) => match s.parse::<u32>() {
            Ok(n) if n > 0 => BufferChoice::Fixed(n),
            _ => BufferChoice::Fixed(target()),
        },
        None => BufferChoice::Fixed(target()),
    }
}

/// Escalade one-way [`LOW`] → [`SAFE`]. Renvoie `true` UNIQUEMENT si la cible a
/// effectivement changé (première escalade) — permet au caller de ne déclencher
/// la reconstruction et le log qu'une seule fois. Idempotent ensuite.
pub fn escalate_to_safe() -> bool {
    TARGET
        .compare_exchange(LOW, SAFE, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Demande une reconstruction des streams à la cible courante (après escalade).
pub fn request_rebuild() {
    REBUILD_PENDING.store(true, Ordering::Release);
}

/// Consomme une éventuelle demande de reconstruction (true une seule fois par
/// demande). Appelé par le superviseur de liveness à chaque tick.
pub fn take_rebuild_request() -> bool {
    REBUILD_PENDING.swap(false, Ordering::AcqRel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    // Un seul test : les statics sont globaux au process, l'exécution parallèle
    // de plusieurs tests sur le même état donnerait des résultats non
    // déterministes. On remet l'état à zéro en début ET fin.
    #[test]
    fn escalation_is_one_way_and_signals_rebuild_once() {
        TARGET.store(LOW, Ordering::Release);
        REBUILD_PENDING.store(false, Ordering::Release);

        assert_eq!(target(), LOW);

        // Première escalade : change la cible + peut demander un rebuild.
        assert!(escalate_to_safe(), "première escalade doit changer la cible");
        assert_eq!(target(), SAFE);
        request_rebuild();
        assert!(take_rebuild_request(), "demande de rebuild consommée une fois");
        assert!(!take_rebuild_request(), "puis plus rien à consommer");

        // Escalade répétée : no-op (déjà à SAFE), ne redéclenche rien.
        assert!(!escalate_to_safe(), "déjà escaladé ⇒ pas de nouveau changement");
        assert_eq!(target(), SAFE);

        // Reset pour ne pas polluer d'autres tests éventuels.
        TARGET.store(LOW, Ordering::Release);
        REBUILD_PENDING.store(false, Ordering::Release);
    }
}
