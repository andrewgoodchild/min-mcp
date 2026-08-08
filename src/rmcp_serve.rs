//! Server-facing stdio transport via the official MCP SDK (`rmcp`).
//!
//! min-mcp's `Surface` stays the single source of truth for the tool catalog
//! and dispatch; this module only puts it *behind* rmcp's `ServerHandler`, so
//! the agent-facing wire protocol is the SDK's (spec-tracked) rather than our
//! hand-rolled JSON-RPC. The upstream clients, the OpenAPI spec executor, and
//! the whole minify/overlay/projection surface are unchanged — rmcp has no
//! concept of relaying arbitrary upstream tools, so that half remains ours.
//!
//! We convert at the boundary: `Surface::list_tools()` / `Surface::call()`
//! speak `serde_json::Value` (MCP-shaped), which maps cleanly onto rmcp's typed
//! `Tool` / `CallToolResult` (content blocks deserialize straight from our
//! already-MCP-shaped JSON, so images/audio survive, not just text).

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

use crate::surface::Surface;

/// rmcp `ServerHandler` wrapping a shared `Surface`. The handler methods take
/// `&self`, but dispatch mutates the surface (usage priors, logging), so it's
/// behind a `tokio::Mutex`.
#[derive(Clone)]
pub struct MinMcpServer {
    surface: Arc<Mutex<Surface>>,
}

impl MinMcpServer {
    pub fn new(surface: Surface) -> Self {
        Self { surface: Arc::new(Mutex::new(surface)) }
    }
}

impl ServerHandler for MinMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("min-mcp", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Minified MCP surface. Use search_tools to find a tool, get_tool_details for its \
             schema, then call_tool to run it."
                .to_string(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let listing = self.surface.lock().await.list_tools();
        let tools = listing
            .get("tools")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(json_to_tool).collect())
            .unwrap_or_default();
        // Single-page: the surface returns the whole (already minified) catalog.
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let args = request.arguments.map(Value::Object).unwrap_or_else(|| json!({}));
        let result = {
            let mut surface = self.surface.lock().await;
            surface.call(&name, args).await
        };
        // A genuine upstream/transport failure is a protocol error; the surface
        // already returns client-side/tool errors as isError results (Ok).
        let result = result.map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(value_to_call_result(&result).into())
    }
}

/// One `{name, description, inputSchema}` surface tool → rmcp `Tool`.
fn json_to_tool(v: &Value) -> Option<Tool> {
    let name = v.get("name").and_then(Value::as_str)?.to_string();
    let description = v.get("description").and_then(Value::as_str).unwrap_or("").to_string();
    let schema = v.get("inputSchema").and_then(Value::as_object).cloned().unwrap_or_default();
    Some(Tool::new(name, description, Arc::new(schema)))
}

/// A surface tool result `{content:[...], isError, structuredContent?}` → rmcp
/// `CallToolResult`. Each content block deserializes straight from its MCP JSON
/// (text/image/audio/resource all round-trip); a block that somehow doesn't is
/// degraded to its text, never dropped.
fn value_to_call_result(result: &Value) -> CallToolResult {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let content: Vec<ContentBlock> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| {
                    serde_json::from_value::<ContentBlock>(b.clone()).unwrap_or_else(|_| {
                        ContentBlock::text(b.get("text").and_then(Value::as_str).unwrap_or("").to_string())
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out =
        if is_error { CallToolResult::error(content) } else { CallToolResult::success(content) };
    out.structured_content = result.get("structuredContent").cloned();
    out
}

/// Serve the minified surface over stdio using rmcp's transport, running until
/// the client disconnects.
pub async fn serve_stdio(surface: Surface) -> Result<()> {
    let running = MinMcpServer::new(surface).serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// Serve the minified surface over Streamable HTTP using rmcp's
/// `StreamableHttpService` (JSON-RPC over POST, SSE for streamed replies,
/// session ids, Host-header validation for DNS-rebinding defence), driven on a
/// TCP listener by hyper. One shared `Surface` backs every session (the
/// per-session factory just clones the handle).
pub async fn serve_http(surface: Surface, addr: &str) -> Result<()> {
    use anyhow::Context;
    use hyper::server::conn::http1;
    use hyper_util::rt::TokioIo;
    use hyper_util::service::TowerToHyperService;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
    use tokio::net::TcpListener;

    let server = MinMcpServer::new(surface);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let listener = TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    let bound = listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| addr.to_string());
    eprintln!("min-mcp: Streamable HTTP (rmcp) listening on http://{bound}/");

    loop {
        let (tcp, _peer) = listener.accept().await.context("accepting connection")?;
        let io = TokioIo::new(tcp);
        let hyper_service = TowerToHyperService::new(service.clone());
        // One task per connection; a slow request never blocks the accept loop.
        tokio::spawn(async move {
            let _ = http1::Builder::new().serve_connection(io, hyper_service).await;
        });
    }
}
