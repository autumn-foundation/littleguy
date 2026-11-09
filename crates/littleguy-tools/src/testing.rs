//! An MCP server for tests. It needs the `testing` feature.
//!
//! [`TestServer`] has these tools:
//!
//! - `echo` answers with its `text` argument.
//! - `fail` answers with an error result.
//! - `env` answers with the value of the variable `name`, or `unset`.
//! - `structured` answers with structured content.
//! - `picture` answers with one text block and one image block.
//! - `count` answers with the number of `count` calls so far.
//! - `has space` has a name that a model provider does not take.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::response::IntoResponse;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use serde_json::{Value, json};

/// The tools of [`TestServer`], by name.
pub const TOOLS: [&str; 7] = [
    "echo",
    "fail",
    "env",
    "structured",
    "picture",
    "count",
    "has space",
];

/// The MCP server of the tests.
#[derive(Debug, Clone, Default)]
pub struct TestServer {
    count: Arc<AtomicU64>,
}

impl TestServer {
    /// A server whose `count` tool shares `count`.
    #[must_use]
    pub const fn counting(count: Arc<AtomicU64>) -> Self {
        Self { count }
    }
}

/// The input schema of a tool with one string argument `field`, or none.
fn schema(field: Option<&str>) -> Arc<serde_json::Map<String, Value>> {
    let value = field.map_or_else(
        || json!({ "type": "object", "properties": {} }),
        |field| {
            json!({
                "type": "object",
                "properties": { field: { "type": "string" } },
                "required": [field],
            })
        },
    );
    match value {
        Value::Object(map) => Arc::new(map),
        _ => Arc::default(),
    }
}

/// The string argument `field` of `request`.
fn argument(request: &CallToolRequestParams, field: &str) -> Result<String, ErrorData> {
    request
        .arguments
        .as_ref()
        .and_then(|args| args.get(field))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ErrorData::invalid_params(format!("`{field}` is missing"), None))
}

impl TestServer {
    fn answer(&self, request: &CallToolRequestParams) -> Result<CallToolResult, ErrorData> {
        let text = |t: String| Ok(CallToolResult::success(vec![ContentBlock::text(t)]));
        match request.name.as_ref() {
            "echo" => text(argument(request, "text")?),
            "fail" => Ok(CallToolResult::error(vec![ContentBlock::text(
                "the test tool failed",
            )])),
            "env" => {
                let name = argument(request, "name")?;
                text(std::env::var(name).unwrap_or_else(|_| String::from("unset")))
            }
            "structured" => Ok(CallToolResult::structured(json!({ "n": 1 }))),
            "picture" => Ok(CallToolResult::success(vec![
                ContentBlock::text("a picture:"),
                ContentBlock::image("AAAA", "image/png"),
            ])),
            "count" => {
                let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
                text(n.to_string())
            }
            "has space" => text(String::from("spaced")),
            other => Err(ErrorData::invalid_params(
                format!("no tool `{other}`"),
                None,
            )),
        }
    }
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        let field = |name: &str| match name {
            "echo" => Some("text"),
            "env" => Some("name"),
            _ => None,
        };
        let tools = TOOLS
            .iter()
            .map(|name| Tool::new(*name, format!("The test tool {name}."), schema(field(name))))
            .collect();
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send + '_ {
        std::future::ready(self.answer(&request).map(CallToolResponse::from))
    }
}

/// Serves [`TestServer`] on stdin and stdout until the client closes.
///
/// # Errors
///
/// Returns an error when the handshake fails.
pub async fn serve_stdio() -> Result<(), std::io::Error> {
    let running = TestServer::default()
        .serve(rmcp::transport::stdio())
        .await
        .map_err(std::io::Error::other)?;
    running.waiting().await.map_err(std::io::Error::other)?;
    Ok(())
}

/// A [`TestServer`] that answers streamable HTTP on a loopback port.
#[derive(Debug)]
pub struct HttpServer {
    /// The URL of the MCP endpoint.
    pub url: String,
    /// The `count` calls that reached the server.
    pub count: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts a [`TestServer`] over HTTP. With `bearer`, each request without
/// `Authorization: Bearer <bearer>` gets 401.
///
/// # Errors
///
/// Returns an error when no loopback port binds.
pub async fn serve_http(bearer: Option<String>) -> Result<HttpServer, std::io::Error> {
    let count = Arc::new(AtomicU64::new(0));
    let shared = Arc::clone(&count);
    let service = StreamableHttpService::new(
        move || Ok(TestServer::counting(Arc::clone(&shared))),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_sse_keep_alive(None),
    );
    let expected = bearer.map(|token| format!("Bearer {token}"));
    let guard = move |request: axum::extract::Request, next: axum::middleware::Next| {
        let expected = expected.clone();
        async move {
            let sent = request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok());
            match expected {
                Some(expected) if sent != Some(expected.as_str()) => {
                    axum::http::StatusCode::UNAUTHORIZED.into_response()
                }
                _ => next.run(request).await,
            }
        }
    };
    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(guard));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(HttpServer {
        url: format!("http://{addr}/mcp"),
        count,
        task,
    })
}
