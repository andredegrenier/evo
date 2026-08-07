//! Background page rasterization. hayro's render cache is not thread-safe
//! (`Rc` internally), so one worker thread owns the parsed `Pdf` and its
//! caches; the UI thread sends requests and receives finished RGBA images.

pub mod cache;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use eframe::egui;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::AlphaColor;
use hayro::{RenderCache, RenderSettings};

/// Quantize a raw scale (framebuffer pixels per PDF point) into a bucket so
/// the texture cache gets reusable keys while zooming.
pub fn scale_bucket(scale: f32) -> f32 {
    ((scale * 4.0).ceil() / 4.0).clamp(0.25, 8.0)
}

/// Cache/request key for a bucketed scale.
pub fn scale_key(scale: f32) -> u32 {
    (scale * 100.0).round() as u32
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RenderRequest {
    /// Original-document page index.
    pub page: usize,
    /// Framebuffer pixels per PDF point (already bucketed).
    pub scale: f32,
}

pub struct RenderResponse {
    pub page: usize,
    pub scale: f32,
    pub image: egui::ColorImage,
}

pub struct RenderWorker {
    tx: Sender<RenderRequest>,
    rx: Receiver<RenderResponse>,
    had_warnings: Arc<AtomicBool>,
}

impl RenderWorker {
    /// Spawn the worker. `source` must already have been validated by
    /// [`crate::doc::Document::load_bytes`].
    pub fn spawn(source: Arc<Vec<u8>>, ctx: egui::Context) -> Self {
        let (req_tx, req_rx) = channel::<RenderRequest>();
        let (res_tx, res_rx) = channel::<RenderResponse>();
        let had_warnings = Arc::new(AtomicBool::new(false));
        let warnings = had_warnings.clone();

        std::thread::Builder::new()
            .name("evo-render".into())
            .spawn(move || {
                let Ok(pdf) = Pdf::new(source) else {
                    return;
                };
                let cache = RenderCache::new();
                let warn = warnings.clone();
                let settings = InterpreterSettings {
                    warning_sink: Arc::new(move |_| warn.store(true, Ordering::Relaxed)),
                    ..Default::default()
                };

                // Block for the first request, then drain the queue and keep
                // only the newest request per page so a fast zoom doesn't
                // build a backlog of stale renders.
                while let Ok(first) = req_rx.recv() {
                    let mut batch: Vec<RenderRequest> = vec![first];
                    loop {
                        match req_rx.try_recv() {
                            Ok(req) => {
                                batch.retain(|r| r.page != req.page);
                                batch.push(req);
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }

                    for req in batch {
                        let pages = pdf.pages();
                        let Some(page) = pages.get(req.page) else {
                            continue;
                        };
                        let pixmap = hayro::render(
                            page,
                            &cache,
                            &settings,
                            &RenderSettings {
                                x_scale: req.scale,
                                y_scale: req.scale,
                                width: None,
                                height: None,
                                bg_color: AlphaColor::WHITE,
                            },
                        );
                        let size = [pixmap.width() as usize, pixmap.height() as usize];
                        let image = egui::ColorImage::from_rgba_premultiplied(
                            size,
                            pixmap.data_as_u8_slice(),
                        );
                        if res_tx
                            .send(RenderResponse {
                                page: req.page,
                                scale: req.scale,
                                image,
                            })
                            .is_err()
                        {
                            return;
                        }
                        ctx.request_repaint();
                    }
                }
            })
            .expect("failed to spawn render thread");

        Self {
            tx: req_tx,
            rx: res_rx,
            had_warnings,
        }
    }

    pub fn request(&self, req: RenderRequest) {
        let _ = self.tx.send(req);
    }

    pub fn try_recv(&self) -> Option<RenderResponse> {
        self.rx.try_recv().ok()
    }

    pub fn had_warnings(&self) -> bool {
        self.had_warnings.load(Ordering::Relaxed)
    }
}
