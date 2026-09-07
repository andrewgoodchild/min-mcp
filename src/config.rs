//! Config: upstreams, surface mode, scope rules, overlays.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a tool-call result body is serialized back to the agent.
///
/// The default depends on who owns the serialization. A **spec** upstream's
/// response envelope is built by min-mcp, so compacting it is free and safe:
/// default `json`. An **MCP/HTTP** upstream's result is someone else's bytes —
/// compacting a `read_file` result that happens to be pretty-printed JSON
/// destroys the file's real formatting, and the agent then edits or writes back
/// mangled content. So for those upstreams the default is `raw`, and `json` is
/// the explicit opt-in for servers whose results are JSON *payloads*, not
/// documents.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResultFormat {
    /// Re-serialize JSON text as compact JSON (strip inter-token whitespace).
    Json,
    /// Pass the upstream's result through byte-for-byte.
    Raw,
}

/// The minified surface the agent sees. Only the two modes that measured as a
/// win over the baseline ship: `three_tool` (search→details→call) and
/// `passthrough` (plain federation). The `hotset` and `pd` (uniform progressive
/// disclosure) experiments lost to tiering on held-out tasks and were removed —
/// see the design notes.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Passthrough,
    #[default]
    ThreeTool,
}

/// An upstream is one of three kinds: an MCP server subprocess (`command`), a
/// remote MCP server over Streamable HTTP (`url`), or a mounted OpenAPI spec
/// (`spec` + `base_url` + `auth_env`). Secrets are never in config — `auth_env`
/// names an env var, and `headers` values expand `${VAR}` from the environment.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    /// Keep serving if this upstream can't be spawned, connected, or listed at
    /// startup: it is skipped with a warning instead of failing the whole proxy,
    /// and its tools are simply absent until the next restart. Off by default —
    /// a missing upstream is usually a config error you want to hear about.
    #[serde(default)]
    pub optional: bool,

    // --- MCP-server (subprocess) mode ---
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,

    // --- remote MCP-server (Streamable HTTP) mode ---
    /// URL of a remote MCP endpoint to proxy over Streamable HTTP.
    #[serde(default)]
    pub url: Option<String>,
    /// Auth/other headers sent to the upstream. Values may contain `${VAR}`,
    /// expanded from the environment at connect time (keeps secrets out of
    /// config): e.g. `Authorization: "Bearer ${GITHUB_PAT}"`.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// OAuth 2.0 client-credentials for an OAuth-protected remote MCP upstream:
    /// min-mcp fetches a bearer token from `token_url`, caches it, and refreshes
    /// on expiry — so you don't have to mint tokens by hand.
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// Working directory to spawn the child in. Filled at load time with the
    /// config file's directory (so relative args resolve predictably no matter
    /// where minmcp itself was invoked); an explicit `cwd:` overrides it.
    #[serde(default)]
    pub cwd: Option<PathBuf>,

    // --- OpenAPI-spec mode ---
    /// Path to an OpenAPI spec to mount as tools (relative to the config file).
    #[serde(default)]
    pub spec: Option<String>,
    /// API base URL the spec's operations are called against.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the env var holding the API key (never the key itself).
    #[serde(default)]
    pub auth_env: Option<String>,
    /// The API key as a secret reference — `${env:NAME}`, `${file:/run/secrets/x}`,
    /// or `${vault:path#field}` — for stores other than the environment. One of
    /// `auth_env` / `api_key`, not both.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional Accept header (e.g. application/vnd.github+json).
    #[serde(default)]
    pub accept: Option<String>,
    /// How this upstream's response bodies are serialized to the agent. On spec
    /// upstreams `json` embeds the parsed body in the result envelope; on MCP /
    /// HTTP upstreams `json` re-encodes JSON text blocks compactly and `raw`
    /// passes results through byte-for-byte. Defaults per upstream kind — spec:
    /// `json` (we own that serialization), MCP/HTTP: `raw` (byte fidelity for
    /// results we don't own). See [`UpstreamConfig::result_format`].
    #[serde(default)]
    result_format: Option<ResultFormat>,
}

/// OAuth 2.0 client-credentials grant config for an upstream.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    /// Token endpoint (the OAuth `token_url`).
    pub token_url: String,
    pub client_id: String,
    /// Client secret; supports `${VAR}` env expansion (keep it out of config).
    pub client_secret: String,
    /// Optional space-delimited scopes to request.
    #[serde(default)]
    pub scope: Option<String>,
}

impl UpstreamConfig {
    /// The effective result format: the configured value, or the kind-dependent
    /// default — `json` for spec upstreams (min-mcp builds that envelope, so
    /// compacting is safe), `raw` for MCP/HTTP upstreams (their bytes are not
    /// ours to rewrite; `json` is the opt-in).
    pub fn result_format(&self) -> ResultFormat {
        self.result_format.unwrap_or(if self.is_spec() {
            ResultFormat::Json
        } else {
            ResultFormat::Raw
        })
    }

    pub fn is_spec(&self) -> bool {
        self.spec.is_some()
    }
    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

/// A token-bucket limit: `calls` per `per_s` seconds, with `calls` as the burst.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    pub calls: u32,
    #[serde(default = "default_per_s")]
    pub per_s: u64,
}

fn default_per_s() -> u64 {
    60
}

/// Fleet-wide rate limits on tool calls (search/details are free). Keyed by
/// the caller's subject; every anonymous caller shares one bucket. A refused
/// call is a `RATE_LIMITED` isError result with a retry-after.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct RateLimits {
    /// Calls per caller, across all tools.
    #[serde(default)]
    pub per_caller: Option<Limit>,
    /// Calls per (caller, tool).
    #[serde(default)]
    pub per_tool: Option<Limit>,
}

impl RateLimits {
    /// Is any caller-keyed limit configured? (An overlay's `rate_limit` is keyed
    /// by tool across all callers, so it is unaffected by the caller's subject.)
    pub fn any(&self) -> bool {
        self.per_caller.is_some() || self.per_tool.is_some()
    }
}

/// Secret stores beyond the environment. `${file:…}` needs no configuration;
/// `${vault:…}` needs `vault`.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    #[serde(default)]
    pub vault: Option<VaultConfig>,
    /// Seconds a Vault read is cached (default 300), so a `user_supplied` field
    /// costs one round-trip per TTL rather than one per call. `env` and `file`
    /// are never cached — re-reading a mounted file is how rotation lands.
    #[serde(default = "default_secret_ttl")]
    pub cache_ttl_s: u64,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        SecretsConfig { vault: None, cache_ttl_s: default_secret_ttl() }
    }
}

fn default_secret_ttl() -> u64 {
    300
}

/// HashiCorp Vault / OpenBao, KV v2. Address and token fall back to the
/// standard `VAULT_ADDR` / `VAULT_TOKEN` environment variables.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    /// e.g. `https://vault.example.com:8200` (default: `$VAULT_ADDR`).
    #[serde(default)]
    pub address: Option<String>,
    /// Vault Enterprise namespace, if any.
    #[serde(default)]
    pub namespace: Option<String>,
    /// KV v2 mount that `${vault:path#field}` paths are under (default `secret`).
    #[serde(default = "default_vault_mount")]
    pub mount: String,
    /// PEM bundle for a private CA (default: `$VAULT_CACERT`).
    #[serde(default)]
    pub ca_cert: Option<String>,
    #[serde(default)]
    pub auth: VaultAuth,
}

fn default_vault_mount() -> String {
    "secret".to_string()
}

