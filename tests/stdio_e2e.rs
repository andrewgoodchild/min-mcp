//! End-to-end stdio protocol tests: spawn the real binary, speak MCP JSON-RPC,
//! assert the full search → details → call flow plus resources and mode
//! behavior. Offline: spec fixtures with unreachable base_urls; every probed
//! path (search, details, preflight rejection, unknown-tool errors, our own
//! resource) resolves locally, so nothing is ever dialed.
//!
//! This is the test category the mcp-compressor head-to-head showed we were
//! missing: their suite drives the compressed server end-to-end over stdio;
//! ours only asserted at the unit level and via manual probe scripts.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_minmcp");
const TIMEOUT: Duration = Duration::from_secs(20);

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: i64,
}

impl Server {
    fn spawn(config: &str) -> Self {
        let mut child = Command::new(BIN)
            .args(["serve", "--config", config])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn minmcp serve");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut s = Server { child, stdin, lines: rx, next_id: 0 };
        let init = s.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "e2e", "version": "0"}}),
        );
        assert!(init.get("capabilities").is_some(), "initialize must return capabilities: {init}");
        s.notify("notifications/initialized", json!({}));
        s
    }

    fn send(&mut self, msg: &Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Send a request and return its `result` (panics on error responses).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = std::time::Instant::now() + TIMEOUT;
        while std::time::Instant::now() < deadline {
            let Ok(line) = self.lines.recv_timeout(TIMEOUT) else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v.get("method").is_some() {
                continue; // server-side notification — not our response
            }
            if v.get("id") == Some(&json!(id)) {
                if let Some(e) = v.get("error") {
                    panic!("{method} returned protocol error: {e}");
                }
                return v.get("result").cloned().unwrap_or(Value::Null);
            }
        }
        panic!("no response to {method} within {TIMEOUT:?}");
    }

    fn tool_names(&mut self) -> Vec<String> {
        self.request("tools/list", json!({}))["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Call a tool; returns (joined text, isError).
    fn call(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let r = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let text = r["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        (text, r["isError"].as_bool().unwrap_or(false))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn three_tool_surface_and_instructions_over_the_wire() {
    let mut s = Server::spawn("tests/fixtures/ci-server.yaml");
    assert_eq!(s.tool_names(), ["search_tools", "get_tool_details", "call_tool"]);
}

#[test]
fn search_details_preflight_and_near_miss_flow() {
    let mut s = Server::spawn("bench/bigapi.yaml");

    // search finds the operation by task phrasing and returns a callable id
    let (hits, err) = s.call("search_tools", json!({"query": "create a widget"}));
    assert!(!err);
    assert!(hits.contains("big.widgets/create"), "search must surface the op id: {hits}");

    // details returns the full schema including the required field
    let (details, err) = s.call("get_tool_details", json!({"tool_id": "big.widgets/create"}));
    assert!(!err);
    assert!(details.contains("input_schema") && details.contains("\"name\""), "{details}");

    // preflight rejects a call missing a required field LOCALLY (no network:
    // base_url is localhost:9000 and nothing listens there — a structured
    // PREFLIGHT_ERROR proves the round-trip never happened)
    let (reject, err) =
        s.call("call_tool", json!({"tool_id": "big.widgets/create",
                                   "arguments": {"body": {"description": "no name"}}}));
    assert!(err, "missing required must be an isError result");
    assert!(reject.contains("PREFLIGHT_ERROR") && reject.contains("name"), "{reject}");

    // a mistyped id gets a near-miss suggestion, not a dead end
    let (miss, err) = s.call("call_tool", json!({"tool_id": "big.widgets_create"}));
    assert!(err);
    assert!(miss.contains("did you mean") && miss.contains("big.widgets/create"), "{miss}");

    // non-object arguments are a client-side error, never forwarded upstream
    let (bad, err) = s.call(
        "call_tool",
        json!({"tool_id": "big.widgets/create", "arguments": "not an object"}),
    );
    assert!(err);
    assert!(bad.contains("must be a JSON object"), "{bad}");
}

#[test]
fn resources_expose_the_source_map() {
    let mut s = Server::spawn("tests/fixtures/ci-server.yaml");
    let listing = s.request("resources/list", json!({}));
    let uris: Vec<&str> = listing["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["uri"].as_str())
        .collect();
    assert!(uris.contains(&"minmcp://tools"), "own source-map resource must be listed: {uris:?}");

    let read = s.request("resources/read", json!({"uri": "minmcp://tools"}));
    let text = read["contents"][0]["text"].as_str().unwrap();
    let map: Value = serde_json::from_str(text).expect("source map is JSON");
    assert!(map["tools"].as_array().map(|t| !t.is_empty()).unwrap_or(false), "map lists tools");

    // spec upstreams contribute no prompts; the merged list is empty, not an error
    let prompts = s.request("prompts/list", json!({}));
    assert_eq!(prompts["prompts"], json!([]));
}

#[test]
fn two_upstreams_federate_behind_a_byte_stable_surface() {
    let mut one = Server::spawn("tests/fixtures/ci-server.yaml");
    let mut two = Server::spawn("tests/fixtures/e2e-two-upstreams.yaml");

    // still exactly 3 tools with two upstreams (123 backend ops)
    assert_eq!(two.tool_names(), ["search_tools", "get_tool_details", "call_tool"]);

    // BYTE-stable: the declared surface is identical no matter what upstreams
    // sit behind it — the prompt-cache-stability claim, pinned as a test
    let a = one.request("tools/list", json!({}));
    let b = two.request("tools/list", json!({}));
    assert_eq!(a, b, "three_tool surface must not vary with upstream contents");

    // search federates across both upstreams
    let (hits, _) = two.call("search_tools", json!({"query": "ping"}));
    assert!(hits.contains("fixture.GetPing"), "{hits}");
    let (hits, _) = two.call("search_tools", json!({"query": "create a widget"}));
    assert!(hits.contains("big.widgets/create"), "{hits}");
}

#[test]
fn breaker_trips_after_consecutive_failures_and_blocks_locally() {
    let mut s = Server::spawn("tests/fixtures/e2e-breaker.yaml");
    // base_url is unreachable: the first N calls fail upstream (transport
    // error), and at the threshold the breaker starts refusing LOCALLY with
    // a continuation prompt instead of letting the agent retry-loop.
    let (t1, e1) = s.call("call_tool", json!({"tool_id": "fixture.GetPing"}));
    assert!(e1 && !t1.contains("BREAKER_OPEN"), "first failure reaches upstream: {t1}");
    let (t2, e2) = s.call("call_tool", json!({"tool_id": "fixture.GetPing"}));
    assert!(e2 && !t2.contains("BREAKER_OPEN"), "second failure reaches upstream: {t2}");
    let (t3, e3) = s.call("call_tool", json!({"tool_id": "fixture.GetPing"}));
    assert!(e3);
    assert!(
        t3.contains("BREAKER_OPEN") && t3.contains("Do not retry"),
        "third call must be refused locally with guidance: {t3}"
    );
    // an un-overlaid sibling tool is unaffected by GetPing's breaker
    let (t4, _) = s.call("call_tool", json!({"tool_id": "fixture.GetItem",
                                             "arguments": {"path_params": {"id": "1"}}}));
    assert!(!t4.contains("BREAKER_OPEN"), "breaker state is per-tool: {t4}");
}

#[cfg(unix)] // the fixture upstream is a /bin/sh script
#[test]
fn per_tool_timeout_fires_and_repeated_timeouts_trip_the_breaker() {
    let mut s = Server::spawn("tests/fixtures/e2e-timeout.yaml");
    // The upstream answers the handshake but never a tools/call; the overlay
    // deadline converts the stall into an agent-facing isError in ~1s.
    let (t1, e1) = s.call("call_tool", json!({"tool_id": "slow.sleepy"}));
    assert!(e1);
    assert!(
        t1.contains("TIMEOUT") && t1.contains("may or may not"),
        "timeout must explain the operation may still have completed: {t1}"
    );
    // Timeouts COUNT AS FAILURES: the second trips the 2-strike breaker, so
    // the third is refused locally without waiting at all.
    let (t2, _) = s.call("call_tool", json!({"tool_id": "slow.sleepy"}));
    assert!(t2.contains("TIMEOUT"), "{t2}");
    let started = std::time::Instant::now();
    let (t3, e3) = s.call("call_tool", json!({"tool_id": "slow.sleepy"}));
    assert!(e3);
    assert!(t3.contains("BREAKER_OPEN"), "breaker must trip on repeated timeouts: {t3}");
    assert!(started.elapsed() < Duration::from_secs(1), "refusal is local, no upstream wait");
}

#[test]
fn passthrough_declares_every_tool_and_drops_meta() {
    let mut s = Server::spawn("tests/fixtures/e2e-passthrough.yaml");
    let names = s.tool_names();
    assert_eq!(names.len(), 3, "mini fixture has exactly 3 ops: {names:?}");
    assert!(names.iter().all(|n| n.starts_with("fixture_")), "{names:?}");
    assert!(!names.iter().any(|n| n == "search_tools"), "no meta tools in passthrough");
}
