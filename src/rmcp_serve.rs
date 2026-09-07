//! Server-facing transports via the official MCP SDK (`rmcp`).
//!
//! min-mcp's `Surface` stays the single source of truth for the tool catalog
//! and dispatch; this module puts it *behind* rmcp's `ServerHandler`, so the
//! agent-facing wire protocol is the SDK's (spec-tracked) rather than our
//! hand-rolled JSON-RPC. The upstream clients, the OpenAPI spec executor, and
//! the whole minify/overlay/projection surface are unchanged — rmcp has no
//! concept of relaying arbitrary upstream tools, so that half remains ours.
//!
//! **Identity.** Every handler resolves a `Caller` per request. Over HTTP the
//! guard in [`serve_http`] authenticates the request (a validated bearer, or
//! identity headers from a trusted gateway) and attaches the caller to the
//! request; rmcp carries the request parts into the handler's context. Over
//! stdio there are no request parts, so the caller is the process identity
//! (`--jwt` / `--scopes`). The surface is shared (`Arc`), not locked: a slow
//! call blocks only its own caller.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

use crate::auth::{ClaimChecks, JwtVerifier};
use crate::caller::Caller;
use crate::config::TrustedHeaders;
use crate::surface::Surface;

/// How long a TLS handshake may take before the connection is dropped. Not
/// configurable on purpose: it bounds an allocation no legitimate client comes
/// near, and a knob here is one more thing to get wrong.
/// How long a client may take to complete the TLS handshake. Short on purpose:
/// an un-handshaked socket is the cheapest thing for an attacker to create and
/// carries no legitimate reason to stall.
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a connection may go without producing a request head, on either
/// scheme. This bounds the same "opens a socket and sends nothing" hazard, but
/// hyper applies it to EVERY head on a connection, not just the first — so it
/// doubles as the idle keep-alive timeout, and a value as short as the handshake
/// deadline would drop a connection whenever an agent paused to think, charging
/// a fresh TCP (and TLS) handshake to the next tool call. 30s keeps the bound
/// while leaving normal think-time alone.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// rmcp `ServerHandler` over a shared `Surface`.
#[derive(Clone)]
pub struct MinMcpServer {
    surface: Arc<Surface>,
    /// The identity used when a request carries none: stdio, or HTTP with no
    /// identity configured (or `allow_anonymous`).
    process_caller: Arc<Caller>,
    /// True when every HTTP request must carry its own identity.
    identity_enforced: bool,
}

impl MinMcpServer {
    pub fn new(surface: Arc<Surface>, process_caller: Caller, identity_enforced: bool) -> Self {
        Self { surface, process_caller: Arc::new(process_caller), identity_enforced }
    }

