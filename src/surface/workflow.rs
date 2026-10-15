//! Composite workflows: linear step chains run as one tool call.

use super::*;

impl Surface {
    /// Run a composite: each step resolves its inputs from the workflow inputs +
    /// prior step outputs, calls its tool, and extracts named outputs. One
    /// upstream chain, one round-trip for the agent. A failed step aborts, and
    /// the failure says which earlier steps already took effect (see
    /// [`workflow_failure`]).
    pub(super) async fn execute_workflow(
        &self,
        caller: &Caller,
        wf: &crate::config::Workflow,
        inputs: Value,
    ) -> Result<Value> {
        let mut outs: HashMap<String, Value> = HashMap::new();
        let mut completed: Vec<Value> = Vec::new();
        for (i, step) in wf.steps.iter().enumerate() {
            let args = resolve_input(&step.input, &inputs, &outs);
            // A transport-level Err is folded into the same failure report: by
            // then earlier steps may have written, and the agent must hear which.
            let (error, outcome) = match self.dispatch(caller, &step.tool, args, &[]).await {
                Ok(r) if !r.get("isError").and_then(Value::as_bool).unwrap_or(false) => {
                    let payload = result_payload(&r, self.tool_from_spec(&step.tool));
                    let mut step_outs = serde_json::Map::new();
                    for (name, path) in &step.output {
                        if let Some(v) = get_path(&payload, path) {
                            outs.insert(format!("{}.{}", step.id, name), v.clone());
                            step_outs.insert(name.clone(), v.clone());
                        }
                    }
                    completed.push(json!({
                        "step": step.id, "tool": step.tool,
                        "effect": self.step_effect(&step.tool), "outputs": step_outs,
                    }));
                    continue;
                }
                Ok(r) => {
                    let text = result_text(&r).to_string();
                    let outcome = step_outcome(&text, self.tool_from_spec(&step.tool));
                    (text, outcome)
                }
                Err(e) => (format!("{e:#}"), "unknown"),
            };
            let remaining: Vec<Value> = wf.steps[i + 1..]
                .iter()
                .map(|s| json!({"step": s.id, "tool": s.tool}))
                .collect();
            self.log_event(
                caller,
                "workflow",
                json!({
                    "workflow": wf.id, "steps": wf.steps.len(), "failed_step": step.id,
                    "failed_step_outcome": outcome, "completed": completed.len(),
                }),
            );
            let report = workflow_failure(wf, step, &error, outcome, completed, remaining, self.step_effect(&step.tool));
            return Ok(text_result(format!("WORKFLOW_FAILED: {report}"), true));
        }
        let final_out: serde_json::Map<String, Value> = wf
            .outputs
            .iter()
            .map(|(name, expr)| (name.clone(), resolve_input(expr, &inputs, &outs)))
            .collect();
        self.log_event(caller, "workflow", json!({"workflow": wf.id, "steps": wf.steps.len()}));
        Ok(text_result(serde_json::to_string(&Value::Object(final_out)).unwrap_or_default(), false))
    }

    /// `read` only on the upstream's own read-only signal; anything else is
    /// assumed to write — the same convention the read cache uses.
    fn step_effect(&self, tool_id: &str) -> &'static str {
        match self.def_for(tool_id).and_then(|t| t.read_only) {
            Some(true) => "read",
            _ => "write",
        }
    }
}

/// What a failed step's error text says about whether it reached the upstream.
///
/// - `not_run`: refused locally before any upstream call (rate limit, preflight,
///   breaker, a missing injected value, an unknown or hidden tool).
/// - `unknown`: the call was sent and no answer came back (timeout, transport
///   failure) — it may have taken effect.
/// - `failed`: the upstream answered with an error.
///
/// Keyed on the prefixes dispatch itself writes, so an upstream error that
/// happens to contain one of these words mid-text is still `failed`. A spec
/// upstream (`from_spec`) reports its own transport failures inside the
/// envelope instead (`{"error": "TIMEOUT after 5s: …"}`, no `status`) — see
/// [`spec_outcome`]. An MCP upstream's text is its own, never read that way.
pub(super) fn step_outcome(error: &str, from_spec: bool) -> &'static str {
    const NOT_RUN: [&str; 5] =
        ["RATE_LIMITED:", "PREFLIGHT_ERROR:", "BREAKER_OPEN:", "USER_SUPPLIED_MISSING:", "unknown tool_id"];
    const UNKNOWN: [&str; 2] = ["TIMEOUT:", "UPSTREAM_ERROR:"];
    if NOT_RUN.iter().any(|p| error.starts_with(p)) {
        "not_run"
    } else if UNKNOWN.iter().any(|p| error.starts_with(p)) {
        "unknown"
    } else {
        from_spec.then(|| spec_outcome(error)).flatten().unwrap_or("failed")
    }
}

