//! Snapping engine: aligns dragged/resized geometry to the page center,
//! page edges, and other annotations, and reports guide lines to draw.

use crate::doc::geometry::PdfRect;

/// A guide line across the page, in PDF coordinates.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Guide {
    /// x = value
    Vertical(f32),
    /// y = value
    Horizontal(f32),
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SnapCorrection {
    pub dx: f32,
    pub dy: f32,
}

pub struct SnapResult {
    pub correction: SnapCorrection,
    pub guides: Vec<Guide>,
}

/// Which x/y features of the moving rect participate in snapping. A full drag
/// snaps edges and center; an edge resize snaps only the moving edge.
#[derive(Clone, Copy)]
pub struct SnapFeatures {
    pub left: bool,
    pub right: bool,
    pub center_x: bool,
    pub top: bool,
    pub bottom: bool,
    pub center_y: bool,
}

impl SnapFeatures {
    pub const ALL: Self = Self {
        left: true,
        right: true,
        center_x: true,
        top: true,
        bottom: true,
        center_y: true,
    };
}

/// Compute the snap correction for `rect` on a `page_w` x `page_h` page.
/// `others` are the bounds of other annotations on the same page.
/// `tolerance` is in PDF points (derive it from screen pixels / zoom).
pub fn snap_rect(
    rect: PdfRect,
    features: SnapFeatures,
    page_w: f32,
    page_h: f32,
    others: &[PdfRect],
    tolerance: f32,
) -> SnapResult {
    let mut x_candidates: Vec<f32> = vec![0.0, page_w / 2.0, page_w];
    let mut y_candidates: Vec<f32> = vec![0.0, page_h / 2.0, page_h];
    for o in others {
        x_candidates.extend([o.min.x, o.center().x, o.max.x]);
        y_candidates.extend([o.min.y, o.center().y, o.max.y]);
    }

    let mut best_x: Option<(f32, f32)> = None; // (correction, target)
    let mut best_y: Option<(f32, f32)> = None;

    let consider_x = |value: f32, best: &mut Option<(f32, f32)>| {
        for &c in &x_candidates {
            let d = c - value;
            if d.abs() <= tolerance && best.is_none_or(|(bd, _)| d.abs() < bd.abs()) {
                *best = Some((d, c));
            }
        }
    };
    if features.left {
        consider_x(rect.min.x, &mut best_x);
    }
    if features.right {
        consider_x(rect.max.x, &mut best_x);
    }
    if features.center_x {
        consider_x(rect.center().x, &mut best_x);
    }

    let consider_y = |value: f32, best: &mut Option<(f32, f32)>| {
        for &c in &y_candidates {
            let d = c - value;
            if d.abs() <= tolerance && best.is_none_or(|(bd, _)| d.abs() < bd.abs()) {
                *best = Some((d, c));
            }
        }
    };
    if features.bottom {
        consider_y(rect.min.y, &mut best_y);
    }
    if features.top {
        consider_y(rect.max.y, &mut best_y);
    }
    if features.center_y {
        consider_y(rect.center().y, &mut best_y);
    }

    let mut guides = Vec::new();
    let mut correction = SnapCorrection::default();
    if let Some((d, target)) = best_x {
        correction.dx = d;
        guides.push(Guide::Vertical(target));
    }
    if let Some((d, target)) = best_y {
        correction.dy = d;
        guides.push(Guide::Horizontal(target));
    }
    SnapResult { correction, guides }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::geometry::PdfPoint;

    #[test]
    fn snaps_center_to_page_center() {
        // Page 100x100; rect centered at (48, 50) should snap dx=+2 to center.
        let rect = PdfRect::from_points(PdfPoint::new(38.0, 40.0), PdfPoint::new(58.0, 60.0));
        let result = snap_rect(rect, SnapFeatures::ALL, 100.0, 100.0, &[], 3.0);
        assert!((result.correction.dx - 2.0).abs() < 1e-4);
        assert!(result.guides.contains(&Guide::Vertical(50.0)));
    }

    #[test]
    fn out_of_tolerance_does_not_snap() {
        let rect = PdfRect::from_points(PdfPoint::new(10.0, 10.0), PdfPoint::new(20.0, 20.0));
        let result = snap_rect(rect, SnapFeatures::ALL, 100.0, 100.0, &[], 1.0);
        assert_eq!(result.guides.len(), 0);
    }
}
