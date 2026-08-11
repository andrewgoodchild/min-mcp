//! The call path: preflight, cache, headers, pagination, response shaping, error hints.

use super::*;

impl Surface {
    pub(super) async fn dispatch(&mut self, tool_id: &str, arguments: Value, fields: &[String]) -> Result<Value> {
        let Some(t) = self.visible_def(tool_id) else {
            return Ok(text_result(
                format!(
                    "unknown tool_id {tool_id:?} —{} {}",
                    self.suggest_near_misses(tool_id),
                    self.recovery()
                ),
                true,
            ));
        };
        let (idx, original_name, read_only) = (t.upstream_idx, t.name.clone(), t.read_only);
        // Everything overlay-derived, from ONE lookup (cloned/finished before
        // the &mut upstreams call below): preflight override (global default is
        // ON — a local structured error beats a raw upstream dump), per-tool
        // timeout + breaker, request defaults, and pagination config.
        let mut arguments = arguments;
        let (preflight_on, timeout_s, breaker_cfg, paginate) = {
            let ov = self.config.overlay_for(tool_id);
            if let Some(o) = ov {
                for (path, val) in &o.defaults {
                    crate::project::set_default(&mut arguments, path, val.clone());
                }
            }
            (
                ov.and_then(|o| o.preflight).unwrap_or(self.config.preflight),
                ov.and_then(|o| o.timeout_s),
                ov.and_then(|o| o.breaker.clone()),
                ov.and_then(|o| o.paginate.clone()),
            )
        };
        // Clamped to the transport ceiling: `timeout_s` TIGHTENS the 120s default,
        // it never extends it. Unclamped, an HTTP/spec deadline replaced reqwest's
        // client timeout (a timeout_s: 600 held the shared Surface mutex for ten
        // minutes, freezing every session), while stdio's 120s per-line read made
        // the same value unenforceable — the transports diverged and both
        // contradicted the config doc.
        let deadline = timeout_s
            .map(|t| std::time::Duration::from_secs(t.min(crate::upstream::TRANSPORT_CEILING_S)));
        // Resolve the patched schema up front (owned) for pre-flight. Only when
        // preflight is enabled (skips the clone otherwise).
        let preflight_schema = if preflight_on {
            Some(self.resolved_schema(t))
        } else {
            None
        };
        // Per-operation overlay headers, with per-request generators resolved fresh
        // for THIS call. `{{hash}}` = an idempotency key derived from the request
        // (identical args → identical key, so an agent retry is de-duplicated); it's
        // only worth serializing+hashing the request when a header actually uses it.
        let extra_headers: Vec<(String, String)> = match self.tool_headers.get(tool_id) {
            Some(hs) => {
                let args_hash = if hs.iter().any(|(_, v)| v.contains("{{hash}}")) {
                    // canonical (key-order-insensitive), matching the cache key:
                    // a retry that reorders JSON keys must produce the SAME
                    // idempotency key, or the upstream's dedup never fires
                    format!("{:016x}", fnv1a(&cache::canonical_args(&arguments)))
                } else {
                    String::new()
                };
                hs.iter().map(|(k, v)| (k.clone(), resolve_generators(v, &args_hash))).collect()
            }
            None => Vec::new(),
        };
        // User-supplied fields: inject the authoritative value the agent can't see
        // or fabricate (it was stripped from the schema; source is session/env).
        // Runs before pre-flight so the injected value satisfies the schema; an
        // unresolvable source is a clear local error, never a fabricated success.
        if let Some(us) = self.user_supplied.get(tool_id).cloned() {
            for (path, source) in us {
                match resolve_user_source(&source) {
                    Some(v) => crate::project::set(&mut arguments, &path, Value::String(v)),
                    None => {
                        let e = json!({
                            "error": "missing_user_supplied_value", "field": path,
                            "source": source,
                            "fix": "this value comes from the session/environment, not the agent; it is not available"
                        });
                        return Ok(text_result(format!("USER_SUPPLIED_MISSING: {e}"), true));
                    }
                }
            }
        }
        // Pre-flight: reject a call that violates the patched schema locally, with
        // a structured error — no upstream round-trip, no opaque 400 → thrash.
        // Runs AFTER defaults (so a default-filled field counts as present).
        if let Some(schema) = &preflight_schema {
            if let Some(se) = preflight_error(schema, &arguments) {
                // Operator-facing: preflight is ON by default, so if a spec
                // over-declares `required` this is where calls that used to
                // reach the API now stop. Name the escape hatch in the log so
                // the cause is self-diagnosing (MINMCP_LOG=warn).
                crate::log_warn!(
                    "preflight rejected a call to {tool_id} locally: {se}. If this tool's schema \
                     over-declares its requirements, set `preflight: false` in its overlay (or \
                     globally) to let the call reach the upstream."
                );
                return Ok(text_result(format!("PREFLIGHT_ERROR: {se}"), true));
            }
        }
        // Idempotent-read cache: keyed on (tool, post-default arguments), only for
        // tools with a read-only signal (spec GET / readOnlyHint / overlay
        // `cacheable: true`), only when a TTL is configured. The cached value is
        // the RAW pre-shaping result (post-pagination), so each hit still gets
        // THIS call's overlay transform + `fields` projection below.
        let cacheable = self.cache_allowed(tool_id, read_only);
        let cache_key =
            cacheable.then(|| (tool_id.to_string(), cache::canonical_args(&arguments)));
        let ttl = std::time::Duration::from_secs(self.config.read_cache_ttl_s);
        // A call is a "read" for coherence purposes if the upstream says so or
        // an overlay opted it into the cache; anything else is assumed to write.
        let is_read = read_only == Some(true) || cacheable;
        let cached: Option<Value> = cache_key
            .as_ref()
            .and_then(|k| self.read_cache.get(k))
            .filter(|(at, _)| at.elapsed() < ttl)
            .map(|(_, v)| v.clone());
        let from_cache = cached.is_some();
        let mut was_probe = false;
        let mut result = match cached {
            Some(v) => v,
            None => {
                // Circuit breaker: refuse locally while open — an agent must not
                // burn turns re-calling a tool that fails identically (law 6,
                // made structural). A cache hit above never reaches this gate.
                if let Some(b) = &breaker_cfg {
                    let now = std::time::Instant::now();
                    match self.breakers.entry(tool_id.to_string()).or_default().check(b, now) {
                        breaker::Decision::Block { failures, retry_in_s } => {
                            self.log_event("breaker", json!({"tool_id": tool_id, "state": "open"}));
                            return Ok(text_result(
                                format!(
                                    "BREAKER_OPEN: {tool_id} has failed {failures} time(s) in a row and is paused for ~{retry_in_s}s. \
                                     Do not retry it now — use a different tool or report what is failing."
                                ),
                                true,
                            ));
                        }
                        breaker::Decision::Allow { probe } => was_probe = probe,
                    }
                }
                // The deadline is applied INSIDE each backend so a stdio write
                // is never cancelled mid-frame (a dropped half-written line
                // would merge with the next request into one corrupt frame).
                // Clone the arguments only when pagination will reuse them.
                let call_args = match &paginate {
                    Some(_) => arguments.clone(),
                    None => std::mem::take(&mut arguments),
                };
                match self
                    .upstreams[idx]
                    .call_tool(&original_name, call_args, &extra_headers, deadline)
                    .await
                {
                    Ok(r) => r,
                    Err(e) if e.downcast_ref::<crate::upstream::TimeoutElapsed>().is_some() => {
                        // An isError the agent can reason about, never a silent
                        // stall — with the shared write-safety guidance.
                        text_result(
                            format!(
                                "TIMEOUT: {tool_id} did not respond within {}s. {}",
                                timeout_s.unwrap_or_default(),
                                crate::upstream::TIMEOUT_GUIDANCE
                            ),
                            true,
                        )
                    }
                    Err(e) => {
                        // Transport failure (propagated as a protocol error):
                        // still a FAILURE for the breaker — a hard-down upstream
                        // must trip it, and a failed half-open probe must release
                        // its in-flight slot — and still a write for coherence.
                        if let Some(b) = &breaker_cfg {
                            self.breakers
                                .entry(tool_id.to_string())
                                .or_default()
                                .on_result(b, true, was_probe, std::time::Instant::now());
                        }
                        if !is_read {
                            self.bust_upstream_cache(idx);
                        }
                        return Err(e);
                    }
                }
            }
        };
        let mut is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        // The usage prior counts SUCCESSES, not attempts. It used to fire on every
        // path — preflight rejections that never left the proxy, timeouts, upstream
        // isError — which promoted tools in search ranking *by failing*: the exact
        // opposite of the breaker's philosophy, and a feedback loop for confusable
        // tools (the agent keeps picking the wrong sibling, the wrong sibling keeps
        // rising). Found when the recall eval's own failing calls warmed the prior
        // and flipped a correct top-1 (PostPrices behind PostProducts at 2 failed
        // "uses" — the 1.16x boost outweighed a close lexical margin).
        if !is_error {
            self.index.record_use(tool_id);
        }
        if !from_cache {
            // Feed the breaker the PRIMARY call's outcome (cache hits never count).
            if let Some(b) = &breaker_cfg {
                self.breakers
                    .entry(tool_id.to_string())
                    .or_default()
                    .on_result(b, is_error, was_probe, std::time::Instant::now());
            }
            // Follow pagination and concatenate before any response shaping, so the
            // agent gets one complete list instead of hand-rolling a cursor loop.
            // (A cache hit already stored the merged list.)
            let mut partial_pages = false;
            if let Some(p) = &paginate {
                if !is_error {
                    let (merged, partial) = self
                        .paginate(idx, &original_name, arguments, &extra_headers, result, p, deadline)
                        .await?;
                    result = merged;
                    partial_pages = partial;
                    is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
                }
            }
            // Cache only clean successes (an error result must never be
            // replayed) — and a pagination run that stopped early is NOT clean:
            // its merged list is known-incomplete, and serving it for the full
            // TTL would replay the gap long after the upstream recovered.
            // Only reasonably-sized ones — 512 entries of
            // multi-MB pre-truncation payloads would be a memory foot-gun.
            // (The size probe runs only when this call is actually cacheable.)
            if !is_error && !partial_pages {
                if let Some(k) = cache_key {
                    const CACHE_ENTRY_MAX_BYTES: usize = 262_144;
                    if json_len(&result) <= CACHE_ENTRY_MAX_BYTES {
                        if self.read_cache.len() >= READ_CACHE_MAX_ENTRIES {
                            // bounded: drop expired first; if still full, start fresh
                            self.read_cache.retain(|_, (at, _)| at.elapsed() < ttl);
                            if self.read_cache.len() >= READ_CACHE_MAX_ENTRIES {
                                self.read_cache.clear();
                            }
                        }
                        self.read_cache.insert(k, (std::time::Instant::now(), result.clone()));
                    }
                }
            }
            // Write-through invalidation: a non-read call may have changed the
            // state this upstream's cached reads reflect — bust them all, even
            // on an error result (a timeout can land after the write committed).
            // Single-proxy coherence only; external writers are what the TTL is for.
            if !is_read {
                self.bust_upstream_cache(idx);
            }
        }
        // Response shaping, on the payload before hints: the overlay's declarative
        // transform (remove/rename/set, then a jq escape hatch) runs ALWAYS —
        // even on errors, to strip secrets — and the caller's `fields` projection
        // narrows further, on success only (never hide an error payload).
        // Borrow the overlay's transform (no per-call clone); the immutable
        // config borrow ends before the &mut self logging below.
        // Gate the transform on `when` (success/error/always).
        let rt = self
            .config
            .overlay_for(tool_id)
            .map(|o| &o.response)
            .filter(|r| r.applies(is_error));
        let has_transform = rt.map(|r| !r.is_noop()).unwrap_or(false);
        let keep: &[String] = if is_error { &[] } else { fields };
        if has_transform || !keep.is_empty() {
            // provenance, not shape: only spec-backend results are envelopes
            let from_spec = matches!(self.upstreams[idx], crate::backend::Backend::Spec(_));
            transform_result(&mut result, from_spec, |payload| {
                if let Some(rt) = rt {
                    apply_response_transform(payload, rt, is_error);
                }
                if !keep.is_empty() {
                    *payload = crate::project::project(payload, keep);
                }
            });
        }
        // NOTE: no truncation here. dispatch() serves two audiences — the agent
        // (via Surface::call / cli_call) and internal machinery (workflow step
        // outputs, verify assertions) that must read the FULL result: truncating
        // here cut >8KB step results mid-JSON, every output path resolved to
        // None, and the next step received the literal '$steps.…' placeholder.
        // The agent-facing budget is applied at the boundary in Surface::call.
        self.apply_error_hints(tool_id, &mut result);
        // Nudge (law 6): a large result the caller didn't project is the teachable
        // moment to point at `fields`. Only in three_tool, where call_tool exists.
        if self.config.mode == Mode::ThreeTool && fields.is_empty() && !is_error {
            nudge_projection(&mut result);
        }
        // observability: which tool was called, where it routed, and whether it
        // errored (owned strings first so the upstream borrow ends before &mut).
        let (upstream, origin) = {
            let b = &self.upstreams[idx];
            (b.name().to_string(), b.origin(&original_name))
        };
        self.log_event(
            "call",
            json!({"tool_id": tool_id, "upstream": upstream, "origin": origin, "is_error": is_error, "cached": from_cache}),
        );
        Ok(result)
    }

