//! Windows: a tiny OLE drop target slipped under the cursor tells which files
//! Explorer is dragging, and a shell drag (SHDoDragDrop) carries files that
//! arrive from another machine to wherever the user drops them.

use crate::input::{Backend, Platform};
use crate::protocol::Button;
use crate::state::Shared;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock};
use windows::core::{implement, w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, POINTL, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::{
    IBindCtx, IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    IDropSource, IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium, CF_HDROP,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, DragQueryFileW, ILFree, IShellItemArray, SHCreateShellItemArrayFromIDLists,
    SHDoDragDrop, SHParseDisplayName, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_PROBE: u32 = WM_APP + 1;
const WM_PROBE_DONE: u32 = WM_APP + 2;
const WM_BEGIN: u32 = WM_APP + 3;
const TIMER_HIDE: usize = 1;
const TIMER_GIVE_UP: usize = 2;
/// Side of the invisible windows, in pixels.
const SIDE: i32 = 24;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static TARGET: AtomicIsize = AtomicIsize::new(0);
static SOURCE: AtomicIsize = AtomicIsize::new(0);
static PROBE_AT: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// Probed once per press of the button.
static PROBED: AtomicBool = AtomicBool::new(false);
/// Files waiting for the injected button press that starts their drag.
static BEGIN: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);

fn hwnd(slot: &AtomicIsize) -> HWND {
    HWND(slot.load(Ordering::SeqCst) as _)
}

pub fn start(shared: Arc<Shared>) {
    let _ = SHARED.set(shared.clone());
    let _ = std::thread::Builder::new()
        .name("dnd".into())
        .spawn(move || unsafe {
            if OleInitialize(None).is_err() {
                shared.log("drag and drop unavailable: OLE did not start");
                return;
            }
            let Ok(target) = window(w!("crossdesk-drop-target"), Some(target_proc)) else {
                return;
            };
            let Ok(source) = window(w!("crossdesk-drag-source"), Some(source_proc)) else {
                return;
            };
            TARGET.store(target.0 as isize, Ordering::SeqCst);
            SOURCE.store(source.0 as isize, Ordering::SeqCst);
            let drop_target: IDropTarget = DropTarget.into();
            if let Err(e) = RegisterDragDrop(target, &drop_target) {
                shared.log(format!("dragging files out unavailable: {e}"));
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
}

/// A hidden, nearly transparent popup: hit-testable (unlike fully transparent
/// windows), never activated, above everything.
unsafe fn window(class: PCWSTR, proc_: WNDPROC) -> windows::core::Result<HWND> {
    let hinst = GetModuleHandleW(None)?;
    let wc = WNDCLASSW {
        lpfnWndProc: proc_,
        hInstance: hinst.into(),
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class,
        w!(""),
        WS_POPUP,
        0,
        0,
        SIDE,
        SIDE,
        None,
        None,
        hinst,
        None,
    )?;
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA)?;
    Ok(hwnd)
}

/// Show `hwnd` centred on `pt`, kept inside that screen so it stays under the cursor.
unsafe fn show_at(hwnd: HWND, pt: (i32, i32)) {
    let mon = MonitorFromPoint(POINT { x: pt.0, y: pt.1 }, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let (mut x, mut y) = (pt.0 - SIDE / 2, pt.1 - SIDE / 2);
    if GetMonitorInfoW(mon, &mut info).as_bool() {
        let r = info.rcMonitor;
        x = x.clamp(r.left, r.right - SIDE);
        y = y.clamp(r.top, r.bottom - SIDE);
    }
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        x,
        y,
        SIDE,
        SIDE,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
}

/// Hook thread: the left button holds a drag against an edge to another machine.
/// Put the drop target under the cursor; Explorer reports its files in DragEnter.
pub fn probe(pt: (i32, i32)) {
    if PROBED.swap(true, Ordering::SeqCst) || !super::enabled() {
        return;
    }
    *PROBE_AT.lock() = Some(pt);
    unsafe {
        let _ = PostMessageW(hwnd(&TARGET), WM_PROBE, WPARAM(0), LPARAM(0));
    }
}

pub fn local_drag_over(crossed: bool) {
    let probed = PROBED.swap(false, Ordering::SeqCst);
    if crossed {
        // The cursor is parked on our drop target: releasing there ends the
        // Explorer drag with nothing dropped.
        Platform.button(Button::Left, false);
    }
    if !probed && !crossed {
        return;
    }
    unsafe {
        let _ = PostMessageW(hwnd(&TARGET), WM_PROBE_DONE, WPARAM(0), LPARAM(0));
    }
}

pub fn begin_drag(paths: Vec<PathBuf>) {
    *BEGIN.lock() = Some(paths);
    unsafe {
        let _ = PostMessageW(hwnd(&SOURCE), WM_BEGIN, WPARAM(0), LPARAM(0));
    }
}

unsafe extern "system" fn target_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PROBE => {
            let Some(pt) = PROBE_AT.lock().take() else {
                return LRESULT(0);
            };
            let _ = KillTimer(h, TIMER_HIDE);
            show_at(h, pt);
            // The drag only looks under the cursor when the mouse moves.
            let mut p = Platform;
            p.move_abs(pt.0 - 1, pt.1);
            p.move_abs(pt.0 + 1, pt.1);
            p.move_abs(pt.0, pt.1);
            LRESULT(0)
        }
        WM_PROBE_DONE => {
            // Give a pending drop on us a moment before the window goes away.
            SetTimer(h, TIMER_HIDE, 400, None);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_HIDE => {
            let _ = KillTimer(h, TIMER_HIDE);
            let _ = ShowWindow(h, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

unsafe extern "system" fn source_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_BEGIN => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            show_at(h, (pt.x, pt.y));
            // Press the button on our own window; its WM_LBUTTONDOWN starts the drag.
            if let Some(s) = SHARED.get() {
                s.injector.lock().button(Button::Left, true);
            }
            SetTimer(h, TIMER_GIVE_UP, 1500, None);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = KillTimer(h, TIMER_GIVE_UP);
            let Some(paths) = BEGIN.lock().take() else {
                return LRESULT(0);
            };
            let effect = data_object(&paths)
                .and_then(|data| SHDoDragDrop(h, &data, None::<&IDropSource>, DROPEFFECT_COPY));
            let _ = ShowWindow(h, SW_HIDE);
            let dropped = effect.is_ok_and(|e| e != DROPEFFECT_NONE);
            super::drag_finished(paths, dropped);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_GIVE_UP => {
            // The press never reached us: let go and save the files instead.
            let _ = KillTimer(h, TIMER_GIVE_UP);
            let _ = ShowWindow(h, SW_HIDE);
            if let Some(paths) = BEGIN.lock().take() {
                if let Some(s) = SHARED.get() {
                    s.injector.lock().button(Button::Left, false);
                }
                super::drag_finished(paths, false);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// The shell's own data object for these files, as Explorer would drag them.
unsafe fn data_object(paths: &[PathBuf]) -> windows::core::Result<IDataObject> {
    let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
    let mut result = Ok(());
    for p in paths {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let name = HSTRING::from(p.as_os_str());
        match SHParseDisplayName(&name, None::<&IBindCtx>, &mut pidl, 0, None) {
            Ok(()) => pidls.push(pidl),
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    let array: windows::core::Result<IShellItemArray> =
        result.and_then(|()| SHCreateShellItemArrayFromIDLists(&pidls));
    for p in pidls {
        ILFree(Some(p));
    }
    array?.BindToHandler(None::<&IBindCtx>, &BHID_DataObject)
}

/// Paths in a CF_HDROP data object.
unsafe fn dropped_files(data: &IDataObject) -> Option<Vec<PathBuf>> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = data.GetData(&format).ok()?;
    let drop = HDROP(medium.u.hGlobal.0);
    let n = DragQueryFileW(drop, u32::MAX, None);
    let mut out = Vec::new();
    for i in 0..n {
        let len = DragQueryFileW(drop, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
        use std::os::windows::ffi::OsStringExt;
        out.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..got])));
    }
    ReleaseStgMedium(&mut medium);
    Some(out)
}

#[implement(IDropTarget)]
struct DropTarget;

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data: Option<&IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let files = data.and_then(|d| unsafe { dropped_files(d) });
        let any = files.as_ref().is_some_and(|f| !f.is_empty());
        super::set_local(files);
        unsafe {
            *effect = if any { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *effect = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        _data: Option<&IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // Released here, not on the other machine: nothing to do.
        unsafe {
            *effect = DROPEFFECT_NONE;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_data_object_carries_the_files() {
        let dir = std::env::temp_dir().join(format!("crossdesk-dnd-{}", rand::random::<u32>()));
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        std::fs::write(dir.join("a file.txt"), b"x").unwrap();
        let paths = vec![dir.join("a file.txt"), dir.join("folder")];
        unsafe {
            OleInitialize(None).unwrap();
            let data = data_object(&paths).unwrap();
            // The shell may hand back short (8.3) names for the same files.
            let canon = |v: Vec<PathBuf>| -> Vec<PathBuf> {
                v.iter().map(|p| std::fs::canonicalize(p).unwrap()).collect()
            };
            assert_eq!(canon(dropped_files(&data).unwrap()), canon(paths.clone()));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
