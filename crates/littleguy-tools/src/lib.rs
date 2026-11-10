//! The tools of the littleguy assistant.
//!
//! - [`FetchTool`] reads one web page as text. It resolves the host first
//!   and refuses an address that is not public, so a page or a model cannot
//!   aim it at the router, a metadata service or a local daemon.
//! - [`SearchTool`] asks the Brave Search API for web results.
//! - [`file_tools`] read and write the files of the persistent workspace of
//!   the agent.
//! - [`mcp::connect`] turns the tools of each MCP server in the config into
//!   tools of the agent.
//! - [`google::google_tools`] read and send Gmail, and read and add Google
//!   Calendar events, through a token proxy.
//!
//! The web tools read only, and label their results as untrusted input,
//! so flow rules can fence what follows them. The file tools change only
//! the private workspace of the agent.

mod fetch;
mod files;
pub mod google;
pub mod html;
pub mod mcp;
mod search;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(test)]
mod tests;

pub use fetch::{FetchTool, MAX_BODY_BYTES, MAX_REDIRECTS};
pub use files::{MAX_READ_CHARS, file_tools};
pub use search::{BRAVE_ENDPOINT, SearchTool};

use autumn_harvest_agent::{AgentError, ErrorKind};

/// A tool failure that the model reads and can act on. It never retries.
fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

/// The string field `name` of a tool input.
fn text_field<'a>(input: &'a serde_json::Value, name: &str) -> Result<&'a str, AgentError> {
    input
        .get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| refuse(format!("the input needs a string `{name}`")))
}

/// The first `max` characters of `text`, and whether it was longer.
fn clip(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((end, _)) => (text[..end].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// Installs the TLS provider. Reqwest panics without one, and an `Err`
/// means that one is installed already.
fn tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn clip_cuts_on_a_character_boundary() {
        assert_eq!(clip("héllo", 2), (String::from("hé"), true));
        assert_eq!(clip("hé", 2), (String::from("hé"), false));
        assert_eq!(clip("", 0), (String::new(), false));
    }

    #[test]
    fn a_missing_field_is_a_tool_error() {
        let error = text_field(&serde_json::json!({}), "url").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool);
        assert!(error.to_string().contains("`url`"));
    }
}
