//! The crossing between the async MCP server and the synchronous app.
//!
//! A tool body puts an [`AppCommand`] on a channel, carrying a one-shot sender
//! for its answer, and awaits it. The UI thread drains the channel at the top
//! of every frame and answers with `&mut self` in hand, so a tool reaches the
//! open document, the library and the undo history without any of them having
//! to become thread-safe.
//!
//! The repaint request in [`AppBridge::submit`] is the part that makes this
//! work at all: an idle egui app draws nothing until something wakes it, so a
//! command posted to a window nobody is touching would otherwise sit in the
//! channel until the user moved the mouse.

use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::Duration;

use eframe::egui;
use serde_json::Value;

use crate::doc::annotation::{Annotation, AnnotationId, AnnotationKind, Color, Style, TextAlign};
use crate::doc::geometry::{PdfPoint, PdfRect};

/// How long a tool waits for the UI thread. Every command is a handful of
/// milliseconds of work, so reaching this means the app is not drawing --
/// almost always a modal file dialog, which blocks the frame loop.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Where a command's answer goes.
pub type Reply = tokio::sync::oneshot::Sender<Result<Value, String>>;

/// One markup annotation to add, in the terms a caller can reasonably know:
/// 1-based source page numbers and PDF points measured from the bottom-left.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkupReq {
    pub page: usize,
    pub kind: String,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    /// `#rrggbb` or `#rrggbbaa`.
    pub color: Option<String>,
    /// The contents of a text box.
    pub text: Option<String>,
}

/// Something for the UI thread to do, and somewhere to put the answer.
pub enum AppCommand {
    ListLibrary {
        reply: Reply,
    },
    SearchLibrary {
        query: String,
        limit: usize,
        reply: Reply,
    },
    GetDocumentText {
        doc_id: String,
        /// 1-based and inclusive; `None` means "from the start" / "as much as
        /// one call will give".
        first: Option<usize>,
        last: Option<usize>,
        reply: Reply,
    },
    OpenDocument {
        doc_id: String,
        /// 1-based source page to land on.
        page: Option<usize>,
        reply: Reply,
    },
    AddMarkup {
        req: MarkupReq,
        reply: Reply,
    },
    ExportPdf {
        path: String,
        flatten: bool,
        reply: Reply,
    },
    FindMatches {
        query: String,
        reply: Reply,
    },
}

/// The server's end of the crossing.
///
/// `mpsc::Sender` is `Send` but not `Sync`, and a tool router hands the same
/// handler to every request, so the sender lives behind a mutex. It is held
/// only for the length of a `send`.
pub struct AppBridge {
    tx: Mutex<Sender<AppCommand>>,
    egui: egui::Context,
}

impl AppBridge {
    pub fn new(tx: Sender<AppCommand>, egui: egui::Context) -> Self {
        Self {
            tx: Mutex::new(tx),
            egui,
        }
    }

    /// Post a command and wait for the UI thread's answer.
    ///
    /// `build` is handed the reply channel so each command can carry it in its
    /// own shape. The error side is a sentence for the model to read: a tool
    /// that cannot run is something it can work around.
    pub async fn submit(&self, build: impl FnOnce(Reply) -> AppCommand) -> Result<Value, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        {
            let tx = self.tx.lock().map_err(|_| CLOSED.to_owned())?;
            tx.send(build(reply)).map_err(|_| CLOSED.to_owned())?;
        }
        // Without this an idle window never draws, and never drains the queue.
        self.egui.request_repaint();
        match tokio::time::timeout(TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CLOSED.to_owned()),
            Err(_) => Err(
                "evo did not respond (is a modal dialog open?). The request was \
                 not carried out."
                    .to_owned(),
            ),
        }
    }
}

const CLOSED: &str = "evo is no longer accepting requests";

/// The markup kinds a caller may ask for, named the way the toolbar names them.
pub const MARKUP_KINDS: [&str; 8] = [
    "highlight",
    "rect",
    "ellipse",
    "cloud",
    "line",
    "arrow",
    "text",
    "stamp",
];

