//! Stdio MCP client: spawn an upstream server, speak newline-delimited
//! JSON-RPC, expose initialize / tools/list / tools/call.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Duration};

use crate::config::UpstreamConfig;
use crate::jsonrpc;

/// The transport-level ceiling every per-call deadline is clamped to. An overlay
/// `timeout_s` may tighten a call below this; nothing may exceed it.
///
/// KNOWN LIMIT: the deadline wraps `read_response`, which may itself WRITE to
/// upstream stdin (replying to server pings). Expiry mid-write drops that reply,
/// and in the worst case (reply > PIPE_BUF into a full pipe) could desync the
/// frame stream until the upstream restarts. Restructuring server-request
/// replies out of the deadline scope is tracked as future work.
pub(crate) const TRANSPORT_CEILING_S: u64 = 120;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(TRANSPORT_CEILING_S);
/// Typed marker for an overlay `timeout_s` deadline expiring — dispatch
/// downcasts it into an agent-facing isError instead of a protocol error.
#[derive(Debug)]
pub struct TimeoutElapsed {
    pub secs: u64,
}

impl std::fmt::Display for TimeoutElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "call exceeded its {}s deadline", self.secs)
    }
}

impl std::error::Error for TimeoutElapsed {}

/// The load-bearing sentence every timeout rendering shares (asserted by the
/// E2E suite): a timed-out WRITE must never be blindly retried.
pub const TIMEOUT_GUIDANCE: &str =
    "The operation may or may not have completed upstream — for writes, check state before retrying.";

/// `tools/list` / `resources/list` / `prompts/list` cursor page params.
fn cursor_params(cursor: &Option<String>) -> Value {
    match cursor {
        Some(c) => json!({"cursor": c}),
        None => json!({}),
    }
}

/// `prompts/get` params — `arguments` omitted when null (spec shape).
fn prompt_params(name: &str, arguments: Value) -> Value {
    if arguments.is_null() {
        json!({"name": name})
    } else {
        json!({"name": name, "arguments": arguments})
    }
}

/// The JSON-RPC seam every MCP client transport implements. Everything
/// protocol-shaped above it — tool listing, passthrough listing, resource and
/// prompt fetches, tool calls — is provided ONCE here, so the stdio and HTTP
/// clients cannot drift apart.
/// Hard ceiling on `nextCursor` pages followed in one listing. Generous — a
/// paginated upstream would need >100k tools to hit it — but it turns a buggy or
/// adversarial upstream that echoes a cursor forever from a permanent wedge (the
/// listing loop runs while rmcp_serve holds the shared Surface mutex, so every
/// session's every call would block) into a truncated listing.
const LIST_MAX_PAGES: usize = 1_000;

#[allow(async_fn_in_trait)] // concrete impls only; no dyn use
pub trait McpRpc {
    /// The upstream's public name (prefixes tool ids).
    fn rpc_name(&self) -> &str;
    /// One JSON-RPC request; `deadline` is the overlay `timeout_s` (the
    /// transport's own 120s ceiling still applies underneath).
    async fn rpc(&mut self, method: &str, params: Value, deadline: Option<Duration>)
        -> Result<Value>;

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        deadline: Option<Duration>,
    ) -> Result<Value> {
        self.rpc("tools/call", json!({"name": name, "arguments": arguments}), deadline).await
    }

    async fn list_tools(&mut self, upstream_idx: usize) -> Result<Vec<ToolDef>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..LIST_MAX_PAGES {
            let result = self.rpc("tools/list", cursor_params(&cursor), None).await?;
            let name = self.rpc_name().to_string();
            let next = push_tools_page(&result, &name, upstream_idx, &mut tools);
            // a non-advancing cursor would refetch the same page forever
            if next.is_none() || next == cursor {
                return Ok(tools);
            }
            cursor = next;
        }
        Ok(tools)
    }

    /// Best-effort passthrough listing: an upstream without the capability (or
    /// erroring on the method) contributes what it returned so far rather than
    /// failing the merge. Follows `nextCursor` pagination like `list_tools`.
    async fn list_passthrough(&mut self, method: &str, key: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..LIST_MAX_PAGES {
            let Ok(mut r) = self.rpc(method, cursor_params(&cursor), None).await else {
                return out;
            };
            if let Some(arr) = r.get_mut(key).and_then(Value::as_array_mut) {
                out.append(arr); // take, don't clone — r is owned and dropped
            }
            let next = r.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            // stop on end-of-listing AND on a cursor that didn't advance
            if next.is_none() || next == cursor {
                return out;
            }
            cursor = next;
        }
        out
    }

    async fn read_resource(&mut self, uri: &str) -> Result<Value> {
        self.rpc("resources/read", json!({"uri": uri}), None).await
    }

    async fn get_prompt(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.rpc("prompts/get", prompt_params(name, arguments), None).await
    }
}
/// Protocol version min-mcp announces to *upstream* servers it proxies. (The
/// agent-facing server side negotiates versions via rmcp now.)
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Index of the owning upstream in the Surface's upstream list.
    pub upstream_idx: usize,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Cached `upstream.name` — avoids re-allocating on every lookup.
    pub id: String,
    /// The upstream's own read-only signal (`annotations.readOnlyHint` for MCP
    /// tools; `method == GET` for spec operations). Drives read-result caching.
    pub read_only: Option<bool>,
}

