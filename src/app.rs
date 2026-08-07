//! Top-level application: window layout, menus, keyboard shortcuts, and
//! open/save orchestration.

use std::path::PathBuf;

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};

use crate::doc::Document;
use crate::doc::history::Command;
use crate::state::DocState;
use crate::tools::{self, ActiveTool};
use crate::ui;

pub struct EvoApp {
    dc: Option<DocState>,
    error: Option<String>,
    show_thumbnails: bool,
    flatten_on_save: bool,
    temp_print_files: Vec<PathBuf>,
}

const ZOOM_STEP: f32 = 1.25;

impl EvoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        install_fonts(&cc.egui_ctx);
        let mut app = Self {
            dc: None,
            error: None,
            show_thumbnails: true,
            flatten_on_save: false,
            temp_print_files: Vec::new(),
        };
        if let Some(path) = initial_file {
            app.open_path(path, &cc.egui_ctx);
        }
        app
    }

    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        match Document::load_path(path) {
            Ok(doc) => {
                self.dc = Some(DocState::new(doc, ctx));
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF documents", &["pdf"])
            .pick_file()
        {
            self.open_path(path, ctx);
        }
    }

    fn save_pdf_as(&mut self) {
        let Some(dc) = &self.dc else { return };
        let default_name = dc
            .doc
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| format!("{} (edited).pdf", s.to_string_lossy()))
            .unwrap_or_else(|| "Untitled.pdf".into());
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF document", &["pdf"])
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };
        let options = crate::export::pdf::ExportOptions {
            flatten: self.flatten_on_save,
        };
        if let Err(e) =
            crate::export::pdf::export_pdf(&dc.doc, &dc.pages, &dc.store, options, &path)
        {
            self.error = Some(format!("Export failed: {e}"));
        }
    }

    fn export_svg_as(&mut self) {
        let Some(dc) = &self.dc else { return };
        let default_name = dc
            .doc
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| format!("{}.svg", s.to_string_lossy()))
            .unwrap_or_else(|| "Untitled.svg".into());
        let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG image", &["svg"])
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };
        if let Err(e) = crate::export::svg::export_svg(&dc.doc, &dc.pages, &dc.store, &path) {
            self.error = Some(format!("SVG export failed: {e}"));
        }
    }

    fn print(&mut self) {
        let Some(dc) = &self.dc else { return };
        match crate::export::print::print_via_system_viewer(&dc.doc, &dc.pages, &dc.store) {
            Ok(temp) => self.temp_print_files.push(temp),
            Err(e) => self.error = Some(format!("Print failed: {e}")),
        }
    }

    fn print_direct(&mut self) {
        let Some(dc) = &self.dc else { return };
        match crate::export::print::print_direct(&dc.doc, &dc.pages, &dc.store) {
            Ok(temp) => self.temp_print_files.push(temp),
            Err(e) => self.error = Some(format!("Print failed: {e}")),
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;

        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::O))) {
            self.open_dialog(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::S))) {
            self.save_pdf_as();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::P))) {
            self.print();
        }

        let Some(dc) = &mut self.dc else {
            return;
        };

        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::W))) {
            self.dc = None;
            return;
        }

        // Undo / redo.
        if ctx.input_mut(|i| {
            i.consume_shortcut(&KeyboardShortcut::new(cmd | Modifiers::SHIFT, Key::Z))
        }) {
            dc.history.redo(&mut dc.store, &mut dc.pages);
        } else if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::Z))) {
            if dc.editing_text.is_some() {
                // Let the text field's own undo take precedence; commit instead.
            } else {
                dc.history.undo(&mut dc.store, &mut dc.pages);
            }
        }

        // Zoom.
        if ctx.input_mut(|i| {
            i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::Equals))
                || i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::Plus))
        }) {
            let z = dc.viewport.zoom * ZOOM_STEP;
            dc.viewport.set_zoom(z);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::Minus))) {
            let z = dc.viewport.zoom / ZOOM_STEP;
            dc.viewport.set_zoom(z);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::Num0))) {
            dc.viewport.set_zoom(1.0);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::Num9))) {
            dc.viewport.fit_width = true;
        }

        // The rest only applies when not typing in a text field.
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        // Tool shortcuts.
        let tool_keys = [
            (Key::V, ActiveTool::Select),
            (Key::H, ActiveTool::Highlight),
            (Key::T, ActiveTool::Text),
            (Key::R, ActiveTool::Rect),
            (Key::O, ActiveTool::Ellipse),
            (Key::L, ActiveTool::Line),
            (Key::A, ActiveTool::Arrow),
            (Key::P, ActiveTool::Pen),
        ];
        for (key, tool) in tool_keys {
            if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, key))) {
                dc.tool = tool;
            }
        }

        // Escape: cancel gesture / deselect / back to select tool.
        if ctx.input_mut(|i| i.key_pressed(Key::Escape)) {
            tools::cancel(dc);
            if dc.selection.is_some() {
                dc.selection = None;
            } else {
                dc.tool = ActiveTool::Select;
            }
        }

        // Delete selection.
        if ctx.input_mut(|i| i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace))
            && let Some(id) = dc.selection
            && let Some(removed) = dc.store.remove(id)
        {
            dc.history.record(Command::RemoveAnnotation(removed));
            dc.selection = None;
        }

        // Arrow-key nudge: 1pt, shift = 10pt.
        let shift = ctx.input(|i| i.modifiers.shift);
        let step = if shift { 10.0 } else { 1.0 };
        let nudge = ctx.input_mut(|i| {
            let mut d = (0.0f32, 0.0f32);
            if i.key_pressed(Key::ArrowLeft) {
                d.0 -= step;
            }
            if i.key_pressed(Key::ArrowRight) {
                d.0 += step;
            }
            if i.key_pressed(Key::ArrowUp) {
                d.1 += step;
            }
            if i.key_pressed(Key::ArrowDown) {
                d.1 -= step;
            }
            d
        });
        if (nudge.0 != 0.0 || nudge.1 != 0.0)
            && let Some(before) = dc.selected_annotation().cloned()
        {
            let mut after = before.clone();
            after.translate(nudge.0, nudge.1);
            dc.store.replace(after.clone());
            dc.history
                .record(Command::ModifyAnnotation { before, after });
        }
    }

    fn menu_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open…\t⌘O").clicked() {
                    self.open_dialog(ctx);
                    ui.close();
                }
                let has_doc = self.dc.is_some();
                ui.separator();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Save As PDF…\t⌘S"))
                    .clicked()
                {
                    self.save_pdf_as();
                    ui.close();
                }
                ui.add_enabled_ui(has_doc, |ui| {
                    ui.checkbox(&mut self.flatten_on_save, "Flatten markup on save")
                        .on_hover_text(
                            "Bake markup into the page content instead of keeping \
                             editable annotations",
                        );
                });
                if ui
                    .add_enabled(has_doc, egui::Button::new("Export SVG…"))
                    .clicked()
                {
                    self.export_svg_as();
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Print…\t⌘P"))
                    .clicked()
                {
                    self.print();
                    ui.close();
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Print to Default Printer"))
                    .clicked()
                {
                    self.print_direct();
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Close\t⌘W"))
                    .clicked()
                {
                    self.dc = None;
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                let (can_undo, can_redo) = self
                    .dc
                    .as_ref()
                    .map(|d| (d.history.can_undo(), d.history.can_redo()))
                    .unwrap_or((false, false));
                if ui
                    .add_enabled(can_undo, egui::Button::new("Undo\t⌘Z"))
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.history.undo(&mut dc.store, &mut dc.pages);
                    ui.close();
                }
                if ui
                    .add_enabled(can_redo, egui::Button::new("Redo\t⇧⌘Z"))
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.history.redo(&mut dc.store, &mut dc.pages);
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                let has_doc = self.dc.is_some();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Zoom In\t⌘+"))
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    let z = dc.viewport.zoom * ZOOM_STEP;
                    dc.viewport.set_zoom(z);
                    ui.close();
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Zoom Out\t⌘-"))
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    let z = dc.viewport.zoom / ZOOM_STEP;
                    dc.viewport.set_zoom(z);
                    ui.close();
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Actual Size\t⌘0"))
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.viewport.set_zoom(1.0);
                    ui.close();
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Fit Width\t⌘9"))
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.viewport.fit_width = true;
                    ui.close();
                }
                ui.separator();
                ui.checkbox(&mut self.show_thumbnails, "Show Thumbnails");
            });
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(dc) = &self.dc {
                let title = if dc.is_modified() {
                    format!("{} — edited", dc.doc.title())
                } else {
                    dc.doc.title()
                };
                ui.label(title);
                ui.separator();
                ui.label(format!("{} pages", dc.pages.len()));
                ui.separator();
                ui.label(format!("{:.0}%", dc.viewport.zoom * 100.0));
                if dc.worker.had_warnings() {
                    ui.separator();
                    ui.label("⚠ rendered with warnings").on_hover_text(
                        "Some page content may not display exactly; exports are unaffected.",
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(dc.tool.label());
                });
            } else {
                ui.label("Open a PDF to get started (⌘O, or drop a file here)");
            }
        });
    }
}

