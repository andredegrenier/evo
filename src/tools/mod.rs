//! Markup tools and the pointer-interaction state machine. The canvas feeds
//! pointer events here; completed gestures become undoable commands.

pub mod pen;
pub mod select;
pub mod snap;

use eframe::egui::Modifiers;

use crate::doc::annotation::{Annotation, AnnotationId, AnnotationKind, Color, Style, TextAlign};
use crate::doc::geometry::{PdfPoint, PdfRect};
use crate::doc::history::Command;
use crate::state::DocState;
use select::Handle;
use snap::{Guide, SnapFeatures, snap_rect};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum ActiveTool {
    Select,
    Pan,
    Highlight,
    Text,
    Rect,
    Ellipse,
    Line,
    Arrow,
    Pen,
}

impl ActiveTool {
    pub fn label(self) -> &'static str {
        match self {
            ActiveTool::Select => "Select",
            ActiveTool::Pan => "Pan",
            ActiveTool::Highlight => "Highlight",
            ActiveTool::Text => "Text",
            ActiveTool::Rect => "Rectangle",
            ActiveTool::Ellipse => "Ellipse",
            ActiveTool::Line => "Line",
            ActiveTool::Arrow => "Arrow",
            ActiveTool::Pen => "Pen",
        }
    }

    fn is_drag_create(self) -> bool {
        matches!(
            self,
            ActiveTool::Highlight
                | ActiveTool::Rect
                | ActiveTool::Ellipse
                | ActiveTool::Line
                | ActiveTool::Arrow
        )
    }
}

pub enum ToolState {
    Idle,
    /// Drag-creating a shape on `page` (original index).
    Creating {
        page: usize,
        start: PdfPoint,
        current: PdfPoint,
    },
    /// Freehand stroke in progress.
    Drawing {
        page: usize,
        points: Vec<PdfPoint>,
    },
    /// Moving an existing annotation.
    Dragging {
        orig: Annotation,
        press: PdfPoint,
        moved: bool,
    },
    /// Resizing via a handle.
    Resizing {
        orig: Annotation,
        handle: Handle,
    },
}

/// Interaction state + live snap guides, owned by [`DocState`].
pub struct ToolController {
    pub state: ToolState,
    /// Guides to draw this frame, on `guides_page` (original index).
    pub guides: Vec<Guide>,
    pub guides_page: Option<usize>,
    /// Snapshot of a text annotation before inline editing began
    /// (`None` while editing a freshly created one).
    pub editing_before: Option<Annotation>,
    /// Give the inline text editor keyboard focus on its first frame.
    pub editing_focus_pending: bool,
    /// Snapshot for coalescing a numeric-field edit gesture in the inspector.
    pub inspector_before: Option<Annotation>,
}

impl Default for ToolController {
    fn default() -> Self {
        Self {
            state: ToolState::Idle,
            guides: Vec::new(),
            guides_page: None,
            editing_before: None,
            editing_focus_pending: false,
            inspector_before: None,
        }
    }
}

/// Everything about the pointer the tools need for one event.
pub struct PointerInfo {
    /// Original page index the event applies to (the interaction's page for
    /// drag/release events).
    pub page: usize,
    /// Pointer position in that page's PDF space.
    pub pos: PdfPoint,
    pub modifiers: Modifiers,
    /// Snap tolerance in PDF points (screen px / zoom); also used for handle
    /// and hit tolerances.
    pub tol: f32,
    /// Page size in PDF points (displayed orientation, pre-user-rotation).
    pub page_w: f32,
    pub page_h: f32,
}

impl ToolController {
    pub fn active_page(&self) -> Option<usize> {
        match &self.state {
            ToolState::Idle => None,
            ToolState::Creating { page, .. } | ToolState::Drawing { page, .. } => Some(*page),
            ToolState::Dragging { orig, .. } | ToolState::Resizing { orig, .. } => Some(orig.page),
        }
    }

    fn set_guides(&mut self, page: usize, guides: Vec<Guide>) {
        self.guides = guides;
        self.guides_page = Some(page);
    }

