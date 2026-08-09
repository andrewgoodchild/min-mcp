//! Execute one OpenAPI operation as a real HTTP call. The body encoding is
//! chosen from the spec's declared request media type — form (Stripe's
//! bracket notation) vs JSON (GitHub) — so mechanics are data-driven, never
//! per-API special cases (design law 1). Ported/generalized from the Python
//! executor (ported from a Python prototype).

use anyhow::Result;
use serde_json::{json, Value};

use crate::config::ResultFormat;
use crate::spec::{Operation, QueryStyle, Spec};

// A high safety net only — NOT the agent-facing budget. The budget truncation
// happens in the surface AFTER overlays/projection shrink the payload; if we
// truncated to a small size here, a large body would be cut mid-JSON and the
// overlay/projection transform couldn't parse it (defeating filtering on
// exactly the large responses it's meant for).
const MAX_RESPONSE_CHARS: usize = 2_000_000;

/// Shape a raw response body per the configured format, then apply the safety
/// cap. `Raw` keeps the upstream bytes as a string; `Json` embeds the parsed
/// value AS JSON in the result envelope — `{"status":200,"body":{...}}` — so
/// the agent never reads a JSON blob escaped inside a string (measured ~15%
/// token overhead, and it blinded downstream structural transforms). A body
/// that isn't JSON (empty, HTML error) stays a string either way.
fn format_body(text: &str, fmt: ResultFormat) -> (Value, bool) {
    if fmt == ResultFormat::Json {
        if let Ok(v) = serde_json::from_str::<Value>(text) {
            // Cap by the compact rendering's char count — same budget the string
            // path uses. An over-budget body degrades to a truncated string
            // (truncated JSON can't be embedded as a value).
            let compact = serde_json::to_string(&v).unwrap_or_else(|_| text.to_string());
            if compact.chars().count() <= MAX_RESPONSE_CHARS {
                return (v, false);
            }
            let (cut, _) = truncate_chars(&compact);
            return (Value::String(cut), true);
        }
    }
    let (cut, truncated) = truncate_chars(text);
    (Value::String(cut), truncated)
}

/// Truncate by CHARACTER count (byte-boundary safe) and report whether a char
/// actually had to be cut — not byte length, which would mislabel a multibyte
/// body that fits in the char budget.
fn truncate_chars(rendered: &str) -> (String, bool) {
    match rendered.char_indices().nth(MAX_RESPONSE_CHARS) {
        Some((byte_idx, _)) => (rendered[..byte_idx].to_string(), true),
        None => (rendered.to_string(), false),
    }
}

/// Percent-encode an agent-supplied value as a single URL path segment (RFC
/// 3986 unreserved kept; everything else — crucially `/` — encoded). A pure `.`
/// or `..` segment is encoded too, since it would otherwise traverse a level.
fn encode_path_segment(s: &str) -> String {
    if s == "." {
        return "%2E".to_string();
    }
    if s == ".." {
        return "%2E%2E".to_string();
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// {"a": {"b": 1}, "c": [{"d": 2}]} -> [("a[b]","1"), ("c[0][d]","2")]
/// (Stripe's form-encoding convention for nested bodies.)
pub fn flatten_form(value: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}[{k}]") };
                flatten_form(v, &key, out);
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                flatten_form(v, &format!("{prefix}[{i}]"), out);
            }
        }
        Value::Bool(b) => out.push((prefix.to_string(), if *b { "true" } else { "false" }.into())),
        Value::Null => {}
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        n => out.push((prefix.to_string(), n.to_string())),
    }
}

/// Serialize one query parameter (`key` -> `value`) into wire pairs per its
/// OpenAPI serialization `style`. Scalars are always `key=value`; the style only
/// decides how arrays expand. Objects always use bracket notation (`deepObject` /
/// the Stripe convention). An array whose elements aren't all scalar has no
/// well-defined flat form, so it falls back to indexed brackets.
fn serialize_query(key: &str, value: &Value, style: QueryStyle, out: &mut Vec<(String, String)>) {
    match value {
        Value::Array(items) => {
            let scalars: Option<Vec<String>> = items.iter().map(scalar_str).collect();
            match (style, scalars) {
                (QueryStyle::Deep, _) | (_, None) => flatten_form(value, key, out),
                (QueryStyle::Repeated, Some(vs)) => {
                    out.extend(vs.into_iter().map(|v| (key.to_string(), v)));
                }
                (QueryStyle::Delimited(sep), Some(vs)) => {
                    out.push((key.to_string(), vs.join(&sep.to_string())));
                }
            }
        }
        Value::Object(_) => flatten_form(value, key, out), // deepObject / nested
        Value::Null => {}
        scalar => {
            if let Some(s) = scalar_str(scalar) {
                out.push((key.to_string(), s));
            }
        }
    }
}

