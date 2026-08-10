//! Properties: what has to hold for *every* input, including the inputs
//! nobody would send on purpose.
//!
//! The ordinary tests in this repository each pin one example. These pin a
//! rule over a whole family of inputs at once, and the family is chosen to be
//! hostile: bytes that are not a PDF, a real PDF that stops in the middle, a
//! real PDF with a few bits knocked over, markup with no points in it and
//! rectangles with no area. What is being asserted is nearly always the same
//! modest thing -- **evo answers rather than crashing** -- because a PDF editor
//! that panics on a damaged file loses whatever the person was doing to
//! everything else they had open.
//!
//! ## Why the runner is built by hand
//!
//! `proptest!` seeds itself from the operating system, so a property that only
//! fails on one input in ten thousand fails on a random build and passes on the
//! next one. That is not a test, it is a coin. Every property here runs on
//! [`runner`], whose randomness is fixed: the same cases run on this machine,
//! on CI, and on a laptop in a year's time, so a green run means something and
//! a red one is reproducible by anybody. New cases come from raising the case
//! count or widening a strategy -- deliberately, in a commit -- and never from
//! the weather.
//!
//! Failures are still persisted under `proptest-regressions/`, and that file is
//! committed: its seeds replay before any new case, so something a wider local
//! run found keeps being checked. A crasher worth keeping gets a fixture in
//! `tests/fixtures/broken/` as well -- a named file and a named test say what
//! is wrong with it, which a seed does not.
//!
//! ## Budget
//!
//! The whole module is meant to stay a couple of seconds in a debug `cargo
//! test`, because it runs on every push. Case counts are sized for that, not
//! for exhaustiveness -- exhaustiveness is `fuzz/`'s job, and it has fifteen
//! minutes a target and a nightly compiler to do it with.

use std::sync::Arc;

use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, RngAlgorithm, TestRng, TestRunner};

use crate::doc::annotation::{Annotation, AnnotationKind, Color, Style, TextAlign};
use crate::doc::geometry::{PdfPoint, PdfRect};
use crate::doc::page_ops::PageList;
use crate::doc::store::AnnotationStore;
use crate::doc::{Document, LoadError};
use crate::export::pdf::{ExportOptions, export_pdf_bytes};
use crate::library::extract::extract_all_pages;

/// A test runner that draws the same cases every time it is asked.
///
/// `cases` is the budget: these run on every push, so each property spends
/// what its inputs actually cost rather than the default 256.
///
/// `EVO_PROPTEST_MULTIPLIER=50 cargo test robustness` runs fifty times as many
/// of them, which is what to do after touching a parser or before a release.
/// The extra cases are a superset of the committed ones -- same fixed seed,
/// same sequence, just more of it -- so a longer run can find new failures but
/// can never disagree with a short one.
fn runner(cases: u32) -> TestRunner {
    let multiplier: u32 = std::env::var("EVO_PROPTEST_MULTIPLIER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    let config = Config {
        cases: cases.saturating_mul(multiplier),
        // Shrinking a whole PDF can wander a long way. A bounded search still
        // reports the failure; it just may report a slightly larger case.
        max_shrink_iters: 2_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "proptest-regressions",
        ))),
        source_file: Some(file!()),
        ..Config::default()
    };
    TestRunner::new_with_rng(config, TestRng::deterministic_rng(RngAlgorithm::ChaCha))
}

/// The document every mutation starts from.
fn sample() -> Vec<u8> {
    std::fs::read("tests/fixtures/sample.pdf").expect("the sample fixture")
}

/// A protected document, for the mutations that land in the decryption path.
/// AES-256 because it is the longest walk: key derivation, then a cipher, then
/// the ordinary parser over plaintext that damage has turned into noise.
fn encrypted_sample() -> Vec<u8> {
    std::fs::read("tests/fixtures/encrypted-aes256.pdf").expect("the encrypted fixture")
}

// ---------------------------------------------------------------------------
// Strategies: the shapes of bad input
// ---------------------------------------------------------------------------

