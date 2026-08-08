//! GraphQL-style field projection for tool results — the response-side
//! compression lever. The caller names the fields it wants; we prune the
//! response to just those, preserving structure. A proxy can do this over any
//! JSON response, even from APIs that have no field-selection of their own,
//! turning a blunt char-budget truncation into precise selection.
//!
//! Path syntax: dotted keys navigate objects; `[]` maps over an array element.
//! e.g. `["data[].id", "data[].amount", "has_more"]` keeps each element's id and
//! amount plus the top-level has_more, dropping everything else.

use serde_json::{Map, Value};

/// Prune `value` to the union of `paths`, preserving nesting. Unmatched paths
/// contribute nothing; an empty `paths` returns Null (callers should skip
/// projection when no fields are requested).
pub fn project(value: &Value, paths: &[String]) -> Value {
    let mut acc = Value::Null;
    for p in paths {
        let segs = parse_path(p);
        if let Some(sel) = select_path(value, &segs) {
            acc = deep_merge(acc, sel);
        }
    }
    acc
}

/// Remove `paths` from `value` in place — the denylist counterpart to `project`.
/// Used by overlays to strip fields that should never reach the model (secrets,
/// PII, noise), always, regardless of what the caller requested.
pub fn prune(value: &mut Value, paths: &[String]) {
    for p in paths {
        remove_at(value, &parse_path(p));
    }
}

fn remove_at(v: &mut Value, segs: &[String]) {
    match segs.split_first() {
        None => {}
        // final segment: delete it here
        Some((seg, [])) => {
            if seg == "[]" {
                if let Some(a) = v.as_array_mut() {
                    a.clear();
                }
            } else if let Some(o) = v.as_object_mut() {
                o.remove(seg);
            }
        }
        // `[]` in the middle: descend into every element
        Some((seg, rest)) if seg == "[]" => {
            if let Some(a) = v.as_array_mut() {
                for e in a.iter_mut() {
                    remove_at(e, rest);
                }
            }
        }
        Some((seg, rest)) => {
            if let Some(child) = v.as_object_mut().and_then(|o| o.get_mut(seg)) {
                remove_at(child, rest);
            }
        }
    }
}

/// Rename the key at `path` to `new_name`, in place. `[]` maps
/// over array elements. No-op if the path is absent.
pub fn rename(value: &mut Value, path: &str, new_name: &str) {
    rename_at(value, &parse_path(path), new_name);
}

fn rename_at(v: &mut Value, segs: &[String], new: &str) {
    match segs.split_first() {
        Some((seg, rest)) if rest.is_empty() && seg != "[]" => {
            if let Some(o) = v.as_object_mut() {
                if let Some(val) = o.remove(seg) {
                    o.insert(new.to_string(), val);
                }
            }
        }
        Some((seg, rest)) if seg == "[]" => {
            if let Some(a) = v.as_array_mut() {
                for e in a.iter_mut() {
                    rename_at(e, rest, new);
                }
            }
        }
        Some((seg, rest)) => {
            if let Some(child) = v.as_object_mut().and_then(|o| o.get_mut(seg)) {
                rename_at(child, rest, new);
            }
        }
        None => {}
    }
}

/// Add-or-replace the value at `path` (objects only; missing objects are
/// created) — add-or-replace combined.
pub fn set(value: &mut Value, path: &str, val: Value) {
    set_at(value, &parse_path(path), val);
}

/// Set the value at `path` only if it is absent (request-side defaults). Creates
/// intermediate objects; never overwrites an existing value.
pub fn set_default(value: &mut Value, path: &str, val: Value) {
    default_at(value, &parse_path(path), val);
}

fn default_at(v: &mut Value, segs: &[String], val: Value) {
    match segs.split_first() {
        None => {}
        Some((seg, [])) => {
            if let Some(o) = v.as_object_mut() {
                o.entry(seg.clone()).or_insert(val); // only if absent
            }
        }
        Some((seg, rest)) => {
            if let Some(o) = v.as_object_mut() {
                let child = o.entry(seg.clone()).or_insert_with(|| Value::Object(Default::default()));
                default_at(child, rest, val);
            }
        }
    }
}

