//! The main page canvas: continuous vertical page layout with pan/zoom,
//! background-rendered page textures, markup painting, and pointer routing
//! into the tool state machine.

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Mesh, Modifiers, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, TextEdit, Vec2, epaint,
};

use crate::doc::annotation::{Annotation, AnnotationKind, Color, TextAlign};
use crate::doc::geometry::{ExtraRotation, PageTransform, PdfRect};
use crate::doc::history::Command;
use crate::render::{RenderRequest, scale_bucket};
use crate::state::DocState;
use crate::tools::{self, ActiveTool, PointerInfo, ToolState, snap::Guide};
use crate::ui::viewport::{Layout, PageSlot, Viewport};

pub use crate::render::THUMB_SCALE;

const SELECTION_COLOR: Color32 = Color32::from_rgb(0x2f, 0x7c, 0xf6);
const GUIDE_COLOR: Color32 = Color32::from_rgb(0x00, 0xb4, 0xd8);
/// Find-in-document highlights: every hit, then the one being stepped through.
const FIND_MATCH_COLOR: Color32 = Color32::from_rgba_unmultiplied_const(255, 235, 59, 110);
const FIND_ACTIVE_COLOR: Color32 = Color32::from_rgba_unmultiplied_const(255, 152, 0, 150);

pub fn color32(c: Color, opacity: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, (c.a as f32 * opacity).round() as u8)
}

/// Decoded pictures for the image stamps on a document, one texture each.
///
/// Keyed by annotation id rather than by the bytes: the same signature stamped
/// twice is two annotations, and either can be deleted on its own.
/// [`sync`](StampTextures::sync) is what makes the deletion free -- it drops
/// every texture whose annotation is no longer in the store, which covers Undo
/// and page deletion as well as the Delete key without any of them knowing this
/// cache exists. A PNG that does not decode is remembered as `None` so it is
/// not re-decoded once a frame forever.
#[derive(Default)]
pub struct StampTextures {
    map: std::collections::HashMap<
        crate::doc::annotation::AnnotationId,
        Option<egui::TextureHandle>,
    >,
}

impl StampTextures {
    /// Decode what is new, forget what is gone. Called once a frame.
    pub fn sync(&mut self, ctx: &egui::Context, store: &crate::doc::store::AnnotationStore) {
        self.map.retain(|id, _| store.get(*id).is_some());
        for (id, png) in store.image_stamps() {
            if self.map.contains_key(&id) {
                continue;
            }
            let tex = decode_png(png).map(|(size, rgba)| {
                ctx.load_texture(
                    format!("stamp-{id}"),
                    egui::ColorImage::from_rgba_unmultiplied(size, &rgba),
                    egui::TextureOptions::LINEAR,
                )
            });
            self.map.insert(id, tex);
        }
    }

