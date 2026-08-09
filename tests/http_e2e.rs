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

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_minmcp");

struct Server {
    child: Child,
    addr: String,
}

impl Server {
    /// Each test gets its OWN port: two servers on one port means the second
    /// fails to bind, the readiness probe passes against the first, and
    /// whichever test finishes first kills the server the other is still using.
    fn start(port: u16) -> Self {
        let addr = format!("127.0.0.1:{port}");
        let child = Command::new(BIN)
            .args(["serve", "--http", &addr, "--config", "tests/fixtures/ci-server.yaml"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn minmcp serve --http");
        let s = Server { child, addr };
        s.await_ready();
        s
    }

    /// Poll until the port answers, so the test never races the bind.
    fn await_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let up = Command::new("curl")
                .args(["-s", "-o", "/dev/null", "--max-time", "2", &format!("http://{}/", self.addr)])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if up {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("minmcp serve --http never became ready on {}", self.addr);
    }

    /// One JSON-RPC POST. Returns (body, response headers).
    fn post(&self, body: &str, session: Option<&str>) -> (String, String) {
        // include the port: tests share a process, so a shared filename races
        let hdr_file = std::env::temp_dir()
            .join(format!("minmcp_http_hdrs_{}_{}", std::process::id(), self.addr.replace([':', '.'], "_")));
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
        args.push("-d".into());
        args.push(body.into());
        args.push(format!("http://{}/", self.addr));

        let out = Command::new("curl").args(&args).output().expect("curl");
        let headers = std::fs::read_to_string(&hdr_file).unwrap_or_default();
        let _ = std::fs::remove_file(&hdr_file);
        (String::from_utf8_lossy(&out.stdout).into_owned(), headers)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    let s = Server::start(38731);

    // 1. initialize — must return capabilities and assign a session id
    let (body, headers) = s.post(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"http-e2e","version":"0"}}}"#,
        None,
    );
    let init = payload(&body);
    assert!(init.contains("\"protocolVersion\""), "initialize reply: {init}");
    assert!(init.contains("min-mcp"), "serverInfo should name min-mcp: {init}");
    let session = headers
        .lines()
        .filter_map(|l| l.trim().split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
        .map(|(_, value)| value.trim().to_string())
        .expect("server must assign an Mcp-Session-Id at initialize");
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

#[test]
fn http_rejects_a_foreign_origin() {
    // DNS-rebinding defence: the transport validates Origin. A browser-style
    // cross-origin POST must not be served.
    let s = Server::start(38732);
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
