//! Streamable-HTTP MCP client: proxy a *remote* MCP server (one reachable at a
//! URL, not spawned as a subprocess). Speaks the 2025-06-18 transport — POST
//! JSON-RPC to the single MCP endpoint, capture the `Mcp-Session-Id` at
//! initialize and echo it back, accept either an `application/json` reply or a
//! `text/event-stream` (SSE) one.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::config::{expand_env, UpstreamConfig};
use crate::oauth::OAuthClient;
use crate::upstream::{TimeoutElapsed, PROTOCOL_VERSION};

/// Per-request ceiling, matching the stdio client's REQUEST_TIMEOUT. Without it
/// a slow or stream-holding remote MCP server would stall min-mcp forever:
/// reqwest has no default timeout, and reading an SSE reply drains the whole
/// body — which a server keeping the stream open never ends. Covers connect +
/// send + body read (reqwest's `timeout` is whole-request).
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub struct HttpUpstream {
    pub name: String,
    /// How this upstream's tool results are serialized to the agent (`json` =
    /// compact re-encode of JSON text blocks; `raw` = byte-for-byte).
    pub result_format: crate::config::ResultFormat,
    client: reqwest::Client,
    url: String,
    /// Static auth headers (values with `${VAR}` expanded from the environment).
    headers: Vec<(String, String)>,
    /// OAuth client-credentials, if this upstream is OAuth-protected.
    oauth: Option<OAuthClient>,
    session_id: Option<String>,
    next_id: i64,
}

impl HttpUpstream {
    pub async fn connect(cfg: &UpstreamConfig) -> Result<Self> {
        let url = cfg
            .url
            .as_ref()
            .ok_or_else(|| anyhow!("http upstream {} needs `url`", cfg.name))?
            .clone();
        let headers = cfg
            .headers
            .iter()
            .map(|(k, v)| Ok((k.clone(), expand_env(v)?)))
            .collect::<Result<Vec<_>>>()?;
        let oauth = cfg.oauth.as_ref().map(OAuthClient::new).transpose()?;
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("building HTTP client for upstream")?;
        let mut up = HttpUpstream {
            name: cfg.name.clone(),
            result_format: cfg.result_format(),
            client,
            url,
            headers,
            oauth,
            session_id: None,
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
        .with_context(|| format!("initializing http upstream {}", cfg.name))?;
        up.notify("notifications/initialized", json!({})).await?;
        Ok(up)
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
            req = req.header(k, v);
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        req
    }

    /// The OAuth bearer to attach, if this upstream is OAuth-protected.
    async fn bearer(&mut self) -> Result<Option<String>> {
        match self.oauth.as_mut() {
            Some(o) => Ok(Some(o.bearer().await?)),
            None => Ok(None),
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_deadline(method, params, None).await
    }

    /// `deadline` (overlay `timeout_s`) is applied per-request via reqwest; on
    /// expiry the Err carries a typed [`TimeoutElapsed`] so dispatch renders an
    /// agent-facing timeout instead of a protocol error. HTTP cancellation is
    /// frame-safe (one request per exchange, no shared stream).
    async fn request_deadline(
        &mut self,
        method: &str,
        params: Value,
        deadline: Option<std::time::Duration>,
    ) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let bearer = self.bearer().await?;
        let mut rb = self.post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
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
            self.session_id = Some(sid.to_string());
        }
        let is_sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false);
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("upstream {} returned HTTP {status} on {method}: {}", self.name, text.trim());
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

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
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
    let want = json!(id);
    let mut data = String::new();
    let mut fallback: Option<Value> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        } else if line.trim().is_empty() && !data.is_empty() {
            if let Some(v) = frame_match(&data, &want, &mut fallback) {
                return Ok(v);
            }
            data.clear();
        }
    }
    // trailing frame with no terminating blank line
    if !data.is_empty() {
        if let Some(v) = frame_match(&data, &want, &mut fallback) {
            return Ok(v);
        }
    }
    fallback.ok_or_else(|| anyhow!("no JSON-RPC response found in SSE stream"))
}

/// Parse one SSE frame's data: return it if it is the response we want (id
/// matches); otherwise remember the first result/error-bearing message as a
/// fallback (for servers that don't echo the request id). Non-JSON is ignored.
fn frame_match(data: &str, want: &Value, fallback: &mut Option<Value>) -> Option<Value> {
    let v = serde_json::from_str::<Value>(data).ok()?;
    if v.get("id") == Some(want) {
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

    async fn rpc(
        &mut self,
        method: &str,
        params: Value,
        deadline: Option<std::time::Duration>,
    ) -> Result<Value> {
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
    fn parses_plain_json_via_from_str() {
        // the application/json path is just serde_json; sanity-check shape
        let v: Value = serde_json::from_str("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}").unwrap();
        assert!(v.get("result").is_some());
    }
}
