//! Streamable HTTP transport, end to end against the real binary.
//!
//! `minmcp serve --http` was a documented feature with no automated coverage:
//! a regression there would have shipped silently. This drives the 2025-06-18
//! transport with a hand-rolled client (POST JSON-RPC, capture `Mcp-Session-Id`
//! at initialize, echo it back, accept either an `application/json` or an
//! `text/event-stream` reply) using `curl`, so the test needs no HTTP crate in
//! dev-dependencies.
//!
//! Offline: the upstream is the bundled mini OpenAPI spec.

#![cfg(unix)] // uses curl and a POSIX kill; the stdio suites cover other platforms

use std::process::Command;

mod common;
use common::{HttpServer, INIT, TOKEN_READ, TOKEN_WRITE};

/// The HTTP suite's view of a running server: `HttpServer` (spawn, OS-assigned
/// port, kill on drop) plus the curl-driven JSON-RPC POSTs these tests need.
struct Server {
    inner: HttpServer,
    addr: String,
}

impl Server {
    fn start() -> Self {
        Self::start_with("tests/fixtures/ci-server.yaml")
    }

    fn start_with(config: &str) -> Self {
        let inner = HttpServer::start("127.0.0.1", &["--config", config]);
        let addr = format!("127.0.0.1:{}", inner.port);
        Server { inner, addr }
    }

    /// One JSON-RPC POST. Returns (body, response headers).
    fn post(&self, body: &str, session: Option<&str>) -> (String, String) {
        self.post_as(body, session, None)
    }

    /// One JSON-RPC POST with an optional bearer token.
    fn post_as(&self, body: &str, session: Option<&str>, bearer: Option<&str>) -> (String, String) {
        // include the port: tests share a process, so a shared filename races
        let hdr_file = std::env::temp_dir()
            .join(format!("minmcp_http_hdrs_{}_{}", std::process::id(), self.inner.port));
        let mut args: Vec<String> = vec![
            "-s".into(), "--max-time".into(), "20".into(),
            "-D".into(), hdr_file.to_string_lossy().into_owned(),
            "-H".into(), "Content-Type: application/json".into(),
            // the spec requires the client to accept both reply shapes
            "-H".into(), "Accept: application/json, text/event-stream".into(),
            "-H".into(), "MCP-Protocol-Version: 2025-06-18".into(),
        ];
        if let Some(sid) = session {
            args.push("-H".into());
            args.push(format!("Mcp-Session-Id: {sid}"));
        }
        if let Some(tok) = bearer {
            args.push("-H".into());
            args.push(format!("Authorization: Bearer {tok}"));
        }
        args.push("-d".into());
        args.push(body.into());
        args.push(format!("http://{}/", self.addr));

        let out = Command::new("curl").args(&args).output().expect("curl");
        let headers = std::fs::read_to_string(&hdr_file).unwrap_or_default();
        let _ = std::fs::remove_file(&hdr_file);
        (String::from_utf8_lossy(&out.stdout).into_owned(), headers)
    }
}

/// Pull the JSON payload out of either a plain body or an SSE `data:` frame.
/// (rmcp replies with SSE here, and its first frame is an empty keep-alive.)
fn payload(body: &str) -> String {
    let sse: String = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:").map(str::trim))
        .collect::<Vec<_>>()
        .join("");
    if sse.is_empty() { body.trim().to_string() } else { sse }
}

