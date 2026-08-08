//! Model Context Protocol: evo as a tool server.
//!
//! Two ways in, one set of tools. The running app serves them over HTTP on the
//! loopback interface so an assistant can search the library, open a document
//! and mark it up while you watch it happen; `evo mcp-serve` serves the
//! library-only subset over stdio for clients that would rather start their own
//! process, and works whether or not the app is running (it is not, if it can
//! take the database lock).
//!
//! The interesting problem is that MCP is async and evo is a synchronous egui
//! app that owns everything worth reaching. [`bridge`] is the answer: tool
//! bodies post a command with a one-shot reply channel and wait for it, and the
//! UI thread answers between frames with `&mut self` in hand. Nothing about the
//! app has to become thread-safe, and no tool can be running while the UI is
//! halfway through a frame.

pub mod bridge;
pub mod headless;
pub mod library_tools;
pub mod runtime;
pub mod server;

use serde::{Deserialize, Serialize};

/// The port evo offers by default. Not registered with anyone; picked to be
/// out of the way of the usual local model servers.
pub const DEFAULT_PORT: u16 = 8137;

/// How the MCP server is set up. Persisted under `"mcp_prefs"`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct McpPrefs {
    /// Off until asked for: evo does not open a port on your machine because
    /// you installed it.
    #[serde(default)]
    pub server_enabled: bool,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Shared secret every request must present. Anything on this machine can
    /// reach a loopback port, so the token is what makes "loopback only"
    /// mean something.
    #[serde(default = "new_token")]
    pub token: String,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for McpPrefs {
    fn default() -> Self {
        Self {
            server_enabled: false,
            port: DEFAULT_PORT,
            token: new_token(),
        }
    }
}

impl McpPrefs {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// A block a user can paste into an MCP client's configuration.
    pub fn client_config(&self) -> String {
        format!(
            "{{\n  \"mcpServers\": {{\n    \"evo\": {{\n      \"url\": \"{}\",\n      \
             \"headers\": {{\n        \"Authorization\": \"Bearer {}\"\n      }}\n    \
             }}\n  }}\n}}",
            self.url(),
            self.token
        )
    }
}

/// A fresh 32-hex-character token.
///
/// `getrandom` is the operating system's own generator; it was already in the
/// tree, and a token nobody can guess is the whole point.
pub fn new_token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No entropy source at all is not something to paper over with a
        // predictable token: say so, and let the user regenerate.
        return "unavailable-regenerate-me".to_owned();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `presented` is the expected token, compared without leaking how much
/// of it matched.
pub fn token_matches(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.bytes().zip(presented.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_is_off_until_it_is_asked_for() {
        let prefs = McpPrefs::default();
        assert!(!prefs.server_enabled, "evo does not open a port unasked");
        assert_eq!(prefs.port, DEFAULT_PORT);
    }

    #[test]
    fn a_token_is_thirty_two_hex_characters_and_not_the_same_twice() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 32, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b, "two tokens in a row must not match");
    }

    /// Older preferences knew nothing about MCP; they must still load, and get
    /// a token of their own rather than an empty one that would let anything
    /// in.
    #[test]
    fn preferences_without_a_token_get_one() {
        let prefs: McpPrefs = serde_json::from_str("{}").expect("defaults fill in");
        assert!(!prefs.server_enabled);
        assert_eq!(prefs.port, DEFAULT_PORT);
        assert_eq!(prefs.token.len(), 32);
    }

    #[test]
    fn a_token_only_matches_itself() {
        let token = new_token();
        assert!(token_matches(&token, &token.clone()));
        assert!(!token_matches(&token, ""));
        assert!(!token_matches(&token, &token[..31]));
        let mut wrong = token.clone();
        wrong.replace_range(0..1, if token.starts_with('a') { "b" } else { "a" });
        assert!(!token_matches(&token, &wrong));
    }

    #[test]
    fn the_pasteable_config_carries_the_url_and_the_token() {
        let prefs = McpPrefs {
            port: 9000,
            token: "abc123".to_owned(),
            ..Default::default()
        };
        let config = prefs.client_config();
        assert!(config.contains("http://127.0.0.1:9000/mcp"), "{config}");
        assert!(config.contains("Bearer abc123"), "{config}");
        // It is offered as something to paste, so it had better parse.
        let parsed: serde_json::Value = serde_json::from_str(&config).expect("valid JSON");
        assert_eq!(parsed["mcpServers"]["evo"]["url"], prefs.url());
    }
}
