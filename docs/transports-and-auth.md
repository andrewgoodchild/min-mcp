# Transports & auth

## Serving the surface: stdio or Streamable HTTP

Both transports are served by the **official MCP SDK**
([`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)): min-mcp's surface is
wrapped behind rmcp's `ServerHandler`, so protocol conformance — version
negotiation, sessions, SSE, Host-header/DNS-rebinding defence — tracks the SDK
rather than a bespoke implementation, and every request is handled on its own task
(a health-check `ping` is answered while a slow `tools/call` is still in flight).

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
Host-header validation (it binds localhost by default) — driven on a TCP listener
by hyper. The MCP endpoint is served at the root path (`/`).

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
```

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
```

The token's scope claim (validated against the configured verifier) becomes the
granted scopes. For local dev without JWTs, `--scopes billing.read,billing.write`
sets an identity directly (prefer `--jwt` in production).

## `scopes` vs `filters`

- **`filters`** (config) decide what is passed through *at all*, for everyone — a
  filtered tool is never even connected. Use it to drop whole APIs or dangerous
  families (`stripe.Delete*`). See [Configuration](configuration.md#filters).
- **`scopes`** (per caller) decide what *this caller* sees among what survives
  filtering.

## Security notes

- Agent-supplied path params on spec upstreams are strictly segment-encoded, so a
  value like `../` can't escape its endpoint.
- HTTP serving validates `Origin` and binds localhost by default.
- Secrets are only ever referenced by env-var name; nothing sensitive belongs in
  the committed config.
