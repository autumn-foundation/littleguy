//! The file tools against an in-memory workspace.

use std::sync::Arc;

use autumn_harvest_agent::{ErrorKind, Tool, ToolEffect, Workspace, WorkspaceHandle};
use serde_json::json;

use super::ctx;
use crate::file_tools;

fn tool(name: &str) -> Arc<dyn Tool> {
    file_tools()
        .into_iter()
        .find(|tool| tool.name() == name)
        .unwrap()
}

#[tokio::test]
async fn the_file_tools_write_read_list_and_delete() {
    let handle = WorkspaceHandle::new(Workspace::default());
    let call = ctx(Some(handle.clone()));
    let wrote = tool("write_file")
        .execute(json!({"path": "notes/tea.md", "text": "green"}), &call)
        .await
        .unwrap();
    assert_eq!(wrote, json!({"path": "notes/tea.md", "bytes": 5}));
    let read = tool("read_file")
        .execute(json!({"path": "notes/tea.md"}), &call)
        .await
        .unwrap();
    assert_eq!(read["text"], "green");
    assert_eq!(read["truncated"], false);
    let listed = tool("list_files").execute(json!({}), &call).await.unwrap();
    assert_eq!(
        listed,
        json!({"files": [{"path": "notes/tea.md", "bytes": 5}]})
    );
    let deleted = tool("delete_file")
        .execute(json!({"path": "notes/tea.md"}), &call)
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], true);
    let again = tool("delete_file")
        .execute(json!({"path": "notes/tea.md"}), &call)
        .await
        .unwrap();
    assert_eq!(again["deleted"], false);
    assert!(handle.snapshot().files().is_empty());
}

#[tokio::test]
async fn the_file_tools_refuse_bad_paths_and_missing_files() {
    let handle = WorkspaceHandle::new(Workspace::default());
    handle
        .with(|files| files.write("blob.bin", vec![0xff, 0xfe]))
        .unwrap();
    let call = ctx(Some(handle));
    for path in ["../escape", "/etc/passwd", "a/../../b"] {
        let error = tool("write_file")
            .execute(json!({"path": path, "text": "x"}), &call)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool, "{path}");
    }
    let missing = tool("read_file")
        .execute(json!({"path": "nope.md"}), &call)
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("no file at `nope.md`"));
    let binary = tool("read_file")
        .execute(json!({"path": "blob.bin"}), &call)
        .await
        .unwrap_err();
    assert!(binary.to_string().contains("not text"));
    let no_text = tool("write_file")
        .execute(json!({"path": "a.md"}), &call)
        .await
        .unwrap_err();
    assert!(no_text.to_string().contains("`text`"));
}

#[tokio::test]
async fn the_file_tools_need_a_workspace() {
    for tool in file_tools() {
        assert!(tool.uses_workspace());
        let error = tool
            .execute(json!({"path": "a.md", "text": "x"}), &ctx(None))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no workspace"),
            "{}",
            tool.name()
        );
    }
    let effects: Vec<ToolEffect> = file_tools().iter().map(|tool| tool.effect()).collect();
    assert_eq!(
        effects,
        [
            ToolEffect::ReadOnly,
            ToolEffect::Internal,
            ToolEffect::ReadOnly,
            ToolEffect::Internal
        ]
    );
}

#[tokio::test]
async fn a_long_file_is_cut_when_read() {
    let handle = WorkspaceHandle::new(Workspace::default());
    let long = "x".repeat(crate::MAX_READ_CHARS + 5);
    handle.with(|files| files.write("long.txt", long)).unwrap();
    let read = tool("read_file")
        .execute(json!({"path": "long.txt"}), &ctx(Some(handle)))
        .await
        .unwrap();
    assert_eq!(read["text"].as_str().unwrap().len(), crate::MAX_READ_CHARS);
    assert_eq!(read["truncated"], true);
}

