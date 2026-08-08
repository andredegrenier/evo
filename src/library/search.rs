//! tantivy full-text index over library documents: one index document per
//! page, with stored bodies so result snippets can be generated.
//!
//! Since schema v2 a document may also carry one *meta* document: its summary
//! and automatic tags, indexed as if it were a page so that a search finds a
//! document by what it is about and not only by what it literally says. The
//! `kind` field tells the two apart, and the indexer is still the only writer
//! -- enrichment asks it to (re)write a meta document, it never writes itself.

use std::ops::Range;
use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexWriter, TantivyDocument, Term, doc};

use super::LibraryError;

/// What one index document holds: a page of the PDF, or the summary and tags
/// of the document as a whole.
pub const KIND_PAGE: &str = "page";
pub const KIND_META: &str = "meta";

/// The layout this module writes. Bumping it makes [`super::Library`] throw the
/// index away and rebuild it, which reconciliation does on its own.
pub const SCHEMA_VERSION: u64 = 2;

pub struct SearchIndex {
    index: Index,
    f_doc_id: Field,
    f_page: Field,
    f_title: Field,
    f_body: Field,
    f_kind: Field,
}

/// One search result: a page of a library document, or its summary.
#[derive(Clone)]
pub struct SearchHit {
    pub doc_id: String,
    /// Zero-based page index in the source document.
    pub page: usize,
    pub title: String,
    /// Snippet text with byte ranges to highlight.
    pub snippet: String,
    pub highlights: Vec<std::ops::Range<usize>>,
    /// The match was in the document's summary or tags rather than on a page.
    pub is_summary: bool,
}

fn err(e: impl std::fmt::Display) -> LibraryError {
    LibraryError::Db(format!("search index: {e}"))
}

impl SearchIndex {
    pub fn open_or_create(dir: &Path) -> Result<Self, LibraryError> {
        let mut builder = Schema::builder();
        let f_doc_id = builder.add_text_field("doc_id", STRING | STORED);
        let f_page = builder.add_u64_field("page", INDEXED | STORED | FAST);
        let f_title = builder.add_text_field("title", TEXT | STORED);
        let f_body = builder.add_text_field("body", TEXT | STORED);
        let f_kind = builder.add_text_field("kind", STRING | STORED);
        let schema = builder.build();

        std::fs::create_dir_all(dir)?;
        let index = match Index::open_in_dir(dir) {
            Ok(index) => index,
            Err(_) => Index::create_in_dir(dir, schema.clone()).map_err(err)?,
        };
        Ok(Self {
            index,
            f_doc_id,
            f_page,
            f_title,
            f_body,
            f_kind,
        })
    }

    pub fn writer(&self) -> Result<IndexWriter, LibraryError> {
        self.index.writer(24_000_000).map_err(err)
    }

    /// Replace all pages of `doc_id` with the given page texts. `meta_body` is
    /// the document's summary and tags, if it has any: deleting by `doc_id`
    /// takes the meta document with the pages, so it is written back here.
    pub fn index_document(
        &self,
        writer: &mut IndexWriter,
        doc_id: &str,
        title: &str,
        page_texts: &[String],
        meta_body: Option<&str>,
    ) -> Result<(), LibraryError> {
        writer.delete_term(Term::from_field_text(self.f_doc_id, doc_id));
        for (page, text) in page_texts.iter().enumerate() {
            writer
                .add_document(doc!(
                    self.f_doc_id => doc_id,
                    self.f_page => page as u64,
                    self.f_title => title,
                    self.f_body => text.as_str(),
                    self.f_kind => KIND_PAGE,
                ))
                .map_err(err)?;
        }
        if let Some(body) = meta_body {
            self.add_meta(writer, doc_id, title, body)?;
        }
        writer.commit().map_err(err)?;
        Ok(())
    }

    /// Replace just the meta document of `doc_id`, leaving its pages alone.
    /// `body` of `None` removes it (the summary was cleared).
    pub fn index_meta(
        &self,
        writer: &mut IndexWriter,
        doc_id: &str,
        title: &str,
        body: Option<&str>,
    ) -> Result<(), LibraryError> {
        writer.delete_query(self.meta_query(doc_id)).map_err(err)?;
        if let Some(body) = body {
            self.add_meta(writer, doc_id, title, body)?;
        }
        writer.commit().map_err(err)?;
        Ok(())
    }

