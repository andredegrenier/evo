//! Chat with a document, as a stream of events.
//!
//! An answer takes seconds and arrives a word at a time, so the reply is not
//! one response but a run of them: what the server is doing, then the text as
//! it is generated, then the finished answer and the pages it was drawn from.
//! Server-sent events over a POST -- `EventSource` cannot POST, so the client
//! reads the stream itself, which is thirty lines of `chat.js`.
//!
//! Three rules shape the module.
//!
//! The retrieval and the prompts are [`crate::chat::retrieval`]'s, unchanged.
//! What the phone asks and what the desktop app asks have to be the same
//! question, or the two would quietly diverge into different products.
//!
//! Nothing slow happens on an async task. Extraction, and the model itself, run
//! in `spawn_blocking`; the bytes are cloned out of the library's lock before
//! either starts. Generations are serialized by a semaphore, because two
//! four-billion-parameter models in memory at once is how a small server dies.
//!
//! And a reader who closes the tab stops the work. The events go through an
//! `mpsc` channel; when the browser goes away axum drops the receiver, the
//! blocking side's `send` fails, `on_token` answers [`ControlFlow::Break`], and
//! the generation is abandoned. There is no separate cancellation to get wrong.
//!
//! Two endpoints, one machine. `/api/docs/{id}/chat` asks about a document and
//! quotes its pages; `/api/agent/chat` asks about the library and has nothing
//! quoted at all, because what it is for is the tools -- see [`super::tools`].
//! Both take `tools`, and both default to false: switching evo's own controls
//! over to a language model is something a person says yes to.

use std::cell::RefCell;
use std::convert::Infallible;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::chat::{agent, history_for, retrieval};
use crate::mcp::McpAccess;
use crate::mcp::client::RemoteTool;
use crate::script::model::{
    ChatMessage, GenerateRequest, ModelBackend, ModelError, ToolCall, ToolDef,
};

use super::Shared;
use super::library_api::{check_id, document_bytes, fail, no_such_document, with_library};
use super::tools::{Emit, ServerTools};

/// Grounded answers want little invention; some helps them read as prose. The
/// same figure the desktop app's chat uses -- an answer should not depend on
/// which screen asked for it.
const TEMPERATURE: f32 = 0.2;

/// How many events may be waiting for the browser before the model is made to
/// slow down. Small on purpose: the backpressure is what stops a fast model
/// filling memory for a phone on a slow train.
const BACKLOG: usize = 64;

/// The longest transcript the server will keep for one document. A
/// conversation is a few dozen turns; anything past this is a client that has
/// stopped trimming, and the library is not a log file.
const MAX_TRANSCRIPT: usize = 500;

// ---------------------------------------------------------------------------
// Page text, kept for the length of a conversation
// ---------------------------------------------------------------------------

/// Extracted page text for the documents most recently asked about.
///
/// Reading every page of a long PDF costs seconds, and a conversation asks
/// several questions of one document. Keyed by document id, which is a content
/// hash, so an entry can never be stale -- only unwanted, which is what the
/// capacity is for.
#[derive(Default)]
pub struct PageText {
    /// Least recently used first.
    entries: Vec<(String, Arc<Vec<String>>)>,
}

impl PageText {
    /// How many documents' text to keep. A phone has one document open and
    /// perhaps flicks back to the last one or two.
    const CAPACITY: usize = 4;

    pub fn get(&mut self, id: &str) -> Option<Arc<Vec<String>>> {
        let at = self.entries.iter().position(|(key, _)| key == id)?;
        let entry = self.entries.remove(at);
        let pages = entry.1.clone();
        self.entries.push(entry);
        Some(pages)
    }

    pub fn put(&mut self, id: &str, pages: Arc<Vec<String>>) {
        self.entries.retain(|(key, _)| key != id);
        self.entries.push((id.to_owned(), pages));
        while self.entries.len() > Self::CAPACITY {
            self.entries.remove(0);
        }
    }
}

// ---------------------------------------------------------------------------
// What the server says while it answers
// ---------------------------------------------------------------------------

