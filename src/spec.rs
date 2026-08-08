//! Load an OpenAPI spec (3.x or Swagger 2.0) and expose its operations in a
//! uniform shape, with $ref resolution. Ported from the Python spec_loader
//! (a Python prototype), whose logic is unit-tested there.
//!
//! This is the "API spec -> tools" half of the converter: it turns a spec into
//! ToolDef-shaped operations. The exec module is the other half (calling them).

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::{json, Value};

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];
/// Cap on a $ref expansion chain (ref -> schema -> ref -> ...). Structural
/// nesting does not count; cycles are cut via the seen-set.
const MAX_REF_CHAIN: usize = 12;

#[derive(Debug, Clone)]
pub struct Operation {
    pub op_id: String,
    pub method: String,
    pub path: String,
    pub summary: String,
    pub description: String,
    pub raw: Value,
}

impl Operation {
    pub fn one_line(&self) -> String {
        let text = if !self.summary.is_empty() {
            self.summary.clone()
        } else {
            self.description.lines().next().unwrap_or("").chars().take(120).collect()
        };
        format!("{}: {} {} — {text}", self.op_id, self.method.to_uppercase(), self.path)
    }
}

/// How one query parameter's value is serialized onto the wire. Arrays are the
/// only interesting case — scalars are always `key=value` — and different specs
/// disagree: `Repeated` is the OpenAPI 3 default (`form`+`explode`, e.g. GitHub);
/// `Deep` is bracket notation (`deepObject`, e.g. Stripe's `expand[0]=`);
/// `Delimited` joins with a separator (`form`+`explode:false`, or Swagger 2.0
/// `collectionFormat` csv/ssv/tsv/pipes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryStyle {
    Repeated,
    Delimited(char),
    Deep,
}

pub struct Spec {
    // spec metadata, surfaced by `minmcp inspect` diagnostics
    #[allow(dead_code)]
    pub title: String,
    #[allow(dead_code)]
    pub version: String,
    /// Swagger 2.0 (`swagger: "2.0"`) vs OpenAPI 3.x — they differ on the default
    /// array-query serialization (2.0 defaults to csv; 3.x to repeated keys).
    swagger2: bool,
    root: Value,
    pub operations: Vec<Operation>,
    by_id: HashMap<String, usize>,
}

