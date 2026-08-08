//! Talking to a local language model.
//!
//! The backend is a trait so the transport is not baked into the Lua API. The
//! shipped one speaks to a model server over HTTP -- Ollama's own API, plus the
//! OpenAI-compatible endpoint most local servers also expose -- which keeps evo
//! out of the business of shipping and running weights. Running a model in
//! process (rten is already in the tree for OCR) is a second implementation of
//! this trait, not a rewrite of everything above it.

use std::io::{BufRead, BufReader};
use std::ops::ControlFlow;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("could not reach the model at {url}: {source}")]
    Unreachable {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },
    #[error("the model returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("could not read the model's reply: {0}")]
    Read(String),
    /// The backend cannot run at all: no weights, no such model, or a build
    /// without the built-in engine.
    #[error("{0}")]
    Unavailable(String),
    #[error("cancelled")]
    Cancelled,
}

/// Who said something in a conversation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// The name both dialects use on the wire.
    pub fn wire(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// One turn of a conversation.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

pub struct GenerateRequest {
    pub model: String,
    pub prompt: String,
    pub system: Option<String>,
    /// Earlier turns, oldest first. Empty for a one-shot completion.
    pub history: Vec<ChatMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// A source of generated text.
pub trait ModelBackend: Send {
    /// Generate a completion, handing each chunk to `on_token` as it arrives.
    /// Returning [`ControlFlow::Break`] abandons the request.
    fn generate(
        &self,
        req: &GenerateRequest,
        on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
    ) -> Result<String, ModelError>;

    fn list_models(&self) -> Result<Vec<String>, ModelError>;

    /// Human-readable description for logs and the Preferences pane.
    fn describe(&self) -> String;
}

/// Which API dialect the configured endpoint speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Api {
    /// Ollama's native `/api/chat`, newline-delimited JSON.
    #[default]
    Ollama,
    /// `/v1/chat/completions`, server-sent events. llama.cpp's server,
    /// LM Studio, vLLM and others speak this.
    OpenAiCompatible,
    /// A model evo downloaded itself and runs in process. No server, no
    /// network, nothing to install.
    Builtin,
}

impl Api {
    pub const ALL: [Api; 3] = [Self::Ollama, Self::OpenAiCompatible, Self::Builtin];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::Builtin => "Built-in (downloaded model)",
        }
    }

    pub fn default_url(self) -> &'static str {
        match self {
            Self::Ollama => "http://localhost:11434",
            Self::OpenAiCompatible => "http://localhost:8080",
            Self::Builtin => "",
        }
    }

    /// Whether this dialect talks to a server the user has to point evo at.
    pub fn is_http(self) -> bool {
        !matches!(self, Self::Builtin)
    }
}

/// How to reach the model. Persisted with the rest of the preferences.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub api: Api,
    pub base_url: String,
    pub model: String,
    /// Which catalogue entry [`Api::Builtin`] runs. Kept alongside the HTTP
    /// settings so switching between them does not lose either.
    #[serde(default = "default_builtin_model")]
    pub builtin_model: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    120
}

fn default_builtin_model() -> String {
    crate::llm::DEFAULT_MODEL.to_owned()
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            api: Api::Ollama,
            base_url: Api::Ollama.default_url().to_owned(),
            model: "llama3.2".to_owned(),
            builtin_model: default_builtin_model(),
            timeout_secs: default_timeout(),
        }
    }
}

impl ModelConfig {
    pub fn build(&self) -> Box<dyn ModelBackend> {
        match self.api {
            Api::Builtin => builtin(&self.builtin_model),
            _ => Box::new(HttpBackend {
                config: self.clone(),
            }),
        }
    }
}

#[cfg(feature = "builtin-llm")]
fn builtin(model_id: &str) -> Box<dyn ModelBackend> {
    Box::new(crate::llm::backend::BuiltinBackend::new(model_id))
}

/// Without the `builtin-llm` feature there is no engine to run, so the
/// setting still exists but says so plainly rather than failing obscurely.
#[cfg(not(feature = "builtin-llm"))]
fn builtin(_model_id: &str) -> Box<dyn ModelBackend> {
    Box::new(UnavailableBackend)
}

#[cfg(not(feature = "builtin-llm"))]
struct UnavailableBackend;

