//! What the model may do to evo, from a phone.
//!
//! The desktop app lets an assistant drive it through MCP: the tools post a
//! command to the UI thread and the user watches their document open and get
//! marked up. There is no UI thread here, so the same idea is built the other
//! way round. A tool that *reads* -- the library, a search, a document's text --
//! calls [`library_tools`] directly with the library's lock in hand, exactly as
//! `evo mcp-serve` does. A tool that *does* something the reader should see
//! sends an event down the same server-sent-events stream the answer is
//! arriving on, and the browser acts on it: `open_document` turns the page,
//! `highlight_text` writes the markup and asks the viewer to redraw it.
//!
//! That is the whole of "the agent drives evo": there is no second channel and
//! no polling, so a tool's effect lands in the transcript in the order it
//! happened, between the words that explain it.
//!
//! Servers the operator configured in `config.json` are merged in beside these,
//! under `server__tool` names, by the same [`McpClients`] the desktop app uses.
//! They are config-file-only on purpose: naming a program to run is not
//! something an HTTP API should ever accept.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::doc::annotation::{Annotation, AnnotationKind, Color, Style, TextAlign};
use crate::doc::geometry::{PdfPoint, PdfRect};
use crate::library::extract::{extract_page_layout, find_in_line, rect_for_range};
use crate::library::{Library, SavedMarkup};
use crate::mcp::McpAccess;
use crate::mcp::client::{McpClients, RemoteTool};
use crate::mcp::library_tools;
use crate::script::model::ToolDef;

use super::chat_api::Say;
use super::library_api::is_doc_id;
use super::markup_api;

/// The server evo's own tools belong to. Unqualified names, so a model that has
/// used evo over MCP calls them by the names it already knows.
const EVO: &str = "evo";

/// How many separate places one `highlight_text` will mark. A word that appears
/// forty times on a page is a search, not a highlight, and covering the page in
/// yellow is not what was asked for.
const MAX_MARKS: usize = 20;

/// The colours markup is made in. The same values `viewer.js` uses when a
/// finger draws one, because a highlight the model made and a highlight the
/// reader made are the same thing in the sidecar and must look it.
const HIGHLIGHTER: Color = Color::rgba(250, 220, 50, 255);
const NOTE_PAPER: Color = Color::rgba(255, 245, 180, 255);
const NOTE_INK: Color = Color::rgba(30, 30, 46, 255);
const HIGHLIGHT_OPACITY: f32 = 0.35;
const NOTE_OPACITY: f32 = 0.95;
/// Note text, in points: about the size of the body text on a letter page.
const NOTE_FONT: f32 = 11.0;
/// How wide a note's box is, and how much room one line of it takes.
const NOTE_WIDTH: f32 = 180.0;
const NOTE_LINE: f32 = NOTE_FONT * 1.3;
/// Roughly how many characters of `NOTE_FONT` fit across `NOTE_WIDTH`.
const NOTE_CHARS_PER_LINE: usize = 34;

/// Where a tool's visible consequence goes.
///
/// A function rather than the channel itself so the tests can watch what a tool
/// did without a socket, and so nothing in here has to know that the transport
/// is server-sent events.
pub type Emit = Arc<dyn Fn(Say) + Send + Sync>;

/// An [`Emit`] that throws everything away, for a caller with no reader.
/// Every real one has a browser at the end of it.
#[cfg(test)]
pub fn ignore() -> Emit {
    Arc::new(|_| {})
}

/// Everything the model may reach: this library, and whatever MCP servers the
/// configuration named.
pub struct ServerTools {
    library: Arc<Mutex<Library>>,
    /// Other people's servers. `None` when the configuration named none, which
    /// is the ordinary case and costs nothing.
    clients: Option<Arc<McpClients>>,
    emit: Emit,
}

impl ServerTools {
    pub fn new(library: Arc<Mutex<Library>>, clients: Option<Arc<McpClients>>, emit: Emit) -> Self {
        Self {
            library,
            clients,
            emit,
        }
    }

    /// The library, with the lock held for exactly as long as `work` takes.
    ///
    /// Every tool goes through here, and none of them may keep the guard: this
    /// runs on a blocking thread while the rest of the server is answering
    /// other requests against the same library.
    fn with<T>(&self, work: impl FnOnce(&Library) -> Result<T, String>) -> Result<T, String> {
        let library = self
            .library
            .lock()
            .expect("the library lock is never poisoned");
        work(&library)
    }