impl eframe::App for EvoApp {
    fn on_exit(&mut self) {
        for path in self.temp_print_files.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Files dropped onto the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if let Some(path) = dropped
            .into_iter()
            .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
        {
            self.open_path(path, ctx);
        }

        self.handle_shortcuts(ctx);

        if let Some(dc) = &mut self.dc {
            ui::canvas::poll_worker(dc, ctx);
        }

        egui::Panel::top("menu").show(ui, |ui| {
            self.menu_bar(ctx, ui);
        });

        if let Some(dc) = &mut self.dc {
            egui::Panel::top("toolbar").show(ui, |ui| {
                ui::toolbar::show(ui, dc);
            });
        }

        egui::Panel::bottom("status").show(ui, |ui| {
            self.status_bar(ui);
        });

        if let Some(dc) = &mut self.dc {
            if self.show_thumbnails {
                egui::Panel::left("thumbnails")
                    .resizable(false)
                    .exact_size(150.0)
                    .show(ui, |ui| {
                        ui::thumbnails::show(ui, dc);
                    });
            }
            egui::Panel::right("inspector")
                .resizable(true)
                .show(ui, |ui| {
                    ui::inspector::show(ui, dc);
                });
            egui::CentralPanel::default_margins()
                .frame(egui::Frame::default().fill(egui::Color32::from_gray(60)))
                .show(ui, |ui| {
                    ui::canvas::show(ui, dc);
                });
        } else {
            egui::CentralPanel::default_margins().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.heading("evo — drop a PDF here or press ⌘O");
                });
            });
        }

        if let Some(error) = self.error.clone() {
            egui::Window::new("Cannot open file")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(&error);
                    if ui.button("OK").clicked() {
                        self.error = None;
                    }
                });
        }
    }
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "liberation_sans".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/LiberationSans-Regular.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "liberation_sans".into());
    ctx.set_fonts(fonts);
}
