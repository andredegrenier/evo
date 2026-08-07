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
    #[error("cancelled")]
    Cancelled,
}

pub struct GenerateRequest {
    pub model: String,
    pub prompt: String,
    pub system: Option<String>,
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
    /// Ollama's native `/api/generate`, newline-delimited JSON.
    #[default]
    Ollama,
    /// `/v1/chat/completions`, server-sent events. llama.cpp's server,
    /// LM Studio, vLLM and others speak this.
    OpenAiCompatible,
}

impl Api {
    pub const ALL: [Api; 2] = [Self::Ollama, Self::OpenAiCompatible];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::OpenAiCompatible => "OpenAI-compatible",
        }
    }

    pub fn default_url(self) -> &'static str {
        match self {
            Self::Ollama => "http://localhost:11434",
            Self::OpenAiCompatible => "http://localhost:8080",
        }
    }
}

/// How to reach the model. Persisted with the rest of the preferences.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub api: Api,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    120
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            api: Api::Ollama,
            base_url: Api::Ollama.default_url().to_owned(),
            model: "llama3.2".to_owned(),
            timeout_secs: default_timeout(),
        }
    }
}

impl ModelConfig {
    pub fn build(&self) -> Box<dyn ModelBackend> {
        Box::new(HttpBackend {
            config: self.clone(),
        })
    }
}

pub struct HttpBackend {
    config: ModelConfig,
}

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
            Api::Ollama => ("api/generate", ollama_body(req)),
            Api::OpenAiCompatible => ("v1/chat/completions", openai_body(req)),
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
        "prompt": req.prompt,
        "stream": true,
    });
    if let Some(system) = &req.system {
        body["system"] = system.clone().into();
    }
    if !options.is_empty() {
        body["options"] = serde_json::Value::Object(options);
    }
    body
}

fn openai_body(req: &GenerateRequest) -> serde_json::Value {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    messages.push(serde_json::json!({"role": "user", "content": req.prompt}));
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
            Some(Chunk {
                text: v["response"].as_str().unwrap_or_default().to_owned(),
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
    }
}

pub fn parse_model_list(api: Api, body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let key = match api {
        Api::Ollama => "models",
        Api::OpenAiCompatible => "data",
    };
    let field = match api {
        Api::Ollama => "name",
        Api::OpenAiCompatible => "id",
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

    #[test]
    fn ollama_chunks_carry_text_and_the_done_flag() {
        let c = parse_chunk(Api::Ollama, r#"{"response":"Hel","done":false}"#).expect("a chunk");
        assert_eq!(c.text, "Hel");
        assert!(!c.done);

        let c = parse_chunk(Api::Ollama, r#"{"response":"","done":true}"#).expect("a chunk");
        assert!(c.done);
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
        let req = GenerateRequest {
            model: "m".into(),
            prompt: "p".into(),
            system: Some("s".into()),
            temperature: Some(0.5),
            max_tokens: Some(64),
        };
        let body = ollama_body(&req);
        assert_eq!(body["system"], "s");
        assert_eq!(body["options"]["num_predict"], 64);
        assert_eq!(body["stream"], true);

        let body = openai_body(&req);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "p");
        assert_eq!(body["max_tokens"], 64);
    }

    #[test]
    fn unset_options_are_left_out_entirely() {
        let req = GenerateRequest {
            model: "m".into(),
            prompt: "p".into(),
            system: None,
            temperature: None,
            max_tokens: None,
        };
        let body = ollama_body(&req);
        assert!(body.get("system").is_none());
        assert!(body.get("options").is_none());
    }
}
