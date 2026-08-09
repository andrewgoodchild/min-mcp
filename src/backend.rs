//! A Surface upstream is either a spawned MCP server (`Mcp`) or an OpenAPI spec
//! mounted directly (`Spec`) — the same tool surface either way. This is what
//! lets min-mcp minify a raw API spec, not just proxy existing MCP servers.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::config::UpstreamConfig;
use crate::exec::Executor;
use crate::http_upstream::HttpUpstream;
use crate::spec::Spec;
use crate::upstream::{McpRpc, ToolDef, Upstream};

// `Mcp` is about twice the size of the next variant *on Windows only*, where the
// process handles inside `Upstream` (`Child`, `ChildStdin`, the buffered stdout
// reader) are wider than their Unix fd equivalents — so this lint fires there and
// nowhere else. Boxing would trade a startup-time byte count for an indirection on
// every upstream call, and the lint's actual concern doesn't apply: these live in
// `Surface.upstreams: Vec<Backend>`, one per configured upstream, moved exactly
// once at build time and only borrowed afterwards.
#[allow(clippy::large_enum_variant)]
pub enum Backend {
    Mcp(Upstream),
    Http(HttpUpstream),
    Spec(SpecBackend),
}

impl Backend {
    /// `idx` is this backend's position in the Surface's backend list, stamped
    /// onto each ToolDef so dispatch can route back here.
    pub async fn list_tools(&mut self, idx: usize) -> Result<Vec<ToolDef>> {
        match self {
            Backend::Mcp(u) => u.list_tools(idx).await,
            Backend::Http(h) => h.list_tools(idx).await,
            Backend::Spec(s) => Ok(s.list_tools(idx)),
        }
    }

    /// Returns an MCP tool-result Value ({content, isError}) either way.
    /// `extra_headers` are per-operation request headers (from an overlay); they
    /// apply to spec (HTTP) upstreams only — MCP subprocess/HTTP upstreams ignore
    /// them (their transport isn't a per-call REST request).
    pub async fn call_tool(
        &mut self,
        name: &str,
        args: Value,
        extra_headers: &[(String, String)],
        deadline: Option<std::time::Duration>,
    ) -> Result<Value> {
        let mut r = match self {
            // Spec results compact in exec.rs, where the envelope is built.
            Backend::Spec(s) => return s.call_tool(name, args, extra_headers, deadline).await,
            Backend::Mcp(u) => u.call_tool(name, args, deadline).await?,
            Backend::Http(h) => h.call_tool(name, args, deadline).await?,
        };
        if self.result_format() == crate::config::ResultFormat::Json {
            compact_json_texts(&mut r);
        }
        Ok(r)
    }

    fn result_format(&self) -> crate::config::ResultFormat {
        match self {
            Backend::Mcp(u) => u.result_format,
            Backend::Http(h) => h.result_format,
            Backend::Spec(_) => crate::config::ResultFormat::Json, // unused on that path
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Backend::Mcp(u) => &u.name,
            Backend::Http(h) => &h.name,
            Backend::Spec(s) => &s.name,
        }
    }

    /// Fully-resolved input schema for one tool, computed ON DEMAND. Spec upstreams
    /// list tools with a cheap unresolved schema (so a 10k-op spec loads instantly);
    /// this expands the `$ref`s for just the tool the caller inspects. Non-spec
    /// backends already ship a final schema, so they return None (use the stored one).
    pub fn resolved_schema(&self, tool_name: &str) -> Option<Value> {
        match self {
            Backend::Spec(s) => s.spec.get(tool_name).map(|op| s.spec.tool_input_schema(op)),
            _ => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Mcp(_) => "mcp",
            Backend::Http(_) => "http",
            Backend::Spec(_) => "spec",
        }
    }

    /// Merged passthrough listing of one MCP collection (`resources/list` →
    /// `resources`, `prompts/list` → `prompts`). Spec upstreams have none; an
    /// upstream without the capability contributes nothing.
    pub async fn list_passthrough(&mut self, method: &str, key: &str) -> Vec<Value> {
        match self {
            Backend::Mcp(u) => u.list_passthrough(method, key).await,
            Backend::Http(h) => h.list_passthrough(method, key).await,
            Backend::Spec(_) => Vec::new(),
        }
    }

    pub async fn read_resource(&mut self, uri: &str) -> Result<Value> {
        match self {
            Backend::Mcp(u) => u.read_resource(uri).await,
            Backend::Http(h) => h.read_resource(uri).await,
            Backend::Spec(_) => anyhow::bail!("spec upstream has no resources"),
        }
    }

    pub async fn get_prompt(&mut self, name: &str, arguments: Value) -> Result<Value> {
        match self {
            Backend::Mcp(u) => u.get_prompt(name, arguments).await,
            Backend::Http(h) => h.get_prompt(name, arguments).await,
            Backend::Spec(_) => anyhow::bail!("spec upstream has no prompts"),
        }
    }

    /// Human origin of a tool by its original (upstream) name — the far end of
    /// the source map. For a spec backend that's `METHOD /path`; for an MCP
    /// server it's the tool's own name on that server.
    pub fn origin(&self, original_name: &str) -> String {
        match self {
            Backend::Mcp(_) | Backend::Http(_) => original_name.to_string(),
            Backend::Spec(s) => s
                .spec
                .get(original_name)
                .map(|op| format!("{} {}", op.method.to_uppercase(), op.path))
                .unwrap_or_else(|| original_name.to_string()),
        }
    }
}

pub struct SpecBackend {
    name: String,
    spec: Arc<Spec>,
    executor: Executor,
}

