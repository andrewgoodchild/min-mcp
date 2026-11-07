//! The catalog: everything derived from the upstreams' tool listings, and
//! re-indexing it when an upstream's tools change.
//!
//! An upstream signals a change in one of two ways. A legacy one sends
//! `notifications/tools/list_changed`, which flags it stale. A 2026-07-28 one
//! announces changes only to subscribers, but stamps each listing with how long
//! it stays fresh (`ttlMs`). Either way the next request re-lists that upstream
//! alone, rebuilds the catalog from its new listing and everyone else's last
//! one, runs the startup checks on it, and swaps it in whole. A request holds
//! one snapshot from start to finish, so it never sees half of each.
//!
//! The checks keep their startup meaning: a broken STRONG binding or a strict
//! poisoning hit refuses the new tools. At startup that stops the proxy; here
//! that upstream keeps its last good tools, logged, and is retried in 30 s
//! rather than going down. Each changed upstream is vetted on its own, so one
//! refusal doesn't hold back another upstream's change.

use super::*;
use std::time::Duration;

/// Floor between re-lists of one upstream on its `ttlMs`, so an upstream that
/// says 0 ("don't cache") can't turn every request into a re-list.
const MIN_RECHECK: Duration = Duration::from_secs(10);
/// Ceiling on a listing's `ttlMs`: an upstream that says "fresh for a year"
/// is still re-checked daily.
const MAX_RECHECK: Duration = Duration::from_secs(24 * 60 * 60);
/// Wait after a failed re-list before trying that upstream again.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30);

pub(crate) struct Catalog {
    pub(super) tools: Vec<Arc<ToolDef>>,
    /// tool_id -> index into `tools` (single source of truth for existence)
    pub(super) by_id: HashMap<String, usize>,
    /// exposed MCP tool name -> tool_id (passthrough mode), over EVERY tool;
    /// `list_tools` filters it per caller. BTreeMap so list order is
    /// deterministic without a per-call sort.
    pub(super) exposed: BTreeMap<String, String>,
    pub(super) index: Index,
    /// tool_id -> fingerprint of the RAW upstream tool (description + input
    /// schema), captured BEFORE overlay patching. `authored_sha` pins this, so a
    /// rug-pull that changes only the top-level description is caught as drift —
    /// not just a schema-shape change.
    pub(super) origin_sha: HashMap<String, String>,
    /// tool_id -> fully-resolved input schema WITH the overlay's field patches
    /// applied. Only tools that carry a schema-changing overlay are here (so a
    /// huge un-overlaid spec still resolves lazily); `resolved_schema` prefers it.
    pub(super) patched_schemas: HashMap<String, Value>,
    /// tool_id -> per-operation request headers (overlay `headers:`), with `${…}`
    /// already resolved but per-request generators (`{{uuid}}`, `{{now}}`) still as
    /// tokens — resolved fresh on every call in `dispatch`.
    pub(super) tool_headers: HashMap<String, Vec<(String, String)>>,
    /// tool_id -> [(field_path, source)] for `user_supplied` fields: stripped from
    /// the agent schema and injected from `source` (`env:VAR`, `file:…`, `vault:…`)
    /// at call time.
    pub(super) user_supplied: HashMap<String, Vec<(String, String)>>,
    /// tool_id -> lazily resolved schema. Default-on preflight resolves on every
    /// call; without this a spec tool would re-expand its `$ref`s each time.
    /// Per catalog, so a re-indexed tool never serves its old schema.
    pub(super) resolved_cache: RwLock<HashMap<String, Value>>,
    /// Each upstream's listing as last fetched, before filters and overlays,
    /// by backend index: a re-index of one upstream rebuilds from the others'
    /// without listing them again.
    pub(super) listings: Vec<Vec<ToolDef>>,
}

impl Catalog {
    pub(super) fn def(&self, tool_id: &str) -> Option<&Arc<ToolDef>> {
        self.by_id.get(tool_id).map(|&i| &self.tools[i])
    }

