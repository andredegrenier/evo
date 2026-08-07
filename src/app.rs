//! Top-level application: window layout, menus, keyboard shortcuts, and
//! open/save orchestration.

use std::path::PathBuf;

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};

use crate::doc::Document;
use crate::doc::history::Command;
use crate::state::DocState;
use crate::tools::{self, ActiveTool};
use crate::ui;
use crate::ui::theme::ThemeChoice;

pub struct EvoApp {
    dc: Option<DocState>,
    error: Option<String>,
    show_thumbnails: bool,
    flatten_on_save: bool,
    temp_print_files: Vec<PathBuf>,
    /// OS blur-behind is active; panels use translucent fills.
    glass: bool,
    theme: ThemeChoice,
    /// What `theme::apply` was last called with, so we restyle when the
    /// choice, the resolved light/dark theme, or `glass` changes.
    applied: Option<(ThemeChoice, egui::Theme, bool)>,
    library: Option<crate::library::Library>,
    lib_view: ui::library_view::LibraryViewState,
}

const ZOOM_STEP: f32 = 1.25;

impl EvoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        install_fonts(&cc.egui_ctx);
        let vibrancy = apply_window_effects(cc);
        let (theme, glass_pref) = match cc.storage {
            Some(storage) => (
                eframe::get_value(storage, "theme").unwrap_or_default(),
                eframe::get_value(storage, "glass").unwrap_or(true),
            ),
            None => (ThemeChoice::default(), true),
        };
        let glass = glass_pref && vibrancy;
        crate::ui::theme::apply(&cc.egui_ctx, theme, glass);
        let library = match crate::library::Library::open_default() {
            Ok(mut lib) => {
                lib.start_indexer(&cc.egui_ctx);
                Some(lib)
            }
            Err(e) => {
                eprintln!("library unavailable: {e}");
                None
            }
        };
        let mut app = Self {
            dc: None,
            error: None,
            show_thumbnails: true,
            flatten_on_save: false,
            temp_print_files: Vec::new(),
            glass,
            theme,
            applied: Some((theme, cc.egui_ctx.theme(), glass)),
            library,
            lib_view: ui::library_view::LibraryViewState::default(),
        };
        if let Some(path) = initial_file {
            app.open_path(path, &cc.egui_ctx);
        }
        app
    }

    /// Clearing `applied` makes `ui()` restyle on the next frame.
    fn set_glass(&mut self, glass: bool) {
        self.glass = glass;
        self.applied = None;
    }

    /// Persist the markup sidecar for a library document.
    fn save_sidecar(&mut self) {
        if let Some(dc) = &self.dc
            && let Some(id) = &dc.library_id
            && let Some(lib) = &self.library
        {
            let markup = crate::library::SavedMarkup {
                version: 1,
                annotations: dc.store.to_vec(),
                pages: dc.pages.clone(),
            };
            if let Err(e) = lib.save_markup(id, &markup) {
                self.error = Some(format!("Could not save markup to library: {e}"));
            }
        }
    }

    /// Close the current document, autosaving library markup first.
    fn close_document(&mut self) {
        self.save_sidecar();
        self.dc = None;
    }

    fn open_library_doc(&mut self, id: &str, ctx: &egui::Context) {
        self.open_library_doc_at(id, None, ctx);
    }

    fn open_library_doc_at(&mut self, id: &str, source_page: Option<usize>, ctx: &egui::Context) {
        let Some(lib) = &self.library else { return };
        let result = lib
            .load_bytes(id)
            .map_err(|e| e.to_string())
            .and_then(|bytes| Document::load_bytes(bytes, None).map_err(|e| e.to_string()));
        match result {
            Ok(doc) => {
                self.close_document();
                let title = self.library.as_ref().and_then(|lib| {
                    lib.list()
                        .ok()
                        .and_then(|docs| docs.into_iter().find(|d| d.id == id).map(|d| d.title))
                });
                let mut dc = DocState::new(doc, ctx);
                dc.library_id = Some(id.to_owned());
                dc.title_override = title;
                if let Some(lib) = &self.library
                    && let Ok(Some(markup)) = lib.load_markup(id)
                {
                    dc.store = crate::doc::store::AnnotationStore::restore(markup.annotations);
                    // Guard against a stale sidecar from a different blob.
                    if markup.pages.source_of.len() >= dc.doc.pages.len() {
                        dc.pages = markup.pages;
                    }
                }
                if let Some(page) = source_page {
                    // Find the display position showing that source page.
                    let position = dc
                        .pages
                        .order
                        .iter()
                        .position(|&logical| dc.pages.source_of(logical) == page);
                    dc.viewport.scroll_to_page = position;
                }
                self.dc = Some(dc);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not open library document: {e}")),
        }
    }

    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        match Document::load_path(path) {
            Ok(doc) => {
                self.close_document();
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

    /// Merge the given PDFs into the open document, carrying the editing
    /// session over. The current document's pages keep their indices, so
    /// markup and history stay valid.
    fn insert_files(&mut self, ctx: &egui::Context, files: Vec<PathBuf>) {
        let Some(dc) = self.dc.take() else { return };
        let mut loaded: Vec<Vec<u8>> = Vec::new();
        for path in &files {
            match std::fs::read(path) {
                Ok(bytes) => loaded.push(bytes),
                Err(e) => {
                    self.error = Some(format!("Could not read {}: {e}", path.display()));
                    self.dc = Some(dc);
                    return;
                }
            }
        }
        let mut sources: Vec<&[u8]> = vec![&dc.doc.source];
        sources.extend(loaded.iter().map(|b| b.as_slice()));
        let merged = match crate::export::merge::merge_pdfs(&sources) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.error = Some(format!("Insert failed: {e}"));
                self.dc = Some(dc);
                return;
            }
        };
        match Document::load_bytes(merged, dc.doc.path.clone()) {
            Ok(doc) => {
                let old_sources = dc.doc.pages.len();
                let new_sources = doc.pages.len();
                let before = dc.pages.clone();
                let mut new_dc = DocState::adopt(doc, ctx, dc);
                new_dc
                    .pages
                    .append_source_pages(old_sources, new_sources - old_sources);
                new_dc
                    .history
                    .record(crate::doc::history::Command::SetPageList {
                        before,
                        after: new_dc.pages.clone(),
                    });
                self.dc = Some(new_dc);
            }
            Err(e) => {
                self.error = Some(format!("Insert failed: {e}"));
                self.dc = Some(dc);
            }
        }
    }

    /// Combine several PDFs into a brand-new untitled document.
    fn combine_files(&mut self, ctx: &egui::Context, files: Vec<PathBuf>) {
        let mut loaded: Vec<Vec<u8>> = Vec::new();
        for path in &files {
            match std::fs::read(path) {
                Ok(bytes) => loaded.push(bytes),
                Err(e) => {
                    self.error = Some(format!("Could not read {}: {e}", path.display()));
                    return;
                }
            }
        }
        let sources: Vec<&[u8]> = loaded.iter().map(|b| b.as_slice()).collect();
        match crate::export::merge::merge_pdfs(&sources)
            .map_err(|e| e.to_string())
            .and_then(|bytes| Document::load_bytes(bytes, None).map_err(|e| e.to_string()))
        {
            Ok(doc) => {
                let mut dc = DocState::new(doc, ctx);
                dc.force_modified = true;
                self.dc = Some(dc);
            }
            Err(e) => self.error = Some(format!("Combine failed: {e}")),
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;

        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::O))) {
            self.open_dialog(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::S))) {
            if self.dc.as_ref().is_some_and(|d| d.library_id.is_some()) {
                self.save_sidecar();
            } else {
                self.save_pdf_as();
            }
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::P))) {
            self.print();
        }

        // Find: in a document it opens the find bar, on the library home it
        // jumps to the search field.
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::F))) {
            match &mut self.dc {
                Some(dc) => {
                    dc.find.open = true;
                    dc.find.focus_pending = true;
                }
                None => self.lib_view.focus_search_pending = true,
            }
        }

        let Some(dc) = &mut self.dc else {
            return;
        };

        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::W))) {
            self.close_document();
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

    /// Platform-aware menu label, e.g. "Open…\t⌘O" on macOS,
    /// "Open…\tCtrl+O" elsewhere.
    fn label(ctx: &egui::Context, text: &str, modifiers: Modifiers, key: Key) -> String {
        format!(
            "{text}\t{}",
            ctx.format_shortcut(&KeyboardShortcut::new(modifiers, key))
        )
    }

    fn menu_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui
                    .button(Self::label(ctx, "Open…", Modifiers::COMMAND, Key::O))
                    .clicked()
                {
                    self.open_dialog(ctx);
                    ui.close();
                }
                let has_doc = self.dc.is_some();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Insert Pages from PDF…"))
                    .clicked()
                {
                    if let Some(files) = rfd::FileDialog::new()
                        .add_filter("PDF documents", &["pdf"])
                        .pick_files()
                    {
                        self.insert_files(ctx, files);
                    }
                    ui.close();
                }
                if ui.button("Combine PDFs…").clicked() {
                    if let Some(files) = rfd::FileDialog::new()
                        .add_filter("PDF documents", &["pdf"])
                        .pick_files()
                    {
                        self.combine_files(ctx, files);
                    }
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(Self::label(
                            ctx,
                            "Save As PDF…",
                            Modifiers::COMMAND,
                            Key::S,
                        )),
                    )
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
                    .add_enabled(
                        has_doc,
                        egui::Button::new(Self::label(ctx, "Print…", Modifiers::COMMAND, Key::P)),
                    )
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
                    .add_enabled(
                        has_doc,
                        egui::Button::new(Self::label(ctx, "Close", Modifiers::COMMAND, Key::W)),
                    )
                    .clicked()
                {
                    self.close_document();
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
                    .add_enabled(
                        can_undo,
                        egui::Button::new(Self::label(ctx, "Undo", Modifiers::COMMAND, Key::Z)),
                    )
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.history.undo(&mut dc.store, &mut dc.pages);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        can_redo,
                        egui::Button::new(Self::label(
                            ctx,
                            "Redo",
                            Modifiers::COMMAND | Modifiers::SHIFT,
                            Key::Z,
                        )),
                    )
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
                    .add_enabled(
                        has_doc,
                        egui::Button::new(Self::label(
                            ctx,
                            "Zoom In",
                            Modifiers::COMMAND,
                            Key::Plus,
                        )),
                    )
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    let z = dc.viewport.zoom * ZOOM_STEP;
                    dc.viewport.set_zoom(z);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(Self::label(
                            ctx,
                            "Zoom Out",
                            Modifiers::COMMAND,
                            Key::Minus,
                        )),
                    )
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    let z = dc.viewport.zoom / ZOOM_STEP;
                    dc.viewport.set_zoom(z);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(Self::label(
                            ctx,
                            "Actual Size",
                            Modifiers::COMMAND,
                            Key::Num0,
                        )),
                    )
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.viewport.set_zoom(1.0);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(Self::label(
                            ctx,
                            "Fit Width",
                            Modifiers::COMMAND,
                            Key::Num9,
                        )),
                    )
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.viewport.fit_width = true;
                    ui.close();
                }
                ui.separator();
                ui.checkbox(&mut self.show_thumbnails, "Show Thumbnails");
                ui.menu_button("Theme", |ui| {
                    for choice in ThemeChoice::ALL {
                        if ui
                            .radio_value(&mut self.theme, choice, choice.label())
                            .clicked()
                        {
                            ui.close();
                        }
                    }
                });
                let mut solid = !self.glass;
                if ui
                    .checkbox(&mut solid, "Solid Background")
                    .on_hover_text("Disable window translucency")
                    .changed()
                {
                    self.set_glass(!solid);
                }
            });
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(dc) = &self.dc {
                let title = if dc.is_modified() {
                    format!("{} — edited", dc.title())
                } else {
                    dc.title()
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
                ui.label(format!(
                    "Open a PDF to get started ({}, or drop a file here)",
                    ui.ctx()
                        .format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::O))
                ));
            }
        });
    }
}