/// The spec executor's envelope: a `status` means the API answered; an
/// `error` without one means it never did — refused locally (a missing path
/// parameter, an unknown operation) or lost in transport (a timeout, a
/// connection error), which may have committed.
fn spec_outcome(error: &str) -> Option<&'static str> {
    if error.starts_with("unknown operation") {
        return Some("not_run");
    }
    let env: Value = serde_json::from_str(error).ok()?;
    if env.get("status").is_some() {
        return None;
    }
    let e = env.get("error")?.as_str()?;
    Some(if e.starts_with("missing path parameter") { "not_run" } else { "unknown" })
}

/// The structured failure of a composite: which step failed and how, which
/// steps already ran (with their effect and extracted outputs), which never
/// ran, and what to do next.
///
/// A composite is not a transaction. Before this, the agent got only
/// `failed at step "price"` and the natural move — re-run the composite —
/// repeats every write that already committed. The `fix` line is the point:
/// it says whether a re-run is safe, and when it is not, to finish the
/// remaining steps individually with the outputs already in hand.
pub(super) fn workflow_failure(
    wf: &crate::config::Workflow,
    step: &crate::config::Step,
    error: &str,
    outcome: &str,
    completed: Vec<Value>,
    remaining: Vec<Value>,
    failed_effect: &str,
) -> Value {
    let wrote: Vec<&str> = completed
        .iter()
        .filter(|c| c["effect"] == "write")
        .filter_map(|c| c["step"].as_str())
        .collect();
    let in_doubt = outcome == "unknown" && failed_effect == "write";
    let mut fix = if !wrote.is_empty() {
        format!(
            "Step(s) {wrote:?} already took effect upstream and are NOT rolled back. Do not re-run this \
             workflow — that repeats them. Fix the cause, then finish the failed and remaining steps \
             individually with call_tool, using the outputs above."
        )
    } else if in_doubt {
        "No earlier step wrote anything, but do not re-run this workflow until the failed step's state is known."
            .to_string()
    } else {
        "No earlier step wrote anything, so re-running this workflow after fixing the cause repeats no effect."
            .to_string()
    };
    if in_doubt {
        fix.push_str(&format!(
            " Step {:?} may or may not have completed; check state with a read before retrying it.",
            step.id
        ));
    }
    json!({
        "workflow": wf.id,
        "failed_step": {"step": step.id, "tool": step.tool, "outcome": outcome, "error": error},
        "completed": completed,
        "not_run": remaining,
        "fix": fix,
    })
}

/// Resolve a workflow input template: substitute `$inputs.<path>` and
/// `$steps.<stepId>.<name>` string refs; recurse into objects/arrays; pass
/// literals through unchanged. `steps` is keyed `"stepId.name"`.
pub(super) fn resolve_input(tmpl: &Value, inputs: &Value, steps: &HashMap<String, Value>) -> Value {
    match tmpl {
        Value::String(s) => resolve_ref(s, inputs, steps).unwrap_or_else(|| tmpl.clone()),
        Value::Object(m) => {
            Value::Object(m.iter().map(|(k, v)| (k.clone(), resolve_input(v, inputs, steps))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| resolve_input(v, inputs, steps)).collect()),
        other => other.clone(),
    }
}

pub(super) fn resolve_ref(s: &str, inputs: &Value, steps: &HashMap<String, Value>) -> Option<Value> {
    if let Some(rest) = s.strip_prefix("$inputs.") {
        // workflow inputs are optional: an omitted one resolves to null, which
        // form/JSON encoding then drops (so it isn't sent as a literal).
        return Some(get_path(inputs, rest).cloned().unwrap_or(Value::Null));
    }
    if let Some(rest) = s.strip_prefix("$steps.") {
        // a missing prior-step output is a workflow bug — keep the literal so it
        // is visibly wrong rather than silently null.
        return steps.get(rest).cloned();
    }
    None // not an expression -> the caller keeps the literal
}
