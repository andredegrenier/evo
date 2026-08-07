//! The `evo-text` background worker behind find-in-document (⌘F).
//!
//! One thread per opened document: it walks every source page, extracts
//! positioned text, and falls back to OCR for pages whose text layer is too
//! thin — but only when the OCR models are already on disk, so pressing ⌘F
//! never starts a download.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;

use super::extract::{self, PageTextLayout, TextSource};
use super::ocr;

/// Same thresholds the indexer uses to decide a page needs OCR.
const MIN_EMBEDDED_CHARS: usize = 32;
const MAX_UNMAPPED_RATIO: f32 = 0.3;

pub struct TextWorker {
    rx: Receiver<(usize, PageTextLayout)>,
}

impl TextWorker {
    /// Start extracting text for `source`. Results arrive per source page via
    /// [`TextWorker::try_recv`]; the thread exits when the document is closed
    /// (receiver dropped) or every page has been sent.
    pub fn spawn(
        source: Arc<Vec<u8>>,
        models_dir: Option<PathBuf>,
        ctx: eframe::egui::Context,
    ) -> Self {
        let (tx, rx) = channel::<(usize, PageTextLayout)>();
        std::thread::Builder::new()
            .name("evo-text".into())
            .spawn(move || {
                let Ok(pdf) = Pdf::new(source) else {
                    return;
                };
                let settings = InterpreterSettings::default();
                let models_dir = models_dir.filter(|dir| ocr::models_present(dir));
                let mut engine: Option<ocr::Ocr> = None;

                let pages = pdf.pages();
                for (i, page) in pages.iter().enumerate() {
                    let (mut layout, unmapped) = extract::extract_page_layout(page, &settings);
                    let thin = extract::join_lines(&layout.lines).trim().len() < MIN_EMBEDDED_CHARS
                        || unmapped > MAX_UNMAPPED_RATIO;
                    if thin && let Some(dir) = &models_dir {
                        if engine.is_none() {
                            // Models are present, so this never downloads.
                            engine = ocr::Ocr::load(dir).ok();
                        }
                        if let Some(engine) = &engine
                            && let Ok(lines) = ocr::ocr_page_layout(engine, page, &settings)
                            && !lines.is_empty()
                        {
                            layout = PageTextLayout {
                                lines,
                                source: Some(TextSource::Ocr),
                            };
                        }
                    }
                    if tx.send((i, layout)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("failed to spawn text thread");
        Self { rx }
    }

    pub fn try_recv(&self) -> Option<(usize, PageTextLayout)> {
        self.rx.try_recv().ok()
    }
}
