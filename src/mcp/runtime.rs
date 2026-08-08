//! Running the MCP server inside the app: a tokio runtime on its own thread,
//! an axum router with the MCP service under `/mcp`, and a bearer token in
//! front of it.
//!
//! Two workers is plenty -- every tool spends its time waiting on the UI
//! thread, not computing -- and the whole thing exists only while the user has
//! the switch on.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use eframe::egui;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use super::bridge::{AppBridge, AppCommand};
use super::server::EvoMcp;
use super::token_matches;

/// What the Preferences pane shows about the server.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct McpStatus {
    /// The port it is actually listening on.
    pub listening: Option<u16>,
    /// Why it is not listening, if it should be.
    pub error: Option<String>,
}

/// A running server. Dropping it asks the runtime to stop and waits for it.
pub struct McpServer {
    cancel: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
    status: Arc<Mutex<McpStatus>>,
    /// What it was started with, so the app knows when a setting changed.
    pub port: u16,
    pub token: String,
}

impl McpServer {
    /// Start the server, returning it and the receiver the UI thread drains.
    pub fn spawn(
        port: u16,
        token: String,
        ctx: &egui::Context,
    ) -> (Self, std::sync::mpsc::Receiver<AppCommand>) {
        let (tx, rx) = std::sync::mpsc::channel::<AppCommand>();
        let bridge = Arc::new(AppBridge::new(tx, ctx.clone()));
        let cancel = CancellationToken::new();
        let status = Arc::new(Mutex::new(McpStatus::default()));

        let thread = {
            let cancel = cancel.clone();
            let status = status.clone();
            let token = token.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("evo-mcp".into())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(e) => {
                            set_error(&status, format!("could not start the MCP server: {e}"));
                            ctx.request_repaint();
                            return;
                        }
                    };
                    runtime.block_on(serve(bridge, port, token, cancel, &status, &ctx));
                })
                .expect("failed to spawn the MCP thread")
        };

        (
            Self {
                cancel,
                thread: Some(thread),
                status,
                port,
                token,
            },
            rx,
        )
    }

    pub fn status(&self) -> McpStatus {
        self.status.lock().unwrap().clone()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn set_error(status: &Arc<Mutex<McpStatus>>, message: String) {
    *status.lock().unwrap() = McpStatus {
        listening: None,
        error: Some(message),
    };
}

async fn serve(
    bridge: Arc<AppBridge>,
    port: u16,
    token: String,
    cancel: CancellationToken,
    status: &Arc<Mutex<McpStatus>>,
    ctx: &egui::Context,
) {
    // Loopback only, and only IPv4: a port bound to 0.0.0.0 is a port on every
    // network the machine is on, which is not what "let my assistant reach evo"
    // asks for.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(e) => {
            set_error(
                status,
                format!(
                    "could not listen on 127.0.0.1:{port}: {e}. Another program may \
                     already be using that port; try a different one."
                ),
            );
            ctx.request_repaint();
            return;
        }
    };
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    *status.lock().unwrap() = McpStatus {
        listening: Some(bound),
        error: None,
    };
    ctx.request_repaint();

    let config = StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone());
    let app = router(bridge, token, config);
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled_owned().await })
        .await;
    if let Err(e) = served {
        set_error(status, format!("the MCP server stopped: {e}"));
    } else {
        *status.lock().unwrap() = McpStatus::default();
    }
    ctx.request_repaint();
}

