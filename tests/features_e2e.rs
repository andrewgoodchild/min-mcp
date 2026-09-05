//! End-to-end coverage for the documented features that previously had no
//! over-the-wire test: composites (`workflows:`), `minmcp verify`,
//! auto-pagination, JWT-derived caller scopes, and per-tool `preflight`
//! opt-out. All offline — the upstream is a scriptable POSIX-sh MCP server
//! (`tests/fixtures/fake-mcp-server.sh`) and a bundled spec.
//!
//! `cfg(unix)` where a fixture needs `/bin/sh`; the rest is portable.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_minmcp");

/// HS256 tokens signed with the fixture secret `test-secret`, `exp` in 2100.
/// Hardcoded rather than minted at runtime because `jsonwebtoken` is a
/// dependency of the binary, not of this test.
const TOKEN_WRITE: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzY29wZSI6InN0b3JlLndyaXRlIiwiZXhwIjo0MTAyNDQ0ODAwfQ.y7gSKROF0_0tej7CTwv2pwqds5YKn6UqPqUojnZqXow";
const TOKEN_READ: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzY29wZSI6InN0b3JlLnJlYWQiLCJleHAiOjQxMDI0NDQ4MDB9.EuAvpxVlc9I6NeYBSpMJEH7ThR4guKjGNx-2KyX7wjg";

