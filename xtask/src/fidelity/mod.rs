//! The fidelity harness: `cargo run -p xtask -- fidelity`.
//!
//! evo bets its rendering on hayro -- a young, pure-Rust rasterizer -- and
//! since M32 it can also draw with PDFium, the engine in Chrome. This harness
//! is the evidence base for that bet. It renders a corpus of PDFs with both
//! and records two different things:
//!
//! 1. **A SHA-256 of every hayro page.** A tripwire, nothing more: if a hash
//!    moves, hayro's output moved, and somebody should be able to say why.
//! 2. **How far hayro's pixels are from PDFium's.** Not a pass/fail judgement
//!    on any one page -- the two engines legitimately differ on antialiasing,
//!    on font hinting, on how they resolve an unusual colour space -- but a
//!    number that can be watched over time and a picture when it moves.
//!
//! Both live in `xtask/fidelity-baseline.json`, keyed by platform, and a run
//! fails only when something got *worse* than what is committed there. Nothing
//! about a first run on a new machine is a failure; `--bless` writes what it
//! found, and the diff in that file is the review.
//!
//! ```text
//! cargo run -p xtask -- fidelity                    # check against baseline
//! cargo run -p xtask -- fidelity --corpus fixtures  # committed PDFs, no network
//! cargo run -p xtask -- fidelity --bless            # rewrite baseline + report
//! ```
//!
//! PDFium comes from wherever `EVO_PDFIUM_PATH` points, or from `target/debug`
//! after `cargo run -- fetch-pdfium --into target/debug`. Without it the run
//! still checks hayro's hashes and simply has nothing to compare them to.

mod compare;
mod corpus;
mod render;
mod report;

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use compare::Divergence;
use render::{HayroDoc, PdfiumEngine, Rendered};

pub const USAGE: &str = "\
usage: cargo run -p xtask -- fidelity [options]

    --corpus <name>   fixtures | verapdf | all   (default: all)
    --max-pages <n>   pages per document        (default: 5)
    --bless           rewrite the baseline and the committed report
    --help";

/// Pixels per point. 1.5 is a real viewing zoom: high enough that hinting and
/// antialiasing decisions show up, low enough that a few hundred pages render
/// in a couple of minutes.
const SCALE: f32 = 1.5;

const DEFAULT_MAX_PAGES: usize = 5;

/// How much worse than the baseline a page may get before the run fails.
/// Small enough to catch a shading that stopped being drawn, large enough that
/// a PDFium point release moving a glyph edge does not cry wolf.
const MEAN_ABS_EPSILON: f64 = 0.5;
const FRAC_OFF_EPSILON: f64 = 0.005;

/// Above this a page goes in the report's worst-pages table and gets a diff
/// image. Not a failure -- a page hayro and PDFium have always disagreed about
/// is a known disagreement, not a regression.
const NOTABLE_MEAN_ABS: f64 = 2.0;
const NOTABLE_FRAC_OFF: f64 = 0.02;

/// At most this many diff images per run: enough to look at, few enough to
/// upload as a CI artifact.
const MAX_DIFF_IMAGES: usize = 25;

// ---------------------------------------------------------------------------
// The baseline
// ---------------------------------------------------------------------------

/// What is committed at `xtask/fidelity-baseline.json`.
///
/// Keyed by platform because the numbers are: hayro's rasterizer takes
/// different SIMD paths on aarch64 and x86-64, and PDFium is a different build
/// on every operating system. One file with a section per platform beats
/// either a baseline that only one machine can check or three files that drift
/// apart. A platform with no section is not a failure -- it is a `--bless`
/// waiting to happen.
#[derive(Serialize, Deserialize, Clone)]
pub struct Baseline {
    /// The settings the numbers depend on. Change one and the whole file has
    /// to be re-blessed, which is why they are written down next to it.
    pub scale: f32,
    pub max_pages: usize,
    pub platforms: BTreeMap<String, Vec<Doc>>,
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            scale: SCALE,
            max_pages: DEFAULT_MAX_PAGES,
            platforms: BTreeMap::new(),
        }
    }
}

