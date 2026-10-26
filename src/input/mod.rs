//! Platform input: capturing local devices and injecting remote input.

pub mod keymap;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{init_process, shutdown, start_capture, warp, Platform};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::{init_process, shutdown, start_capture, warp, Platform};

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "macos")))]
pub use self::unsupported::{init_process, shutdown, start_capture, warp, Platform};

use crate::layout::Monitor;
use crate::protocol::Button;
use std::collections::HashSet;

/// What the OS layer must do with a captured event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Decision {
    /// Swallow the event so the local machine does not see it.
    pub block: bool,
    /// Local cursor just left for another machine: freeze it here.
    pub freeze_at: Option<(i32, i32)>,
    /// Control came back: unfreeze and put the cursor here.
    pub release_to: Option<(i32, i32)>,
}

pub trait Backend: Send {
    fn monitors() -> Vec<Monitor>
    where
        Self: Sized;
    fn move_abs(&mut self, x: i32, y: i32);
    fn button(&mut self, button: Button, down: bool);
    fn wheel(&mut self, dx: i32, dy: i32);
    fn key(&mut self, hid: u16, down: bool);
    fn set_scroll_lines(&mut self, _lines: f32) {}
}

/// Injects remote input and remembers what is held so it can all be released
/// when the controlling machine leaves or disconnects.
pub struct Injector {
    backend: Platform,
    keys: HashSet<u16>,
    buttons: HashSet<Button>,
}

impl Injector {
    #[allow(clippy::default_constructed_unit_structs)]
    pub fn new() -> Injector {
        Injector { backend: Platform::default(), keys: HashSet::new(), buttons: HashSet::new() }
    }

    pub fn move_abs(&mut self, x: i32, y: i32) {
        self.backend.move_abs(x, y);
    }

    pub fn button(&mut self, b: Button, down: bool) {
        if down {
            self.buttons.insert(b);
        } else if !self.buttons.remove(&b) {
            return;
        }
        self.backend.button(b, down);
    }

    pub fn wheel(&mut self, dx: i32, dy: i32) {
        self.backend.wheel(dx, dy);
    }

    pub fn key(&mut self, hid: u16, down: bool) {
        if down {
            self.keys.insert(hid);
        } else if !self.keys.remove(&hid) {
            return;
        }
        self.backend.key(hid, down);
    }

    pub fn set_scroll_lines(&mut self, lines: f32) {
        self.backend.set_scroll_lines(lines);
    }

    pub fn release_all(&mut self) {
        for k in std::mem::take(&mut self.keys) {
            self.backend.key(k, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            self.backend.button(b, false);
        }
    }
}

pub fn monitors() -> Vec<Monitor> {
    <Platform as Backend>::monitors()
}
