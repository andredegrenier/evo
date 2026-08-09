//! The PDFium backend.
//!
//! PDFium is the rasterizer inside Chrome, and the reference implementation
//! most PDF producers actually test against. evo binds to it dynamically --
//! `libloading` at runtime, no C toolchain at build time -- so a build without
//! the library, or a `--no-default-features` build without this module at all,
//! is the pure-Rust evo that came before it.
//!
//! **Every call into PDFium in this process goes through [`locked`].** PDFium
//! is a single-threaded C++ library with process-global state -- the installed
//! font mapper above all -- and `pdfium-render`'s `thread_safe` feature turns
//! out to be an empty feature flag in 0.9.3: it makes `Pdfium` `Send + Sync`
//! and adds no mutex of its own. Two threads rendering at once corrupts
//! PDFium's heap within a page or two (verified: a malloc abort inside
//! `CFX_FontMapper::AddInstalledFont`), and evo renders from several threads by
//! design -- the render worker, thumbnail jobs, and `evo serve`'s blocking
//! pool. So the lock is evo's, it is global, and it is held across whole
//! operations rather than individual FFI calls.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use pdfium_render::prelude::{PdfDocument, PdfRenderConfig, Pdfium};

use super::engine::{Engine, EngineDoc, OpenError, RenderedPage, pdfium_library_path};

/// The one PDFium in this process.
///
/// `pdfium-render` keeps its bindings in a global that may be initialized
/// exactly once, so evo cannot bind per document however much it might like
/// to; a `OnceLock` makes that constraint explicit.
static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

/// Set once evo has looked for the library and not found one. Without it,
/// every page of every document on a machine with no PDFium would pay for
/// another failed `dlopen`.
static SEARCHED_IN_VAIN: AtomicBool = AtomicBool::new(false);

/// Serializes everything that touches PDFium. See the module comment.
static FFI: Mutex<()> = Mutex::new(());

/// Take the PDFium lock, ignoring poisoning.
///
/// A thread that panicked mid-render leaves PDFium's globals in whatever state
/// it left them, and refusing to draw anything ever again would be a worse
/// answer than carrying on: the guard protects against concurrency, not
/// against a caller's bug.
fn locked() -> MutexGuard<'static, ()> {
    FFI.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The process's PDFium, loading it on first use.
///
/// Callers reach PDFium only through this, so nothing can be calling into the
/// library while it is being bound.
pub fn instance() -> Option<&'static Pdfium> {
    if let Some(pdfium) = PDFIUM.get() {
        return Some(pdfium);
    }
    if SEARCHED_IN_VAIN.load(Ordering::Relaxed) {
        return None;
    }

    // The lock does double duty here: it keeps a second thread out of PDFium
    // while the first is binding, and it means only one thread at a time pays
    // for a failed search.
    let _guard = locked();
    if let Some(pdfium) = PDFIUM.get() {
        return Some(pdfium);
    }
    if SEARCHED_IN_VAIN.load(Ordering::Relaxed) {
        return None;
    }

    let bindings = match pdfium_library_path() {
        Some(path) => Pdfium::bind_to_library(&path)
            .inspect_err(|e| eprintln!("evo found {} but could not load it: {e}", path.display()))
            .ok(),
        // Nothing evo installed, nothing beside the binary: the last chance is
        // a PDFium the operating system already has.
        None => Pdfium::bind_to_system_library().ok(),
    };
    match bindings {
        Some(bindings) => {
            let _ = PDFIUM.set(Pdfium::new(bindings));
            PDFIUM.get()
        }
        None => {
            SEARCHED_IN_VAIN.store(true, Ordering::Relaxed);
            None
        }
    }
}

/// Look again, after something has been installed.
///
/// `evo fetch-pdfium` and the Preferences button put a library where there was
/// none a moment ago, and a user who has just watched a download finish should
/// not have to restart evo for it to count. Only a *failed* search is
/// forgotten -- once PDFium is bound it is bound for the life of the process,
/// because `pdfium-render`'s bindings can be initialized only once.
pub fn search_again() {
    SEARCHED_IN_VAIN.store(false, Ordering::Relaxed);
}

pub struct PdfiumEngineDoc {
    /// `Option` only so that [`Drop`] can close the document while holding the
    /// lock: closing is an FFI call like any other, and a field dropped after
    /// `drop` returns would make it the one unguarded call in the module.
    document: Option<PdfDocument<'static>>,
}

impl PdfiumEngineDoc {
    pub fn open(bytes: &[u8], password: Option<&str>) -> Result<Self, OpenError> {
        let pdfium = instance().ok_or(OpenError::PdfiumMissing)?;
        let _guard = locked();
        // PDFium reads from the buffer for as long as the document is open, so
        // it is handed an owned copy rather than a borrow of the `Arc` evo
        // shares with the parser and the exporter.
        let document = pdfium
            .load_pdf_from_byte_vec(bytes.to_vec(), password)
            .map_err(|e| OpenError::Pdfium(e.to_string()))?;
        Ok(Self {
            document: Some(document),
        })
    }
}