/// How min-mcp authenticates to Vault. Default: a token from `$VAULT_TOKEN`.
/// Exactly one of `approle` / `kubernetes` may be set; both re-login when Vault
/// rejects the token (403), so a token TTL shorter than the process is fine.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct VaultAuth {
    /// Env var holding a Vault token (default `VAULT_TOKEN`).
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub approle: Option<VaultAppRole>,
    #[serde(default)]
    pub kubernetes: Option<VaultKubernetes>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct VaultAppRole {
    /// May reference the environment: `${VAULT_ROLE_ID}`.
    pub role_id: String,
    /// Keep it out of the file: `${VAULT_SECRET_ID}`.
    pub secret_id: String,
    #[serde(default = "default_approle_mount")]
    pub mount: String,
}

fn default_approle_mount() -> String {
    "approle".to_string()
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct VaultKubernetes {
    pub role: String,
    /// The pod's projected service-account token.
    #[serde(default = "default_sa_jwt_path")]
    pub jwt_path: String,
    #[serde(default = "default_k8s_mount")]
    pub mount: String,
}

fn default_sa_jwt_path() -> String {
    "/var/run/secrets/kubernetes.io/serviceaccount/token".to_string()
}

fn default_k8s_mount() -> String {
    "kubernetes".to_string()
}

/// A composite tool: a linear sequence of steps (the useful subset of Arazzo),
/// exposed to the agent as ONE tool that runs the chain internally, threading
/// each step's outputs into the next step's inputs.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// The composite tool's id/name (what the agent calls).
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema of the composite's inputs (what the agent provides).
    #[serde(default)]
    pub inputs: Value,
    pub steps: Vec<Step>,
    /// Composite outputs: name -> expression (`$steps.<id>.<name>` / `$inputs.x`).
    #[serde(default)]
    pub outputs: HashMap<String, Value>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    /// Tool id this step calls, e.g. "stripe.PostProducts".
    pub tool: String,
    /// Argument template; string values may reference `$inputs.<path>` or
    /// `$steps.<stepId>.<name>` (a prior step's extracted output).
    #[serde(default)]
    pub input: Value,
    /// Outputs to extract from this step's response: name -> dotted path into
    /// the response body (e.g. {id: "id"} keeps `body.id`).
    #[serde(default)]
    pub output: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ScopeRule {
    pub scope: String,
    /// Tool-id patterns this scope grants: exact `up.tool` or prefix `up.Post*`.
    pub tools: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ErrorHint {
    /// Substring of the upstream error/result text that triggers the hint.
    pub contains: String,
    /// Recovery instruction appended for the agent (design law 6).
    pub hint: String,
    /// Optional pointer to a schema field (dotted, e.g. `body.zone`). When set,
    /// min-mcp renders a machine-shaped error block from that field's PATCHED
    /// schema — `{error, field, allowed_values, description, fix}` — instead of
    /// the prose `hint`. Measured: structured errors recover weak agents 0%→100%
    /// where prose hints don't. One source of truth: the codes/meanings come from
    /// the `fields` patch, not a duplicated string.
    #[serde(default)]
    pub field: Option<String>,
    /// Structured retry signal for the agent: `true` = transient, wait and retry;
    /// `false` = permanent, fix the request, don't retry. Appended as an explicit
    /// line so the model doesn't have to guess retryability (429/5xx vs 4xx).
    #[serde(default)]
    pub retryable: Option<bool>,
}

/// Proxy-side auto-pagination: when a list endpoint pages its results, follow the
/// cursor and concatenate — so the agent gets one complete result instead of
/// hand-rolling a cursor loop (and usually truncating). Applies to spec upstreams.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Paginate {
    /// Dotted path to the item array to accumulate across pages (e.g. `data`).
    pub items: String,
    /// Dotted path to the next-page cursor value in the response. A `[last]`
    /// segment takes the last array element (Stripe: `data[last].id`); otherwise
    /// a plain field (`next_cursor`, `links.next`).
    pub cursor: String,
    /// Request path (dotted, into {path_params,query_params,body}) to write the
    /// cursor for the next page — e.g. `query_params.starting_after`.
    pub into: String,
    /// Optional dotted path to a boolean gate (e.g. `has_more`); page only while
    /// it's true. Omit to page until the cursor is empty/null.
    #[serde(default)]
    pub more: Option<String>,
    /// Safety cap on pages fetched (default 10).
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
}

fn default_max_pages() -> usize {
    10
}

/// A declarative response transform: remove / rename / set on the
/// JSON payload, plus a jq escape hatch for anything the declarative ops can't
/// express. Paths are dotted; `[]` maps over an array element. Applied to every
/// call (before any caller `fields` projection).
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseTransform {
    /// Allowlist: keep ONLY these field paths (projection), e.g. ["data[].id"].
    /// Applied server-side and always — an aggressive filter that can drop
    /// context the agent might have needed, so use with care.
    #[serde(default)]
    pub keep: Vec<String>,
    /// Field paths to delete, e.g. ["data[].secret", "livemode"].
    #[serde(default)]
    pub remove: Vec<String>,
    /// Rename a field's key in place: path -> new key name, e.g.
    /// {"data[].balance_transaction": "txn"}.
    #[serde(default)]
    pub rename: HashMap<String, String>,
    /// Add-or-replace a field at a path (objects only), e.g. {"source": "min-mcp"}.
    #[serde(default)]
    pub set: HashMap<String, Value>,
    /// Escape hatch: a jq program applied to the whole payload after the
    /// declarative ops, for arbitrary reshaping the ops above can't express.
    #[serde(default)]
    pub jq: Option<String>,
    /// When to apply this transform, keyed on the tool result's `isError`
    /// (gated on the result's error flag): `always` (default), `success`,
    /// or `error` — e.g. strip a verbose error body but leave successes intact.
    #[serde(default)]
    pub when: When,
}

/// Gates a response transform on the result's `isError` flag.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum When {
    #[default]
    Always,
    Success,
    Error,
}

impl ResponseTransform {
    pub fn is_noop(&self) -> bool {
        self.keep.is_empty()
            && self.remove.is_empty()
            && self.rename.is_empty()
            && self.set.is_empty()
            && self.jq.is_none()
    }

    /// Whether this transform applies to a result with the given `isError`.
    pub fn applies(&self, is_error: bool) -> bool {
        match self.when {
            When::Always => true,
            When::Success => !is_error,
            When::Error => is_error,
        }
    }
}

/// What to do when a binding (overlay) is broken against the live upstream
/// schema — its tool is gone, or a field it patches no longer exists. This is
/// the config-wide DEFAULT; a per-overlay `binding:` overrides it.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BindingPolicy {
    /// Log a warning and serve anyway (broken parts are skipped). Default.
    #[default]
    Warn,
    /// Refuse to start: a broken binding is a config error. For CI / prod.
    Strict,
}

/// What to do when a tool's own name or description trips a tool-poisoning rule
/// (see `crate::lint::poisoning`) — text aimed at the model rather than at a
/// reader, or engineered to be invisible to one.
///
/// The linter only ever *reports*; this is what the SERVER does about it, so a
/// poisoned tool is not merely detectable but refusable. `warn` by default: the
/// rules measure zero false positives across Stripe (589 ops) and GitHub
/// (1,216), but refusing to start is a big hammer to hand someone by default,
/// and a hand-written MCP upstream is a likelier source of a false positive than
/// the OpenAPI specs the rules were tuned on.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PoisoningPolicy {
    /// Do not check at all.
    Off,
    /// Log a warning naming each flagged tool, and serve it anyway. Default.
    #[default]
    Warn,
    /// Refuse to start. For CI, and for a fleet where an upstream you do not
    /// own could change its descriptions under you.
    Strict,
}

