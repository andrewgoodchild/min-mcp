//! The minified surface: what the agent sees, in one of three modes, after
//! scope filtering and overlay application. Design laws apply here:
//! details stay a separate call (law 4/5), errors carry recovery hints
//! (law 6), usage priors are damped (law 6 of the index).

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::backend::{Backend, SpecBackend};
use crate::config::{Config, Mode};
use crate::http_upstream::HttpUpstream;
use crate::index::Index;
use crate::upstream::{ToolDef, Upstream};

const MAX_DETAIL_CHARS: usize = 20_000;
const DEFAULT_SEARCH_K: usize = 10;
const MAX_NAME_LEN: usize = 64;

mod audit;
mod breaker;
mod cache;
mod dispatch;
mod generators;
mod ids;
mod minify;
mod patch;
mod preflight;
mod resources;
mod results;
mod workflow;
#[cfg(test)]
mod tests;

use cache::READ_CACHE_MAX_ENTRIES;
use generators::{fnv1a, resolve_generators, tool_fingerprint};
use ids::sanitize_name;
use minify::{budget_truncate, minify_schema, minify_schema_hard, prune_below_depth, truncate_in_place};
use patch::{apply_field_patches, binding_status};
use preflight::{preflight_error, resolve_user_source, structured_field_error_at};
use results::{apply_response_transform, bad_arg, eval_expect, get_path, json_len, nudge_projection, result_payload, result_text, text_result, transform_result, truncate_result_text, AGENT_RESULT_BUDGET};

pub struct Surface {
    config: Config,
    granted: Vec<String>,
    upstreams: Vec<Backend>,
    tools: Vec<ToolDef>,
    /// tool_id -> index into `tools` (single source of truth for existence)
    by_id: HashMap<String, usize>,
    /// exposed MCP tool name -> tool_id (promoted/passthrough tools).
    /// BTreeMap so list order is deterministic without a per-call sort.
    exposed: BTreeMap<String, String>,
    index: Index,
    /// workflow id -> index into `config.workflows` (composite tools).
    workflow_by_id: HashMap<String, usize>,
    /// Append-only NDJSON event log (search/details/call), if configured.
    log: Option<std::fs::File>,
    /// tool_id -> fingerprint of the RAW upstream tool (description + input
    /// schema), captured BEFORE overlay patching. `authored_sha` pins this, so a
    /// rug-pull that changes only the top-level description is caught as drift —
    /// not just a schema-shape change.
    origin_sha: HashMap<String, String>,
    /// tool_id -> fully-resolved input schema WITH the overlay's field patches
    /// applied. Only tools that carry a schema-changing overlay are here (so a
    /// huge un-overlaid spec still resolves lazily); `resolved_schema` prefers it.
    patched_schemas: HashMap<String, Value>,
    /// tool_id -> per-operation request headers (overlay `headers:`), with `${ENV}`
    /// already expanded but per-request generators (`{{uuid}}`, `{{now}}`) still as
    /// tokens — resolved fresh on every call in `dispatch`.
    tool_headers: HashMap<String, Vec<(String, String)>>,
    /// tool_id -> [(field_path, source)] for `user_supplied` fields: stripped from
    /// the agent schema and injected from `source` (`env:VAR`) at call time.
    user_supplied: HashMap<String, Vec<(String, String)>>,
    /// (tool_id, canonical args JSON) -> (inserted-at, raw pre-shaping result),
    /// for read-only tools when `read_cache_ttl_s` > 0. Response shaping and
    /// projection re-run per call on a clone, so a cache hit honors THIS call's
    /// `fields`. Bounded by `READ_CACHE_MAX_ENTRIES`.
    read_cache: HashMap<(String, String), (std::time::Instant, Value)>,
    /// resource URI -> owning upstream index, learned from the last
    /// `resources/list` merge (refreshed on a read miss).
    resource_origin: HashMap<String, usize>,
    /// tool_id -> circuit-breaker state, for tools whose overlay sets `breaker:`.
    breakers: HashMap<String, breaker::BreakerState>,
    /// tool_id -> lazily resolved schema (immutable after load). Default-on
    /// preflight resolves on every call; without this a spec tool would re-expand
    /// its `$ref`s each time. RefCell: fills under `&self` (Surface sits behind
    /// the transport's Mutex, so borrows never overlap).
    resolved_cache: std::cell::RefCell<HashMap<String, Value>>,
}