/// A single query/form value as a string — matching `flatten_form`'s rendering
/// (bool → `true`/`false`, numbers verbatim). `None` for anything non-scalar
/// (array/object/null), which the caller treats as "no flat form".
fn scalar_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub struct Executor {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    /// Sent as the Accept header (GitHub wants application/vnd.github+json).
    accept: Option<String>,
    /// Extra static request headers (already ${VAR}-resolved). Covers mandatory
    /// runtime headers a spec omits — e.g. Notion's `Notion-Version` — which the
    /// "From REST to MCP" study (arXiv:2507.16044) found caused 47% of real
    /// API-wrapping failures.
    headers: Vec<(String, String)>,
    /// How response bodies are serialized back to the agent.
    result_format: ResultFormat,
}

/// Resolve a spec upstream's static header map: expand `${VAR}` from the
/// environment so a required runtime header can be set without hardcoding a
/// secret. An unset variable is a hard error (fail loud). Reserved headers
/// (Authorization, Accept, User-Agent) are set elsewhere and skipped here.
pub fn resolve_headers(
    headers: &std::collections::HashMap<String, String>,
) -> Result<Vec<(String, String)>> {
    const RESERVED: [&str; 3] = ["authorization", "accept", "user-agent"];
    headers
        .iter()
        .filter(|(k, _)| !RESERVED.contains(&k.to_lowercase().as_str()))
        .map(|(k, v)| Ok((k.clone(), crate::config::expand_env(v)?)))
        .collect()
}

impl Executor {
    pub fn new(
        base_url: &str,
        api_key: &str,
        accept: Option<String>,
        headers: Vec<(String, String)>,
        result_format: ResultFormat,
    ) -> Self {
        Executor {
            // Same 120s transport ceiling the MCP clients have — a spec upstream
            // that accepts the connection but never responds must not wedge the
            // whole proxy behind the surface mutex (reqwest has NO default).
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            accept,
            headers,
            result_format,
        }
    }

