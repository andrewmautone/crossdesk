//! Files and images sent to another machine over its connection: an `Offer`
//! lists the items, their bytes follow as `Chunk`s in item order, then a
//! `TransferEnd`. Each send runs on its own thread and queues on the
//! connection's bulk lane, behind input; one thread writes what arrives.

use crate::protocol::{Item, Msg, Offer, TransferKind};
use crate::state::Shared;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Copied files above this total are left to dragging (they would travel on every crossing).
pub const CLIP_FILES_MAX: u64 = 500 * 1024 * 1024;
const CHUNK: usize = 32 * 1024;
/// Received files are kept this long for pasting and dropping, then removed.
const KEEP_INCOMING: Duration = Duration::from_secs(24 * 3600);

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static INCOMING: OnceLock<Mutex<Sender<(String, Msg)>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
/// Latest clipboard transfer per peer; an older one still sending gives up.
static CLIP_LATEST: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

pub fn start(shared: Arc<Shared>) {
    let _ = SHARED.set(shared.clone());
    NEXT_ID.store(rand::random::<u64>() >> 1, Ordering::Relaxed);
    let (tx, rx) = channel::<(String, Msg)>();
    let _ = INCOMING.set(Mutex::new(tx));
    std::thread::Builder::new()
        .name("transfer-in".into())
        .spawn(move || {
            remove_old_incoming();
            let mut open: HashMap<(String, u64), Receiving> = HashMap::new();
            for (peer, msg) in rx {
                receive(&shared, &mut open, peer, msg);
            }
        })
        .expect("spawn transfer thread");
}

pub fn new_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// The peer's build knows the transfer messages. Older builds drop the
/// connection on a message they cannot decode, so nothing is sent to them.
pub fn supports(peer: &str) -> bool {
    if cfg!(test) {
        return true;
    }
    let Some(s) = SHARED.get() else { return false };
    let Some(theirs) = s.peers.lock().get(peer).and_then(|p| p.build) else {
        return false;
    };
    match crate::updater::build() {
        // Development builds only talk files with each other.
        0 => theirs == 0,
        mine => theirs >= mine,
    }
}

/// Hand a transfer message from the network to the writer thread.
pub fn incoming(peer: &str, msg: Msg) {
    if let Some(tx) = INCOMING.get() {
        let _ = tx.lock().send((peer.to_string(), msg));
    }
}

enum Source {
    File(PathBuf),
    Bytes(Arc<Vec<u8>>),
}

/// Send files and folders (recursively) in the background.
pub fn send_paths(peer: &str, id: u64, kind: TransferKind, paths: Vec<PathBuf>) {
    let peer = peer.to_string();
    spawn_send(move || {
        let (items, sources) = collect(&paths);
        if items.is_empty() {
            return None;
        }
        let total: u64 = items.iter().map(|i| i.size).sum();
        if kind == TransferKind::ClipFiles && total > CLIP_FILES_MAX {
            if let Some(s) = SHARED.get() {
                s.log(format!(
                    "copied files not shared: {} is over the {} limit for the clipboard; \
                     drag them across instead",
                    human_size(total),
                    human_size(CLIP_FILES_MAX)
                ));
            }
            return None;
        }
        Some((peer, id, kind, items, sources))
    });
}

/// Send one in-memory file (a copied image as PNG).
pub fn send_bytes(peer: &str, kind: TransferKind, name: &str, bytes: Vec<u8>) {
    let item = Item {
        path: name.to_string(),
        size: bytes.len() as u64,
        dir: false,
    };
    let peer = peer.to_string();
    spawn_send(move || {
        Some((
            peer,
            new_id(),
            kind,
            vec![item],
            vec![Some(Source::Bytes(Arc::new(bytes)))],
        ))
    });
}

type Prepared = (String, u64, TransferKind, Vec<Item>, Vec<Option<Source>>);

