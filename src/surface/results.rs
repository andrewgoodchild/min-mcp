//! Tool-result shaping: envelopes, transforms, budgets, projection nudges.

use super::*;

pub(crate) fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// First text block of a tool result.
pub(super) fn result_text(result: &Value) -> &str {
    result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|b| b.first())
        .and_then(|b| b.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// The API payload inside a tool result: the spec envelope's `body` (embedded
/// JSON, or a string body parsed), the parsed content JSON (MCP), or raw text.
pub(super) fn result_payload(result: &Value) -> Value {
    let text = result_text(result);
    let Ok(parsed) = serde_json::from_str::<Value>(text) else {
        return Value::String(text.to_string());
    };
    if is_envelope(&parsed) {
        match &parsed["body"] {
            Value::String(body) => {
                if let Ok(b) = serde_json::from_str::<Value>(body) {
                    return b;
                }
            }
            body => return body.clone(),
        }
    }
    parsed
}

/// Is this parsed result text the spec-executor envelope `{status, body, ...}`?
/// (An MCP payload that merely *has* a `body` key lacks `status`, and vice versa.)
pub(super) fn is_envelope(parsed: &Value) -> bool {
    parsed.is_object() && parsed.get("status").is_some() && parsed.get("body").is_some()
}

/// Evaluate a verify check's assertions against a tool result. Returns
/// (passed, failure-reasons). Every specified assertion must hold.
pub(super) fn eval_expect(result: &Value, e: &crate::config::Expect) -> (bool, Vec<String>) {
    let mut fails = Vec::new();
    let text = result_text(result);

    if let Some(want) = e.is_error {
        let got = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        if got != want {
            fails.push(format!("is_error: expected {want}, got {got}"));
        }
    }
    if let Some(want) = e.status {
        // spec-backend envelope carries `status`; MCP results don't
        let got = serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|v| v.get("status").and_then(Value::as_u64));
        if got != Some(want) {
            fails.push(format!("status: expected {want}, got {got:?}"));
        }
    }
    if let Some(sub) = &e.contains {
        if !text.contains(sub.as_str()) {
            fails.push(format!("contains {sub:?}: not found in result"));
        }
    }
    // has/missing check the parsed API payload (through the spec envelope)
    if !e.has.is_empty() || !e.missing.is_empty() {
        let payload = result_payload(result);
        for p in &e.has {
            if matches!(get_path(&payload, p), None | Some(Value::Null)) {
                fails.push(format!("has {p:?}: missing or null"));
            }
        }
        for p in &e.missing {
            if !matches!(get_path(&payload, p), None | Some(Value::Null)) {
                fails.push(format!("missing {p:?}: unexpectedly present"));
            }
        }
    }
    (fails.is_empty(), fails)
}

/// Navigate a dotted path into a JSON value: object keys, numeric array index, or
/// a `key[last]` segment that takes the last element of that array (the pagination
/// cursor-by-last-item case, e.g. Stripe's `data[last].id`).
pub(super) fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = if let Some(key) = seg.strip_suffix("[last]") {
            let arr = if key.is_empty() { cur.as_array()? } else { cur.get(key)?.as_array()? };
            arr.last()?
        } else {
            match cur {
                Value::Object(m) => m.get(seg)?,
                Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
                _ => return None,
            }
        };
    }
    Some(cur)
}

pub(super) const PROJECTION_NUDGE_CHARS: usize = 2_000;

/// Max chars of a tool result shown to the agent, applied AFTER overlays and
/// projection have shrunk the payload (so a projected result is never clipped).
pub(super) const AGENT_RESULT_BUDGET: usize = 8_000;

/// Bound each result text block to the agent budget, char-boundary safe.
pub(super) fn truncate_result_text(result: &mut Value, max: usize) {
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for b in blocks {
        let Some(text) = b.get("text").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        if text.len() > max {
            let cut = budget_truncate(
                text,
                max,
                "\n…[truncated by min-mcp — request fewer fields to see the rest]",
            );
            if let Some(obj) = b.as_object_mut() {
                obj.insert("text".into(), Value::String(cut));
            }
        }
    }
}

