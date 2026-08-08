//! Running a downloaded GGUF model in process, through llama.cpp.
//!
//! This is the second implementation of [`ModelBackend`]: same trait, same
//! streaming callback and same cancellation as the HTTP one, so nothing above
//! it -- chat, scripts, the Lua API -- knows which is answering.
//!
//! Loading weights costs seconds and a couple of gigabytes, so the model is
//! kept alive between requests in a process-wide cache of exactly one. The
//! *context* (the KV cache) is not: a fresh one per request is cheap next to
//! the load and means no conversation can leak into the next.
//!
//! All FFI stays inside `llama-cpp-2`; there is no `unsafe` here.

use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use crate::script::model::{GenerateOutcome, GenerateRequest, ModelBackend, ModelError, Role};

use super::toolfmt;

/// How much of a conversation the model is given. Larger costs memory: the KV
/// cache is allocated for the whole window whether it is used or not.
const N_CTX: u32 = 8192;

/// Tokens generated when the caller does not say.
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Batch size for feeding the prompt in. Also the context's `n_batch`.
const N_BATCH: usize = 512;

const TOP_P: f32 = 0.9;
const DEFAULT_TEMPERATURE: f32 = 0.7;

/// llama.cpp's "pick a seed for me" sentinel.
const DEFAULT_SEED: u32 = u32::MAX;

/// llama.cpp's global init. It is a process-wide resource, so it is created
/// once and never dropped; the error (if the backend refuses to start) is
/// remembered rather than retried.
fn backend() -> Result<&'static LlamaBackend, ModelError> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            let mut backend = LlamaBackend::init().map_err(|e| e.to_string())?;
            // llama.cpp writes a great deal to stderr; a windowed application
            // has nowhere to show it and every failure here comes back as a
            // `Result` anyway.
            backend.void_logs();
            Ok(backend)
        })
        .as_ref()
        .map_err(|e| ModelError::Unavailable(format!("could not start llama.cpp: {e}")))
}

/// The last model loaded. Weights are megabytes to gigabytes, so a second
/// request for the same file reuses them; a request for a different one
/// replaces the entry and lets the old weights go.
static LOADED: Mutex<Option<(PathBuf, Arc<LlamaModel>)>> = Mutex::new(None);

fn load(path: &PathBuf) -> Result<Arc<LlamaModel>, ModelError> {
    let backend = backend()?;
    let mut slot = LOADED.lock().unwrap();
    if let Some((loaded, model)) = &*slot
        && loaded == path
    {
        return Ok(model.clone());
    }
    // Drop the previous model before loading the next, so two sets of weights
    // are never resident at once.
    *slot = None;
    let model = LlamaModel::load_from_file(backend, path, &LlamaModelParams::default())
        .map_err(|e| ModelError::Unavailable(format!("could not load {}: {e}", path.display())))?;
    let model = Arc::new(model);
    *slot = Some((path.clone(), model.clone()));
    Ok(model)
}

/// Let the cached weights go.
///
/// This has to happen before the process exits. llama.cpp's Metal backend
/// tears itself down from a C++ static destructor and asserts that nothing is
/// still holding its buffers; weights parked in a `static` for the life of the
/// program are exactly that, and the assert aborts the process on the way out.
/// So the application drops them itself when it is closing.
pub fn unload() {
    if let Ok(mut slot) = LOADED.lock() {
        *slot = None;
    }
}

/// Answers from a GGUF file on disk.
pub struct BuiltinBackend {
    /// Catalogue id, resolved to a path at generation time so that a model
    /// downloaded after the backend was built is picked up.
    model_id: String,
}

impl BuiltinBackend {
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
        }
    }

    fn model_path(&self) -> Result<PathBuf, ModelError> {
        let Some(entry) = super::entry(&self.model_id) else {
            return Err(ModelError::Unavailable(format!(
                "“{}” is not one of the built-in models",
                self.model_id
            )));
        };
        let dir = super::llm_models_dir().ok_or_else(|| {
            ModelError::Unavailable("could not find evo's data directory".to_owned())
        })?;
        entry.installed_in(&dir).ok_or_else(|| {
            ModelError::Unavailable(format!(
                "{} has not been downloaded yet — Preferences ▸ Model",
                entry.label
            ))
        })
    }
}

