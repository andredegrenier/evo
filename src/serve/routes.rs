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

use super::{Shared, auth, library_api, markup_api, pages};

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
            // The shell is small and changes with each release. M24's service
            // worker is what will make it fast offline; guessing at cache
            // lifetimes before then only makes upgrades confusing.
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
    use crate::serve::{ServeConfig, ServeOptions, ServePaths, ServeState, now_secs};
    use serde_json::Value;
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
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.cancel.cancel();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Harness {
        fn start(name: &str) -> Self {
            Self::start_with(name, false)
        }

        /// The same server with the text-extraction worker running. Only the
        /// search test needs it: it is a thread and a tantivy index, and every
        /// other test would be paying for both.
        fn start_indexing(name: &str) -> Self {
            Self::start_with(name, true)
        }

        fn start_with(name: &str, index: bool) -> Self {
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
                library.start_indexer(&eframe::egui::Context::default());
            }
            let state = Arc::new(ServeState {
                library: Arc::new(Mutex::new(library)),
                config: ServeConfig::default(),
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
            });

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
        assert!(
            shell.text().contains("/api/login"),
            "the form posts to the API"
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
        assert!(
            evo.dir
                .join(format!("library/pagecache/{id}/1-2.png"))
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
