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

    /// Whether the two rects share any area (touching edges count: a marquee
    /// dragged exactly along an annotation's edge did reach it).
    pub fn intersects(&self, other: Self) -> bool {
        self.min.x <= other.max.x
            && other.min.x <= self.max.x
            && self.min.y <= other.max.y
            && other.min.y <= self.max.y
    }

    /// Smallest rect containing both.
    pub fn union(&self, other: Self) -> Self {
        Self {
            min: PdfPoint::new(self.min.x.min(other.min.x), self.min.y.min(other.min.y)),
            max: PdfPoint::new(self.max.x.max(other.max.x), self.max.y.max(other.max.y)),
        }
    }

    pub fn expanded(&self, by: f32) -> Self {
        Self {
            min: PdfPoint::new(self.min.x - by, self.min.y - by),
            max: PdfPoint::new(self.max.x + by, self.max.y + by),
        }
    }
}

// ---------------------------------------------------------------------------
// Cloud scallops
// ---------------------------------------------------------------------------

/// One cubic Bézier segment of a cloud's outline, in PDF page space.
///
/// Every consumer of a cloud draws cubics -- egui's `CubicBezierShape`, the
/// PDF `c` operator, SVG's `C` command -- so the scallops are computed once,
/// here, and each of the three only has to write them out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CubicArc {
    pub from: PdfPoint,
    pub c1: PdfPoint,
    pub c2: PdfPoint,
    pub to: PdfPoint,
}

/// The narrowest and widest scallops a cloud may be drawn with. Matches the
/// `/BE /I` range Acrobat and Bluebeam write into a PDF.
pub const CLOUD_INTENSITY_MIN: f32 = 1.0;
pub const CLOUD_INTENSITY_MAX: f32 = 2.0;

pub fn clamp_cloud_intensity(intensity: f32) -> f32 {
    if intensity.is_nan() {
        return CLOUD_INTENSITY_MIN;
    }
    intensity.clamp(CLOUD_INTENSITY_MIN, CLOUD_INTENSITY_MAX)
}

/// Radius of one scallop, in points, for a given intensity.
pub fn cloud_radius(intensity: f32) -> f32 {
    5.0 * clamp_cloud_intensity(intensity)
}

/// How far apart scallop centres sit, as a fraction of the diameter. Below 1
/// so neighbouring bumps overlap and the outline reads as one cloud rather
/// than as a string of beads -- which is what Acrobat and Bluebeam draw.
const CLOUD_OVERLAP: f32 = 0.8;

/// The scalloped outline of the closed polygon through `points`, as cubics.
///
/// The bumps are laid out by arc length around the whole perimeter (not per
/// edge), so they stay evenly spaced across corners, and each one is a circular
/// arc of more than a half turn -- neighbours cross, which is the overlap that
/// makes a cloud. The returned arcs are a closed loop: each one starts where
/// the last ended, and the last ends where the first began.
///
/// Degenerate input (fewer than three distinct points, a zero-length
/// perimeter) yields no arcs rather than a panic: the caller draws nothing.
pub fn cloud_arcs(points: &[PdfPoint], intensity: f32) -> Vec<CubicArc> {
    let ring = closed_ring(points);
    if ring.len() < 2 {
        return Vec::new();
    }

    // Cumulative arc length around the closed ring.
    let mut lengths: Vec<f32> = Vec::with_capacity(ring.len());
    let mut perimeter = 0.0f32;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        let len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
        lengths.push(len);
        perimeter += len;
    }
    if !perimeter.is_finite() || perimeter <= 1e-3 {
        return Vec::new();
    }

    let radius = cloud_radius(intensity);
    let spacing = 2.0 * radius * CLOUD_OVERLAP;
    let count = ((perimeter / spacing).round() as i64).clamp(3, 4096) as usize;
    let step = perimeter / count as f32;

    // The joints between neighbouring bumps sit half a step past each bump's
    // centre and a little inside the outline, so the bumps cross there.
    let inset = (radius * radius - (step / 2.0) * (step / 2.0))
        .max(0.0)
        .sqrt();
    // +1 when the ring runs counter-clockwise. The interior is then to the
    // left of the direction of travel, which is what says which way is out.
    let wind = if signed_area(&ring) >= 0.0 { 1.0 } else { -1.0 };

    let joints: Vec<PdfPoint> = (0..count)
        .map(|i| {
            let (p, t) = walk(&ring, &lengths, (i as f32 + 0.5) * step);
            // Inward: the left-hand normal for a counter-clockwise ring.
            PdfPoint::new(p.x - wind * t.y * inset, p.y + wind * t.x * inset)
        })
        .collect();

    let mut arcs = Vec::with_capacity(count * 3);
    for i in 0..count {
        let a = joints[i];
        let b = joints[(i + 1) % count];
        push_bump(&mut arcs, a, b, radius, wind);
    }
    arcs
}

