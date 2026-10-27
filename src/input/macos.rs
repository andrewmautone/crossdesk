//! macOS: a Quartz event tap for capture, CGEventPost for injection.
//! Both need the Accessibility permission.

use super::{keymap, Backend, Decision};
use crate::layout::Monitor;
use crate::protocol::Button;
use crate::state::Shared;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{CGDisplay, CGPoint};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Marks events we post so our own tap ignores them.
const MAGIC: i64 = 0x4352_4F53;

static TAP_PORT: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static FROZEN: AtomicBool = AtomicBool::new(false);
/// Where the local cursor waits while another machine has control.
static FREEZE_PT: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// Mouse events whose deltas to ignore: the first event after a warp reports
/// the warp distance as if the user had moved the mouse that far.
static SKIP_DELTAS: AtomicU32 = AtomicU32::new(0);

/// Move the cursor without the jump showing up as mouse movement.
fn warp_to(x: i32, y: i32, skip_next: bool) {
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x as f64 + 0.5, y as f64 + 0.5));
    let (mut dx, mut dy) = (0, 0);
    unsafe { CGGetLastMouseDelta(&mut dx, &mut dy) };
    if skip_next {
        SKIP_DELTAS.store(1, Ordering::SeqCst);
    }
}
static CAPS_DOWN: Mutex<bool> = Mutex::new(false);
/// Reported the first key sent since the cursor last left this machine.
static KEYS_ANNOUNCED: AtomicBool = AtomicBool::new(false);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGSetLocalEventsSuppressionInterval(seconds: f64) -> i32;
    fn CGGetLastMouseDelta(dx: *mut i32, dy: *mut i32);
}

/// Ask for the Accessibility permission (shows the system prompt once).
fn accessibility_trusted(prompt: bool) -> bool {
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let value = if prompt {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        let opts = CFDictionary::from_CFType_pairs(&[(key, value)]);
        AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
    }
}

pub fn init_process() {}

/// Localized name of the running app with this pid.
fn app_name(pid: i32) -> Option<String> {
    use objc2::rc::autoreleasepool;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    autoreleasepool(|_| unsafe {
        let app: *mut AnyObject = msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ];
        if app.is_null() {
            return None;
        }
        let name: *mut NSString = msg_send![app, localizedName];
        name.as_ref().map(|n| n.to_string())
    })
}