/// Per-overlay binding strength. The consequence of breakage differs by overlay
/// — a PII-strip that silently stops applying is a data leak (fail hard), a
/// description patch that stops applying is cosmetic (fail soft) — so strength
/// is per-binding, overriding the config-wide `binding_policy`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BindingStrength {
    /// Broken → skip the broken parts, keep serving (fail open).
    Weak,
    /// Broken → error out hard, refuse to start (fail closed).
    Strong,
}

/// A patch applied to one input field, addressed by a dotted path through the
/// schema's `properties`: `body.currency`, `path_params.owner`, or a bare `owner`
/// for a flat (MCP) tool. A bare string is shorthand for a description-only patch,
/// backward-compatible with the original `fields: {name: "text"}`.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum FieldPatch {
    /// `fields: {owner: "the repo owner"}` — replace the description only.
    Description(String),
    /// `fields: {owner: {required: true, example: "octocat"}}` — full patch.
    Spec(FieldSpec),
}

/// The structured half of a [`FieldPatch`]: everything an overlay can change about
/// one input field. This is what makes the lint findings *fixable* — notably
/// `required`, the fix for undocumented-required params.
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    #[serde(default)]
    pub description: Option<String>,
    /// Add (`true`) or remove (`false`) this field from its object's `required`
    /// list. The single highest-value fix — a write op that 400s without a field
    /// the schema calls optional.
    #[serde(default)]
    pub required: Option<bool>,
    #[serde(default)]
    pub example: Option<Value>,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<Value>>,
    #[serde(default, rename = "type")]
    pub ty: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    /// Drop this field from the input schema entirely (flatten/hide a rarely-used
    /// or over-nested field).
    #[serde(default)]
    pub hide: Option<bool>,
    /// Mark this field USER-supplied, not agent-supplied. It is stripped from the
    /// agent-facing schema (so the agent can neither set nor *fabricate* it) and
    /// injected by the proxy at call time from `source`. Format: `env:VAR`. The
    /// structural anti-fabrication fix — an agent can't invent a value it can't
    /// see; the value comes from the session/environment, not the model. If the
    /// source can't be resolved at call time, the call fails with a clear
    /// `missing_user_supplied_value` error (never a fabricated success).
    #[serde(default)]
    pub user_supplied: Option<String>,
}

impl FieldPatch {
    /// Normalize to a [`FieldSpec`] (the string shorthand becomes a description).
    pub fn spec(&self) -> std::borrow::Cow<'_, FieldSpec> {
        match self {
            FieldPatch::Description(d) => {
                std::borrow::Cow::Owned(FieldSpec { description: Some(d.clone()), ..Default::default() })
            }
            FieldPatch::Spec(s) => std::borrow::Cow::Borrowed(s),
        }
    }
}

/// A dynamic verification check: call the (overlaid) tool with `arguments` and
/// assert on the result. This is the third leg of the loop — detect (lint) → fix
/// (overlay) → **verify** — and it's *reusable*: the same check proves a fix at
/// author time, guards against behavioural drift in CI, and can gate an
/// agent-proposed patch. Assertions are deterministic (no LLM judge).
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct VerifyCheck {
    /// Human label for the check (shown in the report).
    #[serde(default)]
    pub name: Option<String>,
    /// The `call_tool` arguments ({path_params, query_params, body}).
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub expect: Expect,
}

/// Deterministic assertions on a tool result. All are optional; a check passes
/// when every specified assertion holds.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Exact HTTP status (spec upstreams; the executor envelope's `status`).
    #[serde(default)]
    pub status: Option<u64>,
    /// The MCP `isError` flag.
    #[serde(default)]
    pub is_error: Option<bool>,
    /// Dotted response-payload paths that must be present and non-null.
    #[serde(default)]
    pub has: Vec<String>,
    /// Dotted response-payload paths that must be absent/null (e.g. a stripped secret).
    #[serde(default)]
    pub missing: Vec<String>,
    /// A substring that must appear in the result text.
    #[serde(default)]
    pub contains: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    /// Full tool id (`upstream.tool`) the overlay applies to.
    pub tool: String,
    /// Fingerprint of the target tool's schema this overlay was authored
    /// against (copy from `minmcp map`'s `schema_sha`). When set, min-mcp
    /// detects upstream drift: if the live schema no longer matches, the
    /// binding is flagged `changed` (or `broken` if its contract fails).
    #[serde(default)]
    pub authored_sha: Option<String>,
    /// `weak` (fail open) or `strong` (fail closed) if this binding breaks.
    /// Defaults to the config-wide `binding_policy` (strict→strong, warn→weak).
    #[serde(default)]
    pub binding: Option<BindingStrength>,
    #[serde(default)]
    pub description: Option<String>,
    /// Field path -> patch. A bare string is a description; a map is a full
    /// [`FieldSpec`] (`required`/`example`/`enum`/`type`/`format`/`hide`). Paths
    /// are dotted through `properties` (`body.currency`), so nested API params —
    /// not just the envelope — are reachable.
    #[serde(default)]
    pub fields: HashMap<String, FieldPatch>,
    /// Extra names indexed for `search_tools` only (discovery), so a poorly-named
    /// tool is findable by an outcome phrase ("find_user_by_email"). Routing and
    /// the tool id are unchanged.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Per-operation request headers (spec upstreams). Fills a header the spec
    /// *requires* but the mount doesn't supply, per endpoint — e.g. the CDR
    /// `x-v` API-version header, which differs by endpoint. Values may contain
    /// `${ENV}` (expanded once, at load) and per-request generators resolved on
    /// every call: `{{uuid}}` (a fresh UUIDv4, for `x-fapi-interaction-id`),
    /// `{{now}}`/`{{now_ms}}` (unix epoch), `{{iso8601}}` (UTC RFC-3339).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Request-side input defaults, applied before the upstream call for any
    /// path the agent omitted. Dotted paths (`body.currency`), so the value
    /// lands in the right place: e.g. {"body.currency": "usd"}. The agent may
    /// still override; these only fill what's absent.
    #[serde(default)]
    pub defaults: HashMap<String, Value>,
    #[serde(default)]
    pub error_hints: Vec<ErrorHint>,
    /// Declarative (+ jq) transform applied to every response before it reaches
    /// the model — strip secrets/PII/noise, rename or reshape fields.
    #[serde(default)]
    pub response: ResponseTransform,
    /// Dynamic verification checks — call the tool and assert the fix works.
    /// Run by `minmcp verify` (makes real upstream calls).
    #[serde(default)]
    pub verify: Vec<VerifyCheck>,
    /// Auto-follow pagination and concatenate results (spec upstreams).
    ///
    /// KNOWN LIMIT: combined with a `{{hash}}` header this reuses page 1's key
    /// on every follow-up page (headers resolve once per call). Against an
    /// idempotency-honouring API that replays page 1, the non-advancing-cursor
    /// guard then ends pagination with page 1 only. Don't combine the two; a
    /// per-page re-resolution is tracked as future work.
    #[serde(default)]
    pub paginate: Option<Paginate>,
    /// Per-tool preflight override: `false` disables local schema validation for
    /// this tool (e.g. the spec over-declares `required`); `true` forces it even
    /// when the global `preflight` is off. Absent → the global setting.
    #[serde(default)]
    pub preflight: Option<bool>,
    /// Per-tool read-cache override: `true` marks this tool's results cacheable
    /// even without a read-only signal; `false` exempts a tool that IS read-only
    /// but must never be cached (e.g. a token mint). Absent → inferred (spec GET
    /// / `annotations.readOnlyHint`). Only effective when `read_cache_ttl_s` > 0.
    #[serde(default)]
    pub cacheable: Option<bool>,
    /// Per-tool call timeout (seconds) for the upstream call, tighter than the
    /// transport's 120s default. Values above 120 are clamped to 120 — the
    /// transport ceiling is a ceiling, not a suggestion. On expiry the agent gets an isError result that
    /// says the operation may still have completed — never a silent hang eating
    /// the turn budget. Applies to every upstream call, pagination follow-ups
    /// included; a follow-up that times out stops pagination and the pages
    /// already fetched are returned with a PAGINATION notice.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Per-tool rate limit across ALL callers (a vendor quota, a fragile
    /// endpoint) — on top of the fleet-wide `rate_limits`.
    #[serde(default)]
    pub rate_limit: Option<Limit>,
    /// Circuit breaker: after `consecutive_failures` isError results this tool
    /// is paused for `cooldown_s` (then one probe call is let through). The
    /// structural fix for identical-retry loops — measured burning 15 turns in
    /// the spike and 562K tokens in the recall benchmark. (Mined from
    /// ContextForge's circuit_breaker plugin; consecutive-failure subset.)
    #[serde(default)]
    pub breaker: Option<Breaker>,
}

