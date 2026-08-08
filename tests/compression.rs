//! Reproduce + lock the token-compression claim in CI — deterministic, no LLM.
//! `minmcp inspect` reports the naive one-tool-per-endpoint token cost of a spec
//! vs the 3-tool minified surface; these tests assert the compression holds on
//! the two bundled specs, so the headline claim can't silently rot.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_minmcp");

fn inspect(config: &str) -> serde_yaml::Value {
    let out = Command::new(BIN)
        .args(["inspect", "--config", config])
        .output()
        .expect("run minmcp inspect");
    assert!(
        out.status.success(),
        "inspect failed for {config}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_yaml::from_slice(&out.stdout).expect("parse inspect JSON")
}

fn n(v: &serde_yaml::Value, key: &str) -> u64 {
    v.get(key)
        .and_then(serde_yaml::Value::as_u64)
        .unwrap_or_else(|| panic!("inspect output missing {key:?}"))
}

#[test]
fn minified_surface_is_three_tools_and_compresses() {
    let big = inspect("bench/bigapi.yaml");
    assert_eq!(n(&big, "surface_tools"), 3, "the agent always sees exactly 3 tools");
    assert_eq!(n(&big, "upstream_tools"), 120);
    let (raw, mini) = (n(&big, "est_tokens_raw"), n(&big, "est_tokens_minified"));
    assert!(mini < raw, "minified ({mini}) must be smaller than raw ({raw})");
    assert!(raw / mini >= 20, "a 120-op spec should compress >=20x (got {}x)", raw / mini);
}

#[test]
fn minified_tokens_stay_flat_as_the_surface_grows() {
    // The compression thesis: minified cost is ~constant regardless of upstream
    // size, while the naive cost scales with the number of operations.
    let small = inspect("examples/demo-overlays.yaml"); // 4 ops
    let big = inspect("bench/bigapi.yaml"); // 120 ops
    let (m_small, m_big) = (n(&small, "est_tokens_minified"), n(&big, "est_tokens_minified"));
    assert!(
        m_big <= m_small + 50,
        "minified stays flat as ops grow: {m_small} (4 ops) vs {m_big} (120 ops)"
    );
    assert!(
        n(&big, "est_tokens_raw") > 10 * n(&small, "est_tokens_raw"),
        "raw cost scales hard with op count"
    );
}
