//! A small transparent, click-through, always-on-top window for the effects.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::Overlay;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::{request_capture_permission, Overlay};

#[cfg(not(any(windows, target_os = "macos")))]
mod other;
#[cfg(not(any(windows, target_os = "macos")))]
pub use self::other::Overlay;
