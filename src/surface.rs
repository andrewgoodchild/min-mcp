//! The minified surface: what the agent sees, in one of the modes, after
//! scope filtering and overlay application. Design laws apply here:
//! details stay a separate call (law 4/5), errors carry recovery hints
//! (law 6), usage priors are damped (law 6 of the index).
//!
//! Shared, not locked: the catalog, index, schemas, and backends are built
//! once and read by every caller concurrently. The few things that change per
//! call — usage prior, read cache, breakers, rate-limit buckets, the audit
//! sink — sit behind their own small locks, never held across an await. So a
//! slow upstream call blocks only its own caller, and the identity that
//! filters the surface is a per-request `Caller`, not a field on the surface.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::backend::{Backend, SpecBackend};
use crate::caller::Caller;
use crate::config::{Config, Mode};
use crate::http_upstream::HttpUpstream;
use crate::index::Index;
use crate::secrets::Secrets;
use crate::sync::{lock, read, write};
use crate::upstream::{ToolDef, Upstream};

const MAX_DETAIL_CHARS: usize = 20_000;
const DEFAULT_SEARCH_K: usize = 20;
const MAX_NAME_LEN: usize = 64;
/// Largest resolved schema worth keeping: a runaway one costs far more to hold than
/// to resolve again.
const MAX_CACHED_SCHEMA_BYTES: usize = 64_000;

/// Annotations for a meta-tool that only reads min-mcp's own state. Just the
/// hint hosts approve on: these definitions ride every model request, so each
/// extra field is a cost paid on every turn.
fn read_only_local() -> Value {
    json!({"readOnlyHint": true})
}

mod audit;
mod breaker;
mod cache;
mod catalog;
mod dispatch;
mod generators;
mod ids;
mod minify;
mod patch;
mod preflight;
mod ratelimit;
mod resources;
pub use resources::UnknownResource;
mod results;
mod shadow;
mod workflow;
#[cfg(test)]
mod tests;

pub(crate) use cache::canonical_args;
use catalog::Catalog;

/// A call's `arguments` as dispatched: an explicit null is treated like
/// omitted (clients send it for no-arg tools); any OTHER non-object would hand
/// the upstream a protocol-invalid call, so is `None`. `minmcp audit-args`
/// normalises with this too, so its mac matches the one logged.
pub(crate) fn call_arguments(raw: Option<&Value>) -> Option<Value> {
    let v = raw.filter(|v| !v.is_null()).cloned().unwrap_or(json!({}));
    v.is_object().then_some(v)
}
use cache::READ_CACHE_MAX_ENTRIES;
use generators::{fnv1a, resolve_generators, tool_fingerprint};
use ids::sanitize_name;
use minify::{budget_truncate, minify_schema, minify_schema_hard, prune_below_depth, truncate_in_place};
use patch::{apply_field_patches, binding_status};
use preflight::{decode_stringified, preflight_error, structured_field_error_at};
use results::{apply_response_transform, bad_arg, eval_expect, get_path, json_len, nudge_projection, result_payload, result_text, text_result, transform_result, truncate_passthrough_text, truncate_result_text, AGENT_RESULT_BUDGET};

/// Where audit events (NDJSON, one per line) go.
pub(crate) enum AuditSink {
    File(Mutex<std::fs::File>),
    /// `log_file: stderr` — for containers, where the platform's log shipper
    /// (Fluent Bit, the Datadog agent, …) forwards stderr to the SIEM. stdout
    /// is never an option: on stdio it IS the MCP protocol.
    Stderr,
}

