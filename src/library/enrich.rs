//! The `evo-enrich` background worker: a summary and a handful of tags for
//! every document in the library, written by the same local model that answers
//! chat.
//!
//! It is deliberately *not* part of the indexer. Extraction and OCR are
//! measured in milliseconds a page and everything waits on them; a model takes
//! seconds a document and nothing should. So this is a second worker fed from
//! two places: the indexer announces each document as it finishes reading it,
//! and a reconciliation pass picks up everything that has no summary yet (at
//! startup, and again whenever the setting is switched on).
//!
//! It never writes to the search index itself. tantivy has exactly one writer
//! and it belongs to the indexer thread, so enrichment stores what it produced
//! in the metadata store and asks the indexer for an [`IndexJob::Meta`].
//!
//! The whole feature is off until asked for: it reads every document you own
//! through a language model, which is a decision for the user to make and not
//! a default to discover.

use std::collections::{HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::DocMeta;
use super::indexer::{IndexJob, Indexer};
use super::search::SearchIndex;
use super::store::MetaDb;
use crate::script::model::{GenerateRequest, ModelConfig};

/// Assistant features that cost something the user did not ask for -- time,
/// electricity, and every document going through a model. Persisted under
/// `"assistant_prefs"`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct AssistantPrefs {
    /// Write a summary and tags for each library document. Off by default.
    #[serde(default)]
    pub enrich_enabled: bool,
}

/// How many opening pages are read for a summary.
const PAGES: usize = 20;
/// How much of that text the model is shown.
const INPUT_CHARS: usize = 6000;
/// Long enough for two sentences, short enough for a library card.
const MAX_SUMMARY_CHARS: usize = 400;
const MAX_TAGS: usize = 5;
const MAX_TAG_CHARS: usize = 24;

/// Little invention wanted: this is a description, not a composition.
const TEMPERATURE: f32 = 0.2;
const MAX_TOKENS: u32 = 256;

/// Asking the worker to look for anything unsummarized. Document ids are hex
/// digests, so the empty string cannot collide with one.
const RESCAN: &str = "";

#[derive(Default, Clone)]
pub struct EnrichStatus {
    /// Documents queued or in flight.
    pub pending: usize,
    /// Title being summarized right now.
    pub current: Option<String>,
    /// Document id being summarized right now.
    pub current_id: Option<String>,
    /// Summaries written this session.
    pub done: usize,
    pub last_error: Option<String>,
}

pub struct Enricher {
    /// The same channel the indexer announces finished documents on; also how
    /// a reconciliation pass is asked for.
    tx: Sender<String>,
    status: Arc<Mutex<EnrichStatus>>,
    enabled: Arc<AtomicBool>,
    config: Arc<Mutex<ModelConfig>>,
}

impl Enricher {
    /// Spawn the worker. `tx`/`rx` are the two ends of the channel the indexer
    /// also holds a sender for.
    pub fn spawn(
        index_dir: PathBuf,
        db: Arc<MetaDb>,
        indexer: Arc<Indexer>,
        tx: Sender<String>,
        rx: Receiver<String>,
        ctx: eframe::egui::Context,
    ) -> Self {
        let status = Arc::new(Mutex::new(EnrichStatus::default()));
        let enabled = Arc::new(AtomicBool::new(false));
        let config = Arc::new(Mutex::new(ModelConfig::default()));

        let worker = Worker {
            index_dir,
            db,
            indexer,
            status: status.clone(),
            enabled: enabled.clone(),
            config: config.clone(),
            ctx,
            index: None,
            attempted: HashSet::new(),
        };
        std::thread::Builder::new()
            .name("evo-enrich".into())
            .spawn(move || worker.run(rx))
            .expect("failed to spawn the enrichment thread");

        Self {
            tx,
            status,
            enabled,
            config,
        }
    }

    /// Whether enrichment may run, and which model it should use. Switching it
    /// on starts a pass over every document that has no summary yet.
    pub fn configure(&self, enabled: bool, model: &ModelConfig) {
        if let Ok(mut config) = self.config.lock()
            && *config != *model
        {
            *config = model.clone();
        }
        let was = self.enabled.swap(enabled, Ordering::Relaxed);
        if enabled && !was {
            let _ = self.tx.send(RESCAN.to_owned());
        }
    }