impl Drop for PdfiumEngineDoc {
    fn drop(&mut self) {
        let _guard = locked();
        drop(self.document.take());
    }
}

/// PDFium indexes pages with a `c_int`; anything past that is not a document.
fn page_index(page: usize) -> Option<i32> {
    i32::try_from(page).ok()
}

impl EngineDoc for PdfiumEngineDoc {
    fn engine(&self) -> Engine {
        Engine::Pdfium
    }

    fn page_count(&self) -> usize {
        let _guard = locked();
        self.document
            .as_ref()
            .map_or(0, |doc| doc.pages().len() as usize)
    }

    fn page_size(&self, page: usize) -> Option<(f32, f32)> {
        let index = page_index(page)?;
        let _guard = locked();
        // PDFium applies the page's own /Rotate before reporting a size, which
        // is what hayro's `render_dimensions` does too.
        let page = self.document.as_ref()?.pages().get(index).ok()?;
        Some((page.width().value, page.height().value))
    }

    fn render(&mut self, page: usize, scale: f32) -> Option<RenderedPage> {
        let index = page_index(page)?;
        let _guard = locked();
        let page = self.document.as_ref()?.pages().get(index).ok()?;
        // Both axes by the same factor: the caller has already decided how
        // many pixels a point is worth, and the aspect ratio is the document's
        // business, not the renderer's.
        let config = PdfRenderConfig::new().scale_page_by_factor(scale);
        let bitmap = page.render_with_config(&config).ok()?;
        let (width, height) = (bitmap.width(), bitmap.height());
        if width <= 0 || height <= 0 {
            return None;
        }
        // `as_rgba_bytes` normalizes whatever PDFium produced -- by default a
        // reversed-byte-order BGRA buffer, which is already RGBA -- into the
        // straight-alpha RGBA the trait promises. The page was cleared to
        // opaque white first, so nothing is partly transparent.
        Some(RenderedPage {
            width: width as u32,
            height: height as u32,
            rgba: bitmap.as_rgba_bytes(),
        })
    }

