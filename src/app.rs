//! Top-level application: window layout, menus, keyboard shortcuts, and
//! open/save orchestration.

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};

use crate::doc::Document;
use crate::doc::history::Command;
use crate::export::pdf::OcrLine;
use crate::keymap::{Action, Keymap, StoredKeymap};
use crate::library::extract::TextSource;
use crate::state::DocState;
use crate::tools::{self, ActiveTool};
use crate::ui;
use crate::ui::theme::ThemeChoice;

pub struct EvoApp {
    dc: Option<DocState>,
    error: Option<String>,
    show_thumbnails: bool,
    flatten_on_save: bool,
    /// Write the ⌘F OCR text into exports as an invisible text layer.
    embed_ocr_on_save: bool,
    temp_print_files: Vec<PathBuf>,
    /// OS blur-behind is active; panels use translucent fills.
    glass: bool,
    theme: ThemeChoice,
    /// What `theme::apply` was last called with, so we restyle when the
    /// choice, the resolved light/dark theme, or `glass` changes.
    applied: Option<(ThemeChoice, egui::Theme, bool)>,
    library: Option<crate::library::Library>,
    lib_view: ui::library_view::LibraryViewState,
    keymap: Keymap,
    prefs: ui::preferences::PreferencesState,
    wizard: ui::merge_wizard::MergeWizardState,
    ribbon: ui::ribbon::RibbonConfig,
    /// Spawned on first use: most sessions never run a script.
    script_engine: Option<crate::script::ScriptEngine>,
    script_prefs: crate::script::ScriptPrefs,
    /// What the model may do unasked: summarizing the library, so far.
    assistant_prefs: crate::library::enrich::AssistantPrefs,
    scripts_ui: ui::scripts::ScriptsState,
    /// Spawned the first time the chat panel is opened.
    chat_engine: Option<crate::chat::ChatEngine>,
    /// Model weights being fetched, if the user asked for any this session.
    llm_downloads: crate::llm::download::Downloads,
}

pub const ZOOM_STEP: f32 = 1.25;

/// Tool actions and the tool each selects.
const TOOL_ACTIONS: [(Action, ActiveTool); 9] = [
    (Action::ToolSelect, ActiveTool::Select),
    (Action::ToolPan, ActiveTool::Pan),
    (Action::ToolHighlight, ActiveTool::Highlight),
    (Action::ToolText, ActiveTool::Text),
    (Action::ToolRect, ActiveTool::Rect),
    (Action::ToolEllipse, ActiveTool::Ellipse),
    (Action::ToolLine, ActiveTool::Line),
    (Action::ToolArrow, ActiveTool::Arrow),
    (Action::ToolPen, ActiveTool::Pen),
];

fn cmd() -> Modifiers {
    Modifiers::COMMAND
}

/// A title turned into something safe to use as a filename.
fn sanitize_filename(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('-').trim();
    if trimmed.is_empty() {
        "document".to_owned()
    } else {
        trimmed.chars().take(80).collect()
    }
}

/// Read every file, naming the one that failed. `merge_pdfs` reports errors
/// per batch, so without this the user is told the merge failed but not which
/// file caused it.
fn read_all(files: &[PathBuf]) -> Result<Vec<Vec<u8>>, String> {
    files
        .iter()
        .map(|path| {
            std::fs::read(path).map_err(|e| format!("Could not read {}: {e}", path.display()))
        })
        .collect()
}

