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
fn run(
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

    #[test]
    fn nothing_is_in_flight_before_anything_starts() {
        let d = Downloads::default();
        assert!(!d.in_flight("qwen3-4b-instruct-2507"));
        assert_eq!(d.status("qwen3-4b-instruct-2507"), None);
    }
}