/// Bytes nobody meant anything by. Almost all of these are refused at the
/// header, which is the point: the refusal has to be a refusal.
fn arbitrary_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..2048)
}

/// A real PDF that stops in the middle -- a half-finished download, a file on
/// a disk that filled up, a `scp` somebody interrupted.
fn truncated_sample() -> impl Strategy<Value = Vec<u8>> {
    let bytes = sample();
    let len = bytes.len();
    (0usize..=len).prop_map(move |n| bytes[..n].to_vec())
}

/// A real PDF with a handful of bits knocked over: the file is still the right
/// length and still the right shape, so the parser gets a long way in before it
/// meets the damage. This is the mutation that reaches the interesting code.
fn bitflipped(bytes: Vec<u8>) -> impl Strategy<Value = Vec<u8>> {
    let len = bytes.len().max(1);
    prop::collection::vec((0usize..len, 0u8..8), 1..=8).prop_map(move |flips| {
        let mut out = bytes.clone();
        for (index, bit) in flips {
            out[index] ^= 1 << bit;
        }
        out
    })
}

fn bitflipped_sample() -> impl Strategy<Value = Vec<u8>> {
    bitflipped(sample())
}

/// A real PDF with a stretch of it replaced by something a different length.
///
/// This is the damage bit-flipping cannot do: every byte offset after the
/// splice moves, so the cross-reference table now points into the middle of
/// objects. A reader that trusts those offsets reads a dictionary where a
/// stream should be, which is where the interesting failures live.
fn spliced_sample() -> impl Strategy<Value = Vec<u8>> {
    let bytes = sample();
    let len = bytes.len();
    (
        0usize..len,
        0usize..64,
        prop::collection::vec(any::<u8>(), 0..64),
    )
        .prop_map(move |(at, cut, insert)| {
            let end = (at + cut).min(len);
            let mut out = bytes[..at].to_vec();
            out.extend_from_slice(&insert);
            out.extend_from_slice(&bytes[end..]);
            out
        })
}

/// A protected document with damage in it. The decryption path is the one
/// place evo runs a cipher over attacker-supplied lengths, and a wrong length
/// there is a slice out of bounds rather than a wrong picture.
fn damaged_encrypted() -> impl Strategy<Value = Vec<u8>> {
    bitflipped(encrypted_sample())
}

/// Header-shaped nonsense: a genuine PDF signature and trailer wrapped around
/// bytes that are not a document. Without the header a reader stops at the
/// first byte and everything past it stays unexercised.
fn pdf_shaped_garbage() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..1024).prop_map(|tail| {
        let mut out = b"%PDF-1.7\n".to_vec();
        out.extend_from_slice(&tail);
        out.extend_from_slice(b"\ntrailer\n<< /Size 1 /Root 1 0 R >>\nstartxref\n9\n%%EOF\n");
        out
    })
}

/// Everything above, weighted towards the mutations that get furthest in.
fn hostile_pdf() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => arbitrary_bytes(),
        2 => truncated_sample(),
        4 => bitflipped_sample(),
        3 => spliced_sample(),
        2 => pdf_shaped_garbage(),
        2 => damaged_encrypted(),
    ]
}

/// The mutations that stay a plausible file, for the properties about what
/// happens *after* a document opens.
fn mutated_sample() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => bitflipped_sample(),
        2 => spliced_sample(),
        1 => truncated_sample(),
    ]
}

// ---------------------------------------------------------------------------
// Strategies: the shapes of bad markup
// ---------------------------------------------------------------------------

/// Coordinates, including the ones a person could not have drawn: exact zero,
/// a million points off the page, the smallest number that is not one.
///
/// Never NaN and never infinite. Those are not reachable through the JSON the
/// API speaks -- serde writes them as `null` and refuses to read them back --
/// so a property that generated them would be testing a state the program
/// cannot be in.
fn coord() -> impl Strategy<Value = f32> {
    prop_oneof![
        6 => -1000.0f32..1000.0f32,
        1 => Just(0.0f32),
        1 => Just(1.0e6f32),
        1 => Just(-1.0e6f32),
        1 => Just(f32::MIN_POSITIVE),
    ]
}