pub struct Surface {
    config: Config,
    secrets: Secrets,
    upstreams: Vec<Backend>,
    /// Everything derived from the upstreams' listings, swapped whole when an
    /// upstream's tools change. See `surface/catalog.rs`.
    catalog: RwLock<Arc<Catalog>>,
    /// Held while re-indexing, so one request does it and the rest keep serving.
    refreshing: tokio::sync::Mutex<()>,
    /// When each upstream is next due a re-list, from its listing's `ttlMs`;
    /// `None` = only when it announces a change.
    recheck_at: Mutex<Vec<Option<Instant>>>,
    /// workflow id -> index into `config.workflows` (composite tools).
    workflow_by_id: HashMap<String, usize>,
    /// Append-only NDJSON audit events (search/details/call/…), if configured.
    log: Option<AuditSink>,
    /// Tamper evidence for the audit stream (`log_hmac_key`). None = off.
    log_chain: Option<Mutex<crate::audit_chain::Chain>>,
    /// Keys each `call` line's `args_mac`; set exactly when `log_chain` is.
    args_key: Option<crate::audit_chain::ArgsKey>,
    /// (tool_id, canonical args JSON) -> (inserted-at, raw pre-shaping result),
    /// for read-only tools when `read_cache_ttl_s` > 0. Response shaping and
    /// projection re-run per call on a clone, so a cache hit honors THIS call's
    /// `fields`. Bounded by `READ_CACHE_MAX_ENTRIES`.
    read_cache: Mutex<HashMap<(String, String), (std::time::Instant, Value)>>,
    /// resource URI -> owning upstream index, learned from `resources/list`
    /// merges (refreshed on a read miss).
    resource_origin: Mutex<HashMap<String, usize>>,
    /// tool_id -> circuit-breaker state, for tools whose overlay sets `breaker:`.
    breakers: Mutex<HashMap<String, breaker::BreakerState>>,
    /// Token buckets for `rate_limits` and overlay `rate_limit`.
    limits: ratelimit::Buckets,
    /// Slots for tool calls in flight to upstreams (`max_concurrent_calls`).
    /// `None` when disabled. Held ONLY across the upstream call itself, never
    /// across cache, breaker or shaping work — and never across a nested call,
    /// or a pagination follow-up would wait on a slot its own parent holds.
    upstream_slots: Option<tokio::sync::Semaphore>,
    /// Alternative retrievers scored against real traffic, serving nothing. Empty
    /// (and free) unless `shadow: true`. See `surface/shadow.rs`. Built from the
    /// startup catalog; a re-index does not rebuild it.
    shadow: shadow::Shadow,
}

/// What to do about a tool that trips a poisoning rule. Kept out of the
/// `bail!` so the prose can be read and edited as prose.
const POISONING_HINT: &str = "Inspect them with `minmcp lint`, then either exclude them (`filters`), \
replace the description with an overlay, or set `poisoning_policy: warn` if they are false positives.";

