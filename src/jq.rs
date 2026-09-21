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

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, Compiler, Ctx, Vars};
use jaq_json::Val;
use serde_json::Value;

/// A compiled jaq program. `Filter` owns its lookup table and borrows nothing
/// from the `Arena` it was loaded through, so it can outlive compilation and
/// be cached — which is what makes the per-call cost just the run.
type Program = jaq_core::Filter<data::JustLut<Val>>;

thread_local! {
    /// program text -> compiled filter, or None for a program that failed to
    /// parse/compile (warned once, then remembered so a broken overlay doesn't
    /// re-parse and re-warn on every call). Thread-local because jaq's values
    /// are `Rc`-based; the surface runs on one runtime thread behind its mutex.
    static COMPILED: RefCell<HashMap<String, Option<Program>>> = RefCell::new(HashMap::new());
}

fn compile(program: &str) -> Option<Program> {
    // The builtin set is assembled from three crates in jaq 3.x: the core
    // language, the standard library, and the JSON-specific filters that used
    // to live inside the interpreter.
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());

    let arena = Arena::default();
    let modules = match Loader::new(defs).load(&arena, File { code: program, path: () }) {
        Ok(m) => m,
        Err(_) => {
            crate::log_warn!("overlay jq program failed to parse: {program:?}");
            return None;
        }
    };
    match Compiler::default().with_funs(funs).compile(modules) {
        Ok(f) => Some(f),
        Err(_) => {
            crate::log_warn!("overlay jq program failed to compile: {program:?}");
            None
        }
    }
}

/// Run `program` over `input`, returning the first output value (or None on any
/// parse/compile/runtime failure).
pub fn run(program: &str, input: &Value) -> Option<Value> {
    COMPILED.with(|cache| {
        let mut cache = cache.borrow_mut();
        let filter = cache.entry(program.to_string()).or_insert_with(|| compile(program)).as_ref()?;
        // serde_json <-> jaq through text, deliberately: it is stable across
        // jaq versions, where a direct value conversion is the part that moved
        // house in 2.x. (jq numbers are f64 either way — see the module note.)
        let text = serde_json::to_string(input).ok()?;
        let input: Val = jaq_json::read::parse_single(text.as_bytes()).ok()?;
        let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
        // Deliberately NOT jaq's `unwrap_valr`, which the crate's own example
        // uses: it calls `std::process::exit` on a jq `halt`. That would let a
        // `halt` in an overlay's jq program terminate the proxy — a config typo
        // taking the server down. Matching the raw result keeps every failure,
        // exception and halt alike, on the same best-effort path as any other:
        // leave the payload unchanged.
        let mut out = filter.id.run((ctx, input));
        match out.next() {
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
    fn a_halt_leaves_the_payload_alone_instead_of_exiting() {
        // jaq's own example pipes results through `unwrap_valr`, which calls
        // `std::process::exit` on a jq `halt`. Taking that verbatim would let
        // `halt` in an overlay's jq program kill the proxy. If this ever
        // regresses, the test process exits and the suite dies outright —
        // which is exactly the failure being guarded.
        let input = json!({"a": 1});
        assert!(run("halt", &input).is_none());
        assert!(run("halt_error", &input).is_none());
        // and the module keeps working afterwards
        assert_eq!(run(".a", &input).unwrap(), json!(1));
    }

    #[test]
    fn cached_program_reruns_on_fresh_input() {
        // the compiled filter is reused, but every call sees ITS input
        assert_eq!(run(".a + 1", &json!({"a": 1})).unwrap(), json!(2));
        assert_eq!(run(".a + 1", &json!({"a": 41})).unwrap(), json!(42));
    }
}
