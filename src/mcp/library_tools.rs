//! The library tools themselves, written once against [`Library`].
//!
//! The in-app server reaches them from the UI thread through the bridge and
//! `evo mcp-serve` calls them directly, so the answer a client gets is the same
//! either way -- there is no "headless version" of what a document is.

use serde_json::{Value, json};

use crate::library::Library;

/// The most pages one call will quote, however wide a range is asked for. A
/// model that wants a whole book can ask again; a model that asks for one by
/// accident should not get one.
pub const MAX_PAGES: usize = 40;

pub fn list_library(lib: &Library) -> Result<Value, String> {
    let docs = lib.list().map_err(|e| e.to_string())?;
    let documents: Vec<Value> = docs
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "title": d.title,
                "pages": d.page_count,
                "tags": d.all_tags(),
                "summary": d.summary,
            })
        })
        .collect();
    Ok(json!({ "count": documents.len(), "documents": documents }))
}

pub fn search_library(lib: &Library, query: &str, limit: usize) -> Result<Value, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("a search needs something to search for".to_owned());
    }
    let hits = lib.search(query).map_err(|e| e.to_string())?;
    let limit = limit.clamp(1, 50);
    let matches: Vec<Value> = hits
        .iter()
        .take(limit)
        .map(|hit| {
            json!({
                "doc_id": hit.doc_id,
                "title": hit.title,
                // Pages are 1-based everywhere a person or a model sees them.
                "page": hit.page + 1,
                "snippet": hit.snippet,
                "from_summary": hit.is_summary,
            })
        })
        .collect();
    Ok(json!({ "query": query, "count": matches.len(), "matches": matches }))
}

/// The text of `first..=last` (1-based, inclusive). `last` of `None` means
/// "as much as one call will give", which is what a caller that just wants to
/// read the document should ask for.
pub fn document_text(
    lib: &Library,
    doc_id: &str,
    first: Option<usize>,
    last: Option<usize>,
) -> Result<Value, String> {
    let Some(meta) = lib.doc(doc_id).map_err(|e| e.to_string())? else {
        return Err(format!(
            "there is no document with id {doc_id} in the library; \
             list_library gives the ids"
        ));
    };
    if !lib.is_indexed(doc_id).map_err(|e| e.to_string())? {
        return Err(format!(
            "the text of “{}” has not been read yet -- evo indexes documents \
             in the background, and scanned pages have to be recognized first. \
             Try again in a moment.",
            meta.title
        ));
    }

    let (first, last) = page_range(meta.page_count, first, last);
    let texts = lib
        .page_texts(doc_id, first - 1..last)
        .map_err(|e| e.to_string())?;
    let page_values: Vec<Value> = texts
        .iter()
        .enumerate()
        .map(|(i, text)| json!({ "page": first + i, "text": text }))
        .collect();
    Ok(json!({
        "doc_id": doc_id,
        "title": meta.title,
        "total_pages": meta.page_count,
        "first_page": first,
        "last_page": last,
        "truncated": last < meta.page_count,
        "pages": page_values,
    }))
}