fn point() -> impl Strategy<Value = PdfPoint> {
    (coord(), coord()).prop_map(|(x, y)| PdfPoint::new(x, y))
}

/// Point lists including the empty one. A polygon with no vertices is what an
/// agent sends when its loop ran zero times, and every drawing path has to
/// survive it.
fn points() -> impl Strategy<Value = Vec<PdfPoint>> {
    prop::collection::vec(point(), 0..8)
}

/// Rectangles, a quarter of which have no area at all.
fn rect() -> impl Strategy<Value = PdfRect> {
    prop_oneof![
        3 => (point(), point()).prop_map(|(a, b)| PdfRect::from_points(a, b)),
        1 => point().prop_map(|p| PdfRect::from_points(p, p)),
    ]
}

fn color() -> impl Strategy<Value = Color> {
    (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>())
        .prop_map(|(r, g, b, a)| Color::rgba(r, g, b, a))
}

fn style() -> impl Strategy<Value = Style> {
    (
        color(),
        prop_oneof![Just(0.0f32), 0.1f32..20.0f32, Just(1.0e5f32)],
        color(),
        0.0f32..=1.0f32,
    )
        .prop_map(|(stroke, stroke_width, fill, opacity)| Style {
            stroke,
            stroke_width,
            fill,
            opacity,
        })
}

/// Text that has to survive being written into a PDF string: empty, plain
/// ASCII, the characters PDF syntax itself uses, and scripts outside Latin-1.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        1 => Just(String::new()),
        4 => "[a-zA-Z0-9 .,-]{0,30}",
        2 => r"[()\\\r\n\t]{0,12}",
        2 => r"[À-ſΑ-ω一-丯]{0,12}",
    ]
}

fn font_size() -> impl Strategy<Value = f32> {
    prop_oneof![Just(0.0f32), 1.0f32..200.0f32, Just(1.0e5f32)]
}

/// Bytes offered as a picture: a real PNG at a few sizes, one that is not a
/// PNG at all, and none.
fn stamp_png() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        4 => (1u32..24, 1u32..24).prop_map(|(w, h)| crate::export::pdf::tests::png_fixture(w, h)),
        1 => Just(Vec::new()),
        1 => prop::collection::vec(any::<u8>(), 1..64),
    ]
}

fn kind() -> impl Strategy<Value = AnnotationKind> {
    prop_oneof![
        Just(AnnotationKind::Highlight),
        Just(AnnotationKind::Rect),
        Just(AnnotationKind::Ellipse),
        (text(), font_size(), align()).prop_map(|(text, font_size, align)| {
            AnnotationKind::TextBox {
                text,
                font_size,
                align,
            }
        }),
        (point(), point(), any::<bool>()).prop_map(|(p1, p2, arrow_end)| AnnotationKind::Line {
            p1,
            p2,
            arrow_end
        }),
        points().prop_map(|points| AnnotationKind::Freehand { points }),
        (points(), prop::option::of(1.0f32..=2.0f32))
            .prop_map(|(points, cloudy)| AnnotationKind::Polygon { points, cloudy }),
        (points(), any::<bool>())
            .prop_map(|(points, arrow_end)| AnnotationKind::PolyLine { points, arrow_end }),
        (text(), font_size())
            .prop_map(|(text, font_size)| AnnotationKind::Stamp { text, font_size }),
        stamp_png().prop_map(|png| AnnotationKind::ImageStamp { png }),
    ]
}

fn align() -> impl Strategy<Value = TextAlign> {
    prop_oneof![
        Just(TextAlign::Left),
        Just(TextAlign::Center),
        Just(TextAlign::Right),
    ]
}