    fn get(&self, id: crate::doc::annotation::AnnotationId) -> Option<&egui::TextureHandle> {
        self.map.get(&id)?.as_ref()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// A PNG as ([width, height], straight-alpha RGBA), or `None` if it is not one.
fn decode_png(png: &[u8]) -> Option<([usize; 2], Vec<u8>)> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (w, h) = img.dimensions();
    Some(([w as usize, h as usize], img.into_raw()))
}

/// Route finished renders from the worker into the texture caches, and start
/// the caches' frame. Called once per frame, before anything paints.
pub fn poll_worker(dc: &mut DocState, ctx: &egui::Context) {
    dc.cache.begin_frame();
    dc.thumb_cache.begin_frame();
    dc.stamp_images.sync(ctx, &dc.store);
    while let Some(res) = dc.worker.try_recv() {
        // The first answer of any kind says which rasterizer opened the
        // document, which is what the status bar reports.
        dc.engine = Some(res.engine);
        let cache = if (res.scale - THUMB_SCALE).abs() < 1e-3 {
            &mut dc.thumb_cache
        } else {
            &mut dc.cache
        };
        match res.image {
            Some(image) => cache.insert(ctx, res.page, res.scale, image),
            // Superseded before it ran: stop waiting on it, so a page that is
            // still on screen gets asked for again next frame.
            None => cache.clear_pending(res.page, res.scale),
        }
    }
}

pub fn show(ui: &mut egui::Ui, dc: &mut DocState) {
    let viewport_rect = ui.max_rect();

    if dc.viewport.fit_width {
        // Fit to the width the scroll area will actually have. Using the full
        // rect makes the widest page overflow by exactly the scrollbar's width,
        // which raises a horizontal scrollbar, which shrinks the area, which
        // re-derives the zoom — a feedback loop that flaps the render scale
        // across a bucket boundary. (Floating scrollbars allocate 0, so this is
        // a no-op for them.)
        let avail = viewport_rect.width() - ui.spacing().scroll.allocated_width();
        dc.viewport.zoom = Viewport::fit_width_zoom(&dc.doc, &dc.pages, avail);
    }

    // Pinch / ctrl-scroll zoom, anchored under the pointer.
    let (zoom_delta, pointer) = ui.input(|i| (i.zoom_delta(), i.pointer.hover_pos()));
    if (zoom_delta - 1.0).abs() > 1e-4
        && let Some(pointer) = pointer
        && viewport_rect.contains(pointer)
    {
        let layout = dc
            .viewport
            .layout(&dc.doc, &dc.pages, viewport_rect.width());
        let anchor = pointer - viewport_rect.min;
        let new_zoom = dc.viewport.zoom * zoom_delta;
        dc.viewport.zoom_about(
            &dc.doc,
            &dc.pages,
            &layout,
            anchor,
            new_zoom,
            viewport_rect.width(),
        );
    }

    // Drag-to-scroll defaults to `OnTouch`, and where it does kick in it fights
    // the Pan tool: both write the scroll offset, and on a direction reversal
    // the two disagree and the view snaps back. Dragging inside the canvas is
    // ours to route (tools or pan); the wheel and the scrollbars are untouched.
    let mut scroll_area = egui::ScrollArea::both()
        .auto_shrink(false)
        .scroll_source(egui::containers::scroll_area::ScrollSource {
            drag: egui::containers::scroll_area::DragScroll::Never,
            ..Default::default()
        })
        .id_salt("page-canvas");
    if let Some(offset) = dc.viewport.pending_offset.take() {
        scroll_area = scroll_area.scroll_offset(offset);
        // Mirror it now: `offset` is otherwise only refreshed from the scroll
        // area's output at the end of the frame, so a pan started this frame
        // would compute its delta against a stale base.
        dc.viewport.offset = offset;
    }

    let output = scroll_area.show(ui, |ui| {
        let layout = dc
            .viewport
            .layout(&dc.doc, &dc.pages, viewport_rect.width());
        let (content_rect, response) =
            ui.allocate_exact_size(layout.content_size, Sense::click_and_drag());

        // Scroll a specific page into view if requested (thumbnail click).
        if let Some(position) = dc.viewport.scroll_to_page.take()
            && let Some(slot) = layout.slots.get(position)
        {
            let target =
                Rect::from_min_size(content_rect.min + slot.rect.min.to_vec2(), slot.rect.size());
            ui.scroll_to_rect(target, Some(egui::Align::TOP));
        }

        // Center a find match (⌘F next/previous).
        if let Some((position, rect)) = dc.viewport.scroll_to_rect.take()
            && let Some(slot) = layout.slots.get(position)
        {
            let t = transform_for(dc, slot, content_rect);
            ui.scroll_to_rect(
                t.rect_to_screen(rect).expand(40.0),
                Some(egui::Align::Center),
            );
        }

        paint_and_interact(ui, dc, &layout, content_rect, &response);
    });
    dc.viewport.offset = output.state.offset;
}

fn transform_for(dc: &DocState, slot: &PageSlot, content_rect: Rect) -> PageTransform {
    let info = &dc.doc.pages[dc.pages.source_of(slot.original)];
    PageTransform {
        screen_rect: Rect::from_min_size(
            content_rect.min + slot.rect.min.to_vec2(),
            slot.rect.size(),
        ),
        page_w: info.width,
        page_h: info.height,
        rotation: dc.pages.rotation_of(slot.original),
        zoom: dc.viewport.zoom,
    }
}

fn paint_and_interact(
    ui: &mut egui::Ui,
    dc: &mut DocState,
    layout: &Layout,
    content_rect: Rect,
    response: &egui::Response,
) {
    let ppp = ui.ctx().pixels_per_point();
    let render_scale = scale_bucket(dc.viewport.zoom * ppp);
    let clip = ui.clip_rect();
    let painter = ui.painter_at(clip);

    // ---- pages ----
    for slot in &layout.slots {
        let t = transform_for(dc, slot, content_rect);
        if !t.screen_rect.intersects(clip) {
            continue;
        }

        painter.rect_filled(
            t.screen_rect.translate(Vec2::new(2.0, 3.0)),
            CornerRadius::ZERO,
            Color32::from_black_alpha(40),
        );
        painter.rect_filled(t.screen_rect, CornerRadius::ZERO, Color32::WHITE);

        let source = dc.pages.source_of(slot.original);
        let exact = dc.cache.get(source, render_scale);
        // Fall back to another canvas scale before the 0.22 rail thumbnail:
        // scaling a sharp texture reads as "not re-rendered yet", while
        // dropping to the thumbnail is a visible blur flash every time zoom
        // crosses a bucket boundary.
        let tex = exact
            .clone()
            .or_else(|| dc.cache.best_effort(source).map(|(_, t)| t))
            .or_else(|| dc.thumb_cache.get(source, THUMB_SCALE));
        if exact.is_none() && !dc.cache.is_pending(source, render_scale) {
            dc.cache.mark_pending(source, render_scale);
            dc.worker.request(RenderRequest {
                page: source,
                scale: render_scale,
            });
        }
        if let Some(tex) = tex {
            paint_rotated_texture(
                &painter,
                &tex,
                t.screen_rect,
                dc.pages.rotation_of(slot.original),
            );
        }
        painter.rect_stroke(
            t.screen_rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(190)),
            StrokeKind::Outside,
        );

        // Find matches, under the markup.
        let page_painter = ui.painter_at(t.screen_rect.intersect(clip).expand(2.0));
        if dc.find.open {
            for (i, m) in dc.find.matches.iter().enumerate() {
                if m.source_page != source {
                    continue;
                }
                let color = if i == dc.find.active {
                    FIND_ACTIVE_COLOR
                } else {
                    FIND_MATCH_COLOR
                };
                page_painter.rect_filled(
                    t.rect_to_screen(m.rect).expand(1.0),
                    CornerRadius::same(2),
                    color,
                );
            }
        }

        // Markup on this page.
        for ann in dc.store.on_page(slot.original) {
            paint_annotation(&page_painter, &t, ann, &dc.stamp_images);
        }
    }

