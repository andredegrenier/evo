//! Per-document editor state: the document itself plus everything the UI
//! needs to edit it (selection, active tool, undo history, render caches).

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use eframe::egui;

use crate::doc::Document;
use crate::doc::annotation::{AnnotationId, Style};
use crate::doc::geometry::PdfRect;
use crate::doc::history::History;
use crate::doc::page_ops::PageList;
use crate::doc::store::AnnotationStore;
use crate::library::extract::PageTextLayout;
use crate::library::textjob::TextWorker;
use crate::render::RenderWorker;
use crate::render::cache::{self, TextureCache};
use crate::render::engine::{Engine, EnginePref};
use crate::tools::{ActiveTool, ToolController};
use crate::ui::chat::ChatSessionState;
use crate::ui::viewport::Viewport;

pub struct DocState {
    pub doc: Document,
    pub pages: PageList,
    pub store: AnnotationStore,
    pub history: History,
    pub worker: RenderWorker,
    /// Which rasterizer this document's worker was started with, so that
    /// changing the preference can tell whether there is anything to redo.
    pub engine_pref: EnginePref,
    /// Which one actually drew, learned from the first page that came back.
    /// `Auto` cannot be resolved from here: only the worker knows whether
    /// PDFium loaded.
    pub engine: Option<Engine>,
    pub cache: TextureCache,
    pub thumb_cache: TextureCache,
    /// Decoded pictures for image stamps, one per annotation.
    pub stamp_images: crate::ui::canvas::StampTextures,
    pub viewport: Viewport,

    pub tool: ActiveTool,
    pub tool_ctl: ToolController,
    pub selection: Selection,
    /// Annotation currently in inline text-edit mode.
    pub editing_text: Option<AnnotationId>,
    /// Style applied to newly created annotations.
    pub current_style: Style,
    pub current_font_size: f32,

    /// Multi-selection on the page rail, as display positions.
    pub rail: RailSelection,
    /// Logical page indices copied via the rail's Copy action.
    pub page_clipboard: Vec<usize>,
    /// Set when the underlying source bytes differ from the file on disk
    /// (e.g. after Insert Pages), independent of markup/page-list changes.
    pub force_modified: bool,
    /// When the document came from the library: its content id, for sidecar
    /// markup persistence.
    pub library_id: Option<String>,
    /// Display title when there is no filesystem path (library/untitled docs).
    pub title_override: Option<String>,

    /// Positioned text per *source* page, filled in by `text_worker`.
    pub page_text: HashMap<usize, PageTextLayout>,
    /// Background text extraction/OCR, started the first time ⌘F is used.
    pub text_worker: Option<TextWorker>,
    pub find: FindState,
    /// Chat-with-document conversation. Lives here so it dies with the
    /// document it is about.
    pub chat: ChatSessionState,
}

/// Find-in-document (⌘F) state.
#[derive(Default)]
pub struct FindState {
    pub open: bool,
    pub query: String,
    /// Matches in display order, at most one pass per source page.
    pub matches: Vec<FindMatch>,
    /// Index into `matches` of the highlighted hit.
    pub active: usize,
    /// Ask the find field for keyboard focus on the next frame.
    pub focus_pending: bool,
    /// Query the current `matches` were computed for.
    pub last_query: String,
    /// New page text arrived; recompute matches.
    pub dirty: bool,
}

/// One occurrence of the find query.
pub struct FindMatch {
    pub source_page: usize,
    /// Index into the page layout's lines.
    pub line: usize,
    /// Byte range inside that line's text.
    pub range: Range<usize>,
    /// Union of the matched characters' boxes, in display space.
    pub rect: PdfRect,
}

/// The markup that is selected, and which one of it the inspector is about.
///
/// A set rather than one id, because moving four things at once is what people
/// do with markup, and because a group is only a selection somebody made once.
/// `primary` is the last one added: with several selected the panel has to edit
/// *something*, and the thing just clicked is the least surprising answer.
#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub struct Selection {
    ids: BTreeSet<AnnotationId>,
    primary: Option<AnnotationId>,
}

impl Selection {
    /// Just this one, and nothing else.
    pub fn select_one(&mut self, id: AnnotationId) {
        self.ids.clear();
        self.ids.insert(id);
        self.primary = Some(id);
    }

    /// Exactly these, with `primary` the last of them still standing.
    pub fn select_all(&mut self, ids: impl IntoIterator<Item = AnnotationId>) {
        self.ids = ids.into_iter().collect();
        self.primary = self.ids.iter().next_back().copied();
    }

    /// Add or remove one (shift-click).
    pub fn toggle(&mut self, id: AnnotationId) {
        if self.ids.remove(&id) {
            if self.primary == Some(id) {
                self.primary = self.ids.iter().next_back().copied();
            }
        } else {
            self.ids.insert(id);
            self.primary = Some(id);
        }
    }

    /// Add these without dropping what is already selected.
    pub fn add_all(&mut self, ids: impl IntoIterator<Item = AnnotationId>) {
        for id in ids {
            self.ids.insert(id);
            self.primary = Some(id);
        }
    }

    pub fn clear(&mut self) {
        self.ids.clear();
        self.primary = None;
    }

    pub fn contains(&self, id: AnnotationId) -> bool {
        self.ids.contains(&id)
    }

