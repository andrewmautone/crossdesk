//! The settings window: machines on the LAN, pairing, and the screen layout editor.

use crate::config::Settings;
use crate::layout::{Layout, World};
use crate::net;
use crate::state::Shared;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

#[derive(PartialEq)]
enum Tab {
    Layout,
    Settings,
    Log,
}

#[derive(Clone, Copy)]
struct View {
    scale: f32,
    offset: Vec2,
}

impl View {
    fn to_screen(self, x: f64, y: f64) -> Pos2 {
        Pos2::new(x as f32 * self.scale + self.offset.x, y as f32 * self.scale + self.offset.y)
    }
}

struct Drag {
    machine: String,
    layout: Layout,
    view: View,
}

pub struct App {
    shared: Arc<Shared>,
    tab: Tab,
    name: String,
    manual_addr: String,
    code_input: String,
    drag: Option<Drag>,
}

impl App {
    pub fn new(shared: Arc<Shared>, cc: &eframe::CreationContext) -> App {
        *shared.repaint.lock() = Some(cc.egui_ctx.clone());
        let name = shared.config.lock().name.clone();
        App { shared, tab: Tab::Layout, name, manual_addr: String::new(), code_input: String::new(), drag: None }
    }
}

fn machine_color(id: &str, local: bool) -> Color32 {
    if local {
        return Color32::from_rgb(52, 120, 220);
    }
    let h = id.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    let palette = [
        Color32::from_rgb(220, 120, 50),
        Color32::from_rgb(60, 160, 100),
        Color32::from_rgb(160, 90, 200),
        Color32::from_rgb(200, 70, 110),
        Color32::from_rgb(40, 150, 170),
    ];
    palette[(h as usize) % palette.len()]
}

fn os_label(os: &str) -> &'static str {
    match os {
        "windows" => "Windows",
        "macos" => "macOS",
        _ => "Other",
    }
}

