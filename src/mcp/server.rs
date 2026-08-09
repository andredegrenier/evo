//! The tools the running app offers, and their MCP wrapping.
//!
//! Every one of them is a line and a half: describe the arguments, post the
//! command, hand back the JSON. The work happens on the UI thread, in
//! `EvoApp::handle_mcp_command`, where the library and the open document
//! actually live.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::bridge::{AppBridge, AppCommand, MarkupReq};

/// What an MCP client is told evo is, and what it can do here.
pub const INSTRUCTIONS: &str = "evo is a PDF editor with a local document library. Search \
    and read the library, open a document in the editor, mark it up, and export it. \
    Markup goes onto the document the user has open and is undoable, so say what you \
    are about to draw before you draw it.";

/// Turn a tool's own failure into a result the model reads, rather than a
/// protocol error the client renders as "something went wrong".
pub(super) fn outcome(result: Result<Value, String>) -> Result<CallToolResult, ErrorData> {
    Ok(match result {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
        Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// What to search for. Ordinary words; the index is full-text.
    pub query: String,
    /// How many matches to return (1-50, default 10).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TextArgs {
    /// The document's id, from list_library or search_library.
    pub doc_id: String,
    /// First page to read, 1-based. Defaults to the first page.
    pub first_page: Option<usize>,
    /// Last page to read, 1-based and inclusive.
    pub last_page: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpenArgs {
    /// The document's id, from list_library or search_library.
    pub doc_id: String,
    /// Page to scroll to, 1-based.
    pub page: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MarkupArgs {
    /// Page to draw on, 1-based.
    pub page: usize,
    /// One of: highlight, rect, ellipse, cloud, line, arrow, text, stamp.
    pub kind: String,
    /// Left edge, in PDF points from the left of the page.
    pub x0: f32,
    /// Bottom edge, in PDF points from the *bottom* of the page.
    pub y0: f32,
    /// Right edge, in PDF points.
    pub x1: f32,
    /// Top edge, in PDF points from the bottom of the page.
    pub y1: f32,
    /// Colour as #rrggbb or #rrggbbaa. Defaults to the editor's current colour.
    pub color: Option<String>,
    /// The words to write, for kind "text" and kind "stamp".
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportArgs {
    /// Where to write the PDF. An absolute path.
    pub path: String,
    /// Bake the markup into the page content instead of keeping it editable.
    pub flatten: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    /// The text to look for in the open document.
    pub query: String,
}

/// The MCP server the running app offers.
#[derive(Clone)]
pub struct EvoMcp {
    bridge: Arc<AppBridge>,
    tool_router: ToolRouter<Self>,
}

impl EvoMcp {
    pub fn new(bridge: Arc<AppBridge>) -> Self {
        Self {
            bridge,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl EvoMcp {
    /// List the documents in evo's library: id, title, page count, tags and
    /// (when evo has written one) a summary. The id is what the other library
    /// tools take.
    #[tool]
    async fn list_library(&self) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.bridge
                .submit(|reply| AppCommand::ListLibrary { reply })
                .await,
        )
    }

    /// Full-text search across every indexed page of the library. Returns
    /// matching documents with the page number and a snippet of the matching
    /// text.
    #[tool]
    async fn search_library(
        &self,
        Parameters(SearchArgs { query, limit }): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.bridge
                .submit(|reply| AppCommand::SearchLibrary {
                    query,
                    limit: limit.unwrap_or(10),
                    reply,
                })
                .await,
        )
    }

    /// Read the text of a document's pages, by 1-based page number. The text is
    /// what evo indexed, which for scanned pages is what OCR recovered.
    #[tool]
    async fn get_document_text(
        &self,
        Parameters(TextArgs {
            doc_id,
            first_page,
            last_page,
        }): Parameters<TextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.bridge
                .submit(|reply| AppCommand::GetDocumentText {
                    doc_id,
                    first: first_page,
                    last: last_page,
                    reply,
                })
                .await,
        )
    }

    /// Open a library document in the editor, optionally scrolled to a page.
    /// The user sees it happen; whatever was open is closed first.
    #[tool]
    async fn open_document(
        &self,
        Parameters(OpenArgs { doc_id, page }): Parameters<OpenArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.bridge
                .submit(|reply| AppCommand::OpenDocument {
                    doc_id,
                    page,
                    reply,
                })
                .await,
        )
    }

    /// Draw one piece of markup on the open document: a highlight, rectangle,
    /// ellipse, revision cloud, line, arrow, text box or stamp. Coordinates are PDF
    /// points measured from the bottom-left of the page. It is added to the
    /// undo history, so the user can take it back.
    #[tool]
    async fn add_markup(
        &self,
        Parameters(MarkupArgs {
            page,
            kind,
            x0,
            y0,
            x1,
            y1,
            color,
            text,
        }): Parameters<MarkupArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let req = MarkupReq {
            page,
            kind,
            x0,
            y0,
            x1,
            y1,
            color,
            text,
        };
        outcome(
            self.bridge
                .submit(|reply| AppCommand::AddMarkup { req, reply })
                .await,
        )
    }

    /// Write the open document, markup and all, to a PDF file at the given
    /// path.
    #[tool]
    async fn export_pdf(
        &self,
        Parameters(ExportArgs { path, flatten }): Parameters<ExportArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.bridge
                .submit(|reply| AppCommand::ExportPdf {
                    path,
                    flatten: flatten.unwrap_or(false),
                    reply,
                })
                .await,
        )
    }

    /// Find text in the open document, returning the page and position of each
    /// match in PDF points. Useful for working out where to put markup.
    #[tool]
    async fn get_find_matches(
        &self,
        Parameters(FindArgs { query }): Parameters<FindArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.bridge
                .submit(|reply| AppCommand::FindMatches { query, reply })
                .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EvoMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("evo", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy() -> EvoMcp {
        let (tx, rx) = std::sync::mpsc::channel();
        // Keeping the receiver alive would need a thread; the router does not
        // send anything, and a closed channel is a tested failure mode anyway.
        drop(rx);
        EvoMcp::new(Arc::new(AppBridge::new(
            tx,
            eframe::egui::Context::default(),
        )))
    }

    /// The seven tools are the contract with every MCP client; a rename or a
    /// dropped one is a break, not a refactor.
    #[test]
    fn every_tool_is_offered_with_a_description_and_a_schema() {
        let tools = dummy().tool_router.list_all();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
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
        for tool in &tools {
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.len() > 20,
                "{} needs a description a model can act on",
                tool.name
            );
            assert_eq!(
                tool.input_schema.get("type").and_then(|t| t.as_str()),
                Some("object"),
                "{} has no argument schema",
                tool.name
            );
        }
    }

    /// The arguments a model has to guess at are the ones worth documenting;
    /// check the generated schema actually carries the page numbering and the
    /// coordinate system.
    #[test]
    fn the_markup_schema_says_which_way_up_the_page_is() {
        let tools = dummy().tool_router.list_all();
        let markup = tools
            .iter()
            .find(|t| t.name == "add_markup")
            .expect("add_markup");
        let schema = serde_json::to_string(&markup.input_schema).expect("schema");
        assert!(schema.contains("1-based"), "{schema}");
        assert!(schema.contains("bottom"), "{schema}");
        assert!(schema.contains("highlight"), "{schema}");
    }

    #[test]
    fn the_server_introduces_itself_as_evo_with_tools() {
        let info = dummy().get_info();
        assert!(info.capabilities.tools.is_some(), "tools are on offer");
        assert_eq!(info.server_info.name, "evo");
        assert!(
            info.instructions
                .as_deref()
                .is_some_and(|i| i.contains("undoable")),
            "the instructions warn that markup lands on the user's document"
        );
    }

    /// A tool that could not run tells the model why, in the result, instead of
    /// failing the request: the model can then do something else.
    #[test]
    fn a_refused_command_becomes_a_readable_tool_error() {
        let result = outcome(Err("no document is open".to_owned())).expect("a result");
        assert_eq!(result.is_error, Some(true));
        let text = serde_json::to_string(&result.content).expect("content");
        assert!(text.contains("no document is open"), "{text}");

        let ok = outcome(Ok(serde_json::json!({"count": 0}))).expect("a result");
        assert_ne!(ok.is_error, Some(true));
    }
}
