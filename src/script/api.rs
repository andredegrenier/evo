//! The `evo` table scripts see, and the sandbox around it.

use std::cell::RefCell;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;
use mlua::{HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value, VmState};

use super::model::{GenerateRequest, ModelBackend};
use super::{DocSnapshot, GeneratedDoc, ScriptPrefs, ScriptStatus, docgen};
use crate::mcp::McpAccess;

/// What a script is told when it reaches for MCP without being given it.
///
/// The wording names the switch, because the answer to "I need this" is a tick
/// box the user has to find, not something the script can arrange for itself.
pub const NOT_GRANTED: &str = "MCP access was not granted for this run — tick \u{201c}Allow MCP\u{201d} in the Scripts window";

/// Scripts are user-authored but shareable, so the VM gets no filesystem, no
/// process control and no loader. Everything it can reach is on the `evo`
/// table, and the only thing that leaves the machine is a request to the
/// model endpoint the user configured.
fn safe_libs() -> StdLib {
    StdLib::MATH | StdLib::STRING | StdLib::TABLE | StdLib::COROUTINE
}

/// A tight loop with no function calls would otherwise never yield to the
/// cancel check.
const HOOK_INSTRUCTIONS: u32 = 200_000;

const MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// Per-run state the Lua closures share.
struct RunCtx {
    doc: Option<DocSnapshot>,
    backend: Box<dyn ModelBackend>,
    /// The MCP servers this run may reach. `None` -- the default, and what a
    /// run gets unless the user ticked the box -- makes `evo.mcp` refuse.
    mcp: Option<Arc<dyn McpAccess>>,
    status: Arc<Mutex<ScriptStatus>>,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
    generated: RefCell<Vec<GeneratedDoc>>,
    /// Page text, extracted on first use and kept for the rest of the run.
    text_cache: RefCell<Option<Vec<String>>>,
}

impl RunCtx {
    fn log(&self, line: impl Into<String>) {
        self.status.lock().unwrap().log(line.into());
        self.ctx.request_repaint();
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The MCP access this run was given, or the reason it has none.
    fn granted(&self) -> mlua::Result<&Arc<dyn McpAccess>> {
        self.mcp
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime(NOT_GRANTED))
    }

    /// Page text, extracted from the snapshot on the worker thread. The UI
    /// thread's own cache is not shared: it may not be populated, and reaching
    /// into it would mean sharing `DocState` across threads.
    fn page_text(&self) -> Vec<String> {
        if let Some(cached) = self.text_cache.borrow().as_ref() {
            return cached.clone();
        }
        let pages = match &self.doc {
            Some(doc) => crate::library::extract::extract_all_pages(&doc.source),
            None => Vec::new(),
        };
        *self.text_cache.borrow_mut() = Some(pages.clone());
        pages
    }
}