/// One annotation of any kind evo can make, with geometry that may be nonsense.
///
/// `pages` is how many pages the document it is going onto has; the page index
/// stays inside it so the annotation is actually exported rather than quietly
/// skipped.
pub(crate) fn annotation(pages: usize) -> impl Strategy<Value = Annotation> {
    (1u64..64, 0..pages.max(1), kind(), rect(), style(), group()).prop_map(
        |(id, page, kind, rect, style, group)| Annotation {
            id,
            page,
            kind,
            rect,
            style,
            group,
        },
    )
}

fn group() -> impl Strategy<Value = Option<u64>> {
    prop::option::of(0u64..4)
}

/// A whole markup layer for a document of `pages` pages.
pub(crate) fn annotations(pages: usize) -> impl Strategy<Value = Vec<Annotation>> {
    prop::collection::vec(annotation(pages), 0..6)
}

/// Field names, half of them ones the markup reader is actually looking for.
///
/// Garbage made only of random strings never reaches the interesting code: the
/// reader gives up at the first unknown key. Garbage that says `annotations`
/// and `kind` gets several layers in before it is found out, which is where the
/// mistakes are.
fn json_key() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop::sample::select(vec![
            "version", "annotations", "pages", "id", "page", "kind", "rect", "style", "group",
            "points", "text", "font_size", "cloudy", "arrow_end", "png", "min", "max", "x", "y",
            "order", "states", "source_of", "Highlight", "Polygon", "ImageStamp",
        ])
        .prop_map(str::to_owned),
        1 => "[a-z_]{0,8}",
    ]
}

/// A JSON number well inside `f64` and far outside `f32`.
///
/// This is the input class that bricked a document: a body carrying one of
/// these was accepted, stored with a `null` where the coordinate had been, and
/// made every subsequent read of that document's markup a 500. JSON has one
/// number type and it is the wider one, so any endpoint that reads a
/// coordinate has to expect this.
pub(crate) fn out_of_range_number() -> impl Strategy<Value = serde_json::Value> {
    prop::sample::select(vec![1.0e40f64, -1.0e40, 1.0e308, -1.0e308, 3.5e38])
        .prop_map(serde_json::Value::from)
}

/// JSON of any shape at all, bounded in depth and width so a case is a
/// document rather than a denial of service.
pub(crate) fn json_value() -> impl Strategy<Value = serde_json::Value> {
    use serde_json::Value;
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        (-1.0e9f64..1.0e9f64).prop_map(Value::from),
        json_key().prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 40, 5, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(Value::from),
            prop::collection::vec((json_key(), inner), 0..5)
                .prop_map(|pairs| Value::Object(pairs.into_iter().collect())),
        ]
    })
}

// ---------------------------------------------------------------------------
// Opening documents
// ---------------------------------------------------------------------------

/// Whatever arrives, `Document::load_bytes` answers: a document, or a sentence
/// saying why not. It does not panic, and it does not hand back a document that
/// the rest of the app would then divide by.
///
/// The second half matters as much as the first. `PageInfo` is what the canvas
/// fits the view to and what the exporter builds `/MediaBox` from; a page whose
/// width came back as zero or as infinity from a damaged `/MediaBox` would be a
/// crash somewhere much further away from the bad file.
#[test]
fn opening_hostile_bytes_gives_an_answer_and_never_a_panic() {
    runner(220)
        .run(&hostile_pdf(), |bytes| {
            match Document::load_bytes(bytes, None) {
                Err(
                    LoadError::Invalid
                    | LoadError::Empty
                    | LoadError::NeedsPassword
                    | LoadError::WrongPassword
                    | LoadError::UnsupportedEncryption,
                ) => Ok(()),
                Err(LoadError::Io(e)) => Err(TestCaseError::fail(format!(
                    "nothing in this property reads a file, so this cannot happen: {e}"
                ))),
                Ok(doc) => {
                    prop_assert!(
                        !doc.pages.is_empty(),
                        "an empty document is LoadError::Empty"
                    );
                    for (n, page) in doc.pages.iter().enumerate() {
                        prop_assert!(
                            page.width.is_finite() && page.height.is_finite(),
                            "page {n} is {} x {}",
                            page.width,
                            page.height
                        );
                        prop_assert!(
                            page.width > 0.0 && page.height > 0.0,
                            "page {n} is {} x {}",
                            page.width,
                            page.height
                        );
                        prop_assert!(
                            [0, 90, 180, 270].contains(&page.intrinsic_rotation),
                            "page {n} claims {} degrees",
                            page.intrinsic_rotation
                        );
                        prop_assert!(
                            page.crop_origin.0.is_finite() && page.crop_origin.1.is_finite(),
                            "page {n} has crop origin {:?}",
                            page.crop_origin
                        );
                    }
                    Ok(())
                }
            }
        })
        .expect("Document::load_bytes");
}

