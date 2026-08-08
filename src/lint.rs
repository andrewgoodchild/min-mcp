//! A static quality/usability linter for tool definitions, grounded in published
//! best practices (Anthropic's tool-writing guidance; MCP conventions; the "From
//! REST to MCP" study, arXiv:2507.16044). It reads only the definition, so it
//! catches static *smells* — NOT runtime breakage (undocumented-required params,
//! opaque errors, bloated responses), which are invisible until a call is made
//! and need telemetry. Findings mark where an overlay *might* help; the linter
//! applies nothing.
//!
//! This is the quality/usability linter (drafts fixes for the agent surface) —
//! distinct from the tool-poisoning/security linter cut earlier (ceded to
//! dedicated security tools).

use serde_json::Value;

/// (rule id, one-line rationale). Kept as data so the CLI can print a legend and
/// the aggregate report can enumerate every rule even when its count is zero.
///
/// Each rule is tagged with its grounding. The description rules map to the
/// six-component tool-description taxonomy (Purpose / Guidelines / Limitations /
/// Parameter Explanation / Length / Examples — arXiv:2602.18914); the schema
/// rules map to a schema-quality axis. NB the highest-value
/// components — Guidelines, Limitations, and example *quality* — are NOT
/// statically checkable (they need reading intent / a task eval), which is
/// exactly why a static linter is a weak prior and the real play is fix+verify.
pub const RULES: &[(&str, &str)] = &[
    ("no_description", "Purpose: no description at all — agents can't tell what the tool does"),
    ("thin_description", "Purpose/Length: description only restates the signature (no behaviour)"),
    ("confusable_descriptions", "Ambiguity: shares a summary AND HTTP method with another tool — the agent can't disambiguate"),
    ("params_undescribed", "Parameter Explanation: majority of parameters carry no description"),
    ("untyped_param", "Parameter Explanation: a parameter declares no type/schema (opaque blob)"),
    ("mutating_no_required", "Schema safety: a write/delete op with no required parameters"),
    ("many_required", "Schema fillability: >8 required parameters — hard to call correctly"),
    ("deep_schema", "Schema fillability: nests deeper than 6 levels — hard for an agent to fill"),
];

const REQUIRED_MAX: usize = 8;
const DEEP_MAX: usize = 6;

/// Lint one tool from its (name, description, resolved input schema). `mutating`
/// says whether the tool has a side effect (a write/delete), used by the schema-
/// safety rule; it's determined reliably from the HTTP method for spec tools (see
/// [`is_mutating`]). Returns the ids of the rules that fired.
///
/// The cross-tool `confusable_descriptions` rule can't be decided from one tool,
/// so it is injected by the caller ([`Surface::lint_report`]) after a whole-surface
/// pass; see [`confusable_key`].
pub fn lint(_name: &str, description: &str, schema: &Value, mutating: bool) -> Vec<&'static str> {
    let mut fired = Vec::new();

    // --- description rules ---
    if description.trim().is_empty() {
        fired.push("no_description");
    } else if human_desc(description).split_whitespace().count() < 2 {
        // Thin = no real behavioural phrase: empty, or a single word/fragment left
        // once the mechanical "OpId: METHOD /path —" signature is stripped. A clear
        // short summary like "List all customers" is NOT thin — flagging brevity was
        // over-aggressive (flagged 54% of well-documented Stripe at 0% task failure;
        // see the fix-loop benchmark).
        fired.push("thin_description");
    }

    // --- schema rules (one recursive pass) ---
    let mut w = Walk::default();
    walk(schema, 0, &mut w);
    if w.param_total > 0 && w.param_undescribed * 2 > w.param_total {
        fired.push("params_undescribed");
    }
    if w.untyped {
        fired.push("untyped_param");
    }
    // A write/delete that requires nothing is under-specified: the agent can't tell
    // what it must supply, and can fire it with empty args (the schema-safety
    // check, and the exact case the GAP-1 `required:` overlay patch is meant to fix).
    if mutating && w.param_total > 0 && w.max_required == 0 {
        fired.push("mutating_no_required");
    }
    if w.max_required > REQUIRED_MAX {
        fired.push("many_required");
    }
    if w.max_depth > DEEP_MAX {
        fired.push("deep_schema");
    }
    fired
}

