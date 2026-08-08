//! Talking to *other* MCP servers, so evo's chat and its scripts can use them.
//!
//! Each configured server is a child process evo starts the first time
//! something asks for it, and keeps for the rest of the session. The processes
//! and the connections live on a tokio runtime of their own; everything that
//! wants them -- the chat worker, the Lua thread, the Preferences pane -- is
//! synchronous, so the whole surface is the sync facade in [`McpClients`]: post
//! the work to the runtime, wait for the answer, give up after
//! [`CALL_TIMEOUT`].
//!
//! Not `Handle::block_on`: that panics inside a runtime thread, and this is
//! called from three different threads that may or may not be one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::script::model::ToolDef;

use super::McpAccess;

/// How long any one call to another server may take. Generous: a server may be
/// starting a process, reading a file or asking a network service.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// An MCP server evo may start. Child processes over stdio only -- a URL client
/// needs somewhere to put credentials, which is a design of its own.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ClientEntry {
    /// What the tools are called after: `name__tool`.
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl ClientEntry {
    /// Whether this entry is filled in enough to try.
    pub fn is_runnable(&self) -> bool {
        !self.name.trim().is_empty() && !self.command.trim().is_empty()
    }
}

/// One tool on one server, and the name a model calls it by.
#[derive(Clone, PartialEq, Debug)]
pub struct RemoteTool {
    pub server: String,
    /// The name the server knows it by.
    pub tool: String,
    /// The name evo offers it under: `server__tool`.
    pub def: ToolDef,
}

/// The name a model uses for a tool on another server.
///
/// Two servers may both have a `search`, so the server's name goes in front.
/// It is sanitized because the name has to satisfy every model API's idea of a
/// function name -- letters, digits and underscores.
pub fn qualified_name(server: &str, tool: &str) -> String {
    format!("{}__{}", sanitize(server), tool)
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    // A doubled underscore inside the server's name would make the split
    // ambiguous, so squeeze runs down to one.
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('_').to_owned()
}

/// What the Preferences pane knows about one server after pressing Test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Probe {
    Running,
    Ok(usize),
    Failed(String),
}

/// Every configured server, connected as needed.
#[derive(Default)]
pub struct McpClients {
    runtime: OnceLock<tokio::runtime::Runtime>,
    entries: Mutex<Vec<ClientEntry>>,
    connections: Mutex<HashMap<String, Arc<RunningService<rmcp::RoleClient, ()>>>>,
    /// The tool list, as of the last time anything asked for it.
    cache: Mutex<Option<Arc<Vec<RemoteTool>>>>,
    probes: Mutex<HashMap<String, Probe>>,
}

impl McpClients {
    /// Adopt a new configuration. Servers that changed are disconnected, so the
    /// next call starts them again with the new command.
    pub fn configure(&self, entries: &[ClientEntry]) {
        let mut current = self.entries.lock().unwrap();
        if current.as_slice() == entries {
            return;
        }
        current.clear();
        current.extend(entries.iter().cloned());
        drop(current);
        self.forget();
    }

    /// Drop every connection and the cached tool list. The child processes go
    /// with them.
    pub fn forget(&self) {
        self.connections.lock().unwrap().clear();
        *self.cache.lock().unwrap() = None;
        self.probes.lock().unwrap().clear();
    }

    pub fn entries(&self) -> Vec<ClientEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// Whether any server is configured at all -- the question the chat panel
    /// asks before offering to use tools.
    pub fn is_configured(&self) -> bool {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .any(ClientEntry::is_runnable)
    }

