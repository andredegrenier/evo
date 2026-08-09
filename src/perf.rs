//! The performance harness: a thousand-page document, built here rather than
//! committed, and the handful of numbers that decide whether evo feels quick.
//!
//! Every test in this module is `#[ignore]`d and named `perf_*`, so an ordinary
//! `cargo test` never runs one. They are meant to be run deliberately, in
//! release mode, on a machine nobody is otherwise using:
//!
//! ```text
//! cargo run -p xtask -- perf
//! # which is
//! cargo test --release -- --ignored --nocapture --test-threads=1 perf_
//! ```
//!
//! The document is generated with lopdf into memory and never written to the
//! repository or to disk: a 6 MB fixture that can be rebuilt in ten
//! milliseconds is a liability, not an asset. Its pages carry real text and
//! real vector art -- forty lines of prose, rules, a filled panel, a polyline
//! and a bezier -- for a related reason. Blank pages would flatter every number
//! here.
//!
//! The assertions are the plan's targets times [`SLACK`]. They are loose on
//! purpose: this measures a whole machine, and a test that fails because
//! something else was compiling teaches nothing.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use lopdf::{Dictionary, Document as LoDocument, Object, Stream, dictionary};

use crate::render::engine::{self, EnginePref};

/// How much slower than the target a run may be before the test fails.
const SLACK: f64 = 3.0;

/// The size of the synthetic document. A thousand pages is past the point
/// where anything O(pages) at open time stops being invisible.
const SYNTHETIC_PAGES: usize = 1000;

// ---------------------------------------------------------------------------
// The synthetic document
// ---------------------------------------------------------------------------

/// One page's content stream: a heading, a rule, forty lines of body text, a
/// filled panel, a polyline and a filled bezier.
///
/// Every page differs -- the page number is in the heading and threaded
/// through the body -- so no engine can answer a second page by recognising
/// the first.
fn page_content(page: usize) -> Vec<u8> {
    let mut ops = String::with_capacity(4096);
    ops.push_str("q 0.15 0.22 0.55 RG 1.5 w 54 726 m 558 726 l S Q\n");
    ops.push_str(&format!(
        "BT /F1 17 Tf 54 738 Td (Structural inspection report -- sheet {}) Tj ET\n",
        page + 1
    ));
    ops.push_str("BT /F1 9.5 Tf 54 706 Td 13 TL\n");
    for line in 0..40 {
        ops.push_str(&format!(
            "(Grid line {}.{line:02} bears on the north elevation; deflection measured at \
             {:.2} mm against an allowance of 14.00 mm, within tolerance.) Tj T*\n",
            page + 1,
            (page * 7 + line * 3) as f32 % 1300.0 / 100.0
        ));
    }
    ops.push_str("ET\n");
    ops.push_str("q 0.92 0.93 0.96 rg 54 96 216 96 re f Q\n");
    ops.push_str("q 0.15 0.22 0.55 RG 0.7 w 54 96 216 96 re S Q\n");
    ops.push_str(&format!(
        "BT /F1 8 Tf 62 172 Td (Revision {} -- checked) Tj ET\n",
        page % 9
    ));
    ops.push_str(
        "q 0.75 0.15 0.10 RG 1.2 w 300 104 m 348 176 l 402 120 l 456 184 l 540 128 l S Q\n",
    );
    ops.push_str(
        "q 0.10 0.45 0.25 rg 300 210 m 360 268 440 268 500 210 c 500 196 l 300 196 l f Q\n",
    );
    ops.into_bytes()
}

/// A PDF of `pages` pages, in memory.
///
/// Helvetica rather than an embedded font: it is what the base-14 machinery in
/// both engines actually exercises, and it keeps the document small enough that
/// generating it is never the thing being measured.
fn synthetic_pdf(pages: usize) -> Vec<u8> {
    let mut lo = LoDocument::with_version("1.7");
    let pages_id = lo.new_object_id();
    let font_id = lo.add_object(crate::export::pdf::helvetica_font_dict());
    let resources_id = lo.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let kids: Vec<Object> = (0..pages)
        .map(|page| {
            let content_id = lo.add_object(Stream::new(Dictionary::new(), page_content(page)));
            lo.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => resources_id,
                "Contents" => content_id,
            })
            .into()
        })
        .collect();

    lo.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => kids.len() as i64,
            "Kids" => kids,
        }),
    );
    let catalog_id = lo.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    lo.trailer.set("Root", catalog_id);

    let mut out = Vec::new();
    lo.save_to(&mut out).expect("the synthetic PDF saves");
    out
}

