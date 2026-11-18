#![cfg_attr(
    all(not(debug_assertions), windows),
    windows_subsystem = "windows"
)]

mod app;
mod logo;
mod model;
mod supervisor;

use eframe::egui;

use std::path::PathBuf;

fn main() -> eframe::Result {
    install_panic_hook();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RunWeb")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([980.0, 640.0])
            .with_icon(std::sync::Arc::new(logo::icon_data())),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "RunWeb",
        options,
        Box::new(|cc| Ok(Box::new(app::RunWebApp::new(cc)))),
    )
}

fn install_panic_hook() {
    let path = panic_log_path();
    std::panic::set_hook(Box::new(move |info| {
        let _ = std::fs::write(&path, format!("{info}\n"));
    }));
}

fn panic_log_path() -> PathBuf {
    let config = model::config_path();
    let dir = config
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("panic.log")
}
