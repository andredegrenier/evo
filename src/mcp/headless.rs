//! `evo mcp-serve`: the library tools over stdio, without a window.
//!
//! This is for clients that would rather start their own process than be
//! pointed at a running app. It offers the three tools that only need the
//! library -- there is no open document to mark up and nobody watching a
//! window, so the editing tools would be answering questions nobody asked.
//!
//! Only one process may hold the library's redb file, so this is the *other*
//! half of a pair: if evo is running, say so and point at the HTTP server
//! rather than failing with a lock error.

use std::sync::{Arc, Mutex};

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};

use crate::library::Library;

use super::library_tools;
use super::server::{SearchArgs, TextArgs, outcome};

/// What to tell someone whose library is locked by the running app.
pub const LOCKED: &str = "evo is already running; connect to its HTTP MCP server instead \
                          (Preferences ▸ MCP).";

const INSTRUCTIONS: &str = "evo's document library, read-only: list the documents, search \
    their full text, and read their pages. This is the headless server; to open documents \
    and mark them up, connect to a running evo instead (Preferences ▸ MCP).";

/// The library tools, served straight from the library on this thread.
///
/// The mutex is not for contention -- nothing here mutates -- but so one handler
/// can be shared across the runtime's workers.
#[derive(Clone)]
pub struct EvoLibraryMcp {
    library: Arc<Mutex<Library>>,
    tool_router: ToolRouter<Self>,
}

impl EvoLibraryMcp {
    pub fn new(library: Library) -> Self {
        Self {
            library: Arc::new(Mutex::new(library)),
            tool_router: Self::tool_router(),
        }
    }

    fn with_library<T>(&self, f: impl FnOnce(&Library) -> T) -> T {
        let library = self
            .library
            .lock()
            .expect("the library lock is never poisoned");
        f(&library)
    }
}

#[tool_router]
impl EvoLibraryMcp {
    /// List the documents in evo's library: id, title, page count, tags and
    /// (when evo has written one) a summary. The id is what the other library
    /// tools take.
    #[tool]
    fn list_library(&self) -> Result<CallToolResult, ErrorData> {
        outcome(self.with_library(library_tools::list_library))
    }

    /// Full-text search across every indexed page of the library. Returns
    /// matching documents with the page number and a snippet of the matching
    /// text.
    #[tool]
    fn search_library(
        &self,
        Parameters(SearchArgs { query, limit }): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.with_library(|lib| {
                library_tools::search_library(lib, &query, limit.unwrap_or(10))
            }),
        )
    }

    /// Read the text of a document's pages, by 1-based page number. The text is
    /// what evo indexed, which for scanned pages is what OCR recovered.
    #[tool]
    fn get_document_text(
        &self,
        Parameters(TextArgs {
            doc_id,
            first_page,
            last_page,
        }): Parameters<TextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.with_library(|lib| {
                library_tools::document_text(lib, &doc_id, first_page, last_page)
            }),
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EvoLibraryMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "evo-library",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Turn a failure to open the library into something worth reading. The
/// overwhelmingly likely cause is that the app has the database open.
fn explain(error: crate::library::LibraryError) -> String {
    let text = error.to_string();
    if text.contains("already open") {
        LOCKED.to_owned()
    } else {
        format!("evo could not open its library: {text}")
    }
}

/// Serve the library over stdio until the client goes away.
pub fn serve_stdio() -> Result<(), String> {
    let library = Library::open_default().map_err(explain)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the MCP server: {e}"))?;
    runtime.block_on(async {
        let service = EvoLibraryMcp::new(library)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| format!("could not start the MCP server: {e}"))?;
        service
            .waiting()
            .await
            .map_err(|e| format!("the MCP server stopped: {e}"))?;
        Ok(())
    })
}

/// The `evo mcp-serve` entry point. Never returns: the process exists to be
/// this server.
pub fn main() -> ! {
    match serve_stdio() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own library: redb lets exactly one process hold a
    /// database, and the test binary runs them in parallel.
    fn handler(name: &str) -> EvoLibraryMcp {
        let dir =
            std::env::temp_dir().join(format!("evo-mcp-headless-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        EvoLibraryMcp::new(Library::open_at(dir).expect("a library"))
    }

    /// The headless server is deliberately smaller than the in-app one: it
    /// offers only what a library alone can answer.
    #[test]
    fn only_the_library_tools_are_offered() {
        let handler = handler("tools");
        let tools = handler.tool_router.list_all();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["get_document_text", "list_library", "search_library"]
        );
    }

    #[test]
    fn it_says_it_is_the_headless_one_and_where_the_other_is() {
        let info = handler("info").get_info();
        assert_eq!(info.server_info.name, "evo-library");
        let instructions = info.instructions.unwrap_or_default();
        assert!(instructions.contains("Preferences"), "{instructions}");
    }

    /// A locked database is the expected failure, and the one worth explaining:
    /// the answer is not "try again" but "you already have a server".
    #[test]
    fn a_locked_library_says_to_use_the_running_app() {
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

    /// The tools answer from a real library, without a bridge or a UI thread.
    #[test]
    fn the_tools_read_the_library_directly() {
        let handler = handler("read");
        let result = handler.list_library().expect("a result");
        assert_ne!(result.is_error, Some(true));
        let text = serde_json::to_string(&result.content).expect("content");
        assert!(text.contains("\\\"count\\\":0"), "{text}");
    }
}