    /// The caller for this request: what the HTTP guard attached, else the
    /// process identity (stdio has no request parts).
    ///
    /// When identity is enforced, a request that reaches a handler *without*
    /// one is refused rather than silently served as the process — the process
    /// may hold broader scopes (`--scopes admin`) than any real caller, so
    /// falling back would hand them to whoever slipped past the guard. Today
    /// rmcp attaches request parts on every HTTP path; this makes a future one
    /// that doesn't a 400, not a privilege escalation.
    fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<Arc<Caller>, ErrorData> {
        if let Some(c) = ctx
            .extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.extensions.get::<Arc<Caller>>().cloned())
        {
            return Ok(c);
        }
        if self.identity_enforced {
            crate::log_warn!("refusing a request that carried no caller identity");
            return Err(ErrorData::invalid_request(
                "unauthenticated: this request carried no caller identity",
                None,
            ));
        }
        Ok(self.process_caller.clone())
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
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let caller = self.caller(&context)?;
        let v = self.surface.list_resources(&caller).await;
        // Per-entry tolerant, like list_tools below: one malformed upstream
        // entry (missing `name`, wrong-typed field) must drop THAT entry, not
        // fail the whole merged listing (min-mcp's own resource included).
        Ok(ListResourcesResult::with_all_items(collect_valid(&v, "resources")))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let caller = self.caller(&context)?;
        let v = self
            .surface
            .read_resource(&caller, &request.uri)
            .await
            .map_err(|e| ErrorData::resource_not_found(format!("{e:#}"), None))?;
        let result: ReadResourceResult = serde_json::from_value(v)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(result.into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let caller = self.caller(&context)?;
        let v = self.surface.list_prompts(&caller).await;
        Ok(ListPromptsResult::with_all_items(collect_valid(&v, "prompts")))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let caller = self.caller(&context)?;
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        let v = self
            .surface
            .get_prompt(&caller, &request.name, args)
            .await
            .map_err(|e| ErrorData::invalid_params(format!("{e:#}"), None))?;
        let result: GetPromptResult = serde_json::from_value(v)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(result.into())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let caller = self.caller(&context)?;
        let listing = self.surface.list_tools(&caller);
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
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = self.caller(&context)?;
        let name = request.name.to_string();
        let args = request.arguments.map(Value::Object).unwrap_or_else(|| json!({}));
        // The surface returns every tool-level outcome — client-side argument
        // errors, upstream isError, timeouts, AND transport failures — as an
        // isError result (Ok), so the agent can reason about it. Only a failure
        // of min-mcp itself reaches the Err branch; `{:#}` keeps the cause chain.
        let result = self
            .surface
            .call(&caller, &name, args)
            .await
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;
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
/// the client disconnects. One process, one caller.
pub async fn serve_stdio(surface: Arc<Surface>, caller: Caller) -> Result<()> {
    // stdio is one process, one caller: there is no per-request identity to
    // enforce, so the process identity is the caller by definition.
    let running = MinMcpServer::new(surface, caller, false).serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// How HTTP requests are authenticated, from `auth:`.
pub struct HttpIdentity {
    pub verifier: Option<Arc<JwtVerifier>>,
    pub checks: ClaimChecks,
    pub scope_claim: String,
    pub subject_claim: String,
    pub trusted: Option<TrustedHeaders>,
    pub allow_anonymous: bool,
}

/// Why a request was refused, rendered as a 401.
#[derive(Debug)]
struct Refusal {
    /// The `WWW-Authenticate` challenge.
    challenge: &'static str,
    reason: String,
}

impl HttpIdentity {
    /// Everything the request guard needs, from one `auth:` section — so a new
    /// auth knob is added in `Auth` and here, never copied field by field at
    /// the call site (where the HTTP path could silently keep a default).
    pub fn from_auth(a: &crate::config::Auth, verifier: Option<Arc<JwtVerifier>>) -> Self {
        HttpIdentity {
            verifier,
            checks: ClaimChecks { audience: a.audience.clone(), issuer: a.issuer.clone() },
            scope_claim: a.scope_claim.clone(),
            subject_claim: a.subject_claim.clone(),
            trusted: a.trusted_headers.clone(),
            allow_anonymous: a.allow_anonymous,
        }
    }

    /// Is any identity source configured? Without one every request is the
    /// process caller (the local-dev model).
    pub fn required(&self) -> bool {
        self.verifier.is_some() || self.trusted.is_some()
    }

    /// Do requests HAVE to authenticate (identity configured, no anonymous
    /// fallback)? This is what lets a non-loopback bind stand without
    /// `--allow-remote`.
    pub fn enforced(&self) -> bool {
        self.required() && !self.allow_anonymous
    }

    /// The caller a request identifies, in precedence: a bearer validated by
    /// the verifier; identity headers from the trusted gateway; the process
    /// identity when nothing is configured (or anonymous is allowed); else 401.
    async fn authenticate(&self, headers: &hyper::HeaderMap, process: &Arc<Caller>) -> Result<Arc<Caller>, Refusal> {
        let bearer = headers
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let (scheme, tok) = v.trim().split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then_some(tok.trim())
            });
        if let (Some(tok), Some(verifier)) = (bearer, &self.verifier) {
            return match verifier.caller(tok, &self.scope_claim, &self.subject_claim, &self.checks).await {
                Ok(c) => Ok(Arc::new(c)),
                Err(e) => Err(Refusal {
                    challenge: "Bearer error=\"invalid_token\"",
                    reason: format!("invalid bearer token: {e:#}"),
                }),
            };
        }
        if let Some(t) = &self.trusted {
            if let Some(raw) = headers.get(t.scopes.as_str()).and_then(|v| v.to_str().ok()) {
                let scopes = raw
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                let subject = t
                    .subject
                    .as_deref()
                    .and_then(|h| headers.get(h))
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                return Ok(Arc::new(Caller::new(scopes, subject)));
            }
        }
        if !self.required() || self.allow_anonymous {
            return Ok(process.clone());
        }
        Err(Refusal {
            challenge: "Bearer",
            reason: "unauthorized: this server requires an Authorization: Bearer token (or identity \
                     headers from the configured gateway)"
                .to_string(),
        })
    }
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
    // (several = a bare unbracketed IPv6 like `::1`).
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
/// session ids), driven on a TCP listener by hyper, with caller authentication
/// (see [`HttpIdentity::authenticate`]) in front of every request; the caller
/// rides the request into the handler. One shared `Surface` backs every session.
///
/// The listener is loopback-only unless `allow_remote` is set, identity is
/// enforced on every request, OR mutual TLS is required (`http.tls.client_ca_file`
/// — a real authentication boundary, though a channel-level one) — without one
/// of those, anyone who can reach the port gets the process's scopes and every
/// upstream credential it holds.
///
/// DNS-rebinding defences depend on the bind. A **loopback** server validates
/// both headers: `Host` (rmcp's allow-list) and `Origin` (see
/// [`origin_allowed`]) — that is the attack's shape: a page on
/// `evil.example` resolving to 127.0.0.1. A **non-loopback** server cannot
/// validate `Host` — clients address it by whatever name or address they
/// reach it on, and rmcp's loopback-only default would 403 every one of them —
/// so Host validation is off. The `Origin` check stays on under
/// `--allow-remote` (no auth, so a rebinding page must still be refused) and
/// is off when identity is enforced: a rebinding page cannot present a
/// bearer, and a legitimate browser-based client with one must not be refused
/// for having a non-loopback origin.
pub async fn serve_http(
    surface: Arc<Surface>,
    process_caller: Caller,
    identity: HttpIdentity,
    addr: &str,
    allow_remote: bool,
    http_cfg: &crate::config::HttpConfig,
) -> Result<()> {
    use anyhow::Context;
    use hyper::server::conn::http1;
    use hyper_util::rt::TokioIo;
    use hyper_util::service::TowerToHyperService;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
    use tokio::net::TcpListener;
    use tower::Layer;

    let server = MinMcpServer::new(surface, process_caller, identity.enforced());
    let process_caller = server.process_caller.clone();
    let identity = Arc::new(identity);

    let listener = TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr().context("reading the bound address")?;
    // Checked on the RESOLVED address, so every spelling of loopback passes
    // (`localhost`, `127.0.0.1`, `[::1]`) and every spelling of "everyone" is
    // caught (`0.0.0.0`, `[::]`, a LAN ip, a public hostname).
    // Built before the guard so the guard can ask it what it enforces, rather
    // than re-deriving that from config (see `crate::tls::Tls`). A bad
    // certificate is a deployment error: failing every handshake is worse than
    // never binding.
    let tls = http_cfg.tls.as_ref().map(crate::tls::acceptor).transpose()?;
    // Mutual TLS authenticates the CHANNEL: only a client holding a certificate
    // signed by the configured CA can open a connection at all. That is a real
    // authentication boundary, so it satisfies the non-loopback guard the same
    // way per-request identity does — though every such client still shares the
    // process's scopes, which is why it is not a substitute for `auth:`.
    let mutual_tls = tls.as_ref().is_some_and(|t| t.requires_client_cert);
    if !local.ip().is_loopback() && !allow_remote && !identity.enforced() && !mutual_tls {
        anyhow::bail!(
            "refusing to serve HTTP on non-loopback address {local} without caller identity: \
             every client that can reach it would get this process's scopes and upstream \
             credentials. Either configure `auth:` (a JWT verifier or trusted_headers, \
             without allow_anonymous) so each request is authenticated, require client \
             certificates (`http.tls.client_ca_file`), bind 127.0.0.1, or pass \
             --allow-remote if an authenticating proxy or a private network is in front."
        );
    }
    let remote = !local.ip().is_loopback();
    if remote {
        if identity.enforced() {
            crate::log_info!("serving HTTP on non-loopback {local}; every request must authenticate");
        } else {
            crate::log_warn!("serving HTTP on non-loopback {local} with NO inbound authentication (--allow-remote)");
        }
    }
    // See the doc comment above for why these follow the bind.
    let check_origin = !(remote && identity.enforced());
    let limits = &http_cfg.limits;
    let max_body = limits.max_body_bytes;
    let http_config = if remote {
        StreamableHttpServerConfig::default().disable_allowed_hosts()
    } else {
        StreamableHttpServerConfig::default()
    }
    // rmcp enforces the body cap itself while it streams the POST body, and it
    // has its OWN default (4 MiB). Handing it the configured value is what makes
    // `max_body_bytes` real at every size — left at the default, anything above
    // 4 MiB is refused by rmcp with a 413 quoting a number nobody configured —
    // and it renders both the declared and the chunked case as a proper 413.
    .with_max_request_body_bytes(max_body);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );
    // One shared semaphore bounds how many requests rmcp is inside at once, over
    // every connection; past the cap a request waits rather than failing.
    //
    // Measured caveat, so nobody reads more into the knob than it does: tower
    // releases the permit when the response FUTURE resolves, and rmcp answers a
    // POST with an SSE stream that resolves as soon as the stream handle is
    // returned — before the tool call runs. So this bounds request *handling*,
    // NOT concurrent upstream calls. With `max_in_flight: 1`, three concurrent
    // 4s tool calls still complete in ~4s, not ~12s. Bounding upstream
    // concurrency belongs in the dispatch path, beside the per-caller buckets,
    // breakers and timeouts that already govern it.
    let service = tower::limit::GlobalConcurrencyLimitLayer::new(limits.max_in_flight).layer(service);
    let scheme = if tls.is_some() { "https" } else { "http" };
    // Everything the guard needed from `Tls` has been read; the accept loop
    // only needs the acceptor itself (an `Arc<ServerConfig>` bump per clone).
    let tls = tls.map(|t| t.acceptor);
    if tls.is_none() && remote {
        crate::log_warn!(
            "serving PLAINTEXT HTTP on non-loopback {local}: bearer tokens and gateway identity \
             headers cross the network in the clear unless something in front terminates TLS"
        );
    }
    if mutual_tls {
        crate::log_info!("requiring a client certificate on every connection (mutual TLS)");
    }
    eprintln!("min-mcp: Streamable HTTP (rmcp) listening on {scheme}://{local}/");

    loop {
        // A transient accept failure (EMFILE, a reset mid-handshake) must not
        // take the whole server down; log it and keep accepting, with a short
        // pause so a persistent condition can't spin the loop.
        let (tcp, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                crate::log_warn!("accepting connection failed: {e}; continuing");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        // Arc, not a per-request clone: `TowerToHyperService::call` already
        // clones the stack internally (hyper-util's `Oneshot`), and each clone
        // copies rmcp's config Vec. One atomic bump per request instead.
        let inner = Arc::new(TowerToHyperService::new(service.clone()));
        let identity = identity.clone();
        let process_caller = process_caller.clone();
        let tls = tls.clone();
        // What the CONNECTION knows about its client, for rate-limit bucketing
        // when the request names no subject. Filled in below once the transport
        // is up — before a single request can be served on it — so the guard can
        // read it without the guard having to be built per handshake outcome.
        let origin: Arc<std::sync::OnceLock<String>> = Arc::new(std::sync::OnceLock::new());
        let conn_origin = origin.clone();
        // Gate on Origin and identity before the request reaches the MCP
        // service, then hand it off with the caller attached. One task per
        // connection; a slow request never blocks the accept loop.
        let guarded = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
            let inner = inner.clone();
            let identity = identity.clone();
            let process_caller = process_caller.clone();
            let origin = origin.clone();
            async move {
                // Borrowed, and only owned on the refusal path; the flag is
                // tested first so an identity-enforced remote bind (where the
                // check is off) never reads the header at all.
                let refused = {
                    let origin = req.headers().get(hyper::header::ORIGIN).and_then(|v| v.to_str().ok());
                    (check_origin && !origin_allowed(origin)).then(|| origin.unwrap_or_default().to_string())
                };
                if let Some(origin) = refused {
                    crate::log_warn!("refused an HTTP request from Origin {origin:?} (DNS-rebinding defence)");
                    return Ok(plain_response(hyper::StatusCode::FORBIDDEN, None, "forbidden: Origin not allowed"));
                }
                let caller = match identity.authenticate(req.headers(), &process_caller).await {
                    Ok(c) => c,
                    Err(r) => {
                        crate::log_info!("refused an HTTP request: {}", r.reason);
                        return Ok(plain_response(hyper::StatusCode::UNAUTHORIZED, Some(r.challenge), &r.reason));
                    }
                };
                // Never overrides a subject the request supplied; `rate_key`
                // prefers the subject and falls back to this.
                let caller = if caller.subject.is_none() {
                    Arc::new((*caller).clone().with_origin(origin.get().cloned()))
                } else {
                    caller
                };
                req.extensions_mut().insert(caller);
                // The body cap is rmcp's, from `with_max_request_body_bytes`
                // above: it refuses on the first frame that would exceed, so
                // declared and chunked bodies alike get a 413 and nothing past
                // the cap is buffered. One limit, one enforcement point.
                hyper::service::Service::call(&*inner, req).await
            }
        });
        // The handshake happens in the connection's own task: doing it in the
        // accept loop would let one slow or hostile client stall every other
        // connection's accept.
        //
        // The invariant both arms hold: an accepted socket that does not produce
        // a request is dropped. Otherwise a client can open connections and send
        // nothing, pinning a task and a file descriptor each — the one
        // allocation `http.limits` cannot bound, because no request ever exists
        // to be counted. hyper sets no such deadline by default, so
        // `header_read_timeout` covers what hyper can see and the explicit
        // timeout covers the handshake, which it cannot. A long-lived SSE stream
        // is unaffected: the deadline is on reading a request head, not on
        // writing a response (verified — a GET stream held well past it).
        tokio::spawn(async move {
            let conn = || {
                let mut b = http1::Builder::new();
                b.timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(HEADER_READ_TIMEOUT);
                b
            };
            match tls {
                None => {
                    let _ = conn_origin.set(format!("peer:{}", peer.ip()));
                    let _ = conn().serve_connection(TokioIo::new(tcp), guarded).await;
                }
                Some(tls) => match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, tls.accept(tcp)).await {
                    Ok(Ok(stream)) => {
                        // A client certificate identifies its holder far better
                        // than an address does — several tenants can share an
                        // egress IP, but not a certificate.
                        let who = client_fingerprint(stream.get_ref().1)
                            .unwrap_or_else(|| format!("peer:{}", peer.ip()));
                        let _ = conn_origin.set(who);
                        let _ = conn().serve_connection(TokioIo::new(stream), guarded).await;
                    }
                    // Includes a client that presented no certificate, or one
                    // that did not chain to the configured CA, under mutual TLS.
                    Ok(Err(e)) => crate::log_info!("TLS handshake failed: {e}"),
                    Err(_) => crate::log_info!("TLS handshake timed out after {TLS_HANDSHAKE_TIMEOUT:?}"),
                },
            }
        });
    }
}