/// The same pages with a real embedded TrueType font instead of Helvetica.
///
/// Base-14 fonts are the easy case: both engines carry their own copies and
/// nothing has to be parsed out of the file. A document whose glyphs live in a
/// 400 KB `/FontFile2` is where a per-document font cache would earn its keep,
/// so it is what the cache is judged on.
///
/// The widths are uniform rather than measured -- reading them properly means
/// parsing the `hmtx` table, and the text lands slightly unevenly without it.
/// That changes where glyphs are drawn and not how much work drawing them is,
/// which is all this document is for.
fn embedded_font_pdf(pages: usize) -> Vec<u8> {
    let ttf = include_bytes!("../assets/fonts/LiberationSans-Regular.ttf").to_vec();
    let mut lo = LoDocument::with_version("1.7");
    let pages_id = lo.new_object_id();

    let file_id = lo.add_object(Stream::new(
        dictionary! { "Length1" => ttf.len() as i64 },
        ttf,
    ));
    let descriptor_id = lo.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => "LiberationSans",
        "Flags" => 32i64,
        "FontBBox" => vec![(-543).into(), (-303).into(), 1300.into(), 980.into()],
        "ItalicAngle" => 0i64,
        "Ascent" => 905i64,
        "Descent" => (-212i64),
        "CapHeight" => 716i64,
        "StemV" => 80i64,
        "FontFile2" => file_id,
    });
    let font_id = lo.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "TrueType",
        "BaseFont" => "LiberationSans",
        "FirstChar" => 32i64,
        "LastChar" => 126i64,
        "Widths" => vec![Object::Integer(556); 95],
        "Encoding" => "WinAnsiEncoding",
        "FontDescriptor" => descriptor_id,
    });
    let resources_id = lo.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let kids: Vec<Object> = (0..pages)
        .map(|page| {
            let content_id = lo.add_object(Stream::new(Dictionary::new(), page_content(page)));
            lo.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => resources_id,
                "Contents" => content_id,
            })
            .into()
        })
        .collect();

    lo.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => kids.len() as i64,
            "Kids" => kids,
        }),
    );
    let catalog_id = lo.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    lo.trailer.set("Root", catalog_id);

    let mut out = Vec::new();
    lo.save_to(&mut out).expect("the embedded-font PDF saves");
    out
}

