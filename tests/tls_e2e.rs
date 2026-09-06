//! TLS, mutual TLS, and the request caps, end to end against the real binary.
//!
//! The point of these tests is that the guarantees are *observable from the
//! wire*, not just that a `rustls` config object builds: a real client either
//! completes a handshake or it does not. Driven with `curl` for the same reason
//! as `http_e2e.rs` — no HTTP crate in dev-dependencies.
//!
//! Offline: the upstream is the bundled mini OpenAPI spec, never dialed.

#![cfg(unix)] // uses curl; the stdio suites cover other platforms

use std::process::Command;

mod common;
use common::{HttpServer, INIT};

const CA: &str = "tests/fixtures/tls/ca.crt";
const CLIENT_CRT: &str = "tests/fixtures/tls/client.crt";
const CLIENT_KEY: &str = "tests/fixtures/tls/client.key";
const CONTENT_TYPE: &str = "Content-Type: application/json";
const ACCEPT: &str = "Accept: application/json, text/event-stream";

/// `curl` with the given args. Returns (stdout, exit code).
fn curl(args: &[&str]) -> (String, i32) {
    let out = Command::new("curl").args(args).output().expect("run curl");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.code().unwrap_or(-1))
}

/// The MCP handshake POST, with any extra curl flags (TLS material) in front.
/// Returns (body, curl exit code) — a non-zero exit is a handshake failure.
fn init_post(url: &str, extra: &[&str]) -> (String, i32) {
    post(url, INIT, extra, &[])
}

/// One JSON-RPC POST returning the response body.
fn post(url: &str, body: &str, extra: &[&str], headers: &[&str]) -> (String, i32) {
    let mut args: Vec<&str> = extra.to_vec();
    args.extend(["-s", "-X", "POST", url, "-H", CONTENT_TYPE, "-H", ACCEPT]);
    args.extend(headers);
    args.extend(["-d", body]);
    curl(&args)
}

/// One JSON-RPC POST returning only the HTTP status code, for the cap tests.
///
/// The body goes through a file rather than `-d`: these tests deliberately send
/// multi-megabyte bodies, and an argv-passed one hits the OS argument limit
/// (E2BIG) before it ever reaches the server.
fn post_status(url: &str, body: &str, headers: &[&str]) -> String {
    let path = std::env::temp_dir().join(format!("minmcp-body-{}-{:p}.json", std::process::id(), body));
    std::fs::write(&path, body).expect("write the request body");
    let at = format!("@{}", path.display());
    let mut args: Vec<&str> = vec![
        "-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "60", "-X", "POST", url, "-H",
        CONTENT_TYPE, "-H", ACCEPT,
    ];
    args.extend(headers);
    args.extend(["--data-binary", &at]);
    let (code, exit) = curl(&args);
    let _ = std::fs::remove_file(&path);
    assert_eq!(exit, 0, "curl should complete");
    code.trim().to_string()
}

/// An initialize body padded to more than `n` bytes.
fn padded(n: usize) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"pad":"{}"}}}}"#, "x".repeat(n))
}

#[test]
fn tls_serves_the_banner_and_a_real_handshake() {
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/e2e-tls.yaml"]);
    // The banner itself is the first assertion: the scheme flipped to https.
    assert_eq!(s.scheme, "https", "TLS config should serve https");

    let (body, code) = init_post(&s.url(), &["--cacert", CA]);
    assert_eq!(code, 0, "curl should complete a verified TLS handshake; got {body}");
    assert!(body.contains("protocolVersion"), "expected an initialize result, got: {body}");
}

#[test]
fn tls_refuses_a_client_that_does_not_trust_the_ca() {
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/e2e-tls.yaml"]);
    // No --cacert: curl's default trust store has never heard of our test CA,
    // so this must fail at the handshake rather than return a body.
    let (_, code) = init_post(&s.url(), &[]);
    assert_ne!(code, 0, "an untrusted certificate must not verify");
}

#[test]
fn mutual_tls_requires_a_client_certificate() {
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/e2e-mtls.yaml"]);
    assert_eq!(s.scheme, "https");

    // Trusting the CA is not enough: with no client certificate the server
    // closes the connection during the handshake.
    let (_, code) = init_post(&s.url(), &["--cacert", CA]);
    assert_ne!(code, 0, "mutual TLS must refuse a client with no certificate");

    // The same request, now presenting a certificate signed by that CA.
    let (body, code) =
        init_post(&s.url(), &["--cacert", CA, "--cert", CLIENT_CRT, "--key", CLIENT_KEY]);
    assert_eq!(code, 0, "a CA-signed client certificate should be accepted; got {body}");
    assert!(body.contains("protocolVersion"), "expected an initialize result, got: {body}");
}

#[test]
fn an_oversize_body_is_refused_with_413() {
    // The fixture caps bodies at 512 bytes; this declares far more.
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/e2e-small-body.yaml"]);
    assert_eq!(post_status(&s.url(), &padded(4096), &[]), "413");
}

#[test]
fn an_undeclared_oversize_body_is_also_refused_with_413() {
    // Chunked, so there is no content-length to pre-check: rmcp enforces the
    // same cap on the first frame that would exceed it, and returns the same
    // status. One limit, one enforcement point, one observable behaviour.
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/e2e-small-body.yaml"]);
    let code = post_status(&s.url(), &padded(4096), &["-H", "Transfer-Encoding: chunked"]);
    assert_eq!(code, "413", "an undeclared oversize body must be refused the same way");

    // The refusal must not take the listener with it.
    let (body, code) = init_post(&s.url(), &[]);
    assert_eq!(code, 0);
    assert!(body.contains("protocolVersion"), "server should still serve: {body}");
}

#[test]
fn a_cap_above_rmcps_own_default_actually_applies() {
    // rmcp's own default body cap is 4 MiB. Unless the configured value is
    // handed to it, an 8 MiB cap silently behaves as 4 MiB and the operator
    // gets a 413 quoting a number they never set.
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/e2e-big-body.yaml"]);
    // The assertion is "not 413": the padded body is deliberately not a valid
    // initialize, so rmcp answers 422 — which proves it read all 5 MiB before
    // judging the CONTENT. Capped at rmcp's 4 MiB default it never gets that
    // far and answers 413 instead.
    let code = post_status(&s.url(), &padded(5 * 1024 * 1024), &[]);
    assert_ne!(code, "413", "an 8 MiB cap must accept a 5 MiB body; got {code}");
}

#[test]
fn a_body_within_the_cap_still_works() {
    let s = HttpServer::start("127.0.0.1", &["--config", "tests/fixtures/e2e-small-body.yaml"]);
    let (body, code) = init_post(&s.url(), &[]);
    assert_eq!(code, 0);
    assert!(body.contains("protocolVersion"), "a small body must not be capped: {body}");
}