impl Surface {
    pub async fn build(config: Config, granted: Vec<String>) -> Result<Self> {
        let mut upstreams = Vec::new();
        let mut tools: Vec<ToolDef> = Vec::new();
        let mut by_id = HashMap::new();
        let mut origin_sha: HashMap<String, String> = HashMap::new();
        let mut patched: HashMap<String, Value> = HashMap::new();
        let mut tool_headers: HashMap<String, Vec<(String, String)>> = HashMap::new();
        let mut user_supplied: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for ucfg in &config.upstreams {
            // A fully filtered-out upstream is never spawned/loaded — so an
            // excluded API needs no command and no credentials.
            if !config.upstream_enabled(&ucfg.name) {
                continue;
            }
            // Index into the surviving `upstreams` list (NOT the config index —
            // skipped upstreams would misalign it); stamped onto each ToolDef so
            // dispatch routes back to the right backend.
            let backend_idx = upstreams.len();
            let mut up = if ucfg.is_spec() {
                Backend::Spec(SpecBackend::new(ucfg)?)
            } else if ucfg.is_http() {
                Backend::Http(HttpUpstream::connect(ucfg).await?)
            } else {
                Backend::Mcp(Upstream::spawn(ucfg).await?)
            };
            for mut t in up.list_tools(backend_idx).await? {
                // static include/exclude: a filtered tool never enters the
                // surface — not listed, searchable, or callable, for any caller.
                if !config.passes_filter(&ucfg.name, t.id()) {
                    continue;
                }
                // Fingerprint the RAW upstream tool (description + schema) BEFORE
                // any overlay patches it — this is what drift/rug-pull detection
                // compares against.
                origin_sha.insert(t.id().to_string(), tool_fingerprint(&t.description, &t.input_schema));
                // overlays patch the tool at load time (law 1: data, not code)
                if let Some(o) = config.overlay_for(t.id()) {
                    if let Some(d) = &o.description {
                        t.description = d.clone();
                    }
                    // Field patches need each field's real location, which for a
                    // spec tool sits behind a `$ref` in the shallow schema — so
                    // resolve THIS tool (only overlaid tools pay the cost) and patch
                    // the resolved schema, stored for `resolved_schema` to serve.
                    if !o.fields.is_empty() {
                        let mut schema =
                            up.resolved_schema(&t.name).unwrap_or_else(|| t.input_schema.clone());
                        apply_field_patches(&mut schema, &o.fields);
                        patched.insert(t.id().to_string(), schema);
                        // Record user-supplied fields for call-time injection (they
                        // were just stripped from the agent schema above).
                        let us: Vec<(String, String)> = o
                            .fields
                            .iter()
                            .filter_map(|(path, patch)| {
                                patch.spec().user_supplied.clone().map(|s| (path.clone(), s))
                            })
                            .collect();
                        if !us.is_empty() {
                            user_supplied.insert(t.id().to_string(), us);
                        }
                    }
                    // Per-tool headers: expand ${ENV} now (fail loud, keeps secrets
                    // out of config); leave {{...}} generators for per-call resolution.
                    if !o.headers.is_empty() {
                        let hs = o
                            .headers
                            .iter()
                            .map(|(k, v)| Ok((k.clone(), crate::config::expand_env(v)?)))
                            .collect::<Result<Vec<_>>>()?;
                        tool_headers.insert(t.id().to_string(), hs);
                    }
                }
                by_id.insert(t.id().to_string(), tools.len());
                tools.push(t);
            }
            upstreams.push(up);
        }
        // Index upstream tools AND composite workflows (both searchable/callable).
        // Overlay `aliases` are appended to a tool's indexed text so a poorly-named
        // tool is findable by an outcome phrase, without changing its id.
        let mut corpus: Vec<(String, String)> = tools
            .iter()
            .map(|t| {
                let aliases =
                    config.overlay_for(t.id()).map(|o| o.aliases.join(" ")).unwrap_or_default();
                let text = if aliases.is_empty() {
                    t.description.clone()
                } else {
                    format!("{} {}", t.description, aliases)
                };
                (t.id().to_string(), text)
            })
            .collect();
        for wf in &config.workflows {
            corpus.push((wf.id.clone(), wf.description.clone()));
        }
        let index = Index::build(&corpus);
        let workflow_by_id: HashMap<String, usize> =
            config.workflows.iter().enumerate().map(|(i, w)| (w.id.clone(), i)).collect();

        let log = match &config.log_file {
            Some(path) => Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("opening log_file {path}"))?,
            ),
            None => None,
        };
        let mut surface = Surface {
            config,
            granted,
            upstreams,
            tools,
            by_id,
            exposed: BTreeMap::new(),
            index,
            workflow_by_id,
            log,
            origin_sha,
            patched_schemas: patched,
            tool_headers,
            user_supplied,
            read_cache: HashMap::new(),
            resource_origin: HashMap::new(),
            breakers: HashMap::new(),
            resolved_cache: std::cell::RefCell::new(HashMap::new()),
        };
        surface.build_exposed();
        // Binding registry: check overlays against the live upstream schema. A
        // broken STRONG binding refuses to start (fail closed); a broken WEAK
        // one is logged and its broken parts are skipped (fail open).
        use crate::config::BindingStrength;
        let broken = surface.broken_bindings();
        let fmt = |set: &[&(String, Vec<String>, BindingStrength)]| {
            set.iter().map(|(t, why, _)| format!("  {t}: {}", why.join("; "))).collect::<Vec<_>>().join("\n")
        };
        let strong: Vec<_> = broken.iter().filter(|b| b.2 == BindingStrength::Strong).collect();
        let weak: Vec<_> = broken.iter().filter(|b| b.2 == BindingStrength::Weak).collect();
        if !weak.is_empty() {
            crate::log_warn!("{} weak overlay binding(s) broken (skipped):\n{}", weak.len(), fmt(&weak));
        }
        if !strong.is_empty() {
            anyhow::bail!("{} strong overlay binding(s) broken (fail-closed):\n{}", strong.len(), fmt(&strong));
        }
        Ok(surface)
    }

    /// Append one NDJSON observability event (best-effort; logging never fails a
    /// request). `event` is the kind (search/details/call); `fields` are merged in.
    fn log_event(&mut self, event: &str, fields: Value) {
        let Some(file) = self.log.as_mut() else { return };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut line = json!({"ts_ms": ts, "event": event});
        if let (Some(obj), Some(extra)) = (line.as_object_mut(), fields.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    }

    fn allowed(&self, tool_id: &str) -> bool {
        self.config.allowed(tool_id, &self.granted)
    }

    fn visible_ids(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| t.id().to_string())
            .filter(|id| self.allowed(id))
            .collect()
    }

    fn build_exposed(&mut self) {
        let ids: Vec<String> = match self.config.mode {
            // passthrough declares every visible tool by name; three_tool
            // declares none (the agent reaches them via the meta-tools).
            Mode::Passthrough => self.visible_ids(),
            Mode::ThreeTool => vec![],
        };
        for id in ids {
            let mut name = sanitize_name(&id, 0);
            if self.exposed.contains_key(&name) {
                // Reserve room for a "_N" suffix up front so the disambiguated
                // name is strictly shorter than the 64-char cap — otherwise a
                // re-truncated name could reproduce the colliding string and
                // loop forever.
                let base = sanitize_name(&id, 4);
                let mut n = 2;
                loop {
                    name = format!("{base}_{n}");
                    if !self.exposed.contains_key(&name) {
                        break;
                    }
                    n += 1;
                }
            }
            self.exposed.insert(name, id);
        }
    }

    fn meta_search(&self) -> Value {
        json!({
            "name": "search_tools",
            "description": "Search every available tool by task description (e.g. \"create a customer\"). Returns matching tool ids with one-line summaries. Call get_tool_details next for the exact input schema.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "What you are trying to do."},
                "k": {"type": "integer", "description": "Max results (default 10)."}
            }, "required": ["query"]}
        })
    }

    fn meta_details(&self) -> Value {
        json!({
            "name": "get_tool_details",
            "description": "Get the full description and input schema for one tool id.",
            "inputSchema": {"type": "object", "properties": {
                "tool_id": {"type": "string", "description": "The tool id to fetch, as returned by search_tools (e.g. \"stripe.PostCustomers\")."}
            }, "required": ["tool_id"]}
        })
    }

    fn meta_call(&self) -> Value {
        json!({
            "name": "call_tool",
            "description": "Call any tool by id with its arguments. If a required value is unknown, do not invent it — search for a tool that can look it up, or report what is missing. When a tool returns a large list or object, pass `fields` to get back only the parts you need and keep your context small.",
            "inputSchema": {"type": "object", "properties": {
                "tool_id": {"type": "string", "description": "The tool id to call, from search_tools / get_tool_details (e.g. \"stripe.PostCustomers\")."},
                "arguments": {"type": "object", "description": "Arguments per the tool's input schema."},
                "fields": {
                    "type": "array", "items": {"type": "string"},
                    "description": "Optional. Return ONLY these response fields, to shrink a large result. Dotted paths; `[]` maps over an array element — e.g. [\"data[].id\", \"data[].amount\", \"has_more\"]. Omit to get the full response. Note: you only receive the fields you list, so include everything you need."
                }
            }, "required": ["tool_id"]}
        })
    }

    /// Meta-tools advertised for the current mode.
    fn meta_tool_defs(&self) -> Vec<Value> {
        match self.config.mode {
            Mode::ThreeTool => vec![self.meta_search(), self.meta_details(), self.meta_call()],
            Mode::Passthrough => vec![],
        }
    }

    fn def_for(&self, tool_id: &str) -> Option<&ToolDef> {
        self.by_id.get(tool_id).map(|&i| &self.tools[i])
    }

    /// The tool def, only if the current caller is allowed to see it.
    fn visible_def(&self, tool_id: &str) -> Option<&ToolDef> {
        self.def_for(tool_id).filter(|_| self.allowed(tool_id))
    }

    /// The full input schema to surface for a tool — resolved on demand for spec
    /// upstreams (which store a cheap unresolved schema at load so huge specs load
    /// instantly), else the schema already stored on the ToolDef.
    fn resolved_schema(&self, t: &ToolDef) -> Value {
        // An overlaid tool's patched schema wins (it was resolved + patched at
        // load); otherwise resolve lazily, once, and memoize (schemas are
        // immutable after load; default-on preflight hits this per call).
        if let Some(p) = self.patched_schemas.get(t.id()) {
            return p.clone();
        }
        if let Some(c) = self.resolved_cache.borrow().get(t.id()) {
            return c.clone();
        }
        let v = self.raw_resolved(t);
        self.resolved_cache.borrow_mut().insert(t.id().to_string(), v.clone());
        v
    }

    /// The resolved schema WITHOUT overlay patches — the upstream's own shape.
    /// Used by binding-integrity checks (does the field an overlay targets still
    /// exist upstream?), which must see through the patch.
    fn raw_resolved(&self, t: &ToolDef) -> Value {
        self.upstreams
            .get(t.upstream_idx)
            .and_then(|b| b.resolved_schema(&t.name))
            .unwrap_or_else(|| t.input_schema.clone())
    }

    /// The tools/list result the agent sees.
    pub fn list_tools(&self) -> Value {
        let mut defs: Vec<Value> = Vec::new();
        for (name, id) in &self.exposed {
            if let Some(t) = self.def_for(id) {
                let mut desc = t.description.clone();
                truncate_in_place(&mut desc, 1_000);
                defs.push(json!({
                    "name": name,
                    "description": desc,
                    "inputSchema": self.resolved_schema(t), // resolve $refs on demand
                }));
            }
        }
        defs.extend(self.meta_tool_defs());
        json!({"tools": defs})
    }

    /// Guidance appended to unknown-tool errors — never names a tool the
    /// current mode doesn't expose.
    fn recovery(&self) -> &'static str {
        match self.config.mode {
            // passthrough lists every tool by name; search_tools isn't advertised
            Mode::Passthrough => "check the tool name against tools/list",
            Mode::ThreeTool => "use search_tools to find the right tool id",
        }
    }

    // --- CLI entry points: the three operations, reachable from the shell
    // regardless of surface mode (they bypass the meta-tool advertisement that
    // `call` gates on, so `minmcp search/help/call` work even in passthrough).

    /// Search tools by task description (CLI `minmcp search`).
    pub fn cli_search(&self, query: &str, k: usize) -> String {
        self.search_text(query, k)
    }

    /// Full schema for one tool (CLI `minmcp help`).
    pub fn cli_details(&self, tool_id: &str) -> String {
        self.details_text(tool_id)
    }

    /// Invoke a tool or composite by id with optional projection (`minmcp call`).
    pub async fn cli_call(&mut self, tool_id: &str, arguments: Value, fields: &[String]) -> Result<Value> {
        self.route_call(tool_id, arguments, fields).await
    }

    /// Client-side errors (bad args, unknown tool) are returned as isError
    /// tool results with a `bad_arg` marker — NOT as Err, so the transport
    /// loop never mislabels them "the upstream may be down".
    pub async fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        let has_meta = self.config.mode != Mode::Passthrough;
        match name {
            "search_tools" if has_meta => {
                let Some(query) = args.get("query").and_then(Value::as_str) else {
                    return Ok(bad_arg("search_tools requires a 'query' string"));
                };
                let k = args
                    .get("k")
                    .and_then(Value::as_u64)
                    .and_then(|k| usize::try_from(k).ok())
                    .unwrap_or(DEFAULT_SEARCH_K);
                let text = self.search_text(query, k);
                self.log_event("search", json!({"query": query, "k": k}));
                Ok(text_result(text, false))
            }
            "get_tool_details" if has_meta => {
                let Some(id) = args.get("tool_id").and_then(Value::as_str) else {
                    return Ok(bad_arg("get_tool_details requires a 'tool_id' string"));
                };
                let text = self.details_text(id);
                self.log_event("details", json!({"tool_id": id}));
                Ok(text_result(text, false))
            }
            "call_tool" if has_meta => {
                let Some(id) = args.get("tool_id").and_then(Value::as_str) else {
                    return Ok(bad_arg("call_tool requires a 'tool_id' string"));
                };
                let id = id.to_string();
                // An explicit null is treated like omitted (clients send it for
                // no-arg tools); any OTHER non-object would hand the upstream a
                // protocol-invalid call — reject it client-side.
                let inner = args
                    .get("arguments")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or(json!({}));
                if !inner.is_object() {
                    return Ok(bad_arg("call_tool 'arguments' must be a JSON object"));
                }
                // optional GraphQL-style field projection to shrink the response
                let fields: Vec<String> = args
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                self.route_call(&id, inner, &fields).await
            }
            _ => {
                // promoted / passthrough tool name (no meta wrapper -> no projection)
                let Some(id) = self.exposed.get(name).cloned() else {
                    let hint = ids::did_you_mean(name, self.exposed.keys().map(String::as_str));
                    return Ok(text_result(
                        format!("unknown tool {name:?} —{hint} {}", self.recovery()),
                        true,
                    ));
                };
                self.dispatch(&id, args, &[]).await
            }
        }
    }

    fn search_text(&self, query: &str, k: usize) -> String {
        // over-fetch so scope filtering can't starve the result list
        // (saturating: k is client-controlled, must not overflow)
        let hits = self.index.search(query, k.saturating_mul(3));
        let mut lines: Vec<String> = Vec::new();
        for (id, _) in hits {
            if !self.allowed(&id) {
                continue; // invisible, not forbidden
            }
            // upstream tool or composite workflow (both indexed)
            if let Some(desc) = self.describe(&id) {
                let mut summary = desc.lines().next().unwrap_or("").to_string();
                truncate_in_place(&mut summary, 110);
                lines.push(format!("{id} — {summary}"));
            }
            if lines.len() >= k {
                break;
            }
        }
        if lines.is_empty() {
            return "no matches — try different words (resource or action names work best)".into();
        }
        lines.join("\n")
    }

    fn details_text(&self, tool_id: &str) -> String {
        // composite workflow: describe its declared inputs
        if let Some(&i) = self.workflow_by_id.get(tool_id) {
            let wf = &self.config.workflows[i];
            let schema = if wf.inputs.is_null() { json!({"type": "object"}) } else { wf.inputs.clone() };
            return serde_json::to_string_pretty(&json!({
                "tool_id": wf.id,
                "description": wf.description,
                "input_schema": schema,
                "composite": true,
            }))
            .unwrap_or_default();
        }
        let Some(t) = self.visible_def(tool_id) else {
            return format!(
                "unknown tool_id {tool_id:?} —{} {}",
                self.suggest_near_misses(tool_id),
                self.recovery()
            );
        };
        let schema = self.resolved_schema(t); // resolve $refs on demand
        // One envelope renderer for every stage; only the note and pretty/compact
        // choice vary (indentation is pure token overhead once over budget).
        let render = |schema: &Value, note: Option<&str>, pretty: bool| {
            let mut v = json!({"tool_id": tool_id, "description": t.description});
            if let Some(n) = note {
                v["schema_minified"] = json!(n);
            }
            v["input_schema"] = schema.clone();
            if pretty { serde_json::to_string_pretty(&v) } else { serde_json::to_string(&v) }
                .unwrap_or_default()
        };
        let full = render(&schema, None, true);
        if full.len() <= MAX_DETAIL_CHARS {
            return full;
        }
        // Over budget: rather than blind-truncating (which HIDES trailing fields
        // — the `success_url` failure class), degrade in stages that always keep
        // every field NAME: prose-minify → hard-minify (compact) → depth-prune.
        let mut minified = schema;
        minify_schema(&mut minified);
        let out = render(
            &minified,
            Some("true — examples/long prose dropped and huge enums capped to fit the display budget; EVERY field name and type is listed."),
            true,
        );
        if out.len() <= MAX_DETAIL_CHARS {
            return out;
        }
        minify_schema_hard(&mut minified);
        let out = render(
            &minified,
            Some("hard — all field docs and enums elided to fit the display budget; EVERY field name, type, and required flag is listed. Use the field names as-is; consult the API's docs for value formats."),
            false,
        );
        if out.len() <= MAX_DETAIL_CHARS {
            return out;
        }
        // Elide the DEEPEST nesting first, pruning ONE working copy in place
        // (pruning at depth d then d-1 equals pruning at d-1 directly — the
        // elision counts are per-node property counts, untouched by deeper
        // prunes) — shallow, task-critical fields stay complete at every step.
        const DEPTH_NOTE: &str = "hard+depth — nested objects beyond a depth were elided (see nested_fields_elided counts); every shallower field name is listed.";
        for depth in (2..=5).rev() {
            prune_below_depth(&mut minified, depth);
            let out = render(&minified, Some(DEPTH_NOTE), false);
            if out.len() <= MAX_DETAIL_CHARS {
                return out;
            }
        }
        prune_below_depth(&mut minified, 1);
        // Truly pathological width: truncate the depth-1 render — the smallest
        // structured form, under a note that never overclaims completeness.
        budget_truncate(
            render(&minified, Some(DEPTH_NOTE), false),
            MAX_DETAIL_CHARS,
            "\n…TRUNCATED — even the depth-pruned schema exceeds the budget; unlisted fields still exist.",
        )
    }

    /// ` did you mean "x" or "y"?` for a mistyped id, from the closest known
    /// tool/workflow ids (we hold the whole catalog — a dot-for-underscore slip
    /// should never dead-end). Empty string when nothing is plausibly close.
    fn suggest_near_misses(&self, wrong: &str) -> String {
        let candidates = self
            .by_id
            .keys()
            .filter(|id| self.allowed(id))
            .chain(self.workflow_by_id.keys())
            .map(String::as_str);
        ids::did_you_mean(wrong, candidates)
    }


    /// Route a call to a composite workflow if the id names one, else a tool.
    async fn route_call(&mut self, id: &str, args: Value, fields: &[String]) -> Result<Value> {
        if let Some(&i) = self.workflow_by_id.get(id) {
            let wf = self.config.workflows[i].clone(); // small; frees the borrow for &mut dispatch
            return self.execute_workflow(&wf, args).await;
        }
        self.dispatch(id, args, fields).await
    }


    /// One-line description of any callable id — upstream tool or workflow.
    fn describe(&self, id: &str) -> Option<String> {
        if let Some(t) = self.def_for(id) {
            return Some(t.description.lines().next().unwrap_or("").to_string());
        }
        self.workflow_by_id.get(id).map(|&i| self.config.workflows[i].description.clone())
    }


}


