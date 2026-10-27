//! Windows: low-level hooks for capture, SendInput for injection.

use super::{keymap, Backend, Decision};
use crate::layout::Monitor;
use crate::protocol::Button;
use crate::state::Shared;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
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
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Marks events we inject so our own hooks ignore them.
const MAGIC: usize = 0x4352_4F53;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
/// While another machine has the cursor, the real one is parked (hidden) here
/// and every hook event is measured against it to get the movement delta.
static PARK: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// Milliseconds (since START) of the last raw mouse report; 0 = none yet.
static LAST_RAW: AtomicU64 = AtomicU64::new(0);
static RAW_SEEN: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

// Keyboard watchdog. Raw input sees every physical key; if the low-level hook
// stops seeing them, Windows dropped the hook (it does so silently when a hook
// call is too slow) or an elevated window has the focus.
static HINST: AtomicIsize = AtomicIsize::new(0);
static KBD_HOOK: AtomicIsize = AtomicIsize::new(0);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
// Mouse watchdog, the same idea: physical movement that never reaches the
// hook means Windows dropped it, and a cursor parked for another machine
// would then be invisible on both.
static LAST_HOOK_MOUSE: AtomicU64 = AtomicU64::new(0);
static LAST_RAW_MOUSE: AtomicU64 = AtomicU64::new(0);
static MOUSE_MISSING: AtomicBool = AtomicBool::new(false);
/// When a probe key was injected to see whether the hook still gets any keys.
static SELFTEST_AT: AtomicU64 = AtomicU64::new(0);
static SELFTEST_SEEN: AtomicU64 = AtomicU64::new(0);
/// Our own window that had the focus when the cursor left; it gets it back on return.
static SET_ASIDE: AtomicIsize = AtomicIsize::new(0);
static LAST_HOOK_KEY: AtomicU64 = AtomicU64::new(0);
static LAST_RAW_KEY: AtomicU64 = AtomicU64::new(0);
static LAST_REINSTALL: AtomicU64 = AtomicU64::new(0);
static HOOK_MISSING: AtomicBool = AtomicBool::new(false);
/// Reported the first key sent since the cursor last left this machine.
static KEYS_ANNOUNCED: AtomicBool = AtomicBool::new(false);
/// A hook call slower than this risks Windows removing the hook.
const SLOW_HOOK_MS: u64 = 150;

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Raw input is flowing, so the push can be measured in pixels.
fn raw_alive() -> bool {
    let last = LAST_RAW.load(Ordering::Relaxed);
    last != 0 && now_ms() - last < 3000
}

pub fn init_process() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // A previous copy that died while another machine had the cursor left the
    // system cursors blank; put them back.
    HIDDEN.store(true, Ordering::SeqCst);
    show_cursor();
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
            HINST.store(hinst.0 as isize, Ordering::SeqCst);
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0);
            let kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinst, 0);
            if let Err(e) = mouse.as_ref().and(kbd.as_ref()) {
                *shared.capture_error.lock() = Some(format!("cannot install input hooks: {e}"));
                return;
            }
            if let Ok(h) = kbd {
                KBD_HOOK.store(h.0 as isize, Ordering::SeqCst);
            }
            if let Ok(h) = mouse {
                MOUSE_HOOK.store(h.0 as isize, Ordering::SeqCst);
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
        KEYS_ANNOUNCED.store(false, Ordering::Relaxed);
        crate::effects::burst(pt, crate::effects::Kind::Leave);
        step_aside_focus();
        // A file drag stays over the drop target that ends it later (see `crate::dnd`).
        let park = if crate::dnd::crossed() {
            pt
        } else {
            park_point(pt)
        };
        *PARK.lock() = Some(park);
        unsafe {
            let _ = SetCursorPos(park.0, park.1);
        }
        hide_cursor();
    }
    if let Some(pt) = d.release_to {
        warp(pt.0, pt.1);
        crate::effects::burst(pt, crate::effects::Kind::Arrive);
    }
    d.block
}

