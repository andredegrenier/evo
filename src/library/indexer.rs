//! The `evo-index` background worker: extracts text from imported documents
//! (hayro glyph device; OCR for scanned pages comes in via `ocr.rs`) and
//! keeps the tantivy index in sync. Per-page outcomes are written back to the
//! document's `DocMeta` so the library view can show progress and failures.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;

use super::extract::extract_page_text;
use super::search::SearchIndex;
use super::store::MetaDb;
use super::{BlobStore, DocMeta, PageTextStatus};

pub enum IndexJob {
    Index { id: String, title: String },
    Delete { id: String },
}

#[derive(Default, Clone)]
pub struct IndexStatus {
    /// Jobs queued or in flight.
    pub pending: usize,
    /// Title currently being indexed.
    pub current: Option<String>,
    /// Document id currently being indexed.
    pub current_id: Option<String>,
    /// Pages that had no usable embedded text and are waiting on OCR.
    pub ocr_pending: usize,
    /// OCR progress within the current document.
    pub ocr_done: usize,
    pub ocr_total: usize,
    pub last_error: Option<String>,
}

/// Should this document be (re-)indexed at startup?
///
/// Besides documents missing from the index, this picks up pages whose OCR
/// failed (so a transient failure retries on the next launch) and pre-v0.3
/// records with no per-page state at all (a one-time back-fill).
pub fn needs_reindex(meta: &DocMeta, in_index: bool) -> bool {
    !in_index
        || meta.text_status.is_empty()
        || meta
            .text_status
            .iter()
            .any(|s| matches!(s, PageTextStatus::Pending | PageTextStatus::Failed))
}

pub struct Indexer {
    tx: Sender<IndexJob>,
    status: Arc<Mutex<IndexStatus>>,
}

impl Indexer {
    /// Spawn the worker. `known_docs` seeds a reconciliation pass so documents
    /// imported in previous sessions get indexed too. `models_dir` is where
    /// OCR models live (downloaded on first need).
    pub fn spawn(
        index_dir: PathBuf,
        models_dir: PathBuf,
        blobs: Arc<dyn BlobStore>,
        known_docs: Vec<DocMeta>,
        db: Arc<MetaDb>,
        ctx: eframe::egui::Context,
    ) -> Self {
        let (tx, rx) = channel::<IndexJob>();
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        let shared = status.clone();

        std::thread::Builder::new()
            .name("evo-index".into())
            .spawn(move || {
                let index = match SearchIndex::open_or_create(&index_dir) {
                    Ok(index) => index,
                    Err(e) => {
                        shared.lock().unwrap().last_error = Some(e.to_string());
                        return;
                    }
                };
                let mut writer = match index.writer() {
                    Ok(w) => w,
                    Err(e) => {
                        shared.lock().unwrap().last_error = Some(e.to_string());
                        return;
                    }
                };

                // Reconcile: index anything previous sessions missed or failed.
                let backlog: Vec<(String, String)> = known_docs
                    .into_iter()
                    .filter(|m| needs_reindex(m, index.has_document(&m.id).unwrap_or(true)))
                    .map(|m| (m.id, m.title))
                    .collect();
                shared.lock().unwrap().pending = backlog.len();

                let mut worker = Worker {
                    blobs,
                    db,
                    models_dir,
                    settings: InterpreterSettings::default(),
                    ocr: None,
                    status: shared,
                    ctx,
                };

                for (id, title) in backlog {
                    worker.handle(IndexJob::Index { id, title }, &index, &mut writer);
                }
                while let Ok(job) = rx.recv() {
                    worker.handle(job, &index, &mut writer);
                }
            })
            .expect("failed to spawn index thread");

        Self { tx, status }
    }

    pub fn submit(&self, job: IndexJob) {
        if let Ok(mut st) = self.status.lock() {
            st.pending += 1;
        }
        let _ = self.tx.send(job);
    }

    pub fn status(&self) -> IndexStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Dismiss the last error (the library view offers this as an X button).
    pub fn clear_error(&self) {
        if let Ok(mut st) = self.status.lock() {
            st.last_error = None;
        }
    }
}

/// Everything the worker thread carries between jobs.
struct Worker {
    blobs: Arc<dyn BlobStore>,
    db: Arc<MetaDb>,
    models_dir: PathBuf,
    settings: InterpreterSettings,
    /// OCR engine, created lazily on the first scanned page: `None` = not
    /// tried, `Some(Err)` = unavailable (offline or failed init).
    ocr: Option<Result<super::ocr::Ocr, String>>,
    status: Arc<Mutex<IndexStatus>>,
    ctx: eframe::egui::Context,
}

impl Worker {
    fn set_error(&self, msg: impl std::fmt::Display) {
        self.status.lock().unwrap().last_error = Some(msg.to_string());
    }

    fn set_current(&self, text: String) {
        self.status.lock().unwrap().current = Some(text);
        self.ctx.request_repaint();
    }

    /// Persist per-page state; a store failure only surfaces as a banner.
    fn persist(&self, id: &str, statuses: &[PageTextStatus], error: Option<&str>) {
        if let Err(e) = self.db.update_text_status(id, statuses, error) {
            self.set_error(e);
        }
    }

    /// Mark every page of the document as failed (parse or blob failure).
    fn fail_document(&self, id: &str, reason: &str) {
        let pages = self
            .db
            .get_doc(id)
            .ok()
            .flatten()
            .map(|m| m.page_count)
            .unwrap_or(0);
        self.persist(id, &vec![PageTextStatus::Failed; pages], Some(reason));
        self.set_error(reason);
    }

