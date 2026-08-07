//! Freehand stroke helpers.

use crate::doc::geometry::{PdfPoint, PdfRect};

/// Ramer-Douglas-Peucker polyline simplification. `epsilon` in PDF points.
pub fn simplify(points: &[PdfPoint], epsilon: f32) -> Vec<PdfPoint> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let (first, last) = (points[0], points[points.len() - 1]);
    let mut max_dist = 0.0f32;
    let mut max_idx = 0;
    for (i, p) in points.iter().enumerate().skip(1).take(points.len() - 2) {
        let d = perpendicular_distance(*p, first, last);
        if d > max_dist {
            max_dist = d;
            max_idx = i;
        }
    }
    if max_dist > epsilon {
        let mut left = simplify(&points[..=max_idx], epsilon);
        let right = simplify(&points[max_idx..], epsilon);
        left.pop();
        left.extend(right);
        left
    } else {
        vec![first, last]
    }
}

fn perpendicular_distance(p: PdfPoint, a: PdfPoint, b: PdfPoint) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len <= f32::EPSILON {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    ((dy * p.x - dx * p.y + b.x * a.y - b.y * a.x) / len).abs()
}

pub fn bounding_rect(points: &[PdfPoint]) -> PdfRect {
    let mut min = PdfPoint::new(f32::MAX, f32::MAX);
    let mut max = PdfPoint::new(f32::MIN, f32::MIN);
    for p in points {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    PdfRect::from_points(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_line_collapses_to_endpoints() {
        let pts: Vec<PdfPoint> = (0..50).map(|i| PdfPoint::new(i as f32, 0.01)).collect();
        let out = simplify(&pts, 0.5);
        assert_eq!(out.len(), 2);
    }
}
