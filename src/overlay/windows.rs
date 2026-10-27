//! Windows: a per-pixel-alpha layered popup updated with UpdateLayeredWindow.
//! It lives on the effects thread, which also pumps its messages.

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub struct Overlay {
    hwnd: HWND,
    dc: HDC,
    bitmap: Option<(HBITMAP, *mut u8, usize)>,
    shown: bool,
}

// Only ever used on the effects thread that created it.
unsafe impl Send for Overlay {}

impl Overlay {
    /// Liquid Glass exists only on macOS.
    pub fn glass_available() -> bool {
        false
    }

    pub fn present_glass(&mut self, _center: (i32, i32), _logical: f32, _radius: f32, _alpha: f32) {
    }

    pub fn new() -> Option<Overlay> {
        unsafe {
            let hinst = GetModuleHandleW(None).ok()?;
            let class = w!("crossdesk-effect");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                class,
                w!("crossdesk effect"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                hinst,
                None,
            )
            .ok()?;
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(None, screen);
            Some(Overlay {
                hwnd,
                dc,
                bitmap: None,
                shown: false,
            })
        }
    }

    /// Physical pixels per logical pixel: follow the monitor size so the ripple
    /// looks the same on 1080p and 4K.
    pub fn pixel_scale(monitor_scale: f32) -> f32 {
        monitor_scale.clamp(1.0, 2.5)
    }

    fn surface(&mut self, side: usize) -> Option<*mut u8> {
        if let Some((_, bits, s)) = self.bitmap {
            if s == side {
                return Some(bits);
            }
        }
        unsafe {
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: side as i32,
                    biHeight: -(side as i32), // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(self.dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            SelectObject(self.dc, bmp);
            if let Some((prev, _, _)) = self.bitmap.take() {
                let _ = DeleteObject(prev);
            }
            self.bitmap = Some((bmp, bits as *mut u8, side));
            Some(bits as *mut u8)
        }
    }

    pub fn present(&mut self, rgba: &[u8], side: usize, center: (i32, i32), _logical: f32) {
        let Some(bits) = self.surface(side) else {
            return;
        };
        unsafe {
            let dst = std::slice::from_raw_parts_mut(bits, side * side * 4);
            let (dst, _) = dst.as_chunks_mut::<4>();
            let (src_px, _) = rgba.as_chunks::<4>();
            for (o, i) in dst.iter_mut().zip(src_px) {
                o[0] = i[2];
                o[1] = i[1];
                o[2] = i[0];
                o[3] = i[3];
            }
            let pos = POINT {
                x: center.0 - side as i32 / 2,
                y: center.1 - side as i32 / 2,
            };
            let size = SIZE {
                cx: side as i32,
                cy: side as i32,
            };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pos),
                Some(&size),
                self.dc,
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            if !self.shown {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                self.shown = true;
            }
        }
    }

    pub fn hide(&mut self) {
        if self.shown {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.shown = false;
        }
    }

    /// Copy the screen patch centered on `center`. Plain BitBlt (no CAPTUREBLT)
    /// leaves layered windows out, so our own overlay never ends up in it.
    pub fn capture(
        center: (i32, i32),
        side: usize,
        _logical: f32,
    ) -> Option<crate::effects::Snapshot> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(screen);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: side as i32,
                    biHeight: -(side as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let result = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .ok()
                .and_then(|bmp| {
                    SelectObject(dc, bmp);
                    let half = side as i32 / 2;
                    let ok = BitBlt(
                        dc,
                        0,
                        0,
                        side as i32,
                        side as i32,
                        screen,
                        center.0 - half,
                        center.1 - half,
                        SRCCOPY,
                    )
                    .is_ok();
                    let snap = ok.then(|| {
                        let src = std::slice::from_raw_parts(bits as *const u8, side * side * 4);
                        let mut rgba = vec![0u8; side * side * 4];
                        for (o, i) in rgba
                            .as_chunks_mut::<4>()
                            .0
                            .iter_mut()
                            .zip(src.as_chunks::<4>().0)
                        {
                            *o = [i[2], i[1], i[0], 255];
                        }
                        crate::effects::Snapshot {
                            w: side,
                            h: side,
                            rgba,
                        }
                    });
                    let _ = DeleteObject(bmp);
                    snap
                });
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            result
        }
    }

    pub fn pump(&self) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
