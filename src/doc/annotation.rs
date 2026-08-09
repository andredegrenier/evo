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
    /// A rubber stamp: a word in a box, the way a drawing gets marked
    /// APPROVED. The dynamic tokens (`%date`, `%user`, `%filename`) are
    /// expanded when the stamp is placed, so a stamp says the same thing in a
    /// year as it said the day it was applied -- which is the only thing a
    /// record of approval may do.
    Stamp {
        text: String,
        font_size: f32,
    },
    /// A picture stamp: a PNG placed on the page, for a signature or a company
    /// mark. The bytes travel inside the markup, so a sidecar carries the whole
    /// stamp and a phone gets it for free.
    ImageStamp {
        #[serde(with = "png_base64")]
        png: Vec<u8>,
    },
}

/// PNG bytes as base64 in JSON.
///
/// The sidecar is read by hand, by `curl`, and by a browser; a byte array of
/// four thousand numbers is none of those things' idea of a picture. Base64 is
/// what every other format puts an image in, and it is what `data:` URLs --
/// which is how the phone overlay draws these -- already speak.
mod png_base64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(serde::de::Error::custom)
    }
}

/// The stamps every reviewer expects to find already made, paired with the
/// `/Name` ISO 32000-1 §12.5.6.12 gives them. A stamp whose text is one of
/// these leaves as a named standard stamp; anything else leaves as a stamp
/// with an appearance and no name, which is the same thing minus the label.
pub const STANDARD_STAMPS: [(&str, &str); 6] = [
    ("APPROVED", "Approved"),
    ("NOT APPROVED", "NotApproved"),
    ("DRAFT", "Draft"),
    ("FINAL", "Final"),
    ("CONFIDENTIAL", "Confidential"),
    ("FOR COMMENT", "ForComment"),
];

/// The `/Name` for a stamp reading `text`, if it reads as one of the standards.
pub fn standard_stamp_name(text: &str) -> Option<&'static str> {
    let wanted = text.trim();
    STANDARD_STAMPS
        .iter()
        .find(|(label, _)| label.eq_ignore_ascii_case(wanted))
        .map(|(_, name)| *name)
}

/// The tokens a stamp's text may carry, and what each one stands for. Shown in
/// the stamp popover, because a token nobody is told about is a typo.
pub const STAMP_TOKENS: [(&str, &str); 3] = [
    ("%date", "today's date"),
    ("%user", "the name you are signed in as"),
    ("%filename", "the document's title"),
];

/// Replace the dynamic tokens in a stamp's text with what they stand for
/// *now*.
///
/// Called once, when the stamp is placed. A stamp that re-evaluated its date
/// every time it was drawn would be a stamp that quietly rewrites when a
/// document was approved, which is worse than useless on a drawing set.
pub fn expand_stamp_tokens(text: &str, filename: &str) -> String {
    text.replace("%date", &today())
        .replace("%user", &current_user())
        .replace("%filename", filename)
}

/// Today, as `YYYY-MM-DD` in UTC.
///
/// Days-to-civil-date arithmetic rather than a calendar crate: this is the only
/// date evo ever formats, and the algorithm is older than any of the crates
/// that would do it (Howard Hinnant's `civil_from_days`).
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Whoever is at the keyboard, as the operating system knows them.
fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
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
            AnnotationKind::Polygon { .. }
            | AnnotationKind::PolyLine { .. }
            | AnnotationKind::Stamp { .. }
            | AnnotationKind::ImageStamp { .. } => 2,
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
            AnnotationKind::Stamp { .. } => "stamp",
            AnnotationKind::ImageStamp { .. } => "image stamp",
        }
    }

    /// Whether a resize should hold the shape's proportions unless told
    /// otherwise. A stretched signature is a forged one, so a picture keeps its
    /// aspect ratio by default and shift is what lets go of it -- the opposite
    /// of every other shape, and the right way round for this one.
    pub fn keeps_aspect(&self) -> bool {
        matches!(self, AnnotationKind::ImageStamp { .. })
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
    /// Which group this annotation belongs to, if any. Members of a group are
    /// selected, moved and deleted together.
    ///
    /// Additive, and absent from the JSON when there is none: a sidecar full of
    /// ungrouped markup is byte-for-byte what version 2 already was, so no
    /// reader has to learn anything to keep reading it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupId>,
}

