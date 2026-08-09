//! Display-budget minification: cap prose without ever hiding a field
//! name, and bound rendered text on char boundaries. Split from the
//! surface module; behavior is pinned by `surface/tests.rs` and the
//! stdio E2E suite.

use serde_json::{json, Value};

pub(super) fn truncate_in_place(s: &mut String, max: usize) {
    if s.len() > max {
        s.truncate(s.floor_char_boundary(max));
        s.push('…');
    }
}

/// Cap on enum values shown in a minified schema (huge enums — country lists —
/// are the classic budget killer; the marker says how many were elided).
pub(super) const MINIFY_ENUM_CAP: usize = 24;
/// Cap on per-field description length in a minified schema.
pub(super) const MINIFY_DESC_CHARS: usize = 120;

/// Recurse into exactly the places a JSON-Schema node keeps CHILD SCHEMAS —
/// never into `properties`' key space itself, whose keys are user field names.
/// (A field literally named `description` or `enum` must survive minification;
/// stripping keywords by walking every object indiscriminately deleted them.)
fn for_each_child_schema(map: &mut serde_json::Map<String, Value>, f: &mut impl FnMut(&mut Value)) {
    for key in ["properties", "patternProperties", "definitions", "$defs"] {
        if let Some(children) = map.get_mut(key).and_then(Value::as_object_mut) {
            for (_, child) in children.iter_mut() {
                f(child); // values are schemas; KEYS are user names — untouched
            }
        }
    }
    for key in ["items", "additionalProperties", "not"] {
        if let Some(child) = map.get_mut(key) {
            if child.is_object() || child.is_array() {
                f(child);
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(arr) = map.get_mut(key).and_then(Value::as_array_mut) {
            for child in arr {
                f(child);
            }
        }
    }
}

/// Minify a JSON schema for DISPLAY: lossless on structure (every property
/// name, type, and `required` list survives), lossy only on prose — examples
/// dropped, descriptions capped, huge enums cut with an explicit
/// `enum_truncated` marker. This replaces blind text truncation, which hid
/// trailing fields entirely (the `success_url` failure class). `v` must be a
/// SCHEMA node — keyword stripping happens only at schema level, and recursion
/// follows schema structure, never raw object children.
pub(super) fn minify_schema(v: &mut Value) {
    let Some(map) = v.as_object_mut() else { return };
    map.remove("example");
    map.remove("examples");
    if let Some(Value::String(d)) = map.get_mut("description") {
        truncate_in_place(d, MINIFY_DESC_CHARS);
    }
    let elided = match map.get_mut("enum").and_then(Value::as_array_mut) {
        Some(vals) if vals.len() > MINIFY_ENUM_CAP => {
            let n = vals.len() - MINIFY_ENUM_CAP;
            vals.truncate(MINIFY_ENUM_CAP);
            Some(n)
        }
        _ => None,
    };
    if let Some(n) = elided {
        map.insert("enum_truncated".into(), json!(format!("+{n} more values (see the API's own docs)")));
    }
    for_each_child_schema(map, &mut minify_schema);
}

/// Second-stage minification for schemas that overflow the budget even after
/// [`minify_schema`]: strip ALL prose (`description`, `title`) and `enum`
/// lists, keeping only structure — property names, types, `required`. The
/// field LIST stays complete; per-field docs go to zero. Same schema-aware
/// recursion rules as [`minify_schema`].
pub(super) fn minify_schema_hard(v: &mut Value) {
    let Some(map) = v.as_object_mut() else { return };
    map.remove("description");
    map.remove("title");
    map.remove("format");
    map.remove("enum_truncated"); // stage-one marker superseded below
    if map.remove("enum").is_some() {
        map.insert("enum_truncated".into(), json!("elided"));
    }
    for_each_child_schema(map, &mut minify_schema_hard);
}

/// Replace `properties` maps nested deeper than `depth` with an explicit
/// `nested_fields_elided` count. Depth is consumed per `properties` hop only —
/// `items`/`anyOf`/`oneOf`/`allOf` wrappers are transparent — so "depth 2"
/// means two levels of named fields survive everywhere.
pub(super) fn prune_below_depth(v: &mut Value, depth: usize) {
    let Some(map) = v.as_object_mut() else { return };
    if map.get("properties").map(Value::is_object).unwrap_or(false) {
        if depth == 0 {
            let n = map.get("properties").and_then(Value::as_object).map(|p| p.len()).unwrap_or(0);
            map.remove("properties");
            map.insert("nested_fields_elided".into(), json!(n));
        } else if let Some(props) = map.get_mut("properties").and_then(Value::as_object_mut) {
            for (_, child) in props.iter_mut() {
                prune_below_depth(child, depth - 1);
            }
        }
    }
    if let Some(items) = map.get_mut("items") {
        prune_below_depth(items, depth);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(arr) = map.get_mut(key).and_then(Value::as_array_mut) {
            for c in arr {
                prune_below_depth(c, depth);
            }
        }
    }
}

/// Fit `s` into `max` chars, reserving room for `suffix` INSIDE the budget so
/// the appended label is never itself clipped by a downstream cap. Always cuts
/// on a char boundary (never panics on multibyte input).
pub(super) fn budget_truncate(mut s: String, max: usize, suffix: &str) -> String {
    if s.len() <= max {
        return s;
    }
    s.truncate(s.floor_char_boundary(max.saturating_sub(suffix.len())));
    s.push_str(suffix);
    s
}