/// Turn a request into an annotation, or say why it is not one.
///
/// This is where a caller's terms become evo's: 1-based pages, a colour written
/// the way the web writes it, and one flat rectangle whatever the shape. `base`
/// is the editor's current style, so markup an assistant adds looks like markup
/// the user would have added.
pub fn annotation_from(
    req: &MarkupReq,
    id: AnnotationId,
    base: Style,
    font_size: f32,
) -> Result<Annotation, String> {
    if req.page == 0 {
        return Err("pages are numbered from 1".to_owned());
    }
    let rect = PdfRect::from_points(PdfPoint::new(req.x0, req.y0), PdfPoint::new(req.x1, req.y1));
    if !rect.min.x.is_finite()
        || !rect.min.y.is_finite()
        || !rect.max.x.is_finite()
        || !rect.max.y.is_finite()
    {
        return Err("the coordinates have to be real numbers".to_owned());
    }
    let color = req.color.as_deref().map(parse_color).transpose()?;

    let (kind, style) = match req.kind.trim().to_ascii_lowercase().as_str() {
        "highlight" => (
            AnnotationKind::Highlight,
            Style {
                stroke: Color::TRANSPARENT,
                stroke_width: 0.0,
                fill: color.unwrap_or(Color::rgba(250, 220, 50, 255)),
                opacity: 0.45,
            },
        ),
        "rect" | "rectangle" => (AnnotationKind::Rect, stroked(base, color)),
        "ellipse" | "circle" | "oval" => (AnnotationKind::Ellipse, stroked(base, color)),
        // A revision cloud is the rectangle an assistant asked for, drawn with
        // scalloped edges. The free-form polygon is not offered here: this
        // request carries two corners and nothing else.
        "cloud" | "revision cloud" => (
            AnnotationKind::Polygon {
                points: crate::tools::rect_points(rect),
                cloudy: Some(crate::tools::DEFAULT_CLOUD_INTENSITY),
            },
            stroked(base, color),
        ),
        kind @ ("line" | "arrow") => (
            AnnotationKind::Line {
                p1: PdfPoint::new(req.x0, req.y0),
                p2: PdfPoint::new(req.x1, req.y1),
                arrow_end: kind == "arrow",
            },
            stroked(base, color),
        ),
        "text" | "textbox" | "note" => {
            let text = req
                .text
                .clone()
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| "a text box needs some text to put in it".to_owned())?;
            (
                AnnotationKind::TextBox {
                    text,
                    font_size,
                    align: TextAlign::Left,
                },
                stroked(base, color),
            )
        }
        "stamp" => {
            let text = req
                .text
                .clone()
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| {
                    "a stamp needs some words to stamp; APPROVED and DRAFT are the usual ones"
                        .to_owned()
                })?;
            (
                AnnotationKind::Stamp {
                    text,
                    font_size: crate::tools::DEFAULT_STAMP_FONT,
                },
                Style {
                    stroke: color.unwrap_or(crate::tools::STAMP_RED),
                    stroke_width: 1.5,
                    fill: Color::TRANSPARENT,
                    opacity: base.opacity,
                },
            )
        }
        other => {
            return Err(format!(
                "there is no markup kind called “{other}”; evo draws {}",
                MARKUP_KINDS.join(", ")
            ));
        }
    };

    Ok(Annotation {
        id,
        page: req.page - 1,
        kind,
        rect,
        style,
    })
}

/// The editor's current style with the requested colour, if one was asked for.
fn stroked(base: Style, color: Option<Color>) -> Style {
    match color {
        Some(stroke) => Style { stroke, ..base },
        None => base,
    }
}