/// One thing worth telling the reader about a question in flight.
///
/// The data of every event is JSON, never bare text. A model chunk may hold a
/// newline, and a newline in an SSE `data:` line is a frame boundary -- so
/// putting the chunk in a JSON string is what stops a paragraph break from
/// looking like the end of the answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Say {
    /// What the server is doing before there is anything to read.
    Stage(&'static str),
    /// Text from the model, as it arrives.
    Token(String),
    /// Something the model is having evo do, as it happens. The reader watches
    /// the tool run rather than finding out about it in the answer.
    Tool { name: String, text: String },
    /// Something for the app itself to do: turn to a page, redraw the markup.
    /// The payload is [`super::tools`]'s, and the browser switches on `action`.
    Ui(Value),
    /// The finished answer, the pages (1-based) it was allowed to use, and the
    /// tools it ran on the way.
    Done {
        text: String,
        pages: Vec<usize>,
        tools: Vec<String>,
    },
    /// Why there is no answer. A sentence, for a person.
    Error(String),
}

impl Say {
    /// The SSE event type. `chat.js` switches on exactly these.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stage(_) => "stage",
            Self::Token(_) => "token",
            Self::Tool { .. } => "tool",
            Self::Ui(_) => "ui",
            Self::Done { .. } => "done",
            Self::Error(_) => "error",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            Self::Stage(text) => json!({ "text": text }),
            Self::Token(text) => json!({ "text": text }),
            Self::Tool { name, text } => json!({ "name": name, "text": text }),
            Self::Ui(payload) => payload.clone(),
            Self::Done { text, pages, tools } => {
                json!({ "text": text, "pages": pages, "tools_used": tools })
            }
            Self::Error(message) => json!({ "error": message }),
        }
    }

    fn event(&self) -> Event {
        // `json_data` only fails if the value cannot be serialized, and a
        // `serde_json::Value` always can.
        Event::default()
            .event(self.name())
            .json_data(self.data())
            .unwrap_or_else(|_| Event::default().event(self.name()).data("{}"))
    }
}

/// What the reader is told while the pages are being read.
const READING: &str = "Reading the document…";
/// And while somebody else's question is being answered first.
const WAITING: &str = "Waiting for the model…";
/// And while the configured MCP servers are being started, which can take a
/// moment the first time.
const TOOLING: &str = "Asking the tool servers what they can do…";
const ASKING: &str = "Asking the model…";

// ---------------------------------------------------------------------------
// One question, from prompt to answer
// ---------------------------------------------------------------------------

/// What the model may have evo do this turn: the tools it was offered, and the
/// thing that runs them.
///
/// Both halves together or neither: a tool list nothing can run would have the
/// model asking for things that always fail, and a runner with no list would
/// have it guessing at names.
pub struct Toolbox<'a> {
    pub access: &'a dyn McpAccess,
    pub tools: &'a [RemoteTool],
}

/// Ask `backend` the question in `request` and report what happens.
///
/// Everything above this is transport and everything below it is a model, so
/// this is the seam a test can stand in: a scripted backend and a closure that
/// collects what was said give the whole event sequence with nothing
/// downloaded and no socket open.
///
/// `say` returning [`ControlFlow::Break`] means nobody is listening any more,
/// and the request is abandoned where it stands.
pub fn converse(
    backend: &dyn ModelBackend,
    request: GenerateRequest,
    pages: Vec<usize>,
    toolbox: Option<Toolbox<'_>>,
    say: &mut dyn FnMut(Say) -> ControlFlow<()>,
) {
    // Streaming and tool-running both report to the reader, and `run_agent`
    // calls them one at a time from one thread -- so they share `say` through a
    // `RefCell` rather than each having half of the conversation.
    let say = RefCell::new(say);
    let tell = |said: Say| -> ControlFlow<()> {
        let mut say = say.borrow_mut();
        (*say)(said)
    };

    if tell(Say::Stage(ASKING)).is_break() {
        return;
    }

    let cancel = AtomicBool::new(false);
    let mut used: Vec<String> = Vec::new();

    let outcome = {
        let mut on_token = |chunk: &str| tell(Say::Token(chunk.to_owned()));
        // A model that invents a call is told there is no such thing, in the
        // same words the desktop app uses, and carries on -- which is better
        // than failing the answer over it.
        let mut execute = |call: &ToolCall| {
            let unknown = || format!("evo has no tool called \u{201c}{}\u{201d}", call.name);
            let Some(toolbox) = &toolbox else {
                return Err(unknown());
            };
            let Some(tool) = toolbox.tools.iter().find(|t| t.def.name == call.name) else {
                return Err(unknown());
            };
            used.push(call.name.clone());
            let _ = tell(Say::Tool {
                name: call.name.clone(),
                text: format!("Running {}…", call.name),
            });
            let result = toolbox
                .access
                .call(&tool.server, &tool.tool, call.arguments.clone());
            let _ = tell(match &result {
                Ok(text) => Say::Tool {
                    name: call.name.clone(),
                    text: format!("{} answered ({} characters).", call.name, text.len()),
                },
                Err(e) => Say::Tool {
                    name: call.name.clone(),
                    text: format!("{} failed: {e}", call.name),
                },
            });
            result
        };
        agent::run_agent(
            backend,
            request,
            &mut execute,
            agent::MAX_ITERATIONS,
            &mut on_token,
            &cancel,
        )
    };

    match outcome {
        Ok(answer) => {
            let _ = tell(Say::Done {
                text: answer.text,
                pages,
                tools: used,
            });
        }
        // Cancelled is the reader having gone: there is nobody to tell.
        Err(ModelError::Cancelled) => {}
        Err(e) => {
            let _ = tell(Say::Error(explain(&e)));
        }
    }
}

