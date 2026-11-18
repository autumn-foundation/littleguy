//! `search_notes`: find the files of the workspace that hold some words.
//!
//! The assistant can read a file only when it knows the path. This tool
//! scans the workspace for the words of a query and returns the files that
//! hold them, with a short snippet each. It reads no note of `memory.db`.
//! `list_notes` with a query ranks those, with the same [`rank`] (issue
//! #35). It is a linear scan with no index:
//! a workspace of a few hundred short notes is far below the size where an
//! index earns its keep. It only reads, so a heartbeat tick may call it.

use std::sync::Arc;

use autumn_harvest_agent::{AgentError, FnTool, Tool, ToolContext, ToolEffect};
use serde_json::{Value, json};

use crate::{refuse, text_field};

/// The most matches that one search returns when the call names none.
pub const DEFAULT_MATCHES: usize = 10;

/// How many characters of text a snippet shows on each side of a hit.
const SNIPPET_SIDE: usize = 80;

/// How many hits of one word in one file count. More say little more.
const MAX_HITS_PER_WORD: usize = 5;

/// What a word in the path of a file counts, against one hit in its text.
const PATH_WEIGHT: usize = 3;

/// The words of `query`: lowercase runs of letters and digits, two
/// characters or more, each once, in order.
fn words(query: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.chars().count() >= 2)
    {
        if !words.contains(&word) {
            words.push(word);
        }
    }
    words
}

/// One file that holds a word of the query.
#[derive(Debug, PartialEq, Eq)]
struct Match {
    path: String,
    /// How many words of the query the file holds.
    matched: usize,
    score: usize,
    snippet: String,
}

/// The text around byte `at` of `text`, on character boundaries, on one
/// line.
fn snippet(text: &str, at: usize) -> String {
    let start = text[..at]
        .char_indices()
        .rev()
        .nth(SNIPPET_SIDE - 1)
        .map_or(0, |(index, _)| index);
    let end = text[at..]
        .char_indices()
        .nth(SNIPPET_SIDE)
        .map_or(text.len(), |(index, _)| at + index);
    let mut shown = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if start > 0 {
        shown.insert(0, '…');
    }
    if end < text.len() {
        shown.push('…');
    }
    shown
}

/// How `path` with `text` matches `words`, or `None` when it holds none.
fn score(path: &str, text: &str, words: &[String]) -> Option<Match> {
    let lower_text = text.to_lowercase();
    let lower_path = path.to_lowercase();
    let mut matched = 0;
    let mut score = 0;
    let mut first: Option<usize> = None;
    for word in words {
        let hits = lower_text.matches(word.as_str()).count();
        let in_path = lower_path.contains(word.as_str());
        if hits == 0 && !in_path {
            continue;
        }
        matched += 1;
        score += hits.min(MAX_HITS_PER_WORD) + if in_path { PATH_WEIGHT } else { 0 };
        if first.is_none() {
            first = lower_text.find(word.as_str());
        }
    }
    // The hit is a byte in the lowercase text. Lowercase can change the
    // length of a character, so the snippet finds it by its count of
    // characters in the text. A word only in the path shows the start.
    let at = first.map_or(0, |first| {
        let chars = lower_text[..first].chars().count();
        text.char_indices().nth(chars).map_or(0, |(index, _)| index)
    });
    (matched > 0).then(|| Match {
        path: path.to_owned(),
        matched,
        score,
        snippet: snippet(text, at),
    })
}

/// One text that holds words of a query, as [`rank`] gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The path or the name of the text.
    pub name: String,
    /// How many words of the query it holds.
    pub matched: usize,
    /// How many words the query has.
    pub of: usize,
    /// The text around the first hit, on one line.
    pub snippet: String,
}

