//! `evo serve`: the library over HTTP, for a phone.
//!
//! A different program wearing the same binary, like `evo mcp-serve`. There is
//! no window, no eframe storage and nobody to click a dialog, so everything the
//! desktop app keeps in its preferences lives here in a JSON file next to the
//! library, and everything it would ask about is a command-line flag.
//!
//! Two rules shape the whole module. redb permits one process per database, so
//! `evo serve` and the desktop app cannot hold the same library at once and the
//! refusal has to say so rather than print a lock error. And tantivy permits
//! one writer, so serve starts the ordinary [`Library::start_indexer`] worker
//! and never opens an index of its own.

pub mod auth;
pub mod chat_api;
pub mod library_api;
pub mod markup_api;
pub mod pages;
pub mod routes;
pub mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::library::Library;
use crate::library::enrich::AssistantPrefs;
use crate::mcp::client::{ClientEntry, McpClients};
use crate::script::model::{ModelBackend, ModelConfig};

/// Where the server listens unless told otherwise. Not registered with anyone;
/// 8443 says "HTTPS-ish, not privileged" to whoever reads the systemd unit.
pub const DEFAULT_PORT: u16 = 8443;

/// The default interface. `evo serve` exists to be reached from a phone, so
/// unlike the in-app MCP server it is not loopback-only -- which is exactly why
/// nothing answers it without a password and a code.
pub const DEFAULT_BIND: &str = "0.0.0.0";

/// Seconds since the Unix epoch. Sessions and rate limits are both wall-clock
/// facts -- they have to survive a restart -- so this is the clock they use.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the blobs live. An enum with one arm today because M28 adds S3 and the
/// config file it writes should not have to change shape when it does.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlobBackend {
    /// Files under `<library>/docs`, the same as the desktop app.
    #[default]
    Local,
}

/// The biggest upload accepted, in megabytes. A scanned book is tens of
/// megabytes; anything past this is a mistake or an attempt to fill the disk.
pub const DEFAULT_MAX_UPLOAD_MB: usize = 200;

fn default_max_upload_mb() -> usize {
    DEFAULT_MAX_UPLOAD_MB
}

/// What the desktop app keeps in eframe storage, for a process that has none.
///
/// Every field is optional on the way in: a config file someone wrote by hand
/// should not have to name settings they do not care about.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServeConfig {
    /// Which model answers chat and writes summaries (M26 onwards).
    pub model: ModelConfig,
    /// Whether summaries and tags are written at all.
    pub assistant: AssistantPrefs,
    /// MCP servers the agent may use. Deliberately config-file-only: it names
    /// programs to run, which is not something an HTTP API should accept.
    pub mcp_clients: Vec<ClientEntry>,
    pub blobs: BlobBackend,
    /// The upload size limit. Configurable because "how big is a document"
    /// is a question about the library, not about evo.
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: usize,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            model: ModelConfig::default(),
            assistant: AssistantPrefs::default(),
            mcp_clients: Vec::new(),
            blobs: BlobBackend::default(),
            max_upload_mb: DEFAULT_MAX_UPLOAD_MB,
        }
    }
}

impl ServeConfig {
    /// The upload limit in bytes, never zero: a limit of nothing would make
    /// the server refuse every document, which is not a configuration anyone
    /// means to write.
    pub fn upload_limit(&self) -> usize {
        self.max_upload_mb.max(1) * 1024 * 1024
    }
}

impl ServeConfig {
    /// Read the config, or return the defaults if the file is not there yet.
    /// A file that exists but does not parse is an error: silently ignoring a
    /// typo would point the server at the wrong model without saying so.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                format!(
                    "{} is not valid evo serve configuration: {e}",
                    path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("could not read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("could not write the configuration: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
    }
}

/// Everything the server reads or writes, worked out once at startup.
///
/// `--data-dir` moves the library and the server's own files. It does *not*
/// move the downloaded models: those live in the platform data directory,
/// shared with the desktop app, because a model is a big file and nobody wants
/// two copies.
#[derive(Clone, Debug)]
pub struct ServePaths {
    pub library_root: PathBuf,
    pub serve_dir: PathBuf,
    pub config: PathBuf,
    pub auth: PathBuf,
    pub sessions: PathBuf,
}

