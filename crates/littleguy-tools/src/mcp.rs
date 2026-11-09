//! MCP servers as tools.
//!
//! Each `[[mcp.server]]` of the config becomes one [`Connection`]. At open,
//! [`connect`] lists the tools of each server. Each tool becomes one
//! [`McpTool`], named `<server>__<tool>`. The server config sets the flow
//! labels and the effect of each tool. The server cannot set them, because
//! a server must not choose its own trust.
//!
//! A child process gets only `PATH`, `HOME` and the variables that its
//! config names. So a server never sees the Discord token or a model key.
//!
//! A call that fails on a closed connection is not sent again. The server
//! may have run it, and a second send could repeat a side effect. The next
//! call opens a new connection.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use autumn_harvest_agent::{AgentError, BoxFuture, Tool, ToolContext, ToolEffect};
use littleguy_core::{McpServer, McpTransport};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{RoleClient, RunningService, ServiceError, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use thiserror::Error;

use crate::{clip, refuse, tls};

/// The text between the server id and the tool name.
pub const SEPARATOR: &str = "__";

/// The longest tool name that a model provider takes.
pub const MAX_TOOL_NAME: usize = 64;

/// The most characters of one result that the model reads.
pub const MAX_RESULT_CHARS: usize = 20_000;

/// The most characters of one tool description.
pub const MAX_DESCRIPTION_CHARS: usize = 1_024;

/// How long a connect and the tool list may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The variables that every child process gets.
pub const BASE_ENV: [&str; 2] = ["PATH", "HOME"];

/// An open MCP client.
pub type Client = RunningService<RoleClient, ()>;

/// Reads one variable of the daemon environment.
pub type Vars = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Opens a new client. A test opens one over an in-memory pipe.
pub type Connector = Box<dyn Fn() -> BoxFuture<'static, Result<Client, McpError>> + Send + Sync>;

/// Why a server gave no tools.
#[derive(Debug, Error)]
pub enum McpError {
    /// The child process did not start.
    #[error("the server process did not start: {0}")]
    Spawn(String),
    /// The variable of the bearer token is not set.
    #[error("the variable {0} of the bearer token is not set")]
    Bearer(String),
    /// The MCP handshake failed.
    #[error("the server did not answer the MCP handshake: {0}")]
    Handshake(String),
    /// The tool list failed.
    #[error("the server did not list its tools: {0}")]
    List(String),
    /// The connect or the list took too long.
    #[error("the server did not answer within {0:?}")]
    Timeout(Duration),
}

/// The tools of the servers that answered, and why each other failed.
#[derive(Debug, Default)]
pub struct McpTools {
    /// The tools, in config order.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Each server that gave no tools, and why.
    pub failed: Vec<(String, McpError)>,
    /// Each server tool that was left out, and why.
    pub skipped: Vec<String>,
}

/// Opens each server in `servers` and lists its tools. A server that fails
/// gives no tools and goes into [`McpTools::failed`]. `vars` reads the
/// variables that a server config names.
pub async fn connect(servers: &[McpServer], vars: &Vars) -> McpTools {
    let mut all = McpTools::default();
    let mut names = BTreeSet::new();
    for server in servers {
        let connection = Connection::new(server.clone(), connector(server, vars));
        match connection.tools(&mut names).await {
            Ok((tools, skipped)) => {
                all.tools.extend(tools);
                all.skipped.extend(skipped);
            }
            Err(error) => all.failed.push((server.id.clone(), error)),
        }
    }
    all
}

/// The connector of `server`.
fn connector(server: &McpServer, vars: &Vars) -> Connector {
    let transport = server.transport.clone();
    let vars = Arc::clone(vars);
    Box::new(move || {
        let transport = transport.clone();
        let vars = Arc::clone(&vars);
        Box::pin(async move { open(&transport, &vars).await })
    })
}

/// Opens one client on `transport`.
async fn open(transport: &McpTransport, vars: &Vars) -> Result<Client, McpError> {
    let handshake = |e: rmcp::service::ClientInitializeError| McpError::Handshake(e.to_string());
    match transport {
        McpTransport::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).env_clear();
            for name in BASE_ENV
                .iter()
                .copied()
                .chain(env.iter().map(String::as_str))
            {
                if let Some(value) = vars(name) {
                    cmd.env(name, value);
                }
            }
            let (child, _) = TokioChildProcess::builder(cmd)
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .map_err(|e| McpError::Spawn(e.to_string()))?;
            ().serve(child).await.map_err(handshake)
        }
        McpTransport::Http { url, bearer_env } => {
            tls();
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(name) = bearer_env {
                let token = vars(name).ok_or_else(|| McpError::Bearer(name.clone()))?;
                config = config.auth_header(token);
            }
            let transport = StreamableHttpClientTransport::from_config(config);
            ().serve(transport).await.map_err(handshake)
        }
    }
}