    /// evo's own tools, as the model is offered them.
    pub fn builtin() -> Vec<RemoteTool> {
        [
            (
                "list_library",
                "List the documents in evo's library: id, title, page count, tags and \
                 (when evo has written one) a summary. The id is what every other library \
                 tool takes.",
                json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            ),
            (
                "search_library",
                "Full-text search across every indexed page of the library. Returns matching \
                 documents with the 1-based page number and a snippet of the matching text. \
                 Use this to find out where something is before reading or marking it.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "What to search for. Ordinary words; the index is full-text.",
                        },
                        "limit": {
                            "type": "integer",
                            "description": "How many matches to return (1-50, default 10).",
                        },
                    },
                    "required": ["query"],
                }),
            ),
            (
                "get_document_text",
                "Read the text of a document's pages, by 1-based page number. The text is what \
                 evo indexed, which for scanned pages is what OCR recovered.",
                json!({
                    "type": "object",
                    "properties": {
                        "doc_id": {
                            "type": "string",
                            "description": "The document's id, from list_library or search_library.",
                        },
                        "first_page": {
                            "type": "integer",
                            "description": "First page to read, 1-based. Defaults to the first page.",
                        },
                        "last_page": {
                            "type": "integer",
                            "description": "Last page to read, 1-based and inclusive.",
                        },
                    },
                    "required": ["doc_id"],
                }),
            ),
            (
                "open_document",
                "Turn the reader's screen to a document, at a page. They see it happen \
                 immediately, so say what you are opening and why.",
                json!({
                    "type": "object",
                    "properties": {
                        "doc_id": {
                            "type": "string",
                            "description": "The document's id, from list_library or search_library.",
                        },
                        "page": {
                            "type": "integer",
                            "description": "Page to turn to, 1-based. Defaults to the first page.",
                        },
                    },
                    "required": ["doc_id"],
                }),
            ),
            (
                "highlight_text",
                "Highlight words on a page of a document, optionally with a note beside them. \
                 The text has to appear on that page: evo finds it and marks exactly where it \
                 is, so quote it as it is written rather than describing it. The highlight is \
                 saved with the document and the reader sees it appear.",
                json!({
                    "type": "object",
                    "properties": {
                        "doc_id": {
                            "type": "string",
                            "description": "The document's id, from list_library or search_library.",
                        },
                        "page": {
                            "type": "integer",
                            "description": "Which page the words are on, 1-based.",
                        },
                        "text": {
                            "type": "string",
                            "description": "The words to highlight, exactly as they appear on the page. \
                                            Matching ignores case.",
                        },
                        "note": {
                            "type": "string",
                            "description": "A note to write beside the highlight, if there is something \
                                            worth saying about it.",
                        },
                    },
                    "required": ["doc_id", "page", "text"],
                }),
            ),
        ]
        .into_iter()
        .map(|(name, description, parameters)| RemoteTool {
            server: EVO.to_owned(),
            tool: name.to_owned(),
            def: ToolDef {
                name: name.to_owned(),
                description: description.to_owned(),
                parameters,
            },
        })
        .collect()
    }

    /// Run one of evo's own tools.
    fn run(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        match tool {
            "list_library" => self.with(library_tools::list_library),
            "search_library" => {
                let query = text_arg(arguments, "query")?;
                let limit = number_arg(arguments, "limit").unwrap_or(10);
                self.with(|lib| library_tools::search_library(lib, &query, limit))
            }
            "get_document_text" => {
                let doc_id = id_arg(arguments)?;
                let first = number_arg(arguments, "first_page");
                let last = number_arg(arguments, "last_page");
                self.with(|lib| library_tools::document_text(lib, &doc_id, first, last))
            }
            "open_document" => self.open_document(arguments),
            "highlight_text" => self.highlight_text(arguments),
            other => Err(format!("evo has no tool called \u{201c}{other}\u{201d}")),
        }
    }

    /// Turn the reader's screen to a document.
    ///
    /// The document is looked up before the event goes out: a model that
    /// invented an id should be told so, not left to wonder why nothing
    /// happened at the other end.
    fn open_document(&self, arguments: &Value) -> Result<Value, String> {
        let doc_id = id_arg(arguments)?;
        let page = number_arg(arguments, "page").unwrap_or(1).max(1);
        let wanted = doc_id.clone();
        let (title, page_count) = self.with(move |lib| meta_of(lib, &wanted))?;
        let page = page.min(page_count.max(1));

        (self.emit)(Say::Ui(json!({
            "action": "open",
            "doc": doc_id,
            "page": page,
        })));
        Ok(json!({
            "opened": doc_id,
            "title": title,
            "page": page,
            "shown": "the reader is now looking at this page",
        }))
    }

    /// Find words on a page and mark them.
    ///
    /// The rectangles are the ones find-in-document would draw: the page's own
    /// text layout, the same case-insensitive match, the same union of the
    /// character boxes a match covers. Nothing here has its own idea of where
    /// words are.
    fn highlight_text(&self, arguments: &Value) -> Result<Value, String> {
        let doc_id = id_arg(arguments)?;
        let text = text_arg(arguments, "text")?;
        let page = number_arg(arguments, "page")
            .ok_or_else(|| "highlight_text needs the 1-based page the words are on".to_owned())?;
        if page == 0 {
            return Err("pages are numbered from 1".to_owned());
        }
        let note = arguments
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(str::to_owned);

        let wanted = doc_id.clone();
        let (title, page_count) = self.with(move |lib| meta_of(lib, &wanted))?;
        if page > page_count {
            return Err(format!(
                "\u{201c}{title}\u{201d} has {page_count} pages, so there is no page {page}"
            ));
        }
        // The bytes leave the lock with them: parsing a PDF takes long enough
        // that holding the library shut for it would stall every other reader.
        let wanted = doc_id.clone();
        let bytes = self.with(move |lib| lib.load_bytes(&wanted).map_err(|e| e.to_string()))?;

        let found = locate(Arc::new(bytes), page, &text)?;
        if found.rects.is_empty() {
            return Err(format!(
                "page {page} of \u{201c}{title}\u{201d} does not say \u{201c}{text}\u{201d}. \
                 Read the page with get_document_text and quote it as it is written."
            ));
        }

        let marks = found.rects.len();
        let (etag, added) = self.with(|lib| {
            let mut markup = lib
                .load_markup(&doc_id)
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| markup_api::empty_markup(page_count));
            let added = draw(&mut markup, page, &found, note.as_deref());
            lib.save_markup(&doc_id, &markup)
                .map_err(|e| e.to_string())?;
            // The same digest the markup endpoint answers with, computed from
            // the same bytes: a tool's write moves the version tag the viewer
            // is holding, and it has to move to the value the API will report.
            Ok((markup_api::etag(&markup), added))
        })?;

        (self.emit)(Say::Ui(json!({
            "action": "markup-changed",
            "doc": doc_id,
            "page": page,
            "etag": etag,
        })));
        Ok(json!({
            "highlighted": text,
            "doc_id": doc_id,
            "title": title,
            "page": page,
            "marks": marks,
            "note_added": note.is_some(),
            "rects": found
                .rects
                .iter()
                .map(|r| json!({ "x0": r.min.x, "y0": r.min.y, "x1": r.max.x, "y1": r.max.y }))
                .collect::<Vec<Value>>(),
            "annotation_ids": added,
            "shown": "the highlight is on the reader's screen now",
        }))
    }
}

