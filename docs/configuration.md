# Configuration reference

min-mcp is configured with a single YAML file (default `min.yaml`, or
`--config <path>`). Secrets never live in the config — env vars are named, not
embedded. Paths (`spec:`, `jwt_public_key_file:`, `log_file:`) resolve relative to
the config file's directory, so the binary can be launched from anywhere.

Every key is checked. An unknown or misspelled key anywhere in the file
(`overlay:`, `preflght:`) is a startup error that names it, never a silent
no-op. So is a duplicate upstream `name`, overlay `tool`, or workflow `id`, and
an upstream that sets more than one of `command` / `url` / `spec`.

## Top-level keys

```yaml
mode: three_tool          # three_tool (default) | passthrough
upstreams: [ ... ]        # required: one or more upstreams (below)
filters: { ... }          # static include/exclude of whole APIs or tools
scopes: { ... }           # per-caller visibility rules (default-deny if present)
auth: { ... }             # JWT verification (HS256 / RS256 / JWKS)
overlays: [ ... ]         # patch/reshape tools you don't own — see overlays.md
workflows: [ ... ]        # composite multi-step tools — see composites.md
binding_policy: warn      # warn (default) | strict — how broken overlays behave
error_hints: [ ... ]      # fleet-wide error→recovery hints (all tools)
preflight: true           # validate calls against the patched schema locally (default ON)
read_cache_ttl_s: 0       # TTL cache for read-only tools' results (0 = off)
log_file: events.ndjson   # NDJSON audit stream — a path, or `stderr` for containers
rate_limits: { ... }      # token buckets per caller / per (caller, tool)
secrets: { ... }          # secret stores for ${…} references (Vault); env/file need none
shadow: false             # score alternative retrievers on real traffic (see below)
```