/// The model name of `tool` on `server`, or `None` when it does not fit.
///
/// Each character outside `A-Z a-z 0-9 _ -` becomes `_`, because model
/// providers take only those. A name over [`MAX_TOOL_NAME`] is refused, not
/// cut, because a cut can make two tools share one name.
#[must_use]
pub fn tool_name(server: &str, tool: &str) -> Option<String> {
    let plain: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{server}{SEPARATOR}{plain}");
    (!tool.is_empty() && name.len() <= MAX_TOOL_NAME).then_some(name)
}

/// The text that the model reads for `result`.
///
/// Structured content wins, because it holds the full answer. Otherwise
/// the text blocks join with new lines. Each other block becomes a short
/// note. The text is cut at [`MAX_RESULT_CHARS`].
#[must_use]
pub fn result_text(result: &CallToolResult) -> String {
    let text = result.structured_content.as_ref().map_or_else(
        || {
            result
                .content
                .iter()
                .map(block_text)
                .collect::<Vec<_>>()
                .join("\n")
        },
        Value::to_string,
    );
    let (mut text, cut) = clip(&text, MAX_RESULT_CHARS);
    if cut {
        text.push_str("\n[the result was cut]");
    }
    text
}

/// The text of one content block.
fn block_text(block: &ContentBlock) -> String {
    if let Some(text) = block.as_text() {
        return text.text.clone();
    }
    let kind = serde_json::to_value(block)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| String::from("other"));
    format!("[a {kind} block is not shown]")
}

/// One server, and its client while the client is open.
pub struct Connection {
    server: McpServer,
    connector: Connector,
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("server", &self.server.id)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// A connection to `server` that opens with `connector`. It opens no
    /// client yet.
    #[must_use]
    pub fn new(server: McpServer, connector: Connector) -> Arc<Self> {
        Arc::new(Self {
            server,
            connector,
            client: tokio::sync::Mutex::new(None),
        })
    }

    /// The open client, or a new one.
    async fn client(&self) -> Result<Arc<Client>, McpError> {
        // The lock is held across the connect, so two calls never open two
        // clients at once.
        let mut held = self.client.lock().await;
        let open = held
            .as_ref()
            .filter(|client| !client.is_transport_closed())
            .map(Arc::clone);
        if let Some(client) = open {
            drop(held);
            return Ok(client);
        }
        let client = tokio::time::timeout(CONNECT_TIMEOUT, (self.connector)())
            .await
            .map_err(|_| McpError::Timeout(CONNECT_TIMEOUT))??;
        let client = Arc::new(client);
        *held = Some(Arc::clone(&client));
        drop(held);
        Ok(client)
    }

    /// Drops the client, so the next call opens a new one.
    async fn forget(&self) {
        self.client.lock().await.take();
    }