#[cfg(not(feature = "builtin-llm"))]
impl UnavailableBackend {
    fn error() -> ModelError {
        ModelError::Unavailable(
            "this build of evo was compiled without the built-in model; \
             point Preferences ▸ Model at a local server instead"
                .to_owned(),
        )
    }
}

#[cfg(not(feature = "builtin-llm"))]
impl ModelBackend for UnavailableBackend {
    fn generate(
        &self,
        _req: &GenerateRequest,
        _on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
    ) -> Result<String, ModelError> {
        Err(Self::error())
    }

    fn list_models(&self) -> Result<Vec<String>, ModelError> {
        Ok(Vec::new())
    }

    fn describe(&self) -> String {
        "no built-in model in this build".to_owned()
    }
}

pub struct HttpBackend {
    config: ModelConfig,
}

const BUILTIN_NOT_HTTP: &str = "the built-in model does not run over HTTP";

impl HttpBackend {
    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            // Connecting to localhost either works at once or not at all, but
            // generating can take minutes on a slow model, so the two get very
            // different allowances.
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(self.config.timeout_secs)))
            .build()
            .into()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.config.base_url.trim_end_matches('/'), path)
    }
}

impl ModelBackend for HttpBackend {
    fn generate(
        &self,
        req: &GenerateRequest,
        on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
    ) -> Result<String, ModelError> {
        let (path, body) = match self.config.api {
            Api::Ollama => ("api/chat", ollama_body(req)),
            Api::OpenAiCompatible => ("v1/chat/completions", openai_body(req)),
            // `ModelConfig::build` sends this dialect elsewhere; a config that
            // reached here anyway has no server to talk to.
            Api::Builtin => return Err(ModelError::Unavailable(BUILTIN_NOT_HTTP.to_owned())),
        };
        let url = self.url(path);

        let response = self
            .agent()
            .post(&url)
            .header("content-type", "application/json")
            .send(body.to_string())
            .map_err(|e| ModelError::Unreachable {
                url: url.clone(),
                source: Box::new(e),
            })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .into_body()
                .read_to_string()
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            return Err(ModelError::Status { status, body });
        }

        // Both dialects stream line by line, so the same reader serves for
        // both; only the shape of each line differs.
        let api = self.config.api;
        let mut full = String::new();
        let reader = BufReader::new(response.into_body().into_reader());
        for line in reader.lines() {
            let line = line.map_err(|e| ModelError::Read(e.to_string()))?;
            let Some(chunk) = parse_chunk(api, &line) else {
                continue;
            };
            if !chunk.text.is_empty() {
                full.push_str(&chunk.text);
                if on_token(&chunk.text).is_break() {
                    // Dropping the reader closes the connection, which is what
                    // tells the server to stop generating.
                    return Err(ModelError::Cancelled);
                }
            }
            if chunk.done {
                break;
            }
        }
        Ok(full)
    }

    fn list_models(&self) -> Result<Vec<String>, ModelError> {
        let path = match self.config.api {
            Api::Ollama => "api/tags",
            Api::OpenAiCompatible => "v1/models",
            Api::Builtin => return Err(ModelError::Unavailable(BUILTIN_NOT_HTTP.to_owned())),
        };
        let url = self.url(path);
        let body = self
            .agent()
            .get(&url)
            .call()
            .map_err(|e| ModelError::Unreachable {
                url: url.clone(),
                source: Box::new(e),
            })?
            .into_body()
            .read_to_string()
            .map_err(|e| ModelError::Read(e.to_string()))?;
        Ok(parse_model_list(self.config.api, &body))
    }

    fn describe(&self) -> String {
        format!(
            "{} at {} ({})",
            self.config.model,
            self.config.base_url,
            self.config.api.label()
        )
    }
}

/// The conversation both dialects send: the system prompt, the earlier turns,
/// and this request's prompt as the last user message.
fn messages(req: &GenerateRequest) -> Vec<serde_json::Value> {
    let mut messages = Vec::with_capacity(req.history.len() + 2);
    if let Some(system) = &req.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    for turn in &req.history {
        messages.push(serde_json::json!({
            "role": turn.role.wire(),
            "content": turn.content,
        }));
    }
    messages.push(serde_json::json!({"role": "user", "content": req.prompt}));
    messages
}

