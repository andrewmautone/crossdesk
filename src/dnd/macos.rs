//! macOS: the drag pasteboard tells which files Finder is dragging, and an
//! NSDraggingSession started from a small panel under the cursor carries files
//! that arrive from another machine to wherever the user drops them.

use crate::protocol::Button;
use crate::state::Shared;
use core_graphics::display::CGDisplay;
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// NSPasteboardNameDrag.
const DRAG_PASTEBOARD: &str = "Apple CFPasteboard drag";
/// Side of the panel that starts incoming drags, in points.
const SIDE: f64 = 24.0;
const NS_DRAG_OPERATION_COPY: usize = 1;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
/// Drag pasteboard change count when the left button went down.
static COUNT_AT_DOWN: AtomicIsize = AtomicIsize::new(-1);
static LAST_PROBE_MS: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();
/// Files waiting for the injected press that starts their drag.
static BEGIN: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);
/// Files of the drag in progress.
static FLIGHT: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);
static PANEL: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

pub fn start(shared: Arc<Shared>) {
    let _ = SHARED.set(shared);
}

unsafe fn drag_pasteboard() -> *mut AnyObject {
    let name = NSString::from_str(DRAG_PASTEBOARD);
    msg_send![class!(NSPasteboard), pasteboardWithName: &*name]
}

/// Tap thread: the left button went down; a drag starting now changes the drag pasteboard.
pub fn note_mouse_down() {
    if !super::enabled() {
        return;
    }
    let count = autoreleasepool(|_| unsafe {
        let pb = drag_pasteboard();
        if pb.is_null() {
            -1
        } else {
            msg_send![pb, changeCount]
        }
    });
    COUNT_AT_DOWN.store(count, Ordering::SeqCst);
}

/// Tap thread (engine unlocked): the held button reached an edge to another
/// machine. If Finder put files on the drag pasteboard since the press, it is a file drag.
pub fn probe_now() {
    let now = now_ms();
    if now - LAST_PROBE_MS.load(Ordering::Relaxed) < 200 || !super::enabled() {
        return;
    }
    LAST_PROBE_MS.store(now, Ordering::Relaxed);
    let files = autoreleasepool(|_| unsafe {
        let pb = drag_pasteboard();
        if pb.is_null() {
            return None;
        }
        let count: isize = msg_send![pb, changeCount];
        if count == COUNT_AT_DOWN.load(Ordering::SeqCst) {
            return None; // not a drag that wrote anything: text selection, window move
        }
        let items: *mut AnyObject = msg_send![pb, pasteboardItems];
        if items.is_null() {
            return None;
        }
        let n: usize = msg_send![items, count];
        let file_url = NSString::from_str("public.file-url");
        let mut out = Vec::new();
        for i in 0..n {
            let item: *mut AnyObject = msg_send![items, objectAtIndex: i];
            let s: *mut NSString = msg_send![item, stringForType: &*file_url];
            let Some(s) = s.as_ref() else { continue };
            let url: *mut AnyObject = msg_send![class!(NSURL), URLWithString: s];
            if url.is_null() {
                continue;
            }
            // Finder drags file reference URLs (file:///.file/id=...).
            let url: *mut AnyObject = msg_send![url, filePathURL];
            if url.is_null() {
                continue;
            }
            let path: *mut NSString = msg_send![url, path];
            if let Some(p) = path.as_ref() {
                out.push(PathBuf::from(p.to_string()));
            }
        }
        Some(out)
    });
    if files.is_some() {
        super::set_local(files);
    }
}

pub fn local_drag_over(crossed: bool) {
    LAST_PROBE_MS.store(0, Ordering::Relaxed);
    COUNT_AT_DOWN.store(-1, Ordering::SeqCst);
    if crossed {
        crate::input::cancel_os_drag();
    }
}

extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

type Job = Box<dyn FnOnce() + Send>;

extern "C" fn run_job(ctx: *mut c_void) {
    let job: Box<Job> = unsafe { Box::from_raw(ctx as *mut Job) };
    job();
}

fn on_main(f: impl FnOnce() + Send + 'static) {
    let job: Box<Job> = Box::new(Box::new(f));
    unsafe {
        dispatch_async_f(
            &_dispatch_main_q as *const c_void,
            Box::into_raw(job) as *mut c_void,
            run_job,
        );
    }
}

pub fn begin_drag(paths: Vec<PathBuf>) {
    let Some(s) = SHARED.get() else { return };
    let Some(at) = *s.remote_cursor.lock() else {
        super::drag_finished(paths, false);
        return;
    };
    *BEGIN.lock() = Some(paths);
    on_main(move || unsafe {
        let panel = panel();
        if panel.is_null() {
            give_up();
            return;
        }
        // Cocoa puts the origin at the bottom left of the main screen.
        let main_h = CGDisplay::main().bounds().size.height;
        let frame = NSRect::new(
            NSPoint::new(at.0 as f64 - SIDE / 2.0, main_h - at.1 as f64 - SIDE / 2.0),
            NSSize::new(SIDE, SIDE),
        );
        let _: () = msg_send![panel, setFrame: frame, display: Bool::NO];
        let _: () = msg_send![panel, orderFrontRegardless];
        // Press the button on our panel; its mouseDown: starts the drag.
        if let Some(s) = SHARED.get() {
            s.injector.lock().button(Button::Left, true);
        }
    });
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(1500));
        if BEGIN.lock().is_some() {
            on_main(|| unsafe { give_up() });
        }
    });
}

