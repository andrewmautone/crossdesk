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
mod ui;

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

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("crossdesk")
            .with_inner_size([980.0, 620.0])
            .with_min_inner_size([760.0, 480.0]),
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
