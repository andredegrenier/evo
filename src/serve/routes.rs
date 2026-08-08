//! The HTTP surface: what is on offer, and in what order the guards run.
//!
//! Assembling the router is a function rather than something inlined into
//! startup so the tests exercise the shipped one. The authorization rules are
//! not something to have a second implementation of.

use axum::Json;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rust_embed::Embed;
use serde_json::json;

use super::{Shared, auth};

/// The web app, read from `assets/web/` at build time.
///
/// In debug builds rust-embed reads the files from disk on every request, so
/// the HTML and CSS can be edited while the server runs; release builds carry
/// them inside the binary, which is what makes `evo` a single file to copy onto
/// a server.
#[derive(Embed)]
#[folder = "assets/web/"]
struct Assets;

/// The whole thing: four endpoints, the app shell, and two layers of guard.
///
/// Order matters. `layer` wraps everything registered before it, so the CSRF
/// check -- added last, and therefore outermost -- runs first, and the session
/// check runs second. Both wrap the fallback too, which is what makes an
/// unknown `/api/` path answer 401 rather than 404: evo does not confirm which
/// endpoints exist to someone who has not signed in.
pub fn router(state: Shared) -> axum::Router {
    axum::Router::new()
        .route("/api/health", get(health))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/setup-qr", get(auth::setup_qr))
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
            let dir = std::env::temp_dir().join(format!("evo-serve-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let paths = ServePaths::new(Some(dir.clone()), None).expect("paths");
            std::fs::create_dir_all(&paths.serve_dir).expect("the serve directory");

            let auth = auth::AuthStore::create(PASSWORD).expect("credentials");
            auth.save(&paths.auth).expect("saving the credentials");
            let secret = auth.totp_secret.clone();

            let library = Library::open_at(paths.library_root.clone()).expect("a library");
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
            .timeout_global(Some(std::time::Duration::from_secs(20)))
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
                };
            }
            Err(e) => panic!("{e}"),
        };
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let cookie = header("set-cookie");
        let content_type = header("content-type").unwrap_or_default();
        Answer {
            status,
            body: response.into_body().read_to_vec().unwrap_or_default(),
            content_type,
            cookie,
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
        assert_eq!(
            known.status, 404,
            "past the guard, and there is no such route yet"
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
}