/// The human-readable part of a description. Spec-derived descriptions read
/// "OpId: GET /path — <summary>"; the signature before the em dash is mechanical,
/// so the summary after it is what an agent actually reads (and what search shows).
pub fn human_desc(description: &str) -> &str {
    let d = description.trim();
    d.rsplit_once('—').map(|(_, r)| r.trim()).unwrap_or(d)
}

/// Does this tool have a side effect? Reliable for spec tools (the origin is
/// "METHOD /path", so the HTTP method decides). For MCP tools the origin is just
/// the tool name with no method, and we don't yet capture `annotations`
/// (destructiveHint) — so we return false rather than guess from the name.
pub fn is_mutating(origin: &str) -> bool {
    matches!(http_method(origin), Some("POST" | "PUT" | "PATCH" | "DELETE"))
}

/// The HTTP method of a spec tool's origin ("GET /path" -> "GET"), or None for an
/// MCP tool (whose origin is a bare name). Used to keep CRUD siblings — same
/// summary, different verb — out of the same `confusable_descriptions` group.
pub fn http_method(origin: &str) -> Option<&'static str> {
    match origin.split_whitespace().next().unwrap_or("").to_ascii_uppercase().as_str() {
        "GET" => Some("GET"),
        "POST" => Some("POST"),
        "PUT" => Some("PUT"),
        "PATCH" => Some("PATCH"),
        "DELETE" => Some("DELETE"),
        "HEAD" => Some("HEAD"),
        "OPTIONS" => Some("OPTIONS"),
        _ => None,
    }
}

/// A normalized key for cross-tool confusability: the human summary, lowercased
/// and whitespace-collapsed. `None` for summaries too short to be a meaningful
/// collision (an empty/thin one is already caught by no_/thin_description).
pub fn confusable_key(description: &str) -> Option<String> {
    let human = human_desc(description).to_lowercase();
    let key: String = human.split_whitespace().collect::<Vec<_>>().join(" ");
    (key.chars().count() >= 12).then_some(key)
}

#[derive(Default)]
struct Walk {
    param_total: usize,
    param_undescribed: usize,
    untyped: bool,
    max_required: usize,
    max_depth: usize,
}

/// Walk a resolved JSON-Schema, tallying param-doc coverage, opaque/enum params,
/// required-count, and nesting depth. Every entry of a `properties` map counts as
/// one "parameter" (this naturally descends through the spec envelope's
/// path_params/query_params/body, which are described, into the real fields).
fn walk(schema: &Value, depth: usize, w: &mut Walk) {
    let Some(obj) = schema.as_object() else { return };
    w.max_depth = w.max_depth.max(depth);

    if let Some(req) = obj.get("required").and_then(Value::as_array) {
        w.max_required = w.max_required.max(req.len());
    }

    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (_pname, p) in props {
            w.param_total += 1;
            let described = p
                .get("description")
                .and_then(Value::as_str)
                .map(|d| !d.trim().is_empty())
                .unwrap_or(false);
            if !described {
                w.param_undescribed += 1;
            }
            if is_opaque(p) {
                w.untyped = true;
            }
            walk(p, depth + 1, w);
        }
    }
    // array element schemas add depth but aren't named params
    if let Some(items) = obj.get("items") {
        walk(items, depth + 1, w);
    }
}

