//! Reading the words out.
//!
//! Text extraction is the deepest walk evo takes through a document: every
//! content stream interpreted operator by operator, every font parsed, every
//! glyph looked up in a character map. It is also the least visible -- it runs
//! on a background thread when a document joins the library, so a panic in it
//! takes down the indexer rather than showing anybody an error.
//!
//! Its contract with a damaged file is that it returns whatever it could read,
//! including nothing.

#![no_main]

use std::sync::Arc;

use evo::library::extract::extract_all_pages;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let source = Arc::new(data.to_vec());
    for page in extract_all_pages(&source, None) {
        // A page of text is bounded by the page: an extractor that looped would
        // otherwise be found by the memory limit rather than by this.
        assert!(
            page.len() < 64 * 1024 * 1024,
            "{} bytes from one page",
            page.len()
        );
    }
    // Again with a password, for the corpus entries that are encrypted: without
    // one, none of the decrypted content streams are ever interpreted.
    let _ = extract_all_pages(&source, Some("evo"));
});
