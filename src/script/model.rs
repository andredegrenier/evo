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
    /// Tools this assistant turn asked to run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For a [`Role::Tool`] turn: which call this is the result of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For a [`Role::Tool`] turn: which tool produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    /// The assistant turn that asked for tools. The text is whatever it said
    /// alongside the request, which is often nothing.
    pub fn calling(text: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls: calls,
            ..Self::new(Role::Assistant, text)
        }
    }

    /// What one tool answered, as the model should see it.
    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: call.id.clone(),
            name: Some(call.name.clone()),
            ..Self::new(Role::Tool, content)
        }
    }
}

/// A tool the model may ask for, described the way both dialects want it: a
/// name, a sentence, and a JSON Schema for the arguments.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema object. `{"type":"object","properties":{…}}`.
    pub parameters: serde_json::Value,
}

impl ToolDef {
    /// The shape both dialects put in their `tools` array.
    pub fn wire(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// The model asking for one tool to be run.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    /// The dialect's own handle for this call, when it gave one. OpenAI needs
    /// it back on the result message; Ollama does not use one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    /// Always an object once parsed; `{}` when the model sent nothing usable.
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            id: None,
            name: name.into(),
            arguments,
        }
    }

    /// The arguments as the wire wants them: OpenAI carries them as a JSON
    /// *string*, Ollama as an object.
    pub fn arguments_json(&self) -> String {
        self.arguments.to_string()
    }
}

/// What one generation produced: text, and whatever tools it asked for.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct GenerateOutcome {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

impl GenerateOutcome {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tool_calls: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct GenerateRequest {
    pub model: String,
    /// The question being asked. Empty when the conversation in `history` is
    /// already up to date -- the agent loop continues that way after a tool
    /// has answered.
    pub prompt: String,
    pub system: Option<String>,
    /// Earlier turns, oldest first. Empty for a one-shot completion.
    pub history: Vec<ChatMessage>,
    /// Tools the model may ask for. Empty means it is only being asked for
    /// text, and no dialect is told about tools at all.
    pub tools: Vec<ToolDef>,
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
    ) -> Result<GenerateOutcome, ModelError>;

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
    ) -> Result<GenerateOutcome, ModelError> {
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
    ) -> Result<GenerateOutcome, ModelError> {
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
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut pending = PendingCalls::default();
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
            // Ollama sends a call in one piece; OpenAI dribbles it out.
            tool_calls.extend(chunk.tool_calls);
            for fragment in &chunk.fragments {
                pending.add(fragment);
            }
            if chunk.done {
                break;
            }
        }
        tool_calls.extend(pending.finish());
        Ok(GenerateOutcome {
            tool_calls,
            ..GenerateOutcome::text(full)
        })
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
/// and this request's prompt as the last user message. An empty prompt adds no
/// message -- the agent loop continues a conversation that already ends where
/// the model should carry on from.
fn messages(req: &GenerateRequest, api: Api) -> Vec<serde_json::Value> {
    let mut messages = Vec::with_capacity(req.history.len() + 2);
    if let Some(system) = &req.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    for turn in &req.history {
        messages.push(turn_json(turn, api));
    }
    if !req.prompt.is_empty() {
        messages.push(serde_json::json!({"role": "user", "content": req.prompt}));
    }
    messages
}

/// One turn on the wire. Plain text turns are identical in both dialects; the
/// ones that carry a tool call or its result are not.
fn turn_json(turn: &ChatMessage, api: Api) -> serde_json::Value {
    let mut value = serde_json::json!({
        "role": turn.role.wire(),
        "content": turn.content,
    });
    if !turn.tool_calls.is_empty() {
        let calls: Vec<serde_json::Value> = turn
            .tool_calls
            .iter()
            .map(|call| match api {
                // OpenAI identifies each call and carries the arguments as a
                // string; the result message quotes the id back.
                Api::OpenAiCompatible => serde_json::json!({
                    "id": call.id.clone().unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments_json(),
                    }
                }),
                _ => serde_json::json!({
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    }
                }),
            })
            .collect();
        value["tool_calls"] = serde_json::Value::Array(calls);
    }
    if turn.role == Role::Tool {
        match api {
            Api::OpenAiCompatible => {
                value["tool_call_id"] =
                    serde_json::Value::String(turn.tool_call_id.clone().unwrap_or_default());
            }
            // Ollama names the tool instead: it never issued an id to quote.
            _ => {
                if let Some(name) = &turn.name {
                    value["tool_name"] = serde_json::Value::String(name.clone());
                }
            }
        }
    }
    value
}

