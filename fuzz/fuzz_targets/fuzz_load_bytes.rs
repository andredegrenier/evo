//! Opening a file.
//!
//! `Document::load_bytes` is evo's front door: everything opened from disk,
//! dropped on the window, uploaded to `evo serve` or handed to an agent arrives
//! through it. Its contract is that it answers -- a document, or a sentence
//! saying why not -- for any bytes at all.
//!
//! Both doors are walked, because they are different code. Without a password
//! a protected file is refused at the `/Encrypt` dictionary; with one, a key is
//! derived and a cipher runs over lengths that came out of the file, and only
//! then does the ordinary parser see anything. `evo` is the password on the
//! committed encrypted fixtures, so seeding the corpus with those reaches it.

#![no_main]

use evo::doc::Document;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    check(Document::load_bytes(data.to_vec(), None).ok());
    check(Document::load_bytes_with_password(data.to_vec(), None, Some("evo")).ok());
});

/// What a document that opened has to be true of.
///
/// These are not decoration. `PageInfo` is what the canvas fits the view to and
/// what the exporter builds `/MediaBox` from, so a page of zero or infinite
/// size from a damaged `/MediaBox` is a division by zero somewhere a long way
/// from the file that caused it -- exactly the sort of bug a fuzzer exists to
/// find while the file is still in hand.
fn check(doc: Option<Document>) {
    let Some(doc) = doc else { return };
    assert!(
        !doc.pages.is_empty(),
        "a document that opened with no pages"
    );
    for (n, page) in doc.pages.iter().enumerate() {
        assert!(
            page.width.is_finite() && page.width > 0.0,
            "page {n} is {} wide",
            page.width
        );
        assert!(
            page.height.is_finite() && page.height > 0.0,
            "page {n} is {} high",
            page.height
        );
        assert!(
            [0, 90, 180, 270].contains(&page.intrinsic_rotation),
            "page {n} claims {} degrees",
            page.intrinsic_rotation
        );
        assert!(
            page.crop_origin.0.is_finite() && page.crop_origin.1.is_finite(),
            "page {n} has crop origin {:?}",
            page.crop_origin
        );
    }
}
