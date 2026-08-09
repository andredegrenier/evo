//! Drawing a page with each engine.
//!
//! evo is a binary crate with no library target, so the harness cannot call
//! `src/render/engine.rs`; it mirrors it instead. The mirror is deliberately
//! literal -- same hayro `RenderSettings` (opaque white background, one scale
//! on both axes), same `take_unpremultiplied()` straight-alpha RGBA, same
//! `PdfRenderConfig::scale_page_by_factor`, same `as_rgba_bytes()` -- because
//! the whole point of the hayro hash is to notice when evo's pixels move, and
//! a mirror that renders differently would notice the wrong things. The two
//! crates come from one lock file, so both halves always draw with the same
//! hayro and the same pdfium-render.
//!
//! If `engine.rs` ever grows render settings that matter, they belong here
//! too, and the baseline needs re-blessing. (The alternative -- giving evo a
//! library target so xtask could `use evo::render::engine` -- would compile
//! eframe, tantivy and llama.cpp to hash a bitmap.)

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::AlphaColor;
use hayro::{RenderCache, RenderSettings};
use pdfium_render::prelude::{PdfDocument, PdfRenderConfig, Pdfium};

/// One rasterized page: straight-alpha RGBA, row-major, tightly packed.
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

// ---------------------------------------------------------------------------
// hayro
// ---------------------------------------------------------------------------

/// A document hayro has parsed, plus the warning flag its sink sets.
pub struct HayroDoc {
    pdf: Pdf,
    settings: InterpreterSettings,
    warnings: Arc<AtomicBool>,
}

impl HayroDoc {
    /// `None` when hayro cannot parse these bytes at all -- which the harness
    /// records as evidence rather than treating as a failure.
    pub fn open(bytes: Arc<Vec<u8>>, password: Option<&str>) -> Option<Self> {
        let pdf = Pdf::new_with_password(bytes, password.unwrap_or_default()).ok()?;
        let warnings = Arc::new(AtomicBool::new(false));
        let sink = warnings.clone();
        let settings = InterpreterSettings {
            warning_sink: Arc::new(move |_| sink.store(true, Ordering::Relaxed)),
            ..Default::default()
        };
        Some(Self {
            pdf,
            settings,
            warnings,
        })
    }

    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    /// Whether anything in this document has been drawn approximately so far.
    /// Read after the pages have been rendered, not before.
    pub fn had_warnings(&self) -> bool {
        self.warnings.load(Ordering::Relaxed)
    }

    pub fn render(&self, page: usize, scale: f32) -> Option<Rendered> {
        let pages = self.pdf.pages();
        let target = pages.get(page)?;
        // A cache per page rather than per document: the harness renders each
        // page once, and a cache that lives no longer than the borrow it came
        // from spares this file the self-referential struct `engine.rs` needs.
        let cache = RenderCache::new();
        let pixmap = hayro::render(
            target,
            &cache,
            &self.settings,
            &RenderSettings {
                x_scale: scale,
                y_scale: scale,
                width: None,
                height: None,
                bg_color: AlphaColor::WHITE,
            },
        );
        let (width, height) = (pixmap.width() as u32, pixmap.height() as u32);
        let rgba = pixmap
            .take_unpremultiplied()
            .into_iter()
            .flat_map(|p| [p.r, p.g, p.b, p.a])
            .collect();
        Some(Rendered {
            width,
            height,
            rgba,
        })
    }
}

// ---------------------------------------------------------------------------
// PDFium
// ---------------------------------------------------------------------------

/// Where the harness looks for PDFium: `EVO_PDFIUM_PATH` first (a file or the
/// directory holding one), then the build directories a developer would have
/// run `evo fetch-pdfium --into target/debug` into, then the system library.
///
/// A shorter list than the app's -- there is no `.app` bundle to look inside
/// when the caller is cargo.
fn library_path(repo: &std::path::Path) -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    };
    let at = |candidate: PathBuf| -> Option<PathBuf> {
        if candidate.is_file() {
            return Some(candidate);
        }
        let inside = candidate.join(name);
        inside.is_file().then_some(inside)
    };

    if let Some(from_env) = std::env::var_os("EVO_PDFIUM_PATH")
        && let Some(found) = at(PathBuf::from(from_env))
    {
        return Some(found);
    }
    at(repo.join("target/debug")).or_else(|| at(repo.join("target/release")))
}

/// PDFium, bound once, or `None` on a machine that has not got it.
///
/// The harness is single-threaded, so unlike the app it needs no lock around
/// the FFI: there is only ever one caller.
pub struct PdfiumEngine {
    pdfium: Pdfium,
    pub path: Option<PathBuf>,
}

impl PdfiumEngine {
    pub fn find(repo: &std::path::Path) -> Option<Self> {
        let path = library_path(repo);
        let bindings = match &path {
            Some(path) => Pdfium::bind_to_library(path)
                .inspect_err(|e| eprintln!("found {} but could not load it: {e}", path.display()))
                .ok()?,
            None => Pdfium::bind_to_system_library().ok()?,
        };
        Some(Self {
            pdfium: Pdfium::new(bindings),
            path,
        })
    }

    pub fn open(&self, bytes: &[u8], password: Option<&str>) -> Option<PdfDocument<'_>> {
        self.pdfium
            .load_pdf_from_byte_vec(bytes.to_vec(), password)
            .ok()
    }
}

/// Draw one page with PDFium, or `None` if PDFium will not draw it.
pub fn pdfium_render(doc: &PdfDocument<'_>, page: usize, scale: f32) -> Option<Rendered> {
    let index = i32::try_from(page).ok()?;
    let page = doc.pages().get(index).ok()?;
    let config = PdfRenderConfig::new().scale_page_by_factor(scale);
    let bitmap = page.render_with_config(&config).ok()?;
    let (width, height) = (bitmap.width(), bitmap.height());
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(Rendered {
        width: width as u32,
        height: height as u32,
        rgba: bitmap.as_rgba_bytes(),
    })
}