    // ---- live creation preview ----
    if let ToolState::Creating {
        page,
        start,
        current,
    } = &dc.tool_ctl.state
        && let Some(slot) = layout.slots.iter().find(|s| s.original == *page)
    {
        let t = transform_for(dc, slot, content_rect);
        if let Some(preview) = creation_preview(dc.tool, *start, *current, dc) {
            paint_annotation(&painter, &t, &preview, &dc.stamp_images);
        }
    }
    // Vertices going down: the outline so far, plus a segment following the
    // pointer to where the next click would put a corner.
    if let ToolState::Placing {
        page,
        points,
        hover,
    } = &dc.tool_ctl.state
        && let Some(slot) = layout.slots.iter().find(|s| s.original == *page)
    {
        let t = transform_for(dc, slot, content_rect);
        let stroke = Stroke::new(
            dc.current_style.stroke_width * t.zoom,
            color32(dc.current_style.stroke, dc.current_style.opacity),
        );
        let mut screen: Vec<Pos2> = points.iter().map(|p| t.to_screen(*p)).collect();
        screen.push(t.to_screen(*hover));
        if dc.tool == ActiveTool::Polygon && screen.len() >= 3 {
            painter.add(Shape::closed_line(screen.clone(), stroke));
        } else if screen.len() >= 2 {
            painter.add(Shape::line(screen.clone(), stroke));
        }
        for p in points {
            paint_handle(&painter, t.to_screen(*p), 6.0);
        }
    }
    if let ToolState::Drawing { page, points } = &dc.tool_ctl.state
        && let Some(slot) = layout.slots.iter().find(|s| s.original == *page)
        && points.len() >= 2
    {
        let t = transform_for(dc, slot, content_rect);
        let screen: Vec<Pos2> = points.iter().map(|p| t.to_screen(*p)).collect();
        painter.add(Shape::line(
            screen,
            Stroke::new(
                dc.current_style.stroke_width * t.zoom,
                color32(dc.current_style.stroke, dc.current_style.opacity),
            ),
        ));
    }

    // ---- selection handles ----
    if dc.editing_text.is_none()
        && let Some(ann) = dc.selected_annotation().cloned()
        && let Some(slot) = layout.slots.iter().find(|s| s.original == ann.page)
    {
        let t = transform_for(dc, slot, content_rect);
        paint_selection(&painter, &t, &ann);
    }

    // ---- snap guides ----
    if let Some(page) = dc.tool_ctl.guides_page
        && let Some(slot) = layout.slots.iter().find(|s| s.original == page)
    {
        let t = transform_for(dc, slot, content_rect);
        for guide in &dc.tool_ctl.guides {
            let (a, b) = match guide {
                Guide::Vertical(x) => (
                    t.to_screen(crate::doc::geometry::PdfPoint::new(*x, 0.0)),
                    t.to_screen(crate::doc::geometry::PdfPoint::new(*x, t.page_h)),
                ),
                Guide::Horizontal(y) => (
                    t.to_screen(crate::doc::geometry::PdfPoint::new(0.0, *y)),
                    t.to_screen(crate::doc::geometry::PdfPoint::new(t.page_w, *y)),
                ),
            };
            painter.add(Shape::dashed_line(
                &[a, b],
                Stroke::new(1.0, GUIDE_COLOR),
                6.0,
                4.0,
            ));
        }
    }

    // ---- text editing overlay ----
    text_edit_overlay(ui, dc, layout, content_rect);

    // ---- pointer routing ----
    route_pointer(ui, dc, layout, content_rect, response);

