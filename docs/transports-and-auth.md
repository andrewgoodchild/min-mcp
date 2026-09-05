# Transports & auth

## Serving the surface: stdio or Streamable HTTP

Both transports are served by the **official MCP SDK**
([`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)): min-mcp's surface is
wrapped behind rmcp's `ServerHandler`, so protocol conformance — version
negotiation, sessions, SSE, Host-header/DNS-rebinding defence — tracks the SDK
rather than a bespoke implementation. Protocol-level requests (`initialize`,
`ping`) are answered by the SDK concurrently, but everything that touches the
surface — `tools/list`, `tools/call`, resources, prompts — runs behind one lock,
one call at a time, across every session. A slow upstream call therefore holds
up the others for as long as it runs (at most the 120s transport ceiling), which
is what an overlay's [`timeout_s`](overlays.md#timeout_s-and-breaker--guard-a-slow-or-flaky-tool)
is for.

### stdio (default)

```sh
minmcp serve --config myconfig.yaml
```

Point your MCP client at this command. This is the usual local-agent setup.

### Streamable HTTP

```sh
minmcp serve --http 127.0.0.1:8080 --config myconfig.yaml
```

rmcp's `StreamableHttpService` — JSON-RPC over POST, SSE replies, session ids, and
Host-header validation — driven on a TCP listener by hyper. The MCP endpoint is
served at the root path (`/`).

**There is no inbound authentication on this transport, and no per-connection
identity.** The scopes are the process's (`--jwt` / `--scopes`, resolved once at
startup), so every client of one `serve --http` sees the same tools and calls
them with the same upstream credentials. That is why the listener is
**loopback-only**: a non-loopback bind (`0.0.0.0`, a LAN address, a public
hostname) is refused unless you pass `--allow-remote`, which belongs only behind
an authenticating reverse proxy or on a private network. Per-request bearer
validation is future work; until then stdio — one process per caller — is the
per-caller mode.

## Upstream kinds and their auth

min-mcp proxies three kinds of upstream (full config in
[Configuration](configuration.md#upstreams)):

| kind | key | auth |
|---|---|---|
| MCP server subprocess | `command:` | child `env:` (literal; also inherits min-mcp's env) |
| Remote MCP server (HTTP) | `url:` | `headers:` (`${VAR}` expands) or `oauth:` |
| OpenAPI spec | `spec:` | `auth_env:` names the key's env var |

### Header auth for remote upstreams

```yaml
upstreams:
  - name: remote
    url: https://mcp.example.com/mcp
    headers:
      Authorization: "Bearer ${REMOTE_TOKEN}"
```

`${VAR}` is expanded from the environment at connect time; an unset var is a hard
error (no empty credential is ever sent).

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

The token is refreshed shortly before expiry, so you never mint one by hand. See
[`examples/oauth-upstream.yaml`](../examples/oauth-upstream.yaml).

## Caller identity: JWT-derived scopes

min-mcp can gate *which tools a caller sees* by the scopes in their JWT. Tools a
caller's scopes don't allow never enter the context — not listed, not searchable,
not callable.

### 1. Configure a verifier (`auth:`)

Precedence is JWKS → RS256 public key → HS256 secret:

```yaml
auth:
  # pick one:
  jwt_secret: "${MINMCP_JWT_SECRET}"          # HS256 shared secret (or MINMCP_JWT_SECRET env)
  jwt_public_key: "-----BEGIN PUBLIC KEY-----\n..."   # RS256, inline PEM
  jwt_public_key_file: ./pub.pem              # RS256, from a file
  jwks_url: https://auth.example.com/.well-known/jwks.json  # JWKS, kid-selected
  scope_claim: scope                          # claim to read scopes from (default "scope")
  # audience: minmcp                          # if set, `aud` must contain this (else unchecked)
  # issuer: https://auth.example.com          # if set, `iss` must equal this (else unchecked)
```

Signature and `exp` are always checked. `audience` and `issuer` are off unless
set — an internal gateway with no audience discipline still works — but where
one issuer mints tokens for several services, set both so a token for another
service doesn't grant scopes here. A JWKS document is fetched once at startup
(30s timeout); key rotation needs a restart.

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

Tool patterns are exact (`up.tool`) or prefix (`up.Post*`).

### 3. Pass a caller token

```sh
minmcp serve --config myconfig.yaml --jwt "$CALLER_JWT"
# or, keeping the token out of argv / `ps` / shell history:
MINMCP_JWT="$CALLER_JWT" minmcp serve --config myconfig.yaml
```

The token's scope claim (validated against the configured verifier) becomes the
granted scopes. For local dev without JWTs, `--scopes billing.read,billing.write`
sets an identity directly (prefer `--jwt` in production).

Scopes are resolved **once per process**. Over stdio that is one caller, which
is the model this was built for; over HTTP it means every connecting client
shares them (see [Streamable HTTP](#streamable-http)).

## `scopes` vs `filters`

- **`filters`** (config) decide what is passed through *at all*, for everyone — a
  filtered tool is never even connected. Use it to drop whole APIs or dangerous
  families (`stripe.Delete*`). See [Configuration](configuration.md#filters).
- **`scopes`** (per caller) decide what *this caller* sees among what survives
  filtering.

## Security notes

- Agent-supplied path params on spec upstreams are strictly segment-encoded, so a
  value like `../` can't escape its endpoint.
- HTTP serving is loopback-only unless `--allow-remote` is given, has **no
  inbound authentication**, and validates **both** DNS-rebinding headers: the
  `Host` header (by rmcp) and the `Origin` header (min-mcp — a *present* Origin
  must be loopback; an absent one is allowed, since non-browser clients don't
  send one). A cross-origin POST is refused with 403 and logged.
- A failed upstream call — transport error, timeout, or the upstream's own
  `isError` — always comes back to the agent as an `isError` tool result with
  guidance, never as a JSON-RPC protocol error, so a dead upstream is something
  the model can route around rather than a hard stop.
- Secrets are only ever referenced by env-var name; nothing sensitive belongs in
  the committed config.