| key | meaning |
|---|---|
| `mode` | Surface shape. `three_tool` (search→details→call) or `passthrough` (declare everything). See [Concepts](concepts.md). |
| `upstreams` | The servers/specs to proxy. See below. |
| `filters` | Static allow/deny of tools, for everyone. See [Filters](#filters). |
| `scopes` | Per-caller visibility, keyed off JWT scopes. See [Transports & auth](transports-and-auth.md). |
| `auth` | How callers are identified: a JWT verifier (HS256 / RS256 / JWKS, with optional `audience` / `issuer` / `subject_claim`), or `trusted_headers` from a gateway; `allow_anonymous`. Over HTTP identity is per request. See [Transports & auth](transports-and-auth.md#caller-identity). |
| `overlays` | Per-tool fixes for a server you don't own: patch descriptions, the input schema (`fields`: required/example/enum/hide/`user_supplied`), errors (`error_hints` + `retryable` + structured `field`), responses, request `defaults`/`headers` (with `{{uuid}}`/`{{hash}}` generators), `aliases`, `paginate`, `verify` checks, and per-tool guards — `timeout_s` (call deadline) and `breaker` (circuit breaker on consecutive failures). See [Overlays](overlays.md). |
| `workflows` | Composite tools. See [Composites](composites.md). |
| `binding_policy` | Default reaction when an overlay no longer matches the live schema: `warn` (serve, skip broken parts) or `strict` (refuse to start). Overridable per overlay. |
| `error_hints` | Recovery instructions appended to any tool result whose text contains a substring. Per-tool overlay hints stack on top. A hint with a `field:` pointer renders a machine-shaped error from the patched schema. |
| `preflight` | **On by default.** Validate each call against its (patched) input schema *before* the upstream call — a missing-required or out-of-enum value returns a structured error locally, with no round-trip. Makes the patched schema authoritative; where a spec over-declares `required`, disable per tool with an overlay's `preflight: false` (or globally here). |
| `read_cache_ttl_s` | TTL (seconds) for caching results of **read-only** tools keyed by (tool, arguments): spec `GET` operations, MCP tools with `annotations.readOnlyHint`, or overlay `cacheable: true`. `0` (default) disables. Cached values are the raw pre-shaping result — each hit still gets this call's overlays and `fields` projection. Errors are never cached. |
| `log_file` | One NDJSON audit line per event — `search`, `details`, `call`, `paginate`, `workflow`, `breaker`, `rate_limited`, `shadow` — to this path, or to `stderr` (the container path; the platform's log shipper forwards it). Every line carries `caller`; a call adds tool, upstream, origin, `is_error`, `cached`, `latency_ms`, `result_bytes`. Never the arguments. |
| `rate_limits` | Token buckets on tool calls: `per_caller: {calls, per_s}` and `per_tool: {calls, per_s}` (per caller × tool). A refused call is a `RATE_LIMITED` isError result with a retry-after. Overlays add a per-tool cap across all callers (`rate_limit`). See [Rate limits](transports-and-auth.md#rate-limits). |
| `secrets` | Stores behind `${…}` references. `${env:X}` and `${file:/path}` need nothing here; `${vault:path#field}` needs `secrets.vault` (address, mount, `auth`: token / approle / kubernetes) and honours `cache_ttl_s`. See [Secrets](transports-and-auth.md#secrets). |
| `shadow` | **Off by default.** Score alternative retrieval configurations against real traffic without serving them; results land in `log_file` as `shadow` events. See [Shadow mode](#shadow-mode). |

## Upstreams

An upstream is one of **three kinds**, distinguished by which key you set —
exactly one of `command`, `url`, or `spec`. All share `name` (the prefix for its
tool ids, e.g. `stripe.PostCustomers`) and `optional`: with `optional: true` an
upstream that can't be spawned, connected, or listed at startup is skipped with a
warning instead of failing the whole proxy, and its tools are simply absent
until the next restart. Off by default, because a missing upstream is usually a
config error you want to hear about.

### a) MCP server subprocess

```yaml
upstreams:
  - name: myserver
    command: npx
    args: ["-y", "@some/mcp-server"]
    env: { LOG_LEVEL: debug }           # optional literal env vars set on the child
    # cwd: ./subdir                      # optional; defaults to the config's dir
    # optional: true                     # skip with a warning if it can't start (any kind)
    # result_format: raw                 # raw (default for MCP/HTTP): results pass
    #                                    # through byte-for-byte — a read_file result
    #                                    # is never rewritten. json: opt in to compact
    #                                    # re-encoding of JSON text results
```

Upstream **prompts and resources pass through** under the same object model as
tools: prompt names are namespaced `upstream.name`, an upstream hidden by
filters/scopes is invisible through these side doors too, and min-mcp adds its
own `minmcp://tools` resource — the [source map](cli.md) over the protocol.

### b) Remote MCP server over Streamable HTTP

```yaml
upstreams:
  - name: remote
    url: https://mcp.example.com/mcp
    headers:
      Authorization: "Bearer ${REMOTE_TOKEN}"   # ${VAR} expands at connect time
    # oauth: { ... }   # OR fetch a bearer automatically (below)
```

To let min-mcp obtain and refresh the bearer itself (OAuth 2.0
client-credentials):

```yaml
    oauth:
      token_url: https://auth.example.com/oauth/token
      client_id: my-client
      client_secret: "${OAUTH_SECRET}"
      scope: "read write"     # optional, space-delimited
```

### c) OpenAPI spec

```yaml
upstreams:
  - name: stripe
    spec: ./stripe.json          # path relative to the config file
    base_url: https://api.stripe.com
    auth_env: STRIPE_TEST_KEY    # NAME of the env var holding the key, OR:
    # api_key: "${vault:stripe/prod#key}"   # a ${env:…}/${file:…}/${vault:…} reference
    accept: application/json     # optional Accept header
    headers:                     # optional static request headers (${VAR} expands)
      Notion-Version: "2022-06-28"   # e.g. a mandatory runtime header a spec omits
    result_format: json          # json (default for spec upstreams — min-mcp builds
                                 # this envelope, compacting is safe) | raw
```

Request bodies are encoded from the spec's declared media type (form vs JSON);
agent-supplied path params are strictly segment-encoded so they can't escape their
endpoint.

## Filters

Static, config-level filtering — distinct from per-caller `scopes`. A filtered
tool is never spawned, listed, searched, or callable, for anyone; a fully excluded
API isn't even connected (needs no credentials).

```yaml
filters:
  include: ["stripe.Get*", "stripe.PostCustomers"]  # if present, ONLY these survive
  exclude: ["stripe.Delete*"]                        # dropped even if included
```

Patterns match a whole API (bare name `stripe`) or a tool id, with a trailing `*`
as a prefix wildcard.

**Filtering is a search-quality lever, not only an access-control one.** Every
tool you exclude is a tool search can no longer confuse with the one the agent
wanted — and shrinking the candidate pool consistently helps retrieval more than
any ranking improvement. Two unrelated measurements agree: our mcp-compressor
head-to-head found filters the strongest lever either proxy has, and a separate
RAG study found its hard filter worth more relevance than every ranking change it
tested combined ("isolation is not just compliance, it is relevance"). If you know
your agents never delete or never touch a subsystem, saying so in `filters:` makes
every remaining search better.

## Auth

How callers are identified. Over stdio the caller is the process (`--jwt` /
`MINMCP_JWT`, else `--scopes`); over HTTP, with any of this configured, each
request is identified on its own. Semantics: [Transports → Caller identity](transports-and-auth.md#caller-identity).

```yaml
auth:
  # a JWT verifier — precedence: jwks_url, then jwt_public_key(_file), then jwt_secret
  jwks_url: https://idp.example.com/.well-known/jwks.json   # RS256 by `kid`; refreshed on rotation and every 10 min
  jwt_public_key_file: ./idp.pem                           # RS256 PEM, relative to the config
  jwt_public_key: "-----BEGIN PUBLIC KEY-----\n..."           # RS256 PEM, inline
  jwt_secret: "${MINMCP_JWT_SECRET}"                       # HS256; a ${…} reference, or the
                                                           # MINMCP_JWT_SECRET env var directly
  audience: minmcp                 # if set, `aud` must contain this (default: unchecked)
  issuer: https://idp.example.com  # if set, `iss` must equal this (default: unchecked)
  scope_claim: scope               # claim holding the scopes (default "scope"; string or array)
  subject_claim: sub               # claim naming the caller for audit (default "sub")
  # OR, behind a gateway that already authenticated the caller (HTTP only):
  trusted_headers:
    scopes: X-Auth-Scopes          # space- or comma-separated scopes
    subject: X-Auth-Subject        # optional; the audit label
  allow_anonymous: false           # true: a request with no identity gets the process caller
```

## Rate limits

Token buckets on tool calls, keyed by the caller's subject (`search_tools` and
`get_tool_details` are free). `calls` is also the burst; the bucket refills at
`calls / per_s` per second. A refused call is a `RATE_LIMITED` isError result
with a retry-after. Per-tool caps across all callers live on the overlay
(`rate_limit`).

```yaml
rate_limits:
  per_caller: { calls: 600, per_s: 60 }   # a caller's total tool calls
  per_tool:   { calls: 120, per_s: 60 }   # per (caller, tool); per_s defaults to 60
```

## Secrets

Stores behind `${…}` references (see [Secret references](#secret-references)).
`env` and `file` need nothing here; Vault does:

```yaml
secrets:
  vault:
    address: https://vault.example.com:8200   # default: $VAULT_ADDR
    namespace: team-a                          # optional (Vault Enterprise)
    mount: secret                              # KV v2 mount (default "secret")
    ca_cert: /etc/ssl/private-ca.pem           # optional; default $VAULT_CACERT
    auth:                                      # exactly one of the three
      token_env: VAULT_TOKEN                   # a static token (the default source)
      approle: { role_id: "${VAULT_ROLE_ID}", secret_id: "${VAULT_SECRET_ID}", mount: approle }
      kubernetes: { role: minmcp, jwt_path: /var/run/secrets/kubernetes.io/serviceaccount/token, mount: kubernetes }
  cache_ttl_s: 300                             # Vault reads cached this long (default 300)
```

## Shadow mode

`shadow: true` builds a handful of alternative search indexes (different
tokenization/weighting/corpus switches) alongside the served one. Every
`search_tools` call runs them all; **none of their results are ever served**. When
the agent then calls a tool, min-mcp logs — to `log_file`, as `shadow` events —
the rank each alternative *would* have given that tool, alongside the served
rank. The tool the agent actually chose is the label, so retrieval changes get
judged on your real workload with no annotation effort.

Off by default: each challenger is a full extra index, costing startup time and
memory production shouldn't pay. Turn it on when evaluating an upgrade, read the
NDJSON, turn it off. Interpretation caveat: it measures agreement with what the
agent *did*, which is ideal for comparing rankers and catching regressions, but a
consistently-wrong tool choice scores as a hit — absolute correctness still needs
a labelled set.

## Secret references

Every credential-shaped value — `headers` (upstream and overlay),
`oauth.client_secret`, a spec upstream's `api_key`, `auth.jwt_secret`, a
`user_supplied` field's source — is a **reference**, resolved at startup (or at
call time for `user_supplied`):

| form | source |
|---|---|
| `${NAME}` / `${env:NAME}` | the process environment |
| `${file:/run/secrets/x}` | a file (Kubernetes / Docker secret mounts); trailing newline trimmed |
| `${vault:path/to/secret#field}` | HashiCorp Vault / OpenBao KV v2 — needs `secrets.vault` |

An unresolvable reference is a hard error — min-mcp fails loudly rather than
sending an empty credential upstream. Full store configuration and the
`user_supplied` sources are in [Transports & auth → Secrets](transports-and-auth.md#secrets).

Subprocess `env:` values are **literal** (no expansion); the child also inherits
min-mcp's own environment, so pass a secret to a subprocess by exporting it
before launching min-mcp, not by embedding it in the config.

## Examples

The [`examples/`](../examples/) directory has one config per shape — offline
demos (`demo-overlays.yaml`, `demo-workflow.yaml`, `demo-scopes.yaml`) and
real-API ones (`proxy-mcp-server.yaml`, `github-mcp-server.yaml`,
`stripe-from-spec.yaml`, `github-from-spec.yaml`, `github-fixups.yaml`,
`oauth-upstream.yaml`, `stripe-narrow-filter.yaml`, `stripe-composite.yaml`),
and `enterprise.yaml` — gateway identity, per-caller scopes, rate limits, an
audit stream, secrets, and an optional upstream, all in one annotated file.
See [`examples/README.md`](../examples/README.md).
