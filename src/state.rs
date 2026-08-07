//! Per-document editor state: the document itself plus everything the UI
//! needs to edit it (selection, active tool, undo history, render caches).

use eframe::egui;

use crate::doc::Document;
use crate::doc::annotation::{AnnotationId, Style};
use crate::doc::history::History;
use crate::doc::page_ops::PageList;
use crate::doc::store::AnnotationStore;
use crate::render::RenderWorker;
use crate::render::cache::TextureCache;
use crate::tools::{ActiveTool, ToolController};
use crate::ui::viewport::Viewport;

pub struct DocState {
    pub doc: Document,
    pub pages: PageList,
    pub store: AnnotationStore,
    pub history: History,
    pub worker: RenderWorker,
    pub cache: TextureCache,
    pub thumb_cache: TextureCache,
    pub viewport: Viewport,

    pub tool: ActiveTool,
    pub tool_ctl: ToolController,
    pub selection: Option<AnnotationId>,
    /// Annotation currently in inline text-edit mode.
    pub editing_text: Option<AnnotationId>,
    /// Style applied to newly created annotations.
    pub current_style: Style,
    pub current_font_size: f32,
}

impl DocState {
    pub fn new(doc: Document, ctx: &egui::Context) -> Self {
        let worker = RenderWorker::spawn(doc.source.clone(), ctx.clone());
        let page_count = doc.pages.len();
        Self {
            doc,
            pages: PageList::new(page_count),
            store: AnnotationStore::default(),
            history: History::default(),
            worker,
            cache: TextureCache::default(),
            thumb_cache: TextureCache::default(),
            viewport: Viewport::default(),
            tool: ActiveTool::Select,
            tool_ctl: ToolController::default(),
            selection: None,
            editing_text: None,
            current_style: Style::default(),
            current_font_size: 14.0,
        }
    }

    /// True when there is anything to save (markup or page changes).
    pub fn is_modified(&self) -> bool {
        !self.store.is_empty() || self.pages.is_modified(self.doc.pages.len())
    }

    pub fn selected_annotation(&self) -> Option<&crate::doc::annotation::Annotation> {
        self.selection.and_then(|id| self.store.get(id))
    }
}
