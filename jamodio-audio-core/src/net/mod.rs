pub mod rtcp;
pub mod rtp;
// Lot 1-D2 — heure d'arrivée d'un paquet dans le SYSTÈME, pour séparer une
// attente locale (tâche de réception servie tard) d'un retard en amont.
pub mod rx_timestamp;
pub mod seq;
// SRTP : 2 backends derrière la même API publique. macOS/Linux = libsrtp2,
// Windows = webrtc-srtp (cf. mémoire `srtp_strategy.md`).
#[cfg(not(windows))]
#[path = "srtp_libsrtp.rs"]
pub mod srtp;
#[cfg(windows)]
#[path = "srtp_webrtc.rs"]
pub mod srtp;
// Tests communs aux deux backends (API publique seulement).
#[cfg(test)]
mod srtcp_tests;
pub mod udp;
pub mod uplink;