/// Per-tool circuit-breaker thresholds (see `Overlay::breaker`).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Breaker {
    /// Consecutive isError results that trip the breaker open (default 5).
    #[serde(default = "default_breaker_failures")]
    pub consecutive_failures: u32,
    /// Seconds the breaker stays open before allowing one probe (default 60).
    #[serde(default = "default_breaker_cooldown")]
    pub cooldown_s: u64,
}

fn default_breaker_failures() -> u32 {
    5
}

fn default_breaker_cooldown() -> u64 {
    60
}

impl Overlay {
    /// Effective strength: the per-overlay `binding` if set, else derived from
    /// the config-wide policy.
    pub fn strength(&self, policy: BindingPolicy) -> BindingStrength {
        self.binding.unwrap_or(match policy {
            BindingPolicy::Strict => BindingStrength::Strong,
            BindingPolicy::Warn => BindingStrength::Weak,
        })
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Scopes {
    #[serde(default)]
    pub rules: Vec<ScopeRule>,
}

/// Static, config-level tool filtering (distinct from per-caller `scopes`).
/// Decides which upstream tools are *passed through at all* — a filtered tool
/// is never spawned/loaded, listed, searched, or callable, for any caller.
///
/// Patterns match either a whole API (bare upstream name, e.g. `stripe`) or a
/// tool id, with a trailing `*` acting as a prefix wildcard: `stripe.*` (whole
/// API), `stripe.Post*` (a family), `stripe.PostCustomers` (one tool).
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    /// Allowlist. If non-empty, ONLY tools matching one of these survive.
    #[serde(default)]
    pub include: Vec<String>,
    /// Denylist, applied after `include`. A tool matching any of these is
    /// dropped even if it was included.
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_scope_claim() -> String {
    "scope".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// HS256 shared secret for validating caller JWTs. May also come from the
    /// MINMCP_JWT_SECRET env var (keeps the secret out of the committed config).
    #[serde(default)]
    pub jwt_secret: Option<String>,
    /// RS256 public key (PEM, inline) for validating caller JWTs.
    #[serde(default)]
    pub jwt_public_key: Option<String>,
    /// RS256 public key read from a PEM file (path relative to the config).
    #[serde(default)]
    pub jwt_public_key_file: Option<String>,
    /// JWKS endpoint; keys fetched once at startup, selected by the token `kid`.
    #[serde(default)]
    pub jwks_url: Option<String>,
    /// Claim the caller's scopes are read from (OAuth `scope` by default).
    #[serde(default = "default_scope_claim")]
    pub scope_claim: String,
    /// If set, the token's `aud` claim must contain this value. Unset means the
    /// audience is not checked (a token minted for another service validates).
    #[serde(default)]
    pub audience: Option<String>,
    /// If set, the token's `iss` claim must equal this value.
    #[serde(default)]
    pub issuer: Option<String>,
    /// Claim naming the caller, for audit lines (default `sub`).
    #[serde(default = "default_subject_claim")]
    pub subject_claim: String,
    /// HTTP only. Behind a gateway that has ALREADY authenticated the caller,
    /// take identity from these request headers instead of a bearer. Trust
    /// them only when the gateway is the sole route to this port and strips
    /// inbound copies — a client that can reach the port directly can set any
    /// header it likes.
    #[serde(default)]
    pub trusted_headers: Option<TrustedHeaders>,
    /// HTTP only. When identity is configured (a verifier or trusted headers),
    /// a request carrying none is refused with 401 — unless this is true, in
    /// which case it gets the process identity (`--jwt` / `--scopes`). Off by
    /// default: an unauthenticated path next to an authenticated one is how
    /// scoping gets bypassed.
    #[serde(default)]
    pub allow_anonymous: bool,
}

/// Transport-level hardening for the HTTP listener: TLS, and caps bounding what
/// one client can make the process allocate. Both are off/loose by default —
/// `serve --http` on loopback for local development needs neither.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Terminate TLS on the listener itself. Unset means plaintext, which is
    /// only safe on loopback or behind a TLS-terminating gateway.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// See [`HttpLimits`].
    #[serde(default)]
    pub limits: HttpLimits,
}

/// Server certificate, key, and optionally the CA that client certificates must
/// chain to. Paths rather than inline PEM: key material belongs in a mounted
/// secret (Kubernetes, systemd credentials), never in the committed config.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain — the server certificate first, then intermediates.
    pub cert_file: String,
    /// PEM private key (PKCS#8, PKCS#1, or SEC1).
    pub key_file: String,
    /// If set, MUTUAL TLS: a client must present a certificate chaining to this
    /// CA bundle, or the connection is closed before any request is read.
    ///
    /// This authenticates the CHANNEL, not the caller — it is what makes
    /// `auth.trusted_headers` safe to believe, by ensuring only the gateway can
    /// open a connection at all. It never names the caller: identity still
    /// comes from the bearer or the gateway's identity headers.
    #[serde(default)]
    pub client_ca_file: Option<String>,
}

/// Caps on what one client can make the process allocate. The defaults are far
/// above what a legitimate MCP client sends, so they bite only on abuse.
///
/// `default` at the container level fills any missing field from the `Default`
/// impl below, so each cap is stated exactly once.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct HttpLimits {
    /// Largest accepted request body; a larger one is refused with 413 before
    /// being buffered. 1 MiB by default — model-generated tool arguments are
    /// bounded by the model's own context long before this.
    pub max_body_bytes: usize,
    /// Most requests rmcp is handling at once across every connection. Beyond
    /// this a request waits rather than being refused.
    ///
    /// This bounds request HANDLING, not concurrent upstream calls: the permit
    /// is released when the response future resolves, and an SSE reply resolves
    /// before the tool runs. See the note in `rmcp_serve::serve_http`.
    pub max_in_flight: usize,
}

// Not derived: a derived Default would give 0 for both, and 0 means "refuse
// every request" / "never issue a permit" rather than "unset".
impl Default for HttpLimits {
    fn default() -> Self {
        HttpLimits { max_body_bytes: 1024 * 1024, max_in_flight: 256 }
    }
}

impl Auth {
    /// Will an authenticated request carry a subject to key rate limits and
    /// audit lines on? A bearer does (`subject_claim`, `sub` by default); a
    /// gateway does only if it forwards an identity header as well as scopes.
    pub fn names_the_caller(&self) -> bool {
        match &self.trusted_headers {
            // Trusted headers take effect only when no bearer is presented, so a
            // gateway without a subject header is the gap even alongside a verifier.
            Some(t) => t.subject.is_some(),
            // `has_verifier` rather than an inline field check: the HS256 secret
            // may arrive via MINMCP_JWT_SECRET with no config field set at all,
            // and missing that spelling warns a deployment that is already fine.
            None => self.has_verifier(),
        }
    }
}