fn spawn_send(prepare: impl FnOnce() -> Option<Prepared> + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("transfer-out".into())
        .spawn(move || {
            let Some(s) = SHARED.get() else { return };
            let Some((peer, id, kind, items, sources)) = prepare() else {
                return;
            };
            let clip = kind != TransferKind::Drag;
            if clip {
                CLIP_LATEST
                    .lock()
                    .get_or_insert_with(HashMap::new)
                    .insert(peer.clone(), id);
            }
            let superseded = || {
                clip && CLIP_LATEST
                    .lock()
                    .as_ref()
                    .and_then(|m| m.get(&peer))
                    .is_some_and(|&latest| latest != id)
            };
            let total: u64 = items.iter().map(|i| i.size).sum();
            let files = items.iter().filter(|i| !i.dir).count();
            let started = Instant::now();
            if !s.registry.send_bulk(&peer, Msg::Offer(Offer { id, kind, items })) {
                return;
            }
            let mut ok = true;
            let mut buf = vec![0u8; CHUNK];
            'items: for source in sources.into_iter().flatten() {
                match source {
                    Source::Bytes(bytes) => {
                        for part in bytes.chunks(CHUNK) {
                            let chunk = Msg::Chunk {
                                id,
                                data: part.to_vec(),
                            };
                            if superseded() || !s.registry.send_bulk(&peer, chunk) {
                                ok = false;
                                break 'items;
                            }
                        }
                    }
                    Source::File(path) => {
                        let Ok(mut f) = File::open(&path) else {
                            ok = false;
                            break;
                        };
                        loop {
                            let n = match f.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => n,
                                Err(_) => {
                                    ok = false;
                                    break 'items;
                                }
                            };
                            let chunk = Msg::Chunk {
                                id,
                                data: buf[..n].to_vec(),
                            };
                            if superseded() || !s.registry.send_bulk(&peer, chunk) {
                                ok = false;
                                break 'items;
                            }
                        }
                    }
                }
            }
            let _ = s.registry.send_bulk(&peer, Msg::TransferEnd { id, ok });
            if kind != TransferKind::ClipImage {
                let name = peer_name(s, &peer);
                if ok {
                    s.log(format!(
                        "sent {} to {name} in {:.1} s",
                        describe(files, total),
                        started.elapsed().as_secs_f32()
                    ));
                } else {
                    s.log(format!("sending {} to {name} stopped", describe(files, total)));
                }
            }
        });
}

/// Items for `paths` (folders walked, links skipped) and where each file's bytes come from.
fn collect(paths: &[PathBuf]) -> (Vec<Item>, Vec<Option<Source>>) {
    let mut items = Vec::new();
    let mut sources = Vec::new();
    let mut taken = std::collections::HashSet::new();
    for p in paths {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let name = unique_name(&name, |n| taken.contains(n));
        taken.insert(name.clone());
        walk(p, &name, &mut items, &mut sources);
    }
    (items, sources)
}

fn walk(path: &Path, rel: &str, items: &mut Vec<Item>, sources: &mut Vec<Option<Source>>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        items.push(Item {
            path: rel.to_string(),
            size: 0,
            dir: true,
        });
        sources.push(None);
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for c in children {
            if let Some(n) = c.file_name() {
                walk(&c, &format!("{rel}/{}", n.to_string_lossy()), items, sources);
            }
        }
    } else if meta.is_file() {
        items.push(Item {
            path: rel.to_string(),
            size: meta.len(),
            dir: false,
        });
        sources.push(Some(Source::File(path.to_path_buf())));
    }
}

struct Receiving {
    kind: TransferKind,
    root: PathBuf,
    items: Vec<Item>,
    /// Index of the next item to open.
    next: usize,
    file: Option<File>,
    /// Bytes still expected for the open file.
    left: u64,
    failed: bool,
}

impl Receiving {
    /// Open the next file that still expects bytes; empty files are created on the way.
    fn advance(&mut self) -> bool {
        self.file = None;
        while self.next < self.items.len() {
            let item = &self.items[self.next];
            self.next += 1;
            if item.dir {
                continue;
            }
            let Ok(f) = File::create(self.root.join(&item.path)) else {
                return false;
            };
            if item.size > 0 {
                self.file = Some(f);
                self.left = item.size;
                return true;
            }
        }
        false
    }

