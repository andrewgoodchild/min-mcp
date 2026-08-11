//! Composite workflows: linear step chains run as one tool call.

use super::*;

impl Surface {
    /// Run a composite: each step resolves its inputs from the workflow inputs +
    /// prior step outputs, calls its tool, and extracts named outputs. One
    /// upstream chain, one round-trip for the agent. A failed step aborts.
    pub(super) async fn execute_workflow(&mut self, wf: &crate::config::Workflow, inputs: Value) -> Result<Value> {
        let mut outs: HashMap<String, Value> = HashMap::new();
        for step in &wf.steps {
            let args = resolve_input(&step.input, &inputs, &outs);
            let result = self.dispatch(&step.tool, args, &[]).await?;
            if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
                let text = result_text(&result);
                return Ok(text_result(
                    format!("workflow {:?} failed at step {:?}: {text}", wf.id, step.id),
                    true,
                ));
            }
            let payload = result_payload(&result, self.tool_from_spec(&step.tool));
            for (name, path) in &step.output {
                if let Some(v) = get_path(&payload, path) {
                    outs.insert(format!("{}.{}", step.id, name), v.clone());
                }
            }
        }
        let final_out: serde_json::Map<String, Value> = wf
            .outputs
            .iter()
            .map(|(name, expr)| (name.clone(), resolve_input(expr, &inputs, &outs)))
            .collect();
        self.log_event("workflow", json!({"workflow": wf.id, "steps": wf.steps.len()}));
        Ok(text_result(serde_json::to_string(&Value::Object(final_out)).unwrap_or_default(), false))
    }
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
