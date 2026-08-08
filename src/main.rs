#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chat;
mod doc;
mod export;
mod keymap;
mod library;
mod llm;
mod render;
mod script;
mod state;
mod tools;
mod ui;

use app::EvoApp;

fn load_icon() -> eframe::egui::IconData {
    let png = include_bytes!("../assets/icon/evo-256.png");
    let image = image::load_from_memory(png)
        .expect("bundled icon is valid PNG")
        .into_rgba8();
    let (width, height) = image.dimensions();
    eframe::egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn main() -> eframe::Result {
    let initial_file = std::env::args().nth(1).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 850.0])
            .with_min_inner_size([700.0, 500.0])
            .with_title("evo")
            .with_icon(load_icon())
            .with_transparent(true),
        ..Default::default()
    };

    eframe::run_native(
        "evo",
        options,
        Box::new(move |cc| Ok(Box::new(EvoApp::new(cc, initial_file)))),
    )
}