/// The app that gets the keys: the frontmost one, not the one under the cursor.
pub fn focused_app() -> Option<String> {
    use objc2::rc::autoreleasepool;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    let pid = autoreleasepool(|_| unsafe {
        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let app: *mut AnyObject = msg_send![ws, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let pid: i32 = msg_send![app, processIdentifier];
        Some(pid)
    })?;
    app_name(pid).or_else(|| Some(format!("pid {pid}")))
}

/// Secure Event Input (password fields, password managers, Terminal's Secure
/// Keyboard Entry) keeps keys from other processes out of the focused app.
pub fn injection_blocker() -> Option<String> {
    if !unsafe { IsSecureEventInputEnabled() } {
        return None;
    }
    let holder = unsafe {
        let dict = CGSessionCopyCurrentDictionary();
        if dict.is_null() {
            None
        } else {
            let dict: CFDictionary<CFString, core_foundation::base::CFType> =
                CFDictionary::wrap_under_create_rule(dict);
            dict.find(CFString::from_static_string("kCGSSessionSecureInputPID"))
                .and_then(|v| v.downcast::<core_foundation::number::CFNumber>())
                .and_then(|n| n.to_i32())
        }
    };
    let who = match holder {
        Some(pid) => app_name(pid).unwrap_or_else(|| format!("pid {pid}")),
        None => "an unknown app".into(),
    };
    Some(format!(
        "macOS Secure Input is on, held by {who} (a password field, password manager or \
         Terminal's Secure Keyboard Entry); macOS drops typed keys from other apps until it ends"
    ))
}

pub fn shutdown() {
    if FROZEN.swap(false, Ordering::SeqCst) {
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    }
}

pub fn start_capture(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("event-tap".into())
        .spawn(move || {
            let mut prompted = false;
            loop {
                if !accessibility_trusted(!prompted) {
                    prompted = true;
                    *shared.capture_error.lock() = Some(
                        "Allow crossdesk in System Settings > Privacy & Security > Accessibility \
                         (and Input Monitoring). It starts working by itself once allowed."
                            .into(),
                    );
                    shared.repaint();
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                match run_tap(shared.clone()) {
                    Ok(()) => return,
                    Err(e) => {
                        *shared.capture_error.lock() = Some(e);
                        shared.repaint();
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        })
        .expect("spawn event tap thread");
}

fn run_tap(shared: Arc<Shared>) -> Result<(), String> {
    use CGEventType::*;
    let events = vec![
        MouseMoved,
        LeftMouseDown,
        LeftMouseUp,
        RightMouseDown,
        RightMouseUp,
        OtherMouseDown,
        OtherMouseUp,
        LeftMouseDragged,
        RightMouseDragged,
        OtherMouseDragged,
        ScrollWheel,
        KeyDown,
        KeyUp,
        FlagsChanged,
    ];
    let s = shared.clone();
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events,
        move |_proxy, etype, event| {
            if let TapDisabledByTimeout | TapDisabledByUserInput = etype {
                s.diag(format!(
                    "macOS disabled the event tap ({etype:?}); re-enabling it"
                ));
                let port = TAP_PORT.load(Ordering::SeqCst);
                if !port.is_null() {
                    unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
                }
                return Some(event.clone());
            }
            if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == MAGIC {
                return Some(event.clone());
            }
            if handle(&s, etype, event) {
                // core-graphics 0.24 turns `None` back into the original event, so
                // an event is swallowed by turning it into a Null event instead.
                event.set_type(CGEventType::Null);
            }
            Some(event.clone())
        },
    )
    .map_err(|_| "cannot create the event tap; check the Accessibility permission".to_string())?;
    TAP_PORT.store(
        tap.mach_port.as_concrete_TypeRef() as *mut _,
        Ordering::SeqCst,
    );
    let source = tap
        .mach_port
        .create_runloop_source(0)
        .map_err(|_| "cannot attach the event tap".to_string())?;
    unsafe {
        CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
    }
    tap.enable();
    // Warping the cursor must not freeze the real mouse for a quarter second.
    unsafe { CGSetLocalEventsSuppressionInterval(0.0) };
    *shared.capture_error.lock() = None;
    shared.repaint();
    CFRunLoop::run_current();
    Ok(())
}

fn apply(d: Decision) -> bool {
    if let Some(pt) = d.freeze_at {
        KEYS_ANNOUNCED.store(false, Ordering::Relaxed);
        crate::effects::burst(pt, crate::effects::Kind::Leave);
        // Park the idle cursor away from the edge so the Dock and hover previews let go;
        // a file drag stays put so it hovers nothing until it is cancelled.
        let park = if crate::dnd::crossed() {
            pt
        } else {
            super::away_from_edge(pt, &Platform::monitors())
        };
        warp_to(park.0, park.1, true);
        *FREEZE_PT.lock() = Some(park);
        if !FROZEN.swap(true, Ordering::SeqCst) {
            let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
        }
    }
    if let Some(pt) = d.release_to {
        warp(pt.0, pt.1);
        crate::effects::burst(pt, crate::effects::Kind::Arrive);
    }
    d.block
}

/// The drag macOS runs for the frozen cursor crossed to another machine and was
/// dropped there: cancel it here with Escape, then let go of the button.
pub fn cancel_os_drag() {
    std::thread::spawn(|| {
        for down in [true, false] {
            if let Some(e) = source().and_then(|s| CGEvent::new_keyboard_event(s, 0x35, down).ok()) {
                post(e);
            }
        }
        std::thread::sleep(Duration::from_millis(150));
        let Some(pt) = *FREEZE_PT.lock() else { return };
        let at = CGPoint::new(pt.0 as f64, pt.1 as f64);
        if let Some(e) = source().and_then(|s| {
            CGEvent::new_mouse_event(s, CGEventType::LeftMouseUp, at, CGMouseButton::Left).ok()
        }) {
            post(e);
        }
    });
}

/// Put the cursor at `(x, y)` and give it back to the user.
pub fn warp(x: i32, y: i32) {
    *FREEZE_PT.lock() = None;
    warp_to(x, y, true);
    if FROZEN.swap(false, Ordering::SeqCst) {
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    }
}

/// Device-dependent modifier bit for each modifier keycode (left/right aware).
fn modifier_bit(keycode: u16) -> Option<u64> {
    Some(match keycode {
        0x3B => 0x0001, // left control
        0x38 => 0x0002, // left shift
        0x3C => 0x0004, // right shift
        0x37 => 0x0008, // left command
        0x36 => 0x0010, // right command
        0x3A => 0x0020, // left option
        0x3D => 0x0040, // right option
        0x3E => 0x2000, // right control
        _ => return None,
    })
}

fn handle(s: &Arc<Shared>, etype: CGEventType, event: &CGEvent) -> bool {
    use CGEventType::*;
    let button = |b: Button, down: bool| s.engine.lock().on_button(b, down);
    let other = || match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
        3 => Button::Back,
        4 => Button::Forward,
        _ => Button::Middle,
    };
    match etype {
        MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
            let loc = event.location();
            let pt = (loc.x.floor() as i32, loc.y.floor() as i32);
            let mut dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64;
            let mut dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64;
            // Right after a warp, or an implausible jump: not real movement.
            let skip = SKIP_DELTAS.load(Ordering::SeqCst) > 0;
            if skip {
                SKIP_DELTAS.fetch_sub(1, Ordering::SeqCst);
            }
            if skip || dx.abs() > 400.0 || dy.abs() > 400.0 {
                dx = 0.0;
                dy = 0.0;
            }
            let frozen = FROZEN.load(Ordering::SeqCst);
            let mut engine = s.engine.lock();
            let mut d = engine.on_move(pt, frozen.then_some((dx, dy)));
            if !frozen && d == Decision::default() {
                // The deltas keep coming while the cursor is pinned on an edge.
                d = engine.on_push_delta(dx, dy);
                let progress = engine.push_progress();
                if d == Decision::default() && progress > 0.0 {
                    if let Some(at) = engine.push_point() {
                        crate::effects::push(at, progress);
                    }
                }
            }
            drop(engine);
            if d.probe_drag {
                crate::dnd::probe_now();
            }
            let block = apply(d);
            // Belt and braces: if the cursor still drifted while frozen, put it back.
            if FROZEN.load(Ordering::SeqCst) {
                if let Some(f) = *FREEZE_PT.lock() {
                    if (pt.0 - f.0).abs() > 1 || (pt.1 - f.1).abs() > 1 {
                        warp_to(f.0, f.1, false);
                    }
                }
            }
            block
        }
        LeftMouseDown => {
            crate::dnd::note_mouse_down();
            button(Button::Left, true)
        }
        LeftMouseUp => {
            let block = button(Button::Left, false);
            crate::dnd::local_released();
            block
        }
        RightMouseDown => button(Button::Right, true),
        RightMouseUp => button(Button::Right, false),
        OtherMouseDown => button(other(), true),
        OtherMouseUp => button(other(), false),
        ScrollWheel => {
            let continuous =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0;
            let (dx, dy) = if continuous {
                (
                    event
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2)
                        * 3,
                    event
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1)
                        * 3,
                )
            } else {
                (
                    event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
                        * 120,
                    event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)
                        * 120,
                )
            };
            s.engine.lock().on_wheel(dx as i32, dy as i32)
        }
        KeyDown | KeyUp => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let remote = s.engine.lock().active_target().is_some();
            let Some(hid) = keymap::hid_from_mac(keycode) else {
                if remote && matches!(etype, KeyDown) {
                    s.diag(format!("key not sent, no mapping: keycode=0x{keycode:02X}"));
                }
                return remote;
            };
            if remote && matches!(etype, KeyDown) && !KEYS_ANNOUNCED.swap(true, Ordering::Relaxed) {
                s.log("[diag] sending keys to the other machine");
            }
            let d = s.engine.lock().on_key(hid, matches!(etype, KeyDown));
            apply(d)
        }
        FlagsChanged => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let remote = s.engine.lock().active_target().is_some();
            let Some(hid) = keymap::hid_from_mac(keycode) else {
                return remote;
            };
            if keycode == 0x39 {
                // Caps Lock reports only state changes; send a full press.
                let mut caps = CAPS_DOWN.lock();
                *caps = !*caps;
                let mut e = s.engine.lock();
                let a = e.on_key(hid, true);
                let b = e.on_key(hid, false);
                drop(e);
                return apply(a) | apply(b);
            }
            let Some(bit) = modifier_bit(keycode) else {
                return remote;
            };
            let down = event.get_flags().bits() & bit != 0;
            let d = s.engine.lock().on_key(hid, down);
            apply(d)
        }
        _ => false,
    }
}

