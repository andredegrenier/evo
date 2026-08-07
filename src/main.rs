#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod doc;
mod export;
mod render;
mod state;
mod tools;
mod ui;

use app::EvoApp;

fn main() -> eframe::Result {
    let initial_file = std::env::args().nth(1).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 850.0])
            .with_min_inner_size([700.0, 500.0])
            .with_title("evo"),
        ..Default::default()
    };

    eframe::run_native(
        "evo",
        options,
        Box::new(move |cc| Ok(Box::new(EvoApp::new(cc, initial_file)))),
    )
}
