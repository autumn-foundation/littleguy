//! The workspace file tools: `read_file`, `write_file`, `list_files` and
//! `delete_file`.
//!
//! Each tool works on the persistent workspace of the agent. The engine
//! loads it before the call and saves a changed one after a call that
//! succeeds, so a crash never keeps half a change. The workspace checks
//! each path: a path is plain and relative, so it never leaves the
//! workspace.

use std::sync::Arc;

use autumn_harvest_agent::{AgentError, FnTool, Tool, ToolContext, ToolEffect, WorkspaceHandle};
use serde_json::{Value, json};

use crate::{clip, refuse, text_field};

/// The most characters that one `read_file` returns.
pub const MAX_READ_CHARS: usize = 100_000;

/// The workspace of a call.
fn workspace(ctx: &ToolContext) -> Result<&WorkspaceHandle, AgentError> {
    ctx.workspace
        .as_ref()
        .ok_or_else(|| refuse("this agent has no workspace"))
}

fn path_schema(extra: Option<(&str, &str)>) -> Value {
    let mut properties = json!({
        "path": {"type": "string", "description": "A relative path, such as notes/todo.md."}
    });
    let mut required = vec!["path"];
    if let Some((name, description)) = extra {
        properties[name] = json!({"type": "string", "description": description});
        required.push(name);
    }
    json!({"type": "object", "properties": properties, "required": required})
}

fn read(input: &Value, ctx: &ToolContext) -> Result<Value, AgentError> {
    let path = text_field(input, "path")?;
    workspace(ctx)?.with(|files| {
        let file = files
            .get(path)
            .ok_or_else(|| refuse(format!("there is no file at `{path}`")))?;
        let text =
            std::str::from_utf8(&file.data).map_err(|_| refuse(format!("`{path}` is not text")))?;
        let (text, truncated) = clip(text, MAX_READ_CHARS);
        Ok(json!({"path": path, "text": text, "truncated": truncated}))
    })
}

fn write(input: &Value, ctx: &ToolContext) -> Result<Value, AgentError> {
    let path = text_field(input, "path")?;
    let text = text_field(input, "text")?;
    workspace(ctx)?.with(|files| files.write(path, text))?;
    Ok(json!({"path": path, "bytes": text.len()}))
}

fn list(ctx: &ToolContext) -> Result<Value, AgentError> {
    let files: Vec<Value> = workspace(ctx)?.with(|files| {
        files
            .files()
            .iter()
            .map(|(path, file)| json!({"path": path, "bytes": file.data.len()}))
            .collect()
    });
    Ok(json!({"files": files}))
}

fn delete(input: &Value, ctx: &ToolContext) -> Result<Value, AgentError> {
    let path = text_field(input, "path")?;
    let deleted = workspace(ctx)?.with(|files| files.remove(path));
    Ok(json!({"path": path, "deleted": deleted}))
}

/// The four file tools of the workspace.
#[must_use]
pub fn file_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        FnTool::with_context(
            "read_file",
            "Read a text file of your workspace.",
            path_schema(None),
            |input, ctx| async move { read(&input, &ctx) },
        )
        .effect(ToolEffect::ReadOnly)
        .uses_workspace()
        .shared(),
        FnTool::with_context(
            "write_file",
            "Write a text file in your workspace. It replaces a file at the same path. \
             The workspace keeps it across conversations.",
            path_schema(Some(("text", "The whole content of the file."))),
            |input, ctx| async move { write(&input, &ctx) },
        )
        .effect(ToolEffect::Internal)
        .uses_workspace()
        .shared(),
        FnTool::with_context(
            "list_files",
            "List the files of your workspace, with their sizes.",
            json!({"type": "object", "properties": {}}),
            |_input, ctx| async move { list(&ctx) },
        )
        .effect(ToolEffect::ReadOnly)
        .uses_workspace()
        .shared(),
        FnTool::with_context(
            "delete_file",
            "Delete a file of your workspace.",
            path_schema(None),
            |input, ctx| async move { delete(&input, &ctx) },
        )
        .effect(ToolEffect::Internal)
        .uses_workspace()
        .shared(),
    ]
}