/// Show the swelling drop while the user pushes against an edge.
fn show_push(engine: &crate::engine::Engine) {
    let progress = engine.push_progress();
    if progress > 0.0 {
        if let Some(pt) = engine.push_point() {
            crate::effects::push(pt, progress);
        }
    }
}

/// Put the cursor at `(x, y)` and give it back to the user.
pub fn warp(x: i32, y: i32) {
    *PARK.lock() = None;
    unsafe {
        let _ = SetCursorPos(x, y);
    }
    show_cursor();
    restore_focus();
}

/// Keys meant for another machine went missing whenever one of our own windows
/// had the focus: the keyboard hook stops being called (seen in every case in
/// the field). While the cursor is away, the focus goes to the desktop.
/// Off the hook thread: focus changes wait on other threads.
fn step_aside_focus() {
    std::thread::spawn(|| unsafe {
        let fg = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if fg.0.is_null() || pid != windows::Win32::System::Threading::GetCurrentProcessId() {
            return;
        }
        SET_ASIDE.store(fg.0 as isize, Ordering::SeqCst);
        let _ = SetForegroundWindow(GetShellWindow());
    });
}

fn restore_focus() {
    let h = SET_ASIDE.swap(0, Ordering::SeqCst);
    if h == 0 {
        return;
    }
    std::thread::spawn(move || unsafe {
        let hwnd = HWND(h as _);
        if IsWindow(hwnd).as_bool() && GetForegroundWindow() == GetShellWindow() {
            let _ = SetForegroundWindow(hwnd);
        }
    });
}