impl ServePaths {
    pub fn new(data_dir: Option<PathBuf>, config: Option<PathBuf>) -> Result<Self, String> {
        let data_dir = match data_dir {
            Some(dir) => dir,
            None => directories::ProjectDirs::from("", "", "evo")
                .map(|d| d.data_dir().to_path_buf())
                .ok_or_else(|| {
                    "evo could not find a data directory on this platform; pass --data-dir."
                        .to_owned()
                })?,
        };
        let serve_dir = data_dir.join("serve");
        Ok(Self {
            // The same place `Library::open_default` looks, so `evo serve` with
            // no flags serves the library the desktop app already has.
            library_root: data_dir.join("library"),
            config: config.unwrap_or_else(|| serve_dir.join("config.json")),
            auth: serve_dir.join("auth.json"),
            sessions: serve_dir.join("sessions.json"),
            serve_dir,
        })
    }

    fn ensure_dir(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.serve_dir)
            .map_err(|e| format!("could not create {}: {e}", self.serve_dir.display()))
    }
}

/// The flags, and the decisions that follow from them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServeArgs {
    /// `evo serve init`: set up the password and the authenticator, then stop.
    pub init: bool,
    pub port: u16,
    pub bind: String,
    pub data_dir: Option<PathBuf>,
    pub config: Option<PathBuf>,
    /// Serve plain HTTP. Only sane behind a reverse proxy that terminates TLS,
    /// or on a machine you are sitting at.
    pub insecure_http: bool,
    /// Believe `X-Forwarded-For`. Off by default: a header anyone can send is
    /// not an identity, and the rate limiter counts by it.
    pub trust_proxy: bool,
}

impl Default for ServeArgs {
    fn default() -> Self {
        Self {
            init: false,
            port: DEFAULT_PORT,
            bind: DEFAULT_BIND.to_owned(),
            data_dir: None,
            config: None,
            insecure_http: false,
            trust_proxy: false,
        }
    }
}

pub const USAGE: &str = "\
usage: evo serve [init] [options]

  init                 set the password and enrol an authenticator app, then exit
  --port <n>           port to listen on (default 8443)
  --bind <addr>        interface to listen on (default 0.0.0.0)
  --data-dir <path>    where the library and the server's files live
  --config <path>      configuration file (default <data-dir>/serve/config.json)
  --insecure-http      serve plain HTTP; drops the Secure flag from the cookie
  --trust-proxy        take the client address from X-Forwarded-For
";

/// Parse the arguments after `evo serve`.
///
/// Hand-rolled because evo has no argument-parsing crate and this is seven
/// flags. Both `--flag value` and `--flag=value` work, because people type
/// both.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<ServeArgs, String> {
    let mut parsed = ServeArgs::default();
    let mut args = args.into_iter().peekable();
    let mut first = true;

    while let Some(arg) = args.next() {
        if first && arg == "init" {
            parsed.init = true;
            first = false;
            continue;
        }
        first = false;

        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone().or_else(|| args.next()) {
                Some(value) => Ok(value),
                None => Err(format!("{name} needs a value.\n\n{USAGE}")),
            }
        };

        match name.as_str() {
            "--port" => {
                let raw = value("--port")?;
                parsed.port = raw
                    .parse()
                    .map_err(|_| format!("{raw} is not a port number.\n\n{USAGE}"))?;
            }
            "--bind" => parsed.bind = value("--bind")?,
            "--data-dir" => parsed.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--config" => parsed.config = Some(PathBuf::from(value("--config")?)),
            "--insecure-http" => parsed.insecure_http = true,
            "--trust-proxy" => parsed.trust_proxy = true,
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("evo serve does not know {other}.\n\n{USAGE}")),
        }
    }
    Ok(parsed)
}

/// The decisions the request handlers need, separated from the flags because
/// "is the cookie Secure" is a policy and `--insecure-http` is only how it was
/// asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ServeOptions {
    pub secure_cookies: bool,
    pub trust_proxy: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            secure_cookies: true,
            trust_proxy: false,
        }
    }
}

