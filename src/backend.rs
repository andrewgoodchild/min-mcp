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
use crate::upstream::{ToolDef, Upstream};

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
    ) -> Result<Value> {
        match self {
            Backend::Mcp(u) => u.call_tool(name, args).await,
            Backend::Http(h) => h.call_tool(name, args).await,
            Backend::Spec(s) => s.call_tool(name, args, extra_headers).await,
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
            })
            .collect()
    }

    async fn call_tool(
        &mut self,
        op_id: &str,
        args: Value,
        extra_headers: &[(String, String)],
    ) -> Result<Value> {
        let Some(op) = self.spec.get(op_id) else {
            return Ok(err_result(format!("unknown operation {op_id:?}")));
        };
        let pp = args.get("path_params").cloned().unwrap_or(json!({}));
        let qp = args.get("query_params").cloned().unwrap_or(json!({}));
        let body = args.get("body").cloned().unwrap_or(json!({}));
        let out = self.executor.execute(&self.spec, op, &pp, &qp, &body, extra_headers).await?;
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