    /// Follow a paginated list: from `first`, read the next cursor, re-call with it
    /// written into the request, and concatenate items — up to `max_pages`. Returns
    /// `first` with its item array replaced by the concatenation (and any `more`
    /// gate set false), so downstream shaping sees one complete result.
    #[allow(clippy::too_many_arguments)] // internal call-path plumbing, one caller
    async fn paginate(
        &mut self,
        idx: usize,
        name: &str,
        mut args: Value,
        headers: &[(String, String)],
        first: Value,
        p: &crate::config::Paginate,
        deadline: Option<std::time::Duration>,
    ) -> Result<(Value, bool)> {
        // Parse each page's payload exactly once and carry it forward.
        let from_spec = matches!(self.upstreams[idx], crate::backend::Backend::Spec(_));
        let mut payload = result_payload(&first, from_spec);
        let page_items = |pl: &Value| get_path(pl, &p.items).and_then(Value::as_array).cloned().unwrap_or_default();
        let mut items = page_items(&payload);
        let mut pages = 1usize;
        let mut prev_cursor: Option<Value> = None;
        let mut partial_error = false;
        while pages < p.max_pages {
            // gate on `more` if configured
            if let Some(mf) = &p.more {
                if get_path(&payload, mf).and_then(Value::as_bool) != Some(true) {
                    break;
                }
            }
            // next cursor; stop when absent/null/empty
            let cursor = match get_path(&payload, &p.cursor).cloned() {
                Some(c) if !c.is_null() && c.as_str() != Some("") => c,
                _ => break,
            };
            // Guard against a non-advancing cursor (misconfigured `into`, or an API
            // that ignores it): if the cursor didn't move, stop rather than re-fetch
            // the same page in a loop up to max_pages.
            if prev_cursor.as_ref() == Some(&cursor) {
                break;
            }
            prev_cursor = Some(cursor.clone());
            crate::project::set(&mut args, &p.into, cursor);
            // A follow-up failure — isError, timeout, or transport — must NOT
            // discard the pages already fetched. `?` here used to let a follow-up
            // TimeoutElapsed escape as a protocol error, throwing away pages 1..k
            // and skipping the PAGINATION notice; now every failure shape stops
            // pagination and surfaces on the merged (partial) result instead.
            let next = match self.upstreams[idx].call_tool(name, args.clone(), headers, deadline).await {
                Ok(n) => n,
                Err(_) => {
                    partial_error = true;
                    break;
                }
            };
            if next.get("isError").and_then(Value::as_bool).unwrap_or(false) {
                partial_error = true; // don't swallow it — flagged on the merged result
                break;
            }
            let next_payload = result_payload(&next, from_spec);
            let page = page_items(&next_payload);
            if page.is_empty() {
                break;
            }
            items.extend(page);
            payload = next_payload;
            pages += 1;
        }
        // Merge the accumulated items back into the first page's result.
        let count = items.len();
        let mut merged = first;
        let items_path = p.items.clone();
        let more_path = p.more.clone();
        transform_result(&mut merged, from_spec, |payload| {
            crate::project::set(payload, &items_path, Value::Array(items.clone()));
            if let Some(mf) = &more_path {
                crate::project::set(payload, mf, Value::Bool(false));
            }
        });
        // A mid-pagination upstream error is surfaced, not swallowed — the agent must
        // know the concatenated list may be incomplete.
        if partial_error {
            if let Some(blocks) = merged.get_mut("content").and_then(Value::as_array_mut) {
                blocks.push(json!({"type": "text", "text": format!(
                    "PAGINATION: stopped after {pages} page(s) on an upstream error or timeout; the list may be incomplete."
                )}));
            }
        }
        self.log_event("paginate", json!({"tool": name, "pages": pages, "items": count, "partial": partial_error}));
        Ok((merged, partial_error))
    }

