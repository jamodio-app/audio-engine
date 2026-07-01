pub mod asio_reset;
/// P2.0 — spike de faisabilité host ASIO duplex (Windows only, opt-in env var).
#[cfg(target_os = "windows")]
pub mod asio_probe;
pub mod buffer_size;
pub mod com_exec;
pub mod device;
pub mod host;
pub mod capture;
pub mod playback;
pub mod midi;
pub mod rt_priority;
