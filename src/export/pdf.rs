//! PDF export via lopdf: the original file's bytes are re-read and the markup
//! layer is written in as real PDF annotations (with appearance streams so
//! every viewer shows them identically), or flattened into the page content.
//! Page rotate/delete/reorder are applied here too.

use std::collections::HashMap;
use std::path::Path;

use lopdf::{
    Dictionary, Document as LoDocument, LoadOptions, Object, ObjectId, Stream, dictionary,
};

use crate::doc::annotation::{Annotation, AnnotationKind, Color, TextAlign};
use crate::doc::geometry::{PdfPoint, PdfRect};
use crate::doc::page_ops::PageList;
use crate::doc::store::AnnotationStore;
use crate::doc::{Document, PageInfo};

/// One line of OCR text to write invisibly over a scanned page, in display
/// space (PDF points, y-up).
#[derive(Clone, Debug)]
pub struct OcrLine {
    pub text: String,
    pub rect: PdfRect,
}

#[derive(Clone, Default)]
pub struct ExportOptions {
    /// Bake markup into the page content stream instead of writing
    /// editable annotation objects.
    pub flatten: bool,
    /// Invisible text to overlay on scanned pages, keyed by *source* page,
    /// so the exported file is selectable and searchable.
    pub ocr_layers: Option<HashMap<usize, Vec<OcrLine>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("PDF error: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("could not write file: {0}")]
    Io(#[from] std::io::Error),
    #[error("page structure error")]
    BadStructure,
}

pub fn export_pdf(
    doc: &Document,
    pages: &PageList,
    store: &AnnotationStore,
    options: ExportOptions,
    path: &Path,
) -> Result<(), ExportError> {
    let bytes = export_pdf_bytes(doc, pages, store, options)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// What every saved copy of a protected document has to say out loud, because
/// lopdf decrypts on load and there is nowhere in the writing path where the
/// encryption could survive: the file evo writes is readable without the
/// password the original needed.
pub const DECRYPTED_EXPORT_NOTICE: &str = "Saved copies are not password-protected.";

/// Re-read the document's own bytes with lopdf, presenting the password the
/// document was opened with.
///
/// lopdf decrypts every object and drops `/Encrypt` on a successful encrypted
/// load, so from here on the export path is the ordinary one -- and the file it
/// writes is a decrypted copy. That is why [`DECRYPTED_EXPORT_NOTICE`] exists.
///
/// The second branch is for documents nobody typed a password for: a file
/// encrypted with an *empty* user password opens with no prompt anywhere in
/// evo, and lopdf's plain load already decrypts those. If some file ever slips
/// through with `/Encrypt` still standing, loading it again with the empty
/// password is the difference between a decrypted copy and a file half of
/// whose objects are ciphertext.
fn load_source(doc: &Document) -> Result<LoDocument, ExportError> {
    if let Some(password) = doc.password() {
        return Ok(LoDocument::load_mem_with_options(
            &doc.source,
            LoadOptions::with_password(password),
        )?);
    }
    let plain = LoDocument::load_mem(&doc.source)?;
    if !plain.trailer.has(b"Encrypt") {
        return Ok(plain);
    }
    Ok(LoDocument::load_mem_with_options(
        &doc.source,
        LoadOptions::with_password(""),
    )?)
}

pub fn export_pdf_bytes(
    doc: &Document,
    pages: &PageList,
    store: &AnnotationStore,
    options: ExportOptions,
) -> Result<Vec<u8>, ExportError> {
    let mut lo = load_source(doc)?;

    let page_map = lo.get_pages();
    let source_ids: Vec<ObjectId> = (1..=doc.pages.len() as u32)
        .map(|n| page_map.get(&n).copied().ok_or(ExportError::BadStructure))
        .collect::<Result<_, _>>()?;

    // Resolve each display slot's logical page to a page OBJECT. The first
    // occurrence of a source page keeps its original object; every further
    // occurrence (duplicated pages) gets a cloned page dict so rotation and
    // annotations stay independent (content streams remain shared).
    let mut seen_source: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut slot_ids: Vec<(usize, ObjectId)> = Vec::with_capacity(pages.order.len()); // (logical, id)
    for &logical in &pages.order {
        let source = pages.source_of(logical);
        let id = if seen_source.insert(source) {
            source_ids[source]
        } else {
            let cloned = lo.get_dictionary(source_ids[source])?.clone();
            lo.add_object(Object::Dictionary(cloned))
        };
        slot_ids.push((logical, id));
    }

    // Markup + OCR text layer + rotation, per display slot.
    for &(logical, page_id) in &slot_ids {
        let source = pages.source_of(logical);
        let info = doc.pages[source];

        if let Some(layers) = &options.ocr_layers
            && let Some(lines) = layers.get(&source)
        {
            append_ocr_text_layer(&mut lo, page_id, &info, lines)?;
        }

        let annotations: Vec<Annotation> = store.on_page(logical).cloned().collect();
        if !annotations.is_empty() {
            if options.flatten {
                flatten_annotations(&mut lo, page_id, &info, &annotations)?;
            } else {
                for ann in &annotations {
                    let annot_id = build_annotation(&mut lo, &info, ann);
                    push_page_annot(&mut lo, page_id, annot_id)?;
                }
            }
        }

        let extra = pages.rotation_of(logical).degrees();
        if extra != 0 {
            let total = (info.intrinsic_rotation + extra).rem_euclid(360);
            let page_dict = lo.get_dictionary_mut(page_id)?;
            page_dict.set("Rotate", total);
        }
    }

    // Reorder / delete / duplicate: rebuild the page tree flat, in display
    // order, whenever the slots differ from the pristine document.
    let order_changed = slot_ids.len() != doc.pages.len()
        || slot_ids
            .iter()
            .enumerate()
            .any(|(i, &(logical, id))| logical != i || id != source_ids[i]);
    if order_changed {
        let kids: Vec<ObjectId> = slot_ids.iter().map(|&(_, id)| id).collect();
        rebuild_page_tree(&mut lo, &kids)?;
    }

    let mut buf = Vec::new();
    lo.save_to(&mut buf)?;
    Ok(buf)
}

fn push_page_annot(
    lo: &mut LoDocument,
    page_id: ObjectId,
    annot_id: ObjectId,
) -> Result<(), ExportError> {
    // /Annots may be missing, a direct array, or a reference to an array.
    let existing = lo
        .get_dictionary(page_id)
        .ok()
        .and_then(|d| d.get(b"Annots").ok().cloned());
    let mut array = match existing {
        Some(Object::Array(a)) => a,
        Some(Object::Reference(r)) => match lo.get_object(r).ok() {
            Some(Object::Array(a)) => a.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    array.push(Object::Reference(annot_id));
    let page_dict = lo.get_dictionary_mut(page_id)?;
    page_dict.set("Annots", array);
    Ok(())
}

fn rebuild_page_tree(lo: &mut LoDocument, kids: &[ObjectId]) -> Result<(), ExportError> {
    let pages_id = lo
        .catalog()?
        .get(b"Pages")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .ok_or(ExportError::BadStructure)?;

    let kid_refs: Vec<Object> = kids.iter().map(|&id| Object::Reference(id)).collect();
    let count = kid_refs.len() as i64;

    let pages_dict = lo.get_dictionary_mut(pages_id)?;
    pages_dict.set("Kids", kid_refs);
    pages_dict.set("Count", count);

    // Reparent every kept page to the root so the flattened tree is valid.
    for &id in kids {
        let page_dict = lo.get_dictionary_mut(id)?;
        page_dict.set("Parent", Object::Reference(pages_id));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Coordinate mapping
// ---------------------------------------------------------------------------

/// Base (unrotated) crop-box dimensions of a page.
fn base_dims(info: &PageInfo) -> (f32, f32) {
    if info.intrinsic_rotation % 180 == 0 {
        (info.width, info.height)
    } else {
        (info.height, info.width)
    }
}

/// Map a point from evo's display space (y-up, intrinsic rotation applied,
/// origin at the displayed bottom-left) back to raw PDF user space.
fn display_to_user(info: &PageInfo, p: PdfPoint) -> (f32, f32) {
    let (x0, y0) = info.crop_origin;
    let (bw, bh) = base_dims(info);
    match info.intrinsic_rotation.rem_euclid(360) {
        90 => (x0 + bw - p.y, y0 + p.x),
        180 => (x0 + bw - p.x, y0 + bh - p.y),
        270 => (x0 + p.y, y0 + bh - p.x),
        _ => (x0 + p.x, y0 + p.y),
    }
}

/// Map a display-space rect to a normalized user-space [x0, y0, x1, y1].
fn user_rect(info: &PageInfo, r: crate::doc::geometry::PdfRect) -> [f32; 4] {
    let (ax, ay) = display_to_user(info, r.min);
    let (bx, by) = display_to_user(info, r.max);
    [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)]
}

// ---------------------------------------------------------------------------
// Annotation objects with appearance streams
// ---------------------------------------------------------------------------

fn color_array(c: Color) -> Vec<Object> {
    vec![
        Object::Real(c.r as f32 / 255.0),
        Object::Real(c.g as f32 / 255.0),
        Object::Real(c.b as f32 / 255.0),
    ]
}

fn fmt(v: f32) -> String {
    // Compact fixed-point formatting for content streams. Negative zero is
    // written as zero: it is the same number, and `-0` in a matrix reads as a
    // bug to anyone opening the stream.
    let v = if v == 0.0 { 0.0 } else { v };
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn rg(c: Color) -> String {
    format!(
        "{} {} {} rg",
        fmt(c.r as f32 / 255.0),
        fmt(c.g as f32 / 255.0),
        fmt(c.b as f32 / 255.0)
    )
}

fn stroke_rg(c: Color) -> String {
    format!(
        "{} {} {} RG",
        fmt(c.r as f32 / 255.0),
        fmt(c.g as f32 / 255.0),
        fmt(c.b as f32 / 255.0)
    )
}

/// Paint operator honoring which of stroke/fill are visible.
fn paint_op(ann: &Annotation) -> &'static str {
    let has_stroke = ann.style.stroke.is_visible() && ann.style.stroke_width > 0.0;
    let has_fill = ann.style.fill.is_visible();
    match (has_stroke, has_fill) {
        (true, true) => "B",
        (true, false) => "S",
        (false, true) => "f",
        (false, false) => "n",
    }
}

/// Resource names of the image XObjects a content stream may draw, by
/// annotation id. Image stamps are the only markup that needs anything from
/// outside its own operators, and this is how it is told what that thing is
/// called where it is about to be drawn.
type ImageNames = HashMap<crate::doc::annotation::AnnotationId, String>;

/// Content-stream ops for one annotation, in PDF **user space**.
fn content_ops(info: &PageInfo, ann: &Annotation, images: &ImageNames) -> String {
    let mut ops = String::from("q\n");
    let style = &ann.style;
    if style.stroke.is_visible() {
        ops.push_str(&format!(
            "{}\n{} w\n1 j 1 J\n",
            stroke_rg(style.stroke),
            fmt(style.stroke_width)
        ));
    }
    if style.fill.is_visible() {
        ops.push_str(&format!("{}\n", rg(style.fill)));
    }

    match &ann.kind {
        AnnotationKind::Highlight => {
            // Highlights always fill with their own color, never inherit.
            if style.fill.is_visible() {
                let r = user_rect(info, ann.rect);
                ops.push_str(&format!(
                    "{}\n{} {} {} {} re\nf\n",
                    rg(style.fill),
                    fmt(r[0]),
                    fmt(r[1]),
                    fmt(r[2] - r[0]),
                    fmt(r[3] - r[1]),
                ));
            }
        }
        AnnotationKind::Rect => {
            let r = user_rect(info, ann.rect);
            ops.push_str(&format!(
                "{} {} {} {} re\n{}\n",
                fmt(r[0]),
                fmt(r[1]),
                fmt(r[2] - r[0]),
                fmt(r[3] - r[1]),
                paint_op(ann)
            ));
        }
        AnnotationKind::Ellipse => {
            let r = user_rect(info, ann.rect);
            ops.push_str(&ellipse_path(r));
            ops.push_str(&format!("{}\n", paint_op(ann)));
        }
        AnnotationKind::Line { p1, p2, arrow_end } => {
            let (ax, ay) = display_to_user(info, *p1);
            let (bx, by) = display_to_user(info, *p2);
            ops.push_str(&format!(
                "{} {} m\n{} {} l\nS\n",
                fmt(ax),
                fmt(ay),
                fmt(bx),
                fmt(by)
            ));
            if *arrow_end {
                ops.push_str(&arrowhead_ops(
                    ax,
                    ay,
                    bx,
                    by,
                    style.stroke_width,
                    style.stroke,
                ));
            }
        }
        AnnotationKind::Freehand { points } => {
            if let Some(first) = points.first() {
                let (x, y) = display_to_user(info, *first);
                ops.push_str(&format!("{} {} m\n", fmt(x), fmt(y)));
                for p in &points[1..] {
                    let (x, y) = display_to_user(info, *p);
                    ops.push_str(&format!("{} {} l\n", fmt(x), fmt(y)));
                }
                ops.push_str("S\n");
            }
        }
        AnnotationKind::Polygon { points, cloudy } => {
            match cloudy {
                // The scallops are drawn here, in the appearance stream, as
                // well as declared in `/BE`: a viewer that ignores the border
                // effect still has to show a cloud rather than a bare box.
                Some(intensity) => ops.push_str(&cloud_path(info, points, *intensity)),
                None => ops.push_str(&polygon_path(info, points, true)),
            }
            ops.push_str(&format!("{}\n", paint_op(ann)));
        }
        AnnotationKind::PolyLine { points, arrow_end } => {
            ops.push_str(&polygon_path(info, points, false));
            ops.push_str("S\n");
            if *arrow_end && points.len() >= 2 {
                let (ax, ay) = display_to_user(info, points[points.len() - 2]);
                let (bx, by) = display_to_user(info, points[points.len() - 1]);
                ops.push_str(&arrowhead_ops(
                    ax,
                    ay,
                    bx,
                    by,
                    style.stroke_width,
                    style.stroke,
                ));
            }
        }
        AnnotationKind::Stamp { text, font_size } => {
            ops.push_str(&stamp_ops(info, ann, text, *font_size));
        }
        AnnotationKind::ImageStamp { .. } => {
            if let Some(name) = images.get(&ann.id) {
                // An image XObject draws into the unit square, so the matrix
                // that places it is the box itself, turned to face whichever
                // way the page is rotated.
                ops.push_str(&placement_matrix(info, ann.rect, &format!("/{name} Do\n")));
            }
        }
        AnnotationKind::TextBox {
            text,
            font_size,
            align,
        } => {
            ops.push_str(&text_ops_user_space(info, ann, text, *font_size, *align));
        }
    }
    ops.push_str("Q\n");
    ops
}

/// The basis vectors of the page's intrinsic rotation: where display-space
/// `+x` points in user space. Display `+y` is then `(-s, c)`.
fn rotation_basis(info: &PageInfo) -> (f32, f32) {
    match info.intrinsic_rotation.rem_euclid(360) {
        90 => (0.0f32, 1.0f32),
        180 => (-1.0, 0.0),
        270 => (0.0, -1.0),
        _ => (1.0, 0.0),
    }
}

/// Wrap `body` in a `cm` that maps the unit square onto `rect` as it is
/// displayed: `(0,0)` is the box's bottom-left corner, `(1,1)` its top-right,
/// whichever way the page itself is turned.
fn placement_matrix(info: &PageInfo, rect: PdfRect, body: &str) -> String {
    let (c, s) = rotation_basis(info);
    let (ox, oy) = display_to_user(info, rect.min);
    let (w, h) = (rect.width(), rect.height());
    format!(
        "q\n{} {} {} {} {} {} cm\n{body}Q\n",
        fmt(w * c),
        fmt(w * s),
        fmt(-h * s),
        fmt(h * c),
        fmt(ox),
        fmt(oy)
    )
}

/// A stamp: a rounded box with its word centred inside, drawn in a local space
/// where the box is `w` by `h` with its origin at the bottom-left corner.
///
/// The word is set in Helvetica in text rendering mode 2 -- filled *and*
/// stroked -- which is how it comes out heavy without a second font programme,
/// and it keeps the standard widths honest so the centring is arithmetic
/// rather than guesswork.
fn stamp_ops(info: &PageInfo, ann: &Annotation, text: &str, font_size: f32) -> String {
    let (c, s) = rotation_basis(info);
    let (ox, oy) = display_to_user(info, ann.rect.min);
    let (w, h) = (ann.rect.width(), ann.rect.height());
    let mut ops = format!(
        "q\n{} {} {} {} {} {} cm\n",
        fmt(c),
        fmt(s),
        fmt(-s),
        fmt(c),
        fmt(ox),
        fmt(oy)
    );

    let radius = (h * 0.18).clamp(1.0, 12.0);
    let border = (ann.style.stroke_width * 1.5).max(1.0);
    ops.push_str(&format!(
        "{}\n{}\n{} w\n1 j 1 J\n",
        stroke_rg(ann.style.stroke),
        rg(ann.style.fill),
        fmt(border)
    ));
    ops.push_str(&rounded_rect_path(w, h, radius));
    ops.push_str(if ann.style.fill.is_visible() {
        "B\n"
    } else {
        "S\n"
    });

    if !text.is_empty() {
        let natural: f32 = text.chars().map(|ch| char_width(ch, font_size)).sum();
        let inner = (w - 2.0 * radius).max(1.0);
        let size = if natural > inner && natural > 0.0 {
            font_size * inner / natural
        } else {
            font_size
        };
        let line_w: f32 = text.chars().map(|ch| char_width(ch, size)).sum();
        // Caps sit from the baseline up to about 0.72em, so their middle is
        // 0.36em above it: that is what centres the word rather than its box.
        let x = (w - line_w) / 2.0;
        let y = h / 2.0 - 0.36 * size;
        ops.push_str(&format!(
            "BT\n/EvoHelv {} Tf\n2 Tr\n{} w\n{}\n{}\n1 0 0 1 {} {} Tm\n({}) Tj\nET\n",
            fmt(size),
            fmt((size * 0.035).max(0.2)),
            rg(ann.style.stroke),
            stroke_rg(ann.style.stroke),
            fmt(x),
            fmt(y),
            escape_pdf_string(text)
        ));
    }
    ops.push_str("Q\n");
    ops
}

/// A rounded rectangle from (0,0) to (w,h), as a closed path.
fn rounded_rect_path(w: f32, h: f32, radius: f32) -> String {
    const K: f32 = 0.552_284_8;
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= 0.01 {
        return format!("0 0 {} {} re\n", fmt(w), fmt(h));
    }
    let k = r * K;
    let m = |x: f32, y: f32| format!("{} {} m\n", fmt(x), fmt(y));
    let l = |x: f32, y: f32| format!("{} {} l\n", fmt(x), fmt(y));
    let c = |x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32| {
        format!(
            "{} {} {} {} {} {} c\n",
            fmt(x1),
            fmt(y1),
            fmt(x2),
            fmt(y2),
            fmt(x),
            fmt(y)
        )
    };
    // Counter-clockwise from the bottom-left corner's end, rounding each corner
    // with the usual quarter-circle bezier.
    [
        m(r, 0.0),
        l(w - r, 0.0),
        c(w - r + k, 0.0, w, r - k, w, r),
        l(w, h - r),
        c(w, h - r + k, w - r + k, h, w - r, h),
        l(r, h),
        c(r - k, h, 0.0, h - r + k, 0.0, h - r),
        l(0.0, r),
        c(0.0, r - k, r - k, 0.0, r, 0.0),
        "h\n".to_owned(),
    ]
    .concat()
}

/// A straight path through `points`, closed for a polygon and left open for a
/// polyline.
fn polygon_path(info: &PageInfo, points: &[PdfPoint], close: bool) -> String {
    let mut ops = String::new();
    for (i, p) in points.iter().enumerate() {
        let (x, y) = display_to_user(info, *p);
        ops.push_str(&format!(
            "{} {} {}\n",
            fmt(x),
            fmt(y),
            if i == 0 { "m" } else { "l" }
        ));
    }
    if close && !points.is_empty() {
        ops.push_str("h\n");
    }
    ops
}

/// The scalloped outline of a cloudy polygon, as `c` operators.
fn cloud_path(info: &PageInfo, points: &[PdfPoint], intensity: f32) -> String {
    let arcs = crate::doc::geometry::cloud_arcs(points, intensity);
    if arcs.is_empty() {
        return polygon_path(info, points, true);
    }
    let mut ops = String::new();
    let (sx, sy) = display_to_user(info, arcs[0].from);
    ops.push_str(&format!("{} {} m\n", fmt(sx), fmt(sy)));
    for arc in &arcs {
        let (c1x, c1y) = display_to_user(info, arc.c1);
        let (c2x, c2y) = display_to_user(info, arc.c2);
        let (tx, ty) = display_to_user(info, arc.to);
        ops.push_str(&format!(
            "{} {} {} {} {} {} c\n",
            fmt(c1x),
            fmt(c1y),
            fmt(c2x),
            fmt(c2y),
            fmt(tx),
            fmt(ty)
        ));
    }
    ops.push_str("h\n");
    ops
}

/// Cubic-bezier ellipse inscribed in a user-space rect.
fn ellipse_path(r: [f32; 4]) -> String {
    const K: f32 = 0.552_284_8;
    let (cx, cy) = ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
    let (rx, ry) = ((r[2] - r[0]) / 2.0, (r[3] - r[1]) / 2.0);
    let (kx, ky) = (rx * K, ry * K);
    format!(
        "{} {} m\n{} {} {} {} {} {} c\n{} {} {} {} {} {} c\n{} {} {} {} {} {} c\n{} {} {} {} {} {} c\nh\n",
        fmt(cx + rx),
        fmt(cy),
        fmt(cx + rx),
        fmt(cy + ky),
        fmt(cx + kx),
        fmt(cy + ry),
        fmt(cx),
        fmt(cy + ry),
        fmt(cx - kx),
        fmt(cy + ry),
        fmt(cx - rx),
        fmt(cy + ky),
        fmt(cx - rx),
        fmt(cy),
        fmt(cx - rx),
        fmt(cy - ky),
        fmt(cx - kx),
        fmt(cy - ry),
        fmt(cx),
        fmt(cy - ry),
        fmt(cx + kx),
        fmt(cy - ry),
        fmt(cx + rx),
        fmt(cy - ky),
        fmt(cx + rx),
        fmt(cy),
    )
}

fn arrowhead_ops(ax: f32, ay: f32, bx: f32, by: f32, width: f32, color: Color) -> String {
    let (dx, dy) = (bx - ax, by - ay);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return String::new();
    }
    let (ux, uy) = (dx / len, dy / len);
    let size = (width * 4.0).max(8.0).min(len * 0.5);
    let (px, py) = (-uy, ux);
    let p1 = (
        bx - ux * size + px * size * 0.5,
        by - uy * size + py * size * 0.5,
    );
    let p2 = (
        bx - ux * size - px * size * 0.5,
        by - uy * size - py * size * 0.5,
    );
    format!(
        "{}\n{} {} m\n{} {} l\n{} {} l\nh f\n",
        rg(color),
        fmt(bx),
        fmt(by),
        fmt(p1.0),
        fmt(p1.1),
        fmt(p2.0),
        fmt(p2.1)
    )
}

// ---------------------------------------------------------------------------
// Text: Helvetica metrics, wrapping, and ops
// ---------------------------------------------------------------------------

/// Standard Helvetica AFM widths for ASCII 32..=126, in 1/1000 em.
/// (Liberation Sans, used on screen, is metric-compatible.)
const HELVETICA_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278,
    278, // ' '..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584,
    556, // '0'..'?'
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722,
    778, // '@'..'O'
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469,
    556, // 'P'..'_'
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556,
    556, // '`'..'o'
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, // 'p'..'~'
];

pub fn char_width(c: char, font_size: f32) -> f32 {
    let units = match c {
        ' '..='~' => HELVETICA_WIDTHS[(c as usize) - 32],
        _ => 556,
    };
    units as f32 / 1000.0 * font_size
}

/// Greedy word wrap matching the on-screen galley closely enough for v1.
pub fn wrap_text(text: &str, font_size: f32, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_w = 0.0f32;
        for word in raw_line.split(' ') {
            let word_w: f32 = word.chars().map(|c| char_width(c, font_size)).sum();
            let space_w = char_width(' ', font_size);
            let needed = if line.is_empty() {
                word_w
            } else {
                line_w + space_w + word_w
            };
            if needed <= max_width || line.is_empty() {
                if !line.is_empty() {
                    line.push(' ');
                    line_w += space_w;
                }
                line.push_str(word);
                line_w += word_w;
            } else {
                lines.push(std::mem::take(&mut line));
                line.push_str(word);
                line_w = word_w;
            }
        }
        lines.push(line);
    }
    lines
}

pub fn escape_pdf_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            c if (c as u32) < 256 => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

/// Text drawing ops for the FLATTEN path, drawn in user space with a rotation
/// correction so text reads upright on intrinsically rotated pages.
fn text_ops_user_space(
    info: &PageInfo,
    ann: &Annotation,
    text: &str,
    font_size: f32,
    align: TextAlign,
) -> String {
    // Transform from annotation-local display coords (origin at box top-left,
    // x right, y DOWN) into user space, as a cm matrix.
    let w = ann.rect.width();
    let top_left = PdfPoint::new(ann.rect.min.x, ann.rect.max.y);
    let (ox, oy) = display_to_user(info, top_left);
    // Basis vectors: local +x and local -y (down) in user space.
    let (c, s) = match info.intrinsic_rotation.rem_euclid(360) {
        90 => (0.0f32, 1.0f32),
        180 => (-1.0, 0.0),
        270 => (0.0, -1.0),
        _ => (1.0, 0.0),
    };
    // cm = [c s -s c ox oy]: rotation by the page's intrinsic angle.
    let mut ops = format!(
        "q\n{} {} {} {} {} {} cm\n",
        fmt(c),
        fmt(s),
        fmt(-s),
        fmt(c),
        fmt(ox),
        fmt(oy)
    );
    ops.push_str(&text_ops_local(ann, text, font_size, align, w));
    ops.push_str("Q\n");
    ops
}

/// Text ops in annotation-local space: origin at the box's top-left corner,
/// x right, y up (PDF text space), box extends to negative y.
fn text_ops_local(
    ann: &Annotation,
    text: &str,
    font_size: f32,
    align: TextAlign,
    box_width: f32,
) -> String {
    let pad = 2.0f32;
    let leading = font_size * 1.25;
    let lines = wrap_text(text, font_size, (box_width - 2.0 * pad).max(font_size));
    let mut ops = format!(
        "BT\n/EvoHelv {} Tf\n{}\n{} TL\n",
        fmt(font_size),
        rg(ann.style.stroke),
        fmt(leading)
    );
    let mut prev_x: Option<f32> = None;
    for line in &lines {
        let line_w: f32 = line.chars().map(|c| char_width(c, font_size)).sum();
        let x = match align {
            TextAlign::Left => pad,
            TextAlign::Center => (box_width - line_w) / 2.0,
            TextAlign::Right => box_width - pad - line_w,
        };
        match prev_x {
            None => ops.push_str(&format!("{} {} Td\n", fmt(x), fmt(-font_size))),
            Some(prev) => {
                ops.push_str("T*\n");
                if (x - prev).abs() > 0.01 {
                    ops.push_str(&format!("{} 0 Td\n", fmt(x - prev)));
                }
            }
        }
        prev_x = Some(x);
        ops.push_str(&format!("({}) Tj\n", escape_pdf_string(line)));
    }
    ops.push_str("ET\n");
    ops
}

// ---------------------------------------------------------------------------
// Invisible OCR text layer
// ---------------------------------------------------------------------------

/// Bounds for the synthesized font size of an OCR line, in points.
const OCR_MIN_SIZE: f32 = 4.0;
const OCR_MAX_SIZE: f32 = 72.0;
/// The baseline sits this fraction of the font size above the line box bottom.
const OCR_BASELINE_LIFT: f32 = 0.2;

/// Draw `lines` into the page's content in text rendering mode 3 (invisible),
/// so selection and search in other viewers land on the OCR text while the
/// scanned image is what's actually seen.
fn append_ocr_text_layer(
    lo: &mut LoDocument,
    page_id: ObjectId,
    info: &PageInfo,
    lines: &[OcrLine],
) -> Result<(), ExportError> {
    // Basis vectors for the page's intrinsic rotation, as in
    // `text_ops_user_space`: the text has to read along the displayed page.
    let (c, s) = match info.intrinsic_rotation.rem_euclid(360) {
        90 => (0.0f32, 1.0f32),
        180 => (-1.0, 0.0),
        270 => (0.0, -1.0),
        _ => (1.0, 0.0),
    };

    let mut body = String::new();
    for line in lines {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        let size = line.rect.height().clamp(OCR_MIN_SIZE, OCR_MAX_SIZE);
        let anchor = PdfPoint::new(line.rect.min.x, line.rect.min.y + OCR_BASELINE_LIFT * size);
        let (x, y) = display_to_user(info, anchor);
        // Stretch the standard-font glyphs to cover the recognized box, so a
        // selection rectangle in the viewer matches what's on the page.
        let natural: f32 = text.chars().map(|ch| char_width(ch, size)).sum();
        let tz = if natural > 0.0 {
            (line.rect.width() / natural * 100.0).clamp(10.0, 500.0)
        } else {
            100.0
        };
        body.push_str(&format!(
            "/EvoHelv {} Tf\n{} Tz\n{} {} {} {} {} {} Tm\n({}) Tj\n",
            fmt(size),
            fmt(tz),
            fmt(c),
            fmt(s),
            fmt(-s),
            fmt(c),
            fmt(x),
            fmt(y),
            escape_pdf_string(text)
        ));
    }
    if body.is_empty() {
        return Ok(());
    }

    materialize_resources(lo, page_id, true, false)?;
    let ops = format!("q\nBT\n3 Tr\n{body}ET\nQ\n");
    let stream_id = lo.add_object(Stream::new(Dictionary::new(), ops.into_bytes()));
    append_content_stream(lo, page_id, stream_id)
}

pub fn helvetica_font_dict() -> Dictionary {
    dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    }
}

// ---------------------------------------------------------------------------
// Building annotation dictionaries
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Image XObjects (image stamps)
// ---------------------------------------------------------------------------

/// zlib-compressed bytes, which is what `/FlateDecode` names.
fn deflate(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing to a Vec cannot fail; a corrupt stream is not a failure mode we
    // can reach here, so the fallback is simply the bytes uncompressed -- which
    // would then be tagged wrongly, so we keep the panic-free path honest by
    // returning the raw bytes only via the caller's `None`.
    if encoder.write_all(bytes).is_err() {
        return Vec::new();
    }
    encoder.finish().unwrap_or_default()
}

/// Turn a PNG into an image XObject (plus a soft mask when it has any
/// transparency), and return its object id.
///
/// PDF has no PNG: an image is samples plus a colour space, and the alpha
/// channel is a second, greyscale image hung off `/SMask`. The samples go in
/// `/FlateDecode`d, which is the same compression the PNG already used and the
/// only one every viewer is required to understand.
fn image_xobject(lo: &mut LoDocument, png: &[u8]) -> Option<ObjectId> {
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let pixels = decoded.into_raw();
    let mut rgb = Vec::with_capacity(pixels.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(pixels.len() / 4);
    for px in pixels.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
        alpha.push(px[3]);
    }

    let smask = if alpha.iter().any(|a| *a != 255) {
        let samples = deflate(&alpha);
        if samples.is_empty() {
            return None;
        }
        let dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
            "Filter" => "FlateDecode",
        };
        Some(lo.add_object(Stream::new(dict, samples)))
    } else {
        None
    };

    let samples = deflate(&rgb);
    if samples.is_empty() {
        return None;
    }
    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => w as i64,
        "Height" => h as i64,
        "ColorSpace" => "DeviceRGB",
        "BitsPerComponent" => 8,
        "Filter" => "FlateDecode",
    };
    if let Some(smask) = smask {
        dict.set("SMask", Object::Reference(smask));
    }
    Some(lo.add_object(Stream::new(dict, samples)))
}

