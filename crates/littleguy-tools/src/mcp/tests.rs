use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use autumn_harvest_agent::{ErrorKind, RunId};
use rmcp::model::ContentBlock;
use serde_json::json;

use super::*;
use crate::testing::{self, TestServer};

fn ctx() -> ToolContext {
    ToolContext {
        run_id: RunId::new("run-1"),
        call_id: String::from("call-1"),
        session_id: None,
        step: 0,
        workspace: None,
    }
}

fn http(url: &str, bearer_env: Option<&str>) -> McpServer {
    McpServer {
        id: String::from("web"),
        transport: McpTransport::Http {
            url: url.to_owned(),
            bearer_env: bearer_env.map(str::to_owned),
        },
        labels: vec![String::from("untrusted_input")],
        effect: ToolEffect::Write,
        tools: None,
        timeout_secs: 5,
    }
}

fn vars(pairs: &[(&str, &str)]) -> Vars {
    let map: std::collections::BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Arc::new(move |name: &str| map.get(name).cloned())
}

fn find<'a>(tools: &'a McpTools, name: &str) -> &'a Arc<dyn Tool> {
    tools
        .tools
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

/// A connection to an in-memory [`TestServer`]. Each open starts a new
/// server and counts itself in `opens`. `servers` holds the server tasks,
/// so a test can stop them.
fn piped(
    server: McpServer,
    opens: Arc<AtomicUsize>,
    servers: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    calls: Arc<AtomicU64>,
) -> Arc<Connection> {
    let connector: Connector = Box::new(move || {
        let opens = Arc::clone(&opens);
        let servers = Arc::clone(&servers);
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            opens.fetch_add(1, Ordering::SeqCst);
            let (client_side, server_side) = tokio::io::duplex(64 * 1024);
            let task = tokio::spawn(async move {
                if let Ok(running) = TestServer::counting(calls).serve(server_side).await {
                    let _ = running.waiting().await;
                }
            });
            servers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(task);
            ().serve(client_side)
                .await
                .map_err(|e| McpError::Handshake(e.to_string()))
        })
    });
    Connection::new(server, connector)
}

#[test]
fn tool_names_are_plain_and_bounded() {
    assert_eq!(
        tool_name("gh", "list_issues").as_deref(),
        Some("gh__list_issues")
    );
    assert_eq!(
        tool_name("gh", "issues.list v2").as_deref(),
        Some("gh__issues_list_v2")
    );
    assert_eq!(tool_name("gh", "héllo").as_deref(), Some("gh__h_llo"));
    assert_eq!(tool_name("gh", ""), None);
    let long = "x".repeat(MAX_TOOL_NAME);
    assert_eq!(tool_name("gh", &long), None);
    let fits = "x".repeat(MAX_TOOL_NAME - 4);
    assert_eq!(tool_name("gh", &fits).map(|n| n.len()), Some(MAX_TOOL_NAME));
}

#[test]
fn results_become_text() {
    let text = CallToolResult::success(vec![ContentBlock::text("a"), ContentBlock::text("b")]);
    assert_eq!(result_text(&text), "a\nb");
    let structured = CallToolResult::structured(json!({"n": 1}));
    assert_eq!(result_text(&structured), "{\"n\":1}");
    let image = CallToolResult::success(vec![ContentBlock::image("AAAA", "image/png")]);
    assert_eq!(result_text(&image), "[a image block is not shown]");
    let long = CallToolResult::success(vec![ContentBlock::text("y".repeat(MAX_RESULT_CHARS + 5))]);
    let cut = result_text(&long);
    assert!(cut.ends_with("[the result was cut]"));
    assert!(cut.chars().count() < MAX_RESULT_CHARS + 30);
}

#[tokio::test]
async fn an_http_server_gives_tools_with_the_config_labels() {
    let server = testing::serve_http(None).await.unwrap();
    let tools = connect(&[http(&server.url, None)], &vars(&[])).await;
    assert!(tools.failed.is_empty(), "{:?}", tools.failed);
    assert_eq!(tools.tools.len(), testing::TOOLS.len());
    let echo = find(&tools, "web__echo");
    assert_eq!(echo.effect(), ToolEffect::Write);
    assert_eq!(echo.labels(), ["untrusted_input"]);
    assert!(echo.description().starts_with("[web] "));
    assert_eq!(echo.input_schema()["required"], json!(["text"]));
    let out = echo
        .execute(json!({"text": "hello"}), &ctx())
        .await
        .unwrap();
    assert_eq!(out, json!("hello"));
    let picture = find(&tools, "web__picture")
        .execute(json!({}), &ctx())
        .await
        .unwrap();
    assert_eq!(picture, json!("a picture:\n[a image block is not shown]"));
    let structured = find(&tools, "web__structured")
        .execute(Value::Null, &ctx())
        .await
        .unwrap();
    assert_eq!(structured, json!("{\"n\":1}"));
}

