//! The stateless 2026-07-28 revision, as the upstream clients need it: telling
//! a modern server from a legacy one, the per-request `_meta`, the mirrored HTTP
//! headers, and the `x-mcp-header` rules. Kept in one place so the stdio and
//! HTTP clients classify servers identically.
//!
//! The agent-facing side needs none of this: rmcp serves both eras.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Result};
use base64::Engine;
use serde_json::{json, Value};

/// The modern revision the upstream clients speak.
pub const MODERN_VERSION: &str = "2026-07-28";

/// How long a `server/discover` probe waits. A legacy stdio server may ignore
/// a request that arrives before `initialize`, so silence has to mean "legacy";
/// matches rmcp's own auto-discover window.
pub const PROBE_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(10) };

const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
const HEADER_MISMATCH: i64 = -32020;
const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;

const BASE64_PREFIX: &str = "=?base64?";
const BASE64_SUFFIX: &str = "?=";

/// Which side of the handshake removal an upstream is on. A property of the
/// server, so it is decided once per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    /// `initialize` handshake, sessions over HTTP.
    Legacy,
    /// Self-describing requests, no handshake, no sessions.
    Modern,
}

/// A JSON-RPC error reply from an upstream, kept structured because the probe
/// reads its code. Displays as the error text the agent and the logs have
/// always seen.
#[derive(Debug)]
pub struct RpcError {
    pub upstream: String,
    pub method: String,
    pub error: Value,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream {} error on {}: {}", self.upstream, self.method, self.error)
    }
}

impl std::error::Error for RpcError {}

/// `params` with the `_meta` every modern request has to carry.
pub fn with_meta(mut params: Value) -> Value {
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": MODERN_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "min-mcp", "version": env!("CARGO_PKG_VERSION")},
    });
    if let Some(obj) = params.as_object_mut() {
        obj.insert("_meta".to_string(), meta);
    }
    params
}

/// Decide the era from a `server/discover` outcome. Only a `DiscoverResult` or
/// a recognized modern error identifies a modern server; the spec forbids
/// keying the fallback to any one legacy error, so everything else is legacy.
pub fn classify_probe(upstream: &str, probe: &Result<Value>) -> Result<Era> {
    match probe {
        Ok(result) => match result.get("supportedVersions").and_then(Value::as_array) {
            Some(versions) => from_versions(upstream, versions),
            None => Ok(Era::Legacy),
        },
        Err(e) => {
            let Some(rpc) = e.downcast_ref::<RpcError>() else {
                return Ok(Era::Legacy);
            };
            match rpc.error.get("code").and_then(Value::as_i64) {
                Some(UNSUPPORTED_PROTOCOL_VERSION) => {
                    let versions = rpc.error.pointer("/data/supported").and_then(Value::as_array);
                    from_versions(upstream, versions.map_or(&[], Vec::as_slice))
                }
                Some(HEADER_MISMATCH | MISSING_REQUIRED_CLIENT_CAPABILITY) => Ok(Era::Modern),
                _ => Ok(Era::Legacy),
            }
        }
    }
}

/// The era to use against a server's advertised versions. ISO dates compare
/// lexically, so anything before the modern revision still has `initialize`.
fn from_versions(upstream: &str, versions: &[Value]) -> Result<Era> {
    let names: Vec<&str> = versions.iter().filter_map(Value::as_str).collect();
    if names.contains(&MODERN_VERSION) {
        return Ok(Era::Modern);
    }
    if names.iter().any(|v| *v < MODERN_VERSION) {
        return Ok(Era::Legacy);
    }
    bail!("upstream {upstream} supports only {names:?}; min-mcp speaks {MODERN_VERSION} and earlier")
}

/// A modern result min-mcp can act on. `input_required` asks the client to run
/// an interaction and retry; a headless proxy has no one to ask, and treating
/// it as a finished result would hand the agent an empty success.
pub fn ensure_complete(upstream: &str, method: &str, result: &Value) -> Result<()> {
    match result.get("resultType").and_then(Value::as_str) {
        None | Some("complete") => Ok(()),
        Some("input_required") => {
            bail!("upstream {upstream} needs interactive input for {method}, which min-mcp cannot provide")
        }
        Some(other) => bail!("upstream {upstream} returned unsupported resultType {other:?} for {method}"),
    }
}

/// Encode a value for an `Mcp-Name` or `Mcp-Param-*` header: plain when it is
/// safe as an ASCII field value, the Base64 sentinel otherwise.
pub fn header_value(value: &str) -> String {
    let edge = |b: Option<&u8>| matches!(b, Some(b' ' | b'\t'));
    let bytes = value.as_bytes();
    let unsafe_text = edge(bytes.first())
        || edge(bytes.last())
        || value.chars().any(|c| !(' '..='~').contains(&c))
        || (value.starts_with(BASE64_PREFIX) && value.ends_with(BASE64_SUFFIX));
    if !unsafe_text {
        return value.to_string();
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(value);
    format!("{BASE64_PREFIX}{encoded}{BASE64_SUFFIX}")
}

/// The `Mcp-Name` source for a request, if the method has one.
pub fn request_name<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    let key = match method {
        "tools/call" | "prompts/get" => "name",
        "resources/read" => "uri",
        _ => return None,
    };
    params.get(key).and_then(Value::as_str)
}