    fn had_warnings(&self) -> bool {
        // PDFium has no warning channel: it draws what it can and says
        // nothing. The badge belongs to hayro renders only.
        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::engine::{self, Engine, EnginePref, Zoom};

    fn fixture() -> Arc<Vec<u8>> {
        Arc::new(std::fs::read("tests/fixtures/sample.pdf").expect("the fixture"))
    }

    /// The pixel contract, settled empirically rather than by reading two
    /// rasterizers' documentation: PDFium has to agree with hayro about how
    /// big a page is, and it has to draw a blank page as opaque white in the
    /// channel order evo hands to egui and to PNG.
    ///
    /// Ignored by default because it needs the shared library:
    ///
    /// ```text
    /// cargo run -- fetch-pdfium --into target/debug
    /// cargo test -- --ignored pdfium_
    /// ```
    #[test]
    #[ignore = "needs the PDFium library; run with --ignored pdfium_"]
    fn pdfium_draws_a_blank_page_opaque_white_at_hayro_s_size() {
        assert!(
            engine::pdfium_available(),
            "PDFium is not loadable: {:?}. Run `evo fetch-pdfium` or set EVO_PDFIUM_PATH.",
            engine::pdfium_library_path()
        );

        let mut hayro = engine::open(fixture(), None, EnginePref::Hayro).expect("hayro opens");
        let mut pdfium = engine::open(fixture(), None, EnginePref::Pdfium).expect("PDFium opens");
        assert_eq!(pdfium.engine(), Engine::Pdfium);
        assert_eq!(pdfium.page_count(), hayro.page_count());
        assert!(!pdfium.had_warnings(), "PDFium has no warning channel");

        for page in 0..hayro.page_count() {
            let (hw, hh) = hayro.page_size(page).expect("hayro size");
            let (pw, ph) = pdfium.page_size(page).expect("PDFium size");
            assert!((hw - pw).abs() <= 1.0, "page {page}: {hw} vs {pw}");
            assert!((hh - ph).abs() <= 1.0, "page {page}: {hh} vs {ph}");
        }

        // The rail's thumbnail scale is in the list on purpose: it is the
        // one where the two rasterizers round the pixel size differently
        // (hayro truncates, PDFium rounds), which is what the tolerance is for.
        for scale in [1.0_f32, 2.0, crate::render::THUMB_SCALE] {
            let drawn = hayro.render(0, scale).expect("hayro draws");
            let same = pdfium.render(0, scale).expect("PDFium draws");
            assert!(
                (drawn.width as i64 - same.width as i64).abs() <= 1,
                "at {scale}x: {} vs {}",
                drawn.width,
                same.width
            );
            assert!(
                (drawn.height as i64 - same.height as i64).abs() <= 1,
                "at {scale}x: {} vs {}",
                drawn.height,
                same.height
            );
            assert_eq!(
                same.rgba.len(),
                same.width as usize * same.height as usize * 4,
                "the buffer is not tightly packed RGBA"
            );
            assert!(
                same.rgba.chunks(4).all(|p| p[3] == 255),
                "the page is not opaque"
            );
        }

        // The fixture's second page has a wide top margin, so the top-left
        // corner is page background: white in every channel, in RGBA order.
        let page = pdfium.render(1, 1.0).expect("page two");
        assert_eq!(
            &page.rgba[..4],
            &[255, 255, 255, 255],
            "not white, or not RGBA"
        );
    }

    /// `Auto` has to actually choose PDFium once the library is there, and the
    /// one-shot path has to report that it did.
    #[test]
    #[ignore = "needs the PDFium library; run with --ignored pdfium_"]
    fn pdfium_is_what_automatic_chooses_when_it_is_installed() {
        assert!(engine::pdfium_available());
        assert_eq!(engine::resolve(EnginePref::Auto), Engine::Pdfium);

        let (page, used) =
            engine::render_page(fixture(), None, 0, Zoom::Factor(1.0), EnginePref::Auto)
                .expect("page one");
        assert_eq!(used, Engine::Pdfium);
        assert_eq!((page.width, page.height), (612, 792));

        let (thumb, used) =
            engine::render_page(fixture(), None, 0, Zoom::FitWidth(320.0), EnginePref::Auto)
                .expect("a thumbnail");
        assert_eq!(used, Engine::Pdfium);
        assert!((thumb.width as i64 - 320).abs() <= 1, "{}", thumb.width);
    }

    /// PDFium takes the same password hayro does, for every encryption evo
    /// ships a fixture for -- so switching renderers on a protected document
    /// is not a way to lose it.
    #[test]
    #[ignore = "needs the PDFium library; run with --ignored pdfium_"]
    fn pdfium_opens_the_encrypted_fixtures_with_their_password() {
        assert!(engine::pdfium_available());
        for path in crate::doc::tests::PROTECTED {
            let bytes = std::sync::Arc::new(crate::doc::tests::encrypted(path));
            let (page, used) = engine::render_page(
                bytes.clone(),
                Some("evo"),
                0,
                Zoom::Factor(1.0),
                EnginePref::Pdfium,
            )
            .unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(used, Engine::Pdfium, "{path}");
            assert_eq!((page.width, page.height), (612, 792), "{path}");

            // And without it, an error rather than a blank page.
            assert!(
                engine::open(bytes, None, EnginePref::Pdfium).is_err(),
                "{path}: opened with no password"
            );
        }
    }

    /// The desktop path, end to end: the render worker opens the document,
    /// draws a page, and says which engine it used -- which is what the status
    /// bar reads and what the texture cache stores.
    #[test]
    #[ignore = "needs the PDFium library; run with --ignored pdfium_"]
    fn pdfium_is_what_the_render_worker_uses() {
        use crate::render::{RenderRequest, RenderWorker};

        assert!(engine::pdfium_available());
        // A detached context: nothing is listening for the repaints the worker
        // asks for, which is exactly right for a test.
        let worker = RenderWorker::spawn(
            fixture(),
            eframe::egui::Context::default(),
            EnginePref::Auto,
            None,
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
                "the worker never answered"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        assert_eq!(response.engine, Engine::Pdfium);
        assert_eq!(response.page, 0);
        let image = response.image.expect("a drawn page");
        assert_eq!(image.size, [612, 792]);
        assert!(!worker.had_warnings(), "PDFium raises no hayro warnings");
    }

    /// PDFium has process-global state and `pdfium-render`'s `thread_safe`
    /// feature does nothing about it, so the lock in this module is the only
    /// thing standing between evo's several rendering threads and a corrupted
    /// heap. Without it this test aborts inside PDFium's font mapper within a
    /// second, which is how the bug was found.
    #[test]
    #[ignore = "needs the PDFium library; run with --ignored pdfium_"]
    fn pdfium_draws_from_several_threads_at_once() {
        assert!(engine::pdfium_available());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..4 {
                        let mut doc = engine::open(fixture(), None, EnginePref::Pdfium)
                            .expect("PDFium opens");
                        for page in 0..doc.page_count() {
                            let drawn = doc.render(page, 1.5).expect("PDFium draws");
                            assert!(drawn.rgba.chunks(4).all(|p| p[3] == 255));
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no thread fell over");
        }
    }

    /// A document PDFium cannot parse is an error sentence, not a panic.
    #[test]
    #[ignore = "needs the PDFium library; run with --ignored pdfium_"]
    fn pdfium_refuses_nonsense_bytes_politely() {
        let err = engine::open(
            Arc::new(b"certainly not a PDF".to_vec()),
            None,
            EnginePref::Pdfium,
        )
        .err()
        .expect("not a PDF");
        let message = err.to_string();
        assert!(message.starts_with("PDFium could not draw"), "{message}");
    }
}
