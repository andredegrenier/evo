//! The local document library: content-addressed PDF blobs plus a redb
//! metadata store, under the platform data directory. Markup made on library
//! documents persists as a sidecar record — the PDF blob itself is immutable.

pub mod enrich;
pub mod extract;
pub mod indexer;
pub mod ocr;
#[cfg(feature = "s3")]
pub mod s3;
pub mod search;
pub mod store;
pub mod textjob;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::doc::annotation::Annotation;
use crate::doc::page_ops::PageList;
use serde::{Deserialize, Serialize};

/// Storage abstraction so a remote (S3/MinIO) backend can slot in later.
pub trait BlobStore: Send + Sync {
    fn put(&self, id: &str, bytes: &[u8]) -> io::Result<()>;
    fn get(&self, id: &str) -> io::Result<Vec<u8>>;
    fn delete(&self, id: &str) -> io::Result<()>;
}

pub struct LocalBlobStore {
    dir: PathBuf,
}

impl LocalBlobStore {
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.pdf"))
    }
}

impl BlobStore for LocalBlobStore {
    fn put(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path_of(id);
        if path.exists() {
            return Ok(()); // content-addressed: same id == same bytes
        }
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, path)
    }

    fn get(&self, id: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.path_of(id))
    }

    fn delete(&self, id: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path_of(id)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// How the searchable text for one page was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageTextStatus {
    /// Not indexed yet, or queued for OCR.
    Pending,
    /// Text came from the PDF's own text layer.
    Embedded,
    /// Text was recovered by OCR.
    Ocr,
    /// Extraction and OCR both failed; see `DocMeta::index_error`.
    Failed,
}

/// Metadata for one library document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocMeta {
    /// SHA-256 hex of the PDF bytes (also the blob filename stem).
    pub id: String,
    pub title: String,
    pub original_filename: String,
    /// Unix seconds.
    pub imported_at: i64,
    pub page_count: usize,
    pub file_size: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Per-page indexing state, one entry per source page. Empty for records
    /// written before v0.3 (they get re-indexed once on first launch).
    #[serde(default)]
    pub text_status: Vec<PageTextStatus>,
    /// Why the last indexing attempt failed, if it did.
    #[serde(default)]
    pub index_error: Option<String>,
    /// A sentence or two the assistant wrote about the document, if
    /// enrichment is switched on and has got to it.
    #[serde(default)]
    pub summary: Option<String>,
    /// Tags the assistant proposed. Kept apart from `tags` so the user's own
    /// are never overwritten -- or silently claimed as the machine's.
    #[serde(default)]
    pub auto_tags: Vec<String>,
}

impl DocMeta {
    /// Every tag shown for this document: the user's first, then whatever the
    /// assistant added that the user had not already thought of.
    pub fn all_tags(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.tags.iter().map(String::as_str).collect();
        for tag in &self.auto_tags {
            if !out.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                out.push(tag);
            }
        }
        out
    }
}