    // One HTTP call's inputs, threaded from dispatch. Grouping them into a
    // struct would add a type for a single call site without making the call
    // clearer, so the lint is allowed deliberately.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        spec: &Spec,
        op: &Operation,
        path_params: &Value,
        query_params: &Value,
        body: &Value,
        extra_headers: &[(String, String)],
        deadline: Option<std::time::Duration>,
    ) -> Result<Value> {
        // fill {placeholders}. Path param values are agent-controlled, so they
        // are percent-encoded as a single path segment — otherwise a value like
        // "../charges" or ".." would let the agent reach a DIFFERENT endpoint on
        // the upstream host, bypassing the tool boundary and scope filtering.
        let mut path = op.path.clone();
        if let Some(pp) = path_params.as_object() {
            for (k, v) in pp {
                let val = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                path = path.replace(&format!("{{{k}}}"), &encode_path_segment(&val));
            }
        }
        if let Some(start) = path.find('{') {
            let missing = &path[start + 1..path[start..].find('}').map(|e| start + e).unwrap_or(path.len())];
            return Ok(json!({"error": format!("missing path parameter {missing:?} for {}", op.op_id)}));
        }

        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .client
            .request(reqwest::Method::from_bytes(op.method.to_uppercase().as_bytes())?, &url);
        // Assemble the FULL header set as one case-insensitive-deduped list so a
        // per-operation overlay header *overrides* any base header of the same name
        // — including the reserved User-Agent/Authorization/Accept — instead of being
        // sent as a second, duplicate header (reqwest's `.header()` appends, and two
        // conflicting Authorization headers is exactly the ambiguity to avoid).
        let mut merged: Vec<(String, String)> = vec![("user-agent".into(), "min-mcp".into())];
        // Only send Authorization when we actually have a key — a no-auth public API
        // (auth_env unset/empty) must not receive an empty `Bearer `, which many
        // servers reject with a 4xx/5xx.
        if !self.api_key.is_empty() {
            merged.push(("authorization".into(), format!("Bearer {}", self.api_key)));
        }
        if let Some(accept) = &self.accept {
            merged.push(("accept".into(), accept.clone()));
        }
        merged.extend(self.headers.iter().cloned());
        for (k, v) in extra_headers {
            merged.retain(|(mk, _)| !mk.eq_ignore_ascii_case(k)); // overlay wins over any prior
            merged.push((k.clone(), v.clone()));
        }
        for (k, v) in &merged {
            req = req.header(k, v);
        }

        // query params — serialized per each parameter's OpenAPI style/explode
        // (or Swagger 2.0 collectionFormat), so array params are correct for both
        // GitHub-style repeated keys and Stripe-style bracket notation.
        if let Some(q) = query_params.as_object() {
            let styles = spec.query_styles(op);
            let default = spec.default_query_style();
            let mut pairs = Vec::new();
            for (k, v) in q {
                let style = styles.get(k).copied().unwrap_or(default);
                serialize_query(k, v, style, &mut pairs);
            }
            req = req.query(&pairs);
        }

        // body: encode per the operation's declared media type
        if !body.is_null() && body.as_object().map(|o| !o.is_empty()).unwrap_or(true) {
            let media = spec.body_media_type(op).unwrap_or_default();
            if media.contains("json") {
                req = req.json(body);
            } else {
                // default to form encoding (Stripe and most legacy APIs)
                let mut pairs = Vec::new();
                flatten_form(body, "", &mut pairs);
                req = req.form(&pairs);
            }
        }

        if let Some(d) = deadline {
            req = req.timeout(d); // overlay timeout_s: tighter than the 120s default
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if deadline.is_some() && e.is_timeout() => {
                // Spec-path transport issues surface as error envelopes (not
                // Err), so the timeout does too — worded for the agent.
                return Ok(json!({"error": format!(
                    "TIMEOUT after {}s: {}",
                    deadline.unwrap_or_default().as_secs(),
                    crate::upstream::TIMEOUT_GUIDANCE
                )}));
            }
            Err(e) => return Ok(json!({"error": format!("transport error: {e}")})),
        };
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let (body_out, truncated) = format_body(&text, self.result_format);
        Ok(json!({"status": status, "body": body_out, "truncated": truncated}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(v: Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        flatten_form(&v, "", &mut out);
        out
    }

    #[test]
    fn resolve_headers_expands_env_and_skips_reserved() {
        use std::collections::HashMap;
        std::env::set_var("MINMCP_TEST_HDR", "2022-06-28");
        let mut h = HashMap::new();
        h.insert("Notion-Version".to_string(), "${MINMCP_TEST_HDR}".to_string());
        h.insert("X-Static".to_string(), "literal".to_string());
        h.insert("Authorization".to_string(), "Bearer nope".to_string()); // reserved -> dropped
        let mut got = resolve_headers(&h).unwrap();
        got.sort();
        assert_eq!(got, vec![
            ("Notion-Version".to_string(), "2022-06-28".to_string()),
            ("X-Static".to_string(), "literal".to_string()),
        ]);
        // an unset var is a hard error (no empty header sent)
        let mut bad = HashMap::new();
        bad.insert("X".to_string(), "${MINMCP_DEFINITELY_UNSET_XYZ}".to_string());
        assert!(resolve_headers(&bad).is_err());
    }

    #[test]
    fn nested_bracket_notation() {
        assert_eq!(
            flat(json!({"line_items": [{"price": "p_1", "quantity": 1}]})),
            vec![("line_items[0][price]".into(), "p_1".into()),
                 ("line_items[0][quantity]".into(), "1".into())]
        );
    }

    #[test]
    fn booleans_and_nulls() {
        assert_eq!(flat(json!({"active": true, "x": null, "n": 2})),
                   vec![("active".into(), "true".into()), ("n".into(), "2".into())]);
    }

    fn q(key: &str, v: Value, style: QueryStyle) -> Vec<(String, String)> {
        let mut out = Vec::new();
        serialize_query(key, &v, style, &mut out);
        out
    }

    #[test]
    fn query_array_repeated_is_openapi_default() {
        // GitHub declares no style -> form+explode default -> repeated keys.
        // (The old code sent exclude[0]=/exclude[1]=, which GitHub ignores.)
        assert_eq!(
            q("exclude", json!(["repos", "issues"]), QueryStyle::Repeated),
            vec![("exclude".into(), "repos".into()), ("exclude".into(), "issues".into())]
        );
    }

    #[test]
    fn query_array_deepobject_keeps_brackets() {
        // Stripe `expand` is style=deepObject -> bracketed (unchanged behavior).
        assert_eq!(
            q("expand", json!(["customer", "data.source"]), QueryStyle::Deep),
            vec![("expand[0]".into(), "customer".into()), ("expand[1]".into(), "data.source".into())]
        );
    }

    #[test]
    fn query_array_delimited_joins() {
        assert_eq!(
            q("status", json!(["available", "sold"]), QueryStyle::Delimited(',')),
            vec![("status".into(), "available,sold".into())]
        );
        assert_eq!(q("t", json!([1, 2, 3]), QueryStyle::Delimited('|')), vec![("t".into(), "1|2|3".into())]);
    }

    #[test]
    fn query_object_and_scalar_unaffected_by_style() {
        // Objects always bracket (Stripe created[gte]=), regardless of the style
        // that would apply to an array; scalars are always key=value.
        assert_eq!(
            q("created", json!({"gte": 1600}), QueryStyle::Repeated),
            vec![("created[gte]".into(), "1600".into())]
        );
        assert_eq!(q("limit", json!(10), QueryStyle::Deep), vec![("limit".into(), "10".into())]);
        // array of objects has no flat form -> indexed brackets even under Repeated
        assert_eq!(
            q("f", json!([{"k": "v"}]), QueryStyle::Repeated),
            vec![("f[0][k]".into(), "v".into())]
        );
    }

    #[test]
    fn path_segment_encoding_blocks_traversal() {
        // agent-supplied path params can't escape their segment
        assert_eq!(encode_path_segment("../charges"), "..%2Fcharges");
        assert!(!encode_path_segment("../charges").contains('/'));
        assert_eq!(encode_path_segment(".."), "%2E%2E");
        assert_eq!(encode_path_segment("."), "%2E");
        // legitimate ids pass through unchanged
        assert_eq!(encode_path_segment("cus_ABC123"), "cus_ABC123");
        // spaces / reserved chars are encoded
        assert_eq!(encode_path_segment("a b"), "a%20b");
    }

    #[test]
    fn format_body_json_embeds_parsed_value() {
        let raw = "{ \"data\": [ {\"id\": 1} ] }"; // whitespace + valid JSON
        let (body, t) = format_body(raw, ResultFormat::Json);
        assert_eq!(body, json!({"data": [{"id": 1}]})); // real JSON, not an escaped string
        assert!(!t);
    }

    #[test]
    fn format_body_raw_and_non_json_pass_through() {
        let (out, _) = format_body("plain text, not json", ResultFormat::Raw);
        assert_eq!(out, json!("plain text, not json"));
        // Json mode falls back to the raw text when the body isn't JSON
        let (out, _) = format_body("<html>500</html>", ResultFormat::Json);
        assert_eq!(out, json!("<html>500</html>"));
    }

    #[test]
    fn format_body_truncation_flag_counts_chars_not_bytes() {
        // A body of exactly MAX multibyte chars fits the char budget: NOT truncated,
        // even though its byte length far exceeds MAX (the code-review fix).
        let fits = "€".repeat(MAX_RESPONSE_CHARS); // 3 bytes each
        let (out, truncated) = format_body(&fits, ResultFormat::Raw);
        assert!(!truncated, "char count within budget must not be flagged truncated");
        assert_eq!(out.as_str().unwrap().chars().count(), MAX_RESPONSE_CHARS);
        // One more char and it must truncate, on a char boundary.
        let over = "€".repeat(MAX_RESPONSE_CHARS + 100);
        let (out, truncated) = format_body(&over, ResultFormat::Raw);
        assert!(truncated);
        let s = out.as_str().unwrap();
        assert_eq!(s.chars().count(), MAX_RESPONSE_CHARS);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
    }

    #[test]
    fn format_body_json_over_budget_degrades_to_truncated_string() {
        // A parseable JSON body larger than the cap can't be embedded whole —
        // it degrades to a truncated string and flags `truncated`.
        let big = format!("{{\"blob\":\"{}\"}}", "x".repeat(MAX_RESPONSE_CHARS + 10));
        let (out, truncated) = format_body(&big, ResultFormat::Json);
        assert!(truncated);
        assert!(out.is_string());
    }
}