/// Run `source`, returning the documents it generated.
#[allow(clippy::too_many_arguments)]
pub fn run(
    source: &str,
    doc: Option<DocSnapshot>,
    prefs: &ScriptPrefs,
    mcp: Option<Arc<dyn McpAccess>>,
    status: &Arc<Mutex<ScriptStatus>>,
    cancel: &Arc<AtomicBool>,
    deadline: Instant,
    ctx: &egui::Context,
) -> Result<Vec<GeneratedDoc>, String> {
    let lua = Lua::new_with(safe_libs(), LuaOptions::default()).map_err(|e| e.to_string())?;
    lua.set_memory_limit(MEMORY_LIMIT)
        .map_err(|e| e.to_string())?;

    // Without this hook there is no cancel button and no time limit, so a
    // runaway script would hang its thread for good. Refuse to run instead.
    let hook_cancel = cancel.clone();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INSTRUCTIONS),
        move |_, _| {
            if hook_cancel.load(Ordering::Relaxed) {
                Err(mlua::Error::runtime("cancelled"))
            } else if Instant::now() > deadline {
                Err(mlua::Error::runtime(
                    "the script ran past its time limit (Preferences ▸ Scripting)",
                ))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .map_err(|e| format!("could not install the script interrupt: {e}"))?;

    let run = Rc::new(RunCtx {
        doc,
        backend: prefs.model.build(),
        mcp,
        status: status.clone(),
        cancel: cancel.clone(),
        ctx: ctx.clone(),
        generated: RefCell::new(Vec::new()),
        text_cache: RefCell::new(None),
    });

    install(&lua, &run).map_err(|e| e.to_string())?;

    run.log(format!("Model: {}", run.backend.describe()));
    lua.load(source).exec().map_err(describe_error)?;

    Ok(run.generated.borrow().clone())
}

/// Lua's own error text is noisy (chunk names, stack traces). Keep the useful
/// part, and don't dress a cancellation up as a failure.
fn describe_error(e: mlua::Error) -> String {
    let text = e.to_string();
    if text.contains("cancelled") {
        return "cancelled".to_owned();
    }
    match text.split_once("stack traceback") {
        Some((head, _)) => head.trim().to_owned(),
        None => text,
    }
}

fn install(lua: &Lua, run: &Rc<RunCtx>) -> mlua::Result<()> {
    let evo = lua.create_table()?;

    let r = run.clone();
    evo.set(
        "log",
        lua.create_function(move |_, msg: String| {
            r.log(msg);
            Ok(())
        })?,
    )?;

    evo.set("doc", doc_table(lua, run)?)?;
    evo.set("model", model_table(lua, run)?)?;
    // Always installed, granted or not: a script that asks gets a sentence
    // telling it what to do, rather than "attempt to index a nil value".
    evo.set("mcp", mcp_table(lua, run)?)?;

    let r = run.clone();
    evo.set(
        "create_document",
        lua.create_function(move |_, (title, text): (String, String)| {
            if docgen::has_unrepresentable_chars(&text) {
                r.log(
                    "Note: some characters aren't in the built-in font and \
                     will appear as '?'.",
                );
            }
            let bytes = docgen::text_to_pdf(&title, &text)
                .map_err(|e| mlua::Error::runtime(format!("could not build the PDF: {e}")))?;
            r.log(format!("Generated \"{title}\" ({} bytes).", bytes.len()));
            r.generated.borrow_mut().push(GeneratedDoc { title, bytes });
            Ok(())
        })?,
    )?;

    let globals = lua.globals();
    globals.set("evo", evo)?;

    // `dofile` and `loadfile` live in the base library, which is always loaded
    // -- leaving out StdLib::IO does not remove them, and they will happily
    // read any file the user can. Take them away explicitly.
    for name in ["dofile", "loadfile"] {
        globals.set(name, Value::Nil)?;
    }

    Ok(())
}

fn doc_table(lua: &Lua, run: &Rc<RunCtx>) -> mlua::Result<Table> {
    let doc = lua.create_table()?;

    let r = run.clone();
    doc.set(
        "title",
        lua.create_function(move |_, ()| {
            Ok(r.doc.as_ref().map(|d| d.title.clone()).unwrap_or_default())
        })?,
    )?;

    let r = run.clone();
    doc.set(
        "page_count",
        lua.create_function(move |_, ()| Ok(r.doc.as_ref().map(|d| d.page_count).unwrap_or(0)))?,
    )?;

    let r = run.clone();
    doc.set(
        "is_open",
        lua.create_function(move |_, ()| Ok(r.doc.is_some()))?,
    )?;

    // `evo.doc.text()` for the whole document, `evo.doc.text(n)` for one page,
    // 1-based to match Lua's own indexing.
    let r = run.clone();
    doc.set(
        "text",
        lua.create_function(move |_, page: Option<usize>| {
            let pages = r.page_text();
            Ok(match page {
                None => pages.join("\n\n"),
                Some(n) if n >= 1 && n <= pages.len() => pages[n - 1].clone(),
                Some(n) => {
                    return Err(mlua::Error::runtime(format!(
                        "page {n} is out of range (the document has {})",
                        pages.len()
                    )));
                }
            })
        })?,
    )?;

    Ok(doc)
}

fn model_table(lua: &Lua, run: &Rc<RunCtx>) -> mlua::Result<Table> {
    let model = lua.create_table()?;

    let r = run.clone();
    model.set(
        "generate",
        lua.create_function(move |_, (prompt, opts): (String, Option<Table>)| {
            let opts = Options::read(opts.as_ref(), &r)?;
            r.log(format!("Generating ({} chars of prompt)…", prompt.len()));

            let request = GenerateRequest {
                model: opts.model,
                prompt,
                system: opts.system,
                temperature: opts.temperature,
                max_tokens: opts.max_tokens,
                ..Default::default()
            };

            // Check for cancellation between chunks as well as in the
            // instruction hook: a long generation makes no Lua progress at
            // all, so the hook alone would never fire.
            let mut on_token = |_: &str| {
                if r.cancelled() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            // Scripts ask for text and get text: a script that wants a tool
            // run calls the tool itself.
            match r.backend.generate(&request, &mut on_token) {
                Ok(outcome) => {
                    r.log(format!("Model returned {} characters.", outcome.text.len()));
                    Ok(outcome.text)
                }
                Err(e) => Err(mlua::Error::runtime(e.to_string())),
            }
        })?,
    )?;

    let r = run.clone();
    model.set(
        "list",
        lua.create_function(move |lua, ()| {
            let names = r
                .backend
                .list_models()
                .map_err(|e| mlua::Error::runtime(e.to_string()))?;
            lua.create_sequence_from(names)
        })?,
    )?;

    Ok(model)
}

/// `evo.mcp`: the tools other MCP servers offer, when this run was allowed
/// them.
fn mcp_table(lua: &Lua, run: &Rc<RunCtx>) -> mlua::Result<Table> {
    let mcp = lua.create_table()?;

    let r = run.clone();
    mcp.set(
        "tools",
        lua.create_function(move |lua, ()| {
            let access = r.granted()?;
            let listed: Vec<serde_json::Value> = access
                .tools()
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "server": tool.server,
                        "name": tool.tool,
                        "description": tool.def.description,
                        "parameters": tool.def.parameters,
                    })
                })
                .collect();
            lua.to_value(&listed)
        })?,
    )?;

    let r = run.clone();
    mcp.set(
        "call",
        lua.create_function(
            move |lua, (server, tool, args): (String, String, Option<Table>)| {
                let access = r.granted()?;
                let arguments = match args {
                    Some(table) => normalize_arguments(lua.from_value(Value::Table(table))?),
                    None => serde_json::Value::Null,
                };
                r.log(format!("Calling {server}/{tool}…"));
                access
                    .call(&server, &tool, arguments)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    Ok(mcp)
}

/// An empty Lua table is indistinguishable from an empty list, and comes back
/// from serde as `[]`. A tool call with no arguments means an empty object.
fn normalize_arguments(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) if items.is_empty() => serde_json::json!({}),
        other => other,
    }
}

