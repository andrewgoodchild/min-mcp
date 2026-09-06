//! Stdio MCP client: spawn an upstream server, speak newline-delimited
//! JSON-RPC, expose initialize / tools/list / tools/call.
//!
//! **Multiplexed.** One reader task per child routes each response to the call
//! that sent it (by id), so concurrent callers of the same upstream don't queue
//! behind one another and a slow call never blocks a fast one.
//!
//! **Self-healing.** A child that exits is respawned on the next call (with a
//! crash-loop guard), re-initialized, and the call proceeds. The tool catalog
//! is a startup snapshot: if the new instance announces a different catalog
//! (`notifications/tools/list_changed`), the upstream is flagged stale in
//! `inspect` and a restart is advised.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::config::UpstreamConfig;
use crate::jsonrpc::{self, response_id};
use crate::sync::{lock, read, write};

/// The transport-level ceiling every per-call deadline is clamped to. An overlay
/// `timeout_s` may tighten a call below this; nothing may exceed it.
pub(crate) const TRANSPORT_CEILING_S: u64 = 120;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(TRANSPORT_CEILING_S);
/// Minimum gap between two spawns of one upstream. A child that dies within
/// this window of starting is a crash loop: callers get an error until the
/// window passes, instead of a fork storm.
const MIN_RESPAWN_INTERVAL: Duration = Duration::from_secs(1);

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

/// Hard ceiling on `nextCursor` pages followed in one listing. Generous — a
/// paginated upstream would need >100k tools to hit it — but it turns a buggy or
/// adversarial upstream that echoes a cursor forever into a truncated listing
/// rather than a permanent wedge.
const LIST_MAX_PAGES: usize = 1_000;

/// The JSON-RPC seam every MCP client transport implements. Everything
/// protocol-shaped above it — tool listing, passthrough listing, resource and
/// prompt fetches, tool calls — is provided ONCE here, so the stdio and HTTP
/// clients cannot drift apart. `&self` throughout: each transport is internally
/// synchronized, so callers never need a lock around it.
#[allow(async_fn_in_trait)] // concrete impls only; no dyn use
pub trait McpRpc {
    /// The upstream's public name (prefixes tool ids).
    fn rpc_name(&self) -> &str;
    /// One JSON-RPC request; `deadline` is the overlay `timeout_s` (the
    /// transport's own 120s ceiling still applies underneath).
    async fn rpc(&self, method: &str, params: Value, deadline: Option<Duration>) -> Result<Value>;

    async fn call_tool(&self, name: &str, arguments: Value, deadline: Option<Duration>) -> Result<Value> {
        self.rpc("tools/call", json!({"name": name, "arguments": arguments}), deadline).await
    }

    async fn list_tools(&self, upstream_idx: usize) -> Result<Vec<ToolDef>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..LIST_MAX_PAGES {
            let result = self.rpc("tools/list", cursor_params(&cursor), None).await?;
            let next = push_tools_page(&result, self.rpc_name(), upstream_idx, &mut tools);
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
    async fn list_passthrough(&self, method: &str, key: &str) -> Vec<Value> {
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
            if next.is_none() || next == cursor {
                return out;
            }
            cursor = next;
        }
        out
    }

    async fn read_resource(&self, uri: &str) -> Result<Value> {
        self.rpc("resources/read", json!({"uri": uri}), None).await
    }

    async fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value> {
        self.rpc("prompts/get", prompt_params(name, arguments), None).await
    }
}

/// Protocol version min-mcp announces to *upstream* servers it proxies. (The
/// agent-facing server side negotiates versions via rmcp.)
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

/// The half of a live connection that callers and the reader task both touch.
struct Shared {
    name: String,
    writer: tokio::sync::Mutex<ChildStdin>,
    /// request id -> the caller waiting for that response (Ok(result) or
    /// Err(JSON-RPC error text)).
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>,
    /// Set by the reader on EOF/error; `Upstream::connection` respawns.
    dead: AtomicBool,
    /// Set when the upstream announces `tools/list_changed` (catalog snapshot
    /// is now stale). Shared with the owning `Upstream` across respawns.
    stale: Arc<AtomicBool>,
}

/// One spawned child: its handles, the reader task, and the id counter.
struct Conn {
    _child: Child,
    shared: Arc<Shared>,
    next_id: AtomicI64,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // The child is killed by `kill_on_drop`; make sure the reader task goes
        // with it instead of lingering on a pipe that is about to close.
        self.reader.abort();
    }
}

