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
    /// Rectangular revision cloud, dragged out like a rectangle.
    Cloud,
    /// Closed outline, one clicked vertex at a time.
    Polygon,
    /// Open chain of segments, one clicked vertex at a time.
    PolyLine,
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
            ActiveTool::Cloud => "Cloud",
            ActiveTool::Polygon => "Polygon",
            ActiveTool::PolyLine => "Polyline",
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
                | ActiveTool::Cloud
        )
    }

    /// Tools built by clicking vertices rather than by dragging a box.
    pub fn places_vertices(self) -> bool {
        matches!(self, ActiveTool::Polygon | ActiveTool::PolyLine)
    }

    /// How many vertices this tool needs before it has a shape.
    fn least_vertices(self) -> usize {
        match self {
            ActiveTool::Polygon => 3,
            _ => 2,
        }
    }
}

/// The intensity a cloud is drawn with until somebody changes it.
pub const DEFAULT_CLOUD_INTENSITY: f32 = 1.0;

/// The four corners of `rect`, counter-clockwise from the bottom left.
///
/// Counter-clockwise because that is the winding `cloud_arcs` reads to decide
/// which side of the outline the scallops belong on.
pub fn rect_points(rect: PdfRect) -> Vec<PdfPoint> {
    vec![
        PdfPoint::new(rect.min.x, rect.min.y),
        PdfPoint::new(rect.max.x, rect.min.y),
        PdfPoint::new(rect.max.x, rect.max.y),
        PdfPoint::new(rect.min.x, rect.max.y),
    ]
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
    /// Vertices placed so far by the Polygon/PolyLine tools, plus where the
    /// pointer is now so the outline can be drawn as far as the cursor.
    Placing {
        page: usize,
        points: Vec<PdfPoint>,
        hover: PdfPoint,
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
            ToolState::Creating { page, .. }
            | ToolState::Drawing { page, .. }
            | ToolState::Placing { page, .. } => Some(*page),
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
        tool if tool.places_vertices() => {
            dc.selection = None;
            let state = std::mem::replace(&mut dc.tool_ctl.state, ToolState::Idle);
            let mut points = match state {
                // Vertices already going down on this page continue; a click
                // that lands on a different page starts again there, which is
                // the only reading of it that isn't a shape spanning two pages.
                ToolState::Placing { page, points, .. } if page == p.page => points,
                _ => Vec::new(),
            };
            points.push(p.pos);
            dc.tool_ctl.state = ToolState::Placing {
                page: p.page,
                points,
                hover: p.pos,
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
        ToolState::Placing { page, points, .. } => {
            // Dragging with a vertex tool is just a slow click: the shape only
            // grows where the pointer is let go, which `on_press` has already
            // recorded. The hover point follows so the preview does too.
            dc.tool_ctl.state = ToolState::Placing {
                page,
                points,
                hover: p.pos,
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

            if let Handle::Vertex(index) = handle {
                if snapping {
                    let others = other_bounds(dc, orig.page, Some(orig.id));
                    let result = snap_rect(
                        PdfRect::from_points(pos, pos),
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
                if let Some(points) = ann.kind.points_mut()
                    && let Some(vertex) = points.get_mut(index)
                {
                    *vertex = pos;
                    let moved = points.clone();
                    ann.rect = pen::bounding_rect(&moved);
                }
                dc.store.replace(ann);
                dc.tool_ctl.state = ToolState::Resizing { orig, handle };
                return;
            }

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
        // Letting go of the pointer does not end a vertex gesture: it ends
        // when the user says so (a double click, or Enter).
        ToolState::Placing {
            page,
            points,
            hover,
        } => {
            dc.tool_ctl.state = ToolState::Placing {
                page,
                points,
                hover,
            };
        }
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
                ActiveTool::Cloud => (
                    AnnotationKind::Polygon {
                        points: rect_points(rect),
                        cloudy: Some(DEFAULT_CLOUD_INTENSITY),
                    },
                    dc.current_style,
                ),
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

/// The pointer moved over a page with no button down.
///
/// Only the vertex tools care: the segment from the last placed vertex to the
/// cursor is what tells the user where the next click will land.
pub fn on_hover(dc: &mut DocState, page: usize, pos: PdfPoint) {
    if let ToolState::Placing {
        page: placing,
        hover,
        ..
    } = &mut dc.tool_ctl.state
        && *placing == page
    {
        *hover = pos;
    }
}

/// Finish a vertex gesture (double click, or Enter), turning the vertices
/// placed so far into an annotation. Returns whether one was made.
///
/// Too few vertices is not an error to report -- a stray double click on the
/// page is a slip, and the shape is simply abandoned.
pub fn finish_placement(dc: &mut DocState) -> bool {
    let ToolState::Placing { page, points, .. } =
        std::mem::replace(&mut dc.tool_ctl.state, ToolState::Idle)
    else {
        return false;
    };
    dc.tool_ctl.clear_guides();
    // A double click puts the same vertex down twice; the second one is the
    // instruction to stop, not a corner.
    let mut points = points;
    while points.len() >= 2 {
        let last = points[points.len() - 1];
        let before = points[points.len() - 2];
        if (last.x - before.x).abs() < 0.5 && (last.y - before.y).abs() < 0.5 {
            points.pop();
        } else {
            break;
        }
    }
    if points.len() < dc.tool.least_vertices() {
        return false;
    }
    let kind = match dc.tool {
        ActiveTool::Polygon => AnnotationKind::Polygon {
            points: points.clone(),
            cloudy: None,
        },
        ActiveTool::PolyLine => AnnotationKind::PolyLine {
            points: points.clone(),
            arrow_end: false,
        },
        _ => return false,
    };
    let id = dc.store.alloc_id();
    let ann = Annotation {
        id,
        page,
        kind,
        rect: pen::bounding_rect(&points),
        style: dc.current_style,
    };
    dc.selection = Some(id);
    dc.history
        .apply(Command::AddAnnotation(ann), &mut dc.store, &mut dc.pages);
    true
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::history::Command;
    use crate::render::engine::EnginePref;
    use eframe::egui;

    fn doc_state() -> DocState {
        let ctx = egui::Context::default();
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("fixture");
        let doc = crate::doc::Document::load_bytes(bytes, None).expect("load");
        DocState::new(doc, &ctx, EnginePref::Hayro)
    }

    fn at(x: f32, y: f32) -> PointerInfo {
        PointerInfo {
            page: 0,
            pos: PdfPoint::new(x, y),
            modifiers: Modifiers::NONE,
            tol: 4.0,
            page_w: 612.0,
            page_h: 792.0,
        }
    }

    /// One click is one vertex: the click arrives as a press and a release
    /// together (that is how the canvas routes it), and neither may end the
    /// shape -- only the user saying so does.
    fn click(dc: &mut DocState, x: f32, y: f32) {
        let p = at(x, y);
        on_press(dc, &p);
        on_release(dc, &p);
    }

    #[test]
    fn clicking_out_a_polygon_makes_one_annotation_and_one_undo_step() {
        let mut dc = doc_state();
        dc.tool = ActiveTool::Polygon;
        click(&mut dc, 100.0, 100.0);
        click(&mut dc, 200.0, 100.0);
        assert!(
            dc.store.on_page(0).next().is_none(),
            "nothing exists until the shape is finished"
        );
        assert!(
            matches!(&dc.tool_ctl.state, ToolState::Placing { points, .. } if points.len() == 2)
        );

        // Two vertices are not a polygon.
        assert!(!finish_placement(&mut dc));
        assert!(dc.store.on_page(0).next().is_none());

        dc.tool_ctl.state = ToolState::Idle;
        click(&mut dc, 100.0, 100.0);
        click(&mut dc, 200.0, 100.0);
        click(&mut dc, 150.0, 200.0);
        assert!(finish_placement(&mut dc));

        let made = dc.store.on_page(0).next().expect("a polygon").clone();
        match &made.kind {
            AnnotationKind::Polygon { points, cloudy } => {
                assert_eq!(points.len(), 3);
                assert_eq!(*cloudy, None, "the Polygon tool draws a plain outline");
            }
            other => panic!("got {other:?}"),
        }
        assert_eq!(made.rect.min.x, 100.0);
        assert_eq!(made.rect.max.y, 200.0);
        assert_eq!(dc.selection, Some(made.id));

        // One undo takes the whole shape back, and redo brings it back whole.
        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        assert!(dc.store.get(made.id).is_none());
        assert!(dc.history.redo(&mut dc.store, &mut dc.pages));
        assert_eq!(dc.store.get(made.id), Some(&made));
    }

    /// A double click puts the same vertex down twice before it says "done";
    /// the repeat is the instruction, not a corner.
    #[test]
    fn finishing_on_a_repeated_vertex_does_not_keep_it() {
        let mut dc = doc_state();
        dc.tool = ActiveTool::PolyLine;
        click(&mut dc, 100.0, 100.0);
        click(&mut dc, 200.0, 150.0);
        click(&mut dc, 200.0, 150.0);
        assert!(finish_placement(&mut dc));
        let made = dc.store.on_page(0).next().expect("a polyline");
        match &made.kind {
            AnnotationKind::PolyLine { points, arrow_end } => {
                assert_eq!(points.len(), 2, "{points:?}");
                assert!(!arrow_end, "the arrowhead is off until asked for");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn escape_abandons_the_vertices_placed_so_far() {
        let mut dc = doc_state();
        dc.tool = ActiveTool::Polygon;
        click(&mut dc, 10.0, 10.0);
        click(&mut dc, 90.0, 10.0);
        cancel(&mut dc);
        assert!(matches!(dc.tool_ctl.state, ToolState::Idle));
        assert!(dc.store.on_page(0).next().is_none());
        assert!(!dc.history.can_undo(), "an abandoned shape is not history");
    }

    /// The Cloud tool is the rectangle drag everybody already knows, and what
    /// it leaves behind is a four-cornered cloudy polygon.
    #[test]
    fn dragging_the_cloud_tool_leaves_a_cloudy_rectangle() {
        let mut dc = doc_state();
        dc.tool = ActiveTool::Cloud;
        on_press(&mut dc, &at(100.0, 100.0));
        on_drag(&mut dc, &at(300.0, 220.0));
        on_release(&mut dc, &at(300.0, 220.0));

        let made = dc.store.on_page(0).next().expect("a cloud").clone();
        match &made.kind {
            AnnotationKind::Polygon { points, cloudy } => {
                assert_eq!(points.len(), 4);
                assert_eq!(*cloudy, Some(DEFAULT_CLOUD_INTENSITY));
                // Counter-clockwise, which is what puts the scallops outside.
                assert_eq!(points[0], PdfPoint::new(made.rect.min.x, made.rect.min.y));
                assert_eq!(points[1], PdfPoint::new(made.rect.max.x, made.rect.min.y));
                assert_eq!(points[3], PdfPoint::new(made.rect.min.x, made.rect.max.y));
            }
            other => panic!("got {other:?}"),
        }
        assert!(dc.history.can_undo());
    }

    /// Everything that can be done to one of the new shapes has to be
    /// undoable: making it, moving it, dragging a corner, editing it.
    #[test]
    fn moving_and_reshaping_a_polygon_are_undoable() {
        let mut dc = doc_state();
        dc.tool = ActiveTool::Polygon;
        click(&mut dc, 100.0, 100.0);
        click(&mut dc, 200.0, 100.0);
        click(&mut dc, 150.0, 200.0);
        finish_placement(&mut dc);
        let made = dc.store.on_page(0).next().expect("a polygon").clone();

        // Drag the whole shape with the select tool.
        dc.tool = ActiveTool::Select;
        on_press(&mut dc, &at(150.0, 100.0));
        assert!(matches!(dc.tool_ctl.state, ToolState::Dragging { .. }));
        on_drag(&mut dc, &at(160.0, 130.0));
        on_release(&mut dc, &at(160.0, 130.0));
        let moved = dc.store.get(made.id).expect("still there").clone();
        assert_ne!(moved, made, "the drag moved it");
        let first = moved.kind.points().expect("points")[0];
        assert!((first.y - 130.0).abs() < 1.0, "{first:?}");

        // Drag one vertex.
        on_press(&mut dc, &at(first.x, first.y));
        assert!(
            matches!(
                dc.tool_ctl.state,
                ToolState::Resizing {
                    handle: crate::tools::select::Handle::Vertex(0),
                    ..
                }
            ),
            "a corner is grabbed by its own handle"
        );
        on_drag(&mut dc, &at(first.x - 40.0, first.y - 40.0));
        on_release(&mut dc, &at(first.x - 40.0, first.y - 40.0));
        let reshaped = dc.store.get(made.id).expect("still there").clone();
        assert_ne!(reshaped, moved);
        assert!(reshaped.rect.min.x < moved.rect.min.x, "the box followed");

        // And the whole session unwinds, step by step, back to nothing.
        for expected in [Some(moved), Some(made), None] {
            dc.history.undo(&mut dc.store, &mut dc.pages);
            assert_eq!(dc.store.get(1).cloned(), expected);
        }
        assert!(!dc.history.can_undo());
    }

    /// Changing the cloudiness is an ordinary annotation edit, so the history
    /// machinery carries it with no help from the new kinds.
    #[test]
    fn turning_a_cloud_off_and_on_again_is_undoable() {
        let mut dc = doc_state();
        let id = dc.store.alloc_id();
        let ann = Annotation {
            id,
            page: 0,
            kind: AnnotationKind::Polygon {
                points: rect_points(PdfRect::from_points(
                    PdfPoint::new(10.0, 10.0),
                    PdfPoint::new(90.0, 60.0),
                )),
                cloudy: Some(DEFAULT_CLOUD_INTENSITY),
            },
            rect: PdfRect::from_points(PdfPoint::new(10.0, 10.0), PdfPoint::new(90.0, 60.0)),
            style: dc.current_style,
        };
        dc.history.apply(
            Command::AddAnnotation(ann.clone()),
            &mut dc.store,
            &mut dc.pages,
        );

        let mut plain = ann.clone();
        if let AnnotationKind::Polygon { cloudy, .. } = &mut plain.kind {
            *cloudy = None;
        }
        dc.store.replace(plain.clone());
        dc.history.record(Command::ModifyAnnotation {
            before: ann.clone(),
            after: plain,
        });

        dc.history.undo(&mut dc.store, &mut dc.pages);
        assert_eq!(dc.store.get(id), Some(&ann), "the scallops came back");
    }
}
