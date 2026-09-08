//! Prompts & resources: the same object model as tools (namespaced,
//! upstream-scoped, source-mapped), NOT a raw tunnel — plus min-mcp's own
//! `minmcp://tools` source-map resource. The protocol verbs stay native
//! because these lists are client-pulled on demand — unlike tool definitions
//! they don't ride every model request, so the context-tax pressure that
//! justifies the 3-tool surface doesn't apply here.

use super::*;

/// Our own MCP resource: the source map, over the protocol.
const SOURCE_MAP_URI: &str = "minmcp://tools";

impl Surface {
    /// Are this upstream's prompts/resources visible to `caller`? An upstream
    /// is hidden through these side doors exactly when scoping hides ALL of
    /// its tools — but an upstream that exposes NO tools at all (a prompt
    /// library, a resource server; valid per the MCP spec) has nothing to
    /// scope on and stays visible.
    pub(super) fn upstream_visible(&self, caller: &Caller, idx: usize) -> bool {
        let mut has_tools = false;
        for t in self.tools.iter().filter(|t| t.upstream_idx == idx) {
            has_tools = true;
            if self.allowed(caller, t.id()) {
                return true;
            }
        }
        !has_tools
    }

    /// Merged `resources/list`: every visible upstream's resources plus min-mcp's
    /// own `minmcp://tools` source-map resource. Also (re)builds the uri→upstream
    /// routing table reads use.
    pub async fn list_resources(&self, caller: &Caller) -> Value {
        let mut resources: Vec<Value> = Vec::new();
        let mut origin: HashMap<String, usize> = HashMap::new();
        for (idx, up) in self.upstreams.iter().enumerate() {
            if !self.upstream_visible(caller, idx) {
                continue;
            }
            for r in up.list_passthrough("resources/list", "resources").await {
                if let Some(uri) = r.get("uri").and_then(Value::as_str) {
                    if let Some(prev) = origin.insert(uri.to_string(), idx) {
                        // Duplicate URI across upstreams: last-listed wins for
                        // reads. Warn — silent mis-routing is worse than noise.
                        crate::log_warn!(
                            "resource uri {uri:?} listed by two upstreams (#{prev} and #{idx}); reads route to the latter"
                        );
                    }
                }
                resources.push(r);
            }
        }
        // Merge rather than replace: a scoped caller's listing must not erase
        // the routes another caller's listing learned.
        lock(&self.resource_origin).extend(origin);
        resources.push(json!({
            "uri": SOURCE_MAP_URI,
            "name": "uncompressed-tools",
            "title": "min-mcp source map",
            "description": "Every tool behind this minified surface, mapped to its origin (METHOD /path or upstream tool name), overlay/binding status, and schema fingerprint.",
            "mimeType": "application/json",
        }));
        json!({"resources": resources})
    }

    /// `resources/read`: our own source-map resource, or forwarded to the
    /// upstream that listed the URI. The routing table refreshes on ANY miss —
    /// not just when empty — so a resource added after the last list is
    /// readable without the client re-listing first.
    pub async fn read_resource(&self, caller: &Caller, uri: &str) -> Result<Value> {
        if uri == SOURCE_MAP_URI {
            // The CLI's `minmcp map` shows the full pre-scope surface; over the
            // PROTOCOL the map is scope-filtered at the source (rows, counts,
            // and orphan-overlay ids alike — no side-door leaks).
            let map = self.source_map(Some(caller));
            let text = serde_json::to_string_pretty(&map).unwrap_or_default();
            return Ok(json!({"contents": [
                {"uri": SOURCE_MAP_URI, "mimeType": "application/json", "text": text}
            ]}));
        }
        let known = lock(&self.resource_origin).get(uri).copied();
        let idx = match known {
            Some(i) => i,
            None => {
                let _ = self.list_resources(caller).await; // refresh the routing table
                let Some(i) = lock(&self.resource_origin).get(uri).copied() else {
                    anyhow::bail!("unknown resource uri {uri:?} — list resources first");
                };
                i
            }
        };
        if !self.upstream_visible(caller, idx) {
            anyhow::bail!("unknown resource uri {uri:?}"); // invisible, not forbidden
        }
        self.upstreams[idx].read_resource(uri).await
    }

    /// Merged `prompts/list`, names namespaced `upstream.name` (same id scheme
    /// as tools) so two upstreams' prompts can't shadow each other.
    pub async fn list_prompts(&self, caller: &Caller) -> Value {
        let mut prompts: Vec<Value> = Vec::new();
        for (idx, up) in self.upstreams.iter().enumerate() {
            if !self.upstream_visible(caller, idx) {
                continue;
            }
            let upstream_name = up.name();
            for mut p in up.list_passthrough("prompts/list", "prompts").await {
                if let Some(name) = p.get("name").and_then(Value::as_str).map(str::to_string) {
                    if let Some(obj) = p.as_object_mut() {
                        obj.insert("name".into(), json!(format!("{upstream_name}.{name}")));
                    }
                }
                prompts.push(p);
            }
        }
        json!({"prompts": prompts})
    }

    /// `prompts/get` for a namespaced prompt name (`upstream.name`), forwarded
    /// to the owning upstream with its own (bare) prompt name. Resolution
    /// matches KNOWN upstream names as prefixes — longest first — so an
    /// upstream whose configured name itself contains a dot (`corp.jira`)
    /// still round-trips; a bare `split_once('.')` would strand its prompts.
    pub async fn get_prompt(&self, caller: &Caller, name: &str, arguments: Value) -> Result<Value> {
        let mut best: Option<(usize, usize)> = None; // (upstream idx, prefix len)
        for (i, up) in self.upstreams.iter().enumerate() {
            let up = up.name();
            let stripped = name.strip_prefix(up).and_then(|r| r.strip_prefix('.'));
            let Some(bare) = stripped else { continue };
            if up.is_empty() || bare.is_empty() || !self.upstream_visible(caller, i) {
                continue;
            }
            if best.map(|(_, l)| up.len() > l).unwrap_or(true) {
                best = Some((i, up.len()));
            }
        }
        let Some((idx, prefix_len)) = best else {
            anyhow::bail!("unknown prompt {name:?} — prompt names are \"upstream.name\"");
        };
        let bare = &name[prefix_len + 1..];
        self.upstreams[idx].get_prompt(bare, arguments).await
    }
}
