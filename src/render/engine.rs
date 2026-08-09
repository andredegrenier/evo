//! The rasterization seam.
//!
//! evo parses PDFs in pure Rust and always will: page geometry, text, markup
//! and export are hayro and lopdf from end to end. Drawing pixels is the one
//! job where being the only implementation of a twenty-year-old specification
//! is a liability rather than a virtue, so it is the one job that is allowed a
//! second engine -- PDFium, the rasterizer in Chrome, Edge and half the PDF
//! viewers on the planet.
//!
//! Everything behind this module is an [`EngineDoc`]: an opened document that
//! can be asked for a page's size and a page's pixels. Deliberately `!Send` --
//! hayro's render cache is an `Rc` and PDFium's document handles belong to
//! whoever opened them -- so one thread owns a document, exactly as the render
//! worker already did.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::AlphaColor;
use hayro::{RenderCache, RenderSettings};
use serde::{Deserialize, Serialize};

/// No page is rasterized larger than this on either side. A PDF may declare a
/// page metres across; at a high zoom that is a picture nobody can hold in
/// memory, let alone look at.
const MAX_PIXELS: f32 = 8000.0;

/// Which rasterizer drew a page.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Engine {
    Hayro,
    Pdfium,
}

impl Engine {
    /// How the engine is named to a person.
    pub fn label(self) -> &'static str {
        match self {
            Engine::Hayro => "hayro",
            Engine::Pdfium => "PDFium",
        }
    }

    /// Lower-case and filename-safe: this is part of the `evo serve` page
    /// cache key, so that switching engines can never serve stale pixels.
    pub fn tag(self) -> &'static str {
        match self {
            Engine::Hayro => "hayro",
            Engine::Pdfium => "pdfium",
        }
    }
}

/// Which rasterizer the user asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum EnginePref {
    /// PDFium when its library is on this machine, hayro otherwise. The
    /// default, and what almost everybody should leave it on.
    #[default]
    Auto,
    Hayro,
    Pdfium,
}

impl EnginePref {
    pub fn label(self) -> &'static str {
        match self {
            EnginePref::Auto => "Automatic",
            EnginePref::Hayro => "hayro",
            EnginePref::Pdfium => "PDFium",
        }
    }
}

/// One rasterized page: straight (un-premultiplied) RGBA, row-major, no
/// padding. Every engine renders onto opaque white, so alpha is 255 throughout
/// and straight and premultiplied happen to agree -- but the contract is
/// straight, because that is what PNG encoders and `image` expect.
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for RenderedPage {
    /// Without this a failing assertion prints several megabytes of pixels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RenderedPage {{ {}x{}, {} bytes }}",
            self.width,
            self.height,
            self.rgba.len()
        )
    }
}

