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
            AnnotationKind::Freehand { points } => {
                for p in points {
                    p.x += dx;
                    p.y += dy;
                }
            }
            _ => {}
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
            AnnotationKind::Freehand { points } => points.iter_mut().for_each(map),
            _ => {}
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
}