impl Surface {
    pub async fn build(config: Config, secrets: Secrets) -> Result<Self> {
        let mut upstreams = Vec::new();
        let mut listings: Vec<Vec<ToolDef>> = Vec::new();
        let mut ttls: Vec<Option<u64>> = Vec::new();
        let mut skipped = 0usize;
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
            let connected: Result<(Backend, Vec<ToolDef>, Option<u64>)> = async {
                let up = if ucfg.is_spec() {
                    Backend::Spec(SpecBackend::new(ucfg, &secrets).await?)
                } else if ucfg.is_http() {
                    Backend::Http(HttpUpstream::connect(ucfg, &secrets).await?)
                } else {
                    Backend::Mcp(Upstream::spawn(ucfg).await?)
                };
                let (listed, ttl) = up.list_tools_ttl(backend_idx).await?;
                Ok((up, listed, ttl))
            }
            .await;
            let (up, listed, ttl) = match connected {
                Ok(v) => v,
                // `optional: true` — degrade instead of refusing to start: the
                // rest of the surface serves, this upstream's tools are absent,
                // and the warning says why. Nothing was pushed, so indices of
                // the upstreams that DID connect stay aligned.
                Err(e) if ucfg.optional => {
                    crate::log_warn!(
                        "optional upstream {:?} unavailable and skipped: {e:#}",
                        ucfg.name
                    );
                    skipped += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            upstreams.push(up);
            listings.push(listed);
            ttls.push(ttl);
        }
        let catalog = Catalog::assemble(&config, &secrets, &upstreams, listings).await?;
        let shadow = shadow::Shadow::new(&catalog::corpus(&config, &catalog.tools), config.shadow);
        let workflow_by_id: HashMap<String, usize> =
            config.workflows.iter().enumerate().map(|(i, w)| (w.id.clone(), i)).collect();

        let log = match config.log_file.as_deref() {
            None => None,
            Some("stderr") => Some(AuditSink::Stderr),
            Some(path) => Some(AuditSink::File(Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("opening log_file {path}"))?,
            ))),
        };
        // Tamper evidence, when a key is configured. Resumed from the file so
        // a restart continues one chain instead of starting a second at seq 1
        // mid-file, which verification would read as tampering.
        let (log_chain, args_key) = match (&config.log_hmac_key, &log) {
            (Some(key_ref), Some(_)) => {
                let key = secrets.expand(key_ref).await.context("resolving log_hmac_key")?;
                let mut chain = crate::audit_chain::Chain::new(&key);
                if let Some(path) = config.log_file.as_deref().filter(|p| *p != "stderr") {
                    chain.resume_from(path)?;
                }
                (Some(Mutex::new(chain)), Some(crate::audit_chain::ArgsKey::new(&key)))
            }
            (Some(_), None) => {
                anyhow::bail!("log_hmac_key is set but log_file is not — there is no audit stream to sign")
            }
            _ => (None, None),
        };
        // Read before `config` is moved into the surface.
        let config_max_concurrent = config.max_concurrent_calls;
        // Degrading is the point of `optional`, but a proxy with nothing behind
        // it looks healthy to every client while serving no tools.
        if skipped > 0 && catalog.tools.is_empty() {
            crate::log_error!("every upstream failed and was skipped as optional: serving an EMPTY tool catalog");
        }
        let n_upstreams = upstreams.len();
        let surface = Surface {
            shadow,
            config,
            secrets,
            upstreams,
            catalog: RwLock::new(Arc::new(catalog)),
            refreshing: tokio::sync::Mutex::new(()),
            recheck_at: Mutex::new(vec![None; n_upstreams]),
            workflow_by_id,
            log,
            log_chain,
            args_key,
            read_cache: Mutex::new(HashMap::new()),
            resource_origin: Mutex::new(HashMap::new()),
            breakers: Mutex::new(HashMap::new()),
            limits: ratelimit::Buckets::default(),
            upstream_slots: (config_max_concurrent > 0)
                .then(|| tokio::sync::Semaphore::new(config_max_concurrent)),
        };
        for (idx, ttl) in ttls.into_iter().enumerate() {
            surface.schedule_recheck(idx, ttl);
        }
        surface.vet(&surface.cat())?;
        Ok(surface)
    }

    /// The checks a catalog must pass to be served: overlay bindings against the
    /// live upstream schema, and tool poisoning. Problems that only warn are
    /// logged here; an Err refuses the catalog (at startup the proxy, on a
    /// re-index the new tools).
    fn vet(&self, cat: &Catalog) -> Result<()> {
        // Binding registry: check overlays against the live upstream schema. A
        // broken STRONG binding refuses to start (fail closed); a broken WEAK
        // one is logged and its broken parts are skipped (fail open).
        use crate::config::BindingStrength;
        let broken = self.broken_bindings_in(cat);
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
        // Tool poisoning: a description written to steer the MODEL rather than
        // inform a reader. The linter reports these; this is where the server
        // decides whether to serve them. Cheap enough to run every start —
        // the rules read name and description, and resolve no schemas.
        use crate::config::PoisoningPolicy;
        if self.config.poisoning_policy != PoisoningPolicy::Off {
            let poisoned = Self::poisoned_in(cat);
            if !poisoned.is_empty() {
                let detail = poisoned
                    .iter()
                    .map(|(t, rules)| format!("  {t}: {}", rules.join(", ")))
                    .collect::<Vec<_>>()
                    .join("\n");
                match self.config.poisoning_policy {
                    PoisoningPolicy::Strict => anyhow::bail!(
                        "{} tool(s) tripped a tool-poisoning rule (poisoning_policy: strict):\n{detail}\n{POISONING_HINT}",
                        poisoned.len()
                    ),
                    // `off` is filtered out above; `warn` is the default.
                    _ => crate::log_warn!(
                        "{} tool(s) tripped a tool-poisoning rule and are being served anyway (poisoning_policy: warn):\n{detail}",
                        poisoned.len()
                    ),
                }
            }
        }
        Ok(())
    }

    /// A slot for one upstream call, when `max_concurrent_calls` is enabled.
    ///
    /// The permit must be held across the upstream call ONLY. Holding it any
    /// wider — over pagination, say — would let a call wait on a slot its own
    /// parent is holding, which at a cap of 1 is a deadlock rather than a
    /// throttle.
    pub(crate) async fn upstream_slot(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        match &self.upstream_slots {
            Some(s) => s.acquire().await.ok(),
            None => None,
        }
    }

    /// Append one NDJSON audit event (best-effort; logging never fails a
    /// request). Every line carries who acted (`caller`: the subject, or
    /// `anonymous`), the event kind, and the event's own fields.
    fn log_event(&self, caller: &Caller, event: &str, fields: Value) {
        let Some(sink) = &self.log else { return };
        let mut line = json!({"ts_ms": generators::epoch_millis(), "event": event, "caller": caller.label()});
        if let (Some(obj), Some(extra)) = (line.as_object_mut(), fields.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        if let Some(chain) = &self.log_chain {
            lock(chain).stamp(&mut line);
        }
        match sink {
            AuditSink::File(f) => {
                use std::io::Write;
                let mut f = lock(f);
                let _ = writeln!(f, "{line}");
            }
            AuditSink::Stderr => eprintln!("{line}"),
        }
    }

    /// Visibility as enforced. Under `scopes.report_only` everything is
    /// visible, and [`Self::may_call`] records what would not have been.
    fn allowed(&self, caller: &Caller, tool_id: &str) -> bool {
        self.config.scopes.report_only || self.config.allowed(tool_id, &caller.scopes)
    }

    /// The gate for a CALL — every call path goes through this, so
    /// report-only cannot be bypassed by a new one. Logs `would_deny` for a
    /// call the rules would refuse under `scopes.report_only`. Listing and
    /// search use [`Self::allowed`] and are not logged: what a caller would
    /// lose is the call, and a search per keystroke would bury it.
    fn may_call(&self, caller: &Caller, id: &str) -> bool {
        if !self.config.scopes.report_only {
            return self.config.allowed(id, &caller.scopes);
        }
        if !self.config.allowed(id, &caller.scopes) {
            self.log_event(caller, "would_deny", json!({"tool_id": id, "scopes": caller.scopes}));
        }
        true
    }


    /// Has an overlay changed what this tool returns? Its upstream `outputSchema`
    /// then describes a shape the result no longer has, and clients validate
    /// `structuredContent` against it.
    fn reshapes_result(&self, tool_id: &str) -> bool {
        self.config
            .overlay_for(tool_id)
            .is_some_and(|o| o.paginate.is_some() || (o.response.applies(false) && !o.response.is_noop()))
    }

    fn meta_search(&self) -> Value {
        json!({
            "name": "search_tools",
            // Reads min-mcp's own index: no side effects, nothing outside it.
            // Hosts may then auto-approve it.
            "annotations": read_only_local(),
            "description": "Search every available tool by task description (e.g. \"create a customer\"). Returns matching tool ids with one-line summaries. Call get_tool_details next for the exact input schema.",
            "inputSchema": {"type": "object", "properties": {
                "query": {"type": "string", "description": "What you are trying to do."},
                "k": {"type": "integer", "description": "Max results (default 20)."}
            }, "required": ["query"]}
        })
    }

    fn meta_details(&self) -> Value {
        json!({
            "name": "get_tool_details",
            "annotations": read_only_local(),
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

    fn def_for(&self, tool_id: &str) -> Option<Arc<ToolDef>> {
        self.cat().def(tool_id).cloned()
    }

    /// The composite's index, only if `caller` is allowed to see it.
    fn visible_workflow(&self, caller: &Caller, id: &str) -> Option<&usize> {
        self.workflow_by_id.get(id).filter(|_| self.allowed(caller, id))
    }

    /// Provenance for envelope unwrapping: does this tool's result come from a
    /// spec backend (whose executor wraps responses in `{status, body, …}`)?
    /// MCP/HTTP upstream results are never envelopes, whatever their shape.
    fn tool_from_spec(&self, tool_id: &str) -> bool {
        self.cat()
            .def(tool_id)
            .map(|t| t.upstream_idx)
            .is_some_and(|u| matches!(self.upstreams.get(u), Some(Backend::Spec(_))))
    }

    /// The full input schema to surface for a tool — an overlaid tool's patched
    /// schema wins (resolved + patched at load); otherwise resolve lazily, once,
    /// and memoize (schemas are immutable after load; default-on preflight hits
    /// this per call).
    ///
    /// Takes the catalog `t` came from: re-reading it here could pair one
    /// catalog's tool with another's cache across a re-index, and store the old
    /// schema under the new catalog.
    fn resolved_schema(&self, cat: &Catalog, t: &ToolDef) -> Value {
        if let Some(p) = cat.patched_schemas.get(t.id()) {
            return p.clone();
        }
        if let Some(c) = read(&cat.resolved_cache).get(t.id()) {
            return c.clone();
        }
        let v = self.raw_resolved(t);
        if json_len(&v) <= MAX_CACHED_SCHEMA_BYTES {
            write(&cat.resolved_cache).insert(t.id().to_string(), v.clone());
        }
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

    /// Does `tools/list` declare upstream tools by name?
    pub fn is_passthrough(&self) -> bool {
        self.config.mode == Mode::Passthrough
    }

    /// The tools/list result `caller` sees.
    pub fn list_tools(&self, caller: &Caller) -> Value {
        let mut defs: Vec<Value> = Vec::new();
        self.for_each_tool_def(caller, |def| defs.push(def));
        json!({"tools": defs})
    }

    /// Each definition `tools/list` declares, one at a time, so a caller that only
    /// measures them doesn't hold a huge catalog in memory.
    fn for_each_tool_def(&self, caller: &Caller, mut visit: impl FnMut(Value)) {
        let cat = self.cat();
        for (name, id) in &cat.exposed {
            if let Some(t) = cat.def(id).filter(|_| self.allowed(caller, id)) {
                let mut def = json!({
                    "name": name,
                    "description": t.description,
                    "inputSchema": self.resolved_schema(&cat, t), // resolve $refs on demand
                });
                if let Some(obj) = def.as_object_mut() {
                    obj.extend(t.extra.iter().map(|(k, v)| (k.clone(), v.clone())));
                    if self.reshapes_result(id) {
                        obj.remove("outputSchema");
                    }
                }
                visit(def);
            }
        }
        self.meta_tool_defs().into_iter().for_each(visit);
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
    pub fn cli_search(&self, caller: &Caller, query: &str, k: usize) -> String {
        self.search_text(caller, query, k).0
    }

    /// Full schema for one tool (CLI `minmcp help`).
    pub fn cli_details(&self, caller: &Caller, tool_id: &str) -> String {
        self.details_text(caller, tool_id)
    }

    /// Invoke a tool or composite by id with optional projection (`minmcp call`).
    pub async fn cli_call(&self, caller: &Caller, tool_id: &str, arguments: Value, fields: &[String]) -> Result<Value> {
        let mut r = self.route_call(caller, tool_id, arguments, fields).await?;
        truncate_result_text(&mut r, AGENT_RESULT_BUDGET);
        Ok(r)
    }

    /// Client-side errors (bad args, unknown tool) are returned as isError
    /// tool results with a `bad_arg` marker — NOT as Err, so the transport
    /// loop never mislabels them "the upstream may be down".
    pub async fn call(&self, caller: &Caller, name: &str, args: Value) -> Result<Value> {
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
                let (text, served) = self.search_text(caller, query, k);
                self.log_event(caller, "search", json!({"query": query, "k": k}));
                if self.shadow.enabled() {
                    self.shadow.observe_search(query, &served);
                }
                Ok(text_result(text, false))
            }
            "get_tool_details" if has_meta => {
                let Some(id) = args.get("tool_id").and_then(Value::as_str) else {
                    return Ok(bad_arg("get_tool_details requires a 'tool_id' string"));
                };
                let text = self.details_text(caller, id);
                self.log_event(caller, "details", json!({"tool_id": id}));
                Ok(text_result(text, false))
            }
            "call_tool" if has_meta => {
                let Some(id) = args.get("tool_id").and_then(Value::as_str) else {
                    return Ok(bad_arg("call_tool requires a 'tool_id' string"));
                };
                let id = id.to_string();
                if self.shadow.enabled() {
                    // Eligible = the id is in the searchable surface. A composite
                    // called directly, or a typo, must not count against a retriever
                    // that was never given the chance to rank it.
                    let eligible = self.cat().by_id.contains_key(&id) || self.workflow_by_id.contains_key(&id);
                    for o in self.shadow.observe_call(&id, eligible) {
                        self.log_event(
                            caller,
                            "shadow",
                            json!({"tool_id": id, "method": o.method, "hit": o.hit, "rank": o.rank}),
                        );
                    }
                }
                let Some(inner) = call_arguments(args.get("arguments")) else {
                    return Ok(bad_arg("call_tool 'arguments' must be a JSON object"));
                };
                // optional GraphQL-style field projection to shrink the response
                let fields: Vec<String> = args
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                // Agent boundary: bound the result AFTER overlays/projection ran
                // on the full body (workflow steps and verify call route_call/
                // dispatch directly and read untruncated results).
                let mut r = self.route_call(caller, &id, inner, &fields).await?;
                truncate_result_text(&mut r, AGENT_RESULT_BUDGET);
                Ok(r)
            }
            _ => {
                // Passthrough tool name (no meta wrapper -> no projection).
                // Visibility is applied HERE, before the name is resolved:
                // handing a scoped-out name to dispatch would answer with
                // `unknown tool_id "up.GetX" — did you mean …`, giving a caller
                // the canonical id and its neighbours for tools tools/list
                // deliberately withholds. A hidden tool must be shaped exactly
                // like one that does not exist.
                let cat = self.cat();
                let Some(id) = cat.exposed.get(name).filter(|id| self.allowed(caller, id)).cloned() else {
                    let visible_names = cat
                        .exposed
                        .iter()
                        .filter(|(_, id)| self.allowed(caller, id))
                        .map(|(n, _)| n.as_str());
                    let hint = ids::did_you_mean(name, visible_names);
                    return Ok(text_result(
                        format!("unknown tool {name:?} —{hint} {}", self.recovery()),
                        true,
                    ));
                };
                let mut r = self.dispatch(caller, &id, args, &[]).await?;
                // A tool that declares an `outputSchema` MUST return conforming
                // `structuredContent`, and strict clients error without it, so
                // the budget bounds only its text. Without a schema the
                // structured copy is optional, and an oversized one goes.
                let declared = cat.def(&id).is_some_and(|t| t.extra.contains_key("outputSchema"))
                    && !self.reshapes_result(&id);
                let structured = if declared { r.as_object_mut().and_then(|o| o.remove("structuredContent")) } else { None };
                truncate_passthrough_text(&mut r);
                if let (Some(sc), Some(o)) = (structured, r.as_object_mut()) {
                    o.insert("structuredContent".into(), sc);
                }
                Ok(r)
            }
        }
    }

    /// Returns the agent-facing text and the ids it served, in rank order. The ids
    /// are what shadow mode compares its challengers against.
    fn search_text(&self, caller: &Caller, query: &str, k: usize) -> (String, Vec<String>) {
        // k=0 used to yield "no matches", which reads as a search failure; treat
        // it as "the default", like an omitted k.
        let k = if k == 0 { DEFAULT_SEARCH_K } else { k };
        // over-fetch so scope filtering can't starve the result list
        // (saturating: k is client-controlled, must not overflow)
        let hits = self.cat().index.search(query, k.saturating_mul(3));
        let mut lines: Vec<String> = Vec::new();
        let mut served: Vec<String> = Vec::new();
        for (id, _) in hits {
            if !self.allowed(caller, &id) {
                continue; // invisible, not forbidden
            }
            // upstream tool or composite workflow (both indexed)
            if let Some(desc) = self.describe(&id) {
                let mut summary = desc.lines().next().unwrap_or("").to_string();
                truncate_in_place(&mut summary, 110);
                lines.push(format!("{id} — {summary}"));
                served.push(id);
            }
            if lines.len() >= k {
                break;
            }
        }
        if lines.is_empty() {
            return (
                "no matches — try different words (resource or action names work best)".into(),
                served,
            );
        }
        (lines.join("\n"), served)
    }

    fn details_text(&self, caller: &Caller, tool_id: &str) -> String {
        // composite workflow: describe its declared inputs. Scope-gated like a
        // tool (search already hides it; details and call must agree).
        if let Some(&i) = self.visible_workflow(caller, tool_id) {
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
        let cat = self.cat();
        let Some(t) = cat.def(tool_id).filter(|_| self.allowed(caller, tool_id)) else {
            return format!(
                "unknown tool_id {tool_id:?} —{} {}",
                self.suggest_near_misses(caller, tool_id),
                self.recovery()
            );
        };
        let schema = self.resolved_schema(&cat, t); // resolve $refs on demand
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
    /// tool/workflow ids the caller may see (we hold the whole catalog — a
    /// dot-for-underscore slip should never dead-end). Empty string when nothing
    /// is plausibly close.
    fn suggest_near_misses(&self, caller: &Caller, wrong: &str) -> String {
        let cat = self.cat();
        let candidates = cat
            .by_id
            .keys()
            .chain(self.workflow_by_id.keys())
            .filter(|id| self.allowed(caller, id))
            .map(String::as_str);
        ids::did_you_mean(wrong, candidates)
    }

    /// Route a call to a composite workflow if the id names one, else a tool.
    async fn route_call(&self, caller: &Caller, id: &str, args: Value, fields: &[String]) -> Result<Value> {
        if let Some(&i) = self.workflow_by_id.get(id).filter(|_| self.may_call(caller, id)) {
            let wf = &self.config.workflows[i];
            return self.execute_workflow(caller, wf, args).await;
        }
        self.dispatch(caller, id, args, fields).await
    }

    /// One-line description of any callable id — upstream tool or workflow.
    fn describe(&self, id: &str) -> Option<String> {
        if let Some(t) = self.def_for(id) {
            return Some(t.description.lines().next().unwrap_or("").to_string());
        }
        self.workflow_by_id.get(id).map(|&i| self.config.workflows[i].description.clone())
    }
}