impl ServeOptions {
    /// The session cookie's name.
    ///
    /// `__Host-` is the prefix that tells the browser to refuse the cookie
    /// unless it is Secure, path `/` and un-scoped to a domain -- which is
    /// exactly the guarantee wanted, and exactly why it cannot be used when
    /// `--insecure-http` has dropped Secure. Under plain HTTP the name is the
    /// unadorned one, and the weaker cookie is the honest one.
    pub fn cookie_name(&self) -> &'static str {
        if self.secure_cookies {
            "__Host-evo"
        } else {
            "evo"
        }
    }
}

/// Where a generation's model comes from.
///
/// A function rather than a direct call to [`ModelConfig::build`] so that the
/// endpoint the tests exercise is the shipped one -- routing, framing, tools
/// and all -- with a scripted model behind it instead of a download. The server
/// itself only ever installs [`default_backend`].
pub type Backends = Arc<dyn Fn(&ModelConfig) -> Box<dyn ModelBackend> + Send + Sync>;

/// The real one: whatever the configuration says.
pub fn default_backend() -> Backends {
    Arc::new(|config: &ModelConfig| config.build())
}

/// Everything a handler can reach. One `Arc` of it is the axum router's state.
pub struct ServeState {
    pub library: Arc<Mutex<Library>>,
    pub config: ServeConfig,
    pub paths: ServePaths,
    pub options: ServeOptions,
    pub auth: Mutex<auth::AuthStore>,
    pub sessions: Mutex<auth::Sessions>,
    pub logins: Mutex<auth::RateLimiter>,
    /// The short-lived permission to fetch the enrolment QR code, held in
    /// memory only: it is worth less than a session and outlives nothing.
    pub setup: Mutex<Option<auth::SetupToken>>,
    /// Page dimensions per document, worked out by parsing the PDF once.
    ///
    /// The viewer asks for them on every page turn and they cannot change --
    /// the blob is content-addressed, so the same id is the same bytes for
    /// ever. Small enough (two floats a page) that nothing is evicted.
    pub page_sizes: Mutex<HashMap<String, Arc<Vec<library_api::PageSize>>>>,
    /// The text of the documents most recently asked questions about. Unlike
    /// the page sizes this is megabytes rather than bytes, so it has a lid.
    pub pages_text: Mutex<chat_api::PageText>,
    /// One model generation at a time. A language model is the largest thing
    /// this process does and the least willing to share: a queue of one is
    /// slower for the second reader and survivable for the machine.
    pub generation: tokio::sync::Semaphore,
    /// The MCP servers the configuration named, if any -- the same client the
    /// desktop app uses, with the child processes kept between questions so
    /// only the first one pays to start them.
    pub mcp: Arc<McpClients>,
    /// How to get a model. Always [`default_backend`] outside the tests.
    pub backend: Backends,
}

pub type Shared = Arc<ServeState>;

/// Turn a failure to open the library into something worth reading. The
/// overwhelmingly likely cause is that the desktop app has the database open,
/// and the answer to that is not "try again" but "quit the app".
pub fn explain(error: crate::library::LibraryError) -> String {
    let text = error.to_string();
    if text.contains("already open") {
        LOCKED.to_owned()
    } else {
        format!("evo could not open its library: {text}")
    }
}

/// What to tell someone whose library is locked by the running app.
pub const LOCKED: &str = "evo is already running and has this library open; quit the app before \
                          starting `evo serve` (one library, one process).";