/// One parameter mirrored into an `Mcp-Param-{header}` request header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamHeader {
    pub path: Vec<String>,
    pub header: String,
}

/// The `x-mcp-header` annotations of a tool's `inputSchema`, or why the
/// definition is invalid (the spec has the HTTP client drop such a tool).
pub fn param_headers(schema: &Value) -> Result<Vec<ParamHeader>, String> {
    let mut found = Vec::new();
    collect_annotations(schema, Some(&[]), &mut found)?;
    let mut seen = HashSet::new();
    for p in &found {
        if !seen.insert(p.header.to_ascii_lowercase()) {
            return Err(format!("duplicate x-mcp-header {:?}", p.header));
        }
    }
    Ok(found)
}

/// Walk a schema. `path` is `Some` while every step so far was a `properties`
/// key, the only place an annotation is valid.
fn collect_annotations(schema: &Value, path: Option<&[String]>, out: &mut Vec<ParamHeader>) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        if let Some(items) = schema.as_array() {
            for item in items {
                collect_annotations(item, None, out)?;
            }
        }
        return Ok(());
    };
    for (key, value) in obj {
        match key.as_str() {
            "x-mcp-header" => match path {
                Some(p) if !p.is_empty() => out.push(annotation(value, obj, p)?),
                _ => return Err("x-mcp-header outside a property reachable through `properties`".to_string()),
            },
            "properties" => {
                for (name, property) in value.as_object().into_iter().flatten() {
                    let next = path.map(|p| [p, std::slice::from_ref(name)].concat());
                    collect_annotations(property, next.as_deref(), out)?;
                }
            }
            // instance data, where the key is not an annotation
            "default" | "const" | "enum" | "examples" | "example" => {}
            _ => collect_annotations(value, None, out)?,
        }
    }
    Ok(())
}

fn annotation(value: &Value, property: &serde_json::Map<String, Value>, path: &[String]) -> Result<ParamHeader, String> {
    let header = value.as_str().filter(|h| !h.is_empty() && h.chars().all(is_tchar));
    let header = header.ok_or_else(|| format!("invalid x-mcp-header {value} on {}", path.join(".")))?;
    match property.get("type").and_then(Value::as_str) {
        Some("string" | "integer" | "boolean") => {}
        _ => return Err(format!("x-mcp-header on non-primitive parameter {}", path.join("."))),
    }
    Ok(ParamHeader { path: path.to_vec(), header: header.to_string() })
}

/// RFC 9110 token character.
fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

