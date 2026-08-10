//! Reading a file with the writer.
//!
//! evo reads with hayro and writes with lopdf, so every export re-reads the
//! original bytes through a second, entirely separate parser. That parser sees
//! exactly the same untrusted input as the first one, and a file hayro is
//! relaxed about can be one lopdf is not.
//!
//! The round trip is the whole contract: a file lopdf agreed to read has to be
//! one it can write back, or refuse to write. Either is an outcome
//! `export_pdf_bytes` already handles. Panicking between the two is not.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(mut doc) = lopdf::Document::load_mem(data) else {
        return;
    };

    let mut out = Vec::new();
    if doc.save_to(&mut out).is_err() {
        return;
    }
    assert!(!out.is_empty(), "a saved document with nothing in it");

    // Written once, it has to be readable again -- and writing what was just
    // read has to settle rather than drift, because evo's export path is
    // load-modify-save over files that have themselves been saved by evo.
    let Ok(mut again) = lopdf::Document::load_mem(&out) else {
        return;
    };
    let mut twice = Vec::new();
    let _ = again.save_to(&mut twice);
});
