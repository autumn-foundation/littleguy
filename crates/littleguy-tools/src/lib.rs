//! The tools of the littleguy assistant.
//!
//! - [`FetchTool`] reads one web page as text. It resolves the host first
//!   and refuses an address that is not public, so a page or a model cannot
//!   aim it at the router, a metadata service or a local daemon.
//! - [`SearchTool`] asks the Brave Search API for web results.
//! - [`file_tools`] read and write the files of the persistent workspace of
//!   the agent.
//! - [`search_notes_tool`] finds the files of the workspace that hold some
//!   words, so the agent can recall a note whose path it forgot.
//! - [`grep_files_tool`] finds the lines of the workspace that match a
//!   pattern, with their paths and line numbers, as `rg` does.
//! - [`mcp::connect`] turns the tools of each MCP server in the config into
//!   tools of the agent.
//! - [`google::google_tools`] read and send Gmail, and read and add Google
//!   Calendar events, through a token proxy.
//!
//! Feature `web` builds the fetch and search tools, feature `mcp` the MCP
//! tools, feature `google` the Google tools, feature `search-notes` the
//! note search and feature `grep-files` the line search. All five are on by
//! default. The file tools are always on.
//!
//! The web tools read only, and label their results as untrusted input,
//! so flow rules can fence what follows them. The file tools change only
//! the private workspace of the agent.

#[cfg(feature = "web")]
mod fetch;
mod files;
#[cfg(feature = "google")]
pub mod google;
#[cfg(feature = "grep-files")]
mod grep;
pub mod html;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "search-notes")]
mod notes;
#[cfg(feature = "web")]
mod search;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(test)]
mod tests;

#[cfg(feature = "web")]
pub use fetch::{FetchTool, MAX_BODY_BYTES, MAX_REDIRECTS};
pub use files::{MAX_READ_CHARS, file_tools};
#[cfg(feature = "grep-files")]
pub use grep::{DEFAULT_GREP_MATCHES, MAX_GREP_MATCHES, grep_files_tool};
#[cfg(feature = "search-notes")]
pub use notes::{DEFAULT_MATCHES, search_notes_tool};
#[cfg(feature = "web")]
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

/// A window of `text`: at most `max` characters from character `offset`.
/// Returns the window, whether more text follows it, and the length of
/// `text` in characters. An `offset` past the end gives an empty window
/// (issue #73).
fn window(text: &str, offset: usize, max: usize) -> (String, bool, usize) {
    let length = text.chars().count();
    let start = text
        .char_indices()
        .nth(offset)
        .map_or(text.len(), |(at, _)| at);
    let (shown, more) = clip(&text[start..], max);
    (shown, more, length)
}

/// The optional `offset` of `input`: a whole number of characters, 0 when
/// it is missing.
fn offset_field(input: &serde_json::Value) -> Result<usize, AgentError> {
    match input.get("offset") {
        None | Some(serde_json::Value::Null) => Ok(0),
        Some(value) => value
            .as_u64()
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| refuse("`offset` must be a whole number of characters, from 0")),
    }
}

/// The schema of an `offset` field.
fn offset_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "integer",
        "minimum": 0,
        "description": "Where to start, in characters from the start. Leave it out to start at 0. \
                        `length` is the whole size in characters. When `offset` plus the \
                        characters of the `text` you got is less than `length`, call again from \
                        there to read on. Count what you got: a long result can reach you cut.",
    })
}

/// Installs the TLS provider. Reqwest panics without one, and an `Err`
/// means that one is installed already.
#[cfg(any(feature = "web", feature = "mcp"))]
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
    fn a_window_starts_at_its_offset_and_counts_characters() {
        assert_eq!(window("héllo", 1, 2), (String::from("él"), true, 5));
        assert_eq!(window("héllo", 3, 10), (String::from("lo"), false, 5));
        assert_eq!(window("héllo", 5, 10), (String::new(), false, 5));
        assert_eq!(window("héllo", 99, 10), (String::new(), false, 5));
        assert_eq!(window("", 0, 10), (String::new(), false, 0));
    }

    #[test]
    fn an_offset_is_a_whole_number_or_nothing() {
        assert_eq!(offset_field(&serde_json::json!({})).unwrap(), 0);
        assert_eq!(
            offset_field(&serde_json::json!({"offset": null})).unwrap(),
            0
        );
        assert_eq!(
            offset_field(&serde_json::json!({"offset": 12})).unwrap(),
            12
        );
        for bad in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("3"),
        ] {
            let error = offset_field(&serde_json::json!({"offset": bad})).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Tool);
            assert!(error.to_string().contains("`offset`"), "{error}");
        }
    }

    #[test]
    fn a_missing_field_is_a_tool_error() {
        let error = text_field(&serde_json::json!({}), "url").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool);
        assert!(error.to_string().contains("`url`"));
    }
}