fn build_annotation(lo: &mut LoDocument, info: &PageInfo, ann: &Annotation) -> ObjectId {
    // Derive the box from the actual geometry so the appearance stream is
    // never clipped, even if `rect` is stale for line/pen shapes.
    let geom_rect = match &ann.kind {
        AnnotationKind::Line { p1, p2, .. } => crate::doc::geometry::PdfRect::from_points(*p1, *p2),
        kind => match kind.points() {
            Some(points) => crate::tools::pen::bounding_rect(points),
            None => ann.rect,
        },
    };
    let r = user_rect(info, geom_rect);
    let mut pad = ann.style.stroke_width.max(1.0);
    if matches!(
        &ann.kind,
        AnnotationKind::Line {
            arrow_end: true,
            ..
        } | AnnotationKind::PolyLine {
            arrow_end: true,
            ..
        }
    ) {
        // Arrowheads extend perpendicular to the line beyond the stroke.
        pad = pad.max((ann.style.stroke_width * 4.0).max(8.0) * 0.6 + 1.0);
    }
    if let AnnotationKind::Polygon {
        cloudy: Some(intensity),
        ..
    } = &ann.kind
    {
        // The scallops stand a whole radius outside the vertices; a box drawn
        // round the vertices alone would shave the bumps off.
        pad += crate::doc::geometry::cloud_radius(*intensity);
    }
    // /Rect and the form BBox are kept identical so viewers map the
    // appearance 1:1 with no scaling.
    let tight = r;
    let r = [r[0] - pad, r[1] - pad, r[2] + pad, r[3] + pad];
    let bbox = r;

    // The picture an image stamp draws has to exist before the stream that
    // draws it, and it is named inside that stream's own resources.
    let mut images = ImageNames::new();
    let mut image_id = None;
    if let AnnotationKind::ImageStamp { png } = &ann.kind
        && let Some(id) = image_xobject(lo, png)
    {
        image_id = Some(id);
        images.insert(ann.id, "EvoImg".to_owned());
    }

    // Appearance stream: a Form XObject drawn in user-space coordinates.
    let mut ap_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "BBox" => bbox.iter().map(|v| Object::Real(*v)).collect::<Vec<_>>(),
    };
    if matches!(
        ann.kind,
        AnnotationKind::TextBox { .. } | AnnotationKind::Stamp { .. }
    ) {
        ap_dict.set(
            "Resources",
            dictionary! {
                "Font" => dictionary! { "EvoHelv" => Object::Dictionary(helvetica_font_dict()) },
            },
        );
    }
    if let Some(id) = image_id {
        let mut resources = match ap_dict.get(b"Resources").ok().cloned() {
            Some(Object::Dictionary(d)) => d,
            _ => Dictionary::new(),
        };
        resources.set("XObject", dictionary! { "EvoImg" => Object::Reference(id) });
        ap_dict.set("Resources", resources);
    }
    if ann.style.opacity < 1.0 {
        ap_dict.set(
            "Resources",
            merge_gs_resources(ap_dict.get(b"Resources").ok().cloned(), ann.style.opacity),
        );
    }

    let mut ops = String::new();
    if ann.style.opacity < 1.0 {
        ops.push_str("q\n/EvoGS gs\n");
    }
    ops.push_str(&content_ops(info, ann, &images));
    if ann.style.opacity < 1.0 {
        ops.push_str("Q\n");
    }
    let ap_id = lo.add_object(Stream::new(ap_dict, ops.into_bytes()));

    let subtype = match &ann.kind {
        AnnotationKind::Highlight => "Highlight",
        AnnotationKind::Rect => "Square",
        AnnotationKind::Ellipse => "Circle",
        AnnotationKind::Line { .. } => "Line",
        AnnotationKind::Freehand { .. } => "Ink",
        AnnotationKind::TextBox { .. } => "FreeText",
        AnnotationKind::Polygon { .. } => "Polygon",
        AnnotationKind::PolyLine { .. } => "PolyLine",
        AnnotationKind::Stamp { .. } | AnnotationKind::ImageStamp { .. } => "Stamp",
    };

    let mut dict = dictionary! {
        "Type" => "Annot",
        "Subtype" => subtype,
        "Rect" => r.iter().map(|v| Object::Real(*v)).collect::<Vec<_>>(),
        "F" => 4, // print
        "AP" => dictionary! { "N" => Object::Reference(ap_id) },
        "Border" => vec![0.into(), 0.into(), 0.into()],
    };
    if ann.style.stroke.is_visible() {
        dict.set("C", color_array(ann.style.stroke));
    }
    if ann.style.opacity < 1.0 {
        dict.set("CA", Object::Real(ann.style.opacity));
    }

    match &ann.kind {
        AnnotationKind::Highlight => {
            dict.set("C", color_array(ann.style.fill));
            // QuadPoints: UL, UR, LL, LR (tight around the highlighted area).
            dict.set(
                "QuadPoints",
                vec![
                    Object::Real(tight[0]),
                    Object::Real(tight[3]),
                    Object::Real(tight[2]),
                    Object::Real(tight[3]),
                    Object::Real(tight[0]),
                    Object::Real(tight[1]),
                    Object::Real(tight[2]),
                    Object::Real(tight[1]),
                ],
            );
        }
        AnnotationKind::Rect | AnnotationKind::Ellipse => {
            if ann.style.fill.is_visible() {
                dict.set("IC", color_array(ann.style.fill));
            }
            if ann.style.stroke_width > 0.0 {
                dict.set(
                    "BS",
                    dictionary! { "W" => Object::Real(ann.style.stroke_width) },
                );
            }
        }
        AnnotationKind::Line { p1, p2, arrow_end } => {
            let (ax, ay) = display_to_user(info, *p1);
            let (bx, by) = display_to_user(info, *p2);
            dict.set(
                "L",
                vec![
                    Object::Real(ax),
                    Object::Real(ay),
                    Object::Real(bx),
                    Object::Real(by),
                ],
            );
            if *arrow_end {
                dict.set(
                    "LE",
                    vec![
                        Object::Name(b"None".to_vec()),
                        Object::Name(b"OpenArrow".to_vec()),
                    ],
                );
            }
            if ann.style.stroke_width > 0.0 {
                dict.set(
                    "BS",
                    dictionary! { "W" => Object::Real(ann.style.stroke_width) },
                );
            }
        }
        AnnotationKind::Freehand { points } => {
            let list: Vec<Object> = points
                .iter()
                .flat_map(|p| {
                    let (x, y) = display_to_user(info, *p);
                    [Object::Real(x), Object::Real(y)]
                })
                .collect();
            dict.set("InkList", vec![Object::Array(list)]);
            if ann.style.stroke_width > 0.0 {
                dict.set(
                    "BS",
                    dictionary! { "W" => Object::Real(ann.style.stroke_width) },
                );
            }
        }
        AnnotationKind::Polygon { points, cloudy } => {
            dict.set("Vertices", vertices(info, points));
            if ann.style.fill.is_visible() {
                dict.set("IC", color_array(ann.style.fill));
            }
            if ann.style.stroke_width > 0.0 {
                dict.set(
                    "BS",
                    dictionary! { "W" => Object::Real(ann.style.stroke_width) },
                );
            }
            if let Some(intensity) = cloudy {
                // ISO 32000-1 12.5.6.9: a cloudy border effect at this
                // intensity. Acrobat and Bluebeam both write the /IT alongside
                // it, and both read it back as "this is a revision cloud".
                dict.set(
                    "BE",
                    dictionary! {
                        "S" => Object::Name(b"C".to_vec()),
                        "I" => Object::Real(crate::doc::geometry::clamp_cloud_intensity(*intensity)),
                    },
                );
                dict.set("IT", Object::Name(b"PolygonCloud".to_vec()));
            }
        }
        AnnotationKind::PolyLine { points, arrow_end } => {
            dict.set("Vertices", vertices(info, points));
            if *arrow_end {
                dict.set(
                    "LE",
                    vec![
                        Object::Name(b"None".to_vec()),
                        Object::Name(b"OpenArrow".to_vec()),
                    ],
                );
            }
            if ann.style.stroke_width > 0.0 {
                dict.set(
                    "BS",
                    dictionary! { "W" => Object::Real(ann.style.stroke_width) },
                );
            }
        }
        AnnotationKind::Stamp { text, .. } => {
            // `/Name` is the standard stamp a viewer may substitute its own
            // artwork for, so it is set only when the words really are one of
            // the standards. The appearance stream is written either way: a
            // stamp reading "Reviewed by Ada" has no name to go by, and a
            // viewer that recognizes /Approved still has to be shown what evo
            // drew rather than left to guess at the box size.
            if let Some(name) = crate::doc::annotation::standard_stamp_name(text) {
                dict.set("Name", Object::Name(name.as_bytes().to_vec()));
            }
            dict.set("Contents", Object::string_literal(text.as_str()));
        }
        AnnotationKind::ImageStamp { .. } => {
            dict.set("Contents", Object::string_literal("Image stamp"));
        }
        AnnotationKind::TextBox {
            text,
            font_size,
            align,
        } => {
            let c = ann.style.stroke;
            dict.set(
                "DA",
                Object::string_literal(format!(
                    "{} {} {} rg /EvoHelv {} Tf",
                    fmt(c.r as f32 / 255.0),
                    fmt(c.g as f32 / 255.0),
                    fmt(c.b as f32 / 255.0),
                    fmt(*font_size)
                )),
            );
            dict.set("Contents", Object::string_literal(text.as_str()));
            dict.set(
                "Q",
                match align {
                    TextAlign::Left => 0i64,
                    TextAlign::Center => 1,
                    TextAlign::Right => 2,
                },
            );
        }
    }

    lo.add_object(dict)
}