/// The polygon's points with consecutive duplicates (and a repeated closing
/// point) removed, so the arc-length walk never divides by zero.
fn closed_ring(points: &[PdfPoint]) -> Vec<PdfPoint> {
    let mut ring: Vec<PdfPoint> = Vec::with_capacity(points.len());
    for p in points {
        if !p.x.is_finite() || !p.y.is_finite() {
            continue;
        }
        if ring
            .last()
            .is_some_and(|last| (last.x - p.x).abs() < 1e-6 && (last.y - p.y).abs() < 1e-6)
        {
            continue;
        }
        ring.push(*p);
    }
    while ring.len() >= 2 {
        let (first, last) = (ring[0], ring[ring.len() - 1]);
        if (first.x - last.x).abs() < 1e-6 && (first.y - last.y).abs() < 1e-6 {
            ring.pop();
        } else {
            break;
        }
    }
    ring
}

/// Twice the signed area of the ring: positive counter-clockwise.
fn signed_area(ring: &[PdfPoint]) -> f32 {
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        sum += a.x * b.y - b.x * a.y;
    }
    sum
}

/// The point `distance` along the closed ring, and the unit direction of
/// travel there.
fn walk(ring: &[PdfPoint], lengths: &[f32], distance: f32) -> (PdfPoint, PdfPoint) {
    let mut left = distance;
    for i in 0..ring.len() {
        let len = lengths[i];
        if len <= f32::EPSILON {
            continue;
        }
        if left <= len {
            let a = ring[i];
            let b = ring[(i + 1) % ring.len()];
            let t = left / len;
            return (
                PdfPoint::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t),
                PdfPoint::new((b.x - a.x) / len, (b.y - a.y) / len),
            );
        }
        left -= len;
    }
    // Rounding overshot the last edge: the ring's start is where it wraps to.
    let a = ring[0];
    let b = ring[1 % ring.len()];
    let len = lengths[0].max(f32::EPSILON);
    (a, PdfPoint::new((b.x - a.x) / len, (b.y - a.y) / len))
}

