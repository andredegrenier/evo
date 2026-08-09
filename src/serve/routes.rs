//! The HTTP surface: what is on offer, and in what order the guards run.
//!
//! Assembling the router is a function rather than something inlined into
//! startup so the tests exercise the shipped one. The authorization rules are
//! not something to have a second implementation of.

use axum::Json;
use axum::extract::DefaultBodyLimit;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rust_embed::Embed;
use serde_json::json;

use super::{Shared, auth, chat_api, library_api, markup_api, pages};

/// The web app, read from `assets/web/` at build time.
///
/// In debug builds rust-embed reads the files from disk on every request, so
/// the HTML and CSS can be edited while the server runs; release builds carry
/// them inside the binary, which is what makes `evo` a single file to copy onto
/// a server.
#[derive(Embed)]
#[folder = "assets/web/"]
struct Assets;

/// The whole thing: the API, the app shell, and two layers of guard.
///
/// Order matters. `layer` wraps everything registered before it, so the CSRF
/// check -- added last, and therefore outermost -- runs first, and the session
/// check runs second. Both wrap the fallback too, which is what makes an
/// unknown `/api/` path answer 401 rather than 404: evo does not confirm which
/// endpoints exist to someone who has not signed in.
///
/// Note the shape of the page-image route: the router matches whole segments,
/// so `{file}` captures `3.png` and the handler takes the extension off. The
/// URL is what the plan calls for -- `page/3.png` -- and a service worker can
/// cache it under a name that says what it holds.
pub fn router(state: Shared) -> axum::Router {
    let upload_limit = state.config.upload_limit();
    axum::Router::new()
        .route("/api/health", get(health))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/setup-qr", get(auth::setup_qr))
        .route("/api/status", get(library_api::status))
        .route(
            "/api/docs",
            get(library_api::list)
                .post(library_api::upload)
                // The body is a whole PDF, so the default limit (2MB) would
                // refuse most real documents. This one is the operator's.
                .layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route(
            "/api/docs/{id}",
            get(library_api::document).delete(library_api::delete),
        )
        .route("/api/docs/{id}/manifest", get(library_api::manifest))
        .route("/api/docs/{id}/thumb.png", get(library_api::thumbnail))
        .route("/api/docs/{id}/page/{file}", get(pages::page_png))
        .route(
            "/api/docs/{id}/markup",
            get(markup_api::get_markup).put(markup_api::put_markup),
        )
        .route("/api/docs/{id}/markup.svg", get(markup_api::markup_svg))
        .route("/api/docs/{id}/chat", post(chat_api::chat))
        // The agent is about the library rather than about a document, so it
        // hangs off the root and not off an id.
        .route("/api/agent/chat", post(chat_api::agent_chat))
        .route(
            "/api/docs/{id}/chatlog",
            get(chat_api::get_chatlog).put(chat_api::put_chatlog),
        )
        .fallback(asset)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ))
        .layer(axum::middleware::from_fn(auth::csrf))
        .with_state(state)
}