    /// The runtime the child processes live on, started on first use. Most
    /// sessions never configure a server, and the ones that do pay for it once.
    fn runtime(&self) -> Result<&tokio::runtime::Runtime, String> {
        if let Some(runtime) = self.runtime.get() {
            return Ok(runtime);
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("evo-mcp-client")
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the MCP client: {e}"))?;
        let _ = self.runtime.set(runtime);
        Ok(self.runtime.get().expect("just set"))
    }

    /// Run an async job on the runtime and wait for it here.
    ///
    /// `Handle::block_on` would panic when this is called from a thread that is
    /// already inside a runtime; posting the work and waiting on a channel is
    /// correct from any thread.
    fn block<T: Send + 'static>(
        &self,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, String> {
        let runtime = self.runtime()?;
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let _ = tx.send(work.await);
        });
        rx.recv_timeout(CALL_TIMEOUT).map_err(|_| {
            format!(
                "the MCP server did not answer within {} seconds",
                CALL_TIMEOUT.as_secs()
            )
        })
    }

    fn entry(&self, name: &str) -> Result<ClientEntry, String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .ok_or_else(|| format!("there is no MCP server called “{name}” in Preferences ▸ MCP"))
    }

    /// The connection to `name`, starting the process if it is not running.
    fn connect(&self, name: &str) -> Result<Arc<RunningService<rmcp::RoleClient, ()>>, String> {
        if let Some(existing) = self.connections.lock().unwrap().get(name) {
            return Ok(existing.clone());
        }
        let entry = self.entry(name)?;
        if !entry.is_runnable() {
            return Err(format!("the MCP server “{name}” has no command to run"));
        }

        let service = self.block(async move {
            let mut command = tokio::process::Command::new(&entry.command);
            command.args(&entry.args);
            let transport = TokioChildProcess::new(command)
                .map_err(|e| format!("could not start `{}`: {e}", entry.command))?;
            ().serve(transport)
                .await
                .map_err(|e| format!("`{}` is not answering as an MCP server: {e}", entry.command))
        })??;

        let service = Arc::new(service);
        self.connections
            .lock()
            .unwrap()
            .insert(name.to_owned(), service.clone());
        Ok(service)
    }

    /// Every tool on every configured server, cached until something changes.
    ///
    /// A server that will not start is left out with its reason recorded rather
    /// than failing the whole list: one broken entry should not take the others
    /// away.
    pub fn tools(&self) -> Arc<Vec<RemoteTool>> {
        if let Some(cached) = self.cache.lock().unwrap().clone() {
            return cached;
        }
        let mut all = Vec::new();
        for entry in self.entries() {
            if !entry.is_runnable() {
                continue;
            }
            match self.tools_of(&entry.name) {
                Ok(tools) => {
                    self.probes
                        .lock()
                        .unwrap()
                        .insert(entry.name.clone(), Probe::Ok(tools.len()));
                    all.extend(tools);
                }
                Err(e) => {
                    self.probes
                        .lock()
                        .unwrap()
                        .insert(entry.name.clone(), Probe::Failed(e));
                }
            }
        }
        let tools = Arc::new(all);
        *self.cache.lock().unwrap() = Some(tools.clone());
        tools
    }

    fn tools_of(&self, name: &str) -> Result<Vec<RemoteTool>, String> {
        let service = self.connect(name)?;
        let listed = self
            .block(async move { service.list_all_tools().await })?
            .map_err(|e| format!("could not list the tools of “{name}”: {e}"))?;
        Ok(listed
            .into_iter()
            .map(|tool| RemoteTool {
                server: name.to_owned(),
                tool: tool.name.to_string(),
                def: ToolDef {
                    name: qualified_name(name, &tool.name),
                    description: tool.description.unwrap_or_default().to_string(),
                    // The schema is passed through as the server wrote it: it
                    // is the same JSON Schema both sides speak.
                    parameters: serde_json::to_value(tool.input_schema.as_ref())
                        .unwrap_or_else(|_| serde_json::json!({"type": "object"})),
                },
            })
            .collect())
    }

    /// Run one tool on one server, returning what it said as text.
    pub fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<String, String> {
        // Checked before anything is started: a malformed call should not cost
        // a process launch.
        let arguments = match arguments {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(format!("tool arguments have to be an object, not {other}"));
            }
        };
        let service = self.connect(server)?;
        let mut params = CallToolRequestParams::new(tool.to_owned());
        params.arguments = arguments;
        let result = self
            .block(async move { service.call_tool(params).await })?
            .map_err(|e| format!("“{tool}” on “{server}” failed: {e}"))?;
        let text = content_text(&result.content);
        if result.is_error == Some(true) {
            return Err(text);
        }
        Ok(text)
    }

    /// Start a server and count its tools, for the Test button. Runs in the
    /// background so the window keeps drawing.
    pub fn start_probe(self: &Arc<Self>, name: &str, ctx: &eframe::egui::Context) {
        self.probes
            .lock()
            .unwrap()
            .insert(name.to_owned(), Probe::Running);
        // A fresh attempt, not whatever a previous failure left connected.
        self.connections.lock().unwrap().remove(name);
        *self.cache.lock().unwrap() = None;

        let clients = self.clone();
        let name = name.to_owned();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("evo-mcp-probe".into())
            .spawn(move || {
                let outcome = match clients.tools_of(&name) {
                    Ok(tools) => Probe::Ok(tools.len()),
                    Err(e) => Probe::Failed(e),
                };
                clients.probes.lock().unwrap().insert(name, outcome);
                ctx.request_repaint();
            })
            .expect("failed to spawn the MCP probe thread");
    }

    pub fn probe(&self, name: &str) -> Option<Probe> {
        self.probes.lock().unwrap().get(name).cloned()
    }
}