/// The thousand-page document, built once per test process.
fn synthetic() -> Arc<Vec<u8>> {
    static PDF: OnceLock<Arc<Vec<u8>>> = OnceLock::new();
    PDF.get_or_init(|| {
        let started = Instant::now();
        let bytes = synthetic_pdf(SYNTHETIC_PAGES);
        println!(
            "  (built {SYNTHETIC_PAGES}-page synthetic document, {} KB, in {})",
            bytes.len() / 1024,
            ms(started.elapsed())
        );
        Arc::new(bytes)
    })
    .clone()
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn ms(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 10.0 {
        format!("{ms:.2} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

/// Print a measurement and fail if it is more than [`SLACK`] times its target.
fn report(what: &str, took: Duration, target: Duration) {
    let ratio = took.as_secs_f64() / target.as_secs_f64();
    println!(
        "PERF  {what:<52} {:>10}   (target {}, {ratio:.2}x)",
        ms(took),
        ms(target)
    );
    assert!(
        took.as_secs_f64() <= target.as_secs_f64() * SLACK,
        "{what}: {} is more than {SLACK}x the {} target",
        ms(took),
        ms(target)
    );
}

/// Print a measurement that has no target -- the ones the plan asks to record
/// rather than to enforce.
fn note(what: &str, value: impl std::fmt::Display) {
    println!("PERF  {what:<52} {value:>10}   (recorded)");
}

/// hayro always; PDFium as well when its library is on this machine. A run
/// without the dylib still measures something useful, and says so.
fn engines() -> Vec<EnginePref> {
    let mut prefs = vec![EnginePref::Hayro];
    if engine::pdfium_available() {
        prefs.push(EnginePref::Pdfium);
    } else {
        println!("  (no PDFium library found; set EVO_PDFIUM_PATH to measure it too)");
    }
    prefs
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

/// Target: a thousand pages open in under two seconds.
///
/// Also splits the time in half -- hayro's parse against evo's own eager
/// [`crate::doc::PageInfo`] walk -- because the two have different owners, and
/// a number that blamed the wrong one would send the next person to the wrong
/// code.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_open_a_thousand_pages() {
    let bytes = synthetic();

    let started = Instant::now();
    let parsed = hayro::hayro_syntax::Pdf::new(bytes.clone()).expect("the synthetic document");
    let parse = started.elapsed();

    let started = Instant::now();
    let count = parsed
        .pages()
        .iter()
        .map(|page| {
            let (w, h) = page.render_dimensions();
            let crop = page.intersected_crop_box();
            (w + h + crop.x0 as f32 + crop.y0 as f32) as usize
        })
        .sum::<usize>();
    let page_info = started.elapsed();
    assert!(count > 0);

    let started = Instant::now();
    let doc = crate::doc::Document::load_bytes(bytes.as_ref().clone(), None).expect("it opens");
    let load = started.elapsed();
    assert_eq!(doc.pages.len(), SYNTHETIC_PAGES);

    note("open: hayro parse only", ms(parse));
    note("open: eager PageInfo walk only", ms(page_info));
    report(
        "Document::load_bytes, 1000 pages",
        load,
        Duration::from_secs(2),
    );
}

// ---------------------------------------------------------------------------
// The render worker
// ---------------------------------------------------------------------------

/// Wait for the worker's answer to `page`, discarding answers to anything
/// else. Returns how long it took from `since`.
fn wait_for_page(worker: &crate::render::RenderWorker, page: usize, since: Instant) -> Duration {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match worker.try_recv() {
            Some(res) if res.page == page && res.image.is_some() => return since.elapsed(),
            Some(_) => continue,
            None => {
                assert!(
                    Instant::now() < deadline,
                    "the worker never drew page {page}"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// Target: half a second from asking for a page to holding its pixels, through
/// the real worker, on a document whose thousand pages are already open.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_first_page_after_a_jump() {
    let bytes = synthetic();
    for pref in engines() {
        let ctx = eframe::egui::Context::default();
        let worker = crate::render::RenderWorker::spawn(bytes.clone(), ctx, pref, None);

        // Warm up: the first request also pays for opening the document, which
        // is not what a jump costs.
        let started = Instant::now();
        worker.request(crate::render::RenderRequest {
            page: 0,
            scale: 1.0,
        });
        let cold = wait_for_page(&worker, 0, started);

        let mut worst = Duration::ZERO;
        for page in [500usize, 501, 750, 999] {
            let started = Instant::now();
            worker.request(crate::render::RenderRequest { page, scale: 1.0 });
            worst = worst.max(wait_for_page(&worker, page, started));
        }

        note(
            &format!("worker: open + first page ({})", pref.label()),
            ms(cold),
        );
        report(
            &format!("worker: page after a jump, worst of 4 ({})", pref.label()),
            worst,
            Duration::from_millis(500),
        );
    }
}

/// What the newest-first drain order is for.
///
/// A fast scroll leaves a batch of requests behind it; the last one is the page
/// under the cursor and the rest have already gone past. This measures how long
/// the page somebody is actually looking at waits.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_newest_request_in_a_burst() {
    const BURST: usize = 24;
    let bytes = synthetic();
    for pref in engines() {
        let ctx = eframe::egui::Context::default();
        let worker = crate::render::RenderWorker::spawn(bytes.clone(), ctx, pref, None);
        worker.request(crate::render::RenderRequest {
            page: 0,
            scale: 1.0,
        });
        wait_for_page(&worker, 0, Instant::now());

        let started = Instant::now();
        for page in 100..100 + BURST {
            worker.request(crate::render::RenderRequest { page, scale: 1.0 });
        }
        let newest = wait_for_page(&worker, 100 + BURST - 1, started);

        report(
            &format!("worker: newest of a {BURST}-page burst ({})", pref.label()),
            newest,
            Duration::from_millis(500),
        );
    }
}

/// A thousand-page scroll through the real worker and the real texture cache.
///
/// The budget is the thing under test: 384 MB of canvas textures is 198 US
/// Letter pages at 1x, so a thousand pages cannot fit and the cache has to
/// throw away the ones nobody is looking at. Growing instead would be a
/// gigabyte of GPU memory by the end of one document.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_texture_budget_holds_over_a_thousand_page_scroll() {
    let bytes = synthetic();
    let ctx = eframe::egui::Context::default();
    // Whichever engine is quickest here: the cache holds the same bytes and
    // evicts by the same rule whoever drew them, so running this twice would
    // only measure the rasterizers again.
    let pref = *engines().last().expect("at least hayro");
    let worker = crate::render::RenderWorker::spawn(bytes, ctx.clone(), pref, None);
    let mut cache = crate::render::cache::TextureCache::default();

    let started = Instant::now();
    let mut peak = 0usize;
    for page in 0..SYNTHETIC_PAGES {
        cache.begin_frame();
        worker.request(crate::render::RenderRequest { page, scale: 1.0 });
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match worker.try_recv() {
                Some(res) => {
                    if let Some(image) = res.image {
                        cache.insert(&ctx, res.page, res.scale, image);
                    }
                    if res.page == page {
                        break;
                    }
                }
                None => {
                    assert!(
                        Instant::now() < deadline,
                        "the worker stopped at page {page}"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        peak = peak.max(cache.bytes());
        assert!(
            cache.bytes() <= crate::render::cache::CANVAS_BUDGET,
            "page {page}: the canvas cache is {} bytes, over its {} budget",
            cache.bytes(),
            crate::render::cache::CANVAS_BUDGET
        );
    }
    let took = started.elapsed();

    // Eviction has to have actually happened, or the assertion above proved
    // nothing at all.
    assert!(
        cache.texture_count() < SYNTHETIC_PAGES,
        "nothing was ever evicted"
    );
    note(
        &format!("scroll: 1000 pages rendered ({})", pref.label()),
        ms(took),
    );
    note("scroll: peak canvas cache", format!("{} MB", peak >> 20));
    note(
        "scroll: textures held at the end",
        cache.texture_count().to_string(),
    );
}

// ---------------------------------------------------------------------------
// Find
// ---------------------------------------------------------------------------

/// Target: a second from pressing ⌘F to the first page's matches, on a
/// document that is already open.
///
/// This is also the evidence that the text worker is lazy. It walks pages in
/// order and sends each one as it finishes, so the first result cannot be
/// waiting on the thousandth page -- if it were, this number would be the cost
/// of extracting the whole document rather than one page of it, and the
/// `all pages` note below would be roughly equal to it instead of hundreds of
/// times larger.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_find_first_page_results() {
    let bytes = synthetic();
    let doc = crate::doc::Document::load_bytes(bytes.as_ref().clone(), None).expect("it opens");

    let started = Instant::now();
    let worker = crate::library::textjob::TextWorker::spawn(
        doc.source.clone(),
        None,
        None,
        eframe::egui::Context::default(),
        EnginePref::Hayro,
    );

    let mut first = None;
    let mut pages = 0usize;
    let deadline = Instant::now() + Duration::from_secs(300);
    while pages < SYNTHETIC_PAGES {
        match worker.try_recv() {
            Some((page, layout)) => {
                if page == 0 {
                    let text = crate::library::extract::join_lines(&layout.lines);
                    assert!(text.contains("deflection"), "page one has no text");
                    first = Some(started.elapsed());
                }
                pages += 1;
            }
            None => {
                assert!(Instant::now() < deadline, "the text worker stalled");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    let all = started.elapsed();

    report(
        "find: first page of matches",
        first.expect("page one arrived"),
        Duration::from_secs(1),
    );
    note("find: all 1000 pages extracted", ms(all));
}

// ---------------------------------------------------------------------------
// evo serve
// ---------------------------------------------------------------------------

/// Target: a page and a half of a second for a `render_png` deep into a
/// thousand-page document -- parse, render and PNG encode, which is what the
/// phone waits for the first time it asks for a page.
///
/// Measured twice, because the two calls can mean different things. The first
/// render an engine does in a process pays whatever it sets up once -- PDFium
/// builds its font mapper, hayro warms its interpreter -- and one unlucky
/// reader really does pay that: run on its own, this test measures PDFium's
/// first call at 115 ms against 4.6 ms for the second. Run after the other
/// perf tests it is 5 ms both times, because they have already paid it. Both
/// are held to the same target, and the gap between them is the number worth
/// watching: if it grew, the set-up would be worth doing at server start-up
/// rather than in front of a reader.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_serve_render_png_page_500() {
    // Resolving the preference loads the PDFium library, so that is done here
    // rather than inside a measurement.
    let prefs = engines();
    let bytes = synthetic();
    for pref in prefs {
        for (nth, page) in [("1st call", 499usize), ("2nd call", 498)] {
            let started = Instant::now();
            let png = crate::serve::pages::render_png(
                bytes.clone(),
                page,
                crate::serve::pages::Zoom::Factor(1.0),
                pref,
            )
            .expect("the page draws");
            let took = started.elapsed();
            assert!(png.len() > 1024, "that PNG is suspiciously small");
            report(
                &format!(
                    "serve: render_png page {}, {nth} ({})",
                    page + 1,
                    pref.label()
                ),
                took,
                Duration::from_millis(1500),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// hayro's render cache
// ---------------------------------------------------------------------------

/// Does hayro's per-document [`hayro::RenderCache`] earn the `unsafe` it takes
/// to keep one alive between renders?
///
/// `RenderCache<'a>` borrows from the `Pdf` it was built from, so a struct
/// holding both has to launder the lifetime through a `mem::transmute`.
/// [`crate::render::engine::HayroEngineDoc`] did exactly that until M37, and it
/// was the only `unsafe` block in evo. This is the measurement that removed it,
/// kept as a standing one: if a later hayro makes its cache pay for itself, the
/// numbers here will say so and the question can be reopened on evidence rather
/// than on the fact that caching sounds like it ought to help.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_hayro_render_cache_is_worth_its_unsafe() {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::AlphaColor;
    use hayro::{RenderCache, RenderSettings};

    let settings = InterpreterSettings::default();
    let render_settings = RenderSettings {
        x_scale: 1.0,
        y_scale: 1.0,
        width: None,
        height: None,
        bg_color: AlphaColor::WHITE,
    };

    // Both halves call hayro directly and differ in one line -- where
    // `RenderCache::new()` sits. Comparing against `HayroEngineDoc::render`
    // instead would have measured its RGBA conversion too, which is the same
    // work either way and enough of it to swamp the answer.
    //
    // Nothing here needs the `unsafe`: a cache and the `Pdf` it borrows from
    // can share a stack frame quite happily. It is only holding the two in one
    // struct that the borrow checker cannot express.
    let run = |bytes: &Arc<Vec<u8>>, pages: &[usize], keep: bool| {
        let pdf = Pdf::new(bytes.clone()).expect("it parses");
        let all = pdf.pages();
        let page_at = |n: usize| all.get(n).expect("that page");
        let kept = RenderCache::new();
        // Warm the interpreter before the clock starts.
        let _ = hayro::render(page_at(0), &RenderCache::new(), &settings, &render_settings);
        let started = Instant::now();
        for &page in pages {
            let per_render = RenderCache::new();
            let cache = if keep { &kept } else { &per_render };
            let pixmap = hayro::render(page_at(page), cache, &settings, &render_settings);
            std::hint::black_box(pixmap.width());
        }
        started.elapsed()
    };

    let base14 = synthetic();
    let embedded = Arc::new(embedded_font_pdf(50));
    for (doc, name) in [(&base14, "base-14"), (&embedded, "embedded font")] {
        for (what, pages) in [
            ("one page 20x", vec![7usize; 20]),
            ("50 pages", (0..50).collect::<Vec<_>>()),
        ] {
            // Alternate and take the best of three each way: this is a five
            // percent question on a machine with other things on it.
            let mut with = Duration::MAX;
            let mut without = Duration::MAX;
            for _ in 0..3 {
                with = with.min(run(doc, &pages, true));
                without = without.min(run(doc, &pages, false));
            }
            let saved = 100.0 * (1.0 - with.as_secs_f64() / without.as_secs_f64());
            note(&format!("hayro cache kept, {name}, {what}"), ms(with));
            note(&format!("hayro cache fresh, {name}, {what}"), ms(without));
            note(
                &format!("hayro cache saves, {name}, {what}"),
                format!("{saved:.1}%"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The library at scale
// ---------------------------------------------------------------------------

/// Ten thousand documents in one library: how long listing and searching take.
///
/// Recorded, not enforced. The point is to know whether either is pathological
/// -- linear in documents is expected, quadratic would not be -- and both are
/// answered by one run. Measured: 11 ms to list ten thousand and 2.7 ms to
/// search them.
///
/// Getting there takes about a quarter of an hour, almost all of it building
/// the index: [`crate::library::search::SearchIndex::index_document`] commits
/// once per document, which is right for the real indexer (a document is
/// searchable the moment it has been read, and nothing is lost if evo is
/// closed) and about 86 ms of fsync each here. That is a bulk-import cost, not
/// a cost anybody waits on, so it is a note rather than a target.
///
/// `EVO_PERF_LIBRARY_DOCS` shrinks it for a quicker pass.
#[test]
#[ignore = "timing; run with --release --ignored perf_"]
fn perf_library_at_ten_thousand_documents() {
    let count: usize = std::env::var("EVO_PERF_LIBRARY_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let root = std::env::temp_dir().join(format!("evo-perf-library-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let library = crate::library::Library::open_at(root.clone()).expect("a temporary library");
    let index = crate::library::search::SearchIndex::open_or_create(&root.join("index"))
        .expect("a search index");

    // One-page documents, each with its own words, so ids differ and the index
    // has something to tell them apart by.
    let started = Instant::now();
    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        let mut lo = LoDocument::with_version("1.7");
        let pages_id = lo.new_object_id();
        let font_id = lo.add_object(crate::export::pdf::helvetica_font_dict());
        let content = format!(
            "BT /F1 11 Tf 54 700 Td (Report {n} -- quarterly reinforcement schedule, \
             lot {}, revision {}.) Tj ET\n",
            n % 97,
            n % 13
        );
        let content_id = lo.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
        let page_id = lo.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            "Contents" => content_id,
        });
        lo.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Count" => 1i64,
                "Kids" => vec![Object::from(page_id)],
            }),
        );
        let catalog_id = lo.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        lo.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        lo.save_to(&mut bytes).expect("it saves");

        let meta = library
            .import_bytes(bytes, &format!("Report {n}"), &format!("report-{n}.pdf"))
            .expect("it imports");
        ids.push(meta.id);
    }
    let import = started.elapsed();

    let started = Instant::now();
    let mut writer = index.writer().expect("a writer");
    for (n, id) in ids.iter().enumerate() {
        index
            .index_document(
                &mut writer,
                id,
                &format!("Report {n}"),
                &[format!(
                    "Report {n} -- quarterly reinforcement schedule, lot {}, revision {}.",
                    n % 97,
                    n % 13
                )],
                None,
            )
            .expect("it indexes");
    }
    let indexing = started.elapsed();

    let started = Instant::now();
    let listed = library.list().expect("it lists");
    let list = started.elapsed();
    assert_eq!(listed.len(), count);

    let started = Instant::now();
    let hits = library.search("reinforcement").expect("it searches");
    let search = started.elapsed();
    assert!(!hits.is_empty());

    note(&format!("library: import {count} documents"), ms(import));
    note(&format!("library: index {count} documents"), ms(indexing));
    note(&format!("library: list {count} documents"), ms(list));
    note(&format!("library: search {count} documents"), ms(search));

    drop(library);
    let _ = std::fs::remove_dir_all(&root);
}

/// The generator itself has to produce something the engines can read, and it
/// is cheap enough to check on every ordinary test run -- if it broke, every
/// number above would be measuring the wrong thing.
#[test]
fn the_synthetic_document_is_a_readable_pdf() {
    use crate::render::engine::EngineDoc;

    let bytes = synthetic_pdf(3);
    let doc = crate::doc::Document::load_bytes(bytes.clone(), None).expect("it opens");
    assert_eq!(doc.pages.len(), 3);
    assert_eq!((doc.pages[0].width, doc.pages[0].height), (612.0, 792.0));

    // Real text and real ink: a page that drew blank would make every timing
    // here meaningless.
    let mut engine =
        crate::render::engine::HayroEngineDoc::open(Arc::new(bytes), None).expect("it parses");
    let page = engine.render(1, 1.0).expect("it draws");
    let inked = page.rgba.chunks(4).filter(|p| p[0] < 200).count();
    assert!(inked > 5_000, "only {inked} inked pixels");
}
