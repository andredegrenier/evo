//! Getting the PDFium shared library onto this machine.
//!
//! Release builds of evo ship PDFium beside the binary, so most people never
//! come near this module. It is here for the two cases the bundle cannot
//! cover: somebody who built evo from source (`cargo install evo`), and the
//! Debian box that runs `evo serve` from a tarball it assembled itself.
//!
//! The version, the URL and the checksums live in `deploy/pdfium.lock`, which
//! the release workflow reads as shell and this module reads as text. One file
//! means the binary evo downloads and the binary evo bundles can never drift
//! apart.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::llm::download::{DownloadStatus, fetch, verify_and_commit};

use super::engine::{library_file_name, pdfium_data_dir};

/// The pinned release, as checked in.
static LOCK: LazyLock<Lock> =
    LazyLock::new(|| Lock::parse(include_str!("../../deploy/pdfium.lock")));

/// The PDFium build evo asks for. Also the name of the directory it is
/// installed into, so two versions can sit side by side.
pub fn version() -> &'static str {
    LOCK.version
}

pub fn tag() -> &'static str {
    LOCK.tag
}

/// One prebuilt archive: what to ask for, what it should hash to, and which
/// file inside it is the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    pub file: &'static str,
    pub sha256: &'static str,
    /// Path of the shared library within the archive.
    pub member: &'static str,
}

impl Asset {
    pub fn url(&self) -> String {
        format!("{}/{}", LOCK.base_url, self.file)
    }
}

/// The archive for the platform evo is running on, or `None` where
/// pdfium-binaries publishes a build evo does not pin.
pub fn asset() -> Option<Asset> {
    let lock = &*LOCK;
    match (std::env::consts::OS, std::env::consts::ARCH) {
        // Universal: one archive covers Apple silicon and Intel, so there is
        // nothing to `lipo` together and nothing to get wrong.
        ("macos", _) => Some(Asset {
            file: "pdfium-mac-univ.tgz",
            sha256: lock.mac_univ,
            member: "lib/libpdfium.dylib",
        }),
        ("linux", "x86_64") => Some(Asset {
            file: "pdfium-linux-x64.tgz",
            sha256: lock.linux_x64,
            member: "lib/libpdfium.so",
        }),
        ("windows", "x86_64") => Some(Asset {
            file: "pdfium-win-x64.tgz",
            sha256: lock.win_x64,
            member: "bin/pdfium.dll",
        }),
        _ => None,
    }
}

