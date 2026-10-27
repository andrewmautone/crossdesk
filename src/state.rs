//! State shared by the UI, the network tasks and the input hooks.

use crate::clipboard::Clipboard;
use crate::config::{Config, Settings, TrustedPeer};
use crate::engine::Engine;
use crate::input::{self, keymap, Injector};
use crate::layout::{self, Layout, Monitor, Placement, World};
use crate::net::Registry;
use crate::protocol::{this_os, Msg};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

pub struct Discovered {
    pub name: String,
    pub os: String,
    pub addr: SocketAddr,
    pub last_seen: Instant,
}

pub struct PeerInfo {
    pub conn_id: u64,
    pub name: String,
    pub os: String,
    pub monitors: Vec<Monitor>,
    /// Their build, once their `Info` arrived.
    pub build: Option<u64>,
}

pub struct IncomingPair {
    pub peer_name: String,
    pub code: String,
    pub since: Instant,
}

pub struct OutgoingPair {
    pub peer_name: String,
    pub code_tx: Option<oneshot::Sender<String>>,
    pub error: Option<String>,
}

pub struct Shared {
    pub rt: tokio::runtime::Handle,
    pub config: Mutex<Config>,
    pub registry: Arc<Registry>,
    pub engine: Mutex<Engine>,
    pub injector: Mutex<Injector>,
    pub clipboard: Clipboard,
    pub discovered: Mutex<HashMap<String, Discovered>>,
    pub peers: Mutex<HashMap<String, PeerInfo>>,
    pub local_monitors: Mutex<Vec<Monitor>>,
    pub incoming_pair: Mutex<Option<IncomingPair>>,
    pub outgoing_pair: Mutex<Option<OutgoingPair>>,
    pair_failures: Mutex<Vec<Instant>>,
    pub controlled_by: Mutex<Option<String>>,
    pub capture_error: Mutex<Option<String>>,
    pub log: Mutex<VecDeque<String>>,
    pub repaint: Mutex<Option<eframe::egui::Context>>,
    pub updater: OnceLock<crate::updater::Updater>,
    pub update_status: Mutex<String>,
    /// Last position the controlling machine put our cursor at.
    pub remote_cursor: Mutex<Option<(i32, i32)>>,
    /// Keyboard diagnostics for the machine controlling us.
    key_diag: Mutex<KeyDiag>,
    log_file: Mutex<Option<std::fs::File>>,
}

#[derive(Default)]
struct KeyDiag {
    /// Reported where the first key since the last Enter went.
    announced: bool,
    /// Something on this machine is swallowing injected keys.
    blocked: bool,
    /// Reported keys arriving from a machine that is not in control.
    ignored: bool,
}

/// `crossdesk.log` next to the config, restarted when it grows past 2 MB.
fn open_log_file() -> Option<std::fs::File> {
    let path = crate::config::path().with_file_name("crossdesk.log");
    let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 * 1024 * 1024);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!big)
        .write(true)
        .truncate(big)
        .open(path)
        .ok()
}

