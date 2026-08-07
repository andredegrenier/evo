//! Geometry primitives in PDF page space.
//!
//! Everything in the editor (annotations, hit-testing, the inspector, export)
//! works in one canonical coordinate space: **PDF points, y-up**, in the
//! page's *displayed* orientation as rendered by hayro (i.e. the intrinsic
//! `/Rotate` of the page is already applied; the origin is the bottom-left
//! corner of the visible page). User-applied rotation from page ops is handled
//! separately by [`PageTransform`].

use eframe::emath::{Pos2, Rect, Vec2};
use serde::{Deserialize, Serialize};

/// A point in PDF page space (points, y-up).
#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PdfPoint {
    pub x: f32,
    pub y: f32,
}

impl PdfPoint {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle in PDF page space, kept normalized (min <= max).
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct PdfRect {
    pub min: PdfPoint,
    pub max: PdfPoint,
}

impl PdfRect {
    pub fn from_points(a: PdfPoint, b: PdfPoint) -> Self {
        Self {
            min: PdfPoint::new(a.x.min(b.x), a.y.min(b.y)),
            max: PdfPoint::new(a.x.max(b.x), a.y.max(b.y)),
        }
    }

    pub fn from_min_size(min: PdfPoint, w: f32, h: f32) -> Self {
        Self::from_points(min, PdfPoint::new(min.x + w, min.y + h))
    }

    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    pub fn center(&self) -> PdfPoint {
        PdfPoint::new(
            (self.min.x + self.max.x) / 2.0,
            (self.min.y + self.max.y) / 2.0,
        )
    }

    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        Self {
            min: PdfPoint::new(self.min.x + dx, self.min.y + dy),
            max: PdfPoint::new(self.max.x + dx, self.max.y + dy),
        }
    }

    pub fn contains(&self, p: PdfPoint) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    pub fn expanded(&self, by: f32) -> Self {
        Self {
            min: PdfPoint::new(self.min.x - by, self.min.y - by),
            max: PdfPoint::new(self.max.x + by, self.max.y + by),
        }
    }
}

/// User-applied page rotation, clockwise, on top of the intrinsic `/Rotate`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ExtraRotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl ExtraRotation {
    pub fn degrees(self) -> i64 {
        match self {
            ExtraRotation::None => 0,
            ExtraRotation::Cw90 => 90,
            ExtraRotation::Cw180 => 180,
            ExtraRotation::Cw270 => 270,
        }
    }

    pub fn rotated_cw(self) -> Self {
        match self {
            ExtraRotation::None => ExtraRotation::Cw90,
            ExtraRotation::Cw90 => ExtraRotation::Cw180,
            ExtraRotation::Cw180 => ExtraRotation::Cw270,
            ExtraRotation::Cw270 => ExtraRotation::None,
        }
    }

    pub fn rotated_ccw(self) -> Self {
        self.rotated_cw().rotated_cw().rotated_cw()
    }

    pub fn swaps_axes(self) -> bool {
        matches!(self, ExtraRotation::Cw90 | ExtraRotation::Cw270)
    }
}

/// Maps between PDF page space and screen space for one page as currently
/// laid out on the canvas (zoom, scroll position, user rotation).
#[derive(Clone, Copy, Debug)]
pub struct PageTransform {
    /// Where the (possibly user-rotated) page sits on screen, in egui points.
    pub screen_rect: Rect,
    /// Page size in PDF points, before user rotation.
    pub page_w: f32,
    pub page_h: f32,
    pub rotation: ExtraRotation,
    /// Screen points per PDF point.
    pub zoom: f32,
}

#[allow(clippy::wrong_self_convention)]
impl PageTransform {
    /// PDF page point -> display coordinates (y-down, origin at the top-left
    /// of the rotated page, still in PDF points).
    fn to_display(&self, p: PdfPoint) -> Vec2 {
        let (w, h) = (self.page_w, self.page_h);
        match self.rotation {
            ExtraRotation::None => Vec2::new(p.x, h - p.y),
            ExtraRotation::Cw90 => Vec2::new(p.y, p.x),
            ExtraRotation::Cw180 => Vec2::new(w - p.x, p.y),
            ExtraRotation::Cw270 => Vec2::new(h - p.y, w - p.x),
        }
    }

    fn from_display(&self, d: Vec2) -> PdfPoint {
        let (w, h) = (self.page_w, self.page_h);
        match self.rotation {
            ExtraRotation::None => PdfPoint::new(d.x, h - d.y),
            ExtraRotation::Cw90 => PdfPoint::new(d.y, d.x),
            ExtraRotation::Cw180 => PdfPoint::new(w - d.x, d.y),
            ExtraRotation::Cw270 => PdfPoint::new(w - d.y, h - d.x),
        }
    }

    pub fn to_screen(&self, p: PdfPoint) -> Pos2 {
        self.screen_rect.min + self.to_display(p) * self.zoom
    }

    pub fn from_screen(&self, pos: Pos2) -> PdfPoint {
        self.from_display((pos - self.screen_rect.min) / self.zoom)
    }

    /// Axis-aligned PDF rect -> axis-aligned screen rect (rotation is always a
    /// multiple of 90 degrees, so rectangles stay axis-aligned).
    pub fn rect_to_screen(&self, r: PdfRect) -> Rect {
        let a = self.to_screen(r.min);
        let b = self.to_screen(r.max);
        Rect::from_two_pos(a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::emath::pos2;

    fn transform(rotation: ExtraRotation) -> PageTransform {
        PageTransform {
            screen_rect: Rect::from_min_size(pos2(10.0, 20.0), Vec2::new(0.0, 0.0)),
            page_w: 612.0,
            page_h: 792.0,
            rotation,
            zoom: 2.0,
        }
    }

    #[test]
    fn round_trips_at_all_rotations() {
        for rotation in [
            ExtraRotation::None,
            ExtraRotation::Cw90,
            ExtraRotation::Cw180,
            ExtraRotation::Cw270,
        ] {
            let t = transform(rotation);
            let p = PdfPoint::new(100.0, 250.0);
            let back = t.from_screen(t.to_screen(p));
            assert!((back.x - p.x).abs() < 1e-3 && (back.y - p.y).abs() < 1e-3);
        }
    }

    #[test]
    fn origin_maps_to_bottom_left_unrotated() {
        let t = transform(ExtraRotation::None);
        let s = t.to_screen(PdfPoint::new(0.0, 0.0));
        assert_eq!(s, pos2(10.0, 20.0 + 792.0 * 2.0));
    }
}
