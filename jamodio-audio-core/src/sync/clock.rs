//! Horloge monotone process-wide de l'agent (Option B — ancrage échantillon↔mural).
//!
//! Fournit un temps en millisecondes depuis un epoch fixé au **premier appel**.
//! C'est le domaine temporel COMMUN utilisé par :
//!   - le mixer, qui horodate son ancre de sortie (`OutputAnchor.mono_ms`) dans
//!     `mix_into` (frame de sortie `F0` ↔ instant monotone) ;
//!   - le serveur WS, qui stampe `agentMonoMs` dans `reference-clock-pong`.
//!
//! Le browser relie ensuite ce domaine à son horloge murale via le ping/pong
//! filtré min-RTT (cf. B0 §3.2). Les deux côtés DOIVENT lire le même epoch :
//! d'où un `OnceLock<Instant>` unique au process (jamais réinitialisé), au lieu
//! d'un epoch par instance qui désynchroniserait ancre et pong.
//!
//! `Instant` est monotone (jamais en arrière, insensible aux ajustements NTP de
//! l'horloge murale système) — exactement ce qu'il faut pour un mapping stable
//! échantillon↔temps.

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Millisecondes écoulées depuis l'epoch monotone de l'agent (fixé au 1er appel).
///
/// Résolution sub-milliseconde (f64). Coût : un `Instant::now()` + un load
/// atomique — négligeable, appelable depuis le callback audio temps-réel.
pub fn mono_now_ms() -> f64 {
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_now_ms_is_monotone_non_negative() {
        let a = mono_now_ms();
        let b = mono_now_ms();
        assert!(a >= 0.0, "temps depuis epoch toujours ≥ 0");
        assert!(b >= a, "horloge monotone : b ≥ a ({b} < {a})");
    }
}