    pub fn status(&self) -> EnrichStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn clear_error(&self) {
        if let Ok(mut st) = self.status.lock() {
            st.last_error = None;
        }
    }
}

struct Worker {
    index_dir: PathBuf,
    db: Arc<MetaDb>,
    indexer: Arc<Indexer>,
    status: Arc<Mutex<EnrichStatus>>,
    enabled: Arc<AtomicBool>,
    config: Arc<Mutex<ModelConfig>>,
    ctx: eframe::egui::Context,
    /// Opened on first use: the indexer creates the index, and at startup it
    /// may not have done so yet.
    index: Option<SearchIndex>,
    /// Documents tried this session. A model that cannot produce usable JSON
    /// for a document will not produce it on the tenth attempt either, and the
    /// indexer announces the same document again on every re-index.
    attempted: HashSet<String>,
}

impl Worker {
    fn run(mut self, rx: Receiver<String>) {
        let mut queue: VecDeque<String> = VecDeque::new();
        while let Ok(msg) = rx.recv() {
            self.take(msg, &mut queue);
            while let Ok(msg) = rx.try_recv() {
                self.take(msg, &mut queue);
            }
            while let Some(id) = queue.pop_front() {
                if !self.enabled.load(Ordering::Relaxed) {
                    queue.clear();
                    break;
                }
                self.set_pending(queue.len() + 1);
                if let Err(true) = self.enrich(&id) {
                    queue.clear();
                }
                // Stay responsive to arrivals during a slow generation.
                while let Ok(msg) = rx.try_recv() {
                    self.take(msg, &mut queue);
                }
            }
            self.finish(queue.len());
        }
    }

    /// Fold one message into the queue: a document id, or a request to look at
    /// everything. Nothing is queued while the feature is off -- switching it
    /// on scans afresh.
    fn take(&mut self, msg: String, queue: &mut VecDeque<String>) {
        if !self.enabled.load(Ordering::Relaxed) {
            queue.clear();
            return;
        }
        if msg == RESCAN {
            for meta in self.db.list_docs().unwrap_or_default() {
                if needs_enrich(&meta) && !queue.contains(&meta.id) {
                    queue.push_back(meta.id);
                }
            }
        } else if !queue.contains(&msg) {
            queue.push_back(msg);
        }
    }

    fn set_pending(&self, pending: usize) {
        if let Ok(mut st) = self.status.lock() {
            st.pending = pending;
        }
    }

    fn finish(&self, pending: usize) {
        if let Ok(mut st) = self.status.lock() {
            st.pending = pending;
            st.current = None;
            st.current_id = None;
        }
        self.ctx.request_repaint();
    }

    fn set_error(&self, msg: impl std::fmt::Display) {
        if let Ok(mut st) = self.status.lock() {
            st.last_error = Some(msg.to_string());
        }
        self.ctx.request_repaint();
    }

    /// Summarize one document. `Err(true)` means nothing else in the queue is
    /// worth trying either (no model to ask).
    fn enrich(&mut self, id: &str) -> Result<(), bool> {
        if self.attempted.contains(id) {
            return Ok(());
        }
        let Some(meta) = self.db.get_doc(id).ok().flatten() else {
            return Ok(());
        };
        if !needs_enrich(&meta) {
            return Ok(());
        }

        let config = self.config.lock().map(|c| c.clone()).unwrap_or_default();
        if config.api == crate::script::model::Api::Builtin
            && let Some(reason) = crate::llm::unavailable_reason(&config.builtin_model)
        {
            self.set_error(reason);
            return Err(true);
        }

        self.attempted.insert(id.to_owned());
        {
            let mut st = self.status.lock().unwrap();
            st.current = Some(meta.title.clone());
            st.current_id = Some(meta.id.clone());
        }
        self.ctx.request_repaint();

        let text = match self.opening_text(id) {
            Ok(text) => text,
            Err(e) => {
                self.set_error(e);
                return Ok(());
            }
        };
        if text.trim().is_empty() {
            // Nothing was read from it; there is nothing to describe.
            return Ok(());
        }

        let enrichment = match self.ask(&config, &meta.title, &text) {
            Ok(e) => e,
            Err(e) => {
                self.set_error(&e);
                // A model that is not there at all will not be there for the
                // next document either; a bad reply about one document says
                // nothing about the rest.
                return Err(is_fatal(&e));
            }
        };

        let tags = clamp_tags(&enrichment.tags, &meta.tags);
        let summary = clamp_summary(&enrichment.summary);
        if summary.is_empty() && tags.is_empty() {
            return Ok(());
        }
        if let Err(e) = self.db.update_enrichment(id, Some(summary.as_str()), &tags) {
            self.set_error(e);
            return Ok(());
        }
        // The indexer owns the writer; ask it to put this in the index.
        self.indexer.submit(IndexJob::Meta { id: id.to_owned() });
        if let Ok(mut st) = self.status.lock() {
            st.done += 1;
        }
        self.ctx.request_repaint();
        Ok(())
    }