/// Append the cubics of one scallop from `a` to `b`, bulging outward.
fn push_bump(out: &mut Vec<CubicArc>, a: PdfPoint, b: PdfPoint, radius: f32, wind: f32) {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let chord = (dx * dx + dy * dy).sqrt();
    if chord <= 1e-4 {
        return;
    }
    // The outward side of the chord, in the ring's own winding.
    let (ox, oy) = (wind * dy / chord, -wind * dx / chord);
    // A bump can never be flatter than a half circle on its own chord.
    let r = radius.max(chord / 2.0 * 1.0001);
    let sag = (r * r - (chord / 2.0) * (chord / 2.0)).max(0.0).sqrt();
    // The centre sits on the outward side of the chord -- it is a point of the
    // polygon's own perimeter -- so the long way round the circle is the part
    // that bulges out, and it is more than a half turn. That is the overlap.
    let center = PdfPoint::new((a.x + b.x) / 2.0 + ox * sag, (a.y + b.y) / 2.0 + oy * sag);

    let start = (a.y - center.y).atan2(a.x - center.x);
    let end = (b.y - center.y).atan2(b.x - center.x);
    // The major arc is the one that goes the long way round, through the
    // outward extreme -- that is the visible part of the scallop.
    let tau = std::f32::consts::TAU;
    let mut sweep = end - start;
    while sweep <= -std::f32::consts::PI {
        sweep += tau;
    }
    while sweep > std::f32::consts::PI {
        sweep -= tau;
    }
    sweep -= sweep.signum() * tau;

    let steps = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let delta = sweep / steps as f32;
    let k = 4.0 / 3.0 * (delta / 4.0).tan();
    let on = |angle: f32| PdfPoint::new(center.x + r * angle.cos(), center.y + r * angle.sin());
    for i in 0..steps {
        let t0 = start + delta * i as f32;
        let t1 = t0 + delta;
        // Exact endpoints at the joints keep the loop closed to the bit.
        let from = if i == 0 { a } else { on(t0) };
        let to = if i + 1 == steps { b } else { on(t1) };
        out.push(CubicArc {
            from,
            c1: PdfPoint::new(from.x - k * r * t0.sin(), from.y + k * r * t0.cos()),
            c2: PdfPoint::new(to.x + k * r * t1.sin(), to.y - k * r * t1.cos()),
            to,
        });
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

    fn square(size: f32) -> Vec<PdfPoint> {
        vec![
            PdfPoint::new(0.0, 0.0),
            PdfPoint::new(size, 0.0),
            PdfPoint::new(size, size),
            PdfPoint::new(0.0, size),
        ]
    }

    fn bounds(arcs: &[CubicArc]) -> PdfRect {
        let mut r = PdfRect::from_points(arcs[0].from, arcs[0].from);
        for arc in arcs {
            for p in [arc.from, arc.c1, arc.c2, arc.to] {
                r = r.union(PdfRect::from_points(p, p));
            }
        }
        r
    }

    /// A cloud is one path, not a scattering of bumps: every arc has to start
    /// where the last one ended, all the way round to the first.
    #[test]
    fn the_scallops_join_into_one_closed_loop() {
        let arcs = cloud_arcs(&square(200.0), 1.0);
        assert!(arcs.len() > 8, "only {} arcs", arcs.len());
        for pair in arcs.windows(2) {
            assert!(
                (pair[0].to.x - pair[1].from.x).abs() < 1e-3
                    && (pair[0].to.y - pair[1].from.y).abs() < 1e-3,
                "a gap between {:?} and {:?}",
                pair[0].to,
                pair[1].from
            );
        }
        let (first, last) = (arcs[0], arcs[arcs.len() - 1]);
        assert!(
            (last.to.x - first.from.x).abs() < 1e-3 && (last.to.y - first.from.y).abs() < 1e-3,
            "the loop does not close: {:?} != {:?}",
            last.to,
            first.from
        );
    }

    /// Bumps are a fixed size, so a bigger outline is more of them -- the way
    /// a cloud round a paragraph has more scallops than one round a word.
    #[test]
    fn a_longer_perimeter_is_more_scallops() {
        let small = cloud_arcs(&square(60.0), 1.0).len();
        let large = cloud_arcs(&square(240.0), 1.0).len();
        assert!(large > small * 2, "{small} then {large}");
    }

    /// Intensity is how far the scallops stand off the outline.
    #[test]
    fn intensity_widens_the_scallops() {
        let quiet = bounds(&cloud_arcs(&square(200.0), 1.0));
        let loud = bounds(&cloud_arcs(&square(200.0), 2.0));
        assert!(
            loud.width() > quiet.width() + 4.0,
            "{} then {}",
            quiet.width(),
            loud.width()
        );
        // And the range is the one a PDF viewer understands.
        assert_eq!(cloud_radius(0.1), cloud_radius(CLOUD_INTENSITY_MIN));
        assert_eq!(cloud_radius(99.0), cloud_radius(CLOUD_INTENSITY_MAX));
        assert_eq!(clamp_cloud_intensity(f32::NAN), CLOUD_INTENSITY_MIN);
    }

    /// Nothing a user can draw -- a two-point "polygon", a double-clicked
    /// vertex, a shape with no area -- may take the renderer down with it.
    #[test]
    fn degenerate_outlines_draw_nothing_rather_than_panicking() {
        assert!(cloud_arcs(&[], 1.0).is_empty());
        assert!(cloud_arcs(&[PdfPoint::new(3.0, 4.0)], 1.5).is_empty());
        assert!(
            cloud_arcs(&[PdfPoint::new(1.0, 1.0), PdfPoint::new(1.0, 1.0)], 1.0).is_empty(),
            "a repeated point has no perimeter"
        );
        assert!(
            cloud_arcs(&[PdfPoint::new(0.0, 0.0), PdfPoint::new(0.0, 1e-5)], 1.0).is_empty(),
            "a perimeter under a thousandth of a point is nothing to draw"
        );
        assert!(
            cloud_arcs(
                &[
                    PdfPoint::new(0.0, 0.0),
                    PdfPoint::new(f32::NAN, 2.0),
                    PdfPoint::new(50.0, 0.0),
                    PdfPoint::new(50.0, 50.0),
                ],
                1.0
            )
            .iter()
            .all(|a| a.from.x.is_finite() && a.to.y.is_finite()),
            "a point that is not a number is dropped, not propagated"
        );

        // A two-point line still has a perimeter (out and back), and drawing
        // it as a cloud is a thin sausage rather than a crash.
        let line = cloud_arcs(&[PdfPoint::new(0.0, 0.0), PdfPoint::new(100.0, 0.0)], 1.0);
        assert!(!line.is_empty());
        assert!(line.iter().all(|a| a.from.x.is_finite()));
    }

    /// The scallops belong outside the outline: that is what makes a revision
    /// cloud read as surrounding what it is about.
    #[test]
    fn the_bumps_stand_outside_the_outline() {
        for order in [square(100.0), square(100.0).into_iter().rev().collect()] {
            let arcs = cloud_arcs(&order, 1.0);
            let b = bounds(&arcs);
            assert!(b.min.x < -1.0 && b.min.y < -1.0, "{b:?}");
            assert!(b.max.x > 101.0 && b.max.y > 101.0, "{b:?}");
        }
    }
}