fn parse_addr(text: &str) -> Option<SocketAddr> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let with_port = if t.contains(':') { t.to_string() } else { format!("{t}:{}", crate::protocol::TCP_PORT) };
    with_port.to_socket_addrs().ok()?.find(|a| a.is_ipv4())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(700));
        self.top_bar(ctx);
        self.machines_panel(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Layout, "Screens");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.selectable_value(&mut self.tab, Tab::Log, "Log");
            });
            ui.separator();
            match self.tab {
                Tab::Layout => self.layout_tab(ui),
                Tab::Settings => self.settings_tab(ui),
                Tab::Log => self.log_tab(ui),
            }
        });
        self.pairing_windows(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        crate::input::shutdown();
    }
}

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("crossdesk");
                ui.separator();
                ui.label("This computer:");
                let r = ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(160.0));
                if r.lost_focus() && !self.name.trim().is_empty() && self.name != self.shared.config.lock().name {
                    self.shared.set_name(self.name.trim().to_string());
                }
                ui.separator();
                let target = self.shared.engine.lock().active_target().map(str::to_string);
                let locked = self.shared.engine.lock().is_locked();
                let controlled = self.shared.controlled_by.lock().clone();
                let peer_name = |id: &str| {
                    self.shared.peers.lock().get(id).map(|p| p.name.clone()).unwrap_or_else(|| id[..6.min(id.len())].into())
                };
                if let Some(t) = target {
                    ui.colored_label(Color32::from_rgb(220, 140, 40), format!("Controlling {}", peer_name(&t)));
                } else if let Some(c) = controlled {
                    ui.colored_label(Color32::from_rgb(60, 170, 100), format!("Controlled by {}", peer_name(&c)));
                } else {
                    ui.label("Using this computer");
                }
                if locked {
                    ui.colored_label(Color32::from_rgb(200, 60, 60), "Cursor locked (Ctrl+Alt+Shift+L)");
                }
            });
            if let Some(err) = self.shared.capture_error.lock().clone() {
                ui.colored_label(Color32::from_rgb(210, 60, 60), err);
            }
            ui.add_space(4.0);
        });
    }

    fn machines_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("machines").resizable(false).exact_width(290.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.strong("Computers on your network");
            ui.add_space(4.0);

            struct Row {
                id: String,
                name: String,
                os: String,
                addr: Option<SocketAddr>,
                trusted: bool,
                connected: bool,
            }
            let my_id = self.shared.config.lock().id.clone();
            let mut rows: Vec<Row> = Vec::new();
            for (id, t) in self.shared.config.lock().trusted.iter() {
                rows.push(Row {
                    id: id.clone(),
                    name: t.name.clone(),
                    os: t.os.clone(),
                    addr: t.last_addr.as_deref().and_then(|a| a.parse().ok()),
                    trusted: true,
                    connected: self.shared.registry.is_connected(id),
                });
            }
            for (id, d) in self.shared.discovered.lock().iter() {
                if *id == my_id || d.last_seen.elapsed() > Duration::from_secs(10) {
                    continue;
                }
                if let Some(r) = rows.iter_mut().find(|r| r.id == *id) {
                    r.addr = Some(d.addr);
                    continue;
                }
                rows.push(Row {
                    id: id.clone(),
                    name: d.name.clone(),
                    os: d.os.clone(),
                    addr: Some(d.addr),
                    trusted: false,
                    connected: false,
                });
            }
            rows.sort_by_key(|r| r.name.to_lowercase());

            if rows.is_empty() {
                ui.label(RichText::new("Searching… Open crossdesk on the other computer (same Wi-Fi/network).").weak());
            }
            for r in rows {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.strong(&r.name);
                        ui.label(RichText::new(os_label(&r.os)).weak());
                    });
                    ui.horizontal(|ui| {
                        let (text, color) = match (r.trusted, r.connected) {
                            (true, true) => ("Connected", Color32::from_rgb(60, 170, 100)),
                            (true, false) => ("Paired, offline", Color32::GRAY),
                            _ => ("Not paired", Color32::from_rgb(200, 150, 40)),
                        };
                        ui.colored_label(color, text);
                        if let Some(a) = r.addr {
                            ui.label(RichText::new(a.ip().to_string()).weak().small());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if r.trusted {
                                if ui.small_button("Forget").clicked() {
                                    self.shared.forget(&r.id);
                                }
                            } else if let Some(a) = r.addr {
                                if ui.button("Pair").clicked() {
                                    self.code_input.clear();
                                    net::start_pairing(self.shared.clone(), a);
                                }
                            }
                        });
                    });
                });
            }

            ui.add_space(10.0);
            ui.separator();
            ui.label("Not listed? Pair by IP address:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.manual_addr).hint_text("192.168.0.20").desired_width(170.0));
                let addr = parse_addr(&self.manual_addr);
                if ui.add_enabled(addr.is_some(), egui::Button::new("Pair")).clicked() {
                    self.code_input.clear();
                    net::start_pairing(self.shared.clone(), addr.unwrap());
                }
            });
            ui.add_space(6.0);
            let port = self.shared.config.lock().port;
            ui.label(RichText::new(format!("Uses TCP {port} and UDP {} on the local network.", crate::protocol::DISCOVERY_PORT)).weak().small());
        });
    }

    fn layout_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("Drag the screens so they match your desk. Push the mouse past an edge that touches another computer to move there.");
        ui.add_space(6.0);

        let machines = self.shared.machines();
        let my_id = self.shared.config.lock().id.clone();
        let layout = match &self.drag {
            Some(d) => d.layout.clone(),
            None => self.shared.config.lock().layout.clone(),
        };
        let world = World::build(&machines, &layout);
        let names: std::collections::HashMap<String, String> = {
            let peers = self.shared.peers.lock();
            let mut m: std::collections::HashMap<String, String> =
                peers.iter().map(|(id, p)| (id.clone(), p.name.clone())).collect();
            m.insert(my_id.clone(), self.shared.config.lock().name.clone());
            m
        };

        let canvas_h = (ui.available_height() - 90.0).max(220.0);
        let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), canvas_h), Sense::hover());
        let area = response.rect;
        painter.rect_filled(area, 8.0, ui.visuals().extreme_bg_color);

        if world.monitors.is_empty() {
            painter.text(area.center(), Align2::CENTER_CENTER, "No screens yet", FontId::proportional(16.0), Color32::GRAY);
            return;
        }

        let view = match &self.drag {
            Some(d) => d.view,
            None => {
                let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for m in &world.monitors {
                    x0 = x0.min(m.rect.x);
                    y0 = y0.min(m.rect.y);
                    x1 = x1.max(m.rect.right());
                    y1 = y1.max(m.rect.bottom());
                }
                let margin = 30.0;
                let sw = (area.width() - 2.0 * margin) / (x1 - x0).max(1.0) as f32;
                let sh = (area.height() - 2.0 * margin) / (y1 - y0).max(1.0) as f32;
                let scale = sw.min(sh);
                let content = Vec2::new((x1 - x0) as f32 * scale, (y1 - y0) as f32 * scale);
                let origin = area.center() - content / 2.0;
                View { scale, offset: Vec2::new(origin.x - x0 as f32 * scale, origin.y - y0 as f32 * scale) }
            }
        };

        let mut drag_started: Option<String> = None;
        let mut drag_delta: Option<(String, Vec2)> = None;
        let mut drag_stopped = false;
        let mut labeled: std::collections::HashSet<String> = Default::default();

        for (i, m) in world.monitors.iter().enumerate() {
            let rect = Rect::from_min_max(view.to_screen(m.rect.x, m.rect.y), view.to_screen(m.rect.right(), m.rect.bottom()))
                .shrink(1.5);
            let local = m.machine == my_id;
            let color = machine_color(&m.machine, local);
            let resp = ui.interact(rect, ui.id().with(("monitor", i)), Sense::drag());
            let hovered = resp.hovered() || self.drag.as_ref().is_some_and(|d| d.machine == m.machine);
            painter.rect_filled(rect, 4.0, color.gamma_multiply(if hovered { 0.55 } else { 0.35 }));
            painter.rect_stroke(rect, 4.0, Stroke::new(if m.native.primary { 2.5_f32 } else { 1.5 }, color));
            let mut lines = Vec::new();
            if labeled.insert(m.machine.clone()) {
                let name = names.get(&m.machine).cloned().unwrap_or_default();
                lines.push(if local { format!("{name} (this one)") } else { name });
            }
            lines.push(format!("{} × {}", m.native.w, m.native.h));
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                lines.join("\n"),
                FontId::proportional(13.0),
                ui.visuals().strong_text_color(),
            );
            if resp.drag_started() {
                drag_started = Some(m.machine.clone());
            }
            if resp.dragged() {
                drag_delta = Some((m.machine.clone(), resp.drag_delta()));
            }
            if resp.drag_stopped() {
                drag_stopped = true;
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }

        // The virtual cursor while controlling another computer.
        if let Some(target) = self.shared.engine.lock().active_target() {
            if let Some(m) = world.monitors.iter().find(|m| m.machine == target) {
                let c = view.to_screen(m.rect.center().0, m.rect.center().1);
                painter.circle_stroke(c, 6.0, Stroke::new(2.0_f32, Color32::WHITE));
            }
        }

        if let Some(machine) = drag_started {
            self.drag = Some(Drag { machine, layout: layout.clone(), view });
        }
        if let (Some(d), Some((machine, delta))) = (self.drag.as_mut(), drag_delta) {
            if d.machine == machine {
                if let Some(p) = d.layout.placements.get_mut(&machine) {
                    p.x += (delta.x / d.view.scale) as f64;
                    p.y += (delta.y / d.view.scale) as f64;
                }
            }
        }
        if drag_stopped {
            if let Some(d) = self.drag.take() {
                let snapped = self.shared.snap_layout(d.layout, &d.machine);
                self.shared.set_layout(snapped);
            }
        }

        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Screen size on the map:");
            let mut layout = self.shared.config.lock().layout.clone();
            let mut changed = false;
            for (id, _) in &machines {
                let Some(p) = layout.placements.get_mut(id) else { continue };
                let name = names.get(id).cloned().unwrap_or_default();
                ui.label(name);
                changed |= ui
                    .add(egui::Slider::new(&mut p.scale, 0.25..=4.0).logarithmic(true).max_decimals(2))
                    .on_hover_text("Makes this computer's screens bigger or smaller on the map. Also changes how fast the cursor crosses them.")
                    .changed();
                ui.add_space(12.0);
            }
            if changed && self.drag.is_none() {
                self.shared.set_layout(layout);
            }
        });
        ui.label(
            RichText::new("Ctrl+Alt+Shift+Q brings the cursor back · Ctrl+Alt+Shift+L locks it to the current screen").weak().small(),
        );
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let mut s: Settings = self.shared.config.lock().settings.clone();
        let before = s.clone();
        ui.checkbox(&mut s.edge_switch, "Switch computers by pushing the mouse past a screen edge");
        ui.horizontal(|ui| {
            ui.label("Ignore corners (pixels):");
            ui.add(egui::DragValue::new(&mut s.corner_px).range(0..=400));
        });
        ui.checkbox(&mut s.clipboard_sync, "Share copied text between computers");
        ui.checkbox(&mut s.swap_ctrl_cmd, "Swap Ctrl and Cmd/Win when the other computer runs a different system")
            .on_hover_text("With this on, Ctrl+C on the PC keyboard does Cmd+C on the Mac.");
        ui.checkbox(&mut s.invert_scroll, "Invert scroll direction when controlled from another computer");
        if cfg!(target_os = "macos") {
            ui.horizontal(|ui| {
                ui.label("Lines per wheel notch:");
                ui.add(egui::Slider::new(&mut s.scroll_lines, 1.0..=10.0));
            });
        }
        if s != before {
            self.shared.injector.lock().set_scroll_lines(s.scroll_lines);
            self.shared.set_settings(s);
        }
        ui.add_space(12.0);
        ui.separator();
        ui.label(RichText::new(format!("Settings file: {}", crate::config::path().display())).weak().small());
    }

    fn log_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
            for line in self.shared.log.lock().iter() {
                ui.label(RichText::new(line).monospace());
            }
        });
    }

    fn pairing_windows(&mut self, ctx: &egui::Context) {
        let incoming = self.shared.incoming_pair.lock().as_ref().map(|p| (p.peer_name.clone(), p.code.clone(), p.since));
        if let Some((peer, code, since)) = incoming {
            if since.elapsed() > Duration::from_secs(120) {
                *self.shared.incoming_pair.lock() = None;
            } else {
                egui::Window::new("Pairing request")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(format!("{peer} wants to share the mouse and keyboard with this computer."));
                        ui.label("Type this code on it:");
                        let spaced = format!("{} {}", &code[..3], &code[3..]);
                        ui.label(RichText::new(spaced).size(36.0).monospace().strong());
                        ui.label(RichText::new("If you did not start this, just ignore it.").weak().small());
                    });
            }
        }

        let mut close = false;
        let mut submit = false;
        {
            let guard = self.shared.outgoing_pair.lock();
            let Some(p) = guard.as_ref() else { return };
            let peer = p.peer_name.clone();
            let waiting_code = p.code_tx.is_some();
            let error = p.error.clone();
            drop(guard);
            egui::Window::new("Pair with another computer")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    if let Some(e) = &error {
                        ui.colored_label(Color32::from_rgb(210, 60, 60), e);
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    } else if waiting_code {
                        ui.label(format!("Enter the 6-digit code shown on {peer}:"));
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.code_input)
                                .font(FontId::monospace(28.0))
                                .desired_width(170.0)
                                .char_limit(7),
                        );
                        r.request_focus();
                        let digits: String = self.code_input.chars().filter(|c| c.is_ascii_digit()).collect();
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            if ui.add_enabled(digits.len() == 6, egui::Button::new("Pair")).clicked() || (enter && digits.len() == 6) {
                                submit = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Checking the code with {peer}…"));
                        });
                    }
                });
        }
        if submit {
            let digits: String = self.code_input.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Some(p) = self.shared.outgoing_pair.lock().as_mut() {
                if let Some(tx) = p.code_tx.take() {
                    let _ = tx.send(digits);
                }
            }
            self.code_input.clear();
        }
        if close {
            *self.shared.outgoing_pair.lock() = None;
        }
    }
}