impl EvoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        install_fonts(&cc.egui_ctx);
        let vibrancy = apply_window_effects(cc);
        let (theme, glass_pref, keymap, mut ribbon, script_prefs, assistant_prefs) =
            match cc.storage {
                Some(storage) => (
                    eframe::get_value(storage, "theme").unwrap_or_default(),
                    // Solid by default: translucency is a finish, not the design.
                    eframe::get_value(storage, "glass").unwrap_or(false),
                    Keymap::from_stored(
                        eframe::get_value::<StoredKeymap>(storage, "keymap").unwrap_or_default(),
                    ),
                    eframe::get_value(storage, "ribbon").unwrap_or_default(),
                    eframe::get_value(storage, "script_prefs").unwrap_or_default(),
                    eframe::get_value(storage, "assistant_prefs").unwrap_or_default(),
                ),
                None => (
                    ThemeChoice::default(),
                    false,
                    Keymap::default(),
                    ui::ribbon::RibbonConfig::default(),
                    crate::script::ScriptPrefs::default(),
                    crate::library::enrich::AssistantPrefs::default(),
                ),
            };
        // A stored layout predates any item added since it was written.
        ribbon.sanitize();
        let glass = glass_pref && vibrancy;
        crate::ui::theme::apply(&cc.egui_ctx, theme, glass);
        let library = match crate::library::Library::open_default() {
            Ok(mut lib) => {
                lib.start_indexer(&cc.egui_ctx);
                // Enrichment starts switched off inside the worker; this is
                // where a saved "yes" turns it on and starts the first pass.
                lib.set_assistant(&assistant_prefs, &script_prefs.model);
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
            embed_ocr_on_save: false,
            temp_print_files: Vec::new(),
            glass,
            theme,
            applied: Some((theme, cc.egui_ctx.theme(), glass)),
            library,
            lib_view: ui::library_view::LibraryViewState::default(),
            keymap,
            prefs: ui::preferences::PreferencesState::default(),
            wizard: ui::merge_wizard::MergeWizardState::default(),
            ribbon,
            script_engine: None,
            script_prefs,
            assistant_prefs,
            scripts_ui: ui::scripts::ScriptsState::default(),
            chat_engine: None,
            llm_downloads: crate::llm::download::Downloads::default(),
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
            // The conversation is about this document, so it belongs with it.
            if let Err(e) = lib.save_chat(id, &dc.chat.messages) {
                self.error = Some(format!("Could not save the chat to library: {e}"));
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
                // The library id is the chat worker's cache key too, so there
                // is nothing to hash later.
                dc.chat.doc_key = Some(id.to_owned());
                if let Some(lib) = &self.library
                    && let Ok(messages) = lib.load_chat(id)
                {
                    dc.chat.messages = messages;
                }
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

    /// Whether the ⌘F cache holds OCR text that could be embedded on save.
    fn has_ocr_text(&self) -> bool {
        self.dc.as_ref().is_some_and(|dc| {
            dc.page_text
                .values()
                .any(|page| page.source == Some(TextSource::Ocr))
        })
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
            ocr_layers: self.embed_ocr_on_save.then(|| ocr_layers(dc)).flatten(),
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
        let loaded = match read_all(&files) {
            Ok(loaded) => loaded,
            Err(e) => {
                self.error = Some(e);
                self.dc = Some(dc);
                return;
            }
        };
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
        let loaded = match read_all(&files) {
            Ok(loaded) => loaded,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let sources: Vec<&[u8]> = loaded.iter().map(|b| b.as_slice()).collect();
        match crate::export::merge::merge_pdfs(&sources)
            .map_err(|e| e.to_string())
            .and_then(|bytes| Document::load_bytes(bytes, None).map_err(|e| e.to_string()))
        {
            Ok(doc) => {
                // Combining replaces whatever was open, so let the outgoing
                // document save its markup sidecar first. Assigning over
                // `self.dc` would drop it silently.
                self.close_document();
                let mut dc = DocState::new(doc, ctx);
                dc.force_modified = true;
                self.dc = Some(dc);
            }
            Err(e) => self.error = Some(format!("Combine failed: {e}")),
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // While Preferences is recording a binding it owns the keyboard;
        // otherwise recording ⌘S would also save the document.
        if self.prefs.is_capturing() {
            return;
        }

        if self.keymap.consume(ctx, Action::Open) {
            self.open_dialog(ctx);
        }
        if self.keymap.consume(ctx, Action::Preferences) {
            self.prefs.open();
        }
        if self.keymap.consume(ctx, Action::Save) {
            if self.dc.as_ref().is_some_and(|d| d.library_id.is_some()) {
                self.save_sidecar();
            } else {
                self.save_pdf_as();
            }
        }
        if self.keymap.consume(ctx, Action::Print) {
            self.print();
        }

        // Find: in a document it opens the find bar, on the library home it
        // jumps to the search field.
        if self.keymap.consume(ctx, Action::Find) {
            match &mut self.dc {
                Some(dc) => {
                    dc.find.open = true;
                    dc.find.focus_pending = true;
                }
                None => self.lib_view.focus_search_pending = true,
            }
        }

        if self.dc.is_none() {
            return;
        }

        if self.keymap.consume(ctx, Action::CloseDocument) {
            self.close_document();
            return;
        }

        if self.keymap.consume(ctx, Action::ToggleChat) {
            self.toggle_chat(ctx);
        }

        // Redo before undo: with the default bindings ⇧⌘Z would otherwise also
        // satisfy ⌘Z.
        let redo = self.keymap.consume(ctx, Action::Redo);
        let undo = !redo && self.keymap.consume(ctx, Action::Undo);
        let zoom_in = self.keymap.consume(ctx, Action::ZoomIn)
            // ⌘+ and ⌘= are the same key to a user, and which one the OS
            // reports depends on the layout.
            || ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd(), Key::Plus)));
        let zoom_out = self.keymap.consume(ctx, Action::ZoomOut);
        let zoom_actual = self.keymap.consume(ctx, Action::ZoomActual);
        let fit_width = self.keymap.consume(ctx, Action::ZoomFitWidth);
        let delete = self.keymap.consume(ctx, Action::DeleteSelection)
            // Backspace is the other delete key; it isn't separately bindable.
            || (!ctx.egui_wants_keyboard_input()
                && ctx.input_mut(|i| i.key_pressed(Key::Backspace)));

        let tools_pressed: Vec<ActiveTool> = TOOL_ACTIONS
            .iter()
            .filter(|(action, _)| self.keymap.consume(ctx, *action))
            .map(|(_, tool)| *tool)
            .collect();

        let escape =
            !ctx.egui_wants_keyboard_input() && ctx.input_mut(|i| i.key_pressed(Key::Escape));

        let Some(dc) = &mut self.dc else {
            return;
        };

        if redo {
            dc.history.redo(&mut dc.store, &mut dc.pages);
        } else if undo && dc.editing_text.is_none() {
            // While editing text, let the text field's own undo take precedence.
            dc.history.undo(&mut dc.store, &mut dc.pages);
        }

        if zoom_in {
            let z = dc.viewport.zoom * ZOOM_STEP;
            dc.viewport.set_zoom(z);
        }
        if zoom_out {
            let z = dc.viewport.zoom / ZOOM_STEP;
            dc.viewport.set_zoom(z);
        }
        if zoom_actual {
            dc.viewport.set_zoom(1.0);
        }
        if fit_width {
            dc.viewport.fit_width = true;
        }

        for tool in tools_pressed {
            dc.tool = tool;
        }

        // Escape: cancel gesture / deselect / back to select tool.
        if escape {
            tools::cancel(dc);
            if dc.selection.is_some() {
                dc.selection = None;
            } else {
                dc.tool = ActiveTool::Select;
            }
        }

        if delete
            && let Some(id) = dc.selection
            && let Some(removed) = dc.store.remove(id)
        {
            dc.history.record(Command::RemoveAnnotation(removed));
            dc.selection = None;
        }

        // The rest only applies when not typing in a text field.
        if ctx.egui_wants_keyboard_input() {
            return;
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

    /// Platform-aware menu label carrying the action's current binding, e.g.
    /// "Open…\t⌘O" on macOS, "Open…\tCtrl+O" elsewhere.
    fn label(&self, ctx: &egui::Context, text: &str, action: Action) -> String {
        self.keymap.menu_label(ctx, text, action)
    }

    /// Draw the Scripts window and act on it: start and cancel runs, and take
    /// delivery of whatever a finished run produced.
    fn scripts_window(&mut self, ctx: &egui::Context) {
        if !self.scripts_ui.open {
            return;
        }
        // The Lua thread and its vendored VM are only worth starting for
        // someone who actually opens this window.
        if self.script_engine.is_none() {
            self.script_engine = Some(crate::script::ScriptEngine::spawn(ctx));
        }

        let title = self.dc.as_ref().map(|dc| dc.title());
        let action = ui::scripts::show(
            ctx,
            &mut self.scripts_ui,
            self.script_engine.as_ref(),
            title.as_deref(),
        );

        match action {
            Some(ui::scripts::ScriptsAction::Run { name, source }) => {
                if let Some(reason) = self.model_unavailable() {
                    // A script that asks for text would fail seconds in; say
                    // so before it starts.
                    self.error = Some(reason);
                } else {
                    let snapshot = self.dc.as_ref().map(|dc| crate::script::DocSnapshot {
                        title: dc.title(),
                        source: dc.doc.source.clone(),
                        page_count: dc.doc.pages.len(),
                    });
                    if let Some(engine) = &self.script_engine {
                        engine.run(name, source, snapshot, self.script_prefs.clone());
                    }
                }
            }
            Some(ui::scripts::ScriptsAction::Cancel) => {
                if let Some(engine) = &self.script_engine {
                    engine.cancel();
                }
            }
            Some(ui::scripts::ScriptsAction::Open(doc)) => {
                match Document::load_bytes(doc.bytes, None) {
                    Ok(loaded) => {
                        self.close_document();
                        let mut dc = DocState::new(loaded, ctx);
                        dc.title_override = Some(doc.title);
                        self.dc = Some(dc);
                    }
                    Err(e) => self.error = Some(format!("Could not open the document: {e}")),
                }
            }
            Some(ui::scripts::ScriptsAction::RevealFolder(dir)) => {
                if let Err(e) = crate::export::print::open_with_default_app(&dir) {
                    self.error = Some(format!("Could not open {}: {e}", dir.display()));
                }
            }
            None => {}
        }

        self.collect_script_output(ctx);
    }

    /// Import what a finished run generated. The script thread hands back
    /// bytes and never touches the library itself, so this is where a
    /// generated document actually becomes one.
    fn collect_script_output(&mut self, ctx: &egui::Context) {
        let Some(engine) = &self.script_engine else {
            return;
        };
        let Some(outcome) = engine.take_outcome() else {
            return;
        };
        let docs = match outcome {
            Ok(docs) => docs,
            // The failure is already in the log the user is looking at;
            // an error dialog on top of it would just be noise.
            Err(_) => return,
        };

        for doc in docs {
            let Some(lib) = &self.library else {
                // Without a library there is nowhere to put it, but the user
                // can still open it from the window.
                self.scripts_ui.produced.push(doc);
                continue;
            };
            let filename = format!("{}.pdf", sanitize_filename(&doc.title));
            match lib.import_bytes(doc.bytes.clone(), &doc.title, &filename) {
                Ok(meta) => {
                    if let Ok(bytes) = lib.load_bytes(&meta.id) {
                        crate::library::spawn_thumbnail_job(
                            std::sync::Arc::new(bytes),
                            lib.thumb_path(&meta.id),
                            ctx.clone(),
                        );
                    }
                    self.lib_view.mark_dirty();
                }
                Err(e) => self.error = Some(format!("Could not add to the library: {e}")),
            }
            self.scripts_ui.produced.push(doc);
        }
    }

    /// Show or hide the chat panel, starting its worker the first time.
    fn toggle_chat(&mut self, ctx: &egui::Context) {
        let Some(dc) = &mut self.dc else { return };
        dc.chat.open = !dc.chat.open;
        if dc.chat.open {
            dc.chat.focus_pending = true;
            if self.chat_engine.is_none() {
                self.chat_engine = Some(crate::chat::ChatEngine::spawn(ctx));
            }
        }
    }

    /// Why the configured model cannot answer, if it cannot. Asked before a
    /// job is submitted: a missing download is something to say up front, not
    /// a failure to report a thread later.
    fn model_unavailable(&self) -> Option<String> {
        let config = &self.script_prefs.model;
        (config.api == crate::script::model::Api::Builtin)
            .then(|| crate::llm::unavailable_reason(&config.builtin_model))
            .flatten()
    }

    /// Send a question to the chat worker, recording it in the transcript.
    fn ask_chat(&mut self, question: String) {
        let Some(engine) = &self.chat_engine else {
            return;
        };
        let unavailable = self.model_unavailable();
        let Some(dc) = &mut self.dc else { return };
        if let Some(reason) = unavailable {
            dc.chat.error = Some(reason);
            dc.chat.input = question;
            return;
        }
        // The worker caches page text under this key. A library document has
        // one already; anything else is identified by its bytes.
        let doc_key = match &dc.chat.doc_key {
            Some(key) => key.clone(),
            None => {
                let key = crate::library::hex_digest(&dc.doc.source);
                dc.chat.doc_key = Some(key.clone());
                key
            }
        };
        dc.chat.error = None;
        dc.chat.last_pages.clear();
        let history = crate::chat::history_for(&dc.chat.messages);
        dc.chat
            .messages
            .push(crate::script::model::ChatMessage::new(
                crate::script::model::Role::User,
                question.clone(),
            ));
        engine.ask(crate::chat::ChatJob {
            doc_key,
            source: dc.doc.source.clone(),
            title: dc.title(),
            question,
            history,
            config: self.script_prefs.model.clone(),
        });
    }

    /// Take a finished answer, if one is waiting, into the transcript it
    /// belongs to. An answer for a document that has since been closed is
    /// dropped rather than shown against whatever is open now.
    fn collect_chat_answer(&mut self) {
        let Some(engine) = &self.chat_engine else {
            return;
        };
        let Some(outcome) = engine.take_outcome() else {
            return;
        };
        let Some(dc) = &mut self.dc else { return };
        if dc.chat.doc_key.as_deref() != Some(outcome.doc_key.as_str()) {
            return;
        }
        match outcome.result {
            Ok(answer) => {
                dc.chat.last_pages = answer.pages;
                dc.chat
                    .messages
                    .push(crate::script::model::ChatMessage::new(
                        crate::script::model::Role::Assistant,
                        answer.text,
                    ));
            }
            Err(e) => {
                // The question was never answered, so it does not belong in the
                // transcript; put it back in the box to be asked again, unless
                // the user has since started typing something else.
                if dc.chat.messages.last().map(|m| m.role) == Some(crate::script::model::Role::User)
                    && dc.chat.input.trim().is_empty()
                    && let Some(last) = dc.chat.messages.pop()
                {
                    dc.chat.input = last.content;
                }
                let hint = if e.contains("could not reach the model") {
                    "\n\nThe model is chosen in Preferences ▸ Model."
                } else {
                    ""
                };
                dc.chat.error = Some(format!("{e}{hint}"));
            }
        }
    }

    fn handle_chat_action(&mut self, action: ui::chat::ChatAction, ctx: &egui::Context) {
        match action {
            ui::chat::ChatAction::Ask(question) => self.ask_chat(question),
            ui::chat::ChatAction::Cancel => {
                if let Some(engine) = &self.chat_engine {
                    engine.cancel();
                }
            }
            ui::chat::ChatAction::Clear => {
                if let Some(dc) = &mut self.dc {
                    dc.chat.messages.clear();
                    dc.chat.last_pages.clear();
                    dc.chat.error = None;
                }
                self.save_sidecar();
            }
            ui::chat::ChatAction::GoToPage(page) => {
                if let Some(dc) = &mut self.dc {
                    // Citations name source pages; the view is in display
                    // order, which page moves and deletions can change.
                    dc.viewport.scroll_to_page = dc
                        .pages
                        .order
                        .iter()
                        .position(|&logical| dc.pages.source_of(logical) == page - 1);
                    ctx.request_repaint();
                }
            }
        }
    }

    fn menu_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button(self.label(ctx, "Open…", Action::Open)).clicked() {
                    self.open_dialog(ctx);
                    ui.close();
                }
                let has_doc = self.dc.is_some();
                if ui.button("Combine / Insert PDFs…").clicked() {
                    self.wizard.open_for(has_doc);
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(self.label(ctx, "Save As PDF…", Action::Save)),
                    )
                    .clicked()
                {
                    self.save_pdf_as();
                    ui.close();
                }
                let has_ocr = self.has_ocr_text();
                ui.add_enabled_ui(has_doc, |ui| {
                    ui.checkbox(&mut self.flatten_on_save, "Flatten markup on save")
                        .on_hover_text(
                            "Bake markup into the page content instead of keeping \
                             editable annotations",
                        );
                    let ocr = ui.add_enabled(
                        has_ocr,
                        egui::Checkbox::new(&mut self.embed_ocr_on_save, "Embed OCR text layer"),
                    );
                    if has_ocr {
                        ocr.on_hover_text(
                            "Write the recognized text invisibly over scanned pages so \
                             the exported PDF is selectable and searchable",
                        );
                    } else {
                        ocr.on_disabled_hover_text(
                            "Open Find (⌘F) once to run OCR on scanned pages",
                        );
                    }
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
                        egui::Button::new(self.label(ctx, "Print…", Action::Print)),
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
                        egui::Button::new(self.label(ctx, "Close", Action::CloseDocument)),
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
                        egui::Button::new(self.label(ctx, "Undo", Action::Undo)),
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
                        egui::Button::new(self.label(ctx, "Redo", Action::Redo)),
                    )
                    .clicked()
                    && let Some(dc) = &mut self.dc
                {
                    dc.history.redo(&mut dc.store, &mut dc.pages);
                    ui.close();
                }
                ui.separator();
                if ui
                    .button(self.label(ctx, "Preferences…", Action::Preferences))
                    .clicked()
                {
                    self.prefs.open();
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                let has_doc = self.dc.is_some();
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(self.label(ctx, "Zoom In", Action::ZoomIn)),
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
                        egui::Button::new(self.label(ctx, "Zoom Out", Action::ZoomOut)),
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
                        egui::Button::new(self.label(ctx, "Actual Size", Action::ZoomActual)),
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
                        egui::Button::new(self.label(ctx, "Fit Width", Action::ZoomFitWidth)),
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
            ui.menu_button("Tools", |ui| {
                let has_doc = self.dc.is_some();
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(self.label(
                            ctx,
                            "Chat with Document",
                            Action::ToggleChat,
                        )),
                    )
                    .on_hover_text(
                        "Ask questions about the open document; a local model \
                         answers from its pages and cites them",
                    )
                    .clicked()
                {
                    self.toggle_chat(ctx);
                    ui.close();
                }
                if ui
                    .button("Scripts…")
                    .on_hover_text(
                        "Run a Lua script over this document — for instance, to have \
                         a local model write a summary",
                    )
                    .clicked()
                {
                    self.scripts_ui.open();
                    ui.close();
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
        eframe::set_value(storage, "keymap", &self.keymap.to_stored());
        eframe::set_value(storage, "ribbon", &self.ribbon);
        eframe::set_value(storage, "script_prefs", &self.script_prefs);
        eframe::set_value(storage, "assistant_prefs", &self.assistant_prefs);
    }

    fn on_exit(&mut self) {
        self.save_sidecar();
        for path in self.temp_print_files.drain(..) {
            let _ = std::fs::remove_file(path);
        }
        // Model weights have to be released before the process tears itself
        // down, or llama.cpp's own shutdown asserts on them.
        crate::llm::unload();
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
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
            // Dropping onto the open wizard adds to its list.
            _ if self.wizard.open => self.wizard.add_files(dropped_pdfs),
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
            // Several files at once: show them in the wizard rather than
            // merging on the spot in whatever order they arrived.
            (_, has_doc) => {
                self.wizard.open_for(has_doc);
                self.wizard.add_files(dropped_pdfs);
            }
        }

        self.handle_shortcuts(ctx);

        if let Some(dc) = &mut self.dc {
            ui::canvas::poll_worker(dc, ctx);
        }

        egui::Panel::top("menu").show(ui, |ui| {
            self.menu_bar(ctx, ui);
        });

        let tokens = ui::theme::tokens(ctx, self.theme, self.glass);
        let mut ribbon_action = None;
        if let Some(dc) = &mut self.dc {
            let keymap = &self.keymap;
            let ribbon = &mut self.ribbon;
            ribbon_action = egui::Panel::top("ribbon")
                .exact_size(tokens.ribbon_height)
                .show(ui, |ui| ui::ribbon::show(ui, dc, ribbon, keymap, &tokens))
                .inner;
        }
        if let Some(ui::ribbon::RibbonAction::GoToLibrary) = ribbon_action {
            self.close_document();
        }

        // Shortcuts, ribbon layout and script settings are only worth writing
        // out when the user actually changed one; autosave would otherwise be
        // up to 30s behind a force-quit.
        if ui::preferences::show(
            ctx,
            &mut self.prefs,
            &mut self.keymap,
            &mut self.ribbon,
            &mut self.script_prefs,
            &mut self.assistant_prefs,
            &mut self.llm_downloads,
        ) {
            // Turning enrichment on (or changing the model) is the worker's
            // cue to start; it is the only place either can change.
            if let Some(lib) = &self.library {
                lib.set_assistant(&self.assistant_prefs, &self.script_prefs.model);
            }
            if let Some(storage) = frame.storage_mut() {
                self.save(storage);
                storage.flush();
            }
        }
        self.scripts_window(ctx);

        let (wizard_title, wizard_pages) = match &self.dc {
            Some(dc) => (Some(dc.title()), dc.pages.len()),
            None => (None, 0),
        };
        if let Some(confirm) = ui::merge_wizard::show(
            ctx,
            &mut self.wizard,
            self.dc.is_some(),
            wizard_title.as_deref(),
            wizard_pages,
        ) {
            match confirm.dest {
                ui::merge_wizard::Destination::AppendToCurrent => {
                    self.insert_files(ctx, confirm.files)
                }
                ui::merge_wizard::Destination::NewDocument => {
                    self.combine_files(ctx, confirm.files)
                }
            }
        }

        egui::Panel::bottom("status").show(ui, |ui| {
            self.status_bar(ui);
        });

        self.collect_chat_answer();

        let mut open_extracted: Option<Vec<u8>> = None;
        let mut pending_temp_file: Option<PathBuf> = None;
        let mut pending_error: Option<String> = None;
        let mut chat_action: Option<ui::chat::ChatAction> = None;
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
            // Outermost on the right: the chat is about the whole document,
            // the inspector about one piece of markup inside it.
            if dc.chat.open {
                let engine = self.chat_engine.as_ref();
                chat_action = egui::Panel::right("chat")
                    .resizable(true)
                    .default_size(340.0)
                    .min_size(240.0)
                    .show(ui, |ui| ui::chat::show(ui, dc, engine))
                    .inner;
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

        if let Some(action) = chat_action {
            self.handle_chat_action(action, ctx);
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

/// Collect the OCR text the ⌘F cache holds, keyed by source page, as export
/// layers. Pages whose text came from the PDF itself are skipped: they are
/// already selectable.
fn ocr_layers(dc: &DocState) -> Option<HashMap<usize, Vec<OcrLine>>> {
    let mut layers: HashMap<usize, Vec<OcrLine>> = HashMap::new();
    for (&page, layout) in &dc.page_text {
        if layout.source != Some(TextSource::Ocr) {
            continue;
        }
        let lines: Vec<OcrLine> = layout
            .lines
            .iter()
            .filter(|line| !line.text.trim().is_empty())
            .filter_map(|line| {
                let rect = line
                    .chars
                    .iter()
                    .map(|c| c.rect)
                    .reduce(|acc, r| acc.union(r))?;
                Some(OcrLine {
                    text: line.text.clone(),
                    rect,
                })
            })
            .collect();
        if !lines.is_empty() {
            layers.insert(page, lines);
        }
    }
    (!layers.is_empty()).then_some(layers)
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
