//! `grep_files`: find the lines of the workspace that match a pattern
//! (issue #75).
//!
//! `search_notes` ranks notes by their words. Code needs the other kind of
//! search: each line that matches, with its path and its line number, as
//! `rg` gives them. The `regex` crate matches in linear time, so no
//! pattern can hang a call, and a size limit keeps a pattern small. It only
//! reads, so a heartbeat tick may call it.

use std::sync::Arc;

use autumn_harvest_agent::{AgentError, FnTool, Tool, ToolContext, ToolEffect};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::{refuse, text_field};

/// The most matches that one call returns when it names none. A call
/// names any count, and `more` says when more lines match.
pub const DEFAULT_GREP_MATCHES: usize = 50;

/// The longest pattern, in bytes.
const MAX_PATTERN_BYTES: usize = 1000;

/// The most memory that a compiled pattern may take, in bytes.
const PATTERN_SIZE_LIMIT: usize = 1 << 20;

/// One line that matches.
#[derive(Debug, PartialEq, Eq)]
struct Hit {
    path: String,
    /// The line number, from 1.
    line: usize,
    text: String,
}

/// The regex of a call.
fn pattern(input: &Value) -> Result<Regex, AgentError> {
    let pattern = text_field(input, "pattern")?;
    if pattern.is_empty() {
        return Err(refuse("the pattern is empty"));
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(refuse(format!(
            "the pattern is longer than {MAX_PATTERN_BYTES} bytes"
        )));
    }
    let literal = input["literal"].as_bool().unwrap_or(false);
    let source = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    RegexBuilder::new(&source)
        .case_insensitive(input["ignore_case"].as_bool().unwrap_or(false))
        .size_limit(PATTERN_SIZE_LIMIT)
        .build()
        .map_err(|error| refuse(format!("`{pattern}` is not a pattern: {error}")))
}

/// The matching lines of `text`, at most `room` of them, and whether more
/// lines match.
fn hits(path: &str, text: &str, regex: &Regex, room: usize) -> (Vec<Hit>, bool) {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if !regex.is_match(line) {
            continue;
        }
        if found.len() == room {
            return (found, true);
        }
        found.push(Hit {
            path: path.to_owned(),
            line: index + 1,
            text: line.to_owned(),
        });
    }
    (found, false)
}

fn grep(input: &Value, ctx: &ToolContext) -> Result<Value, AgentError> {
    let regex = pattern(input)?;
    let prefix = input["path"].as_str().unwrap_or_default();
    let limit = input["limit"]
        .as_u64()
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(DEFAULT_GREP_MATCHES)
        .max(1);
    let workspace = ctx
        .workspace
        .as_ref()
        .ok_or_else(|| refuse("this agent has no workspace"))?;
    let (found, searched, more) = workspace.with(|files| {
        let mut found = Vec::new();
        let mut searched = 0;
        let mut more = false;
        let mut paths: Vec<_> = files
            .files()
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .collect();
        paths.sort_by(|a, b| a.0.cmp(b.0));
        for (path, file) in paths {
            // A file that is not text has no lines.
            let Ok(text) = std::str::from_utf8(&file.data) else {
                continue;
            };
            searched += 1;
            if found.len() == limit {
                // The room is full: a match here is one more than it shows.
                more = more || regex.is_match(text);
                continue;
            }
            let (hits, cut) = hits(path, text, &regex, limit - found.len());
            found.extend(hits);
            more = more || cut;
        }
        (found, searched, more)
    });
    Ok(json!({
        "pattern": regex.as_str(),
        "searched": searched,
        "more": more,
        "matches": found.iter().map(|hit| json!({
            "path": hit.path,
            "line": hit.line,
            "text": hit.text,
        })).collect::<Vec<_>>(),
    }))
}

/// The `grep_files` tool over the workspace.
#[must_use]
pub fn grep_files_tool() -> Arc<dyn Tool> {
    FnTool::with_context(
        "grep_files",
        "Find the lines of the files of your workspace that match a pattern, as rg does: \
         each match gives its path, its line number from 1, and the line. Use it to find \
         where code is defined or used. Then read around a match with read_file.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "A regex in Rust syntax, such as `fn (begin|end)_turn`. With `literal`, plain text."},
                "literal": {"type": "boolean", "description": "Match `pattern` as plain text. Default false."},
                "ignore_case": {"type": "boolean", "description": "Match upper and lower case alike. Default false."},
                "path": {"type": "string", "description": "Keep only the files whose path starts with this, such as `crates/littleguy/src/`."},
                "limit": {"type": "integer", "minimum": 1, "description": "The most matches. Default 50."},
            },
            "required": ["pattern"],
        }),
        |input, ctx| async move { grep(&input, &ctx) },
    )
    .effect(ToolEffect::ReadOnly)
    .uses_workspace()
    .shared()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex(source: &str) -> Regex {
        Regex::new(source).unwrap()
    }

    #[test]
    fn hits_give_their_line_from_one_and_stop_at_the_room() {
        let text = "fn a() {}\nlet x = 1;\nfn b() {}\nfn c() {}\n";
        let (found, more) = hits("src/lib.rs", text, &regex("^fn "), 2);
        let lines: Vec<usize> = found.iter().map(|hit| hit.line).collect();
        assert_eq!(lines, [1, 3]);
        assert_eq!(found[1].text, "fn b() {}");
        assert!(more);
        let (found, more) = hits("src/lib.rs", text, &regex("^fn "), 3);
        assert_eq!(found.len(), 3);
        assert!(!more);
    }

    #[test]
    fn a_long_line_comes_whole() {
        let line = "x".repeat(10_000);
        let (found, _) = hits("a.txt", &line, &regex("x"), 1);
        assert_eq!(found[0].text, line);
    }

    #[test]
    fn a_bad_pattern_is_a_tool_error() {
        for input in [
            json!({"pattern": "("}),
            json!({"pattern": ""}),
            json!({"pattern": "x".repeat(MAX_PATTERN_BYTES + 1)}),
            json!({}),
        ] {
            assert!(pattern(&input).is_err(), "{input}");
        }
        let literal = pattern(&json!({"pattern": "a.b(", "literal": true})).unwrap();
        assert!(literal.is_match("a.b("));
        assert!(!literal.is_match("axb("));
        let loose = pattern(&json!({"pattern": "TICK", "ignore_case": true})).unwrap();
        assert!(loose.is_match("tick_log"));
    }
}
