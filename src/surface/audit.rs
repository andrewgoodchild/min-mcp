//! The audit/inspect side: source map, binding registry, lint, verify, stats.

use super::*;

impl Surface {
    /// A **source map** for the minified surface: every tool the surface knows,
    /// mapped back to its origin — the JS-minifier source map, for tools. It is
    /// what makes the minification auditable: trace a wrong-tool selection back
    /// to `METHOD /path`, see which tools an overlay rewrote, spot name
    /// collisions the sanitizer renamed, and diff `schema_sha` across spec
    /// versions to catch drift. Scope-independent (shows the full pre-scope
    /// surface); a debugging/audit artifact, not something the agent sees.
    /// `visible_only` scope-filters rows AND derived counts for the current
    /// caller (the protocol path); `false` is the full pre-scope audit view
    /// (`minmcp map`). One builder, so a scoped view can never leak a hidden
    /// tool through a side field.
    pub fn source_map(&self, visible_only: bool) -> Value {
        // Invert exposed (name -> id) so each tool can show the MCP name it is
        // declared under in the current mode (null in three_tool: reached via
        // call_tool by id, not declared by name).
        let mut name_of: HashMap<&str, &str> = HashMap::new();
        for (name, id) in &self.exposed {
            name_of.insert(id.as_str(), name.as_str());
        }
        let mut collisions = 0usize;
        let tools: Vec<Value> = self
            .tools
            .iter()
            .filter(|t| !visible_only || self.allowed(t.id()))
            .map(|t| {
                let (upstream, kind, origin) = match self.upstreams.get(t.upstream_idx) {
                    Some(b) => (b.name().to_string(), b.kind(), b.origin(&t.name)),
                    None => (
                        t.id().split('.').next().unwrap_or("").to_string(),
                        "unknown",
                        t.name.clone(),
                    ),
                };
                let sanitized = sanitize_name(t.id(), 0);
                let exposed_as = name_of.get(t.id()).copied();
                // a rename happened if this tool is exposed under a name that
                // isn't its own straight sanitization (disambiguated collision)
                if exposed_as.map(|n| n != sanitized).unwrap_or(false) {
                    collisions += 1;
                }
                let live_sha = self.origin_sha.get(t.id()).cloned().unwrap_or_default();
                let resolved = self.raw_resolved(t);
                let overlay = self.config.overlay_for(t.id()).map(|o| {
                    let (status, reasons) = binding_status(o, Some(&resolved), &live_sha);
                    json!({
                        "description": o.description.is_some(),
                        "fields": o.fields.keys().collect::<Vec<_>>(),
                        "error_hints": o.error_hints.len(),
                        "authored_sha": o.authored_sha,
                        "status": status,          // ok | changed | broken
                        "reasons": reasons,        // why it's changed/broken
                    })
                });
                json!({
                    "tool_id": t.id(),
                    "upstream": upstream,
                    "backend": kind,
                    "origin": origin,
                    "exposed_as": exposed_as,
                    "overlay": overlay,
                    "schema_sha": live_sha,
                })
            })
            .collect();
        // Every overlay whose target tool is absent entirely (a hard break the
        // per-tool loop above can't see, since there's no tool row for it).
        let orphan_overlays: Vec<Value> = self
            .config
            .overlays
            .iter()
            // scoped view: never leak ids of tools the caller can't see
            .filter(|_| !visible_only)
            .filter(|o| self.def_for(&o.tool).is_none())
            .map(|o| {
                let (status, reasons) = binding_status(o, None, "");
                json!({"tool": o.tool, "status": status, "reasons": reasons})
            })
            .collect();
        json!({
            "mode": format!("{:?}", self.config.mode),
            "tool_count": tools.len(),
            "exposed_by_name": self.exposed.len(),
            "renamed_collisions": collisions,
            "overlaid": self.config.overlays.len(),
            "orphan_overlays": orphan_overlays,
            "tools": tools,
        })
    }

    /// Bindings (overlays) broken against the live upstream schema, each with
    /// its effective strength (strong = fail closed, weak = fail open). Drives
    /// startup enforcement and `minmcp map`.
    pub fn broken_bindings(&self) -> Vec<(String, Vec<String>, crate::config::BindingStrength)> {
        let policy = self.config.binding_policy;
        let mut out = Vec::new();
        for o in &self.config.overlays {
            let resolved = self.def_for(&o.tool).map(|d| self.raw_resolved(d));
            let live = self.origin_sha.get(o.tool.as_str()).cloned().unwrap_or_default();
            let (status, reasons) = binding_status(o, resolved.as_ref(), &live);
            if status == "broken" {
                out.push((o.tool.clone(), reasons, o.strength(policy)));
            }
        }
        out
    }

