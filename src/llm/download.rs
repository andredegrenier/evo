//! Downloading model weights.
//!
//! These files are gigabytes, so unlike the OCR models they are never held in
//! memory: the response is copied straight to a `.part` file and hashed on the
//! way past. The finished file is only put in place once its checksum matches
//! the source it came from, which means a truncated or corrupted download can
//! never be mistaken for a usable model.
//!
//! One thread per download, a shared status the UI polls, and a repaint after
//! progress moves -- the same shape as every other worker in evo.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use sha2::{Digest, Sha256};

use super::{CatalogEntry, ModelSource};

/// Marker carried by the io error that cancellation raises.
const CANCELLED: &str = "cancelled";

/// How often a download in flight asks for a repaint.
const REPAINT_EVERY: Duration = Duration::from_millis(150);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadStatus {
    pub received: u64,
    /// What the server said the file is, or the catalogue's size until it does.
    pub total: u64,
    pub error: Option<String>,
    pub done: bool,
}

impl DownloadStatus {
    /// 0.0 to 1.0, or `None` while the size is unknown.
    pub fn fraction(&self) -> Option<f32> {
        (self.total > 0).then(|| (self.received as f64 / self.total as f64) as f32)
    }
}

struct Download {
    status: Arc<Mutex<DownloadStatus>>,
    cancel: Arc<AtomicBool>,
}

/// The downloads this session has started, by catalogue id.
#[derive(Default)]
pub struct Downloads {
    active: HashMap<&'static str, Download>,
}

impl Downloads {
    /// Start fetching `entry` into `dir`. A second call while one is already
    /// running is ignored.
    pub fn start(&mut self, entry: &'static CatalogEntry, dir: PathBuf, ctx: &egui::Context) {
        if self.in_flight(entry.id) {
            return;
        }
        let status = Arc::new(Mutex::new(DownloadStatus {
            total: entry.size(),
            ..Default::default()
        }));
        let cancel = Arc::new(AtomicBool::new(false));

        let worker_status = status.clone();
        let worker_cancel = cancel.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("evo-llm-download".into())
            .spawn(move || {
                let result = run(entry, &dir, &worker_status, &worker_cancel, &ctx);
                {
                    let mut s = worker_status.lock().unwrap();
                    s.done = true;
                    s.error = result.err();
                }
                ctx.request_repaint();
            });

        match spawned {
            Ok(_) => {
                self.active.insert(entry.id, Download { status, cancel });
            }
            Err(e) => {
                self.active.insert(
                    entry.id,
                    Download {
                        status: Arc::new(Mutex::new(DownloadStatus {
                            done: true,
                            error: Some(e.to_string()),
                            ..Default::default()
                        })),
                        cancel,
                    },
                );
            }
        }
    }