/// A point list as the flat `[x1 y1 x2 y2 ...]` array `/Vertices` wants, in
/// PDF user space.
fn vertices(info: &PageInfo, points: &[PdfPoint]) -> Vec<Object> {
    points
        .iter()
        .flat_map(|p| {
            let (x, y) = display_to_user(info, *p);
            [Object::Real(x), Object::Real(y)]
        })
        .collect()
}

fn merge_gs_resources(existing: Option<Object>, opacity: f32) -> Dictionary {
    let mut resources = match existing {
        Some(Object::Dictionary(d)) => d,
        _ => Dictionary::new(),
    };
    resources.set(
        "ExtGState",
        dictionary! {
            "EvoGS" => dictionary! {
                "Type" => "ExtGState",
                "ca" => Object::Real(opacity),
                "CA" => Object::Real(opacity),
            },
        },
    );
    resources
}

// ---------------------------------------------------------------------------
// Flatten path
// ---------------------------------------------------------------------------

/// Bake annotations into the page's content stream. Adds the needed font and
/// ExtGState entries to the page's (materialized) resources.
fn flatten_annotations(
    lo: &mut LoDocument,
    page_id: ObjectId,
    info: &PageInfo,
    annotations: &[Annotation],
) -> Result<(), ExportError> {
    let needs_font = annotations.iter().any(|a| {
        matches!(
            a.kind,
            AnnotationKind::TextBox { .. } | AnnotationKind::Stamp { .. }
        )
    });
    let min_opacity = annotations
        .iter()
        .map(|a| a.style.opacity)
        .fold(1.0f32, f32::min);

    // Every image stamp's picture, added to the page's own resources: baked
    // markup draws from the page, not from an appearance stream of its own.
    let mut images = ImageNames::new();
    let mut image_objects: Vec<(String, ObjectId)> = Vec::new();
    for ann in annotations {
        if let AnnotationKind::ImageStamp { png } = &ann.kind
            && let Some(id) = image_xobject(lo, png)
        {
            let name = format!("EvoImg{}", ann.id);
            images.insert(ann.id, name.clone());
            image_objects.push((name, id));
        }
    }

    materialize_resources(lo, page_id, needs_font, min_opacity < 1.0)?;
    if !image_objects.is_empty() {
        add_image_xobjects(lo, page_id, &image_objects)?;
    }

    let mut ops = String::from("q\n");
    for ann in annotations {
        if ann.style.opacity < 1.0 {
            ops.push_str(&format!("q\n/EvoGS{} gs\n", gs_key(ann.style.opacity)));
            ops.push_str(&content_ops(info, ann, &images));
            ops.push_str("Q\n");
        } else {
            ops.push_str(&content_ops(info, ann, &images));
        }
    }
    ops.push_str("Q\n");

    let stream_id = lo.add_object(Stream::new(Dictionary::new(), ops.into_bytes()));
    append_content_stream(lo, page_id, stream_id)?;

    // Register every distinct opacity ExtGState used above.
    let mut opacities: Vec<u32> = annotations
        .iter()
        .filter(|a| a.style.opacity < 1.0)
        .map(|a| gs_key(a.style.opacity))
        .collect();
    opacities.sort_unstable();
    opacities.dedup();
    if !opacities.is_empty() {
        add_gs_states(lo, page_id, &opacities)?;
    }
    Ok(())
}

