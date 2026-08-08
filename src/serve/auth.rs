//! Who may talk to `evo serve`: a password, an authenticator app, and a cookie.
//!
//! The threat this is built for is a port on the open internet. So: the
//! password is argon2id-hashed and never stored in the clear, a second factor
//! is mandatory (a stolen password alone gets nobody in), codes are single-use
//! so replaying one off the wire is worthless, sessions are stored as hashes so
//! reading `sessions.json` does not let you become anybody, login is rate
//! limited per client address, and every mutation needs a header no
//! cross-origin form can set.
//!
//! Every use of the `totp-rs` crate lives in this file. It is version 6.0.0,
//! which was days old when this was written; keeping it in one module is what
//! makes dropping back to 5.7 a local edit rather than an archaeology project.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use totp_rs::{Algorithm, Builder, Secret, Totp};

use crate::mcp::token_matches;

use super::{ServeOptions, ServePaths, Shared, now_secs};

/// What an authenticator app files the entry under.
const ISSUER: &str = "evo";
/// The account name inside that entry. Shown next to the code on the phone.
const ACCOUNT: &str = "evo serve";
/// Six digits, thirty seconds, SHA-1: what every authenticator app expects.
/// Anything else is silently mishandled by some of them.
const DIGITS: u8 = 6;
const STEP: u64 = 30;
/// One step either side of now, per RFC 6238 §5.2. More would be generous to
/// an attacker for no gain to a phone with a working clock.
const SKEW: u16 = 1;

/// A month, so the phone is not asked for a code every morning.
pub const SESSION_SECS: u64 = 30 * 24 * 60 * 60;
/// A session is only rewritten to disk when it has less than this much life
/// left. Sliding expiry should not mean a file write per request.
const REFRESH_AFTER: u64 = SESSION_SECS - 24 * 60 * 60;

/// How many sign-in attempts one address gets, and over what window.
pub const LOGIN_ATTEMPTS: usize = 5;
pub const LOGIN_WINDOW_SECS: u64 = 5 * 60;

/// Long enough to fetch a QR code and scan it, short enough to be worthless if
/// it leaks.
pub const SETUP_SECS: u64 = 10 * 60;

/// The shortest password `evo serve init` will accept. Short of a policy nobody
/// reads, this at least rules out the ones typed by accident.
const MIN_PASSWORD: usize = 8;

/// The header every mutation must carry. Its value does not matter; that a
/// cross-origin form cannot set a custom header at all is the point.
pub const CSRF_HEADER: &str = "x-evo";

// ---------------------------------------------------------------------------
// The secrets on disk
// ---------------------------------------------------------------------------

/// `<data-dir>/serve/auth.json`, mode 0600.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthStore {
    /// PHC string: algorithm, parameters, salt and hash, all in one field.
    pub password_hash: String,
    /// Base32, as authenticator apps spell it.
    pub totp_secret: String,
    /// False until a code from the app has been accepted once. Until then the
    /// server offers the QR code instead of asking for a code.
    #[serde(default)]
    pub totp_confirmed: bool,
    /// The last time step accepted. RFC 6238 §5.2 says a code is good once;
    /// this is what makes that true, and it is why a code copied off the
    /// network is of no use a second later.
    #[serde(default)]
    pub last_totp_step: u64,
}

/// What a submitted code turned out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeCheck {
    Accepted,
    /// Right code, already used. Worth its own answer because it is the shape
    /// of an attack, not of a typo.
    Replayed,
    Wrong,
}