impl Conn {
    async fn spawn(cfg: &UpstreamConfig, stale: Arc<AtomicBool>) -> Result<Conn> {
        let command = cfg.command.as_ref().ok_or_else(|| {
            anyhow!(
                "upstream {} has no `command` (expected an MCP server subprocess; `url` and `spec` are the other kinds)",
                cfg.name
            )
        })?;
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
        let shared = Arc::new(Shared {
            name: cfg.name.clone(),
            writer: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            stale,
        });
        let reader = tokio::spawn(read_loop(BufReader::new(stdout).lines(), shared.clone()));
        let conn = Conn { _child: child, shared, next_id: AtomicI64::new(0), reader };
        conn.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "min-mcp", "version": env!("CARGO_PKG_VERSION")},
            }),
            None,
        )
        .await
        .with_context(|| format!("initializing upstream {}", cfg.name))?;
        conn.notify("notifications/initialized", json!({})).await?;
        Ok(conn)
    }

    /// Write one frame. The writer lock makes a frame atomic: two concurrent
    /// callers can never interleave halves of a line.
    async fn send_line(shared: &Shared, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        let mut w = shared.writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        Self::send_line(&self.shared, &json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    /// Send a request and await ITS response. The write completes before the
    /// deadline starts, deliberately: cancelling a stdio write mid-frame would
    /// leave a partial line in the pipe. On expiry the pending slot is dropped,
    /// so a late response is discarded rather than mis-delivered.
    async fn request(&self, method: &str, params: Value, deadline: Option<Duration>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        lock(&self.shared.pending).insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = Self::send_line(&self.shared, &frame).await {
            lock(&self.shared.pending).remove(&id);
            self.shared.dead.store(true, Ordering::Relaxed);
            return Err(e).with_context(|| format!("writing {method} to upstream {}", self.shared.name));
        }
        // The reader may have drained `pending` (child gone) between our
        // insert and this point — its `dead` store happens after that drain,
        // so seeing `dead` here means nothing will ever answer us. Without
        // this check such a call waits out the full deadline for a reply that
        // cannot come, and the agent sees a timeout instead of a dead upstream.
        if self.shared.dead.load(Ordering::Relaxed) {
            lock(&self.shared.pending).remove(&id);
            bail!("upstream {} closed its stdout", self.shared.name);
        }
        let name = &self.shared.name;
        let wait = async {
            match rx.await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(err)) => Err(anyhow!("upstream {name} error on {method}: {err}")),
                Err(_) => Err(anyhow!("upstream {name} closed its stdout")),
            }
        };
        // One wait, whichever bound applies: an overlay `timeout_s` is a typed
        // TimeoutElapsed the agent sees as TIMEOUT; the transport ceiling is a
        // plain error. Only the error differs, so only the error is branched.
        match timeout(deadline.unwrap_or(REQUEST_TIMEOUT), wait).await {
            Ok(r) => r,
            Err(_) => {
                lock(&self.shared.pending).remove(&id);
                Err(match deadline {
                    Some(d) => anyhow::Error::new(TimeoutElapsed { secs: d.as_secs() }),
                    None => anyhow!("upstream {} timed out on {method}", self.shared.name),
                })
            }
        }
    }
}

/// The reader task: route responses to their callers, answer server-to-client
/// requests (ping; everything else method-not-found) so a blocking upstream
/// can't stall us, note catalog-change notifications, skip banner noise. On
/// EOF, fail every in-flight call and mark the connection dead.
async fn read_loop(mut lines: Lines<BufReader<ChildStdout>>, shared: Arc<Shared>) {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue; // stray non-JSON output
        };
        // A message carrying a method is a request/notification FROM the
        // upstream, not a response — even if its id happens to equal one of ours.
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            match v.get("id") {
                Some(req_id) => {
                    let reply = if method == "ping" {
                        jsonrpc::result(req_id, json!({}))
                    } else {
                        jsonrpc::error(req_id, jsonrpc::METHOD_NOT_FOUND, format!("min-mcp does not support {method}"))
                    };
                    if Conn::send_line(&shared, &reply).await.is_err() {
                        break;
                    }
                }
                None if method == "notifications/tools/list_changed" => {
                    shared.stale.store(true, Ordering::Relaxed);
                    crate::log_warn!(
                        "upstream {} announced tools/list_changed; the catalog is a startup snapshot — \
                         restart minmcp to pick up the new tools (flagged in `inspect` as upstreams_stale)",
                        shared.name
                    );
                }
                None => {}
            }
            continue;
        }
        let Some(id) = response_id(&v) else { continue };
        let waiting = lock(&shared.pending).remove(&id);
        if let Some(tx) = waiting {
            let outcome = match v.get("error") {
                Some(e) => Err(e.to_string()),
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(outcome); // caller may have timed out and left
        }
    }
    shared.dead.store(true, Ordering::Relaxed);
    let in_flight = std::mem::take(&mut *lock(&shared.pending));
    if in_flight.is_empty() {
        crate::log_warn!("upstream {} exited; it will be respawned on the next call", shared.name);
    } else {
        crate::log_warn!(
            "upstream {} exited with {} call(s) in flight; they fail now, it respawns on the next call",
            shared.name,
            in_flight.len()
        );
    }
    // dropping `in_flight` fails every waiting caller with "closed its stdout"
}

