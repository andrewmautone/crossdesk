//! Tray icon (Windows notification area / macOS menu bar) and the app icon.
//!
//! Menu events arrive on the UI thread even while the window is hidden, when
//! egui stops calling `update`, so they act on the shared state and the egui
//! context directly.

use crate::state::Shared;
use eframe::egui::{self, ViewportCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Set when the user picks Quit, so closing the window really exits.
pub static QUITTING: AtomicBool = AtomicBool::new(false);

/// Decode one of the PNGs rendered by scripts/make-icons.py into RGBA.
fn decode(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().expect("bundled icon is a valid PNG");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .expect("bundled icon is a valid PNG");
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}

pub fn window_icon() -> egui::IconData {
    let (rgba, width, height) = decode(include_bytes!("../assets/generated/window-256.png"));
    egui::IconData {
        rgba,
        width,
        height,
    }
}

/// macOS gets a black template image it tints for the menu bar; Windows the color icon.
fn tray_icon() -> Result<Icon, String> {
    let bytes: &[u8] = if cfg!(target_os = "macos") {
        include_bytes!("../assets/generated/tray-template-64x48.png")
    } else {
        include_bytes!("../assets/generated/tray-color-32.png")
    };
    let (rgba, w, h) = decode(bytes);
    Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())
}

fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
    ctx.request_repaint();
}

pub struct Tray {
    _icon: TrayIcon,
    lock: CheckMenuItem,
}

impl Tray {
    pub fn create(shared: Arc<Shared>, ctx: egui::Context) -> Result<Tray, String> {
        let open = MenuItem::new("Open crossdesk", true, None);
        let lock = CheckMenuItem::new("Keep cursor on this computer", true, false, None);
        let quit = MenuItem::new("Quit crossdesk", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &lock, &PredefinedMenuItem::separator(), &quit])
            .map_err(|e| e.to_string())?;

        let mono = cfg!(target_os = "macos");
        let icon = tray_icon()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("crossdesk")
            .with_icon(icon)
            .with_icon_as_template(mono)
            // On Windows a left click opens the window and the right click shows the menu.
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .build()
            .map_err(|e| e.to_string())?;

        let (open_id, lock_id, quit_id) = (open.id().clone(), lock.id().clone(), quit.id().clone());
        let menu_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                show_window(&menu_ctx);
            } else if e.id == lock_id {
                // The item toggles its own check mark; mirror it in the engine.
                let mut engine = shared.engine.lock();
                let locked = !engine.is_locked();
                engine.set_locked(locked);
                menu_ctx.request_repaint();
            } else if e.id == quit_id {
                QUITTING.store(true, Ordering::SeqCst);
                menu_ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                menu_ctx.send_viewport_cmd(ViewportCommand::Close);
                menu_ctx.request_repaint();
            }
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                if !cfg!(target_os = "macos") {
                    show_window(&ctx);
                }
            }
        }));
        Ok(Tray { _icon: tray, lock })
    }

    /// Keep the menu check mark in sync with the hotkey.
    pub fn sync(&self, locked: bool) {
        if self.lock.is_checked() != locked {
            self.lock.set_checked(locked);
        }
    }
}
