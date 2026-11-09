//! The MCP client against a real child process.
//!
//! This test has its own `main`. With the argument `serve-mcp`, the binary
//! is the MCP test server on stdin and stdout. Otherwise it runs the
//! checks, and each check starts this binary again as its server.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;

use littleguy_core::{McpServer, McpTransport};
use littleguy_tools::mcp::{self, McpError, McpTools, Vars};
use serde_json::json;

const SERVE: &str = "serve-mcp";

/// The variable that names the coverage profile file.
const PROFILE: &str = "LLVM_PROFILE_FILE";

fn server(command: &str, env: &[&str], tools: Option<&[&str]>) -> McpServer {
    McpServer {
        id: String::from("kid"),
        transport: McpTransport::Stdio {
            command: command.to_owned(),
            args: vec![String::from(SERVE)],
            // A coverage run names its profile file here. Without it, the
            // child writes its profile into the crate folder.
            env: env
                .iter()
                .copied()
                .chain([PROFILE])
                .map(str::to_owned)
                .collect(),
        },
        labels: vec![String::from("private_data")],
        effect: autumn_harvest_agent::ToolEffect::ReadOnly,
        tools: tools.map(|t| t.iter().map(|v| (*v).to_owned()).collect()),
        timeout_secs: 10,
    }
}

fn vars() -> Vars {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for name in ["PATH", "HOME", PROFILE] {
        if let Ok(value) = std::env::var(name) {
            map.insert(name.to_owned(), value);
        }
    }
    map.insert(String::from("LG_MCP_PASSED"), String::from("passed"));
    map.insert(String::from("LG_MCP_SECRET"), String::from("secret"));
    Arc::new(move |name: &str| map.get(name).cloned())
}

type Outcome = Result<(), Box<dyn std::error::Error>>;

/// Fails the check with `message` when `ok` is false.
fn check(ok: bool, message: &str) -> Outcome {
    if ok { Ok(()) } else { Err(message.into()) }
}

async fn call(tools: &McpTools, name: &str, input: serde_json::Value) -> Result<String, String> {
    let tool = tools
        .tools
        .iter()
        .find(|t| t.name() == name)
        .ok_or_else(|| format!("no tool {name}"))?;
    let ctx = autumn_harvest_agent::ToolContext {
        run_id: autumn_harvest_agent::RunId::new("run-1"),
        call_id: String::from("call-1"),
        session_id: None,
        step: 0,
        workspace: None,
    };
    let out = tool.execute(input, &ctx).await.map_err(|e| e.to_string())?;
    out.as_str()
        .map(str::to_owned)
        .ok_or_else(|| String::from("the result is not text"))
}

fn this_binary() -> Result<String, Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    Ok(exe.to_str().ok_or("the test path is not UTF-8")?.to_owned())
}

async fn a_child_server_gives_its_tools_and_only_the_named_variables() -> Outcome {
    let exe = this_binary()?;
    let tools = mcp::connect(&[server(&exe, &["LG_MCP_PASSED"], None)], &vars()).await;
    check(tools.failed.is_empty(), "the child server failed")?;
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name()).collect();
    check(
        names
            == [
                "kid__echo",
                "kid__fail",
                "kid__env",
                "kid__structured",
                "kid__picture",
                "kid__count",
                "kid__has_space",
            ],
        "the tool names differ",
    )?;
    check(
        call(&tools, "kid__echo", json!({"text": "hi"})).await? == "hi",
        "echo",
    )?;
    let passed = call(&tools, "kid__env", json!({"name": "LG_MCP_PASSED"})).await?;
    check(
        passed == "passed",
        "a named variable did not reach the child",
    )?;
    let secret = call(&tools, "kid__env", json!({"name": "LG_MCP_SECRET"})).await?;
    check(
        secret == "unset",
        "a variable the config does not name leaked",
    )?;
    let spaced = call(&tools, "kid__has_space", json!({})).await?;
    check(spaced == "spaced", "a renamed tool calls its server name")
}

async fn the_tool_list_keeps_only_the_named_tools() -> Outcome {
    let exe = this_binary()?;
    let kept = server(&exe, &[], Some(&["echo", "gone"]));
    let tools = mcp::connect(&[kept], &vars()).await;
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name()).collect();
    check(names == ["kid__echo"], "the list keeps another tool")?;
    check(
        matches!(tools.skipped.as_slice(), [note] if note.contains("`gone`")),
        "a missing tool is not noted",
    )
}

async fn a_missing_program_gives_no_tools() -> Outcome {
    let tools = mcp::connect(&[server("/nonexistent/littleguy-mcp", &[], None)], &vars()).await;
    check(tools.tools.is_empty(), "a missing program gave tools")?;
    check(
        matches!(tools.failed.as_slice(), [(id, McpError::Spawn(_))] if id == "kid"),
        "a missing program is not a spawn failure",
    )
}

async fn checks() -> Outcome {
    a_child_server_gives_its_tools_and_only_the_named_variables().await?;
    println!("ok: a child server gives its tools and only the named variables");
    the_tool_list_keeps_only_the_named_tools().await?;
    println!("ok: the tool list keeps only the named tools");
    a_missing_program_gives_no_tools().await?;
    println!("ok: a missing program gives no tools");
    Ok(())
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("no runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let outcome = if std::env::args().nth(1).as_deref() == Some(SERVE) {
        runtime
            .block_on(littleguy_tools::testing::serve_stdio())
            .map_err(Into::into)
    } else {
        runtime.block_on(checks())
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("FAILED: {error}");
            ExitCode::FAILURE
        }
    }
}