#[tokio::test]
async fn a_failed_call_is_a_tool_error() {
    let server = testing::serve_http(None).await.unwrap();
    let tools = connect(&[http(&server.url, None)], &vars(&[])).await;
    let failed = find(&tools, "web__fail")
        .execute(json!({}), &ctx())
        .await
        .unwrap_err();
    assert_eq!(failed.kind(), ErrorKind::Tool);
    assert!(failed.to_string().contains("the test tool failed"));
    let refused = find(&tools, "web__echo")
        .execute(json!({}), &ctx())
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("refused the call"));
    let bad = find(&tools, "web__echo")
        .execute(json!([1]), &ctx())
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("JSON object"));
}

#[tokio::test]
async fn the_bearer_token_comes_from_its_variable() {
    let server = testing::serve_http(Some(String::from("s3cret")))
        .await
        .unwrap();
    let good = connect(
        &[http(&server.url, Some("LG_T"))],
        &vars(&[("LG_T", "s3cret")]),
    )
    .await;
    assert!(good.failed.is_empty(), "{:?}", good.failed);
    let wrong = connect(
        &[http(&server.url, Some("LG_T"))],
        &vars(&[("LG_T", "nope")]),
    )
    .await;
    assert!(matches!(
        wrong.failed.as_slice(),
        [(_, McpError::Handshake(_))]
    ));
    let none = connect(&[http(&server.url, None)], &vars(&[])).await;
    assert!(matches!(
        none.failed.as_slice(),
        [(_, McpError::Handshake(_))]
    ));
    let unset = connect(&[http(&server.url, Some("LG_T"))], &vars(&[])).await;
    match unset.failed.as_slice() {
        [(_, McpError::Bearer(name))] => assert_eq!(name, "LG_T"),
        other => panic!("{other:?}"),
    }
    // The token never shows in an error.
    assert!(!format!("{:?}", wrong.failed).contains("s3cret"));
}

#[tokio::test]
async fn two_servers_never_share_a_tool_name() {
    let server = testing::serve_http(None).await.unwrap();
    let mut second = http(&server.url, None);
    second.tools = Some(vec![String::from("echo")]);
    let tools = connect(&[http(&server.url, None), second], &vars(&[])).await;
    assert_eq!(tools.tools.len(), testing::TOOLS.len());
    assert_eq!(tools.skipped.len(), 1);
    assert!(tools.skipped[0].contains("repeats the name web__echo"));
}

#[tokio::test]
async fn a_closed_connection_is_not_sent_again_and_the_next_call_reopens() {
    let opens = Arc::new(AtomicUsize::new(0));
    let servers = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicU64::new(0));
    let mut config = http("http://127.0.0.1:1/mcp", None);
    config.id = String::from("pipe");
    let connection = piped(
        config,
        Arc::clone(&opens),
        Arc::clone(&servers),
        Arc::clone(&calls),
    );
    let (tools, _) = connection.tools(&mut BTreeSet::new()).await.unwrap();
    let count = tools.iter().find(|t| t.name() == "pipe__count").unwrap();
    assert_eq!(count.execute(json!({}), &ctx()).await.unwrap(), json!("1"));
    for task in servers.lock().unwrap().drain(..) {
        task.abort();
    }
    // The abort ends the server side. A call either sees the closed
    // connection and fails, or finds it closed first and opens a new one.
    // Each failed call must reach no server, so no call runs twice.
    let mut answered = 1;
    for _ in 0..50 {
        match count.execute(json!({}), &ctx()).await {
            Ok(out) => {
                answered += 1;
                assert_eq!(out, json!(calls.load(Ordering::SeqCst).to_string()));
                if opens.load(Ordering::SeqCst) >= 2 {
                    break;
                }
            }
            Err(error) => {
                let text = error.to_string();
                assert!(
                    text.contains("closed") || text.contains("not open"),
                    "{text}"
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        opens.load(Ordering::SeqCst) >= 2,
        "the connection never reopened"
    );
    assert_eq!(calls.load(Ordering::SeqCst), answered);
}

#[tokio::test(start_paused = true)]
async fn a_slow_server_times_out() {
    let connector: Connector = Box::new(|| {
        Box::pin(async {
            std::future::pending::<()>().await;
            Err(McpError::Spawn(String::from("never")))
        })
    });
    let mut config = http("http://127.0.0.1:1/mcp", None);
    config.timeout_secs = 1;
    let connection = Connection::new(config, connector);
    let listed = connection.tools(&mut BTreeSet::new()).await;
    assert!(matches!(listed, Err(McpError::Timeout(_))));
    assert!(format!("{connection:?}").contains("web"));
}
