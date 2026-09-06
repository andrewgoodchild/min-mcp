# Transports, identity & operations

## Serving the surface: stdio or Streamable HTTP

Both transports are served by the **official MCP SDK**
([`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)): min-mcp's surface is
wrapped behind rmcp's `ServerHandler`, so protocol conformance — version
negotiation, sessions, SSE, Host-header/DNS-rebinding defence — tracks the SDK
rather than a bespoke implementation.

The surface is **shared, not locked**. The catalog, search index, schemas, and
upstream connections are built once and read by every caller concurrently; the
few things that change per call (usage prior, read cache, breakers, rate-limit
buckets, the audit sink) sit behind small locks that are never held across an
upstream call. A slow upstream call therefore blocks only its own caller. Even
one stdio upstream serves concurrent calls: the client multiplexes requests by
id over the single pipe, so a fast call never waits behind a slow one to the
same server.

### stdio (default)

```sh
minmcp serve --config myconfig.yaml
```

Point your MCP client at this command. One process, one caller — the usual
local-agent setup. The caller's identity is the process's (see below).

### Streamable HTTP

```sh
minmcp serve --http 127.0.0.1:8080 --config myconfig.yaml
```

rmcp's `StreamableHttpService` — JSON-RPC over POST, SSE replies, session ids, and
Host-header validation — driven on a TCP listener by hyper. The MCP endpoint is
served at the root path (`/`). Two guards sit in front of every request:

- **Origin** — a browser-style cross-origin POST (DNS rebinding) is refused with
  403. An absent Origin is fine; non-browser clients don't send one.
- **Identity** — with `auth:` configured, each request is authenticated on its
  own (a bearer token, or headers from a trusted gateway) and the resulting
  caller is what that request sees. See [Caller identity](#caller-identity).

**Binding beyond loopback.** The listener refuses a non-loopback address
(`0.0.0.0`, a LAN address, a public hostname) unless one of two things holds:
every request must authenticate (an `auth:` verifier or `trusted_headers`, with
`allow_anonymous` off), or you pass `--allow-remote` to say an authenticating
proxy or a private network is in front. Without either, anyone who can reach
the port would get the process's scopes and every upstream credential it holds.

The two DNS-rebinding checks follow the bind. A loopback server validates both
`Host` (rmcp's loopback allow-list) and `Origin`, because that is the attack's
shape. A non-loopback server cannot validate `Host` — clients address it by
whatever name or address they reach it on — so that check is off; the `Origin`
check stays on under `--allow-remote` (nothing else stands between a rebinding
page and an unauthenticated port) and is off when identity is enforced (a
rebinding page cannot present a bearer, and a browser-based client that can
must not be refused for its origin).

## Caller identity

min-mcp gates *which tools a caller sees* by scope: tools a caller's scopes don't
grant are never listed, searched, callable, or visible in the source map. Where
the caller's identity comes from depends on the transport.

| transport | the caller is | identity source |
|---|---|---|
| stdio | the process | `--jwt` (validated) / `MINMCP_JWT` env, else `--scopes` |
| HTTP, `auth:` configured | **each request** | `Authorization: Bearer <jwt>` validated here, or gateway headers |
| HTTP, no `auth:` | the process | same as stdio (local-dev model; loopback only) |

Over HTTP with `auth:` configured, two clients of one `serve --http` with
different tokens see two different surfaces, and every audit line names the
caller. A request with no identity is refused with **401** and a
`WWW-Authenticate: Bearer` challenge (unless `allow_anonymous: true`).

### 1. Configure a verifier (`auth:`)

Precedence is JWKS → RS256 public key → HS256 secret:

```yaml
auth:
  # pick one:
  jwt_secret: "${MINMCP_JWT_SECRET}"          # HS256 shared secret (env, file, or vault reference)
  jwt_public_key: "-----BEGIN PUBLIC KEY-----\n..."   # RS256, inline PEM
  jwt_public_key_file: ./pub.pem              # RS256, from a file
  jwks_url: https://auth.example.com/.well-known/jwks.json  # JWKS, kid-selected
  scope_claim: scope                          # claim to read scopes from (default "scope")
  subject_claim: sub                          # claim naming the caller, for audit (default "sub")
  # audience: minmcp                          # if set, `aud` must contain this (else unchecked)
  # issuer: https://auth.example.com          # if set, `iss` must equal this (else unchecked)
```

Signature and `exp` are always checked. `audience` and `issuer` are off unless
set — an internal gateway with no audience discipline still works — but where
one issuer mints tokens for several services, set both so a token for another
service doesn't grant scopes here.

A JWKS set is **live**. It is fetched at startup (30s timeout, a failure is a
startup error) and refreshed by the verifier itself: when a token names a `kid`
it doesn't know (the IdP just rotated), and when the set is older than ten
minutes (a key the IdP withdrew must stop being trusted). Refreshes are
single-flight and never more than once a minute, so an unauthenticated caller
inventing `kid`s cannot drive fetches against your IdP; a failed refresh keeps
the current keys and logs a warning.

### 2. Define scope rules (`scopes:`)

If any rules exist, visibility is **default-deny** — a caller sees only tools
granted by a scope they hold:

```yaml
scopes:
  rules:
    - scope: billing.read
      tools: ["stripe.Get*"]
    - scope: billing.write
      tools: ["stripe.PostCustomers", "stripe.Post*"]
```

Tool patterns are exact (`up.tool`) or prefix (`up.Post*`). Composite tools
(`workflows:`) are matched by their `id` like any tool.

### 3a. Bearer tokens (agents call min-mcp directly)

Each HTTP request carries `Authorization: Bearer <jwt>`; min-mcp validates it
with the verifier above and derives that request's caller. Over stdio, pass the
process's token once:

```sh
minmcp serve --config myconfig.yaml --jwt "$CALLER_JWT"
# or, keeping the token out of argv / `ps` / shell history:
MINMCP_JWT="$CALLER_JWT" minmcp serve --config myconfig.yaml
```

For local dev without JWTs, `--scopes billing.read,billing.write` sets a process
identity directly.

### 3b. Behind a gateway that already authenticated the caller

When Kong, Envoy, Apigee, API Management, or your own gateway terminates
authentication and forwards the caller's identity in headers, trust those
instead of re-validating a token:

```yaml
auth:
  trusted_headers:
    scopes: X-Auth-Scopes          # "store.read store.write" or comma-separated
    subject: X-Auth-Subject        # optional; who, for the audit line
```

Trust them **only** when the gateway is the sole route to this port (network
policy) and strips inbound copies of these headers — a client that can reach
the port directly can set any header it likes. A verifier and trusted headers
can coexist: a bearer, when present, wins.

### `allow_anonymous`

With identity configured, a request carrying none is refused. Setting
`auth.allow_anonymous: true` gives such requests the process identity instead.
Off by default: an unauthenticated path next to an authenticated one is how
scoping gets bypassed, and it also disables the identity-based exemption from
the loopback-only bind rule.

## Rate limits

Token buckets on tool calls (`search_tools` and `get_tool_details` are free),
keyed by the caller's subject. A refused call is a `RATE_LIMITED` isError result
with a retry-after — a continuation prompt the agent can act on, not a protocol
error — and never reaches the upstream, the breaker, or the usage prior.

```yaml
rate_limits:
  per_caller: { calls: 600, per_s: 60 }   # a caller's total tool calls
  per_tool:   { calls: 120, per_s: 60 }   # per (caller, tool)
overlays:
  - tool: vendor.expensive_call
    rate_limit: { calls: 30, per_s: 60 } # this tool across ALL callers (a vendor quota)
```

`calls` is also the burst; the bucket refills at `calls / per_s` per second.
Every anonymous caller shares one bucket — over HTTP without identity, that is
the whole port.

## Audit log

`log_file` writes one NDJSON line per event — `search`, `details`, `call`,
`paginate`, `workflow`, `breaker`, `rate_limited`, `shadow` — to a file, or to
`stderr` for containers (where the platform's log shipper forwards it to the
SIEM; stdout is never an option because on stdio it *is* the protocol). Every
line carries `ts_ms`, `event`, and `caller` (the subject, or `anonymous`); a
`call` adds the tool, its upstream and origin, `is_error`, `cached`,
`latency_ms`, and `result_bytes`. Arguments are deliberately never logged: they
carry customer data and injected secrets.

## Secrets

Anywhere a config value is a credential — upstream `headers`, `oauth.client_secret`,
a spec upstream's `api_key`, `auth.jwt_secret`, an overlay's `headers`, a
`user_supplied` field's source — it is a **reference**, never the value:

```yaml
${NAME} / ${env:NAME}             the process environment
${file:/run/secrets/api-key}      a file — Kubernetes and Docker secret mounts
${vault:path/to/secret#field}     HashiCorp Vault / OpenBao KV v2
```

`env` and `file` need no configuration and are re-read on every use, so a
rotated mounted secret reaches a running process. Vault needs:

```yaml
secrets:
  vault:
    address: https://vault.example.com:8200   # default: $VAULT_ADDR
    namespace: team-a                          # Vault Enterprise, optional
    mount: secret                              # KV v2 mount (default)
    ca_cert: /etc/ssl/private-ca.pem           # default: $VAULT_CACERT
    auth:
      kubernetes: { role: minmcp }             # pod service-account login (default jwt_path / mount)
      # approle: { role_id: "${VAULT_ROLE_ID}", secret_id: "${VAULT_SECRET_ID}" }
      # token_env: VAULT_TOKEN                 # a static token (the default source)
  cache_ttl_s: 300                             # Vault reads are cached this long
```

Vault is reached through [`vaultrs`](https://crates.io/crates/vaultrs). AppRole
and Kubernetes logins re-authenticate when Vault rejects the token (403), so a
token TTL shorter than the process is fine. Resolved secrets live in memory as
`SecretString`s (no Debug output, zeroed on drop). An unresolvable reference is a
startup error — no empty credential is ever sent.

## Upstream kinds and their auth

min-mcp proxies three kinds of upstream (full config in
[Configuration](configuration.md#upstreams)):

| kind | key | auth |
|---|---|---|
| MCP server subprocess | `command:` | child `env:` (literal; also inherits min-mcp's env) |
| Remote MCP server (HTTP) | `url:` | `headers:` (`${…}` references) or `oauth:` |
| OpenAPI spec | `spec:` | `auth_env:` (an env var's name) or `api_key:` (a `${…}` reference) |

### Header auth for remote upstreams

```yaml
upstreams:
  - name: remote
    url: https://mcp.example.com/mcp
    headers:
      Authorization: "Bearer ${REMOTE_TOKEN}"
```

### Outbound OAuth (client-credentials)

For an OAuth-protected remote MCP upstream, min-mcp can fetch, cache, and refresh
the bearer token itself:

```yaml
upstreams:
  - name: remote
    url: https://mcp.example.com/mcp
    oauth:
      token_url: https://auth.example.com/oauth/token
      client_id: my-client
      client_secret: "${OAUTH_SECRET}"
      scope: "read write"        # optional
```

The token is refreshed shortly before expiry (single-flight under concurrency),
so you never mint one by hand. See
[`examples/oauth-upstream.yaml`](../examples/oauth-upstream.yaml).

## Upstream health

- **A subprocess that exits is respawned** on the next call, re-initialized,
  and the call proceeds. The call that was in flight when it died returns an
  `UPSTREAM_ERROR` result (with the write-safety guidance), never a protocol
  error. A child that dies within a second of starting is a crash loop: callers
  get a clear error until the window passes, not a fork storm.
- **A remote MCP session the server has expired** (HTTP 404) is re-initialized
  once and the request retried.
- **`optional: true`** on an upstream skips it with a warning when it can't
  start, instead of failing the whole proxy.
- **The catalog is a startup snapshot.** If an upstream announces
  `notifications/tools/list_changed`, min-mcp flags it in `inspect` as
  `upstreams_stale` and logs a warning; picking up the new tools takes a
  restart.

## `scopes` vs `filters`

- **`filters`** (config) decide what is passed through *at all*, for everyone — a
  filtered tool is never even connected. Use it to drop whole APIs or dangerous
  families (`stripe.Delete*`). See [Configuration](configuration.md#filters).
- **`scopes`** (per caller) decide what *this caller* sees among what survives
  filtering.

## Security notes

- Agent-supplied path params on spec upstreams are strictly segment-encoded, so a
  value like `../` can't escape its endpoint.
- HTTP serving is loopback-only unless every request must authenticate or
  `--allow-remote` is given, and validates **both** DNS-rebinding headers: the
  `Host` header (by rmcp) and the `Origin` header (min-mcp — a *present* Origin
  must be loopback; an absent one is allowed, since non-browser clients don't
  send one). A cross-origin POST is refused with 403 and logged.
- A failed upstream call — transport error, timeout, or the upstream's own
  `isError` — always comes back to the agent as an `isError` tool result with
  guidance, never as a JSON-RPC protocol error, so a dead upstream is something
  the model can route around rather than a hard stop.
- Secrets are only ever referenced (`${env:…}`, `${file:…}`, `${vault:…}`);
  nothing sensitive belongs in the committed config, and nothing resolved is
  ever logged.
- Not built: TLS termination on the listener (put it on the gateway), per-tenant
  isolation in one process (run one process per tenant behind the gateway), and
  binding an HTTP session to the identity that opened it.
