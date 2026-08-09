#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chat;
mod doc;
mod export;
mod keymap;
mod library;
mod llm;
mod mcp;
/// The timing harness. Tests only: it builds a thousand-page document to
/// measure against, and nothing the app does needs one.
#[cfg(test)]
mod perf;
mod render;
mod script;
mod serve;
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
    let first_arg = std::env::args().nth(1);

    // `evo mcp-serve` is a different program wearing the same binary: an MCP
    // server on stdin/stdout, with no window at all. Answer it before eframe
    // gets anywhere near opening one.
    if first_arg.as_deref() == Some("mcp-serve") {
        mcp::headless::main();
    }

    // So is `evo serve`: the library over HTTP for a phone, with its own
    // configuration file because there is no eframe storage to read.
    if first_arg.as_deref() == Some("serve") {
        serve::main();
    }

    // And `evo fetch-model`: the Preferences pane's download, for a machine
    // with no Preferences pane. Whoever is setting up a server needs the
    // weights on it before `evo serve` can answer anything.
    if first_arg.as_deref() == Some("fetch-model") {
        llm::download::main();
    }

    // And `evo fetch-pdfium`: the rasterizer's shared library, for a build
    // that did not come with one -- `cargo install evo`, or a server.
    if first_arg.as_deref() == Some("fetch-pdfium") {
        render::pdfium_fetch::main();
    }

    let initial_file = first_arg.map(std::path::PathBuf::from);

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