    /// Filter, overlay-patch and index the upstreams' listings. Shared by
    /// startup and re-indexing, so a re-indexed tool gets exactly the treatment
    /// it would have had at startup.
    pub(super) async fn assemble(
        config: &Config,
        secrets: &Secrets,
        upstreams: &[Backend],
        listings: Vec<Vec<ToolDef>>,
    ) -> Result<Catalog> {
        let mut tools: Vec<Arc<ToolDef>> = Vec::new();
        let mut by_id = HashMap::new();
        let mut origin_sha: HashMap<String, String> = HashMap::new();
        let mut patched: HashMap<String, Value> = HashMap::new();
        let mut tool_headers: HashMap<String, Vec<(String, String)>> = HashMap::new();
        let mut user_supplied: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (up, listed) in upstreams.iter().zip(&listings) {
            for t in listed {
                // static include/exclude: a filtered tool never enters the
                // surface — not listed, searchable, or callable, for any caller.
                if !config.passes_filter(up.name(), t.id()) {
                    continue;
                }
                let mut t = t.clone();
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
                    // Per-tool headers: resolve `${…}` now (fail loud, keeps secrets
                    // out of config); leave {{...}} generators for per-call resolution.
                    if !o.headers.is_empty() {
                        let mut hs = Vec::with_capacity(o.headers.len());
                        for (k, v) in &o.headers {
                            hs.push((k.clone(), secrets.expand(v).await?));
                        }
                        tool_headers.insert(t.id().to_string(), hs);
                    }
                }
                by_id.insert(t.id().to_string(), tools.len());
                tools.push(Arc::new(t));
            }
        }
        let index = Index::build(&corpus(config, &tools));
        let exposed = exposed_names(config.mode, &tools);
        Ok(Catalog {
            tools,
            by_id,
            exposed,
            index,
            origin_sha,
            patched_schemas: patched,
            tool_headers,
            user_supplied,
            resolved_cache: RwLock::new(HashMap::new()),
            listings,
        })
    }
}

/// What search indexes: upstream tools AND composite workflows (both
/// searchable/callable). Overlay `aliases` are appended to a tool's indexed
/// text so a poorly-named tool is findable by an outcome phrase, without
/// changing its id.
pub(super) fn corpus(config: &Config, tools: &[Arc<ToolDef>]) -> Vec<crate::index::IndexedTool> {
    let mut corpus: Vec<crate::index::IndexedTool> = tools
        .iter()
        .map(|t| {
            let aliases = config.overlay_for(t.id()).map(|o| o.aliases.join(" ")).unwrap_or_default();
            let text = if aliases.is_empty() { t.description.clone() } else { format!("{} {}", t.description, aliases) };
            crate::index::IndexedTool {
                id: t.id().to_string(),
                description: text,
                // Harvested unconditionally but only *indexed* by variants that
                // opt in (IndexOptions::params) — the shipped index ignores it.
                params: crate::index::schema_param_text(&t.input_schema),
            }
        })
        .collect();
    for wf in &config.workflows {
        corpus.push(crate::index::IndexedTool {
            id: wf.id.clone(),
            description: wf.description.clone(),
            params: String::new(), // composites declare no input schema
        });
    }
    corpus
}

/// Passthrough tool names. Three-tool mode declares no upstream tool by name:
/// the agent reaches them via the meta-tools. A tool keeps its upstream name,
/// which is what host permission rules are written against; only a name two
/// upstreams share takes the `upstream_` prefix, on both sides, so the
/// outcome doesn't depend on upstream order.
pub(super) fn exposed_names(mode: Mode, tools: &[Arc<ToolDef>]) -> BTreeMap<String, String> {
    let mut exposed = BTreeMap::new();
    let ids: Vec<(String, String)> = match mode {
        Mode::Passthrough => tools.iter().map(|t| (t.id().to_string(), sanitize_name(&t.name, 0))).collect(),
        Mode::ThreeTool => vec![],
    };
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for (_, bare) in &ids {
        *uses.entry(bare).or_default() += 1;
    }
    for (id, bare) in &ids {
        let mut name = if uses[bare.as_str()] == 1 { bare.clone() } else { sanitize_name(id, 0) };
        if exposed.contains_key(&name) {
            // Reserve room for a "_N" suffix up front so the disambiguated
            // name is strictly shorter than the 64-char cap — otherwise a
            // re-truncated name could reproduce the colliding string and
            // loop forever.
            let base = sanitize_name(id, 4);
            let mut n = 2;
            loop {
                name = format!("{base}_{n}");
                if !exposed.contains_key(&name) {
                    break;
                }
                n += 1;
            }
        }
        exposed.insert(name, id.clone());
    }
    exposed
}

/// The identity of a listing, to tell a real change from a re-list that
/// returned the same tools.
fn listing_fingerprint(tools: &[ToolDef]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> =
        tools.iter().map(|t| (t.id().to_string(), tool_fingerprint(&t.description, &t.input_schema))).collect();
    v.sort_unstable();
    v
}

