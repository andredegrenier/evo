//! Background page rasterization. No rasterizer's document is thread-safe --
//! hayro's render cache is an `Rc`, PDFium's handles belong to whoever opened
//! them -- so one worker thread owns the opened document, and the UI thread
//! sends requests and receives finished RGBA images.

pub mod cache;
pub mod engine;
#[cfg(feature = "pdfium")]
pub mod pdfium;
pub mod pdfium_fetch;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use eframe::egui;

use engine::{Engine, EnginePref};

/// Scale the page rail renders at. Lives here rather than in the UI because
/// the worker has to tell rail requests apart from canvas ones.
pub const THUMB_SCALE: f32 = 0.22;

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

impl RenderRequest {
    /// Rail thumbnails and canvas pages are separate classes of work: they go
    /// to separate caches and are wanted at the same time.
    fn is_thumb(&self) -> bool {
        (self.scale - THUMB_SCALE).abs() < 1e-3
    }
}

pub struct RenderResponse {
    pub page: usize,
    pub scale: f32,
    /// `None` when the request was superseded before it ran. The worker always
    /// answers, so the cache can clear the pending flag either way.
    pub image: Option<egui::ColorImage>,
    /// Which rasterizer drew it.
    pub engine: Engine,
}

/// Add `req` to a pending batch, dropping any request it supersedes: the same
/// page, at the same class of scale. Dropped requests are returned so the
/// worker can report them.
///
/// Coalescing across classes would let a rail thumbnail cancel a full-quality
/// canvas render of the same page (they share one queue), which is how pages
/// used to get stuck blurry.
fn coalesce(batch: &mut Vec<RenderRequest>, req: RenderRequest) -> Vec<RenderRequest> {
    let mut dropped = Vec::new();
    batch.retain(|r| {
        if r.page == req.page && r.is_thumb() == req.is_thumb() {
            dropped.push(*r);
            false
        } else {
            true
        }
    });
    batch.push(req);
    dropped
}

pub struct RenderWorker {
    tx: Sender<RenderRequest>,
    rx: Receiver<RenderResponse>,
    had_warnings: Arc<AtomicBool>,
}

