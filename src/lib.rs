//! evo, as a library.
//!
//! The application is `src/main.rs`: a window, a couple of headless
//! subcommands, and nothing else. Everything the app is made of lives here so
//! that a second crate can call it.
//!
//! There is exactly one such crate, and it is `fuzz/`. A fuzzer has to run the
//! shipped parser -- `Document::load_bytes`, `export_pdf_bytes`,
//! `extract_all_pages`, the markup reader -- rather than a copy of it, because
//! a copy is not what crashes in front of a person. `xtask` mirrors sixty lines
//! of rendering instead of importing them (see `xtask/src/fidelity/render.rs`)
//! because a hash of a bitmap does not need eframe and tantivy compiled to
//! produce it; a fuzz target that must exercise the real code has no such
//! choice, and pays the build.
//!
//! Modules are `pub` because that is what "another crate can call it" means.
//! The API this exposes is not a promised one: evo is an application, its
//! version number tracks the application, and nothing outside this repository
//! should depend on these paths.

pub mod app;
pub mod chat;
pub mod doc;
pub mod export;
pub mod keymap;
pub mod library;
pub mod llm;
pub mod mcp;
/// The timing harness. Tests only: it builds a thousand-page document to
/// measure against, and nothing the app does needs one.
#[cfg(test)]
mod perf;
pub mod render;
/// The properties: what must hold for every input, including the inputs
/// nobody would send on purpose. Tests only.
#[cfg(test)]
mod robustness;
pub mod script;
pub mod serve;
pub mod state;
pub mod tools;
pub mod ui;