fn run(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(BIN).args(args).output().expect("run minmcp");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[cfg(unix)]
#[test]
fn composite_threads_one_steps_output_into_the_next() {
    // The whole point of a composite: step 2 receives step 1's extracted output,
    // and the agent pays for one call instead of the chain. The fake upstream
    // returns an error unless `create_price` was actually given `prod_1`.
    let (out, _, ok) = run(&[
        "call", "--config", "tests/fixtures/e2e-fake.yaml",
        "fake.product_with_price", "--args", r#"{"name":"Widget","amount":2500}"#,
    ]);
    assert!(ok, "composite call failed: {out}");
    assert!(out.contains("\"product_id\":\"prod_1\""), "{out}");
    assert!(out.contains("\"price_id\":\"price_1\""), "{out}");
    assert!(
        out.contains("\"threaded_product\":\"prod_1\""),
        "step 2 must have received step 1's id, not a literal: {out}"
    );
}

#[cfg(unix)]
#[test]
fn auto_pagination_follows_the_cursor_and_concatenates() {
    // Upstream serves 2 items + next_cursor, then 2 more and has_more:false.
    // The agent should see one merged list and a closed `more` gate.
    let (out, _, ok) = run(&[
        "call", "--config", "tests/fixtures/e2e-fake.yaml", "fake.list_items", "--args", "{}",
    ]);
    assert!(ok, "{out}");
    for id in ["i1", "i2", "i3", "i4"] {
        assert!(out.contains(id), "page item {id} missing from merged result: {out}");
    }
    assert!(out.contains("\"has_more\":false"), "more-gate must be closed: {out}");
}

#[cfg(unix)]
#[test]
fn verify_runs_overlay_checks_against_a_live_upstream() {
    // `minmcp verify` is the drift gate: it calls the tool for real and asserts.
    // The fixture's check asserts the response transform stripped `secret`.
    let (out, _, ok) = run(&["verify", "--config", "tests/fixtures/e2e-fake.yaml"]);
    assert!(ok, "verify should exit 0 when every check passes: {out}");
    assert!(out.contains("\"passed\": 1") && out.contains("\"failed\": 0"), "{out}");
    assert!(out.contains("\"pass\": true"), "{out}");
}

#[cfg(unix)]
#[test]
fn verify_exits_nonzero_when_a_check_fails() {
    // The CI-gate property: a broken binding must fail the build, not warn.
    let dir = std::env::temp_dir();
    let cfg = dir.join(format!("minmcp_verify_fail_{}.yaml", std::process::id()));
    let root = std::env::current_dir().unwrap();
    let script = root.join("tests/fixtures/fake-mcp-server.sh");
    let yaml = format!(
        "mode: three_tool
upstreams:
  - name: fake
    command: sh
    args: [\"{}\"]
overlays:
  - tool: fake.whoami
    verify:
      - name: \"deliberately wrong\"
        arguments: {{}}
        expect:
          has: [\"no_such_field\"]
",
        script.display()
    );
    std::fs::write(&cfg, yaml).unwrap();
    let (out, _, ok) = run(&["verify", "--config", cfg.to_str().unwrap()]);
    assert!(!ok, "a failing check must exit non-zero: {out}");
    assert!(out.contains("\"failed\": 1"), "{out}");
    let _ = std::fs::remove_file(&cfg);
}

#[cfg(unix)]
#[test]
fn a_dead_mcp_upstream_is_an_iserror_result_not_a_protocol_error() {
    // The subprocess exits on its first tools/call. A spec upstream's transport
    // failure was always an isError result; an MCP upstream's used to escape as
    // a JSON-RPC protocol error (non-zero exit here). Both must now be a result
    // the agent can act on, carrying the write-safety guidance.
    let (out, stderr, ok) =
        run(&["call", "--config", "tests/fixtures/e2e-dying.yaml", "dying.boom", "--args", "{}"]);
    assert!(ok, "a dead upstream must not be a protocol error: {stderr}");
    assert!(out.contains("UPSTREAM_ERROR") && out.contains("dying.boom"), "{out}");
    assert!(out.contains("may or may not have completed"), "write-safety guidance missing: {out}");
}

#[test]
fn optional_upstream_that_fails_to_start_is_skipped_not_fatal() {
    let (out, stderr, ok) = run(&["inspect", "--config", "tests/fixtures/e2e-optional.yaml"]);
    assert!(ok, "optional upstream must not fail startup: {stderr}");
    let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
    let n = |k: &str| v.get(k).and_then(serde_yaml::Value::as_u64).unwrap();
    assert_eq!(n("upstreams_configured"), 2);
    assert_eq!(n("upstreams_active"), 1, "the ghost is skipped, the fixture serves");
    assert_eq!(n("upstream_tools"), 3, "the surviving upstream's tools are all there");
    assert!(stderr.contains("optional upstream \"ghost\""), "the skip is logged: {stderr}");

    // Same config without `optional`: the missing command is fatal, as before.
    let dir = std::env::temp_dir();
    let cfg = dir.join(format!("minmcp_required_{}.yaml", std::process::id()));
    let spec = std::env::current_dir().unwrap().join("tests/fixtures/mini-openapi.json");
    std::fs::write(
        &cfg,
        format!(
            "upstreams:\n  - name: ghost\n    command: /nonexistent/minmcp-no-such-binary\n  - name: fixture\n    spec: {}\n    base_url: https://example.invalid\n",
            spec.display()
        ),
    )
    .unwrap();
    let (_, stderr, ok) = run(&["inspect", "--config", cfg.to_str().unwrap()]);
    assert!(!ok, "a required upstream that can't start must be fatal");
    assert!(stderr.contains("ghost"), "{stderr}");
    let _ = std::fs::remove_file(&cfg);
}

#[test]
fn unknown_config_keys_are_a_startup_error_naming_the_key() {
    let dir = std::env::temp_dir();
    let cfg = dir.join(format!("minmcp_typo_{}.yaml", std::process::id()));
    std::fs::write(&cfg, "upstreams: []\npreflght: false\n").unwrap();
    let (_, stderr, ok) = run(&["inspect", "--config", cfg.to_str().unwrap()]);
    assert!(!ok, "a misspelled key must not be a silent no-op");
    assert!(stderr.contains("preflght"), "the error names the key: {stderr}");
    let _ = std::fs::remove_file(&cfg);
}

#[test]
fn jwt_scopes_filter_the_surface_per_caller() {
    // A caller sees ONLY what their scope grants: same config, two tokens,
    // disjoint tool sets. This is the security-relevant path.
    let (write_out, _, ok) =
        run(&["search", "--config", "tests/fixtures/e2e-scopes.yaml", "--jwt", TOKEN_WRITE, "widget"]);
    assert!(ok, "{write_out}");
    assert!(write_out.contains("widgets/create"), "write scope must see create: {write_out}");
    assert!(!write_out.contains("widgets/list"), "write scope must NOT see list: {write_out}");

    let (read_out, _, ok) =
        run(&["search", "--config", "tests/fixtures/e2e-scopes.yaml", "--jwt", TOKEN_READ, "widget"]);
    assert!(ok, "{read_out}");
    assert!(read_out.contains("widgets/list"), "read scope must see list: {read_out}");
    assert!(!read_out.contains("widgets/create"), "read scope must NOT see create: {read_out}");
}

#[test]
fn a_tampered_jwt_is_refused() {
    let mut bad = TOKEN_WRITE.to_string();
    bad.pop();
    bad.push('X'); // corrupt the signature
    let (_, stderr, ok) =
        run(&["search", "--config", "tests/fixtures/e2e-scopes.yaml", "--jwt", &bad, "widget"]);
    assert!(!ok, "a bad signature must not be served");
    assert!(stderr.contains("JWT validation failed"), "{stderr}");
}

#[test]
fn scoped_out_tools_are_invisible_to_the_source_map_too() {
    // The side-door check: `map` over the protocol is scope-filtered, so a
    // restricted caller can't enumerate hidden tools. (The CLI `map` is the
    // full audit view by design; this asserts the scope-aware surface count.)
    let (out, _, ok) =
        run(&["inspect", "--config", "tests/fixtures/e2e-scopes.yaml", "--jwt", TOKEN_READ]);
    assert!(ok, "{out}");
    let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
    let visible = v.get("visible_after_scopes").and_then(serde_yaml::Value::as_u64).unwrap();
    let total = v.get("upstream_tools").and_then(serde_yaml::Value::as_u64).unwrap();
    assert!(visible < total, "read scope must see fewer than all {total} tools (saw {visible})");
    assert_eq!(visible, 2, "exactly the two read tools the scope grants");
}

#[test]
fn per_tool_preflight_opt_out_reaches_the_upstream() {
    // preflight defaults ON, so a spec that over-declares `required` would
    // reject locally. The documented escape hatch is a per-tool override; this
    // asserts it actually disables the local check (the call then fails at the
    // unreachable upstream instead — a DIFFERENT error, which is the proof).
    let dir = std::env::temp_dir();
    let root = std::env::current_dir().unwrap();
    let spec = root.join("examples/specs/acme-store.json");

    let mk = |name: &str, extra: &str| {
        let p = dir.join(format!("minmcp_pf_{name}_{}.yaml", std::process::id()));
        std::fs::write(
            &p,
            format!(
                "mode: three_tool\nupstreams:\n  - name: acme\n    spec: {}\n    base_url: https://example.invalid\n{extra}",
                spec.display()
            ),
        )
        .unwrap();
        p
    };

    // 1. default: the missing required field is caught locally
    let on = mk("on", "");
    let (out, _, _) = run(&[
        "call", "--config", on.to_str().unwrap(), "acme.widgets/create", "--args", r#"{"body":{}}"#,
    ]);
    assert!(out.contains("PREFLIGHT_ERROR"), "default should preflight: {out}");

    // 2. opt out for that tool: no local rejection, the call goes upstream
    let off = mk("off", "overlays:\n  - tool: acme.widgets/create\n    preflight: false\n");
    let (out2, _, _) = run(&[
        "call", "--config", off.to_str().unwrap(), "acme.widgets/create", "--args", r#"{"body":{}}"#,
    ]);
    assert!(!out2.contains("PREFLIGHT_ERROR"), "opt-out must disable the local check: {out2}");

    let _ = std::fs::remove_file(&on);
    let _ = std::fs::remove_file(&off);
}