/// The `evo serve` entry point. Never returns: the process exists to be this
/// server.
pub fn main() -> ! {
    // Skip the binary and the `serve` word itself.
    let args: Vec<String> = std::env::args().skip(2).collect();
    match run(args) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let args = parse_args(args)?;
    let paths = ServePaths::new(args.data_dir.clone(), args.config.clone())?;
    paths.ensure_dir()?;

    if args.init {
        return auth::init(&paths);
    }

    let auth = auth::AuthStore::load(&paths.auth)?;
    let config = ServeConfig::load(&paths.config)?;
    let sessions = auth::Sessions::load(&paths.sessions)?;

    let mut library = Library::open_at(paths.library_root.clone()).map_err(explain)?;
    // The indexer wants somewhere to ask for a repaint. There is no window, so
    // it gets a detached context: the same answer `mcp::client` gives.
    library.start_indexer(&egui::Context::default());

    // The servers the agent may reach. Configured here and never from a
    // request: an entry names a program to run, which is not something an HTTP
    // API has any business accepting. Nothing is started until a question with
    // tools switched on asks what they can do.
    let mcp = Arc::new(McpClients::default());
    mcp.configure(&config.mcp_clients);

    let options = ServeOptions {
        secure_cookies: !args.insecure_http,
        trust_proxy: args.trust_proxy,
    };
    let state = Arc::new(ServeState {
        library: Arc::new(Mutex::new(library)),
        config,
        paths,
        options,
        auth: Mutex::new(auth),
        sessions: Mutex::new(sessions),
        logins: Mutex::new(auth::RateLimiter::default()),
        setup: Mutex::new(None),
        page_sizes: Mutex::new(HashMap::new()),
        pages_text: Mutex::new(chat_api::PageText::default()),
        generation: tokio::sync::Semaphore::new(1),
        mcp,
        backend: default_backend(),
    });

    // Four workers: enough that a slow render or a model turn does not stall
    // the rest, few enough to be inconspicuous on a small server.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the server: {e}"))?;

    runtime.block_on(serve(state, &args.bind, args.port))
}

async fn serve(state: Shared, bind: &str, port: u16) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind((bind, port))
        .await
        .map_err(|e| {
            format!(
                "could not listen on {bind}:{port}: {e}. Another program may already be using \
                 that port; try --port."
            )
        })?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("could not read the listening address: {e}"))?;
    let scheme = if state.options.secure_cookies {
        "https"
    } else {
        "http"
    };
    // What the operator wants to see in the journal: the address, how big the
    // library is, and which model the config file points at -- the two things
    // that are silently wrong most often.
    let documents = state
        .library
        .lock()
        .expect("the library lock is never poisoned")
        .list()
        .map(|docs| docs.len())
        .unwrap_or(0);
    println!("evo serve is listening on {scheme}://{addr} ({documents} documents)");
    let model = &state.config.model;
    if model.api.is_http() {
        println!("  model: {} at {}", model.model, model.base_url);
    } else {
        println!("  model: built-in {}", model.builtin_model);
    }
    let servers = state.config.mcp_clients.len();
    if servers > 0 {
        println!(
            "  tools: {servers} MCP server{} configured (started when first asked for)",
            if servers == 1 { "" } else { "s" }
        );
    }
    if !state.options.secure_cookies {
        println!(
            "  --insecure-http: the session cookie is not marked Secure. Put a TLS proxy in \
             front of this before it leaves your network."
        );
    }

    let cancel = CancellationToken::new();
    tokio::spawn(watch_for_shutdown(cancel.clone()));

    let app = routes::router(state);
    axum::serve(
        listener,
        // The rate limiter counts logins per client address, so the handlers
        // have to be able to see one.
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move { cancel.cancelled_owned().await })
    .await
    .map_err(|e| format!("the server stopped: {e}"))
}

