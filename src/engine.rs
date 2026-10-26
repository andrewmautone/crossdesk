//! Decides where local input goes: to this machine, or to the machine the
//! virtual cursor is on. Called synchronously from the OS input hooks, so
//! every path here must be fast and never block on the network.

use crate::clipboard::Clipboard;
use crate::config::Settings;
use crate::input::{keymap, Decision};
use crate::layout::{Dir, World};
use crate::net::Registry;
use crate::protocol::{Button, Msg};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug)]
struct Remote {
    target: String,
    monitor: usize,
    pos: (f64, f64),
    /// Where the local cursor was when it left; used if control returns abruptly.
    return_pt: (i32, i32),
}

pub struct Engine {
    local_id: String,
    world: World,
    settings: Settings,
    registry: Arc<Registry>,
    clipboard: Clipboard,
    remote: Option<Remote>,
    locked: bool,
    local_buttons: HashSet<Button>,
    remote_buttons: HashSet<Button>,
    held_keys: HashSet<u16>,
}

const HOTKEY_RETURN: u16 = 0x14; // Q
const HOTKEY_LOCK: u16 = 0x0F; // L

impl Engine {
    pub fn new(local_id: String, settings: Settings, registry: Arc<Registry>, clipboard: Clipboard) -> Engine {
        Engine {
            local_id,
            world: World::default(),
            settings,
            registry,
            clipboard,
            remote: None,
            locked: false,
            local_buttons: HashSet::new(),
            remote_buttons: HashSet::new(),
            held_keys: HashSet::new(),
        }
    }