pub struct Upstream {
    pub name: String,
    /// How this upstream's tool results are serialized to the agent (`json` =
    /// compact re-encode of JSON text blocks; `raw` = byte-for-byte).
    pub result_format: crate::config::ResultFormat,
    cfg: UpstreamConfig,
    /// The live connection, replaced on respawn. A healthy call only reads it
    /// (clone an `Arc`), so callers never queue behind one another here.
    conn: RwLock<Option<Arc<Conn>>>,
    /// Held across a respawn so exactly one caller spawns a replacement child;
    /// the others wait, then find the new connection through `live()`.
    respawn: tokio::sync::Mutex<()>,
    stale: Arc<AtomicBool>,
    last_spawn: Mutex<Instant>,
}

impl Upstream {
    pub async fn spawn(cfg: &UpstreamConfig) -> Result<Self> {
        let stale = Arc::new(AtomicBool::new(false));
        let conn = Conn::spawn(cfg, stale.clone()).await?;
        Ok(Upstream {
            name: cfg.name.clone(),
            result_format: cfg.result_format(),
            cfg: cfg.clone(),
            conn: RwLock::new(Some(Arc::new(conn))),
            respawn: tokio::sync::Mutex::new(()),
            stale,
            last_spawn: Mutex::new(Instant::now()),
        })
    }

    /// Has this upstream announced a catalog change since startup?
    pub fn stale(&self) -> bool {
        self.stale.load(Ordering::Relaxed)
    }

    /// The current connection, if it is still running.
    fn live(&self) -> Option<Arc<Conn>> {
        read(&self.conn).as_ref().filter(|c| !c.shared.dead.load(Ordering::Relaxed)).cloned()
    }

    /// The live connection — respawning the child if the previous one died,
    /// unless it died within `MIN_RESPAWN_INTERVAL` of starting (crash loop).
    async fn connection(&self) -> Result<Arc<Conn>> {
        if let Some(c) = self.live() {
            return Ok(c); // fast path: a read lock, no contention
        }
        let _flight = self.respawn.lock().await;
        if let Some(c) = self.live() {
            return Ok(c); // another caller respawned while we waited
        }
        let now = Instant::now();
        let since = now.saturating_duration_since(*lock(&self.last_spawn));
        if since < MIN_RESPAWN_INTERVAL {
            bail!(
                "upstream {} exited {:.1}s after starting and is not being restarted yet \
                 (crash-loop guard: at least {}s between starts)",
                self.name,
                since.as_secs_f64(),
                MIN_RESPAWN_INTERVAL.as_secs()
            );
        }
        crate::log_warn!(
            "upstream {} is not running; respawning {:?}",
            self.name,
            self.cfg.command.as_deref().unwrap_or("")
        );
        *lock(&self.last_spawn) = now;
        let conn = Conn::spawn(&self.cfg, self.stale.clone())
            .await
            .with_context(|| format!("respawning upstream {}", self.name))?;
        let conn = Arc::new(conn);
        *write(&self.conn) = Some(conn.clone());
        Ok(conn)
    }
}

impl McpRpc for Upstream {
    fn rpc_name(&self) -> &str {
        &self.name
    }

    async fn rpc(&self, method: &str, params: Value, deadline: Option<Duration>) -> Result<Value> {
        let conn = self.connection().await?;
        conn.request(method, params, deadline).await
    }
}

