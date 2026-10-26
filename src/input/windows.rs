//! Windows: low-level hooks for capture, SendInput for injection.

use super::{keymap, Backend, Decision};
use crate::layout::Monitor;
use crate::protocol::Button;
use crate::state::Shared;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use windows::core::w;
use windows::Win32::Foundation::{BOOL, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Marks events we inject so our own hooks ignore them.
const MAGIC: usize = 0x4352_4F53;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
/// While another machine has the cursor, the real one is parked (hidden) here
/// and every hook event is measured against it to get the movement delta.
static PARK: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static HIDDEN: AtomicBool = AtomicBool::new(false);

pub fn init_process() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        show_cursor();
        prev(info);
    }));
}

pub fn shutdown() {
    show_cursor();
}

pub fn start_capture(shared: Arc<Shared>) {
    let _ = SHARED.set(shared.clone());
    std::thread::Builder::new()
        .name("input-hooks".into())
        .spawn(move || unsafe {
            let hmod = match GetModuleHandleW(None) {
                Ok(h) => h,
                Err(e) => {
                    *shared.capture_error.lock() = Some(format!("GetModuleHandle failed: {e}"));
                    return;
                }
            };
            let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0);
            let kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinst, 0);
            if let Err(e) = mouse.as_ref().and(kbd.as_ref()) {
                *shared.capture_error.lock() = Some(format!("cannot install input hooks: {e}"));
                return;
            }
            if let Err(e) = raw_input_window(hinst) {
                shared.log(format!(
                    "raw mouse input unavailable, edge push disabled: {e}"
                ));
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .expect("spawn hook thread");
}

fn shared() -> Option<&'static Arc<Shared>> {
    SHARED.get()
}

fn park_point(pt: (i32, i32)) -> (i32, i32) {
    let mons = shared()
        .map(|s| s.local_monitors.lock().clone())
        .unwrap_or_default();
    let m = mons
        .iter()
        .find(|m| pt.0 >= m.x && pt.0 < m.x + m.w && pt.1 >= m.y && pt.1 < m.y + m.h)
        .or_else(|| mons.iter().find(|m| m.primary));
    match m {
        Some(m) => (m.x + m.w / 2, m.y + m.h / 2),
        None => pt,
    }
}

fn apply(d: Decision) -> bool {
    if let Some(pt) = d.freeze_at {
        let park = park_point(pt);
        *PARK.lock() = Some(park);
        unsafe {
            let _ = SetCursorPos(park.0, park.1);
        }
        hide_cursor();
    }
    if let Some(pt) = d.release_to {
        warp(pt.0, pt.1);
    }
    d.block
}

/// Put the cursor at `(x, y)` and give it back to the user.
pub fn warp(x: i32, y: i32) {
    *PARK.lock() = None;
    unsafe {
        let _ = SetCursorPos(x, y);
    }
    show_cursor();
}

const CURSOR_IDS: [u32; 13] = [
    32512, 32513, 32514, 32515, 32516, 32642, 32643, 32644, 32645, 32646, 32648, 32649, 32650,
];

fn hide_cursor() {
    if HIDDEN.swap(true, Ordering::SeqCst) {
        return;
    }
    let and_mask = [0xFFu8; 128];
    let xor_mask = [0u8; 128];
    for id in CURSOR_IDS {
        unsafe {
            if let Ok(c) = CreateCursor(
                None,
                0,
                0,
                32,
                32,
                and_mask.as_ptr().cast(),
                xor_mask.as_ptr().cast(),
            ) {
                let _ = SetSystemCursor(c, SYSTEM_CURSOR_ID(id));
            }
        }
    }
}

