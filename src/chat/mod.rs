//! Chat with the open document.
//!
//! The same shape as the script engine: a named worker thread, jobs over an
//! `mpsc` channel, a shared status the UI polls, and a repaint after every
//! change. The worker is handed a snapshot of the document's bytes and never
//! sees `DocState`, the library or egui state, so nothing about the open
//! document has to be shared across threads.
//!
//! A question is answered from the pages [`retrieval`] picks for it, quoted in
//! the prompt; the model is asked to cite them and the panel turns those
//! citations into links. Earlier turns are replayed as history *without* their
//! quoted pages -- each question retrieves afresh, so a conversation does not
//! drag every page it ever touched along behind it.

pub mod retrieval;

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::script::model::{ChatMessage, GenerateRequest, ModelConfig, Role};

/// Grounded answers want little invention; some helps them read as prose.
const TEMPERATURE: f32 = 0.2;

/// One question, with everything needed to answer it.
pub struct ChatJob {
    /// Identifies the document across requests: the library id, or the hash of
    /// the bytes for a document that is not in the library. The worker caches
    /// extracted page text under it, and the UI uses it to check that an
    /// arriving answer belongs to the document still on screen.
    pub doc_key: String,
    pub source: Arc<Vec<u8>>,
    pub title: String,
    pub question: String,
    /// Earlier turns, oldest first, without their quoted pages.
    pub history: Vec<ChatMessage>,
    pub config: ModelConfig,
}

/// A finished request, waiting for the UI thread to take it.
pub struct ChatOutcome {
    pub doc_key: String,
    pub result: Result<Answer, String>,
}

/// A complete reply and the pages it was allowed to draw on (1-based).
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub text: String,
    pub pages: Vec<usize>,
}

#[derive(Default)]
pub struct ChatStatus {
    /// `doc_key` of the request in flight, if any.
    pub running: Option<String>,
    /// What has arrived from the model so far.
    pub streaming: String,
    /// What the worker is doing before the first token shows up.
    pub stage: Option<&'static str>,
    pub outcome: Option<ChatOutcome>,
}

pub struct ChatEngine {
    tx: Sender<ChatJob>,
    status: Arc<Mutex<ChatStatus>>,
    cancel: Arc<AtomicBool>,
}

impl ChatEngine {
    pub fn spawn(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel::<ChatJob>();
        let status = Arc::new(Mutex::new(ChatStatus::default()));
        let cancel = Arc::new(AtomicBool::new(false));

        let worker_status = status.clone();
        let worker_cancel = cancel.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("evo-chat".into())
            .spawn(move || worker(rx, worker_status, worker_cancel, ctx))
            .expect("failed to spawn the chat thread");

        Self { tx, status, cancel }
    }

    pub fn ask(&self, job: ChatJob) {
        self.cancel.store(false, Ordering::Relaxed);
        {
            let mut status = self.status.lock().unwrap();
            status.running = Some(job.doc_key.clone());
            status.streaming.clear();
            status.stage = Some("Reading the document…");
            status.outcome = None;
        }
        let _ = self.tx.send(job);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether a request for this document is in flight.
    pub fn is_running(&self, doc_key: &str) -> bool {
        self.status.lock().unwrap().running.as_deref() == Some(doc_key)
    }

    pub fn with_status<T>(&self, f: impl FnOnce(&ChatStatus) -> T) -> T {
        f(&self.status.lock().unwrap())
    }

    /// Take the finished request's result, if one is waiting.
    pub fn take_outcome(&self) -> Option<ChatOutcome> {
        self.status.lock().unwrap().outcome.take()
    }
}

fn worker(
    rx: Receiver<ChatJob>,
    status: Arc<Mutex<ChatStatus>>,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    // One document's page text, kept between questions: extraction costs
    // seconds on a long PDF and a conversation asks many questions of one
    // document. One entry is the whole cache -- only one document is open.
    let mut cached: Option<(String, Arc<Vec<String>>)> = None;

    while let Ok(job) = rx.recv() {
        let pages = match &cached {
            Some((key, pages)) if *key == job.doc_key => pages.clone(),
            _ => {
                let pages = Arc::new(crate::library::extract::extract_all_pages(&job.source));
                cached = Some((job.doc_key.clone(), pages.clone()));
                pages
            }
        };

        let doc_key = job.doc_key.clone();
        let result = answer(&job, &pages, &status, &cancel, &ctx);
        {
            let mut s = status.lock().unwrap();
            s.running = None;
            s.stage = None;
            s.outcome = Some(ChatOutcome { doc_key, result });
        }
        ctx.request_repaint();
    }
}

fn answer(
    job: &ChatJob,
    pages: &[String],
    status: &Arc<Mutex<ChatStatus>>,
    cancel: &Arc<AtomicBool>,
    ctx: &egui::Context,
) -> Result<Answer, String> {
    if pages.iter().all(|p| p.trim().is_empty()) {
        return Err(format!(
            "No text could be read from “{}”. Scanned pages have to be \
             recognized before the chat can quote them.",
            job.title
        ));
    }

    let selected = retrieval::select_pages(pages, &job.question);
    let context = retrieval::context_block(pages, &selected);
    let request = GenerateRequest {
        model: job.config.model.clone(),
        prompt: retrieval::user_prompt(&job.title, &context, &job.question),
        system: Some(retrieval::system_prompt(&job.title)),
        history: job.history.clone(),
        temperature: Some(TEMPERATURE),
        max_tokens: None,
    };

    {
        let mut s = status.lock().unwrap();
        s.stage = Some("Asking the model…");
    }
    ctx.request_repaint();

    let backend = job.config.build();
    let mut on_token = |chunk: &str| {
        if cancel.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        {
            let mut s = status.lock().unwrap();
            s.stage = None;
            s.streaming.push_str(chunk);
        }
        ctx.request_repaint();
        ControlFlow::Continue(())
    };

    let pages: Vec<usize> = selected.iter().map(|p| p + 1).collect();
    match backend.generate(&request, &mut on_token) {
        Ok(text) => Ok(Answer { text, pages }),
        Err(e) => {
            // A cancelled request still produced whatever had arrived; throwing
            // it away would be a worse answer to "stop" than keeping it.
            let partial = status.lock().unwrap().streaming.clone();
            if cancel.load(Ordering::Relaxed) && !partial.trim().is_empty() {
                Ok(Answer {
                    text: partial,
                    pages,
                })
            } else {
                Err(e.to_string())
            }
        }
    }
}

/// The user's side of a conversation, as the model should see it: the earlier
/// turns only, since the current question travels with its quoted pages.
pub fn history_for(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::Assistant))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_the_conversation_and_drops_anything_else() {
        let messages = vec![
            ChatMessage::new(Role::User, "what is this?"),
            ChatMessage::new(Role::Assistant, "A report. [p.1]"),
            ChatMessage::new(Role::System, "internal note"),
        ];
        let history = history_for(&messages);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].content, "A report. [p.1]");
    }
}
