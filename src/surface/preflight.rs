//! Local schema validation and structured field errors (errors as continuation prompts).

use super::*;

/// Render a machine-shaped error for one field from its container schema:
/// `{error, field, required, allowed_values?, description?, fix}`. Structured
/// errors let a weak agent fix its call in one step (measured 0%→100% vs prose).
/// `display` is the dotted path shown to the agent; `field` is the leaf key in
/// `container.properties`. None if the field isn't present.
pub(super) fn structured_field_error(container: &Value, field: &str, display: &str, reason: &str) -> Option<Value> {
    let prop = container.get("properties")?.get(field)?;
    let required = container
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().any(|v| v.as_str() == Some(field)))
        .unwrap_or(false);
    let mut obj = serde_json::Map::new();
    obj.insert("error".into(), json!(reason));
    obj.insert("field".into(), json!(display));
    obj.insert("required".into(), json!(required));
    let has_enum = prop.get("enum").is_some();
    if let Some(e) = prop.get("enum") {
        obj.insert("allowed_values".into(), e.clone());
    }
    if let Some(d) = prop.get("description").and_then(Value::as_str) {
        obj.insert("description".into(), json!(d));
    }
    let fix = if has_enum {
        format!("set {display} to one of allowed_values, then retry")
    } else {
        format!("set {display} (see description) in the arguments, then retry")
    };
    obj.insert("fix".into(), json!(fix));
    Some(Value::Object(obj))
}

/// Render the structured error for a dotted path against a whole input schema.
pub(super) fn structured_field_error_at(root: &Value, dotted: &str, reason: &str) -> Option<Value> {
    let (container, field) = nav_ref(root, dotted)?;
    structured_field_error(container, &field, dotted, reason)
}

/// Pre-flight validation: the first required-missing / out-of-enum violation in
/// `args` against the (patched) input `schema`, as a structured error. Recurses
/// into object-typed properties (so `body.zone` is reached). None if the call
/// satisfies the schema's `required`/`enum` constraints.
pub(super) fn preflight_error(schema: &Value, args: &Value) -> Option<Value> {
    preflight_walk(schema, args, "")
}

pub(super) fn preflight_walk(container: &Value, args: &Value, prefix: &str) -> Option<Value> {
    let props = container.get("properties").and_then(Value::as_object)?;
    let required: Vec<&str> = container
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for (name, pschema) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let val = args.get(name);
        // A present null is "absent-ish" (form encoding drops it; workflows emit
        // it for omitted optional inputs) — it violates `required` only when the
        // schema doesn't explicitly allow null.
        if required.contains(&name.as_str()) {
            match val {
                None => return structured_field_error(container, name, &path, "missing_required_field"),
                Some(Value::Null) if !null_allowed(pschema) => {
                    return structured_field_error(container, name, &path, "missing_required_field");
                }
                _ => {}
            }
        }
        let Some(v) = val else { continue };
        if v.is_null() {
            continue; // never validate a null beyond requiredness (see above)
        }
        // A schema with alternative branches can't be evaluated by this walker —
        // be permissive: preflight must never reject a call the upstream accepts.
        if pschema.get("anyOf").is_some() || pschema.get("oneOf").is_some() {
            continue;
        }
        if let Some(en) = pschema.get("enum").and_then(Value::as_array) {
            if !en.iter().any(|x| x == v) {
                return structured_field_error(container, name, &path, "invalid_enum_value");
            }
        }
        if pschema.get("type").and_then(Value::as_str) == Some("object") {
            if let Some(inner) = preflight_walk(pschema, v, &path) {
                return Some(inner);
            }
        }
    }
    None
}

/// Does this property schema explicitly admit null? (OpenAPI 3.0
/// `nullable: true`, a JSON-Schema `type` array containing "null", or an enum
/// listing null.)
fn null_allowed(pschema: &Value) -> bool {
    if pschema.get("nullable").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if let Some(types) = pschema.get("type").and_then(Value::as_array) {
        if types.iter().any(|t| t.as_str() == Some("null")) {
            return true;
        }
    }
    pschema
        .get("enum")
        .and_then(Value::as_array)
        .map(|e| e.iter().any(Value::is_null))
        .unwrap_or(false)
}

/// Immutable twin of `nav_to_container`: walk a dotted path to the container
/// object that holds the leaf `field` (through nested `properties`).
pub(super) fn nav_ref<'a>(schema: &'a Value, path: &str) -> Option<(&'a Value, String)> {
    let mut parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    let field = parts.pop()?.to_string();
    let mut node = schema;
    for s in parts {
        node = node.get("properties")?.get(s)?;
    }
    Some((node, field))
}

/// Resolve a `user_supplied` source to its value. MVP: `env:VAR` from the
/// process environment. Unset/empty → None (the call fails with a clear error).
pub(super) fn resolve_user_source(source: &str) -> Option<String> {
    source
        .strip_prefix("env:")
        .and_then(|var| std::env::var(var).ok())
        .filter(|s| !s.is_empty())
}
