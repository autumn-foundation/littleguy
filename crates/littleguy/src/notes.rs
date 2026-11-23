//! The note tools of the assistant (ADR 0029).
//!
//! A note is text under a name in `memory.db`, beside the ideas and goals.
//! A tick is refused `write_file` and `delete_file`, so a note in the
//! workspace cannot be fixed unattended. These notes can: `write_note` and `delete_note` are
//! `Internal`, as `add_idea` is, and they persist on a tick. `read_note`
//! and `list_notes` only read. A note can hold what a page said, so both
//! carry the `untrusted_input` label, as `list_plans` does.

use std::sync::Arc;

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::{AgentError, ErrorKind, FnTool, Tool, ToolEffect};
use jiff::Timestamp;
use littleguy_stores::SqliteStores;
use serde_json::{Value, json};

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

fn text<'a>(input: &'a Value, name: &str) -> Result<&'a str, AgentError> {
    input[name]
        .as_str()
        .ok_or_else(|| refuse(format!("`{name}` must be text")))
}

/// Adds when a note last changed, as the store stamped it, to `entry`.
/// `updated_ms` is exact for code. `updated` is RFC 3339, so a model reads
/// a date and not an epoch count. A stale note then looks stale before a
/// re-read.
fn stamped(mut entry: Value, updated_ms: i64) -> Value {
    entry["updated_ms"] = json!(updated_ms);
    entry["updated"] = crate::stamp::from_millis(updated_ms).map_or(Value::Null, |at| {
        json!(at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
    });
    entry
}

/// The reply of `list_notes`: every note, or the notes that match a query.
fn list_notes(stores: &SqliteStores, input: &Value) -> Result<Value, AgentError> {
    let notes = stores.notes()?;
    let Some(query) = input["query"].as_str().filter(|q| !q.trim().is_empty()) else {
        let all = notes.iter().map(|note| {
            let entry = json!({"note": note.name, "chars": note.text.chars().count()});
            stamped(entry, note.updated_ms)
        });
        return Ok(json!({"notes": all.collect::<Vec<_>>()}));
    };
    let docs = notes
        .iter()
        .map(|note| (note.name.as_str(), note.text.as_str()));
    let (hits, more) = littleguy_tools::rank(docs, query, littleguy_tools::limit_of(input))?;
    let matches = hits.iter().map(|hit| {
        let entry = json!({
            "note": hit.name,
            "matched": hit.matched,
            "of": hit.of,
            "snippet": hit.snippet,
        });
        // The ranker drops the row, so the hit finds it again by name.
        // Names are unique in the store, so the first match is the row.
        match notes.iter().find(|note| note.name == hit.name) {
            Some(note) => stamped(entry, note.updated_ms),
            None => entry,
        }
    });
    Ok(json!({"query": query, "more": more, "matches": matches.collect::<Vec<_>>()}))
}

fn name_schema(extra: Option<(&str, &str)>) -> Value {
    let mut properties = json!({
        "name": {"type": "string", "description": "A short name, such as ui-punchlist."}
    });
    let mut required = vec!["name"];
    if let Some((field, description)) = extra {
        properties[field] = json!({"type": "string", "description": description});
        required.push(field);
    }
    json!({"type": "object", "properties": properties, "required": required})
}

/// The four note tools over `stores`.
#[must_use]
pub fn tools(stores: &Arc<SqliteStores>) -> Vec<Arc<dyn Tool>> {
    let write = Arc::clone(stores);
    let read = Arc::clone(stores);
    let list = Arc::clone(stores);
    let delete = Arc::clone(stores);
    vec![
        FnTool::new(
            "write_note",
            "Write a note: what you learned and will need again. It replaces a note of the \
             same name. Notes are your own, so this works on a heartbeat too, where \
             write_file and delete_file are refused. Fix a note as soon as you learn it is wrong.",
            name_schema(Some(("text", "The whole note, in markdown."))),
            move |input| {
                let stores = Arc::clone(&write);
                async move {
                    let name = text(&input, "name")?;
                    let body = text(&input, "text")?;
                    let new = stores.write_note(name, body, Timestamp::now().as_millisecond())?;
                    Ok(json!({"note": name.trim(), "new": new, "chars": body.chars().count()}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
        FnTool::new(
            "read_note",
            "Read one of your notes by its name, with when it last changed.",
            name_schema(None),
            move |input| {
                let stores = Arc::clone(&read);
                async move {
                    let name = text(&input, "name")?;
                    let note = stores
                        .note(name)?
                        .ok_or_else(|| refuse(format!("there is no note named `{name}`")))?;
                    Ok(stamped(
                        json!({"note": note.name, "text": note.text}),
                        note.updated_ms,
                    ))
                }
            },
        )
        .effect(ToolEffect::ReadOnly)
        .label(labels::UNTRUSTED_INPUT)
        .shared(),
        FnTool::new(
            "list_notes",
            "List your notes with their sizes and when each last changed. With a query, list only the notes that hold its \
             words, the best first, each with a snippet, when you half remember a note.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to look for. Leave out to list all."},
                    "limit": {"type": "integer", "minimum": 1, "description": "The most matches of a query. Default 10."},
                },
            }),
            move |input| {
                let stores = Arc::clone(&list);
                async move {
                    list_notes(&stores, &input)
                }
            },
        )
        .effect(ToolEffect::ReadOnly)
        .label(labels::UNTRUSTED_INPUT)
        .shared(),
        FnTool::new(
            "delete_note",
            "Delete one of your notes, for example one that you joined into another.",
            name_schema(None),
            move |input| {
                let stores = Arc::clone(&delete);
                async move {
                    let name = text(&input, "name")?;
                    Ok(json!({"note": name.trim(), "deleted": stores.delete_note(name)?}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #35: the workspace is not read-only on a tick. A tick is
    /// refused the file tools that change it, which are `Write`.
    #[test]
    fn write_note_names_the_refusal_and_not_a_read_only_workspace() {
        let stores = Arc::new(SqliteStores::open_in_memory().unwrap());
        let write = tools(&stores)
            .into_iter()
            .find(|tool| tool.name() == "write_note")
            .unwrap();
        let said = write.description();
        assert!(!said.contains("read-only"), "{said}");
        assert!(said.contains("write_file"), "{said}");
    }

    #[tokio::test]
    async fn notes_are_written_found_read_and_deleted_and_safe_on_a_tick() {
        let stores = Arc::new(SqliteStores::open_in_memory().unwrap());
        let tools = tools(&stores);
        let tool = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name() == name)
                .unwrap()
                .clone()
        };
        for tool in &tools {
            assert!(
                tool.effect() <= ToolEffect::Internal,
                "{} runs on a tick",
                tool.name()
            );
        }
        let ctx = autumn_harvest_agent::ToolContext {
            run_id: autumn_harvest_agent::RunId::new("r"),
            call_id: String::from("c"),
            session_id: None,
            step: 0,
            workspace: None,
        };
        let wrote = tool("write_note")
            .execute(
                json!({"name": "drinks", "text": "Mark likes green tea."}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(wrote["new"], true);
        tool("write_note")
            .execute(json!({"name": "todo", "text": "Fix the docs links."}), &ctx)
            .await
            .unwrap();
        let found = tool("list_notes")
            .execute(json!({"query": "green tea"}), &ctx)
            .await
            .unwrap();
        assert_eq!(found["matches"][0]["note"], "drinks");
        assert_eq!(found["matches"].as_array().unwrap().len(), 1);
        let all = tool("list_notes").execute(json!({}), &ctx).await.unwrap();
        assert_eq!(all["notes"].as_array().unwrap().len(), 2);
        let read = tool("read_note")
            .execute(json!({"name": "drinks"}), &ctx)
            .await
            .unwrap();
        assert_eq!(read["text"], "Mark likes green tea.");
        let gone = tool("delete_note")
            .execute(json!({"name": "drinks"}), &ctx)
            .await
            .unwrap();
        assert_eq!(gone["deleted"], true);
        assert!(
            tool("read_note")
                .execute(json!({"name": "drinks"}), &ctx)
                .await
                .is_err()
        );
        for name in ["read_note", "list_notes"] {
            assert!(
                tool(name)
                    .labels()
                    .iter()
                    .any(|l| l == labels::UNTRUSTED_INPUT)
            );
        }
    }

    /// `read_note` and both forms of `list_notes` name when a note last
    /// changed, so a stale note does not read like a fresh one.
    #[tokio::test]
    async fn each_read_of_a_note_names_when_it_last_changed() {
        let stores = Arc::new(SqliteStores::open_in_memory().unwrap());
        // 2025-11-16T12:00:00Z and 2025-01-02T03:04:05Z.
        let fresh = 1_763_294_400_000_i64;
        let stale = 1_735_787_045_000_i64;
        stores
            .write_note("drinks", "Mark likes green tea.", fresh)
            .unwrap();
        stores
            .write_note("todo", "Fix the docs links.", stale)
            .unwrap();
        let tools = tools(&stores);
        let tool = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name() == name)
                .unwrap()
                .clone()
        };
        let ctx = autumn_harvest_agent::ToolContext {
            run_id: autumn_harvest_agent::RunId::new("r"),
            call_id: String::from("c"),
            session_id: None,
            step: 0,
            workspace: None,
        };

        let read = tool("read_note")
            .execute(json!({"name": "todo"}), &ctx)
            .await
            .unwrap();
        assert_eq!(read["updated_ms"], stale);
        assert_eq!(read["updated"], "2025-01-02T03:04:05Z");

        let all = tool("list_notes").execute(json!({}), &ctx).await.unwrap();
        let all = all["notes"].as_array().unwrap();
        assert_eq!(all[0]["note"], "drinks");
        assert_eq!(all[0]["updated_ms"], fresh);
        assert_eq!(all[0]["updated"], "2025-11-16T12:00:00Z");
        assert_eq!(all[1]["note"], "todo");
        assert_eq!(all[1]["updated_ms"], stale);

        let found = tool("list_notes")
            .execute(json!({"query": "docs links"}), &ctx)
            .await
            .unwrap();
        assert_eq!(found["matches"][0]["note"], "todo");
        assert_eq!(found["matches"][0]["updated_ms"], stale);
        assert_eq!(found["matches"][0]["updated"], "2025-01-02T03:04:05Z");

        // A rewrite moves the stamp forward.
        stores
            .write_note("todo", "The docs links are fixed.", fresh)
            .unwrap();
        let read = tool("read_note")
            .execute(json!({"name": "todo"}), &ctx)
            .await
            .unwrap();
        assert_eq!(read["updated_ms"], fresh);
    }
}