/// What a phone is told when a generation fails.
///
/// The model layer writes its failures for the desktop app, where the answer to
/// "there is no model" is a Preferences pane and where a refused connection is
/// something the reader can go and fix. Neither is true here: the model lives on
/// a server the reader is not sitting at, and `evo serve` has no settings screen
/// at all. So the two failures that mean "nothing is going to answer you" are
/// rewritten to name what someone with a shell on that box can actually do, and
/// the original is kept in brackets -- it is the only thing that distinguishes a
/// missing model from an endpoint pointed at the wrong port.
fn explain(error: &ModelError) -> String {
    match error {
        ModelError::Unreachable { .. } | ModelError::Unavailable(_) => format!(
            "No language model is available to answer this. On the server, download one with \
             `evo fetch-model`, or point the \u{201c}model\u{201d} section of serve/config.json at \
             a model server that is running. ({error})"
        ),
        other => other.to_string(),
    }
}

/// The request the model is given for one question about one document.
///
/// Deliberately assembled from [`retrieval`]'s own prompts rather than from
/// anything written here: the promise the system prompt makes ("only these
/// pages", "cite them as [p.N]") is the promise `select_pages` keeps.
pub fn ask_about(
    title: &str,
    pages: &[String],
    selected: &[usize],
    question: &str,
    history: Vec<ChatMessage>,
    model: String,
    tools: Vec<ToolDef>,
) -> GenerateRequest {
    let context = retrieval::context_block(pages, selected);
    GenerateRequest {
        model,
        prompt: retrieval::user_prompt(title, &context, question),
        system: Some(retrieval::system_prompt_with_tools(
            title,
            !tools.is_empty(),
        )),
        history,
        tools,
        temperature: Some(TEMPERATURE),
        max_tokens: None,
    }
}

/// The request the model is given for a question about the library itself.
///
/// Nothing is quoted, because nothing has been chosen: the agent's context is
/// whatever it goes and looks up. Which is why the prompt is a different one --
/// there are no pages here to promise to answer only from.
pub fn ask_library(
    question: &str,
    history: Vec<ChatMessage>,
    model: String,
    tools: Vec<ToolDef>,
) -> GenerateRequest {
    GenerateRequest {
        model,
        prompt: question.to_owned(),
        system: Some(retrieval::library_system_prompt(!tools.is_empty())),
        history,
        tools,
        temperature: Some(TEMPERATURE),
        max_tokens: None,
    }
}

// ---------------------------------------------------------------------------
// The endpoints
// ---------------------------------------------------------------------------

/// What the phone sends.
#[derive(Debug, Default, Deserialize)]
pub struct Ask {
    pub question: String,
    /// The conversation so far, oldest first, without the pages each turn
    /// quoted: every question retrieves afresh.
    #[serde(default)]
    pub history: Vec<ChatMessage>,
    /// Whether the model may drive evo -- search the library, open a document,
    /// mark it up. Off unless asked for: the panel that wants it says so on
    /// every request, so nothing is remembered on the server that the reader
    /// cannot see the state of.
    #[serde(default)]
    pub tools: bool,
}

