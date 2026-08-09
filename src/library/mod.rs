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

/// The sidecar format this build writes.
///
/// Version 2 added the polygon family (polygons, polylines, clouds). Version 1
/// files still read: nothing in them has changed meaning, so a library written
/// by v0.5 opens here untouched, and only a document that is actually given one
/// of the new shapes gains anything a v0.5 evo could not parse.
pub const MARKUP_VERSION: u32 = 2;

/// The persisted markup layer for one library document.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedMarkup {
    pub version: u32,
    pub annotations: Vec<Annotation>,
    pub pages: PageList,
}

impl SavedMarkup {
    /// A markup layer stamped with the version this build writes.
    pub fn new(annotations: Vec<Annotation>, pages: PageList) -> Self {
        Self {
            version: MARKUP_VERSION,
            annotations,
            pages,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("could not access the library: {0}")]
    Io(#[from] io::Error),
    #[error("library database error: {0}")]
    Db(String),
    #[error("{0}")]
    Doc(#[from] crate::doc::LoadError),
    /// Rewriting a protected document as a plain one failed. Separate from
    /// [`LibraryError::Doc`] because it means evo could *read* the document
    /// and could not *write* it back out -- a different thing to tell somebody.
    #[error("this PDF could not be unlocked for the library: {0}")]
    Decrypt(#[from] lopdf::Error),
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

/// The same document with its encryption taken off.
///
/// lopdf decrypts every object as it loads and drops `/Encrypt`, so writing
/// the loaded document straight back out is the whole of it. The bytes that
/// come back are an ordinary PDF that opens with no password anywhere.
fn decrypted_copy(bytes: &[u8], password: &str) -> Result<Vec<u8>, LibraryError> {
    let mut lo =
        lopdf::Document::load_mem_with_options(bytes, lopdf::LoadOptions::with_password(password))?;
    let mut out = Vec::new();
    lo.save_to(&mut out)?;
    Ok(out)
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

    /// Import a PDF file: validate, hash, store blob + metadata, unlocking it
    /// with `password` first if it needs one. Returns the metadata (existing
    /// metadata if the same bytes were imported before).
    pub fn import(&self, path: &Path, password: Option<&str>) -> Result<DocMeta, LibraryError> {
        let bytes = std::fs::read(path)?;
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document.pdf".into());
        let title = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Document".into());
        self.import_bytes_with_password(bytes, &title, &filename, password)
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
        self.import_bytes_with_password(bytes, title, filename, None)
    }

    /// Import bytes that need `password` to open.
    ///
    /// A protected document is unlocked **once**, here, and what the library
    /// stores is the decrypted copy. Everything downstream -- the search
    /// index, OCR, thumbnails, `evo serve` and the phone -- reads the stored
    /// blob directly and none of them has anywhere to keep a password or
    /// anybody to ask for one. The password itself is used and dropped; it is
    /// never written to the database, the blob store or a log. Making that
    /// trade is the person's decision, so the desktop asks before calling
    /// this.
    pub fn import_bytes_with_password(
        &self,
        bytes: Vec<u8>,
        title: &str,
        filename: &str,
        password: Option<&str>,
    ) -> Result<DocMeta, LibraryError> {
        // Validate + page count via the normal loader.
        let doc = crate::doc::Document::load_bytes_with_password(bytes.clone(), None, password)?;

        let bytes = match password {
            None => bytes,
            Some(password) => {
                let plain = decrypted_copy(&bytes, password)?;
                // Prove the copy opens with no password at all before it is
                // committed: a blob the rest of evo cannot read would be worse
                // than a refused import.
                crate::doc::Document::load_bytes(plain.clone(), None)?;
                plain
            }
        };

        // The id is the hash of what is stored, so an unlocked import and the
        // same file already unlocked by hand are one document, not two.
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

    /// The markup sidecar for a document, if one has ever been saved.
    ///
    /// A sidecar from a *newer* evo is refused by name rather than parsed
    /// half-way: the shapes it holds are ones this build cannot draw, and
    /// opening it would end with the next save quietly deleting them.
    pub fn load_markup(&self, id: &str) -> Result<Option<SavedMarkup>, LibraryError> {
        let markup = self.db.get_markup(id)?;
        if let Some(markup) = &markup
            && markup.version > MARKUP_VERSION
        {
            return Err(LibraryError::Db(format!(
                "this document's markup was saved by a newer version of evo \
                 (format {}, this one reads up to {MARKUP_VERSION}); update evo to open it",
                markup.version
            )));
        }
        Ok(markup)
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
        let meta = lib
            .import(Path::new("tests/fixtures/sample.pdf"), None)
            .unwrap();
        assert_eq!(meta.page_count, 2);
        assert_eq!(meta.title, "sample");

        // Re-import dedups to the same id.
        let again = lib
            .import(Path::new("tests/fixtures/sample.pdf"), None)
            .unwrap();
        assert_eq!(again.id, meta.id);
        assert_eq!(lib.list().unwrap().len(), 1);

        // Blob round-trips.
        let bytes = lib.load_bytes(&meta.id).unwrap();
        assert_eq!(hex_digest(&bytes), meta.id);

        lib.delete(&meta.id).unwrap();
        assert!(lib.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Importing a protected PDF unlocks it once and stores the unlocked
    /// copy. Everything after import -- indexing, OCR, thumbnails, the phone --
    /// opens the blob with no password, so this test insists the stored bytes
    /// do, and that the password is nowhere in what was written down.
    #[test]
    fn a_protected_import_is_unlocked_once_and_stored_unlocked() {
        let (lib, dir) = temp_library("encrypted-import");

        for path in crate::doc::tests::PROTECTED {
            let meta = lib
                .import(Path::new(path), Some("evo"))
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(meta.page_count, 2, "{path}");

            let stored = lib.load_bytes(&meta.id).expect("the blob");
            // The id is the hash of what was stored, not of the file on disk.
            assert_eq!(hex_digest(&stored), meta.id, "{path}");
            assert_ne!(
                stored,
                std::fs::read(path).unwrap(),
                "{path}: the encrypted bytes were stored as they came"
            );
            assert_eq!(meta.file_size, stored.len() as u64, "{path}");

            // The whole point: it opens with nothing at all.
            let reopened =
                crate::doc::Document::load_bytes(stored.clone(), None).expect("opens unlocked");
            assert_eq!(reopened.pages.len(), 2, "{path}");
            assert_eq!(reopened.password(), None, "{path}");
            assert!(
                !lopdf::Document::load_mem(&stored)
                    .expect("lopdf opens it")
                    .trailer
                    .has(b"Encrypt"),
                "{path}: the stored copy still declares encryption"
            );

            // The text the index and the phone will read is really there.
            let text = extract::extract_all_pages(&std::sync::Arc::new(stored), None);
            assert_eq!(text.len(), 2, "{path}");
            assert!(!text[0].trim().is_empty(), "{path}: no text on page one");

            // Nothing anywhere remembers the password.
            let json = serde_json::to_string(&meta).expect("meta serializes");
            assert!(!json.contains("evo-owner"), "{path}");
            assert!(!json.contains("\"evo\""), "{path}");
        }

        std::fs::remove_dir_all(dir).ok();
    }

    /// Without a password, a protected document is refused with the error the
    /// desktop turns into a prompt -- and nothing is written.
    #[test]
    fn a_protected_import_without_a_password_asks_rather_than_storing() {
        let (lib, dir) = temp_library("encrypted-import-refused");
        let err = lib
            .import(Path::new(crate::doc::tests::PROTECTED[0]), None)
            .map(|m| m.id)
            .expect_err("no password, no import");
        assert!(
            matches!(&err, LibraryError::Doc(e) if e.wants_password()),
            "{err:?}"
        );
        assert!(lib.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    /// A document protected with an empty user password needs no consent and
    /// no prompt: it imports like any other file.
    #[test]
    fn an_empty_user_password_import_needs_nothing() {
        let (lib, dir) = temp_library("encrypted-import-empty");
        let meta = lib
            .import(Path::new("tests/fixtures/encrypted-empty-user.pdf"), None)
            .expect("imports with no password");
        assert_eq!(meta.page_count, 2);
        let stored = lib.load_bytes(&meta.id).expect("the blob");
        assert_eq!(
            crate::doc::Document::load_bytes(stored, None)
                .expect("opens")
                .pages
                .len(),
            2
        );
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
        let mut meta = lib
            .import(Path::new("tests/fixtures/sample.pdf"), None)
            .unwrap();
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
        let meta = lib
            .import(Path::new("tests/fixtures/sample.pdf"), None)
            .unwrap();
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
        let meta = lib
            .import(Path::new("tests/fixtures/sample.pdf"), None)
            .unwrap();

        // A sidecar as v0.5 wrote it: version 1, and nothing in it has changed
        // meaning since, so it reads back exactly as it was left.
        let markup = SavedMarkup {
            version: 1,
            annotations: vec![],
            pages: crate::doc::page_ops::PageList::new(2),
        };
        lib.save_markup(&meta.id, &markup).unwrap();
        let loaded = lib.load_markup(&meta.id).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.pages.order, vec![0, 1]);

        // What this build writes, with a shape only this build knows.
        let cloud = crate::doc::annotation::Annotation {
            id: 1,
            page: 0,
            kind: crate::doc::annotation::AnnotationKind::Polygon {
                points: vec![
                    crate::doc::geometry::PdfPoint::new(10.0, 10.0),
                    crate::doc::geometry::PdfPoint::new(90.0, 10.0),
                    crate::doc::geometry::PdfPoint::new(90.0, 60.0),
                ],
                cloudy: Some(1.5),
            },
            rect: crate::doc::geometry::PdfRect::from_points(
                crate::doc::geometry::PdfPoint::new(10.0, 10.0),
                crate::doc::geometry::PdfPoint::new(90.0, 60.0),
            ),
            style: crate::doc::annotation::Style::default(),
            group: None,
        };
        let v2 = SavedMarkup::new(vec![cloud.clone()], crate::doc::page_ops::PageList::new(2));
        assert_eq!(v2.version, 2);
        lib.save_markup(&meta.id, &v2).unwrap();
        let loaded = lib.load_markup(&meta.id).unwrap().unwrap();
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.annotations, vec![cloud]);

        // And a sidecar from a version that does not exist yet is refused by
        // name rather than opened with its shapes silently dropped.
        let future = SavedMarkup {
            version: MARKUP_VERSION + 1,
            annotations: vec![],
            pages: crate::doc::page_ops::PageList::new(2),
        };
        lib.save_markup(&meta.id, &future).unwrap();
        let err = match lib.load_markup(&meta.id) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a sidecar from the future was opened anyway"),
        };
        assert!(err.contains("newer version of evo"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn chat_sidecar_round_trip() {
        use crate::script::model::{ChatMessage, Role};

        let (lib, dir) = temp_library("chat");
        let meta = lib
            .import(Path::new("tests/fixtures/sample.pdf"), None)
            .unwrap();
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