/// Identifies a group within one document's markup. Allocated from what is
/// already there, so an id never collides with one already in use.
pub type GroupId = u64;

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
            group: None,
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
            group: None,
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

    /// The PNG inside an image stamp has to survive the sidecar exactly, and
    /// it has to go through it as base64 rather than as four thousand numbers
    /// -- the format is read by people and by `curl`, and a `data:` URL is how
    /// the phone overlay ends up drawing it.
    #[test]
    fn an_image_stamp_carries_its_png_through_json_as_base64() {
        let png = b"\x89PNG\r\n\x1a\n0123456789".to_vec();
        let ann = with_kind(AnnotationKind::ImageStamp { png: png.clone() });
        let json = serde_json::to_string(&ann).expect("serialize");
        assert!(
            json.contains("\"ImageStamp\":{\"png\":\"iVBORw0KGgowMTIzNDU2Nzg5\"}"),
            "{json}"
        );
        let back: Annotation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ann);

        // Bytes that are not base64 are a broken sidecar, not silent emptiness.
        let broken = json.replace("iVBORw0KGgowMTIzNDU2Nzg5", "not base64!!");
        assert!(
            serde_json::from_str::<Annotation>(&broken).is_err(),
            "{broken}"
        );
    }

    #[test]
    fn a_stamp_survives_a_json_round_trip() {
        let ann = with_kind(AnnotationKind::Stamp {
            text: "APPROVED".into(),
            font_size: 24.0,
        });
        let json = serde_json::to_string(&ann).expect("serialize");
        assert!(
            json.contains("\"Stamp\":{\"text\":\"APPROVED\",\"font_size\":24.0}"),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<Annotation>(&json).expect("deserialize"),
            ann
        );
    }

    /// The standard stamps are matched by what they say, however it is typed;
    /// anything else is a stamp with no standard name, which is fine.
    #[test]
    fn the_standard_stamps_are_recognized_by_their_words() {
        assert_eq!(standard_stamp_name("APPROVED"), Some("Approved"));
        assert_eq!(standard_stamp_name("  approved "), Some("Approved"));
        assert_eq!(standard_stamp_name("Not Approved"), Some("NotApproved"));
        assert_eq!(standard_stamp_name("For Comment"), Some("ForComment"));
        assert_eq!(standard_stamp_name("Reviewed by me"), None);
        assert_eq!(standard_stamp_name(""), None);
    }

    /// A stamp is baked when it is placed: what it says is fixed from then on,
    /// so the tokens have to be gone by the time it is stored.
    #[test]
    fn the_dynamic_tokens_are_expanded_once_and_leave_nothing_behind() {
        // SAFETY: single-threaded test setup, before any thread is spawned.
        unsafe { std::env::set_var("USER", "ada") };
        let out = expand_stamp_tokens("%user %filename %date", "plans.pdf");
        assert!(out.starts_with("ada plans.pdf "), "{out}");
        assert!(!out.contains('%'), "{out}");
        let date = out.rsplit(' ').next().expect("a date");
        assert_eq!(date.len(), 10, "{date}");
        assert_eq!(date.matches('-').count(), 2, "{date}");

        // Text with no tokens is left exactly as it was typed.
        assert_eq!(expand_stamp_tokens("100% done", "x.pdf"), "100% done");
    }

    /// The date arithmetic is written out here rather than pulled in, so the
    /// days it gets wrong are the ones worth pinning: epoch, and a leap day.
    #[test]
    fn the_calendar_arithmetic_lands_on_the_right_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29), "a leap day");
        assert_eq!(civil_from_days(20_671), (2026, 8, 6));
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
        assert_eq!(
            AnnotationKind::Stamp {
                text: "DRAFT".into(),
                font_size: 20.0
            }
            .min_sidecar_version(),
            2
        );
        assert_eq!(
            AnnotationKind::ImageStamp { png: Vec::new() }.min_sidecar_version(),
            2
        );
    }
}