impl eframe::App for EvoApp {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.glass {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            visuals.panel_fill.to_normalized_gamma_f32()
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "theme", &self.theme);
        eframe::set_value(storage, "glass", &self.glass);
    }

    fn on_exit(&mut self) {
        self.save_sidecar();
        for path in self.temp_print_files.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;

        // Restyle when the picked theme, the OS light/dark theme, or the
        // translucency setting changed.
        if self.applied != Some((self.theme, ctx.theme(), self.glass)) {
            ui::theme::apply(ctx, self.theme, self.glass);
            self.applied = Some((self.theme, ctx.theme(), self.glass));
            ctx.request_repaint();
        }

        // Files dropped onto the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        let dropped_pdfs: Vec<PathBuf> = dropped
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
            .collect();
        match (dropped_pdfs.len(), self.dc.is_some()) {
            (0, _) => {}
            // Dropping onto the library home imports into the library.
            (_, false) if self.library.is_some() => {
                let lib = self.library.as_ref().unwrap();
                for path in &dropped_pdfs {
                    match lib.import(path) {
                        Ok(meta) => {
                            if let Ok(bytes) = lib.load_bytes(&meta.id) {
                                crate::library::spawn_thumbnail_job(
                                    std::sync::Arc::new(bytes),
                                    lib.thumb_path(&meta.id),
                                    ctx.clone(),
                                );
                            }
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                self.lib_view.mark_dirty();
            }
            (1, _) => self.open_path(dropped_pdfs.into_iter().next().unwrap(), ctx),
            // Several files onto an open document: insert them as pages.
            (_, true) => self.insert_files(ctx, dropped_pdfs),
            // Several files with nothing open (no library): combine.
            (_, false) => self.combine_files(ctx, dropped_pdfs),
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

        let mut open_extracted: Option<Vec<u8>> = None;
        let mut pending_temp_file: Option<PathBuf> = None;
        let mut pending_error: Option<String> = None;
        // Find-time OCR reuses the library's models, but never downloads them.
        let models_dir = self.library.as_ref().map(|lib| lib.root.join("models"));
        if let Some(dc) = &mut self.dc {
            if self.show_thumbnails {
                let action = egui::Panel::left("thumbnails")
                    .resizable(false)
                    .exact_size(150.0)
                    .show(ui, |ui| ui::thumbnails::show(ui, dc))
                    .inner;
                match action {
                    Some(ui::thumbnails::RailAction::OpenExtracted(bytes)) => {
                        open_extracted = Some(bytes);
                    }
                    Some(ui::thumbnails::RailAction::TempPrintFile(path)) => {
                        pending_temp_file = Some(path);
                    }
                    Some(ui::thumbnails::RailAction::Error(msg)) => pending_error = Some(msg),
                    None => {}
                }
            }
            egui::Panel::right("inspector")
                .resizable(true)
                .show(ui, |ui| {
                    ui::inspector::show(ui, dc);
                });
            let fill = ui::theme::canvas_fill(ctx, self.theme, self.glass);
            let canvas_rect = egui::CentralPanel::default_margins()
                .frame(egui::Frame::default().fill(fill))
                .show(ui, |ui| {
                    ui::canvas::show(ui, dc);
                })
                .response
                .rect;
            ui::findbar::show(ctx, dc, models_dir, canvas_rect);
        } else {
            let mut lib_action = None;
            egui::CentralPanel::default_margins().show(ui, |ui| {
                if let Some(lib) = &self.library {
                    lib_action = ui::library_view::show(ui, lib, &mut self.lib_view);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.heading(format!(
                            "evo — drop a PDF here or press {}",
                            ui.ctx().format_shortcut(&KeyboardShortcut::new(
                                Modifiers::COMMAND,
                                Key::O
                            ))
                        ));
                    });
                }
            });
            match lib_action {
                Some(ui::library_view::LibraryAction::Open(id)) => self.open_library_doc(&id, ctx),
                Some(ui::library_view::LibraryAction::OpenAtPage(id, page)) => {
                    self.open_library_doc_at(&id, Some(page), ctx)
                }
                Some(ui::library_view::LibraryAction::Error(msg)) => self.error = Some(msg),
                None => {}
            }
        }

        if let Some(path) = pending_temp_file {
            self.temp_print_files.push(path);
        }
        if let Some(msg) = pending_error {
            self.error = Some(msg);
        }
        if let Some(bytes) = open_extracted {
            match crate::doc::Document::load_bytes(bytes, None) {
                Ok(doc) => self.dc = Some(DocState::new(doc, ctx)),
                Err(e) => self.error = Some(e.to_string()),
            }
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

/// Try to enable OS blur-behind. Returns whether it took effect.
fn apply_window_effects(cc: &eframe::CreationContext<'_>) -> bool {
    #[cfg(target_os = "macos")]
    {
        use window_vibrancy::{NSVisualEffectMaterial, apply_vibrancy};
        apply_vibrancy(
            cc,
            NSVisualEffectMaterial::UnderWindowBackground,
            None,
            None,
        )
        .is_ok()
    }
    #[cfg(target_os = "windows")]
    {
        use window_vibrancy::{apply_acrylic, apply_mica};
        apply_mica(cc, None).is_ok() || apply_acrylic(cc, Some((24, 24, 28, 130))).is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = cc;
        false
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
    // Phosphor puts its glyphs in the private-use area and self-inserts at
    // index 1, so it only ever fills in the icons Liberation lacks.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}