pub struct Platform {
    keys: HashSet<u16>,
    buttons: HashSet<Button>,
    last_click: Option<(Button, Instant, i64)>,
    pos: CGPoint,
}

impl Default for Platform {
    fn default() -> Self {
        Platform {
            keys: HashSet::new(),
            buttons: HashSet::new(),
            last_click: None,
            pos: CGPoint::new(0.0, 0.0),
        }
    }
}

// The injector lives behind a mutex in `Shared`; CG objects are created per event.
unsafe impl Send for Platform {}

fn source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
}

fn post(event: CGEvent) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, MAGIC);
    event.post(CGEventTapLocation::HID);
}

impl Platform {
    fn flags(&self) -> CGEventFlags {
        let mut bits: u64 = 0;
        let mut add = |hid: u16, general: u64, device: u64| {
            if self.keys.contains(&hid) {
                bits |= general | device;
            }
        };
        add(keymap::LSHIFT, 0x0002_0000, 0x0002);
        add(keymap::RSHIFT, 0x0002_0000, 0x0004);
        add(keymap::LCTRL, 0x0004_0000, 0x0001);
        add(keymap::RCTRL, 0x0004_0000, 0x2000);
        add(keymap::LALT, 0x0008_0000, 0x0020);
        add(keymap::RALT, 0x0008_0000, 0x0040);
        add(keymap::LMETA, 0x0010_0000, 0x0008);
        add(keymap::RMETA, 0x0010_0000, 0x0010);
        CGEventFlags::from_bits_retain(bits)
    }
}

