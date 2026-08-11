# Configuration reference

min-mcp is configured with a single YAML file (default `min.yaml`, or
`--config <path>`). Secrets never live in the config — env vars are named, not
embedded. Paths (`spec:`, `jwt_public_key_file:`, `log_file:`) resolve relative to
the config file's directory, so the binary can be launched from anywhere.

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
log_file: events.ndjson   # optional NDJSON audit log of search/details/call
```

| key | meaning |
|---|---|
| `mode` | Surface shape. `three_tool` (search→details→call) or `passthrough` (declare everything). See [Concepts](concepts.md). |
| `upstreams` | The servers/specs to proxy. See below. |
| `filters` | Static allow/deny of tools, for everyone. See [Filters](#filters). |
| `scopes` | Per-caller visibility, keyed off JWT scopes. See [Transports & auth](transports-and-auth.md). |
| `auth` | How caller JWTs are validated. See [Transports & auth](transports-and-auth.md). |
| `overlays` | Per-tool fixes for a server you don't own: patch descriptions, the input schema (`fields`: required/example/enum/hide/`user_supplied`), errors (`error_hints` + `retryable` + structured `field`), responses, request `defaults`/`headers` (with `{{uuid}}`/`{{hash}}` generators), `aliases`, `paginate`, `verify` checks, and per-tool guards — `timeout_s` (call deadline) and `breaker` (circuit breaker on consecutive failures). See [Overlays](overlays.md). |
| `workflows` | Composite tools. See [Composites](composites.md). |
| `binding_policy` | Default reaction when an overlay no longer matches the live schema: `warn` (serve, skip broken parts) or `strict` (refuse to start). Overridable per overlay. |
| `error_hints` | Recovery instructions appended to any tool result whose text contains a substring. Per-tool overlay hints stack on top. A hint with a `field:` pointer renders a machine-shaped error from the patched schema. |
| `preflight` | **On by default.** Validate each call against its (patched) input schema *before* the upstream call — a missing-required or out-of-enum value returns a structured error locally, with no round-trip. Makes the patched schema authoritative; where a spec over-declares `required`, disable per tool with an overlay's `preflight: false` (or globally here). |
| `read_cache_ttl_s` | TTL (seconds) for caching results of **read-only** tools keyed by (tool, arguments): spec `GET` operations, MCP tools with `annotations.readOnlyHint`, or overlay `cacheable: true`. `0` (default) disables. Cached values are the raw pre-shaping result — each hit still gets this call's overlays and `fields` projection. Errors are never cached. |
| `log_file` | If set, one NDJSON line per search/details/call — what the agent searched, selected, and called, its origin, and whether it was served from cache. |

## Upstreams

An upstream is one of **three kinds**, distinguished by which key you set. All
share `name` (the prefix for its tool ids, e.g. `stripe.PostCustomers`).

### a) MCP server subprocess

```yaml
upstreams:
  - name: myserver
    command: npx
    args: ["-y", "@some/mcp-server"]
    env: { LOG_LEVEL: debug }           # optional literal env vars set on the child
    # cwd: ./subdir                      # optional; defaults to the config's dir
    # result_format: json                # json (default): re-encode JSON text results
    #                                    # compactly; raw: pass through byte-for-byte
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
    auth_env: STRIPE_TEST_KEY    # NAME of the env var holding the key
    accept: application/json     # optional Accept header
    headers:                     # optional static request headers (${VAR} expands)
      Notion-Version: "2022-06-28"   # e.g. a mandatory runtime header a spec omits
    result_format: json          # json (default, compact) | raw (byte-for-byte)
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

## Environment expansion

`${VAR}` is expanded from the environment (at connect time) in **`headers`** values
and **`oauth.client_secret`**. An **unset** variable is a hard error — min-mcp
fails loudly rather than sending an empty credential upstream.

Subprocess `env:` values are **literal** (no `${VAR}` expansion); the child also
inherits min-mcp's own environment, so pass a secret to a subprocess by exporting
it before launching min-mcp, not by embedding it in the config. `spec:` upstream
credentials use `auth_env:` (the env var's *name*), so they're never in the file
either.

## Examples

The [`examples/`](../examples/) directory has one config per shape:
`proxy-mcp-server.yaml`, `stripe-from-spec.yaml`, `github-from-spec.yaml`,
`oauth-upstream.yaml`, `stripe-narrow-filter.yaml`, `stripe-composite.yaml`,
`stripe-faithful-proxy.yaml`.