/// If a result is large and wasn't projected, append a one-line hint pointing at
/// `fields` (agents understand projection but don't reach for it unprompted).
pub(super) fn nudge_projection(result: &mut Value) {
    let size: usize = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .map(str::len)
                .sum()
        })
        .unwrap_or(0);
    if size <= PROJECTION_NUDGE_CHARS {
        return;
    }
    if let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) {
        blocks.push(json!({"type": "text", "text": format!(
            "HINT: this result is ~{size} chars. If you only need some of it, re-call \
             call_tool with a `fields` array (e.g. fields=[\"data[].id\",\"data[].amount\"]) \
             to return just those fields and save context."
        )}));
    }
}

/// Apply an overlay's declarative response transform (remove /
/// rename / set) and then its jq escape hatch, to a JSON payload in place.
///
/// `is_error` gates the `keep` allowlist only: keep paths are tuned to the
/// SUCCESS shape, so applying them to an error body annihilates it — the agent
/// gets `null` instead of the error and cannot recover (measured: 94%→11% task
/// success). remove/rename/set stay active on errors (shape-tolerant no-ops on
/// miss, and stripping secrets from error bodies is the point of `always`).
/// An explicit `when: error` opts keep back in — the author is targeting errors.
pub(super) fn apply_response_transform(
    payload: &mut Value,
    rt: &crate::config::ResponseTransform,
    is_error: bool,
) {
    if !rt.keep.is_empty() && (!is_error || rt.when == crate::config::When::Error) {
        *payload = crate::project::project(payload, &rt.keep); // allowlist first
    }
    if !rt.remove.is_empty() {
        crate::project::prune(payload, &rt.remove);
    }
    for (path, new_name) in &rt.rename {
        crate::project::rename(payload, path, new_name);
    }
    for (path, val) in &rt.set {
        crate::project::set(payload, path, val.clone());
    }
    if let Some(program) = &rt.jq {
        if let Some(out) = crate::jq::run(program, payload) {
            *payload = out; // jq failures leave the payload unchanged (best-effort)
        }
    }
}

/// Apply a transform to a tool result's JSON payload in place. A spec-backend
/// result is the envelope `{status, body, truncated}` — we transform the JSON
/// body (embedded value, or a legacy string body parsed and re-embedded) and
/// keep status/truncated. An MCP result is transformed directly. Text that
/// isn't JSON (or a body that isn't JSON, e.g. TOON) is left untouched. This is
/// the shared unwrap used by overlay field-drop and caller field-projection.
pub(super) fn transform_result(result: &mut Value, mut f: impl FnMut(&mut Value)) {
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let Ok(mut parsed) = serde_json::from_str::<Value>(&text) else {
            continue; // not JSON — leave as-is
        };
        let new_text = if is_envelope(&parsed) {
            // take, don't clone — parsed is local, and the continue arms drop it
            match std::mem::take(&mut parsed["body"]) {
                // current shape: body embedded as JSON — transform in place
                body @ (Value::Object(_) | Value::Array(_)) => {
                    let mut body = body;
                    f(&mut body);
                    parsed["body"] = body;
                    serde_json::to_string(&parsed).unwrap_or(text)
                }
                // legacy / raw-mode shape: body is a JSON string — parse, transform,
                // re-embed as a string (stay faithful to the declared raw format)
                Value::String(body_str) => {
                    // (body was taken; on the non-JSON continue the block text is
                    // left untouched, so the emptied local `parsed` is irrelevant)
                    let Ok(mut body_val) = serde_json::from_str::<Value>(&body_str) else {
                        continue; // body isn't JSON (e.g. TOON) — leave the block alone
                    };
                    f(&mut body_val);
                    parsed["body"] =
                        Value::String(serde_json::to_string(&body_val).unwrap_or_default());
                    serde_json::to_string(&parsed).unwrap_or(text)
                }
                _ => continue, // null/number body — nothing to transform
            }
        } else {
            f(&mut parsed);
            serde_json::to_string(&parsed).unwrap_or(text)
        };
        if let Some(obj) = block.as_object_mut() {
            obj.insert("text".into(), Value::String(new_text));
        }
    }
}

/// Client-side error result, marked so callers can tell it from a real failure.
pub(super) fn bad_arg(msg: &str) -> Value {
    text_result(
        format!("{msg}. This is a client-side argument error; fix the arguments — do not retry unchanged."),
        true,
    )
}

/// Serialized length of a JSON value WITHOUT materializing the string —
/// a byte-counting sink for size gates and estimates.
pub(super) fn json_len(v: &Value) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Counter(0);
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}