    /// The opening pages, read back out of the index rather than out of the
    /// PDF: the indexer has already done the expensive part.
    fn opening_text(&mut self, id: &str) -> Result<String, super::LibraryError> {
        if self.index.is_none() {
            self.index = Some(SearchIndex::open_or_create(&self.index_dir)?);
        }
        let index = self.index.as_ref().expect("just opened");
        let pages = index.page_texts(id, 0..PAGES)?;
        Ok(opening_text(&pages))
    }

    fn ask(
        &self,
        config: &ModelConfig,
        title: &str,
        text: &str,
    ) -> Result<Enrichment, crate::script::model::ModelError> {
        // Switching the feature off stops what is in flight too.
        let mut keep_going = || self.enabled.load(Ordering::Relaxed);
        describe(config, title, text, &mut keep_going)
    }
}

/// Ask the model to describe one document: one generation, with a second,
/// sterner attempt if the first reply was not the JSON object it was asked
/// for. `keep_going` is polled as tokens arrive and abandons the request when
/// it answers false.
pub fn describe(
    config: &ModelConfig,
    title: &str,
    text: &str,
    keep_going: &mut dyn FnMut() -> bool,
) -> Result<Enrichment, crate::script::model::ModelError> {
    let backend = config.build();
    let mut last: Option<crate::script::model::ModelError> = None;
    for attempt in 0..2 {
        let request = GenerateRequest {
            model: config.model.clone(),
            prompt: user_prompt(title, text, attempt > 0),
            system: Some(SYSTEM_PROMPT.to_owned()),
            history: Vec::new(),
            temperature: Some(TEMPERATURE),
            max_tokens: Some(MAX_TOKENS),
        };
        let mut on_token = |_: &str| {
            if keep_going() {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        };
        let reply = backend.generate(&request, &mut on_token)?;
        match parse_enrichment(&reply) {
            Some(enrichment) => return Ok(enrichment),
            None => {
                last = Some(crate::script::model::ModelError::Read(format!(
                    "the model did not describe “{title}” in the shape that was asked for"
                )));
            }
        }
    }
    Err(last.expect("two attempts leave a reason"))
}

/// Is this failure about the model itself rather than about one document? If
/// it is, the rest of the queue is abandoned instead of failing the same way
/// once per document.
fn is_fatal(e: &crate::script::model::ModelError) -> bool {
    use crate::script::model::ModelError as E;
    matches!(
        e,
        E::Unreachable { .. } | E::Unavailable(_) | E::Cancelled | E::Status { .. }
    )
}

/// Has this document been read but not yet described? Pages still pending or
/// failed mean the text is incomplete, and a summary of half a document is
/// worse than none.
pub fn needs_enrich(meta: &DocMeta) -> bool {
    meta.summary.is_none()
        && !meta.text_status.is_empty()
        && meta.text_status.iter().all(|s| {
            matches!(
                s,
                super::PageTextStatus::Embedded | super::PageTextStatus::Ocr
            )
        })
}

/// What goes into the index as the document's meta document: the summary and
/// the tags, so a search for either finds the document itself.
pub fn meta_body(meta: &DocMeta) -> Option<String> {
    let tags = meta.all_tags();
    if meta.summary.is_none() && tags.is_empty() {
        return None;
    }
    let mut body = meta.summary.clone().unwrap_or_default();
    if !tags.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&tags.join(", "));
    }
    Some(body)
}