/// Chat reaches evo's tools and everybody else's through the same door.
impl McpAccess for ServerTools {
    fn tools(&self) -> Vec<RemoteTool> {
        let mut all = Self::builtin();
        if let Some(clients) = &self.clients {
            all.extend(clients.tools().as_ref().clone());
        }
        all
    }

    fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<String, String> {
        // evo's own tools first, and only for the names it really has: an
        // operator may well call one of their servers "evo", and their tools
        // are qualified anyway, so a name that is not one of ours goes out.
        if server == EVO && Self::builtin().iter().any(|t| t.tool == tool) {
            return self.run(tool, &arguments).map(|value| value.to_string());
        }
        match &self.clients {
            Some(clients) => clients.call(server, tool, arguments),
            None => Err(format!("evo has no tool called \u{201c}{tool}\u{201d}")),
        }
    }
}

/// One document's title and length, or why there is no such document.
fn meta_of(lib: &Library, doc_id: &str) -> Result<(String, usize), String> {
    match lib.doc(doc_id).map_err(|e| e.to_string())? {
        Some(meta) => Ok((meta.title, meta.page_count)),
        None => Err(format!(
            "there is no document with id {doc_id} in the library; \
             list_library gives the ids"
        )),
    }
}

/// Where some words are on a page, and how big that page is.
struct Found {
    rects: Vec<PdfRect>,
    page_width: f32,
    page_height: f32,
}

