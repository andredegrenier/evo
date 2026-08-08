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
use crate::script::model::{
    ChatMessage, GenerateRequest, ModelBackend, ModelError, ToolCall, ToolDef,
};

use super::Shared;
use super::library_api::{check_id, document_bytes, fail, no_such_document, with_library};

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
    /// The finished answer, and the pages (1-based) it was allowed to use.
    Done { text: String, pages: Vec<usize> },
    /// Why there is no answer. A sentence, for a person.
    Error(String),
}

impl Say {
    /// The SSE event type. `chat.js` switches on exactly these.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stage(_) => "stage",
            Self::Token(_) => "token",
            Self::Done { .. } => "done",
            Self::Error(_) => "error",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            Self::Stage(text) => json!({ "text": text }),
            Self::Token(text) => json!({ "text": text }),
            Self::Done { text, pages } => json!({ "text": text, "pages": pages }),
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
const ASKING: &str = "Asking the model…";

// ---------------------------------------------------------------------------
// One question, from prompt to answer
// ---------------------------------------------------------------------------

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
    say: &mut dyn FnMut(Say) -> ControlFlow<()>,
) {
    if say(Say::Stage(ASKING)).is_break() {
        return;
    }

    // The tools arrive in M27. Until then a model that invents a call is told
    // there is no such thing, in the same words the desktop app uses, and
    // carries on -- which is better than failing the answer over it.
    let mut execute = |call: &ToolCall| {
        Err(format!(
            "evo has no tool called \u{201c}{}\u{201d}",
            call.name
        ))
    };
    let cancel = AtomicBool::new(false);

    // The borrow of `say` ends with this block, so the outcome can be reported
    // through the same closure afterwards.
    let outcome = {
        let mut on_token = |chunk: &str| say(Say::Token(chunk.to_owned()));
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
            let _ = say(Say::Done {
                text: answer.text,
                pages,
            });
        }
        // Cancelled is the reader having gone: there is nobody to tell.
        Err(ModelError::Cancelled) => {}
        Err(e) => {
            let _ = say(Say::Error(e.to_string()));
        }
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

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// What the phone sends.
#[derive(Debug, Default, Deserialize)]
pub struct Ask {
    pub question: String,
    /// The conversation so far, oldest first, without the pages each turn
    /// quoted: every question retrieves afresh.
    #[serde(default)]
    pub history: Vec<ChatMessage>,
    /// Whether the model may drive evo. Read and ignored until M27 adds the
    /// tools -- the field is here now so the client that asks for them does
    /// not have to change shape later.
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "M27 gives the agent tools; the field is its contract"
    )]
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
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
        // A phone on a mobile network sits behind proxies that close a quiet
        // connection, and a model can think for a while before its first word.
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The work behind one question, from the blob to the last token.
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
    let request = ask_about(
        &title,
        &pages,
        &selected,
        &ask.question,
        history_for(&ask.history),
        state.config.model.model.clone(),
        // M27's business. Saying so here rather than in the client keeps the
        // decision in one place.
        Vec::new(),
    );

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
    let generating = tokio::task::spawn_blocking(move || {
        let backend = config.build();
        converse(backend.as_ref(), request, cited, &mut |said| {
            // The one place cancellation lives: a closed browser is a dropped
            // receiver, and a dropped receiver is a failed send.
            match tx.blocking_send(Ok(said.event())) {
                Ok(()) => ControlFlow::Continue(()),
                Err(_) => ControlFlow::Break(()),
            }
        });
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
    let read =
        tokio::task::spawn_blocking(move || crate::library::extract::extract_all_pages(&bytes))
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
        converse(backend, GenerateRequest::default(), pages_cited, &mut |s| {
            said.push(s);
            ControlFlow::Continue(())
        });
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
        converse(&backend, GenerateRequest::default(), vec![1], &mut |s| {
            let stop = s.name() == "token";
            said.push(s);
            if stop {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
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
        assert_eq!(
            said[1].data()["error"],
            "Qwen3 4B has not been downloaded yet."
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
        assert!(request.tools.is_empty(), "the tools arrive in M27");
        assert_eq!(request.history.len(), 1);
        assert_eq!(request.temperature, Some(TEMPERATURE));
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