/// Class name of the foreground window, to tell our own windows apart.
fn foreground_class() -> String {
    unsafe {
        let mut buf = [0u16; 128];
        let n = GetClassNameW(GetForegroundWindow(), &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
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

/// Log hook calls slow enough that Windows may drop the hook.
fn check_slow(which: &str, started: Instant) {
    let ms = started.elapsed().as_millis() as u64;
    if ms >= SLOW_HOOK_MS {
        if let Some(s) = shared() {
            s.diag(format!(
                "slow {which} hook call: {ms} ms (Windows drops hooks that stall)"
            ));
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let started = Instant::now();
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let injected = info.flags & LLMHF_INJECTED != 0 || info.dwExtraInfo == MAGIC;
        if !injected {
            LAST_HOOK_MOUSE.store(now_ms(), Ordering::Relaxed);
        }
        let block = !injected && handle_mouse(wparam.0 as u32, info);
        check_slow("mouse", started);
        if block {
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
            let mut engine = s.engine.lock();
            let mut d = engine.on_move(pt, delta);
            // Resting on the edge counts as the push only where raw input never
            // worked: an idle mouse's jitter must not send the cursor across.
            if delta.is_none() && d == Decision::default() && !RAW_SEEN.load(Ordering::Relaxed) {
                d = engine.on_edge_dwell();
            }
            drop(engine);
            if d.probe_drag {
                crate::dnd::probe(pt);
            }
            apply(d)
        }
        WM_LBUTTONDOWN => button(Button::Left, true),
        WM_LBUTTONUP => {
            let block = button(Button::Left, false);
            crate::dnd::local_released();
            block
        }
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
        let started = Instant::now();
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if info.dwExtraInfo == MAGIC && info.vkCode == VK_NONAME.0 as u32 {
            // The watchdog's probe key: proof the hook is still called.
            SELFTEST_SEEN.store(now_ms(), Ordering::Relaxed);
            return LRESULT(1);
        }
        LAST_HOOK_KEY.store(now_ms(), Ordering::Relaxed);
        let injected = info.flags.0 & LLKHF_INJECTED.0 != 0 || info.dwExtraInfo == MAGIC;
        let block = !injected && handle_key(wparam.0 as u32, info);
        check_slow("keyboard", started);
        if block {
            return LRESULT(1);
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

fn handle_key(msg: u32, info: &KBDLLHOOKSTRUCT) -> bool {
    let Some(s) = shared() else { return false };
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let target = s.engine.lock().active_target().map(str::to_string);
    let remote = target.is_some();
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
    let Some(hid) = hid else {
        if remote && down {
            s.diag(format!(
                "key not sent, no mapping: vk=0x{:02X} scan=0x{sc:02X} extended={extended}",
                info.vkCode
            ));
        }
        return remote;
    };
    if let Some(t) = &target {
        if down && !KEYS_ANNOUNCED.swap(true, Ordering::Relaxed) {
            let name = s
                .peers
                .lock()
                .get(t)
                .map_or_else(|| t.clone(), |p| p.name.clone());
            s.log(format!("[diag] sending keys to {name}"));
        }
    }
    let d = s.engine.lock().on_key(hid, down);
    apply(d)
}

/// Executable name of the foreground window's process, and whether it runs
/// elevated (its token cannot be opened from here). Elevated windows receive
/// neither our hook's view of the keyboard nor keys we inject.
fn foreground_process() -> Option<(String, bool)> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let name = if QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok()
        {
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            full.rsplit('\\').next().unwrap_or(&full).to_string()
        } else {
            format!("pid {pid}")
        };
        let mut token = HANDLE::default();
        let elevated = if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok() {
            let mut e = TOKEN_ELEVATION::default();
            let mut size = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut e as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
            .is_ok();
            let _ = CloseHandle(token);
            ok && e.TokenIsElevated != 0 && !self_elevated()
        } else {
            !self_elevated()
        };
        let _ = CloseHandle(process);
        Some((name, elevated))
    }
}

fn self_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| unsafe {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut e as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && e.TokenIsElevated != 0
    })
}

pub fn focused_app() -> Option<String> {
    foreground_process().map(|(name, _)| name)
}

pub fn injection_blocker() -> Option<String> {
    match foreground_process() {
        Some((name, true)) => Some(format!(
            "{name} runs as administrator, and Windows does not let a normal app type into it"
        )),
        _ => None,
    }
}

/// Called every second on the hook thread: raw input saw keys the hook did not.
fn check_keyboard_hook() {
    let Some(s) = shared() else { return };
    let (raw, hook, now) = (
        LAST_RAW_KEY.load(Ordering::Relaxed),
        LAST_HOOK_KEY.load(Ordering::Relaxed),
        now_ms(),
    );
    let probe = SELFTEST_AT.load(Ordering::Relaxed);
    if probe != 0 && now - probe > 300 {
        SELFTEST_AT.store(0, Ordering::Relaxed);
        let seen = SELFTEST_SEEN.load(Ordering::Relaxed) >= probe;
        s.diag(format!(
            "keyboard hook self-test: an injected key {} the hook",
            if seen { "still reaches" } else { "does not reach" }
        ));
    }
    if raw == 0 || now - raw < 300 {
        return; // nothing typed yet, or the hook may still be about to run
    }
    let missing = raw > hook + 300;
    if !missing {
        if HOOK_MISSING.swap(false, Ordering::Relaxed) {
            s.diag("keyboard hook sees keys again");
        }
        return;
    }
    let fg = foreground_process();
    let first = !HOOK_MISSING.swap(true, Ordering::Relaxed);
    if first {
        let fg_text = match &fg {
            Some((name, true)) => format!("{name} (administrator)"),
            Some((name, false)) => name.clone(),
            None => "unknown".into(),
        };
        let remote = s.engine.lock().active_target().is_some();
        s.diag(format!(
            "keyboard hook is not receiving keys (focused app: {fg_text}, window class: {}, \
             controlling another machine: {remote})",
            foreground_class()
        ));
        // Is the hook dead, or do only physical keys skip it?
        SELFTEST_AT.store(now, Ordering::Relaxed);
        let probe = |up: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_NONAME,
                    wScan: 0,
                    dwFlags: up,
                    time: 0,
                    dwExtraInfo: MAGIC,
                },
            },
        };
        send(&[probe(KEYBD_EVENT_FLAGS(0)), probe(KEYEVENTF_KEYUP)]);
    }
    if fg.is_some_and(|(_, elevated)| elevated) {
        return; // reinstalling cannot help while an elevated window has the focus
    }
    if now - LAST_REINSTALL.load(Ordering::Relaxed) < 5000 {
        return;
    }
    LAST_REINSTALL.store(now, Ordering::Relaxed);
    unsafe {
        let old = KBD_HOOK.load(Ordering::SeqCst);
        if old != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(old as _));
        }
        let hinst = HINSTANCE(HINST.load(Ordering::SeqCst) as _);
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinst, 0) {
            Ok(h) => {
                KBD_HOOK.store(h.0 as isize, Ordering::SeqCst);
                s.diag("keyboard hook reinstalled");
            }
            Err(e) => s.diag(format!("keyboard hook reinstall failed: {e}")),
        }
    }
}