    // ---- cursor ----
    match dc.tool {
        ActiveTool::Pan => {
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        ActiveTool::Select => {}
        _ => {
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
        }
    }
}

fn creation_preview(
    tool: ActiveTool,
    start: crate::doc::geometry::PdfPoint,
    current: crate::doc::geometry::PdfPoint,
    dc: &DocState,
) -> Option<Annotation> {
    use crate::doc::annotation::Style;
    let rect = PdfRect::from_points(start, current);
    let (kind, style) = match tool {
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
                points: crate::tools::rect_points(rect),
                cloudy: Some(crate::tools::DEFAULT_CLOUD_INTENSITY),
            },
            dc.current_style,
        ),
        ActiveTool::Line | ActiveTool::Arrow => (
            AnnotationKind::Line {
                p1: start,
                p2: current,
                arrow_end: tool == ActiveTool::Arrow,
            },
            dc.current_style,
        ),
        _ => return None,
    };
    Some(Annotation {
        id: 0,
        page: 0,
        kind,
        rect,
        style,
    })
}

pub fn paint_annotation(
    painter: &egui::Painter,
    t: &PageTransform,
    ann: &Annotation,
    images: &StampTextures,
) {
    let stroke = Stroke::new(
        ann.style.stroke_width * t.zoom,
        color32(ann.style.stroke, ann.style.opacity),
    );
    let fill = color32(ann.style.fill, ann.style.opacity);
    let rect = t.rect_to_screen(ann.rect);

    match &ann.kind {
        AnnotationKind::Highlight => {
            painter.rect_filled(rect, CornerRadius::same(1), fill);
        }
        AnnotationKind::Rect => {
            painter.rect(rect, CornerRadius::ZERO, fill, stroke, StrokeKind::Middle);
        }
        AnnotationKind::Ellipse => {
            painter.add(epaint::EllipseShape {
                center: rect.center(),
                radius: rect.size() / 2.0,
                fill,
                stroke,
                angle: 0.0,
            });
        }
        AnnotationKind::Line { p1, p2, arrow_end } => {
            let a = t.to_screen(*p1);
            let b = t.to_screen(*p2);
            painter.line_segment([a, b], stroke);
            if *arrow_end {
                paint_arrowhead(painter, a, b, stroke);
            }
        }
        AnnotationKind::Freehand { points } => {
            let screen: Vec<Pos2> = points.iter().map(|p| t.to_screen(*p)).collect();
            if screen.len() >= 2 {
                painter.add(Shape::line(screen, stroke));
            }
        }
        AnnotationKind::Polygon { points, cloudy } => match cloudy {
            Some(intensity) => paint_cloud(painter, t, points, *intensity, fill, stroke),
            None => {
                let screen: Vec<Pos2> = points.iter().map(|p| t.to_screen(*p)).collect();
                if screen.len() >= 3 {
                    if ann.style.fill.is_visible() {
                        painter.add(Shape::convex_polygon(screen.clone(), fill, Stroke::NONE));
                    }
                    painter.add(Shape::closed_line(screen, stroke));
                }
            }
        },
        AnnotationKind::PolyLine { points, arrow_end } => {
            let screen: Vec<Pos2> = points.iter().map(|p| t.to_screen(*p)).collect();
            if screen.len() >= 2 {
                painter.add(Shape::line(screen.clone(), stroke));
                if *arrow_end {
                    paint_arrowhead(
                        painter,
                        screen[screen.len() - 2],
                        screen[screen.len() - 1],
                        stroke,
                    );
                }
            }
        }
        AnnotationKind::Stamp { text, font_size } => {
            paint_stamp(painter, rect, text, *font_size * t.zoom, ann);
        }
        AnnotationKind::ImageStamp { .. } => {
            match images.get(ann.id) {
                Some(tex) => {
                    let tint = Color32::from_white_alpha(
                        (ann.style.opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
                    );
                    let mut mesh = Mesh::with_texture(tex.id());
                    mesh.add_rect_with_uv(
                        rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        tint,
                    );
                    painter.add(Shape::mesh(mesh));
                }
                // A picture that will not decode still occupies the page, and
                // saying so beats a hole where a signature should be.
                None => {
                    painter.rect_stroke(
                        rect,
                        CornerRadius::same(2),
                        Stroke::new(1.0, Color32::from_gray(140)),
                        StrokeKind::Inside,
                    );
                }
            }
        }
        AnnotationKind::TextBox {
            text,
            font_size,
            align,
        } => {
            if ann.style.fill.is_visible() {
                painter.rect_filled(rect, CornerRadius::ZERO, fill);
            }
            let color = color32(ann.style.stroke, ann.style.opacity);
            let galley = painter.layout(
                text.clone(),
                FontId::proportional(font_size * t.zoom),
                color,
                rect.width().max(4.0),
            );
            let x = match align {
                TextAlign::Left => rect.min.x,
                TextAlign::Center => rect.min.x + (rect.width() - galley.size().x) / 2.0,
                TextAlign::Right => rect.max.x - galley.size().x,
            };
            painter.galley(Pos2::new(x, rect.min.y), galley, color);
        }
    }
}

/// Draw a stamp: a rounded box in the stamp's colour with its word centred
/// inside, shrunk to fit if the box is narrower than the word.
///
/// The word is drawn heavy rather than in a bold face: evo ships one weight of
/// Liberation Sans, and painting the galley a few tenths of a pixel apart is
/// what the PDF appearance stream does too (text rendering mode 2, fill *and*
/// stroke). Screen and print thicken the same letters the same way.
fn paint_stamp(painter: &egui::Painter, rect: Rect, text: &str, font_px: f32, ann: &Annotation) {
    let ink = color32(ann.style.stroke, ann.style.opacity);
    if ann.style.fill.is_visible() {
        painter.rect_filled(
            rect,
            CornerRadius::same(stamp_radius(rect) as u8),
            color32(ann.style.fill, ann.style.opacity),
        );
    }
    painter.rect_stroke(
        rect,
        CornerRadius::same(stamp_radius(rect) as u8),
        Stroke::new((ann.style.stroke_width * 1.5).max(1.0), ink),
        StrokeKind::Inside,
    );
    if text.is_empty() {
        return;
    }
    let inner = rect.width() - stamp_radius(rect) * 2.0;
    let mut galley =
        painter.layout_no_wrap(text.to_owned(), FontId::proportional(font_px.max(1.0)), ink);
    if galley.size().x > inner && galley.size().x > 0.0 {
        let shrunk = (font_px * inner / galley.size().x).max(1.0);
        galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(shrunk), ink);
    }
    let pos = rect.center() - galley.size() / 2.0;
    let weight = (font_px * 0.03).clamp(0.3, 1.2);
    for offset in [
        Vec2::ZERO,
        Vec2::new(weight, 0.0),
        Vec2::new(0.0, weight),
        Vec2::new(weight, weight),
    ] {
        painter.galley(pos + offset, galley.clone(), ink);
    }
}