/// Ctrl-C, or systemd asking the unit to stop. Either way, finish the requests
/// in flight and close the port.
async fn watch_for_shutdown(cancel: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                cancel.cancel();
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    println!("evo serve is stopping.");
    cancel.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_a_public_port_and_a_secure_cookie() {
        let args = parse_args(Vec::new()).expect("no flags is valid");
        assert_eq!(args, ServeArgs::default());
        assert_eq!(args.port, DEFAULT_PORT);
        assert_eq!(args.bind, "0.0.0.0");
        assert!(!args.insecure_http, "TLS is assumed until it is waived");
        assert_eq!(ServeOptions::default().cookie_name(), "__Host-evo");
    }

    #[test]
    fn flags_are_read_both_ways_round() {
        let spaced = parse_args(
            [
                "--port",
                "9000",
                "--bind",
                "127.0.0.1",
                "--data-dir",
                "/srv/evo",
            ]
            .map(str::to_owned),
        )
        .expect("spaced flags");
        let joined = parse_args(
            ["--port=9000", "--bind=127.0.0.1", "--data-dir=/srv/evo"].map(str::to_owned),
        )
        .expect("joined flags");
        assert_eq!(spaced, joined);
        assert_eq!(spaced.port, 9000);
        assert_eq!(spaced.data_dir, Some(PathBuf::from("/srv/evo")));
    }

    #[test]
    fn init_is_a_subcommand_and_only_in_first_place() {
        let init = parse_args(["init", "--insecure-http"].map(str::to_owned)).expect("init");
        assert!(init.init);
        assert!(init.insecure_http);

        let misplaced = parse_args(["--insecure-http", "init"].map(str::to_owned));
        assert!(misplaced.is_err(), "`init` is a subcommand, not a flag");
    }

    #[test]
    fn an_unknown_flag_is_refused_with_the_usage() {
        let message = parse_args(["--admin".to_owned()]).expect_err("unknown flags are refused");
        assert!(message.contains("--admin"), "{message}");
        assert!(message.contains("--insecure-http"), "{message}");

        let missing = parse_args(["--port".to_owned()]).expect_err("a flag without its value");
        assert!(missing.contains("needs a value"), "{missing}");
    }

    /// `--insecure-http` cannot keep the `__Host-` prefix: browsers refuse that
    /// prefix on a cookie that is not Secure, so keeping the name would mean
    /// setting a cookie nobody stores.
    #[test]
    fn plain_http_gives_up_the_host_prefix_with_the_secure_flag() {
        let insecure = ServeOptions {
            secure_cookies: false,
            trust_proxy: false,
        };
        assert_eq!(insecure.cookie_name(), "evo");
        assert!(!insecure.cookie_name().starts_with("__Host-"));
    }

    #[test]
    fn the_data_directory_holds_the_library_and_the_servers_own_files() {
        let paths = ServePaths::new(Some(PathBuf::from("/srv/evo")), None).expect("paths");
        assert_eq!(paths.library_root, PathBuf::from("/srv/evo/library"));
        assert_eq!(paths.auth, PathBuf::from("/srv/evo/serve/auth.json"));
        assert_eq!(paths.config, PathBuf::from("/srv/evo/serve/config.json"));

        let elsewhere = ServePaths::new(
            Some(PathBuf::from("/srv/evo")),
            Some(PathBuf::from("/etc/evo.json")),
        )
        .expect("paths");
        assert_eq!(elsewhere.config, PathBuf::from("/etc/evo.json"));
        assert_eq!(elsewhere.auth, paths.auth, "--config moves only the config");
    }

    /// A missing config is the ordinary case -- nobody has to write one -- but
    /// a broken one is worth stopping for.
    #[test]
    fn a_missing_configuration_is_the_defaults_and_a_broken_one_is_an_error() {
        let dir = std::env::temp_dir().join(format!("evo-serve-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir");

        let path = dir.join("config.json");
        let fresh = ServeConfig::load(&path).expect("a missing file is the defaults");
        assert_eq!(fresh.blobs, BlobBackend::Local);
        assert!(fresh.mcp_clients.is_empty());

        fresh.save(&path).expect("writing it back");
        let round_tripped = ServeConfig::load(&path).expect("what was just written");
        assert_eq!(round_tripped.model, fresh.model);

        std::fs::write(&path, "{ not json").expect("a broken file");
        let message = ServeConfig::load(&path).expect_err("a typo is not a default");
        assert!(
            message.contains("not valid evo serve configuration"),
            "{message}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The expected failure, and the one worth explaining: the desktop app has
    /// the database.
    #[test]
    fn a_locked_library_says_to_quit_the_app() {
        let locked = crate::library::LibraryError::Db(
            "Database already open. Cannot acquire lock.".to_owned(),
        );
        assert_eq!(explain(locked), LOCKED);

        let other = crate::library::LibraryError::Db("disk on fire".to_owned());
        let message = explain(other);
        assert!(message.contains("disk on fire"), "{message}");
        assert!(
            !message.contains("already running"),
            "a different failure must not be blamed on the app: {message}"
        );
    }
}
