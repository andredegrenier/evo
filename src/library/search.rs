//! tantivy full-text index over library documents: one index document per
//! page, with stored bodies so result snippets can be generated.

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{FAST, Field, INDEXED, STORED, STRING, Schema, TEXT, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexWriter, TantivyDocument, Term, doc};

use super::LibraryError;

pub struct SearchIndex {
    index: Index,
    f_doc_id: Field,
    f_page: Field,
    f_title: Field,
    f_body: Field,
}

/// One search result: a page of a library document.
#[derive(Clone)]
pub struct SearchHit {
    pub doc_id: String,
    /// Zero-based page index in the source document.
    pub page: usize,
    pub title: String,
    /// Snippet text with byte ranges to highlight.
    pub snippet: String,
    pub highlights: Vec<std::ops::Range<usize>>,
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
        })
    }

    pub fn writer(&self) -> Result<IndexWriter, LibraryError> {
        self.index.writer(24_000_000).map_err(err)
    }

    /// Replace all pages of `doc_id` with the given page texts.
    pub fn index_document(
        &self,
        writer: &mut IndexWriter,
        doc_id: &str,
        title: &str,
        page_texts: &[String],
    ) -> Result<(), LibraryError> {
        writer.delete_term(Term::from_field_text(self.f_doc_id, doc_id));
        for (page, text) in page_texts.iter().enumerate() {
            writer
                .add_document(doc!(
                    self.f_doc_id => doc_id,
                    self.f_page => page as u64,
                    self.f_title => title,
                    self.f_body => text.as_str(),
                ))
                .map_err(err)?;
        }
        writer.commit().map_err(err)?;
        Ok(())
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

    /// Whether any pages of `doc_id` are in the index.
    pub fn has_document(&self, doc_id: &str) -> Result<bool, LibraryError> {
        use tantivy::collector::Count;
        use tantivy::query::TermQuery;
        use tantivy::schema::IndexRecordOption;
        let reader = self.index.reader().map_err(err)?;
        let searcher = reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.f_doc_id, doc_id),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count).map_err(err)?;
        Ok(count > 0)
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
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_search_round_trip() {
        let dir = std::env::temp_dir().join(format!("evo-search-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let index = SearchIndex::open_or_create(&dir).unwrap();
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
            )
            .unwrap();

        let hits = index.search("quick fox", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc_id, "abc123");
        assert_eq!(hits[0].page, 0);
        assert!(!hits[0].snippet.is_empty());

        // Deletion empties the index for that doc.
        index.delete_document(&mut writer, "abc123").unwrap();
        let hits = index.search("quick fox", 10).unwrap();
        assert!(hits.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
