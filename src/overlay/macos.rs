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
use std::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};
use std::sync::Arc;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Ask once for Screen Recording, which the realistic distortion needs.
pub fn request_capture_permission() {
    unsafe {
        if !CGPreflightScreenCaptureAccess() {
            CGRequestScreenCaptureAccess();
        }
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

/// The overlay window; only touched on the main thread.
static WINDOW: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
/// Its window number, so captures can skip it.
static WINDOW_NUMBER: AtomicIsize = AtomicIsize::new(0);

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
    let number: isize = msg_send![win, windowNumber];
    WINDOW_NUMBER.store(number, Ordering::SeqCst);
    WINDOW.store(win, Ordering::SeqCst);
    win
}

/// The Liquid Glass drop inside the overlay window, created on first use.
static GLASS: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());

fn glass_class() -> Option<&'static objc2::runtime::AnyClass> {
    objc2::runtime::AnyClass::get("NSGlassEffectView")
}

unsafe fn glass_view(win: *mut AnyObject) -> *mut AnyObject {
    let existing = GLASS.load(Ordering::SeqCst);
    if !existing.is_null() {
        return existing;
    }
    let Some(cls) = glass_class() else {
        return std::ptr::null_mut();
    };
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
    let g: *mut AnyObject = msg_send![cls, alloc];
    let g: *mut AnyObject = msg_send![g, initWithFrame: rect];
    if g.is_null() {
        return g;
    }
    // NSGlassEffectViewStyleClear: refracts more and frosts less than Regular.
    let style = objc2::sel!(setStyle:);
    let responds: Bool = msg_send![g, respondsToSelector: style];
    if responds.as_bool() {
        let _: () = msg_send![g, setStyle: 1isize];
    }
    let view: *mut AnyObject = msg_send![win, contentView];
    let _: () = msg_send![view, addSubview: g];
    GLASS.store(g, Ordering::SeqCst);
    g
}

/// Hide the glass drop (the CPU-rendered effect is showing instead).
unsafe fn hide_glass() {
    let g = GLASS.load(Ordering::SeqCst);
    if !g.is_null() {
        let _: () = msg_send![g, setHidden: Bool::YES];
    }
}

pub struct Overlay;

impl Overlay {
    /// Liquid Glass (macOS 26+) refracts whatever is behind the window on the
    /// GPU, with no Screen Recording permission and no snapshot.
    pub fn glass_available() -> bool {
        glass_class().is_some()
    }

    /// Show a round glass drop of `radius` (points) centered on `center`.
    pub fn present_glass(&mut self, center: (i32, i32), logical: f32, radius: f32, alpha: f32) {
        let logical = logical as f64;
        let main_h = CGDisplay::main().bounds().size.height;
        let x = center.0 as f64 - logical / 2.0;
        let y = main_h - (center.1 as f64 + logical / 2.0);
        let r = radius.max(0.0) as f64;
        on_main(move || unsafe {
            let win = window();
            if win.is_null() {
                return;
            }
            let g = glass_view(win);
            if g.is_null() {
                return;
            }
            let view: *mut AnyObject = msg_send![win, contentView];
            let layer: *mut AnyObject = msg_send![view, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setContents: std::ptr::null_mut::<AnyObject>()];
            }
            let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(logical, logical));
            let _: () = msg_send![win, setFrame: frame, display: Bool::NO];
            let drop = NSRect::new(
                NSPoint::new(logical / 2.0 - r, logical / 2.0 - r),
                NSSize::new(2.0 * r, 2.0 * r),
            );
            let _: () = msg_send![g, setFrame: drop];
            let corner = objc2::sel!(setCornerRadius:);
            let responds: Bool = msg_send![g, respondsToSelector: corner];
            if responds.as_bool() {
                let _: () = msg_send![g, setCornerRadius: r];
            }
            let _: () = msg_send![g, setAlphaValue: alpha.clamp(0.0, 1.0) as f64];
            let _: () = msg_send![g, setHidden: Bool::from(r < 1.0)];
            let _: () = msg_send![win, orderFrontRegardless];
        });
    }

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
            hide_glass();
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

    /// Copy the screen patch around `center` (everything below our overlay).
    /// Without Screen Recording permission macOS returns only the wallpaper,
    /// which would look wrong, so then there is no snapshot at all.
    pub fn capture(
        center: (i32, i32),
        _side: usize,
        logical: f32,
    ) -> Option<crate::effects::Snapshot> {
        use core_graphics::geometry::{CGPoint, CGRect, CGSize};
        use core_graphics::window::{
            create_image, kCGNullWindowID, kCGWindowImageBestResolution,
            kCGWindowListOptionOnScreenBelowWindow, kCGWindowListOptionOnScreenOnly,
        };
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            return None;
        }
        let l = logical as f64;
        let rect = CGRect::new(
            &CGPoint::new(center.0 as f64 - l / 2.0, center.1 as f64 - l / 2.0),
            &CGSize::new(l, l),
        );
        let ours = WINDOW_NUMBER.load(Ordering::SeqCst);
        let image = if ours > 0 {
            create_image(
                rect,
                kCGWindowListOptionOnScreenBelowWindow,
                ours as u32,
                kCGWindowImageBestResolution,
            )
        } else {
            create_image(
                rect,
                kCGWindowListOptionOnScreenOnly,
                kCGNullWindowID,
                kCGWindowImageBestResolution,
            )
        }?;
        let (w, h, row) = (image.width(), image.height(), image.bytes_per_row());
        if image.bits_per_pixel() != 32 || w == 0 || h == 0 {
            return None;
        }
        let data = image.data();
        let bytes = data.bytes();
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = y * row + x * 4;
                let o = (y * w + x) * 4;
                // Screen captures come back as BGRA (32-bit little endian, alpha first).
                rgba[o] = bytes[i + 2];
                rgba[o + 1] = bytes[i + 1];
                rgba[o + 2] = bytes[i];
                rgba[o + 3] = 255;
            }
        }
        Some(crate::effects::Snapshot { w, h, rgba })
    }
}
