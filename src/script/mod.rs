//! Lua scripting: run a script over the open document, have a local model
//! write something from it, and get the result back as a new PDF.
//!
//! The VM lives on its own thread and never touches `DocState`, the library or
//! egui. It is handed a snapshot of the document's bytes and returns finished
//! PDFs as byte vectors; the UI thread does the importing. That keeps every
//! question about sharing app state across threads from arising at all.

pub mod api;
pub mod docgen;
pub mod model;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use model::ModelConfig;

/// Scripting preferences, persisted with the rest.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ScriptPrefs {
    #[serde(default)]
    pub model: ModelConfig,
    /// Wall-clock ceiling for one run.
    #[serde(default = "default_deadline")]
    pub deadline_secs: u64,
}

fn default_deadline() -> u64 {
    300
}

impl Default for ScriptPrefs {
    fn default() -> Self {
        Self {
            model: ModelConfig::default(),
            deadline_secs: default_deadline(),
        }
    }
}

/// What the script may know about the open document. Taken on the UI thread
/// before the run starts; the source `Arc` makes it cheap.
#[derive(Clone)]
pub struct DocSnapshot {
    pub title: String,
    pub source: Arc<Vec<u8>>,
    pub page_count: usize,
}

/// A document a script produced, waiting for the UI thread to import it.
#[derive(Clone)]
pub struct GeneratedDoc {
    pub title: String,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for GeneratedDoc {
    /// The bytes are a whole PDF; printing them in a test failure helps nobody.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedDoc")
            .field("title", &self.title)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Default)]
pub struct ScriptStatus {
    /// Name of the running script, if any.
    pub running: Option<String>,
    pub log: Vec<String>,
    /// Set once when a run ends: the documents it made, or why it failed.
    pub outcome: Option<Result<Vec<GeneratedDoc>, String>>,
}

impl ScriptStatus {
    fn log(&mut self, line: impl Into<String>) {
        // A runaway script logging in a loop shouldn't grow without bound.
        const MAX_LINES: usize = 500;
        self.log.push(line.into());
        if self.log.len() > MAX_LINES {
            self.log.drain(..self.log.len() - MAX_LINES);
        }
    }
}

struct Job {
    name: String,
    source: String,
    doc: Option<DocSnapshot>,
    prefs: ScriptPrefs,
    /// The MCP servers this run may use, if the user ticked the box for it.
    /// `None` is the ordinary case and the default.
    mcp: Option<Arc<dyn crate::mcp::McpAccess>>,
}

pub struct ScriptEngine {
    tx: Sender<Job>,
    status: Arc<Mutex<ScriptStatus>>,
    cancel: Arc<AtomicBool>,
}

impl ScriptEngine {
    pub fn spawn(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel::<Job>();
        let status = Arc::new(Mutex::new(ScriptStatus::default()));
        let cancel = Arc::new(AtomicBool::new(false));

        let worker_status = status.clone();
        let worker_cancel = cancel.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("evo-script".into())
            .spawn(move || worker(rx, worker_status, worker_cancel, ctx))
            .expect("failed to spawn the script thread");

        Self { tx, status, cancel }
    }

    pub fn run(
        &self,
        name: String,
        source: String,
        doc: Option<DocSnapshot>,
        prefs: ScriptPrefs,
        mcp: Option<Arc<dyn crate::mcp::McpAccess>>,
    ) {
        self.cancel.store(false, Ordering::Relaxed);
        {
            let mut status = self.status.lock().unwrap();
            status.running = Some(name.clone());
            status.log.clear();
            status.outcome = None;
        }
        let _ = self.tx.send(Job {
            name,
            source,
            doc,
            prefs,
            mcp,
        });
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.status.lock().unwrap().running.is_some()
    }

    pub fn with_status<T>(&self, f: impl FnOnce(&ScriptStatus) -> T) -> T {
        f(&self.status.lock().unwrap())
    }

    /// Take the finished run's result, if one is waiting.
    pub fn take_outcome(&self) -> Option<Result<Vec<GeneratedDoc>, String>> {
        self.status.lock().unwrap().outcome.take()
    }
}

fn worker(
    rx: Receiver<Job>,
    status: Arc<Mutex<ScriptStatus>>,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    while let Ok(job) = rx.recv() {
        status.lock().unwrap().log(format!("Running {}…", job.name));
        let deadline = Instant::now() + Duration::from_secs(job.prefs.deadline_secs.max(1));
        let outcome = api::run(
            &job.source,
            job.doc,
            &job.prefs,
            job.mcp.clone(),
            &status,
            &cancel,
            deadline,
            &ctx,
        );
        {
            let mut s = status.lock().unwrap();
            match &outcome {
                Ok(docs) => s.log(format!(
                    "Finished: {} document{} generated.",
                    docs.len(),
                    if docs.len() == 1 { "" } else { "s" }
                )),
                Err(e) => s.log(format!("Failed: {e}")),
            }
            s.running = None;
            s.outcome = Some(outcome);
        }
        ctx.request_repaint();
    }
}

/// Where user scripts live.
pub fn scripts_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "evo")?;
    Some(dirs.data_dir().join("scripts"))
}

/// Create the scripts directory, seeding the examples on first use.
pub fn ensure_scripts_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let empty = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .all(|e| e.path().extension().is_none_or(|x| x != "lua"));
    if empty {
        for (name, body) in EXAMPLES {
            std::fs::write(dir.join(name), body)?;
        }
    }
    Ok(())
}

/// Shipped examples, written out when the scripts folder is first created.
/// They double as the API documentation.
pub const EXAMPLES: [(&str, &str); 3] = [
    ("summarize.lua", include_str!("examples/summarize.lua")),
    ("outline.lua", include_str!("examples/outline.lua")),
    (
        "action-items.lua",
        include_str!("examples/action-items.lua"),
    ),
];

/// The `.lua` files in `dir`, sorted by name.
pub fn list_scripts(dir: &Path) -> Vec<PathBuf> {
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .collect();
    scripts.sort();
    scripts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_does_not_grow_without_bound() {
        let mut status = ScriptStatus::default();
        for i in 0..2000 {
            status.log(format!("line {i}"));
        }
        assert!(status.log.len() <= 500);
        // The tail is what's kept: the end of a run is the interesting part.
        assert_eq!(status.log.last().unwrap(), "line 1999");
    }

    #[test]
    fn examples_are_written_once_then_left_alone() {
        let dir = tempdir();
        ensure_scripts_dir(&dir).expect("create");
        assert_eq!(list_scripts(&dir).len(), EXAMPLES.len());

        // An edited example is not overwritten, and a deleted one stays gone.
        std::fs::write(dir.join("summarize.lua"), "-- mine now").expect("write");
        std::fs::remove_file(dir.join("outline.lua")).expect("remove");
        ensure_scripts_dir(&dir).expect("create again");

        assert_eq!(
            std::fs::read_to_string(dir.join("summarize.lua")).unwrap(),
            "-- mine now"
        );
        assert!(!dir.join("outline.lua").exists());
    }

    #[test]
    fn only_lua_files_are_listed() {
        let dir = tempdir();
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::write(dir.join("b.lua"), "").expect("write");
        std::fs::write(dir.join("a.lua"), "").expect("write");
        std::fs::write(dir.join("notes.txt"), "").expect("write");
        let names: Vec<_> = list_scripts(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.lua", "b.lua"]);
    }

    #[test]
    fn listing_a_missing_directory_is_empty_rather_than_an_error() {
        assert!(list_scripts(Path::new("/nope/not/here")).is_empty());
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "evo-script-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
}
