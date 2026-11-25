pub mod app;
#[cfg(windows)]
pub mod frame;
#[cfg(target_os = "linux")]
#[path = "frame_linux.rs"]
pub mod frame;
pub mod pickers;
pub mod preview;
pub mod settings;
pub mod theme;