/// Opacity quantized to a resource-name-friendly key (percent).
fn gs_key(opacity: f32) -> u32 {
    (opacity * 100.0).round().clamp(1.0, 100.0) as u32
}

/// Ensure the page has a direct /Resources dictionary (copying an inherited
/// one if needed) and add the flatten font entry.
fn materialize_resources(
    lo: &mut LoDocument,
    page_id: ObjectId,
    needs_font: bool,
    _needs_gs: bool,
) -> Result<(), ExportError> {
    // Find effective resources: on the page, or inherited via /Parent.
    let mut resources: Option<Dictionary> = None;
    let mut current = page_id;
    for _ in 0..32 {
        let dict = lo.get_dictionary(current)?;
        match dict.get(b"Resources") {
            Ok(Object::Dictionary(d)) => {
                resources = Some(d.clone());
                break;
            }
            Ok(Object::Reference(r)) => {
                if let Ok(Object::Dictionary(d)) = lo.get_object(*r) {
                    resources = Some(d.clone());
                }
                break;
            }
            _ => match dict.get(b"Parent") {
                Ok(Object::Reference(parent)) => current = *parent,
                _ => break,
            },
        }
    }
    let mut resources = resources.unwrap_or_default();

    if needs_font {
        let mut fonts = match resources.get(b"Font") {
            Ok(Object::Dictionary(d)) => d.clone(),
            Ok(Object::Reference(r)) => match lo.get_object(*r) {
                Ok(Object::Dictionary(d)) => d.clone(),
                _ => Dictionary::new(),
            },
            _ => Dictionary::new(),
        };
        fonts.set("EvoHelv", Object::Dictionary(helvetica_font_dict()));
        resources.set("Font", fonts);
    }

    let page_dict = lo.get_dictionary_mut(page_id)?;
    page_dict.set("Resources", resources);
    Ok(())
}