    fn write(&mut self, data: &[u8]) -> bool {
        if self.left == 0 && !self.advance() {
            return false;
        }
        if data.len() as u64 > self.left {
            return false;
        }
        let Some(f) = self.file.as_mut() else {
            return false;
        };
        if f.write_all(data).is_err() {
            return false;
        }
        self.left -= data.len() as u64;
        true
    }

    /// All bytes arrived: create trailing empty files and close.
    fn finish(&mut self) -> bool {
        if self.left > 0 {
            return false;
        }
        while self.advance() {
            if self.left > 0 {
                return false;
            }
        }
        true
    }

    fn top_level(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .filter(|i| !i.path.contains('/'))
            .map(|i| self.root.join(&i.path))
            .collect()
    }
}

fn receive(s: &Arc<Shared>, open: &mut HashMap<(String, u64), Receiving>, peer: String, msg: Msg) {
    match msg {
        Msg::Offer(offer) => {
            let Some(items) = safe_items(offer.items) else {
                s.log(format!(
                    "refused files from {}: unsafe file names",
                    peer_name(s, &peer)
                ));
                return;
            };
            let root = incoming_root().join(format!("{:x}", offer.id));
            let _ = std::fs::remove_dir_all(&root);
            if std::fs::create_dir_all(&root).is_err() {
                return;
            }
            for i in items.iter().filter(|i| i.dir) {
                let _ = std::fs::create_dir_all(root.join(&i.path));
            }
            if offer.kind != TransferKind::ClipImage {
                let total = items.iter().map(|i| i.size).sum();
                let files = items.iter().filter(|i| !i.dir).count();
                s.log(format!(
                    "receiving {} from {}",
                    describe(files, total),
                    peer_name(s, &peer)
                ));
            }
            open.insert(
                (peer, offer.id),
                Receiving {
                    kind: offer.kind,
                    root,
                    items,
                    next: 0,
                    file: None,
                    left: 0,
                    failed: false,
                },
            );
        }
        Msg::Chunk { id, data } => {
            if let Some(r) = open.get_mut(&(peer, id)) {
                if !r.failed && !r.write(&data) {
                    r.failed = true;
                }
            }
        }
        Msg::TransferEnd { id, ok } => {
            let Some(mut r) = open.remove(&(peer.clone(), id)) else {
                return;
            };
            let complete = ok && !r.failed && r.finish();
            r.file = None;
            if !complete {
                let _ = std::fs::remove_dir_all(&r.root);
                if r.kind == TransferKind::Drag {
                    crate::dnd::arrived(&peer, id, None);
                }
                if r.kind != TransferKind::ClipImage {
                    s.log(format!("files from {} did not arrive", peer_name(s, &peer)));
                }
                return;
            }
            let paths = r.top_level();
            match r.kind {
                TransferKind::ClipImage => {
                    if let Some(p) = paths.first() {
                        s.clipboard.set_image_png(p.clone());
                    }
                }
                TransferKind::ClipFiles => s.clipboard.set_files(paths),
                TransferKind::Drag => crate::dnd::arrived(&peer, id, Some(paths)),
            }
        }
        _ => {}
    }
}

/// Item paths that stay inside the transfer folder, with characters this OS
/// cannot store in a name replaced.
fn safe_items(items: Vec<Item>) -> Option<Vec<Item>> {
    items
        .into_iter()
        .map(|mut i| {
            let parts: Option<Vec<String>> = i.path.split('/').map(safe_component).collect();
            i.path = parts?.join("/");
            Some(i)
        })
        .collect()
}

fn safe_component(c: &str) -> Option<String> {
    if c.is_empty() || c == "." || c == ".." {
        return None;
    }
    let mut out: String = c
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*' => '_',
            ch if (ch as u32) < 32 => '_',
            ch => ch,
        })
        .collect();
    if cfg!(windows) {
        // Windows silently drops trailing dots and spaces.
        while out.ends_with('.') || out.ends_with(' ') {
            out.pop();
        }
        if out.is_empty() {
            out.push('_');
        }
    }
    Some(out)
}