    fn clear_guides(&mut self) {
        self.guides.clear();
        self.guides_page = None;
    }
}

fn other_bounds(dc: &DocState, page: usize, exclude: Option<AnnotationId>) -> Vec<PdfRect> {
    dc.store
        .on_page(page)
        .filter(|a| Some(a.id) != exclude)
        .map(|a| a.bounds())
        .collect()
}

/// Pointer pressed on a page with the current tool.
pub fn on_press(dc: &mut DocState, p: &PointerInfo) {
    match dc.tool {
        ActiveTool::Pan => {}
        ActiveTool::Select => {
            // Handles of the current selection take priority.
            if let Some(ann) = dc.selected_annotation().cloned()
                && ann.page == p.page
                && let Some(handle) = select::handle_at(&ann, p.pos, p.tol * 1.5)
            {
                dc.tool_ctl.state = ToolState::Resizing { orig: ann, handle };
                return;
            }
            if let Some(id) = select::hit_test(&dc.store, p.page, p.pos, p.tol) {
                dc.selection = Some(id);
                let orig = dc.store.get(id).unwrap().clone();
                dc.tool_ctl.state = ToolState::Dragging {
                    orig,
                    press: p.pos,
                    moved: false,
                };
            } else {
                dc.selection = None;
            }
        }
        ActiveTool::Text => {
            let id = dc.store.alloc_id();
            let w = 180.0f32;
            let h = (dc.current_font_size * 1.4 + 8.0).max(28.0);
            let rect = PdfRect::from_points(p.pos, PdfPoint::new(p.pos.x + w, p.pos.y - h));
            let ann = Annotation {
                id,
                page: p.page,
                kind: AnnotationKind::TextBox {
                    text: String::new(),
                    font_size: dc.current_font_size,
                    align: TextAlign::Left,
                },
                rect,
                style: dc.current_style,
            };
            dc.store.insert(ann);
            dc.selection = Some(id);
            dc.editing_text = Some(id);
            dc.tool_ctl.editing_before = None;
            dc.tool_ctl.editing_focus_pending = true;
            dc.tool = ActiveTool::Select;
        }
        ActiveTool::Pen => {
            dc.tool_ctl.state = ToolState::Drawing {
                page: p.page,
                points: vec![p.pos],
            };
        }
        tool if tool.is_drag_create() => {
            dc.selection = None;
            dc.tool_ctl.state = ToolState::Creating {
                page: p.page,
                start: p.pos,
                current: p.pos,
            };
        }
        _ => {}
    }
}