fn ollama_body(req: &GenerateRequest) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    if let Some(t) = req.temperature {
        options.insert("temperature".into(), t.into());
    }
    if let Some(n) = req.max_tokens {
        options.insert("num_predict".into(), n.into());
    }
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages(req),
        "stream": true,
    });
    if !options.is_empty() {
        body["options"] = serde_json::Value::Object(options);
    }
    body
}

fn openai_body(req: &GenerateRequest) -> serde_json::Value {
    let messages = messages(req);
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });
    if let Some(t) = req.temperature {
        body["temperature"] = t.into();
    }
    if let Some(n) = req.max_tokens {
        body["max_tokens"] = n.into();
    }
    body
}

#[derive(Debug, PartialEq)]
pub struct Chunk {
    pub text: String,
    pub done: bool,
}

/// One line of a streaming response. `None` for keep-alives, blank lines and
/// anything unparseable -- a malformed line mid-stream shouldn't abort a
/// generation that is otherwise working.
pub fn parse_chunk(api: Api, line: &str) -> Option<Chunk> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match api {
        Api::Ollama => {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            // `/api/chat` puts the text under `message.content`; `/api/generate`
            // used a flat `response`. Older servers, and anything proxying the
            // generate endpoint, still speak the latter.
            let text = v["message"]["content"]
                .as_str()
                .or_else(|| v["response"].as_str())
                .unwrap_or_default();
            Some(Chunk {
                text: text.to_owned(),
                done: v["done"].as_bool().unwrap_or(false),
            })
        }
        Api::OpenAiCompatible => {
            let data = line.strip_prefix("data:")?.trim();
            if data == "[DONE]" {
                return Some(Chunk {
                    text: String::new(),
                    done: true,
                });
            }
            let v: serde_json::Value = serde_json::from_str(data).ok()?;
            let delta = &v["choices"][0]["delta"]["content"];
            Some(Chunk {
                text: delta.as_str().unwrap_or_default().to_owned(),
                done: v["choices"][0]["finish_reason"].is_string(),
            })
        }
        // The built-in model streams through a callback, not over the wire.
        Api::Builtin => None,
    }
}