/// The press never reached the panel: let go and save the files instead.
unsafe fn give_up() {
    hide_panel();
    if let Some(paths) = BEGIN.lock().take() {
        if let Some(s) = SHARED.get() {
            s.injector.lock().button(Button::Left, false);
        }
        super::drag_finished(paths, false);
    }
}

unsafe fn hide_panel() {
    let panel = PANEL.load(Ordering::SeqCst);
    if !panel.is_null() {
        let _: () = msg_send![panel, orderOut: std::ptr::null_mut::<AnyObject>()];
    }
}

/// Main thread: the borderless, non-activating panel that hosts the drag source view.
unsafe fn panel() -> *mut AnyObject {
    let existing = PANEL.load(Ordering::SeqCst);
    if !existing.is_null() {
        return existing;
    }
    let Some(view_class) = source_view_class() else {
        return std::ptr::null_mut();
    };
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIDE, SIDE));
    let panel: *mut AnyObject = msg_send![class!(NSPanel), alloc];
    // borderless (0) | nonactivatingPanel (1 << 7); backing 2 = buffered
    let panel: *mut AnyObject = msg_send![panel, initWithContentRect: rect, styleMask: 128usize, backing: 2usize, defer: Bool::NO];
    if panel.is_null() {
        return panel;
    }
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let _: () = msg_send![panel, setOpaque: Bool::NO];
    let _: () = msg_send![panel, setBackgroundColor: clear];
    let _: () = msg_send![panel, setHasShadow: Bool::NO];
    let _: () = msg_send![panel, setAlphaValue: 0.02f64];
    // Clicks land on it although it is see-through.
    let _: () = msg_send![panel, setIgnoresMouseEvents: Bool::NO];
    let _: () = msg_send![panel, setReleasedWhenClosed: Bool::NO];
    let _: () = msg_send![panel, setLevel: 1001isize];
    // canJoinAllSpaces | stationary | ignoresCycle | fullScreenAuxiliary
    let _: () = msg_send![panel, setCollectionBehavior: (1usize | 16 | 64 | 256)];
    let view: *mut AnyObject = msg_send![view_class, alloc];
    let view: *mut AnyObject = msg_send![view, initWithFrame: rect];
    let _: () = msg_send![panel, setContentView: view];
    PANEL.store(panel, Ordering::SeqCst);
    panel
}

/// An NSView subclass that starts a file drag on mouseDown: and is its own dragging source.
fn source_view_class() -> Option<&'static AnyClass> {
    static CLASS: OnceLock<Option<&'static AnyClass>> = OnceLock::new();
    *CLASS.get_or_init(|| unsafe {
        let mut b = ClassBuilder::new("CrossdeskDragSourceView", class!(NSView))?;
        b.add_method(
            sel!(acceptsFirstMouse:),
            accepts_first_mouse as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
        );
        b.add_method(
            sel!(mouseDown:),
            mouse_down as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
        );
        b.add_method(
            sel!(draggingSession:sourceOperationMaskForDraggingContext:),
            operation_mask as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, isize) -> usize,
        );
        b.add_method(
            sel!(draggingSession:endedAtPoint:operation:),
            ended as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, NSPoint, usize),
        );
        if let Some(p) = AnyProtocol::get("NSDraggingSource") {
            b.add_protocol(p);
        }
        Some(b.register())
    })
}

extern "C" fn accepts_first_mouse(_this: *mut AnyObject, _sel: Sel, _event: *mut AnyObject) -> Bool {
    Bool::YES
}

extern "C" fn operation_mask(
    _this: *mut AnyObject,
    _sel: Sel,
    _session: *mut AnyObject,
    _context: isize,
) -> usize {
    NS_DRAG_OPERATION_COPY
}

extern "C" fn mouse_down(this: *mut AnyObject, _sel: Sel, event: *mut AnyObject) {
    let Some(paths) = BEGIN.lock().take() else {
        return;
    };
    unsafe {
        let items: *mut AnyObject = msg_send![class!(NSMutableArray), array];
        let in_window: NSPoint = msg_send![event, locationInWindow];
        let at: NSPoint = msg_send![this, convertPoint: in_window, fromView: std::ptr::null_mut::<AnyObject>()];
        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        for (i, p) in paths.iter().enumerate() {
            let path = NSString::from_str(&p.to_string_lossy());
            let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*path];
            if url.is_null() {
                continue;
            }
            let item: *mut AnyObject = msg_send![class!(NSDraggingItem), alloc];
            let item: *mut AnyObject = msg_send![item, initWithPasteboardWriter: url];
            let icon: *mut AnyObject = msg_send![ws, iconForFile: &*path];
            let off = i as f64 * 6.0;
            let frame = NSRect::new(
                NSPoint::new(at.x - 32.0 + off, at.y - 32.0 - off),
                NSSize::new(64.0, 64.0),
            );
            let _: () = msg_send![item, setDraggingFrame: frame, contents: icon];
            let _: () = msg_send![items, addObject: item];
        }
        let n: usize = msg_send![items, count];
        if n == 0 {
            hide_panel();
            if let Some(s) = SHARED.get() {
                s.injector.lock().button(Button::Left, false);
            }
            super::drag_finished(paths, false);
            return;
        }
        *FLIGHT.lock() = Some(paths);
        let _: *mut AnyObject =
            msg_send![this, beginDraggingSessionWithItems: items, event: event, source: this];
    }
}

extern "C" fn ended(
    _this: *mut AnyObject,
    _sel: Sel,
    _session: *mut AnyObject,
    _at: NSPoint,
    operation: usize,
) {
    unsafe { hide_panel() };
    if let Some(paths) = FLIGHT.lock().take() {
        super::drag_finished(paths, operation != 0);
    }
}