    pub fn cancel(&self, id: &str) {
        if let Some(d) = self.active.get(id) {
            d.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// What this download is doing, if evo has started one this session.
    pub fn status(&self, id: &str) -> Option<DownloadStatus> {
        self.active
            .get(id)
            .map(|d| d.status.lock().unwrap().clone())
    }

    pub fn in_flight(&self, id: &str) -> bool {
        self.status(id).is_some_and(|s| !s.done)
    }

    /// Forget a finished download, so its row goes back to showing the file
    /// itself. Errors are kept until the user starts another attempt.
    pub fn dismiss(&mut self, id: &str) {
        self.active.remove(id);
    }

    /// Drop the record of downloads that finished cleanly: the file on disk is
    /// the state from then on.
    pub fn forget_finished(&mut self) {
        self.active.retain(|_, d| {
            let s = d.status.lock().unwrap();
            !(s.done && s.error.is_none())
        });
    }
}

/// Try each mirror in turn. The first one whose bytes match its checksum wins.
///
/// `pub(crate)` because `evo fetch-model` runs it directly: a headless server
/// has no Preferences pane to press a button in, but it wants exactly the same
/// download, with the same mirrors and the same checksum, rather than a second
/// implementation that agrees with this one until it does not.
pub(crate) fn run(
    entry: &CatalogEntry,
    dir: &Path,
    status: &Arc<Mutex<DownloadStatus>>,
    cancel: &Arc<AtomicBool>,
    ctx: &egui::Context,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let dest = entry.path_in(dir);
    let part = dest.with_extension("part");

    let mut last_error = String::from("no sources are configured");
    for source in &entry.sources {
        {
            let mut s = status.lock().unwrap();
            s.received = 0;
            s.total = source.size;
        }
        // There is no resume: a partial file from an earlier attempt is a
        // different quantization as often as not.
        let _ = std::fs::remove_file(&part);

        match fetch(source, &part, status, cancel, ctx) {
            Ok(digest) => match verify_and_commit(&part, &dest, &digest, source.sha256) {
                Ok(()) => return Ok(()),
                Err(e) => last_error = e,
            },
            Err(e) if e == CANCELLED => {
                let _ = std::fs::remove_file(&part);
                return Err("download cancelled".to_owned());
            }
            Err(e) => last_error = e,
        }
    }
    let _ = std::fs::remove_file(&part);
    Err(last_error)
}

/// Stream one source into `part`, returning the SHA-256 of what arrived.
fn fetch(
    source: &ModelSource,
    part: &Path,
    status: &Arc<Mutex<DownloadStatus>>,
    cancel: &Arc<AtomicBool>,
    ctx: &egui::Context,
) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        // No global timeout: a 2.5 GB file over a slow line is not a stuck
        // request. Cancellation is the user's, not the clock's.
        .timeout_global(None)
        .build()
        .into();

    let response = agent
        .get(source.url)
        .call()
        .map_err(|e| format!("could not reach {}: {e}", source.url))?;
    let http_status = response.status().as_u16();
    if !(200..300).contains(&http_status) {
        return Err(format!("{} returned {http_status}", source.url));
    }

    let body = response.into_body();
    if let Some(len) = body.content_length() {
        status.lock().unwrap().total = len;
    }

    let file =
        File::create(part).map_err(|e| format!("could not write {}: {e}", part.display()))?;
    let mut sink = Sink {
        file: BufWriter::with_capacity(1 << 20, file),
        hasher: Sha256::new(),
        received: 0,
        status,
        cancel,
        ctx,
        last_repaint: Instant::now(),
    };

    // `into_reader` is unlimited -- the 10 MB cap only applies to the
    // read-it-all-into-memory helpers, which is exactly what this avoids.
    let mut reader = body.into_reader();
    io::copy(&mut reader, &mut sink).map_err(|e| {
        if e.kind() == ErrorKind::Interrupted {
            CANCELLED.to_owned()
        } else {
            format!("download failed: {e}")
        }
    })?;

    let Sink { file, hasher, .. } = sink;
    let file = file
        .into_inner()
        .map_err(|e| format!("could not write {}: {e}", part.display()))?;
    file.sync_all()
        .map_err(|e| format!("could not flush {}: {e}", part.display()))?;
    Ok(hex(&hasher.finalize()))
}

/// The download's destination while it is in flight: writes through to the
/// `.part` file, hashes as it goes, and keeps the UI informed.
struct Sink<'a> {
    file: BufWriter<File>,
    hasher: Sha256,
    received: u64,
    status: &'a Arc<Mutex<DownloadStatus>>,
    cancel: &'a Arc<AtomicBool>,
    ctx: &'a egui::Context,
    last_repaint: Instant,
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(ErrorKind::Interrupted, CANCELLED));
        }
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.received += n as u64;
        self.status.lock().unwrap().received = self.received;
        // Repainting per 8 KB chunk would spend more time drawing a progress
        // bar than downloading.
        if self.last_repaint.elapsed() >= REPAINT_EVERY {
            self.ctx.request_repaint();
            self.last_repaint = Instant::now();
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Put a finished `.part` file in place, but only if it is the file that was
/// asked for. A mismatch takes the download with it: half a model is worse
/// than none, since llama.cpp would only discover it much later.
pub fn verify_and_commit(
    part: &Path,
    dest: &Path,
    digest: &str,
    expected: &str,
) -> Result<(), String> {
    if !digest.eq_ignore_ascii_case(expected) {
        let _ = std::fs::remove_file(part);
        return Err(format!(
            "the download did not match its checksum (expected {expected}, got {digest}); \
             it has been discarded, please try again"
        ));
    }
    // Rename is atomic, but only durable if the bytes are on the disk first.
    // Windows refuses sync_all on a read-only handle (os error 5), so the
    // file must be reopened writable for the flush.
    std::fs::OpenOptions::new()
        .write(true)
        .open(part)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("could not flush {}: {e}", part.display()))?;
    std::fs::rename(part, dest).map_err(|e| {
        format!(
            "could not move {} into place: {e}",
            part.file_name().unwrap_or_default().to_string_lossy()
        )
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// `evo fetch-model` -- the same download, without a window
// ---------------------------------------------------------------------------

/// How often the progress line is reprinted, in whole percent. Every chunk
/// would be a wall of text in a journal; a line every twentieth of the file is
/// enough to see that a download on a slow server is still moving.
const PROGRESS_STEP: u64 = 5;

pub fn usage() -> String {
    let mut out = String::from(
        "usage: evo fetch-model [id]\n\n  \
         Downloads a model's weights so `evo serve` can answer questions with\n  \
         them. With no id, the catalogue's default.\n\nModels:\n",
    );
    for entry in &super::CATALOG {
        let default = if entry.id == super::DEFAULT_MODEL {
            "  (default)"
        } else {
            ""
        };
        out.push_str(&format!(
            "  {:<24} {} \u{2014} {}{default}\n",
            entry.id,
            super::human_size(entry.size()),
            entry.label,
        ));
    }
    out
}

/// Which model the arguments name.
fn chosen(args: &[String]) -> Result<&'static CatalogEntry, String> {
    let id = match args {
        [] => super::DEFAULT_MODEL,
        [one] if one == "--help" || one == "-h" => return Err(usage()),
        [one] if !one.starts_with('-') => one.as_str(),
        _ => {
            return Err(format!(
                "evo fetch-model takes one model id.\n\n{}",
                usage()
            ));
        }
    };
    super::entry(id).ok_or_else(|| {
        format!(
            "evo has no model called \u{201c}{id}\u{201d}.\n\n{}",
            usage()
        )
    })
}

/// The `evo fetch-model` entry point. Never returns: the process exists to
/// fetch one file.
pub fn main() -> ! {
    // Skip the binary and the `fetch-model` word itself.
    let args: Vec<String> = std::env::args().skip(2).collect();
    match fetch_model(&args) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn fetch_model(args: &[String]) -> Result<(), String> {
    let entry = chosen(args)?;
    let dir = super::llm_models_dir()
        .ok_or_else(|| "evo could not find a data directory to keep models in.".to_owned())?;

    if let Some(path) = entry.installed_in(&dir) {
        println!("{} is already here: {}", entry.label, path.display());
        return Ok(());
    }
    println!(
        "Fetching {} ({}) into {}.",
        entry.label,
        super::human_size(entry.size()),
        dir.display()
    );

    let status = Arc::new(Mutex::new(DownloadStatus {
        total: entry.size(),
        ..Default::default()
    }));
    let cancel = Arc::new(AtomicBool::new(false));

    // The download reports itself through the same shared status the
    // Preferences pane polls, so the progress line is a second reader of it
    // rather than anything new in the download path.
    let watched = status.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let stop = finished.clone();
    let printer = std::thread::Builder::new()
        .name("evo-fetch-progress".into())
        .spawn(move || {
            let mut last = u64::MAX;
            while !stop.load(Ordering::Relaxed) {
                let seen = watched.lock().unwrap().clone();
                if let Some(percent) = percent(&seen)
                    && (last == u64::MAX || percent >= last + PROGRESS_STEP)
                {
                    println!(
                        "  {percent}% \u{2014} {} of {}",
                        super::human_size(seen.received),
                        super::human_size(seen.total)
                    );
                    last = percent;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        })
        .map_err(|e| format!("could not start the progress thread: {e}"))?;

    // No window, so a detached context: nothing is listening for the repaints
    // the download asks for, which is exactly the point.
    let result = run(entry, &dir, &status, &cancel, &egui::Context::default());
    finished.store(true, Ordering::Relaxed);
    let _ = printer.join();
    result?;

    let path = entry.path_in(&dir);
    println!(
        "{} is ready: {} ({}).",
        entry.label,
        path.display(),
        super::human_size(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0))
    );
    println!("Point `evo serve` at it with `\"api\": \"Builtin\"` in serve/config.json.");
    Ok(())
}

/// Whole percent downloaded, once there is a size to be a percentage of.
/// Widened first: a model is gigabytes, and gigabytes times a hundred is not
/// a number to trust to 64 bits.
fn percent(status: &DownloadStatus) -> Option<u64> {
    (status.total > 0)
        .then(|| (u128::from(status.received) * 100 / u128::from(status.total)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "evo-llm-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn digest_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex(&h.finalize())
    }

    #[test]
    fn a_matching_download_is_moved_into_place() {
        let dir = tempdir("commit");
        let part = dir.join("model.part");
        let dest = dir.join("model.gguf");
        std::fs::write(&part, b"weights").expect("write");

        verify_and_commit(&part, &dest, &digest_of(b"weights"), &digest_of(b"weights"))
            .expect("commit");

        assert!(!part.exists(), "the part file is gone");
        assert_eq!(std::fs::read(&dest).expect("read"), b"weights");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The digest comparison is the whole point, so a wrong one must not only
    /// fail but take the bad file with it.
    #[test]
    fn a_mismatched_download_is_discarded_and_says_why() {
        let dir = tempdir("mismatch");
        let part = dir.join("model.part");
        let dest = dir.join("model.gguf");
        std::fs::write(&part, b"truncated").expect("write");

        let err = verify_and_commit(
            &part,
            &dest,
            &digest_of(b"truncated"),
            &digest_of(b"the real thing"),
        )
        .expect_err("a mismatch");

        assert!(err.contains("checksum"), "{err}");
        assert!(err.contains(&digest_of(b"the real thing")), "{err}");
        assert!(!part.exists(), "the bad download was kept: {err}");
        assert!(!dest.exists(), "a bad download must never be committed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digests_compare_regardless_of_case() {
        let dir = tempdir("case");
        let part = dir.join("model.part");
        std::fs::write(&part, b"x").expect("write");
        let d = digest_of(b"x");
        verify_and_commit(&part, &dir.join("model.gguf"), &d.to_uppercase(), &d).expect("commit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_part_file_is_an_error_not_a_panic() {
        let dir = tempdir("missing");
        let err = verify_and_commit(&dir.join("gone.part"), &dir.join("m.gguf"), "ab", "ab")
            .expect_err("no such file");
        assert!(err.contains("could not flush"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_is_a_fraction_only_once_the_size_is_known() {
        let mut s = DownloadStatus::default();
        assert_eq!(s.fraction(), None);
        s.total = 100;
        s.received = 25;
        assert_eq!(s.fraction(), Some(0.25));
    }

    /// The whole path, for real: fetch a catalogue entry from Hugging Face,
    /// hash it while it streams, and put it in place only if it matches.
    /// Ignored by default -- it is a gigabyte over the network:
    ///
    /// ```text
    /// EVO_LLM_DOWNLOAD_TEST=qwen3-1.7b cargo test -- --ignored downloading
    /// ```
    #[test]
    #[ignore = "downloads gigabytes; set EVO_LLM_DOWNLOAD_TEST to a catalogue id"]
    fn downloading_a_real_model_ends_with_a_verified_file() {
        let id = std::env::var("EVO_LLM_DOWNLOAD_TEST")
            .expect("set EVO_LLM_DOWNLOAD_TEST to a catalogue id");
        let entry = crate::llm::entry(&id).expect("a catalogue entry");
        let dir = crate::llm::llm_models_dir().expect("a data directory");

        let ctx = egui::Context::default();
        let mut downloads = Downloads::default();
        downloads.start(entry, dir.clone(), &ctx);

        let mut status = downloads.status(entry.id).expect("a download");
        while !status.done {
            std::thread::sleep(Duration::from_millis(500));
            status = downloads.status(entry.id).expect("a download");
        }
        assert_eq!(status.error, None, "the download failed");
        assert!(status.received > 0, "nothing was received");

        let path = entry.installed_in(&dir).expect("the model is in place");
        let on_disk = std::fs::metadata(&path).expect("metadata").len();
        assert!(
            entry.sources.iter().any(|s| s.size == on_disk),
            "{on_disk} bytes is not any source's size"
        );
        assert!(
            !path.with_extension("part").exists(),
            "a part file was left"
        );
    }

    /// `evo fetch-model` is typed once, over SSH, by somebody who has just
    /// built evo on a server -- so the id has to be optional and a wrong one
    /// has to print what the right ones are.
    #[test]
    fn fetch_model_defaults_to_the_catalogue_default_and_names_the_rest() {
        let args = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        assert_eq!(
            chosen(&[]).expect("the default").id,
            crate::llm::DEFAULT_MODEL
        );
        assert_eq!(
            chosen(&args(&["qwen3-1.7b"])).expect("by id").id,
            "qwen3-1.7b"
        );

        let unknown = chosen(&args(&["llama3"])).expect_err("no such model");
        assert!(unknown.contains("llama3"), "{unknown}");
        for entry in &crate::llm::CATALOG {
            assert!(unknown.contains(entry.id), "{unknown} does not list them");
        }

        let help = chosen(&args(&["--help"])).expect_err("usage is not a model");
        assert!(help.starts_with("usage: evo fetch-model"), "{help}");
        assert!(help.contains("(default)"), "{help}");

        let flag = chosen(&args(&["--everything"])).expect_err("not a flag it knows");
        assert!(flag.contains("one model id"), "{flag}");
        let two = chosen(&args(&["a", "b"])).expect_err("one at a time");
        assert!(two.contains("one model id"), "{two}");
    }

    #[test]
    fn progress_is_whole_percent_and_only_once_the_size_is_known() {
        assert_eq!(percent(&DownloadStatus::default()), None);
        assert_eq!(
            percent(&DownloadStatus {
                received: 25,
                total: 200,
                ..Default::default()
            }),
            Some(12)
        );
        // A file bigger than u64::MAX / 100 must not overflow into nonsense.
        assert_eq!(
            percent(&DownloadStatus {
                received: u64::MAX,
                total: u64::MAX,
                ..Default::default()
            }),
            Some(100)
        );
    }

    #[test]
    fn nothing_is_in_flight_before_anything_starts() {
        let d = Downloads::default();
        assert!(!d.in_flight("qwen3-4b-instruct-2507"));
        assert_eq!(d.status("qwen3-4b-instruct-2507"), None);
    }
}