struct Options {
    model: String,
    system: Option<String>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
}

impl Options {
    fn read(table: Option<&Table>, run: &RunCtx) -> mlua::Result<Self> {
        let default_model = run
            .backend
            .describe()
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        let Some(t) = table else {
            return Ok(Self {
                model: default_model,
                system: None,
                temperature: None,
                max_tokens: None,
            });
        };
        Ok(Self {
            model: match t.get::<Value>("model")? {
                Value::String(s) => s.to_str()?.to_owned(),
                _ => default_model,
            },
            system: t.get::<Option<String>>("system")?,
            temperature: t.get::<Option<f32>>("temperature")?,
            max_tokens: t.get::<Option<u32>>("max_tokens")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Returns canned text without touching the network.
    struct MockBackend {
        reply: String,
        calls: Arc<AtomicUsize>,
    }

    impl ModelBackend for MockBackend {
        fn generate(
            &self,
            _req: &GenerateRequest,
            on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
        ) -> Result<super::super::model::GenerateOutcome, super::super::model::ModelError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if on_token(&self.reply).is_break() {
                return Err(super::super::model::ModelError::Cancelled);
            }
            Ok(super::super::model::GenerateOutcome::text(&self.reply))
        }

        fn list_models(&self) -> Result<Vec<String>, super::super::model::ModelError> {
            Ok(vec!["mock-model".to_owned()])
        }

        fn describe(&self) -> String {
            "mock-model (test)".to_owned()
        }
    }

    /// An MCP grant that records what a script asked for, so a test can check
    /// that a call arrived intact -- and that one never arrives when the grant
    /// was withheld.
    #[derive(Default)]
    struct MockMcp {
        calls: Mutex<Vec<(String, String, serde_json::Value)>>,
    }

    impl McpAccess for MockMcp {
        fn tools(&self) -> Vec<crate::mcp::client::RemoteTool> {
            vec![crate::mcp::client::RemoteTool {
                server: "files".to_owned(),
                tool: "read".to_owned(),
                def: crate::script::model::ToolDef {
                    name: "files__read".to_owned(),
                    description: "Read a file".to_owned(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            }]
        }

        fn call(
            &self,
            server: &str,
            tool: &str,
            arguments: serde_json::Value,
        ) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((server.to_owned(), tool.to_owned(), arguments));
            if tool == "explode" {
                return Err("the server said no".to_owned());
            }
            Ok("the file's contents".to_owned())
        }
    }

    struct Harness {
        status: Arc<Mutex<ScriptStatus>>,
        cancel: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        /// What this run is allowed to reach. `None` is the default, and the
        /// state every existing test runs in.
        mcp: Option<Arc<MockMcp>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                status: Arc::new(Mutex::new(ScriptStatus::default())),
                cancel: Arc::new(AtomicBool::new(false)),
                calls: Arc::new(AtomicUsize::new(0)),
                mcp: None,
            }
        }

        /// The same harness with MCP granted, as ticking the box does.
        fn with_mcp() -> Self {
            Self {
                mcp: Some(Arc::new(MockMcp::default())),
                ..Self::new()
            }
        }

        fn mcp_calls(&self) -> Vec<(String, String, serde_json::Value)> {
            match &self.mcp {
                Some(mcp) => mcp.calls.lock().unwrap().clone(),
                None => Vec::new(),
            }
        }

        fn run_with(
            &self,
            source: &str,
            doc: Option<DocSnapshot>,
            reply: &str,
            timeout: Duration,
        ) -> Result<Vec<GeneratedDoc>, String> {
            let lua = Lua::new_with(safe_libs(), LuaOptions::default()).expect("vm");
            lua.set_memory_limit(MEMORY_LIMIT).expect("limit");
            let deadline = Instant::now() + timeout;
            let hook_cancel = self.cancel.clone();
            lua.set_hook(
                HookTriggers::new().every_nth_instruction(HOOK_INSTRUCTIONS),
                move |_, _| {
                    if hook_cancel.load(Ordering::Relaxed) {
                        Err(mlua::Error::runtime("cancelled"))
                    } else if Instant::now() > deadline {
                        Err(mlua::Error::runtime("the script ran past its time limit"))
                    } else {
                        Ok(VmState::Continue)
                    }
                },
            )
            .expect("hook");

            let run = Rc::new(RunCtx {
                doc,
                backend: Box::new(MockBackend {
                    reply: reply.to_owned(),
                    calls: self.calls.clone(),
                }),
                mcp: self.mcp.clone().map(|mcp| mcp as Arc<dyn McpAccess>),
                status: self.status.clone(),
                cancel: self.cancel.clone(),
                ctx: egui::Context::default(),
                generated: RefCell::new(Vec::new()),
                text_cache: RefCell::new(None),
            });
            install(&lua, &run).map_err(|e| e.to_string())?;
            lua.load(source).exec().map_err(describe_error)?;
            Ok(run.generated.borrow().clone())
        }

        fn run(&self, source: &str) -> Result<Vec<GeneratedDoc>, String> {
            self.run_with(source, None, "generated text", Duration::from_secs(10))
        }

        fn log(&self) -> Vec<String> {
            self.status.lock().unwrap().log.clone()
        }
    }

    fn snapshot() -> DocSnapshot {
        let bytes = docgen::text_to_pdf("Test Document", "Alpha beta gamma.").expect("pdf");
        DocSnapshot {
            title: "Test Document".to_owned(),
            source: Arc::new(bytes),
            page_count: 1,
        }
    }

    #[test]
    fn a_script_can_generate_a_document_from_the_model() {
        let h = Harness::new();
        let docs = h
            .run(r#"evo.create_document("Report", evo.model.generate("summarize"))"#)
            .expect("run");
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].title, "Report");
        assert!(!docs[0].bytes.is_empty());
        assert_eq!(h.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_generated_document_is_a_readable_pdf() {
        let h = Harness::new();
        let docs = h
            .run(r#"evo.create_document("Out", "body text here")"#)
            .expect("run");
        let doc = crate::doc::Document::load_bytes(docs[0].bytes.clone(), None).expect("load");
        assert_eq!(doc.pages.len(), 1);
    }

    #[test]
    fn the_script_reads_the_open_documents_text() {
        let h = Harness::new();
        h.run_with(
            r#"evo.log(evo.doc.title() .. " | " .. evo.doc.text() .. " | " .. evo.doc.page_count())"#,
            Some(snapshot()),
            "",
            Duration::from_secs(10),
        )
        .expect("run");
        let logged = h.log().join("\n");
        assert!(logged.contains("Test Document"), "{logged}");
        assert!(logged.contains("Alpha beta gamma"), "{logged}");
        assert!(logged.contains("| 1"), "{logged}");
    }

    #[test]
    fn with_nothing_open_the_document_api_is_empty_rather_than_an_error() {
        let h = Harness::new();
        h.run(r#"evo.log("[" .. evo.doc.text() .. "]"); evo.log(tostring(evo.doc.is_open()))"#)
            .expect("run");
        assert!(h.log().contains(&"[]".to_owned()));
        assert!(h.log().contains(&"false".to_owned()));
    }

    #[test]
    fn asking_for_a_page_that_does_not_exist_is_an_error_the_script_can_see() {
        let h = Harness::new();
        let err = h
            .run_with(
                "evo.doc.text(99)",
                Some(snapshot()),
                "",
                Duration::from_secs(10),
            )
            .expect_err("should fail");
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn a_runaway_loop_is_stopped_by_the_deadline() {
        let h = Harness::new();
        let err = h
            .run_with("while true do end", None, "", Duration::from_millis(200))
            .expect_err("should time out");
        assert!(err.contains("time limit"), "{err}");
    }

    #[test]
    fn cancelling_stops_a_running_script() {
        let h = Harness::new();
        h.cancel.store(true, Ordering::Relaxed);
        let err = h.run("while true do end").expect_err("should cancel");
        assert_eq!(err, "cancelled");
    }

    #[test]
    fn cancelling_interrupts_generation_too() {
        let h = Harness::new();
        h.cancel.store(true, Ordering::Relaxed);
        // The model call itself must notice, not just the instruction hook:
        // a long generation makes no Lua progress for the hook to catch.
        let err = h
            .run(r#"evo.model.generate("anything")"#)
            .expect_err("should cancel");
        assert!(err.contains("cancel"), "{err}");
    }

    /// `dofile` and `loadfile` are in Lua's base library, which is always
    /// loaded; omitting StdLib::IO does not remove them, and they read any
    /// file the user can. An early version of this sandbox leaked exactly that
    /// way, so check the reason each one fails, not just that it failed.
    #[test]
    fn the_filesystem_and_process_libraries_are_not_reachable() {
        let h = Harness::new();
        for expr in [
            "io.open('/etc/passwd')",
            "os.execute('id')",
            "require('os')",
            "dofile('/etc/passwd')",
            "loadfile('/etc/passwd')",
            "local p = package.path",
        ] {
            let err = h.run(expr).expect_err(&format!("{expr} should not work"));
            assert!(
                err.contains("nil value"),
                "{expr} should have failed as a missing global, but: {err}"
            );
        }
    }

    #[test]
    fn a_script_cannot_read_a_file_through_the_loader() {
        let h = Harness::new();
        // The giveaway for the old hole: the file was read and then failed to
        // parse as Lua, rather than the call being unavailable at all.
        let err = h.run("dofile('/etc/hostname')").expect_err("should fail");
        assert!(
            !err.contains("/etc/hostname:"),
            "the file was opened and parsed: {err}"
        );
    }

    #[test]
    fn generation_options_reach_the_backend_without_error() {
        let h = Harness::new();
        h.run(
            r#"evo.model.generate("p", { system = "s", temperature = 0.2,
                                         max_tokens = 32, model = "other" })"#,
        )
        .expect("run");
        assert_eq!(h.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_model_list_is_visible_to_scripts() {
        let h = Harness::new();
        h.run(r#"evo.log(evo.model.list()[1])"#).expect("run");
        assert!(h.log().contains(&"mock-model".to_owned()));
    }

    #[test]
    fn several_documents_can_come_out_of_one_run() {
        let h = Harness::new();
        let docs = h
            .run(r#"for i = 1, 3 do evo.create_document("Doc " .. i, "body " .. i) end"#)
            .expect("run");
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[2].title, "Doc 3");
    }

    /// The default is no access. Both entry points have to refuse, and the
    /// refusal has to name the switch: a script that is told "nil value" leaves
    /// its author with nothing to do about it.
    #[test]
    fn without_consent_the_mcp_api_refuses_and_says_how_to_grant_it() {
        let h = Harness::new();
        for expr in [
            "evo.mcp.tools()",
            r#"evo.mcp.call("files", "read", { path = "/etc/passwd" })"#,
        ] {
            let err = h.run(expr).expect_err(&format!("{expr} should be refused"));
            assert!(
                err.contains("not granted"),
                "{expr} should say it was not granted, but: {err}"
            );
            assert!(
                err.contains("Allow MCP"),
                "{expr} should name the switch, but: {err}"
            );
        }
        assert!(
            h.mcp_calls().is_empty(),
            "nothing may reach a server without consent"
        );
    }

    /// The table itself is always there, so a script can be written against it
    /// and fail with a sentence rather than with "index a nil value".
    #[test]
    fn the_mcp_table_exists_even_when_access_was_withheld() {
        let h = Harness::new();
        h.run(r#"evo.log(type(evo.mcp) .. " " .. type(evo.mcp.call))"#)
            .expect("run");
        assert!(
            h.log().contains(&"table function".to_owned()),
            "{:?}",
            h.log()
        );
    }

    #[test]
    fn with_consent_a_script_can_list_and_run_a_tool() {
        let h = Harness::with_mcp();
        h.run(
            r#"
            local tools = evo.mcp.tools()
            evo.log(tools[1].server .. "/" .. tools[1].name)
            local out = evo.mcp.call("files", "read", { path = "/tmp/x", lines = 3 })
            evo.log(out)
            "#,
        )
        .expect("run");
        assert!(h.log().contains(&"files/read".to_owned()), "{:?}", h.log());
        assert!(
            h.log().contains(&"the file's contents".to_owned()),
            "{:?}",
            h.log()
        );

        // The arguments crossed from Lua to JSON as written.
        let calls = h.mcp_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "files");
        assert_eq!(calls[0].1, "read");
        assert_eq!(calls[0].2["path"], "/tmp/x");
        assert_eq!(calls[0].2["lines"], 3);
    }

    /// A call with no arguments is a call with an empty object, not with an
    /// empty list: an empty Lua table is both, and only one of them is valid.
    #[test]
    fn a_call_without_arguments_sends_an_object() {
        let h = Harness::with_mcp();
        h.run(r#"evo.mcp.call("files", "read", {}); evo.mcp.call("files", "read")"#)
            .expect("run");
        let calls = h.mcp_calls();
        assert!(calls[0].2.is_object(), "{:?}", calls[0].2);
        assert!(calls[1].2.is_null(), "{:?}", calls[1].2);
    }

    /// A server's own failure is the script's to handle, so it arrives as an
    /// ordinary Lua error that `pcall` can catch.
    #[test]
    fn a_failing_tool_is_an_error_the_script_can_catch() {
        let h = Harness::with_mcp();
        h.run(
            r#"
            local ok, err = pcall(function() evo.mcp.call("files", "explode", {}) end)
            evo.log(tostring(ok) .. ": " .. tostring(err))
            "#,
        )
        .expect("run");
        let logged = h.log().join("\n");
        assert!(logged.contains("false"), "{logged}");
        assert!(logged.contains("the server said no"), "{logged}");
    }

    /// Consent to MCP is not consent to anything else: the rest of the sandbox
    /// is exactly as tight with it as without.
    #[test]
    fn granting_mcp_does_not_open_the_filesystem() {
        let h = Harness::with_mcp();
        for expr in [
            "io.open('/etc/passwd')",
            "os.execute('id')",
            "dofile('/etc/passwd')",
            "loadfile('/etc/passwd')",
        ] {
            let err = h.run(expr).expect_err(&format!("{expr} should not work"));
            assert!(
                err.contains("nil value"),
                "{expr} should still be a missing global, but: {err}"
            );
        }
    }

    #[test]
    fn a_syntax_error_is_reported_without_a_stack_traceback() {
        let h = Harness::new();
        let err = h.run("this is not lua").expect_err("should fail");
        assert!(!err.contains("stack traceback"), "{err}");
        assert!(!err.is_empty());
    }
}