/// The corner rounding of a stamp box, in the same units as the box.
fn stamp_radius(rect: Rect) -> f32 {
    (rect.height() * 0.18).clamp(1.0, 12.0)
}

/// Draw a cloudy polygon: the base shape's fill, with the scalloped outline
/// from [`cloud_arcs`](crate::doc::geometry::cloud_arcs) as its edge.
///
/// The fill follows the plain outline rather than the scallops, which keeps a
/// filled cloud one tessellated shape instead of a bump-by-bump patchwork; the
/// scallops are what the eye reads anyway.
fn paint_cloud(
    painter: &egui::Painter,
    t: &PageTransform,
    points: &[crate::doc::geometry::PdfPoint],
    intensity: f32,
    fill: Color32,
    stroke: Stroke,
) {
    if fill.a() > 0 && points.len() >= 3 {
        let screen: Vec<Pos2> = points.iter().map(|p| t.to_screen(*p)).collect();
        painter.add(Shape::convex_polygon(screen, fill, Stroke::NONE));
    }
    for arc in crate::doc::geometry::cloud_arcs(points, intensity) {
        painter.add(epaint::CubicBezierShape::from_points_stroke(
            [
                t.to_screen(arc.from),
                t.to_screen(arc.c1),
                t.to_screen(arc.c2),
                t.to_screen(arc.to),
            ],
            false,
            Color32::TRANSPARENT,
            stroke,
        ));
    }
}

fn paint_arrowhead(painter: &egui::Painter, from: Pos2, to: Pos2, stroke: Stroke) {
    let dir = to - from;
    let len = dir.length();
    if len < 1.0 {
        return;
    }
    let dir = dir / len;
    let size = (stroke.width * 4.0).max(8.0).min(len * 0.5);
    let perp = Vec2::new(-dir.y, dir.x);
    let p1 = to - dir * size + perp * size * 0.5;
    let p2 = to - dir * size - perp * size * 0.5;
    painter.add(Shape::convex_polygon(
        vec![to, p1, p2],
        stroke.color,
        Stroke::NONE,
    ));
}

fn paint_selection(painter: &egui::Painter, t: &PageTransform, ann: &Annotation) {
    use crate::tools::select::Handle;
    let handle_px = 7.0;

    if let AnnotationKind::Line { p1, p2, .. } = &ann.kind {
        for p in [p1, p2] {
            let center = t.to_screen(*p);
            paint_handle(painter, center, handle_px);
        }
        return;
    }

    // Point-list shapes are edited vertex by vertex, so that is where the
    // handles go -- a bounding box would only offer to stretch them.
    if let AnnotationKind::Polygon { points, .. } | AnnotationKind::PolyLine { points, .. } =
        &ann.kind
    {
        for p in points {
            paint_handle(painter, t.to_screen(*p), handle_px);
        }
        return;
    }

    let rect = t.rect_to_screen(ann.rect);
    painter.rect_stroke(
        rect.expand(1.0),
        CornerRadius::ZERO,
        Stroke::new(1.0, SELECTION_COLOR),
        StrokeKind::Outside,
    );
    for handle in Handle::RECT_HANDLES {
        let center = t.to_screen(handle.anchor(ann.rect));
        paint_handle(painter, center, handle_px);
    }
}

