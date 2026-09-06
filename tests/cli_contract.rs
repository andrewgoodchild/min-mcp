//! Binary-level CLI contract tests (the mcp-compressor suite's `cli_binary.rs`
//! category, which min-mcp lacked): flags, exit codes, error messages, and the
//! serve banner. All offline.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

mod common;
use common::{run, HttpServer, BIN, INIT, TOKEN_READ};

#[test]
fn version_flag_prints_version() {
    let (stdout, _, ok) = run(&["--version"]);
    assert!(ok);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
}

#[test]
fn missing_config_exits_nonzero_with_a_clear_message() {
    let (_, stderr, ok) = run(&["inspect", "--config", "no/such/file.yaml"]);
    assert!(!ok, "nonexistent config must be a hard error");
    assert!(stderr.contains("no/such/file.yaml"), "error names the path: {stderr}");
}

#[test]
fn invalid_yaml_exits_nonzero() {
    let dir = std::env::temp_dir();
    let bad = dir.join(format!("minmcp_bad_{}.yaml", std::process::id()));
    std::fs::write(&bad, "mode: [unclosed").unwrap();
    let (_, stderr, ok) = run(&["inspect", "--config", bad.to_str().unwrap()]);
    assert!(!ok);
    assert!(stderr.to_lowercase().contains("config"), "{stderr}");
    let _ = std::fs::remove_file(&bad);
}

#[test]
fn search_and_help_cli_mirror_the_meta_tools() {
    let (hits, _, ok) = run(&["search", "--config", "bench/bigapi.yaml", "create a widget"]);
    assert!(ok);
    assert!(hits.contains("big.widgets/create"), "{hits}");

    let (details, _, ok) = run(&["help", "--config", "bench/bigapi.yaml", "big.widgets/create"]);
    assert!(ok);
    assert!(details.contains("input_schema") && details.contains("\"name\""), "{details}");
}