impl Backend for Platform {
    fn monitors() -> Vec<Monitor> {
        let main = CGDisplay::main().id;
        let ids = CGDisplay::active_displays().unwrap_or_default();
        let mut out: Vec<Monitor> = ids
            .into_iter()
            .map(|id| {
                let display = CGDisplay::new(id);
                let b = display.bounds();
                let mm = display.screen_size();
                Monitor {
                    x: b.origin.x.round() as i32,
                    y: b.origin.y.round() as i32,
                    w: b.size.width.round() as i32,
                    h: b.size.height.round() as i32,
                    primary: id == main,
                    name: format!("Display {id}"),
                }
                .with_physical_mm(mm.width, mm.height)
            })
            .collect();
        out.sort_by_key(|m| (m.x, m.y));
        out
    }

    fn move_abs(&mut self, x: i32, y: i32) {
        self.pos = CGPoint::new(x as f64, y as f64);
        let (etype, button) = if self.buttons.contains(&Button::Left) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons.contains(&Button::Right) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if !self.buttons.is_empty() {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        let Some(src) = source() else { return };
        if let Ok(e) = CGEvent::new_mouse_event(src, etype, self.pos, button) {
            e.set_flags(self.flags());
            post(e);
        }
    }

    fn button(&mut self, button: Button, down: bool) {
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        let (etype, cg, number) = match (button, down) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
            (b, true) => (
                CGEventType::OtherMouseDown,
                CGMouseButton::Center,
                other_number(b),
            ),
            (b, false) => (
                CGEventType::OtherMouseUp,
                CGMouseButton::Center,
                other_number(b),
            ),
        };
        // Count consecutive clicks so double and triple clicks work.
        let clicks = if down {
            let n = match self.last_click {
                Some((b, t, n)) if b == button && t.elapsed() < Duration::from_millis(450) => n + 1,
                _ => 1,
            };
            self.last_click = Some((button, Instant::now(), n));
            n
        } else {
            self.last_click.map_or(1, |(_, _, n)| n)
        };
        let Some(src) = source() else { return };
        if let Ok(e) = CGEvent::new_mouse_event(src, etype, self.pos, cg) {
            e.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, clicks);
            e.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
            e.set_flags(self.flags());
            post(e);
        }
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        let lines = SCROLL_LINES.lock().max(0.1);
        let Some(src) = source() else { return };
        let event = if dy % 120 == 0 && dx % 120 == 0 {
            let l = |v: i32| (v as f32 / 120.0 * lines).round() as i32;
            CGEvent::new_scroll_event(src, ScrollEventUnit::LINE, 2, l(dy), l(dx), 0)
        } else {
            let px = |v: i32| (v as f32 / 3.0) as i32;
            CGEvent::new_scroll_event(src, ScrollEventUnit::PIXEL, 2, px(dy), px(dx), 0)
        };
        if let Ok(e) = event {
            post(e);
        }
    }

    fn key(&mut self, hid: u16, down: bool) {
        let Some(code) = keymap::mac_from_hid(hid) else {
            return;
        };
        let repeat = down && self.keys.contains(&hid);
        if down {
            self.keys.insert(hid);
        } else {
            self.keys.remove(&hid);
        }
        let Some(src) = source() else { return };
        if let Ok(e) = CGEvent::new_keyboard_event(src, code, down) {
            if keymap::is_modifier(hid) {
                e.set_type(CGEventType::FlagsChanged);
            }
            e.set_flags(self.flags());
            if repeat {
                e.set_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT, 1);
            }
            post(e);
        }
    }

    fn set_scroll_lines(&mut self, lines: f32) {
        *SCROLL_LINES.lock() = lines;
    }
}

static SCROLL_LINES: Mutex<f32> = Mutex::new(3.0);

fn other_number(b: Button) -> i64 {
    match b {
        Button::Back => 3,
        Button::Forward => 4,
        _ => 2,
    }
}
