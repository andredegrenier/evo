//! The built-in language model: a small GGUF model run in process by
//! llama.cpp.
//!
//! Weights are never bundled -- they are far larger than the binary and carry
//! their own licences -- so this module is mostly about the *catalogue*: which
//! models evo knows how to fetch, where they live on disk, and the checksums
//! that say a download arrived intact. [`download`] does the fetching and
//! [`backend`] (only compiled with the `builtin-llm` feature) does the
//! generating.
//!
//! Each catalogue entry lists two independent sources. They are separate
//! quantizations by different people, so each carries its own checksum and
//! size: whichever one answers, the file is verified against *that* source.

pub mod download;

#[cfg(feature = "builtin-llm")]
pub mod backend;

use std::path::{Path, PathBuf};

/// One place to download a model file from, with the checksum of the file that
/// particular uploader produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelSource {
    pub url: &'static str,
    /// Lowercase hex SHA-256, taken from the Hugging Face LFS pointer.
    pub sha256: &'static str,
    pub size: u64,
}

/// A model evo knows how to download and run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Stable identifier, stored in preferences.
    pub id: &'static str,
    pub label: &'static str,
    /// What the file is called once it is on disk. Deliberately evo's own
    /// name, not the uploader's, since the sources disagree about that.
    pub filename: &'static str,
    /// Mirrors, tried in order.
    pub sources: [ModelSource; 2],
    /// A sentence for the Preferences pane.
    pub summary: &'static str,
    pub license: &'static str,
    /// Where the model card (and its licence) can be read.
    pub attribution: &'static str,
    pub attribution_url: &'static str,
}

impl CatalogEntry {
    /// Roughly how large the download is, for the UI. The mirrors differ by a
    /// few hundred bytes; the first one is what will normally be fetched.
    pub fn size(&self) -> u64 {
        self.sources[0].size
    }

    pub fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(self.filename)
    }

    /// The file, if it has been downloaded.
    pub fn installed_in(&self, dir: &Path) -> Option<PathBuf> {
        let path = self.path_in(dir);
        path.is_file().then_some(path)
    }
}

/// The models offered in Preferences. The first is the default. A `static`
/// rather than a `const` so a row in the UI can hand a `&'static CatalogEntry`
/// to a download thread.
pub static CATALOG: [CatalogEntry; 2] = [
    CatalogEntry {
        id: "qwen3-4b-instruct-2507",
        label: "Qwen3 4B Instruct 2507",
        filename: "qwen3-4b-instruct-2507-q4_k_m.gguf",
        sources: [
            ModelSource {
                url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
                sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
                size: 2_497_281_120,
            },
            ModelSource {
                url: "https://huggingface.co/bartowski/Qwen_Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen_Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
                sha256: "2fde00ce69dd4899c70d020845e2638353015bba0fdf161b3eb965f2bca4464e",
                size: 2_497_280_736,
            },
        ],
        summary: "The default. Answers straight away, without thinking out loud.",
        license: "Apache-2.0",
        attribution: "Qwen3-4B-Instruct-2507 © Alibaba Cloud, Apache-2.0",
        attribution_url: "https://huggingface.co/Qwen/Qwen3-4B-Instruct-2507",
    },
    CatalogEntry {
        id: "qwen3-1.7b",
        label: "Qwen3 1.7B",
        filename: "qwen3-1.7b-q4_k_m.gguf",
        sources: [
            ModelSource {
                url: "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q4_K_M.gguf",
                sha256: "b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897",
                size: 1_107_409_472,
            },
            ModelSource {
                url: "https://huggingface.co/bartowski/Qwen_Qwen3-1.7B-GGUF/resolve/main/Qwen_Qwen3-1.7B-Q4_K_M.gguf",
                sha256: "72c5c3cb38fa32d5256e2fe30d03e7a64c6c79e668ad84057e3bd66e250b24fb",
                size: 1_282_439_584,
            },
        ],
        summary: "Less than half the size, for older machines. It reasons out \
                  loud before answering, so replies begin with its thinking.",
        license: "Apache-2.0",
        attribution: "Qwen3-1.7B © Alibaba Cloud, Apache-2.0",
        attribution_url: "https://huggingface.co/Qwen/Qwen3-1.7B",
    },
];

