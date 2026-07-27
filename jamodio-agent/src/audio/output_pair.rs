//! Paire de canaux de sortie (Lot B ASIO + extension CoreAudio) — helper PARTAGÉ.
//!
//! En sortie multicanal (interface ASIO Windows OU device CoreAudio multicanal),
//! l'agent ouvre TOUS les canaux de sortie et n'écrit le mix stéréo que dans la
//! PAIRE choisie (`output_pair_start`, index de départ 0-based : 0 = canaux 1-2,
//! 2 = 3-4…), zéros ailleurs. L'index est un `AtomicUsize` partagé (pipeline ↔
//! callback) → changer de paire = swap LIVE, sans réouverture.
//!
//! Ce module ne contient que la borne PURE (testable sur toutes plateformes) ;
//! `asio_host` (Windows) et `playback` (cpal, toutes plateformes) l'utilisent.

/// Borne l'index de départ de la paire à `[0, n_out-2]` : la paire occupe les
/// canaux `start` et `start+1`. Appelé dans le callback à chaque bloc (un `load`
/// atomique + ce clamp). Si l'interface a moins de 2 sorties (cas dégénéré),
/// renvoie 0 (jamais d'accès hors borne).
pub fn clamp_output_pair(start: usize, n_out: usize) -> usize {
    if n_out < 2 {
        return 0;
    }
    start.min(n_out - 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn clamps_into_range() {
        // 8 sorties → paires valides : start ∈ {0,2,4,6}. Le clamp borne à n_out-2.
        assert_eq!(clamp_output_pair(0, 8), 0, "1-2");
        assert_eq!(clamp_output_pair(6, 8), 6, "7-8 (dernière paire)");
        assert_eq!(clamp_output_pair(7, 8), 6, "start hors borne → dernière paire valide");
        assert_eq!(clamp_output_pair(100, 8), 6, "très grand → borné");
    }

    #[test]
    fn degenerate_devices() {
        // Interface stéréo : seule la paire 0-1 existe.
        assert_eq!(clamp_output_pair(0, 2), 0);
        assert_eq!(clamp_output_pair(5, 2), 0, "pas d'autre paire qu'un stéréo → 0");
        // Cas dégénéré (< 2 sorties) : jamais de paire → 0 (pas d'accès hors borne).
        assert_eq!(clamp_output_pair(0, 1), 0);
        assert_eq!(clamp_output_pair(3, 0), 0);
    }

    #[test]
    fn swap_is_pure_no_driver_call() {
        // INVARIANT (pattern 787a7eb) : changer la paire = un simple store/load
        // atomique, JAMAIS une réouverture (open/ASIOInit/create_buffers ni rebuild
        // du stream cpal). On le matérialise ici : le pilotage passe par un
        // AtomicUsize partagé, lu dans le callback — aucun appel driver requis.
        let pair = Arc::new(AtomicUsize::new(0));
        let shared = pair.clone();               // clone donné au callback
        pair.store(4, Ordering::Relaxed);         // « set_output_pair(4) » côté pipeline
        assert_eq!(clamp_output_pair(shared.load(Ordering::Relaxed), 8), 4,
            "le callback lit la nouvelle paire sans réouverture");
    }
}
