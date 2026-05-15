pub mod rtp;
// SRTP : 2 backends derrière la même API publique. macOS/Linux = libsrtp2,
// Windows = webrtc-srtp (cf. mémoire `srtp_strategy.md`).
#[cfg(not(windows))]
#[path = "srtp_libsrtp.rs"]
pub mod srtp;
#[cfg(windows)]
#[path = "srtp_webrtc.rs"]
pub mod srtp;
pub mod udp;