/// Opening the same bytes twice is the same answer twice. A parser that
/// depended on uninitialized memory, a hash order or the clock would fail this
/// one and pass everything else.
#[test]
fn opening_the_same_bytes_twice_says_the_same_thing() {
    runner(80)
        .run(&mutated_sample(), |bytes| {
            let first = Document::load_bytes(bytes.clone(), None);
            let second = Document::load_bytes(bytes, None);
            match (first, second) {
                (Ok(a), Ok(b)) => {
                    prop_assert_eq!(a.pages.len(), b.pages.len());
                    for (x, y) in a.pages.iter().zip(&b.pages) {
                        prop_assert_eq!(x.width, y.width);
                        prop_assert_eq!(x.height, y.height);
                        prop_assert_eq!(x.intrinsic_rotation, y.intrinsic_rotation);
                    }
                    Ok(())
                }
                (Err(_), Err(_)) => Ok(()),
                (a, b) => Err(TestCaseError::fail(format!(
                    "the same bytes read twice gave {:?} then {:?}",
                    a.err(),
                    b.err()
                ))),
            }
        })
        .expect("Document::load_bytes");
}

/// A damaged protected document, opened with the right password.
///
/// Without a password the reader stops at the `/Encrypt` dictionary and none of
/// the cryptography runs. With one it derives a key, runs a cipher over lengths
/// that came out of the damaged file, and only then parses the result -- which
/// is the deepest and least-travelled path in the whole reader, and the only
/// one where a wrong number is a slice out of bounds rather than a wrong
/// picture.
#[test]
fn a_damaged_protected_document_is_refused_rather_than_fatal() {
    runner(120)
        .run(&damaged_encrypted(), |bytes| {
            match Document::load_bytes_with_password(bytes.clone(), None, Some("evo")) {
                Err(_) => Ok(()),
                Ok(doc) => {
                    prop_assert!(!doc.pages.is_empty());
                    // Whatever came out of the cipher is now ordinary input to
                    // everything downstream, so run the two deepest readers
                    // over it as well.
                    let _ = extract_all_pages(&Arc::new(bytes), Some("evo"));
                    let pages = PageList::new(doc.pages.len());
                    let store = AnnotationStore::default();
                    let _ = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default());
                    Ok(())
                }
            }
        })
        .expect("load_bytes_with_password");
}

// ---------------------------------------------------------------------------
// Reading text
// ---------------------------------------------------------------------------

/// Extraction is what the chat, the search index and the OCR decision all run
/// first, and it is the deepest walk evo takes through a file: every content
/// stream, every font, every glyph. Its contract with a damaged document is
/// that it returns whatever it could read -- possibly nothing -- and never
/// takes the process down.
#[test]
fn reading_the_text_of_a_damaged_document_never_panics() {
    runner(120)
        .run(&hostile_pdf(), |bytes| {
            let pages = extract_all_pages(&Arc::new(bytes), None);
            for page in &pages {
                // Whatever came out is text, which is the only thing the index
                // and the model can be handed.
                prop_assert!(page.len() < 8_000_000, "{} bytes from one page", page.len());
            }
            Ok(())
        })
        .expect("extract_all_pages");
}

// ---------------------------------------------------------------------------
// Writing documents back out
// ---------------------------------------------------------------------------

