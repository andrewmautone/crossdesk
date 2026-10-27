//! Dragging files from one machine and dropping them on another.
//!
//! Out: while the left button holds a file drag against an edge, the platform
//! layer finds which files are being dragged (`set_local`); the engine then
//! crosses, sends `DragStart` and the files as a `Drag` transfer, and keeps the
//! button held on the other machine. When the button is released over there,
//! the drag still running here is cancelled (`local_released`).
//!
//! In: `DragStart` marks the button as held. Once the files arrive, a real
//! drag starts under the cursor, so the release drops them wherever the user
//! points: a folder, the desktop, an app. Released before they arrived, or a
//! drop nobody took, and they go to Downloads instead.

use crate::protocol::TransferKind;
use crate::state::Shared;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use self::macos as platform;

#[cfg(not(any(windows, target_os = "macos")))]
mod other;
#[cfg(not(any(windows, target_os = "macos")))]
use self::other as platform;

#[cfg(target_os = "macos")]
pub use self::macos::{note_mouse_down, probe_now};
#[cfg(windows)]
pub use self::windows::probe;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
/// Files of the drag the user is holding on this machine.
static LOCAL: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);
/// That drag crossed to another machine; the local one must be cancelled on release.
static CROSSED: AtomicBool = AtomicBool::new(false);
/// A drag arriving from the machine that controls this one.
static INCOMING: Mutex<Option<Incoming>> = Mutex::new(None);

struct Incoming {
    peer: String,
    id: u64,
    /// The button was released (or control left) before the drag could start.
    released: bool,
    /// The local drag is running; the release is passed through as the drop.
    started: bool,
}

pub fn start(shared: Arc<Shared>) {
    let _ = SHARED.set(shared.clone());
    platform::start(shared);
}

fn enabled() -> bool {
    SHARED
        .get()
        .is_some_and(|s| s.config.lock().settings.drag_files)
}

/// The platform found the files under a drag held here (None: not a file drag).
pub fn set_local(paths: Option<Vec<PathBuf>>) {
    let found = paths.as_ref().is_some_and(|p| !p.is_empty());
    *LOCAL.lock() = paths.filter(|p| !p.is_empty());
    if let Some(s) = SHARED.get() {
        s.engine.lock().set_file_drag(found);
    }
}

/// Engine: the cursor crosses to `peer` while dragging files. Returns the drag id.
pub fn outgoing(peer: &str) -> Option<u64> {
    // Whatever happens next, the drag held here ends when the button is released over there.
    CROSSED.store(true, Ordering::SeqCst);
    let paths = LOCAL.lock().clone()?;
    if !enabled() || !crate::transfer::supports(peer) {
        return None;
    }
    let id = crate::transfer::new_id();
    crate::transfer::send_paths(peer, id, TransferKind::Drag, paths);
    Some(id)
}

/// Platform: the physical left button went up. If the drag had crossed, the
/// drop happened on the other machine, so the drag still open here is cancelled.
pub fn local_released() {
    if LOCAL.lock().take().is_some() {
        if let Some(s) = SHARED.get() {
            s.engine.lock().set_file_drag(false);
        }
    }
    let crossed = CROSSED.swap(false, Ordering::SeqCst);
    platform::local_drag_over(crossed);
}

/// The local drag crossed and is waiting for its release on the other machine.
pub fn crossed() -> bool {
    CROSSED.load(Ordering::SeqCst)
}

/// `DragStart` from the machine in control: its button is held, with files on the way.
pub fn drag_start(peer: &str, id: u64) {
    *INCOMING.lock() = Some(Incoming {
        peer: peer.to_string(),
        id,
        released: false,
        started: false,
    });
}

/// The controller released the left button or left this machine.
pub fn controller_released(peer: &str) {
    let mut inc = INCOMING.lock();
    match inc.as_mut() {
        Some(i) if i.peer == peer && !i.started => i.released = true,
        Some(i) if i.peer == peer => {} // the release is the drop of the running drag
        _ => {}
    }
}

/// The `Drag` transfer finished (`None`: it failed).
pub fn arrived(peer: &str, id: u64, paths: Option<Vec<PathBuf>>) {
    let Some(s) = SHARED.get() else { return };
    let mut inc = INCOMING.lock();
    let mine = inc.as_ref().is_some_and(|i| i.peer == peer && i.id == id);
    let Some(paths) = paths else {
        if mine {
            *inc = None;
        }
        return;
    };
    let still_held = mine
        && !inc.as_ref().unwrap().released
        && s.controlled_by.lock().as_deref() == Some(peer);
    if !still_held {
        if mine {
            *inc = None;
        }
        drop(inc);
        to_downloads(s, &paths);
        return;
    }
    inc.as_mut().unwrap().started = true;
    drop(inc);
    platform::begin_drag(paths);
}

/// The drag started by `begin_drag` ended; `dropped` if something took the files.
pub fn drag_finished(paths: Vec<PathBuf>, dropped: bool) {
    INCOMING.lock().take();
    let Some(s) = SHARED.get() else { return };
    if dropped {
        s.log(format!("dropped {} from the other computer", count(&paths)));
    } else {
        to_downloads(s, &paths);
    }
}

fn to_downloads(s: &Shared, paths: &[PathBuf]) {
    let moved = crate::transfer::move_to_downloads(paths);
    if let Some(first) = moved.first() {
        s.log(format!(
            "saved {} to {}",
            count(&moved),
            first.parent().map_or_else(String::new, |p| p.display().to_string())
        ));
        crate::transfer::reveal(first);
    }
}

fn count(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one
            .file_name()
            .map_or_else(|| "1 item".into(), |n| n.to_string_lossy().into_owned()),
        many => format!("{} items", many.len()),
    }
}
