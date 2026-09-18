//! Activité d'un flux reçu : depuis quand aucun paquet n'est arrivé.
//!
//! Un flux reçu n'est JAMAIS supprimé parce qu'il se tait : sa durée de vie
//! appartient au navigateur (`add-stream` / `remove-stream`, lui-même prévenu par le
//! SFU à chaque fermeture de producer). Un silence — coupure réseau, Wi-Fi qui
//! décroche — est un ÉTAT, publié au navigateur (`perf-stats.recvStreams[].silentMs`),
//! et le son reprend tout seul quand les paquets reviennent. (Jusqu'en 0.6.2, 8 s
//! sans paquet supprimaient le flux instrument sans prévenir personne : le musicien
//! restait muet jusqu'à la fin de la session — recette du 16/09/2026.)
//!
//! Écrit par la tâche I/O de réception (tokio, hors thread audio) à chaque paquet,
//! lu à 1 Hz par les perf-stats : un seul `store` atomique, aucun verrou.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::time::Duration;

/// Attente après une erreur de réception UDP, en fonction du nombre d'erreurs
/// CONSÉCUTIVES (1 = la première depuis le dernier paquet reçu).
///
/// Avant N13, toute erreur coûtait 10 ms d'aveuglement, y compris la plus
/// fréquente : le `WSAECONNRESET` fantôme de Windows, qui ne dit RIEN sur la
/// santé de la socket (cf. `net/udp.rs`). Pendant ces 10 ms, les paquets
/// continuaient d'arriver sans être lus — de quoi fabriquer l'accroc que le
/// chantier cherche justement à supprimer.
///
/// Donc : la première erreur ne coûte plus rien, et l'attente ne monte que si
/// les erreurs S'ENCHAÎNENT — seul cas où elles disent quelque chose (socket
/// réellement en peine). Le plafond garde l'ancien comportement comme pire cas,
/// et garantit qu'une socket durablement fautive ne fait pas tourner la boucle
/// à vide.
pub fn recv_error_backoff(consecutive: u32) -> Duration {
    const CAP_MS: u64 = 10;
    let ms = match consecutive {
        0 | 1 => 0,
        n if n >= 6 => CAP_MS,
        n => 1u64 << (n - 2),
    };
    Duration::from_millis(ms)
}

/// Au-delà de ce silence, la tâche I/O le journalise (une fois), puis journalise la
/// reprise. Information de diagnostic seulement : rien n'est coupé.
pub const SILENCE_LOG_AFTER_MS: u64 = 3_000;

/// Horodatage du dernier paquet d'un flux, relatif à la création du flux.
#[derive(Debug)]
pub struct RecvActivity {
    born: Instant,
    /// Millisecondes entre `born` et le dernier paquet (0 = aucun paquet encore :
    /// le silence compte alors depuis la création du flux).
    last_packet_ms: AtomicU64,
    /// Lot 0 (chantier tampon) — erreurs rendues par la socket UDP pour ce flux.
    /// Chacune coûte aujourd'hui 10 ms d'attente avant la reprise : sans ce
    /// compteur, on ne sait pas si ce chemin est emprunté en vrai (N13).
    recv_errors: AtomicU64,
}

impl RecvActivity {
    pub fn new(born: Instant) -> Self {
        Self {
            born,
            last_packet_ms: AtomicU64::new(0),
            recv_errors: AtomicU64::new(0),
        }
    }

    /// Un paquet est arrivé à `at`.
    pub fn mark_packet(&self, at: Instant) {
        let ms = at.saturating_duration_since(self.born).as_millis() as u64;
        self.last_packet_ms.store(ms, Ordering::Relaxed);
    }

    /// La socket a rendu une erreur de réception.
    pub fn mark_recv_error(&self) {
        self.recv_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Cumul des erreurs de réception depuis la création du flux.
    pub fn recv_errors(&self) -> u64 {
        self.recv_errors.load(Ordering::Relaxed)
    }

    /// Durée sans paquet à l'instant `now`, en ms.
    pub fn silent_ms(&self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.born).as_millis() as u64;
        elapsed.saturating_sub(self.last_packet_ms.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn silence_counts_from_creation_until_first_packet() {
        let born = Instant::now();
        let activity = RecvActivity::new(born);
        assert_eq!(activity.silent_ms(born), 0);
        assert_eq!(
            activity.silent_ms(born + Duration::from_millis(2_500)),
            2_500
        );
    }

    #[test]
    fn a_packet_resets_the_silence() {
        let born = Instant::now();
        let activity = RecvActivity::new(born);
        activity.mark_packet(born + Duration::from_secs(10));
        assert_eq!(activity.silent_ms(born + Duration::from_secs(10)), 0);
        assert_eq!(
            activity.silent_ms(born + Duration::from_millis(10_400)),
            400
        );
    }

    #[test]
    fn long_silence_then_resume() {
        // Coupure de 20 s (au-delà des 8 s qui supprimaient le flux avant 0.6.3) :
        // le silence se mesure, puis la reprise le remet à zéro.
        let born = Instant::now();
        let activity = RecvActivity::new(born);
        activity.mark_packet(born + Duration::from_secs(5));
        assert_eq!(activity.silent_ms(born + Duration::from_secs(25)), 20_000);
        activity.mark_packet(born + Duration::from_secs(25));
        assert_eq!(activity.silent_ms(born + Duration::from_millis(25_010)), 10);
    }

    #[test]
    fn recv_error_backoff_ne_punit_pas_la_premiere_erreur() {
        // Le WSAECONNRESET fantôme de Windows est isolé : il ne doit coûter
        // aucune milliseconde d'aveuglement.
        assert_eq!(recv_error_backoff(1), Duration::ZERO);
        assert_eq!(recv_error_backoff(0), Duration::ZERO);
    }

    #[test]
    fn recv_error_backoff_monte_puis_plafonne() {
        // Des erreurs qui s'enchaînent disent quelque chose : on lève le pied,
        // sans jamais dépasser l'attente d'avant N13 (10 ms).
        assert_eq!(recv_error_backoff(2), Duration::from_millis(1));
        assert_eq!(recv_error_backoff(3), Duration::from_millis(2));
        assert_eq!(recv_error_backoff(4), Duration::from_millis(4));
        assert_eq!(recv_error_backoff(5), Duration::from_millis(8));
        for n in [6, 7, 64, u32::MAX] {
            assert_eq!(recv_error_backoff(n), Duration::from_millis(10), "n={n}");
        }
    }

    #[test]
    fn instants_before_creation_never_underflow() {
        let born = Instant::now() + Duration::from_secs(1);
        let activity = RecvActivity::new(born);
        activity.mark_packet(born - Duration::from_millis(500));
        assert_eq!(activity.silent_ms(born - Duration::from_millis(200)), 0);
    }
}
