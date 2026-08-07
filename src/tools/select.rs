//! Hit-testing and resize-handle math for the select tool.

use crate::doc::annotation::{Annotation, AnnotationId, AnnotationKind};
use crate::doc::geometry::{PdfPoint, PdfRect};
use crate::doc::store::AnnotationStore;

/// The eight rect handles plus line endpoint handles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
    LineStart,
    LineEnd,
}

impl Handle {
    pub const RECT_HANDLES: [Handle; 8] = [
        Handle::Nw,
        Handle::N,
        Handle::Ne,
        Handle::E,
        Handle::Se,
        Handle::S,
        Handle::Sw,
        Handle::W,
    ];

    /// Handle position on a rect, in PDF coords (N = top = max.y).
    pub fn anchor(self, r: PdfRect) -> PdfPoint {
        let c = r.center();
        match self {
            Handle::Nw => PdfPoint::new(r.min.x, r.max.y),
            Handle::N => PdfPoint::new(c.x, r.max.y),
            Handle::Ne => PdfPoint::new(r.max.x, r.max.y),
            Handle::E => PdfPoint::new(r.max.x, c.y),
            Handle::Se => PdfPoint::new(r.max.x, r.min.y),
            Handle::S => PdfPoint::new(c.x, r.min.y),
            Handle::Sw => PdfPoint::new(r.min.x, r.min.y),
            Handle::W => PdfPoint::new(r.min.x, c.y),
            Handle::LineStart | Handle::LineEnd => c,
        }
    }

    pub fn moves_left(self) -> bool {
        matches!(self, Handle::Nw | Handle::W | Handle::Sw)
    }

    pub fn moves_right(self) -> bool {
        matches!(self, Handle::Ne | Handle::E | Handle::Se)
    }

    pub fn moves_top(self) -> bool {
        matches!(self, Handle::Nw | Handle::N | Handle::Ne)
    }

    pub fn moves_bottom(self) -> bool {
        matches!(self, Handle::Sw | Handle::S | Handle::Se)
    }
}

fn dist_to_segment(p: PdfPoint, a: PdfPoint, b: PdfPoint) -> f32 {
    let (vx, vy) = (b.x - a.x, b.y - a.y);
    let len2 = vx * vx + vy * vy;
    if len2 <= f32::EPSILON {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = (((p.x - a.x) * vx + (p.y - a.y) * vy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a.x + t * vx, a.y + t * vy);
    ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt()
}

fn hits(ann: &Annotation, pos: PdfPoint, tol: f32) -> bool {
    match &ann.kind {
        AnnotationKind::Line { p1, p2, .. } => dist_to_segment(pos, *p1, *p2) <= tol,
        AnnotationKind::Freehand { points } => points
            .windows(2)
            .any(|w| dist_to_segment(pos, w[0], w[1]) <= tol),
        _ => ann.rect.expanded(tol).contains(pos),
    }
}

/// Topmost annotation on `page` under `pos`, if any. `tol` in PDF points.
pub fn hit_test(
    store: &AnnotationStore,
    page: usize,
    pos: PdfPoint,
    tol: f32,
) -> Option<AnnotationId> {
    store
        .on_page(page)
        .filter(|a| hits(a, pos, tol))
        .map(|a| a.id)
        .last()
}

/// Handle under `pos` for the selected annotation, if any.
pub fn handle_at(ann: &Annotation, pos: PdfPoint, tol: f32) -> Option<Handle> {
    let near = |h: PdfPoint| (pos.x - h.x).abs() <= tol && (pos.y - h.y).abs() <= tol;
    if let AnnotationKind::Line { p1, p2, .. } = &ann.kind {
        if near(*p1) {
            return Some(Handle::LineStart);
        }
        if near(*p2) {
            return Some(Handle::LineEnd);
        }
        return None;
    }
    Handle::RECT_HANDLES
        .into_iter()
        .find(|h| near(h.anchor(ann.rect)))
}

/// New bounds when dragging `handle` of `orig` to `pos`.
/// `lock_aspect` keeps the original aspect ratio (corner handles only).
pub fn resize_rect(orig: PdfRect, handle: Handle, pos: PdfPoint, lock_aspect: bool) -> PdfRect {
    let mut min = orig.min;
    let mut max = orig.max;
    if handle.moves_left() {
        min.x = pos.x;
    }
    if handle.moves_right() {
        max.x = pos.x;
    }
    if handle.moves_bottom() {
        min.y = pos.y;
    }
    if handle.moves_top() {
        max.y = pos.y;
    }

    if lock_aspect && orig.width() > f32::EPSILON && orig.height() > f32::EPSILON {
        let aspect = orig.width() / orig.height();
        let is_corner = matches!(handle, Handle::Nw | Handle::Ne | Handle::Se | Handle::Sw);
        if is_corner {
            let w = (max.x - min.x).abs();
            let h = (max.y - min.y).abs();
            // Follow the dominant axis of the drag.
            if w / aspect >= h {
                let new_h = w / aspect;
                if handle.moves_top() {
                    max.y = min.y + new_h;
                } else {
                    min.y = max.y - new_h;
                }
            } else {
                let new_w = h * aspect;
                if handle.moves_left() {
                    min.x = max.x - new_w;
                } else {
                    max.x = min.x + new_w;
                }
            }
        }
    }
    PdfRect::from_points(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::annotation::Style;

    #[test]
    fn resize_se_corner() {
        let orig = PdfRect::from_points(PdfPoint::new(0.0, 0.0), PdfPoint::new(100.0, 50.0));
        let out = resize_rect(orig, Handle::Se, PdfPoint::new(120.0, -10.0), false);
        assert_eq!(out.max.x, 120.0);
        assert_eq!(out.min.y, -10.0);
        assert_eq!(out.max.y, 50.0);
    }

    #[test]
    fn aspect_lock_preserves_ratio() {
        let orig = PdfRect::from_points(PdfPoint::new(0.0, 0.0), PdfPoint::new(100.0, 50.0));
        let out = resize_rect(orig, Handle::Se, PdfPoint::new(200.0, 0.0), true);
        let ratio = out.width() / out.height();
        assert!((ratio - 2.0).abs() < 1e-3, "ratio was {ratio}");
    }

    #[test]
    fn hit_test_prefers_topmost() {
        let mut store = AnnotationStore::default();
        for id in [1, 2] {
            store.insert(Annotation {
                id,
                page: 0,
                kind: AnnotationKind::Rect,
                rect: PdfRect::from_points(PdfPoint::new(0.0, 0.0), PdfPoint::new(50.0, 50.0)),
                style: Style::default(),
            });
        }
        assert_eq!(hit_test(&store, 0, PdfPoint::new(25.0, 25.0), 2.0), Some(2));
    }
}