    fn add_meta(
        &self,
        writer: &mut IndexWriter,
        doc_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(), LibraryError> {
        writer
            .add_document(doc!(
                self.f_doc_id => doc_id,
                // A summary is about the whole document; page 1 is where
                // opening it from a result should land.
                self.f_page => 0u64,
                self.f_title => title,
                self.f_body => body,
                self.f_kind => KIND_META,
            ))
            .map_err(err)?;
        Ok(())
    }

    fn term_query(&self, field: Field, value: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, value),
            IndexRecordOption::Basic,
        ))
    }

    /// The one meta document of `doc_id`, if it has one.
    fn meta_query(&self, doc_id: &str) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, self.term_query(self.f_doc_id, doc_id)),
            (Occur::Must, self.term_query(self.f_kind, KIND_META)),
        ]))
    }

    pub fn delete_document(
        &self,
        writer: &mut IndexWriter,
        doc_id: &str,
    ) -> Result<(), LibraryError> {
        writer.delete_term(Term::from_field_text(self.f_doc_id, doc_id));
        writer.commit().map_err(err)?;
        Ok(())
    }

    /// Whether any pages of `doc_id` are in the index. A meta document alone
    /// does not count: the pages are what indexing is for.
    pub fn has_document(&self, doc_id: &str) -> Result<bool, LibraryError> {
        use tantivy::collector::Count;
        let reader = self.index.reader().map_err(err)?;
        let searcher = reader.searcher();
        let query = BooleanQuery::new(vec![
            (Occur::Must, self.term_query(self.f_doc_id, doc_id)),
            (Occur::Must, self.term_query(self.f_kind, KIND_PAGE)),
        ]);
        let count = searcher.search(&query, &Count).map_err(err)?;
        Ok(count > 0)
    }

    /// The stored text of the given zero-based pages, in page order. Pages the
    /// index does not hold come back empty, so the result always has one entry
    /// per page of the range.
    ///
    /// Reading text back out of the index costs nothing next to extracting it
    /// from the PDF again, which is why enrichment (and, later, the MCP server)
    /// reads it from here.
    pub fn page_texts(
        &self,
        doc_id: &str,
        range: Range<usize>,
    ) -> Result<Vec<String>, LibraryError> {
        use std::ops::Bound;
        use tantivy::collector::DocSetCollector;
        use tantivy::query::RangeQuery;

        if range.is_empty() {
            return Ok(Vec::new());
        }
        let reader = self.index.reader().map_err(err)?;
        let searcher = reader.searcher();
        let pages = RangeQuery::new(
            Bound::Included(Term::from_field_u64(self.f_page, range.start as u64)),
            Bound::Excluded(Term::from_field_u64(self.f_page, range.end as u64)),
        );
        let query = BooleanQuery::new(vec![
            (Occur::Must, self.term_query(self.f_doc_id, doc_id)),
            (Occur::Must, self.term_query(self.f_kind, KIND_PAGE)),
            (Occur::Must, Box::new(pages) as Box<dyn Query>),
        ]);

        let mut out = vec![String::new(); range.len()];
        for address in searcher.search(&query, &DocSetCollector).map_err(err)? {
            let stored: TantivyDocument = searcher.doc(address).map_err(err)?;
            let page = stored
                .get_first(self.f_page)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let Some(slot) = page.checked_sub(range.start).and_then(|i| out.get_mut(i)) else {
                continue;
            };
            *slot = stored
                .get_first(self.f_body)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
        }
        Ok(out)
    }

    pub fn search(&self, query_text: &str, limit: usize) -> Result<Vec<SearchHit>, LibraryError> {
        let reader = self.index.reader().map_err(err)?;
        let searcher = reader.searcher();
        let mut parser = QueryParser::for_index(&self.index, vec![self.f_title, self.f_body]);
        parser.set_field_fuzzy(self.f_body, true, 1, true);
        let query = parser.parse_query_lenient(query_text).0;

        let top = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(err)?;

        // Snippets need plain terms; the fuzzy query yields none, so parse
        // the query again without fuzziness just for highlighting.
        let plain_parser = QueryParser::for_index(&self.index, vec![self.f_title, self.f_body]);
        let plain_query = plain_parser.parse_query_lenient(query_text).0;
        let snippets = SnippetGenerator::create(&searcher, &plain_query, self.f_body).ok();

        let mut hits = Vec::with_capacity(top.len());
        for (_score, address) in top {
            let stored: TantivyDocument = searcher.doc(address).map_err(err)?;
            let get_str = |field: Field| {
                stored
                    .get_first(field)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned()
            };
            let page = stored
                .get_first(self.f_page)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            let (mut snippet, highlights) = match &snippets {
                Some(generator) => {
                    let snippet = generator.snippet_from_doc(&stored);
                    (
                        snippet.fragment().to_owned(),
                        snippet.highlighted().to_vec(),
                    )
                }
                None => (String::new(), Vec::new()),
            };
            if snippet.is_empty() {
                // Title-only or fuzzy-only match: show the body's beginning.
                let body = get_str(self.f_body);
                let cut = body
                    .char_indices()
                    .nth(120)
                    .map(|(i, _)| i)
                    .unwrap_or(body.len());
                snippet = body[..cut].to_owned();
            }

            hits.push(SearchHit {
                doc_id: get_str(self.f_doc_id),
                page,
                title: get_str(self.f_title),
                snippet,
                highlights,
                is_summary: get_str(self.f_kind) == KIND_META,
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_index(name: &str) -> (SearchIndex, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("evo-search-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (SearchIndex::open_or_create(&dir).unwrap(), dir)
    }

    #[test]
    fn index_and_search_round_trip() {
        let (index, dir) = temp_index("roundtrip");
        let mut writer = index.writer().unwrap();
        index
            .index_document(
                &mut writer,
                "abc123",
                "Fox Manual",
                &[
                    "The quick brown fox jumps over the lazy dog".into(),
                    "Completely unrelated second page".into(),
                ],
                None,
            )
            .unwrap();

        let hits = index.search("quick fox", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc_id, "abc123");
        assert_eq!(hits[0].page, 0);
        assert!(!hits[0].snippet.is_empty());
        assert!(!hits[0].is_summary);

        // Deletion empties the index for that doc.
        index.delete_document(&mut writer, "abc123").unwrap();
        let hits = index.search("quick fox", 10).unwrap();
        assert!(hits.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A summary is searchable in its own right, marked as such, and points at
    /// the first page.
    #[test]
    fn a_meta_document_is_searchable_and_says_it_is_a_summary() {
        let (index, dir) = temp_index("meta");
        let mut writer = index.writer().unwrap();
        index
            .index_document(
                &mut writer,
                "doc1",
                "Boiler Report",
                &["Pressure readings for the north wing".into()],
                None,
            )
            .unwrap();
        index
            .index_meta(
                &mut writer,
                "doc1",
                "Boiler Report",
                Some("An inspection of the heating plant. maintenance, boiler"),
            )
            .unwrap();

        let hits = index.search("maintenance", 10).unwrap();
        assert_eq!(hits.len(), 1, "the tag is findable");
        assert!(hits[0].is_summary);
        assert_eq!(hits[0].page, 0, "a summary opens the document at page 1");

        // The pages are untouched by writing the meta document.
        let hits = index.search("pressure", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].is_summary);

        // Replacing the meta document leaves exactly one.
        index
            .index_meta(
                &mut writer,
                "doc1",
                "Boiler Report",
                Some("maintenance log"),
            )
            .unwrap();
        assert_eq!(index.search("maintenance", 10).unwrap().len(), 1);

        // Clearing it removes it without touching the pages.
        index
            .index_meta(&mut writer, "doc1", "Boiler Report", None)
            .unwrap();
        assert!(index.search("maintenance", 10).unwrap().is_empty());
        assert_eq!(index.search("pressure", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Re-indexing the pages of a document must not lose its summary.
    #[test]
    fn reindexing_pages_carries_the_summary_back_in() {
        let (index, dir) = temp_index("reindex-meta");
        let mut writer = index.writer().unwrap();
        index
            .index_document(
                &mut writer,
                "doc1",
                "T",
                &["first".into()],
                Some("a digest"),
            )
            .unwrap();
        assert!(index.search("digest", 10).unwrap()[0].is_summary);

        index
            .index_document(
                &mut writer,
                "doc1",
                "T",
                &["first again".into()],
                Some("a digest"),
            )
            .unwrap();
        let hits = index.search("digest", 10).unwrap();
        assert_eq!(hits.len(), 1, "one summary, not two");

        // A document with no summary keeps the meta document gone.
        index
            .index_document(&mut writer, "doc1", "T", &["first".into()], None)
            .unwrap();
        assert!(index.search("digest", 10).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stored bodies are the cheapest source of a document's text; enrichment
    /// reads the opening pages back out of the index rather than re-extracting.
    #[test]
    fn stored_page_text_reads_back_in_page_order() {
        let (index, dir) = temp_index("page-texts");
        let mut writer = index.writer().unwrap();
        index
            .index_document(
                &mut writer,
                "doc1",
                "T",
                &["one".into(), "two".into(), "three".into(), "four".into()],
                Some("a summary that is not a page"),
            )
            .unwrap();

        assert_eq!(
            index.page_texts("doc1", 0..3).unwrap(),
            ["one", "two", "three"]
        );
        assert_eq!(index.page_texts("doc1", 1..3).unwrap(), ["two", "three"]);
        // Past the end: one empty entry per missing page, never a short vec.
        assert_eq!(index.page_texts("doc1", 3..6).unwrap(), ["four", "", ""]);
        assert!(index.page_texts("doc1", 0..0).unwrap().is_empty());
        assert_eq!(index.page_texts("nope", 0..2).unwrap(), ["", ""]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A document known only by its summary is not indexed as far as the
    /// reconciliation pass is concerned -- its pages still have to be read.
    #[test]
    fn a_summary_alone_does_not_count_as_an_indexed_document() {
        let (index, dir) = temp_index("has-document");
        let mut writer = index.writer().unwrap();
        index
            .index_meta(&mut writer, "doc1", "T", Some("a digest"))
            .unwrap();
        assert!(!index.has_document("doc1").unwrap());

        index
            .index_document(&mut writer, "doc1", "T", &["page".into()], None)
            .unwrap();
        assert!(index.has_document("doc1").unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }
}