/// Called every second on the hook thread: raw input saw the mouse move (or
/// click) but the hook did not. Put the hook back, and if another machine had
/// the cursor, bring it home and visible.
fn check_mouse_hook() {
    let Some(s) = shared() else { return };
    let (raw, hook, now) = (
        LAST_RAW_MOUSE.load(Ordering::Relaxed),
        LAST_HOOK_MOUSE.load(Ordering::Relaxed),
        now_ms(),
    );
    if raw == 0 || now - raw < 300 || raw <= hook + 500 {
        // No movement yet, the hook may still run, or it is keeping up.
        if hook > raw {
            MOUSE_MISSING.store(false, Ordering::Relaxed);
        }
        return;
    }
    let first = !MOUSE_MISSING.swap(true, Ordering::Relaxed);
    unsafe {
        let old = MOUSE_HOOK.load(Ordering::SeqCst);
        if old != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(old as _));
        }
        let hinst = HINSTANCE(HINST.load(Ordering::SeqCst) as _);
        match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0) {
            Ok(h) => MOUSE_HOOK.store(h.0 as isize, Ordering::SeqCst),
            Err(e) => s.diag(format!("mouse hook reinstall failed: {e}")),
        }
    }
    // Count from now, so one missed stretch is handled once.
    LAST_HOOK_MOUSE.store(now, Ordering::Relaxed);
    let back = s.engine.lock().force_local();
    match back {
        Some(pt) => {
            warp(pt.0, pt.1);
            s.diag("mouse hook stopped receiving the mouse; reinstalled it and brought the cursor back");
        }
        None => {
            // Not driving another machine; just make sure nothing stays hidden.
            *PARK.lock() = None;
            show_cursor();
            if first {
                let fg = foreground_process().map_or_else(|| "unknown".into(), |(n, e)| {
                    if e {
                        format!("{n} (administrator)")
                    } else {
                        n
                    }
                });
                s.diag(format!(
                    "mouse hook stopped receiving the mouse (focused app: {fg}); reinstalled it"
                ));
            }
        }
    }
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
    RegisterRawInputDevices(
        &[raw_device(hwnd, 0x02), raw_device(hwnd, 0x06)],
        std::mem::size_of::<RAWINPUTDEVICE>() as u32,
    )?;
    // Raw input is registered per process, and the UI toolkit registers its own
    // window later, taking the mouse over. Take it back every second.
    SetTimer(hwnd, 1, 1000, None);
    Ok(hwnd)
}

/// Generic desktop page: usage 0x02 is the mouse, 0x06 the keyboard.
fn raw_device(hwnd: HWND, usage: u16) -> RAWINPUTDEVICE {
    RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: usage,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    }
}

unsafe fn claim_raw_input(hwnd: HWND, usage: u16) {
    let _ = RegisterRawInputDevices(
        &[raw_device(hwnd, usage)],
        std::mem::size_of::<RAWINPUTDEVICE>() as u32,
    );
}