fn tools_json(tools: &[ToolDef]) -> serde_json::Value {
    serde_json::Value::Array(tools.iter().map(ToolDef::wire).collect())
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
        "messages": messages(req, Api::Ollama),
        "stream": true,
    });
    if !options.is_empty() {
        body["options"] = serde_json::Value::Object(options);
    }
    if !req.tools.is_empty() {
        body["tools"] = tools_json(&req.tools);
    }
    body
}

fn openai_body(req: &GenerateRequest) -> serde_json::Value {
    let messages = messages(req, Api::OpenAiCompatible);
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
    if !req.tools.is_empty() {
        body["tools"] = tools_json(&req.tools);
        // Let the model decide; evo never insists on a particular tool.
        body["tool_choice"] = serde_json::Value::String("auto".to_owned());
    }
    body
}

/// Part of one tool call, as OpenAI streams them: fragments identified by
/// position in the call list, with the arguments arriving a few characters at
/// a time.
#[derive(Debug, Default, PartialEq)]
pub struct ToolCallFragment {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}

/// Tool calls being assembled from fragments.
#[derive(Debug, Default)]
pub struct PendingCalls {
    /// Keyed by the index the dialect gave, so calls come out in the order the
    /// model asked for them however the fragments interleave.
    calls: std::collections::BTreeMap<usize, PendingCall>,
}

#[derive(Debug, Default)]
struct PendingCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl PendingCalls {
    pub fn add(&mut self, fragment: &ToolCallFragment) {
        let call = self.calls.entry(fragment.index).or_default();
        // The id and name arrive once, in the first fragment of a call; later
        // fragments repeat neither, and must not blank them.
        if let Some(id) = &fragment.id
            && !id.is_empty()
        {
            call.id = Some(id.clone());
        }
        if let Some(name) = &fragment.name
            && !name.is_empty()
        {
            call.name.push_str(name);
        }
        call.arguments.push_str(&fragment.arguments);
    }

    /// The finished calls. Arguments that do not parse become `{}`: a tool
    /// that is asked for with nonsense arguments should fail in the tool, with
    /// a message the model can read, rather than take the whole answer down.
    pub fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_values()
            .filter(|call| !call.name.is_empty())
            .map(|call| ToolCall {
                id: call.id,
                ..ToolCall::new(call.name, parse_arguments(&call.arguments))
            })
            .collect()
    }
}

/// Arguments as an object, whether the dialect sent an object or a string of
/// JSON, and `{}` for anything else.
fn parse_arguments(value: &str) -> serde_json::Value {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({}),
    }
}

/// The same, for a value that may already be parsed.
fn arguments_of(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(_) => value.clone(),
        serde_json::Value::String(s) => parse_arguments(s),
        _ => serde_json::json!({}),
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Chunk {
    pub text: String,
    pub done: bool,
    /// Calls that arrived complete (Ollama sends them in one piece).
    pub tool_calls: Vec<ToolCall>,
    /// Pieces of calls still being assembled (OpenAI streams them).
    pub fragments: Vec<ToolCallFragment>,
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
                tool_calls: ollama_tool_calls(&v["message"]["tool_calls"]),
                fragments: Vec::new(),
            })
        }
        Api::OpenAiCompatible => {
            let data = line.strip_prefix("data:")?.trim();
            if data == "[DONE]" {
                return Some(Chunk {
                    done: true,
                    ..Default::default()
                });
            }
            let v: serde_json::Value = serde_json::from_str(data).ok()?;
            let delta = &v["choices"][0]["delta"]["content"];
            Some(Chunk {
                text: delta.as_str().unwrap_or_default().to_owned(),
                done: v["choices"][0]["finish_reason"].is_string(),
                tool_calls: Vec::new(),
                fragments: openai_fragments(&v["choices"][0]["delta"]["tool_calls"]),
            })
        }
        // The built-in model streams through a callback, not over the wire.
        Api::Builtin => None,
    }
}

