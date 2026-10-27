#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod clipboard;
mod config;
mod crypto;
mod dnd;
mod effects;
mod engine;
mod input;
mod layout;
mod net;
mod overlay;
mod protocol;
mod state;
mod transfer;
#[cfg(any(windows, target_os = "macos"))]
mod tray;
mod ui;
mod updater;

use std::sync::Arc;

#[cfg(any(windows, target_os = "macos"))]
fn app_icon() -> eframe::egui::IconData {
    tray::window_icon()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn app_icon() -> eframe::egui::IconData {
    eframe::egui::IconData::default()
}

fn main() -> eframe::Result<()> {
    input::init_process();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let shared = state::Shared::new(rt.handle().clone());
    let scroll_lines = shared.config.lock().settings.scroll_lines;
    shared.injector.lock().set_scroll_lines(scroll_lines);
    let fx_shared = shared.clone();
    let (fx_on, fx_distort) = {
        let c = shared.config.lock();
        (c.settings.effects, c.settings.distort)
    };
    #[cfg(target_os = "macos")]
    if fx_on && fx_distort {
        overlay::request_capture_permission();
    }
    effects::start(
        move || fx_shared.local_monitors.lock().clone(),
        fx_on,
        fx_distort,
    );
    shared.log(if overlay::Overlay::glass_available() {
        "effects: Liquid Glass"
    } else {
        "effects: screen snapshot refraction"
    });
    if std::env::args().any(|a| a == "--fx-demo") {
        // Plays the effects on the right edge of the main screen, for checking how they look.
        let mons = shared.local_monitors.lock().clone();
        if let Some(m) = mons.iter().find(|m| m.primary).or(mons.first()).cloned() {
            std::thread::spawn(move || {
                let pt = (m.x + m.w - 1, m.y + m.h / 2);
                std::thread::sleep(std::time::Duration::from_millis(800));
                for i in 1..=30 {
                    effects::push(pt, i as f64 / 30.0);
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                effects::burst(pt, effects::Kind::Leave);
                std::thread::sleep(std::time::Duration::from_millis(900));
                effects::burst((m.x + 1, m.y + m.h / 3), effects::Kind::Arrive);
            });
        }
    }
    transfer::start(shared.clone());
    dnd::start(shared.clone());
    net::start(shared.clone());
    shared.watch_monitors();
    input::start_capture(shared.clone());
    let updater = updater::Updater::start(shared.clone());
    if std::env::args().any(|a| a == "--check-update") {
        updater.check_now();
    }
    let _ = shared.updater.set(updater);

    let start_hidden =
        cfg!(any(windows, target_os = "macos")) && shared.config.lock().settings.start_hidden;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("crossdesk")
            .with_inner_size([980.0, 620.0])
            .with_min_inner_size([760.0, 480.0])
            .with_visible(!start_hidden)
            .with_icon(Arc::new(app_icon())),
        ..Default::default()
    };
    let app_shared = shared.clone();
    let result = eframe::run_native(
        "crossdesk",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(app_shared, cc)))),
    );
    input::shutdown();
    rt.shutdown_background();
    result
}