/// The catalogue's default model id, used by preferences that predate the
/// setting.
pub const DEFAULT_MODEL: &str = "qwen3-4b-instruct-2507";

pub fn entry(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// Where downloaded weights live. Path only -- nothing is created until
/// something is actually downloaded.
pub fn llm_models_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "evo")?;
    Some(dirs.data_dir().join("library").join("models").join("llm"))
}

/// Total bytes the downloaded models take up.
pub fn disk_usage(dir: &Path) -> u64 {
    CATALOG
        .iter()
        .filter_map(|e| std::fs::metadata(e.path_in(dir)).ok())
        .map(|m| m.len())
        .sum()
}

/// Let go of any weights held in memory. Both a courtesy (they are gigabytes)
/// and a necessity: see [`backend::unload`].
pub fn unload() {
    #[cfg(feature = "builtin-llm")]
    backend::unload();
}

/// Remove a downloaded model (and any half-finished download of it).
pub fn delete_model(dir: &Path, entry: &CatalogEntry) -> std::io::Result<()> {
    // The file may be mapped into this process; on Windows that alone would
    // stop it being deleted.
    unload();
    let path = entry.path_in(dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let _ = std::fs::remove_file(path.with_extension("part"));
    Ok(())
}

/// Why the built-in model cannot answer, if it cannot. The UI asks before
/// submitting a job so that "you have not downloaded it yet" arrives as a
/// sentence in front of the user instead of a failure from a worker thread.
pub fn unavailable_reason(model_id: &str) -> Option<String> {
    if !cfg!(feature = "builtin-llm") {
        return Some(
            "This build of evo has no built-in model. Preferences ▸ Model can \
             point it at a local server instead."
                .to_owned(),
        );
    }
    let Some(entry) = entry(model_id) else {
        return Some(format!(
            "“{model_id}” is not one of the built-in models. Pick one in \
             Preferences ▸ Model."
        ));
    };
    let Some(dir) = llm_models_dir() else {
        return Some("evo could not find a data directory to keep models in.".to_owned());
    };
    if entry.installed_in(&dir).is_none() {
        return Some(format!(
            "{} has not been downloaded yet ({}). Preferences ▸ Model will \
             fetch it.",
            entry.label,
            human_size(entry.size())
        ));
    }
    None
}

/// Bytes as the Preferences pane says them: "2.5 GB".
pub fn human_size(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{bytes} bytes")
    }
}

/// ChatML, written out by hand. Used when a GGUF carries no chat template of
/// its own; every model evo ships in the catalogue speaks it, and a model that
/// does not is better served by its own template.
/// Compiled whether or not the engine is, so it is always under test.
#[cfg_attr(not(feature = "builtin-llm"), allow(dead_code))]
pub fn chatml_prompt(messages: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (role, content) in messages {
        out.push_str("<|im_start|>");
        out.push_str(role);
        out.push('\n');
        out.push_str(content);
        out.push_str("<|im_end|>\n");
    }
    out.push_str("<|im_start|>assistant\n");
    out
}