pub fn parse_model_list(api: Api, body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let (key, field) = match api {
        Api::Ollama => ("models", "name"),
        Api::OpenAiCompatible => ("data", "id"),
        // Nothing is served, so there is no list to read.
        Api::Builtin => return Vec::new(),
    };
    v[key]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e[field].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(prompt: &str) -> GenerateRequest {
        GenerateRequest {
            model: "m".into(),
            prompt: prompt.into(),
            system: None,
            history: Vec::new(),
            temperature: None,
            max_tokens: None,
        }
    }

    #[test]
    fn ollama_chunks_carry_text_and_the_done_flag() {
        let c = parse_chunk(Api::Ollama, r#"{"message":{"content":"Hel"},"done":false}"#)
            .expect("a chunk");
        assert_eq!(c.text, "Hel");
        assert!(!c.done);

        let c =
            parse_chunk(Api::Ollama, r#"{"message":{"content":""},"done":true}"#).expect("a chunk");
        assert!(c.done);
    }

    /// `/api/generate`'s flat field still reads, so a server (or proxy) that
    /// answers in the old shape keeps working.
    #[test]
    fn ollama_still_reads_the_generate_endpoints_flat_field() {
        let c = parse_chunk(Api::Ollama, r#"{"response":"Hel","done":false}"#).expect("a chunk");
        assert_eq!(c.text, "Hel");
        assert!(!c.done);

        // When both are present the chat shape wins.
        let both = r#"{"message":{"content":"chat"},"response":"generate","done":false}"#;
        assert_eq!(
            parse_chunk(Api::Ollama, both).expect("a chunk").text,
            "chat"
        );
    }

    #[test]
    fn openai_chunks_are_read_from_the_data_field() {
        let line = r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#;
        let c = parse_chunk(Api::OpenAiCompatible, line).expect("a chunk");
        assert_eq!(c.text, "Hel");
        assert!(!c.done);
    }

    #[test]
    fn openai_signals_the_end_with_a_sentinel() {
        let c = parse_chunk(Api::OpenAiCompatible, "data: [DONE]").expect("a chunk");
        assert!(c.done);
        assert!(c.text.is_empty());
    }

    #[test]
    fn a_finish_reason_ends_the_stream() {
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let c = parse_chunk(Api::OpenAiCompatible, line).expect("a chunk");
        assert!(c.done);
    }

    #[test]
    fn blank_and_malformed_lines_are_skipped_not_fatal() {
        for line in ["", "   ", "not json", "data: {oops"] {
            assert!(parse_chunk(Api::Ollama, line).is_none(), "{line:?}");
            assert!(
                parse_chunk(Api::OpenAiCompatible, line).is_none(),
                "{line:?}"
            );
        }
        // Server-sent-event comments are keep-alives, not content.
        assert!(parse_chunk(Api::OpenAiCompatible, ": ping").is_none());
    }

    #[test]
    fn model_lists_are_read_from_either_dialect() {
        let ollama = r#"{"models":[{"name":"llama3.2"},{"name":"qwen2.5"}]}"#;
        assert_eq!(
            parse_model_list(Api::Ollama, ollama),
            ["llama3.2", "qwen2.5"]
        );

        let openai = r#"{"data":[{"id":"local-model"}]}"#;
        assert_eq!(
            parse_model_list(Api::OpenAiCompatible, openai),
            ["local-model"]
        );
    }

    #[test]
    fn an_unreadable_model_list_is_empty_rather_than_an_error() {
        assert!(parse_model_list(Api::Ollama, "<html>404</html>").is_empty());
    }

    #[test]
    fn the_request_body_carries_the_options_that_were_set() {
        let mut req = request("p");
        req.system = Some("s".into());
        req.temperature = Some(0.5);
        req.max_tokens = Some(64);

        let body = ollama_body(&req);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "s");
        assert_eq!(body["options"]["num_predict"], 64);
        assert_eq!(body["stream"], true);

        let body = openai_body(&req);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "p");
        assert_eq!(body["max_tokens"], 64);
    }

    #[test]
    fn unset_options_are_left_out_entirely() {
        let body = ollama_body(&request("p"));
        assert!(body.get("options").is_none());
        // With no system prompt the only message is the user's.
        assert_eq!(body["messages"].as_array().expect("an array").len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn both_dialects_interleave_the_history_before_the_new_prompt() {
        let mut req = request("and the second?");
        req.system = Some("s".into());
        req.history = vec![
            ChatMessage::new(Role::User, "what is on the first page?"),
            ChatMessage::new(Role::Assistant, "A title. [p.1]"),
        ];

        for body in [ollama_body(&req), openai_body(&req)] {
            let messages = body["messages"].as_array().expect("an array");
            let pairs: Vec<(&str, &str)> = messages
                .iter()
                .map(|m| {
                    (
                        m["role"].as_str().unwrap_or_default(),
                        m["content"].as_str().unwrap_or_default(),
                    )
                })
                .collect();
            assert_eq!(
                pairs,
                [
                    ("system", "s"),
                    ("user", "what is on the first page?"),
                    ("assistant", "A title. [p.1]"),
                    ("user", "and the second?"),
                ]
            );
        }
    }

    /// A one-shot HTTP server that answers the first request with `reply` and
    /// hands back what it was asked. Enough of a server to prove which endpoint
    /// the backend posts to and that it reads a streamed body -- the parser
    /// tests above cover the shapes, this covers the plumbing between them.
    fn serve_once(reply: &'static str) -> (String, std::thread::JoinHandle<(String, String)>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a connection");
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("a request line");

            let mut length = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("a header");
                if header.trim().is_empty() {
                    break;
                }
                if let Some(value) = header
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                {
                    length = value.parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("the body");

            let mut stream = reader.into_inner();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\
                         Connection: close\r\n\r\n{reply}"
                    )
                    .as_bytes(),
                )
                .expect("write the reply");
            stream.flush().ok();
            (
                request_line.trim().to_owned(),
                String::from_utf8_lossy(&body).into_owned(),
            )
        });
        (url, handle)
    }

    #[test]
    fn the_ollama_backend_posts_a_conversation_to_the_chat_endpoint() {
        let reply = "{\"message\":{\"content\":\"The alarm \"},\"done\":false}\n\
                     {\"message\":{\"content\":\"panel. [p.3]\"},\"done\":false}\n\
                     {\"message\":{\"content\":\"\"},\"done\":true}\n";
        let (url, server) = serve_once(reply);

        let backend = ModelConfig {
            api: Api::Ollama,
            base_url: url,
            // The request names the model; the config only supplies the
            // default the caller starts from.
            model: "from-config".into(),
            timeout_secs: 10,
            ..Default::default()
        }
        .build();
        let mut req = request("where is it?");
        req.model = "test-model".into();
        req.system = Some("answer from the pages".into());
        req.history = vec![ChatMessage::new(Role::User, "hello")];

        let mut streamed = Vec::new();
        let text = backend
            .generate(&req, &mut |chunk: &str| {
                streamed.push(chunk.to_owned());
                ControlFlow::Continue(())
            })
            .expect("a completion");

        assert_eq!(text, "The alarm panel. [p.3]");
        assert_eq!(streamed, ["The alarm ", "panel. [p.3]"]);

        let (request_line, body) = server.join().expect("the server thread");
        assert_eq!(request_line, "POST /api/chat HTTP/1.1");
        let sent: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(sent["model"], "test-model");
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][1]["content"], "hello");
        assert_eq!(sent["messages"][2]["content"], "where is it?");
    }

    #[test]
    fn breaking_out_of_the_stream_reports_a_cancellation() {
        let reply = "{\"message\":{\"content\":\"one\"},\"done\":false}\n\
                     {\"message\":{\"content\":\"two\"},\"done\":false}\n";
        let (url, server) = serve_once(reply);
        let backend = ModelConfig {
            api: Api::Ollama,
            base_url: url,
            model: "m".into(),
            timeout_secs: 10,
            ..Default::default()
        }
        .build();

        let mut seen = 0;
        let result = backend.generate(&request("q"), &mut |_: &str| {
            seen += 1;
            ControlFlow::Break(())
        });
        assert!(matches!(result, Err(ModelError::Cancelled)));
        assert_eq!(seen, 1, "the stream stopped at the first chunk");
        let _ = server.join();
    }

    /// Preferences saved before the built-in model existed must still load,
    /// with the default model selected.
    #[test]
    fn older_preferences_load_with_the_default_builtin_model() {
        let saved = r#"{"api":"Ollama","base_url":"http://localhost:11434","model":"llama3.2"}"#;
        let config: ModelConfig = serde_json::from_str(saved).expect("deserialize");
        assert_eq!(config.builtin_model, crate::llm::DEFAULT_MODEL);
        assert_eq!(config.timeout_secs, default_timeout());
        assert_eq!(config.api, Api::Ollama);
    }

    #[test]
    fn every_dialect_is_listed_once_with_a_label() {
        assert_eq!(Api::ALL.len(), 3);
        for api in Api::ALL {
            assert!(!api.label().is_empty());
            assert_eq!(Api::ALL.iter().filter(|a| **a == api).count(), 1);
        }
        // Only the served dialects have an address to point at.
        assert!(Api::Ollama.is_http() && Api::OpenAiCompatible.is_http());
        assert!(!Api::Builtin.is_http());
        assert!(Api::Builtin.default_url().is_empty());
    }

    /// Whichever way evo was built, asking the built-in backend for something
    /// it cannot do must produce a sentence, not a panic. With the feature on
    /// and nothing downloaded that is "not downloaded yet"; with it off it is
    /// "compiled without".
    #[test]
    fn an_unusable_builtin_model_explains_itself() {
        let config = ModelConfig {
            api: Api::Builtin,
            builtin_model: "no-such-model".into(),
            ..Default::default()
        };
        let err = config
            .build()
            .generate(&request("hello"), &mut |_: &str| ControlFlow::Continue(()))
            .expect_err("no such model");
        assert!(matches!(err, ModelError::Unavailable(_)), "{err}");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn roles_round_trip_through_storage_under_their_wire_names() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).expect("serialize");
            assert_eq!(json, format!("\"{}\"", role.wire()));
            assert_eq!(
                serde_json::from_str::<Role>(&json).expect("deserialize"),
                role
            );
        }
    }
}