fn set_at(v: &mut Value, segs: &[String], val: Value) {
    match segs.split_first() {
        None => {}
        Some((seg, [])) => {
            if let Some(o) = v.as_object_mut() {
                o.insert(seg.clone(), val);
            }
        }
        Some((seg, rest)) => {
            if let Some(o) = v.as_object_mut() {
                let child = o.entry(seg.clone()).or_insert_with(|| Value::Object(Default::default()));
                set_at(child, rest, val);
            }
        }
    }
}

fn parse_path(p: &str) -> Vec<String> {
    // "data[].id" -> ["data", "[]", "id"]; also accept "*" as an array wildcard.
    p.replace("[]", ".[].")
        .replace(".*.", ".[].")
        .split('.')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The pruned sub-structure of `v` along one path (single leaf kept per path).
fn select_path(v: &Value, segs: &[String]) -> Option<Value> {
    match segs.split_first() {
        None => Some(v.clone()), // reached the leaf: take the whole value
        Some((seg, rest)) if seg == "[]" => {
            let arr = v.as_array()?;
            // Preserve array POSITION: an element that doesn't match (missing key,
            // null, or the path runs past a scalar) becomes Null rather than being
            // dropped. Dropping shifts indices, and since two selected paths are
            // merged positionally by `deep_merge`, a shift misaligns and corrupts
            // rows across selections. Null keeps lengths stable and merges cleanly.
            Some(Value::Array(arr.iter().map(|e| select_path(e, rest).unwrap_or(Value::Null)).collect()))
        }
        Some((seg, rest)) => {
            let child = v.as_object()?.get(seg)?;
            let sub = select_path(child, rest)?;
            let mut m = Map::new();
            m.insert(seg.clone(), sub);
            Some(Value::Object(m))
        }
    }
}

/// Merge two pruned structures. Objects merge by key; arrays merge element-wise
/// (the selections come from the same source array, so positions correspond).
fn deep_merge(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Object(mut am), Value::Object(bm)) => {
            for (k, v) in bm {
                let merged = match am.remove(&k) {
                    Some(av) => deep_merge(av, v),
                    None => v,
                };
                am.insert(k, merged);
            }
            Value::Object(am)
        }
        (Value::Array(aa), Value::Array(ba)) => {
            let mut ai = aa.into_iter();
            let mut bi = ba.into_iter();
            let mut out = Vec::new();
            loop {
                match (ai.next(), bi.next()) {
                    (Some(x), Some(y)) => out.push(deep_merge(x, y)),
                    (Some(x), None) => out.push(x),
                    (None, Some(y)) => out.push(y),
                    (None, None) => break,
                }
            }
            Value::Array(out)
        }
        (Value::Null, b) => b,
        (a, _) => a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(ps: &[&str]) -> Vec<String> {
        ps.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn projects_array_elements_and_top_level_field() {
        let v = json!({
            "object": "list",
            "data": [
                {"id": "ch_1", "amount": 2345, "currency": "usd", "metadata": {"x": 1}},
                {"id": "ch_2", "amount": 1200, "currency": "usd", "metadata": {"x": 2}}
            ],
            "has_more": false,
            "url": "/v1/charges"
        });
        let out = project(&v, &paths(&["data[].id", "data[].amount", "has_more"]));
        assert_eq!(
            out,
            json!({"data": [{"id": "ch_1", "amount": 2345}, {"id": "ch_2", "amount": 1200}], "has_more": false})
        );
    }

    #[test]
    fn nested_object_paths_and_star_wildcard() {
        let v = json!({"data": [{"id": 1, "src": {"type": "card", "last4": "4242"}}]});
        let out = project(&v, &paths(&["data.*.id", "data[].src.last4"]));
        assert_eq!(out, json!({"data": [{"id": 1, "src": {"last4": "4242"}}]}));
    }

    #[test]
    fn missing_paths_are_skipped_not_errored() {
        let v = json!({"a": 1, "b": 2});
        assert_eq!(project(&v, &paths(&["a", "nope", "deep.missing"])), json!({"a": 1}));
    }

    #[test]
    fn whole_object_leaf() {
        let v = json!({"a": {"x": 1, "y": 2}, "b": 3});
        assert_eq!(project(&v, &paths(&["a"])), json!({"a": {"x": 1, "y": 2}}));
    }

    #[test]
    fn prune_removes_denylisted_fields_including_in_arrays() {
        let mut v = json!({
            "data": [
                {"id": "ch_1", "amount": 10, "secret_key": "sk_live_x", "meta": {"pii": "a@b.c", "ok": 1}},
                {"id": "ch_2", "amount": 20, "secret_key": "sk_live_y", "meta": {"pii": "d@e.f", "ok": 2}}
            ],
            "livemode": true,
            "url": "/v1/charges"
        });
        prune(&mut v, &paths(&["data[].secret_key", "data[].meta.pii", "livemode"]));
        assert_eq!(
            v,
            json!({
                "data": [
                    {"id": "ch_1", "amount": 10, "meta": {"ok": 1}},
                    {"id": "ch_2", "amount": 20, "meta": {"ok": 2}}
                ],
                "url": "/v1/charges"
            })
        );
    }

    #[test]
    fn prune_missing_path_is_a_noop() {
        let mut v = json!({"a": 1});
        prune(&mut v, &paths(&["b", "a.deep.missing"]));
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn rename_key_in_arrays_and_nested() {
        let mut v = json!({"data": [{"balance_transaction": "txn_1", "amount": 5}]});
        rename(&mut v, "data[].balance_transaction", "txn");
        assert_eq!(v["data"][0]["txn"], json!("txn_1"));
        assert!(v["data"][0].get("balance_transaction").is_none());
    }

    #[test]
    fn null_array_element_keeps_its_position() {
        // the fix: a null / non-matching element becomes null in place, not dropped
        let v = json!({"data": [{"id": 1}, null, {"id": 2}]});
        assert_eq!(project(&v, &paths(&["data[].id"])), json!({"data": [{"id": 1}, null, {"id": 2}]}));
    }

    #[test]
    fn two_paths_stay_row_aligned_when_a_field_is_missing() {
        // regression for the corruption bug: element 2 lacks `b`; before the fix
        // path `b` collapsed to length 2 and deep_merge attached b:6 to the wrong
        // row. Position preservation keeps every row correct.
        let v = json!({"data": [{"a": 1, "b": 2}, {"a": 3}, {"a": 5, "b": 6}]});
        let out = project(&v, &paths(&["data[].a", "data[].b"]));
        assert_eq!(out, json!({"data": [{"a": 1, "b": 2}, {"a": 3}, {"a": 5, "b": 6}]}));
    }

    #[test]
    fn arrays_of_arrays_and_empty_arrays() {
        let v = json!({"matrix": [[{"id": 1, "z": 0}, {"id": 2, "z": 0}], [{"id": 3, "z": 0}]]});
        assert_eq!(
            project(&v, &paths(&["matrix[][].id"])),
            json!({"matrix": [[{"id": 1}, {"id": 2}], [{"id": 3}]]})
        );
        // empty array keeps its structure, not dropped
        let e = json!({"data": [], "has_more": true});
        assert_eq!(project(&e, &paths(&["data[].id", "has_more"])), json!({"data": [], "has_more": true}));
    }

    #[test]
    fn null_leaf_kept_and_unicode_keys() {
        assert_eq!(project(&json!({"a": null, "b": 2}), &paths(&["a"])), json!({"a": null}));
        assert_eq!(
            project(&json!({"café": {"π": 42.5, "x": 1}}), &paths(&["café.π"])),
            json!({"café": {"π": 42.5}})
        );
    }

    #[test]
    fn overlapping_prefix_selects_the_whole_object() {
        // documented behaviour: selecting both `a` and `a.b` yields all of `a`
        // (union semantics via deep_merge), not just `a.b`.
        let v = json!({"a": {"b": 1, "c": 2}, "d": 3});
        assert_eq!(project(&v, &paths(&["a", "a.b"])), json!({"a": {"b": 1, "c": 2}}));
    }

    #[test]
    fn set_adds_and_replaces_creating_intermediate_objects() {
        let mut v = json!({"a": 1});
        set(&mut v, "a", json!(2)); // replace
        set(&mut v, "meta.source", json!("min-mcp")); // add, creating `meta`
        assert_eq!(v, json!({"a": 2, "meta": {"source": "min-mcp"}}));
    }
}
