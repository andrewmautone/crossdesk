//! Shared screen layout: every machine's monitors placed on one common canvas.
//!
//! Each machine reports its monitors in its own OS coordinates (physical pixels
//! on Windows, points on macOS). A `Placement` moves and scales that whole group
//! onto the canvas. Cursor transitions happen in canvas space.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub primary: bool,
    pub name: String,
}

impl Monitor {
    /// Append the physical size to the name. It travels inside `name` so older
    /// builds, which only display the name, keep understanding the message.
    pub fn with_physical_mm(mut self, w_mm: f64, h_mm: f64) -> Monitor {
        if w_mm >= 50.0 && h_mm >= 30.0 {
            // EDID reports the panel's native orientation; follow the current one.
            let (w_mm, h_mm) = if (self.w > self.h) == (w_mm > h_mm) {
                (w_mm, h_mm)
            } else {
                (h_mm, w_mm)
            };
            self.name = format!("{}|mm={:.0}x{:.0}", self.name, w_mm, h_mm);
        }
        self
    }

    /// Physical size in millimetres, when the OS reported one.
    pub fn physical_mm(&self) -> Option<(f64, f64)> {
        let (_, size) = self.name.rsplit_once("|mm=")?;
        let (w, h) = size.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub scale: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// Milliseconds since epoch of the last edit; newer layouts win when syncing.
    pub version: u64,
    /// Machine id that made the last edit, used to break version ties.
    pub editor: String,
    pub placements: BTreeMap<String, Placement>,
}

impl Layout {
    /// True if `other` should replace this layout.
    pub fn is_older_than(&self, other: &Layout) -> bool {
        (other.version, other.editor.as_str()) > (self.version, self.editor.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 < self.right() && p.1 >= self.y && p.1 < self.bottom()
    }
    pub fn clamp(&self, p: (f64, f64)) -> (f64, f64) {
        (
            p.0.clamp(self.x, self.right() - 0.01),
            p.1.clamp(self.y, self.bottom() - 0.01),
        )
    }
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug)]
pub struct WorldMonitor {
    pub machine: String,
    pub native: Monitor,
    pub scale: f64,
    pub rect: Rect,
}

impl WorldMonitor {
    pub fn to_native(&self, p: (f64, f64)) -> (i32, i32) {
        let p = self.rect.clamp(p);
        let nx = self.native.x as f64 + (p.0 - self.rect.x) / self.scale;
        let ny = self.native.y as f64 + (p.1 - self.rect.y) / self.scale;
        (
            (nx.floor() as i32).clamp(self.native.x, self.native.x + self.native.w - 1),
            (ny.floor() as i32).clamp(self.native.y, self.native.y + self.native.h - 1),
        )
    }
}

/// Largest gap (in canvas units) between two monitors that still counts as touching.
pub const EDGE_GAP: f64 = 48.0;

#[derive(Clone, Debug, Default)]
pub struct World {
    pub monitors: Vec<WorldMonitor>,
}

impl World {
    pub fn build(machines: &[(String, Vec<Monitor>)], layout: &Layout) -> World {
        let mut monitors = Vec::new();
        for (id, mons) in machines {
            let Some(p) = layout.placements.get(id) else {
                continue;
            };
            for m in mons {
                monitors.push(WorldMonitor {
                    machine: id.clone(),
                    native: m.clone(),
                    scale: p.scale,
                    rect: Rect {
                        x: p.x + m.x as f64 * p.scale,
                        y: p.y + m.y as f64 * p.scale,
                        w: m.w as f64 * p.scale,
                        h: m.h as f64 * p.scale,
                    },
                });
            }
        }
        World { monitors }
    }

    pub fn find(&self, p: (f64, f64)) -> Option<usize> {
        self.monitors.iter().position(|m| m.rect.contains(p))
    }