/// The `(header name, encoded value)` pairs for a call. A parameter absent from
/// the arguments, or null, has no header.
pub fn param_values(headers: &[ParamHeader], arguments: &Value) -> Vec<(String, String)> {
    let value_at = |path: &[String]| path.iter().try_fold(arguments, |v, key| v.get(key));
    headers
        .iter()
        .filter_map(|p| {
            let text = match value_at(&p.path)? {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            Some((format!("Mcp-Param-{}", p.header), header_value(&text)))
        })
        .collect()
}

/// Remove the tools whose `x-mcp-header` annotations are invalid from a
/// `tools/list` result, returning `(tool, headers)` for the rest that have any.
pub fn admit_tools(upstream: &str, result: &mut Value) -> Vec<(String, Vec<ParamHeader>)> {
    let mut admitted = Vec::new();
    let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) else {
        return admitted;
    };
    tools.retain(|tool| {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        match tool.get("inputSchema").map_or_else(|| Ok(Vec::new()), param_headers) {
            Ok(headers) => {
                if !headers.is_empty() {
                    admitted.push((name.to_string(), headers));
                }
                true
            }
            Err(reason) => {
                crate::log_warn!("dropping tool {name:?} from upstream {upstream}: {reason}");
                false
            }
        }
    });
    admitted
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn rpc_error(error: Value) -> Result<Value> {
        Err(anyhow::Error::new(RpcError { upstream: "u".into(), method: "server/discover".into(), error }))
    }

    #[test]
    fn a_discover_result_naming_our_version_is_modern() {
        let probe = Ok(json!({"supportedVersions": ["2025-11-25", MODERN_VERSION]}));
        assert_eq!(classify_probe("u", &probe).unwrap(), Era::Modern);
    }

    #[test]
    fn a_result_without_versions_is_legacy() {
        // a sloppy legacy server answering every method with a generic result
        assert_eq!(classify_probe("u", &Ok(json!({"tools": []}))).unwrap(), Era::Legacy);
    }

    #[test]
    fn the_fallback_is_not_keyed_to_one_error_code() {
        for code in [-32601, -32602, -32000, 7] {
            let probe = rpc_error(json!({"code": code, "message": "nope"}));
            assert_eq!(classify_probe("u", &probe).unwrap(), Era::Legacy, "code {code}");
        }
        assert_eq!(classify_probe("u", &Err(anyhow!("HTTP 400"))).unwrap(), Era::Legacy);
    }

    #[test]
    fn an_unsupported_version_error_uses_the_advertised_list() {
        let dual = rpc_error(json!({"code": -32022, "data": {"supported": ["2025-11-25", "2027-03-01"]}}));
        assert_eq!(classify_probe("u", &dual).unwrap(), Era::Legacy, "a shared legacy version remains usable");
        let future = rpc_error(json!({"code": -32022, "data": {"supported": ["2027-03-01"]}}));
        let err = classify_probe("u", &future).unwrap_err().to_string();
        assert!(err.contains("2027-03-01"), "no shared version must say so: {err}");
    }

    #[test]
    fn a_header_error_is_a_modern_server_not_a_reason_to_fall_back() {
        let probe = rpc_error(json!({"code": -32020, "message": "Header mismatch"}));
        assert_eq!(classify_probe("u", &probe).unwrap(), Era::Modern);
    }

    #[test]
    fn meta_sits_beside_the_params() {
        let p = with_meta(json!({"name": "t"}));
        assert_eq!(p["name"], "t");
        assert_eq!(p["_meta"]["io.modelcontextprotocol/protocolVersion"], MODERN_VERSION);
        assert!(p["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object());
    }

    #[test]
    fn only_a_complete_result_passes() {
        assert!(ensure_complete("u", "tools/call", &json!({})).is_ok(), "legacy-shaped results count as complete");
        assert!(ensure_complete("u", "tools/call", &json!({"resultType": "complete"})).is_ok());
        let err = ensure_complete("u", "tools/call", &json!({"resultType": "input_required"})).unwrap_err();
        assert!(err.to_string().contains("interactive input"), "{err}");
        assert!(ensure_complete("u", "tools/call", &json!({"resultType": "task"})).is_err());
    }

    #[test]
    fn header_values_follow_the_specs_examples() {
        assert_eq!(header_value("us-west1"), "us-west1");
        assert_eq!(header_value("Hello, 世界"), "=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(header_value("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(header_value("=?base64?literal?="), "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=");
    }

    #[test]
    fn annotations_are_found_through_properties_chains() {
        let schema = json!({"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "opts": {"type": "object", "properties": {"tier": {"type": "integer", "x-mcp-header": "Tier"}}},
            "query": {"type": "string"},
        }});
        let found = param_headers(&schema).unwrap();
        let args = json!({"region": "us", "opts": {"tier": 3}, "query": "q"});
        let mut values = param_values(&found, &args);
        values.sort();
        assert_eq!(values, [("Mcp-Param-Region".to_string(), "us".to_string()), ("Mcp-Param-Tier".to_string(), "3".to_string())]);
    }

    #[test]
    fn an_absent_or_null_parameter_has_no_header() {
        let found = param_headers(&json!({"properties": {"r": {"type": "string", "x-mcp-header": "R"}}})).unwrap();
        assert!(param_values(&found, &json!({})).is_empty());
        assert!(param_values(&found, &json!({"r": null})).is_empty());
    }

    #[test]
    fn invalid_annotations_are_rejected() {
        for schema in [
            json!({"properties": {"a": {"type": "number", "x-mcp-header": "A"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": ""}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "has space"}}}),
            json!({"properties": {"a": {"type": "array", "items": {"type": "string", "x-mcp-header": "A"}}}}),
            json!({"oneOf": [{"properties": {"a": {"type": "string", "x-mcp-header": "A"}}}]}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "X"}, "b": {"type": "string", "x-mcp-header": "x"}}}),
        ] {
            assert!(param_headers(&schema).is_err(), "should reject {schema}");
        }
    }

    #[test]
    fn a_property_or_default_named_like_the_annotation_is_not_one() {
        let schema = json!({"properties": {
            "x-mcp-header": {"type": "string"},
            "a": {"type": "object", "default": {"x-mcp-header": "not an annotation"}},
        }});
        assert!(param_headers(&schema).unwrap().is_empty());
    }

    #[test]
    fn a_bad_definition_drops_only_that_tool() {
        let mut result = json!({"tools": [
            {"name": "good", "inputSchema": {"properties": {"r": {"type": "string", "x-mcp-header": "R"}}}},
            {"name": "bad", "inputSchema": {"properties": {"r": {"type": "number", "x-mcp-header": "R"}}}},
            {"name": "plain", "inputSchema": {"type": "object"}},
        ]});
        let admitted = admit_tools("u", &mut result);
        let names: Vec<&str> = result["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["good", "plain"]);
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].0, "good");
    }
}
