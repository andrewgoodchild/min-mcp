//! jq escape hatch for overlays (via `jaq`, a pure-Rust jq). Used only when an
//! overlay sets `response.jq` — for arbitrary reshaping the declarative
//! remove/rename/set ops can't express. Best-effort: a program that fails to
//! parse, compile, or run leaves the payload unchanged (returns None).
//!
//! Each distinct program is parsed and compiled once per thread and cached
//! (overlay programs are a fixed, config-sized set); only the run is per call.
//! Still not the default — the declarative ops are the scalable path, and jq
//! numbers are f64 (like jq itself), so a 128-bit id passing THROUGH a jq
//! program is rewritten even though every other path now preserves literals.
//! See the module comment in `config.rs::ResponseTransform`.

use std::cell::RefCell;
use std::collections::HashMap;

use jaq_interpret::{Ctx, Filter, FilterT, ParseCtx, RcIter, Val};
use serde_json::Value;

thread_local! {
    /// program text -> compiled filter, or None for a program that failed to
    /// parse/compile (warned once, then remembered so a broken overlay doesn't
    /// re-parse and re-warn on every call). Thread-local because jaq's values
    /// are `Rc`-based; the surface runs on one runtime thread behind its mutex.
    static COMPILED: RefCell<HashMap<String, Option<Filter>>> = RefCell::new(HashMap::new());
}

fn compile(program: &str) -> Option<Filter> {
    let mut ctx = ParseCtx::new(Vec::new());
    ctx.insert_natives(jaq_core::core());
    ctx.insert_defs(jaq_std::std());

    let (parsed, errs) = jaq_parse::parse(program, jaq_parse::main());
    if !errs.is_empty() || parsed.is_none() {
        crate::log_warn!("overlay jq program failed to parse: {program:?}");
        return None;
    }
    let filter = ctx.compile(parsed?);
    if !ctx.errs.is_empty() {
        crate::log_warn!("overlay jq program failed to compile: {program:?}");
        return None;
    }
    Some(filter)
}

/// Run `program` over `input`, returning the first output value (or None on any
/// parse/compile/runtime failure).
pub fn run(program: &str, input: &Value) -> Option<Value> {
    COMPILED.with(|cache| {
        let mut cache = cache.borrow_mut();
        let filter = cache.entry(program.to_string()).or_insert_with(|| compile(program)).as_ref()?;
        let inputs = RcIter::new(core::iter::empty());
        let mut out = filter.run((Ctx::new(Vec::new(), &inputs), Val::from(input.clone())));
        match out.next() {
            // Val's Display is JSON, so round-trip through a string (version-stable
            // vs. relying on a direct Val -> serde_json::Value conversion).
            Some(Ok(v)) => serde_json::from_str(&v.to_string()).ok(),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deletes_and_reshapes() {
        let input = json!({"data": [{"id": "a", "amount": 1, "secret": "x"}], "has_more": false});
        let out = run(".data |= map({id, amount})", &input).unwrap();
        assert_eq!(out, json!({"data": [{"id": "a", "amount": 1}], "has_more": false}));
    }

    #[test]
    fn del_builtin_works() {
        let input = json!({"a": 1, "b": 2});
        assert_eq!(run("del(.b)", &input).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn bad_program_returns_none() {
        let input = json!({"a": 1});
        assert!(run("this is not jq (((", &input).is_none());
        // and stays None on the cached retry (a broken program is remembered)
        assert!(run("this is not jq (((", &input).is_none());
    }

    #[test]
    fn cached_program_reruns_on_fresh_input() {
        // the compiled filter is reused, but every call sees ITS input
        assert_eq!(run(".a + 1", &json!({"a": 1})).unwrap(), json!(2));
        assert_eq!(run(".a + 1", &json!({"a": 41})).unwrap(), json!(42));
    }
}