impl AuthStore {
    /// Hash the password and mint a fresh 160-bit authenticator secret.
    pub fn create(password: &str) -> Result<Self, String> {
        if password.chars().count() < MIN_PASSWORD {
            return Err(format!(
                "that password is {} characters; evo wants at least {MIN_PASSWORD}.",
                password.chars().count()
            ));
        }
        Ok(Self {
            password_hash: hash_password(password)?,
            totp_secret: new_totp_secret()?,
            totp_confirmed: false,
            last_totp_step: 0,
        })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "evo serve has no password yet. Run `evo serve init` first (it writes {}).",
                    path.display()
                )
            } else {
                format!("could not read {}: {e}", path.display())
            }
        })?;
        serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid evo serve credentials: {e}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("could not write the credentials: {e}"))?;
        write_private(path, &text)
    }

    /// The authenticator, built from the stored secret.
    pub fn totp(&self) -> Result<Totp, String> {
        let secret = Secret::try_from_base32(&self.totp_secret)
            .map_err(|_| "the stored authenticator secret is not valid base32.".to_owned())?;
        Builder::new()
            .with_algorithm(Algorithm::SHA1)
            .with_digits(DIGITS)
            .with_skew(SKEW)
            .with_step_duration(STEP)
            .with_secret(secret)
            .with_issuer(Some(ISSUER))
            .with_account_name(ACCOUNT)
            .build()
            .map_err(|e| format!("could not set up the authenticator: {e}"))
    }

    /// What goes into the authenticator app: `otpauth://totp/evo:evo%20serve?secret=…&issuer=evo`.
    pub fn otpauth_url(&self) -> Result<String, String> {
        self.totp()?
            .to_url()
            .map_err(|e| format!("could not write the enrolment URL: {e}"))
    }

    pub fn qr_png(&self) -> Result<Vec<u8>, String> {
        self.totp()?
            .to_qr_png()
            .map_err(|e| format!("could not draw the enrolment QR code: {e}"))
    }

    /// Check a code, and remember it so it cannot be used twice.
    ///
    /// A first success is also enrolment finished: the app has demonstrably got
    /// the secret, so there is nothing left to confirm.
    pub fn check_code(&mut self, code: &str, now: u64) -> Result<CodeCheck, String> {
        let totp = self.totp()?;
        match totp.check(code.trim(), now) {
            None => Ok(CodeCheck::Wrong),
            Some(step) if step <= self.last_totp_step => Ok(CodeCheck::Replayed),
            Some(step) => {
                self.last_totp_step = step;
                self.totp_confirmed = true;
                Ok(CodeCheck::Accepted)
            }
        }
    }
}

/// A 160-bit secret, base32-encoded: the size RFC 4226 §4 recommends, and what
/// authenticator apps are built around.
fn new_totp_secret() -> Result<String, String> {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes)
        .map_err(|e| format!("evo could not get random bytes for the authenticator secret: {e}"))?;
    Ok(Secret::from(bytes).to_base32())
}

fn hash_password(password: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt)
        .map_err(|e| format!("evo could not get random bytes for the password salt: {e}"))?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| format!("could not make a salt: {e}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("could not hash the password: {e}"))
}

/// Verify against a PHC string. A hash evo cannot parse is not a reason to let
/// anyone in, so every failure is the same `false`.
fn verify_password(stored: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Write a file only its owner can read. The credentials and the session table
/// are both "anyone holding this is you".
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    // `mode` applies only when the file is created, so an existing file with
    // looser permissions has to be corrected on purpose.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    file.write_all(text.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Session {
    /// sha256 of the token, hex. The server never keeps a token it could leak.
    hash: String,
    /// Unix seconds.
    expires: u64,
}

/// `<data-dir>/serve/sessions.json`, mode 0600.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sessions {
    #[serde(default)]
    entries: Vec<Session>,
}

/// What a presented cookie turned out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionCheck {
    /// Good, and nothing on disk needs changing.
    Valid,
    /// Good, and the expiry was pushed out -- the caller should save.
    Refreshed,
    /// Unknown, expired, or not a session at all.
    Missing,
}