    /// Static quality-lint over every registered tool. Resolves each tool's
    /// schema first (that's what the agent actually sees via get_tool_details),
    /// runs the best-practice rule set, and returns per-rule aggregate stats plus
    /// a sample of flagged tools. A dev/inspect artifact — findings are drafted,
    /// not applied, and this is opt-in (resolving every schema is O(tools), so a
    /// huge spec pays the full-resolution cost here that load deliberately defers).
    pub fn lint_report(&self, sample: usize) -> Value {
        let mut counts: BTreeMap<&'static str, usize> =
            crate::lint::RULES.iter().map(|(id, _)| (*id, 0)).collect();
        let mut flagged: Vec<Value> = Vec::new();
        let total = self.tools.len();
        let mut clean = 0usize;

        // Cross-tool pass: tools sharing a normalized summary are confusable — the
        // agent can't disambiguate them (acute on a federated surface). Group by
        // key, then any group of >1 is flagged.
        let mut by_desc: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, t) in self.tools.iter().enumerate() {
            if let Some(k) = crate::lint::confusable_key(&t.description) {
                // Two REST ops sharing a summary but differing by HTTP method
                // (GET vs POST vs DELETE on the same resource) are disambiguated by
                // the verb — keep the method in the key so CRUD siblings don't group.
                let origin = self.upstreams.get(t.upstream_idx).map(|b| b.origin(&t.name)).unwrap_or_default();
                let key = match crate::lint::http_method(&origin) {
                    Some(m) => format!("{m} {k}"),
                    None => k,
                };
                by_desc.entry(key).or_default().push(i);
            }
        }
        let confusable: std::collections::HashSet<usize> =
            by_desc.values().filter(|v| v.len() > 1).flatten().copied().collect();

        for (i, t) in self.tools.iter().enumerate() {
            let schema = self.resolved_schema(t);
            let origin = self.upstreams.get(t.upstream_idx).map(|b| b.origin(&t.name)).unwrap_or_default();
            let mut fired = crate::lint::lint(&t.name, &t.description, &schema, crate::lint::is_mutating(&origin));
            if confusable.contains(&i) {
                fired.push("confusable_descriptions");
            }
            if fired.is_empty() {
                clean += 1;
            }
            for r in &fired {
                *counts.get_mut(r).unwrap() += 1;
            }
            if !fired.is_empty() && flagged.len() < sample {
                flagged.push(json!({"tool_id": t.id(), "findings": fired}));
            }
        }
        let pct = |c: usize| if total > 0 { (c as f64 * 1000.0 / total as f64).round() / 10.0 } else { 0.0 };
        let rules: Vec<Value> = crate::lint::RULES
            .iter()
            .map(|(id, why)| json!({"rule": id, "count": counts[id], "pct": pct(counts[id]), "why": why}))
            .collect();
        json!({
            "tools": total,
            "clean": clean,
            "flagged": total - clean,
            "rules": rules,
            "sample": flagged,
        })
    }

    /// Run every overlay's `verify:` checks against the live upstream — the
    /// dynamic third leg (detect → fix → **verify**). Each check calls the tool
    /// through the full dispatch path (so overlay headers/defaults/field-patches/
    /// response-transform all apply — it verifies the *fixed* tool the agent sees)
    /// and evaluates deterministic assertions. Makes real network calls.
    pub async fn verify(&mut self) -> Value {
        // Collect first (owned) so the &config borrow ends before &mut dispatch.
        let checks: Vec<(String, crate::config::VerifyCheck)> = self
            .config
            .overlays
            .iter()
            .flat_map(|o| o.verify.iter().map(|c| (o.tool.clone(), c.clone())).collect::<Vec<_>>())
            .collect();
        let (mut passed, mut failed) = (0usize, 0usize);
        let mut results = Vec::new();
        for (tool, check) in checks {
            let outcome = match self.route_call(&tool, check.arguments.clone(), &[]).await {
                Ok(r) => eval_expect(&r, self.tool_from_spec(&tool), &check.expect),
                Err(e) => (false, vec![format!("call errored: {e}")]),
            };
            if outcome.0 {
                passed += 1;
            } else {
                failed += 1;
            }
            results.push(json!({
                "tool": tool,
                "name": check.name,
                "pass": outcome.0,
                "failures": outcome.1,
            }));
        }
        json!({"passed": passed, "failed": failed, "checks": results})
    }

    /// Stats for `minmcp inspect`.
    pub fn stats(&self) -> Value {
        let upstream_defs: usize = self.tools.len();
        // Spec upstreams store cheap UNRESOLVED $ref stubs at load, which badly
        // understate the raw baseline. Small surfaces resolve exactly (that's
        // where the passthrough-vs-minified comparison matters); big ones keep
        // the stub sum as an explicit LOWER BOUND, flagged `est_raw_exact`.
        const EXACT_RESOLVE_CAP: usize = 32;
        let exact = self.tools.len() <= EXACT_RESOLVE_CAP;
        let raw_chars: usize = self
            .tools
            .iter()
            .map(|t| {
                // byte-count via a sink — no throwaway String per tool
                let schema_len = if exact {
                    json_len(&self.raw_resolved(t))
                } else {
                    json_len(&t.input_schema)
                };
                t.description.len() + schema_len + t.name.len()
            })
            .sum();
        let minified = self.list_tools();
        let minified_count = minified["tools"].as_array().map(Vec::len).unwrap_or(0);
        let min_chars = json_len(&minified);
        json!({
            "mode": format!("{:?}", self.config.mode),
            "upstreams_configured": self.config.upstreams.len(),
            // active = configured minus any fully filtered-out upstreams
            "upstreams_active": self.upstreams.len(),
            "upstream_tools": upstream_defs,
            "visible_after_scopes": self.tools.iter().filter(|t| self.allowed(t.id())).count(),
            "surface_tools": minified_count,
            "est_tokens_raw": raw_chars / 4,
            "est_tokens_minified": min_chars / 4,
            "est_raw_exact": exact,
        })
    }
}
