//! The 2026-07-28 revision end to end, over the real binary. The served side
//! comes from rmcp, so nothing in min-mcp names it and a dependency bump could
//! change it silently; these pin it. The last test is the other direction:
//! min-mcp proxying min-mcp, where the probe finds a modern upstream.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_minmcp");
const VERSION: &str = "2026-07-28";

fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "e2e", "version": "0"},
    })
}

/// Send each `(method, params)` to one stdio server with NO handshake.
fn stdio_exchange(config: &str, requests: &[(&str, Value)]) -> Vec<Value> {
    let mut child = Command::new(BIN)
        .args(["serve", "--config", config])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn minmcp serve");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut replies = Vec::new();
    for (i, (method, params)) in requests.iter().enumerate() {
        let mut params = params.clone();
        params["_meta"] = meta();
        let frame = json!({"jsonrpc": "2.0", "id": i + 1, "method": method, "params": params});
        writeln!(stdin, "{frame}").unwrap();
        let line = lines.next().expect("a reply").expect("readable");
        replies.push(serde_json::from_str(&line).expect("JSON reply"));
    }
    let _ = child.kill();
    let _ = child.wait();
    replies
}

#[test]
fn stdio_serves_a_client_that_never_initializes() {
    let replies = stdio_exchange(
        "bench/bigapi.yaml",
        &[
            ("server/discover", json!({})),
            ("tools/list", json!({})),
            ("tools/call", json!({"name": "search_tools", "arguments": {"query": "create a widget", "k": 1}})),
        ],
    );
    let versions = replies[0]["result"]["supportedVersions"].as_array().expect("a DiscoverResult");
    assert!(versions.contains(&json!(VERSION)), "{versions:?}");

    let list = &replies[1]["result"];
    assert_eq!(list["resultType"], "complete");
    assert!(list["ttlMs"].is_u64(), "list results are cacheable: {list}");
    assert!(list["cacheScope"].is_string(), "{list}");
    let names: Vec<&str> = list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["search_tools", "get_tool_details", "call_tool"]);

    let text = replies[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("big.widgets/create"), "{text}");
}

#[cfg(unix)]
mod http {
    use super::*;
    use std::net::TcpListener;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// POST one modern request; returns (status, JSON-RPC message).
    fn post(port: u16, method: &str, params: Value, name: Option<&str>) -> (u16, Value) {
        let mut params = params;
        params["_meta"] = meta();
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let url = format!("http://127.0.0.1:{port}/");
        let mut cmd = Command::new("curl");
        cmd.args(["-s", "-w", "\n%{http_code}", "-X", "POST", &url, "-d", &body])
            .args(["-H", "Content-Type: application/json"])
            .args(["-H", "Accept: application/json, text/event-stream"])
            .args(["-H", &format!("MCP-Protocol-Version: {VERSION}")])
            .args(["-H", &format!("Mcp-Method: {method}")]);
        if let Some(n) = name {
            cmd.args(["-H", &format!("Mcp-Name: {n}")]);
        }
        let out = String::from_utf8(cmd.output().expect("curl").stdout).unwrap();
        let (payload, status) = out.rsplit_once('\n').unwrap();
        let json_text = payload.lines().find_map(|l| l.strip_prefix("data:")).unwrap_or(payload);
        (status.trim().parse().unwrap(), serde_json::from_str(json_text.trim()).unwrap_or(Value::Null))
    }

    fn wait_for(port: u16) {
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("server never listened");
    }

    #[test]
    fn http_serves_a_client_that_never_initializes() {
        let port = free_port();
        let mut server = Command::new(BIN)
            .args(["serve", "--config", "bench/bigapi.yaml", "--http", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        wait_for(port);

        let (status, discover) = post(port, "server/discover", json!({}), None);
        assert_eq!(status, 200, "{discover}");
        assert!(discover["result"]["supportedVersions"].as_array().unwrap().contains(&json!(VERSION)));

        let (status, list) = post(port, "tools/list", json!({}), None);
        assert_eq!(status, 200, "{list}");
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);

        let args = json!({"name": "search_tools", "arguments": {"query": "create a widget"}});
        let (status, call) = post(port, "tools/call", args.clone(), Some("search_tools"));
        assert_eq!(status, 200, "{call}");
        assert!(call["result"]["content"][0]["text"].as_str().unwrap().contains("big.widgets/create"));

        // a gateway routing on the header must not be able to diverge from the body
        let (status, mismatch) = post(port, "tools/call", args, Some("call_tool"));
        assert_eq!(status, 400, "{mismatch}");
        assert_eq!(mismatch["error"]["code"], -32020);

        let _ = server.kill();
        let _ = server.wait();
    }
}

#[test]
fn a_modern_upstream_is_proxied_without_a_handshake() {
    // The inner min-mcp answers `server/discover`, so the outer one must take
    // the modern path; tools arriving at all proves the requests were accepted.
    let inner = std::fs::canonicalize("bench/bigapi.yaml").unwrap();
    let dir = std::env::temp_dir().join(format!("minmcp_modern_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("outer.yaml");
    let yaml = format!(
        "mode: passthrough\nupstreams:\n  - name: inner\n    command: {BIN:?}\n    args: [serve, --config, {inner:?}]\n"
    );
    std::fs::write(&config, yaml).unwrap();

    let out = Command::new(BIN)
        .args(["call", "--config", config.to_str().unwrap(), "inner.search_tools"])
        .args(["--args", r#"{"query":"create a widget","k":1}"#])
        .env("MINMCP_LOG", "debug")
        .output()
        .expect("run minmcp call");
    let _ = std::fs::remove_dir_all(&dir);
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("big.widgets/create"), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("Modern"), "the probe should have found a modern upstream: {stderr}");
}