/// `#rrggbb` or `#rrggbbaa`, with or without the hash.
fn parse_color(text: &str) -> Result<Color, String> {
    let hex = text.trim().trim_start_matches('#');
    let bad = || format!("“{text}” is not a colour; write one as #rrggbb or #rrggbbaa");
    if !matches!(hex.len(), 6 | 8) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| bad());
    Ok(Color::rgba(
        byte(0)?,
        byte(2)?,
        byte(4)?,
        if hex.len() == 8 { byte(6)? } else { 255 },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_command_reaches_the_app_and_its_answer_comes_back() {
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = AppBridge::new(tx, egui::Context::default());

        // The "UI thread": one command, answered the way the app would.
        let app = std::thread::spawn(move || {
            let AppCommand::SearchLibrary {
                query,
                limit,
                reply,
            } = rx.recv().expect("a command")
            else {
                panic!("the wrong command arrived");
            };
            reply
                .send(Ok(json!({ "query": query, "limit": limit })))
                .map_err(|_| "nobody waiting")
                .expect("the caller is waiting");
        });

        let value = bridge
            .submit(|reply| AppCommand::SearchLibrary {
                query: "boiler".into(),
                limit: 5,
                reply,
            })
            .await
            .expect("an answer");
        assert_eq!(value["query"], "boiler");
        assert_eq!(value["limit"], 5);
        app.join().expect("the app thread");
    }

    /// The app's refusal is the tool's error, not a protocol failure: the model
    /// is told what went wrong and can try something else.
    #[tokio::test]
    async fn a_refusal_comes_back_as_the_tools_error() {
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = AppBridge::new(tx, egui::Context::default());
        let app = std::thread::spawn(move || {
            let AppCommand::ExportPdf { reply, .. } = rx.recv().expect("a command") else {
                panic!("the wrong command arrived");
            };
            let _ = reply.send(Err("no document is open".to_owned()));
        });

        let err = bridge
            .submit(|reply| AppCommand::ExportPdf {
                path: "/tmp/x.pdf".into(),
                flatten: false,
                reply,
            })
            .await
            .expect_err("refused");
        assert_eq!(err, "no document is open");
        app.join().expect("the app thread");
    }

    /// A window that never draws must not hang the server for good.
    #[tokio::test(start_paused = true)]
    async fn an_app_that_never_answers_times_out_with_a_reason() {
        let (tx, rx) = std::sync::mpsc::channel();
        let bridge = AppBridge::new(tx, egui::Context::default());
        let err = bridge
            .submit(|reply| AppCommand::ListLibrary { reply })
            .await
            .expect_err("timed out");
        assert!(err.contains("did not respond"), "{err}");
        assert!(err.contains("modal dialog"), "{err}");
        drop(rx);
    }

    fn markup(kind: &str) -> MarkupReq {
        MarkupReq {
            page: 3,
            kind: kind.to_owned(),
            x0: 100.0,
            y0: 700.0,
            x1: 300.0,
            y1: 720.0,
            color: None,
            text: None,
        }
    }

    /// The page a caller names and the page evo stores differ by one, which is
    /// exactly the kind of thing that is wrong for a year before anyone notices.
    #[test]
    fn a_page_number_becomes_an_index() {
        let ann = annotation_from(&markup("rect"), 7, Style::default(), 12.0).expect("mapped");
        assert_eq!(ann.page, 2, "page 3 is index 2");
        assert_eq!(ann.id, 7);
        assert_eq!(ann.rect.min, PdfPoint::new(100.0, 700.0));
        assert_eq!(ann.rect.max, PdfPoint::new(300.0, 720.0));

        let err = annotation_from(&markup("rect"), 1, Style::default(), 12.0);
        assert!(err.is_ok());
        let mut zero = markup("rect");
        zero.page = 0;
        let err = annotation_from(&zero, 1, Style::default(), 12.0).expect_err("no page 0");
        assert!(err.contains("numbered from 1"), "{err}");
    }

    #[test]
    fn every_kind_evo_draws_can_be_asked_for_by_name() {
        for kind in MARKUP_KINDS {
            let mut req = markup(kind);
            req.text = Some("a note".to_owned());
            let ann = annotation_from(&req, 1, Style::default(), 12.0)
                .unwrap_or_else(|e| panic!("{kind} should map: {e}"));
            match kind {
                "highlight" => assert!(matches!(ann.kind, AnnotationKind::Highlight)),
                "rect" => assert!(matches!(ann.kind, AnnotationKind::Rect)),
                "ellipse" => assert!(matches!(ann.kind, AnnotationKind::Ellipse)),
                "line" => assert!(matches!(
                    ann.kind,
                    AnnotationKind::Line {
                        arrow_end: false,
                        ..
                    }
                )),
                "arrow" => assert!(matches!(
                    ann.kind,
                    AnnotationKind::Line {
                        arrow_end: true,
                        ..
                    }
                )),
                "text" => assert!(matches!(ann.kind, AnnotationKind::TextBox { .. })),
                "stamp" => match &ann.kind {
                    AnnotationKind::Stamp { text, .. } => {
                        assert_eq!(text, "a note");
                        assert_eq!(ann.style.stroke, crate::tools::STAMP_RED);
                    }
                    other => panic!("a stamp came back as {other:?}"),
                },
                "cloud" => match &ann.kind {
                    AnnotationKind::Polygon { points, cloudy } => {
                        assert_eq!(points.len(), 4, "a cloud is drawn round a rectangle");
                        assert_eq!(*cloudy, Some(crate::tools::DEFAULT_CLOUD_INTENSITY));
                    }
                    other => panic!("a cloud came back as {other:?}"),
                },
                other => unreachable!("{other}"),
            }
        }
    }

    /// A line's direction is not recoverable from its bounding box, so the
    /// endpoints have to be carried through as given.
    #[test]
    fn a_line_keeps_the_direction_it_was_drawn_in() {
        let mut req = markup("arrow");
        (req.x0, req.y0, req.x1, req.y1) = (300.0, 720.0, 100.0, 700.0);
        let ann = annotation_from(&req, 1, Style::default(), 12.0).expect("mapped");
        let AnnotationKind::Line { p1, p2, arrow_end } = ann.kind else {
            panic!("not a line");
        };
        assert_eq!(p1, PdfPoint::new(300.0, 720.0));
        assert_eq!(p2, PdfPoint::new(100.0, 700.0), "the arrow points here");
        assert!(arrow_end);
        // The bounds are still normalized, whichever way it was drawn.
        assert_eq!(ann.rect.min, PdfPoint::new(100.0, 700.0));
    }

    #[test]
    fn a_highlight_is_translucent_yellow_unless_a_colour_was_asked_for() {
        let ann = annotation_from(&markup("highlight"), 1, Style::default(), 12.0).expect("ok");
        assert_eq!(ann.style.fill, Color::rgba(250, 220, 50, 255));
        assert!(ann.style.opacity < 1.0, "highlights let the page through");

        let mut req = markup("highlight");
        req.color = Some("#00ff0080".to_owned());
        let ann = annotation_from(&req, 1, Style::default(), 12.0).expect("ok");
        assert_eq!(ann.style.fill, Color::rgba(0, 255, 0, 128));
    }

    #[test]
    fn a_colour_is_read_the_way_it_is_written_and_refused_when_it_is_not() {
        let mut req = markup("rect");
        for (text, want) in [
            ("#ff0000", Color::rgba(255, 0, 0, 255)),
            ("00FF00", Color::rgba(0, 255, 0, 255)),
            (" #0000ff40 ", Color::rgba(0, 0, 255, 64)),
        ] {
            req.color = Some(text.to_owned());
            let ann = annotation_from(&req, 1, Style::default(), 12.0).expect(text);
            assert_eq!(ann.style.stroke, want, "{text}");
        }
        for text in ["red", "#fff", "#gggggg", ""] {
            req.color = Some(text.to_owned());
            let err = annotation_from(&req, 1, Style::default(), 12.0)
                .expect_err(&format!("{text:?} is not a colour"));
            assert!(err.contains("not a colour"), "{err}");
        }
    }

    /// The failures a model can recover from are the ones worth spelling out.
    #[test]
    fn an_unknown_kind_and_an_empty_text_box_say_what_to_do_instead() {
        let err = annotation_from(&markup("squiggle"), 1, Style::default(), 12.0)
            .expect_err("no such kind");
        assert!(err.contains("no markup kind called"), "{err}");
        assert!(
            err.contains("highlight"),
            "the list is in the message: {err}"
        );

        let err = annotation_from(&markup("text"), 1, Style::default(), 12.0)
            .expect_err("nothing to write");
        assert!(err.contains("needs some text"), "{err}");

        let err = annotation_from(&markup("stamp"), 1, Style::default(), 12.0)
            .expect_err("nothing to stamp");
        assert!(err.contains("needs some words"), "{err}");
        assert!(err.contains("APPROVED"), "the usual ones are named: {err}");
    }

    /// A closed app is not a hang: the tool fails at once.
    #[tokio::test]
    async fn a_closed_app_fails_immediately() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let bridge = AppBridge::new(tx, egui::Context::default());
        let err = bridge
            .submit(|reply| AppCommand::ListLibrary { reply })
            .await
            .expect_err("closed");
        assert_eq!(err, CLOSED);
    }
}