/// Identity headers set by a trusted fronting gateway (see `Auth::trusted_headers`).
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TrustedHeaders {
    /// Header carrying the caller's scopes, space- or comma-separated.
    pub scopes: String,
    /// Header carrying the caller's identity, for audit lines.
    #[serde(default)]
    pub subject: Option<String>,
}

fn default_subject_claim() -> String {
    "sub".to_string()
}

// Manual Default so an ABSENT `auth:` section still yields the "scope" claim
// (a derived Default would give an empty string).
impl Default for Auth {
    fn default() -> Self {
        Auth {
            jwt_secret: None,
            jwt_public_key: None,
            jwt_public_key_file: None,
            jwks_url: None,
            scope_claim: default_scope_claim(),
            audience: None,
            issuer: None,
            subject_claim: default_subject_claim(),
            trusted_headers: None,
            allow_anonymous: false,
        }
    }
}

impl Auth {
    /// Effective HS256 secret: env var overrides the config field. The config
    /// value may be a `${…}` reference; the caller resolves it.
    pub fn secret(&self) -> Option<String> {
        std::env::var("MINMCP_JWT_SECRET").ok().or_else(|| self.jwt_secret.clone())
    }

    /// Is a JWT verifier configured at all (any key source)?
    pub fn has_verifier(&self) -> bool {
        self.jwks_url.is_some()
            || self.jwt_public_key.is_some()
            || self.jwt_public_key_file.is_some()
            || self.secret().is_some()
    }