    /// Returns where to put the local cursor if control had to come back.
    pub fn set_world(&mut self, world: World) -> Option<(i32, i32)> {
        self.world = world;
        let r = self.remote.as_ref()?;
        // Monitor indexes change with the world; re-find the cursor or give control back.
        let world = &self.world;
        match world.find(r.pos).filter(|&i| world.monitors[i].machine == r.target) {
            Some(i) => {
                self.remote.as_mut().unwrap().monitor = i;
                None
            }
            None => self.force_local(),
        }
    }

    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
    }

    pub fn active_target(&self) -> Option<&str> {
        self.remote.as_ref().map(|r| r.target.as_str())
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    fn local_scale(&self) -> f64 {
        self.world.monitors.iter().find(|m| m.machine == self.local_id).map_or(1.0, |m| m.scale)
    }

    pub fn on_move(&mut self, pt: (i32, i32), delta: Option<(f64, f64)>) -> Decision {
        match self.remote.clone() {
            None => self.local_move(pt),
            Some(r) => self.remote_move(r, delta.unwrap_or((0.0, 0.0))),
        }
    }

    fn local_move(&mut self, pt: (i32, i32)) -> Decision {
        if !self.settings.edge_switch || self.locked || !self.local_buttons.is_empty() {
            return Decision::default();
        }
        let Some((idx, cpos)) = self.world.locate_native(&self.local_id, pt) else {
            return Decision::default();
        };
        let n = &self.world.monitors[idx].native;
        let c = self.settings.corner_px.max(0);
        let in_y = pt.1 >= n.y + c && pt.1 < n.y + n.h - c;
        let in_x = pt.0 >= n.x + c && pt.0 < n.x + n.w - c;
        let edges = [
            (pt.0 >= n.x + n.w - 1 && in_y, Dir::Right),
            (pt.0 <= n.x && in_y, Dir::Left),
            (pt.1 >= n.y + n.h - 1 && in_x, Dir::Down),
            (pt.1 <= n.y && in_x, Dir::Up),
        ];
        for (hit, dir) in edges {
            if !hit {
                continue;
            }
            let Some((j, entry)) = self.world.neighbor(idx, cpos, dir) else { continue };
            let machine = self.world.monitors[j].machine.clone();
            if machine == self.local_id || !self.registry.is_connected(&machine) {
                continue;
            }
            let (x, y) = self.world.monitors[j].to_native(entry);
            self.registry.send(&machine, Msg::Enter { x, y });
            if self.settings.clipboard_sync {
                self.clipboard.send_to(&machine);
            }
            self.remote = Some(Remote { target: machine, monitor: j, pos: entry, return_pt: pt });
            self.remote_buttons.clear();
            return Decision { block: true, freeze_at: Some(pt), release_to: None };
        }
        Decision::default()
    }

    fn remote_move(&mut self, mut r: Remote, delta: (f64, f64)) -> Decision {
        let s = self.local_scale();
        let (j, p) = self.world.step(r.monitor, r.pos, (delta.0 * s, delta.1 * s));
        let machine = self.world.monitors[j].machine.clone();
        // Never switch machines in the middle of a drag.
        let dragging = !self.remote_buttons.is_empty();
        if machine == self.local_id {
            if dragging {
                return self.stay_on_edge(r, delta);
            }
            let to = self.world.monitors[j].to_native(p);
            self.return_local(to);
            return Decision { block: true, freeze_at: None, release_to: Some(to) };
        }
        if machine != r.target {
            if dragging || !self.registry.is_connected(&machine) {
                return self.stay_on_edge(r, delta);
            }
            self.registry.send(&r.target, Msg::Leave);
            let (x, y) = self.world.monitors[j].to_native(p);
            self.registry.send(&machine, Msg::Enter { x, y });
            if self.settings.clipboard_sync {
                self.clipboard.send_to(&machine);
            }
            r.target = machine;
        }
        r.monitor = j;
        r.pos = p;
        let (x, y) = self.world.monitors[j].to_native(p);
        self.registry.send(&r.target, Msg::MouseMove { x, y });
        self.remote = Some(r);
        Decision { block: true, ..Decision::default() }
    }

    fn stay_on_edge(&mut self, mut r: Remote, delta: (f64, f64)) -> Decision {
        let s = self.local_scale();
        let rect = self.world.monitors[r.monitor].rect;
        r.pos = rect.clamp((r.pos.0 + delta.0 * s, r.pos.1 + delta.1 * s));
        let (x, y) = self.world.monitors[r.monitor].to_native(r.pos);
        self.registry.send(&r.target, Msg::MouseMove { x, y });
        self.remote = Some(r);
        Decision { block: true, ..Decision::default() }
    }

    /// Give control back to this machine. The caller moves the real cursor to `pt`.
    fn return_local(&mut self, _pt: (i32, i32)) {
        if let Some(r) = self.remote.take() {
            // The receiver releases its held keys and replies with its clipboard.
            self.registry.send(&r.target, Msg::Leave);
        }
        self.remote_buttons.clear();
    }

    /// Return control from outside the input hook (hotkey thread, disconnects).
    pub fn force_local(&mut self) -> Option<(i32, i32)> {
        let pt = self.remote.as_ref()?.return_pt;
        self.return_local(pt);
        Some(pt)
    }

    pub fn peer_gone(&mut self, peer: &str) -> Option<(i32, i32)> {
        if self.remote.as_ref().is_some_and(|r| r.target == peer) {
            let pt = self.remote.take().unwrap().return_pt;
            self.remote_buttons.clear();
            return Some(pt);
        }
        None
    }

    pub fn on_button(&mut self, b: Button, down: bool) -> bool {
        match &self.remote {
            None => {
                if down {
                    self.local_buttons.insert(b);
                } else {
                    self.local_buttons.remove(&b);
                }
                false
            }
            Some(r) => {
                if down {
                    self.remote_buttons.insert(b);
                } else {
                    self.remote_buttons.remove(&b);
                }
                self.registry.send(&r.target, Msg::MouseButton { button: b, down });
                true
            }
        }
    }

    pub fn on_wheel(&mut self, dx: i32, dy: i32) -> bool {
        match &self.remote {
            None => false,
            Some(r) => {
                self.registry.send(&r.target, Msg::Wheel { dx, dy });
                true
            }
        }
    }

    pub fn on_key(&mut self, hid: u16, down: bool) -> Decision {
        if down {
            self.held_keys.insert(hid);
        } else {
            self.held_keys.remove(&hid);
        }
        let held = |a: u16, b: u16| self.held_keys.contains(&a) || self.held_keys.contains(&b);
        let chord = held(keymap::LCTRL, keymap::RCTRL)
            && held(keymap::LALT, keymap::RALT)
            && held(keymap::LSHIFT, keymap::RSHIFT);
        if chord && (hid == HOTKEY_RETURN || hid == HOTKEY_LOCK) {
            if down && hid == HOTKEY_LOCK {
                self.locked = !self.locked;
            }
            if down && hid == HOTKEY_RETURN {
                if let Some(pt) = self.force_local() {
                    return Decision { block: true, freeze_at: None, release_to: Some(pt) };
                }
            }
            return Decision { block: true, ..Decision::default() };
        }
        match &self.remote {
            None => Decision::default(),
            Some(r) => {
                self.registry.send(&r.target, Msg::Key { hid, down });
                Decision { block: true, ..Decision::default() }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Layout, Monitor, Placement};
    use tokio::sync::mpsc;

    fn setup() -> (Engine, mpsc::UnboundedReceiver<Msg>) {
        let registry = Arc::new(Registry::default());
        let (tx, rx) = mpsc::unbounded_channel();
        registry.register("mac", true, tx, Default::default()).unwrap();
        let settings = Settings { clipboard_sync: false, ..Settings::default() };
        let mut e = Engine::new("pc".into(), settings, registry.clone(), Clipboard::start(registry));
        let mon = |w, h| Monitor { x: 0, y: 0, w, h, primary: true, name: String::new() };
        let machines = vec![("pc".to_string(), vec![mon(1920, 1080)]), ("mac".to_string(), vec![mon(1440, 900)])];
        let mut layout = Layout::default();
        layout.placements.insert("pc".into(), Placement { x: 0.0, y: 0.0, scale: 1.0 });
        layout.placements.insert("mac".into(), Placement { x: 1920.0, y: 0.0, scale: 1.2 });
        e.set_world(World::build(&machines, &layout));
        (e, rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Msg>) -> Vec<Msg> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn crosses_to_mac_and_back() {
        let (mut e, mut rx) = setup();
        assert_eq!(e.on_move((1000, 500), None), Decision::default());
        let d = e.on_move((1919, 600), None);
        assert!(d.block && d.freeze_at == Some((1919, 600)));
        assert_eq!(e.active_target(), Some("mac"));
        assert!(matches!(drain(&mut rx)[..], [Msg::Enter { x: 0, y: 500 }]));

        // Keys and clicks now go to the Mac and are hidden from the PC.
        assert!(e.on_key(0x04, true).block);
        assert!(e.on_button(Button::Left, true));
        assert!(e.on_button(Button::Left, false));
        e.on_move((0, 0), Some((120.0, 0.0)));
        let msgs = drain(&mut rx);
        assert!(matches!(msgs.last(), Some(Msg::MouseMove { x: 100, y: 500 })), "{msgs:?}");

        // Moving back past the Mac's left edge returns to the PC.
        let d = e.on_move((0, 0), Some((-500.0, 0.0)));
        assert_eq!(d.release_to, Some((1540, 600)));
        assert_eq!(e.active_target(), None);
        assert!(matches!(drain(&mut rx).last(), Some(Msg::Leave)));
    }

    #[test]
    fn no_switch_while_dragging_or_locked() {
        let (mut e, _rx) = setup();
        e.on_button(Button::Left, true);
        assert!(!e.on_move((1919, 500), None).block);
        e.on_button(Button::Left, false);
        // Ctrl+Alt+Shift+L locks the cursor to this computer.
        for k in [keymap::LCTRL, keymap::LALT, keymap::LSHIFT] {
            e.on_key(k, true);
        }
        assert!(e.on_key(HOTKEY_LOCK, true).block);
        e.on_key(HOTKEY_LOCK, false);
        assert!(e.is_locked());
        assert!(!e.on_move((1919, 500), None).block);
    }

    #[test]
    fn corners_are_ignored() {
        let (mut e, _rx) = setup();
        e.set_settings(Settings { corner_px: 40, clipboard_sync: false, ..Settings::default() });
        assert!(!e.on_move((1919, 10), None).block);
        assert!(e.on_move((1919, 500), None).block);
    }

    #[test]
    fn hotkey_returns_control() {
        let (mut e, mut rx) = setup();
        e.on_move((1919, 300), None);
        for k in [keymap::LCTRL, keymap::LALT, keymap::LSHIFT] {
            e.on_key(k, true);
        }
        let d = e.on_key(HOTKEY_RETURN, true);
        assert_eq!(d.release_to, Some((1919, 300)));
        assert!(matches!(drain(&mut rx).last(), Some(Msg::Leave)));
    }
}
