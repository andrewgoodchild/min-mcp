//! Idempotent-read cache policy: what may be cached, and write-through
//! invalidation. The cache itself lives on `Surface`; the TTL/insert flow
//! is in `dispatch` (it is call-path logic, not policy).

use super::*;

/// Read-cache size bound (entries, not bytes — results are already capped).
pub(super) const READ_CACHE_MAX_ENTRIES: usize = 512;

/// Canonical cache-key form of a call's arguments: object keys sorted
/// recursively, so `{"a":1,"b":2}` and `{"b":2,"a":1}` — the same call — hit
/// the same entry. (serde_json's `preserve_order` keeps insertion order, which
/// would otherwise split identical calls across spurious keys; mined from
/// ContextForge's `cached_tool_result`, which sorts keys for exactly this.)
pub(super) fn canonical_args(v: &Value) -> String {
    fn write_canon(v: &Value, out: &mut String) {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(k).unwrap_or_default());
                    out.push(':');
                    write_canon(&map[*k], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_canon(item, out);
                }
                out.push(']');
            }
            scalar => out.push_str(&serde_json::to_string(scalar).unwrap_or_default()),
        }
    }
    let mut out = String::new();
    write_canon(v, &mut out);
    out
}

impl Surface {
    /// Drop every cached read belonging to upstream `idx` (write-through
    /// coherence: a write through this proxy must not leave stale reads behind).
    pub(super) fn bust_upstream_cache(&mut self, idx: usize) {
        if self.read_cache.is_empty() {
            return;
        }
        // O(cache entries) via the existing id index — no per-write set build.
        let (by_id, tools) = (&self.by_id, &self.tools);
        self.read_cache
            .retain(|(id, _), _| by_id.get(id).is_none_or(|&i| tools[i].upstream_idx != idx));
    }

    /// May this tool's results be served from the read cache? Overlay
    /// `cacheable` overrides; otherwise only an upstream read-only signal
    /// qualifies. Never when caching is disabled (ttl 0).
    pub(super) fn cache_allowed(&self, tool_id: &str, read_only: Option<bool>) -> bool {
        if self.config.read_cache_ttl_s == 0 {
            return false;
        }
        match self.config.overlay_for(tool_id).and_then(|o| o.cacheable) {
            Some(c) => c,
            None => read_only == Some(true),
        }
    }
}
