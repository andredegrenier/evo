//! Zoom/scroll state and page layout for the continuous vertical page view.

use eframe::egui::{Rect, Vec2, pos2, vec2};

use crate::doc::Document;
use crate::doc::page_ops::PageList;

/// Screen-space margin around the page stack and gap between pages.
pub const PAGE_MARGIN: f32 = 24.0;
pub const PAGE_GAP: f32 = 16.0;

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 8.0;

pub struct Viewport {
    /// Screen (egui) points per PDF point.
    pub zoom: f32,
    /// When set, zoom is recomputed each frame to fit the widest page.
    pub fit_width: bool,
    /// One-shot scroll offset override applied on the next frame.
    pub pending_offset: Option<Vec2>,
    /// Current scroll offset, mirrored from the ScrollArea each frame.
    pub offset: Vec2,
    /// One-shot request to scroll a display-position page into view.
    pub scroll_to_page: Option<usize>,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            fit_width: true,
            pending_offset: None,
            offset: Vec2::ZERO,
            scroll_to_page: None,
        }
    }
}

/// One visible page's place in content coordinates (scrolled space, origin at
/// the top-left of the whole page stack).
#[derive(Clone, Copy, Debug)]
pub struct PageSlot {
    /// Position in the display order.
    pub position: usize,
    /// Index into the original document's pages.
    pub original: usize,
    pub rect: Rect,
}

pub struct Layout {
    pub slots: Vec<PageSlot>,
    pub content_size: Vec2,
}

impl Viewport {
    /// Displayed size (points) of an original page, honoring user rotation.
    pub fn page_display_size(doc: &Document, pages: &PageList, logical: usize) -> Vec2 {
        let info = &doc.pages[pages.source_of(logical)];
        if pages.rotation_of(logical).swaps_axes() {
            vec2(info.height, info.width)
        } else {
            vec2(info.width, info.height)
        }
    }

    /// Lay out all visible pages vertically, centered on the widest page,
    /// in content coordinates. `min_width` lets the caller center the stack
    /// in a wide window.
    pub fn layout(&self, doc: &Document, pages: &PageList, min_width: f32) -> Layout {
        let mut slots = Vec::with_capacity(pages.len());
        let widest = pages
            .order
            .iter()
            .map(|&orig| Self::page_display_size(doc, pages, orig).x * self.zoom)
            .fold(0.0f32, f32::max);
        let content_w = (widest + 2.0 * PAGE_MARGIN).max(min_width);

        let mut y = PAGE_MARGIN;
        for (position, &original) in pages.order.iter().enumerate() {
            let size = Self::page_display_size(doc, pages, original) * self.zoom;
            let x = (content_w - size.x) / 2.0;
            slots.push(PageSlot {
                position,
                original,
                rect: Rect::from_min_size(pos2(x, y), size),
            });
            y += size.y + PAGE_GAP;
        }
        let content_h = y - PAGE_GAP + PAGE_MARGIN;
        Layout {
            slots,
            content_size: vec2(content_w, content_h.max(0.0)),
        }
    }

    /// Zoom that makes the widest page fill `available` width.
    pub fn fit_width_zoom(doc: &Document, pages: &PageList, available: f32) -> f32 {
        let widest = pages
            .order
            .iter()
            .map(|&orig| Self::page_display_size(doc, pages, orig).x)
            .fold(1.0f32, f32::max);
        ((available - 2.0 * PAGE_MARGIN) / widest).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.fit_width = false;
    }

    /// Zoom keeping the viewport point `anchor_in_viewport` fixed relative to
    /// the content under it. `layout_before` is this frame's layout.
    pub fn zoom_about(
        &mut self,
        doc: &Document,
        pages: &PageList,
        layout_before: &Layout,
        anchor_in_viewport: Vec2,
        new_zoom: f32,
        min_width: f32,
    ) {
        let new_zoom = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        let anchor_content = self.offset + anchor_in_viewport;

        // Anchor to the page under the cursor for exact tracking; fall back to
        // proportional scaling in the margins.
        let anchored = layout_before.slots.iter().find(|s| {
            anchor_content.y >= s.rect.min.y - PAGE_GAP / 2.0
                && anchor_content.y <= s.rect.max.y + PAGE_GAP / 2.0
        });

        let old_zoom = self.zoom;
        self.set_zoom(new_zoom);

        let new_offset = if let Some(slot) = anchored {
            let frac = vec2(
                (anchor_content.x - slot.rect.min.x) / slot.rect.width().max(1.0),
                (anchor_content.y - slot.rect.min.y) / slot.rect.height().max(1.0),
            );
            let layout_after = self.layout(doc, pages, min_width);
            let new_slot = layout_after.slots[slot.position];
            let new_anchor_content = new_slot.rect.min.to_vec2()
                + vec2(
                    frac.x * new_slot.rect.width(),
                    frac.y * new_slot.rect.height(),
                );
            new_anchor_content - anchor_in_viewport
        } else {
            anchor_content * (new_zoom / old_zoom) - anchor_in_viewport
        };
        self.pending_offset = Some(new_offset.max(Vec2::ZERO));
    }
}