impl RenderWorker {
    /// Spawn the worker. `source` must already have been validated by
    /// [`crate::doc::Document::load_bytes`].
    pub fn spawn(
        source: Arc<Vec<u8>>,
        ctx: egui::Context,
        pref: EnginePref,
        password: Option<String>,
    ) -> Self {
        let (req_tx, req_rx) = channel::<RenderRequest>();
        let (res_tx, res_rx) = channel::<RenderResponse>();
        let had_warnings = Arc::new(AtomicBool::new(false));
        let warnings = had_warnings.clone();

        std::thread::Builder::new()
            .name("evo-render".into())
            .spawn(move || {
                let mut doc = match engine::open(source, password.as_deref(), pref) {
                    Ok(doc) => doc,
                    Err(e) => {
                        eprintln!("{e}");
                        return;
                    }
                };
                let engine = doc.engine();
                ctx.request_repaint();

                // Block for the first request, then drain the queue and keep
                // only the newest request per page so a fast zoom doesn't
                // build a backlog of stale renders.
                while let Ok(first) = req_rx.recv() {
                    let mut batch: Vec<RenderRequest> = vec![first];
                    let mut dropped: Vec<RenderRequest> = Vec::new();
                    loop {
                        match req_rx.try_recv() {
                            Ok(req) => dropped.extend(coalesce(&mut batch, req)),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }

                    // Report the superseded ones so the cache stops waiting on
                    // them; an unanswered request would pin its pending flag
                    // and the page would never be asked for again.
                    for req in dropped {
                        if res_tx
                            .send(RenderResponse {
                                page: req.page,
                                scale: req.scale,
                                image: None,
                                engine,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }

                    for req in batch {
                        let Some(drawn) = doc.render(req.page, req.scale) else {
                            continue;
                        };
                        let size = [drawn.width as usize, drawn.height as usize];
                        let image = egui::ColorImage::from_rgba_unmultiplied(size, &drawn.rgba);
                        // hayro only notices trouble while drawing, so the
                        // flag the status bar watches is republished after
                        // every page rather than set once at open.
                        warnings.store(doc.had_warnings(), Ordering::Relaxed);
                        if res_tx
                            .send(RenderResponse {
                                page: req.page,
                                scale: req.scale,
                                image: Some(image),
                                engine,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(page: usize, scale: f32) -> RenderRequest {
        RenderRequest { page, scale }
    }

    #[test]
    fn newest_request_for_a_page_wins() {
        let mut batch = vec![req(0, 1.0)];
        let dropped = coalesce(&mut batch, req(0, 2.0));
        assert_eq!(batch, vec![req(0, 2.0)]);
        assert_eq!(dropped, vec![req(0, 1.0)]);
    }

    #[test]
    fn a_thumbnail_never_cancels_a_page_render() {
        let mut batch = vec![req(0, 4.0)];
        let dropped = coalesce(&mut batch, req(0, THUMB_SCALE));
        assert_eq!(batch, vec![req(0, 4.0), req(0, THUMB_SCALE)]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_page_render_never_cancels_a_thumbnail() {
        let mut batch = vec![req(0, THUMB_SCALE)];
        let dropped = coalesce(&mut batch, req(0, 4.0));
        assert_eq!(batch, vec![req(0, THUMB_SCALE), req(0, 4.0)]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn thumbnails_coalesce_with_each_other() {
        let mut batch = vec![req(0, THUMB_SCALE)];
        let dropped = coalesce(&mut batch, req(0, THUMB_SCALE));
        assert_eq!(batch, vec![req(0, THUMB_SCALE)]);
        assert_eq!(dropped, vec![req(0, THUMB_SCALE)]);
    }

    #[test]
    fn other_pages_are_untouched() {
        let mut batch = vec![req(0, 1.0), req(1, 1.0)];
        let dropped = coalesce(&mut batch, req(2, 1.0));
        assert_eq!(batch, vec![req(0, 1.0), req(1, 1.0), req(2, 1.0)]);
        assert!(dropped.is_empty());
    }

    /// A protected document draws like any other once its password has been
    /// handed to the worker -- which is the whole reason `spawn` takes one.
    #[test]
    fn the_worker_draws_a_page_of_a_protected_document() {
        for path in crate::doc::tests::PROTECTED {
            let source = Arc::new(crate::doc::tests::encrypted(path));
            // A detached context: nobody is listening for the repaints.
            let worker = RenderWorker::spawn(
                source,
                egui::Context::default(),
                EnginePref::Hayro,
                Some("evo".to_owned()),
            );
            worker.request(RenderRequest {
                page: 0,
                scale: 1.0,
            });

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let response = loop {
                if let Some(res) = worker.try_recv() {
                    break res;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{path}: the worker never answered"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let image = response
                .image
                .unwrap_or_else(|| panic!("{path}: no pixels"));
            assert_eq!(image.size, [612, 792], "{path}");
            assert_eq!(response.engine, Engine::Hayro, "{path}");
        }
    }

    /// The same document without the password: the worker cannot open it, and
    /// has to stop rather than answer with a blank page forever.
    #[test]
    fn the_worker_gives_up_on_a_document_it_cannot_open() {
        let source = Arc::new(crate::doc::tests::encrypted(
            crate::doc::tests::PROTECTED[0],
        ));
        let worker = RenderWorker::spawn(source, egui::Context::default(), EnginePref::Hayro, None);
        worker.request(RenderRequest {
            page: 0,
            scale: 1.0,
        });
        // The thread returns on the failed open, so the response channel is
        // closed and nothing ever arrives.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(worker.try_recv().is_none());
    }

    #[test]
    fn scale_bucket_is_quantized_and_clamped() {
        assert_eq!(scale_bucket(1.0), 1.0);
        assert_eq!(scale_bucket(1.1), 1.25);
        assert_eq!(scale_bucket(0.01), 0.25);
        assert_eq!(scale_bucket(99.0), 8.0);
    }
}
