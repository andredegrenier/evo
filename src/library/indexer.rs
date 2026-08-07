//! The `evo-index` background worker: extracts text from imported documents
//! (hayro glyph device; OCR for scanned pages comes in via `ocr.rs`) and
//! keeps the tantivy index in sync.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;

use super::BlobStore;
use super::extract::extract_page_text;
use super::search::SearchIndex;

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
    /// Pages that had no usable embedded text and are waiting on OCR.
    pub ocr_pending: usize,
    pub last_error: Option<String>,
}

pub struct Indexer {
    tx: Sender<IndexJob>,
    status: Arc<Mutex<IndexStatus>>,
}

impl Indexer {
    /// Spawn the worker. `known_docs` (id, title) seeds a reconciliation pass
    /// so documents imported in previous sessions get indexed too.
    pub fn spawn(
        index_dir: PathBuf,
        blobs: Arc<dyn BlobStore>,
        known_docs: Vec<(String, String)>,
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
                let settings = InterpreterSettings::default();

                // Reconcile: index anything the previous sessions missed.
                let backlog: Vec<(String, String)> = known_docs
                    .into_iter()
                    .filter(|(id, _)| !index.has_document(id).unwrap_or(true))
                    .collect();
                shared.lock().unwrap().pending = backlog.len();

                let handle = |job: IndexJob,
                              index: &SearchIndex,
                              writer: &mut tantivy::IndexWriter| {
                    match job {
                        IndexJob::Index { id, title } => {
                            {
                                let mut st = shared.lock().unwrap();
                                st.current = Some(title.clone());
                            }
                            match blobs.get(&id) {
                                Ok(bytes) => match Pdf::new(bytes) {
                                    Ok(pdf) => {
                                        let mut needs_ocr = 0usize;
                                        let texts: Vec<String> = pdf
                                            .pages()
                                            .iter()
                                            .map(|p| {
                                                let extracted = extract_page_text(p, &settings);
                                                if extracted.text.trim().len() < 32
                                                    || extracted.unmapped_ratio > 0.3
                                                {
                                                    needs_ocr += 1;
                                                }
                                                extracted.text
                                            })
                                            .collect();
                                        shared.lock().unwrap().ocr_pending += needs_ocr;
                                        if let Err(e) =
                                            index.index_document(writer, &id, &title, &texts)
                                        {
                                            shared.lock().unwrap().last_error = Some(e.to_string());
                                        }
                                    }
                                    Err(_) => {
                                        shared.lock().unwrap().last_error =
                                            Some(format!("could not parse {title} for indexing"));
                                    }
                                },
                                Err(e) => {
                                    shared.lock().unwrap().last_error = Some(e.to_string());
                                }
                            }
                        }
                        IndexJob::Delete { id } => {
                            if let Err(e) = index.delete_document(writer, &id) {
                                shared.lock().unwrap().last_error = Some(e.to_string());
                            }
                        }
                    }
                    let mut st = shared.lock().unwrap();
                    st.pending = st.pending.saturating_sub(1);
                    st.current = None;
                };

                for (id, title) in backlog {
                    handle(IndexJob::Index { id, title }, &index, &mut writer);
                    ctx.request_repaint();
                }

                while let Ok(job) = rx.recv() {
                    handle(job, &index, &mut writer);
                    ctx.request_repaint();
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
}
