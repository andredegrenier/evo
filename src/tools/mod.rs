//! Markup tools and the pointer-interaction state machine. The canvas feeds
//! pointer events here; completed gestures become undoable commands.

pub mod pen;
pub mod select;
pub mod snap;

use eframe::egui::Modifiers;

use crate::doc::annotation::{
    Annotation, AnnotationId, AnnotationKind, Color, GroupId, Style, TextAlign,
};
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
    /// A word in a box, placed with one click.
    Stamp,
    /// A picture, placed with one click once one has been chosen.
    ImageStamp,
    /// Numbered stamps, counting up as they are placed.
    Sequence,
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
            ActiveTool::Stamp => "Stamp",
            ActiveTool::ImageStamp => "Image Stamp",
            ActiveTool::Sequence => "Sequence",
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

/// The red a stamp arrives in: the one every review set is already covered in.
pub const STAMP_RED: Color = Color::rgb(193, 39, 45);

/// How big a stamp's word is until somebody changes it.
pub const DEFAULT_STAMP_FONT: f32 = 20.0;
/// And a sequence number, which is a stamp with a number in it.
pub const DEFAULT_SEQUENCE_FONT: f32 = 12.0;

/// The largest picture a stamp may carry, in bytes of PNG.
///
/// The bytes live in the markup sidecar, which travels to phones and back
/// through an API on every save; a photograph dropped in as a signature would
/// make every one of those trips carry it.
pub const MAX_STAMP_PNG: usize = 2 * 1024 * 1024;

/// What the stamp tool will place next.
#[derive(Clone, Debug)]
pub struct StampSettings {
    /// As typed, tokens and all -- they are expanded when the stamp is placed.
    pub text: String,
    pub font_size: f32,
    /// A PNG chosen from disk, waiting for a click to land on.
    pub image: Option<Vec<u8>>,
}

impl Default for StampSettings {
    fn default() -> Self {
        Self {
            text: crate::doc::annotation::STANDARD_STAMPS[0].0.to_owned(),
            font_size: DEFAULT_STAMP_FONT,
            image: None,
        }
    }
}

/// Where the sequence tool has got to. Per session, not per document: it is
/// reset from the page every time the tool is picked up, so numbering carries
/// on from what is already on the drawing rather than from what this window
/// happens to remember.
#[derive(Clone, Debug)]
pub struct SequenceSettings {
    pub prefix: String,
    pub next: u32,
}

impl Default for SequenceSettings {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            next: 1,
        }
    }
}