/// How big to draw a page.
#[derive(Clone, Copy, Debug)]
pub enum Zoom {
    /// Pixels per PDF point.
    Factor(f32),
    /// Whatever scale makes the page this many pixels wide.
    FitWidth(f32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The bytes are not a PDF evo can read.
    Unreadable,
    /// The document is there, the page is not.
    NoSuchPage(usize),
    /// PDFium was asked for by name and its library is not on this machine.
    PdfiumMissing,
    /// PDFium is here but would not open or draw this document.
    Pdfium(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Unreadable => write!(f, "evo could not read that PDF."),
            OpenError::NoSuchPage(page) => write!(f, "that document has no page {}.", page + 1),
            OpenError::PdfiumMissing => write!(
                f,
                "evo could not find the PDFium library. Run `evo fetch-pdfium` to \
                 download it, reinstall evo to get the copy that ships with it, or \
                 choose the hayro renderer in Preferences."
            ),
            OpenError::Pdfium(why) => write!(f, "PDFium could not draw that PDF: {why}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// An opened document, owned by one thread.
///
/// Not `Send` on purpose: neither engine's document handle is safe to move
/// between threads while it is being drawn from, and pretending otherwise
/// would only move the problem to a place where the compiler cannot see it.
pub trait EngineDoc {
    fn engine(&self) -> Engine;
    fn page_count(&self) -> usize;
    /// Width and height in PDF points, after the page's own `/Rotate`.
    fn page_size(&self, page: usize) -> Option<(f32, f32)>;
    /// Draw one page at `scale` framebuffer pixels per point.
    fn render(&mut self, page: usize, scale: f32) -> Option<RenderedPage>;
    /// Whether anything in this document was drawn approximately. hayro says
    /// so through its warning sink; PDFium has no equivalent and answers false.
    fn had_warnings(&self) -> bool;
}

/// Which engine a preference resolves to right now.
///
/// `Auto` asks whether PDFium can actually be loaded, not merely whether a
/// file exists somewhere: a preference that resolves to an engine that cannot
/// draw would be a blank window rather than a fallback.
pub fn resolve(pref: EnginePref) -> Engine {
    match pref {
        EnginePref::Hayro => Engine::Hayro,
        EnginePref::Pdfium => Engine::Pdfium,
        EnginePref::Auto => {
            if pdfium_available() {
                Engine::Pdfium
            } else {
                Engine::Hayro
            }
        }
    }
}

/// Open `bytes` with whichever engine `pref` resolves to.
pub fn open(
    bytes: Arc<Vec<u8>>,
    password: Option<&str>,
    pref: EnginePref,
) -> Result<Box<dyn EngineDoc>, OpenError> {
    match resolve(pref) {
        Engine::Hayro => Ok(Box::new(HayroEngineDoc::open(bytes, password)?)),
        Engine::Pdfium => open_pdfium(bytes, password),
    }
}

#[cfg(feature = "pdfium")]
fn open_pdfium(
    bytes: Arc<Vec<u8>>,
    password: Option<&str>,
) -> Result<Box<dyn EngineDoc>, OpenError> {
    Ok(Box::new(super::pdfium::PdfiumEngineDoc::open(
        &bytes, password,
    )?))
}

#[cfg(not(feature = "pdfium"))]
fn open_pdfium(
    _bytes: Arc<Vec<u8>>,
    _password: Option<&str>,
) -> Result<Box<dyn EngineDoc>, OpenError> {
    Err(OpenError::PdfiumMissing)
}

/// Open, draw one page, throw the document away.
///
/// What thumbnails and the phone server want: they touch one page of a
/// document they will not look at again, and holding a parsed PDF open for
/// that would cost more than the render.
pub fn render_page(
    bytes: Arc<Vec<u8>>,
    password: Option<&str>,
    page: usize,
    zoom: Zoom,
    pref: EnginePref,
) -> Result<(RenderedPage, Engine), OpenError> {
    let mut doc = open(bytes, password, pref)?;
    let engine = doc.engine();
    if page >= doc.page_count() {
        return Err(OpenError::NoSuchPage(page));
    }
    let (width, height) = doc.page_size(page).ok_or(OpenError::NoSuchPage(page))?;
    let scale = clamp_scale(zoom, width, height);
    let rendered = doc.render(page, scale).ok_or_else(|| match engine {
        Engine::Hayro => OpenError::Unreadable,
        Engine::Pdfium => OpenError::Pdfium("the page could not be drawn".to_owned()),
    })?;
    Ok((rendered, engine))
}

/// Turn a requested zoom into a scale no engine will choke on.
///
/// Clamped rather than refused: a page that is absurdly large is still a page
/// somebody wants to look at, only smaller than they asked for.
fn clamp_scale(zoom: Zoom, width: f32, height: f32) -> f32 {
    let scale = match zoom {
        Zoom::Factor(factor) => factor,
        Zoom::FitWidth(pixels) => pixels / width.max(1.0),
    };
    scale
        .min(MAX_PIXELS / width.max(1.0))
        .min(MAX_PIXELS / height.max(1.0))
        .clamp(0.01, 8.0)
}

/// Can PDFium draw on this machine right now?
///
/// This is the real question, so it is answered by really loading the library
/// -- once per process, cached from then on.
pub fn pdfium_available() -> bool {
    #[cfg(feature = "pdfium")]
    {
        super::pdfium::instance().is_some()
    }
    #[cfg(not(feature = "pdfium"))]
    {
        false
    }
}

/// Look for PDFium again after something has been installed, so a download
/// that has just finished counts without restarting evo.
pub fn pdfium_search_again() {
    #[cfg(feature = "pdfium")]
    super::pdfium::search_again();
}

/// Where the PDFium shared library is, if it is anywhere evo looks.
///
/// In order: `EVO_PDFIUM_PATH` (a file or the directory holding one), then
/// beside the running binary -- including the `Frameworks` directory of a
/// macOS `.app`, which is where the release bundle puts it -- then the copy
/// `evo fetch-pdfium` downloads into the data directory. `None` does not mean
/// PDFium is absent: the loader still tries the system library by name.
pub fn pdfium_library_path() -> Option<PathBuf> {
    if let Some(from_env) = std::env::var_os("EVO_PDFIUM_PATH")
        && let Some(found) = library_at(std::path::Path::new(&from_env))
    {
        return Some(found);
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        if let Some(found) = library_at(dir) {
            return Some(found);
        }
        // `evo.app/Contents/MacOS/evo` -> `evo.app/Contents/Frameworks/`.
        if let Some(found) = library_at(&dir.join("..").join("Frameworks")) {
            return Some(found);
        }
    }

    library_at(&pdfium_data_dir()?)
}

/// The library at `candidate`, which may name the file itself or the directory
/// holding it. Both spellings turn up: `EVO_PDFIUM_PATH` is typed by people,
/// and the other candidates are directories.
fn library_at(candidate: &std::path::Path) -> Option<PathBuf> {
    if candidate.is_file() {
        return Some(candidate.to_owned());
    }
    let in_dir = candidate.join(library_file_name());
    in_dir.is_file().then_some(in_dir)
}

/// The platform's name for the PDFium shared library.
pub fn library_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    }
}

/// Where `evo fetch-pdfium` installs: `<data>/pdfium/<version>/`.
///
/// Versioned, so a later evo that wants a later PDFium does not have to decide
/// whether the file already there is the one it means.
pub fn pdfium_data_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "evo")?;
    Some(
        dirs.data_dir()
            .join("pdfium")
            .join(super::pdfium_fetch::version()),
    )
}

