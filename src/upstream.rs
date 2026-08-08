//! Stdio MCP client: spawn an upstream server, speak newline-delimited
//! JSON-RPC, expose initialize / tools/list / tools/call.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Duration};

use crate::config::UpstreamConfig;
use crate::jsonrpc;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
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
        });
    }
    result.get("nextCursor").and_then(Value::as_str).map(str::to_string)
}

pub struct Upstream {
    pub name: String,
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
        self.next_id += 1;
        let id = self.next_id;
        let want = json!(id);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
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

    pub async fn list_tools(&mut self, upstream_idx: usize) -> Result<Vec<ToolDef>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            cursor = push_tools_page(&result, &self.name, upstream_idx, &mut tools);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }
}