/// Which pages one call actually returns: 1-based and inclusive, inside the
/// document, and never more than [`MAX_PAGES`] of them.
fn page_range(total_pages: usize, first: Option<usize>, last: Option<usize>) -> (usize, usize) {
    let total = total_pages.max(1);
    let first = first.unwrap_or(1).clamp(1, total);
    let last = last.unwrap_or(usize::MAX).clamp(first, total);
    (first, last.min(first + MAX_PAGES - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_library(name: &str) -> (Library, PathBuf) {
        let dir = std::env::temp_dir().join(format!("evo-mcp-lib-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Library::open_at(dir.clone()).unwrap(), dir)
    }

    /// Index the fixture's pages by hand: the tools read the index, and the
    /// indexer thread is not part of what is being tested here.
    fn index_pages(lib: &Library, id: &str, title: &str, pages: &[&str]) {
        let index =
            crate::library::search::SearchIndex::open_or_create(&lib.root.join("index")).unwrap();
        let mut writer = index.writer().unwrap();
        let texts: Vec<String> = pages.iter().map(|p| (*p).to_owned()).collect();
        index
            .index_document(&mut writer, id, title, &texts, None)
            .unwrap();
    }

    #[test]
    fn listing_gives_the_ids_the_other_tools_take() {
        let (lib, dir) = temp_library("list");
        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        let value = list_library(&lib).expect("listed");
        assert_eq!(value["count"], 1);
        assert_eq!(value["documents"][0]["id"], meta.id);
        assert_eq!(value["documents"][0]["pages"], 2);
        assert!(value["documents"][0]["summary"].is_null());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn search_numbers_pages_the_way_a_reader_does() {
        let (lib, dir) = temp_library("search");
        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        index_pages(
            &lib,
            &meta.id,
            "sample",
            &["nothing here", "boiler pressure"],
        );

        let value = search_library(&lib, "boiler", 10).expect("searched");
        assert_eq!(value["count"], 1);
        assert_eq!(
            value["matches"][0]["page"], 2,
            "the second page is page 2, not page 1"
        );
        assert_eq!(value["matches"][0]["doc_id"], meta.id);
        assert_eq!(value["matches"][0]["from_summary"], false);

        // An empty query is a mistake worth naming rather than 50 random hits.
        let err = search_library(&lib, "  ", 10).expect_err("nothing to search for");
        assert!(err.contains("something to search for"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn text_comes_back_by_page_and_stops_at_the_end_of_the_document() {
        let (lib, dir) = temp_library("text");
        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        index_pages(
            &lib,
            &meta.id,
            "sample",
            &["page one text", "page two text"],
        );

        let value = document_text(&lib, &meta.id, None, None).expect("read");
        assert_eq!(value["total_pages"], 2);
        assert_eq!(value["first_page"], 1);
        assert_eq!(value["last_page"], 2);
        assert_eq!(value["truncated"], false);
        assert_eq!(value["pages"][0]["page"], 1);
        assert_eq!(value["pages"][0]["text"], "page one text");
        assert_eq!(value["pages"][1]["page"], 2);

        // A range past the end is clamped, not an error.
        let value = document_text(&lib, &meta.id, Some(2), Some(99)).expect("read");
        assert_eq!(value["first_page"], 2);
        assert_eq!(value["last_page"], 2);
        assert_eq!(value["pages"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    /// A wide range is cut down rather than answered with a whole book, and a
    /// nonsense one still names real pages.
    #[test]
    fn the_page_range_stays_inside_the_document_and_inside_the_cap() {
        assert_eq!(page_range(10, None, None), (1, 10));
        assert_eq!(page_range(10, Some(3), Some(5)), (3, 5));
        assert_eq!(
            page_range(10, Some(0), Some(99)),
            (1, 10),
            "clamped both ends"
        );
        assert_eq!(
            page_range(10, Some(8), Some(2)),
            (8, 8),
            "a backwards range reads the one page it names"
        );
        assert_eq!(page_range(500, Some(1), Some(500)), (1, MAX_PAGES));
        assert_eq!(page_range(500, None, None), (1, MAX_PAGES));
        assert_eq!(page_range(0, None, None), (1, 1), "an empty document");
    }

    /// The two ways a document can be unreadable are told apart, because the
    /// answer to each is different: one is a wrong id, the other is "wait".
    #[test]
    fn an_unknown_id_and_an_unindexed_document_say_different_things() {
        let (lib, dir) = temp_library("errors");
        let err = document_text(&lib, "nope", None, None).expect_err("unknown");
        assert!(err.contains("no document with id"), "{err}");
        assert!(err.contains("list_library"), "{err}");

        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        let err = document_text(&lib, &meta.id, None, None).expect_err("not indexed");
        assert!(err.contains("has not been read yet"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }
}