/// Register image XObjects on the page's (already materialized) resources.
fn add_image_xobjects(
    lo: &mut LoDocument,
    page_id: ObjectId,
    images: &[(String, ObjectId)],
) -> Result<(), ExportError> {
    let page_dict = lo.get_dictionary(page_id)?;
    let mut resources = match page_dict.get(b"Resources") {
        Ok(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    let mut xobjects = match resources.get(b"XObject") {
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Reference(r)) => match lo.get_object(*r) {
            Ok(Object::Dictionary(d)) => d.clone(),
            _ => Dictionary::new(),
        },
        _ => Dictionary::new(),
    };
    for (name, id) in images {
        xobjects.set(name.as_str(), Object::Reference(*id));
    }
    resources.set("XObject", xobjects);
    let page_dict = lo.get_dictionary_mut(page_id)?;
    page_dict.set("Resources", resources);
    Ok(())
}

fn add_gs_states(
    lo: &mut LoDocument,
    page_id: ObjectId,
    opacities: &[u32],
) -> Result<(), ExportError> {
    // Resources are guaranteed direct after materialize_resources.
    let page_dict = lo.get_dictionary(page_id)?;
    let mut resources = match page_dict.get(b"Resources") {
        Ok(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    let mut gs = match resources.get(b"ExtGState") {
        Ok(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    for &key in opacities {
        let alpha = key as f32 / 100.0;
        gs.set(
            format!("EvoGS{key}"),
            dictionary! {
                "Type" => "ExtGState",
                "ca" => Object::Real(alpha),
                "CA" => Object::Real(alpha),
            },
        );
    }
    resources.set("ExtGState", gs);
    let page_dict = lo.get_dictionary_mut(page_id)?;
    page_dict.set("Resources", resources);
    Ok(())
}

fn append_content_stream(
    lo: &mut LoDocument,
    page_id: ObjectId,
    stream_id: ObjectId,
) -> Result<(), ExportError> {
    let page_dict = lo.get_dictionary(page_id)?;
    let contents = page_dict.get(b"Contents").ok().cloned();
    let new_contents = match contents {
        Some(Object::Array(mut a)) => {
            a.push(Object::Reference(stream_id));
            Object::Array(a)
        }
        Some(Object::Reference(r)) => {
            Object::Array(vec![Object::Reference(r), Object::Reference(stream_id)])
        }
        _ => Object::Reference(stream_id),
    };
    let page_dict = lo.get_dictionary_mut(page_id)?;
    page_dict.set("Contents", new_contents);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::doc::annotation::Style;
    use crate::doc::geometry::PdfRect;

    /// A `w` by `h` PNG with a transparent left half, made on the spot.
    ///
    /// Built rather than committed: a fixture whose alpha channel is the whole
    /// point is clearer as three lines of code than as a binary blob nobody can
    /// read in a diff.
    pub(crate) fn png_fixture(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            if x < w / 2 {
                image::Rgba([0, 0, 0, 0])
            } else {
                image::Rgba([(x * 4) as u8, (y * 4) as u8, 200, 255])
            }
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode");
        png.into_inner()
    }

    fn fixture() -> Document {
        Document::load_path("tests/fixtures/sample.pdf".into(), None).unwrap()
    }

    fn sample_annotations(store: &mut AnnotationStore) {
        let kinds = [
            AnnotationKind::Rect,
            AnnotationKind::Ellipse,
            AnnotationKind::Highlight,
            AnnotationKind::Line {
                p1: PdfPoint::new(100.0, 100.0),
                p2: PdfPoint::new(300.0, 200.0),
                arrow_end: true,
            },
            AnnotationKind::Freehand {
                points: vec![
                    PdfPoint::new(50.0, 50.0),
                    PdfPoint::new(80.0, 90.0),
                    PdfPoint::new(120.0, 60.0),
                ],
            },
            AnnotationKind::TextBox {
                text: "Hello evo\nsecond line".into(),
                font_size: 14.0,
                align: TextAlign::Left,
            },
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            let style = if matches!(kind, AnnotationKind::Highlight) {
                Style {
                    stroke: Color::TRANSPARENT,
                    stroke_width: 0.0,
                    fill: Color::rgba(250, 220, 50, 255),
                    opacity: 0.45,
                }
            } else {
                Style::default()
            };
            let id = store.alloc_id();
            store.insert(Annotation {
                id,
                page: 0,
                kind,
                rect: PdfRect::from_points(
                    PdfPoint::new(100.0, 400.0 + 30.0 * i as f32),
                    PdfPoint::new(300.0, 430.0 + 30.0 * i as f32),
                ),
                style,
            });
        }
    }

    /// Every encrypted fixture, exported, has to come back as an ordinary PDF:
    /// hayro opens it with no password and lopdf finds its pages. This is the
    /// test that says out loud what the export path does to a protected
    /// document -- it decrypts it -- and it is why the export UI says so too.
    #[test]
    fn exporting_a_protected_document_writes_a_decrypted_copy() {
        for path in crate::doc::tests::PROTECTED {
            let doc = Document::load_bytes_with_password(
                crate::doc::tests::encrypted(path),
                None,
                Some("evo"),
            )
            .unwrap_or_else(|e| panic!("{path}: {e}"));
            let pages = PageList::new(doc.pages.len());
            let mut store = AnnotationStore::default();
            sample_annotations(&mut store);

            let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default())
                .unwrap_or_else(|e| panic!("{path}: {e}"));

            // No password anywhere: both readers open it cold.
            let reparsed = hayro::hayro_syntax::Pdf::new(std::sync::Arc::new(bytes.clone()))
                .unwrap_or_else(|e| panic!("{path}: hayro: {e:?}"));
            assert_eq!(reparsed.pages().len(), 2, "{path}");

            let lo = LoDocument::load_mem(&bytes).unwrap_or_else(|e| panic!("{path}: lopdf: {e}"));
            assert_eq!(lo.get_pages().len(), 2, "{path}");
            assert!(
                !lo.trailer.has(b"Encrypt"),
                "{path}: the exported copy still declares encryption"
            );

            // And the markup actually made it across.
            let page1 = lo.get_pages()[&1];
            let annots = lo
                .get_dictionary(page1)
                .unwrap()
                .get(b"Annots")
                .unwrap()
                .as_array()
                .unwrap();
            assert_eq!(annots.len(), 6, "{path}");
        }
    }

    /// A document protected with an empty user password is never prompted for
    /// and so carries no password of its own -- the export path still has to
    /// hand back something readable rather than half-ciphertext.
    #[test]
    fn exporting_an_empty_user_password_document_needs_no_password() {
        let doc = Document::load_bytes(
            crate::doc::tests::encrypted("tests/fixtures/encrypted-empty-user.pdf"),
            None,
        )
        .expect("opens without a password");
        assert_eq!(doc.password(), None);

        let pages = PageList::new(doc.pages.len());
        let bytes = export_pdf_bytes(
            &doc,
            &pages,
            &AnnotationStore::default(),
            ExportOptions::default(),
        )
        .expect("export");

        let lo = LoDocument::load_mem(&bytes).expect("lopdf reopens the export");
        assert_eq!(lo.get_pages().len(), 2);
        assert!(!lo.trailer.has(b"Encrypt"));
        assert_eq!(
            hayro::hayro_syntax::Pdf::new(std::sync::Arc::new(bytes))
                .expect("hayro reopens the export")
                .pages()
                .len(),
            2
        );
    }

    /// The wrong password must fail as a PDF error rather than quietly export
    /// a document with no pages in it -- which is what a plain load of an
    /// encrypted file produces.
    #[test]
    fn exporting_with_the_wrong_password_fails_rather_than_writing_nothing() {
        let mut doc = Document::load_bytes_with_password(
            crate::doc::tests::encrypted(crate::doc::tests::PROTECTED[0]),
            None,
            Some("evo"),
        )
        .expect("opens");
        doc.password = Some("not-it".into());
        let pages = PageList::new(doc.pages.len());
        let err = export_pdf_bytes(
            &doc,
            &pages,
            &AnnotationStore::default(),
            ExportOptions::default(),
        )
        .expect_err("a wrong password cannot export");
        // Whatever it says, it must not repeat the password back.
        assert!(!err.to_string().contains("not-it"), "{err}");
    }

    #[test]
    fn the_export_notice_says_the_copy_is_unprotected() {
        assert!(DECRYPTED_EXPORT_NOTICE.contains("not password-protected"));
    }

    #[test]
    fn exports_annotations_reloadable() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let mut store = AnnotationStore::default();
        sample_annotations(&mut store);

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();
        if let Ok(dir) = std::env::var("EVO_DUMP") {
            std::fs::write(std::path::Path::new(&dir).join("export-annots.pdf"), &bytes).unwrap();
        }
        let lo = LoDocument::load_mem(&bytes).unwrap();
        let page_map = lo.get_pages();
        let page1 = page_map[&1];
        let annots = lo
            .get_dictionary(page1)
            .unwrap()
            .get(b"Annots")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(annots.len(), 6);

        // hayro can parse the exported file too.
        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    /// The annotation object for the nth markup on page 1 of an export.
    fn exported_annot(bytes: &[u8], index: usize) -> Dictionary {
        let lo = LoDocument::load_mem(bytes).expect("lopdf reopens the export");
        let page1 = lo.get_pages()[&1];
        let annots = lo
            .get_dictionary(page1)
            .unwrap()
            .get(b"Annots")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        let id = annots[index].as_reference().expect("a reference");
        lo.get_dictionary(id).expect("the annotation").clone()
    }

    fn polygon_store(kinds: Vec<AnnotationKind>) -> AnnotationStore {
        let mut store = AnnotationStore::default();
        for kind in kinds {
            let id = store.alloc_id();
            let rect = crate::tools::pen::bounding_rect(kind.points().expect("points"));
            store.insert(Annotation {
                id,
                page: 0,
                kind,
                rect,
                style: Style::default(),
            });
        }
        store
    }

    fn triangle() -> Vec<PdfPoint> {
        vec![
            PdfPoint::new(100.0, 400.0),
            PdfPoint::new(300.0, 400.0),
            PdfPoint::new(200.0, 520.0),
        ]
    }

    /// A polygon and a cloud have to leave as the PDF annotations every other
    /// viewer knows -- `/Polygon` with `/Vertices`, and the cloudy one wearing
    /// the border effect Acrobat and Bluebeam write -- and the file has to
    /// still parse afterwards.
    #[test]
    fn exports_polygons_and_clouds_as_real_annotations() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = polygon_store(vec![
            AnnotationKind::Polygon {
                points: triangle(),
                cloudy: None,
            },
            AnnotationKind::Polygon {
                points: triangle(),
                cloudy: Some(1.5),
            },
        ]);

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();

        let plain = exported_annot(&bytes, 0);
        assert_eq!(
            plain.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Polygon"
        );
        let verts = plain.get(b"Vertices").unwrap().as_array().unwrap();
        assert_eq!(verts.len(), 6, "three points, two numbers each");
        assert_eq!(verts[0].as_float().unwrap(), 100.0);
        assert_eq!(verts[1].as_float().unwrap(), 400.0);
        assert!(plain.get(b"BE").is_err(), "a plain polygon has no cloud");

        let cloud = exported_annot(&bytes, 1);
        assert_eq!(
            cloud.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Polygon"
        );
        assert_eq!(
            cloud.get(b"IT").unwrap().as_name().unwrap(),
            b"PolygonCloud"
        );
        let be = cloud.get(b"BE").unwrap().as_dict().unwrap();
        assert_eq!(be.get(b"S").unwrap().as_name().unwrap(), b"C");
        assert!((be.get(b"I").unwrap().as_float().unwrap() - 1.5).abs() < 1e-4);

        // The scallops are in the appearance stream too, so a viewer that
        // ignores /BE still shows a cloud rather than a triangle.
        let lo = LoDocument::load_mem(&bytes).unwrap();
        let ap = cloud
            .get(b"AP")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"N")
            .unwrap()
            .as_reference()
            .unwrap();
        let stream = lo.get_object(ap).unwrap().as_stream().unwrap();
        let ops = String::from_utf8_lossy(&stream.content).into_owned();
        assert!(ops.matches(" c\n").count() > 12, "no scallops in: {ops}");

        // The bumps stand outside the vertices; /Rect has to hold them.
        let rect = cloud.get(b"Rect").unwrap().as_array().unwrap();
        assert!(rect[0].as_float().unwrap() < 100.0 - 5.0, "{rect:?}");

        // And the whole file still parses, with hayro as well as lopdf.
        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    /// A polyline is `/PolyLine` with its own vertices, and an arrow-ended one
    /// says so in `/LE` the way a `/Line` does.
    #[test]
    fn exports_polylines_with_their_line_endings() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = polygon_store(vec![
            AnnotationKind::PolyLine {
                points: triangle(),
                arrow_end: false,
            },
            AnnotationKind::PolyLine {
                points: triangle(),
                arrow_end: true,
            },
        ]);

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();

        let plain = exported_annot(&bytes, 0);
        assert_eq!(
            plain.get(b"Subtype").unwrap().as_name().unwrap(),
            b"PolyLine"
        );
        assert_eq!(plain.get(b"Vertices").unwrap().as_array().unwrap().len(), 6);
        assert!(plain.get(b"LE").is_err());

        let arrowed = exported_annot(&bytes, 1);
        let ends = arrowed.get(b"LE").unwrap().as_array().unwrap();
        assert_eq!(ends[0].as_name().unwrap(), b"None");
        assert_eq!(ends[1].as_name().unwrap(), b"OpenArrow");

        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    /// Flattening bakes the same shapes into the page instead, so a cloud
    /// printed from a flattened export is still a cloud.
    #[test]
    fn flattening_bakes_the_new_shapes_into_the_page() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = polygon_store(vec![
            AnnotationKind::Polygon {
                points: triangle(),
                cloudy: Some(2.0),
            },
            AnnotationKind::PolyLine {
                points: triangle(),
                arrow_end: true,
            },
        ]);

        let bytes = export_pdf_bytes(
            &doc,
            &pages,
            &store,
            ExportOptions {
                flatten: true,
                ..Default::default()
            },
        )
        .unwrap();

        let lo = LoDocument::load_mem(&bytes).unwrap();
        let page1 = lo.get_pages()[&1];
        assert!(
            lo.get_dictionary(page1).unwrap().get(b"Annots").is_err(),
            "flattening leaves no annotation objects"
        );
        let content = String::from_utf8_lossy(&lo.get_page_content(page1)).into_owned();
        assert!(content.matches(" c\n").count() > 12, "no scallops");
        assert!(content.contains("h f\n"), "no arrowhead");
        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    fn stamp_store(kinds: Vec<AnnotationKind>) -> AnnotationStore {
        let mut store = AnnotationStore::default();
        for (i, kind) in kinds.into_iter().enumerate() {
            let id = store.alloc_id();
            store.insert(Annotation {
                id,
                page: 0,
                kind,
                rect: PdfRect::from_min_size(
                    PdfPoint::new(100.0, 400.0 + 80.0 * i as f32),
                    160.0,
                    44.0,
                ),
                style: crate::doc::annotation::Style {
                    stroke: crate::tools::STAMP_RED,
                    stroke_width: 1.5,
                    ..Style::default()
                },
            });
        }
        store
    }

    /// A stamp leaves as `/Stamp`, and one whose words are a standard stamp
    /// says so in `/Name` -- but both carry an appearance stream, because a
    /// viewer that substitutes its own artwork for /Approved is not the only
    /// viewer, and one reading "Reviewed by Ada" has no name to go by at all.
    #[test]
    fn exports_stamps_naming_only_the_standard_ones() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = stamp_store(vec![
            AnnotationKind::Stamp {
                text: "APPROVED".into(),
                font_size: 20.0,
            },
            AnnotationKind::Stamp {
                text: "Reviewed by Ada".into(),
                font_size: 20.0,
            },
        ]);

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();

        let standard = exported_annot(&bytes, 0);
        assert_eq!(
            standard.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Stamp"
        );
        assert_eq!(
            standard.get(b"Name").unwrap().as_name().unwrap(),
            b"Approved"
        );

        let custom = exported_annot(&bytes, 1);
        assert_eq!(custom.get(b"Subtype").unwrap().as_name().unwrap(), b"Stamp");
        assert!(
            custom.get(b"Name").is_err(),
            "there is no standard stamp for those words"
        );

        // Both were drawn: a rounded box and the words inside it.
        let lo = LoDocument::load_mem(&bytes).unwrap();
        for annot in [&standard, &custom] {
            let ap = annot
                .get(b"AP")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"N")
                .unwrap()
                .as_reference()
                .unwrap();
            let stream = lo.get_object(ap).unwrap().as_stream().unwrap();
            let ops = String::from_utf8_lossy(&stream.content).into_owned();
            assert!(
                ops.matches(" c\n").count() >= 4,
                "no rounded corners: {ops}"
            );
            assert!(ops.contains("2 Tr"), "the word is not drawn heavy: {ops}");
            assert!(ops.contains(" Tj\n"), "no words at all: {ops}");
        }
        assert!(
            String::from_utf8_lossy(
                &lo.get_object(
                    standard
                        .get(b"AP")
                        .unwrap()
                        .as_dict()
                        .unwrap()
                        .get(b"N")
                        .unwrap()
                        .as_reference()
                        .unwrap()
                )
                .unwrap()
                .as_stream()
                .unwrap()
                .content
            )
            .contains("(APPROVED) Tj"),
            "the standard stamp still draws its own words"
        );

        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    /// An image stamp is the one piece of markup that is not a path: it leaves
    /// as a `/Stamp` whose appearance draws an image XObject, with the picture's
    /// transparency hung off it as an `/SMask`. Both readers have to open it.
    #[test]
    fn exports_an_image_stamp_as_an_image_xobject_with_its_transparency() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = stamp_store(vec![AnnotationKind::ImageStamp {
            png: png_fixture(64, 32),
        }]);

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();
        if let Ok(dir) = std::env::var("EVO_DUMP") {
            std::fs::write(std::path::Path::new(&dir).join("export-stamp.pdf"), &bytes).unwrap();
        }

        let annot = exported_annot(&bytes, 0);
        assert_eq!(annot.get(b"Subtype").unwrap().as_name().unwrap(), b"Stamp");

        let lo = LoDocument::load_mem(&bytes).expect("lopdf reopens the export");
        let ap_id = annot
            .get(b"AP")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"N")
            .unwrap()
            .as_reference()
            .unwrap();
        let ap = lo.get_object(ap_id).unwrap().as_stream().unwrap();
        let ops = String::from_utf8_lossy(&ap.content).into_owned();
        assert!(
            ops.contains("/EvoImg Do"),
            "the picture is not drawn: {ops}"
        );
        // Placed by the box, not by the pixel count.
        assert!(ops.contains("160 0 0 44"), "{ops}");

        let image_id = ap
            .dict
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"XObject")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"EvoImg")
            .unwrap()
            .as_reference()
            .unwrap();
        let image = lo.get_object(image_id).unwrap().as_stream().unwrap();
        assert_eq!(
            image.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Image"
        );
        assert_eq!(image.dict.get(b"Width").unwrap().as_i64().unwrap(), 64);
        assert_eq!(image.dict.get(b"Height").unwrap().as_i64().unwrap(), 32);
        assert_eq!(
            image.dict.get(b"ColorSpace").unwrap().as_name().unwrap(),
            b"DeviceRGB"
        );
        assert_eq!(
            image.dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"FlateDecode"
        );
        // Three bytes a pixel, once the samples are inflated again.
        assert_eq!(image.decompressed_content().unwrap().len(), 64 * 32 * 3);

        let smask_id = image.dict.get(b"SMask").unwrap().as_reference().unwrap();
        let smask = lo.get_object(smask_id).unwrap().as_stream().unwrap();
        assert_eq!(
            smask.dict.get(b"ColorSpace").unwrap().as_name().unwrap(),
            b"DeviceGray"
        );
        let alpha = smask.decompressed_content().unwrap();
        assert_eq!(alpha.len(), 64 * 32, "one byte a pixel");
        assert_eq!(alpha[0], 0, "the left half of the fixture is transparent");
        assert_eq!(alpha[63], 255, "and the right half is not");

        // And hayro -- the other reader in this program -- opens it too.
        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    /// A picture with no transparency needs no soft mask, and must not be
    /// given an empty one.
    #[test]
    fn an_opaque_picture_carries_no_soft_mask() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let opaque = image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(opaque)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode");
        let store = stamp_store(vec![AnnotationKind::ImageStamp {
            png: png.into_inner(),
        }]);

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();
        let lo = LoDocument::load_mem(&bytes).unwrap();
        let ap_id = exported_annot(&bytes, 0)
            .get(b"AP")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"N")
            .unwrap()
            .as_reference()
            .unwrap();
        let image_id = lo
            .get_object(ap_id)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"XObject")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"EvoImg")
            .unwrap()
            .as_reference()
            .unwrap();
        assert!(
            lo.get_object(image_id)
                .unwrap()
                .as_stream()
                .unwrap()
                .dict
                .get(b"SMask")
                .is_err()
        );
    }

    /// Flattened, a stamp is drawn into the page and its picture is registered
    /// on the page's own resources -- there is no appearance stream left to
    /// carry it.
    #[test]
    fn flattening_bakes_stamps_and_their_pictures_into_the_page() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = stamp_store(vec![
            AnnotationKind::Stamp {
                text: "DRAFT".into(),
                font_size: 24.0,
            },
            AnnotationKind::ImageStamp {
                png: png_fixture(16, 16),
            },
        ]);

        let bytes = export_pdf_bytes(
            &doc,
            &pages,
            &store,
            ExportOptions {
                flatten: true,
                ..Default::default()
            },
        )
        .unwrap();

        let lo = LoDocument::load_mem(&bytes).unwrap();
        let page1 = lo.get_pages()[&1];
        assert!(lo.get_dictionary(page1).unwrap().get(b"Annots").is_err());
        let content = String::from_utf8_lossy(&lo.get_page_content(page1)).into_owned();
        assert!(content.contains("(DRAFT) Tj"), "{content}");
        assert!(content.contains("/EvoImg2 Do"), "{content}");

        let resources = lo
            .get_dictionary(page1)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        assert!(
            resources
                .get(b"Font")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"EvoHelv")
                .is_ok(),
            "the stamp's font is on the page"
        );
        assert!(
            resources
                .get(b"XObject")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"EvoImg2")
                .is_ok(),
            "and so is its picture"
        );
        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    /// A picture that is not a picture must not take the export down with it:
    /// the stamp simply draws nothing.
    #[test]
    fn a_broken_picture_exports_as_nothing_rather_than_as_a_failure() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = stamp_store(vec![AnnotationKind::ImageStamp {
            png: b"not a png at all".to_vec(),
        }]);
        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default())
            .expect("the export still happens");
        assert!(hayro::hayro_syntax::Pdf::new(bytes).is_ok());
    }

    #[test]
    fn exports_flattened() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let mut store = AnnotationStore::default();
        sample_annotations(&mut store);

        let bytes = export_pdf_bytes(
            &doc,
            &pages,
            &store,
            ExportOptions {
                flatten: true,
                ..Default::default()
            },
        )
        .unwrap();
        if let Ok(dir) = std::env::var("EVO_DUMP") {
            std::fs::write(std::path::Path::new(&dir).join("export-flat.pdf"), &bytes).unwrap();
        }
        let lo = LoDocument::load_mem(&bytes).unwrap();
        let page1 = lo.get_pages()[&1];
        // No annotation objects; content stream got longer instead.
        assert!(lo.get_dictionary(page1).unwrap().get(b"Annots").is_err());
        let content = lo.get_page_content(page1);
        assert!(String::from_utf8_lossy(&content).contains("re"));
    }

    #[test]
    fn page_ops_apply() {
        let doc = fixture();
        let mut pages = PageList::new(doc.pages.len());
        pages.rotate_cw(0);
        pages.reorder(0, 1); // page 2 first
        let store = AnnotationStore::default();

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();
        let lo = LoDocument::load_mem(&bytes).unwrap();
        let page_map = lo.get_pages();
        assert_eq!(page_map.len(), 2);
        // First displayed page is now original page 2 (no /Rotate);
        // second is original page 1 with /Rotate 90.
        let second = lo.get_dictionary(page_map[&2]).unwrap();
        assert_eq!(second.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
    }

    #[test]
    fn delete_page_applies() {
        let doc = fixture();
        let mut pages = PageList::new(doc.pages.len());
        pages.delete_at(0);
        let store = AnnotationStore::default();

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();
        let lo = LoDocument::load_mem(&bytes).unwrap();
        assert_eq!(lo.get_pages().len(), 1);
    }

    #[test]
    fn duplicated_page_gets_independent_dict() {
        let doc = fixture();
        let mut pages = PageList::new(doc.pages.len());
        // Duplicate page 1 (logical 0) and rotate only the copy.
        let copy = pages.duplicate(0, 1);
        pages.states[copy].extra_rotation = crate::doc::geometry::ExtraRotation::Cw90;
        let store = AnnotationStore::default();

        let bytes = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()).unwrap();
        let lo = LoDocument::load_mem(&bytes).unwrap();
        let page_map = lo.get_pages();
        assert_eq!(page_map.len(), 3);
        // Slot 1 is the rotated copy; slot 0 must remain unrotated.
        let first = lo.get_dictionary(page_map[&1]).unwrap();
        assert!(first.get(b"Rotate").is_err());
        let second = lo.get_dictionary(page_map[&2]).unwrap();
        assert_eq!(second.get(b"Rotate").unwrap().as_i64().unwrap(), 90);
        // And they are different objects.
        assert_ne!(page_map[&1], page_map[&2]);
    }

    /// The OCR layer is written as real (invisible) page text, so any reader
    /// — including hayro, which is how evo indexes text — can select it.
    #[test]
    fn exports_invisible_ocr_text_layer() {
        let doc = fixture();
        let pages = PageList::new(doc.pages.len());
        let store = AnnotationStore::default();

        // Down near the bottom of the page, clear of the fixture's own text.
        let mut layers = HashMap::new();
        layers.insert(
            0,
            vec![OcrLine {
                text: "scannedmarker".into(),
                rect: PdfRect::from_points(PdfPoint::new(72.0, 100.0), PdfPoint::new(300.0, 114.0)),
            }],
        );
        let bytes = export_pdf_bytes(
            &doc,
            &pages,
            &store,
            ExportOptions {
                ocr_layers: Some(layers),
                ..Default::default()
            },
        )
        .unwrap();
        if let Ok(dir) = std::env::var("EVO_DUMP") {
            std::fs::write(std::path::Path::new(&dir).join("export-ocr.pdf"), &bytes).unwrap();
        }

        let lo = LoDocument::load_mem(&bytes).unwrap();
        let page1 = lo.get_pages()[&1];
        let content = String::from_utf8_lossy(&lo.get_page_content(page1)).into_owned();
        assert!(
            content.contains("3 Tr"),
            "no invisible text mode: {content}"
        );
        assert!(content.contains("(scannedmarker) Tj"), "{content}");

        // The font is registered on the page's own resources.
        let resources = lo.get_dictionary(page1).unwrap().get(b"Resources").unwrap();
        let fonts = resources
            .as_dict()
            .unwrap()
            .get(b"Font")
            .unwrap()
            .as_dict()
            .unwrap();
        assert!(fonts.get(b"EvoHelv").is_ok());

        // And it round-trips back out through text extraction.
        let pdf = hayro::hayro_syntax::Pdf::new(bytes).unwrap();
        let extracted = crate::library::extract::extract_page_text(
            &pdf.pages()[0],
            &hayro::hayro_interpret::InterpreterSettings::default(),
        );
        assert!(
            extracted.text.contains("scannedmarker"),
            "got: {}",
            extracted.text
        );
    }

    #[test]
    fn wrap_respects_width() {
        let lines = wrap_text("hello world this is a longer sentence", 12.0, 80.0);
        assert!(lines.len() > 1);
        for line in &lines {
            let w: f32 = line.chars().map(|c| char_width(c, 12.0)).sum();
            assert!(w <= 80.0 + 1e-3, "line too wide: {line}");
        }
    }
}