/// The number a `<prefix><digits>` stamp is carrying, if it is carrying one.
fn sequence_number(text: &str, prefix: &str) -> Option<u32> {
    let rest = text.trim().strip_prefix(prefix)?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// The number the sequence tool should place next, given what is already on
/// the document under this prefix.
pub fn next_sequence_number(store: &crate::doc::store::AnnotationStore, prefix: &str) -> u32 {
    store
        .stamp_texts()
        .filter_map(|text| sequence_number(text, prefix))
        .max()
        .map_or(1, |n| n.saturating_add(1))
}

/// Switch tools, tidying up after the one being left.
///
/// Half-placed vertices belong to the tool that was placing them, and the
/// sequence counter has to pick up where the document leaves off rather than
/// where this window last got to -- both of which are easy to forget at a call
/// site, so neither is left to one.
pub fn set_tool(dc: &mut DocState, tool: ActiveTool) {
    if dc.tool != tool {
        cancel(dc);
    }
    if tool == ActiveTool::Sequence {
        dc.tool_ctl.sequence.next = next_sequence_number(&dc.store, &dc.tool_ctl.sequence.prefix);
    }
    dc.tool = tool;
}

/// The style a stamp is placed in.
fn stamp_style(opacity: f32) -> Style {
    Style {
        stroke: STAMP_RED,
        stroke_width: 1.5,
        fill: Color::TRANSPARENT,
        opacity,
    }
}

/// The longest side an image stamp is placed at, in points -- half a US Letter
/// page, so a picture at any resolution arrives at a size somebody can see and
/// then drag to what they meant.
pub const MAX_STAMP_SIDE: f32 = 300.0;

/// A PNG's dimensions in pixels, taken as points, without decoding the pixels.
fn png_size(png: &[u8]) -> Option<(f32, f32)> {
    let (w, h) = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()
        .map(|img| (img.width(), img.height()))?;
    (w > 0 && h > 0).then_some((w as f32, h as f32))
}

/// Put a stamp on the page, selected and undoable.
fn place_stamp(dc: &mut DocState, page: usize, rect: PdfRect, kind: AnnotationKind) {
    let id = dc.store.alloc_id();
    let ann = Annotation {
        id,
        page,
        kind,
        rect,
        style: stamp_style(dc.current_style.opacity),
        group: None,
    };
    dc.selection.select_one(id);
    dc.history
        .apply(Command::AddAnnotation(ann), &mut dc.store, &mut dc.pages);
}

/// The box a word of this size needs, centred on where it was clicked.
fn stamp_rect(at: PdfPoint, text: &str, font_size: f32, square: bool) -> PdfRect {
    let natural: f32 = text
        .chars()
        .map(|c| crate::export::pdf::char_width(c, font_size))
        .sum();
    let height = font_size * 1.9;
    let width = if square {
        (natural + font_size * 0.9).max(height)
    } else {
        natural + font_size * 1.4
    };
    PdfRect::from_min_size(
        PdfPoint::new(at.x - width / 2.0, at.y - height / 2.0),
        width,
        height,
    )
}

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
    /// Moving the selection. `origs` is every selected annotation as it was
    /// when the drag began, the one actually grabbed first: it is the one the
    /// snap guides are computed for, and the rest follow it exactly.
    Dragging {
        origs: Vec<Annotation>,
        press: PdfPoint,
        moved: bool,
    },
    /// Rubber-banding a box over empty space to select what it touches.
    Marquee {
        page: usize,
        start: PdfPoint,
        current: PdfPoint,
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
    /// What the stamp tool will put down next.
    pub stamp: StampSettings,
    /// Where the sequence tool has counted to.
    pub sequence: SequenceSettings,
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
            stamp: StampSettings::default(),
            sequence: SequenceSettings::default(),
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
            | ToolState::Placing { page, .. }
            | ToolState::Marquee { page, .. } => Some(*page),
            ToolState::Dragging { origs, .. } => origs.first().map(|a| a.page),
            ToolState::Resizing { orig, .. } => Some(orig.page),
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
            // Handles take priority, and only when one thing is selected: with
            // four selected, a corner of one of them is not a resize gesture,
            // it is a place somebody is about to drag all four from.
            if dc.selection.len() == 1
                && let Some(ann) = dc.selected_annotation().cloned()
                && ann.page == p.page
                && let Some(handle) = select::handle_at(&ann, p.pos, p.tol * 1.5)
            {
                dc.tool_ctl.state = ToolState::Resizing { orig: ann, handle };
                return;
            }
            let Some(id) = select::hit_test(&dc.store, p.page, p.pos, p.tol) else {
                // Empty space: shift keeps what is selected (the drag is about
                // to add to it), a plain press starts again from nothing.
                if !p.modifiers.shift {
                    dc.selection.clear();
                }
                dc.tool_ctl.state = ToolState::Marquee {
                    page: p.page,
                    start: p.pos,
                    current: p.pos,
                };
                return;
            };
            // A group is one thing to everybody except the code that stores it.
            let hit = group_of(dc, id);
            if p.modifiers.shift {
                for id in &hit {
                    dc.selection.toggle(*id);
                }
            } else if !dc.selection.contains(id) {
                dc.selection.select_all(hit);
            }
            let origs = dc.selected_annotations();
            if origs.is_empty() {
                return;
            }
            // The grabbed one leads: it is what the snap guides are drawn for.
            let mut origs = origs;
            if let Some(at) = origs.iter().position(|a| a.id == id) {
                origs.swap(0, at);
            }
            dc.tool_ctl.state = ToolState::Dragging {
                origs,
                press: p.pos,
                moved: false,
            };
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
                group: None,
            };
            dc.store.insert(ann);
            dc.selection.select_one(id);
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
        ActiveTool::Stamp => {
            let text = crate::doc::annotation::expand_stamp_tokens(
                &dc.tool_ctl.stamp.text.clone(),
                &dc.title(),
            );
            if text.trim().is_empty() {
                return;
            }
            let font_size = dc.tool_ctl.stamp.font_size;
            place_stamp(
                dc,
                p.page,
                stamp_rect(p.pos, &text, font_size, false),
                AnnotationKind::Stamp { text, font_size },
            );
        }
        ActiveTool::Sequence => {
            let text = format!(
                "{}{}",
                dc.tool_ctl.sequence.prefix, dc.tool_ctl.sequence.next
            );
            let font_size = DEFAULT_SEQUENCE_FONT;
            place_stamp(
                dc,
                p.page,
                stamp_rect(p.pos, &text, font_size, true),
                AnnotationKind::Stamp { text, font_size },
            );
            dc.tool_ctl.sequence.next = dc.tool_ctl.sequence.next.saturating_add(1);
        }
        ActiveTool::ImageStamp => {
            let Some(png) = dc.tool_ctl.stamp.image.clone() else {
                return;
            };
            let Some((w, h)) = png_size(&png) else {
                return;
            };
            // Placed at its own size in points, shrunk to something that fits
            // on a page if the picture is a photograph's worth of pixels.
            let scale = (MAX_STAMP_SIDE / w.max(h)).min(1.0);
            let (w, h) = (w * scale, h * scale);
            let rect =
                PdfRect::from_min_size(PdfPoint::new(p.pos.x - w / 2.0, p.pos.y - h / 2.0), w, h);
            place_stamp(dc, p.page, rect, AnnotationKind::ImageStamp { png });
        }
        tool if tool.places_vertices() => {
            dc.selection.clear();
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
            dc.selection.clear();
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
        ToolState::Marquee { page, start, .. } => {
            dc.tool_ctl.state = ToolState::Marquee {
                page,
                start,
                current: p.pos,
            };
        }
        ToolState::Dragging { origs, press, .. } => {
            let Some(lead) = origs.first().cloned() else {
                return;
            };
            let dx = p.pos.x - press.x;
            let dy = p.pos.y - press.y;
            let mut rect = lead.rect.translated(dx, dy);
            if snapping {
                // Nothing being dragged is something to snap against.
                let moving: Vec<AnnotationId> = origs.iter().map(|a| a.id).collect();
                let others: Vec<PdfRect> = dc
                    .store
                    .on_page(lead.page)
                    .filter(|a| !moving.contains(&a.id))
                    .map(|a| a.bounds())
                    .collect();
                let result = snap_rect(rect, SnapFeatures::ALL, p.page_w, p.page_h, &others, p.tol);
                rect = rect.translated(result.correction.dx, result.correction.dy);
                dc.tool_ctl.set_guides(lead.page, result.guides);
            } else {
                dc.tool_ctl.clear_guides();
            }
            // Whatever correction the lead took, everything else takes too:
            // a selection that changed shape while it moved would be a bug the
            // user could see.
            let (dx, dy) = (rect.min.x - lead.rect.min.x, rect.min.y - lead.rect.min.y);
            for orig in &origs {
                let mut ann = orig.clone();
                ann.translate(dx, dy);
                dc.store.replace(ann);
            }
            dc.tool_ctl.state = ToolState::Dragging {
                origs,
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

            // A picture holds its proportions unless shift says otherwise;
            // every other shape is the other way round.
            let lock_aspect = p.modifiers.shift != orig.kind.keeps_aspect();
            let mut rect = select::resize_rect(orig.rect, handle, pos, lock_aspect);
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
                    rect = select::resize_rect(orig.rect, handle, snapped, lock_aspect);
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
                group: None,
            };
            dc.selection.select_one(id);
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
                group: None,
            };
            dc.selection.select_one(id);
            dc.history
                .apply(Command::AddAnnotation(ann), &mut dc.store, &mut dc.pages);
        }
        ToolState::Marquee {
            page,
            start,
            current,
        } => {
            let rect = PdfRect::from_points(start, current);
            // A press and release in the same place is a click on empty space,
            // which `on_press` has already read as "select nothing".
            if rect.width() < 1.0 && rect.height() < 1.0 {
                return;
            }
            let touched: Vec<AnnotationId> = dc
                .store
                .on_page(page)
                .filter(|a| a.bounds().intersects(rect))
                .flat_map(|a| group_of(dc, a.id))
                .collect();
            if p.modifiers.shift {
                dc.selection.add_all(touched);
            } else {
                dc.selection.select_all(touched);
            }
        }
        ToolState::Dragging { origs, moved, .. } => {
            if !moved {
                // A press on something already selected keeps the whole
                // selection, so that a drag moves all of it. Letting go without
                // having moved says the press was a click after all, and a
                // click picks out the one thing under it.
                if !p.modifiers.shift
                    && dc.selection.len() > 1
                    && let Some(lead) = origs.first()
                {
                    let hit = group_of(dc, lead.id);
                    dc.selection.select_all(hit);
                }
                return;
            }
            // One entry however many moved: a drag is one thing the user did,
            // and one press of ⌘Z has to put all of it back.
            let changes: Vec<Command> = origs
                .into_iter()
                .filter_map(|before| {
                    let after = dc.store.get(before.id).cloned()?;
                    (after != before).then_some(Command::ModifyAnnotation { before, after })
                })
                .collect();
            if let Some(cmd) = one_step(changes) {
                dc.history.record(cmd);
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
        group: None,
    };
    dc.selection.select_one(id);
    dc.history
        .apply(Command::AddAnnotation(ann), &mut dc.store, &mut dc.pages);
    true
}

/// Cancel any in-flight gesture (Esc).
pub fn cancel(dc: &mut DocState) {
    let state = std::mem::replace(&mut dc.tool_ctl.state, ToolState::Idle);
    dc.tool_ctl.clear_guides();
    match state {
        ToolState::Dragging { origs, .. } => {
            for orig in origs {
                dc.store.replace(orig);
            }
        }
        ToolState::Resizing { orig, .. } => {
            dc.store.replace(orig);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Selection, groups, and the things done to whole selections
// ---------------------------------------------------------------------------

/// Several commands as one undo step, or none at all if nothing changed.
///
/// A single change stays a single command rather than a batch of one: it is
/// the same undo either way, and the history reads better for it.
fn one_step(mut commands: Vec<Command>) -> Option<Command> {
    match commands.len() {
        0 => None,
        1 => commands.pop(),
        _ => Some(Command::Batch(commands)),
    }
}

/// Everything that would be selected by clicking `id`: the whole group if it
/// is in one, and otherwise just itself.
fn group_of(dc: &DocState, id: AnnotationId) -> Vec<AnnotationId> {
    match dc.store.get(id).and_then(|a| a.group) {
        Some(group) => dc.store.group_members(group).collect(),
        None => vec![id],
    }
}

/// Delete everything selected, as one undo step.
pub fn delete_selection(dc: &mut DocState) {
    let removed: Vec<Command> = dc
        .selection
        .iter()
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|id| dc.store.remove(id).map(Command::RemoveAnnotation))
        .collect();
    if let Some(cmd) = one_step(removed) {
        dc.history.record(cmd);
        dc.selection.clear();
    }
}

/// Move everything selected by (dx, dy) points, as one undo step. This is the
/// arrow keys: one press is one nudge, however much is selected.
pub fn nudge_selection(dc: &mut DocState, dx: f32, dy: f32) {
    let changes: Vec<Command> = dc
        .selected_annotations()
        .into_iter()
        .map(|before| {
            let mut after = before.clone();
            after.translate(dx, dy);
            dc.store.replace(after.clone());
            Command::ModifyAnnotation { before, after }
        })
        .collect();
    if let Some(cmd) = one_step(changes) {
        dc.history.record(cmd);
    }
}

/// Tie the selection together, so that from now on clicking one of them
/// selects all of them. Nothing to do with fewer than two.
pub fn group_selection(dc: &mut DocState) -> bool {
    let members = dc.selected_annotations();
    if members.len() < 2 {
        return false;
    }
    let group = dc.store.next_group_id();
    apply_group(dc, members, Some(group))
}

/// Untie whatever groups the selection belongs to.
pub fn ungroup_selection(dc: &mut DocState) -> bool {
    let members = dc.selected_annotations();
    apply_group(dc, members, None)
}

fn apply_group(dc: &mut DocState, members: Vec<Annotation>, group: Option<GroupId>) -> bool {
    let changes: Vec<Command> = members
        .into_iter()
        .filter(|before| before.group != group)
        .map(|before| {
            let mut after = before.clone();
            after.group = group;
            dc.store.replace(after.clone());
            Command::ModifyAnnotation { before, after }
        })
        .collect();
    match one_step(changes) {
        Some(cmd) => {
            dc.history.record(cmd);
            true
        }
        None => false,
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

    /// Three rectangles in a row on page 0, at x = 100, 200 and 300.
    fn three_rects(dc: &mut DocState) -> Vec<AnnotationId> {
        (0..3)
            .map(|i| {
                let id = dc.store.alloc_id();
                let x = 100.0 + 100.0 * i as f32;
                dc.store.insert(Annotation {
                    id,
                    page: 0,
                    kind: AnnotationKind::Rect,
                    rect: PdfRect::from_min_size(PdfPoint::new(x, 400.0), 60.0, 40.0),
                    style: Style::default(),
                    group: None,
                });
                id
            })
            .collect()
    }

    fn shift_at(x: f32, y: f32) -> PointerInfo {
        PointerInfo {
            modifiers: Modifiers::SHIFT,
            ..at(x, y)
        }
    }

    fn drag(dc: &mut DocState, from: (f32, f32), to: (f32, f32)) {
        on_press(dc, &at(from.0, from.1));
        on_drag(dc, &at(to.0, to.1));
        on_release(dc, &at(to.0, to.1));
    }

    /// A box dragged over empty space takes everything it touches, and
    /// shift-clicking adds and removes one at a time.
    #[test]
    fn a_marquee_takes_what_it_touches_and_shift_adds_one_at_a_time() {
        let mut dc = doc_state();
        let ids = three_rects(&mut dc);
        dc.tool = ActiveTool::Select;

        // A box across the first two, started well clear of either.
        drag(&mut dc, (80.0, 380.0), (280.0, 460.0));
        assert_eq!(dc.selection.len(), 2, "{:?}", dc.selection);
        assert!(dc.selection.contains(ids[0]) && dc.selection.contains(ids[1]));

        // Shift-clicking the third adds it; again takes it away.
        on_press(&mut dc, &shift_at(330.0, 420.0));
        on_release(&mut dc, &shift_at(330.0, 420.0));
        assert_eq!(dc.selection.len(), 3);
        assert_eq!(dc.selection.primary(), Some(ids[2]), "the last one clicked");
        on_press(&mut dc, &shift_at(330.0, 420.0));
        on_release(&mut dc, &shift_at(330.0, 420.0));
        assert_eq!(dc.selection.len(), 2);
        assert!(!dc.selection.contains(ids[2]));

        // A plain click on empty space starts again from nothing.
        click(&mut dc, 500.0, 200.0);
        assert!(dc.selection.is_empty());

        // And a marquee that touches nothing selects nothing, rather than
        // leaving what was there.
        dc.selection.select_one(ids[0]);
        drag(&mut dc, (500.0, 200.0), (560.0, 260.0));
        assert!(dc.selection.is_empty());
    }

    /// Several moved at once is one thing the user did, so it is one press of
    /// ⌘Z -- and everything moves by the same amount, whichever was grabbed.
    #[test]
    fn moving_several_at_once_moves_them_together_and_undoes_as_one_step() {
        let mut dc = doc_state();
        let ids = three_rects(&mut dc);
        dc.tool = ActiveTool::Select;
        let before: Vec<Annotation> = ids
            .iter()
            .map(|id| dc.store.get(*id).unwrap().clone())
            .collect();
        dc.selection.select_all(ids.clone());

        // Grab the middle one and drag; ⌘ held so nothing snaps on the way.
        let held = PointerInfo {
            modifiers: Modifiers::COMMAND,
            ..at(230.0, 420.0)
        };
        on_press(&mut dc, &held);
        let to = PointerInfo {
            modifiers: Modifiers::COMMAND,
            ..at(250.0, 450.0)
        };
        on_drag(&mut dc, &to);
        on_release(&mut dc, &to);

        for (id, was) in ids.iter().zip(&before) {
            let now = dc.store.get(*id).expect("still there");
            assert!(
                (now.rect.min.x - was.rect.min.x - 20.0).abs() < 0.01,
                "{now:?}"
            );
            assert!(
                (now.rect.min.y - was.rect.min.y - 30.0).abs() < 0.01,
                "{now:?}"
            );
        }

        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        for (id, was) in ids.iter().zip(&before) {
            assert_eq!(dc.store.get(*id), Some(was), "one undo put all three back");
        }
        assert!(!dc.history.can_undo(), "and there was only the one step");
        assert!(dc.history.redo(&mut dc.store, &mut dc.pages));
        assert!(dc.store.get(ids[0]).unwrap().rect.min.x > 100.0, "and back");
    }

    /// The arrow keys nudge everything selected, and one press is one step
    /// back -- the same promise a drag makes.
    #[test]
    fn nudging_moves_the_whole_selection_in_one_step() {
        let mut dc = doc_state();
        let ids = three_rects(&mut dc);
        dc.selection.select_all([ids[0], ids[2]]);
        nudge_selection(&mut dc, 1.0, -1.0);

        assert!((dc.store.get(ids[0]).unwrap().rect.min.x - 101.0).abs() < 0.01);
        assert!((dc.store.get(ids[2]).unwrap().rect.min.y - 399.0).abs() < 0.01);
        assert!(
            (dc.store.get(ids[1]).unwrap().rect.min.x - 200.0).abs() < 0.01,
            "the one that was not selected stayed put"
        );

        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        assert!((dc.store.get(ids[0]).unwrap().rect.min.x - 100.0).abs() < 0.01);
        assert!((dc.store.get(ids[2]).unwrap().rect.min.y - 400.0).abs() < 0.01);
        assert!(!dc.history.can_undo(), "one press, one step");
    }

    /// Deleting a selection is one step too.
    #[test]
    fn deleting_several_at_once_undoes_as_one_step() {
        let mut dc = doc_state();
        let ids = three_rects(&mut dc);
        dc.selection.select_all(ids.clone());
        delete_selection(&mut dc);
        assert_eq!(dc.store.on_page(0).count(), 0);
        assert!(dc.selection.is_empty());

        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        assert_eq!(dc.store.on_page(0).count(), 3, "all three came back");
        assert!(!dc.history.can_undo());

        // Nothing selected is nothing to delete, and nothing to undo either.
        delete_selection(&mut dc);
        assert!(!dc.history.can_undo());
    }

    /// A group is a selection somebody made once: clicking any member selects
    /// all of them, and what is done to them is done to all of them.
    #[test]
    fn a_group_is_selected_moved_and_deleted_as_one() {
        let mut dc = doc_state();
        let ids = three_rects(&mut dc);
        dc.tool = ActiveTool::Select;

        // Two of the three, tied together.
        dc.selection.select_all([ids[0], ids[1]]);
        assert!(group_selection(&mut dc));
        let group = dc.store.get(ids[0]).unwrap().group.expect("a group");
        assert_eq!(dc.store.get(ids[1]).unwrap().group, Some(group));
        assert_eq!(dc.store.get(ids[2]).unwrap().group, None);
        assert!(dc.history.can_undo(), "grouping is undoable");

        // Clicking either of them selects both; the loose one selects itself.
        click(&mut dc, 130.0, 420.0);
        assert_eq!(dc.selection.len(), 2);
        assert!(dc.selection.contains(ids[1]), "its partner came with it");
        click(&mut dc, 330.0, 420.0);
        assert_eq!(dc.selection.len(), 1);

        // A group moves as one, in one undo step.
        let before = dc.store.get(ids[1]).unwrap().clone();
        click(&mut dc, 230.0, 420.0);
        let held = PointerInfo {
            modifiers: Modifiers::COMMAND,
            ..at(230.0, 420.0)
        };
        on_press(&mut dc, &held);
        let to = PointerInfo {
            modifiers: Modifiers::COMMAND,
            ..at(230.0, 470.0)
        };
        on_drag(&mut dc, &to);
        on_release(&mut dc, &to);
        assert!((dc.store.get(ids[0]).unwrap().rect.min.y - 450.0).abs() < 0.01);
        assert!((dc.store.get(ids[1]).unwrap().rect.min.y - 450.0).abs() < 0.01);
        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        assert_eq!(
            dc.store.get(ids[1]),
            Some(&before),
            "both went back at once"
        );

        // Deleting one of them deletes the group, and one undo restores it.
        click(&mut dc, 130.0, 420.0);
        delete_selection(&mut dc);
        assert_eq!(dc.store.on_page(0).count(), 1, "only the loose one is left");
        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        assert_eq!(dc.store.on_page(0).count(), 3);

        // Untying them puts them back to being clicked one at a time.
        dc.selection.select_all([ids[0], ids[1]]);
        assert!(ungroup_selection(&mut dc));
        assert_eq!(dc.store.get(ids[0]).unwrap().group, None);
        click(&mut dc, 130.0, 420.0);
        assert_eq!(dc.selection.len(), 1);
        assert_eq!(dc.selection.primary(), Some(ids[0]));

        // And undoing the ungroup ties them again -- one step for both.
        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        assert_eq!(dc.store.get(ids[0]).unwrap().group, Some(group));
        assert_eq!(dc.store.get(ids[1]).unwrap().group, Some(group));
    }

    /// One thing on its own is not a group, and a fresh group id has to be one
    /// nothing else is already using.
    #[test]
    fn grouping_needs_two_and_never_reuses_an_id() {
        let mut dc = doc_state();
        let ids = three_rects(&mut dc);
        dc.selection.select_one(ids[0]);
        assert!(!group_selection(&mut dc), "one thing is not a group");
        assert!(!dc.history.can_undo());

        dc.selection.select_all([ids[0], ids[1]]);
        group_selection(&mut dc);
        let first = dc.store.get(ids[0]).unwrap().group.expect("a group");
        dc.selection.select_one(ids[2]);
        let another = dc.store.alloc_id();
        dc.store.insert(Annotation {
            id: another,
            page: 0,
            kind: AnnotationKind::Rect,
            rect: PdfRect::from_min_size(PdfPoint::new(400.0, 400.0), 10.0, 10.0),
            style: Style::default(),
            group: None,
        });
        dc.selection.select_all([ids[2], another]);
        group_selection(&mut dc);
        let second = dc.store.get(ids[2]).unwrap().group.expect("a group");
        assert_ne!(first, second);

        // Ungrouping something that is in no group changes nothing.
        dc.selection.select_one(ids[1]);
        dc.selection.clear();
        assert!(!ungroup_selection(&mut dc));
    }

    /// A grouped shape still has to survive the sidecar, and the next group
    /// made after it comes back has to clear the ids that came with it.
    #[test]
    fn a_group_survives_the_sidecar_and_the_next_id_clears_it() {
        let mut dc = doc_state();
        let ids = three_rects(&mut dc);
        dc.selection.select_all([ids[0], ids[1]]);
        group_selection(&mut dc);

        let json = serde_json::to_string(&dc.store.to_vec()).expect("serialize");
        assert!(json.contains("\"group\":1"), "{json}");
        // Ungrouped markup is the version-2 shape exactly as it was.
        assert_eq!(json.matches("\"group\"").count(), 2, "{json}");

        let restored: Vec<Annotation> = serde_json::from_str(&json).expect("deserialize");
        let store = crate::doc::store::AnnotationStore::restore(restored);
        assert_eq!(store.group_members(1).count(), 2);
        assert_eq!(store.next_group_id(), 2);
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
        assert_eq!(dc.selection.primary(), Some(made.id));

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

    /// A stamp is placed with one click, says what the tool was told to say,
    /// and is one undo step -- and the tokens in it are spent at that moment,
    /// so what is stored is words rather than instructions.
    #[test]
    fn stamping_puts_down_one_stamp_with_its_tokens_already_spent() {
        let mut dc = doc_state();
        dc.tool_ctl.stamp.text = "APPROVED %date".into();
        set_tool(&mut dc, ActiveTool::Stamp);
        click(&mut dc, 200.0, 500.0);

        let made = dc.store.on_page(0).next().expect("a stamp").clone();
        let AnnotationKind::Stamp { text, font_size } = &made.kind else {
            panic!("got {:?}", made.kind);
        };
        assert!(text.starts_with("APPROVED 20"), "{text}");
        assert!(!text.contains('%'), "{text}");
        assert_eq!(*font_size, DEFAULT_STAMP_FONT);
        assert_eq!(made.style.stroke, STAMP_RED);
        // Centred on the click, and wide enough for the words.
        let centre = made.rect.center();
        assert!((centre.x - 200.0).abs() < 0.01, "{centre:?}");
        assert!((centre.y - 500.0).abs() < 0.01, "{centre:?}");
        assert!(made.rect.width() > made.rect.height(), "{:?}", made.rect);

        assert_eq!(dc.selection.primary(), Some(made.id));
        assert!(dc.history.undo(&mut dc.store, &mut dc.pages));
        assert!(dc.store.get(made.id).is_none(), "one click, one undo");

        // Nothing to say is nothing to stamp.
        dc.tool_ctl.stamp.text = "   ".into();
        click(&mut dc, 300.0, 500.0);
        assert!(dc.store.on_page(0).next().is_none());
    }

    /// The sequence tool counts up as it goes, and picks up from the document
    /// rather than from the window: the numbers on the page are the record.
    #[test]
    fn the_sequence_tool_counts_up_and_resumes_from_the_page() {
        let mut dc = doc_state();
        set_tool(&mut dc, ActiveTool::Sequence);
        for (i, y) in [700.0f32, 650.0, 600.0].iter().enumerate() {
            click(&mut dc, 100.0, *y);
            let placed = dc.store.on_page(0).last().expect("a number");
            let AnnotationKind::Stamp { text, .. } = &placed.kind else {
                panic!("got {:?}", placed.kind);
            };
            assert_eq!(text, &(i + 1).to_string());
            // A number is stamped in a box no narrower than it is tall.
            assert!(placed.rect.width() >= placed.rect.height(), "{placed:?}");
        }
        assert_eq!(dc.tool_ctl.sequence.next, 4);

        // Picking the tool up again resumes above what is on the page, not
        // above what this window last did.
        dc.tool_ctl.sequence.next = 99;
        set_tool(&mut dc, ActiveTool::Select);
        set_tool(&mut dc, ActiveTool::Sequence);
        assert_eq!(dc.tool_ctl.sequence.next, 4);

        // A prefix is its own count: "A1" is not a fourth "3".
        dc.tool_ctl.sequence.prefix = "A".into();
        set_tool(&mut dc, ActiveTool::Select);
        set_tool(&mut dc, ActiveTool::Sequence);
        assert_eq!(dc.tool_ctl.sequence.next, 1);
        click(&mut dc, 300.0, 700.0);
        let AnnotationKind::Stamp { text, .. } = &dc.store.on_page(0).last().unwrap().kind else {
            panic!()
        };
        assert_eq!(text, "A1");

        // And the numbers a prefix does not own are not counted under it.
        assert_eq!(next_sequence_number(&dc.store, "A"), 2);
        assert_eq!(next_sequence_number(&dc.store, ""), 4);
        assert_eq!(next_sequence_number(&dc.store, "B"), 1);
    }

    /// Deleting the middle of a sequence leaves the count where the highest
    /// number is: renumbering behind the user's back would be worse.
    #[test]
    fn the_sequence_resumes_above_the_highest_number_left() {
        let mut dc = doc_state();
        set_tool(&mut dc, ActiveTool::Sequence);
        click(&mut dc, 100.0, 700.0);
        click(&mut dc, 100.0, 650.0);
        click(&mut dc, 100.0, 600.0);
        let second = dc.store.on_page(0).nth(1).expect("the second").id;
        dc.store.remove(second);
        assert_eq!(next_sequence_number(&dc.store, ""), 4);
    }

    /// A picture stamp needs a picture: without one the click does nothing,
    /// with one it lands at the picture's own proportions.
    #[test]
    fn an_image_stamp_lands_only_once_a_picture_has_been_chosen() {
        let mut dc = doc_state();
        set_tool(&mut dc, ActiveTool::ImageStamp);
        click(&mut dc, 200.0, 400.0);
        assert!(dc.store.on_page(0).next().is_none(), "nothing to place yet");

        dc.tool_ctl.stamp.image = Some(crate::export::pdf::tests::png_fixture(40, 20));
        click(&mut dc, 200.0, 400.0);
        let made = dc.store.on_page(0).next().expect("a picture").clone();
        assert!(matches!(made.kind, AnnotationKind::ImageStamp { .. }));
        assert!((made.rect.width() - 40.0).abs() < 0.01, "{:?}", made.rect);
        assert!((made.rect.height() - 20.0).abs() < 0.01, "{:?}", made.rect);

        // Dragged by a corner it holds those proportions unless shift says not.
        dc.tool = ActiveTool::Select;
        let corner = PdfPoint::new(made.rect.max.x, made.rect.max.y);
        on_press(&mut dc, &at(corner.x, corner.y));
        on_drag(&mut dc, &at(corner.x + 40.0, corner.y + 5.0));
        on_release(&mut dc, &at(corner.x + 40.0, corner.y + 5.0));
        let resized = dc.store.get(made.id).expect("still there");
        let ratio = resized.rect.width() / resized.rect.height();
        assert!((ratio - 2.0).abs() < 0.05, "{:?}", resized.rect);
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
            group: None,
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
