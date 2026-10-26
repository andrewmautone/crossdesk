#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod clipboard;
mod config;
mod crypto;
mod engine;
mod input;
mod layout;
mod net;
mod protocol;
mod state;
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
    net::start(shared.clone());
    shared.watch_monitors();
    input::start_capture(shared.clone());
    let updater = updater::Updater::start(shared.clone());
    if std::env::args().any(|a| a == "--check-update") {
        updater.check_now();
    }
    let _ = shared.updater.set(updater);

    let start_hidden = cfg!(any(windows, target_os = "macos")) && shared.config.lock().settings.start_hidden;
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