pub fn incoming_root() -> PathBuf {
    std::env::temp_dir().join("crossdesk-incoming")
}

fn remove_old_incoming() {
    let Ok(entries) = std::fs::read_dir(incoming_root()) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > KEEP_INCOMING);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Move received files into Downloads (renaming on clashes); returns where they went.
pub fn move_to_downloads(paths: &[PathBuf]) -> Vec<PathBuf> {
    let dir = directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(Path::to_path_buf))
        .or_else(|| directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    let mut out = Vec::new();
    for p in paths {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let name = unique_name(&name, |n| dir.join(n).exists());
        let to = dir.join(name);
        if std::fs::rename(p, &to).is_ok() || copy_all(p, &to).is_ok() {
            out.push(to);
        }
    }
    out
}

fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            copy_all(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// `name`, or `name (2)`, `name (3)`... with the extension kept, whichever is free.
fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|n| !taken(n))
        .unwrap()
}

/// Show `path` selected in Explorer / Finder.
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path);
        let _ = std::process::Command::new("explorer.exe").arg(arg).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = path;
}

fn peer_name(s: &Shared, peer: &str) -> String {
    s.peers
        .lock()
        .get(peer)
        .map_or_else(|| peer.to_string(), |p| p.name.clone())
}

fn describe(files: usize, bytes: u64) -> String {
    let what = if files == 1 {
        "1 file".to_string()
    } else {
        format!("{files} files")
    };
    format!("{what} ({})", human_size(bytes))
}

pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_leave_the_folder() {
        let item = |p: &str| Item {
            path: p.into(),
            size: 0,
            dir: false,
        };
        assert!(safe_items(vec![item("../evil")]).is_none());
        assert!(safe_items(vec![item("a/../../evil")]).is_none());
        assert!(safe_items(vec![item("/abs")]).is_none());
        assert!(safe_items(vec![item("a//b")]).is_none());
        let ok = safe_items(vec![item("C:\\x"), item("dir/f?.txt")]).unwrap();
        assert_eq!(ok[0].path, "C__x");
        assert_eq!(ok[1].path, "dir/f_.txt");
    }

    #[test]
    fn unique_names_keep_the_extension() {
        let taken = ["a.txt", "a (2).txt"];
        assert_eq!(unique_name("a.txt", |n| taken.contains(&n)), "a (3).txt");
        assert_eq!(unique_name("b", |n| taken.contains(&n)), "b");
        assert_eq!(unique_name(".env", |n| n == ".env"), ".env (2)");
    }

    #[test]
    fn writes_items_in_order_and_creates_empty_files() {
        let root = std::env::temp_dir().join(format!("crossdesk-test-{}", rand::random::<u32>()));
        std::fs::create_dir_all(root.join("d")).unwrap();
        let mut r = Receiving {
            kind: TransferKind::Drag,
            root: root.clone(),
            items: vec![
                Item {
                    path: "d".into(),
                    size: 0,
                    dir: true,
                },
                Item {
                    path: "d/a".into(),
                    size: 5,
                    dir: false,
                },
                Item {
                    path: "d/empty".into(),
                    size: 0,
                    dir: false,
                },
                Item {
                    path: "b".into(),
                    size: 3,
                    dir: false,
                },
                Item {
                    path: "tail".into(),
                    size: 0,
                    dir: false,
                },
            ],
            next: 0,
            file: None,
            left: 0,
            failed: false,
        };
        assert!(r.write(b"hel"));
        assert!(r.write(b"lo"));
        assert!(r.write(b"xyz"));
        assert!(r.finish());
        r.file = None;
        assert_eq!(std::fs::read(root.join("d/a")).unwrap(), b"hello");
        assert_eq!(std::fs::read(root.join("b")).unwrap(), b"xyz");
        assert!(root.join("d/empty").exists() && root.join("tail").exists());
        assert_eq!(r.top_level(), vec![root.join("d"), root.join("b"), root.join("tail")]);
        // More bytes than announced fail.
        assert!(!r.write(b"!"));
        let _ = std::fs::remove_dir_all(root);
    }
}