/// "2026-10-27 14:03:09 UTC" without pulling in a date crate.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Shared {
    pub fn new(rt: tokio::runtime::Handle) -> Arc<Shared> {
        Shared::with_config(rt, Config::load())
    }

    pub fn with_config(rt: tokio::runtime::Handle, config: Config) -> Arc<Shared> {
        let registry = Arc::new(Registry::default());
        let clipboard = Clipboard::start(registry.clone());
        let engine = Engine::new(
            config.id.clone(),
            config.settings.clone(),
            registry.clone(),
            clipboard.clone(),
        );
        // Test instances (ephemeral configs) keep out of the real log.
        let log_file = (!config.ephemeral).then(open_log_file).flatten();
        let shared = Arc::new(Shared {
            rt,
            config: Mutex::new(config),
            registry,
            engine: Mutex::new(engine),
            injector: Mutex::new(Injector::new()),
            clipboard,
            discovered: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            local_monitors: Mutex::new(input::monitors()),
            incoming_pair: Mutex::new(None),
            outgoing_pair: Mutex::new(None),
            pair_failures: Mutex::new(Vec::new()),
            controlled_by: Mutex::new(None),
            capture_error: Mutex::new(None),
            log: Mutex::new(VecDeque::new()),
            repaint: Mutex::new(None),
            updater: OnceLock::new(),
            update_status: Mutex::new(String::new()),
            remote_cursor: Mutex::new(None),
            key_diag: Mutex::new(KeyDiag::default()),
            log_file: Mutex::new(log_file),
        });
        shared.rebuild_world();
        shared
    }

    pub fn set_update_status(&self, status: String) {
        *self.update_status.lock() = status;
        self.repaint();
    }

    pub fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        eprintln!("{msg}");
        if let Some(f) = self.log_file.lock().as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{} {msg}", timestamp());
        }
        let mut log = self.log.lock();
        log.push_back(msg);
        while log.len() > 200 {
            log.pop_front();
        }
        drop(log);
        self.repaint();
    }

    /// Log a diagnostic here and in every connected machine's log.
    pub fn diag(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.log(format!("[diag] {msg}"));
        self.registry.broadcast(Msg::Diag(msg));
    }

    /// Log without spamming the UI with the same reconnect failure every few seconds.
    pub fn log_quiet(&self, msg: String) {
        if self.log.lock().back() != Some(&msg) {
            self.log(msg);
        }
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.repaint.lock().as_ref() {
            ctx.request_repaint();
        }
    }

    pub fn info_msg(&self) -> Msg {
        Msg::Info {
            name: self.config.lock().name.clone(),
            os: this_os().into(),
            build: crate::updater::build(),
            monitors: self.local_monitors.lock().clone(),
        }
    }

    /// Machines currently on the canvas: this one plus every connected peer that reported monitors.
    pub fn machines(&self) -> Vec<(String, Vec<Monitor>)> {
        let id = self.config.lock().id.clone();
        let mut out = vec![(id, self.local_monitors.lock().clone())];
        let peers = self.peers.lock();
        let mut ids: Vec<&String> = peers.keys().collect();
        ids.sort();
        for pid in ids {
            let p = &peers[pid];
            if !p.monitors.is_empty() {
                out.push((pid.clone(), p.monitors.clone()));
            }
        }
        out
    }

    /// Give every machine without a placement one to the right of the others,
    /// scaled so its main monitor is as tall as ours. Returns true if anything changed.
    fn place_new_machines(&self, layout: &mut Layout, machines: &[(String, Vec<Monitor>)]) -> bool {
        let mut changed = false;
        let local_h = machines
            .first()
            .and_then(|(_, m)| m.iter().find(|m| m.primary).or(m.first()))
            .map_or(1080.0, |m| m.h as f64);
        for (i, (id, mons)) in machines.iter().enumerate() {
            if layout.placements.contains_key(id) || mons.is_empty() {
                continue;
            }
            let scale = if i == 0 {
                1.0
            } else {
                let h = mons
                    .iter()
                    .find(|m| m.primary)
                    .or(mons.first())
                    .map_or(1.0, |m| m.h as f64);
                (local_h / h.max(1.0) * 100.0).round() / 100.0
            };
            let world = World::build(machines, layout);
            let right = world
                .monitors
                .iter()
                .map(|m| m.rect.right())
                .fold(f64::NEG_INFINITY, f64::max);
            let top = world
                .monitors
                .iter()
                .map(|m| m.rect.y)
                .fold(f64::INFINITY, f64::min);
            let min_x = mons.iter().map(|m| m.x).min().unwrap_or(0) as f64 * scale;
            let min_y = mons.iter().map(|m| m.y).min().unwrap_or(0) as f64 * scale;
            let (x, y) = if right.is_finite() {
                (right - min_x, top - min_y)
            } else {
                (0.0, 0.0)
            };
            layout
                .placements
                .insert(id.clone(), Placement { x, y, scale });
            changed = true;
        }
        changed
    }

    pub fn rebuild_world(&self) {
        let machines = self.machines();
        let (layout, changed) = {
            let mut c = self.config.lock();
            let mut layout = c.layout.clone();
            let changed = self.place_new_machines(&mut layout, &machines);
            if changed {
                layout.version = now_ms();
                layout.editor = c.id.clone();
                c.layout = layout.clone();
                c.save();
            }
            (layout, changed)
        };
        self.apply_world(World::build(&machines, &layout));
        if changed {
            self.registry.broadcast(Msg::Layout(layout));
        }
        self.repaint();
    }

    fn apply_world(&self, world: World) {
        let back = self.engine.lock().set_world(world);
        if let Some(pt) = back {
            input::warp(pt.0, pt.1);
        }
    }

    /// Store a layout edited in the UI and share it with every connected machine.
    pub fn set_layout(&self, mut layout: Layout) {
        let machines = self.machines();
        {
            let mut c = self.config.lock();
            layout.version = now_ms().max(c.layout.version + 1);
            layout.editor = c.id.clone();
            c.layout = layout.clone();
            c.save();
        }
        self.apply_world(World::build(&machines, &layout));
        self.registry.broadcast(Msg::Layout(layout));
    }

    pub fn snap_layout(&self, mut layout: Layout, moving: &str) -> Layout {
        let machines = self.machines();
        let threshold = machines
            .iter()
            .flat_map(|(id, m)| m.iter().map(move |m| (id, m)))
            .filter_map(|(id, m)| layout.placements.get(id).map(|p| m.h as f64 * p.scale))
            .fold(0.0, f64::max)
            * 0.05;
        layout::snap(&mut layout, moving, &machines, threshold.max(20.0));
        layout
    }

    pub fn set_settings(&self, settings: Settings) {
        {
            let mut c = self.config.lock();
            c.settings = settings.clone();
            c.save();
        }
        crate::effects::set_enabled(settings.effects);
        crate::effects::set_distort(settings.distort);
        #[cfg(target_os = "macos")]
        if settings.effects && settings.distort {
            crate::overlay::request_capture_permission();
        }
        self.engine.lock().set_settings(settings);
    }

    pub fn set_name(&self, name: String) {
        {
            let mut c = self.config.lock();
            c.name = name;
            c.save();
        }
        self.registry.broadcast(self.info_msg());
    }

    pub fn trust(&self, id: &str, name: &str, os: &str, key: &[u8; 32], addr: Option<SocketAddr>) {
        let mut c = self.config.lock();
        let last_addr = addr
            .map(|a| a.to_string())
            .or_else(|| c.trusted.get(id).and_then(|t| t.last_addr.clone()));
        c.trusted.insert(
            id.to_string(),
            TrustedPeer {
                name: name.into(),
                os: os.into(),
                key: hex::encode(key),
                last_addr,
            },
        );
        c.save();
    }

    pub fn forget(&self, id: &str) {
        self.registry.send(id, Msg::Unpair);
        self.untrust(id);
    }

    fn untrust(&self, id: &str) {
        {
            let mut c = self.config.lock();
            c.trusted.remove(id);
            c.layout.placements.remove(id);
            c.save();
        }
        let s = self.registry.clone();
        let id = id.to_string();
        // Let the Unpair message go out before closing.
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            s.disconnect(&id);
        });
    }

    pub fn begin_incoming_pair(&self, peer_name: &str) -> Result<String, String> {
        let mut failures = self.pair_failures.lock();
        failures.retain(|t| t.elapsed() < Duration::from_secs(300));
        if failures.len() >= 5 {
            return Err("too many failed pairing attempts; wait a few minutes".into());
        }
        drop(failures);
        let mut incoming = self.incoming_pair.lock();
        if incoming
            .as_ref()
            .is_some_and(|p| p.since.elapsed() < Duration::from_secs(120))
        {
            return Err("another pairing is in progress".into());
        }
        let code = crate::crypto::pairing_code();
        *incoming = Some(IncomingPair {
            peer_name: peer_name.into(),
            code: code.clone(),
            since: Instant::now(),
        });
        drop(incoming);
        self.repaint();
        Ok(code)
    }

    pub fn end_incoming_pair(&self, success: bool) {
        *self.incoming_pair.lock() = None;
        if !success {
            self.pair_failures.lock().push(Instant::now());
        }
        self.repaint();
    }

    pub fn peer_disconnected(&self, peer: &str, conn_id: u64) {
        {
            let mut peers = self.peers.lock();
            if peers.get(peer).is_some_and(|p| p.conn_id == conn_id) {
                peers.remove(peer);
            }
        }
        let back = self.engine.lock().peer_gone(peer);
        if let Some(pt) = back {
            input::warp(pt.0, pt.1);
        }
        let mut controlled = self.controlled_by.lock();
        if controlled.as_deref() == Some(peer) {
            *controlled = None;
            self.injector.lock().release_all();
        }
        drop(controlled);
        self.rebuild_world();
    }

    fn is_controller(&self, peer: &str) -> bool {
        self.controlled_by.lock().as_deref() == Some(peer)
    }

    /// Report (here and on the controller) which app receives the remote keys,
    /// and whenever something starts or stops swallowing them.
    fn check_key_target(&self) {
        let blocker = input::injection_blocker();
        let mut d = self.key_diag.lock();
        let first = !d.announced;
        let changed = d.blocked != blocker.is_some();
        d.announced = true;
        d.blocked = blocker.is_some();
        drop(d);
        if first {
            let app = input::focused_app().unwrap_or_else(|| "unknown".into());
            self.diag(format!("receiving keys; they go to the focused app: {app}"));
        }
        match blocker {
            Some(why) if changed || first => self.diag(format!("keys are being blocked: {why}")),
            None if changed => self.diag("keys are no longer blocked"),
            _ => {}
        }
    }

    pub fn handle_msg(&self, peer: &str, peer_os: &str, msg: Msg) {
        match msg {
            Msg::Auth => {}
            Msg::Info {
                name,
                os,
                build,
                monitors,
            } => {
                if build > crate::updater::build() {
                    if let Some(u) = self.updater.get() {
                        u.peer_is_newer();
                    }
                }
                if let Some(p) = self.peers.lock().get_mut(peer) {
                    p.name = name.clone();
                    p.os = os.clone();
                    p.monitors = monitors;
                    p.build = Some(build);
                }
                let mut c = self.config.lock();
                if let Some(t) = c.trusted.get_mut(peer) {
                    if t.name != name || t.os != os {
                        t.name = name;
                        t.os = os;
                        c.save();
                    }
                }
                drop(c);
                self.rebuild_world();
            }
            Msg::Layout(theirs) => {
                let adopt = {
                    let mut c = self.config.lock();
                    if c.layout.is_older_than(&theirs) {
                        let mut merged = c.layout.clone();
                        merged.placements.extend(theirs.placements);
                        merged.version = theirs.version;
                        merged.editor = theirs.editor;
                        c.layout = merged;
                        c.save();
                        true
                    } else {
                        false
                    }
                };
                if adopt {
                    self.rebuild_world();
                }
            }
            Msg::Enter { x, y } => {
                // Someone else's mouse is here now: stop driving another machine ourselves,
                // which also brings our hidden cursor back.
                let was_driving = self.engine.lock().force_local();
                if was_driving.is_some() {
                    input::warp(x, y);
                }
                *self.controlled_by.lock() = Some(peer.to_string());
                *self.key_diag.lock() = KeyDiag::default();
                *self.remote_cursor.lock() = Some((x, y));
                self.injector.lock().move_abs(x, y);
                crate::effects::burst((x, y), crate::effects::Kind::Arrive);
                self.repaint();
            }
            Msg::Leave => {
                let was = {
                    let mut c = self.controlled_by.lock();
                    let was = c.as_deref() == Some(peer);
                    if was {
                        *c = None;
                    }
                    was
                };
                if was {
                    let mut injector = self.injector.lock();
                    injector.release_all();
                    if let Some(pt) = self.remote_cursor.lock().take() {
                        crate::effects::burst(pt, crate::effects::Kind::Leave);
                        // Do not leave our idle cursor on the edge (taskbar, Dock previews).
                        let park = input::away_from_edge(pt, &self.local_monitors.lock());
                        injector.move_abs(park.0, park.1);
                    }
                    drop(injector);
                    crate::dnd::controller_released(peer);
                    if self.config.lock().settings.clipboard_sync {
                        self.clipboard.send_to(peer);
                    }
                    self.repaint();
                }
            }
            Msg::MouseMove { x, y } if self.is_controller(peer) => {
                *self.remote_cursor.lock() = Some((x, y));
                self.injector.lock().move_abs(x, y)
            }
            Msg::MouseButton { button, down } if self.is_controller(peer) => {
                if button == crate::protocol::Button::Left && !down {
                    crate::dnd::controller_released(peer);
                }
                self.injector.lock().button(button, down)
            }
            Msg::DragStart { id } if self.is_controller(peer) => crate::dnd::drag_start(peer, id),
            msg @ (Msg::Offer(_) | Msg::Chunk { .. } | Msg::TransferEnd { .. }) => {
                crate::transfer::incoming(peer, msg)
            }
            Msg::Wheel { dx, dy } if self.is_controller(peer) => {
                let (dx, dy) = if self.config.lock().settings.invert_scroll {
                    (-dx, -dy)
                } else {
                    (dx, dy)
                };
                self.injector.lock().wheel(dx, dy)
            }
            Msg::Key { hid, down } if self.is_controller(peer) => {
                let swap = self.config.lock().settings.swap_ctrl_cmd && peer_os != this_os();
                let hid = if swap {
                    keymap::swap_ctrl_meta(hid)
                } else {
                    hid
                };
                if down {
                    self.check_key_target();
                }
                self.injector.lock().key(hid, down)
            }
            Msg::Key { .. } => {
                let mut d = self.key_diag.lock();
                if !d.ignored {
                    d.ignored = true;
                    drop(d);
                    let owner = self.controlled_by.lock().clone();
                    self.diag(format!(
                        "ignoring keys from a machine that does not control this one \
                         (controlled by: {})",
                        owner.as_deref().unwrap_or("nobody")
                    ));
                }
            }
            Msg::Diag(text) => {
                let name = self
                    .peers
                    .lock()
                    .get(peer)
                    .map_or_else(|| peer.to_string(), |p| p.name.clone());
                self.log(format!("[diag from {name}] {text}"));
            }
            Msg::Clipboard(text) => {
                if self.config.lock().settings.clipboard_sync {
                    self.clipboard.set(text);
                }
            }
            Msg::Unpair => {
                self.log("the other machine removed the pairing");
                self.untrust(peer);
            }
            _ => {}
        }
    }

    /// Poll local monitors; resolution or arrangement changes are pushed to peers.
    pub fn watch_monitors(self: &Arc<Self>) {
        let s = self.clone();
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let now = input::monitors();
                let changed = {
                    let mut m = s.local_monitors.lock();
                    if *m != now && !now.is_empty() {
                        *m = now;
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    s.registry.broadcast(s.info_msg());
                    s.rebuild_world();
                }
            }
        });
    }
}