impl Spec {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read spec {path}"))?;
        let root: Value = if path.ends_with(".yaml") || path.ends_with(".yml") {
            serde_yaml::from_str(&text).context("parse spec yaml")?
        } else {
            serde_json::from_str(&text).context("parse spec json")?
        };
        Self::from_value(root)
    }

    pub fn from_value(root: Value) -> Result<Self> {
        let info = root.get("info").cloned().unwrap_or(json!({}));
        let title = info.get("title").and_then(Value::as_str).unwrap_or("?").to_string();
        let version = match info.get("version") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => "?".to_string(),
        };

        let mut operations = Vec::new();
        if let Some(paths) = root.get("paths").and_then(Value::as_object) {
            for (path, item) in paths {
                let Some(item) = item.as_object() else { continue };
                for (method, op) in item {
                    let m = method.to_lowercase();
                    if !HTTP_METHODS.contains(&m.as_str()) || !op.is_object() {
                        continue;
                    }
                    operations.push(Operation {
                        op_id: make_op_id(op, &m, path),
                        method: m,
                        path: path.clone(),
                        summary: op.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
                        description: op
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        raw: op.clone(),
                    });
                }
            }
        }
        // Guarantee unique op_ids so every operation stays reachable and dispatch
        // is unambiguous. Messy real specs (min-mcp's target) ship duplicate
        // operationIds, or omit them so two paths generate the same slug; without
        // this the by_id map would silently drop all but the last, and the surface
        // would list two tools sharing one id. Collisions get a numeric suffix,
        // re-checked so a bumped id can't land on an existing one.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for op in &mut operations {
            if seen.contains(&op.op_id) {
                let base = op.op_id.clone();
                let mut i = 2;
                while seen.contains(&format!("{base}_{i}")) {
                    i += 1;
                }
                op.op_id = format!("{base}_{i}");
            }
            seen.insert(op.op_id.clone());
        }
        let by_id = operations.iter().enumerate().map(|(i, o)| (o.op_id.clone(), i)).collect();
        let swagger2 = root.get("swagger").is_some();
        Ok(Spec { title, version, swagger2, root, operations, by_id })
    }

    pub fn get(&self, op_id: &str) -> Option<&Operation> {
        self.by_id.get(op_id).map(|&i| &self.operations[i])
    }

    /// Inline $refs, cutting cycles. `depth` counts ref expansions only.
    pub fn resolve(&self, node: &Value, depth: usize, seen: &[String]) -> Value {
        match node {
            Value::Object(map) => {
                if let Some(Value::String(r)) = map.get("$ref") {
                    if seen.iter().any(|s| s == r) {
                        return json!({ "$circular": r });
                    }
                    if !r.starts_with("#/") || depth >= MAX_REF_CHAIN {
                        return node.clone();
                    }
                    let mut target = &self.root;
                    for part in r[2..].split('/') {
                        match target.get(part) {
                            Some(t) => target = t,
                            None => return node.clone(), // dangling
                        }
                    }
                    let mut next = seen.to_vec();
                    next.push(r.clone());
                    return self.resolve(target, depth + 1, &next);
                }
                Value::Object(
                    map.iter().map(|(k, v)| (k.clone(), self.resolve(v, depth, seen))).collect(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(|v| self.resolve(v, depth, seen)).collect()),
            other => other.clone(),
        }
    }

    /// The request-body schema (the JSON the caller's `body` fills), or null.
    /// `resolve` expands `$ref`s inline (get_tool_details); skipping it keeps load
    /// O(op) for huge specs — refs are expanded lazily on demand.
    pub fn body_schema(&self, op: &Operation, resolve: bool) -> Value {
        let rb = op.raw.get("requestBody").cloned().unwrap_or(Value::Null);
        // `resolve` expands $refs inline (needed for get_tool_details); skipping it
        // keeps load O(op) for huge specs — refs are expanded lazily on demand.
        let rb = if resolve { self.resolve(&rb, 0, &[]) } else { rb };
        // pick application/json first, else the single declared media type
        let content = rb.get("content").and_then(Value::as_object);
        let Some(content) = content else { return Value::Null };
        let media = content
            .get("application/json")
            .or_else(|| content.values().next())
            .cloned()
            .unwrap_or(Value::Null);
        media.get("schema").cloned().unwrap_or(Value::Null)
    }

    /// The media type an operation's request body is sent as (drives encoding).
    pub fn body_media_type(&self, op: &Operation) -> Option<String> {
        op.raw
            .get("requestBody")?
            .get("content")?
            .as_object()?
            .keys()
            .next()
            .cloned()
    }

    /// Per-query-parameter wire serialization, resolved from each parameter's
    /// declared `style`/`explode` (OpenAPI 3) or `collectionFormat` (Swagger 2.0).
    /// This is what makes array query params correct across APIs: GitHub declares
    /// nothing, so its arrays take the OpenAPI default (`form`+`explode` → repeated
    /// keys), while Stripe declares `deepObject` (bracketed). Serializing every
    /// array the same way is wrong for one of them.
    pub fn query_styles(&self, op: &Operation) -> HashMap<String, QueryStyle> {
        let raw = op.raw.get("parameters").cloned().unwrap_or(json!([]));
        let params = self.resolve(&raw, 0, &[]);
        let empty = Vec::new();
        params
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .filter(|p| p.get("in").and_then(Value::as_str) == Some("query"))
            .filter_map(|p| {
                let name = p.get("name").and_then(Value::as_str)?;
                Some((name.to_string(), self.query_style_of(p)))
            })
            .collect()
    }

    /// The serialization to assume for a query parameter the spec doesn't declare
    /// (an agent-supplied key not in `parameters`): the version's array default —
    /// csv for Swagger 2.0, repeated keys for OpenAPI 3.
    pub fn default_query_style(&self) -> QueryStyle {
        if self.swagger2 {
            QueryStyle::Delimited(',')
        } else {
            QueryStyle::Repeated
        }
    }

    fn query_style_of(&self, p: &Value) -> QueryStyle {
        // Swagger 2.0: collectionFormat (has no style/explode).
        if let Some(cf) = p.get("collectionFormat").and_then(Value::as_str) {
            return match cf {
                "multi" => QueryStyle::Repeated,
                "ssv" => QueryStyle::Delimited(' '),
                "tsv" => QueryStyle::Delimited('\t'),
                "pipes" => QueryStyle::Delimited('|'),
                _ => QueryStyle::Delimited(','), // csv — the Swagger 2.0 default
            };
        }
        // OpenAPI 3: style + explode (explode defaults to true only for `form`).
        match p.get("style").and_then(Value::as_str) {
            Some("deepObject") => QueryStyle::Deep,
            Some("spaceDelimited") => delimited_or_repeated(p, ' '),
            Some("pipeDelimited") => delimited_or_repeated(p, '|'),
            Some("form") => {
                if param_explode(p, true) {
                    QueryStyle::Repeated
                } else {
                    QueryStyle::Delimited(',')
                }
            }
            // No style declared (or an unknown one) → the version default.
            _ => self.default_query_style(),
        }
    }

    /// A tool input schema describing the {path_params, query_params, body}
    /// envelope the caller uses. path/query params are surfaced as real named
    /// properties (with their spec descriptions), and the body schema is resolved
    /// inline. Handles BOTH OpenAPI 3.0 (`requestBody`) and Swagger 2.0 (`in: body`
    /// / `in: formData` parameters). This is what get_tool_details shows for a
    /// spec-backed tool.
    pub fn tool_input_schema(&self, op: &Operation) -> Value {
        self.build_input_schema(op, true)
    }

    /// A cheap input schema for LOAD time: the same envelope, but `$ref`s are left
    /// unexpanded. Resolving a shared schema once per op (× thousands of ops) is
    /// what made a 11k-op spec (Microsoft Graph) unloadable; this defers it so a
    /// single tool is resolved only when `get_tool_details` asks for it.
    pub fn tool_input_schema_shallow(&self, op: &Operation) -> Value {
        self.build_input_schema(op, false)
    }

    fn build_input_schema(&self, op: &Operation, resolve: bool) -> Value {
        let raw_params = op.raw.get("parameters").cloned().unwrap_or(json!([]));
        let params = if resolve { self.resolve(&raw_params, 0, &[]) } else { raw_params };
        let empty = Vec::new();
        let params = params.as_array().unwrap_or(&empty);

        let mut path_props = serde_json::Map::new();
        let mut path_req: Vec<String> = Vec::new();
        let mut query_props = serde_json::Map::new();
        let mut query_req: Vec<String> = Vec::new();
        let mut form_props = serde_json::Map::new();
        let mut body = self.body_schema(op, resolve); // OpenAPI 3.0 requestBody

        for p in params {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            if name.is_empty() {
                continue; // an unresolved $ref-param (shallow mode) — surfaced when resolved
            }
            let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
            match p.get("in").and_then(Value::as_str).unwrap_or("") {
                "path" => {
                    path_props.insert(name.clone(), param_prop(p));
                    if required {
                        path_req.push(name);
                    }
                }
                "query" => {
                    query_props.insert(name.clone(), param_prop(p));
                    if required {
                        query_req.push(name);
                    }
                }
                // Swagger 2.0 body/form (OpenAPI 3.0 uses requestBody, handled above)
                "body" => {
                    let s = p.get("schema").cloned().unwrap_or(Value::Null);
                    body = if resolve { self.resolve(&s, 0, &[]) } else { s };
                }
                "formData" => {
                    form_props.insert(name, param_prop(p));
                }
                _ => {} // header / cookie params are set via config, not agent args
            }
        }
        if body.is_null() && !form_props.is_empty() {
            body = json!({"type": "object", "properties": Value::Object(form_props)});
        }

        json!({
            "type": "object",
            "description": format!("{} {} — call via {{path_params, query_params, body}}.", op.method.to_uppercase(), op.path),
            "properties": {
                "path_params": param_object(path_props, path_req, "Values for {placeholders} in the path."),
                "query_params": param_object(query_props, query_req, "Query-string parameters."),
                "body": body,
            }
        })
    }
}

fn param_explode(p: &Value, default: bool) -> bool {
    p.get("explode").and_then(Value::as_bool).unwrap_or(default)
}

/// `spaceDelimited`/`pipeDelimited` join with their separator unless `explode`
/// is set, in which case they degrade to repeated keys (per the OpenAPI table).
fn delimited_or_repeated(p: &Value, sep: char) -> QueryStyle {
    if param_explode(p, false) {
        QueryStyle::Repeated
    } else {
        QueryStyle::Delimited(sep)
    }
}

/// One parameter's JSON-Schema. OpenAPI 3.0 nests the type in `schema`; Swagger 2.0
/// puts type/format/items/enum inline. Carries the `description` either way.
fn param_prop(p: &Value) -> Value {
    let mut prop = p.get("schema").cloned().unwrap_or_else(|| {
        let mut m = serde_json::Map::new();
        for k in ["type", "format", "items", "enum"] {
            if let Some(v) = p.get(k) {
                m.insert(k.to_string(), v.clone());
            }
        }
        Value::Object(m)
    });
    if let (Some(o), Some(d)) = (prop.as_object_mut(), p.get("description")) {
        o.entry("description".to_string()).or_insert_with(|| d.clone());
    }
    prop
}

/// Assemble a path_params/query_params object schema, omitting empty properties
/// (so a tool with no query params still reads cleanly).
fn param_object(props: serde_json::Map<String, Value>, required: Vec<String>, desc: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), json!("object"));
    m.insert("description".into(), json!(desc));
    if !props.is_empty() {
        m.insert("properties".into(), Value::Object(props));
    }
    if !required.is_empty() {
        m.insert("required".into(), json!(required));
    }
    Value::Object(m)
}