    /// The one the inspector edits and whose handles are drawn.
    pub fn primary(&self) -> Option<AnnotationId> {
        self.primary
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = AnnotationId> + '_ {
        self.ids.iter().copied()
    }
}

/// Page-rail multi-selection (display positions).
#[derive(Default)]
pub struct RailSelection {
    pub selected: BTreeSet<usize>,
    pub anchor: Option<usize>,
}

impl RailSelection {
    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Apply a click at `pos` with the platform modifiers.
    pub fn click(&mut self, pos: usize, shift: bool, cmd: bool) {
        if shift && let Some(anchor) = self.anchor {
            let (a, b) = (anchor.min(pos), anchor.max(pos));
            if !cmd {
                self.selected.clear();
            }
            self.selected.extend(a..=b);
        } else if cmd {
            if !self.selected.remove(&pos) {
                self.selected.insert(pos);
            }
            self.anchor = Some(pos);
        } else {
            self.selected.clear();
            self.selected.insert(pos);
            self.anchor = Some(pos);
        }
    }
}

impl DocState {
    pub fn new(doc: Document, ctx: &egui::Context, pref: EnginePref) -> Self {
        // A protected document has to present its password again to whoever
        // re-parses its bytes, and the render worker is the first of those.
        let worker = RenderWorker::spawn(
            doc.source.clone(),
            ctx.clone(),
            pref,
            doc.password().map(str::to_owned),
        );
        let page_count = doc.pages.len();
        Self {
            doc,
            pages: PageList::new(page_count),
            store: AnnotationStore::default(),
            history: History::default(),
            worker,
            engine_pref: pref,
            engine: None,
            cache: TextureCache::with_budget(cache::CANVAS_BUDGET),
            thumb_cache: TextureCache::with_budget(cache::THUMB_BUDGET),
            stamp_images: Default::default(),
            viewport: Viewport::default(),
            tool: ActiveTool::Select,
            tool_ctl: ToolController::default(),
            selection: Selection::default(),
            editing_text: None,
            current_style: Style::default(),
            current_font_size: 14.0,
            rail: RailSelection::default(),
            page_clipboard: Vec::new(),
            force_modified: false,
            library_id: None,
            title_override: None,
            page_text: HashMap::new(),
            text_worker: None,
            find: FindState::default(),
            chat: ChatSessionState::default(),
        }
    }

    /// Build a state for `doc` (e.g. the merged result of Insert Pages) that
    /// carries over the editing session of `old`: markup, history, page list,
    /// viewport, and tool settings. Render workers and caches start fresh.
    pub fn adopt(doc: Document, ctx: &egui::Context, old: DocState) -> Self {
        let same_pages = doc.pages.len() == old.doc.pages.len();
        let pref = old.engine_pref;
        let mut new = Self::new(doc, ctx, pref);
        // Source page indices shift when pages are inserted, so the ⌘F text
        // cache only survives when the source document is unchanged.
        if same_pages {
            new.page_text = old.page_text;
            new.text_worker = old.text_worker;
        }
        new.find = old.find;
        new.find.dirty = true;
        // The conversation carries over, but the pages it was about have moved:
        // clearing the key makes the chat worker re-extract text for the merged
        // document instead of answering from the old one's cache.
        new.chat = old.chat;
        new.chat.doc_key = None;
        new.store = old.store;
        new.history = old.history;
        new.pages = old.pages;
        new.viewport = old.viewport;
        new.tool = old.tool;
        new.current_style = old.current_style;
        new.current_font_size = old.current_font_size;
        new.selection = old.selection;
        new.force_modified = true;
        new.library_id = old.library_id;
        new.title_override = old.title_override;
        new
    }

    pub fn title(&self) -> String {
        self.title_override
            .clone()
            .unwrap_or_else(|| self.doc.title())
    }

    /// True when there is anything to save (markup or page changes).
    pub fn is_modified(&self) -> bool {
        self.force_modified
            || !self.store.is_empty()
            || self.pages.is_modified(self.doc.pages.len())
    }

    pub fn selected_annotation(&self) -> Option<&crate::doc::annotation::Annotation> {
        self.selection.primary().and_then(|id| self.store.get(id))
    }

    /// Everything selected, in the order it is stored, skipping ids the store
    /// no longer has -- undo can take an annotation away while it is selected.
    pub fn selected_annotations(&self) -> Vec<crate::doc::annotation::Annotation> {
        self.selection
            .iter()
            .filter_map(|id| self.store.get(id).cloned())
            .collect()
    }

    /// Switch rasterizer without closing the document.
    ///
    /// Everything already drawn came from the old engine, so the worker is
    /// replaced and both texture caches are thrown away: half a page from each
    /// renderer would be the one bug this whole seam exists to avoid.
    pub fn set_engine_pref(&mut self, pref: EnginePref, ctx: &egui::Context) {
        if self.engine_pref == pref {
            return;
        }
        self.engine_pref = pref;
        self.engine = None;
        self.worker = RenderWorker::spawn(
            self.doc.source.clone(),
            ctx.clone(),
            pref,
            self.doc.password().map(str::to_owned),
        );
        self.cache = TextureCache::with_budget(cache::CANVAS_BUDGET);
        self.thumb_cache = TextureCache::with_budget(cache::THUMB_BUDGET);
        ctx.request_repaint();
    }
}