/// Scripts and chat reach other servers through the same door.
impl McpAccess for McpClients {
    fn tools(&self) -> Vec<RemoteTool> {
        McpClients::tools(self).as_ref().clone()
    }

    fn call(&self, server: &str, tool: &str, arguments: Value) -> Result<String, String> {
        McpClients::call(self, server, tool, arguments)
    }
}

/// What a tool said, as text a model can read. Anything that is not text --
/// an image, an embedded resource -- is described rather than dropped.
fn content_text(content: &[rmcp::model::ContentBlock]) -> String {
    use rmcp::model::ContentBlock;
    let parts: Vec<String> = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(image) => format!("[an image, {}]", image.mime_type),
            ContentBlock::Audio(audio) => format!("[audio, {}]", audio.mime_type),
            other => serde_json::to_string(other).unwrap_or_else(|_| "[content]".to_owned()),
        })
        .collect();
    if parts.is_empty() {
        "the tool returned nothing".to_owned()
    } else {
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_is_named_after_the_server_it_is_on() {
        assert_eq!(qualified_name("files", "read"), "files__read");
        // Two servers can both have a `search`; the names stay apart.
        assert_ne!(
            qualified_name("files", "search"),
            qualified_name("web", "search")
        );
    }

    /// Model APIs want function names made of letters, digits and underscores,
    /// and a doubled underscore inside a server's name would make the split
    /// ambiguous.
    #[test]
    fn a_server_name_is_squeezed_into_something_a_model_api_accepts() {
        assert_eq!(qualified_name("My Files!", "read"), "My_Files__read");
        assert_eq!(qualified_name("a__b", "read"), "a_b__read");
        assert_eq!(qualified_name("--x--", "read"), "x__read");
        for name in ["My Files!", "a__b", "--x--", "everything"] {
            let qualified = qualified_name(name, "read");
            assert!(
                qualified
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{qualified}"
            );
            assert_eq!(
                qualified.split_once("__").map(|(_, tool)| tool),
                Some("read"),
                "the tool is recoverable from {qualified}"
            );
        }
    }

    #[test]
    fn an_entry_has_to_have_a_name_and_a_command_to_be_worth_starting() {
        let full = ClientEntry {
            name: "files".into(),
            command: "npx".into(),
            args: vec!["-y".into()],
        };
        assert!(full.is_runnable());
        assert!(!ClientEntry::default().is_runnable());
        assert!(
            !ClientEntry {
                name: "  ".into(),
                ..full.clone()
            }
            .is_runnable()
        );
        assert!(
            !ClientEntry {
                command: String::new(),
                ..full
            }
            .is_runnable()
        );
    }

    /// Nothing configured means nothing to connect to, and no runtime started.
    #[test]
    fn with_nothing_configured_there_is_nothing_to_offer() {
        let clients = McpClients::default();
        assert!(!clients.is_configured());
        assert!(clients.tools().is_empty());
        let err = clients
            .call("nope", "read", serde_json::json!({}))
            .expect_err("no such server");
        assert!(err.contains("no MCP server called"), "{err}");
        assert!(
            err.contains("Preferences"),
            "it says where to add one: {err}"
        );
    }

    /// Changing the configuration drops the connections, so the next call runs
    /// the command the user actually typed.
    #[test]
    fn reconfiguring_forgets_what_was_connected() {
        let clients = McpClients::default();
        let entry = ClientEntry {
            name: "files".into(),
            command: "true".into(),
            args: Vec::new(),
        };
        clients.configure(std::slice::from_ref(&entry));
        assert!(clients.is_configured());
        assert_eq!(clients.entries(), std::slice::from_ref(&entry));

        clients
            .probes
            .lock()
            .unwrap()
            .insert("files".into(), Probe::Ok(3));
        // Setting the same configuration again is not a change, so a probe
        // result survives.
        clients.configure(std::slice::from_ref(&entry));
        assert_eq!(clients.probe("files"), Some(Probe::Ok(3)));

        clients.configure(&[ClientEntry {
            command: "false".into(),
            ..entry
        }]);
        assert_eq!(
            clients.probe("files"),
            None,
            "a changed command is a new server"
        );
    }

    #[test]
    fn what_a_tool_said_becomes_text_a_model_can_read() {
        use rmcp::model::ContentBlock;
        assert_eq!(
            content_text(&[ContentBlock::text("first"), ContentBlock::text("second")]),
            "first\nsecond"
        );
        // A picture is not text, but saying so beats saying nothing.
        let described = content_text(&[ContentBlock::image("payload", "image/png")]);
        assert!(described.contains("image/png"), "{described}");
        assert!(!described.contains("payload"), "no base64 in the reply");
        assert_eq!(content_text(&[]), "the tool returned nothing");
    }

    /// The real thing, against a real MCP server, which needs the network and
    /// a node installation -- so it is opt-in:
    ///
    /// ```text
    /// EVO_MCP_TEST_SERVER=1 cargo test -- --ignored real_server
    /// ```
    #[test]
    #[ignore = "starts npx @modelcontextprotocol/server-everything"]
    fn a_real_server_is_started_listed_and_called() {
        if std::env::var("EVO_MCP_TEST_SERVER").is_err() {
            eprintln!("set EVO_MCP_TEST_SERVER=1 to run this");
            return;
        }
        let clients = McpClients::default();
        clients.configure(&[ClientEntry {
            name: "everything".into(),
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-everything".into(),
            ],
        }]);

        let tools = clients.tools();
        assert!(!tools.is_empty(), "the server offers tools");
        let echo = tools
            .iter()
            .find(|t| t.tool == "echo")
            .expect("server-everything has an echo tool");
        assert_eq!(echo.def.name, "everything__echo");

        let said = clients
            .call(
                "everything",
                "echo",
                serde_json::json!({"message": "hello"}),
            )
            .expect("echo answers");
        assert!(said.contains("hello"), "{said}");

        // And the whole chain a user actually goes through: consent, the Lua
        // sandbox, this client, the child process, and back.
        let status = Arc::new(Mutex::new(crate::script::ScriptStatus::default()));
        let granted: Arc<dyn McpAccess> = Arc::new(clients);
        crate::script::api::run(
            r#"evo.log(evo.mcp.call("everything", "echo", { message = "from lua" }))"#,
            None,
            &crate::script::ScriptPrefs::default(),
            Some(granted),
            &status,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Instant::now() + Duration::from_secs(60),
            &eframe::egui::Context::default(),
        )
        .expect("the script runs");
        let logged = status.lock().unwrap().log.join("\n");
        assert!(logged.contains("from lua"), "{logged}");
    }

    /// A tool's own failure is the model's to work around, so it comes back as
    /// an error with the server's own words in it.
    #[test]
    fn arguments_have_to_be_an_object() {
        let clients = McpClients::default();
        clients.configure(&[ClientEntry {
            name: "files".into(),
            command: "true".into(),
            args: Vec::new(),
        }]);
        let err = clients
            .call("files", "read", serde_json::json!([1, 2, 3]))
            .expect_err("not an object");
        assert!(err.contains("have to be an object"), "{err}");
    }
}