/// The persisted markup layer for one library document.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedMarkup {
    pub version: u32,
    pub annotations: Vec<Annotation>,
    pub pages: PageList,
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("could not access the library: {0}")]
    Io(#[from] io::Error),
    #[error("library database error: {0}")]
    Db(String),
    #[error("{0}")]
    Doc(#[from] crate::doc::LoadError),
}

pub struct Library {
    pub root: PathBuf,
    pub blobs: Arc<dyn BlobStore>,
    /// Shared with the indexer thread: redb permits only one `Database` per
    /// file, so this handle is the only one that ever exists.
    db: Arc<store::MetaDb>,
    indexer: Option<Arc<indexer::Indexer>>,
    /// Summaries and tags. Spawned alongside the indexer but a separate
    /// worker: a model takes seconds where extraction takes milliseconds, and
    /// nothing that reads a document should wait behind it.
    enricher: Option<enrich::Enricher>,
    search_index: std::sync::OnceLock<search::SearchIndex>,
}

/// Throw the tantivy index away when it was written to an older layout. It is
/// a cache of what the blobs and the metadata store already hold, so the
/// reconciliation pass refills it; there is nothing to migrate.
///
/// Returns whether the index was discarded.
pub fn migrate_search_index(db: &store::MetaDb, index_dir: &Path) -> Result<bool, LibraryError> {
    if db.search_schema()? == Some(search::SCHEMA_VERSION) {
        return Ok(false);
    }
    let existed = index_dir.exists();
    if existed {
        std::fs::remove_dir_all(index_dir)?;
    }
    db.set_search_schema(search::SCHEMA_VERSION)?;
    Ok(existed)
}

impl Library {
    /// Open (creating if needed) the library in the platform data dir.
    pub fn open_default() -> Result<Self, LibraryError> {
        let root = directories::ProjectDirs::from("", "", "evo")
            .map(|d| d.data_dir().join("library"))
            .ok_or_else(|| {
                LibraryError::Io(io::Error::other("no platform data directory available"))
            })?;
        Self::open_at(root)
    }

    pub fn open_at(root: PathBuf) -> Result<Self, LibraryError> {
        let blobs = Arc::new(LocalBlobStore::new(root.join("docs"))?);
        Self::open_at_with_blobs(root, blobs)
    }

    /// The same library with its documents kept somewhere else -- an S3 bucket,
    /// say (see [`s3`], behind the `s3` feature).
    ///
    /// Only the blobs move. The metadata database, the search index, the page
    /// cache and the thumbnails stay under `root` whatever this is given: they
    /// are memory-mapped files and directories of small writes, and neither is
    /// something object storage does.
    pub fn open_at_with_blobs(
        root: PathBuf,
        blobs: Arc<dyn BlobStore>,
    ) -> Result<Self, LibraryError> {
        std::fs::create_dir_all(root.join("thumbs"))?;
        let db = Arc::new(store::MetaDb::open(&root.join("meta.redb"))?);
        Ok(Self {
            root,
            blobs,
            db,
            indexer: None,
            enricher: None,
            search_index: std::sync::OnceLock::new(),
        })
    }

    /// Start the background text-extraction/indexing worker, and the
    /// enrichment worker it feeds. Documents from previous sessions that were
    /// never indexed get picked up here.
    pub fn start_indexer(
        &mut self,
        ctx: &eframe::egui::Context,
        pref: crate::render::engine::EnginePref,
    ) {
        if self.indexer.is_some() {
            return;
        }
        let index_dir = self.root.join("index");
        if let Err(e) = migrate_search_index(&self.db, &index_dir) {
            eprintln!("could not rebuild the search index: {e}");
        }

        // The two workers know about each other in both directions: the
        // indexer says what it has finished, and enrichment asks it to write
        // the summary into the index. The channel is created first so neither
        // has to be built before the other.
        let (on_indexed, indexed_rx) = std::sync::mpsc::channel::<String>();
        let known = self.list().unwrap_or_default();
        let indexer = Arc::new(indexer::Indexer::spawn(
            index_dir.clone(),
            self.root.join("models"),
            self.blobs.clone(),
            known,
            self.db.clone(),
            on_indexed.clone(),
            ctx.clone(),
            pref,
        ));
        self.enricher = Some(enrich::Enricher::spawn(
            index_dir,
            self.db.clone(),
            indexer.clone(),
            on_indexed,
            indexed_rx,
            ctx.clone(),
        ));
        self.indexer = Some(indexer);
    }

    pub fn index_status(&self) -> Option<indexer::IndexStatus> {
        self.indexer.as_ref().map(|i| i.status())
    }

    /// Dismiss the indexer's last error banner.
    pub fn clear_index_error(&self) {
        if let Some(indexer) = &self.indexer {
            indexer.clear_error();
        }
    }

    /// Tell the enrichment worker whether it may run, and which model with.
    /// Switching it on starts a pass over everything that has no summary yet.
    pub fn set_assistant(
        &self,
        prefs: &enrich::AssistantPrefs,
        model: &crate::script::model::ModelConfig,
    ) {
        if let Some(enricher) = &self.enricher {
            enricher.configure(prefs.enrich_enabled, model);
        }
    }

    pub fn enrich_status(&self) -> Option<enrich::EnrichStatus> {
        self.enricher.as_ref().map(|e| e.status())
    }

    pub fn clear_enrich_error(&self) {
        if let Some(enricher) = &self.enricher {
            enricher.clear_error();
        }
    }

    /// Re-run extraction and OCR for one document from scratch.
    pub fn reindex(&self, id: &str) -> Result<(), LibraryError> {
        let Some(mut meta) = self.db.get_doc(id)? else {
            return Ok(());
        };
        meta.text_status = vec![PageTextStatus::Pending; meta.page_count];
        meta.index_error = None;
        self.db.put_doc(&meta)?;
        if let Some(indexer) = &self.indexer {
            indexer.submit(indexer::IndexJob::Index {
                id: meta.id,
                title: meta.title,
            });
        }
        Ok(())
    }

    /// The search index, opened on first use.
    fn index(&self) -> Result<&search::SearchIndex, LibraryError> {
        if let Some(index) = self.search_index.get() {
            return Ok(index);
        }
        let index = search::SearchIndex::open_or_create(&self.root.join("index"))?;
        let _ = self.search_index.set(index);
        Ok(self.search_index.get().expect("just set"))
    }

    /// Full-text search over indexed documents.
    pub fn search(&self, query: &str) -> Result<Vec<search::SearchHit>, LibraryError> {
        self.index()?.search(query, 50)
    }

    /// The stored text of a document's pages, zero-based and half-open. Reading
    /// it back out of the index costs nothing next to extracting it from the
    /// PDF again, which is why the MCP server quotes documents from here.
    pub fn page_texts(
        &self,
        doc_id: &str,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<String>, LibraryError> {
        self.index()?.page_texts(doc_id, range)
    }

    /// Whether the pages of `doc_id` have been indexed.
    pub fn is_indexed(&self, doc_id: &str) -> Result<bool, LibraryError> {
        self.index()?.has_document(doc_id)
    }

    /// One document's metadata, if the library holds it.
    pub fn doc(&self, id: &str) -> Result<Option<DocMeta>, LibraryError> {
        self.db.get_doc(id)
    }

    pub fn thumb_path(&self, id: &str) -> PathBuf {
        self.root.join("thumbs").join(format!("{id}.png"))
    }

    /// Import a PDF file: validate, hash, store blob + metadata.
    /// Returns the metadata (existing metadata if the same bytes were
    /// imported before).
    pub fn import(&self, path: &Path) -> Result<DocMeta, LibraryError> {
        let bytes = std::fs::read(path)?;
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document.pdf".into());
        let title = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Document".into());
        self.import_bytes(bytes, &title, &filename)
    }

    /// Import a PDF that never was a file -- one a script generated, say.
    /// Same validation, hashing and indexing as [`Self::import`]; only the
    /// title and filename come from the caller instead of the path.
    pub fn import_bytes(
        &self,
        bytes: Vec<u8>,
        title: &str,
        filename: &str,
    ) -> Result<DocMeta, LibraryError> {
        // Validate + page count via the normal loader (rejects encrypted).
        let doc = crate::doc::Document::load_bytes(bytes.clone(), None)?;

        let id = hex_digest(&bytes);
        if let Some(existing) = self.db.get_doc(&id)? {
            return Ok(existing);
        }
        self.blobs.put(&id, &bytes)?;

        let filename = filename.to_owned();
        let title = title.to_owned();
        let meta = DocMeta {
            id: id.clone(),
            title,
            original_filename: filename,
            imported_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            page_count: doc.pages.len(),
            file_size: bytes.len() as u64,
            tags: Vec::new(),
            text_status: vec![PageTextStatus::Pending; doc.pages.len()],
            index_error: None,
            summary: None,
            auto_tags: Vec::new(),
        };
        self.db.put_doc(&meta)?;
        if let Some(indexer) = &self.indexer {
            indexer.submit(indexer::IndexJob::Index {
                id: meta.id.clone(),
                title: meta.title.clone(),
            });
        }
        Ok(meta)
    }

    /// All documents, newest import first.
    pub fn list(&self) -> Result<Vec<DocMeta>, LibraryError> {
        let mut docs = self.db.list_docs()?;
        docs.sort_by_key(|d| -d.imported_at);
        Ok(docs)
    }

    pub fn delete(&self, id: &str) -> Result<(), LibraryError> {
        if let Some(indexer) = &self.indexer {
            indexer.submit(indexer::IndexJob::Delete { id: id.to_owned() });
        }
        self.db.delete_doc(id)?;
        self.blobs.delete(id)?;
        let _ = std::fs::remove_file(self.thumb_path(id));
        Ok(())
    }

    pub fn load_bytes(&self, id: &str) -> Result<Vec<u8>, LibraryError> {
        Ok(self.blobs.get(id)?)
    }

    pub fn load_markup(&self, id: &str) -> Result<Option<SavedMarkup>, LibraryError> {
        self.db.get_markup(id)
    }

    pub fn save_markup(&self, id: &str, markup: &SavedMarkup) -> Result<(), LibraryError> {
        self.db.put_markup(id, markup)
    }

    /// The saved chat transcript for a document (empty when there is none).
    pub fn load_chat(
        &self,
        id: &str,
    ) -> Result<Vec<crate::script::model::ChatMessage>, LibraryError> {
        self.db.get_chat(id)
    }

    pub fn save_chat(
        &self,
        id: &str,
        messages: &[crate::script::model::ChatMessage],
    ) -> Result<(), LibraryError> {
        self.db.put_chat(id, messages)
    }
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// How wide a library thumbnail is drawn, in pixels.
pub const THUMB_WIDTH: f32 = 320.0;

/// Render page 1 of `bytes` to `thumb_path` as a PNG in a background thread.
pub fn spawn_thumbnail_job(
    bytes: Arc<Vec<u8>>,
    thumb_path: PathBuf,
    ctx: eframe::egui::Context,
    pref: crate::render::engine::EnginePref,
) {
    if thumb_path.exists() {
        return;
    }
    std::thread::Builder::new()
        .name("evo-thumb".into())
        .spawn(move || {
            let zoom = crate::render::engine::Zoom::FitWidth(THUMB_WIDTH);
            let Ok((drawn, _)) = crate::render::engine::render_page(bytes, None, 0, zoom, pref)
            else {
                return;
            };
            if let Some(img) = image::RgbaImage::from_raw(drawn.width, drawn.height, drawn.rgba) {
                let tmp = thumb_path.with_extension("part");
                if img.save_with_format(&tmp, image::ImageFormat::Png).is_ok() {
                    let _ = std::fs::rename(tmp, thumb_path);
                    ctx.request_repaint();
                }
            }
        })
        .expect("failed to spawn thumbnail thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_library(name: &str) -> (Library, PathBuf) {
        let dir = std::env::temp_dir().join(format!("evo-lib-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Library::open_at(dir.clone()).unwrap(), dir)
    }

    #[test]
    fn import_list_delete_round_trip() {
        let (lib, dir) = temp_library("roundtrip");
        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        assert_eq!(meta.page_count, 2);
        assert_eq!(meta.title, "sample");

        // Re-import dedups to the same id.
        let again = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        assert_eq!(again.id, meta.id);
        assert_eq!(lib.list().unwrap().len(), 1);

        // Blob round-trips.
        let bytes = lib.load_bytes(&meta.id).unwrap();
        assert_eq!(hex_digest(&bytes), meta.id);

        lib.delete(&meta.id).unwrap();
        assert!(lib.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    /// v0.2 wrote DocMeta without `text_status`/`index_error`; those records
    /// must still load (the indexer back-fills them on first launch).
    #[test]
    fn doc_meta_reads_v0_2_records() {
        let v0_2 = r#"{
            "id": "deadbeef",
            "title": "Old Document",
            "original_filename": "old.pdf",
            "imported_at": 1700000000,
            "page_count": 3,
            "file_size": 4096,
            "tags": ["invoice"]
        }"#;
        let meta: DocMeta = serde_json::from_str(v0_2).unwrap();
        assert_eq!(meta.title, "Old Document");
        assert_eq!(meta.page_count, 3);
        assert_eq!(meta.tags, vec!["invoice".to_string()]);
        assert!(meta.text_status.is_empty());
        assert!(meta.index_error.is_none());
        // Nor did it know about summaries.
        assert!(meta.summary.is_none());
        assert!(meta.auto_tags.is_empty());
        assert!(indexer::needs_reindex(&meta, true));
    }

    #[test]
    fn enrichment_round_trips_and_leaves_the_users_tags_alone() {
        let (lib, dir) = temp_library("enrich");
        let mut meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        assert!(meta.summary.is_none());

        meta.tags = vec!["mine".into()];
        lib.db.put_doc(&meta).unwrap();
        lib.db
            .update_enrichment(
                &meta.id,
                Some("A two-page sample."),
                &["sample".to_owned(), "test".to_owned()],
            )
            .unwrap();

        let stored = lib.db.get_doc(&meta.id).unwrap().unwrap();
        assert_eq!(stored.summary.as_deref(), Some("A two-page sample."));
        assert_eq!(stored.auto_tags, ["sample", "test"]);
        assert_eq!(stored.tags, ["mine"], "the user's own tags are untouched");
        assert_eq!(stored.all_tags(), ["mine", "sample", "test"]);
        // Updating an unknown document is a no-op, not an error: it may have
        // been deleted while the worker was describing it.
        lib.db.update_enrichment("nope", Some("x"), &[]).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    /// An index written by an older evo is thrown away rather than migrated;
    /// the reconciliation pass fills the new one in.
    #[test]
    fn an_index_from_an_older_schema_is_rebuilt_once() {
        let (lib, dir) = temp_library("schema");
        let index_dir = lib.root.join("index");
        std::fs::create_dir_all(&index_dir).unwrap();
        std::fs::write(index_dir.join("meta.json"), b"{}").unwrap();

        // No version recorded at all: every library written before v0.4.
        assert!(lib.db.search_schema().unwrap().is_none());
        assert!(migrate_search_index(&lib.db, &index_dir).unwrap());
        assert!(!index_dir.exists(), "the old index was removed");
        assert_eq!(
            lib.db.search_schema().unwrap(),
            Some(search::SCHEMA_VERSION)
        );

        // Second launch: nothing to do, and the index survives.
        std::fs::create_dir_all(&index_dir).unwrap();
        std::fs::write(index_dir.join("meta.json"), b"{}").unwrap();
        assert!(!migrate_search_index(&lib.db, &index_dir).unwrap());
        assert!(index_dir.join("meta.json").exists());

        // A version from some other layout also triggers a rebuild.
        lib.db.set_search_schema(99).unwrap();
        assert!(migrate_search_index(&lib.db, &index_dir).unwrap());
        assert_eq!(
            lib.db.search_schema().unwrap(),
            Some(search::SCHEMA_VERSION)
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn import_seeds_pending_status_and_reindex_resets_it() {
        let (lib, dir) = temp_library("status");
        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        assert_eq!(
            meta.text_status,
            vec![PageTextStatus::Pending; meta.page_count]
        );

        lib.db
            .update_text_status(
                &meta.id,
                &[PageTextStatus::Embedded, PageTextStatus::Failed],
                Some("boom"),
            )
            .unwrap();
        let stored = lib.db.get_doc(&meta.id).unwrap().unwrap();
        assert_eq!(stored.text_status[1], PageTextStatus::Failed);
        assert_eq!(stored.index_error.as_deref(), Some("boom"));

        lib.reindex(&meta.id).unwrap();
        let reset = lib.db.get_doc(&meta.id).unwrap().unwrap();
        assert_eq!(
            reset.text_status,
            vec![PageTextStatus::Pending; meta.page_count]
        );
        assert!(reset.index_error.is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn markup_sidecar_round_trip() {
        let (lib, dir) = temp_library("markup");
        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();

        let markup = SavedMarkup {
            version: 1,
            annotations: vec![],
            pages: crate::doc::page_ops::PageList::new(2),
        };
        lib.save_markup(&meta.id, &markup).unwrap();
        let loaded = lib.load_markup(&meta.id).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.pages.order, vec![0, 1]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn chat_sidecar_round_trip() {
        use crate::script::model::{ChatMessage, Role};

        let (lib, dir) = temp_library("chat");
        let meta = lib.import(Path::new("tests/fixtures/sample.pdf")).unwrap();
        assert!(lib.load_chat(&meta.id).unwrap().is_empty());

        let messages = vec![
            ChatMessage::new(Role::User, "what is on page 2?"),
            ChatMessage::new(Role::Assistant, "The second page. [p.2]"),
        ];
        lib.save_chat(&meta.id, &messages).unwrap();
        assert_eq!(lib.load_chat(&meta.id).unwrap(), messages);

        // Clearing a conversation forgets it rather than storing an empty one.
        lib.save_chat(&meta.id, &[]).unwrap();
        assert!(lib.load_chat(&meta.id).unwrap().is_empty());

        // A deleted document takes its transcript with it.
        lib.save_chat(&meta.id, &messages).unwrap();
        lib.delete(&meta.id).unwrap();
        assert!(lib.load_chat(&meta.id).unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