impl Sessions {
    /// A missing file is an empty table: no sessions is the correct state for a
    /// server that has never been signed in to.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is not a valid session table: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("could not read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string(self)
            .map_err(|e| format!("could not write the session table: {e}"))?;
        write_private(path, &text)
    }

    /// Mint a session and return the token, which is the only time it exists in
    /// a form anyone could read.
    pub fn create(&mut self, now: u64) -> Result<String, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| format!("evo could not get random bytes for the session: {e}"))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.prune(now);
        self.entries.push(Session {
            hash: hash_token(&token),
            expires: now + SESSION_SECS,
        });
        Ok(token)
    }

    /// Is this cookie a session, and does using it extend it?
    pub fn check(&mut self, token: &str, now: u64) -> SessionCheck {
        let presented = hash_token(token);
        self.prune(now);
        // Constant-time, even though these are hashes rather than secrets:
        // there is no reason to publish how far a guess got.
        let Some(session) = self
            .entries
            .iter_mut()
            .find(|s| token_matches(&s.hash, &presented))
        else {
            return SessionCheck::Missing;
        };
        if session.expires - now < REFRESH_AFTER {
            session.expires = now + SESSION_SECS;
            SessionCheck::Refreshed
        } else {
            SessionCheck::Valid
        }
    }

    /// Sign out. Returns whether anything was actually removed.
    pub fn revoke(&mut self, token: &str) -> bool {
        let presented = hash_token(token);
        let before = self.entries.len();
        self.entries.retain(|s| !token_matches(&s.hash, &presented));
        self.entries.len() != before
    }

    fn prune(&mut self, now: u64) {
        self.entries.retain(|s| s.expires > now);
    }
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Sign-in attempts per client address. In memory only: a restart forgiving the
/// count is not worth a file write on every wrong password.
#[derive(Debug, Default)]
pub struct RateLimiter {
    attempts: HashMap<String, Vec<u64>>,
}

impl RateLimiter {
    /// Record an attempt from `key` and say whether it is allowed.
    pub fn allow(&mut self, key: &str, now: u64) -> bool {
        // Addresses that have gone quiet are forgotten, so a spray across many
        // sources cannot grow the table without bound.
        let window = now.saturating_sub(LOGIN_WINDOW_SECS);
        self.attempts.retain(|_, times| {
            times.retain(|&t| t > window);
            !times.is_empty()
        });

        let times = self.attempts.entry(key.to_owned()).or_default();
        if times.len() >= LOGIN_ATTEMPTS {
            return false;
        }
        times.push(now);
        true
    }

    /// Signing in successfully clears the count: the person at the keyboard is
    /// not who the limit is for.
    pub fn forget(&mut self, key: &str) {
        self.attempts.remove(key);
    }
}

// ---------------------------------------------------------------------------
// The enrolment token
// ---------------------------------------------------------------------------

/// Permission to fetch the QR code, given out after a correct password and
/// before the authenticator exists. Held in memory, one at a time.
#[derive(Clone, Debug)]
pub struct SetupToken {
    token: String,
    expires: u64,
}

impl SetupToken {
    pub fn new(now: u64) -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| format!("evo could not get random bytes for the setup token: {e}"))?;
        Ok(Self {
            token: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            expires: now + SETUP_SECS,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn accepts(&self, presented: &str, now: u64) -> bool {
        now < self.expires && token_matches(&self.token, presented)
    }
}

// ---------------------------------------------------------------------------
// `evo serve init`
// ---------------------------------------------------------------------------

/// Set the password, mint the authenticator secret, and print what to scan.
pub fn init(paths: &ServePaths) -> Result<(), String> {
    if paths.auth.exists() {
        return Err(format!(
            "{} already exists. Delete it to start over -- doing so un-enrols your \
             authenticator app and signs out every device.",
            paths.auth.display()
        ));
    }

    let password = read_password()?;
    let store = AuthStore::create(&password)?;
    store.save(&paths.auth)?;

    // A new password means the old sessions are somebody else's.
    let _ = std::fs::remove_file(&paths.sessions);
    if !paths.config.exists() {
        super::ServeConfig::default().save(&paths.config)?;
    }

    let url = store.otpauth_url()?;
    println!(
        "evo serve is set up. Credentials are in {}.",
        paths.auth.display()
    );
    println!();
    println!("Add this to an authenticator app -- as a QR code, or by hand:");
    println!();
    println!("  {url}");
    println!();
    println!("  secret: {}", store.totp_secret);
    println!();
    println!(
        "Then start the server and sign in with the password and a code. The first code \
         evo accepts finishes enrolment; until it does, signing in with the password alone \
         offers the QR code instead."
    );
    Ok(())
}

/// The password comes from the environment when there is a script involved, and
/// from the keyboard otherwise.
fn read_password() -> Result<String, String> {
    if let Ok(password) = std::env::var("EVO_SERVE_PASSWORD") {
        let password = password.trim().to_owned();
        if password.is_empty() {
            return Err("EVO_SERVE_PASSWORD is set but empty.".to_owned());
        }
        return Ok(password);
    }

    use std::io::{BufRead, IsTerminal};
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        // evo has no terminal crate, so it cannot stop the terminal echoing.
        // Saying so is better than a surprise.
        println!("Choose a password for evo serve. It will be visible as you type;");
        println!("set EVO_SERVE_PASSWORD instead if that matters here.");
        print!("password: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("could not read the password: {e}"))?;
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        return Err("no password was given.".to_owned());
    }

    if interactive {
        print!("again: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut again = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut again)
            .map_err(|e| format!("could not read the password: {e}"))?;
        if again.trim_end_matches(['\r', '\n']) != password {
            return Err("those two passwords are not the same.".to_owned());
        }
    }
    Ok(password)
}

// ---------------------------------------------------------------------------
// Where the request came from
// ---------------------------------------------------------------------------

/// The address the rate limiter counts by.
pub struct ClientIp(pub String);

impl FromRequestParts<Shared> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Shared,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(client_ip(
            state.options.trust_proxy,
            &parts.headers,
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0),
        )))
    }
}