    /// Lists the tools of the server. `names` holds the names taken so far.
    /// Returns the tools, and a note for each tool that was left out.
    ///
    /// # Errors
    ///
    /// Returns an error when the server does not open or does not list.
    pub async fn tools(
        self: &Arc<Self>,
        names: &mut BTreeSet<String>,
    ) -> Result<(Vec<Arc<dyn Tool>>, Vec<String>), McpError> {
        let client = self.client().await?;
        let listed = tokio::time::timeout(CONNECT_TIMEOUT, client.list_all_tools())
            .await
            .map_err(|_| McpError::Timeout(CONNECT_TIMEOUT))?
            .map_err(|e| McpError::List(e.to_string()))?;
        let id = &self.server.id;
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut skipped = Vec::new();
        let mut seen = BTreeSet::new();
        for tool in listed {
            let mcp_name = tool.name.to_string();
            seen.insert(mcp_name.clone());
            let kept = self
                .server
                .tools
                .as_ref()
                .is_none_or(|k| k.contains(&mcp_name));
            if !kept {
                continue;
            }
            let Some(name) = tool_name(id, &mcp_name) else {
                skipped.push(format!("{id}: the tool `{mcp_name}` has no name that fits"));
                continue;
            };
            if !names.insert(name.clone()) {
                skipped.push(format!(
                    "{id}: the tool `{mcp_name}` repeats the name {name}"
                ));
                continue;
            }
            let about = tool.description.as_deref().unwrap_or("An MCP tool.");
            let (about, _) = clip(&format!("[{id}] {about}"), MAX_DESCRIPTION_CHARS);
            tools.push(Arc::new(McpTool {
                name,
                mcp_name,
                description: about,
                schema: Value::Object((*tool.input_schema).clone()),
                connection: Arc::clone(self),
            }));
        }
        for wanted in self.server.tools.iter().flatten() {
            if !seen.contains(wanted) {
                skipped.push(format!("{id}: the server has no tool `{wanted}`"));
            }
        }
        Ok((tools, skipped))
    }

    /// Calls the tool `name` with `arguments`.
    async fn call(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, Value>>,
    ) -> Result<CallToolResult, AgentError> {
        let id = &self.server.id;
        let client = self
            .client()
            .await
            .map_err(|e| refuse(format!("the MCP server {id} is not open: {e}")))?;
        let mut params = CallToolRequestParams::new(name.to_owned());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let bound = Duration::from_secs(self.server.timeout_secs);
        let Ok(answer) = tokio::time::timeout(bound, client.call_tool(params)).await else {
            return Err(refuse(format!(
                "the MCP server {id} did not answer within {bound:?}"
            )));
        };
        match answer {
            Ok(result) => Ok(result),
            Err(error @ (ServiceError::TransportClosed | ServiceError::TransportSend(_))) => {
                self.forget().await;
                Err(refuse(format!(
                    "the connection to the MCP server {id} closed ({error}). \
                     The call may have run. The next call opens a new connection"
                )))
            }
            Err(error) => Err(refuse(format!(
                "the MCP server {id} refused the call: {error}"
            ))),
        }
    }
}

/// One tool of an MCP server.
pub struct McpTool {
    name: String,
    mcp_name: String,
    description: String,
    schema: Value,
    connection: Arc<Connection>,
}

impl fmt::Debug for McpTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl McpTool {
    /// The name of the tool on its server.
    #[must_use]
    pub fn mcp_name(&self) -> &str {
        &self.mcp_name
    }

    /// Runs the tool on `input`, a JSON object.
    ///
    /// # Errors
    ///
    /// Returns a tool error when the input is not an object, the call
    /// fails, or the server answers with an error result.
    pub async fn run(&self, input: Value) -> Result<Value, AgentError> {
        let arguments = match input {
            Value::Object(map) => Some(map),
            Value::Null => None,
            _ => return Err(refuse("the input of an MCP tool is a JSON object")),
        };
        let result = self.connection.call(&self.mcp_name, arguments).await?;
        let text = result_text(&result);
        if result.is_error == Some(true) {
            return Err(refuse(format!("the tool {} failed: {text}", self.name)));
        }
        Ok(Value::String(text))
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn execute<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<Value, AgentError>> {
        Box::pin(self.run(input))
    }

    fn effect(&self) -> ToolEffect {
        self.connection.server.effect
    }

    fn labels(&self) -> &[String] {
        &self.connection.server.labels
    }
}

#[cfg(test)]
mod tests;