fn paint_handle(painter: &egui::Painter, center: Pos2, size: f32) {
    let rect = Rect::from_center_size(center, Vec2::splat(size));
    painter.rect(
        rect,
        CornerRadius::same(1),
        Color32::WHITE,
        Stroke::new(1.5, SELECTION_COLOR),
        StrokeKind::Inside,
    );
}

fn pointer_info_at(
    dc: &DocState,
    slot: &PageSlot,
    content_rect: Rect,
    pos: Pos2,
    modifiers: Modifiers,
) -> PointerInfo {
    let t = transform_for(dc, slot, content_rect);
    let info = &dc.doc.pages[dc.pages.source_of(slot.original)];
    PointerInfo {
        page: slot.original,
        pos: t.from_screen(pos),
        modifiers,
        tol: 6.0 / dc.viewport.zoom,
        page_w: info.width,
        page_h: info.height,
    }
}

fn route_pointer(
    ui: &egui::Ui,
    dc: &mut DocState,
    layout: &Layout,
    content_rect: Rect,
    response: &egui::Response,
) {
    let modifiers = ui.input(|i| i.modifiers);
    let space_pan = ui.input(|i| i.key_down(egui::Key::Space));

    // Pan (tool or spacebar): scroll by drag delta.
    if (dc.tool == ActiveTool::Pan || space_pan) && response.dragged() {
        let delta = response.drag_delta();
        dc.viewport.pending_offset = Some((dc.viewport.offset - delta).max(Vec2::ZERO));
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        return;
    }

    let slot_at = |pos: Pos2| -> Option<&PageSlot> {
        layout.slots.iter().find(|slot| {
            Rect::from_min_size(content_rect.min + slot.rect.min.to_vec2(), slot.rect.size())
                .expand(4.0)
                .contains(pos)
        })
    };

    // While vertices are going down, the pointer's own position is part of the
    // drawing: it is where the next corner would land.
    if dc.tool.places_vertices()
        && let Some(pos) = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos())
        && let Some(slot) = slot_at(pos)
    {
        let p = pointer_info_at(dc, slot, content_rect, pos, modifiers);
        tools::on_hover(dc, p.page, p.pos);
    }

    // A second click in the same place says the shape is finished.
    if response.double_clicked() && dc.tool.places_vertices() {
        tools::finish_placement(dc);
        return;
    }

    // Double-click a text box to edit it.
    if response.double_clicked()
        && dc.tool == ActiveTool::Select
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(slot) = slot_at(pos)
    {
        let p = pointer_info_at(dc, slot, content_rect, pos, modifiers);
        if let Some(id) = tools::select::hit_test(&dc.store, p.page, p.pos, p.tol)
            && let Some(ann) = dc.store.get(id)
            && matches!(ann.kind, AnnotationKind::TextBox { .. })
        {
            dc.tool_ctl.editing_before = Some(ann.clone());
            dc.tool_ctl.editing_focus_pending = true;
            dc.selection = Some(id);
            dc.editing_text = Some(id);
            dc.tool_ctl.state = ToolState::Idle;
            return;
        }
    }

    // Plain clicks: egui only sets the drag flags once the pointer moves, and
    // `clicked()` is mutually exclusive with them, so a click has to drive the
    // tool state machine on its own (press immediately followed by release).
    if response.clicked()
        && !response.double_clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        if dc.editing_text.is_some() {
            commit_text_edit(dc);
        }
        if let Some(slot) = slot_at(pos) {
            let p = pointer_info_at(dc, slot, content_rect, pos, modifiers);
            tools::on_press(dc, &p);
            tools::on_release(dc, &p);
        } else if dc.tool == ActiveTool::Select {
            dc.selection = None;
        }
        return;
    }

    if response.drag_started()
        && let Some(pos) = response.interact_pointer_pos()
    {
        // Clicking outside the text overlay commits the edit.
        if dc.editing_text.is_some() {
            commit_text_edit(dc);
        }
        if let Some(slot) = slot_at(pos) {
            let p = pointer_info_at(dc, slot, content_rect, pos, modifiers);
            tools::on_press(dc, &p);
        } else if dc.tool == ActiveTool::Select {
            dc.selection = None;
        }
    } else if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(page) = dc.tool_ctl.active_page()
        && let Some(slot) = layout.slots.iter().find(|s| s.original == page)
    {
        let p = pointer_info_at(dc, slot, content_rect, pos, modifiers);
        tools::on_drag(dc, &p);
    } else if response.drag_stopped()
        && let Some(page) = dc.tool_ctl.active_page()
        && let Some(slot) = layout.slots.iter().find(|s| s.original == page)
    {
        let pos = response.interact_pointer_pos().unwrap_or(content_rect.min);
        let p = pointer_info_at(dc, slot, content_rect, pos, modifiers);
        tools::on_release(dc, &p);
    }
}