fn make_op_id(op: &Value, method: &str, path: &str) -> String {
    if let Some(id) = op.get("operationId").and_then(Value::as_str) {
        return id.to_string();
    }
    // match the Python: strip braces, path separators become underscores
    let slug: String = path.trim_matches('/').replace('/', "_").replace(['{', '}'], "");
    format!("{method}_{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mini() -> Spec {
        Spec::from_value(json!({
            "info": {"title": "Mini", "version": "1.0"},
            "paths": {
                "/widgets": {
                    "get": {"operationId": "ListWidgets", "summary": "List widgets"},
                    "post": {
                        "operationId": "CreateWidget", "summary": "Create a widget",
                        "requestBody": {"content": {"application/json": {"schema":
                            {"$ref": "#/components/schemas/Widget"}}}}
                    }
                },
                "/widgets/{id}": {"get": {"summary": "Get one"}}
            },
            "components": {"schemas": {"Widget": {"type": "object", "properties": {
                "name": {"type": "string"},
                "parent": {"$ref": "#/components/schemas/Widget"}
            }}}}
        })).unwrap()
    }

    #[test]
    fn enumerates_and_generates_op_ids() {
        let s = mini();
        assert_eq!(s.operations.len(), 3);
        assert!(s.get("ListWidgets").is_some());
        assert!(s.get("get_widgets_id").is_some()); // generated
    }

    #[test]
    fn shallow_schema_defers_ref_expansion() {
        // Lazy resolution: the load-time (shallow) schema keeps `$ref`s unexpanded
        // (cheap — this is what lets an 11k-op spec load instantly); the on-demand
        // (full) schema expands them. Same op, two costs.
        let s = mini(); // CreateWidget body -> $ref #/components/schemas/Widget
        let op = s.get("CreateWidget").unwrap();
        let shallow = s.tool_input_schema_shallow(op);
        let full = s.tool_input_schema(op);
        assert!(shallow["properties"]["body"].get("$ref").is_some(), "shallow keeps the $ref (deferred)");
        assert_eq!(full["properties"]["body"]["properties"]["name"]["type"], json!("string"), "resolved expands it");
    }

    #[test]
    fn input_schema_surfaces_params_and_handles_swagger2_body() {
        // Swagger 2.0 shapes: params carry type inline; the body is an `in: body`
        // parameter (not requestBody), and refs are #/definitions/*.
        let s = Spec::from_value(json!({
            "info": {"title": "T", "version": "1"},
            "paths": {
                "/pet": {"post": {"operationId": "addPet", "parameters": [
                    {"name": "verbose", "in": "query", "type": "boolean", "description": "chatty"},
                    {"name": "body", "in": "body", "schema": {"$ref": "#/definitions/Pet"}}
                ]}},
                "/pet/{petId}": {"get": {"operationId": "getPet", "parameters": [
                    {"name": "petId", "in": "path", "required": true, "type": "integer", "description": "the id"}
                ]}}
            },
            "definitions": {"Pet": {"type": "object", "properties": {"name": {"type": "string"}}}}
        })).unwrap();

        let add = s.tool_input_schema(s.get("addPet").unwrap());
        let p = &add["properties"];
        // BUG 1 fixed: no internal metadata leaks to the agent
        assert!(p.get("_openapi_parameters").is_none());
        // BUG 2 fixed: Swagger 2.0 `in: body` resolves into `body`
        assert_eq!(p["body"]["properties"]["name"]["type"], json!("string"));
        // query param surfaces as a named property with its inline type + description
        assert_eq!(p["query_params"]["properties"]["verbose"]["type"], json!("boolean"));
        assert_eq!(p["query_params"]["properties"]["verbose"]["description"], json!("chatty"));

        // path param: named, typed, and marked required
        let get = s.tool_input_schema(s.get("getPet").unwrap());
        let pp = &get["properties"]["path_params"];
        assert_eq!(pp["properties"]["petId"]["type"], json!("integer"));
        assert_eq!(pp["required"], json!(["petId"]));
    }

    #[test]
    fn duplicate_op_ids_are_disambiguated() {
        // Duplicate operationIds (a real hazard in hand-written specs) must leave
        // every operation reachable, not collapse to the last one. A pre-existing
        // suffixed id must not be clobbered either.
        let s = Spec::from_value(json!({
            "info": {"title": "T", "version": "1"},
            "paths": {
                "/a": {"get": {"operationId": "dup"}},
                "/b": {"get": {"operationId": "dup"}},
                "/c": {"get": {"operationId": "dup_2"}}
            }
        })).unwrap();
        assert_eq!(s.operations.len(), 3);
        let ids: std::collections::HashSet<_> = s.operations.iter().map(|o| o.op_id.clone()).collect();
        assert_eq!(ids.len(), 3, "all three op_ids distinct");
        // b's "dup" bumps to "dup_2"; c already *is* "dup_2", so it bumps to
        // "dup_2_2" — the suffix re-checks so a bump never lands on a live id.
        assert_eq!(ids, ["dup", "dup_2", "dup_2_2"].iter().map(|s| s.to_string()).collect());
        // and each is independently resolvable through the by_id map
        for id in ["dup", "dup_2", "dup_2_2"] {
            assert!(s.get(id).is_some(), "{id} resolvable");
        }
    }

    #[test]
    fn query_styles_openapi3_defaults_and_declared() {
        // OpenAPI 3: an undeclared-style array param takes the form+explode
        // default (Repeated); deepObject -> Deep; form+explode:false -> csv.
        let s = Spec::from_value(json!({
            "openapi": "3.0.0", "info": {"title": "T", "version": "1"},
            "paths": {"/x": {"get": {"operationId": "X", "parameters": [
                {"name": "exclude", "in": "query", "schema": {"type": "array", "items": {"type": "string"}}},
                {"name": "expand", "in": "query", "style": "deepObject", "explode": true, "schema": {"type": "array"}},
                {"name": "tags", "in": "query", "style": "form", "explode": false, "schema": {"type": "array"}},
                {"name": "ids", "in": "query", "style": "pipeDelimited", "schema": {"type": "array"}}
            ]}}}
        })).unwrap();
        let m = s.query_styles(s.get("X").unwrap());
        assert_eq!(m["exclude"], QueryStyle::Repeated);
        assert_eq!(m["expand"], QueryStyle::Deep);
        assert_eq!(m["tags"], QueryStyle::Delimited(','));
        assert_eq!(m["ids"], QueryStyle::Delimited('|'));
        assert_eq!(s.default_query_style(), QueryStyle::Repeated);
    }

    #[test]
    fn query_styles_swagger2_collection_format() {
        // Swagger 2.0: collectionFormat drives it; absent -> csv default. And the
        // version default itself differs from OpenAPI 3 (csv, not repeated).
        let s = Spec::from_value(json!({
            "swagger": "2.0", "info": {"title": "T", "version": "1"},
            "paths": {"/p": {"get": {"operationId": "P", "parameters": [
                {"name": "status", "in": "query", "type": "array", "collectionFormat": "multi", "items": {"type": "string"}},
                {"name": "ids", "in": "query", "type": "array", "items": {"type": "integer"}}
            ]}}}
        })).unwrap();
        let m = s.query_styles(s.get("P").unwrap());
        assert_eq!(m["status"], QueryStyle::Repeated); // multi
        assert_eq!(m["ids"], QueryStyle::Delimited(',')); // no collectionFormat -> csv
        assert_eq!(s.default_query_style(), QueryStyle::Delimited(','));
    }

    #[test]
    fn ref_resolution_inlines_and_cuts_cycles() {
        let s = mini();
        let schema = s.body_schema(s.get("CreateWidget").unwrap(), true);
        let dumped = serde_json::to_string(&schema).unwrap();
        assert!(dumped.contains("\"name\""));
        assert!(dumped.contains("$circular")); // the self-reference is cut
    }

    #[test]
    fn body_media_type_drives_encoding_choice() {
        // JSON op (GitHub-style) vs form op (Stripe-style) — the executor picks
        // encoding from this, so mechanics stay data-driven (design law 1).
        let s = Spec::from_value(json!({
            "info": {"title": "T", "version": "1"},
            "paths": {
                "/j": {"post": {"operationId": "J",
                    "requestBody": {"content": {"application/json": {"schema": {}}}}}},
                "/f": {"post": {"operationId": "F",
                    "requestBody": {"content": {"application/x-www-form-urlencoded": {"schema": {}}}}}}
            }
        })).unwrap();
        assert!(s.body_media_type(s.get("J").unwrap()).unwrap().contains("json"));
        assert!(s.body_media_type(s.get("F").unwrap()).unwrap().contains("form-urlencoded"));
    }

    #[test]
    fn ref_chain_cap_terminates() {
        // 20-link non-cyclic chain must terminate, not recurse forever
        let mut schemas = serde_json::Map::new();
        for i in 0..20 {
            schemas.insert(format!("S{i}"), json!({"$ref": format!("#/components/schemas/S{}", i+1)}));
        }
        schemas.insert("S20".into(), json!({"type": "string"}));
        let s = Spec::from_value(json!({
            "info": {"title": "Chain", "version": "1"},
            "paths": {"/x": {"post": {"operationId": "Op",
                "requestBody": {"content": {"application/json": {"schema":
                    {"$ref": "#/components/schemas/S0"}}}}}}},
            "components": {"schemas": schemas}
        })).unwrap();
        let _ = s.body_schema(s.get("Op").unwrap(), true); // must return, not hang
    }
}