impl ModelBackend for BuiltinBackend {
    fn generate(
        &self,
        req: &GenerateRequest,
        on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
    ) -> Result<GenerateOutcome, ModelError> {
        let path = self.model_path()?;
        let model = load(&path)?;

        let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(1);
        // Everything that is not reserved for the answer is available to the
        // prompt, less a little slack for the template's own tokens.
        let budget = N_CTX.saturating_sub(max_tokens + 64).max(256) as usize;
        let tokens = fit_prompt(&model, req, budget)?;

        let threads = threads();
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX))
            .with_n_batch(N_BATCH as u32)
            .with_n_threads(threads)
            .with_n_threads_batch(threads);
        let mut ctx = model
            .new_context(backend()?, ctx_params)
            .map_err(|e| ModelError::Unavailable(format!("could not start the model: {e}")))?;

        let mut batch = LlamaBatch::new(N_BATCH, 1);
        let mut pos = 0i32;
        let last = tokens.len() - 1;
        for chunk in tokens.chunks(N_BATCH) {
            batch.clear();
            for token in chunk {
                let index = pos as usize;
                batch
                    .add(*token, pos, &[0], index == last)
                    .map_err(|e| ModelError::Read(e.to_string()))?;
                pos += 1;
            }
            ctx.decode(&mut batch).map_err(|e| {
                ModelError::Read(format!("the model could not read the prompt: {e}"))
            })?;
        }

        let mut sampler = sampler(req.temperature.unwrap_or(DEFAULT_TEMPERATURE));
        let mut text = String::new();
        // Tokens are pieces of UTF-8, not characters: a piece can end
        // mid-sequence and only complete with the next one.
        let mut pending: Vec<u8> = Vec::new();
        let mut produced = 0u32;
        // When tools are on offer the answer may contain `<tool_call>` blocks,
        // which are addressed to evo and not to the reader. Bytes of `text`
        // already streamed; the rest is held back until the block closes.
        let tools_offered = !req.tools.is_empty();
        let mut streamed = 0usize;

        while produced < max_tokens && (pos as u32) < N_CTX {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if model.is_eog_token(token) {
                break;
            }
            produced += 1;

            pending.extend_from_slice(&piece(&model, token)?);
            if let Some(chunk) = take_utf8(&mut pending) {
                text.push_str(&chunk);
                if stream(&text, &mut streamed, tools_offered, on_token).is_break() {
                    return Err(ModelError::Cancelled);
                }
            }

            batch.clear();
            batch
                .add(token, pos, &[0], true)
                .map_err(|e| ModelError::Read(e.to_string()))?;
            pos += 1;
            ctx.decode(&mut batch)
                .map_err(|e| ModelError::Read(format!("the model stopped generating: {e}")))?;
        }

        // Anything left is an incomplete sequence; the replacement character
        // is a better answer than dropping the byte silently.
        if !pending.is_empty() {
            text.push_str(&String::from_utf8_lossy(&pending));
            if stream(&text, &mut streamed, tools_offered, on_token).is_break() {
                return Err(ModelError::Cancelled);
            }
        }

        if !tools_offered {
            return Ok(GenerateOutcome::text(text));
        }
        let (text, tool_calls) = toolfmt::parse_tool_calls(&text);
        Ok(GenerateOutcome { text, tool_calls })
    }

    fn list_models(&self) -> Result<Vec<String>, ModelError> {
        let Some(dir) = super::llm_models_dir() else {
            return Ok(Vec::new());
        };
        Ok(super::CATALOG
            .iter()
            .filter(|e| e.installed_in(&dir).is_some())
            .map(|e| e.id.to_owned())
            .collect())
    }

    fn describe(&self) -> String {
        let label = super::entry(&self.model_id).map_or(self.model_id.as_str(), |e| e.label);
        format!("{label} (built in)")
    }
}

/// Threads to generate with. All of them would make the interface stutter on
/// the very machines that need the small model, and llama.cpp sees little from
/// the last few cores anyway.
fn threads() -> i32 {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    cores.saturating_sub(1).clamp(1, 8) as i32
}

fn sampler(temperature: f32) -> LlamaSampler {
    if temperature <= 0.0 {
        // Asked for determinism, give determinism.
        LlamaSampler::chain_simple([LlamaSampler::greedy()])
    } else {
        LlamaSampler::chain_simple([
            LlamaSampler::top_p(TOP_P, 1),
            LlamaSampler::temp(temperature),
            LlamaSampler::dist(DEFAULT_SEED),
        ])
    }
}

