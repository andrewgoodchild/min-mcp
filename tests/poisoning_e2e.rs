//! `poisoning_policy` end to end: what the SERVER does about a tool whose
//! description is written to steer the model.
//!
//! The linter has always been able to report these; the point of these tests is
//! that a policy decision is now made about them at startup, on the real binary.
//!
//! Offline: the upstream is a checked-in spec with deliberately poisoned
//! descriptions, never dialed.

mod common;
use common::run;

/// The poisoned fixture has three bad tools and one clean one.
const BAD: [&str; 3] = ["bad.getHijack", "bad.getExfil", "bad.getHidden"];

#[test]
fn warn_names_every_poisoned_tool_and_serves_anyway() {
    let (stdout, stderr, ok) = run(&["inspect", "--config", "tests/fixtures/e2e-poisoned-warn.yaml"]);
    assert!(ok, "warn must still serve; stderr: {stderr}");
    for tool in BAD {
        assert!(stderr.contains(tool), "warning should name {tool}; got: {stderr}");
    }
    assert!(stderr.contains("poisoning_policy: warn"));
    // Served anyway: the surface is built and reports its tools.
    assert!(stdout.contains("\"upstreams_active\": 1"), "should still serve: {stdout}");
}

#[test]
fn strict_refuses_to_start_and_says_which_tools() {
    let (_, stderr, ok) = run(&["inspect", "--config", "tests/fixtures/e2e-poisoned-strict.yaml"]);
    assert!(!ok, "strict must refuse to start");
    for tool in BAD {
        assert!(stderr.contains(tool), "error should name {tool}; got: {stderr}");
    }
    // The operator is told what to do about it, not just that it happened.
    assert!(stderr.contains("minmcp lint"), "error should point at a next step: {stderr}");
}

#[test]
fn a_clean_tool_in_the_same_spec_is_not_flagged() {
    // The rule set must discriminate within one upstream, not condemn it whole.
    let (_, stderr, _) = run(&["inspect", "--config", "tests/fixtures/e2e-poisoned-warn.yaml"]);
    assert!(!stderr.contains("getClean"), "the clean tool must not be flagged: {stderr}");
}

#[test]
fn a_real_spec_trips_nothing_at_startup() {
    // The regression that matters most: this check runs on EVERY start, so a
    // false positive is a server that warns (or refuses) for no reason. The
    // bundled spec stands in for the Stripe/GitHub measurement in the docs.
    let (_, stderr, ok) = run(&["inspect", "--config", "tests/fixtures/ci-server.yaml"]);
    assert!(ok);
    assert!(!stderr.contains("poisoning"), "a clean spec must be silent: {stderr}");
}

#[test]
fn off_disables_the_check_entirely() {
    let (_, stderr, ok) = run(&["inspect", "--config", "tests/fixtures/e2e-poisoned-off.yaml"]);
    assert!(ok, "off must serve: {stderr}");
    assert!(!stderr.contains("poisoning"), "off must not warn: {stderr}");
}
