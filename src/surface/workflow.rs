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
        for (i, step) in wf.steps.iter().enumerate() {
            let args = resolve_input(&step.input, &inputs, &outs);
            // A transport-level Err is folded into the same failure report: by
            // then earlier steps may have written, and the agent must hear which.
            let (error, reach) = match self.dispatch_reach(caller, &step.tool, args, &[]).await {
                Ok((r, _)) if !r.get("isError").and_then(Value::as_bool).unwrap_or(false) => {
                    let payload = result_payload(&r, self.tool_from_spec(&step.tool));
                    for (name, path) in &step.output {
                        if let Some(v) = get_path(&payload, path) {
                            outs.insert(format!("{}.{}", step.id, name), v.clone());
                        }
                    }
                    continue;
                }
                Ok((r, reach)) => (result_text(&r).to_string(), reach),
                Err(e) => (format!("{e:#}"), crate::backend::Reach::NoAnswer),
            };
            let report = workflow_failure(wf, i, &error, reach, &outs, |tool| self.step_effect(tool));
            self.log_event(
                caller,
                "workflow",
                json!({
                    "workflow": wf.id, "steps": wf.steps.len(), "failed_step": step.id,
                    "failed_step_outcome": reach.outcome(), "completed": i,
                }),
            );
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
    /// assumed to write. Stricter than the read cache, which also treats an
    /// overlay `cacheable: true` as a read: a re-run verdict must not lean on
    /// an operator's caching opt-in.
    fn step_effect(&self, tool_id: &str) -> &'static str {
        match self.def_for(tool_id).and_then(|t| t.read_only) {
            Some(true) => "read",
            _ => "write",
        }
    }
}

/// Room for the failed step's error text inside the failure report.
const WORKFLOW_ERROR_BUDGET: usize = 2_000;

/// The structured failure of a composite whose step `failed` did not succeed:
/// how it failed (`not_run` refused locally, `failed` the upstream answered
/// with an error, `unknown` sent with no answer so it may have taken effect),
/// which steps already ran (with their effect and extracted outputs, from
/// `outs`), which never ran, and what to do next. `effect` says whether a
/// tool reads or writes.
///
/// A composite is not a transaction. Before this, the agent got only
/// `failed at step "price"` and the natural move — re-run the composite —
/// repeats every write that already committed. The `fix` line is the point:
/// it says whether a re-run is safe, and when it is not, to finish the
/// remaining steps individually with the outputs already in hand.
pub(super) fn workflow_failure(
    wf: &crate::config::Workflow,
    failed: usize,
    error: &str,
    reach: crate::backend::Reach,
    outs: &HashMap<String, Value>,
    effect: impl Fn(&str) -> &'static str,
) -> Value {
    let step = &wf.steps[failed];
    let mut wrote: Vec<&str> = Vec::new();
    let completed: Vec<Value> = wf.steps[..failed]
        .iter()
        .map(|s| {
            let e = effect(&s.tool);
            if e == "write" {
                wrote.push(&s.id);
            }
            let outputs: serde_json::Map<String, Value> = s
                .output
                .keys()
                .filter_map(|name| Some((name.clone(), outs.get(&format!("{}.{}", s.id, name))?.clone())))
                .collect();
            json!({"step": s.id, "tool": s.tool, "effect": e, "outputs": outputs})
        })
        .collect();
    let in_doubt = reach == crate::backend::Reach::NoAnswer && effect(&step.tool) == "write";
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
    // `fix` first and the error capped: the agent boundary truncates the
    // whole report to AGENT_RESULT_BUDGET, and an upstream's error body (an
    // HTML 502 page) must not push the one line that matters off the end.
    let error = budget_truncate(error.to_string(), WORKFLOW_ERROR_BUDGET, "…[truncated]");
    let not_run: Vec<Value> =
        wf.steps[failed + 1..].iter().map(|s| json!({"step": s.id, "tool": s.tool})).collect();
    json!({
        "workflow": wf.id,
        "fix": fix,
        "failed_step": {"step": step.id, "tool": step.tool, "outcome": reach.outcome(), "error": error},
        "completed": completed,
        "not_run": not_run,
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