/// Hand the reader everything of `text` that is theirs to see and has not been
/// handed over yet.
fn stream(
    text: &str,
    streamed: &mut usize,
    tools_offered: bool,
    on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let visible = if tools_offered {
        std::borrow::Cow::Owned(toolfmt::visible_text(text))
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    if visible.len() <= *streamed {
        return ControlFlow::Continue(());
    }
    // What is visible only ever grows, so everything past the mark is new.
    let chunk = &visible[*streamed..];
    *streamed = visible.len();
    on_token(chunk)
}

/// The conversation as roles and text, in the order the model should see it.
///
/// There is no tool role in a chat template evo can rely on, so a tool round
/// trip is written into the text the way the model itself writes it: the
/// assistant's `<tool_call>` block, and the result as a `<tool_response>`
/// message from the user.
fn turns(req: &GenerateRequest) -> Vec<(&'static str, String)> {
    let mut turns = Vec::with_capacity(req.history.len() + 2);
    let system = if req.tools.is_empty() {
        req.system.clone()
    } else {
        Some(toolfmt::system_with_tools(
            req.system.as_deref(),
            &req.tools,
        ))
    };
    if let Some(system) = system {
        turns.push((Role::System.wire(), system));
    }
    for m in &req.history {
        match m.role {
            Role::Tool => turns.push((
                Role::User.wire(),
                toolfmt::render_tool_result(m.name.as_deref(), &m.content),
            )),
            Role::Assistant if !m.tool_calls.is_empty() => turns.push((
                Role::Assistant.wire(),
                toolfmt::render_tool_calls(&m.content, &m.tool_calls),
            )),
            _ => turns.push((m.role.wire(), m.content.clone())),
        }
    }
    // An empty prompt means the conversation already ends where the model
    // should carry on from -- the agent loop after a tool has answered.
    if !req.prompt.is_empty() {
        turns.push((Role::User.wire(), req.prompt.clone()));
    }
    turns
}

/// Render the conversation through the model's own chat template, falling back
/// to ChatML when the GGUF carries none.
fn render(model: &LlamaModel, turns: &[(&'static str, String)]) -> String {
    let templated = model.chat_template(None).ok().and_then(|tmpl| {
        let chat: Option<Vec<LlamaChatMessage>> = turns
            .iter()
            .map(|(role, content)| LlamaChatMessage::new((*role).to_owned(), content.clone()).ok())
            .collect();
        model.apply_chat_template(&tmpl, &chat?, true).ok()
    });
    templated.unwrap_or_else(|| {
        let borrowed: Vec<(&str, &str)> = turns
            .iter()
            .map(|(role, content)| (*role, content.as_str()))
            .collect();
        super::chatml_prompt(&borrowed)
    })
}

/// Tokenize the conversation, trimming the prompt from the front until it
/// fits. The quoted pages come before the question, so what is lost is
/// evidence rather than the thing being asked.
fn fit_prompt(
    model: &LlamaModel,
    req: &GenerateRequest,
    budget: usize,
) -> Result<Vec<llama_cpp_2::token::LlamaToken>, ModelError> {
    let mut turns = turns(req);
    if turns.is_empty() {
        return Err(ModelError::Read("there was nothing to ask".to_owned()));
    }
    // The last turn is the one trimmed to fit: it carries the quoted pages.
    let prompt_index = turns.len() - 1;
    for _ in 0..8 {
        let rendered = render(model, &turns);
        let tokens = model
            .str_to_token(&rendered, AddBos::Always)
            .map_err(|e| ModelError::Read(format!("could not read the prompt: {e}")))?;
        if tokens.is_empty() {
            return Err(ModelError::Read("the prompt was empty".to_owned()));
        }
        if tokens.len() <= budget {
            return Ok(tokens);
        }
        let over = tokens.len() - budget;
        let prompt = &turns[prompt_index].1;
        // Four bytes a token is a deliberate over-estimate: converging from
        // above beats another round trip through the tokenizer.
        let cut = (over * 4 + 64).min(prompt.len());
        let trimmed = super::truncate_front(prompt, cut);
        if trimmed.chars().count() < 2 {
            return Err(ModelError::Read(
                "the question is too long for the model's context window".to_owned(),
            ));
        }
        turns[prompt_index].1 = trimmed;
    }
    Err(ModelError::Read(
        "the prompt could not be trimmed to fit the model's context window".to_owned(),
    ))
}

fn piece(model: &LlamaModel, token: llama_cpp_2::token::LlamaToken) -> Result<Vec<u8>, ModelError> {
    // Special tokens are the template's own scaffolding; the end-of-turn one
    // is handled above and the rest are not the answer.
    match model.token_to_piece_bytes(token, 32, false, None) {
        Ok(bytes) => Ok(bytes),
        Err(llama_cpp_2::TokenToStringError::InsufficientBufferSpace(needed)) => model
            .token_to_piece_bytes(token, needed.unsigned_abs() as usize, false, None)
            .map_err(|e| ModelError::Read(e.to_string())),
        Err(e) => Err(ModelError::Read(e.to_string())),
    }
}

/// Take the complete UTF-8 at the front of `buf`, leaving any partial
/// character behind for the next token to finish.
fn take_utf8(buf: &mut Vec<u8>) -> Option<String> {
    let valid = match std::str::from_utf8(buf) {
        Ok(s) => s.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        // Genuinely invalid bytes: hand them over lossily rather than stall.
        Err(_) => {
            let out = String::from_utf8_lossy(buf).into_owned();
            buf.clear();
            return Some(out);
        }
    };
    if valid == 0 {
        return None;
    }
    let out = String::from_utf8_lossy(&buf[..valid]).into_owned();
    buf.drain(..valid);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::model::{ChatMessage, ToolCall, ToolDef};

    #[test]
    fn partial_characters_wait_for_the_rest_of_their_bytes() {
        let mut buf = Vec::new();
        // "é" is two bytes; a token can end between them.
        buf.extend_from_slice(&[b'a', 0xC3]);
        assert_eq!(take_utf8(&mut buf).as_deref(), Some("a"));
        assert_eq!(buf, [0xC3]);

        buf.push(0xA9);
        assert_eq!(take_utf8(&mut buf).as_deref(), Some("é"));
        assert!(buf.is_empty());
        assert_eq!(take_utf8(&mut buf), None);
    }

    #[test]
    fn a_conversation_becomes_system_history_then_the_question() {
        let req = GenerateRequest {
            model: "m".into(),
            prompt: "and now?".into(),
            system: Some("be brief".into()),
            history: vec![ChatMessage::new(Role::User, "hello")],
            ..Default::default()
        };
        let turns = turns(&req);
        assert_eq!(
            turns
                .iter()
                .map(|(r, c)| (*r, c.as_str()))
                .collect::<Vec<_>>(),
            [
                ("system", "be brief"),
                ("user", "hello"),
                ("user", "and now?"),
            ]
        );
    }

    /// With tools on offer the system prompt describes them, and a tool round
    /// trip in the history is written the way the model writes it -- there is
    /// no tool role in the chat template to put it in.
    #[test]
    fn tools_and_their_results_are_written_into_the_conversation() {
        let call = ToolCall::new("search_library", serde_json::json!({"query": "boiler"}));
        let req = GenerateRequest {
            model: "m".into(),
            prompt: String::new(),
            system: Some("be brief".into()),
            history: vec![
                ChatMessage::new(Role::User, "what about boilers?"),
                ChatMessage::calling("Looking.", vec![call.clone()]),
                ChatMessage::tool_result(&call, "2 matches"),
            ],
            tools: vec![ToolDef {
                name: "search_library".into(),
                description: "Search the library".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            ..Default::default()
        };
        let turns = turns(&req);
        let roles: Vec<&str> = turns.iter().map(|(r, _)| *r).collect();
        // The result comes back as a user turn: the template has no tool role.
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert!(turns[0].1.starts_with("be brief"));
        assert!(turns[0].1.contains("<tools>"));
        assert!(turns[0].1.contains("search_library"));
        assert!(turns[2].1.contains("<tool_call>"));
        assert!(turns[2].1.starts_with("Looking."));
        assert!(turns[3].1.contains("<tool_response>"));
        assert!(turns[3].1.contains("2 matches"));

        // An empty prompt adds no turn of its own.
        assert!(!turns.iter().any(|(_, c)| c.is_empty()));
    }

    /// While a `<tool_call>` block is being written, the reader sees nothing
    /// of it; text before and after still streams.
    #[test]
    fn a_tool_call_is_held_back_from_the_stream() {
        let mut streamed = Vec::new();
        let mut push = |chunk: &str| {
            streamed.push(chunk.to_owned());
            ControlFlow::Continue(())
        };
        let mut at = 0usize;
        let mut text = String::new();
        for piece in [
            "Let me ",
            "look.\n<tool_",
            "call>{\"name\": \"a\", \"arguments\": {}}",
            "</tool_call>",
            "\nTwo matches.",
        ] {
            text.push_str(piece);
            assert!(stream(&text, &mut at, true, &mut push).is_continue());
        }
        assert_eq!(streamed, ["Let me ", "look.\n", "\nTwo matches."]);

        // Without tools on offer nothing is held back at all.
        let mut streamed = Vec::new();
        let mut push = |chunk: &str| {
            streamed.push(chunk.to_owned());
            ControlFlow::Continue(())
        };
        let mut at = 0usize;
        let text = "a<tool_call>b".to_owned();
        assert!(stream(&text, &mut at, false, &mut push).is_continue());
        assert_eq!(streamed, ["a<tool_call>b"]);
    }

    /// A real round trip against a downloaded model. Ignored by default -- it
    /// needs a couple of gigabytes on disk and a few seconds of CPU:
    ///
    /// ```text
    /// EVO_LLM_TEST_MODEL=qwen3-1.7b cargo test -- --ignored builtin
    /// ```
    #[test]
    #[ignore = "needs a downloaded model; set EVO_LLM_TEST_MODEL"]
    fn the_builtin_backend_answers_and_streams() {
        let Ok(id) = std::env::var("EVO_LLM_TEST_MODEL") else {
            panic!("set EVO_LLM_TEST_MODEL to a catalogue id");
        };
        let backend = BuiltinBackend::new(id);
        let req = GenerateRequest {
            model: String::new(),
            prompt: "Reply with the single word: pineapple.".into(),
            system: Some("You answer in one word.".into()),
            temperature: Some(0.0),
            max_tokens: Some(64),
            ..Default::default()
        };
        let mut chunks = Vec::new();
        let outcome = backend
            .generate(&req, &mut |c: &str| {
                chunks.push(c.to_owned());
                ControlFlow::Continue(())
            })
            .expect("a completion");
        let text = outcome.text;

        assert!(!text.trim().is_empty(), "the model said nothing");
        assert!(!chunks.is_empty(), "nothing was streamed");
        assert_eq!(chunks.concat(), text, "the stream is the answer");
        assert!(
            text.to_lowercase().contains("pineapple"),
            "unexpected answer: {text}"
        );
        // Same reason the application unloads on exit: leaving the weights in
        // the static aborts the process from a C++ destructor.
        unload();
    }

    #[test]
    #[ignore = "needs a downloaded model; set EVO_LLM_TEST_MODEL"]
    fn breaking_out_of_the_stream_reports_a_cancellation() {
        let Ok(id) = std::env::var("EVO_LLM_TEST_MODEL") else {
            panic!("set EVO_LLM_TEST_MODEL to a catalogue id");
        };
        let backend = BuiltinBackend::new(id);
        let req = GenerateRequest {
            model: String::new(),
            prompt: "Count from one to fifty in words.".into(),
            temperature: Some(0.0),
            max_tokens: Some(200),
            ..Default::default()
        };
        let mut seen = 0;
        let result = backend.generate(&req, &mut |_: &str| {
            seen += 1;
            ControlFlow::Break(())
        });
        assert!(matches!(result, Err(ModelError::Cancelled)));
        assert_eq!(seen, 1, "generation stopped at the first chunk");
        unload();
    }

    /// The prompt-based tool format against a real model: it has to produce a
    /// `<tool_call>` block that parses, and the block must not reach the
    /// reader.
    #[test]
    #[ignore = "needs a downloaded model; set EVO_LLM_TEST_MODEL"]
    fn the_builtin_backend_asks_for_a_tool_in_the_hermes_format() {
        let Ok(id) = std::env::var("EVO_LLM_TEST_MODEL") else {
            panic!("set EVO_LLM_TEST_MODEL to a catalogue id");
        };
        let backend = BuiltinBackend::new(id);
        let req = GenerateRequest {
            model: String::new(),
            prompt: "What is the weather in London? Use the tool.".into(),
            system: Some("You are a helpful assistant with tools.".into()),
            tools: vec![ToolDef {
                name: "get_weather".into(),
                description: "Look up the current weather in a city".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                }),
            }],
            temperature: Some(0.0),
            max_tokens: Some(512),
            ..Default::default()
        };
        let mut streamed = String::new();
        let outcome = backend
            .generate(&req, &mut |c: &str| {
                streamed.push_str(c);
                ControlFlow::Continue(())
            })
            .expect("a completion");

        assert_eq!(
            outcome.tool_calls.len(),
            1,
            "expected one call, got text: {}",
            outcome.text
        );
        assert_eq!(outcome.tool_calls[0].name, "get_weather");
        assert!(
            outcome.tool_calls[0].arguments["city"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("london"),
            "unexpected arguments: {}",
            outcome.tool_calls[0].arguments
        );
        assert!(
            !streamed.contains("<tool_call>"),
            "the call reached the reader: {streamed}"
        );
        unload();
    }
}