    /// Canvas point of a native point on the given machine, plus its monitor index.
    pub fn locate_native(&self, machine: &str, pt: (i32, i32)) -> Option<(usize, (f64, f64))> {
        let mut best: Option<(usize, i64)> = None;
        for (i, m) in self.monitors.iter().enumerate() {
            if m.machine != machine {
                continue;
            }
            let n = &m.native;
            let cx = pt.0.clamp(n.x, n.x + n.w - 1);
            let cy = pt.1.clamp(n.y, n.y + n.h - 1);
            let d =
                (cx - pt.0) as i64 * (cx - pt.0) as i64 + (cy - pt.1) as i64 * (cy - pt.1) as i64;
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
        let (i, _) = best?;
        let m = &self.monitors[i];
        let n = &m.native;
        let cx = pt.0.clamp(n.x, n.x + n.w - 1);
        let cy = pt.1.clamp(n.y, n.y + n.h - 1);
        Some((
            i,
            (
                m.rect.x + (cx - n.x) as f64 * m.scale + m.scale * 0.5,
                m.rect.y + (cy - n.y) as f64 * m.scale + m.scale * 0.5,
            ),
        ))
    }

    /// The monitor across `dir` from monitor `cur` at canvas point `p`, and the entry point on it.
    pub fn neighbor(&self, cur: usize, p: (f64, f64), dir: Dir) -> Option<(usize, (f64, f64))> {
        let c = self.monitors[cur].rect;
        let mut best: Option<(usize, f64, (f64, f64))> = None;
        for (i, m) in self.monitors.iter().enumerate() {
            if i == cur {
                continue;
            }
            let r = m.rect;
            let (gap, along_ok, entry) = match dir {
                Dir::Right => (
                    r.x - c.right(),
                    p.1 >= r.y && p.1 < r.bottom(),
                    (r.x + 0.5, p.1),
                ),
                Dir::Left => (
                    c.x - r.right(),
                    p.1 >= r.y && p.1 < r.bottom(),
                    (r.right() - 0.5, p.1),
                ),
                Dir::Down => (
                    r.y - c.bottom(),
                    p.0 >= r.x && p.0 < r.right(),
                    (p.0, r.y + 0.5),
                ),
                Dir::Up => (
                    c.y - r.bottom(),
                    p.0 >= r.x && p.0 < r.right(),
                    (p.0, r.bottom() - 0.5),
                ),
            };
            if !along_ok || gap < -EDGE_GAP || gap > EDGE_GAP {
                continue;
            }
            if best.is_none_or(|(_, bg, _)| gap.abs() < bg) {
                best = Some((i, gap.abs(), entry));
            }
        }
        best.map(|(i, _, e)| (i, e))
    }

    /// Move a virtual cursor by `d` from `pos` on monitor `cur`.
    pub fn step(&self, cur: usize, pos: (f64, f64), d: (f64, f64)) -> (usize, (f64, f64)) {
        let c = self.monitors[cur].rect;
        let target = (pos.0 + d.0, pos.1 + d.1);
        if c.contains(target) {
            return (cur, target);
        }
        if let Some(i) = self.find(target) {
            return (i, target);
        }
        let clamped = c.clamp(target);
        let dirs = [
            (target.0 >= c.right(), Dir::Right),
            (target.0 < c.x, Dir::Left),
            (target.1 >= c.bottom(), Dir::Down),
            (target.1 < c.y, Dir::Up),
        ];
        for (hit, dir) in dirs {
            if hit {
                if let Some(n) = self.neighbor(cur, clamped, dir) {
                    return n;
                }
            }
        }
        (cur, clamped)
    }
}

/// Snap a machine's placement so its monitors touch another machine's monitors.
pub fn snap(
    layout: &mut Layout,
    moving: &str,
    machines: &[(String, Vec<Monitor>)],
    threshold: f64,
) {
    let world = World::build(machines, layout);
    let mine: Vec<Rect> = world
        .monitors
        .iter()
        .filter(|m| m.machine == moving)
        .map(|m| m.rect)
        .collect();
    let others: Vec<Rect> = world
        .monitors
        .iter()
        .filter(|m| m.machine != moving)
        .map(|m| m.rect)
        .collect();
    let mut best_dx: Option<f64> = None;
    let mut best_dy: Option<f64> = None;
    let consider = |best: &mut Option<f64>, d: f64| {
        if d.abs() <= threshold && best.is_none_or(|b| d.abs() < b.abs()) {
            *best = Some(d);
        }
    };
    for a in &mine {
        for b in &others {
            let v_overlap = a.y < b.bottom() + threshold && b.y < a.bottom() + threshold;
            let h_overlap = a.x < b.right() + threshold && b.x < a.right() + threshold;
            if v_overlap {
                consider(&mut best_dx, b.right() - a.x);
                consider(&mut best_dx, b.x - a.right());
            }
            if h_overlap {
                consider(&mut best_dy, b.bottom() - a.y);
                consider(&mut best_dy, b.y - a.bottom());
            }
        }
    }
    // Side by side: also align top or bottom edges when close.
    if best_dx.is_some() && best_dy.is_none() {
        for a in &mine {
            for b in &others {
                consider(&mut best_dy, b.y - a.y);
                consider(&mut best_dy, b.bottom() - a.bottom());
            }
        }
    }
    if best_dy.is_some() && best_dx.is_none() {
        for a in &mine {
            for b in &others {
                consider(&mut best_dx, b.x - a.x);
                consider(&mut best_dx, b.right() - a.right());
            }
        }
    }
    if let Some(p) = layout.placements.get_mut(moving) {
        p.x += best_dx.unwrap_or(0.0);
        p.y += best_dy.unwrap_or(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: i32, h: i32) -> Monitor {
        Monitor {
            x,
            y,
            w,
            h,
            primary: x == 0 && y == 0,
            name: String::new(),
        }
    }

    fn two_machines() -> (Vec<(String, Vec<Monitor>)>, Layout) {
        let machines = vec![
            ("pc".to_string(), vec![mon(0, 0, 1920, 1080)]),
            ("mac".to_string(), vec![mon(0, 0, 1440, 900)]),
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
        (machines, layout)
    }

    #[test]
    fn crossing_right_edge_enters_mac() {
        let (machines, layout) = two_machines();
        let w = World::build(&machines, &layout);
        let (i, p) = w.step(0, (1919.0, 500.0), (5.0, 0.0));
        assert_eq!(w.monitors[i].machine, "mac");
        assert_eq!(w.monitors[i].to_native(p), (3, 416));
    }

    #[test]
    fn stays_clamped_without_neighbor() {
        let (machines, layout) = two_machines();
        let w = World::build(&machines, &layout);
        let (i, p) = w.step(1, (1925.0, 10.0), (0.0, -50.0));
        assert_eq!(i, 1);
        assert_eq!(p.1, 0.0);
    }

    #[test]
    fn neighbor_respects_small_gap() {
        let (machines, mut layout) = two_machines();
        layout.placements.get_mut("mac").unwrap().x = 1950.0;
        let w = World::build(&machines, &layout);
        let (i, _) = w.step(0, (1919.0, 100.0), (3.0, 0.0));
        assert_eq!(w.monitors[i].machine, "mac");
    }

    #[test]
    fn snap_closes_gap() {
        let (machines, mut layout) = two_machines();
        layout.placements.get_mut("mac").unwrap().x = 1930.0;
        layout.placements.get_mut("mac").unwrap().y = 12.0;
        snap(&mut layout, "mac", &machines, 40.0);
        let p = layout.placements["mac"];
        assert_eq!((p.x, p.y), (1920.0, 0.0));
    }

    #[test]
    fn locate_native_round_trips() {
        let (machines, layout) = two_machines();
        let w = World::build(&machines, &layout);
        let (i, p) = w.locate_native("mac", (100, 200)).unwrap();
        assert_eq!(w.monitors[i].to_native(p), (100, 200));
    }
}