/// lopdf is the writing half of evo: everything exported is read by it first.
/// A file that it agrees to read has to be one it can then write, or refuse to
/// write -- either is an outcome the export path already handles.
#[test]
fn rewriting_what_lopdf_agreed_to_read_never_panics() {
    runner(120)
        .run(&mutated_sample(), |bytes| {
            let Ok(mut lo) = lopdf::Document::load_mem(&bytes) else {
                return Ok(());
            };
            let mut out = Vec::new();
            if lo.save_to(&mut out).is_ok() {
                prop_assert!(!out.is_empty(), "a saved document with nothing in it");
            }
            Ok(())
        })
        .expect("lopdf round trip");
}

/// Export on a document that opened but is damaged underneath. hayro is
/// forgiving about structure it does not need; lopdf is not, so plenty of these
/// fail -- and failing is fine. Panicking is not, and neither is writing a file
/// that nothing can open afterwards.
#[test]
fn exporting_a_damaged_document_either_refuses_or_writes_a_readable_file() {
    runner(80)
        .run(&mutated_sample(), |bytes| {
            let Ok(doc) = Document::load_bytes(bytes, None) else {
                return Ok(());
            };
            let pages = PageList::new(doc.pages.len());
            let store = AnnotationStore::default();
            let Ok(out) = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()) else {
                return Ok(());
            };
            prop_assert!(
                hayro::hayro_syntax::Pdf::new(Arc::new(out)).is_ok(),
                "export wrote a file evo cannot open again"
            );
            Ok(())
        })
        .expect("export_pdf_bytes");
}

/// The property the markup staples were built for: any annotation evo can
/// make, however degenerate, goes into a PDF that opens again.
///
/// Both ways of writing it. Annotations and flattening are two completely
/// separate drawing paths -- one builds appearance streams, the other appends
/// operators to the page content -- and a shape with no points in it has to
/// leave both of them alone rather than emitting half a path.
#[test]
fn any_markup_at_all_exports_to_a_file_that_opens_again() {
    let doc = Document::load_bytes(sample(), None).expect("the sample fixture");
    let page_count = doc.pages.len();
    runner(120)
        .run(
            &(annotations(page_count), any::<bool>()),
            |(annotations, flatten)| {
                let pages = PageList::new(page_count);
                let store = AnnotationStore::restore(annotations);
                let options = ExportOptions {
                    flatten,
                    ocr_layers: None,
                };
                let out = export_pdf_bytes(&doc, &pages, &store, options)
                    .map_err(|e| TestCaseError::fail(format!("export refused a fixture: {e}")))?;
                prop_assert!(
                    hayro::hayro_syntax::Pdf::new(Arc::new(out)).is_ok(),
                    "export wrote a file evo cannot open again"
                );
                Ok(())
            },
        )
        .expect("export_pdf_bytes with markup");
}

/// Markup survives the sidecar. Everything drawn on a phone or by an agent
/// reaches the desktop app through JSON, so a shape that cannot make that
/// round trip is a shape that quietly disappears.
#[test]
fn any_markup_at_all_survives_being_written_down_and_read_back() {
    runner(150)
        .run(&annotations(3), |annotations| {
            let json = serde_json::to_string(&annotations)
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            let back: Vec<Annotation> = serde_json::from_str(&json)
                .map_err(|e| TestCaseError::fail(format!("{e} in {json}")))?;
            prop_assert_eq!(back, annotations);
            Ok(())
        })
        .expect("annotation serde round trip");
}

// ---------------------------------------------------------------------------
// Reading markup somebody else wrote
// ---------------------------------------------------------------------------

/// The markup reader is reached by three different strangers: a phone, an
/// agent over HTTP, and a sidecar file on disk that anything could have
/// written. Whatever it is handed, it either understands it or says it does
/// not; it never panics on the way to deciding.
#[test]
fn reading_arbitrary_json_as_markup_is_an_answer_and_never_a_panic() {
    runner(300)
        .run(&json_value(), |value| {
            let text = value.to_string();
            let _ = serde_json::from_str::<crate::serve::markup_api::MarkupBody>(&text);
            let _ = serde_json::from_str::<crate::library::SavedMarkup>(&text);
            let _ = serde_json::from_str::<Annotation>(&text);
            let _ = serde_json::from_str::<PageList>(&text);
            Ok(())
        })
        .expect("markup serde");
}