/// One document's result. Sorted by `(corpus, path)` in the file so that two
/// runs of the same corpus produce byte-identical JSON.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Doc {
    pub corpus: String,
    pub path: String,
    pub status: Status,
    /// hayro drew something it was not sure about. Evidence, not a failure:
    /// this is the same warning sink that lights the badge in the app.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hayro_warnings: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<Page>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ok,
    /// hayro would not parse the file. Recorded rather than raised: a corpus
    /// of deliberately broken PDFs is *supposed* to contain files no parser
    /// accepts, and which ones they are is the interesting part.
    HayroUnparseable,
    /// hayro panicked. Always worth a bug report upstream.
    HayroPanic,
    /// A document with no pages in it.
    NoPages,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Page {
    pub page: usize,
    pub width: u32,
    pub height: u32,
    /// The tripwire.
    pub hayro_sha256: String,
    /// Absent when PDFium was not available, could not open the document, or
    /// disagreed about the page size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_abs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frac_off: Option<f64>,
    /// Why there is no divergence, when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Divergences are rounded before they are stored. Floating point arithmetic
/// is deterministic for a given binary, but the sixteenth decimal of a mean is
/// noise either way, and a baseline diff should be readable.
fn round(value: f64, places: i32) -> f64 {
    let factor = 10f64.powi(places);
    (value * factor).round() / factor
}

/// `macos-aarch64`, `linux-x86_64`, `windows-x86_64`.
fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn baseline_path(repo: &Path) -> PathBuf {
    repo.join("xtask/fidelity-baseline.json")
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Everything a run learned that is not in the baseline: the numbers the
/// report tabulates and the pictures it points at.
#[derive(Default)]
pub struct Run {
    pub docs: Vec<Doc>,
    /// `(corpus, path, page, divergence)` for every page both engines drew.
    pub measured: Vec<(String, String, usize, Divergence)>,
    pub diffs: Vec<PathBuf>,
    pub pdfium: Option<PathBuf>,
    pub pdfium_available: bool,
    pub corpora: Vec<CorpusInfo>,
    pub seconds: f64,
}

pub struct CorpusInfo {
    pub name: String,
    pub title: String,
    pub license: String,
    pub source: String,
    pub files: usize,
}

pub fn main(args: &[String]) {
    match run(args) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(problem) => {
            eprintln!("fidelity: {problem}");
            std::process::exit(2);
        }
    }
}

/// `Ok(false)` means the run found a regression; `Err` means it could not run.
/// The distinction matters to CI, which should be red for the first and
/// shouting for the second.
fn run(args: &[String]) -> Result<bool, String> {
    let mut corpora: Vec<String> = Vec::new();
    let mut bless = false;
    let mut max_pages = DEFAULT_MAX_PAGES;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(true);
            }
            "--bless" => bless = true,
            "--corpus" => {
                let name = rest.next().ok_or("--corpus needs a name")?;
                if name == "all" {
                    corpora = corpus::names().iter().map(|n| (*n).to_owned()).collect();
                } else {
                    corpora.push(name.clone());
                }
            }
            "--max-pages" => {
                max_pages = rest
                    .next()
                    .ok_or("--max-pages needs a number")?
                    .parse()
                    .map_err(|e| format!("--max-pages: {e}"))?;
            }
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    if corpora.is_empty() {
        corpora = corpus::names().iter().map(|n| (*n).to_owned()).collect();
    }

    let repo = crate::repo_root();
    let mut baseline = read_baseline(&repo)?;
    if bless {
        baseline.scale = SCALE;
        baseline.max_pages = max_pages;
    } else if baseline.max_pages != max_pages {
        eprintln!(
            "note: the baseline was blessed at --max-pages {}, this run is {max_pages}",
            baseline.max_pages
        );
    }

    let run = measure(&repo, &corpora, max_pages)?;
    let findings = check(&baseline, &run, &corpora);

    let out = repo.join("target/fidelity");
    std::fs::create_dir_all(&out)
        .map_err(|e| format!("could not create {}: {e}", out.display()))?;
    let report = report::write(&repo, &run, &findings, &baseline)?;
    println!("report: {}", report.display());

    if bless {
        let mut blessed = baseline.clone();
        merge(&mut blessed, &run, &corpora);
        write_baseline(&repo, &blessed)?;
        let published = report::publish(&repo, &report)?;
        println!("blessed: {}", baseline_path(&repo).display());
        println!("published: {}", published.display());
        return Ok(true);
    }

    let failures = findings.iter().filter(|f| f.fails()).count();
    if failures > 0 {
        eprintln!("\n{failures} regression(s) against the baseline:");
        for finding in findings.iter().filter(|f| f.fails()).take(20) {
            eprintln!("  {finding}");
        }
        eprintln!("\nSee {}.", report.display());
        return Ok(false);
    }
    if findings.iter().any(|f| f.kind == Kind::NoBaseline) {
        println!(
            "nothing to check: the baseline has no {} section yet. \
             Re-run with --bless to record one.",
            platform()
        );
    } else {
        println!("no regressions against the baseline for {}.", platform());
    }
    Ok(true)
}