#[cfg(feature = "search-notes")]
#[tokio::test]
async fn search_notes_finds_a_forgotten_note_by_its_words() {
    let handle = WorkspaceHandle::new(Workspace::default());
    handle
        .with(|files| {
            files.write("notes/drinks.md", "Mark likes GREEN tea, never coffee.")?;
            files.write("notes/tea-shops.md", "A shop on 5th sells tea.")?;
            files.write("notes/todo.md", "Fix the docs links.")?;
            files.write("blob.bin", vec![0xff, 0xfe, b't', b'e', b'a'])
        })
        .unwrap();
    let call = ctx(Some(handle.clone()));
    let search = crate::search_notes_tool();
    assert_eq!(search.effect(), ToolEffect::ReadOnly, "a tick may search");

    let found = search
        .execute(json!({"query": "green tea"}), &call)
        .await
        .unwrap();
    assert_eq!(found["searched"], 3, "a file that is not text is skipped");
    let matches = found["matches"].as_array().unwrap();
    let paths: Vec<&str> = matches
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    // The file with both words comes first, then the one with `tea` twice:
    // in its path and its text.
    assert_eq!(paths, ["notes/drinks.md", "notes/tea-shops.md"]);
    assert_eq!(matches[0]["matched"], 2);
    assert_eq!(matches[0]["of"], 2);
    assert!(
        matches[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("GREEN tea")
    );
    assert_eq!(found["more"], false);

    let one = search
        .execute(json!({"query": "tea", "limit": 1}), &call)
        .await
        .unwrap();
    assert_eq!(one["matches"].as_array().unwrap().len(), 1);
    assert_eq!(one["more"], true);

    let none = search
        .execute(json!({"query": "biscuits"}), &call)
        .await
        .unwrap();
    assert_eq!(none["matches"], json!([]));
    // Searching changes nothing.
    assert_eq!(handle.snapshot().files().len(), 4);
}

#[cfg(feature = "search-notes")]
#[tokio::test]
async fn search_notes_refuses_an_empty_query_and_no_workspace() {
    let search = crate::search_notes_tool();
    let call = ctx(Some(WorkspaceHandle::new(Workspace::default())));
    let empty = search
        .execute(json!({"query": "a ?"}), &call)
        .await
        .unwrap_err();
    assert_eq!(empty.kind(), ErrorKind::Tool);
    let alone = search
        .execute(json!({"query": "tea"}), &ctx(None))
        .await
        .unwrap_err();
    assert!(alone.to_string().contains("no workspace"));
}

/// A workspace with one file of `text` at `long.txt`, and `read_file`.
fn reader(text: &str) -> (WorkspaceHandle, Arc<dyn Tool>) {
    let handle = WorkspaceHandle::new(Workspace::default());
    handle.with(|files| files.write("long.txt", text)).unwrap();
    (handle, tool("read_file"))
}

#[tokio::test]
async fn a_read_with_no_offset_starts_at_the_start() {
    let (handle, read_file) = reader("héllo world");
    let read = read_file
        .execute(json!({"path": "long.txt"}), &ctx(Some(handle)))
        .await
        .unwrap();
    assert_eq!(
        read,
        json!({"path": "long.txt", "offset": 0, "text": "héllo world",
               "truncated": false, "length": 11})
    );
}

#[tokio::test]
async fn an_offset_reads_a_window_in_the_middle_of_a_long_file() {
    // Each line holds its number, so a window shows where it starts.
    let text = (0..30_000).fold(String::new(), |mut text, n| {
        use std::fmt::Write as _;
        let _ = writeln!(text, "{n:06}");
        text
    });
    let (handle, read_file) = reader(&text);
    let call = ctx(Some(handle));
    let start = 7 * 20_000;
    let read = read_file
        .execute(json!({"path": "long.txt", "offset": start}), &call)
        .await
        .unwrap();
    let window = read["text"].as_str().unwrap();
    assert!(window.starts_with("020000\n"), "{}", &window[..14]);
    assert_eq!(window.chars().count(), text.chars().count() - start);
    assert_eq!(read["offset"], start);
    assert_eq!(read["truncated"], false);
    assert_eq!(read["length"], 7 * 30_000);
    // A window that ends before the file does says so.
    let read = read_file
        .execute(json!({"path": "long.txt", "offset": 7}), &call)
        .await
        .unwrap();
    assert!(read["text"].as_str().unwrap().starts_with("000001\n"));
    assert_eq!(
        read["text"].as_str().unwrap().chars().count(),
        crate::MAX_READ_CHARS
    );
    assert_eq!(read["truncated"], true);
}

#[tokio::test]
async fn an_offset_past_the_end_reads_nothing_and_gives_the_length() {
    let (handle, read_file) = reader("héllo");
    let call = ctx(Some(handle));
    for offset in [5, 6, 1_000_000] {
        let read = read_file
            .execute(json!({"path": "long.txt", "offset": offset}), &call)
            .await
            .unwrap();
        assert_eq!(read["text"], "");
        assert_eq!(read["truncated"], false);
        assert_eq!(read["length"], 5);
    }
    let error = read_file
        .execute(json!({"path": "long.txt", "offset": -1}), &call)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Tool);
}