/// Pointer moved while a gesture is in progress.
pub fn on_drag(dc: &mut DocState, p: &PointerInfo) {
    let snapping = !p.modifiers.mac_cmd && !p.modifiers.command;
    let state = std::mem::replace(&mut dc.tool_ctl.state, ToolState::Idle);
    match state {
        ToolState::Idle => {}
        ToolState::Creating { page, start, .. } => {
            let mut pos = p.pos;
            if p.modifiers.shift {
                pos = constrain(start, pos, dc.tool);
            }
            if snapping {
                let features = SnapFeatures {
                    left: pos.x < start.x,
                    right: pos.x >= start.x,
                    top: pos.y >= start.y,
                    bottom: pos.y < start.y,
                    center_x: false,
                    center_y: false,
                };
                let rect = PdfRect::from_points(start, pos);
                let others = other_bounds(dc, page, None);
                let result = snap_rect(rect, features, p.page_w, p.page_h, &others, p.tol);
                pos.x += result.correction.dx;
                pos.y += result.correction.dy;
                dc.tool_ctl.set_guides(page, result.guides);
            } else {
                dc.tool_ctl.clear_guides();
            }
            dc.tool_ctl.state = ToolState::Creating {
                page,
                start,
                current: pos,
            };
        }
        ToolState::Drawing { page, mut points } => {
            let last = points.last().copied();
            if last.is_none_or(|l| (l.x - p.pos.x).abs() + (l.y - p.pos.y).abs() > 0.2) {
                points.push(p.pos);
            }
            dc.tool_ctl.state = ToolState::Drawing { page, points };
        }
        ToolState::Dragging { orig, press, .. } => {
            let dx = p.pos.x - press.x;
            let dy = p.pos.y - press.y;
            let mut rect = orig.rect.translated(dx, dy);
            if snapping {
                let others = other_bounds(dc, orig.page, Some(orig.id));
                let result = snap_rect(rect, SnapFeatures::ALL, p.page_w, p.page_h, &others, p.tol);
                rect = rect.translated(result.correction.dx, result.correction.dy);
                dc.tool_ctl.set_guides(orig.page, result.guides);
            } else {
                dc.tool_ctl.clear_guides();
            }
            let mut ann = orig.clone();
            ann.translate(rect.min.x - orig.rect.min.x, rect.min.y - orig.rect.min.y);
            dc.store.replace(ann);
            dc.tool_ctl.state = ToolState::Dragging {
                orig,
                press,
                moved: true,
            };
        }
        ToolState::Resizing { orig, handle } => {
            let mut pos = p.pos;

            if matches!(handle, Handle::LineStart | Handle::LineEnd) {
                if snapping {
                    let point_rect = PdfRect::from_points(pos, pos);
                    let others = other_bounds(dc, orig.page, Some(orig.id));
                    let result = snap_rect(
                        point_rect,
                        SnapFeatures {
                            left: true,
                            right: false,
                            center_x: false,
                            top: false,
                            bottom: true,
                            center_y: false,
                        },
                        p.page_w,
                        p.page_h,
                        &others,
                        p.tol,
                    );
                    pos.x += result.correction.dx;
                    pos.y += result.correction.dy;
                    dc.tool_ctl.set_guides(orig.page, result.guides);
                } else {
                    dc.tool_ctl.clear_guides();
                }
                let mut ann = orig.clone();
                if let AnnotationKind::Line { p1, p2, .. } = &mut ann.kind {
                    let anchor = if handle == Handle::LineStart {
                        *p2
                    } else {
                        *p1
                    };
                    let moved_pos = if p.modifiers.shift {
                        constrain(anchor, pos, ActiveTool::Line)
                    } else {
                        pos
                    };
                    if handle == Handle::LineStart {
                        *p1 = moved_pos;
                    } else {
                        *p2 = moved_pos;
                    }
                    ann.rect = PdfRect::from_points(*p1, *p2);
                }
                dc.store.replace(ann);
                dc.tool_ctl.state = ToolState::Resizing { orig, handle };
                return;
            }

            let mut rect = select::resize_rect(orig.rect, handle, pos, p.modifiers.shift);
            if snapping {
                let features = SnapFeatures {
                    left: handle.moves_left(),
                    right: handle.moves_right(),
                    top: handle.moves_top(),
                    bottom: handle.moves_bottom(),
                    center_x: false,
                    center_y: false,
                };
                let others = other_bounds(dc, orig.page, Some(orig.id));
                let result = snap_rect(rect, features, p.page_w, p.page_h, &others, p.tol);
                if result.correction.dx != 0.0 || result.correction.dy != 0.0 {
                    let snapped =
                        PdfPoint::new(pos.x + result.correction.dx, pos.y + result.correction.dy);
                    rect = select::resize_rect(orig.rect, handle, snapped, p.modifiers.shift);
                }
                dc.tool_ctl.set_guides(orig.page, result.guides);
            } else {
                dc.tool_ctl.clear_guides();
            }
            let mut ann = orig.clone();
            ann.set_bounds(rect);
            dc.store.replace(ann);
            dc.tool_ctl.state = ToolState::Resizing { orig, handle };
        }
    }
}