/// Open on purpose: a monitor or a load balancer has no cookie, and there is
/// nothing here worth hiding.
async fn health() -> Response {
    Json(json!({
        "ok": true,
        "name": "evo",
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// The app shell. Anything that is not a known route is looked for in the
/// embedded assets, and `/` is `index.html`.
async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // rust-embed only ever answers with files that were embedded, so this is
    // belt and braces -- but a path that climbs out of the asset directory is
    // never a request worth serving.
    if path.split('/').any(|part| part == "..") {
        return missing();
    }

    let Some(file) = Assets::get(path) else {
        return missing();
    };
    (
        [
            (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
            // The shell is small and changes with each release, and the
            // service worker is what makes it fast to open -- so the browser
            // may keep a copy but must always ask whether it is still the
            // current one. A cache lifetime here would mean an upgrade that
            // some phone does not see for a week.
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        file.data,
    )
        .into_response()
}

fn missing() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "evo has nothing at that address." })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::script::model::{
        GenerateOutcome, GenerateRequest, ModelBackend, ModelError, ToolCall,
    };
    use crate::serve::{
        Backends, ServeConfig, ServeOptions, ServePaths, ServeState, Shared, default_backend,
        now_secs,
    };
    use serde_json::Value;
    use std::ops::ControlFlow;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    /// A throwaway password. Nothing outside this file has ever seen it.
    const PASSWORD: &str = "a throwaway test password";

    /// A whole server on an ephemeral loopback port, over a library of its own:
    /// redb permits one process per database and the test binary runs tests in
    /// parallel. Shaped after `mcp::runtime`'s harness.
    struct Harness {
        url: String,
        secret: String,
        cancel: CancellationToken,
        thread: Option<std::thread::JoinHandle<()>>,
        dir: std::path::PathBuf,
        /// The server's own state, for the tests that have to look at the
        /// library the way a tool does. Dropped by [`Harness::stop`], because
        /// redb permits one `Database` per file and a test that reopens the
        /// library needs this handle gone.
        state: Option<Shared>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.state = None;
            self.cancel.cancel();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Harness {
        fn start(name: &str) -> Self {
            Self::start_with(name, false, default_backend())
        }

        /// The same server with the text-extraction worker running. Only the
        /// search test needs it: it is a thread and a tantivy index, and every
        /// other test would be paying for both.
        fn start_indexing(name: &str) -> Self {
            Self::start_with(name, true, default_backend())
        }

        /// The shipped server with a scripted model behind it, so a whole agent
        /// turn -- tools, events, framing -- can be watched over a real socket
        /// with nothing downloaded.
        fn start_with_model(name: &str, backend: Backends) -> Self {
            Self::start_with(name, false, backend)
        }

        fn start_with(name: &str, index: bool, backend: Backends) -> Self {
            let dir = std::env::temp_dir().join(format!("evo-serve-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let paths = ServePaths::new(Some(dir.clone()), None).expect("paths");
            std::fs::create_dir_all(&paths.serve_dir).expect("the serve directory");

            let auth = auth::AuthStore::create(PASSWORD).expect("credentials");
            auth.save(&paths.auth).expect("saving the credentials");
            let secret = auth.totp_secret.clone();

            let mut library = Library::open_at(paths.library_root.clone()).expect("a library");
            if index {
                // The detached context the whole server uses: there is no
                // window to repaint.
                library.start_indexer(
                    &eframe::egui::Context::default(),
                    crate::render::engine::EnginePref::Hayro,
                );
            }
            // No test may reach a real language model. Port 1 is not one
            // anything listens on, so a test that asks a question gets the
            // failure path deterministically -- on a machine with Ollama
            // running and on one without.
            let mut config = ServeConfig::default();
            config.model.base_url = "http://127.0.0.1:1".to_owned();
            config.model.timeout_secs = 5;

            let state = Arc::new(ServeState {
                library: Arc::new(Mutex::new(library)),
                config,
                paths,
                // Plain HTTP, because that is what a test client speaks; the
                // cookie is `evo` rather than `__Host-evo` as a result.
                options: ServeOptions {
                    secure_cookies: false,
                    trust_proxy: false,
                },
                auth: Mutex::new(auth),
                sessions: Mutex::new(auth::Sessions::default()),
                logins: Mutex::new(auth::RateLimiter::default()),
                setup: Mutex::new(None),
                page_sizes: Mutex::new(std::collections::HashMap::new()),
                pages_text: Mutex::new(chat_api::PageText::default()),
                generation: tokio::sync::Semaphore::new(1),
                // No test configures an MCP server: starting somebody's child
                // process is not something a test suite does.
                mcp: Arc::new(crate::mcp::client::McpClients::default()),
                backend,
            });
            let shared = state.clone();

            let cancel = CancellationToken::new();
            let (addr_tx, addr_rx) = std::sync::mpsc::channel::<String>();
            let server_cancel = cancel.clone();
            let thread = std::thread::Builder::new()
                .name("evo-serve-test".into())
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
                        let _ = addr_tx.send(format!("http://{addr}"));
                        let app = router(state);
                        let _ = axum::serve(
                            listener,
                            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                        )
                        .with_graceful_shutdown(
                            async move { server_cancel.cancelled_owned().await },
                        )
                        .await;
                    });
                })
                .expect("the server thread");

            let url = addr_rx.recv().expect("the server bound a port");
            Self {
                url,
                secret,
                cancel,
                thread: Some(thread),
                dir,
                state: Some(shared),
            }
        }

        /// Stop the server and let go of the library, leaving what is on disk
        /// where it is.
        ///
        /// redb permits one `Database` per file, so a test that wants to open
        /// the library the way the desktop app would has to wait for the
        /// server to have finished with it. Joining the thread is that wait:
        /// the router owns the only `ServeState`, so the `Library` is dropped
        /// with it.
        fn stop(&mut self) {
            self.state = None;
            self.cancel.cancel();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }

        /// A code the authenticator app would be showing right now.
        fn code(&self) -> String {
            let store = auth::AuthStore {
                password_hash: String::new(),
                totp_secret: self.secret.clone(),
                totp_confirmed: false,
                last_totp_step: 0,
            };
            store
                .totp()
                .expect("an authenticator")
                .generate(now_secs())
                .to_string()
        }
    }

    /// What came back, in the parts these tests care about. The body stays
    /// bytes because one of the answers is a PNG.
    struct Answer {
        status: u16,
        body: Vec<u8>,
        content_type: String,
        cookie: Option<String>,
        /// Everything else, lower-cased, for the tests that are about headers
        /// (version tags, cache lifetimes) rather than about bodies.
        headers: std::collections::HashMap<String, String>,
    }

    impl Answer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }
    }

    /// A fresh agent per request: ureq's cookie jar is off (the `cookies`
    /// feature is not enabled), and these tests want to say exactly which
    /// cookie went out, including none.
    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            // A refusal has a body, and what it says is half of what these
            // tests are checking; ureq would otherwise throw it away.
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn finish(sent: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Answer {
        let response = match sent {
            Ok(response) => response,
            // ureq treats 4xx as an error, and the status is what is under test.
            Err(ureq::Error::StatusCode(code)) => {
                return Answer {
                    status: code,
                    body: Vec::new(),
                    content_type: String::new(),
                    cookie: None,
                    headers: std::collections::HashMap::new(),
                };
            }
            Err(e) => panic!("{e}"),
        };
        let status = response.status().as_u16();
        let headers: std::collections::HashMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let cookie = headers.get("set-cookie").cloned();
        let content_type = headers.get("content-type").cloned().unwrap_or_default();
        Answer {
            status,
            body: response.into_body().read_to_vec().unwrap_or_default(),
            content_type,
            cookie,
            headers,
        }
    }

    fn get(url: &str, cookie: Option<&str>) -> Answer {
        let agent = agent();
        let mut request = agent.get(url).header("accept", "*/*");
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        finish(request.call())
    }

    fn post(url: &str, cookie: Option<&str>, csrf: bool, body: Value) -> Answer {
        let agent = agent();
        let mut request = agent
            .post(url)
            .header("accept", "*/*")
            .header("content-type", "application/json");
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        if csrf {
            request = request.header("X-Evo", "1");
        }
        finish(request.send(body.to_string()))
    }

    fn post_json(url: &str, cookie: Option<&str>, body: Value) -> Answer {
        post(url, cookie, true, body)
    }

    /// An upload: the body is the PDF, the name is in the headers.
    fn post_bytes(url: &str, cookie: &str, extra: &[(&str, &str)], body: Vec<u8>) -> Answer {
        let agent = agent();
        let mut request = agent
            .post(url)
            .header("accept", "*/*")
            .header("content-type", "application/pdf")
            .header("cookie", cookie)
            .header("X-Evo", "1");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        finish(request.send(body))
    }

    fn put_json(url: &str, cookie: &str, extra: &[(&str, &str)], body: Value) -> Answer {
        let agent = agent();
        let mut request = agent
            .put(url)
            .header("accept", "*/*")
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .header("X-Evo", "1");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        finish(request.send(body.to_string()))
    }

    fn delete(url: &str, cookie: &str) -> Answer {
        let agent = agent();
        finish(
            agent
                .delete(url)
                .header("accept", "*/*")
                .header("cookie", cookie)
                .header("X-Evo", "1")
                .call(),
        )
    }

    /// Sign in the way the browser does, and keep the cookie.
    fn sign_in(evo: &Harness) -> String {
        let answer = post_json(
            &format!("{}/api/login", evo.url),
            None,
            json!({"password": PASSWORD, "code": evo.code()}),
        );
        assert_eq!(answer.status, 200, "{}", answer.text());
        answer
            .cookie
            .expect("a session cookie")
            .split(';')
            .next()
            .expect("the name=value pair")
            .to_owned()
    }

    fn fixture() -> Vec<u8> {
        std::fs::read("tests/fixtures/sample.pdf").expect("the fixture")
    }

    /// One trip through the whole thing: enrolment, sign-in, the cookie, and
    /// signing out. One test because it is one server and one enrolment.
    #[test]
    fn a_browser_enrols_signs_in_and_signs_out() {
        let evo = Harness::start("walkthrough");

        // The shell is public: the browser has to be able to draw the form.
        let shell = get(&evo.url, None);
        assert_eq!(shell.status, 200);
        assert!(shell.text().contains("id=\"login\""), "the sign-in form");
        assert!(
            shell.text().contains("/app.js"),
            "and the app that posts it"
        );
        let css = get(&format!("{}/style.css", evo.url), None);
        assert_eq!(css.status, 200);
        assert_eq!(css.content_type, "text/css", "the mime type is guessed");

        // Health is public. Everything else is not, and says so.
        let health = get(&format!("{}/api/health", evo.url), None);
        assert_eq!(health.status, 200);
        assert_eq!(health.json()["ok"], true);
        for guarded in ["/api/logout", "/api/docs", "/api/whatever-comes-later"] {
            let refused = get(&format!("{}{guarded}", evo.url), None);
            assert_eq!(refused.status, 401, "{guarded}");
        }

        // A mutation without the header is not one this app made.
        let no_header = post(
            &format!("{}/api/login", evo.url),
            None,
            false,
            json!({"password": PASSWORD}),
        );
        assert_eq!(no_header.status, 403, "the CSRF header is required");

        // A wrong password gets nothing -- not even the news that enrolment is
        // pending.
        let wrong = post_json(
            &format!("{}/api/login", evo.url),
            None,
            json!({"password": "not it"}),
        );
        assert_eq!(wrong.status, 401);
        assert_eq!(wrong.json()["enroll"], Value::Null);

        // The right password, with no authenticator yet, offers enrolment.
        let enroll = post_json(
            &format!("{}/api/login", evo.url),
            None,
            json!({"password": PASSWORD}),
        );
        assert_eq!(enroll.status, 200, "{}", enroll.text());
        assert_eq!(enroll.json()["enroll"], true);
        assert!(enroll.cookie.is_none(), "half a sign-in is not a session");
        let setup = enroll.json()["setup"]
            .as_str()
            .expect("a setup token")
            .to_owned();

        // The QR code is behind that token and nothing else.
        let qr = get(&format!("{}/api/setup-qr?t={setup}", evo.url), None);
        assert_eq!(qr.status, 200);
        assert_eq!(qr.content_type, "image/png");
        assert_eq!(&qr.body[1..4], b"PNG", "a real PNG, not a JSON error");
        assert_eq!(
            get(&format!("{}/api/setup-qr?t=deadbeef", evo.url), None).status,
            401,
            "a guessed setup token is no token"
        );

        // Password and code together: a session.
        let signed_in = post_json(
            &format!("{}/api/login", evo.url),
            None,
            json!({"password": PASSWORD, "code": evo.code()}),
        );
        assert_eq!(signed_in.status, 200, "{}", signed_in.text());
        let cookie = signed_in.cookie.expect("a session cookie");
        assert!(cookie.starts_with("evo="), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(
            !cookie.contains("Secure"),
            "--insecure-http drops it: {cookie}"
        );
        let session = cookie
            .split(';')
            .next()
            .expect("the name=value pair")
            .to_owned();

        // Enrolment is over, so the setup token is thrown away with it and the
        // secret is not on offer any more.
        assert_eq!(
            get(&format!("{}/api/setup-qr?t={setup}", evo.url), None).status,
            401,
            "signing in retires the setup token"
        );

        // The cookie opens the door a 401 was behind a moment ago.
        let known = get(&format!("{}/api/docs", evo.url), Some(&session));
        assert_eq!(known.status, 200, "past the guard: {}", known.text());
        assert_eq!(
            known.json()["count"],
            0,
            "an empty library is still a library"
        );

        // Signing out takes the session with it.
        let out = post_json(
            &format!("{}/api/logout", evo.url),
            Some(&session),
            json!({}),
        );
        assert_eq!(out.status, 200);
        assert!(out.cookie.is_some_and(|c| c.contains("Max-Age=0")));
        assert_eq!(
            get(&format!("{}/api/docs", evo.url), Some(&session)).status,
            401,
            "a signed-out cookie is not a session"
        );
    }

    /// Guessing is the attack this port is exposed to, so it is the one with a
    /// budget. Its own server because the limiter counts per process lifetime.
    #[test]
    fn five_wrong_passwords_and_the_address_has_to_wait() {
        let evo = Harness::start("ratelimit");
        let url = format!("{}/api/login", evo.url);

        for attempt in 0..auth::LOGIN_ATTEMPTS {
            let answer = post_json(&url, None, json!({"password": "guess"}));
            assert_eq!(answer.status, 401, "attempt {attempt}");
        }
        let stopped = post_json(&url, None, json!({"password": "guess"}));
        assert_eq!(stopped.status, 429, "the sixth attempt is refused");
        // Even the right password waits: otherwise the limit would be an oracle
        // telling an attacker when they had guessed it.
        assert_eq!(
            post_json(&url, None, json!({"password": PASSWORD})).status,
            429
        );
    }

    /// The app is a handful of files with no build step, so "does it load" is
    /// a question about whether every file the shell names is really there and
    /// arrives as the sort of thing the browser will run. All of it before a
    /// session, because the browser fetches the shell in order to ask for one.
    #[test]
    fn the_whole_app_shell_is_served_without_a_session() {
        let evo = Harness::start("shell");
        let shell = get(&evo.url, None).text();

        // Everything index.html points at, and everything the modules import.
        for (path, expected) in [
            ("/index.html", "text/html"),
            ("/style.css", "text/css"),
            ("/api.js", "javascript"),
            ("/app.js", "javascript"),
            ("/viewer.js", "javascript"),
            ("/chat.js", "javascript"),
            ("/sw.js", "javascript"),
            ("/offline.html", "text/html"),
            ("/manifest.webmanifest", "manifest+json"),
            ("/icons/icon-192.png", "image/png"),
            ("/icons/icon-512.png", "image/png"),
            ("/icons/apple-touch-icon.png", "image/png"),
        ] {
            let answer = get(&format!("{}{path}", evo.url), None);
            assert_eq!(answer.status, 200, "{path}");
            assert!(
                answer.content_type.contains(expected),
                "{path} came back as {}",
                answer.content_type
            );
            if path.ends_with(".js") || path.ends_with(".css") || path.ends_with(".html") {
                assert!(!answer.body.is_empty(), "{path} is empty");
            }
        }

        // Both conversations are in the shell, and the toggle that decides
        // whether the model may drive evo starts unchecked in the markup: a
        // browser with no stored preference must not begin with tools on.
        for part in [
            "id=\"panel-doc\"",
            "id=\"panel-agent\"",
            "id=\"tab-agent\"",
            "id=\"agent-tools\"",
            "id=\"chat-tools\"",
        ] {
            assert!(shell.contains(part), "the shell has no {part}");
        }
        assert!(!shell.contains("checked"), "tools are off until asked for");

        // The shell names them, so a rename that forgot one is caught here.
        for reference in [
            "/style.css",
            "/app.js",
            "/manifest.webmanifest",
            "/icons/apple-touch-icon.png",
        ] {
            assert!(
                shell.contains(reference),
                "the shell does not load {reference}"
            );
        }

        // The manifest has to be a manifest, and name icons that exist.
        let manifest = get(&format!("{}/manifest.webmanifest", evo.url), None);
        let parsed: Value = serde_json::from_slice(&manifest.body).expect("valid JSON");
        assert_eq!(parsed["display"], "standalone");
        assert_eq!(parsed["start_url"], "/");
        for icon in parsed["icons"].as_array().expect("icons") {
            let src = icon["src"].as_str().expect("a source");
            assert_eq!(
                get(&format!("{}{src}", evo.url), None).status,
                200,
                "{src} is named but not served"
            );
        }

        // And a file that was never embedded is a 404, not a guess.
        assert_eq!(get(&format!("{}/nope.js", evo.url), None).status, 404);
    }

    /// The phone cannot unlock a document: there is nobody to ask for the
    /// password and nowhere to keep one, and the library only holds documents
    /// everything can read. So the upload is refused as a fault in the
    /// request, with the one thing that does work.
    #[test]
    fn a_password_protected_upload_is_refused_with_somewhere_to_go() {
        let evo = Harness::start("encrypted-upload");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);

        for path in crate::doc::tests::PROTECTED {
            let refused = post_bytes(
                &docs,
                &session,
                &[("X-Evo-Filename", "locked.pdf")],
                crate::doc::tests::encrypted(path),
            );
            assert_eq!(refused.status, 422, "{path}: {}", refused.text());
            assert_eq!(
                refused.json()["error"],
                crate::serve::library_api::ENCRYPTED_UPLOAD,
                "{path}"
            );
            // Nothing was written down on the way to refusing.
            assert_eq!(get(&docs, Some(&session)).json()["count"], 0, "{path}");
        }

        // A document protected only against editing needs no password and is
        // an ordinary upload.
        let allowed = post_bytes(
            &docs,
            &session,
            &[("X-Evo-Filename", "permissions-only.pdf")],
            std::fs::read("tests/fixtures/encrypted-empty-user.pdf").expect("the fixture"),
        );
        assert_eq!(allowed.status, 201, "{}", allowed.text());
        assert_eq!(allowed.json()["pages"], 2);
    }

    /// The walk a phone actually does: put a document in, find it in the list,
    /// read its manifest, fetch a page, and take it out again.
    #[test]
    fn a_document_is_uploaded_read_and_deleted() {
        let evo = Harness::start("library");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);

        assert_eq!(get(&docs, Some(&session)).json()["count"], 0);

        let uploaded = post_bytes(
            &docs,
            &session,
            &[
                ("X-Evo-Title", "Boiler manual"),
                ("X-Evo-Filename", "boiler.pdf"),
            ],
            fixture(),
        );
        assert_eq!(uploaded.status, 201, "{}", uploaded.text());
        let id = uploaded.json()["id"].as_str().expect("an id").to_owned();
        assert_eq!(id.len(), 64, "the id is a digest");
        assert_eq!(uploaded.json()["duplicate"], false);

        // The same bytes again are the same document, and evo says so rather
        // than growing a second copy.
        let again = post_bytes(&docs, &session, &[], fixture());
        assert_eq!(again.status, 200, "a duplicate is not created");
        assert_eq!(again.json()["duplicate"], true);
        assert_eq!(again.json()["id"], id.as_str());

        let listed = get(&docs, Some(&session));
        assert_eq!(listed.json()["count"], 1);
        assert_eq!(listed.json()["documents"][0]["title"], "Boiler manual");
        assert_eq!(listed.json()["documents"][0]["pages"], 2);

        let one = get(&format!("{docs}/{id}"), Some(&session));
        assert_eq!(one.json()["filename"], "boiler.pdf");
        assert_eq!(one.json()["size"], fixture().len());

        // The manifest is what the viewer lays out from.
        let manifest = get(&format!("{docs}/{id}/manifest"), Some(&session));
        assert_eq!(manifest.status, 200, "{}", manifest.text());
        let pages = manifest.json()["pages"].as_array().expect("pages").clone();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0]["width"], 612.0);
        assert_eq!(pages[0]["height"], 792.0);
        assert!(manifest.json()["markup_etag"].as_str().is_some());
        assert_eq!(manifest.json()["chat_len"], 0);

        // A page image, at the size that was asked for, cached where it says.
        let page = get(&format!("{docs}/{id}/page/1.png?scale=2"), Some(&session));
        assert_eq!(page.status, 200, "{}", page.text());
        assert_eq!(page.content_type, "image/png");
        let decoded = image::load_from_memory(&page.body).expect("a real PNG");
        assert_eq!(decoded.width(), 1224);
        assert_eq!(decoded.height(), 1584);
        // Named for the engine that drew it, so a server whose configuration
        // changes engines never serves the other one's pixels.
        let engine = crate::render::engine::resolve(crate::render::engine::EnginePref::default());
        assert!(
            evo.dir
                .join(format!("library/pagecache/{id}/1-2-{}.png", engine.tag()))
                .exists(),
            "the render is kept"
        );
        // And the second ask is the cached one, byte for byte.
        let again = get(&format!("{docs}/{id}/page/1.png?scale=2"), Some(&session));
        assert_eq!(again.body, page.body);

        let thumb = get(&format!("{docs}/{id}/thumb.png"), Some(&session));
        assert_eq!(thumb.status, 200);
        assert_eq!(thumb.content_type, "image/png");

        // Pages that are not there, and scales that are not on offer.
        assert_eq!(
            get(&format!("{docs}/{id}/page/9.png"), Some(&session)).status,
            404
        );
        assert_eq!(
            get(&format!("{docs}/{id}/page/1.png?scale=7"), Some(&session)).status,
            400
        );
        assert_eq!(
            get(&format!("{docs}/{id}/page/one.png"), Some(&session)).status,
            400
        );

        let gone = delete(&format!("{docs}/{id}"), &session);
        assert_eq!(gone.status, 200, "{}", gone.text());
        assert_eq!(get(&docs, Some(&session)).json()["count"], 0);
        assert!(
            !evo.dir.join(format!("library/pagecache/{id}")).exists(),
            "the rendered pages go with the document"
        );
        assert_eq!(
            delete(&format!("{docs}/{id}"), &session).status,
            404,
            "deleting it twice is not deleting it twice"
        );
    }

    /// Markup is written conditionally, so two editors cannot silently
    /// overwrite each other -- and read back as an overlay the browser can lay
    /// straight over the page image.
    #[test]
    fn markup_round_trips_under_its_version_tag_and_draws_as_an_overlay() {
        let evo = Harness::start("markup");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);
        let uploaded = post_bytes(&docs, &session, &[], fixture());
        let id = uploaded.json()["id"].as_str().expect("an id").to_owned();
        let markup = format!("{docs}/{id}/markup");

        let empty = get(&markup, Some(&session));
        assert_eq!(empty.status, 200, "{}", empty.text());
        assert_eq!(
            empty.json()["annotations"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(
            empty.json()["pages"]["order"].as_array().map(Vec::len),
            Some(2),
            "a document with no markup still knows how many pages it has"
        );
        let tag = empty.headers.get("etag").expect("a version tag").to_owned();

        // A highlight 100pt up from the bottom of page one.
        let highlight = json!({
            "annotations": [{
                "id": 1,
                "page": 0,
                "kind": "Highlight",
                "rect": {"min": {"x": 72.0, "y": 100.0}, "max": {"x": 172.0, "y": 120.0}},
                "style": {
                    "stroke": {"r": 0, "g": 0, "b": 0, "a": 0},
                    "stroke_width": 0.0,
                    "fill": {"r": 255, "g": 235, "b": 59, "a": 255},
                    "opacity": 0.35
                }
            }]
        });

        // Without a version, nothing is written.
        let unconditional = put_json(&markup, &session, &[], highlight.clone());
        assert_eq!(unconditional.status, 412, "{}", unconditional.text());
        assert!(
            unconditional.text().contains("If-Match"),
            "{}",
            unconditional.text()
        );

        // Against the wrong version, nothing is written either -- and the
        // current one comes back so the client can re-apply its edit.
        let stale = put_json(
            &markup,
            &session,
            &[("If-Match", "\"nonsense\"")],
            highlight.clone(),
        );
        assert_eq!(stale.status, 409, "{}", stale.text());
        assert!(stale.json()["markup"].is_object(), "{}", stale.text());
        assert_eq!(stale.json()["etag"], tag.as_str());

        let saved = put_json(&markup, &session, &[("If-Match", &tag)], highlight);
        assert_eq!(saved.status, 200, "{}", saved.text());
        let new_tag = saved.json()["etag"].as_str().expect("a new tag").to_owned();
        assert_ne!(new_tag, tag, "the version moved");

        let read_back = get(&markup, Some(&session));
        assert_eq!(read_back.json()["annotations"][0]["page"], 0);
        assert_eq!(
            read_back.json()["pages"]["order"].as_array().map(Vec::len),
            Some(2),
            "a client that said nothing about page order changed nothing"
        );
        assert_eq!(read_back.headers.get("etag"), Some(&new_tag));
        assert_eq!(
            get(&format!("{docs}/{id}/manifest"), Some(&session)).json()["markup_etag"],
            new_tag.as_str(),
            "the manifest names the version the viewer would be drawing"
        );

        // The overlay: the page's own box, with the highlight flipped into it.
        let overlay = get(&format!("{docs}/{id}/markup.svg?page=1"), Some(&session));
        assert_eq!(overlay.status, 200, "{}", overlay.text());
        assert_eq!(overlay.content_type, "image/svg+xml");
        let svg = overlay.text();
        assert!(svg.contains("viewBox=\"0 0 612 792\""), "{svg}");
        assert!(svg.contains("id=\"evo-markup\""), "{svg}");
        // y = 792 - 120: PDF counts up from the bottom, SVG down from the top.
        assert!(svg.contains("y=\"672\""), "{svg}");

        // Page two has none of it.
        let second = get(&format!("{docs}/{id}/markup.svg?page=2"), Some(&session));
        assert!(!second.text().contains("<rect"), "{}", second.text());
        assert_eq!(
            get(&format!("{docs}/{id}/markup.svg?page=9"), Some(&session)).status,
            404
        );
    }

    /// Stamps go over the wire like everything else: an agent PUTs them, the
    /// server hands them straight back, and the overlay the phone draws holds
    /// the words and the picture. The PNG travels as base64 inside the JSON,
    /// which is the part worth pinning -- it is the only markup that carries
    /// bytes rather than numbers.
    #[test]
    fn stamps_round_trip_through_the_api_and_reach_the_overlay() {
        use base64::Engine as _;

        let evo = Harness::start("stamps");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);
        let id = post_bytes(&docs, &session, &[], fixture()).json()["id"]
            .as_str()
            .expect("an id")
            .to_owned();
        let markup = format!("{docs}/{id}/markup");
        let tag = get(&markup, Some(&session))
            .headers
            .get("etag")
            .expect("a version tag")
            .to_owned();

        let png = crate::export::pdf::tests::png_fixture(12, 6);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
        let style = json!({
            "stroke": {"r": 193, "g": 39, "b": 45, "a": 255},
            "stroke_width": 1.5,
            "fill": {"r": 0, "g": 0, "b": 0, "a": 0},
            "opacity": 1.0
        });
        let saved = put_json(
            &markup,
            &session,
            &[("If-Match", &tag)],
            json!({
                "version": 2,
                "annotations": [{
                    "id": 1,
                    "page": 0,
                    "kind": {"Stamp": {"text": "APPROVED", "font_size": 20.0}},
                    "rect": {"min": {"x": 100.0, "y": 700.0}, "max": {"x": 260.0, "y": 744.0}},
                    "style": style,
                }, {
                    "id": 2,
                    "page": 0,
                    "kind": {"ImageStamp": {"png": encoded}},
                    "rect": {"min": {"x": 300.0, "y": 700.0}, "max": {"x": 360.0, "y": 730.0}},
                    "style": style,
                    "group": 3,
                }]
            }),
        );
        assert_eq!(saved.status, 200, "{}", saved.text());

        let back = get(&markup, Some(&session));
        assert_eq!(back.json()["version"], 2);
        assert_eq!(
            back.json()["annotations"][0]["kind"]["Stamp"]["text"],
            "APPROVED"
        );
        assert_eq!(
            back.json()["annotations"][1]["kind"]["ImageStamp"]["png"],
            encoded.as_str(),
            "the picture came back byte for byte"
        );
        // Groups are additive to version 2: a body that names one is stored and
        // handed back, and one that does not mention groups says nothing about
        // them at all.
        assert_eq!(back.json()["annotations"][1]["group"], 3);
        assert!(
            back.json()["annotations"][0].get("group").is_none(),
            "{}",
            back.text()
        );

        let svg = get(&format!("{docs}/{id}/markup.svg?page=1"), Some(&session)).text();
        assert!(svg.contains(">APPROVED</text>"), "{svg}");
        assert!(
            svg.contains("data:image/png;base64,"),
            "no picture in: {svg}"
        );

        // A stamp is not a version-1 shape, and a client that says it is has
        // muddled its own format.
        let tag = back.headers.get("etag").expect("a tag").to_owned();
        let muddled = put_json(
            &markup,
            &session,
            &[("If-Match", &tag)],
            json!({
                "version": 1,
                "annotations": [{
                    "id": 1,
                    "page": 0,
                    "kind": {"Stamp": {"text": "DRAFT", "font_size": 20.0}},
                    "rect": {"min": {"x": 1.0, "y": 2.0}, "max": {"x": 3.0, "y": 4.0}},
                    "style": style,
                }]
            }),
        );
        assert_eq!(muddled.status, 400, "{}", muddled.text());
        assert!(
            muddled.text().contains("need format 2"),
            "{}",
            muddled.text()
        );
    }

    /// The promise the whole markup format is for: a highlight drawn on a
    /// phone is a highlight the desktop app opens.
    ///
    /// It cannot be checked by starting the app, so it is checked where the
    /// compatibility actually lives -- the sidecar. The server writes it
    /// through the API the phone uses; then the server stops, and the library
    /// is opened and read exactly as `state.rs` opens it, including the id
    /// allocation, which is where two writers would collide if the phone
    /// numbered annotations its own way.
    #[test]
    fn a_highlight_made_over_http_is_one_the_desktop_app_can_open() {
        use crate::doc::annotation::AnnotationKind;
        use crate::doc::store::AnnotationStore;

        let mut evo = Harness::start("sidecar");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);
        let id = post_bytes(&docs, &session, &[], fixture()).json()["id"]
            .as_str()
            .expect("an id")
            .to_owned();
        let markup = format!("{docs}/{id}/markup");

        // What viewer.js sends for a drag with the highlighter on: page 1,
        // yellow, a third opaque, and the id it worked out from what the
        // server had (nothing, so 1).
        let tag = get(&markup, Some(&session))
            .headers
            .get("etag")
            .expect("a version tag")
            .to_owned();
        let saved = put_json(
            &markup,
            &session,
            &[("If-Match", &tag)],
            json!({
                "version": 1,
                "annotations": [{
                    "id": 1,
                    "page": 0,
                    "kind": "Highlight",
                    "rect": {"min": {"x": 72.0, "y": 572.0}, "max": {"x": 172.0, "y": 592.0}},
                    "style": {
                        "stroke": {"r": 0, "g": 0, "b": 0, "a": 0},
                        "stroke_width": 0.0,
                        "fill": {"r": 250, "g": 220, "b": 50, "a": 255},
                        "opacity": 0.35
                    }
                }, {
                    "id": 2,
                    "page": 1,
                    "kind": {"TextBox": {"text": "check this", "font_size": 11.0, "align": "Left"}},
                    "rect": {"min": {"x": 72.0, "y": 500.0}, "max": {"x": 240.0, "y": 540.0}},
                    "style": {
                        "stroke": {"r": 30, "g": 30, "b": 46, "a": 255},
                        "stroke_width": 0.0,
                        "fill": {"r": 255, "g": 245, "b": 180, "a": 255},
                        "opacity": 0.95
                    }
                }]
            }),
        );
        assert_eq!(saved.status, 200, "{}", saved.text());

        // The phone is put down and the server stopped -- which is what has to
        // happen before the desktop app can have this library at all.
        evo.stop();

        let library = Library::open_at(evo.dir.join("library")).expect("the library, unlocked");
        let sidecar = library
            .load_markup(&id)
            .expect("reading the sidecar")
            .expect("the phone wrote one");
        // The phone said version 1, because that is what its copy of the app
        // knows; what lands on disk is the format this build writes.
        assert_eq!(sidecar.version, crate::library::MARKUP_VERSION);
        assert_eq!(sidecar.pages.order.len(), 2, "the page order survived");

        let mut store = AnnotationStore::restore(sidecar.annotations);
        let first = store.get(1).expect("the highlight");
        assert_eq!(first.page, 0);
        assert!(matches!(first.kind, AnnotationKind::Highlight));
        assert_eq!(first.rect.min.y, 572.0, "in PDF points, up from the bottom");
        assert_eq!(first.style.fill.r, 250);
        assert!((first.style.opacity - 0.35).abs() < 1e-6);
        assert!(
            !first.style.stroke.is_visible(),
            "a highlight has no outline"
        );

        let note = store.get(2).expect("the note");
        match &note.kind {
            AnnotationKind::TextBox {
                text, font_size, ..
            } => {
                assert_eq!(text, "check this");
                assert!(*font_size > 0.0);
            }
            other => panic!("the note came back as {other:?}"),
        }

        // And the next annotation the app draws does not land on top of one of
        // the phone's: both sides number from the highest id there is.
        assert_eq!(store.alloc_id(), 3);

        let _ = std::fs::remove_dir_all(&evo.dir);
    }

    /// The shapes v0.6 added, over the wire the way an agent would send them:
    /// saved as format 2, read back unchanged, and -- the part that matters to
    /// a phone -- drawn into the overlay it lays over the page image, without
    /// the browser having to learn a thing about polygons.
    #[test]
    fn a_cloud_and_a_polyline_survive_the_api_and_reach_the_overlay() {
        let mut evo = Harness::start("polygons");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);
        let id = post_bytes(&docs, &session, &[], fixture()).json()["id"]
            .as_str()
            .expect("an id")
            .to_owned();
        let markup = format!("{docs}/{id}/markup");

        let tag = get(&markup, Some(&session))
            .headers
            .get("etag")
            .expect("a version tag")
            .to_owned();
        let style = json!({
            "stroke": {"r": 220, "g": 38, "b": 38, "a": 255},
            "stroke_width": 2.0,
            "fill": {"r": 0, "g": 0, "b": 0, "a": 0},
            "opacity": 1.0
        });
        let saved = put_json(
            &markup,
            &session,
            &[("If-Match", &tag)],
            json!({
                "version": 2,
                "annotations": [{
                    "id": 1,
                    "page": 0,
                    "kind": {"Polygon": {
                        "points": [
                            {"x": 100.0, "y": 600.0},
                            {"x": 300.0, "y": 600.0},
                            {"x": 300.0, "y": 700.0},
                            {"x": 100.0, "y": 700.0}
                        ],
                        "cloudy": 1.5
                    }},
                    "rect": {"min": {"x": 100.0, "y": 600.0}, "max": {"x": 300.0, "y": 700.0}},
                    "style": style,
                }, {
                    "id": 2,
                    "page": 0,
                    "kind": {"PolyLine": {
                        "points": [
                            {"x": 72.0, "y": 200.0},
                            {"x": 200.0, "y": 260.0},
                            {"x": 320.0, "y": 200.0}
                        ],
                        "arrow_end": true
                    }},
                    "rect": {"min": {"x": 72.0, "y": 200.0}, "max": {"x": 320.0, "y": 260.0}},
                    "style": style,
                }]
            }),
        );
        assert_eq!(saved.status, 200, "{}", saved.text());

        // Read back: the same two shapes, and the format this build writes.
        let read = get(&markup, Some(&session));
        assert_eq!(read.status, 200);
        let body = read.json();
        assert_eq!(body["version"].as_u64(), Some(2));
        assert_eq!(body["annotations"][0]["kind"]["Polygon"]["cloudy"], 1.5);
        assert_eq!(
            body["annotations"][1]["kind"]["PolyLine"]["arrow_end"],
            true
        );

        // The overlay the browser lays over the page image: a scalloped path
        // for the cloud, a straight one for the polyline, and its arrowhead.
        let overlay = get(&format!("{docs}/{id}/markup.svg?page=1"), Some(&session));
        assert_eq!(overlay.status, 200);
        let svg = overlay.text();
        assert_eq!(svg.matches("<path").count(), 2, "{svg}");
        assert!(svg.matches(" C ").count() > 12, "the cloud is flat: {svg}");
        assert!(svg.contains("<polygon points="), "no arrowhead: {svg}");

        // A client that still speaks the old format may write what the old
        // format could describe...
        let tag = read.headers.get("etag").expect("a tag").to_owned();
        let old = put_json(
            &markup,
            &session,
            &[("If-Match", &tag)],
            json!({"version": 1, "annotations": []}),
        );
        assert_eq!(old.status, 200, "{}", old.text());

        // ...and is refused, in a sentence, when it claims a polygon is one.
        let tag = get(&markup, Some(&session))
            .headers
            .get("etag")
            .expect("a tag")
            .to_owned();
        let muddled = put_json(
            &markup,
            &session,
            &[("If-Match", &tag)],
            json!({
                "version": 1,
                "annotations": [{
                    "id": 1,
                    "page": 0,
                    "kind": {"Polygon": {"points": [{"x": 1.0, "y": 2.0}], "cloudy": null}},
                    "rect": {"min": {"x": 1.0, "y": 2.0}, "max": {"x": 3.0, "y": 4.0}},
                    "style": style,
                }]
            }),
        );
        assert_eq!(muddled.status, 400, "{}", muddled.text());
        assert!(muddled.text().contains("format 2"), "{}", muddled.text());

        // And what the desktop app opens afterwards is the empty layer the
        // old client last wrote, stamped with this build's version.
        evo.stop();
        let library = Library::open_at(evo.dir.join("library")).expect("the library, unlocked");
        let sidecar = library
            .load_markup(&id)
            .expect("reading the sidecar")
            .expect("one was written");
        assert_eq!(sidecar.version, crate::library::MARKUP_VERSION);
        assert!(sidecar.annotations.is_empty());

        let _ = std::fs::remove_dir_all(&evo.dir);
    }

    /// Chat: what the browser gets before the model is reached, what it gets
    /// when the model cannot be reached at all, and the transcript either way.
    ///
    /// The failure is the interesting half. There is no model in CI and there
    /// must not be one in a test, so the harness points at a dead port -- which
    /// exercises the whole stream: the answer is a 200 with events in it, and
    /// the thing that went wrong arrives as one of them rather than as a
    /// status, because by the time it is known the status has been sent.
    #[test]
    fn a_question_streams_events_and_the_conversation_is_kept() {
        let evo = Harness::start("chat");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);
        let id = post_bytes(&docs, &session, &[], fixture()).json()["id"]
            .as_str()
            .expect("an id")
            .to_owned();

        // A question about nothing, and nothing of a question: both are facts
        // about the request, so both are statuses.
        let absent = post_json(
            &format!("{docs}/{}/chat", "f".repeat(64)),
            Some(&session),
            json!({"question": "what is this?"}),
        );
        assert_eq!(absent.status, 404, "{}", absent.text());
        let blank = post_json(
            &format!("{docs}/{id}/chat"),
            Some(&session),
            json!({"question": "   "}),
        );
        assert_eq!(blank.status, 400, "{}", blank.text());
        assert!(blank.text().contains("no question"), "{}", blank.text());

        // A real question. The model is unreachable, so what comes back is the
        // stream up to the point where that was discovered.
        let asked = post_json(
            &format!("{docs}/{id}/chat"),
            Some(&session),
            json!({"question": "what is the fox doing?", "history": [], "tools": false}),
        );
        assert_eq!(asked.status, 200, "{}", asked.text());
        assert!(
            asked.content_type.starts_with("text/event-stream"),
            "{}",
            asked.content_type
        );
        let stream = asked.text();
        assert!(stream.contains("event: stage"), "{stream}");
        assert!(stream.contains("Reading the document"), "{stream}");
        assert!(stream.contains("event: error"), "{stream}");
        assert!(!stream.contains("event: done"), "{stream}");
        // Every frame's data is one line of JSON: that is what stops a model's
        // paragraph break from ending the event.
        for line in stream.lines().filter(|l| l.starts_with("data:")) {
            let data: Value = serde_json::from_str(line.trim_start_matches("data:").trim())
                .unwrap_or_else(|e| panic!("{line} is not JSON: {e}"));
            assert!(data.is_object(), "{line}");
        }

        // The transcript is the desktop app's CHATS table, reached over HTTP.
        let empty = get(&format!("{docs}/{id}/chatlog"), Some(&session));
        assert_eq!(empty.status, 200, "{}", empty.text());
        assert_eq!(empty.json()["messages"].as_array().map(Vec::len), Some(0));

        let kept = put_json(
            &format!("{docs}/{id}/chatlog"),
            &session,
            &[],
            json!({"messages": [
                {"role": "user", "content": "what is the fox doing?"},
                {"role": "assistant", "content": "Jumping. [p.1]"}
            ]}),
        );
        assert_eq!(kept.status, 200, "{}", kept.text());

        let read_back = get(&format!("{docs}/{id}/chatlog"), Some(&session));
        assert_eq!(read_back.json()["messages"][1]["content"], "Jumping. [p.1]");
        assert_eq!(read_back.json()["messages"][1]["role"], "assistant");
        assert_eq!(
            get(&format!("{docs}/{id}/manifest"), Some(&session)).json()["chat_len"],
            2,
            "the manifest says there is a conversation to reopen"
        );

        // Clearing it is saying nothing was said.
        put_json(
            &format!("{docs}/{id}/chatlog"),
            &session,
            &[],
            json!({"messages": []}),
        );
        assert_eq!(
            get(&format!("{docs}/{id}/chatlog"), Some(&session)).json()["messages"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );

        // And a transcript nobody could have had is refused rather than kept.
        let flood: Vec<Value> = (0..600)
            .map(|n| json!({"role": "user", "content": format!("{n}")}))
            .collect();
        let refused = put_json(
            &format!("{docs}/{id}/chatlog"),
            &session,
            &[],
            json!({ "messages": flood }),
        );
        assert_eq!(refused.status, 413, "{}", refused.text());
    }

    // -----------------------------------------------------------------------
    // The agent
    // -----------------------------------------------------------------------

    /// A model that answers from a prepared script, recording what it was
    /// offered. No weights, no server, no network -- which is what lets a whole
    /// agent turn be watched in CI.
    #[derive(Default)]
    struct Script {
        replies: std::collections::VecDeque<GenerateOutcome>,
        /// The tools named in each request, and the system prompt that came
        /// with it: what the model was actually told it could do.
        offered: Vec<(Vec<String>, String)>,
    }

    struct Scripted(Arc<Mutex<Script>>);

    impl ModelBackend for Scripted {
        fn generate(
            &self,
            req: &GenerateRequest,
            on_token: &mut dyn FnMut(&str) -> ControlFlow<()>,
        ) -> Result<GenerateOutcome, ModelError> {
            let outcome = {
                let mut script = self.0.lock().unwrap();
                script.offered.push((
                    req.tools.iter().map(|t| t.name.clone()).collect(),
                    req.system.clone().unwrap_or_default(),
                ));
                script
                    .replies
                    .pop_front()
                    .unwrap_or_else(|| GenerateOutcome::text("out of script"))
            };
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

    fn calls(name: &str, arguments: Value) -> GenerateOutcome {
        GenerateOutcome {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: Some(format!("call_{name}")),
                name: name.to_owned(),
                arguments,
            }],
        }
    }

    /// Every frame of an event stream, in order, as `(event, data)`.
    fn frames(stream: &str) -> Vec<(String, Value)> {
        stream
            .split("\n\n")
            .filter_map(|block| {
                let mut name = String::new();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(rest) = line.strip_prefix("event:") {
                        name = rest.trim().to_owned();
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data.push_str(rest.trim_start());
                    }
                }
                (!data.is_empty()).then(|| {
                    let parsed: Value = serde_json::from_str(&data)
                        .unwrap_or_else(|e| panic!("{data} is not JSON: {e}"));
                    (name, parsed)
                })
            })
            .collect()
    }

    /// The whole promise of this milestone, over a real socket: the agent is
    /// asked to find something and mark it, it uses evo's own tools to do so,
    /// and what it did arrives on the same stream as its words -- as `ui`
    /// frames the app acts on, not as a description of what it would have done.
    ///
    /// The markup it made is then the markup the API serves, under a version
    /// tag that has moved. That is the join between a tool's write and the
    /// conditional writes the viewer makes; if it were not the same hash, a
    /// phone would go on drawing yesterday's overlay.
    #[test]
    fn the_agent_drives_evo_and_the_markup_it_makes_is_the_markup_the_api_serves() {
        let script = Arc::new(Mutex::new(Script::default()));
        let model = script.clone();
        let evo = Harness::start_with_model(
            "agent",
            Arc::new(move |_config: &crate::script::model::ModelConfig| {
                Box::new(Scripted(model.clone())) as Box<dyn ModelBackend>
            }),
        );
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);
        let id =
            post_bytes(&docs, &session, &[("X-Evo-Title", "Fox report")], fixture()).json()["id"]
                .as_str()
                .expect("an id")
                .to_owned();

        // What the markup is before anybody touches it, and the tag the viewer
        // would be holding.
        let before = get(&format!("{docs}/{id}/markup"), Some(&session));
        let first_tag = before.headers.get("etag").expect("a tag").to_owned();

        // Three rounds: look at it, mark it, say what happened.
        script.lock().unwrap().replies.extend([
            calls("open_document", json!({"doc_id": id, "page": 1})),
            calls(
                "highlight_text",
                json!({"doc_id": id, "page": 1, "text": "quick brown fox", "note": "the subject"}),
            ),
            GenerateOutcome::text("Marked it on page 1. [p.1]"),
        ]);

        let asked = post_json(
            &format!("{}/api/agent/chat", evo.url),
            Some(&session),
            json!({
                "question": "find the fox and highlight it",
                "history": [],
                "tools": true,
            }),
        );
        assert_eq!(asked.status, 200, "{}", asked.text());
        assert!(
            asked.content_type.starts_with("text/event-stream"),
            "{}",
            asked.content_type
        );
        let stream = frames(&asked.text());
        let names: Vec<&str> = stream.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"ui"), "the app was never driven: {names:?}");
        assert_eq!(names.last(), Some(&"done"), "{names:?}");

        // The reader watched each tool run.
        let chips: Vec<&str> = stream
            .iter()
            .filter(|(name, _)| name == "tool")
            .filter_map(|(_, data)| data["text"].as_str())
            .collect();
        assert!(chips.contains(&"Running open_document…"), "{chips:?}");
        assert!(chips.contains(&"Running highlight_text…"), "{chips:?}");
        assert!(
            !chips.iter().any(|chip| chip.contains("failed")),
            "a tool did not work: {chips:?}"
        );

        // And each one told the app what to do about it.
        let ui: Vec<&Value> = stream
            .iter()
            .filter(|(name, _)| name == "ui")
            .map(|(_, data)| data)
            .collect();
        assert_eq!(ui.len(), 2, "{ui:?}");
        assert_eq!(ui[0]["action"], "open");
        assert_eq!(ui[0]["doc"], id.as_str());
        assert_eq!(ui[0]["page"], 1);
        assert_eq!(ui[1]["action"], "markup-changed");
        assert_eq!(ui[1]["doc"], id.as_str());

        let done = &stream.last().expect("a done frame").1;
        assert_eq!(done["text"], "Marked it on page 1. [p.1]");
        assert_eq!(
            done["tools_used"],
            json!(["open_document", "highlight_text"])
        );

        // The five tools were really offered, and the prompt says what they are
        // for -- the model is told it is driving evo, not asked to guess.
        let offered = &script.lock().unwrap().offered[0];
        let mut names = offered.0.clone();
        names.sort();
        assert_eq!(
            names,
            [
                "get_document_text",
                "highlight_text",
                "list_library",
                "open_document",
                "search_library"
            ]
        );
        assert!(offered.1.contains("highlight_text"), "{}", offered.1);

        // The markup the tool wrote is the markup the API now serves, at the
        // version the `ui` event named.
        let after = get(&format!("{docs}/{id}/markup"), Some(&session));
        let tag = after.headers.get("etag").expect("a tag").to_owned();
        assert_ne!(tag, first_tag, "the version tag moved");
        assert_eq!(ui[1]["etag"], tag.as_str(), "and moved to the one it said");
        assert_eq!(
            get(&format!("{docs}/{id}/manifest"), Some(&session)).json()["markup_etag"],
            tag.as_str(),
            "the manifest agrees with it"
        );
        let annotations = after.json()["annotations"].as_array().cloned().unwrap();
        assert_eq!(annotations.len(), 2, "a highlight and its note");
        assert_eq!(annotations[0]["kind"], "Highlight");
        assert_eq!(annotations[0]["page"], 0);
        assert_eq!(
            annotations[1]["kind"]["TextBox"]["text"],
            "the subject",
            "{after:?}",
            after = after.text()
        );

        // The overlay draws it, which is what the phone actually fetches.
        let overlay = get(&format!("{docs}/{id}/markup.svg?page=1"), Some(&session));
        assert_eq!(overlay.status, 200);
        assert!(overlay.text().contains("<rect"), "{}", overlay.text());
        assert_eq!(overlay.headers.get("etag"), Some(&tag));
    }

    /// With tools switched off -- the default, and what the toggle in the app
    /// starts at -- the agent is a conversation and nothing more: no tools are
    /// offered, and the prompt says the library cannot be seen.
    #[test]
    fn an_agent_without_tools_is_offered_none_and_is_told_so() {
        let script = Arc::new(Mutex::new(Script::default()));
        let model = script.clone();
        let evo = Harness::start_with_model(
            "agent-quiet",
            Arc::new(move |_config: &crate::script::model::ModelConfig| {
                Box::new(Scripted(model.clone())) as Box<dyn ModelBackend>
            }),
        );
        let session = sign_in(&evo);
        script
            .lock()
            .unwrap()
            .replies
            .push_back(GenerateOutcome::text("I cannot see the library."));

        let asked = post_json(
            &format!("{}/api/agent/chat", evo.url),
            Some(&session),
            json!({"question": "what is in my library?"}),
        );
        assert_eq!(asked.status, 200, "{}", asked.text());
        let stream = frames(&asked.text());
        let names: Vec<&str> = stream.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["stage", "token", "done"], "nothing was driven");
        assert_eq!(stream[2].1["tools_used"], json!([]));

        let offered = &script.lock().unwrap().offered[0];
        assert!(offered.0.is_empty(), "{:?}", offered.0);
        assert!(offered.1.contains("Allow tools"), "{}", offered.1);

        // A question with nothing in it is a fact about the request, so it is a
        // status rather than an event.
        assert_eq!(
            post_json(
                &format!("{}/api/agent/chat", evo.url),
                Some(&session),
                json!({"question": "  "}),
            )
            .status,
            400
        );
        // And the agent is behind the session guard like everything else.
        assert_eq!(
            post_json(
                &format!("{}/api/agent/chat", evo.url),
                None,
                json!({"question": "hello"}),
            )
            .status,
            401
        );
    }

    /// An id becomes a filename in three places, so nothing that is not a
    /// digest may get that far -- and a well-formed id for a document that is
    /// not there is a plain 404, not a hint about what is.
    #[test]
    fn nothing_that_is_not_a_digest_is_treated_as_a_document() {
        let evo = Harness::start("ids");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);

        for id in [
            "%2e%2e",
            "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
            "not-a-digest",
            &"A".repeat(64),
            &"f".repeat(63),
        ] {
            for suffix in ["", "/manifest", "/markup", "/thumb.png", "/page/1.png"] {
                let answer = get(&format!("{docs}/{id}{suffix}"), Some(&session));
                assert_eq!(answer.status, 400, "{id}{suffix} -> {}", answer.text());
            }
            assert_eq!(
                delete(&format!("{docs}/{id}"), &session).status,
                400,
                "{id}"
            );
        }

        // A real-looking id for a document that was never here.
        let absent = "f".repeat(64);
        for suffix in ["", "/manifest", "/markup", "/thumb.png", "/page/1.png"] {
            let answer = get(&format!("{docs}/{absent}{suffix}"), Some(&session));
            assert_eq!(answer.status, 404, "{suffix} -> {}", answer.text());
        }
    }

    /// The other half of the library: text. Upload, wait for the background
    /// worker to read the pages, then find the document by something written
    /// inside it.
    #[test]
    fn a_document_becomes_searchable_once_it_has_been_read() {
        let evo = Harness::start_indexing("search");
        let session = sign_in(&evo);
        let docs = format!("{}/api/docs", evo.url);
        let uploaded = post_bytes(&docs, &session, &[], fixture());
        assert_eq!(uploaded.status, 201, "{}", uploaded.text());
        let id = uploaded.json()["id"].as_str().expect("an id").to_owned();

        // Extraction is a background thread; polling is what the phone does too.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let indexed = loop {
            if get(&format!("{docs}/{id}"), Some(&session)).json()["indexed"] == true {
                break true;
            }
            if std::time::Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert!(indexed, "the fixture's text was never read");

        let hits = get(&format!("{docs}?q=brown"), Some(&session));
        assert_eq!(hits.status, 200, "{}", hits.text());
        assert_eq!(hits.json()["count"], 1, "{}", hits.text());
        assert_eq!(hits.json()["matches"][0]["doc_id"], id.as_str());
        assert_eq!(
            hits.json()["matches"][0]["page"],
            1,
            "the first page is page 1"
        );
        assert!(
            hits.json()["matches"][0]["snippet"]
                .as_str()
                .is_some_and(|s| s.contains("brown")),
            "{}",
            hits.text()
        );

        // A search for something nobody wrote finds nothing, rather than
        // everything.
        let none = get(&format!("{docs}?q=xyzzy"), Some(&session));
        assert_eq!(none.json()["count"], 0);

        // Status says what the server has been doing.
        let status = get(&format!("{}/api/status", evo.url), Some(&session));
        assert_eq!(status.json()["documents"], 1);
        assert!(status.json()["index"].is_object(), "{}", status.text());
        assert_eq!(status.json()["blobs"], "local");
    }
}