// ---------------------------------------------------------------------------
// hayro
// ---------------------------------------------------------------------------

/// evo's original rasterizer, and still the one that draws SVG exports and
/// reads positioned text. Pure Rust, no shared library, always available.
pub struct HayroEngineDoc {
    /// hayro's glyph and object cache, which wants to live as long as the
    /// document it was built from. It cannot say so in the type system --
    /// `RenderCache<'a>` borrows from the `Pdf`, and a struct cannot hold both
    /// halves of that -- so it is declared before `pdf` (fields drop in
    /// declaration order, and the cache must go first) and its lifetime is
    /// rewritten at each use, which is the same shape hayro's own
    /// `CachedPages` uses to hold `Pages<'static>` beside the `Arc` they point
    /// into.
    cache: RenderCache<'static>,
    pdf: Pdf,
    settings: InterpreterSettings,
    warnings: Arc<AtomicBool>,
}

impl HayroEngineDoc {
    pub fn open(bytes: Arc<Vec<u8>>, password: Option<&str>) -> Result<Self, OpenError> {
        let pdf = Pdf::new_with_password(bytes, password.unwrap_or_default())
            .map_err(|_| OpenError::Unreadable)?;
        let warnings = Arc::new(AtomicBool::new(false));
        let sink = warnings.clone();
        let settings = InterpreterSettings {
            warning_sink: Arc::new(move |_| sink.store(true, Ordering::Relaxed)),
            ..Default::default()
        };
        Ok(Self {
            cache: RenderCache::new(),
            pdf,
            settings,
            warnings,
        })
    }
}

impl EngineDoc for HayroEngineDoc {
    fn engine(&self) -> Engine {
        Engine::Hayro
    }

    fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    fn page_size(&self, page: usize) -> Option<(f32, f32)> {
        Some(self.pdf.pages().get(page)?.render_dimensions())
    }