impl ToolDef {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Append the ToolDefs from one `tools/list` result page into `out`, returning
/// the next cursor (None when the last page). Shared by every MCP-client
/// transport (stdio and HTTP) so the id scheme (`upstream.tool`) and the
/// input-schema fallback live in exactly one place.
pub fn push_tools_page(
    result: &Value,
    upstream_name: &str,
    upstream_idx: usize,
    out: &mut Vec<ToolDef>,
) -> Option<String> {
    for t in result.get("tools").and_then(Value::as_array).into_iter().flatten() {
        let name = t.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        out.push(ToolDef {
            upstream_idx,
            id: format!("{upstream_name}.{name}"),
            name,
            description: t.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
            input_schema: t
                .get("inputSchema")
                .cloned()
                .unwrap_or(json!({"type": "object", "properties": {}})),
            read_only: t
                .get("annotations")
                .and_then(|a| a.get("readOnlyHint"))
                .and_then(Value::as_bool),
        });
    }
    result.get("nextCursor").and_then(Value::as_str).map(str::to_string)
}

pub struct Upstream {
    pub name: String,
    /// How this upstream's tool results are serialized to the agent (`json` =
    /// compact re-encode of JSON text blocks; `raw` = byte-for-byte).
    pub result_format: crate::config::ResultFormat,
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: i64,
}

impl Upstream {
    pub async fn spawn(cfg: &UpstreamConfig) -> Result<Self> {
        let command = cfg
            .command
            .as_ref()
            .ok_or_else(|| anyhow!("upstream {} has neither `command` nor `spec`", cfg.name))?;
        let mut cmd = Command::new(command);
        cmd.args(&cfg.args).envs(&cfg.env);
        if let Some(dir) = &cfg.cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true) // don't orphan the child when minmcp exits
            .spawn()
            .with_context(|| format!("spawning upstream {}: {command}", cfg.name))?;
        let stdin = child.stdin.take().context("upstream stdin")?;
        let stdout = child.stdout.take().context("upstream stdout")?;
        let mut up = Upstream {
            name: cfg.name.clone(),
            result_format: cfg.result_format(),
            _child: child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 0,
        };
        up.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "min-mcp", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await
        .with_context(|| format!("initializing upstream {}", cfg.name))?;
        up.notify("notifications/initialized", json!({})).await?;
        Ok(up)
    }

    async fn send(&mut self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Send a request and read lines until its response arrives. Along the way
    /// we answer server-to-client requests (ping, and everything else with a
    /// method-not-found error) so a blocking upstream can't stall us; upstream
    /// notifications and non-JSON banner lines are skipped.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_deadline(method, params, None).await
    }

    /// Like [`request`], but the RESPONSE WAIT runs under `deadline`. The write
    /// is completed before the deadline starts, deliberately: cancelling a
    /// stdio write mid-frame would leave a partial line in the pipe that merges
    /// with the next request into one corrupt frame. On expiry the pending
    /// response stays in the pipe and is skipped later by id mismatch.
    async fn request_deadline(
        &mut self,
        method: &str,
        params: Value,
        deadline: Option<Duration>,
    ) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match deadline {
            Some(d) => match timeout(d, self.read_response(id, method)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::Error::new(TimeoutElapsed { secs: d.as_secs() })),
            },
            None => self.read_response(id, method).await,
        }
    }

    async fn read_response(&mut self, id: i64, method: &str) -> Result<Value> {
        let want = json!(id);
        loop {
            let line = timeout(REQUEST_TIMEOUT, self.lines.next_line())
                .await
                .map_err(|_| anyhow!("upstream {} timed out on {method}", self.name))??
                .ok_or_else(|| anyhow!("upstream {} closed its stdout", self.name))?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue; // stray non-JSON output
            };
            // A message carrying a method is a request/notification FROM the
            // upstream, not our response — even if its id happens to equal ours.
            if v.get("method").is_some() {
                if let Some(req_id) = v.get("id") {
                    self.answer_server_request(
                        req_id.clone(),
                        v.get("method").and_then(Value::as_str).unwrap_or(""),
                    )
                    .await?;
                }
                continue; // notification (no id) → nothing to answer
            }
            // Compare ids by value, not as_i64: tolerates float/string echoes.
            if v.get("id") == Some(&want) {
                if let Some(err) = v.get("error") {
                    return Err(anyhow!("upstream {} error on {method}: {err}", self.name));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            // else: a response to some other request — skip
        }
    }

    /// Reply to a request the upstream sent us. We answer `ping` with `{}` and
    /// decline everything else, so the upstream never blocks awaiting us.
    async fn answer_server_request(&mut self, req_id: Value, method: &str) -> Result<()> {
        let reply = if method == "ping" {
            jsonrpc::result(&req_id, json!({}))
        } else {
            jsonrpc::error(
                &req_id,
                jsonrpc::METHOD_NOT_FOUND,
                format!("min-mcp does not support {method}"),
            )
        };
        self.send(&reply).await
    }





}

impl McpRpc for Upstream {
    fn rpc_name(&self) -> &str {
        &self.name
    }

    async fn rpc(&mut self, method: &str, params: Value, deadline: Option<Duration>) -> Result<Value> {
        self.request_deadline(method, params, deadline).await
    }
}