/// Find `needle` on 1-based `page`, in the page's own coordinates.
fn locate(bytes: Arc<Vec<u8>>, page: usize, needle: &str) -> Result<Found, String> {
    let pdf = hayro::hayro_syntax::Pdf::new(bytes)
        .map_err(|_| "evo cannot read that document any more".to_owned())?;
    let pages = pdf.pages();
    let Some(source) = pages.get(page - 1) else {
        return Err(format!("that document has no page {page}"));
    };
    let (page_width, page_height) = source.render_dimensions();
    let (layout, _) = extract_page_layout(source, &Default::default());

    let mut rects = Vec::new();
    for line in &layout.lines {
        for range in find_in_line(&line.text, needle) {
            if let Some(rect) = rect_for_range(line, range) {
                rects.push(rect);
                if rects.len() >= MAX_MARKS {
                    return Ok(Found {
                        rects,
                        page_width,
                        page_height,
                    });
                }
            }
        }
    }
    Ok(Found {
        rects,
        page_width,
        page_height,
    })
}

/// Add the highlights, and the note if there is one, to `markup`. Returns the
/// ids given out.
///
/// Ids continue from the highest there is, which is the rule
/// `AnnotationStore::restore` and `viewer.js` both follow -- otherwise a mark
/// made by the model and one made by a finger would eventually be the same
/// annotation.
fn draw(markup: &mut SavedMarkup, page: usize, found: &Found, note: Option<&str>) -> Vec<u64> {
    let mut next = markup
        .annotations
        .iter()
        .map(|a| a.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let mut ids = Vec::new();
    for rect in &found.rects {
        markup.annotations.push(Annotation {
            id: next,
            page: page - 1,
            kind: AnnotationKind::Highlight,
            rect: *rect,
            style: Style {
                stroke: Color::TRANSPARENT,
                stroke_width: 0.0,
                fill: HIGHLIGHTER,
                opacity: HIGHLIGHT_OPACITY,
            },
        });
        ids.push(next);
        next = next.saturating_add(1);
    }

    if let (Some(note), Some(first)) = (note, found.rects.first()) {
        markup.annotations.push(Annotation {
            id: next,
            page: page - 1,
            kind: AnnotationKind::TextBox {
                text: note.to_owned(),
                font_size: NOTE_FONT,
                align: TextAlign::Left,
            },
            rect: note_box(*first, note, found.page_width, found.page_height),
            // The stroke colour is the ink: `write_annotation` draws a TextBox's
            // words in it and fills the box with `fill`.
            style: Style {
                stroke: NOTE_INK,
                stroke_width: 0.0,
                fill: NOTE_PAPER,
                opacity: NOTE_OPACITY,
            },
        });
        ids.push(next);
    }
    ids
}

/// Where a note sits: under the words it is about, or above them when there is
/// no room below, and always inside the page.
fn note_box(mark: PdfRect, note: &str, page_width: f32, page_height: f32) -> PdfRect {
    let lines = note.chars().count().div_ceil(NOTE_CHARS_PER_LINE).max(1);
    let height = (lines as f32 * NOTE_LINE + 6.0).min(page_height);
    let width = NOTE_WIDTH.min(page_width);
    let left = mark.min.x.min(page_width - width).max(0.0);

    // Four points of daylight between the note and the words, on whichever side
    // the page has room for it.
    let top = if mark.min.y - 4.0 - height >= 0.0 {
        mark.min.y - 4.0
    } else {
        (mark.max.y + 4.0 + height).min(page_height)
    };
    PdfRect::from_points(
        PdfPoint::new(left, top - height),
        PdfPoint::new(left + width, top),
    )
}

// ---------------------------------------------------------------------------
// Reading a model's arguments
// ---------------------------------------------------------------------------

/// A string argument, or a sentence saying which one is missing. Every failure
/// here is read by the *model*, which can correct itself and try again.
fn text_arg(arguments: &Value, name: &str) -> Result<String, String> {
    match arguments.get(name).and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => Ok(text.trim().to_owned()),
        _ => Err(format!("this tool needs a \u{201c}{name}\u{201d} argument")),
    }
}