fn text_edit_overlay(ui: &mut egui::Ui, dc: &mut DocState, layout: &Layout, content_rect: Rect) {
    let Some(id) = dc.editing_text else {
        return;
    };
    let Some(ann) = dc.store.get(id).cloned() else {
        dc.editing_text = None;
        return;
    };
    let Some(slot) = layout.slots.iter().find(|s| s.original == ann.page) else {
        return;
    };
    let t = transform_for(dc, slot, content_rect);
    let rect = t.rect_to_screen(ann.rect);

    let AnnotationKind::TextBox { font_size, .. } = ann.kind else {
        dc.editing_text = None;
        return;
    };

    let mut text = match &dc.store.get(id).unwrap().kind {
        AnnotationKind::TextBox { text, .. } => text.clone(),
        _ => unreachable!(),
    };

    // Transparent editor so the page stays visible while typing; a thin
    // accent border marks the box bounds instead.
    let response = ui.put(
        rect,
        TextEdit::multiline(&mut text)
            .font(FontId::proportional(font_size * t.zoom))
            .text_color(color32(ann.style.stroke, 1.0))
            .background_color(Color32::TRANSPARENT)
            .margin(egui::Margin::ZERO),
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, SELECTION_COLOR),
        StrokeKind::Outside,
    );

    if response.changed()
        && let Some(a) = dc.store.get_mut(id)
        && let AnnotationKind::TextBox { text: stored, .. } = &mut a.kind
    {
        *stored = text;
    }

    if dc.tool_ctl.editing_focus_pending {
        response.request_focus();
        dc.tool_ctl.editing_focus_pending = false;
    }

    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if esc || (response.lost_focus() && !response.has_focus()) {
        commit_text_edit(dc);
    }
}

/// Finish inline text editing, recording the right undo command.
pub fn commit_text_edit(dc: &mut DocState) {
    let Some(id) = dc.editing_text.take() else {
        return;
    };
    let before = dc.tool_ctl.editing_before.take();
    let Some(current) = dc.store.get(id).cloned() else {
        return;
    };
    let text_empty =
        matches!(&current.kind, AnnotationKind::TextBox { text, .. } if text.trim().is_empty());

    match before {
        None => {
            // Newly placed box: empty text means abandon it silently.
            if text_empty {
                dc.store.remove(id);
                dc.selection = None;
            } else {
                dc.history.record(Command::AddAnnotation(current));
            }
        }
        Some(before) => {
            if text_empty {
                dc.store.remove(id);
                dc.selection = None;
                dc.history.record(Command::RemoveAnnotation(before));
            } else if current != before {
                dc.history.record(Command::ModifyAnnotation {
                    before,
                    after: current,
                });
            }
        }
    }
}

/// Public wrapper for the thumbnail sidebar.
pub fn paint_rotated_texture_pub(
    painter: &egui::Painter,
    tex: &egui::TextureHandle,
    rect: Rect,
    rotation: ExtraRotation,
) {
    paint_rotated_texture(painter, tex, rect, rotation);
}