/// `POST /api/docs/{id}/chat` -- a question, answered as a stream of events.
pub async fn chat(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(ask): Json<Ask>,
) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    if ask.question.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "There is no question in that.");
    }

    // Whether the document exists is a fact about the request, so it is an
    // HTTP status. Everything that goes wrong after the stream has opened is
    // an `error` event, because by then the answer is already 200.
    let wanted = id.clone();
    let found = with_library(&state, move |lib| {
        Ok(lib
            .doc(&wanted)
            .map_err(|e| e.to_string())?
            .map(|meta| meta.title))
    })
    .await;
    let title = match found {
        Ok(Some(title)) => title,
        Ok(None) => return no_such_document(),
        Err(response) => return response,
    };
    let bytes = match document_bytes(&state, &id).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(BACKLOG);
    tokio::spawn(answer(state, id, title, bytes, ask, tx));
    stream(rx)
}

/// `POST /api/agent/chat` -- a question about the library, answered by an
/// assistant that can go and look.
///
/// No document, so no pages to quote and nothing to check the id of: what makes
/// this useful is the tools, and with them switched off it says so.
pub async fn agent_chat(State(state): State<Shared>, Json(ask): Json<Ask>) -> Response {
    if ask.question.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "There is no question in that.");
    }
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(BACKLOG);
    tokio::spawn(answer_about_the_library(state, ask, tx));
    stream(rx)
}

fn stream(rx: mpsc::Receiver<Result<Event, Infallible>>) -> Response {
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
        // A phone on a mobile network sits behind proxies that close a quiet
        // connection, and a model can think for a while before its first word.
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// What a question is being asked about, which is what decides the prompt.
enum Subject {
    /// One document: its title, its text by page, and the pages retrieval
    /// picked for this question.
    Document {
        title: String,
        pages: Arc<Vec<String>>,
        selected: Vec<usize>,
    },
    /// The library as a whole. There is nothing to quote, only tools.
    Library,
}

/// The work behind one question about a document, from the blob to the last
/// token.
async fn answer(
    state: Shared,
    id: String,
    title: String,
    bytes: Arc<Vec<u8>>,
    ask: Ask,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    if tell(&tx, Say::Stage(READING)).await.is_break() {
        return;
    }
    let pages = match page_text(&state, &id, bytes).await {
        Ok(pages) => pages,
        Err(message) => {
            let _ = tell(&tx, Say::Error(message)).await;
            return;
        }
    };
    if pages.iter().all(|page| page.trim().is_empty()) {
        let _ = tell(
            &tx,
            Say::Error(format!(
                "No text could be read from \u{201c}{title}\u{201d}. Scanned pages have to be \
                 recognized before the chat can quote them."
            )),
        )
        .await;
        return;
    }

    let selected = retrieval::select_pages(&pages, &ask.question);
    let cited: Vec<usize> = selected.iter().map(|page| page + 1).collect();
    generate(
        state,
        Subject::Document {
            title,
            pages,
            selected,
        },
        ask,
        cited,
        tx,
    )
    .await;
}

async fn answer_about_the_library(
    state: Shared,
    ask: Ask,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    generate(state, Subject::Library, ask, Vec::new(), tx).await;
}

/// Run one question past the model, tools and all.
///
/// Everything here happens on a blocking thread: starting an MCP server, asking
/// it what it can do, running a tool that opens a PDF, and the generation
/// itself are all things that take long enough to matter, and none of them may
/// happen on an async task.
async fn generate(
    state: Shared,
    subject: Subject,
    ask: Ask,
    cited: Vec<usize>,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    // One generation at a time. A model is the largest thing this process
    // does, and two at once on a small server means neither finishes.
    let permit = match state.generation.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            if tell(&tx, Say::Stage(WAITING)).await.is_break() {
                return;
            }
            match state.generation.acquire().await {
                Ok(permit) => permit,
                // Only if the semaphore were closed, which nothing does.
                Err(_) => return,
            }
        }
    };

    let config = state.config.model.clone();
    let build = state.backend.clone();
    let library = state.library.clone();
    let clients = state.mcp.clone();
    let generating = tokio::task::spawn_blocking(move || {
        // Everything the reader is told goes down one channel, in the order it
        // happened -- the stages, the tokens, the tool chips, and the tools'
        // own effects, which come from `ServerTools` through `emit`.
        let sender = tx.clone();
        let mut say = move |said: Say| {
            // The one place cancellation lives: a closed browser is a dropped
            // receiver, and a dropped receiver is a failed send.
            match sender.blocking_send(Ok(said.event())) {
                Ok(()) => ControlFlow::Continue(()),
                Err(_) => ControlFlow::Break(()),
            }
        };
        let emit: Emit = Arc::new(move |said| {
            let _ = tx.blocking_send(Ok(said.event()));
        });

        // The tool list is fetched here rather than at startup because
        // gathering it can mean starting somebody's child process.
        let mut server_tools = None;
        let mut offered: Vec<RemoteTool> = Vec::new();
        if ask.tools {
            if say(Say::Stage(TOOLING)).is_break() {
                return;
            }
            // An unconfigured client is no client at all: a model that asks
            // for a tool nobody offered should be told evo has no such thing,
            // not sent looking for a server that was never named.
            let tools = ServerTools::new(library, clients.is_configured().then_some(clients), emit);
            offered = tools.tools();
            server_tools = Some(tools);
        }

        let defs: Vec<ToolDef> = offered.iter().map(|tool| tool.def.clone()).collect();
        let request = match subject {
            Subject::Document {
                title,
                pages,
                selected,
            } => ask_about(
                &title,
                &pages,
                &selected,
                &ask.question,
                history_for(&ask.history),
                config.model.clone(),
                defs,
            ),
            Subject::Library => ask_library(
                &ask.question,
                history_for(&ask.history),
                config.model.clone(),
                defs,
            ),
        };

        let toolbox = server_tools.as_ref().map(|tools| Toolbox {
            access: tools as &dyn McpAccess,
            tools: &offered,
        });
        let backend = build(&config);
        converse(backend.as_ref(), request, cited, toolbox, &mut say);
    })
    .await;
    drop(permit);
    if generating.is_err() {
        eprintln!("a chat generation stopped part-way through");
    }
}

