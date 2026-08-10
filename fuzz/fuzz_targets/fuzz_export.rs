//! Saving a copy.
//!
//! Export is load-modify-save: the original bytes are read again by lopdf, the
//! markup layer is written in as annotations or baked into the page content,
//! page rotation and order are applied, and the result is written out. All of
//! that runs over a file evo did not create, so all of it is fuzzed.
//!
//! The property is stronger than "does not crash": a file that export agreed to
//! write has to be one evo can open again. An export that silently produces
//! something unreadable is worse than one that refuses, because the person
//! finds out when they need the file rather than when they save it.

#![no_main]

use std::sync::Arc;

use evo::doc::Document;
use evo::doc::annotation::{Annotation, AnnotationKind, Style};
use evo::doc::geometry::{PdfPoint, PdfRect};
use evo::doc::page_ops::PageList;
use evo::doc::store::AnnotationStore;
use evo::export::pdf::{ExportOptions, export_pdf_bytes};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first byte chooses how to save; the rest is the document. Splitting
    // the input this way costs nothing and gets both writing paths fuzzed by
    // one corpus instead of two.
    let (flatten, bytes) = match data.split_first() {
        Some((first, rest)) => (first & 1 == 1, rest),
        None => return,
    };

    let Ok(doc) = Document::load_bytes(bytes.to_vec(), None) else {
        return;
    };
    let pages = PageList::new(doc.pages.len());
    let options = ExportOptions {
        flatten,
        ocr_layers: None,
    };

    // Twice: an empty markup layer, which is what "save a copy" does, and one
    // holding a shape of every kind, which is what a marked-up drawing does.
    for store in [AnnotationStore::default(), marked_up(doc.pages.len())] {
        let Ok(out) = export_pdf_bytes(&doc, &pages, &store, options.clone()) else {
            continue;
        };
        assert!(
            hayro::hayro_syntax::Pdf::new(Arc::new(out)).is_ok(),
            "export wrote a file evo cannot open again"
        );
    }
});

/// One annotation of every kind evo can make, spread over the pages there are.
///
/// Fixed rather than fuzzed: the *markup* side is covered by the proptest suite
/// and by `fuzz_markup_json`, and what this target is looking for is a bad
/// interaction between untrusted document structure and the appearance streams
/// evo writes into it. Holding the markup still is what makes a crash here
/// point at the document.
fn marked_up(pages: usize) -> AnnotationStore {
    let rect = PdfRect::from_points(PdfPoint::new(20.0, 20.0), PdfPoint::new(180.0, 120.0));
    let points = vec![
        PdfPoint::new(20.0, 20.0),
        PdfPoint::new(120.0, 60.0),
        PdfPoint::new(60.0, 140.0),
    ];
    let kinds = [
        AnnotationKind::Highlight,
        AnnotationKind::Rect,
        AnnotationKind::Ellipse,
        AnnotationKind::Line {
            p1: points[0],
            p2: points[1],
            arrow_end: true,
        },
        AnnotationKind::Freehand {
            points: points.clone(),
        },
        AnnotationKind::Polygon {
            points: points.clone(),
            cloudy: Some(1.5),
        },
        AnnotationKind::PolyLine {
            points,
            arrow_end: true,
        },
        AnnotationKind::Stamp {
            text: "APPROVED".to_owned(),
            font_size: 20.0,
        },
    ];

    let annotations = kinds
        .into_iter()
        .enumerate()
        .map(|(n, kind)| Annotation {
            id: n as u64 + 1,
            page: n % pages.max(1),
            kind,
            rect,
            style: Style::default(),
            group: None,
        })
        .collect();
    AnnotationStore::restore(annotations)
}