/// The message for a platform evo has no pinned build for. Its own function
/// because both the button and the command line say it.
pub fn unsupported_platform() -> String {
    format!(
        "evo has no pinned PDFium build for {} on {}. Build or download one yourself \
         and point evo at it with EVO_PDFIUM_PATH; the hayro renderer works meanwhile.",
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

// ---------------------------------------------------------------------------
// deploy/pdfium.lock
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Lock {
    version: &'static str,
    tag: &'static str,
    base_url: &'static str,
    mac_univ: &'static str,
    linux_x64: &'static str,
    win_x64: &'static str,
}

impl Lock {
    /// Read the checked-in lock file. Panics on a malformed one, and should:
    /// the file is compiled in, so a bad edit is a build that never shipped
    /// rather than a download that fails on a stranger's machine. The unit
    /// test below is what actually catches it.
    fn parse(text: &'static str) -> Self {
        let get = |key: &str| -> &'static str {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.starts_with('#'))
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or_else(|| panic!("deploy/pdfium.lock has no {key}"))
        };
        Self {
            version: get("PDFIUM_VERSION"),
            tag: get("PDFIUM_TAG"),
            base_url: get("PDFIUM_BASE_URL"),
            mac_univ: get("PDFIUM_MAC_UNIV_SHA256"),
            linux_x64: get("PDFIUM_LINUX_X64_SHA256"),
            win_x64: get("PDFIUM_WIN_X64_SHA256"),
        }
    }
}

// ---------------------------------------------------------------------------
// The download itself
// ---------------------------------------------------------------------------

/// Download the pinned archive into `dir` and unpack the library from it.
///
/// The archive is checked against its checksum before anything is unpacked, so
/// a truncated download can never leave a half-library behind for `dlopen` to
/// find. Returns the path of the installed library.
pub fn install(
    dir: &Path,
    status: &Arc<Mutex<DownloadStatus>>,
    cancel: &Arc<AtomicBool>,
    ctx: &egui::Context,
) -> Result<PathBuf, String> {
    let asset = asset().ok_or_else(unsupported_platform)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let archive = dir.join(asset.file);
    let part = archive.with_extension("part");
    let _ = std::fs::remove_file(&part);

    let digest = fetch(&asset.url(), &part, status, cancel, ctx)?;
    verify_and_commit(&part, &archive, &digest, asset.sha256)?;

    let library = unpack(&archive, asset.member, &dir.join(library_file_name()));
    // The archive is 4 MB of headers and licences evo has no further use for.
    let _ = std::fs::remove_file(&archive);
    if library.is_ok() {
        // There was nothing here when evo last looked; there is now.
        super::engine::pdfium_search_again();
    }
    library
}

/// Pull one member out of a gzipped tar and put it at `dest`.
fn unpack(archive: &Path, member: &str, dest: &Path) -> Result<PathBuf, String> {
    let file =
        File::open(archive).map_err(|e| format!("could not open {}: {e}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let part = dest.with_extension("part");
    let entries = tar
        .entries()
        .map_err(|e| format!("{} is not a readable archive: {e}", archive.display()))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("could not read the PDFium archive: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("could not read the PDFium archive: {e}"))?;
        if path.to_string_lossy() != member {
            continue;
        }
        entry
            .unpack(&part)
            .map_err(|e| format!("could not write {}: {e}", part.display()))?;
        std::fs::rename(&part, dest).map_err(|e| {
            let _ = std::fs::remove_file(&part);
            format!("could not finish writing {}: {e}", dest.display())
        })?;
        return Ok(dest.to_owned());
    }
    Err(format!(
        "the PDFium archive did not contain {member}; evo cannot use it"
    ))
}

// ---------------------------------------------------------------------------
// The Preferences button
// ---------------------------------------------------------------------------

/// The "Get PDFium" download, in the shape every other worker in evo has: a
/// thread, a shared status the UI polls, and a repaint when it moves.
#[derive(Default)]
pub struct PdfiumFetch {
    status: Option<Arc<Mutex<DownloadStatus>>>,
}

impl PdfiumFetch {
    /// Start the download. A second call while one is running does nothing.
    pub fn start(&mut self, ctx: &egui::Context) {
        if self.in_flight() {
            return;
        }
        let Some(dir) = pdfium_data_dir() else {
            self.status = Some(Arc::new(Mutex::new(DownloadStatus {
                done: true,
                error: Some("evo could not find a data directory to keep PDFium in.".to_owned()),
                ..Default::default()
            })));
            return;
        };

        let status = Arc::new(Mutex::new(DownloadStatus::default()));
        self.status = Some(status.clone());
        let worker_status = status.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("evo-pdfium-fetch".into())
            .spawn(move || {
                let cancel = Arc::new(AtomicBool::new(false));
                let result = install(&dir, &worker_status, &cancel, &ctx);
                {
                    let mut s = worker_status.lock().unwrap();
                    s.done = true;
                    s.error = result.err();
                }
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            let mut s = status.lock().unwrap();
            s.done = true;
            s.error = Some(format!("could not start the download: {e}"));
        }
    }

    pub fn status(&self) -> Option<DownloadStatus> {
        self.status.as_ref().map(|s| s.lock().unwrap().clone())
    }

    pub fn in_flight(&self) -> bool {
        self.status().is_some_and(|s| !s.done)
    }

    /// Forget a finished download, so the row goes back to showing the library
    /// on disk. Errors stay until the user tries again.
    pub fn dismiss(&mut self) {
        self.status = None;
    }
}

// ---------------------------------------------------------------------------
// `evo fetch-pdfium` -- the same download, without a window
// ---------------------------------------------------------------------------

/// How often the progress line is reprinted, in whole percent.
const PROGRESS_STEP: u64 = 10;

pub fn usage() -> String {
    format!(
        "usage: evo fetch-pdfium [--into <dir>]\n\n  \
         Downloads the PDFium rendering library ({tag}) so evo can rasterize\n  \
         pages with it. Release builds already ship it; this is for source\n  \
         builds and servers.\n\n  \
         --into <dir>   Put the library in <dir> instead of evo's data\n  \
                        directory. Use it to drop a copy beside a development\n  \
                        binary: `cargo run -- fetch-pdfium --into target/debug`.\n",
        tag = tag(),
    )
}

/// The `evo fetch-pdfium` entry point. Never returns: the process exists to
/// fetch one file.
pub fn main() -> ! {
    // Skip the binary and the `fetch-pdfium` word itself.
    let args: Vec<String> = std::env::args().skip(2).collect();
    match fetch_pdfium(&args) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// Where the arguments say to install.
fn destination(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [one] if one == "--help" || one == "-h" => Err(usage()),
        [flag, dir] if flag == "--into" => Ok(Some(PathBuf::from(dir))),
        _ => Err(format!(
            "evo fetch-pdfium takes an optional --into <dir>.\n\n{}",
            usage()
        )),
    }
}

fn fetch_pdfium(args: &[String]) -> Result<(), String> {
    let dir = match destination(args)? {
        Some(dir) => dir,
        None => pdfium_data_dir()
            .ok_or_else(|| "evo could not find a data directory to keep PDFium in.".to_owned())?,
    };
    let asset = asset().ok_or_else(unsupported_platform)?;

    let installed = dir.join(library_file_name());
    if installed.is_file() {
        println!("PDFium {} is already here: {}", tag(), installed.display());
        return Ok(());
    }
    println!("Fetching PDFium {} into {}.", tag(), dir.display());
    println!("  {}", asset.url());

    let status = Arc::new(Mutex::new(DownloadStatus::default()));
    let cancel = Arc::new(AtomicBool::new(false));

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
                        crate::llm::human_size(seen.received),
                        crate::llm::human_size(seen.total)
                    );
                    last = percent;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        })
        .map_err(|e| format!("could not start the progress thread: {e}"))?;

    // No window, so a detached context: nothing is listening for the repaints
    // the download asks for, which is exactly the point.
    let result = install(&dir, &status, &cancel, &egui::Context::default());
    finished.store(true, Ordering::Relaxed);
    let _ = printer.join();
    let path = result?;

    println!(
        "PDFium is ready: {} ({}).",
        path.display(),
        crate::llm::human_size(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0))
    );
    if destination(args)?.is_some() {
        println!("evo finds a library beside its own binary without being told.");
    }
    Ok(())
}