/// The version tag is a fact about the content and about nothing else: the
/// same markup has the same tag in another process and after a round trip
/// through the file it is stored in.
///
/// The desktop app and the server both compute it, and a client's write is
/// refused when the two disagree -- so a tag that moved for a reason other
/// than an edit would look, from the phone, exactly like somebody else having
/// edited the document.
#[test]
fn the_version_tag_is_the_content_and_survives_the_round_trip() {
    use crate::serve::markup_api::etag;
    runner(200)
        .run(&(annotations(3), 0usize..5), |(annotations, pages)| {
            let markup = crate::library::SavedMarkup::new(annotations, PageList::new(pages));
            let tag = etag(&markup);
            prop_assert_eq!(
                &tag,
                &etag(&markup),
                "the tag is not a function of the markup"
            );

            let json =
                serde_json::to_string(&markup).map_err(|e| TestCaseError::fail(format!("{e}")))?;
            let back: crate::library::SavedMarkup = serde_json::from_str(&json)
                .map_err(|e| TestCaseError::fail(format!("{e} in {json}")))?;
            prop_assert_eq!(&etag(&back), &tag, "the tag moved without an edit");
            Ok(())
        })
        .expect("etag");
}

// ---------------------------------------------------------------------------
// The committed broken files
// ---------------------------------------------------------------------------

/// Files small enough to read in a diff, each broken in one specific way.
///
/// A property finds these shapes; a fixture keeps one of them forever, under a
/// name that says what is wrong with it, so a regression is a named test
/// failing rather than a random seed that happens to hit it again. Minimized
/// crashers from `fuzz/` are committed here too.
const BROKEN: &[&str] = &[
    "empty.pdf",
    "garbage-header.pdf",
    "header-only.pdf",
    "truncated-xref.pdf",
    "bad-stream-length.pdf",
    "no-pages.pdf",
    "circular-page-tree.pdf",
    "xref-prev-loop.pdf",
    "absurd-object-count.pdf",
    "negative-mediabox.pdf",
    "encrypt-length-overruns-md5.pdf",
];

/// The one broken file that is not slow to *read* but is slow to *write*, kept
/// out of [`BROKEN`] because the writer test would then take four seconds
/// instead of a tenth of one. See
/// [`saving_a_mangled_xref_finishes_but_takes_seconds`].
const SLOW_TO_SAVE: &str = "xref-slow-lopdf-save.pdf";

fn broken(name: &str) -> Vec<u8> {
    std::fs::read(format!("tests/fixtures/broken/{name}"))
        .unwrap_or_else(|e| panic!("the {name} fixture: {e}"))
}

/// Every broken file is refused, or opens into something usable. Nothing
/// panics, nothing loops forever, nothing comes back claiming a page of
/// infinite width.
#[test]
fn every_broken_fixture_is_an_error_and_not_a_crash() {
    for name in BROKEN.iter().chain(std::iter::once(&SLOW_TO_SAVE)) {
        match Document::load_bytes(broken(name), None) {
            Err(_) => {}
            Ok(doc) => {
                assert!(!doc.pages.is_empty(), "{name}");
                for page in &doc.pages {
                    assert!(
                        page.width.is_finite() && page.width > 0.0,
                        "{name}: width {}",
                        page.width
                    );
                    assert!(
                        page.height.is_finite() && page.height > 0.0,
                        "{name}: height {}",
                        page.height
                    );
                }
            }
        }
    }
}

