//! The note tools of the assistant (ADR 0029).
//!
//! A note is text under a name in `memory.db`, beside the ideas and goals.
//! The workspace is read-only on a tick, so a note there cannot be fixed
//! unattended. These notes can: `write_note` and `delete_note` are
//! `Internal`, as `add_idea` is, and they persist on a tick. `read_note`
//! and `list_notes` only read. A note can hold what a page said, so both
//! carry the `untrusted_input` label, as `list_plans` does.

use std::sync::Arc;

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::{AgentError, ErrorKind, FnTool, Tool, ToolEffect};
use chrono::Utc;
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
             same name. Notes are your own, so this works on a heartbeat too, where the \
             workspace files are read-only. Fix a note as soon as you learn it is wrong.",
            name_schema(Some(("text", "The whole note, in markdown."))),
            move |input| {
                let stores = Arc::clone(&write);
                async move {
                    let name = text(&input, "name")?;
                    let body = text(&input, "text")?;
                    let new = stores.write_note(name, body, Utc::now().timestamp_millis())?;
                    Ok(json!({"note": name.trim(), "new": new, "chars": body.chars().count()}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
        FnTool::new(
            "read_note",
            "Read one of your notes by its name.",
            name_schema(None),
            move |input| {
                let stores = Arc::clone(&read);
                async move {
                    let name = text(&input, "name")?;
                    let note = stores
                        .note(name)?
                        .ok_or_else(|| refuse(format!("there is no note named `{name}`")))?;
                    Ok(json!({"note": note.name, "text": note.text}))
                }
            },
        )
        .effect(ToolEffect::ReadOnly)
        .label(labels::UNTRUSTED_INPUT)
        .shared(),
        FnTool::new(
            "list_notes",
            "List your notes with their sizes. With a query, list only the notes that hold its \
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
                    let notes = stores.notes()?;
                    let Some(query) = input["query"].as_str().filter(|q| !q.trim().is_empty())
                    else {
                        return Ok(json!({"notes": notes.iter().map(|note| json!({
                            "note": note.name,
                            "chars": note.text.chars().count(),
                        })).collect::<Vec<_>>()}));
                    };
                    let docs = notes.iter().map(|note| (note.name.as_str(), note.text.as_str()));
                    let (hits, more) =
                        littleguy_tools::rank(docs, query, littleguy_tools::limit_of(&input))?;
                    Ok(json!({
                        "query": query,
                        "more": more,
                        "matches": hits.iter().map(|hit| json!({
                            "note": hit.name,
                            "matched": hit.matched,
                            "of": hit.of,
                            "snippet": hit.snippet,
                        })).collect::<Vec<_>>(),
                    }))
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
}