    /// RS256 public-key PEM, from the inline field or the referenced file.
    pub fn public_key_pem(&self) -> Result<Option<String>> {
        if let Some(pem) = &self.jwt_public_key {
            return Ok(Some(pem.clone()));
        }
        if let Some(path) = &self.jwt_public_key_file {
            let pem = std::fs::read_to_string(path)
                .with_context(|| format!("reading jwt_public_key_file {path}"))?;
            return Ok(Some(pem));
        }
        Ok(None)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Score alternative retrievers against real traffic without serving them,
    /// logging what each *would* have ranked when the agent calls a tool. Off by
    /// default: each challenger is a second full index. See `surface/shadow.rs`.
    #[serde(default)]
    pub shadow: bool,

    #[serde(default)]
    pub mode: Mode,
    pub upstreams: Vec<UpstreamConfig>,
    /// If any rules exist, visibility is default-deny.
    #[serde(default)]
    pub scopes: Scopes,
    /// Static include/exclude of whole APIs or individual tools.
    #[serde(default)]
    pub filters: Filters,
    #[serde(default)]
    pub auth: Auth,
    /// Transport hardening for `serve --http`. See [`HttpConfig`].
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    /// Composite tools (linear step workflows) exposed alongside upstream tools.
    #[serde(default)]
    pub workflows: Vec<Workflow>,
    /// How to treat bindings that are broken against the live upstream schema.
    #[serde(default)]
    pub binding_policy: BindingPolicy,
    /// What to do about a tool whose name or description trips a tool-poisoning
    /// rule. See [`PoisoningPolicy`].
    #[serde(default)]
    pub poisoning_policy: PoisoningPolicy,
    /// Validate each call against its (patched) input schema BEFORE the upstream
    /// call: a missing required field or an out-of-enum value returns a
    /// structured error locally (no round-trip, no opaque upstream 400). ON by
    /// default — the schema's own `required`/`enum` are treated as authoritative
    /// (measured: a local structured error beats a raw upstream dump; the
    /// mcp-compressor head-to-head found our raw passthrough losing to their
    /// default preflight). Set `preflight: false` globally, or per tool via an
    /// overlay's `preflight: false`, where a spec over-declares `required`.
    #[serde(default = "default_true")]
    pub preflight: bool,
    /// TTL in seconds for caching results of READ-ONLY tools keyed by (tool,
    /// arguments) — spec `GET` operations, MCP tools with
    /// `annotations.readOnlyHint`, or tools an overlay marks `cacheable: true`.
    /// 0 (default) disables caching. A proxy-side latency/token saver for
    /// repeat reads; never applied to writes or error results.
    #[serde(default)]
    pub read_cache_ttl_s: u64,
    /// Error hints applied to EVERY tool (design law 6 at the fleet level).
    /// Per-tool overlay hints are additive on top of these.
    #[serde(default)]
    pub error_hints: Vec<ErrorHint>,
    /// If set, append one NDJSON audit line per search/details/call/… event to
    /// this file (path relative to the config), or to `stderr` — the container
    /// path, where the platform's log shipper forwards it to the SIEM. Every
    /// line carries the caller, the tool, its origin, outcome, latency, and
    /// result size; never the arguments.
    #[serde(default)]
    pub log_file: Option<String>,
    /// Fleet-wide token-bucket limits on tool calls. See [`RateLimits`].
    #[serde(default)]
    pub rate_limits: RateLimits,
    /// Secret stores for `${…}` references. See [`SecretsConfig`].
    #[serde(default)]
    pub secrets: SecretsConfig,
    /// tool_id -> index into `overlays`, so per-call lookup is O(1) instead of a
    /// linear scan. Built at load; not part of the config file.
    #[serde(skip)]
    overlay_index: HashMap<String, usize>,
}

/// Make one configured path relative to the config file's directory, in place.
/// Absolute paths are left alone. Every path-valued config key goes through
/// this, so the rule lives in one place rather than once per key.
fn resolve_rel(base: &std::path::Path, p: &mut String) {
    if !std::path::Path::new(p.as_str()).is_absolute() {
        *p = base.join(p.as_str()).to_string_lossy().into_owned();
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {path}"))?;
        let mut cfg = Self::from_yaml(&text)?;
        // Resolve upstream cwd to the config's directory so relative args
        // (e.g. `uv --directory research`) don't depend on minmcp's own CWD.
        let base = PathBuf::from(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from);
        if let Some(base) = base {
            // Every path in the config is relative to the config file, so the
            // rule is stated once and applied at each site (see `resolve_rel`).
            for up in &mut cfg.upstreams {
                up.cwd.get_or_insert_with(|| base.clone());
                if let Some(spec) = &mut up.spec {
                    resolve_rel(&base, spec);
                }
            }
            if let Some(key) = &mut cfg.auth.jwt_public_key_file {
                resolve_rel(&base, key);
            }
            if let Some(log) = &mut cfg.log_file {
                // `stderr` is a sentinel, not a path.
                if log != "stderr" {
                    resolve_rel(&base, log);
                }
            }
            if let Some(tls) = &mut cfg.http.tls {
                resolve_rel(&base, &mut tls.cert_file);
                resolve_rel(&base, &mut tls.key_file);
                if let Some(ca) = &mut tls.client_ca_file {
                    resolve_rel(&base, ca);
                }
            }
        }
        Ok(cfg)
    }

    /// Parse and validate a config document. Unknown keys are rejected by the
    /// deserializer (`deny_unknown_fields` on every config struct), so a typo
    /// like `overlay:` or `preflght:` is a startup error, not a silent no-op.
    pub fn from_yaml(text: &str) -> Result<Self> {
        let mut cfg: Config = serde_yaml::from_str(text).context("invalid config yaml")?;
        cfg.validate()?;
        cfg.index_overlays();
        Ok(cfg)
    }

    /// Cross-field checks the deserializer can't express. Each one is a routing
    /// hazard, not a style nit: every per-tool map keys on `upstream.tool`, so a
    /// silent last-wins sends a call to the wrong place.
    fn validate(&self) -> Result<()> {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for up in &self.upstreams {
            if !seen.insert(up.name.as_str()) {
                anyhow::bail!(
                    "duplicate upstream name {:?}: tool ids are namespaced by upstream name, \
                     so duplicates would silently route one upstream's calls to the other. \
                     Rename one of them.",
                    up.name
                );
            }
            let kinds = [up.command.is_some(), up.url.is_some(), up.spec.is_some()];
            match kinds.iter().filter(|k| **k).count() {
                1 => {}
                0 => anyhow::bail!(
                    "upstream {:?} needs exactly one of `command` (MCP server subprocess), \
                     `url` (remote MCP server), or `spec` (OpenAPI document)",
                    up.name
                ),
                _ => anyhow::bail!(
                    "upstream {:?} sets more than one of `command`, `url`, and `spec`; an \
                     upstream is exactly one kind — split it into separate upstreams",
                    up.name
                ),
            }
            if up.is_spec() && up.base_url.is_none() {
                anyhow::bail!("spec upstream {:?} needs `base_url`", up.name);
            }
            if up.auth_env.is_some() && up.api_key.is_some() {
                anyhow::bail!(
                    "upstream {:?} sets both `auth_env` and `api_key`; use one (api_key takes a \
                     ${{…}} reference, auth_env names an env var)",
                    up.name
                );
            }
        }
        if let Some(v) = &self.secrets.vault {
            if v.auth.approle.is_some() && v.auth.kubernetes.is_some() {
                anyhow::bail!("secrets.vault.auth: set either `approle` or `kubernetes`, not both");
            }
        }
        if let Some(t) = &self.auth.trusted_headers {
            if t.scopes.trim().is_empty() {
                anyhow::bail!("auth.trusted_headers.scopes must name a header");
            }
        }
        // Both caps are hazards at 0 in DIFFERENT ways, and neither is loud:
        // `max_body_bytes: 0` refuses every request that carries a body, and
        // `max_in_flight: 0` is worse — the semaphore never issues a permit, so
        // the listener binds, prints its banner, and then hangs every request
        // forever with no error anywhere. An overlarge `max_in_flight` is a
        // panic inside tokio's `Semaphore::new`, so it is caught here too.
        if self.http.limits.max_body_bytes == 0 {
            anyhow::bail!(
                "http.limits.max_body_bytes must be at least 1: 0 refuses every request that \
                 carries a body. Omit the field for the 1 MiB default."
            );
        }
        match self.http.limits.max_in_flight {
            0 => anyhow::bail!(
                "http.limits.max_in_flight must be at least 1: with 0 permits every request \
                 waits forever and the server answers nothing. Omit the field for the default (256)."
            ),
            n if n > tokio::sync::Semaphore::MAX_PERMITS => anyhow::bail!(
                "http.limits.max_in_flight is {n}, above the maximum {}",
                tokio::sync::Semaphore::MAX_PERMITS
            ),
            _ => {}
        }
        if self.auth.allow_anonymous && !self.auth.has_verifier() && self.auth.trusted_headers.is_none() {
            anyhow::bail!(
                "auth.allow_anonymous is set but no caller identity is configured (a JWT verifier \
                 or trusted_headers) — without one every request is already the process identity"
            );
        }
        let mut seen = HashSet::new();
        for o in &self.overlays {
            if !seen.insert(o.tool.as_str()) {
                anyhow::bail!(
                    "duplicate overlay for tool {:?}: only one would apply (last wins, \
                     silently). Merge them into one entry.",
                    o.tool
                );
            }
        }
        let mut seen = HashSet::new();
        for w in &self.workflows {
            if !seen.insert(w.id.as_str()) {
                anyhow::bail!("duplicate workflow id {:?}: only one would be callable. Rename one.", w.id);
            }
        }
        Ok(())
    }

    /// Error hints matching a result for `tool_id`: global hints plus this
    /// tool's overlay hints.
    pub fn error_hints_for(&self, tool_id: &str) -> impl Iterator<Item = &ErrorHint> {
        let overlay_hints = self.overlay_for(tool_id).map(|o| o.error_hints.as_slice()).unwrap_or(&[]);
        self.error_hints.iter().chain(overlay_hints)
    }

    /// Does a tool survive the static include/exclude filters? A tool is passed
    /// through only if it matches `include` (when any include is set) and does
    /// not match `exclude`. `upstream` is the owning API's name so a bare
    /// pattern can match a whole API.
    pub fn passes_filter(&self, upstream: &str, tool_id: &str) -> bool {
        let m = |p: &str| filter_match(p, upstream, tool_id);
        if !self.filters.include.is_empty() && !self.filters.include.iter().any(|p| m(p)) {
            return false;
        }
        !self.filters.exclude.iter().any(|p| m(p))
    }

    /// Whether an upstream could contribute *any* tool under the filters — lets
    /// the surface skip spawning a server / loading a spec that is fully
    /// filtered out (so an excluded API needs no command and no credentials).
    /// Conservative: only returns false when the whole upstream is provably out.
    pub fn upstream_enabled(&self, upstream: &str) -> bool {
        if !self.filters.include.is_empty()
            && !self.filters.include.iter().any(|p| references_upstream(p, upstream))
        {
            return false;
        }
        !self.filters.exclude.iter().any(|p| covers_whole_upstream(p, upstream))
    }

    /// Is `tool_id` visible to a caller holding `granted` scopes?
    /// No rules configured => everything visible (opt-in security).
    pub fn allowed(&self, tool_id: &str, granted: &[String]) -> bool {
        if self.scopes.rules.is_empty() {
            return true;
        }
        self.scopes.rules.iter().any(|rule| {
            granted.iter().any(|g| g == &rule.scope)
                && rule.tools.iter().any(|p| pattern_match(p, tool_id))
        })
    }

    /// Build the tool_id -> overlay index (call after populating `overlays`).
    pub fn index_overlays(&mut self) {
        self.overlay_index =
            self.overlays.iter().enumerate().map(|(i, o)| (o.tool.clone(), i)).collect();
    }

    pub fn overlay_for(&self, tool_id: &str) -> Option<&Overlay> {
        // O(1) via the index when built (Config::load); fall back to a linear
        // scan for configs constructed without load() (e.g. unit tests).
        if !self.overlay_index.is_empty() {
            return self.overlay_index.get(tool_id).map(|&i| &self.overlays[i]);
        }
        self.overlays.iter().find(|o| o.tool == tool_id)
    }
}

/// Exact match, or prefix match when the pattern ends with `*`.
pub fn pattern_match(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

/// A filter pattern matches a tool. A bare word (no `.`, no `*`) targets a whole
/// API by upstream name; anything else is matched against the tool id.
fn filter_match(pattern: &str, upstream: &str, tool_id: &str) -> bool {
    if !pattern.contains('.') && !pattern.contains('*') {
        return pattern == upstream;
    }
    pattern_match(pattern, tool_id)
}

/// Does a pattern reference this upstream at all (used to decide if an include
/// list touches it)? True for the bare name and for any `up.…` tool pattern.
fn references_upstream(pattern: &str, upstream: &str) -> bool {
    let core = pattern.strip_suffix('*').unwrap_or(pattern);
    core == upstream || core.starts_with(&format!("{upstream}."))
}

/// Does a pattern remove the *entire* upstream (bare name or `up.*`)? Narrower
/// patterns like `up.Post*` don't qualify — they still leave tools behind.
fn covers_whole_upstream(pattern: &str, upstream: &str) -> bool {
    pattern == upstream || pattern == format!("{upstream}.*")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        Config::from_yaml(yaml).unwrap()
    }

    #[test]
    fn unknown_keys_are_rejected_at_every_level() {
        // A typo used to be a silent no-op; every config struct now denies
        // unknown fields, so it is a startup error naming the key.
        let top = Config::from_yaml("upstreams: []\npreflght: false\n").unwrap_err();
        assert!(format!("{top:#}").contains("preflght"), "{top:#}");
        let up = Config::from_yaml("upstreams:\n  - name: a\n    command: x\n    comand: y\n").unwrap_err();
        assert!(format!("{up:#}").contains("comand"), "{up:#}");
        let ov = Config::from_yaml(
            "upstreams: []\noverlays:\n  - tool: a.b\n    descripton: nope\n",
        )
        .unwrap_err();
        assert!(format!("{ov:#}").contains("descripton"), "{ov:#}");
        let auth = Config::from_yaml("upstreams: []\nauth:\n  jwt_secrets: x\n").unwrap_err();
        assert!(format!("{auth:#}").contains("jwt_secrets"), "{auth:#}");
    }

    #[test]
    fn upstream_must_be_exactly_one_kind() {
        let both = Config::from_yaml(
            "upstreams:\n  - name: a\n    command: x\n    spec: s.json\n    base_url: http://h\n",
        )
        .unwrap_err();
        assert!(format!("{both:#}").contains("more than one"), "{both:#}");
        let none = Config::from_yaml("upstreams:\n  - name: a\n").unwrap_err();
        assert!(format!("{none:#}").contains("exactly one"), "{none:#}");
        let no_base = Config::from_yaml("upstreams:\n  - name: a\n    spec: s.json\n").unwrap_err();
        assert!(format!("{no_base:#}").contains("base_url"), "{no_base:#}");
        // each kind alone is fine
        for body in ["command: x", "url: http://h/mcp", "spec: s.json\n    base_url: http://h"] {
            assert!(Config::from_yaml(&format!("upstreams:\n  - name: a\n    {body}\n")).is_ok(), "{body}");
        }
    }

    #[test]
    fn duplicate_names_overlays_and_workflows_are_rejected() {
        let ups = Config::from_yaml(
            "upstreams:\n  - name: a\n    command: x\n  - name: a\n    command: y\n",
        )
        .unwrap_err();
        assert!(format!("{ups:#}").contains("duplicate upstream name"), "{ups:#}");
        let ovs = Config::from_yaml(
            "upstreams: []\noverlays:\n  - tool: a.b\n  - tool: a.b\n",
        )
        .unwrap_err();
        assert!(format!("{ovs:#}").contains("duplicate overlay"), "{ovs:#}");
        let wfs = Config::from_yaml(
            "upstreams: []\nworkflows:\n  - id: w\n    steps: []\n  - id: w\n    steps: []\n",
        )
        .unwrap_err();
        assert!(format!("{wfs:#}").contains("duplicate workflow"), "{wfs:#}");
    }

    #[test]
    fn pattern_matching() {
        assert!(pattern_match("stripe.Post*", "stripe.PostCustomers"));
        assert!(pattern_match("stripe.GetCharges", "stripe.GetCharges"));
        assert!(!pattern_match("stripe.Post*", "stripe.GetCharges"));
        assert!(!pattern_match("stripe.GetCharges", "stripe.GetChargesCharge"));
    }

    #[test]
    fn no_rules_means_everything_allowed() {
        let c = cfg("upstreams: []\n");
        assert!(c.allowed("any.Tool", &[]));
    }

    #[test]
    fn rules_are_default_deny() {
        let c = cfg(
            "upstreams: []\nscopes:\n  rules:\n    - scope: payments.write\n      tools: [\"stripe.Post*\"]\n",
        );
        let write = vec!["payments.write".to_string()];
        assert!(c.allowed("stripe.PostCustomers", &write));
        assert!(!c.allowed("stripe.GetCharges", &write));
        assert!(!c.allowed("stripe.PostCustomers", &[])); // no scopes -> denied
    }

    #[test]
    fn global_and_per_tool_error_hints_combine() {
        let c = cfg(
            r#"
upstreams: []
error_hints:
  - contains: "401"
    hint: "check credentials"
overlays:
  - tool: stripe.PostCustomers
    error_hints:
      - contains: "email"
        hint: "ask for the email"
"#,
        );
        let hints: Vec<&str> = c.error_hints_for("stripe.PostCustomers").map(|h| h.hint.as_str()).collect();
        assert_eq!(hints, vec!["check credentials", "ask for the email"]);
        // a tool with no overlay still gets the global hint
        let only_global: Vec<&str> = c.error_hints_for("stripe.GetCharges").map(|h| h.hint.as_str()).collect();
        assert_eq!(only_global, vec!["check credentials"]);
    }

    #[test]
    fn no_filters_pass_everything() {
        let c = cfg("upstreams: []\n");
        assert!(c.passes_filter("stripe", "stripe.PostCustomers"));
        assert!(c.upstream_enabled("stripe"));
    }

    #[test]
    fn exclude_drops_a_single_tool_and_a_family() {
        let c = cfg(
            "upstreams: []\nfilters:\n  exclude: [\"stripe.PostCustomers\", \"github.Delete*\"]\n",
        );
        assert!(!c.passes_filter("stripe", "stripe.PostCustomers"));
        assert!(c.passes_filter("stripe", "stripe.GetCharges"));
        assert!(!c.passes_filter("github", "github.DeleteRepo"));
        assert!(c.passes_filter("github", "github.GetRepo"));
        // neither exclusion removes a whole upstream, so both still spawn
        assert!(c.upstream_enabled("stripe"));
        assert!(c.upstream_enabled("github"));
    }

    #[test]
    fn exclude_whole_api_skips_the_upstream() {
        // bare name and the `.*` wildcard both remove an entire API
        let bare = cfg("upstreams: []\nfilters:\n  exclude: [\"stripe\"]\n");
        assert!(!bare.upstream_enabled("stripe"));
        assert!(!bare.passes_filter("stripe", "stripe.GetCharges"));
        assert!(bare.upstream_enabled("github"));

        let star = cfg("upstreams: []\nfilters:\n  exclude: [\"stripe.*\"]\n");
        assert!(!star.upstream_enabled("stripe"));
        assert!(!star.passes_filter("stripe", "stripe.GetCharges"));
    }

    #[test]
    fn include_is_an_allowlist() {
        let c = cfg(
            "upstreams: []\nfilters:\n  include: [\"stripe.Post*\", \"github\"]\n",
        );
        // stripe: only the Post* family survives
        assert!(c.passes_filter("stripe", "stripe.PostCustomers"));
        assert!(!c.passes_filter("stripe", "stripe.GetCharges"));
        assert!(c.upstream_enabled("stripe"));
        // github: whole API included
        assert!(c.passes_filter("github", "github.GetRepo"));
        assert!(c.upstream_enabled("github"));
        // an upstream the include list never mentions is skipped entirely
        assert!(!c.upstream_enabled("slack"));
        assert!(!c.passes_filter("slack", "slack.PostMessage"));
    }

    #[test]
    fn exclude_wins_over_include() {
        let c = cfg(
            "upstreams: []\nfilters:\n  include: [\"stripe\"]\n  exclude: [\"stripe.Delete*\"]\n",
        );
        assert!(c.passes_filter("stripe", "stripe.PostCustomers"));
        assert!(!c.passes_filter("stripe", "stripe.DeleteCustomer"));
    }

    #[test]
    fn absent_auth_section_defaults_scope_claim_to_scope() {
        // regression: a derived Default gave "" here, silently breaking JWT
        // scope extraction when the config had no `auth:` section.
        let c = cfg("upstreams: []\n");
        assert_eq!(c.auth.scope_claim, "scope");
        assert!(c.auth.jwt_secret.is_none());
    }

    #[test]
    fn response_transform_when_gate_matches_is_error() {
        let mk = |y: &str| serde_yaml::from_str::<ResponseTransform>(y).unwrap();
        let always = mk("remove: [x]"); // when defaults to always
        assert!(always.applies(true) && always.applies(false));
        let success = mk("when: success\nremove: [x]");
        assert!(success.applies(false) && !success.applies(true));
        let error = mk("when: error\nremove: [x]");
        assert!(error.applies(true) && !error.applies(false));
    }

    #[test]
    fn rate_limits_secrets_and_identity_keys_parse_and_validate() {
        let c = cfg(
            "upstreams: []\nrate_limits:\n  per_caller: {calls: 100}\n  per_tool: {calls: 5, per_s: 10}\n\
             overlays:\n  - tool: a.b\n    rate_limit: {calls: 2, per_s: 1}\n\
             secrets:\n  vault:\n    address: https://v.example:8200\n    auth: {approle: {role_id: r, secret_id: \"${X}\"}}\n\
             auth:\n  jwt_secret: s\n  subject_claim: email\n  trusted_headers: {scopes: X-Scopes, subject: X-User}\n  allow_anonymous: true\n",
        );
        assert_eq!(c.rate_limits.per_caller.unwrap().per_s, 60, "per_s defaults to 60");
        assert_eq!(c.rate_limits.per_tool.unwrap().calls, 5);
        assert_eq!(c.overlay_for("a.b").unwrap().rate_limit.unwrap().calls, 2);
        let v = c.secrets.vault.as_ref().unwrap();
        assert_eq!(v.mount, "secret");
        assert_eq!(v.auth.approle.as_ref().unwrap().mount, "approle");
        assert_eq!(c.secrets.cache_ttl_s, 300);
        assert_eq!(c.auth.subject_claim, "email");
        assert!(c.auth.allow_anonymous && c.auth.has_verifier());
        let anon = Config::from_yaml("upstreams: []\nauth:\n  allow_anonymous: true\n").unwrap_err();
        assert!(format!("{anon:#}").contains("allow_anonymous"), "{anon:#}");
        // both vault logins → rejected; both key sources on an upstream → rejected
        let both = Config::from_yaml(
            "upstreams: []\nsecrets:\n  vault:\n    auth:\n      approle: {role_id: r, secret_id: s}\n      kubernetes: {role: k}\n",
        )
        .unwrap_err();
        assert!(format!("{both:#}").contains("not both"), "{both:#}");
        let keys = Config::from_yaml(
            "upstreams:\n  - name: a\n    spec: s.json\n    base_url: http://h\n    auth_env: K\n    api_key: \"${file:/x}\"\n",
        )
        .unwrap_err();
        assert!(format!("{keys:#}").contains("auth_env"), "{keys:#}");
        // absent sections keep their defaults
        let d = cfg("upstreams: []\n");
        assert!(d.rate_limits.per_caller.is_none() && d.secrets.vault.is_none());
        assert_eq!(d.auth.subject_claim, "sub");
        assert!(!d.auth.has_verifier());
    }

    #[test]
    fn load_accepts_result_format_on_any_upstream_and_defaults_preflight_on() {
        let dir = std::env::temp_dir();
        // result_format defaults per upstream kind: raw for MCP/HTTP (byte
        // fidelity for results we don't own — a read_file result is never
        // rewritten), json for spec (min-mcp builds that envelope). Explicit
        // values still win.
        let cfg_path = dir.join(format!("minmcp_cfg_rf_{}.yaml", std::process::id()));
        std::fs::write(&cfg_path, "upstreams:\n  - name: srv\n    command: echo\n  - name: exp\n    command: echo\n    result_format: json\n").unwrap();
        let cfg = Config::load(cfg_path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.upstreams[0].result_format(), ResultFormat::Raw, "MCP default is raw");
        assert_eq!(cfg.upstreams[1].result_format(), ResultFormat::Json, "explicit json wins");
        // preflight defaults ON (the head-to-head correction); read cache OFF.
        assert!(cfg.preflight, "preflight must default on");
        assert_eq!(cfg.read_cache_ttl_s, 0, "read cache must default off");
        let _ = std::fs::remove_file(&cfg_path);
    }

    #[test]
    fn config_parses_full_shape() {
        let c = cfg(
            r#"
mode: passthrough
upstreams:
  - name: stripe
    command: uv
    args: ["run", "x"]
http:
  tls:
    cert_file: /etc/minmcp/server.crt
    key_file: /etc/minmcp/server.key
    client_ca_file: /etc/minmcp/clients.crt
  limits:
    max_body_bytes: 2048
    max_in_flight: 8
overlays:
  - tool: stripe.PostCustomers
    description: "Create a customer."
    fields: {email: "Never invent; ask if unknown."}
    error_hints:
      - contains: "Illegal header"
        hint: "Upstream credentials are missing."
"#,
        );
        assert_eq!(c.mode, Mode::Passthrough);
        assert_eq!(c.overlay_for("stripe.PostCustomers").unwrap().fields.len(), 1);
        let tls = c.http.tls.as_ref().expect("tls section");
        assert_eq!(tls.cert_file, "/etc/minmcp/server.crt");
        assert_eq!(tls.client_ca_file.as_deref(), Some("/etc/minmcp/clients.crt"));
        assert_eq!(c.http.limits.max_body_bytes, 2048);
        assert_eq!(c.http.limits.max_in_flight, 8);
    }

    #[test]
    fn poisoning_policy_parses_and_defaults_to_warn() {
        let c = cfg("upstreams: [{name: s, command: x}]\n");
        assert_eq!(c.poisoning_policy, PoisoningPolicy::Warn, "default must not refuse to start");
        for (text, want) in [
            ("poisoning_policy: strict\n", PoisoningPolicy::Strict),
            ("poisoning_policy: off\n", PoisoningPolicy::Off),
            ("poisoning_policy: warn\n", PoisoningPolicy::Warn),
        ] {
            let c = cfg(&format!("upstreams: [{{name: s, command: x}}]\n{text}"));
            assert_eq!(c.poisoning_policy, want, "for {text}");
        }
        assert!(Config::from_yaml("upstreams: [{name: s, command: x}]\npoisoning_policy: loose\n").is_err());
    }

    #[test]
    fn http_limits_default_when_the_section_is_absent() {
        // A derived Default would give 0 here, which would refuse every request.
        let c = cfg("upstreams: [{name: s, command: x}]\n");
        assert_eq!(c.http.limits.max_body_bytes, 1024 * 1024);
        assert_eq!(c.http.limits.max_in_flight, 256);
        assert!(c.http.tls.is_none());
    }

    #[test]
    fn a_zero_cap_is_rejected_rather_than_silently_serving_nothing() {
        // `max_in_flight: 0` is the nastier of the two: the listener binds and
        // prints its banner, then every request waits on a permit that is never
        // issued — a server that answers nothing, with nothing in the log.
        assert!(Config::from_yaml(
            "upstreams: [{name: s, command: x}]\nhttp:\n  limits:\n    max_in_flight: 0\n"
        )
        .is_err());
        assert!(Config::from_yaml(
            "upstreams: [{name: s, command: x}]\nhttp:\n  limits:\n    max_body_bytes: 0\n"
        )
        .is_err());
        // And the sane ones still load.
        let c = cfg("upstreams: [{name: s, command: x}]\nhttp:\n  limits:\n    max_in_flight: 1\n");
        assert_eq!(c.http.limits.max_in_flight, 1);
    }

    #[test]
    fn a_misspelled_http_key_is_rejected() {
        assert!(Config::from_yaml(
            "upstreams: [{name: s, command: x}]\nhttp:\n  limits:\n    max_body: 10\n"
        )
        .is_err());
    }
}
