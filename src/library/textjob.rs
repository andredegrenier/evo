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

use super::extract::{self, PageTextLayout, TextSource};
use super::ocr;
use crate::render::engine::{self as render_engine, EngineDoc, EnginePref};

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
        password: Option<String>,
        models_dir: Option<PathBuf>,
        ctx: eframe::egui::Context,
        pref: EnginePref,
    ) -> Self {
        let (tx, rx) = channel::<(usize, PageTextLayout)>();
        std::thread::Builder::new()
            .name("evo-text".into())
            .spawn(move || {
                let Ok(pdf) =
                    crate::doc::open_pdf(source.clone(), password.as_deref().unwrap_or_default())
                else {
                    return;
                };
                let settings = InterpreterSettings::default();
                let models_dir = models_dir.filter(|dir| ocr::models_present(dir));
                // `None` = not tried yet. Text extraction is hayro's job in
                // every mode; the pixels OCR reads are the chosen engine's, so
                // when a scanned page turns up both documents are open at once
                // on this one thread.
                let mut engine: Option<Option<ocr::Ocr>> = None;
                let mut rasterizer: Option<Box<dyn EngineDoc>> = None;

                let pages = pdf.pages();
                for (i, page) in pages.iter().enumerate() {
                    let (mut layout, unmapped) = extract::extract_page_layout(page, &settings);
                    let thin = extract::join_lines(&layout.lines).trim().len() < MIN_EMBEDDED_CHARS
                        || unmapped > MAX_UNMAPPED_RATIO;
                    if thin && let Some(dir) = &models_dir {
                        let engine = engine.get_or_insert_with(|| {
                            // Models are present, so this never downloads.
                            let loaded = ocr::Ocr::load(dir).ok();
                            if loaded.is_some() {
                                rasterizer =
                                    render_engine::open(source.clone(), password.as_deref(), pref)
                                        .ok();
                            }
                            loaded
                        });
                        if let (Some(engine), Some(doc)) = (engine.as_ref(), &mut rasterizer)
                            && let Ok(lines) = ocr::ocr_page_layout(engine, doc.as_mut(), i)
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