fn read_baseline(repo: &Path) -> Result<Baseline, String> {
    let path = baseline_path(repo);
    match std::fs::read_to_string(&path) {
        Ok(json) => {
            serde_json::from_str(&json).map_err(|e| format!("{} is not valid: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Baseline::default()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

fn write_baseline(repo: &Path, baseline: &Baseline) -> Result<(), String> {
    let path = baseline_path(repo);
    let mut json = serde_json::to_string_pretty(baseline)
        .map_err(|e| format!("could not serialize the baseline: {e}"))?;
    json.push('\n');
    std::fs::write(&path, json).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Replace this platform's records for the corpora that ran, and leave every
/// other platform and every corpus that did not run exactly as it was: a
/// `--corpus fixtures --bless` must not quietly delete the veraPDF numbers.
fn merge(baseline: &mut Baseline, run: &Run, corpora: &[String]) {
    let docs = baseline.platforms.entry(platform()).or_default();
    docs.retain(|doc| !corpora.contains(&doc.corpus));
    docs.extend(run.docs.iter().cloned());
    docs.sort_by(|a, b| (&a.corpus, &a.path).cmp(&(&b.corpus, &b.path)));
}

fn measure(repo: &Path, corpora: &[String], max_pages: usize) -> Result<Run, String> {
    let started = Instant::now();
    let mut run = Run::default();

    let pdfium = PdfiumEngine::find(repo);
    run.pdfium_available = pdfium.is_some();
    run.pdfium = pdfium.as_ref().and_then(|engine| engine.path.clone());
    match &run.pdfium {
        Some(path) => println!("PDFium: {}", path.display()),
        None if run.pdfium_available => println!("PDFium: the system library"),
        None => println!(
            "PDFium: not found -- hashing hayro only. \
             `cargo run -- fetch-pdfium --into target/debug` to compare."
        ),
    }

    let diffs_dir = repo.join("target/fidelity/diffs");
    let _ = std::fs::remove_dir_all(&diffs_dir);

    let mut manifests = Vec::new();
    for name in corpora {
        let manifest = corpus::load(name)?;
        println!(
            "\n{} -- {} file(s), {}",
            manifest.title,
            manifest.files.len(),
            manifest.license
        );
        run.corpora.push(CorpusInfo {
            name: manifest.name.clone(),
            title: manifest.title.clone(),
            license: manifest.license.clone(),
            source: manifest.source.clone(),
            files: manifest.files.len(),
        });

        for (index, entry) in manifest.files.iter().enumerate() {
            if index % 25 == 0 {
                println!("  [{index}/{}] {}", manifest.files.len(), entry.path);
            }
            let bytes = Arc::new(manifest.bytes(entry, repo)?);
            let doc = one_document(
                &manifest.name,
                entry,
                bytes,
                max_pages,
                pdfium.as_ref(),
                &mut run,
            );
            run.docs.push(doc);
        }
        manifests.push(manifest);
    }

    run.docs
        .sort_by(|a, b| (&a.corpus, &a.path).cmp(&(&b.corpus, &b.path)));
    write_diffs(repo, &manifests, pdfium.as_ref(), &mut run);
    run.seconds = started.elapsed().as_secs_f64();
    Ok(run)
}

/// Draw the worst pages again, this time to a picture.
///
/// A second pass rather than saving images during the first: keeping every
/// candidate rendering alive to find out afterwards which were the worst would
/// cost a gigabyte, and re-rendering twenty-five pages costs a second.
fn write_diffs(
    repo: &Path,
    manifests: &[corpus::Manifest],
    pdfium: Option<&PdfiumEngine>,
    run: &mut Run,
) {
    let Some(pdfium) = pdfium else { return };
    let mut worst: Vec<_> = run
        .measured
        .iter()
        .filter(|(_, _, _, divergence)| {
            divergence.mean_abs > NOTABLE_MEAN_ABS || divergence.frac_off > NOTABLE_FRAC_OFF
        })
        .cloned()
        .collect();
    worst.sort_by(|a, b| b.3.mean_abs.total_cmp(&a.3.mean_abs));
    worst.truncate(MAX_DIFF_IMAGES);
    if worst.is_empty() {
        return;
    }

    let dir = repo.join("target/fidelity/diffs");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create {}: {e}", dir.display());
        return;
    }
    for (corpus_name, path, page, _) in worst {
        let Some((manifest, entry)) = manifests.iter().find_map(|manifest| {
            (manifest.name == corpus_name)
                .then(|| manifest.files.iter().find(|entry| entry.path == path))
                .flatten()
                .map(|entry| (manifest, entry))
        }) else {
            continue;
        };
        let Ok(bytes) = manifest.bytes(entry, repo) else {
            continue;
        };
        let bytes = Arc::new(bytes);
        let password = entry.password.as_deref();
        let (Some(hayro), Some(reference)) = (
            HayroDoc::open(bytes.clone(), password),
            pdfium.open(&bytes, password),
        ) else {
            continue;
        };
        let (Some(drawn), Some(same)) = (
            hayro.render(page, SCALE),
            render::pdfium_render(&reference, page, SCALE),
        ) else {
            continue;
        };
        let Some(image) = compare::diff_image(&drawn, &same) else {
            continue;
        };
        let file = dir.join(format!("{corpus_name}-{}-p{}.png", slug(&path), page + 1));
        if image.save(&file).is_ok() {
            run.diffs.push(file);
        }
    }
}

/// Render one document with both engines.
///
/// Every call into hayro is wrapped: a corpus exists to contain files that
/// break parsers, and one file that panics must not take the other three
/// hundred with it.
fn one_document(
    corpus_name: &str,
    entry: &corpus::Entry,
    bytes: Arc<Vec<u8>>,
    max_pages: usize,
    pdfium: Option<&PdfiumEngine>,
    run: &mut Run,
) -> Doc {
    let password = entry.password.as_deref();
    let mut doc = Doc {
        corpus: corpus_name.to_owned(),
        path: entry.path.clone(),
        status: Status::Ok,
        hayro_warnings: false,
        pages: Vec::new(),
    };

    let opened = catch_unwind(AssertUnwindSafe(|| HayroDoc::open(bytes.clone(), password)));
    let hayro = match opened {
        Err(_) => {
            doc.status = Status::HayroPanic;
            return doc;
        }
        Ok(None) => {
            doc.status = Status::HayroUnparseable;
            return doc;
        }
        Ok(Some(hayro)) => hayro,
    };
    let pages = hayro.page_count().min(max_pages);
    if hayro.page_count() == 0 {
        doc.status = Status::NoPages;
        return doc;
    }

    let reference = pdfium.and_then(|engine| engine.open(&bytes, password));

    for page in 0..pages {
        let drawn = match catch_unwind(AssertUnwindSafe(|| hayro.render(page, SCALE))) {
            Ok(Some(drawn)) => drawn,
            // The page index came from hayro's own count, so `None` here means
            // the page tree disagrees with itself: not a panic, not a picture.
            Ok(None) => continue,
            Err(_) => {
                doc.status = Status::HayroPanic;
                return doc;
            }
        };
        let mut record = Page {
            page,
            width: drawn.width,
            height: drawn.height,
            hayro_sha256: corpus::hex(&drawn.rgba),
            mean_abs: None,
            frac_off: None,
            note: None,
        };

        match &reference {
            None if pdfium.is_none() => record.note = Some("no PDFium on this machine".into()),
            None => record.note = Some("PDFium would not open the document".into()),
            Some(reference) => {
                let same = catch_unwind(AssertUnwindSafe(|| {
                    render::pdfium_render(reference, page, SCALE)
                }));
                match same {
                    Ok(Some(same)) => measure_page(&drawn, &same, &mut record, run, &doc),
                    Ok(None) => record.note = Some("PDFium would not draw the page".into()),
                    Err(_) => record.note = Some("PDFium panicked drawing the page".into()),
                }
            }
        }
        doc.pages.push(record);
    }

    doc.hayro_warnings = hayro.had_warnings();
    doc
}

/// Fill in one page's divergence.
fn measure_page(hayro: &Rendered, pdfium: &Rendered, record: &mut Page, run: &mut Run, doc: &Doc) {
    let Some(divergence) = compare::compare(hayro, pdfium) else {
        record.note = Some(format!(
            "size mismatch: hayro {}x{}, PDFium {}x{}",
            hayro.width, hayro.height, pdfium.width, pdfium.height
        ));
        return;
    };
    record.mean_abs = Some(round(divergence.mean_abs, 4));
    record.frac_off = Some(round(divergence.frac_off, 6));
    run.measured.push((
        doc.corpus.clone(),
        doc.path.clone(),
        record.page,
        divergence,
    ));
}

/// A corpus path as a file name: no separators, no spaces, still readable.
fn slug(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

// ---------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------

pub struct Finding {
    pub corpus: String,
    pub path: String,
    pub page: Option<usize>,
    pub kind: Kind,
    pub detail: String,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    /// hayro drew this page differently than it used to. The tripwire.
    HashChanged,
    /// A document that used to parse does not, or the other way round.
    StatusChanged,
    /// hayro moved further away from PDFium than the baseline allows.
    Diverged,
    /// Not in the baseline yet. Bless to adopt.
    New,
    /// In the baseline, not in this run.
    Missing,
    /// Nothing has ever been blessed on this platform, so nothing was
    /// checked. Loud, but not a failure: it is the state every new machine
    /// and every new CI runner starts in.
    NoBaseline,
}

impl Finding {
    pub fn fails(&self) -> bool {
        matches!(
            self.kind,
            Kind::HashChanged | Kind::StatusChanged | Kind::Diverged
        )
    }

    pub fn label(&self) -> &'static str {
        match self.kind {
            Kind::HashChanged => "hayro output changed",
            Kind::StatusChanged => "status changed",
            Kind::Diverged => "diverged further from PDFium",
            Kind::New => "new",
            Kind::Missing => "missing from this run",
            Kind::NoBaseline => "nothing to check against",
        }
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.label(), self.path)?;
        if let Some(page) = self.page {
            write!(f, " page {}", page + 1)?;
        }
        write!(f, " -- {}", self.detail)
    }
}

fn check(baseline: &Baseline, run: &Run, corpora: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some(known) = baseline.platforms.get(&platform()) else {
        findings.push(Finding {
            corpus: String::new(),
            path: platform(),
            page: None,
            kind: Kind::NoBaseline,
            detail: format!(
                "the baseline has no section for {}; run again with --bless to record one",
                platform()
            ),
        });
        return findings;
    };

    let by_path: BTreeMap<(&str, &str), &Doc> = known
        .iter()
        .map(|doc| ((doc.corpus.as_str(), doc.path.as_str()), doc))
        .collect();

    for doc in &run.docs {
        let Some(was) = by_path.get(&(doc.corpus.as_str(), doc.path.as_str())) else {
            findings.push(Finding {
                corpus: doc.corpus.clone(),
                path: doc.path.clone(),
                page: None,
                kind: Kind::New,
                detail: "not in the baseline".into(),
            });
            continue;
        };
        if was.status != doc.status {
            findings.push(Finding {
                corpus: doc.corpus.clone(),
                path: doc.path.clone(),
                page: None,
                kind: Kind::StatusChanged,
                detail: format!("was {:?}, now {:?}", was.status, doc.status),
            });
        }
        for page in &doc.pages {
            let Some(before) = was.pages.iter().find(|p| p.page == page.page) else {
                findings.push(Finding {
                    corpus: doc.corpus.clone(),
                    path: doc.path.clone(),
                    page: Some(page.page),
                    kind: Kind::New,
                    detail: "page not in the baseline".into(),
                });
                continue;
            };
            if before.hayro_sha256 != page.hayro_sha256 {
                findings.push(Finding {
                    corpus: doc.corpus.clone(),
                    path: doc.path.clone(),
                    page: Some(page.page),
                    kind: Kind::HashChanged,
                    detail: format!(
                        "{} -> {} ({}x{} -> {}x{})",
                        &before.hayro_sha256[..12],
                        &page.hayro_sha256[..12],
                        before.width,
                        before.height,
                        page.width,
                        page.height
                    ),
                });
            }
            // Divergence is only comparable when both runs had PDFium. A run
            // without the library is not evidence that nothing got worse, and
            // must not be reported as if it were.
            if let (Some(was_mean), Some(now_mean)) = (before.mean_abs, page.mean_abs)
                && now_mean > was_mean + MEAN_ABS_EPSILON
            {
                findings.push(Finding {
                    corpus: doc.corpus.clone(),
                    path: doc.path.clone(),
                    page: Some(page.page),
                    kind: Kind::Diverged,
                    detail: format!("mean abs {was_mean:.3} -> {now_mean:.3}"),
                });
            }
            if let (Some(was_frac), Some(now_frac)) = (before.frac_off, page.frac_off)
                && now_frac > was_frac + FRAC_OFF_EPSILON
            {
                findings.push(Finding {
                    corpus: doc.corpus.clone(),
                    path: doc.path.clone(),
                    page: Some(page.page),
                    kind: Kind::Diverged,
                    detail: format!(
                        "pixels off by >16: {:.2}% -> {:.2}%",
                        was_frac * 100.0,
                        now_frac * 100.0
                    ),
                });
            }
        }
    }

    let ran: BTreeMap<(&str, &str), ()> = run
        .docs
        .iter()
        .map(|doc| ((doc.corpus.as_str(), doc.path.as_str()), ()))
        .collect();
    for doc in known {
        if corpora.contains(&doc.corpus)
            && !ran.contains_key(&(doc.corpus.as_str(), doc.path.as_str()))
        {
            findings.push(Finding {
                corpus: doc.corpus.clone(),
                path: doc.path.clone(),
                page: None,
                kind: Kind::Missing,
                detail: "in the baseline, not in this run".into(),
            });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smoke test: the harness end to end over evo's own committed
    /// fixtures, hashes only, no network and no PDFium.
    ///
    /// Ignored by default because rendering five documents is not something
    /// every `cargo test` should pay for, and run in CI as
    /// `cargo test -p xtask -- --ignored fidelity_`. What it protects is the
    /// harness itself: that it still compiles, still parses its manifests,
    /// still renders, and still agrees with the baseline about the pixels of
    /// the one corpus that travels with the repository.
    #[test]
    #[ignore = "renders PDFs; run with --ignored fidelity_"]
    fn fidelity_smoke_matches_the_committed_fixture_hashes() {
        let repo = crate::repo_root();
        let manifest = corpus::load("fixtures").expect("the fixtures manifest");
        assert!(!manifest.files.is_empty());
        assert!(
            manifest.base_url.is_none(),
            "the fixtures corpus must never need the network"
        );

        let baseline = read_baseline(&repo).expect("the baseline");
        let known = baseline.platforms.get(&platform());

        let mut run = Run::default();
        let mut checked = 0;
        for entry in &manifest.files {
            let bytes = Arc::new(manifest.bytes(entry, &repo).expect("a committed fixture"));
            let doc = one_document("fixtures", entry, bytes, DEFAULT_MAX_PAGES, None, &mut run);
            assert_eq!(doc.status, Status::Ok, "{}", entry.path);
            assert!(!doc.pages.is_empty(), "{}", entry.path);

            let Some(was) = known
                .and_then(|docs| docs.iter().find(|d| d.path == entry.path))
                .filter(|d| d.corpus == "fixtures")
            else {
                continue;
            };
            for page in &doc.pages {
                let before = was
                    .pages
                    .iter()
                    .find(|p| p.page == page.page)
                    .unwrap_or_else(|| panic!("{} page {} in the baseline", entry.path, page.page));
                assert_eq!(
                    (before.hayro_sha256.as_str(), before.width, before.height),
                    (page.hayro_sha256.as_str(), page.width, page.height),
                    "{} page {}: hayro draws this differently than the baseline. \
                     If that is intended, `cargo run -p xtask -- fidelity --bless`.",
                    entry.path,
                    page.page + 1
                );
                checked += 1;
            }
        }
        if known.is_some() {
            assert!(checked > 0, "the baseline has no fixture pages to check");
        } else {
            println!("no baseline for {} yet; rendering only", platform());
        }
    }

    /// Blessing one corpus must not delete another's numbers, and must not
    /// touch another platform's.
    #[test]
    fn blessing_one_corpus_leaves_the_rest_of_the_baseline_alone() {
        fn doc(corpus: &str, path: &str) -> Doc {
            Doc {
                corpus: corpus.into(),
                path: path.into(),
                status: Status::Ok,
                hayro_warnings: false,
                pages: Vec::new(),
            }
        }

        let mut baseline = Baseline::default();
        baseline.platforms.insert(
            platform(),
            vec![doc("fixtures", "a.pdf"), doc("verapdf", "b.pdf")],
        );
        baseline
            .platforms
            .insert("elsewhere-x86_64".into(), vec![doc("fixtures", "a.pdf")]);

        let run = Run {
            docs: vec![doc("fixtures", "c.pdf")],
            ..Run::default()
        };
        merge(&mut baseline, &run, &["fixtures".to_owned()]);

        let here = &baseline.platforms[&platform()];
        assert_eq!(here.len(), 2, "the veraPDF entry survives");
        assert_eq!(here[0].path, "c.pdf");
        assert_eq!(here[1].corpus, "verapdf");
        assert_eq!(baseline.platforms["elsewhere-x86_64"].len(), 1);
    }

    /// The failure rules, which are the whole contract with CI.
    #[test]
    fn only_regressions_fail_a_run() {
        let page = |hash: &str, mean: f64| Page {
            page: 0,
            width: 10,
            height: 10,
            hayro_sha256: hash.repeat(16),
            mean_abs: Some(mean),
            frac_off: Some(0.0),
            note: None,
        };
        let doc = |hash: &str, mean: f64| Doc {
            corpus: "fixtures".into(),
            path: "a.pdf".into(),
            status: Status::Ok,
            hayro_warnings: false,
            pages: vec![page(hash, mean)],
        };

        let mut baseline = Baseline::default();
        baseline.platforms.insert(platform(), vec![doc("ab", 1.0)]);
        let corpora = vec!["fixtures".to_owned()];

        // Same hash, divergence a hair better: nothing to say.
        let steady = Run {
            docs: vec![doc("ab", 0.9)],
            ..Run::default()
        };
        let findings = check(&baseline, &steady, &corpora);
        assert!(!findings.iter().any(Finding::fails), "steady run failed");

        // A different hash is a failure however small the divergence.
        let moved = Run {
            docs: vec![doc("ef", 0.0)],
            ..Run::default()
        };
        let findings = check(&baseline, &moved, &corpora);
        assert!(findings.iter().any(|f| f.kind == Kind::HashChanged));
        assert!(findings.iter().any(Finding::fails));

        // Worse than the epsilon is a failure; inside it is not.
        let drifted = Run {
            docs: vec![doc("ab", 1.0 + MEAN_ABS_EPSILON / 2.0)],
            ..Run::default()
        };
        assert!(
            !check(&baseline, &drifted, &corpora)
                .iter()
                .any(Finding::fails)
        );
        let worse = Run {
            docs: vec![doc("ab", 1.0 + MEAN_ABS_EPSILON * 2.0)],
            ..Run::default()
        };
        assert!(
            check(&baseline, &worse, &corpora)
                .iter()
                .any(|f| f.kind == Kind::Diverged)
        );
    }

    /// A run on a machine with no PDFium has no divergence to compare, and
    /// must not be mistaken for a clean bill of health.
    #[test]
    fn a_run_without_pdfium_does_not_check_divergence() {
        let mut baseline = Baseline::default();
        let mut before = Doc {
            corpus: "fixtures".into(),
            path: "a.pdf".into(),
            status: Status::Ok,
            hayro_warnings: false,
            pages: vec![Page {
                page: 0,
                width: 10,
                height: 10,
                hayro_sha256: "aa".repeat(16),
                mean_abs: Some(0.1),
                frac_off: Some(0.0),
                note: None,
            }],
        };
        baseline.platforms.insert(platform(), vec![before.clone()]);

        before.pages[0].mean_abs = None;
        before.pages[0].frac_off = None;
        before.pages[0].note = Some("no PDFium on this machine".into());
        let run = Run {
            docs: vec![before],
            ..Run::default()
        };
        assert!(
            !check(&baseline, &run, &["fixtures".to_owned()])
                .iter()
                .any(Finding::fails)
        );
    }

    /// A document nobody can parse is data, not an error -- but a document
    /// that *stopped* parsing is a regression.
    #[test]
    fn a_document_that_stops_parsing_is_a_regression() {
        let mut baseline = Baseline::default();
        let mut doc = Doc {
            corpus: "verapdf".into(),
            path: "broken.pdf".into(),
            status: Status::Ok,
            hayro_warnings: false,
            pages: Vec::new(),
        };
        baseline.platforms.insert(platform(), vec![doc.clone()]);
        doc.status = Status::HayroUnparseable;
        let run = Run {
            docs: vec![doc],
            ..Run::default()
        };
        let findings = check(&baseline, &run, &["verapdf".to_owned()]);
        assert!(findings.iter().any(|f| f.kind == Kind::StatusChanged));
    }

    #[test]
    fn every_corpus_manifest_parses_and_is_licensed() {
        for name in corpus::names() {
            let manifest = corpus::load(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(manifest.name, name);
            assert!(!manifest.license.is_empty(), "{name} has no licence");
            assert!(!manifest.files.is_empty(), "{name} is empty");
            for entry in &manifest.files {
                assert!(entry.path.ends_with(".pdf"), "{}", entry.path);
                assert!(!entry.path.starts_with('/'), "{}", entry.path);
                assert!(!entry.path.contains(".."), "{}", entry.path);
                if manifest.base_url.is_some() {
                    let sha = entry.sha256.as_deref().unwrap_or("");
                    assert_eq!(sha.len(), 64, "{} has no sha256", entry.path);
                }
            }
        }
    }

    #[test]
    fn slugs_are_file_names() {
        assert_eq!(
            slug("TWG test files/A001 (a).pdf"),
            "TWG-test-files-A001--a--pdf"
        );
        assert!(!slug("../evil").contains('/'));
    }

    #[test]
    fn rounding_keeps_the_baseline_readable() {
        assert_eq!(round(1.234_567_89, 4), 1.2346);
        assert_eq!(round(0.000_000_4, 6), 0.0);
    }
}