/// Ollama's `message.tool_calls`: whole calls, arguments already an object.
fn ollama_tool_calls(value: &serde_json::Value) -> Vec<ToolCall> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let function = &item["function"];
            let name = function["name"].as_str()?;
            if name.is_empty() {
                return None;
            }
            Some(ToolCall {
                id: item["id"].as_str().map(str::to_owned),
                name: name.to_owned(),
                arguments: arguments_of(&function["arguments"]),
            })
        })
        .collect()
}

/// OpenAI's `delta.tool_calls`: fragments, keyed by index.
fn openai_fragments(value: &serde_json::Value) -> Vec<ToolCallFragment> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .map(|(position, item)| {
            let function = &item["function"];
            ToolCallFragment {
                // A server that omits the index has one call per delta.
                index: item["index"].as_u64().map_or(position, |i| i as usize),
                id: item["id"].as_str().map(str::to_owned),
                name: function["name"].as_str().map(str::to_owned),
                arguments: function["arguments"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            }
        })
        .collect()
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
            ..Default::default()
        }
    }

    fn weather_tool() -> ToolDef {
        ToolDef {
            name: "get_weather".into(),
            description: "Look up the weather in a city".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            }),
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
        let outcome = backend
            .generate(&req, &mut |chunk: &str| {
                streamed.push(chunk.to_owned());
                ControlFlow::Continue(())
            })
            .expect("a completion");

        assert_eq!(outcome.text, "The alarm panel. [p.3]");
        assert!(outcome.tool_calls.is_empty());
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

    // --- tools -----------------------------------------------------------

    #[test]
    fn a_request_without_tools_says_nothing_about_them() {
        for body in [ollama_body(&request("p")), openai_body(&request("p"))] {
            assert!(body.get("tools").is_none(), "{body}");
            assert!(body.get("tool_choice").is_none(), "{body}");
        }
    }

    #[test]
    fn tools_are_offered_in_the_shape_each_dialect_wants() {
        let mut req = request("what is the weather in London?");
        req.tools = vec![weather_tool()];

        for (api, body) in [
            (Api::Ollama, ollama_body(&req)),
            (Api::OpenAiCompatible, openai_body(&req)),
        ] {
            let tools = body["tools"].as_array().expect("an array");
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["type"], "function");
            assert_eq!(tools[0]["function"]["name"], "get_weather");
            assert_eq!(
                tools[0]["function"]["description"],
                "Look up the weather in a city"
            );
            assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
            // Only OpenAI takes a choice, and evo never insists.
            match api {
                Api::OpenAiCompatible => assert_eq!(body["tool_choice"], "auto"),
                _ => assert!(body.get("tool_choice").is_none()),
            }
        }
    }

    /// A tool round trip in the history: the assistant asked, the tool
    /// answered. OpenAI threads an id through both; Ollama names the tool.
    #[test]
    fn a_finished_tool_round_trip_serializes_per_dialect() {
        let call = ToolCall {
            id: Some("call_1".into()),
            name: "get_weather".into(),
            arguments: serde_json::json!({"city": "London"}),
        };
        let mut req = request("");
        req.history = vec![
            ChatMessage::new(Role::User, "weather in London?"),
            ChatMessage::calling("", vec![call.clone()]),
            ChatMessage::tool_result(&call, "17°C and raining"),
        ];

        let body = openai_body(&req);
        let messages = body["messages"].as_array().expect("an array");
        assert_eq!(messages.len(), 3, "an empty prompt adds no message");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[1]["tool_calls"][0]["type"], "function");
        // OpenAI carries the arguments as a string of JSON, not an object.
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"city":"London"}"#
        );
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[2]["content"], "17°C and raining");

        let body = ollama_body(&req);
        let messages = body["messages"].as_array().expect("an array");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        // Ollama wants an object, and issues no id to quote back.
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"]["city"],
            "London"
        );
        assert!(messages[1]["tool_calls"][0].get("id").is_none());
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_name"], "get_weather");
        assert!(messages[2].get("tool_call_id").is_none());
    }

    #[test]
    fn ollama_sends_a_tool_call_in_one_piece() {
        let line = r#"{"message":{"content":"","tool_calls":[{"function":
            {"name":"get_weather","arguments":{"city":"London"}}}]},"done":false}"#;
        let chunk = parse_chunk(Api::Ollama, line).expect("a chunk");
        assert!(chunk.text.is_empty());
        assert!(chunk.fragments.is_empty());
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].name, "get_weather");
        assert_eq!(chunk.tool_calls[0].arguments["city"], "London");
        assert!(chunk.tool_calls[0].id.is_none());
    }

    /// Some builds hand back the arguments as a string, and some attach an id.
    #[test]
    fn ollama_tolerates_string_arguments_and_an_id() {
        let line = r#"{"message":{"tool_calls":[{"id":"abc","function":
            {"name":"get_weather","arguments":"{\"city\":\"Oslo\"}"}}]},"done":true}"#;
        let chunk = parse_chunk(Api::Ollama, line).expect("a chunk");
        assert_eq!(chunk.tool_calls[0].id.as_deref(), Some("abc"));
        assert_eq!(chunk.tool_calls[0].arguments["city"], "Oslo");
        assert!(chunk.done);

        // Nonsense arguments become an empty object rather than a failure.
        let line = r#"{"message":{"tool_calls":[{"function":
            {"name":"get_weather","arguments":"not json"}}]},"done":true}"#;
        let chunk = parse_chunk(Api::Ollama, line).expect("a chunk");
        assert_eq!(chunk.tool_calls[0].arguments, serde_json::json!({}));

        // A call with no name is not a call.
        let line = r#"{"message":{"tool_calls":[{"function":{"arguments":{}}}]},"done":true}"#;
        assert!(
            parse_chunk(Api::Ollama, line)
                .expect("a chunk")
                .tool_calls
                .is_empty()
        );
    }

    /// OpenAI's fragments are the fiddly part: an id and a name once, then the
    /// arguments a few characters at a time, possibly for several calls at
    /// once. Each row is a captured stream and what it must add up to.
    #[test]
    fn openai_tool_call_fragments_accumulate_into_whole_calls() {
        struct Case {
            name: &'static str,
            lines: &'static [&'static str],
            want: Vec<ToolCall>,
            want_text: &'static str,
        }

        let cases = vec![
            Case {
                name: "one call, arguments in pieces",
                lines: &[
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"ci"}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\": \"Lon"}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"don\"}"}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ],
                want: vec![ToolCall {
                    id: Some("call_a".into()),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({"city": "London"}),
                }],
                want_text: "",
            },
            Case {
                name: "two calls interleaved by index",
                lines: &[
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"first","arguments":"{\"x\":"}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"second","arguments":"{\"y\":"}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2}"}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
                    r#"data: [DONE]"#,
                ],
                want: vec![
                    ToolCall {
                        id: Some("a".into()),
                        name: "first".into(),
                        arguments: serde_json::json!({"x": 1}),
                    },
                    ToolCall {
                        id: Some("b".into()),
                        name: "second".into(),
                        arguments: serde_json::json!({"y": 2}),
                    },
                ],
                want_text: "",
            },
            Case {
                name: "text first, then a call",
                lines: &[
                    r#"data: {"choices":[{"delta":{"content":"Let me look. "}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"get_weather","arguments":"{}"}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ],
                want: vec![ToolCall {
                    id: Some("c".into()),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({}),
                }],
                want_text: "Let me look. ",
            },
            Case {
                name: "arguments never finish, so they are empty rather than wrong",
                lines: &[
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"d","function":{"name":"get_weather","arguments":"{\"city\": \"Lon"}}]}}]}"#,
                    r#"data: [DONE]"#,
                ],
                want: vec![ToolCall {
                    id: Some("d".into()),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({}),
                }],
                want_text: "",
            },
            Case {
                name: "a name split across fragments",
                lines: &[
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"e","function":{"name":"get_","arguments":""}}]}}]}"#,
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"weather","arguments":"{}"}}]}}]}"#,
                    r#"data: [DONE]"#,
                ],
                want: vec![ToolCall {
                    id: Some("e".into()),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({}),
                }],
                want_text: "",
            },
            Case {
                name: "no index: one call per delta",
                lines: &[
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"id":"f","function":{"name":"only","arguments":"{\"n\":1}"}}]}}]}"#,
                    r#"data: [DONE]"#,
                ],
                want: vec![ToolCall {
                    id: Some("f".into()),
                    name: "only".into(),
                    arguments: serde_json::json!({"n": 1}),
                }],
                want_text: "",
            },
            Case {
                name: "plain answer, no calls at all",
                lines: &[
                    r#"data: {"choices":[{"delta":{"content":"17°C."}}]}"#,
                    r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                ],
                want: Vec::new(),
                want_text: "17°C.",
            },
        ];

        for case in cases {
            let mut pending = PendingCalls::default();
            let mut text = String::new();
            for line in case.lines {
                let Some(chunk) = parse_chunk(Api::OpenAiCompatible, line) else {
                    panic!("{}: {line} did not parse", case.name);
                };
                text.push_str(&chunk.text);
                for fragment in &chunk.fragments {
                    pending.add(fragment);
                }
                assert!(
                    chunk.tool_calls.is_empty(),
                    "{}: this dialect streams fragments",
                    case.name
                );
            }
            assert_eq!(text, case.want_text, "{}", case.name);
            assert_eq!(pending.finish(), case.want, "{}", case.name);
        }
    }

    /// The whole way through the backend, not just the parser: fragments over
    /// the wire come back as one finished call.
    #[test]
    fn the_openai_backend_returns_an_accumulated_tool_call() {
        let reply = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"One moment. \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\
             \"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
             {\"arguments\":\"{\\\"city\\\": \\\"London\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (url, server) = serve_once(reply);
        let backend = ModelConfig {
            api: Api::OpenAiCompatible,
            base_url: url,
            model: "m".into(),
            timeout_secs: 10,
            ..Default::default()
        }
        .build();

        let mut req = request("weather in London?");
        req.tools = vec![weather_tool()];
        let outcome = backend
            .generate(&req, &mut |_: &str| ControlFlow::Continue(()))
            .expect("a completion");

        assert_eq!(outcome.text, "One moment. ");
        assert_eq!(outcome.tool_calls.len(), 1);
        assert_eq!(outcome.tool_calls[0].name, "get_weather");
        assert_eq!(outcome.tool_calls[0].arguments["city"], "London");
        assert_eq!(outcome.tool_calls[0].id.as_deref(), Some("call_1"));

        let (request_line, body) = server.join().expect("the server thread");
        assert_eq!(request_line, "POST /v1/chat/completions HTTP/1.1");
        let sent: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(sent["tool_choice"], "auto");
    }

    /// Transcripts saved before tool calling must still load, and a message
    /// with no tools must not grow empty fields in storage.
    #[test]
    fn stored_messages_predating_tools_still_load() {
        let old = r#"{"role":"assistant","content":"A report. [p.1]"}"#;
        let message: ChatMessage = serde_json::from_str(old).expect("deserialize");
        assert_eq!(message.content, "A report. [p.1]");
        assert!(message.tool_calls.is_empty());
        assert!(message.tool_call_id.is_none() && message.name.is_none());

        let written = serde_json::to_string(&message).expect("serialize");
        assert_eq!(written, old, "nothing empty is written out");

        let call = ToolCall::new("get_weather", serde_json::json!({"city": "Oslo"}));
        let calling = ChatMessage::calling("", vec![call.clone()]);
        let round: ChatMessage =
            serde_json::from_str(&serde_json::to_string(&calling).expect("serialize"))
                .expect("deserialize");
        assert_eq!(round, calling);

        let result = ChatMessage::tool_result(&call, "17°C");
        assert_eq!(result.role, Role::Tool);
        assert_eq!(result.name.as_deref(), Some("get_weather"));
    }
}