    fn handle(&mut self, job: IndexJob, index: &SearchIndex, writer: &mut tantivy::IndexWriter) {
        match job {
            IndexJob::Index { id, title } => self.index_doc(&id, &title, index, writer),
            IndexJob::Delete { id } => {
                if let Err(e) = index.delete_document(writer, &id) {
                    self.set_error(e);
                }
            }
        }
        {
            let mut st = self.status.lock().unwrap();
            st.pending = st.pending.saturating_sub(1);
            st.current = None;
            st.current_id = None;
            st.ocr_done = 0;
            st.ocr_total = 0;
        }
        self.ctx.request_repaint();
    }

    fn index_doc(
        &mut self,
        id: &str,
        title: &str,
        index: &SearchIndex,
        writer: &mut tantivy::IndexWriter,
    ) {
        {
            let mut st = self.status.lock().unwrap();
            st.current = Some(format!("Indexing {title}"));
            st.current_id = Some(id.to_owned());
            st.ocr_done = 0;
            st.ocr_total = 0;
        }
        self.ctx.request_repaint();

        let bytes = match self.blobs.get(id) {
            Ok(bytes) => bytes,
            Err(e) => return self.fail_document(id, &e.to_string()),
        };
        let Ok(pdf) = Pdf::new(bytes) else {
            return self.fail_document(id, &format!("could not parse {title} for indexing"));
        };

        // Pass 1: embedded text. Pages with too little of it queue for OCR.
        let pages = pdf.pages();
        let mut texts: Vec<String> = Vec::with_capacity(pages.len());
        let mut statuses = vec![PageTextStatus::Embedded; pages.len()];
        let mut ocr_pages: Vec<usize> = Vec::new();
        for (i, page) in pages.iter().enumerate() {
            let extracted = extract_page_text(page, &self.settings);
            if extracted.text.trim().len() < 32 || extracted.unmapped_ratio > 0.3 {
                ocr_pages.push(i);
                statuses[i] = PageTextStatus::Pending;
            }
            texts.push(extracted.text);
        }
        self.persist(id, &statuses, None);

        // Pass 2: OCR whatever came up short.
        if !ocr_pages.is_empty() {
            {
                let mut st = self.status.lock().unwrap();
                st.ocr_pending += ocr_pages.len();
                st.ocr_total = ocr_pages.len();
            }
            if self.ocr.is_none() {
                self.set_current("Preparing OCR (first use downloads ~10 MB of models)".to_owned());
                let loaded = super::ocr::Ocr::load(&self.models_dir).map_err(|e| e.to_string());
                if let Err(e) = &loaded {
                    self.set_error(e);
                }
                self.ocr = Some(loaded);
            }
            match &self.ocr {
                Some(Ok(engine)) => {
                    let mut last_error: Option<String> = None;
                    for (done, &i) in ocr_pages.iter().enumerate() {
                        self.set_current(format!("OCR: {title} p.{}", i + 1));
                        match super::ocr::ocr_page(engine, &pages[i], &self.settings) {
                            Ok(text) => {
                                texts[i] = text;
                                statuses[i] = PageTextStatus::Ocr;
                            }
                            Err(e) => {
                                statuses[i] = PageTextStatus::Failed;
                                last_error = Some(e.to_string());
                                self.set_error(e);
                            }
                        }
                        {
                            let mut st = self.status.lock().unwrap();
                            st.ocr_pending = st.ocr_pending.saturating_sub(1);
                            st.ocr_done = done + 1;
                        }
                        self.persist(id, &statuses, last_error.as_deref());
                    }
                }
                other => {
                    let reason = match other {
                        Some(Err(e)) => e.clone(),
                        _ => "OCR engine unavailable".to_owned(),
                    };
                    for &i in &ocr_pages {
                        statuses[i] = PageTextStatus::Failed;
                    }
                    {
                        let mut st = self.status.lock().unwrap();
                        st.ocr_pending = st.ocr_pending.saturating_sub(ocr_pages.len());
                    }
                    self.persist(id, &statuses, Some(&reason));
                }
            }
        }

        if let Err(e) = index.index_document(writer, id, title, &texts) {
            self.set_error(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_with(statuses: Vec<PageTextStatus>) -> DocMeta {
        DocMeta {
            id: "abc".into(),
            title: "doc".into(),
            original_filename: "doc.pdf".into(),
            imported_at: 0,
            page_count: statuses.len(),
            file_size: 0,
            tags: Vec::new(),
            text_status: statuses,
            index_error: None,
        }
    }

    #[test]
    fn reindexes_documents_missing_from_the_index() {
        let meta = meta_with(vec![PageTextStatus::Embedded, PageTextStatus::Ocr]);
        assert!(needs_reindex(&meta, false));
    }

    #[test]
    fn skips_fully_indexed_documents() {
        let meta = meta_with(vec![PageTextStatus::Embedded, PageTextStatus::Ocr]);
        assert!(!needs_reindex(&meta, true));
    }

    #[test]
    fn retries_failed_pages() {
        let meta = meta_with(vec![PageTextStatus::Embedded, PageTextStatus::Failed]);
        assert!(needs_reindex(&meta, true));
        let meta = meta_with(vec![PageTextStatus::Pending]);
        assert!(needs_reindex(&meta, true));
    }

    #[test]
    fn backfills_pre_v0_3_records() {
        let meta = meta_with(Vec::new());
        assert!(needs_reindex(&meta, true));
    }
}