/// The opening pages as one block, cut to what the model is shown.
fn opening_text(pages: &[String]) -> String {
    let mut out = String::new();
    for page in pages {
        if page.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(page.trim());
        if out.chars().count() >= INPUT_CHARS {
            break;
        }
    }
    truncate_chars(&out, INPUT_CHARS)
}

const SYSTEM_PROMPT: &str = "You describe documents for a library catalogue. \
     You reply with one JSON object and nothing else: no explanation, no code \
     fence, no commentary.";

fn user_prompt(title: &str, text: &str, retry: bool) -> String {
    let mut prompt = String::new();
    if retry {
        prompt.push_str(
            "Your previous reply was not a JSON object. Reply with the object \
             alone this time.\n\n",
        );
    }
    prompt.push_str(&format!(
        "Here are the opening pages of a document titled “{title}”.\n\n\
         ---\n{text}\n---\n\n\
         Reply with exactly this shape:\n\
         {{\"summary\": \"one or two sentences saying what this document is\", \
         \"tags\": [\"topic\", \"topic\", \"topic\"]}}\n\n\
         The summary describes the document, not your reading of it. Tags are \
         at most {MAX_TAGS} short lowercase topics."
    ));
    prompt
}

/// What the model was asked for, before clamping.
#[derive(Debug, PartialEq)]
pub struct Enrichment {
    pub summary: String,
    pub tags: Vec<String>,
}

/// The first balanced `{…}` in the reply. Models like to wrap JSON in prose or
/// a code fence however plainly they were asked not to, so the object is
/// fished out rather than the whole reply parsed.
pub fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text[start..].char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + c.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Read a reply into a summary and tags, or decide it was not one.
pub fn parse_enrichment(reply: &str) -> Option<Enrichment> {
    let json = extract_json(reply)?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let summary = value["summary"].as_str().unwrap_or_default().trim();
    if summary.is_empty() {
        return None;
    }
    let tags = match &value["tags"] {
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect(),
        // A model that writes "tags": "one, two" meant the same thing.
        serde_json::Value::String(s) => s.split(',').map(|t| t.trim().to_owned()).collect(),
        _ => Vec::new(),
    };
    Some(Enrichment {
        summary: summary.to_owned(),
        tags,
    })
}

/// Tags as they are worth storing: short, lowercase, distinct, few, and never
/// a duplicate of one the user wrote themselves.
pub fn clamp_tags(raw: &[String], user_tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in raw {
        let cleaned: String = tag
            .trim()
            .trim_matches(|c: char| c == '#' || c == '"' || c == '\'' || c == '.')
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let cleaned = truncate_chars(&cleaned, MAX_TAG_CHARS);
        let cleaned = cleaned.trim().to_owned();
        if cleaned.is_empty() {
            continue;
        }
        if user_tags.iter().any(|t| t.eq_ignore_ascii_case(&cleaned)) {
            continue;
        }
        if out.iter().any(|t| t.eq_ignore_ascii_case(&cleaned)) {
            continue;
        }
        out.push(cleaned);
        if out.len() == MAX_TAGS {
            break;
        }
    }
    out
}

/// One paragraph of one or two sentences: whitespace collapsed, length capped.
pub fn clamp_summary(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_SUMMARY_CHARS {
        return collapsed;
    }
    format!(
        "{}…",
        truncate_chars(&collapsed, MAX_SUMMARY_CHARS).trim_end()
    )
}