fn percent(status: &DownloadStatus) -> Option<u64> {
    (status.total > 0)
        .then(|| (u128::from(status.received) * 100 / u128::from(status.total)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock file is the single source of truth for three consumers, so a
    /// typo in it has to fail here rather than in a release workflow.
    #[test]
    fn the_lock_file_pins_a_version_a_url_and_three_checksums() {
        let lock = &*LOCK;
        assert!(
            lock.version.chars().all(|c| c.is_ascii_digit()),
            "{:?}",
            lock.version
        );
        assert_eq!(lock.tag, format!("chromium/{}", lock.version));
        assert!(lock.base_url.starts_with("https://"), "{}", lock.base_url);
        assert!(
            lock.base_url.contains(lock.version),
            "the URL and the version disagree: {}",
            lock.base_url
        );
        for sha in [lock.mac_univ, lock.linux_x64, lock.win_x64] {
            assert_eq!(sha.len(), 64, "{sha} is not a sha256");
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{sha}");
        }
        assert_ne!(lock.mac_univ, lock.linux_x64);
    }

    /// Whatever platform the tests are running on, its archive has to name a
    /// member that ends in the library this platform loads.
    #[test]
    fn this_platform_has_an_archive_naming_the_library_it_loads() {
        let Some(asset) = asset() else {
            // A platform evo does not pin still has to explain itself.
            let message = unsupported_platform();
            assert!(message.contains("EVO_PDFIUM_PATH"), "{message}");
            return;
        };
        assert!(asset.file.ends_with(".tgz"), "{}", asset.file);
        assert!(
            asset.member.ends_with(library_file_name()),
            "{} does not end in {}",
            asset.member,
            library_file_name()
        );
        assert!(asset.url().starts_with("https://"), "{}", asset.url());
        assert!(asset.url().ends_with(asset.file), "{}", asset.url());
        assert_eq!(asset.sha256.len(), 64);
    }

    #[test]
    fn the_install_directory_is_versioned() {
        let Some(dir) = pdfium_data_dir() else { return };
        assert!(dir.ends_with(version()), "{}", dir.display());
        assert!(
            dir.to_string_lossy().contains("pdfium"),
            "{}",
            dir.display()
        );
    }

    /// `evo fetch-pdfium` is typed once, over SSH, by somebody who has just
    /// built evo on a server -- so a wrong argument has to print the usage.
    #[test]
    fn the_command_line_takes_nothing_or_an_into_directory() {
        let args = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        assert_eq!(destination(&[]).expect("the default"), None);
        assert_eq!(
            destination(&args(&["--into", "target/debug"])).expect("--into"),
            Some(PathBuf::from("target/debug"))
        );

        let help = destination(&args(&["--help"])).expect_err("usage is not a directory");
        assert!(help.starts_with("usage: evo fetch-pdfium"), "{help}");
        assert!(help.contains("--into"), "{help}");

        let wrong = destination(&args(&["target"])).expect_err("a bare path is not a flag");
        assert!(wrong.contains("--into"), "{wrong}");
        let two = destination(&args(&["--into", "a", "b"])).expect_err("one directory");
        assert!(two.contains("--into"), "{two}");
    }

    /// An archive that does not hold the library must be an error naming the
    /// file it looked for, not a silent success.
    #[test]
    fn unpacking_an_archive_without_the_library_says_so() {
        let dir = std::env::temp_dir().join(format!("evo-pdfium-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let archive = dir.join("empty.tgz");
        {
            let file = File::create(&archive).expect("create");
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_cksum();
            builder
                .append_data(&mut header.clone(), "LICENSE", &b"BSD"[..])
                .expect("append");
            builder.into_inner().expect("tar").finish().expect("gzip");
        }

        let err = unpack(&archive, "lib/libpdfium.so", &dir.join("libpdfium.so"))
            .expect_err("no library inside");
        assert!(err.contains("lib/libpdfium.so"), "{err}");

        // ...and one that does hold it puts it where it was asked to.
        let archive = dir.join("full.tgz");
        {
            let file = File::create(&archive).expect("create");
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(6);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "lib/libpdfium.so", &b"ELF..."[..])
                .expect("append");
            builder.into_inner().expect("tar").finish().expect("gzip");
        }
        let dest = dir.join("libpdfium.so");
        assert_eq!(
            unpack(&archive, "lib/libpdfium.so", &dest).expect("unpacked"),
            dest
        );
        assert_eq!(std::fs::read(&dest).expect("read"), b"ELF...");
        assert!(
            !dest.with_extension("part").exists(),
            "a part file was left"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_in_flight_before_anything_starts() {
        let fetch = PdfiumFetch::default();
        assert!(!fetch.in_flight());
        assert_eq!(fetch.status(), None);
    }

    /// The real thing: download the pinned archive for this platform, check it
    /// against the checksum in the lock file, and unpack the library. Ignored
    /// by default because it is several megabytes over the network.
    #[test]
    #[ignore = "downloads several MB; run with --ignored"]
    fn pdfium_downloads_and_unpacks_the_pinned_release() {
        let dir = std::env::temp_dir().join(format!("evo-pdfium-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let status = Arc::new(Mutex::new(DownloadStatus::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let path = install(&dir, &status, &cancel, &egui::Context::default()).expect("install");

        assert_eq!(path, dir.join(library_file_name()));
        let size = std::fs::metadata(&path).expect("metadata").len();
        assert!(size > 4 << 20, "a {size}-byte PDFium is not a PDFium");
        assert!(
            !dir.join(asset().unwrap().file).exists(),
            "the archive was left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
