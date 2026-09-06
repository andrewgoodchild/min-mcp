//! Shared test fixtures. Every integration suite runs the same binary against
//! the same throwaway tokens, so they live here once: a rotated fixture secret
//! or a bumped protocol version is one edit, not four that can drift apart.
//!
//! Each suite uses a subset, so `dead_code` is expected per-crate.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_minmcp");

/// HS256 tokens signed with the fixture secret `test-secret` (see
/// `tests/fixtures/e2e-scopes.yaml`), `exp` in 2100. Hardcoded rather than
/// minted at runtime because `jsonwebtoken` is a dependency of the binary, not
/// of the test crates.
pub const TOKEN_WRITE: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzY29wZSI6InN0b3JlLndyaXRlIiwiZXhwIjo0MTAyNDQ0ODAwfQ.y7gSKROF0_0tej7CTwv2pwqds5YKn6UqPqUojnZqXow";
pub const TOKEN_READ: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzY29wZSI6InN0b3JlLnJlYWQiLCJleHAiOjQxMDI0NDQ4MDB9.EuAvpxVlc9I6NeYBSpMJEH7ThR4guKjGNx-2KyX7wjg";

/// The MCP handshake body every HTTP suite opens with.
pub const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}"#;

/// Run the binary to completion: (stdout, stderr, success).
pub fn run(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(BIN).args(args).output().expect("run minmcp");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// A running `minmcp serve --http`, killed on drop.
pub struct HttpServer {
    child: Child,
    /// The port the OS actually assigned.
    pub port: u16,
}

impl HttpServer {
    /// Start on `bind` with an OS-assigned port, and learn that port from the
    /// startup banner. Port 0 rather than a hardcoded one: suites run in
    /// parallel, and two tests sharing a fixed port means the second fails to
    /// bind while its readiness probe passes against the first — then whichever
    /// finishes first kills the server the other is still using.
    pub fn start(bind: &str, extra: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(["serve", "--http", &format!("{bind}:0")])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn minmcp serve --http");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            let line = match rx.recv_timeout(wait) {
                Ok(l) => l,
                Err(_) => {
                    // reap it here: `Drop` never runs for a value we are
                    // panicking out of before returning
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("no 'listening on' banner within the deadline");
                }
            };
            if let Some(rest) = line.split("listening on http://").nth(1) {
                let port = rest
                    .trim_end_matches('/')
                    .trim()
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse::<u16>().ok())
                    .expect("port in the banner");
                return HttpServer { child, port };
            }
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