#[test]
fn serve_http_refuses_a_non_loopback_bind_without_allow_remote() {
    // The HTTP transport has no inbound auth, so "everyone" (0.0.0.0) must be
    // an explicit choice. Port 0 so the bind itself succeeds anywhere; the
    // refusal happens on the resolved address, after binding.
    let mut child = Command::new(BIN)
        .args(["serve", "--http", "0.0.0.0:0", "--config", "tests/fixtures/ci-server.yaml"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve --http");
    // Bounded wait: a guard that DOESN'T fire leaves a server running forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break Some(s);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let status = status.expect("serve --http 0.0.0.0 must exit (refused), not keep serving");
    let mut stderr = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(!status.success(), "non-loopback bind must be refused: {stderr}");
    assert!(stderr.contains("refusing") && stderr.contains("--allow-remote"), "{stderr}");

    // and the override flag is tied to --http
    let (_, stderr, ok) = run(&["serve", "--allow-remote", "--config", "tests/fixtures/ci-server.yaml"]);
    assert!(!ok);
    assert!(stderr.contains("--http"), "--allow-remote without --http is a usage error: {stderr}");
}

/// One raw HTTP/1.1 POST of the initialize request with the given extra
/// headers, returning the status line. Raw sockets, so the test needs no HTTP
/// crate and controls the `Host` header exactly.
trait PostStatus {
    fn post_status(&self, headers: &[(&str, &str)]) -> String;
}

impl PostStatus for HttpServer {
    fn post_status(&self, headers: &[(&str, &str)]) -> String {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let mut req = String::from("POST / HTTP/1.1\r\n");
        let mut has_host = false;
        for (k, v) in headers {
            has_host |= k.eq_ignore_ascii_case("host");
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !has_host {
            req.push_str(&format!("Host: 127.0.0.1:{}\r\n", self.port));
        }
        req.push_str("Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n");
        req.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n{INIT}", INIT.len()));
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = [0u8; 512];
        let n = s.read(&mut buf).unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string()
    }
}

#[test]
fn identity_enforced_remote_bind_serves_clients_by_any_host_name() {
    // With `auth:` configured (a verifier, no allow_anonymous) every request is
    // authenticated, so the non-loopback bind is accepted without --allow-remote
    // — and it must actually SERVE: clients reach a remote server under whatever
    // Host they use, so rmcp's loopback-only Host allow-list is off, and a
    // rebinding page can't present a bearer, so the Origin check is off too.
    let s = HttpServer::start("0.0.0.0", &["--config", "tests/fixtures/e2e-scopes.yaml"]);
    let bearer = format!("Bearer {TOKEN_READ}");
    let ok = s.post_status(&[("Host", "10.1.2.3:8080"), ("Authorization", &bearer)]);
    assert!(ok.contains(" 200 "), "a remote client with a valid bearer must be served, got: {ok}");
    let ok = s.post_status(&[("Host", "mcp.corp.example"), ("Origin", "https://console.corp.example"), ("Authorization", &bearer)]);
    assert!(ok.contains(" 200 "), "a browser-based client with a bearer must be served, got: {ok}");
    let no = s.post_status(&[("Host", "10.1.2.3:8080")]);
    assert!(no.contains(" 401 "), "identity is still enforced, got: {no}");
}

#[test]
fn allow_remote_serves_any_host_but_still_refuses_foreign_origins() {
    // --allow-remote with no auth: Host validation must be off (or nothing
    // remote could ever connect), but the Origin check stays — it is the only
    // thing standing between a DNS-rebinding page and an unauthenticated port.
    let s = HttpServer::start("0.0.0.0", &["--allow-remote", "--config", "tests/fixtures/ci-server.yaml"]);
    let ok = s.post_status(&[("Host", "10.1.2.3:8080")]);
    assert!(ok.contains(" 200 "), "a remote client must be served under --allow-remote, got: {ok}");
    let no = s.post_status(&[("Host", "10.1.2.3:8080"), ("Origin", "https://evil.example")]);
    assert!(no.contains(" 403 "), "a foreign Origin must still be refused, got: {no}");
}

#[test]
fn loopback_bind_keeps_both_dns_rebinding_defences() {
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/ci-server.yaml"]);
    let ok = s.post_status(&[]);
    assert!(ok.contains(" 200 "), "{ok}");
    let host = s.post_status(&[("Host", "evil.example")]);
    assert!(host.contains(" 403 "), "a foreign Host on a loopback server is a rebinding attempt, got: {host}");
    let origin = s.post_status(&[("Origin", "https://evil.example")]);
    assert!(origin.contains(" 403 "), "a foreign Origin on a loopback server is a rebinding attempt, got: {origin}");
}

#[test]
fn serve_prints_an_honest_banner_to_stderr() {
    // stdout is the protocol; the banner must go to stderr, and on a tiny
    // surface it must ADMIT the minified surface isn't smaller (the
    // negative-compression NOTE) rather than always advertising a win.
    let mut child = Command::new(BIN)
        .args(["serve", "--config", "tests/fixtures/ci-server.yaml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let stderr = child.stderr.take().unwrap();
    // Read on a helper thread with a hard deadline: a banner that never
    // arrives must FAIL the test, not hang it (and the child is killed on
    // every exit path, pass or fail).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok).take(2) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Duration::from_secs(20);
    let banner = rx.recv_timeout(deadline);
    let note = rx.recv_timeout(deadline);
    let _ = child.kill();
    let _ = child.wait();
    let banner = banner.expect("banner line within deadline");
    assert!(
        banner.contains("3 surface tool(s)") && banner.contains("min-mcp:"),
        "banner states the surface honestly: {banner}"
    );
    let note = note.expect("note line within deadline");
    assert!(
        note.contains("consider `mode: passthrough`"),
        "tiny surface must trigger the negative-compression NOTE: {note}"
    );
}