impl SpecBackend {
    pub fn new(cfg: &UpstreamConfig) -> Result<Self> {
        let spec_path = cfg.spec.as_ref().ok_or_else(|| anyhow!("spec upstream needs `spec`"))?;
        let base_url = cfg.base_url.as_ref().ok_or_else(|| anyhow!("spec upstream needs `base_url`"))?;
        // The key is read from the named env var — never stored in config.
        // Missing is non-fatal (inspect needs no key; serve surfaces 401s as
        // tool errors), matching how MCP-server upstreams start without creds.
        let api_key = cfg
            .auth_env
            .as_ref()
            .and_then(|var| std::env::var(var).ok())
            .unwrap_or_default();
        let spec = Spec::load(spec_path).with_context(|| format!("loading spec {spec_path}"))?;
        Ok(SpecBackend {
            name: cfg.name.clone(),
            spec: Arc::new(spec),
            executor: Executor::new(
                base_url,
                &api_key,
                cfg.accept.clone(),
                crate::exec::resolve_headers(&cfg.headers)?,
                cfg.result_format,
            ),
        })
    }

    fn list_tools(&self, idx: usize) -> Vec<ToolDef> {
        self.spec
            .operations
            .iter()
            .map(|op| ToolDef {
                upstream_idx: idx,
                id: format!("{}.{}", self.name, op.op_id),
                name: op.op_id.clone(),
                description: op.one_line(),
                // Cheap unresolved schema at load; resolved on demand in
                // get_tool_details (Backend::resolved_schema) so huge specs load fast.
                input_schema: self.spec.tool_input_schema_shallow(op),
                // GET operations are read-only by HTTP semantics → cacheable.
                read_only: Some(op.method.eq_ignore_ascii_case("get")),
            })
            .collect()
    }

    async fn call_tool(
        &mut self,
        op_id: &str,
        args: Value,
        extra_headers: &[(String, String)],
        deadline: Option<std::time::Duration>,
    ) -> Result<Value> {
        let Some(op) = self.spec.get(op_id) else {
            return Ok(err_result(format!("unknown operation {op_id:?}")));
        };
        let pp = args.get("path_params").cloned().unwrap_or(json!({}));
        let qp = args.get("query_params").cloned().unwrap_or(json!({}));
        let body = args.get("body").cloned().unwrap_or(json!({}));
        let out = self.executor.execute(&self.spec, op, &pp, &qp, &body, extra_headers, deadline).await?;
        // shape into an MCP tool result; mark isError on HTTP >= 400 or transport error
        let is_error = out.get("error").is_some()
            || out.get("status").and_then(Value::as_u64).map(|s| s >= 400).unwrap_or(false);
        Ok(json!({
            "content": [{"type": "text", "text": serde_json::to_string(&out).unwrap_or_default()}],
            "isError": is_error,
        }))
    }
}

fn err_result(msg: String) -> Value {
    json!({"content": [{"type": "text", "text": msg}], "isError": true})
}

/// Re-encode each JSON text block of an MCP tool result as compact JSON — the
/// free token win `toon.md` measured (~24% on a spacey `json.dumps` payload),
/// previously applied only on the spec path. Non-JSON text is left untouched.
///
/// The compaction is LEXICAL (strip insignificant whitespace outside strings),
/// not a parse→serialize round-trip: number literals, key order, and escapes
/// pass through byte-for-byte, so a 128-bit integer or 20-digit decimal can
/// never be silently rewritten through f64. Validity is still checked by a
/// real parse first (into `IgnoredAny` — no tree built).
fn compact_json_texts(result: &mut Value) {
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str) else { continue };
        if serde_json::from_str::<serde::de::IgnoredAny>(text).is_err() {
            continue; // not JSON — leave as-is
        }
        let compact = strip_json_whitespace(text);
        // Only replace when it actually shrinks.
        if compact.len() < text.len() {
            if let Some(obj) = block.as_object_mut() {
                obj.insert("text".into(), Value::String(compact));
            }
        }
    }
}

/// Remove whitespace outside string literals from KNOWN-VALID JSON text.
/// Purely lexical: every non-whitespace byte (including all number literals and
/// escape sequences) is copied verbatim.
fn strip_json_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_ascii_whitespace() {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_json_texts_shrinks_spacey_json_and_leaves_prose() {
        let mut r = json!({"content": [
            {"type": "text", "text": "{ \"a\": [ 1, 2 ],  \"b\": \"x\" }"},
            {"type": "text", "text": "plain prose, untouched"}
        ], "isError": false});
        compact_json_texts(&mut r);
        assert_eq!(r["content"][0]["text"], json!("{\"a\":[1,2],\"b\":\"x\"}"));
        assert_eq!(r["content"][1]["text"], json!("plain prose, untouched"));
    }

    #[test]
    fn compaction_is_lexical_and_never_rewrites_numbers() {
        // 128-bit integer and >17-significant-digit decimal: a Value round-trip
        // would push both through f64 and corrupt them; lexical compaction must
        // copy the literals byte-for-byte.
        let big = "{ \"id\": 340282366920938463463374607431768211455, \"rate\": 0.12345678901234567890123 }";
        let mut r = json!({"content": [{"type": "text", "text": big}], "isError": false});
        compact_json_texts(&mut r);
        assert_eq!(
            r["content"][0]["text"],
            json!("{\"id\":340282366920938463463374607431768211455,\"rate\":0.12345678901234567890123}")
        );
        // whitespace INSIDE strings (and escapes) survive untouched
        let s = "{ \"note\": \"two  spaces \\\" and \\\\ stay\" }";
        let mut r = json!({"content": [{"type": "text", "text": s}], "isError": false});
        compact_json_texts(&mut r);
        assert_eq!(r["content"][0]["text"], json!("{\"note\":\"two  spaces \\\" and \\\\ stay\"}"));
    }
}