/// A parameter schema with no type information at all — no `type`, no nested
/// shape, no `$ref`/cut-cycle, no enum, no composition. The agent gets a blank.
fn is_opaque(p: &Value) -> bool {
    const SIGNALS: [&str; 8] =
        ["type", "properties", "items", "enum", "$ref", "$circular", "allOf", "oneOf"];
    let Some(o) = p.as_object() else { return false };
    !SIGNALS.iter().any(|k| o.contains_key(*k)) && p.get("anyOf").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flags_thin_description_and_undescribed_params() {
        // spec-style description with an empty human tail -> thin; and a strict
        // majority of params undescribed (query_params described, but both real
        // fields bare -> 2 of 3 undescribed) -> params_undescribed.
        let f = lint(
            "GetItem",
            "GetItem: GET /items/{id} — ",
            &json!({"type":"object","properties":{
                "query_params":{"type":"object","description":"query values","properties":{
                    "id":{"type":"string"},      // no description
                    "verbose":{"type":"boolean"} // no description
                }}
            }}),
            false,
        );
        assert!(f.contains(&"thin_description"), "empty human tail is thin");
        assert!(f.contains(&"params_undescribed"), "2 of 3 params undescribed is a majority: {f:?}");

        // A clear short summary is NOT thin (the over-aggression fix): "List all
        // customers" describes behaviour even though it's under the old char cap.
        let clear = lint(
            "GetCustomers",
            "GetCustomers: GET /v1/customers — List all customers",
            &json!({"type":"object"}),
            false,
        );
        assert!(!clear.contains(&"thin_description"), "a real phrase is not thin: {clear:?}");
        // a single word left after the signature IS thin
        let word = lint("Op", "Op: GET /x — Retrieve", &json!({"type":"object"}), false);
        assert!(word.contains(&"thin_description"));
    }

    #[test]
    fn clean_tool_fires_nothing() {
        let f = lint(
            "CreateWidget",
            "Create a widget with the given name and colour.",
            &json!({"type":"object","required":["name"],"properties":{
                "name":{"type":"string","description":"the widget's display name"},
                "colour":{"type":"string","description":"hex colour","enum":["#f00","#0f0"]}
            }}),
            true, // mutating, but it has a required field -> no mutating_no_required
        );
        assert!(f.is_empty(), "well-documented tool should be clean, got {f:?}");
    }

    #[test]
    fn flags_enum_untyped_required_and_depth() {
        let mut props = serde_json::Map::new();
        for i in 0..9 {
            props.insert(format!("f{i}"), json!({"type":"string","description":"x"}));
        }
        let required: Vec<String> = (0..9).map(|i| format!("f{i}")).collect();
        let f = lint(
            "Op",
            "A tool that does a thing in detail.",
            &json!({"type":"object","required":required,"properties":props}),
            false,
        );
        assert!(f.contains(&"many_required"));

        // an opaque (typeless) param is flagged
        let g = lint("Op", "Does a thing thoroughly here.", &json!({"type":"object","properties":{
            "blob":{}                                    // opaque, no type/schema
        }}), false);
        assert!(g.contains(&"untyped_param"));
    }

    #[test]
    fn flags_mutating_op_with_nothing_required() {
        // a POST whose only field is optional -> under-specified write
        let schema = json!({"type":"object","properties":{
            "body":{"type":"object","description":"the payload","properties":{
                "note":{"type":"string","description":"an optional note"}
            }}
        }});
        assert!(lint("Op", "Create a thing with an optional note.", &schema, true)
            .contains(&"mutating_no_required"));
        // same tool, read-only -> not flagged
        assert!(!lint("Op", "Create a thing with an optional note.", &schema, false)
            .contains(&"mutating_no_required"));
    }

    #[test]
    fn is_mutating_reads_the_http_method_only() {
        assert!(is_mutating("POST /v1/customers"));
        assert!(is_mutating("delete /items/{id}"));
        assert!(!is_mutating("GET /items"));
        assert!(!is_mutating("echo")); // bare MCP name -> not guessed
    }

    #[test]
    fn confusable_key_normalizes_the_summary_and_skips_short() {
        // spec-style: the mechanical prefix is dropped, summary normalized
        assert_eq!(
            confusable_key("users.GetMessages: GET /users/{id}/messages — Get messages from users"),
            Some("get messages from users".to_string())
        );
        // two tools with the same summary produce the same key (agent can't disambiguate)
        assert_eq!(
            confusable_key("a.X: GET /a — Get messages from users"),
            confusable_key("b.Y: GET /b — Get messages from users")
        );
        // too-short / empty summaries are ignored (already caught by thin/no_description)
        assert_eq!(confusable_key("a.X: GET /a — "), None);
        assert_eq!(confusable_key("Op: GET /x — short"), None);
    }
}
