//! Markup annotations. These live in a layer separate from the source PDF and
//! are only written into a real PDF at export time.

use serde::{Deserialize, Serialize};

use super::geometry::{PdfPoint, PdfRect};

pub type AnnotationId = u64;

/// RGBA color, straight (not premultiplied), 0-255.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);

    pub fn is_visible(&self) -> bool {
        self.a > 0
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Style {
    pub stroke: Color,
    pub stroke_width: f32,
    pub fill: Color,
    /// Overall opacity multiplier, 0.0-1.0 (PDF `/CA`).
    pub opacity: f32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            stroke: Color::rgb(220, 38, 38),
            stroke_width: 2.0,
            fill: Color::TRANSPARENT,
            opacity: 1.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum AnnotationKind {
    /// Semi-transparent marker rectangle.
    Highlight,
    /// A text box; `rect` is the box, text flows from its top-left.
    TextBox {
        text: String,
        font_size: f32,
        align: TextAlign,
    },
    Rect,
    Ellipse,
    /// Line from `p1` to `p2` (both are also encoded by `rect`'s corners; the
    /// explicit points preserve direction).
    Line {
        p1: PdfPoint,
        p2: PdfPoint,
        arrow_end: bool,
    },
    /// Freehand pen stroke.
    Freehand {
        points: Vec<PdfPoint>,
    },
    /// Closed outline through `points`. `cloudy` turns it into a revision
    /// cloud: the scallop intensity, in the 1.0-2.0 range PDF viewers know
    /// (`/BE /I`). The Cloud tool is a rectangular cloudy polygon.
    Polygon {
        points: Vec<PdfPoint>,
        cloudy: Option<f32>,
    },
    /// Open chain of straight segments, optionally arrow-headed at the end.
    PolyLine {
        points: Vec<PdfPoint>,
        arrow_end: bool,
    },
}

impl AnnotationKind {
    /// The oldest sidecar version that can carry this kind.
    ///
    /// Markup travels: a phone still running last week's app, an agent holding
    /// a copy it read an hour ago. A writer that claims version 1 has to be
    /// held to what version 1 could describe, or a `Polygon` would be written
    /// into a file older readers will choke on.
    pub fn min_sidecar_version(&self) -> u32 {
        match self {
            AnnotationKind::Highlight
            | AnnotationKind::TextBox { .. }
            | AnnotationKind::Rect
            | AnnotationKind::Ellipse
            | AnnotationKind::Line { .. }
            | AnnotationKind::Freehand { .. } => 1,
            AnnotationKind::Polygon { .. } | AnnotationKind::PolyLine { .. } => 2,
        }
    }

    /// The name this kind goes by in messages to people.
    pub fn label(&self) -> &'static str {
        match self {
            AnnotationKind::Highlight => "highlight",
            AnnotationKind::TextBox { .. } => "text box",
            AnnotationKind::Rect => "rectangle",
            AnnotationKind::Ellipse => "ellipse",
            AnnotationKind::Line { .. } => "line",
            AnnotationKind::Freehand { .. } => "pen stroke",
            AnnotationKind::Polygon {
                cloudy: Some(_), ..
            } => "cloud",
            AnnotationKind::Polygon { .. } => "polygon",
            AnnotationKind::PolyLine { .. } => "polyline",
        }
    }

    /// The interior points a move or a resize has to carry along, if any.
    pub fn points_mut(&mut self) -> Option<&mut Vec<PdfPoint>> {
        match self {
            AnnotationKind::Freehand { points }
            | AnnotationKind::Polygon { points, .. }
            | AnnotationKind::PolyLine { points, .. } => Some(points),
            _ => None,
        }
    }

    /// The interior points, if this kind has any.
    pub fn points(&self) -> Option<&[PdfPoint]> {
        match self {
            AnnotationKind::Freehand { points }
            | AnnotationKind::Polygon { points, .. }
            | AnnotationKind::PolyLine { points, .. } => Some(points),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub id: AnnotationId,
    /// Index into the ORIGINAL document's pages (stable across page reorder).
    pub page: usize,
    pub kind: AnnotationKind,
    pub rect: PdfRect,
    pub style: Style,
}

impl Annotation {
    /// Bounds used for hit-testing and selection handles.
    pub fn bounds(&self) -> PdfRect {
        self.rect
    }

    /// Move the annotation by (dx, dy) points, carrying interior geometry along.
    pub fn translate(&mut self, dx: f32, dy: f32) {
        self.rect = self.rect.translated(dx, dy);
        match &mut self.kind {
            AnnotationKind::Line { p1, p2, .. } => {
                p1.x += dx;
                p1.y += dy;
                p2.x += dx;
                p2.y += dy;
            }
            kind => {
                if let Some(points) = kind.points_mut() {
                    for p in points {
                        p.x += dx;
                        p.y += dy;
                    }
                }
            }
        }
    }

    /// Set new bounds, scaling interior geometry (line endpoints, pen points)
    /// to fit. `new` is normalized.
    pub fn set_bounds(&mut self, new: PdfRect) {
        let old = self.rect;
        let sx = if old.width() > f32::EPSILON {
            new.width() / old.width()
        } else {
            1.0
        };
        let sy = if old.height() > f32::EPSILON {
            new.height() / old.height()
        } else {
            1.0
        };
        let map = |p: &mut PdfPoint| {
            p.x = new.min.x + (p.x - old.min.x) * sx;
            p.y = new.min.y + (p.y - old.min.y) * sy;
        };
        match &mut self.kind {
            AnnotationKind::Line { p1, p2, .. } => {
                map(p1);
                map(p2);
            }
            kind => {
                if let Some(points) = kind.points_mut() {
                    points.iter_mut().for_each(map);
                }
            }
        }
        self.rect = new;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_scales_line_endpoints() {
        let mut ann = Annotation {
            id: 1,
            page: 0,
            kind: AnnotationKind::Line {
                p1: PdfPoint::new(0.0, 0.0),
                p2: PdfPoint::new(100.0, 50.0),
                arrow_end: false,
            },
            rect: PdfRect::from_points(PdfPoint::new(0.0, 0.0), PdfPoint::new(100.0, 50.0)),
            style: Style::default(),
        };
        ann.set_bounds(PdfRect::from_points(
            PdfPoint::new(10.0, 10.0),
            PdfPoint::new(210.0, 60.0),
        ));
        if let AnnotationKind::Line { p1, p2, .. } = ann.kind {
            assert_eq!((p1.x, p1.y), (10.0, 10.0));
            assert_eq!((p2.x, p2.y), (210.0, 60.0));
        } else {
            unreachable!()
        }
    }

    fn triangle() -> Vec<PdfPoint> {
        vec![
            PdfPoint::new(0.0, 0.0),
            PdfPoint::new(100.0, 0.0),
            PdfPoint::new(50.0, 80.0),
        ]
    }

    fn with_kind(kind: AnnotationKind) -> Annotation {
        let rect = match kind.points() {
            Some(points) => points
                .iter()
                .skip(1)
                .fold(PdfRect::from_points(points[0], points[0]), |r, p| {
                    r.union(PdfRect::from_points(*p, *p))
                }),
            None => PdfRect::from_points(PdfPoint::new(0.0, 0.0), PdfPoint::new(10.0, 10.0)),
        };
        Annotation {
            id: 7,
            page: 1,
            kind,
            rect,
            style: Style::default(),
        }
    }

    /// Moving a point-list annotation moves its points with it -- otherwise a
    /// dragged polygon would leave its outline behind and only take its box.
    #[test]
    fn moving_and_resizing_carry_the_point_lists() {
        for kind in [
            AnnotationKind::Polygon {
                points: triangle(),
                cloudy: Some(1.5),
            },
            AnnotationKind::PolyLine {
                points: triangle(),
                arrow_end: true,
            },
        ] {
            let mut ann = with_kind(kind);
            ann.translate(10.0, -5.0);
            let moved = ann.kind.points().expect("points").to_vec();
            assert_eq!((moved[0].x, moved[0].y), (10.0, -5.0));
            assert_eq!((moved[1].x, moved[1].y), (110.0, -5.0));
            assert_eq!(ann.rect.min.x, 10.0);

            // Twice as wide: the far vertex has to double its offset too.
            let doubled =
                PdfRect::from_min_size(ann.rect.min, ann.rect.width() * 2.0, ann.rect.height());
            ann.set_bounds(doubled);
            let scaled = ann.kind.points().expect("points").to_vec();
            assert!((scaled[1].x - 210.0).abs() < 1e-3, "{:?}", scaled[1]);
            assert!((scaled[2].x - 110.0).abs() < 1e-3, "{:?}", scaled[2]);
        }
    }

    /// The sidecar is the format two evos and a phone agree on, so what goes
    /// out has to be exactly what comes back.
    #[test]
    fn the_new_kinds_survive_a_json_round_trip() {
        for kind in [
            AnnotationKind::Polygon {
                points: triangle(),
                cloudy: None,
            },
            AnnotationKind::Polygon {
                points: triangle(),
                cloudy: Some(2.0),
            },
            AnnotationKind::PolyLine {
                points: triangle(),
                arrow_end: false,
            },
        ] {
            let ann = with_kind(kind);
            let json = serde_json::to_string(&ann).expect("serialize");
            let back: Annotation = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, ann, "{json}");
        }

        // And the shape of it is the one viewer.js and the API tests write by
        // hand, so it is worth pinning.
        let json = serde_json::to_string(&with_kind(AnnotationKind::Polygon {
            points: vec![PdfPoint::new(1.0, 2.0)],
            cloudy: Some(1.0),
        }))
        .expect("serialize");
        assert!(
            json.contains("\"Polygon\":{\"points\":[{\"x\":1.0,\"y\":2.0}],\"cloudy\":1.0}"),
            "{json}"
        );
    }

    /// Old readers cannot be handed shapes they have never heard of.
    #[test]
    fn only_the_new_kinds_need_the_new_sidecar_version() {
        assert_eq!(AnnotationKind::Highlight.min_sidecar_version(), 1);
        assert_eq!(
            AnnotationKind::Freehand { points: triangle() }.min_sidecar_version(),
            1
        );
        assert_eq!(
            AnnotationKind::Polygon {
                points: triangle(),
                cloudy: None
            }
            .min_sidecar_version(),
            2
        );
        assert_eq!(
            AnnotationKind::PolyLine {
                points: triangle(),
                arrow_end: false
            }
            .min_sidecar_version(),
            2
        );
    }
}
