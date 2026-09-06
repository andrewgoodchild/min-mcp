//! Streamable-HTTP MCP client: proxy a *remote* MCP server (one reachable at a
//! URL, not spawned as a subprocess). Speaks the 2025-06-18 transport — POST
//! JSON-RPC to the single MCP endpoint, capture the `Mcp-Session-Id` at
//! initialize and echo it back, accept either an `application/json` reply or a
//! `text/event-stream` (SSE) one.
//!
//! `&self` throughout: reqwest's client is shared, ids are atomic, and the
//! only awaited shared state (the OAuth token cache) sits behind its own mutex
//! so a token refresh is single-flight. A session the server has expired
//! (HTTP 404 on a request carrying our session id) is re-initialized once and
//! the request retried.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};

use crate::config::UpstreamConfig;
use crate::jsonrpc::response_id;
use crate::oauth::OAuthClient;
use crate::secrets::Secrets;
use crate::sync::lock;
use crate::upstream::{TimeoutElapsed, PROTOCOL_VERSION};

/// Per-request ceiling, matching the stdio client's REQUEST_TIMEOUT. Without it
/// a slow or stream-holding remote MCP server would stall a call forever:
/// reqwest has no default timeout, and reading an SSE reply drains the whole
/// body — which a server keeping the stream open never ends.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Marker: the server no longer knows our session (404 with a session id set).
#[derive(Debug)]
struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP session expired upstream (HTTP 404)")
    }
}

impl std::error::Error for SessionExpired {}

pub struct HttpUpstream {
    pub name: String,
    /// How this upstream's tool results are serialized to the agent (`json` =
    /// compact re-encode of JSON text blocks; `raw` = byte-for-byte).
    pub result_format: crate::config::ResultFormat,
    client: reqwest::Client,
    url: String,
    /// Static auth headers, `${…}` references resolved. Values are secrets
    /// (typically a bearer), so they never appear in Debug output.
    headers: Vec<(String, SecretString)>,
    /// OAuth client-credentials, if this upstream is OAuth-protected.
    oauth: Option<OAuthClient>,
    session_id: Mutex<Option<String>>,
    /// Held across a re-handshake so exactly one caller re-initializes; the
    /// others find the new session and reuse it.
    reinit: tokio::sync::Mutex<()>,
    next_id: AtomicI64,
    stale: AtomicBool,
}

impl HttpUpstream {
    pub async fn connect(cfg: &UpstreamConfig, secrets: &Secrets) -> Result<Self> {
        let url = cfg
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("http upstream {} needs `url`", cfg.name))?
            .clone();
        let mut headers = Vec::with_capacity(cfg.headers.len());
        for (k, v) in &cfg.headers {
            headers.push((k.clone(), SecretString::from(secrets.expand(v).await?)));
        }
        let oauth = match &cfg.oauth {
            Some(o) => Some(OAuthClient::new(o, secrets).await?),
            None => None,
        };
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("building HTTP client for upstream")?;
        let up = HttpUpstream {
            name: cfg.name.clone(),
            result_format: cfg.result_format(),
            client,
            url,
            headers,
            oauth,
            session_id: Mutex::new(None),
            reinit: tokio::sync::Mutex::new(()),
            next_id: AtomicI64::new(0),
            stale: AtomicBool::new(false),
        };
        up.initialize().await?;
        Ok(up)
    }

    /// Has this upstream announced a catalog change since startup?
    pub fn stale(&self) -> bool {
        self.stale.load(Ordering::Relaxed)
    }

    /// The session id the server last gave us.
    fn session(&self) -> Option<String> {
        lock(&self.session_id).clone()
    }

    /// The MCP handshake — at connect, and again when the server forgets us.
    async fn initialize(&self) -> Result<()> {
        *lock(&self.session_id) = None;
        let mut frame = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "min-mcp", "version": env!("CARGO_PKG_VERSION")},
            }
        });
        self.request_once("initialize", &mut frame, None)
            .await
            .with_context(|| format!("initializing http upstream {}", self.name))?;
        self.notify("notifications/initialized", json!({})).await
    }

    /// Re-handshake after the server forgot our session. Single-flight, and a
    /// no-op when another caller already replaced the session we saw fail:
    /// two concurrent re-inits would each clear the other's freshly captured
    /// id, and the loser's `notifications/initialized` would go out with no
    /// session at all.
    async fn reinitialize(&self, seen: Option<String>) -> Result<()> {
        let _flight = self.reinit.lock().await;
        if self.session() != seen {
            return Ok(()); // someone else already re-initialized
        }
        crate::log_warn!("upstream {} forgot our session (HTTP 404); re-initializing", self.name);
        self.initialize().await
    }

    /// Common POST wiring: auth headers, the negotiated session id, and the
    /// protocol-version header the spec requires on every non-initialize call.
    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(&self.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(body);
        for (k, v) in &self.headers {
            req = req.header(k, v.expose_secret());
        }
        if let Some(sid) = self.session() {
            req = req.header("Mcp-Session-Id", sid);
        }
        req
    }

    /// The OAuth bearer to attach, if this upstream is OAuth-protected. A
    /// valid cached token is returned without any lock a concurrent fetch
    /// could be holding; see [`OAuthClient::bearer`].
    async fn bearer(&self) -> Result<Option<String>> {
        match &self.oauth {
            Some(o) => Ok(Some(o.bearer().await?)),
            None => Ok(None),
        }
    }

    /// `deadline` (overlay `timeout_s`) is applied per-request via reqwest; on
    /// expiry the Err carries a typed [`TimeoutElapsed`] so dispatch renders an
    /// agent-facing timeout instead of a protocol error. An expired session is
    /// re-initialized once and the request retried.
    async fn request_deadline(
        &self,
        method: &str,
        params: Value,
        deadline: Option<std::time::Duration>,
    ) -> Result<Value> {
        // The frame is built ONCE and its id rewritten for the retry: cloning
        // the params (tool arguments can be large) on every call, for a branch
        // taken only on an expired session, is pure waste.
        let mut frame = json!({"jsonrpc": "2.0", "id": 0, "method": method, "params": params});
        let seen = self.session();
        match self.request_once(method, &mut frame, deadline).await {
            Err(e) if e.downcast_ref::<SessionExpired>().is_some() => {
                self.reinitialize(seen).await?;
                self.request_once(method, &mut frame, deadline).await
            }
            r => r,
        }
    }

    /// Send `frame` (whose `id` this stamps) and return its result.
    async fn request_once(
        &self,
        method: &str,
        frame: &mut Value,
        deadline: Option<std::time::Duration>,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        frame["id"] = json!(id);
        let bearer = self.bearer().await?;
        let had_session = self.session().is_some();
        let mut rb = self.post(frame);
        if let Some(b) = bearer {
            rb = rb.bearer_auth(b);
        }
        if let Some(d) = deadline {
            rb = rb.timeout(d);
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) if deadline.is_some() && e.is_timeout() => {
                return Err(anyhow::Error::new(TimeoutElapsed {
                    secs: deadline.unwrap_or_default().as_secs(),
                }));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("POST {method} to {}", self.url));
            }
        };
        // capture the session id assigned at initialize
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *lock(&self.session_id) = Some(sid.to_string());
        }
        let is_sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false);
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND && had_session && method != "initialize" {
            return Err(anyhow::Error::new(SessionExpired));
        }
        if !status.is_success() {
            bail!("upstream {} returned HTTP {status} on {method}: {}", self.name, text.trim());
        }
        // a catalog-change notification can ride along in an SSE reply
        if is_sse && text.contains("notifications/tools/list_changed") {
            self.stale.store(true, Ordering::Relaxed);
            crate::log_warn!(
                "upstream {} announced tools/list_changed; the catalog is a startup snapshot — restart to pick it up",
                self.name
            );
        }
        let msg = if is_sse {
            sse_response(&text, id)?
        } else {
            serde_json::from_str::<Value>(&text)
                .with_context(|| format!("upstream {} sent non-JSON on {method}", self.name))?
        };
        if let Some(err) = msg.get("error") {
            bail!("upstream {} error on {method}: {err}", self.name);
        }
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        // notifications carry no id; the server replies 202 Accepted, no body
        let bearer = self.bearer().await?;
        let mut rb = self.post(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
        if let Some(b) = bearer {
            rb = rb.bearer_auth(b);
        }
        let resp = rb
            .send()
            .await
            .with_context(|| format!("POST notification {method} to {}", self.url))?;
        if !resp.status().is_success() {
            bail!("upstream {} rejected notification {method}: HTTP {}", self.name, resp.status());
        }
        Ok(())
    }
}