/// The first `max` characters, counted as characters rather than bytes.
fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => text[..i].to_owned(),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::PageTextStatus;

    fn meta_with(statuses: Vec<PageTextStatus>, summary: Option<&str>) -> DocMeta {
        DocMeta {
            id: "abc".into(),
            title: "doc".into(),
            original_filename: "doc.pdf".into(),
            imported_at: 0,
            page_count: statuses.len(),
            file_size: 0,
            tags: Vec::new(),
            text_status: statuses,
            index_error: None,
            summary: summary.map(str::to_owned),
            auto_tags: Vec::new(),
        }
    }

    #[test]
    fn only_fully_read_documents_without_a_summary_are_enriched() {
        use PageTextStatus::*;
        assert!(needs_enrich(&meta_with(vec![Embedded, Ocr], None)));
        // Already described.
        assert!(!needs_enrich(&meta_with(vec![Embedded], Some("a report"))));
        // Still being read, or partly unreadable.
        assert!(!needs_enrich(&meta_with(vec![Embedded, Pending], None)));
        assert!(!needs_enrich(&meta_with(vec![Embedded, Failed], None)));
        // A pre-v0.3 record with no per-page state at all: the indexer will
        // back-fill it and announce it again.
        assert!(!needs_enrich(&meta_with(Vec::new(), None)));
    }

    #[test]
    fn the_meta_document_carries_the_summary_and_every_tag() {
        let mut meta = meta_with(vec![PageTextStatus::Embedded], Some("A boiler report."));
        meta.tags = vec!["mine".into()];
        meta.auto_tags = vec!["boiler".into(), "maintenance".into()];
        let body = meta_body(&meta).expect("a body");
        assert!(body.contains("A boiler report."));
        assert!(body.contains("mine") && body.contains("boiler"));

        // Nothing to say, nothing to index.
        assert!(meta_body(&meta_with(vec![PageTextStatus::Embedded], None)).is_none());
    }

    #[test]
    fn a_tag_the_user_already_wrote_is_not_repeated() {
        let mut meta = meta_with(vec![PageTextStatus::Embedded], None);
        meta.tags = vec!["Invoice".into()];
        meta.auto_tags = vec!["invoice".into(), "2026".into()];
        assert_eq!(meta.all_tags(), ["Invoice", "2026"]);
    }

    #[test]
    fn a_clean_reply_reads_straight_through() {
        let reply = r#"{"summary": "A safety manual.", "tags": ["safety", "manual"]}"#;
        assert_eq!(
            parse_enrichment(reply).expect("parsed"),
            Enrichment {
                summary: "A safety manual.".into(),
                tags: vec!["safety".into(), "manual".into()],
            }
        );
    }

    #[test]
    fn a_reply_wrapped_in_prose_or_a_code_fence_still_reads() {
        for reply in [
            "Sure! Here is the JSON:\n```json\n{\"summary\": \"A safety manual.\", \
             \"tags\": [\"safety\"]}\n```\nHope that helps.",
            "  {\"summary\":\"A safety manual.\",\"tags\":[\"safety\"]}  \n\n",
            "<think>The document seems to be a manual.</think>\
             {\"summary\": \"A safety manual.\", \"tags\": [\"safety\"]}",
        ] {
            let got = parse_enrichment(reply).expect(reply);
            assert_eq!(got.summary, "A safety manual.");
            assert_eq!(got.tags, ["safety"]);
        }
    }

    /// Braces inside the summary must not end the object early.
    #[test]
    fn braces_inside_strings_do_not_confuse_the_scanner() {
        let reply = r#"here: {"summary": "Covers the {n} placeholder syntax.", "tags": []} done"#;
        let got = parse_enrichment(reply).expect("parsed");
        assert_eq!(got.summary, "Covers the {n} placeholder syntax.");
        assert!(got.tags.is_empty());

        let escaped = r#"{"summary": "He said \"hi\" and left.", "tags": []}"#;
        assert_eq!(
            parse_enrichment(escaped).expect("parsed").summary,
            "He said \"hi\" and left."
        );
    }

    #[test]
    fn garbage_is_rejected_rather_than_half_read() {
        for reply in [
            "",
            "I am sorry, I cannot summarize that.",
            "{not json at all}",
            r#"{"summary": "", "tags": ["safety"]}"#,
            r#"{"tags": ["safety"]}"#,
            // Never closed: no object to take.
            r#"{"summary": "A manual", "tags": ["#,
        ] {
            assert!(parse_enrichment(reply).is_none(), "{reply:?}");
        }
    }

    #[test]
    fn tags_given_as_one_string_are_split() {
        let got = parse_enrichment(r#"{"summary": "A manual.", "tags": "safety, manual"}"#)
            .expect("parsed");
        assert_eq!(got.tags, ["safety", "manual"]);
    }

    #[test]
    fn tags_are_clamped_to_a_few_short_lowercase_distinct_ones() {
        let raw: Vec<String> = [
            "  Safety  ",
            "#safety",
            "SAFETY",
            "an extremely long tag that nobody would ever want to read on a card",
            "second   word",
            "invoice",
            "three",
            "four",
            "five",
            "six",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let user = vec!["Invoice".to_owned()];
        let tags = clamp_tags(&raw, &user);

        assert_eq!(tags.len(), MAX_TAGS);
        assert_eq!(tags[0], "safety");
        assert!(!tags.iter().any(|t| t.contains("invoice")), "{tags:?}");
        assert!(tags.iter().all(|t| t.chars().count() <= MAX_TAG_CHARS));
        assert!(tags.iter().all(|t| t.trim() == t && !t.is_empty()));
        assert_eq!(tags[1], "an extremely long tag th");
        assert_eq!(tags[2], "second word");
    }

    #[test]
    fn empty_and_punctuation_only_tags_are_dropped() {
        let raw: Vec<String> = ["", "   ", "\"\"", "#", "."]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(clamp_tags(&raw, &[]).is_empty());
    }

    #[test]
    fn a_summary_is_one_line_and_bounded() {
        assert_eq!(
            clamp_summary("  A report\non the   boiler.\n"),
            "A report on the boiler."
        );
        let long = "word ".repeat(400);
        let clamped = clamp_summary(&long);
        assert!(clamped.chars().count() <= MAX_SUMMARY_CHARS + 1);
        assert!(clamped.ends_with('…'));
        // Multi-byte characters must not be cut in half.
        let accents = "é".repeat(1000);
        assert!(clamp_summary(&accents).chars().count() <= MAX_SUMMARY_CHARS + 1);
    }

    #[test]
    fn the_opening_text_skips_blank_pages_and_is_capped() {
        let pages = vec!["first".to_owned(), "   ".to_owned(), "second".to_owned()];
        assert_eq!(opening_text(&pages), "first\n\nsecond");

        let long = vec!["x".repeat(INPUT_CHARS * 2)];
        assert_eq!(opening_text(&long).chars().count(), INPUT_CHARS);
        assert!(opening_text(&[]).is_empty());
    }

    #[test]
    fn the_retry_prompt_says_what_went_wrong() {
        let first = user_prompt("Manual", "text", false);
        let again = user_prompt("Manual", "text", true);
        assert!(first.contains("Manual") && first.contains("\"summary\""));
        assert!(!first.contains("previous reply"));
        assert!(again.contains("previous reply"));
    }

    /// The whole point of the strict-JSON prompt is that a small local model
    /// obeys it. Ignored by default -- it needs a downloaded model:
    ///
    /// ```text
    /// EVO_LLM_TEST_MODEL=qwen3-1.7b cargo test -- --ignored enrich
    /// ```
    #[test]
    #[ignore = "needs a downloaded model; set EVO_LLM_TEST_MODEL"]
    fn a_real_model_answers_the_prompt_with_usable_json() {
        let Ok(id) = std::env::var("EVO_LLM_TEST_MODEL") else {
            panic!("set EVO_LLM_TEST_MODEL to a catalogue id");
        };
        let config = ModelConfig {
            api: crate::script::model::Api::Builtin,
            builtin_model: id,
            ..Default::default()
        };
        let text = "Annual Fire Safety Inspection. The extinguishers on floors \
                    one to four were checked on 3 March and all were found to \
                    be within their service date. The alarm panel in the north \
                    stairwell failed its test and was replaced.";
        let got =
            describe(&config, "Fire Safety Inspection", text, &mut || true).expect("a description");
        assert!(!got.summary.trim().is_empty(), "no summary");
        let tags = clamp_tags(&got.tags, &[]);
        assert!(tags.len() <= MAX_TAGS);
        crate::llm::unload();
    }

    /// Off by default, and preferences written before the feature existed load
    /// with it off.
    #[test]
    fn enrichment_is_off_until_it_is_asked_for() {
        assert!(!AssistantPrefs::default().enrich_enabled);
        let old: AssistantPrefs = serde_json::from_str("{}").expect("deserialize");
        assert!(!old.enrich_enabled);
        let on: AssistantPrefs =
            serde_json::from_str(r#"{"enrich_enabled":true}"#).expect("deserialize");
        assert!(on.enrich_enabled);
    }
}
