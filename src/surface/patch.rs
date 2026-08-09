//! Overlay field patches and binding-integrity checks against live schemas.

use super::*;

/// Apply overlay field patches to a (resolved) schema in place. Each key is a
/// dotted path through `properties` (`body.currency`); the final segment is the
/// field, and `required` toggles it on its *containing* object. A path that
/// doesn't resolve to a real field is skipped (never patched, and — crucially —
/// never `required`-tagged as a phantom); it is reported separately as a broken
/// binding by `binding_status` (via `field_path_exists`).
pub(super) fn apply_field_patches(schema: &mut Value, fields: &HashMap<String, crate::config::FieldPatch>) {
    for (path, patch) in fields {
        let spec = patch.spec();
        let Some((container, field)) = nav_to_container(schema, path) else { continue };
        let Some(cobj) = container.as_object_mut() else { continue };
        let exists = cobj
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.contains_key(&field))
            .unwrap_or(false);
        if !exists {
            continue;
        }
        // `hide` and `user_supplied` both strip the field from the agent schema
        // (user_supplied additionally injects it at call time — see dispatch).
        if spec.hide.unwrap_or(false) || spec.user_supplied.is_some() {
            if let Some(props) = cobj.get_mut("properties").and_then(Value::as_object_mut) {
                props.remove(&field);
            }
            set_required(cobj, &field, false);
            continue;
        }
        if let Some(prop) = cobj
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|p| p.get_mut(&field))
            .and_then(Value::as_object_mut)
        {
            if let Some(d) = &spec.description {
                prop.insert("description".into(), json!(d));
            }
            if let Some(e) = &spec.example {
                prop.insert("example".into(), e.clone());
            }
            if let Some(en) = &spec.enum_values {
                prop.insert("enum".into(), json!(en));
            }
            if let Some(t) = &spec.ty {
                prop.insert("type".into(), json!(t));
            }
            if let Some(f) = &spec.format {
                prop.insert("format".into(), json!(f));
            }
        }
        if let Some(req) = spec.required {
            set_required(cobj, &field, req);
        }
    }
}

/// Navigate the `properties` chain to the object schema that *contains* the final
/// path segment, returning (container, field_name). `body.currency` ->
/// (schema at `.properties.body`, "currency"). None if any segment is absent.
pub(super) fn nav_to_container<'a>(schema: &'a mut Value, path: &str) -> Option<(&'a mut Value, String)> {
    let mut parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    let field = parts.pop()?.to_string();
    let mut node = schema;
    for s in parts {
        node = node.as_object_mut()?.get_mut("properties")?.as_object_mut()?.get_mut(s)?;
    }
    Some((node, field))
}

/// Add or remove `field` in a container object's `required` array (creating it as
/// needed; removing it when it empties out).
pub(super) fn set_required(container: &mut serde_json::Map<String, Value>, field: &str, required: bool) {
    if required {
        let arr = container
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(a) = arr.as_array_mut() {
            if !a.iter().any(|v| v.as_str() == Some(field)) {
                a.push(json!(field));
            }
        }
    } else if let Some(a) = container.get_mut("required").and_then(Value::as_array_mut) {
        a.retain(|v| v.as_str() != Some(field));
        if a.is_empty() {
            container.remove("required");
        }
    }
}

/// Short, dependency-free fingerprint of an input schema, so two source maps
/// (e.g. before/after a spec bump) can be diffed to spot which tools' schemas
/// changed, and so overlays can pin the schema they were authored against
/// (`authored_sha`). FNV-1a: stable across Rust versions — pins are persisted
/// in user configs, so this hash must never change. Not cryptographic — a
/// drift detector, not a security hash.
/// Compatibility of one overlay binding against the live tool: `ok`, `changed`
/// (drift — pinned schema differs but the contract still holds), or `broken`
/// (the target tool is gone, or a patched field no longer exists). The overlay's
/// contract is latent in itself: the `fields` it re-describes must exist in the
/// tool's schema. Response paths can't be checked statically (no response schema)
/// — drift on those is caught by the `authored_sha` fingerprint.
pub(super) fn binding_status(
    o: &crate::config::Overlay,
    resolved: Option<&Value>,
    live_sha: &str,
) -> (&'static str, Vec<String>) {
    let Some(resolved) = resolved else {
        return ("broken", vec![format!("target tool {:?} is not in the surface", o.tool)]);
    };
    // Each field path (dotted through `properties`) must resolve to a real field
    // in the tool's *own* (unpatched) schema.
    let missing: Vec<String> = o
        .fields
        .keys()
        .filter(|f| !field_path_exists(resolved, f))
        .map(|f| format!("patches field {f:?}, which the tool's schema no longer has"))
        .collect();
    if !missing.is_empty() {
        return ("broken", missing);
    }
    if let Some(authored) = &o.authored_sha {
        if authored != live_sha {
            return (
                "changed",
                vec![format!("upstream schema drifted since authored ({authored} → {live_sha}); re-verify")],
            );
        }
    }
    ("ok", vec![])
}

/// Does a dotted `properties` path resolve to a real field? (binding integrity.)
pub(super) fn field_path_exists(schema: &Value, path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    let Some(field) = parts.pop() else { return false };
    let mut node = schema;
    for s in parts {
        match node.get("properties").and_then(|p| p.get(s)) {
            Some(n) => node = n,
            None => return false,
        }
    }
    node.get("properties").and_then(Value::as_object).map(|p| p.contains_key(field)).unwrap_or(false)
}
