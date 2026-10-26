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
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Remote {
    target: String,
    monitor: usize,
    pos: (f64, f64),
    /// Where the local cursor was when it left; used if control returns abruptly.
    return_pt: (i32, i32),
}

/// The local cursor is resting on an edge that leads to another machine.
#[derive(Clone, Debug)]
struct EdgeHit {
    dir: Dir,
    monitor: usize,
    entry: (f64, f64),
    pt: (i32, i32),
    /// When the cursor started resting on this edge.
    since: Instant,
}

/// How hard the user has pushed against an edge so far.
#[derive(Clone, Copy, Debug)]
struct Push {
    dir: Dir,
    amount: f64,
    last: Instant,
}

/// A pause this long while pushing starts the count over.
const PUSH_RESET: Duration = Duration::from_millis(450);

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
    edge: Option<EdgeHit>,
    push: Option<Push>,
}

fn on_same_edge(e: &EdgeHit, pt: (i32, i32)) -> bool {
    match e.dir {
        Dir::Right | Dir::Left => pt.0 == e.pt.0,
        Dir::Up | Dir::Down => pt.1 == e.pt.1,
    }
}

const HOTKEY_RETURN: u16 = 0x14; // Q
const HOTKEY_LOCK: u16 = 0x0F; // L

impl Engine {
    pub fn new(
        local_id: String,
        settings: Settings,
        registry: Arc<Registry>,
        clipboard: Clipboard,
    ) -> Engine {
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
            edge: None,
            push: None,
        }
    }

    /// Returns where to put the local cursor if control had to come back.
    pub fn set_world(&mut self, world: World) -> Option<(i32, i32)> {
        self.world = world;
        let r = self.remote.as_ref()?;
        // Monitor indexes change with the world; re-find the cursor or give control back.
        let world = &self.world;
        match world
            .find(r.pos)
            .filter(|&i| world.monitors[i].machine == r.target)
        {
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

    /// A machine's main screen on the canvas.
    fn main_monitor(&self, machine: &str) -> Option<&crate::layout::WorldMonitor> {
        let mons = self.world.monitors.iter().filter(|m| m.machine == machine);
        mons.clone()
            .find(|m| m.native.primary)
            .or_else(|| mons.max_by(|a, b| a.rect.h.total_cmp(&b.rect.h)))
    }

    /// Canvas units per local pixel of movement while controlling `target`.
    fn move_scale(&self, target: &str) -> f64 {
        let local_scale = self.local_scale();
        let mut s = local_scale;
        if self.settings.auto_speed {
            if let (Some(ours), Some(theirs)) =
                (self.main_monitor(&self.local_id), self.main_monitor(target))
            {
                s = match (ours.native.physical_mm(), theirs.native.physical_mm()) {
                    // Same physical speed: a hand movement that moves the cursor 1 cm
                    // on this screen moves it 1 cm on theirs.
                    (Some((_, our_mm)), Some((_, their_mm))) => {
                        let ours_mm_per_px = our_mm / ours.native.h.max(1) as f64;
                        let theirs_mm_per_unit = their_mm / theirs.native.h.max(1) as f64;
                        theirs.scale * ours_mm_per_px / theirs_mm_per_unit
                    }
                    // Unknown sizes: crossing their main screen takes the same movement as ours.
                    _ => local_scale * theirs.rect.h / ours.rect.h.max(1.0),
                }
                .clamp(local_scale * 0.05, local_scale * 20.0);
            }
        }
        let user = self
            .settings
            .speed
            .get(target)
            .copied()
            .unwrap_or(1.0)
            .clamp(0.1, 10.0);
        s * user as f64
    }

    fn local_scale(&self) -> f64 {
        self.world
            .monitors
            .iter()
            .find(|m| m.machine == self.local_id)
            .map_or(1.0, |m| m.scale)
    }

    pub fn on_move(&mut self, pt: (i32, i32), delta: Option<(f64, f64)>) -> Decision {
        match self.remote.clone() {
            None => self.local_move(pt),
            Some(r) => self.remote_move(r, delta.unwrap_or((0.0, 0.0))),
        }
    }

    fn resistance(&self) -> f64 {
        self.settings.edge_resistance.max(0) as f64
    }

    /// Add `outward` pixels of push toward `dir`; true once it beats the resistance.
    fn pushed_through(&mut self, dir: Dir, outward: f64) -> bool {
        let needed = self.resistance();
        if needed <= 0.0 {
            return true;
        }
        let now = Instant::now();
        let p = self.push.get_or_insert(Push {
            dir,
            amount: 0.0,
            last: now,
        });
        if p.dir != dir || now.duration_since(p.last) > PUSH_RESET {
            *p = Push {
                dir,
                amount: 0.0,
                last: now,
            };
        }
        if outward > 0.0 {
            p.amount += outward;
            p.last = now;
        } else {
            p.amount = (p.amount + outward).max(0.0);
        }
        p.amount >= needed
    }

    fn enter_remote(&mut self, j: usize, entry: (f64, f64), pt: (i32, i32)) -> Decision {
        let machine = self.world.monitors[j].machine.clone();
        let (x, y) = self.world.monitors[j].to_native(entry);
        self.registry.send(&machine, Msg::Enter { x, y });
        if self.settings.clipboard_sync {
            self.clipboard.send_to(&machine);
        }
        self.remote = Some(Remote {
            target: machine,
            monitor: j,
            pos: entry,
            return_pt: pt,
        });
        self.remote_buttons.clear();
        self.edge = None;
        self.push = None;
        Decision {
            block: true,
            freeze_at: Some(pt),
            release_to: None,
        }
    }

    /// Raw mouse movement, reported even when the cursor is pinned against an edge
    /// (Raw Input on Windows, event deltas on macOS). Only used to measure the push.
    /// Fallback when raw movement is unavailable: keeping the cursor moving against
    /// the edge for a while (3 ms per pixel of resistance) counts as the push.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn on_edge_dwell(&mut self) -> Decision {
        if self.remote.is_some() {
            return Decision::default();
        }
        let Some(hit) = self.edge.clone() else {
            return Decision::default();
        };
        let needed = Duration::from_millis((self.resistance() * 3.0) as u64);
        if hit.since.elapsed() < needed {
            return Decision::default();
        }
        self.enter_remote(hit.monitor, hit.entry, hit.pt)
    }

    /// How far the current push has got, 0..1.
    pub fn push_progress(&self) -> f64 {
        match self.push {
            Some(p) if p.last.elapsed() <= PUSH_RESET && self.resistance() > 0.0 => {
                (p.amount / self.resistance()).min(1.0)
            }
            _ => 0.0,
        }
    }

    /// Where the cursor rests against an edge while a push is building up.
    pub fn push_point(&self) -> Option<(i32, i32)> {
        self.edge.as_ref().map(|e| e.pt)
    }

    pub fn on_push_delta(&mut self, dx: f64, dy: f64) -> Decision {
        if self.remote.is_some() {
            return Decision::default();
        }
        let Some(hit) = self.edge.clone() else {
            return Decision::default();
        };
        let outward = match hit.dir {
            Dir::Right => dx,
            Dir::Left => -dx,
            Dir::Down => dy,
            Dir::Up => -dy,
        };
        if outward == 0.0 || !self.pushed_through(hit.dir, outward) {
            return Decision::default();
        }
        self.enter_remote(hit.monitor, hit.entry, hit.pt)
    }

    fn local_move(&mut self, pt: (i32, i32)) -> Decision {
        let d = self.local_edge(pt);
        // Moving off the edge line forgets the push.
        if self.edge.as_ref().is_some_and(|e| !on_same_edge(e, pt)) {
            self.edge = None;
            self.push = None;
        }
        d
    }

    fn local_edge(&mut self, pt: (i32, i32)) -> Decision {
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
            let Some((j, entry)) = self.world.neighbor(idx, cpos, dir) else {
                continue;
            };
            let machine = self.world.monitors[j].machine.clone();
            if machine == self.local_id || !self.registry.is_connected(&machine) {
                continue;
            }
            if self.resistance() <= 0.0 {
                return self.enter_remote(j, entry, pt);
            }
            // Resting on the edge: wait for enough push (see on_push_delta).
            let since = match &self.edge {
                Some(e) if e.dir == dir => e.since,
                _ => Instant::now(),
            };
            self.edge = Some(EdgeHit {
                dir,
                monitor: j,
                entry,
                pt,
                since,
            });
            return Decision::default();
        }
        Decision::default()
    }

    fn remote_move(&mut self, mut r: Remote, delta: (f64, f64)) -> Decision {
        let s = self.move_scale(&r.target);
        let d = (delta.0 * s, delta.1 * s);
        let (j, p) = self.world.step(r.monitor, r.pos, d);
        let machine = self.world.monitors[j].machine.clone();
        // Never switch machines in the middle of a drag.
        let dragging = !self.remote_buttons.is_empty();
        if machine != r.target {
            // Leaving this machine also takes a push against the edge.
            let c = self.world.monitors[r.monitor].rect;
            let (dir, outward) = if p.0 >= c.right() {
                (Dir::Right, d.0)
            } else if p.0 < c.x {
                (Dir::Left, -d.0)
            } else if p.1 >= c.bottom() {
                (Dir::Down, d.1)
            } else {
                (Dir::Up, -d.1)
            };
            if !dragging && !self.pushed_through(dir, outward) {
                return self.stay_on_edge(r, delta);
            }
        } else {
            self.push = None;
        }
        if machine == self.local_id {
            if dragging {
                return self.stay_on_edge(r, delta);
            }
            let to = self.world.monitors[j].to_native(p);
            self.return_local(to);
            return Decision {
                block: true,
                freeze_at: None,
                release_to: Some(to),
            };
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
        Decision {
            block: true,
            ..Decision::default()
        }
    }

    fn stay_on_edge(&mut self, mut r: Remote, delta: (f64, f64)) -> Decision {
        let s = self.move_scale(&r.target);
        let rect = self.world.monitors[r.monitor].rect;
        r.pos = rect.clamp((r.pos.0 + delta.0 * s, r.pos.1 + delta.1 * s));
        let (x, y) = self.world.monitors[r.monitor].to_native(r.pos);
        self.registry.send(&r.target, Msg::MouseMove { x, y });
        self.remote = Some(r);
        Decision {
            block: true,
            ..Decision::default()
        }
    }

    /// Give control back to this machine. The caller moves the real cursor to `pt`.
    fn return_local(&mut self, _pt: (i32, i32)) {
        self.push = None;
        self.edge = None;
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
                self.registry
                    .send(&r.target, Msg::MouseButton { button: b, down });
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
                    return Decision {
                        block: true,
                        freeze_at: None,
                        release_to: Some(pt),
                    };
                }
            }
            return Decision {
                block: true,
                ..Decision::default()
            };
        }
        match &self.remote {
            None => Decision::default(),
            Some(r) => {
                self.registry.send(&r.target, Msg::Key { hid, down });
                Decision {
                    block: true,
                    ..Decision::default()
                }
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
        registry
            .register("mac", true, tx, Default::default())
            .unwrap();
        let settings = Settings {
            clipboard_sync: false,
            edge_resistance: 0,
            ..Settings::default()
        };
        let mut e = Engine::new(
            "pc".into(),
            settings,
            registry.clone(),
            Clipboard::start(registry),
        );
        let mon = |w, h| Monitor {
            x: 0,
            y: 0,
            w,
            h,
            primary: true,
            name: String::new(),
        };
        let machines = vec![
            ("pc".to_string(), vec![mon(1920, 1080)]),
            ("mac".to_string(), vec![mon(1440, 900)]),
        ];
        let mut layout = Layout::default();
        layout.placements.insert(
            "pc".into(),
            Placement {
                x: 0.0,
                y: 0.0,
                scale: 1.0,
            },
        );
        layout.placements.insert(
            "mac".into(),
            Placement {
                x: 1920.0,
                y: 0.0,
                scale: 1.2,
            },
        );
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
        assert!(
            matches!(msgs.last(), Some(Msg::MouseMove { x: 100, y: 500 })),
            "{msgs:?}"
        );

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
        e.set_settings(Settings {
            corner_px: 40,
            clipboard_sync: false,
            edge_resistance: 0,
            ..Settings::default()
        });
        assert!(!e.on_move((1919, 10), None).block);
        assert!(e.on_move((1919, 500), None).block);
    }

    #[test]
    fn crossing_needs_a_push_when_resistance_is_set() {
        let (mut e, mut rx) = setup();
        e.set_settings(Settings {
            clipboard_sync: false,
            edge_resistance: 100,
            ..Settings::default()
        });
        // Touching the edge alone does not cross.
        assert!(!e.on_move((1919, 600), None).block);
        assert_eq!(e.active_target(), None);
        // Pushing along the edge or back inward does not count.
        assert!(!e.on_push_delta(0.0, 50.0).block);
        assert!(!e.on_push_delta(-30.0, 0.0).block);
        assert!(!e.on_push_delta(60.0, 0.0).block);
        let d = e.on_push_delta(45.0, 0.0);
        assert!(d.block && d.freeze_at == Some((1919, 600)));
        assert_eq!(e.active_target(), Some("mac"));
        assert!(matches!(drain(&mut rx).first(), Some(Msg::Enter { .. })));

        // Coming back also needs the push: one small nudge stays on the Mac.
        let d = e.on_move((0, 0), Some((-50.0, 0.0)));
        assert!(d.release_to.is_none());
        assert_eq!(e.active_target(), Some("mac"));
        let d = e.on_move((0, 0), Some((-60.0, 0.0)));
        assert!(d.release_to.is_some());
        assert_eq!(e.active_target(), None);
    }

    #[test]
    fn dwelling_on_the_edge_crosses_without_raw_input() {
        let (mut e, _rx) = setup();
        e.set_settings(Settings {
            clipboard_sync: false,
            edge_resistance: 10,
            ..Settings::default()
        });
        e.on_move((1919, 600), None);
        assert!(!e.on_edge_dwell().block);
        std::thread::sleep(Duration::from_millis(40));
        e.on_move((1919, 601), None);
        assert!(e.on_edge_dwell().block);
        assert_eq!(e.active_target(), Some("mac"));
    }

    #[test]
    fn leaving_the_edge_resets_the_push() {
        let (mut e, _rx) = setup();
        e.set_settings(Settings {
            clipboard_sync: false,
            edge_resistance: 100,
            ..Settings::default()
        });
        e.on_move((1919, 600), None);
        e.on_push_delta(90.0, 0.0);
        e.on_move((1800, 600), None);
        e.on_move((1919, 600), None);
        assert!(!e.on_push_delta(20.0, 0.0).block);
        assert_eq!(e.active_target(), None);
    }

    #[test]
    fn auto_speed_matches_screen_sizes_and_user_speed_multiplies() {
        let (mut e, mut rx) = setup();
        // Make the Mac half as tall on the map: without auto speed it would feel twice as fast.
        let mon = |w, h| Monitor {
            x: 0,
            y: 0,
            w,
            h,
            primary: true,
            name: String::new(),
        };
        let machines = vec![
            ("pc".to_string(), vec![mon(1920, 1080)]),
            ("mac".to_string(), vec![mon(1440, 900)]),
        ];
        let mut layout = Layout::default();
        layout.placements.insert(
            "pc".into(),
            Placement {
                x: 0.0,
                y: 0.0,
                scale: 1.0,
            },
        );
        layout.placements.insert(
            "mac".into(),
            Placement {
                x: 1920.0,
                y: 0.0,
                scale: 0.6,
            },
        );
        e.set_world(World::build(&machines, &layout));
        e.on_move((1919, 100), None);
        drain(&mut rx);
        // 108 px (a tenth of the PC screen) moves a tenth of the Mac screen (90 of 900).
        e.on_move((0, 0), Some((0.0, 108.0)));
        let Some(Msg::MouseMove { y: y1, .. }) = drain(&mut rx).pop() else {
            panic!()
        };
        let mut s = Settings {
            clipboard_sync: false,
            edge_resistance: 0,
            ..Settings::default()
        };
        s.speed.insert("mac".into(), 2.0);
        e.set_settings(s);
        e.on_move((0, 0), Some((0.0, 108.0)));
        let Some(Msg::MouseMove { y: y2, .. }) = drain(&mut rx).pop() else {
            panic!()
        };
        // Entered at Mac y 167 (100 px on the PC / 0.6); +90, then +180 at 2x.
        assert!((y1 - 257).abs() <= 1, "y1={y1}");
        assert!((y2 - y1 - 180).abs() <= 1, "y1={y1} y2={y2}");
    }

    #[test]
    fn auto_speed_ignores_the_map_scale_of_this_computer() {
        let (mut e, mut rx) = setup();
        // The Mac placed the layout: PC drawn at 0.45, Mac at 1.
        let mon = |w, h| Monitor {
            x: 0,
            y: 0,
            w,
            h,
            primary: true,
            name: String::new(),
        };
        let machines = vec![
            ("pc".to_string(), vec![mon(3840, 2160)]),
            ("mac".to_string(), vec![mon(1512, 982)]),
        ];
        let mut layout = Layout::default();
        layout.placements.insert(
            "pc".into(),
            Placement {
                x: 0.0,
                y: 0.0,
                scale: 0.4546,
            },
        );
        layout.placements.insert(
            "mac".into(),
            Placement {
                x: 1745.7,
                y: 0.0,
                scale: 1.0,
            },
        );
        e.set_world(World::build(&machines, &layout));
        e.on_move((3839, 100), None);
        drain(&mut rx);
        // A tenth of the PC screen (216 px) moves a tenth of the Mac screen (~98 pt).
        e.on_move((0, 0), Some((0.0, 216.0)));
        let Some(Msg::MouseMove { y, .. }) = drain(&mut rx).pop() else {
            panic!()
        };
        assert!((y - (45 + 98)).abs() <= 2, "y={y}");
    }

    #[test]
    fn auto_speed_matches_physical_size_when_known() {
        let (mut e, mut rx) = setup();
        // 27" 4K (336 mm tall) and a 14" laptop (196 mm tall, 982 pt).
        let pc = Monitor {
            x: 0,
            y: 0,
            w: 3840,
            h: 2160,
            primary: true,
            name: String::new(),
        }
        .with_physical_mm(597.0, 336.0);
        let mac = Monitor {
            x: 0,
            y: 0,
            w: 1512,
            h: 982,
            primary: true,
            name: String::new(),
        }
        .with_physical_mm(302.0, 196.0);
        let machines = vec![("pc".to_string(), vec![pc]), ("mac".to_string(), vec![mac])];
        let mut layout = Layout::default();
        layout.placements.insert(
            "pc".into(),
            Placement {
                x: 0.0,
                y: 0.0,
                scale: 1.0,
            },
        );
        layout.placements.insert(
            "mac".into(),
            Placement {
                x: 3840.0,
                y: 0.0,
                scale: 2.2,
            },
        );
        e.set_world(World::build(&machines, &layout));
        e.on_move((3839, 100), None);
        drain(&mut rx);
        // 216 px on the PC = 33.6 mm, which is 168 pt on the Mac (5.01 pt/mm).
        e.on_move((0, 0), Some((0.0, 216.0)));
        let Some(Msg::MouseMove { y, .. }) = drain(&mut rx).pop() else {
            panic!()
        };
        assert!((y - (45 + 168)).abs() <= 2, "y={y}");
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