impl Surface {
    /// The current catalog. A request takes one snapshot and uses it
    /// throughout; a concurrent re-index swaps in a new one without
    /// disturbing it.
    pub(crate) fn cat(&self) -> Arc<Catalog> {
        read(&self.catalog).clone()
    }

    #[cfg(test)]
    pub(super) fn cat_mut(&mut self) -> &mut Catalog {
        Arc::get_mut(self.catalog.get_mut().unwrap_or_else(|e| e.into_inner()))
            .expect("no snapshot is held while a test edits the catalog")
    }

    /// Re-derive passthrough names after a test edits the tool list.
    #[cfg(test)]
    pub(super) fn build_exposed(&mut self) {
        let mode = self.config.mode;
        let cat = self.cat_mut();
        cat.exposed = exposed_names(mode, &cat.tools);
    }

    /// Record when upstream `idx` should next be re-listed, from its `ttlMs`.
    pub(super) fn schedule_recheck(&self, idx: usize, ttl_ms: Option<u64>) {
        // Capped: `Instant + Duration` panics on overflow on some platforms, and
        // an upstream picks the number.
        let at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms).clamp(MIN_RECHECK, MAX_RECHECK));
        if let Some(slot) = lock(&self.recheck_at).get_mut(idx) {
            *slot = at;
        }
    }

    /// Upstreams due a re-list: flagged stale, or past their listing's `ttlMs`.
    fn due(&self) -> Vec<usize> {
        let now = Instant::now();
        let recheck = lock(&self.recheck_at);
        (0..self.upstreams.len())
            .filter(|&i| self.upstreams[i].stale() || recheck.get(i).copied().flatten().is_some_and(|at| at <= now))
            .collect()
    }

    /// Re-index any upstream whose tools may have changed. Called at the start
    /// of each request; cheap when nothing is due. Returns whether the catalog
    /// changed, so the caller can tell its client. Single-flight: a request
    /// that finds a re-index already running serves the current catalog
    /// rather than waiting on it.
    pub async fn refresh_if_due(&self) -> bool {
        if self.due().is_empty() {
            return false;
        }
        let Ok(_flight) = self.refreshing.try_lock() else {
            return false;
        };
        let due = self.due(); // another request may have just finished one
        if due.is_empty() {
            return false;
        }
        let current = self.cat();
        let mut fresh: Vec<(usize, Vec<ToolDef>)> = Vec::new();
        for idx in due {
            let up = &self.upstreams[idx];
            // Cleared BEFORE listing: a change announced mid-list flags it again.
            up.take_stale();
            match up.list_tools_ttl(idx).await {
                Ok((listed, ttl)) => {
                    self.schedule_recheck(idx, ttl);
                    if listing_fingerprint(&listed) != listing_fingerprint(&current.listings[idx]) {
                        crate::log_info!(
                            "upstream {} changed its tools ({} → {}); re-indexing",
                            up.name(),
                            current.listings[idx].len(),
                            listed.len()
                        );
                        fresh.push((idx, listed));
                    }
                }
                Err(e) => {
                    crate::log_warn!("re-listing upstream {} failed, keeping its last tools: {e:#}", up.name());
                    self.retry_later(idx);
                }
            }
        }
        // One upstream at a time, each on top of the last accepted: a change
        // the checks refuse is that upstream's alone, and another upstream's
        // legitimate change still goes through.
        let mut accepted: Option<Catalog> = None;
        for (idx, listed) in fresh {
            let mut listings = accepted.as_ref().unwrap_or(&current).listings.clone();
            listings[idx] = listed;
            let name = self.upstreams[idx].name();
            let built = match Catalog::assemble(&self.config, &self.secrets, &self.upstreams, listings).await {
                Ok(c) => self.vet(&c).map(|()| c),
                Err(e) => Err(e),
            };
            match built {
                Ok(c) => accepted = Some(c),
                Err(e) => {
                    crate::log_error!("refused upstream {name}'s new tools, still serving its previous ones: {e:#}");
                    // Its signal was consumed; without this a legacy upstream
                    // that never announces again would stay stale for good.
                    self.retry_later(idx);
                }
            }
        }
        let Some(next) = accepted else {
            return false;
        };
        next.index.inherit_usage(&current.index);
        crate::log_info!("re-indexed: {} tools (was {})", next.tools.len(), current.tools.len());
        *write(&self.catalog) = Arc::new(next);
        true
    }

    fn retry_later(&self, idx: usize) {
        if let Some(slot) = lock(&self.recheck_at).get_mut(idx) {
            *slot = Some(Instant::now() + RETRY_AFTER_FAILURE);
        }
    }
}
