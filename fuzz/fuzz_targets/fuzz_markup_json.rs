//! Reading markup somebody else wrote.
//!
//! The annotation layer is the one part of evo's state that arrives as a
//! document from outside: a phone PUTs it, an agent PUTs it, and a sidecar file
//! on disk could have been written by anything. Everything downstream of that
//! read -- the overlay the phone draws, the annotations an export writes, the
//! shapes the canvas paints -- trusts what came out of it.
//!
//! Two properties, and the second is the one with teeth. A version tag that
//! moved without an edit looks to a phone exactly like somebody else having
//! edited the document, and markup that cannot be written back out is markup
//! that is lost the next time it is read. That is how the coordinate overflow
//! was found: `{"x": 1e40}` is well-formed JSON that lands in an `f32` as
//! infinity, comes back out as `null`, and takes the whole layer with it.

#![no_main]

use std::sync::Arc;

use evo::doc::Document;
use evo::doc::annotation::Annotation;
use evo::doc::page_ops::PageList;
use evo::doc::store::AnnotationStore;
use evo::export::pdf::{ExportOptions, export_pdf_bytes};
use evo::library::SavedMarkup;
use evo::serve::markup_api::{MarkupBody, etag};
use libfuzzer_sys::fuzz_target;

/// The document the markup is exported onto. Built in rather than read from
/// disk so the target runs anywhere libFuzzer puts it.
const SAMPLE: &[u8] = include_bytes!("../../tests/fixtures/sample.pdf");

fuzz_target!(|data: &[u8]| {
    // Everything a stranger's bytes can be read as.
    let _ = serde_json::from_slice::<PageList>(data);
    let _ = serde_json::from_slice::<Annotation>(data);
    let _ = serde_json::from_slice::<SavedMarkup>(data);

    let Ok(body) = serde_json::from_slice::<MarkupBody>(data) else {
        return;
    };

    // A body evo refuses is a body evo never has to write back out, so only
    // the ones it would accept carry the round-trip promise.
    if !body.annotations.iter().all(Annotation::is_finite) {
        return;
    }

    let markup = SavedMarkup::new(
        body.annotations,
        body.pages.unwrap_or_else(|| PageList::new(0)),
    );
    let tag = etag(&markup);
    assert_eq!(
        tag,
        etag(&markup),
        "the tag is not a function of the markup"
    );

    let json = serde_json::to_string(&markup).expect("markup serializes");
    let back: SavedMarkup = serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("evo wrote markup it cannot read: {e} in {json}"));
    assert_eq!(etag(&back), tag, "the version tag moved without an edit");

    // And onto a real page, both ways of writing it: markup that cannot be
    // exported is markup somebody loses when they save a copy.
    let Ok(doc) = Document::load_bytes(SAMPLE.to_vec(), None) else {
        return;
    };
    let pages = PageList::new(doc.pages.len());
    let store = AnnotationStore::restore(back.annotations);
    for flatten in [false, true] {
        let options = ExportOptions {
            flatten,
            ocr_layers: None,
        };
        let out = export_pdf_bytes(&doc, &pages, &store, options).expect("export of a fixture");
        assert!(
            hayro::hayro_syntax::Pdf::new(Arc::new(out)).is_ok(),
            "export wrote a file evo cannot open again"
        );
    }
});