/// The other thing a fuzzer finds: not a crash, a wait.
///
/// libFuzzer reported this 456-byte file as a slow unit. hayro reads it in
/// under a millisecond and lopdf reads it in under one too -- but lopdf takes
/// between three and seven seconds to *write* the 293 bytes that come out the
/// other side. One xref entry is a byte short (`000100000000000 65 `), which
/// desynchronizes the rest of the table, and `/Prev` points back at the section
/// it is in.
///
/// For a person that is Save As on a small damaged file taking the best part of
/// ten seconds. It is upstream, in lopdf's writer, and it does terminate, so
/// this is a witness rather than a fix: the file is committed, the behaviour is
/// written down, and if it ever becomes unbounded rather than merely slow there
/// is something to point at.
///
/// `#[ignore]`d because four seconds is more than the whole rest of this module
/// costs. `cargo test -- --ignored xref` runs it.
#[test]
#[ignore = "spends seconds inside lopdf's writer on purpose"]
fn saving_a_mangled_xref_finishes_but_takes_seconds() {
    let bytes = broken(SLOW_TO_SAVE);
    let mut lo = lopdf::Document::load_mem(&bytes).expect("lopdf reads it happily");

    let started = std::time::Instant::now();
    let mut out = Vec::new();
    lo.save_to(&mut out).expect("and writes it, eventually");
    let took = started.elapsed();

    assert!(!out.is_empty());
    // Not a threshold anybody should tune -- a machine under load can be
    // slower. What is being pinned is that it finishes at all.
    assert!(
        took < std::time::Duration::from_secs(120),
        "lopdf took {took:?} to write {} bytes; it used to take about four seconds, \
         so this has become something worse than slow",
        out.len()
    );
    println!("lopdf wrote {} bytes in {took:?}", out.len());
}

/// The crasher, by name, through the door it came in at.
///
/// One bit of `tests/fixtures/encrypted-aes256.pdf` is different: `/R 6` reads
/// `/R 4`, so a 256-bit `/Length` goes down the revision-4 path and
/// hayro-syntax 0.7.2 asks for the first 32 bytes of a 16-byte MD5 digest and
/// panics. Every parse in evo goes through [`crate::doc::open_pdf`] for this
/// reason; the file is committed so that a hayro upgrade which fixes it, or a
/// refactor which routes around the guard, is a named test either way.
///
/// It is reached with and without a password because the reader tries to
/// decrypt in both cases -- an empty password is still a password to try.
#[test]
fn the_encryption_crasher_is_an_error_and_not_the_end_of_the_process() {
    let bytes = broken("encrypt-length-overruns-md5.pdf");

    let err = Document::load_bytes(bytes.clone(), None)
        .err()
        .expect("refused");
    assert!(matches!(err, LoadError::Invalid), "{err:?}");

    let err = Document::load_bytes_with_password(bytes.clone(), None, Some("evo"))
        .err()
        .expect("refused");
    assert!(matches!(err, LoadError::Invalid), "{err:?}");

    // The other readers reach the same parser and have to survive it too.
    assert!(extract_all_pages(&Arc::new(bytes.clone()), Some("evo")).is_empty());
    assert!(
        crate::render::engine::open(
            Arc::new(bytes),
            Some("evo"),
            crate::render::engine::EnginePref::Hayro,
        )
        .is_err()
    );
}

/// The same files through the other three doors: the text reader, lopdf, and
/// export. A file that gets past one of them must not take another one down.
#[test]
fn every_broken_fixture_survives_the_reader_the_writer_and_export() {
    for name in BROKEN {
        let bytes = broken(name);

        let _ = extract_all_pages(&Arc::new(bytes.clone()), None);

        if let Ok(mut lo) = lopdf::Document::load_mem(&bytes) {
            let mut out = Vec::new();
            let _ = lo.save_to(&mut out);
        }

        if let Ok(doc) = Document::load_bytes(bytes, None) {
            let pages = PageList::new(doc.pages.len());
            let store = AnnotationStore::default();
            if let Ok(out) = export_pdf_bytes(&doc, &pages, &store, ExportOptions::default()) {
                assert!(
                    hayro::hayro_syntax::Pdf::new(Arc::new(out)).is_ok(),
                    "{name}: export wrote a file evo cannot open again"
                );
            }
        }
    }
}