/// The whole HTTP surface: the MCP service at `/mcp`, behind the token.
///
/// Shared with the tests, which is the point of it being a function -- the
/// authorization is not something to check a second implementation of.
pub fn router(
    bridge: Arc<AppBridge>,
    token: String,
    config: StreamableHttpServerConfig,
) -> axum::Router {
    let service: StreamableHttpService<EvoMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(EvoMcp::new(bridge.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let token = Arc::<str>::from(token);
    axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(move |req, next| {
            authorize(token.clone(), req, next)
        }))
}

/// Everything on the port needs the token. A loopback port is reachable by
/// every program on the machine, so "it is only local" is not a policy.
async fn authorize(expected: Arc<str>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    match presented {
        Some(token) if token_matches(&expected, token) => next.run(req).await,
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Body::from(
            "evo's MCP server needs the token from Preferences ▸ MCP, as \
             `Authorization: Bearer <token>`.\n",
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::bridge::AppCommand;
    use serde_json::{Value, json};
    use std::sync::mpsc::Receiver;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// A whole server on an ephemeral loopback port, with a stand-in for the UI
    /// thread answering the bridge. Returns its base URL and the thread that is
    /// pretending to be evo.
    fn spawn(stateless: bool) -> (String, CancellationToken, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel::<AppCommand>();
        let bridge = Arc::new(AppBridge::new(tx, egui::Context::default()));
        let cancel = CancellationToken::new();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel::<String>();

        let server_cancel = cancel.clone();
        std::thread::Builder::new()
            .name("evo-mcp-test".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("a runtime");
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                        .await
                        .expect("an ephemeral loopback port");
                    let addr = listener.local_addr().expect("an address");
                    assert!(addr.ip().is_loopback(), "evo binds loopback only");
                    let _ = addr_tx.send(format!("http://{addr}/mcp"));

                    // Two framings, one router: `stateless` gives plain JSON,
                    // which is simplest to assert on, and the default is what
                    // the app actually ships (server-sent events, sessions).
                    let config = StreamableHttpServerConfig::default()
                        .with_sse_keep_alive(None)
                        .with_cancellation_token(server_cancel.clone());
                    let config = if stateless {
                        config
                            .with_legacy_session_mode(false)
                            .with_json_response(true)
                    } else {
                        config
                    };
                    let app = router(bridge, TOKEN.to_owned(), config);
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(
                            async move { server_cancel.cancelled_owned().await },
                        )
                        .await;
                })
            })
            .expect("the server thread");

        let url = addr_rx.recv().expect("the server bound a port");
        (url, cancel, std::thread::spawn(move || fake_app(rx)))
    }

    /// What `EvoApp::handle_mcp_command` does, in miniature: answer every
    /// command until the channel closes.
    fn fake_app(rx: Receiver<AppCommand>) {
        while let Ok(command) = rx.recv() {
            match command {
                AppCommand::SearchLibrary {
                    query,
                    limit,
                    reply,
                } => {
                    let _ = reply.send(Ok(json!({ "query": query, "count": limit })));
                }
                AppCommand::ListLibrary { reply } => {
                    let _ = reply.send(Ok(json!({ "count": 0 })));
                }
                AppCommand::AddMarkup { reply, .. } => {
                    let _ = reply.send(Err("no document is open".to_owned()));
                }
                other => {
                    let reply = match other {
                        AppCommand::GetDocumentText { reply, .. }
                        | AppCommand::OpenDocument { reply, .. }
                        | AppCommand::ExportPdf { reply, .. }
                        | AppCommand::FindMatches { reply, .. } => reply,
                        _ => unreachable!(),
                    };
                    let _ = reply.send(Ok(json!({})));
                }
            }
        }
    }

    fn post(url: &str, token: Option<&str>, body: Value) -> (u16, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(20)))
            .build()
            .into();
        let mut request = agent
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(token) = token {
            request = request.header("authorization", &format!("Bearer {token}"));
        }
        match request.send(body.to_string()) {
            Ok(response) => {
                let status = response.status().as_u16();
                (
                    status,
                    response.into_body().read_to_string().unwrap_or_default(),
                )
            }
            // ureq treats 4xx as an error; the status is what is being tested.
            Err(ureq::Error::StatusCode(code)) => (code, String::new()),
            Err(e) => panic!("{e}"),
        }
    }

    fn call(url: &str, id: u32, method: &str, params: Value) -> Value {
        let (status, body) = post(
            url,
            Some(TOKEN),
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        assert_eq!(status, 200, "{method}: {body}");
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{method}: {e} in {body}"))
    }

    /// One trip through the whole thing: the port, the token, the protocol, the
    /// tool list, and a tool call that reaches the app and comes back. Run as
    /// one test because it is one server.
    #[test]
    fn the_server_answers_an_mcp_client_over_loopback() {
        let (url, cancel, app) = spawn(true);

        // Nothing gets in without the token, and the refusal says what is
        // missing rather than pretending the port is not there.
        let init = json!({
            "protocolVersion": "2026-07-28",
            "capabilities": {},
            "clientInfo": {"name": "evo-test", "version": "1.0"},
        });
        let anonymous = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init});
        assert_eq!(post(&url, None, anonymous.clone()).0, 401, "no token");
        assert_eq!(
            post(&url, Some("not-the-token"), anonymous.clone()).0,
            401,
            "a wrong token is as good as none"
        );
        assert_eq!(
            post(&url, Some(&TOKEN[..16]), anonymous).0,
            401,
            "a prefix of the token is not the token"
        );

        // With it, evo introduces itself.
        let hello = call(&url, 1, "initialize", init);
        assert_eq!(hello["result"]["serverInfo"]["name"], "evo", "{hello}");
        assert!(hello["result"]["instructions"].is_string(), "{hello}");

        // Every tool is on offer.
        let listed = call(&url, 2, "tools/list", json!({}));
        let mut names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "add_markup",
                "export_pdf",
                "get_document_text",
                "get_find_matches",
                "list_library",
                "open_document",
                "search_library",
            ]
        );

        // A tool call crosses the bridge, reaches the app, and comes back.
        let called = call(
            &url,
            3,
            "tools/call",
            json!({"name": "search_library", "arguments": {"query": "boiler", "limit": 3}}),
        );
        let text = called["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{called}"));
        let answer: Value = serde_json::from_str(text).expect("the tool's JSON");
        assert_eq!(answer["query"], "boiler");
        assert_eq!(answer["count"], 3, "the arguments arrived intact");

        // A tool the app refuses is an error the model reads, not a broken
        // request.
        let refused = call(
            &url,
            4,
            "tools/call",
            json!({
                "name": "add_markup",
                "arguments": {
                    "page": 1, "kind": "rect",
                    "x0": 0.0, "y0": 0.0, "x1": 10.0, "y1": 10.0,
                },
            }),
        );
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        assert!(
            refused["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("no document is open")),
            "{refused}"
        );

        // Switching the server off releases the port and stops the thread.
        cancel.cancel();
        app.join().expect("the app thread");
    }

    /// The framing the app actually ships is the streaming one, and a client
    /// that speaks it has to get the same answers. Kept apart from the test
    /// above because reading server-sent events is a different exercise from
    /// reading JSON.
    #[test]
    fn the_shipped_configuration_answers_over_server_sent_events() {
        let (url, cancel, app) = spawn(false);

        let init = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "evo-test", "version": "1.0"},
        });
        let anonymous = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init});
        assert_eq!(post(&url, None, anonymous).0, 401, "still behind the token");

        let (status, body) = post(
            &url,
            Some(TOKEN),
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init}),
        );
        assert_eq!(status, 200, "{body}");
        // The stream opens with an empty priming event, so take the first one
        // that actually carries something.
        let payload = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .find(|data| !data.trim().is_empty())
            .unwrap_or_else(|| panic!("no event in {body:?}"));
        let hello: Value = serde_json::from_str(payload).expect("an event carrying JSON");
        assert_eq!(hello["result"]["serverInfo"]["name"], "evo", "{hello}");

        cancel.cancel();
        app.join().expect("the app thread");
    }
}
