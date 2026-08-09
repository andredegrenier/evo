//! Where the PDFs come from.
//!
//! Corpora are *downloaded, never committed*: the licences differ per file,
//! the sizes run to hundreds of megabytes, and a git history full of other
//! people's test suites is a git history nobody can clone. What is committed
//! is the manifest -- a pinned commit, a list of paths, and a SHA-256 per file
//! -- so a run either gets exactly the bytes the baseline was blessed against
//! or stops and says so.
//!
//! Files are fetched one by one from `raw.githubusercontent.com` at a pinned
//! commit rather than as a repository zip, because a zip GitHub regenerates on
//! demand is not guaranteed to be byte-identical over time while a blob at a
//! commit is, and because a curated subset of 2,694 conformance files is what
//! keeps a run to a couple of minutes.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// A corpus as checked in: what it is, where it came from, and what is in it.
#[derive(Deserialize)]
pub struct Manifest {
    pub name: String,
    pub title: String,
    pub license: String,
    pub source: String,
    /// The commit the paths and hashes were taken at, for corpora that have
    /// one. Also the cache directory, so two pins never mix.
    #[serde(default)]
    pub commit: Option<String>,
    /// Prefix for downloads. Absent means the files are already in the
    /// repository (evo's own fixtures), and `path` is relative to its root.
    #[serde(default)]
    pub base_url: Option<String>,
    pub files: Vec<Entry>,
}

#[derive(Deserialize)]
pub struct Entry {
    pub path: String,
    /// Absent for files that live in the repository: git is their checksum.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Corpora may include documents that only open with a password. evo's
    /// encrypted fixtures do, and the harness should exercise them rather than
    /// count them as unreadable.
    #[serde(default)]
    pub password: Option<String>,
}

/// The corpora the harness knows about, embedded so that a run never has to
/// guess where the manifests are.
const MANIFESTS: &[(&str, &str)] = &[
    ("fixtures", include_str!("../../corpus/fixtures.json")),
    ("verapdf", include_str!("../../corpus/verapdf.json")),
];

pub fn names() -> Vec<&'static str> {
    MANIFESTS.iter().map(|(name, _)| *name).collect()
}

pub fn load(name: &str) -> Result<Manifest, String> {
    let (_, json) = MANIFESTS
        .iter()
        .find(|(known, _)| *known == name)
        .ok_or_else(|| format!("no corpus called {name:?}; try one of {:?}", names()))?;
    serde_json::from_str(json).map_err(|e| format!("corpus {name} is not valid JSON: {e}"))
}

/// Where downloads live. `EVO_FIDELITY_CACHE` overrides, which is what CI
/// uses to put the cache somewhere it can restore between runs.
pub fn cache_root() -> Result<PathBuf, String> {
    if let Some(from_env) = std::env::var_os("EVO_FIDELITY_CACHE") {
        return Ok(PathBuf::from(from_env));
    }
    let dirs = directories::ProjectDirs::from("", "", "evo-fidelity")
        .ok_or("this machine has no home directory to cache corpora in")?;
    Ok(dirs.cache_dir().to_path_buf())
}

impl Manifest {
    /// The bytes of one entry: read from the repository, from the cache, or
    /// from the network -- and hashed every time, because a corrupt cache
    /// entry would otherwise look exactly like a rendering regression.
    pub fn bytes(&self, entry: &Entry, repo: &Path) -> Result<Vec<u8>, String> {
        let Some(base_url) = &self.base_url else {
            let path = repo.join(&entry.path);
            return std::fs::read(&path)
                .map_err(|e| format!("could not read {}: {e}", path.display()));
        };

        let expected = entry
            .sha256
            .as_deref()
            .ok_or_else(|| format!("{} is downloaded but has no sha256", entry.path))?;
        let cached = cache_root()?
            .join(&self.name)
            .join(self.commit.as_deref().unwrap_or("head"))
            .join(&entry.path);
        if let Ok(bytes) = std::fs::read(&cached)
            && hex(&bytes) == expected
        {
            return Ok(bytes);
        }

        let url = format!("{base_url}{}", url_escape(&entry.path));
        let bytes = get(&url)?;
        let got = hex(&bytes);
        if got != expected {
            return Err(format!(
                "{url}\n  expected sha256 {expected}\n  got      sha256 {got}\n  \
                 The corpus has moved under the pin. Refusing to measure against \
                 bytes the baseline never saw."
            ));
        }
        if let Some(parent) = cached.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        std::fs::write(&cached, &bytes)
            .map_err(|e| format!("could not write {}: {e}", cached.display()))?;
        Ok(bytes)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Percent-encode everything that is not safe in a path segment, leaving the
/// separators alone. Corpus paths are full of spaces and parentheses.
fn url_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// One small file over HTTPS. Corpus documents are kilobytes to a couple of
/// megabytes; anything larger than the cap is not a test PDF.
fn get(url: &str) -> Result<Vec<u8>, String> {
    const CAP: u64 = 64 << 20;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .map_err(|e| format!("could not reach {url}: {e}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{url} returned {status}"));
    }
    response
        .into_body()
        .with_config()
        .limit(CAP)
        .read_to_vec()
        .map_err(|e| format!("could not read {url}: {e}"))
}