/// Drop about `cut` bytes from the front of `text`, marking the cut with an
/// ellipsis. Used to fit a prompt into the context window: the quoted pages
/// come first and the question last, so what goes is the least of the evidence
/// rather than the question itself.
#[cfg_attr(not(feature = "builtin-llm"), allow(dead_code))]
pub fn truncate_front(text: &str, cut: usize) -> String {
    if cut == 0 {
        return text.to_owned();
    }
    if cut >= text.len() {
        return String::new();
    }
    // Land on a character boundary, then on a word boundary if one is close.
    let mut start = cut;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let rest = &text[start..];
    let rest = match rest.find(char::is_whitespace) {
        Some(i) if i < 40 => &rest[i + 1..],
        _ => rest,
    };
    format!("…{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_entries_are_distinct_and_completely_filled_in() {
        for e in CATALOG {
            assert!(!e.id.is_empty(), "an entry has no id");
            assert!(!e.label.is_empty(), "{} has no label", e.id);
            assert!(
                e.filename.ends_with(".gguf"),
                "{} is not a gguf: {}",
                e.id,
                e.filename
            );
            assert!(!e.summary.is_empty(), "{} has no summary", e.id);
            assert!(
                e.attribution_url.starts_with("https://"),
                "{} needs an https attribution url",
                e.id
            );
            assert!(!e.license.is_empty(), "{} has no licence", e.id);
        }
        for (i, a) in CATALOG.iter().enumerate() {
            for b in &CATALOG[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate id");
                assert_ne!(a.filename, b.filename, "two entries share a file");
            }
        }
    }

    /// The checksums are what stands between a truncated or tampered download
    /// and a crash inside llama.cpp, so they must look like real digests.
    #[test]
    fn every_source_carries_a_plausible_digest_and_size() {
        for e in CATALOG {
            for s in e.sources {
                assert!(
                    s.url.starts_with("https://huggingface.co/"),
                    "{}: {}",
                    e.id,
                    s.url
                );
                assert!(s.url.contains("/resolve/"), "{}: not a download url", e.id);
                assert_eq!(s.sha256.len(), 64, "{}: {} is not a sha256", e.id, s.sha256);
                assert!(
                    s.sha256
                        .chars()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                    "{}: {} is not lowercase hex",
                    e.id,
                    s.sha256
                );
                // Any real quantization of these models is between 0.5 and 5 GB;
                // a placeholder would not be.
                assert!(
                    (500_000_000..5_000_000_000).contains(&s.size),
                    "{}: implausible size {}",
                    e.id,
                    s.size
                );
            }
            // Two uploaders, two different files.
            assert_ne!(e.sources[0].sha256, e.sources[1].sha256, "{}", e.id);
            assert_ne!(e.sources[0].url, e.sources[1].url, "{}", e.id);
        }
    }

    #[test]
    fn the_default_model_is_in_the_catalog() {
        assert!(entry(DEFAULT_MODEL).is_some());
        assert_eq!(CATALOG[0].id, DEFAULT_MODEL, "the default comes first");
        assert!(entry("no-such-model").is_none());
    }

    #[test]
    fn sizes_read_the_way_a_download_prompt_should() {
        assert_eq!(human_size(2_497_281_120), "2.5 GB");
        assert_eq!(human_size(1_107_409_472), "1.1 GB");
        assert_eq!(human_size(10_500_000), "10 MB");
        assert_eq!(human_size(512), "512 bytes");
    }

    #[test]
    fn chatml_ends_with_an_open_assistant_turn() {
        let prompt = chatml_prompt(&[("system", "be brief"), ("user", "hello")]);
        assert_eq!(
            prompt,
            "<|im_start|>system\nbe brief<|im_end|>\n\
             <|im_start|>user\nhello<|im_end|>\n\
             <|im_start|>assistant\n"
        );
    }

    #[test]
    fn truncating_takes_from_the_front_and_says_so() {
        let text = "alpha beta gamma delta";
        let cut = truncate_front(text, 6);
        assert!(cut.starts_with('…'));
        assert!(cut.ends_with("delta"));
        assert!(!cut.contains("alpha"));

        assert_eq!(truncate_front(text, 0), text);
        assert!(truncate_front(text, text.len()).is_empty());
    }

    /// Cutting inside a multi-byte character must not panic or produce
    /// mojibake.
    #[test]
    fn truncating_respects_character_boundaries() {
        let text = "ééééé rest";
        for cut in 1..text.len() {
            let out = truncate_front(text, cut);
            assert!(out.is_empty() || out.starts_with('…'), "{cut}: {out:?}");
        }
    }

    #[test]
    fn disk_usage_counts_only_what_is_there() {
        let dir = std::env::temp_dir().join(format!("evo-llm-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert_eq!(disk_usage(&dir), 0);

        std::fs::write(CATALOG[0].path_in(&dir), b"0123456789").expect("write");
        std::fs::write(dir.join("unrelated.bin"), b"ignored").expect("write");
        assert_eq!(disk_usage(&dir), 10);
        assert!(CATALOG[0].installed_in(&dir).is_some());
        assert!(CATALOG[1].installed_in(&dir).is_none());

        delete_model(&dir, &CATALOG[0]).expect("delete");
        assert_eq!(disk_usage(&dir), 0);
        // Deleting what is not there is not an error.
        delete_model(&dir, &CATALOG[0]).expect("delete again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
