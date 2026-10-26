//! macOS: a borderless, transparent NSWindow that ignores the mouse, showing
//! each frame as its layer's contents. AppKit must be used on the main thread,
//! so every call is queued there with libdispatch.

use core_graphics::base::{kCGImageAlphaPremultipliedLast, kCGRenderingIntentDefault};
use core_graphics::color_space::CGColorSpace;
use core_graphics::data_provider::CGDataProvider;
use core_graphics::display::CGDisplay;
use core_graphics::image::CGImage;
use foreign_types::ForeignType;
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

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

/// The overlay window; only touched on the main thread.
static WINDOW: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());

unsafe fn window() -> *mut AnyObject {
    let existing = WINDOW.load(Ordering::SeqCst);
    if !existing.is_null() {
        return existing;
    }
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
    let win: *mut AnyObject = msg_send![class!(NSWindow), alloc];
    // styleMask 0 = borderless, backing 2 = buffered
    let win: *mut AnyObject = msg_send![win, initWithContentRect: rect, styleMask: 0usize, backing: 2usize, defer: Bool::NO];
    if win.is_null() {
        return win;
    }
    let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
    let _: () = msg_send![win, setOpaque: Bool::NO];
    let _: () = msg_send![win, setBackgroundColor: clear];
    let _: () = msg_send![win, setHasShadow: Bool::NO];
    let _: () = msg_send![win, setIgnoresMouseEvents: Bool::YES];
    let _: () = msg_send![win, setReleasedWhenClosed: Bool::NO];
    // Above normal windows and the menu bar.
    let _: () = msg_send![win, setLevel: 1000isize];
    // canJoinAllSpaces | stationary | ignoresCycle | fullScreenAuxiliary
    let _: () = msg_send![win, setCollectionBehavior: (1usize | 16 | 64 | 256)];
    let view: *mut AnyObject = msg_send![win, contentView];
    let _: () = msg_send![view, setWantsLayer: Bool::YES];
    WINDOW.store(win, Ordering::SeqCst);
    win
}

pub struct Overlay;

impl Overlay {
    pub fn new() -> Option<Overlay> {
        Some(Overlay)
    }

    /// Render at Retina resolution; the layer is told its content is 2x.
    pub fn pixel_scale(_monitor_scale: f32) -> f32 {
        2.0
    }

    pub fn present(&mut self, rgba: &[u8], side: usize, center: (i32, i32), logical: f32) {
        let pixels = Arc::new(rgba.to_vec());
        let logical = logical as f64;
        // Cocoa puts the origin at the bottom left of the main screen.
        let main_h = CGDisplay::main().bounds().size.height;
        let x = center.0 as f64 - logical / 2.0;
        let y = main_h - (center.1 as f64 + logical / 2.0);
        on_main(move || unsafe {
            let win = window();
            if win.is_null() {
                return;
            }
            let provider = CGDataProvider::from_buffer(pixels);
            let image = CGImage::new(
                side,
                side,
                8,
                32,
                side * 4,
                &CGColorSpace::create_device_rgb(),
                kCGImageAlphaPremultipliedLast,
                &provider,
                false,
                kCGRenderingIntentDefault,
            );
            let view: *mut AnyObject = msg_send![win, contentView];
            let layer: *mut AnyObject = msg_send![view, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setContents: image.as_ptr() as *mut AnyObject];
                let _: () = msg_send![layer, setContentsScale: (side as f64 / logical)];
            }
            let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(logical, logical));
            let _: () = msg_send![win, setFrame: frame, display: Bool::NO];
            let _: () = msg_send![win, orderFrontRegardless];
        });
    }

    pub fn hide(&mut self) {
        on_main(|| unsafe {
            let win = WINDOW.load(Ordering::SeqCst);
            if !win.is_null() {
                let _: () = msg_send![win, orderOut: std::ptr::null_mut::<AnyObject>()];
            }
        });
    }

    pub fn pump(&self) {}
}
