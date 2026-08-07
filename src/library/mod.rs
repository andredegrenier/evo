//! The local document library: content-addressed PDF blobs plus a redb
//! metadata store, under the platform data directory. Markup made on library
//! documents persists as a sidecar record — the PDF blob itself is immutable.

pub mod extract;
pub mod indexer;
pub mod search;
pub mod store;

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
    db: store::MetaDb,
    indexer: Option<indexer::Indexer>,
    search_index: std::sync::OnceLock<search::SearchIndex>,
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
        std::fs::create_dir_all(root.join("thumbs"))?;
        let blobs = Arc::new(LocalBlobStore::new(root.join("docs"))?);
        let db = store::MetaDb::open(&root.join("meta.redb"))?;
        Ok(Self {
            root,
            blobs,
            db,
            indexer: None,
            search_index: std::sync::OnceLock::new(),
        })
    }

    /// Start the background text-extraction/indexing worker. Documents from
    /// previous sessions that were never indexed get picked up here.
    pub fn start_indexer(&mut self, ctx: &eframe::egui::Context) {
        if self.indexer.is_some() {
            return;
        }
        let known = self
            .list()
            .map(|docs| docs.into_iter().map(|d| (d.id, d.title)).collect())
            .unwrap_or_default();
        self.indexer = Some(indexer::Indexer::spawn(
            self.root.join("index"),
            self.blobs.clone(),
            known,
            ctx.clone(),
        ));
    }

    pub fn index_status(&self) -> Option<indexer::IndexStatus> {
        self.indexer.as_ref().map(|i| i.status())
    }

    /// Full-text search over indexed documents.
    pub fn search(&self, query: &str) -> Result<Vec<search::SearchHit>, LibraryError> {
        let index = match self.search_index.get() {
            Some(index) => index,
            None => {
                let index = search::SearchIndex::open_or_create(&self.root.join("index"))?;
                let _ = self.search_index.set(index);
                self.search_index.get().unwrap()
            }
        };
        index.search(query, 50)
    }

    pub fn thumb_path(&self, id: &str) -> PathBuf {
        self.root.join("thumbs").join(format!("{id}.png"))
    }

    /// Import a PDF file: validate, hash, store blob + metadata.
    /// Returns the metadata (existing metadata if the same bytes were
    /// imported before).
    pub fn import(&self, path: &Path) -> Result<DocMeta, LibraryError> {
        let bytes = std::fs::read(path)?;
        // Validate + page count via the normal loader (rejects encrypted).
        let doc = crate::doc::Document::load_bytes(bytes.clone(), Some(path.to_path_buf()))?;

        let id = hex_digest(&bytes);
        if let Some(existing) = self.db.get_doc(&id)? {
            return Ok(existing);
        }
        self.blobs.put(&id, &bytes)?;

        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document.pdf".into());
        let title = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Document".into());
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
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Render page 1 of `bytes` to `thumb_path` as a PNG in a background thread.
pub fn spawn_thumbnail_job(bytes: Arc<Vec<u8>>, thumb_path: PathBuf, ctx: eframe::egui::Context) {
    if thumb_path.exists() {
        return;
    }
    std::thread::Builder::new()
        .name("evo-thumb".into())
        .spawn(move || {
            use hayro::hayro_interpret::InterpreterSettings;
            use hayro::vello_cpu::color::AlphaColor;
            use hayro::{RenderCache, RenderSettings};

            let Ok(pdf) = hayro::hayro_syntax::Pdf::new(bytes) else {
                return;
            };
            let pages = pdf.pages();
            let Some(page) = pages.first() else { return };
            let (w, _) = page.render_dimensions();
            let scale = (320.0 / w.max(1.0)).clamp(0.1, 4.0);
            let pixmap = hayro::render(
                page,
                &RenderCache::new(),
                &InterpreterSettings::default(),
                &RenderSettings {
                    x_scale: scale,
                    y_scale: scale,
                    width: None,
                    height: None,
                    bg_color: AlphaColor::WHITE,
                },
            );
            let (pw, ph) = (pixmap.width() as u32, pixmap.height() as u32);
            let rgba: Vec<u8> = pixmap
                .take_unpremultiplied()
                .into_iter()
                .flat_map(|p| [p.r, p.g, p.b, p.a])
                .collect();
            if let Some(img) = image::RgbaImage::from_raw(pw, ph, rgba) {
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
}
