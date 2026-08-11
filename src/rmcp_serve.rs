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
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, ServerCapabilities, ServerInfo, Tool,
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
        info.capabilities =
            ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build();
        info.server_info = Implementation::new("min-mcp", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Minified MCP surface. Use search_tools to find a tool, get_tool_details for its \
             schema, then call_tool to run it."
                .to_string(),
        );
        info
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let v = self.surface.lock().await.list_resources().await;
        // Per-entry tolerant, like list_tools below: one malformed upstream
        // entry (missing `name`, wrong-typed field) must drop THAT entry, not
        // fail the whole merged listing (min-mcp's own resource included).
        Ok(ListResourcesResult::with_all_items(collect_valid(&v, "resources")))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let v = self
            .surface
            .lock()
            .await
            .read_resource(&request.uri)
            .await
            .map_err(|e| ErrorData::resource_not_found(e.to_string(), None))?;
        let result: ReadResourceResult = serde_json::from_value(v)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(result.into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let v = self.surface.lock().await.list_prompts().await;
        Ok(ListPromptsResult::with_all_items(collect_valid(&v, "prompts")))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        let v = self
            .surface
            .lock()
            .await
            .get_prompt(&request.name, args)
            .await
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let result: GetPromptResult = serde_json::from_value(v)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(result.into())
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

/// Deserialize each entry of `listing[key]` individually, dropping (and
/// logging) the ones that don't fit rmcp's typed model — a Vec deserialization
/// is atomic, and one non-compliant upstream entry must not empty the merge.
fn collect_valid<T: serde::de::DeserializeOwned>(listing: &Value, key: &str) -> Vec<T> {
    listing
        .get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| match serde_json::from_value::<T>(e.clone()) {
                    Ok(v) => Some(v),
                    Err(err) => {
                        crate::log_warn!("dropping malformed {key} entry from listing: {err}");
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
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

/// Is this `Origin` header value acceptable for a locally-bound MCP server?
///
/// The MCP spec asks HTTP servers to validate `Origin` against DNS-rebinding:
/// a page on `https://evil.example` can be made to resolve to 127.0.0.1 and
/// POST to a local server, and the browser will attach its own origin. rmcp
/// validates the `Host` header; this adds the `Origin` half.
///
/// A **missing** Origin is allowed — non-browser clients (agents, curl, the MCP
/// SDKs) don't send one, and rejecting that would break every normal caller.
/// A **present** Origin must be loopback, which is the only origin a browser
/// could legitimately have for a localhost-bound server.
pub(crate) fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let origin = origin.trim();
    if origin.is_empty() || origin.eq_ignore_ascii_case("null") {
        return true; // opaque origin (file://, sandboxed iframe) carries no authority
    }
    // Strip scheme, then any :port, then compare the host.
    let after_scheme = origin.split_once("://").map(|(_, rest)| rest).unwrap_or(origin);
    let host = after_scheme.split('/').next().unwrap_or("");
    // Port stripping that survives IPv6. A bracketed literal keeps everything up
    // to `]` (a port can only follow the bracket); otherwise exactly one colon
    // means host:port, while zero or several means there is no port to strip
    // (several = a bare unbracketed IPv6 like `::1`). The previous rsplit-based
    // guard got `http://[::1]` (no port) wrong: it split inside the literal,
    // compared the host `[:`, and refused a legitimate loopback origin.
    let host = if host.starts_with('[') {
        match host.find(']') {
            Some(i) => &host[..=i],
            None => host,
        }
    } else if host.matches(':').count() == 1 {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    } else {
        host
    };
    matches!(
        host.trim_end_matches('.'),
        "localhost" | "127.0.0.1" | "[::1]" | "::1" | "[0:0:0:0:0:0:0:1]"
    )
}

/// Serve the minified surface over Streamable HTTP using rmcp's
/// `StreamableHttpService` (JSON-RPC over POST, SSE for streamed replies,
/// session ids, Host-header validation for DNS-rebinding defence), driven on a
/// TCP listener by hyper, with an added `Origin` check (see
/// [`origin_allowed`]). One shared `Surface` backs every session (the
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
        let inner = TowerToHyperService::new(service.clone());
        // Gate on Origin before the request reaches the MCP service, then hand
        // it off untouched. One task per connection; a slow request never
        // blocks the accept loop.
        let guarded = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let inner = inner.clone();
            async move {
                let origin =
                    req.headers().get(hyper::header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_string);
                if !origin_allowed(origin.as_deref()) {
                    crate::log_warn!(
                        "refused an HTTP request from Origin {:?} (DNS-rebinding defence)",
                        origin.unwrap_or_default()
                    );
                    let body = http_body_util::BodyExt::boxed(http_body_util::Full::new(
                        bytes::Bytes::from_static(b"forbidden: Origin not allowed"),
                    ));
                    let mut res = hyper::Response::new(body);
                    *res.status_mut() = hyper::StatusCode::FORBIDDEN;
                    return Ok(res);
                }
                hyper::service::Service::call(&inner, req).await
            }
        });
        tokio::spawn(async move {
            let _ = http1::Builder::new().serve_connection(io, guarded).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::origin_allowed;

    #[test]
    fn origin_allows_absent_and_loopback_only() {
        // absent / opaque: normal non-browser clients, and sandboxed pages
        assert!(origin_allowed(None));
        assert!(origin_allowed(Some("")));
        assert!(origin_allowed(Some("null")));
        // loopback in its various spellings, with and without ports
        for ok in [
            "http://localhost",
            "http://localhost:8080",
            "https://127.0.0.1:3000",
            "http://[::1]:9000",
            // bare IPv6 loopback, NO port — the case the old port-stripper
            // mangled into `[:` and refused
            "http://[::1]",
            "http://[0:0:0:0:0:0:0:1]",
            "http://[0:0:0:0:0:0:0:1]:8080",
            "http://localhost.",
        ] {
            assert!(origin_allowed(Some(ok)), "should allow {ok}");
        }
        // anything else is a rebinding candidate
        for bad in [
            "https://evil.example",
            "http://evil.example:80",
            "https://localhost.evil.example",
            "http://169.254.169.254",
            "https://sub.localhost.attacker.com",
            // IPv6 non-loopback, bracketed both ways
            "http://[2001:db8::1]",
            "http://[2001:db8::1]:8080",
        ] {
            assert!(!origin_allowed(Some(bad)), "should refuse {bad}");
        }
    }
}