    /// Errors are continuation prompts (law 6): append recovery instructions
    /// when a global or per-tool hint matches the result text.
    fn apply_error_hints(&self, tool_id: &str, result: &mut Value) {
        let text: String = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let matched: Vec<&crate::config::ErrorHint> = self
            .config
            .error_hints_for(tool_id)
            .filter(|h| text.contains(&h.contains))
            .collect();
        if matched.is_empty() {
            return;
        }
        // Any hint with a `field` pointer renders a structured error from the
        // patched schema (measured 0%→100% vs prose). Resolve it once.
        let schema = matched
            .iter()
            .any(|h| h.field.is_some())
            .then(|| self.visible_def(tool_id).map(|t| self.resolved_schema(t)))
            .flatten();
        if let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) {
            for h in matched {
                let text = h
                    .field
                    .as_ref()
                    .and_then(|fp| {
                        schema.as_ref().and_then(|s| {
                            structured_field_error_at(s, fp, "invalid_or_missing_field")
                        })
                    })
                    .map(|se| format!("ERROR_DETAIL: {se}"))
                    .unwrap_or_else(|| format!("HINT: {}", h.hint));
                blocks.push(json!({"type": "text", "text": text}));
                // Structured retry signal so the agent doesn't guess (429/5xx vs 4xx).
                if let Some(r) = h.retryable {
                    let why = if r {
                        "transient — wait briefly and retry"
                    } else {
                        "permanent — do not retry; fix the request"
                    };
                    blocks.push(json!({"type": "text", "text": format!("RETRYABLE: {r} ({why})")}));
                }
            }
        }
    }
}