/// `X-Forwarded-For` is a header anyone can send, so it counts only when evo
/// has been told there is a proxy in front of it. Otherwise an attacker would
/// simply rotate the header to get unlimited password guesses.
fn client_ip(trust_proxy: bool, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if trust_proxy && let Some(forwarded) = headers.get("x-forwarded-for") {
        // The left-most entry is the original client; the rest are proxies.
        if let Some(first) = forwarded
            .to_str()
            .ok()
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return first.to_owned();
        }
    }
    peer.map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

fn session_cookie(options: &ServeOptions, token: &str) -> String {
    let mut cookie = format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_SECS}",
        options.cookie_name()
    );
    if options.secure_cookies {
        cookie.push_str("; Secure");
    }
    cookie
}

fn cleared_cookie(options: &ServeOptions) -> String {
    let mut cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        options.cookie_name()
    );
    if options.secure_cookies {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Pull one cookie out of the header. No cookie crate in the tree, and the
/// grammar is `name=value` pairs separated by `; `.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Which paths answer without a session.
///
/// The app shell -- HTML, CSS, the manifest, the service worker -- is public
/// because the browser has to be able to draw the sign-in form. It contains
/// nothing but the form. Everything under `/api/` needs a session except the
/// three endpoints that exist to get one.
pub fn is_public(path: &str) -> bool {
    match path.strip_prefix("/api/") {
        Some(endpoint) => matches!(endpoint, "health" | "login" | "setup-qr"),
        None => true,
    }
}

/// Every mutation must be one this app made.
///
/// `SameSite=Strict` already stops the cookie riding along on a cross-site
/// request, but it is one flag on one cookie: the required header is a second,
/// independent reason a form on someone else's page cannot act as you, because
/// a form cannot set a header at all. `Sec-Fetch-Site` is a third when the
/// browser sends it.
pub async fn csrf(req: Request, next: Next) -> Response {
    if req.method().is_safe() {
        return next.run(req).await;
    }

    if let Some(site) = req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        && site != "same-origin"
        && site != "none"
    {
        return refuse(
            StatusCode::FORBIDDEN,
            "That request came from another site, so evo did not act on it.",
        );
    }

    if req.headers().get(CSRF_HEADER).is_none() {
        return refuse(
            StatusCode::FORBIDDEN,
            "Requests that change something must carry the `X-Evo: 1` header.",
        );
    }
    next.run(req).await
}

/// The cookie, on everything that is not public.
pub async fn require_session(State(state): State<Shared>, req: Request, next: Next) -> Response {
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }
    let Some(token) = cookie_value(req.headers(), state.options.cookie_name()) else {
        return unauthorized();
    };

    // The lock is taken and dropped before the request runs: a std mutex held
    // across an await is a deadlock waiting for a busy afternoon.
    let outcome = {
        let mut sessions = state
            .sessions
            .lock()
            .expect("the session lock is never poisoned");
        let outcome = sessions.check(&token, now_secs());
        if outcome == SessionCheck::Refreshed
            && let Err(e) = sessions.save(&state.paths.sessions)
        {
            eprintln!("could not save the session table: {e}");
        }
        outcome
    };
    match outcome {
        SessionCheck::Missing => unauthorized(),
        SessionCheck::Valid | SessionCheck::Refreshed => next.run(req).await,
    }
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn unauthorized() -> Response {
    refuse(
        StatusCode::UNAUTHORIZED,
        "Sign in to evo first: POST your password and a code to /api/login.",
    )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub password: String,
    /// Absent on the first leg of enrolment, when there is no app to ask.
    #[serde(default)]
    pub code: Option<String>,
}

pub async fn login(
    State(state): State<Shared>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LoginBody>,
) -> Response {
    let now = now_secs();
    {
        let mut logins = state
            .logins
            .lock()
            .expect("the login lock is never poisoned");
        if !logins.allow(&ip, now) {
            return refuse(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many sign-in attempts from this address. Wait five minutes.",
            );
        }
    }

    let (stored, confirmed) = {
        let auth = state.auth.lock().expect("the auth lock is never poisoned");
        (auth.password_hash.clone(), auth.totp_confirmed)
    };

    // argon2id is deliberately slow -- that is what makes it worth using -- so
    // it does not run on a thread that is meant to be answering other requests.
    let password = body.password.clone();
    let correct = tokio::task::spawn_blocking(move || verify_password(&stored, &password))
        .await
        .unwrap_or(false);
    if !correct {
        // The same answer whether the password or the code was wrong: which one
        // failed is not something an attacker needs told.
        return refuse(StatusCode::UNAUTHORIZED, WRONG);
    }

    let code = body.code.as_deref().map(str::trim).unwrap_or("");

    if !confirmed && code.is_empty() {
        // Enrolment: the password is right but there is no authenticator yet,
        // so hand out a short-lived permission to fetch the QR code.
        let setup = match SetupToken::new(now) {
            Ok(setup) => setup,
            Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, &e),
        };
        let token = setup.token().to_owned();
        *state
            .setup
            .lock()
            .expect("the setup lock is never poisoned") = Some(setup);
        return Json(json!({ "enroll": true, "setup": token })).into_response();
    }

    if code.is_empty() {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "evo also needs the six-digit code from your authenticator app.",
        );
    }

    let checked = {
        let mut auth = state.auth.lock().expect("the auth lock is never poisoned");
        match auth.check_code(code, now) {
            Ok(CodeCheck::Accepted) => {
                if let Err(e) = auth.save(&state.paths.auth) {
                    // Not fatal for this request, but the replay counter has to
                    // survive a restart, so it is worth saying loudly.
                    eprintln!("could not save the credentials: {e}");
                }
                CodeCheck::Accepted
            }
            Ok(other) => other,
            Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, &e),
        }
    };
    match checked {
        CodeCheck::Accepted => {}
        CodeCheck::Replayed => {
            return refuse(
                StatusCode::UNAUTHORIZED,
                "That code has already been used. Wait for your app to show the next one.",
            );
        }
        CodeCheck::Wrong => return refuse(StatusCode::UNAUTHORIZED, WRONG),
    }

    let token = {
        let mut sessions = state
            .sessions
            .lock()
            .expect("the session lock is never poisoned");
        match sessions.create(now) {
            Ok(token) => {
                if let Err(e) = sessions.save(&state.paths.sessions) {
                    eprintln!("could not save the session table: {e}");
                }
                token
            }
            Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, &e),
        }
    };
    state
        .logins
        .lock()
        .expect("the login lock is never poisoned")
        .forget(&ip);
    *state
        .setup
        .lock()
        .expect("the setup lock is never poisoned") = None;

    (
        [(header::SET_COOKIE, session_cookie(&state.options, &token))],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

const WRONG: &str = "That password or code is not right.";

pub async fn logout(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, state.options.cookie_name()) {
        let mut sessions = state
            .sessions
            .lock()
            .expect("the session lock is never poisoned");
        if sessions.revoke(&token)
            && let Err(e) = sessions.save(&state.paths.sessions)
        {
            eprintln!("could not save the session table: {e}");
        }
    }
    (
        [(header::SET_COOKIE, cleared_cookie(&state.options))],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SetupQuery {
    /// The token `/api/login` handed out.
    pub t: String,
}

/// The enrolment QR code, for the window between "the password is right" and
/// "an authenticator app exists".
pub async fn setup_qr(State(state): State<Shared>, Query(query): Query<SetupQuery>) -> Response {
    let now = now_secs();
    let permitted = state
        .setup
        .lock()
        .expect("the setup lock is never poisoned")
        .as_ref()
        .is_some_and(|setup| setup.accepts(&query.t, now));
    if !permitted {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "That setup link has expired. Sign in with your password again.",
        );
    }

    let (png, confirmed) = {
        let auth = state.auth.lock().expect("the auth lock is never poisoned");
        (auth.qr_png(), auth.totp_confirmed)
    };
    if confirmed {
        // Enrolment is over; the secret is not on offer any more.
        return refuse(
            StatusCode::FORBIDDEN,
            "This server is already paired with an authenticator app.",
        );
    }
    match png {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            png,
        )
            .into_response(),
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway password, only ever used in this file.
    const PASSWORD: &str = "correct horse battery staple";

    fn fresh() -> AuthStore {
        AuthStore::create(PASSWORD).expect("credentials")
    }

    #[test]
    fn a_password_round_trips_and_nothing_else_verifies() {
        let store = fresh();
        assert!(
            store.password_hash.starts_with("$argon2id$"),
            "{}",
            store.password_hash
        );
        assert!(verify_password(&store.password_hash, PASSWORD));
        assert!(!verify_password(
            &store.password_hash,
            "correct horse battery stapl"
        ));
        assert!(!verify_password(&store.password_hash, ""));

        // Two of the same password must not produce the same hash: that is what
        // the salt is for.
        assert_ne!(store.password_hash, fresh().password_hash);
    }

    #[test]
    fn an_unparseable_hash_lets_nobody_in() {
        assert!(!verify_password("", PASSWORD));
        assert!(!verify_password("not a phc string", PASSWORD));
        // Not even the empty password against an empty hash.
        assert!(!verify_password("", ""));
    }

    #[test]
    fn a_short_password_is_refused_before_it_is_stored() {
        let message = AuthStore::create("short").expect_err("too short");
        assert!(message.contains("at least"), "{message}");
    }

    #[test]
    fn the_authenticator_secret_is_a_hundred_and_sixty_fresh_bits() {
        let a = new_totp_secret().expect("a secret");
        let b = new_totp_secret().expect("another");
        // 160 bits is 32 base32 characters.
        assert_eq!(a.len(), 32, "{a}");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c)),
            "{a}"
        );
        assert_ne!(a, b, "two secrets in a row must not match");
    }

    /// RFC 6238's own test vector, at a time this test picks, so it says
    /// something about the algorithm rather than about the clock.
    #[test]
    fn the_codes_are_the_ones_rfc_6238_publishes() {
        let secret = Secret::from(b"12345678901234567890".to_vec());
        let totp = Builder::new()
            .with_algorithm(Algorithm::SHA1)
            .with_digits(8)
            .with_skew(0)
            .with_step_duration(30)
            .with_secret(secret)
            .with_issuer(Some(ISSUER))
            .with_account_name(ACCOUNT)
            .build()
            .expect("the RFC's authenticator");

        assert_eq!(totp.generate(59).to_string(), "94287082");
        assert_eq!(totp.generate(1_111_111_109).to_string(), "07081804");
        assert_eq!(totp.check("94287082", 59), Some(1), "the matched step");
        assert_eq!(
            totp.check("07081804", 59),
            None,
            "a code from another minute"
        );
    }

    /// Fixed time throughout: a clock-dependent test is a test that fails at
    /// 23:59:59.
    #[test]
    fn a_code_is_accepted_one_step_either_side_and_no_further() {
        let store = fresh();
        let totp = store.totp().expect("an authenticator");
        let now = 1_700_000_000_u64;

        assert!(totp.check(&totp.generate(now).to_string(), now).is_some());
        assert!(
            totp.check(&totp.generate(now - STEP).to_string(), now)
                .is_some(),
            "a phone thirty seconds behind still works"
        );
        assert!(
            totp.check(&totp.generate(now + STEP).to_string(), now)
                .is_some()
        );
        assert!(
            totp.check(&totp.generate(now - 2 * STEP).to_string(), now)
                .is_none(),
            "two steps is further than RFC 6238 asks for"
        );
        assert!(
            totp.check(&totp.generate(now + 2 * STEP).to_string(), now)
                .is_none()
        );
        assert!(totp.check("000000", now).is_none() || totp.generate(now).to_string() == "000000");
    }

    /// A code copied off the wire is worthless a moment later.
    #[test]
    fn a_code_only_works_once() {
        let mut store = fresh();
        let now = 1_700_000_000_u64;
        let code = store
            .totp()
            .expect("an authenticator")
            .generate(now)
            .to_string();

        assert!(!store.totp_confirmed, "enrolment is not finished yet");
        assert_eq!(store.check_code(&code, now), Ok(CodeCheck::Accepted));
        assert!(
            store.totp_confirmed,
            "the first accepted code finishes enrolment"
        );
        assert_eq!(store.check_code(&code, now), Ok(CodeCheck::Replayed));
        // Even a second later, inside the same step.
        assert_eq!(store.check_code(&code, now + 5), Ok(CodeCheck::Replayed));
        assert_eq!(store.check_code("123456", now), Ok(CodeCheck::Wrong));

        // The next step is a different code, and works.
        let next = store
            .totp()
            .expect("an authenticator")
            .generate(now + STEP)
            .to_string();
        assert_eq!(store.check_code(&next, now + STEP), Ok(CodeCheck::Accepted));
    }

    #[test]
    fn the_enrolment_url_is_what_an_authenticator_app_expects() {
        let store = fresh();
        let url = store.otpauth_url().expect("a URL");
        assert!(url.starts_with("otpauth://totp/evo:evo%20serve?"), "{url}");
        assert!(
            url.contains(&format!("secret={}", store.totp_secret)),
            "{url}"
        );
        assert!(url.contains("issuer=evo"), "{url}");

        let png = store.qr_png().expect("a QR code");
        assert_eq!(&png[1..4], b"PNG", "the QR code is a PNG");
    }

    #[test]
    fn credentials_are_written_where_only_their_owner_can_read_them() {
        let dir = std::env::temp_dir().join(format!("evo-serve-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = dir.join("auth.json");

        let store = fresh();
        store.save(&path).expect("saving");
        let loaded = AuthStore::load(&path).expect("loading");
        assert_eq!(loaded.password_hash, store.password_hash);
        assert_eq!(loaded.totp_secret, store.totp_secret);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }

        let missing = AuthStore::load(&dir.join("nothing.json")).expect_err("no credentials");
        assert!(missing.contains("evo serve init"), "{missing}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_expires_thirty_days_out_and_slides_while_it_is_used() {
        let mut sessions = Sessions::default();
        let now = 1_700_000_000_u64;
        let token = sessions.create(now).expect("a session");
        assert_eq!(token.len(), 64, "32 bytes of hex");

        assert_eq!(sessions.check(&token, now), SessionCheck::Valid);
        assert_eq!(
            sessions.check(&token, now + 60),
            SessionCheck::Valid,
            "using it constantly must not rewrite the file constantly"
        );

        // A day later it is worth pushing the expiry out.
        let later = now + 2 * 24 * 60 * 60;
        assert_eq!(sessions.check(&token, later), SessionCheck::Refreshed);
        assert_eq!(
            sessions.check(&token, later + SESSION_SECS - 60),
            SessionCheck::Refreshed,
            "the slide moved the deadline with it"
        );

        // Left alone for a month, it is gone.
        let mut stale = Sessions::default();
        let token = stale.create(now).expect("a session");
        assert_eq!(
            stale.check(&token, now + SESSION_SECS + 1),
            SessionCheck::Missing
        );
        assert!(
            stale.entries.is_empty(),
            "expired sessions are dropped, not kept"
        );
    }

    #[test]
    fn only_the_token_that_was_issued_opens_a_session() {
        let mut sessions = Sessions::default();
        let now = 1_700_000_000_u64;
        let token = sessions.create(now).expect("a session");

        assert_eq!(sessions.check("", now), SessionCheck::Missing);
        assert_eq!(sessions.check(&token[..63], now), SessionCheck::Missing);
        assert_eq!(
            sessions.check(&hash_token(&token), now),
            SessionCheck::Missing
        );

        // The stored form is a hash, so the file does not hand out sessions.
        let stored = serde_json::to_string(&sessions).expect("json");
        assert!(
            !stored.contains(&token),
            "the token itself is never written down"
        );
        assert!(stored.contains(&hash_token(&token)));

        assert!(sessions.revoke(&token), "signing out removes it");
        assert_eq!(sessions.check(&token, now), SessionCheck::Missing);
        assert!(
            !sessions.revoke(&token),
            "and there is nothing left to remove"
        );
    }

    #[test]
    fn five_attempts_in_five_minutes_and_then_a_wait() {
        let mut limiter = RateLimiter::default();
        let now = 1_700_000_000_u64;

        for attempt in 0..LOGIN_ATTEMPTS {
            assert!(limiter.allow("10.0.0.1", now), "attempt {attempt}");
        }
        assert!(!limiter.allow("10.0.0.1", now), "the sixth is refused");
        assert!(
            !limiter.allow("10.0.0.1", now + LOGIN_WINDOW_SECS - 1),
            "still inside the window"
        );
        assert!(
            limiter.allow("10.0.0.1", now + LOGIN_WINDOW_SECS + 1),
            "the window has passed"
        );

        // The limit is per address, and signing in clears it.
        assert!(limiter.allow("10.0.0.2", now));
        limiter.forget("10.0.0.2");
        assert!(!limiter.attempts.contains_key("10.0.0.2"));
    }

    #[test]
    fn a_forwarded_address_counts_only_behind_a_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.5".parse().unwrap());
        let peer: SocketAddr = "10.0.0.5:51000".parse().unwrap();

        assert_eq!(client_ip(true, &headers, Some(peer)), "203.0.113.9");
        assert_eq!(
            client_ip(false, &headers, Some(peer)),
            "10.0.0.5",
            "a header anyone can send is not an identity"
        );
        assert_eq!(client_ip(true, &HeaderMap::new(), Some(peer)), "10.0.0.5");
        assert_eq!(client_ip(true, &headers, None), "203.0.113.9");
        assert_eq!(client_ip(false, &HeaderMap::new(), None), "unknown");
    }

    #[test]
    fn the_setup_token_is_short_lived_and_only_itself() {
        let now = 1_700_000_000_u64;
        let setup = SetupToken::new(now).expect("a token");
        assert!(setup.accepts(setup.token(), now));
        assert!(setup.accepts(setup.token(), now + SETUP_SECS - 1));
        assert!(!setup.accepts(setup.token(), now + SETUP_SECS));
        assert!(!setup.accepts("", now));
        assert!(!setup.accepts(&setup.token()[..31], now));
    }

    #[test]
    fn the_cookie_says_host_only_secure_strict_unless_tls_was_waived() {
        let secure = ServeOptions::default();
        let cookie = session_cookie(&secure, "abc");
        assert!(cookie.starts_with("__Host-evo=abc;"), "{cookie}");
        assert!(cookie.contains("; Secure"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");

        let plain = ServeOptions {
            secure_cookies: false,
            trust_proxy: false,
        };
        let cookie = session_cookie(&plain, "abc");
        assert!(cookie.starts_with("evo=abc;"), "{cookie}");
        assert!(
            !cookie.contains("Secure"),
            "__Host- would be refused: {cookie}"
        );
        assert!(cleared_cookie(&plain).contains("Max-Age=0"));
    }

    #[test]
    fn one_cookie_is_found_among_several() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "theme=dark; __Host-evo=abc123; other=1".parse().unwrap(),
        );
        assert_eq!(
            cookie_value(&headers, "__Host-evo").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            cookie_value(&headers, "evo"),
            None,
            "a prefix is a different cookie"
        );
        assert_eq!(cookie_value(&HeaderMap::new(), "__Host-evo"), None);
    }

    /// The list of exceptions is the security boundary, so it gets a test of its
    /// own rather than being read off the router.
    #[test]
    fn only_signing_in_and_the_shell_are_reachable_without_a_session() {
        for open in ["/api/health", "/api/login", "/api/setup-qr"] {
            assert!(is_public(open), "{open}");
        }
        for shell in [
            "/",
            "/index.html",
            "/style.css",
            "/manifest.webmanifest",
            "/sw.js",
        ] {
            assert!(is_public(shell), "{shell}");
        }
        for guarded in [
            "/api/logout",
            "/api/docs",
            "/api/docs/abc/page/1.png",
            "/api/status",
            "/api/anything-added-later",
        ] {
            assert!(!is_public(guarded), "{guarded}");
        }
    }
}