    fn render(&mut self, page: usize, scale: f32) -> Option<RenderedPage> {
        let pages = self.pdf.pages();
        let target = pages.get(page)?;
        // SAFETY: the cache holds fonts and objects borrowed from the data
        // behind `self.pdf`'s internal `Arc`, whose address is stable and
        // whose contents this struct keeps alive. The `'static` is never
        // observable outside this method -- it is narrowed here to the borrow
        // of `self.pdf` that produced `target` -- and the cache is declared
        // before `pdf` so it is dropped first.
        let cache: &RenderCache<'_> = unsafe { std::mem::transmute(&self.cache) };
        let pixmap = hayro::render(
            target,
            cache,
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
        Some(RenderedPage {
            width,
            height,
            rgba,
        })
    }

    fn had_warnings(&self) -> bool {
        self.warnings.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Arc<Vec<u8>> {
        Arc::new(std::fs::read("tests/fixtures/sample.pdf").expect("the fixture"))
    }

    #[test]
    fn hayro_opens_the_fixture_and_draws_a_page() {
        let mut doc = open(fixture(), None, EnginePref::Hayro).expect("the fixture opens");
        assert_eq!(doc.engine(), Engine::Hayro);
        assert_eq!(doc.page_count(), 2);
        assert_eq!(doc.page_size(0), Some((612.0, 792.0)));
        assert_eq!(doc.page_size(9), None);

        let page = doc.render(0, 1.0).expect("page one");
        assert_eq!((page.width, page.height), (612, 792));
        assert_eq!(page.rgba.len(), 612 * 792 * 4);
        // Opaque white background, so every pixel is fully opaque.
        assert!(page.rgba.chunks(4).all(|p| p[3] == 255));
        assert!(doc.render(9, 1.0).is_none());
    }

    #[test]
    fn nonsense_bytes_are_an_error_rather_than_a_panic() {
        let err = open(
            Arc::new(b"not a pdf at all".to_vec()),
            None,
            EnginePref::Hayro,
        )
        .err()
        .expect("not a PDF");
        assert_eq!(err, OpenError::Unreadable);
        assert!(err.to_string().starts_with("evo could not read"), "{err}");
    }

    #[test]
    fn one_shot_rendering_reports_which_engine_drew() {
        let (page, engine) = render_page(fixture(), None, 1, Zoom::Factor(2.0), EnginePref::Hayro)
            .expect("page two");
        assert_eq!(engine, Engine::Hayro);
        assert_eq!((page.width, page.height), (1224, 1584));

        let missing = render_page(fixture(), None, 9, Zoom::Factor(1.0), EnginePref::Hayro)
            .expect_err("no page ten");
        assert_eq!(missing, OpenError::NoSuchPage(9));
        assert!(missing.to_string().contains("no page 10"), "{missing}");
    }

    #[test]
    fn fit_width_gets_the_width_that_was_asked_for() {
        let (page, _) = render_page(fixture(), None, 0, Zoom::FitWidth(320.0), EnginePref::Hayro)
            .expect("a thumbnail");
        assert!((page.width as i64 - 320).abs() <= 1, "{}", page.width);
    }

    /// A page the size of a football pitch must come back as a picture, not as
    /// an allocation failure.
    #[test]
    fn an_enormous_page_is_drawn_smaller_rather_than_refused() {
        assert!(clamp_scale(Zoom::Factor(8.0), 20_000.0, 20_000.0) <= MAX_PIXELS / 20_000.0);
        assert_eq!(clamp_scale(Zoom::Factor(1.0), 612.0, 792.0), 1.0);
        assert_eq!(clamp_scale(Zoom::FitWidth(320.0), 640.0, 800.0), 0.5);
        // Zero-sized nonsense must not divide by zero.
        assert!(clamp_scale(Zoom::FitWidth(320.0), 0.0, 0.0).is_finite());
    }

    /// Both spellings of the preference have to agree about hayro, and `Auto`
    /// has to resolve to something in every build.
    #[test]
    fn preferences_resolve_to_an_engine() {
        assert_eq!(resolve(EnginePref::Hayro), Engine::Hayro);
        assert_eq!(resolve(EnginePref::Pdfium), Engine::Pdfium);
        let auto = resolve(EnginePref::Auto);
        if pdfium_available() {
            assert_eq!(auto, Engine::Pdfium);
        } else {
            assert_eq!(auto, Engine::Hayro);
        }
        assert_eq!(EnginePref::default(), EnginePref::Auto);
    }

    /// Asking for PDFium when it is not installed has to say what to do about
    /// it, because the answer is a one-line command.
    #[test]
    fn a_missing_pdfium_library_says_how_to_get_one() {
        let message = OpenError::PdfiumMissing.to_string();
        assert!(message.contains("evo fetch-pdfium"), "{message}");
        assert!(message.contains("Preferences"), "{message}");
    }

    #[test]
    fn engine_tags_are_filename_safe() {
        for engine in [Engine::Hayro, Engine::Pdfium] {
            assert!(
                engine.tag().chars().all(|c| c.is_ascii_lowercase()),
                "{}",
                engine.tag()
            );
        }
        assert_ne!(Engine::Hayro.tag(), Engine::Pdfium.tag());
    }

    #[test]
    fn the_library_file_name_is_the_platform_name() {
        let name = library_file_name();
        assert!(name.contains("pdfium"), "{name}");
        assert!(
            name.ends_with(".dll") || name.ends_with(".dylib") || name.ends_with(".so"),
            "{name}"
        );
    }

    /// Every search location -- and `EVO_PDFIUM_PATH` above all, which is the
    /// escape hatch every packaging problem ends up using -- has to accept
    /// both the library file and the directory holding it.
    #[test]
    fn a_search_location_may_be_the_file_or_its_directory() {
        let dir = std::env::temp_dir().join(format!("evo-pdfium-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        assert_eq!(library_at(&dir), None, "an empty directory holds nothing");

        let file = dir.join(library_file_name());
        std::fs::write(&file, b"not really a library").expect("write");
        assert_eq!(library_at(&dir), Some(file.clone()));
        assert_eq!(library_at(&file), Some(file));
        assert_eq!(library_at(&dir.join("nowhere")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