unsafe extern "system" fn raw_input_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        handle_raw(HRAWINPUT(lparam.0 as _));
    } else if msg == WM_TIMER {
        if !raw_alive() {
            claim_raw_input(hwnd, 0x02);
        }
        // Nothing in the UI reads raw keys, so the keyboard is simply kept.
        claim_raw_input(hwnd, 0x06);
        check_keyboard_hook();
        check_mouse_hook();
    }
    if msg == WM_INPUT && !RAW_SEEN.swap(true, Ordering::Relaxed) {
        if let Some(s) = shared() {
            s.log("raw mouse input is flowing (edge push measured in pixels)");
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn handle_raw(handle: HRAWINPUT) {
    let Some(s) = shared() else { return };
    let mut raw = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let n = GetRawInputData(
        handle,
        RID_INPUT,
        Some(&mut raw as *mut RAWINPUT as *mut _),
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    // Injected input has no device.
    if n == u32::MAX || raw.header.hDevice.0.is_null() {
        return;
    }
    if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
        LAST_RAW_KEY.store(now_ms(), Ordering::Relaxed);
        return;
    }
    // Absolute devices (tablets, remote desktop) cannot push.
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    LAST_RAW_MOUSE.store(now_ms(), Ordering::Relaxed);
    if PARK.lock().is_some() {
        return; // another machine has the cursor; the hook measures movement then
    }
    LAST_RAW.store(now_ms(), Ordering::Relaxed);
    let mouse = raw.data.mouse;
    if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 || (mouse.lLastX == 0 && mouse.lLastY == 0) {
        return;
    }
    let mut engine = s.engine.lock();
    let d = engine.on_push_delta(mouse.lLastX as f64, mouse.lLastY as f64);
    if d == Decision::default() {
        show_push(&engine);
    }
    drop(engine);
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
        let mm = physical_mm(&info.szDevice);
        let m = Monitor {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            name,
        };
        out.push(match mm {
            Some((w, h)) => m.with_physical_mm(w, h),
            None => m,
        });
    }
    BOOL(1)
}

/// Physical size of the monitor on adapter output `device` (e.g. \\.\DISPLAY1),
/// read from its EDID in the registry.
fn physical_mm(device: &[u16; 32]) -> Option<(f64, f64)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY};
    unsafe {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // EDD_GET_DEVICE_INTERFACE_NAME: DeviceID becomes \\?\DISPLAY#<model>#<instance>#{guid}
        if !EnumDisplayDevicesW(PCWSTR(device.as_ptr()), 0, &mut dd, 1).as_bool() {
            return None;
        }
        let len = dd
            .DeviceID
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(dd.DeviceID.len());
        let id = String::from_utf16_lossy(&dd.DeviceID[..len]);
        let mut parts = id.trim_start_matches("\\\\?\\").split('#');
        let (_, model, instance) = (parts.next()?, parts.next()?, parts.next()?);
        let key: Vec<u16> = format!(
            "SYSTEM\\CurrentControlSet\\Enum\\DISPLAY\\{model}\\{instance}\\Device Parameters"
        )
        .encode_utf16()
        .chain([0])
        .collect();
        let value: Vec<u16> = "EDID".encode_utf16().chain([0]).collect();
        let mut edid = [0u8; 256];
        let mut size = edid.len() as u32;
        let err = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(edid.as_mut_ptr().cast()),
            Some(&mut size),
        );
        if err.is_err() || size < 128 {
            return None;
        }
        // First detailed timing descriptor has the size in mm; bytes 21-22 are whole cm.
        let w = edid[66] as u32 | ((edid[68] as u32 & 0xF0) << 4);
        let h = edid[67] as u32 | ((edid[68] as u32 & 0x0F) << 8);
        if w > 0 && h > 0 {
            Some((w as f64, h as f64))
        } else if edid[21] > 0 && edid[22] > 0 {
            Some((edid[21] as f64 * 10.0, edid[22] as f64 * 10.0))
        } else {
            None
        }
    }
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