/// Pull the JSON-RPC response with `id` out of an SSE body. SSE frames are
/// blank-line separated; payload lines start with `data:`. We parse each frame's
/// data as JSON and return the first that is our response (matching id, or any
/// message carrying result/error if the server didn't echo the id).
fn sse_response(body: &str, id: i64) -> Result<Value> {
    let mut data = String::new();
    let mut fallback: Option<Value> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        } else if line.trim().is_empty() && !data.is_empty() {
            if let Some(v) = frame_match(&data, id, &mut fallback) {
                return Ok(v);
            }
            data.clear();
        }
    }
    // trailing frame with no terminating blank line
    if !data.is_empty() {
        if let Some(v) = frame_match(&data, id, &mut fallback) {
            return Ok(v);
        }
    }
    fallback.ok_or_else(|| anyhow!("no JSON-RPC response found in SSE stream"))
}

/// Parse one SSE frame's data: return it if it is the response we want (id
/// matches, by the same tolerant rule the stdio client uses — a server that
/// echoes `"7"` for `7` is matched, not mistaken for someone else's reply);
/// otherwise remember the first result/error-bearing message as a fallback
/// (for servers that don't echo the request id). Non-JSON is ignored.
fn frame_match(data: &str, want: i64, fallback: &mut Option<Value>) -> Option<Value> {
    let v = serde_json::from_str::<Value>(data).ok()?;
    if response_id(&v) == Some(want) {
        return Some(v);
    }
    if fallback.is_none() && (v.get("result").is_some() || v.get("error").is_some()) {
        *fallback = Some(v);
    }
    None
}

impl crate::upstream::McpRpc for HttpUpstream {
    fn rpc_name(&self) -> &str {
        &self.name
    }

    async fn rpc(&self, method: &str, params: Value, deadline: Option<std::time::Duration>) -> Result<Value> {
        self.request_deadline(method, params, deadline).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_response_from_sse_frames() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\n\n";
        let v = sse_response(body, 3).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn skips_unrelated_frames_and_finds_matching_id() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":42}\n\n";
        let v = sse_response(body, 5).unwrap();
        assert_eq!(v["result"], json!(42));
    }

    #[test]
    fn sse_ids_match_by_the_same_rule_as_stdio() {
        // a server echoing the id as a string must still be matched to ITS
        // request, not fall through to the "first result-bearing frame"
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":\"4\",\"result\":{\"ok\":1}}\n\n";
        assert_eq!(sse_response(body, 4).unwrap()["result"]["ok"], json!(1));
    }

    #[test]
    fn parses_plain_json_via_from_str() {
        // the application/json path is just serde_json; sanity-check shape
        let v: Value = serde_json::from_str("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}").unwrap();
        assert!(v.get("result").is_some());
    }
}