/// A whole number, however the model wrote it: some send `2`, some `"2"`.
fn number_arg(arguments: &Value, name: &str) -> Option<usize> {
    let value = arguments.get(name)?;
    match value {
        Value::Number(number) => number.as_u64().map(|n| n as usize),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// The document id, checked to be one before it is used as a key.
fn id_arg(arguments: &Value) -> Result<String, String> {
    let id = text_arg(arguments, "doc_id")?;
    if !is_doc_id(&id) {
        return Err(format!(
            "\u{201c}{id}\u{201d} is not a document id; ids are 64 hexadecimal \
             characters and come from list_library or search_library"
        ));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::extract::extract_page_layout;
    use std::path::{Path, PathBuf};

    /// A library of its own, because redb permits one process per database and
    /// the test binary runs these in parallel.
    fn temp_library(name: &str) -> (Arc<Mutex<Library>>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("evo-serve-tools-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let library = Library::open_at(dir.clone()).expect("a library");
        (Arc::new(Mutex::new(library)), dir)
    }

    /// Everything the tools said to the reader, in order.
    fn watcher() -> (Emit, Arc<Mutex<Vec<Say>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let emit: Emit = Arc::new(move |said| recorder.lock().unwrap().push(said));
        (emit, seen)
    }

    fn imported(library: &Arc<Mutex<Library>>) -> String {
        library
            .lock()
            .unwrap()
            .import(Path::new("tests/fixtures/sample.pdf"), None)
            .expect("the fixture")
            .id
    }

    /// The rectangles find-in-document would draw for `needle` on page 1 of the
    /// fixture, worked out from the PDF and nothing else.
    fn expected_rects(needle: &str) -> Vec<PdfRect> {
        let bytes = std::fs::read("tests/fixtures/sample.pdf").expect("the fixture");
        let pdf = hayro::hayro_syntax::Pdf::new(bytes).expect("a PDF");
        let pages = pdf.pages();
        let (layout, _) = extract_page_layout(&pages[0], &Default::default());
        layout
            .lines
            .iter()
            .flat_map(|line| {
                find_in_line(&line.text, needle)
                    .into_iter()
                    .filter_map(|range| rect_for_range(line, range))
            })
            .collect()
    }

    /// The promise the tool makes: it marks where the words really are. The
    /// rectangles have to be find-in-document's own, to the last decimal --
    /// anything else would be a second idea of where text sits on a page.
    #[test]
    fn a_highlight_lands_exactly_where_the_words_are() {
        let (library, dir) = temp_library("rects");
        let id = imported(&library);
        let (emit, seen) = watcher();
        let tools = ServerTools::new(library.clone(), None, emit);

        let said = tools
            .call(
                EVO,
                "highlight_text",
                json!({"doc_id": id, "page": 1, "text": "quick"}),
            )
            .expect("the fixture says \u{201c}quick\u{201d} on page 1");
        let answer: Value = serde_json::from_str(&said).expect("JSON for the model");
        assert_eq!(answer["page"], 1);
        assert_eq!(answer["marks"], 1);
        assert_eq!(answer["note_added"], false);

        let wanted = expected_rects("quick");
        assert_eq!(wanted.len(), 1, "the fixture has one \u{201c}quick\u{201d}");
        assert_eq!(answer["rects"][0]["x0"], wanted[0].min.x);
        assert_eq!(answer["rects"][0]["y0"], wanted[0].min.y);
        assert_eq!(answer["rects"][0]["x1"], wanted[0].max.x);
        assert_eq!(answer["rects"][0]["y1"], wanted[0].max.y);

        // And what was saved is what was reported.
        let markup = library
            .lock()
            .unwrap()
            .load_markup(&id)
            .expect("reading the sidecar")
            .expect("the tool wrote one");
        assert_eq!(markup.annotations.len(), 1);
        let drawn = &markup.annotations[0];
        assert_eq!(drawn.page, 0, "annotations count pages from zero");
        assert!(matches!(drawn.kind, AnnotationKind::Highlight));
        assert_eq!(drawn.rect, wanted[0]);
        assert_eq!(drawn.style.fill, HIGHLIGHTER);
        assert!(
            !drawn.style.stroke.is_visible(),
            "a highlight has no outline"
        );
        assert_eq!(markup.pages.order.len(), 2, "the page order was seeded");

        // The reader is told, with the version tag the markup endpoint will
        // now report -- which is what makes the viewer refetch the overlay.
        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Say::Ui(value) => {
                assert_eq!(value["action"], "markup-changed");
                assert_eq!(value["doc"], id.as_str());
                assert_eq!(value["page"], 1);
                assert_eq!(value["etag"], markup_api::etag(&markup).as_str());
            }
            other => panic!("the tool said {other:?}"),
        }

        std::fs::remove_dir_all(dir).ok();
    }

    /// A tool's write moves the version tag, which is the whole mechanism the
    /// viewer and the conditional-write path depend on.
    #[test]
    fn marking_a_page_moves_the_version_tag_the_api_reports() {
        let (library, dir) = temp_library("etag");
        let id = imported(&library);
        let before = markup_api::etag_of(None, 2);
        let tools = ServerTools::new(library.clone(), None, ignore());

        tools
            .call(
                EVO,
                "highlight_text",
                json!({"doc_id": id, "page": 1, "text": "quick", "note": "worth reading"}),
            )
            .expect("marked");

        let markup = library
            .lock()
            .unwrap()
            .load_markup(&id)
            .unwrap()
            .expect("saved");
        let after = markup_api::etag_of(Some(&markup), 2);
        assert_ne!(after, before, "the version moved");
        // Two annotations, numbered without collision, and the note is one of
        // them: a highlight the reader can see and words beside it.
        assert_eq!(markup.annotations.len(), 2);
        let ids: Vec<u64> = markup.annotations.iter().map(|a| a.id).collect();
        assert_eq!(ids, [1, 2]);
        match &markup.annotations[1].kind {
            AnnotationKind::TextBox { text, .. } => assert_eq!(text, "worth reading"),
            other => panic!("the note came back as {other:?}"),
        }
        // The note sits beside the words, on the same page and inside it.
        let note = markup.annotations[1].rect;
        assert_eq!(markup.annotations[1].page, 0);
        assert!(note.min.x >= 0.0 && note.max.x <= 612.0, "{note:?}");
        assert!(note.min.y >= 0.0 && note.max.y <= 792.0, "{note:?}");

        // A second mark carries on numbering from where the first stopped.
        tools
            .call(
                EVO,
                "highlight_text",
                json!({"doc_id": id, "page": 1, "text": "brown"}),
            )
            .expect("marked again");
        let markup = library
            .lock()
            .unwrap()
            .load_markup(&id)
            .unwrap()
            .expect("saved");
        assert_eq!(markup.annotations.len(), 3);
        assert_eq!(markup.annotations[2].id, 3);
        assert_ne!(markup_api::etag_of(Some(&markup), 2), after);

        std::fs::remove_dir_all(dir).ok();
    }

    /// Opening a document is an event and nothing else: the browser does the
    /// turning, so the tool's job is to be sure there is something to turn to.
    #[test]
    fn opening_a_document_tells_the_reader_which_page() {
        let (library, dir) = temp_library("open");
        let id = imported(&library);
        let (emit, seen) = watcher();
        let tools = ServerTools::new(library, None, emit);

        let said = tools
            .call(EVO, "open_document", json!({"doc_id": id, "page": 2}))
            .expect("opened");
        assert!(said.contains("\"page\":2"), "{said}");

        let events = seen.lock().unwrap();
        match &events[0] {
            Say::Ui(value) => {
                assert_eq!(value["action"], "open");
                assert_eq!(value["doc"], id.as_str());
                assert_eq!(value["page"], 2);
            }
            other => panic!("the tool said {other:?}"),
        }
        drop(events);

        // A page past the end is the last page, not a refusal: the model was
        // right about the document and only wrong about how long it is.
        let said = tools
            .call(EVO, "open_document", json!({"doc_id": id, "page": 99}))
            .expect("opened");
        assert!(said.contains("\"page\":2"), "{said}");

        std::fs::remove_dir_all(dir).ok();
    }

    /// Every way a call can be wrong ends in a sentence the model can act on,
    /// because a tool error is the model's problem to work around and not
    /// something to fail the answer over.
    #[test]
    fn a_wrong_call_says_what_would_have_been_right() {
        let (library, dir) = temp_library("errors");
        let id = imported(&library);
        let tools = ServerTools::new(library, None, ignore());

        let missing = tools
            .call(EVO, "highlight_text", json!({"page": 1, "text": "quick"}))
            .expect_err("no document named");
        assert!(missing.contains("doc_id"), "{missing}");

        let bogus = tools
            .call(
                EVO,
                "open_document",
                json!({"doc_id": "../../etc/passwd", "page": 1}),
            )
            .expect_err("not an id");
        assert!(bogus.contains("64 hexadecimal"), "{bogus}");

        let absent = tools
            .call(EVO, "open_document", json!({"doc_id": "f".repeat(64)}))
            .expect_err("no such document");
        assert!(absent.contains("list_library"), "{absent}");

        let past_the_end = tools
            .call(
                EVO,
                "highlight_text",
                json!({"doc_id": id, "page": 9, "text": "quick"}),
            )
            .expect_err("no page 9");
        assert!(past_the_end.contains("2 pages"), "{past_the_end}");

        let unsaid = tools
            .call(
                EVO,
                "highlight_text",
                json!({"doc_id": id, "page": 1, "text": "aardvark"}),
            )
            .expect_err("the fixture says no such thing");
        assert!(unsaid.contains("get_document_text"), "{unsaid}");

        let unknown = tools
            .call(EVO, "delete_everything", json!({}))
            .expect_err("no such tool");
        assert!(unknown.contains("no tool called"), "{unknown}");

        std::fs::remove_dir_all(dir).ok();
    }

    /// The five tools are the contract with the model, and their descriptions
    /// are the only instructions it gets about what evo can do.
    #[test]
    fn every_tool_is_offered_with_a_description_and_a_schema() {
        let tools = ServerTools::builtin();
        let mut names: Vec<&str> = tools.iter().map(|t| t.def.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "get_document_text",
                "highlight_text",
                "list_library",
                "open_document",
                "search_library",
            ]
        );
        for tool in &tools {
            assert_eq!(tool.server, EVO);
            assert_eq!(tool.tool, tool.def.name, "evo's own tools are unqualified");
            assert!(
                tool.def.description.len() > 40,
                "{} needs a description a model can act on",
                tool.def.name
            );
            assert_eq!(tool.def.parameters["type"], "object", "{}", tool.def.name);
        }
        // The two arguments a model is most likely to get wrong are the ones
        // the schema has to be explicit about.
        let highlight = tools
            .iter()
            .find(|t| t.def.name == "highlight_text")
            .expect("highlight_text");
        let schema = highlight.def.parameters.to_string();
        assert!(schema.contains("1-based"), "{schema}");
        assert!(schema.contains("as they appear"), "{schema}");
    }

    /// Numbers arrive as numbers from one dialect and as strings from another,
    /// and a page number that is quietly ignored is a highlight on the wrong
    /// page.
    #[test]
    fn a_page_number_is_read_however_the_model_wrote_it() {
        assert_eq!(number_arg(&json!({"page": 3}), "page"), Some(3));
        assert_eq!(number_arg(&json!({"page": "3"}), "page"), Some(3));
        assert_eq!(number_arg(&json!({"page": " 3 "}), "page"), Some(3));
        assert_eq!(number_arg(&json!({"page": "later"}), "page"), None);
        assert_eq!(number_arg(&json!({}), "page"), None);
        assert_eq!(number_arg(&json!({"page": null}), "page"), None);
    }

    /// A configured server's tools are offered beside evo's own, under names
    /// that cannot collide with them.
    #[test]
    fn other_peoples_servers_are_merged_in_beside_evos_own() {
        let (library, dir) = temp_library("merge");
        let clients = Arc::new(McpClients::default());
        clients.configure(&[crate::mcp::client::ClientEntry {
            name: "evo".into(),
            command: "true".into(),
            args: Vec::new(),
        }]);
        let tools = ServerTools::new(library, Some(clients), ignore());

        // The configured server will not start, which is not a reason to have
        // no tools: evo's own are still there.
        let offered = tools.tools();
        assert_eq!(offered.len(), ServerTools::builtin().len());

        // A server the operator called "evo" does not capture evo's own names:
        // its tools are qualified, and an unqualified name that is not one of
        // ours is somebody else's business.
        let refused = tools
            .call(EVO, "some_other_tool", json!({}))
            .expect_err("no such tool on that server");
        assert!(
            refused.contains("true"),
            "the call went out to the configured server rather than being \
             answered as one of evo's own: {refused}"
        );

        std::fs::remove_dir_all(dir).ok();
    }
}
