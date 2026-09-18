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
    fn instants_before_creation_never_underflow() {
        let born = Instant::now() + Duration::from_secs(1);
        let activity = RecvActivity::new(born);
        activity.mark_packet(born - Duration::from_millis(500));
        assert_eq!(activity.silent_ms(born - Duration::from_millis(200)), 0);
    }
}
