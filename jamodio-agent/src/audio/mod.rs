pub mod asio_reset;
/// Host ASIO duplex single-owner (Windows) — remplace les 2 streams cpal sur ASIO.
#[cfg(target_os = "windows")]
pub mod asio_host;
/// P2.0 — spike de faisabilité host ASIO duplex (Windows only, opt-in env var).
#[cfg(target_os = "windows")]
pub mod asio_probe;
pub mod buffer_policy;
pub mod buffer_size;
pub mod com_exec;
pub mod device;
pub mod host;
pub mod capture;
pub mod voice_capture;
pub mod output_pair;
pub mod playback;
pub mod midi;
/// 0.5.4-18 — écoute des réveils de veille Windows → re-init ASIO (no-op ailleurs).
pub mod power_events;
pub mod rt_priority;