#[test]
fn streamable_http_serves_the_same_minified_surface_as_stdio() {
    let s = Server::start();

    // 1. initialize — must return capabilities and assign a session id
    let (body, headers) = s.post(INIT, None);
    let init = payload(&body);
    assert!(init.contains("\"protocolVersion\""), "initialize reply: {init}");
    assert!(init.contains("min-mcp"), "serverInfo should name min-mcp: {init}");
    let session = session_of(&headers);
    assert!(!session.is_empty());

    // the spec requires the initialized notification before normal traffic
    s.post(
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        Some(&session),
    );

    // 2. tools/list — the minified 3-tool surface, same as stdio
    let (body, _) = s.post(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#, Some(&session));
    let list = payload(&body);
    for tool in ["search_tools", "get_tool_details", "call_tool"] {
        assert!(list.contains(tool), "missing {tool} over HTTP: {list}");
    }

    // 3. tools/call — search works, proving dispatch is wired on this transport
    let (body, _) = s.post(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_tools","arguments":{"query":"ping"}}}"#,
        Some(&session),
    );
    let called = payload(&body);
    assert!(
        called.contains("fixture.GetPing"),
        "search over HTTP should find the fixture tool: {called}"
    );
}

fn session_of(headers: &str) -> String {
    headers
        .lines()
        .filter_map(|l| l.trim().split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
        .map(|(_, value)| value.trim().to_string())
        .expect("server must assign an Mcp-Session-Id at initialize")
}

#[test]
fn http_requires_a_bearer_and_scopes_each_request_by_it() {
    // The per-request identity model: with `auth:` configured, an HTTP request
    // without a token is refused (401 + challenge), and two clients with
    // different tokens see two different surfaces from ONE process.
    let s = Server::start_with("tests/fixtures/e2e-scopes.yaml");

    // `-D -` dumps the response headers to stdout ahead of the status code
    let out = Command::new("curl")
        .args([
            "-s", "-D", "-", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "20",
            "-H", "Content-Type: application/json",
            "-H", "Accept: application/json, text/event-stream",
            "-d", INIT, &format!("http://{}/", s.addr),
        ])
        .output()
        .expect("curl");
    let anon = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(anon.trim_end().ends_with("401"), "no token must be 401, got: {anon}");
    assert!(anon.to_lowercase().contains("www-authenticate: bearer"), "401 carries a challenge: {anon}");

    // read token: its own session, sees list, not create
    let (body, headers) = s.post_as(INIT, None, Some(TOKEN_READ));
    assert!(payload(&body).contains("\"protocolVersion\""), "initialize with a valid token: {body}");
    let read_session = session_of(&headers);
    s.post_as(r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#, Some(&read_session), Some(TOKEN_READ));
    let (body, _) = s.post_as(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_tools","arguments":{"query":"widget"}}}"#,
        Some(&read_session),
        Some(TOKEN_READ),
    );
    let hits = payload(&body);
    assert!(hits.contains("widgets/list"), "read scope sees list: {hits}");
    assert!(!hits.contains("widgets/create"), "read scope must NOT see create: {hits}");

    // write token: a second session on the same server sees the other half
    let (_, headers) = s.post_as(INIT, None, Some(TOKEN_WRITE));
    let write_session = session_of(&headers);
    s.post_as(r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#, Some(&write_session), Some(TOKEN_WRITE));
    let (body, _) = s.post_as(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_tools","arguments":{"query":"widget"}}}"#,
        Some(&write_session),
        Some(TOKEN_WRITE),
    );
    let hits = payload(&body);
    assert!(hits.contains("widgets/create"), "write scope sees create: {hits}");
    assert!(!hits.contains("widgets/list"), "write scope must NOT see list: {hits}");

    // and a tampered token on an existing session is refused, not downgraded
    let mut bad = TOKEN_READ.to_string();
    bad.pop();
    bad.push('X');
    let out = Command::new("curl")
        .args([
            "-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "20",
            "-H", "Content-Type: application/json",
            "-H", "Accept: application/json, text/event-stream",
            "-H", &format!("Mcp-Session-Id: {read_session}"),
            "-H", &format!("Authorization: Bearer {bad}"),
            "-d", r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#,
            &format!("http://{}/", s.addr),
        ])
        .output()
        .expect("curl");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "401", "a bad token is 401 even with a live session");
}

#[test]
fn http_rejects_a_foreign_origin() {
    // DNS-rebinding defence: the transport validates Origin. A browser-style
    // cross-origin POST must not be served.
    let s = Server::start();
    let out = Command::new("curl")
        .args([
            "-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "20",
            "-H", "Content-Type: application/json",
            "-H", "Accept: application/json, text/event-stream",
            "-H", "Origin: https://evil.example",
            "-d", r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"x","version":"0"}}}"#,
            &format!("http://{}/", s.addr),
        ])
        .output()
        .expect("curl");
    let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        code.starts_with('4'),
        "a foreign Origin should be refused with 4xx, got {code}"
    );
}