/// Pointer released: finish the gesture and record an undo command.
pub fn on_release(dc: &mut DocState, p: &PointerInfo) {
    let state = std::mem::replace(&mut dc.tool_ctl.state, ToolState::Idle);
    dc.tool_ctl.clear_guides();
    match state {
        ToolState::Idle => {}
        ToolState::Creating {
            page,
            start,
            current,
        } => {
            let rect = PdfRect::from_points(start, current);
            let long_enough = match dc.tool {
                ActiveTool::Line | ActiveTool::Arrow => {
                    ((current.x - start.x).powi(2) + (current.y - start.y).powi(2)).sqrt() > 3.0
                }
                _ => rect.width() > 3.0 && rect.height() > 3.0,
            };
            if !long_enough {
                return;
            }
            let id = dc.store.alloc_id();
            let (kind, style) = match dc.tool {
                ActiveTool::Highlight => (
                    AnnotationKind::Highlight,
                    Style {
                        stroke: Color::TRANSPARENT,
                        stroke_width: 0.0,
                        fill: Color::rgba(250, 220, 50, 255),
                        opacity: 0.45,
                    },
                ),
                ActiveTool::Rect => (AnnotationKind::Rect, dc.current_style),
                ActiveTool::Ellipse => (AnnotationKind::Ellipse, dc.current_style),
                ActiveTool::Line | ActiveTool::Arrow => (
                    AnnotationKind::Line {
                        p1: start,
                        p2: current,
                        arrow_end: dc.tool == ActiveTool::Arrow,
                    },
                    dc.current_style,
                ),
                _ => return,
            };
            let ann = Annotation {
                id,
                page,
                kind,
                rect,
                style,
            };
            dc.selection = Some(id);
            dc.history
                .apply(Command::AddAnnotation(ann), &mut dc.store, &mut dc.pages);
        }
        ToolState::Drawing { page, points } => {
            let points = pen::simplify(&points, 0.6);
            if points.len() < 2 {
                return;
            }
            let rect = pen::bounding_rect(&points);
            let id = dc.store.alloc_id();
            let ann = Annotation {
                id,
                page,
                kind: AnnotationKind::Freehand { points },
                rect,
                style: dc.current_style,
            };
            dc.selection = Some(id);
            dc.history
                .apply(Command::AddAnnotation(ann), &mut dc.store, &mut dc.pages);
        }
        ToolState::Dragging { orig, moved, .. } => {
            if moved
                && let Some(after) = dc.store.get(orig.id).cloned()
                && after != orig
            {
                dc.history.record(Command::ModifyAnnotation {
                    before: orig,
                    after,
                });
            }
        }
        ToolState::Resizing { orig, .. } => {
            if let Some(after) = dc.store.get(orig.id).cloned()
                && after != orig
            {
                dc.history.record(Command::ModifyAnnotation {
                    before: orig,
                    after,
                });
            }
        }
    }
    let _ = p;
}

/// Cancel any in-flight gesture (Esc).
pub fn cancel(dc: &mut DocState) {
    let state = std::mem::replace(&mut dc.tool_ctl.state, ToolState::Idle);
    dc.tool_ctl.clear_guides();
    match state {
        ToolState::Dragging { orig, .. } | ToolState::Resizing { orig, .. } => {
            dc.store.replace(orig);
        }
        _ => {}
    }
}

/// Shift-constrain a drag: square/circle for shapes, 45-degree steps for lines.
fn constrain(start: PdfPoint, pos: PdfPoint, tool: ActiveTool) -> PdfPoint {
    let dx = pos.x - start.x;
    let dy = pos.y - start.y;
    match tool {
        ActiveTool::Line | ActiveTool::Arrow => {
            let angle = dy.atan2(dx);
            let step = std::f32::consts::FRAC_PI_4;
            let snapped = (angle / step).round() * step;
            let len = (dx * dx + dy * dy).sqrt();
            PdfPoint::new(start.x + len * snapped.cos(), start.y + len * snapped.sin())
        }
        _ => {
            let side = dx.abs().max(dy.abs());
            PdfPoint::new(start.x + side * dx.signum(), start.y + side * dy.signum())
        }
    }
}