fn show_cursor() {
    if HIDDEN.swap(false, Ordering::SeqCst) {
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_SETCURSORS,
                0,
                None,
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let injected = info.flags & LLMHF_INJECTED != 0 || info.dwExtraInfo == MAGIC;
        if !injected && handle_mouse(wparam.0 as u32, info) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

fn handle_mouse(msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
    let Some(s) = shared() else { return false };
    let hi = (info.mouseData >> 16) as u16;
    let button = |b: Button, down: bool| s.engine.lock().on_button(b, down);
    match msg {
        WM_MOUSEMOVE => {
            let pt = (info.pt.x, info.pt.y);
            let delta = PARK
                .lock()
                .map(|p| ((pt.0 - p.0) as f64, (pt.1 - p.1) as f64));
            let d = s.engine.lock().on_move(pt, delta);
            apply(d)
        }
        WM_LBUTTONDOWN => button(Button::Left, true),
        WM_LBUTTONUP => button(Button::Left, false),
        WM_RBUTTONDOWN => button(Button::Right, true),
        WM_RBUTTONUP => button(Button::Right, false),
        WM_MBUTTONDOWN => button(Button::Middle, true),
        WM_MBUTTONUP => button(Button::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let b = if hi == XBUTTON2 {
                Button::Forward
            } else {
                Button::Back
            };
            button(b, msg == WM_XBUTTONDOWN)
        }
        WM_MOUSEWHEEL => s.engine.lock().on_wheel(0, hi as i16 as i32),
        WM_MOUSEHWHEEL => s.engine.lock().on_wheel(hi as i16 as i32, 0),
        _ => false,
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let injected = info.flags.0 & LLKHF_INJECTED.0 != 0 || info.dwExtraInfo == MAGIC;
        if !injected && handle_key(wparam.0 as u32, info) {
            return LRESULT(1);
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

fn handle_key(msg: u32, info: &KBDLLHOOKSTRUCT) -> bool {
    let Some(s) = shared() else { return false };
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let remote = s.engine.lock().active_target().is_some();
    // AltGr sends a fake left Ctrl with this marker; the remote side makes its own.
    if info.scanCode & 0x200 != 0 {
        return remote;
    }
    let sc = info.scanCode as u16;
    let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;
    let hid = match VIRTUAL_KEY(info.vkCode as u16) {
        VK_PAUSE => Some(0x48),
        VK_NUMLOCK => Some(0x53),
        _ => keymap::hid_from_win(if extended { 0xE000 | sc } else { sc })
            .or_else(|| keymap::hid_from_win(sc)),
    };
    let Some(hid) = hid else { return remote };
    let d = s.engine.lock().on_key(hid, down);
    apply(d)
}

/// A message-only window that receives raw mouse movement. Unlike the hook,
/// raw input keeps reporting motion while the cursor is pinned on a screen edge,
/// which is what measures the push needed to cross.
unsafe fn raw_input_window(hinst: HINSTANCE) -> windows::core::Result<HWND> {
    let class = w!("crossdesk-raw-input");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(raw_input_proc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!(""),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        None,
        hinst,
        None,
    )?;
    let device = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    RegisterRawInputDevices(&[device], std::mem::size_of::<RAWINPUTDEVICE>() as u32)?;
    Ok(hwnd)
}

unsafe extern "system" fn raw_input_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        handle_raw(HRAWINPUT(lparam.0 as _));
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn handle_raw(handle: HRAWINPUT) {
    let Some(s) = shared() else { return };
    if PARK.lock().is_some() {
        return; // another machine has the cursor; the hook measures movement then
    }
    let mut raw = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let n = GetRawInputData(
        handle,
        RID_INPUT,
        Some(&mut raw as *mut RAWINPUT as *mut _),
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    // Injected input has no device; absolute devices (tablets, remote desktop) cannot push.
    if n == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 || raw.header.hDevice.0.is_null() {
        return;
    }
    let mouse = raw.data.mouse;
    if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 || (mouse.lLastX == 0 && mouse.lLastY == 0) {
        return;
    }
    let d = s
        .engine
        .lock()
        .on_push_delta(mouse.lLastX as f64, mouse.lLastY as f64);
    apply(d);
}

#[derive(Default)]
pub struct Platform;

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as _,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MAGIC,
            },
        },
    }
}

unsafe extern "system" fn monitor_proc(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut info.monitorInfo as *mut MONITORINFO).as_bool() {
        let r = info.monitorInfo.rcMonitor;
        let len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let name = String::from_utf16_lossy(&info.szDevice[..len])
            .trim_start_matches("\\\\.\\")
            .to_string();
        out.push(Monitor {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            name,
        });
    }
    BOOL(1)
}

impl Backend for Platform {
    fn monitors() -> Vec<Monitor> {
        let mut out: Vec<Monitor> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                HDC::default(),
                None,
                Some(monitor_proc),
                LPARAM(&mut out as *mut _ as isize),
            );
        }
        out.sort_by_key(|m| (m.x, m.y));
        out
    }

    fn move_abs(&mut self, x: i32, y: i32) {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let nx = ((x - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32;
        let ny = ((y - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32;
        send(&[mouse_input(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )]);
    }

    fn button(&mut self, button: Button, down: bool) {
        let (flags, data) = match (button, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
            (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
            (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
            (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
        };
        send(&[mouse_input(0, 0, data, flags)]);
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        if dy != 0 {
            send(&[mouse_input(0, 0, dy, MOUSEEVENTF_WHEEL)]);
        }
        if dx != 0 {
            send(&[mouse_input(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
        }
    }

    fn key(&mut self, hid: u16, down: bool) {
        let up = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        let ki = if hid == 0x48 {
            KEYBDINPUT {
                wVk: VK_PAUSE,
                wScan: 0,
                dwFlags: up,
                time: 0,
                dwExtraInfo: MAGIC,
            }
        } else {
            let Some(code) = keymap::win_from_hid(hid) else {
                return;
            };
            let ext = if code & 0xE000 == 0xE000 {
                KEYEVENTF_EXTENDEDKEY
            } else {
                KEYBD_EVENT_FLAGS(0)
            };
            KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: code & 0xFF,
                dwFlags: KEYEVENTF_SCANCODE | ext | up,
                time: 0,
                dwExtraInfo: MAGIC,
            }
        };
        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki },
        }]);
    }
}