/// Draw `tex` into `rect` rotated by the page's user rotation (90-degree
/// steps are pure UV permutations).
fn paint_rotated_texture(
    painter: &egui::Painter,
    tex: &egui::TextureHandle,
    rect: Rect,
    rotation: ExtraRotation,
) {
    let uv = match rotation {
        // Screen corners in order TL, TR, BR, BL -> source UVs.
        ExtraRotation::None => [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
        ExtraRotation::Cw90 => [(0.0, 1.0), (0.0, 0.0), (1.0, 0.0), (1.0, 1.0)],
        ExtraRotation::Cw180 => [(1.0, 1.0), (0.0, 1.0), (0.0, 0.0), (1.0, 0.0)],
        ExtraRotation::Cw270 => [(1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)],
    };
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let mut mesh = Mesh::with_texture(tex.id());
    for (pos, (u, v)) in corners.iter().zip(uv.iter()) {
        mesh.vertices.push(epaint::Vertex {
            pos: *pos,
            uv: Pos2::new(*u, *v),
            color: Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::annotation::{Annotation, Style};
    use crate::doc::geometry::{PdfPoint, PdfRect};
    use crate::tools::ToolState;

    fn state(ctx: &egui::Context) -> DocState {
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("fixture");
        let doc = crate::doc::Document::load_bytes(bytes, None).expect("load");
        DocState::new(doc, ctx, crate::render::engine::EnginePref::Hayro)
    }

    fn one_frame(dc: &mut DocState) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            poll_worker(dc, ui.ctx());
            show(ui, dc);
        });
    }

    /// One real frame with every new shape on the page: painted, selected,
    /// and half-placed. Curves, closed outlines and vertex handles all go
    /// through egui's tessellator, which has opinions about degenerate paths.
    #[test]
    fn a_frame_with_polygons_clouds_and_a_shape_being_placed_draws() {
        let ctx = egui::Context::default();
        let mut dc = state(&ctx);
        let triangle = vec![
            PdfPoint::new(100.0, 600.0),
            PdfPoint::new(300.0, 600.0),
            PdfPoint::new(200.0, 700.0),
        ];
        let kinds = [
            AnnotationKind::Polygon {
                points: triangle.clone(),
                cloudy: None,
            },
            AnnotationKind::Polygon {
                points: triangle.clone(),
                cloudy: Some(2.0),
            },
            AnnotationKind::PolyLine {
                points: triangle.clone(),
                arrow_end: true,
            },
            // Nothing anybody would draw on purpose, and nothing that may take
            // a frame down either.
            AnnotationKind::Polygon {
                points: vec![PdfPoint::new(50.0, 50.0)],
                cloudy: Some(1.0),
            },
            AnnotationKind::PolyLine {
                points: Vec::new(),
                arrow_end: true,
            },
        ];
        for kind in kinds {
            let id = dc.store.alloc_id();
            dc.store.insert(Annotation {
                id,
                page: 0,
                kind,
                rect: PdfRect::from_points(
                    PdfPoint::new(100.0, 600.0),
                    PdfPoint::new(300.0, 700.0),
                ),
                style: Style {
                    fill: crate::doc::annotation::Color::rgba(0, 0, 255, 60),
                    ..Style::default()
                },
            });
        }

        // Selected: the vertex handles are painted too.
        dc.selection = Some(2);
        one_frame(&mut dc);

        // And mid-gesture, with the rubber-band segment following the cursor.
        dc.selection = None;
        dc.tool = crate::tools::ActiveTool::Polygon;
        dc.tool_ctl.state = ToolState::Placing {
            page: 0,
            points: vec![PdfPoint::new(72.0, 100.0), PdfPoint::new(200.0, 140.0)],
            hover: PdfPoint::new(240.0, 90.0),
        };
        one_frame(&mut dc);
        assert_eq!(dc.store.on_page(0).count(), 5, "drawing changed nothing");
    }

    /// A frame with both kinds of stamp on it, and the picture cache that
    /// serves one of them: a texture is made once, kept while its annotation
    /// is, and thrown away with it -- which is what makes Delete and Undo free
    /// without either of them knowing the cache exists.
    #[test]
    fn stamps_draw_and_their_pictures_are_cached_for_exactly_as_long_as_they_last() {
        let ctx = egui::Context::default();
        let mut dc = state(&ctx);
        let place = |dc: &mut DocState, kind: AnnotationKind, y: f32| -> u64 {
            let id = dc.store.alloc_id();
            dc.store.insert(Annotation {
                id,
                page: 0,
                kind,
                rect: PdfRect::from_min_size(PdfPoint::new(100.0, y), 160.0, 44.0),
                style: Style::default(),
            });
            id
        };
        let words = place(
            &mut dc,
            AnnotationKind::Stamp {
                text: "APPROVED".into(),
                font_size: 20.0,
            },
            600.0,
        );
        // A word too long for its box, which is the branch that re-lays it out.
        place(
            &mut dc,
            AnnotationKind::Stamp {
                text: "SUPERSEDED BY REVISION C".into(),
                font_size: 48.0,
            },
            500.0,
        );
        place(
            &mut dc,
            AnnotationKind::Stamp {
                text: String::new(),
                font_size: 20.0,
            },
            450.0,
        );
        let picture = place(
            &mut dc,
            AnnotationKind::ImageStamp {
                png: crate::export::pdf::tests::png_fixture(24, 12),
            },
            400.0,
        );
        // And one that will never decode: it may not take the frame down.
        place(
            &mut dc,
            AnnotationKind::ImageStamp {
                png: b"not a png".to_vec(),
            },
            350.0,
        );

        dc.selection = Some(words);
        one_frame(&mut dc);
        assert_eq!(
            dc.stamp_images.len(),
            2,
            "one entry per picture, good or not"
        );

        dc.store.remove(picture);
        one_frame(&mut dc);
        assert_eq!(
            dc.stamp_images.len(),
            1,
            "the deleted picture's texture went with it"
        );
        assert_eq!(dc.store.on_page(0).count(), 4, "drawing changed nothing");
    }
}