/// A stable per-client key from the leaf client certificate: `cert:<sha256 hex,
/// truncated>`. Used to bucket rate limits under mutual TLS, never for identity
/// or scopes — it names a key holder, not a person, and it is not put in audit
/// lines. Truncated because this keys a HashMap, not a security decision.
fn client_fingerprint(conn: &rustls::ServerConnection) -> Option<String> {
    let leaf = conn.peer_certificates()?.first()?;
    let digest = ring::digest::digest(&ring::digest::SHA256, leaf.as_ref());
    let hex: String = digest.as_ref().iter().take(16).map(|b| format!("{b:02x}")).collect();
    Some(format!("cert:{hex}"))
}

/// A plain-text response in the same body shape rmcp's service returns.
fn plain_response(
    status: hyper::StatusCode,
    www_authenticate: Option<&'static str>,
    body: &str,
) -> hyper::Response<http_body_util::combinators::BoxBody<bytes::Bytes, std::convert::Infallible>> {
    let body = http_body_util::BodyExt::boxed(http_body_util::Full::new(bytes::Bytes::from(body.to_string())));
    let mut res = hyper::Response::new(body);
    *res.status_mut() = status;
    if let Some(ch) = www_authenticate {
        if let Ok(v) = hyper::header::HeaderValue::from_str(ch) {
            res.headers_mut().insert(hyper::header::WWW_AUTHENTICATE, v);
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

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
            "http://[2001:db8::1]",
            "http://[2001:db8::1]:8080",
        ] {
            assert!(!origin_allowed(Some(bad)), "should refuse {bad}");
        }
    }

    const SECRET: &str = "test-secret";

    fn mint(claims: Value) -> String {
        encode(&Header::default(), &claims, &EncodingKey::from_secret(SECRET.as_bytes())).unwrap()
    }

    fn identity(verifier: bool, trusted: bool, allow_anonymous: bool) -> HttpIdentity {
        HttpIdentity {
            verifier: verifier.then(|| Arc::new(JwtVerifier::Hs256(SECRET.as_bytes().to_vec()))),
            checks: ClaimChecks::default(),
            scope_claim: "scope".into(),
            subject_claim: "sub".into(),
            trusted: trusted.then(|| TrustedHeaders { scopes: "X-Scopes".into(), subject: Some("X-User".into()) }),
            allow_anonymous,
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> hyper::HeaderMap {
        let mut h = hyper::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, hyper::header::HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[tokio::test]
    async fn bearer_is_validated_and_becomes_the_caller() {
        let id = identity(true, false, false);
        let process = Arc::new(Caller::with_scopes(vec!["process".into()]));
        let tok = mint(json!({"exp": 4_000_000_000u64, "scope": "store.read", "sub": "alice"}));
        let c = id.authenticate(&headers(&[("authorization", &format!("Bearer {tok}"))]), &process).await.unwrap();
        assert_eq!(c.scopes, vec!["store.read"]);
        assert_eq!(c.label(), "alice");
        // scheme is case-insensitive
        let c = id.authenticate(&headers(&[("authorization", &format!("bearer {tok}"))]), &process).await.unwrap();
        assert_eq!(c.label(), "alice");
        // a bad token is refused with an invalid_token challenge, never the process identity
        let r = id.authenticate(&headers(&[("authorization", "Bearer not.a.jwt")]), &process).await.unwrap_err();
        assert!(r.challenge.contains("invalid_token"), "{}", r.challenge);
        // no token at all → 401 (identity is required, anonymous not allowed)
        let r = id.authenticate(&headers(&[]), &process).await.unwrap_err();
        assert_eq!(r.challenge, "Bearer");
        assert!(r.reason.contains("requires"), "{}", r.reason);
    }

    #[tokio::test]
    async fn no_identity_configured_means_the_process_caller() {
        let id = identity(false, false, false);
        let process = Arc::new(Caller::with_scopes(vec!["process".into()]));
        assert!(!id.required() && !id.enforced());
        let c = id.authenticate(&headers(&[]), &process).await.unwrap();
        assert_eq!(c.scopes, vec!["process"]);
        // a stray bearer with no verifier is ignored, not an error
        let c = id.authenticate(&headers(&[("authorization", "Bearer whatever")]), &process).await.unwrap();
        assert_eq!(c.scopes, vec!["process"]);
    }

    #[tokio::test]
    async fn allow_anonymous_falls_back_to_the_process_caller() {
        let id = identity(true, false, true);
        assert!(id.required() && !id.enforced(), "anonymous allowed → not enforced");
        let process = Arc::new(Caller::with_scopes(vec!["process".into()]));
        let c = id.authenticate(&headers(&[]), &process).await.unwrap();
        assert_eq!(c.scopes, vec!["process"]);
        // but a PRESENT bad token is still refused — anonymous is not "any token"
        assert!(id.authenticate(&headers(&[("authorization", "Bearer bad")]), &process).await.is_err());
    }

    #[tokio::test]
    async fn trusted_gateway_headers_carry_scopes_and_subject() {
        let id = identity(false, true, false);
        let process = Arc::new(Caller::default());
        let c = id
            .authenticate(&headers(&[("x-scopes", "store.read, store.write"), ("x-user", "bob@corp")]), &process)
            .await
            .unwrap();
        assert_eq!(c.scopes, vec!["store.read", "store.write"]);
        assert_eq!(c.label(), "bob@corp");
        // header present but empty → a caller with no scopes (sees only unscoped tools)
        let c = id.authenticate(&headers(&[("x-scopes", "")]), &process).await.unwrap();
        assert!(c.scopes.is_empty() && c.subject.is_none());
        // no identity header → refused
        assert!(id.authenticate(&headers(&[]), &process).await.is_err());
    }
}
