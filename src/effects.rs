//! Liquid ripple shown where the cursor leaves or enters a screen.
//!
//! When it can, the effect takes a snapshot of the small patch of screen
//! around the cursor and redraws it refracted, as if the screen were water:
//! a lens swelling while the user pushes, then ripples. Without a snapshot
//! (setting off, or no Screen Recording permission on macOS) it draws a
//! stylized glowing ripple instead.
//!
//! Frames are rendered on the CPU into a small premultiplied-RGBA buffer and
//! shown in a transparent, click-through overlay window (see `overlay`). The
//! overlay only exists while something is animating, so nothing sits on top of
//! full-screen games the rest of the time.

use crate::layout::{Dir, Monitor};
use crate::overlay::Overlay;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// The cursor left this screen here.
    Leave,
    /// The cursor arrived on this screen here.
    Arrive,
}

enum Cmd {
    Burst((i32, i32), Kind),
    Push((i32, i32), f32),
    Enabled(bool),
    Distort(bool),
}

/// The screen patch under the overlay, opaque RGBA, top-left at `center - size/2`.
pub struct Snapshot {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

/// Logical size of the overlay (pixels at 100% scale / macOS points).
const SIZE: f32 = 340.0;
const FRAME: Duration = Duration::from_micros(16_667);
const BURST: Duration = Duration::from_millis(520);
const PUSH_FADE: Duration = Duration::from_millis(180);
const INDIGO: [f32; 3] = [91.0, 75.0, 240.0];
const CYAN: [f32; 3] = [14.0, 190.0, 220.0];

static EFFECTS: OnceLock<Sender<Cmd>> = OnceLock::new();

/// Start the animation thread. `monitors` returns this computer's screens.
pub fn start(monitors: impl Fn() -> Vec<Monitor> + Send + 'static, enabled: bool, distort: bool) {
    let (tx, rx) = channel::<Cmd>();
    if EFFECTS.set(tx).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("effects".into())
        .spawn(move || {
            let mut enabled = enabled;
            let mut distort = distort;
            let mut snap: Option<((i32, i32), Snapshot)> = None;
            let mut overlay: Option<Overlay> = None;
            let mut burst: Option<(Instant, (i32, i32), Kind)> = None;
            let mut push: Option<(Instant, (i32, i32), f32)> = None;
            let mut shown = false;
            let born = Instant::now();
            // When the next frame is due; rendering time counts toward the 60 fps budget.
            let mut next_frame = Instant::now();
            loop {
                let active = burst.is_some() || push.is_some();
                let wait = if active {
                    next_frame.saturating_duration_since(Instant::now())
                } else {
                    Duration::from_secs(1)
                };
                // Drain everything queued, keeping only the newest push.
                let mut first = match rx.recv_timeout(wait) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                while let Some(cmd) = first.take().or_else(|| rx.try_recv().ok()) {
                    match cmd {
                        Cmd::Enabled(on) => enabled = on,
                        Cmd::Distort(on) => distort = on,
                        _ if !enabled => {}
                        Cmd::Burst(pt, kind) => {
                            burst = Some((Instant::now(), pt, kind));
                            push = None;
                        }
                        Cmd::Push(pt, p) => {
                            if burst.is_none() {
                                push = Some((Instant::now(), pt, p));
                            }
                        }
                    }
                }
                // Pushes arrive with every mouse event; draw no faster than the frame rate.
                if Instant::now() < next_frame {
                    continue;
                }
                if burst.is_some_and(|(t, _, _)| t.elapsed() >= BURST) {
                    burst = None;
                }
                if push.is_some_and(|(t, _, _)| t.elapsed() >= PUSH_FADE) {
                    push = None;
                }
                let Some(center) = burst.map(|b| b.1).or(push.map(|p| p.1)) else {
                    if shown {
                        if let Some(o) = overlay.as_mut() {
                            o.hide();
                        }
                        shown = false;
                    }
                    if let Some(o) = overlay.as_mut() {
                        o.pump();
                    }
                    continue;
                };
                next_frame = Instant::now() + FRAME;
                let mons = monitors();
                let (dir, scale) = edge_of(&mons, center);
                let px_scale = Overlay::pixel_scale(scale);
                let side = (SIZE * px_scale).round() as usize;
                let frame = Frame {
                    time: born.elapsed().as_secs_f32(),
                    burst: burst
                        .map(|(t, _, k)| (t.elapsed().as_secs_f32() / BURST.as_secs_f32(), k)),
                    push: push.map(|(t, _, p)| {
                        p * (1.0 - t.elapsed().as_secs_f32() / PUSH_FADE.as_secs_f32()).max(0.0)
                    }),
                    dir,
                };
                if overlay.is_none() {
                    overlay = Overlay::new();
                }
                // Liquid Glass refracts the real screen by itself, no snapshot needed.
                if distort && Overlay::glass_available() {
                    let (radius, alpha) = glass_drop(&frame);
                    if let Some(o) = overlay.as_mut() {
                        o.present_glass(center, SIZE, radius, alpha);
                        o.pump();
                        shown = true;
                    }
                    continue;
                }
                // Grab the screen under the effect when it starts somewhere new.
                if !distort {
                    snap = None;
                } else if !shown || snap.as_ref().map(|s| s.0) != Some(center) {
                    snap = Overlay::capture(center, side, SIZE).map(|s| (center, s));
                }
                let rgba = match &snap {
                    Some((_, s)) => render_distort(side, px_scale, &frame, s),
                    None => render(side, px_scale, &frame),
                };
                if let Some(o) = overlay.as_mut() {
                    o.present(&rgba, side, center, SIZE);
                    o.pump();
                    shown = true;
                }
            }
        })
        .expect("spawn effects thread");
}

fn send(cmd: Cmd) {
    if let Some(tx) = EFFECTS.get() {
        let _ = tx.send(cmd);
    }
}

pub fn burst(pt: (i32, i32), kind: Kind) {
    send(Cmd::Burst(pt, kind));
}

/// The cursor is being pushed against the edge at `pt`; `progress` is 0..1.
pub fn push(pt: (i32, i32), progress: f64) {
    send(Cmd::Push(pt, progress as f32));
}

pub fn set_distort(on: bool) {
    send(Cmd::Distort(on));
}

pub fn set_enabled(on: bool) {
    send(Cmd::Enabled(on));
}

/// Which edge of its monitor `pt` sits on, and that monitor's size relative to 1080p.
fn edge_of(mons: &[Monitor], pt: (i32, i32)) -> (Dir, f32) {
    let m = mons
        .iter()
        .find(|m| pt.0 >= m.x && pt.0 < m.x + m.w && pt.1 >= m.y && pt.1 < m.y + m.h)
        .or_else(|| mons.first());
    let Some(m) = m else { return (Dir::Right, 1.0) };
    let dists = [
        ((m.x + m.w - 1 - pt.0).abs(), Dir::Right),
        ((pt.0 - m.x).abs(), Dir::Left),
        ((m.y + m.h - 1 - pt.1).abs(), Dir::Down),
        ((pt.1 - m.y).abs(), Dir::Up),
    ];
    let dir = dists.iter().min_by_key(|d| d.0).map_or(Dir::Right, |d| d.1);
    (dir, m.h as f32 / 1080.0)
}

pub struct Frame {
    /// Seconds since start, drives the wobble.
    pub time: f32,
    /// Burst progress 0..1 and its kind.
    pub burst: Option<(f32, Kind)>,
    /// Push bubble size 0..1.
    pub push: Option<f32>,
    /// Edge the effect sits on (the bubble leans outward).
    pub dir: Dir,
}

/// Radius (logical px) and opacity of the Liquid Glass drop for this frame:
/// it swells while pushing, pops outward on leave and lands and melts on arrive.
pub fn glass_drop(f: &Frame) -> (f32, f32) {
    let wobble = 1.0 + 0.05 * (f.time * 9.0).sin();
    if let Some((t, kind)) = f.burst {
        let ease = 1.0 - (1.0 - t).powi(3);
        return match kind {
            Kind::Leave => ((48.0 + 60.0 * ease) * wobble, (1.0 - t).powf(1.5)),
            Kind::Arrive => (56.0 * (1.0 - ease) * wobble, 1.0 - t),
        };
    }
    match f.push {
        Some(p) => ((14.0 + 50.0 * p) * wobble, (p * 3.0).min(1.0)),
        None => (0.0, 0.0),
    }
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Render one frame as premultiplied RGBA, `side` x `side` pixels, centered on the cursor point.
pub fn render(side: usize, px_scale: f32, f: &Frame) -> Vec<u8> {
    let mut out = vec![0u8; side * side * 4];
    let c = side as f32 / 2.0;
    // Unit vector pointing out of the screen, so the bubble bulges past the edge.
    let out_dir = match f.dir {
        Dir::Right => (1.0, 0.0),
        Dir::Left => (-1.0, 0.0),
        Dir::Down => (0.0, 1.0),
        Dir::Up => (0.0, -1.0),
    };
    par_rows(&mut out, side, |y, row| {
        for x in 0..side {
            let px = (x as f32 + 0.5 - c) / px_scale;
            let py = (y as f32 + 0.5 - c) / px_scale;
            let d = (px * px + py * py).sqrt();
            let ang = py.atan2(px);
            let wobble =
                (ang * 5.0 + f.time * 9.0).sin() * 0.6 + (ang * 3.0 - f.time * 6.0).sin() * 0.4;
            let tint = 0.5 + 0.5 * (ang + f.time * 2.0).sin();
            let mut acc = [0.0f32; 3];
            let mut alpha = 0.0f32;
            let mut add = |color: [f32; 3], a: f32| {
                let a = a.clamp(0.0, 1.0);
                acc[0] += color[0] * a;
                acc[1] += color[1] * a;
                acc[2] += color[2] * a;
                alpha += a;
            };

            if let Some(p) = f.push {
                // A drop of liquid swelling on the edge.
                let lean = (px * out_dir.0 + py * out_dir.1) * 0.25;
                let r = (10.0 + 48.0 * p) * (1.0 + 0.07 * wobble * p) + lean;
                let body = 1.0 - smooth(r - 2.0, r + 1.0, d);
                let rim = (-((d - r) / 2.2).powi(2)).exp();
                let glow = (1.0 - (d / r.max(1.0))).max(0.0).powf(0.6);
                let col = mix(INDIGO, CYAN, tint);
                add(col, body * (0.16 + 0.30 * p) * (0.6 + 0.4 * glow));
                add(mix(col, [255.0; 3], 0.45), rim * (0.35 + 0.55 * p));
            }

            if let Some((t, kind)) = f.burst {
                let reach = if kind == Kind::Leave { 165.0 } else { 140.0 };
                for i in 0..3 {
                    let ti = ((t - i as f32 * 0.13) / 0.74).clamp(0.0, 1.0);
                    if ti <= 0.0 || ti >= 1.0 {
                        continue;
                    }
                    let eased = 1.0 - (1.0 - ti).powi(3);
                    let r = eased * reach * (1.0 + 0.045 * wobble);
                    let width = 1.6 + 8.0 * (1.0 - ti);
                    let a = (1.0 - ti).powf(1.5) * (0.95 - i as f32 * 0.2);
                    let ring = (-((d - r) / width).powi(2)).exp();
                    let col = mix(INDIGO, CYAN, (tint + i as f32 * 0.3) % 1.0);
                    add(col, ring * a);
                    add([255.0; 3], ring * a * 0.35 * (1.0 - ti));
                }
                // The drop itself: pops on leave, lands as a splash on arrive.
                let core_r = if kind == Kind::Leave {
                    52.0 * (1.0 - t).powi(2)
                } else {
                    30.0 * (1.0 - t)
                };
                let core = 1.0 - smooth(core_r - 2.0, core_r + 1.5, d * (1.0 + 0.1 * wobble));
                add(mix(INDIGO, CYAN, tint), core * 0.45 * (1.0 - t));
                if kind == Kind::Arrive {
                    let flash = (-(d / 22.0).powi(2)).exp() * (1.0 - t * 3.0).max(0.0);
                    add([255.0; 3], flash * 0.7);
                }
            }

            if alpha <= 0.002 {
                continue;
            }
            let a = alpha.min(1.0);
            // Premultiplied: color channels already carry alpha; normalize if they overflowed.
            let k = if alpha > 1.0 { 1.0 / alpha } else { 1.0 };
            let o = x * 4;
            row[o] = (acc[0] * k).min(255.0 * a) as u8;
            row[o + 1] = (acc[1] * k).min(255.0 * a) as u8;
            row[o + 2] = (acc[2] * k).min(255.0 * a) as u8;
            row[o + 3] = (a * 255.0) as u8;
        }
    });
    out
}

/// Fill `out` (`side` rows of RGBA) on every core.
fn par_rows(out: &mut [u8], side: usize, row_fn: impl Fn(usize, &mut [u8]) + Sync) {
    use rayon::prelude::*;
    out.par_chunks_mut(side * 4)
        .enumerate()
        .for_each(|(y, row)| row_fn(y, row));
}

fn sample(s: &Snapshot, side: usize, x: f32, y: f32, ch: usize) -> f32 {
    let k = s.w as f32 / side as f32;
    let fx = (x * k - 0.5).clamp(0.0, s.w as f32 - 1.001);
    let fy = (y * k - 0.5).clamp(0.0, s.h as f32 - 1.001);
    let (x0, y0) = (fx as usize, fy as usize);
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let at = |x: usize, y: usize| s.rgba[(y * s.w + x) * 4 + ch] as f32;
    let top = at(x0, y0) * (1.0 - tx) + at(x0 + 1, y0) * tx;
    let bottom = at(x0, y0 + 1) * (1.0 - tx) + at(x0 + 1, y0 + 1) * tx;
    top * (1.0 - ty) + bottom * ty
}

/// Refract the snapshot through a liquid surface: a lens while pushing, ripples on burst.
pub fn render_distort(side: usize, px_scale: f32, f: &Frame, snap: &Snapshot) -> Vec<u8> {
    let mut out = vec![0u8; side * side * 4];
    let c = side as f32 / 2.0;
    // Half of the patch lies past the screen edge (black); never sample from there.
    let clamp_on_screen = |x: f32, y: f32| -> (f32, f32) {
        match f.dir {
            Dir::Right => (x.min(c - 1.0), y),
            Dir::Left => (x.max(c + 1.0), y),
            Dir::Down => (x, y.min(c - 1.0)),
            Dir::Up => (x, y.max(c + 1.0)),
        }
    };
    let light = (-0.6f32, -0.8f32);
    par_rows(&mut out, side, |y, row| {
        for x in 0..side {
            let px = (x as f32 + 0.5 - c) / px_scale;
            let py = (y as f32 + 0.5 - c) / px_scale;
            let d = (px * px + py * py).sqrt();
            let (ux, uy) = if d > 1e-3 {
                (px / d, py / d)
            } else {
                (0.0, 0.0)
            };
            let ang = py.atan2(px);
            let wobble =
                (ang * 5.0 + f.time * 9.0).sin() * 0.6 + (ang * 3.0 - f.time * 6.0).sin() * 0.4;
            let facing = ux * light.0 + uy * light.1;
            // Radial displacement of the sample point (logical px), coverage and lighting.
            let mut disp = 0.0f32;
            let mut mask = 0.0f32;
            let mut glint = 0.0f32;
            let mut shade = 0.0f32;

            if let Some(p) = f.push {
                // A drop of water on the glass: magnifies what is under it.
                let r = (12.0 + 54.0 * p) * (1.0 + 0.06 * wobble * p);
                if d < r + 6.0 {
                    let q = (d / r).min(1.0);
                    disp -= d * 0.45 * p * (1.0 - q * q);
                    let u = (d - r) / 2.4;
                    let e = (-u * u).exp();
                    disp -= u * e * 5.0 * p;
                    mask = mask.max(1.0 - smooth(r + 0.5, r + 4.0, d));
                    glint += e * p * (0.25 + 0.55 * facing.max(0.0));
                    shade -= 0.10 * p * q.powi(6);
                }
            }

            if let Some((t, kind)) = f.burst {
                let reach = if kind == Kind::Leave { 170.0 } else { 145.0 };
                for i in 0..3 {
                    let ti = ((t - i as f32 * 0.13) / 0.74).clamp(0.0, 1.0);
                    if ti <= 0.0 || ti >= 1.0 {
                        continue;
                    }
                    let eased = 1.0 - (1.0 - ti).powi(3);
                    let r = eased * reach * (1.0 + 0.035 * wobble);
                    let w = 3.0 + 11.0 * (1.0 - ti);
                    let amp = (if kind == Kind::Leave { 10.0 } else { 8.0 })
                        * (1.0 - ti).powf(1.3)
                        * (1.0 - 0.25 * i as f32);
                    let u = (d - r) / w;
                    let e = (-u * u).exp();
                    let slope = -2.0 * u * e;
                    disp += amp * slope;
                    mask = mask.max((e * 2.2 * (1.0 - ti)).min(1.0));
                    glint += (slope * facing).max(0.0) * amp * 0.035;
                    shade += slope * facing * amp * 0.012;
                }
                // The drop: pops on leave, a small splash lens on arrive.
                let rc = if kind == Kind::Leave {
                    44.0 * (1.0 - t).powi(2)
                } else {
                    26.0 * (1.0 - t)
                };
                if rc > 1.0 && d < rc + 4.0 {
                    let q = (d / rc).min(1.0);
                    disp -= d * 0.35 * (1.0 - t) * (1.0 - q * q);
                    mask = mask.max(1.0 - smooth(rc, rc + 3.0, d));
                }
            }

            if mask < 0.004 {
                continue;
            }
            // Sample along the radius; red bends a bit more than blue (dispersion).
            let at = |k: f32| {
                let sx = c + (px + ux * disp * k) * px_scale;
                let sy = c + (py + uy * disp * k) * px_scale;
                clamp_on_screen(sx, sy)
            };
            let (rx, ry) = at(1.06);
            let (gx, gy) = at(1.0);
            let (bx, by) = at(0.94);
            let col = [
                sample(snap, side, rx, ry, 0),
                sample(snap, side, gx, gy, 1),
                sample(snap, side, bx, by, 2),
            ];
            let a = mask.min(1.0);
            let o = x * 4;
            for ch in 0..3 {
                let v = (col[ch] * (1.0 + shade) + glint * 255.0).clamp(0.0, 255.0);
                row[o + ch] = (v * a) as u8;
            }
            row[o + 3] = (a * 255.0) as u8;
        }
    });
    out
}

#[allow(dead_code)]
pub fn preview_strip(path: &std::path::Path) -> std::io::Result<()> {
    // Development aid: write a few frames side by side as a PPM over a dark background.
    let side = 340usize;
    let frames: Vec<Frame> = [0.25, 0.7, 1.0]
        .iter()
        .map(|&p| Frame {
            time: p * 2.0,
            burst: None,
            push: Some(p),
            dir: Dir::Right,
        })
        .chain([0.05, 0.2, 0.4, 0.65].iter().map(|&t| Frame {
            time: t,
            burst: Some((t, Kind::Leave)),
            push: None,
            dir: Dir::Right,
        }))
        .chain([0.05, 0.25, 0.5].iter().map(|&t| Frame {
            time: t,
            burst: Some((t, Kind::Arrive)),
            push: None,
            dir: Dir::Left,
        }))
        .collect();
    let w = side * frames.len();
    let mut img = vec![0u8; w * side * 3];
    for (i, f) in frames.iter().enumerate() {
        let rgba = render(side, 1.0, f);
        for y in 0..side {
            for x in 0..side {
                let s = (y * side + x) * 4;
                let bg = if x < side / 2 { 40.0 } else { 225.0 };
                let a = rgba[s + 3] as f32 / 255.0;
                let o = (y * w + i * side + x) * 3;
                for ch in 0..3 {
                    img[o + ch] = (rgba[s + ch] as f32 + bg * (1.0 - a)).min(255.0) as u8;
                }
            }
        }
    }
    let mut data = format!("P6 {w} {side} 255\n").into_bytes();
    data.extend_from_slice(&img);
    std::fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_premultiplied_and_fade_out() {
        let f = Frame {
            time: 0.3,
            burst: Some((0.3, Kind::Leave)),
            push: None,
            dir: Dir::Right,
        };
        let px = render(64, 0.2, &f);
        assert!(px.chunks(4).any(|p| p[3] > 0), "something is drawn");
        assert!(px
            .chunks(4)
            .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
        let end = Frame {
            time: 1.0,
            burst: Some((0.999, Kind::Leave)),
            push: None,
            dir: Dir::Right,
        };
        let max_a = render(64, 0.2, &end).chunks(4).map(|p| p[3]).max().unwrap();
        assert!(max_a < 40, "almost gone at the end ({max_a})");
    }

    #[test]
    fn glass_drop_grows_with_the_push_and_fades_after_a_burst() {
        let frame = |push: Option<f32>, burst: Option<(f32, Kind)>| Frame {
            time: 0.0,
            burst,
            push,
            dir: Dir::Right,
        };
        let small = glass_drop(&frame(Some(0.2), None)).0;
        let big = glass_drop(&frame(Some(1.0), None)).0;
        assert!(big > small);
        assert!(glass_drop(&frame(None, Some((0.999, Kind::Leave)))).1 < 0.01);
        assert!(glass_drop(&frame(None, Some((0.999, Kind::Arrive)))).0 < 1.0);
        assert!(glass_drop(&frame(None, Some((0.999, Kind::Leave)))).0 <= SIZE / 2.0);
    }

    #[test]
    fn finds_the_edge_under_the_point() {
        let m = vec![Monitor {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            primary: true,
            name: String::new(),
        }];
        assert_eq!(edge_of(&m, (1919, 500)).0, Dir::Right);
        assert_eq!(edge_of(&m, (3, 500)).0, Dir::Left);
        assert_eq!(edge_of(&m, (900, 1079)).0, Dir::Down);
    }

    #[test]
    fn distortion_leaves_the_screen_alone_outside_the_effect() {
        let side = 64;
        let snap = Snapshot {
            w: side,
            h: side,
            rgba: vec![200; side * side * 4],
        };
        let f = Frame {
            time: 0.0,
            burst: None,
            push: Some(0.5),
            dir: Dir::Right,
        };
        let px = render_distort(side, 0.3, &f, &snap);
        // Corners are outside the lens: fully transparent, the real screen shows.
        assert_eq!(px[3], 0);
        let mid = (side / 2 * side + side / 2) * 4;
        assert_eq!(px[mid + 3], 255);
        assert!(
            px[mid] >= 190,
            "a flat image stays the same colour under the lens"
        );
    }

    /// Development aid: CROSSDESK_FX_SNAPSHOT = raw 340x340 RGBA file of some screen content.
    #[test]
    #[ignore]
    fn write_distort_preview() {
        let raw = std::fs::read(std::env::var("CROSSDESK_FX_SNAPSHOT").unwrap()).unwrap();
        let side = 340usize;
        let snap = Snapshot {
            w: side,
            h: side,
            rgba: raw,
        };
        let frames: Vec<Frame> = [0.4, 1.0]
            .iter()
            .map(|&p| Frame {
                time: p,
                burst: None,
                push: Some(p),
                dir: Dir::Right,
            })
            .chain([0.15, 0.35, 0.6].iter().map(|&t| Frame {
                time: t,
                burst: Some((t, Kind::Leave)),
                push: None,
                dir: Dir::Right,
            }))
            .collect();
        let w = side * frames.len();
        let mut img = vec![0u8; w * side * 3];
        for (i, f) in frames.iter().enumerate() {
            let fx = render_distort(side, 1.0, f, &snap);
            for y in 0..side {
                for x in 0..side {
                    let s = (y * side + x) * 4;
                    let a = fx[s + 3] as f32 / 255.0;
                    let o = (y * w + i * side + x) * 3;
                    for ch in 0..3 {
                        let under = if x < side / 2 {
                            snap.rgba[s + ch] as f32
                        } else {
                            0.0
                        };
                        img[o + ch] = (fx[s + ch] as f32 + under * (1.0 - a)).min(255.0) as u8;
                    }
                }
            }
        }
        let mut data = format!(
            "P6 {w} {side} 255
"
        )
        .into_bytes();
        data.extend_from_slice(&img);
        let out = std::env::temp_dir().join("crossdesk-distort.ppm");
        std::fs::write(&out, data).unwrap();
        eprintln!("preview: {}", out.display());
    }

    #[test]
    #[ignore]
    fn write_preview() {
        let p = std::env::temp_dir().join("crossdesk-effects.ppm");
        preview_strip(&p).unwrap();
        eprintln!("preview: {}", p.display());
    }
}
