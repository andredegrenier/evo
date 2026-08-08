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

pub mod agent;
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
    /// The MCP servers this question may use. `None` -- the default -- means
    /// the model answers from the document alone.
    pub mcp: Option<Arc<dyn crate::mcp::McpAccess>>,
}

/// A finished request, waiting for the UI thread to take it.
pub struct ChatOutcome {
    pub doc_key: String,
    pub result: Result<Answer, String>,
}

/// A complete reply and the pages it was allowed to draw on (1-based).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub text: String,
    pub pages: Vec<usize>,
    /// Tools the model ran on the way to it, in order.
    pub tools_used: Vec<String>,
}

#[derive(Default)]
pub struct ChatStatus {
    /// `doc_key` of the request in flight, if any.
    pub running: Option<String>,
    /// What has arrived from the model so far.
    pub streaming: String,
    /// What the worker is doing before the first token shows up.
    pub stage: Option<&'static str>,
    /// What the model has had evo do this turn, newest last. Shown in the
    /// transcript so a tool run is something the reader watches rather than
    /// something they find out about afterwards.
    pub activity: Vec<String>,
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
            status.activity.clear();
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

    // What the model may ask evo to do, if this panel was allowed tools. The
    // list is fetched here, on the worker, because starting a server can take
    // a moment and the UI thread must not wait for it.
    let remote: Vec<crate::mcp::client::RemoteTool> = match &job.mcp {
        Some(access) => {
            let mut s = status.lock().unwrap();
            s.stage = Some("Asking the tool servers what they can do…");
            drop(s);
            ctx.request_repaint();
            access.tools()
        }
        None => Vec::new(),
    };

    let request = GenerateRequest {
        model: job.config.model.clone(),
        prompt: retrieval::user_prompt(&job.title, &context, &job.question),
        system: Some(retrieval::system_prompt_with_tools(
            &job.title,
            !remote.is_empty(),
        )),
        history: job.history.clone(),
        tools: remote.iter().map(|tool| tool.def.clone()).collect(),
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
    let mut used: Vec<String> = Vec::new();
    let mut execute = |call: &crate::script::model::ToolCall| {
        let Some(access) = &job.mcp else {
            return Err(format!("evo has no tool called “{}”", call.name));
        };
        let Some(tool) = resolve(&remote, &call.name) else {
            return Err(format!("evo has no tool called “{}”", call.name));
        };
        used.push(call.name.clone());
        note(status, ctx, format!("Running {}…", call.name));
        let result = access.call(&tool.server, &tool.tool, call.arguments.clone());
        match &result {
            Ok(text) => note(
                status,
                ctx,
                format!("{} answered ({} characters).", call.name, text.len()),
            ),
            Err(e) => note(status, ctx, format!("{} failed: {e}", call.name)),
        }
        result
    };

    let result = agent::run_agent(
        backend.as_ref(),
        request,
        &mut execute,
        agent::MAX_ITERATIONS,
        &mut on_token,
        cancel,
    );
    match result {
        Ok(outcome) => Ok(Answer {
            text: outcome.text,
            pages,
            tools_used: used,
        }),
        Err(e) => {
            // A cancelled request still produced whatever had arrived; throwing
            // it away would be a worse answer to "stop" than keeping it.
            let partial = status.lock().unwrap().streaming.clone();
            if cancel.load(Ordering::Relaxed) && !partial.trim().is_empty() {
                Ok(Answer {
                    text: partial,
                    pages,
                    tools_used: used,
                })
            } else {
                Err(e.to_string())
            }
        }
    }
}

/// Which remote tool a model's call names.
///
/// The model sees `server__tool`, so the match is on the qualified name and
/// nothing else: two servers may both have a `search`, and picking the wrong
/// one would be a quiet, expensive mistake.
fn resolve<'a>(
    tools: &'a [crate::mcp::client::RemoteTool],
    called: &str,
) -> Option<&'a crate::mcp::client::RemoteTool> {
    tools.iter().find(|tool| tool.def.name == called)
}

/// Put a line about what the model is having evo do in front of the reader.
fn note(status: &Arc<Mutex<ChatStatus>>, ctx: &egui::Context, line: String) {
    status.lock().unwrap().activity.push(line);
    ctx.request_repaint();
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
    use crate::mcp::client::{RemoteTool, qualified_name};
    use crate::script::model::ToolDef;

    fn remote(server: &str, tool: &str) -> RemoteTool {
        RemoteTool {
            server: server.to_owned(),
            tool: tool.to_owned(),
            def: ToolDef {
                name: qualified_name(server, tool),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            },
        }
    }

    /// Two servers may both have a `search`; the model names which one, and the
    /// wrong choice would be quiet and expensive.
    #[test]
    fn a_call_resolves_to_the_server_it_named() {
        let tools = [remote("files", "search"), remote("web", "search")];
        assert_eq!(
            resolve(&tools, "web__search").map(|t| t.server.as_str()),
            Some("web")
        );
        assert_eq!(
            resolve(&tools, "files__search").map(|t| t.server.as_str()),
            Some("files")
        );
        // The server's own name for the tool is not what the model calls it.
        assert!(resolve(&tools, "search").is_none());
        assert!(resolve(&tools, "other__search").is_none());
        assert!(resolve(&[], "web__search").is_none());
    }

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