/// Put an event on the stream. `Break` means the browser has gone.
async fn tell(tx: &mpsc::Sender<Result<Event, Infallible>>, said: Say) -> ControlFlow<()> {
    match tx.send(Ok(said.event())).await {
        Ok(()) => ControlFlow::Continue(()),
        Err(_) => ControlFlow::Break(()),
    }
}

/// The document's text by page, from the cache or by reading it.
async fn page_text(
    state: &Shared,
    id: &str,
    bytes: Arc<Vec<u8>>,
) -> Result<Arc<Vec<String>>, String> {
    if let Some(pages) = state
        .pages_text
        .lock()
        .expect("the page-text lock is never poisoned")
        .get(id)
    {
        return Ok(pages);
    }
    // No password: the library only ever stores documents evo can already
    // read, so a protected one was decrypted at import.
    let read = tokio::task::spawn_blocking(move || {
        crate::library::extract::extract_all_pages(&bytes, None)
    })
    .await
    .map_err(|_| "evo stopped part-way through reading that document.".to_owned())?;
    let pages = Arc::new(read);
    state
        .pages_text
        .lock()
        .expect("the page-text lock is never poisoned")
        .put(id, pages.clone());
    Ok(pages)
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

/// `GET /api/docs/{id}/chatlog` -- the conversation about this document.
///
/// The same CHATS table the desktop app writes, in the same format, so a
/// conversation started on a phone is one the app opens.
pub async fn get_chatlog(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    let wanted = id.clone();
    let found = with_library(&state, move |lib| {
        if lib.doc(&wanted).map_err(|e| e.to_string())?.is_none() {
            return Ok(None);
        }
        lib.load_chat(&wanted).map(Some).map_err(|e| e.to_string())
    })
    .await;
    match found {
        Ok(Some(messages)) => Json(json!({ "messages": messages })).into_response(),
        Ok(None) => no_such_document(),
        Err(response) => response,
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ChatLog {
    pub messages: Vec<ChatMessage>,
}

/// `PUT /api/docs/{id}/chatlog` -- keep this conversation.
///
/// Not conditional, unlike markup: a transcript belongs to whoever is having
/// the conversation, and two people asking the same document questions at once
/// is not a case worth a version tag. An empty list clears it.
pub async fn put_chatlog(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(body): Json<ChatLog>,
) -> Response {
    if let Some(response) = check_id(&id) {
        return response;
    }
    if body.messages.len() > MAX_TRANSCRIPT {
        return fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "That is a longer conversation than evo keeps. Start a new one.",
        );
    }
    let wanted = id.clone();
    let saved = with_library(&state, move |lib| {
        if lib.doc(&wanted).map_err(|e| e.to_string())?.is_none() {
            return Ok(false);
        }
        lib.save_chat(&wanted, &body.messages)
            .map(|()| true)
            .map_err(|e| e.to_string())
    })
    .await;
    match saved {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => no_such_document(),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::model::{GenerateOutcome, Role};
    use std::sync::Mutex;

    /// A backend that answers from a script, after `chat::agent`'s own test
    /// double: one prepared outcome per call, and the text streamed the way a
    /// real one streams it. No weights, no server, no network -- which is what
    /// lets the event framing be tested in CI.
    struct ScriptedBackend {
        replies: Mutex<std::collections::VecDeque<Result<GenerateOutcome, ModelError>>>,
    }

    impl ScriptedBackend {
        fn saying(chunks: &[&str]) -> Self {
            Self::new(vec![Ok(GenerateOutcome::text(chunks.concat()))])
        }

        fn new(replies: Vec<Result<GenerateOutcome, ModelError>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
            }
        }
    }

    impl ModelBackend for ScriptedBackend {
        fn generate(
            &self,
            _req: &GenerateRequest,
            on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
        ) -> Result<GenerateOutcome, ModelError> {
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(GenerateOutcome::text("out of script")));
            let outcome = reply?;
            if !outcome.text.is_empty() && on_token(&outcome.text).is_break() {
                return Err(ModelError::Cancelled);
            }
            Ok(outcome)
        }

        fn list_models(&self) -> Result<Vec<String>, ModelError> {
            Ok(vec!["scripted".to_owned()])
        }

        fn describe(&self) -> String {
            "scripted (test)".to_owned()
        }
    }

    fn pages() -> Vec<String> {
        vec![
            "Introduction to the building.".to_owned(),
            "The boiler is in the basement plant room.".to_owned(),
        ]
    }

    /// Everything one question said, in order.
    fn run(backend: &dyn ModelBackend, pages_cited: Vec<usize>) -> Vec<Say> {
        let mut said = Vec::new();
        converse(
            backend,
            GenerateRequest::default(),
            pages_cited,
            None,
            &mut |s| {
                said.push(s);
                ControlFlow::Continue(())
            },
        );
        said
    }

    #[test]
    fn a_question_is_answered_as_a_stage_some_tokens_and_a_done() {
        let backend = ScriptedBackend::saying(&["The boiler is in the basement. [p.2]"]);
        let said = run(&backend, vec![2]);

        let names: Vec<&str> = said.iter().map(Say::name).collect();
        assert_eq!(names, ["stage", "token", "done"]);
        assert_eq!(said[0].data()["text"], ASKING);
        assert_eq!(
            said[1].data()["text"],
            "The boiler is in the basement. [p.2]"
        );

        // The last event carries the whole answer and the pages the reader is
        // allowed to check it against -- the citation links are built from it.
        let done = said[2].data();
        assert_eq!(done["text"], "The boiler is in the basement. [p.2]");
        assert_eq!(done["pages"], json!([2]));
    }

    /// The framing rule: a chunk is never allowed to look like the end of a
    /// frame. SSE ends an event at a blank line, so a model that writes a
    /// paragraph break would cut its own answer in half if the data were bare
    /// text instead of JSON.
    #[test]
    fn a_newline_in_the_model_output_does_not_end_the_event() {
        let awkward = "First.\n\nSecond \"quoted\" line.\r\nThird: data: not a field.";
        let backend = ScriptedBackend::saying(&[awkward]);
        let said = run(&backend, Vec::new());

        let token = said.iter().find(|s| s.name() == "token").expect("a token");
        assert_eq!(token.data()["text"], awkward);
        let wire = serde_json::to_string(&token.data()).expect("serializable");
        assert!(
            !wire.contains('\n') && !wire.contains('\r'),
            "one event, one line: {wire}"
        );
        // And it survives the trip back, which is what chat.js does with it.
        let parsed: Value = serde_json::from_str(&wire).expect("valid JSON");
        assert_eq!(parsed["text"], awkward);
    }

    /// Closing the tab is the only cancellation there is: the send fails, the
    /// closure says Break, and nothing further is said.
    #[test]
    fn a_reader_who_goes_away_stops_the_generation() {
        let backend = ScriptedBackend::saying(&["half an answer"]);
        let mut said = Vec::new();
        converse(
            &backend,
            GenerateRequest::default(),
            vec![1],
            None,
            &mut |s| {
                let stop = s.name() == "token";
                said.push(s);
                if stop {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        );
        let names: Vec<&str> = said.iter().map(Say::name).collect();
        assert_eq!(names, ["stage", "token"], "no done, and no error either");
    }

    /// The commonest failure by far -- no model configured, or none downloaded
    /// -- and the one that has to arrive as a sentence rather than a hang.
    #[test]
    fn a_model_that_cannot_answer_says_why_in_an_event() {
        let backend = ScriptedBackend::new(vec![Err(ModelError::Unavailable(
            "Qwen3 4B has not been downloaded yet.".to_owned(),
        ))]);
        let said = run(&backend, vec![1]);

        let names: Vec<&str> = said.iter().map(Say::name).collect();
        assert_eq!(names, ["stage", "error"]);
        let message = said[1].data()["error"].as_str().unwrap().to_owned();
        assert!(
            message.contains("evo fetch-model"),
            "the reader is told what to do on the server: {message}"
        );
        assert!(
            message.contains("Qwen3 4B has not been downloaded yet."),
            "and the original failure is still in there: {message}"
        );
    }

    /// A phone reader cannot do anything with "io: Connection refused", and
    /// there is no Preferences pane on a server to send them to.
    #[test]
    fn a_model_server_that_is_not_running_is_explained_rather_than_quoted() {
        let error = ModelError::Unreachable {
            url: "http://localhost:11434/api/chat".to_owned(),
            source: Box::new(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "Connection refused",
            ))),
        };
        let message = explain(&error);

        assert!(
            message.starts_with("No language model is available"),
            "it opens with what happened: {message}"
        );
        assert!(
            message.contains("evo fetch-model") && message.contains("serve/config.json"),
            "and with the two ways out of it: {message}"
        );
        assert!(
            !message.contains("Preferences"),
            "a server has no Preferences pane to send anyone to: {message}"
        );
        assert!(
            message.contains("http://localhost:11434/api/chat"),
            "the raw failure stays for whoever is reading the logs: {message}"
        );
    }

    /// The prompts are the desktop app's, not a second set that drifts. This
    /// is the test that says so: the request carries retrieval's own strings.
    #[test]
    fn the_question_is_asked_with_the_prompts_the_desktop_app_uses() {
        let pages = pages();
        let selected = retrieval::select_pages(&pages, "where is the boiler?");
        assert_eq!(selected, [1], "retrieval picked the page it should have");

        let request = ask_about(
            "Building manual",
            &pages,
            &selected,
            "where is the boiler?",
            vec![ChatMessage::new(Role::User, "what is this?")],
            "qwen3".to_owned(),
            Vec::new(),
        );

        let system = request.system.as_deref().expect("a system prompt");
        assert_eq!(
            system,
            retrieval::system_prompt_with_tools("Building manual", false)
        );
        assert!(system.contains("[p.N]"), "{system}");
        assert_eq!(
            request.prompt,
            retrieval::user_prompt(
                "Building manual",
                &retrieval::context_block(&pages, &selected),
                "where is the boiler?"
            )
        );
        assert!(request.prompt.contains("[Page 2]"), "{}", request.prompt);
        assert!(request.tools.is_empty(), "none were offered");
        assert_eq!(request.history.len(), 1);
        assert_eq!(request.temperature, Some(TEMPERATURE));
    }

    /// The agent asks about the library rather than about a document, so it
    /// gets the other prompt -- and with tools switched off it is told to say
    /// that it cannot see anything, rather than to invent a library.
    #[test]
    fn the_agent_is_told_it_is_driving_evo_and_what_it_may_use() {
        let with_tools = ask_library(
            "find the boiler report",
            Vec::new(),
            "qwen3".to_owned(),
            vec![ToolDef {
                name: "search_library".to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }],
        );
        let system = with_tools.system.as_deref().expect("a system prompt");
        assert_eq!(system, retrieval::library_system_prompt(true));
        assert!(system.contains("highlight_text"), "{system}");
        assert!(system.contains("[p.N]"), "{system}");
        // The question travels as itself: there are no pages to quote around it.
        assert_eq!(with_tools.prompt, "find the boiler report");
        assert_eq!(with_tools.temperature, Some(TEMPERATURE));

        let alone = ask_library(
            "what is in here?",
            Vec::new(),
            "qwen3".to_owned(),
            Vec::new(),
        );
        let system = alone.system.as_deref().expect("a system prompt");
        assert!(system.contains("Allow tools"), "{system}");
        assert!(!system.contains("highlight_text"), "{system}");
    }

    /// The agent loop end to end at this level: the model asks for a tool, the
    /// tool runs, what it did reaches the reader, and the answer follows.
    ///
    /// The order is the point. A `ui` frame is the app being driven, and it has
    /// to arrive between the chip that says the tool started and the words that
    /// explain what it found.
    #[test]
    fn a_tool_call_reaches_the_reader_as_a_chip_a_ui_event_and_an_answer() {
        use crate::mcp::client::RemoteTool;

        /// A toolbox of one tool, which reports what it did the way
        /// `ServerTools` does: through the same channel the words are on.
        struct Fake {
            said: std::sync::Arc<Mutex<Vec<Say>>>,
        }
        impl crate::mcp::McpAccess for Fake {
            fn tools(&self) -> Vec<RemoteTool> {
                Vec::new()
            }
            fn call(&self, _server: &str, tool: &str, arguments: Value) -> Result<String, String> {
                self.said.lock().unwrap().push(Say::Ui(json!({
                    "action": "open",
                    "doc": arguments["doc_id"],
                    "page": 2,
                })));
                Ok(format!("{tool} did it"))
            }
        }

        let said = std::sync::Arc::new(Mutex::new(Vec::new()));
        let access = Fake { said: said.clone() };
        let tools = vec![RemoteTool {
            server: "evo".to_owned(),
            tool: "open_document".to_owned(),
            def: ToolDef {
                name: "open_document".to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            },
        }];
        let backend = ScriptedBackend::new(vec![
            Ok(GenerateOutcome {
                text: "Opening it.".to_owned(),
                tool_calls: vec![ToolCall {
                    id: Some("call_1".to_owned()),
                    name: "open_document".to_owned(),
                    arguments: json!({"doc_id": "a".repeat(64), "page": 2}),
                }],
            }),
            Ok(GenerateOutcome::text("It is open at page 2. [p.2]")),
        ]);

        let recorder = said.clone();
        converse(
            &backend,
            GenerateRequest::default(),
            vec![2],
            Some(Toolbox {
                access: &access,
                tools: &tools,
            }),
            &mut |s| {
                recorder.lock().unwrap().push(s);
                ControlFlow::Continue(())
            },
        );

        let events = said.lock().unwrap();
        let names: Vec<&str> = events.iter().map(Say::name).collect();
        assert_eq!(
            names,
            ["stage", "token", "tool", "ui", "tool", "token", "done"],
            "the app being driven arrives between the chip and the answer"
        );
        assert_eq!(events[2].data()["text"], "Running open_document…");
        assert_eq!(events[3].data()["action"], "open");
        assert_eq!(events[3].data()["page"], 2);
        assert_eq!(events[6].data()["tools_used"], json!(["open_document"]));
        assert_eq!(events[6].data()["text"], "It is open at page 2. [p.2]");
    }

    /// A model that asks for something nobody offered is told so and carries
    /// on, whether or not any tools were allowed at all.
    #[test]
    fn a_tool_nobody_offered_is_refused_in_words_the_model_can_act_on() {
        let backend = ScriptedBackend::new(vec![
            Ok(GenerateOutcome {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: None,
                    name: "delete_everything".to_owned(),
                    arguments: json!({}),
                }],
            }),
            Ok(GenerateOutcome::text("I could not do that.")),
        ]);
        let said = run(&backend, Vec::new());
        let names: Vec<&str> = said.iter().map(Say::name).collect();
        assert_eq!(
            names,
            ["stage", "token", "done"],
            "no chip for a tool that is not"
        );
        assert_eq!(said[2].data()["tools_used"], json!([]));
    }

    /// A conversation asks one document many questions, so its text is read
    /// once; four documents in, the first is let go of.
    #[test]
    fn page_text_is_kept_for_the_documents_last_asked_about() {
        let mut cache = PageText::default();
        let text = |word: &str| Arc::new(vec![word.to_owned()]);

        assert!(cache.get("a").is_none());
        cache.put("a", text("alpha"));
        assert_eq!(cache.get("a").as_deref(), Some(&vec!["alpha".to_owned()]));

        for id in ["b", "c", "d"] {
            cache.put(id, text(id));
        }
        assert!(cache.get("a").is_some(), "four fit");

        // "a" was just read, so it is the newest; "b" is the one to go.
        cache.put("e", text("epsilon"));
        assert!(cache.get("b").is_none(), "the least recently used went");
        assert!(cache.get("a").is_some());
        assert!(cache.get("e").is_some());

        // Writing the same document twice does not fill the cache with it.
        cache.put("a", text("alpha again"));
        assert_eq!(
            cache.get("a").as_deref(),
            Some(&vec!["alpha again".to_owned()])
        );
    }
}