/// The texts of `docs`, each a name and its text, that hold words of
/// `query`: those with the most words first, then the most hits, then by
/// name, at most `limit`. Returns them, and whether more matched.
///
/// # Errors
///
/// Returns a `Tool` error when the query has no word.
pub fn rank<'a>(
    docs: impl IntoIterator<Item = (&'a str, &'a str)>,
    query: &str,
    limit: usize,
) -> Result<(Vec<Hit>, bool), AgentError> {
    let words = words(query);
    if words.is_empty() {
        return Err(refuse("the query needs a word of two letters or more"));
    }
    let mut matches: Vec<Match> = docs
        .into_iter()
        .filter_map(|(name, text)| score(name, text, &words))
        .collect();
    matches.sort_by(|a, b| {
        b.matched
            .cmp(&a.matched)
            .then(b.score.cmp(&a.score))
            .then(a.path.cmp(&b.path))
    });
    let more = matches.len() > limit;
    matches.truncate(limit);
    let hits = matches
        .into_iter()
        .map(|found| Hit {
            name: found.path,
            matched: found.matched,
            of: words.len(),
            snippet: found.snippet,
        })
        .collect();
    Ok((hits, more))
}

/// The most matches that `input` asks for.
#[must_use]
pub fn limit_of(input: &Value) -> usize {
    input["limit"]
        .as_u64()
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(DEFAULT_MATCHES)
        .max(1)
}

fn search(input: &Value, ctx: &ToolContext) -> Result<Value, AgentError> {
    let query = text_field(input, "query")?;
    let workspace = ctx
        .workspace
        .as_ref()
        .ok_or_else(|| refuse("this agent has no workspace"))?;
    let (hits, more, searched) = workspace.with(|files| {
        // A file that is not text holds no words.
        let texts: Vec<(&str, &str)> = files
            .files()
            .iter()
            .filter_map(|(path, file)| Some((path.as_str(), std::str::from_utf8(&file.data).ok()?)))
            .collect();
        let searched = texts.len();
        rank(texts, query, limit_of(input)).map(|(hits, more)| (hits, more, searched))
    })?;
    Ok(json!({
        "query": query,
        "searched": searched,
        "more": more,
        "matches": hits.iter().map(|hit| json!({
            "path": hit.name,
            "matched": hit.matched,
            "of": hit.of,
            "snippet": hit.snippet,
        })).collect::<Vec<_>>(),
    }))
}

/// The `search_notes` tool over the workspace.
#[must_use]
pub fn search_notes_tool() -> Arc<dyn Tool> {
    FnTool::with_context(
        "search_notes",
        "Search the files of your workspace for words, when you half remember a file of \
         your workspace but not its path. It does not read your notes. It returns the files \
         that hold the most of the words first, each with a short snippet. Read a match \
         with read_file.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The words to look for, such as `tea preference`."},
                "limit": {"type": "integer", "minimum": 1, "description": "The most matches. Default 10."},
            },
            "required": ["query"],
        }),
        |input, ctx| async move { search(&input, &ctx) },
    )
    .effect(ToolEffect::ReadOnly)
    .uses_workspace()
    .shared()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_words_of_a_query_are_lowercase_and_each_once() {
        assert_eq!(words("Tea, tea & a GREEN-tea!"), ["tea", "green"]);
        assert_eq!(words("a ! ?"), [] as [String; 0]);
    }

    #[test]
    fn a_snippet_shows_the_hit_on_one_line() {
        let text = format!(
            "{}\nMark likes green tea.\n{}",
            "x ".repeat(100),
            "y ".repeat(100)
        );
        let at = text.find("green").unwrap();
        let shown = snippet(&text, at);
        assert!(shown.contains("Mark likes green tea."));
        assert!(shown.starts_with('…') && shown.ends_with('…'));
        assert!(!shown.contains('\n'));
        assert_eq!(snippet("short", 0), "short");
    }

    #[test]
    fn a_letter_that_lowercases_longer_moves_no_hit_off_a_boundary() {
        // `İ` is two characters in lowercase, so the offsets of the two
        // texts differ after it.
        let found = score("p.md", "İİİ İstanbul has tea.", &words("tea")).unwrap();
        assert!(found.snippet.contains("tea"));
    }
}
