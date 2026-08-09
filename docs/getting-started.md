# Getting started

## Install

min-mcp is a single Rust binary. Build it with a stable Rust toolchain:

```sh
git clone https://github.com/andrewgoodchild/min-mcp && cd min-mcp
cargo build --release
# binary at ./target/release/minmcp
```

There are no runtime services to stand up — min-mcp is a proxy your agent talks
to over stdio or HTTP. Every command below runs from the repo root; if you'd
rather type `minmcp` than `./target/release/minmcp`, install it on your PATH
with `cargo install --path .`.

## 1. See it work — no network, no credentials

The repo bundles a 120-operation OpenAPI spec so you can watch the
minification before wiring anything up:

```sh
./target/release/minmcp inspect --config bench/bigapi.yaml
```

```json
{
  "mode": "ThreeTool",
  "upstreams_configured": 1,
  "upstreams_active": 1,
  "upstream_tools": 120,
  "visible_after_scopes": 120,
  "surface_tools": 3,
  "est_tokens_raw": 17350,
  "est_tokens_minified": 424,
  "est_raw_exact": false
}
```

**120 upstream tools → 3 surface tools; ~17,350 tokens of definitions → 424.**
(`est_raw_exact: false` means the raw figure is a lower bound — spec upstreams
store unresolved `$ref` stubs until a schema is asked for, so the real "before"
number is larger.)

Now serve it. min-mcp prints one honest line to stderr and then speaks MCP on
stdout:

```sh
./target/release/minmcp serve --config bench/bigapi.yaml
```

```
min-mcp: 120 upstream tool(s) across 1 upstream(s) → 3 surface tool(s); ~424 tokens vs ≥17350 raw (≥41×)
```

That's the whole product in one line. `Ctrl-C` to stop.

## The mental model

You point min-mcp at one or more **upstreams** (MCP servers or OpenAPI specs)
via a YAML config; you point your **agent** at min-mcp. Instead of every
upstream's full tool catalog, the agent sees three tools:

```
search_tools(query)                  → ranked tool ids + one-line summaries
get_tool_details(tool_id)            → the full schema, on demand (the source map)
call_tool(tool_id, args, fields?)    → routes to the owning upstream, optionally
                                       projecting the response down to `fields`
```

See [Concepts](concepts.md) for why this is the shape that works — and when it
*isn't*: below roughly a dozen upstream tools, three meta-tools cost more than
just declaring everything, so use `mode: passthrough`. The startup banner tells
you when you're in that regime.

## 2. Explore the surface from the CLI

You don't need an agent to poke at it — the CLI mirrors the three tools:

```sh
./target/release/minmcp search --config bench/bigapi.yaml "create a widget"
```

```
big.widgets/create — widgets/create: POST /widgets — Create a widget.
big.widgets/list — widgets/list: GET /widgets — List widgets.
big.widgets/get — widgets/get: GET /widgets/{id} — Get a widget by id.
…
```

```sh
./target/release/minmcp help --config bench/bigapi.yaml "big.widgets/create"
```

…prints that one tool's full description and input schema — what
`get_tool_details` would hand the agent. `minmcp call` is the third
(it needs a reachable upstream; the bundled specs point at a placeholder
`base_url`). Full list: [CLI reference](cli.md).

## 3. Proxy a real MCP server

Name the server's launch command; min-mcp spawns it and speaks MCP to it:

```yaml
# myconfig.yaml
mode: three_tool
upstreams:
  - name: files
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

```sh
./target/release/minmcp inspect --config myconfig.yaml   # how many tools, how few tokens
./target/release/minmcp serve   --config myconfig.yaml   # speak MCP over stdio
```

Then point your MCP client at `minmcp serve --config myconfig.yaml` as the
command instead of the server itself. Add more `upstreams:` entries and they
federate behind the same three tools — one search index across all of them.
See [`examples/proxy-mcp-server.yaml`](../examples/proxy-mcp-server.yaml).

## 4. Mount your own OpenAPI spec

min-mcp can turn a raw OpenAPI document into the same surface with no server to
run. The API key is read from a named env var, never stored in the config:

```yaml
mode: three_tool
upstreams:
  - name: stripe
    spec: ./stripe.json          # path is relative to THIS config file
    base_url: https://api.stripe.com
    auth_env: STRIPE_TEST_KEY
```

Stripe's spec is the scale demonstration — 587 operations to 3 tools. It isn't
bundled (it's a large third-party document), so download it from
[github.com/stripe/openapi](https://github.com/stripe/openapi), save it next to
your config as `stripe.json`, and:

```sh
STRIPE_TEST_KEY=sk_test_... ./target/release/minmcp inspect --config examples/stripe-from-spec.yaml
```

Request bodies are encoded from the spec's declared media type (form vs JSON),
so the nested-body calls that break a generated client work here.

## 5. Serve over HTTP instead of stdio

```sh
# Streamable HTTP transport; binds localhost, validates Origin:
./target/release/minmcp serve --http 127.0.0.1:8080 --config myconfig.yaml
```

See [Transports & auth](transports-and-auth.md) for remote upstreams, outbound
OAuth, and JWT-derived caller scopes.

## 6. Fix a tool you don't own

The other half of min-mcp: upstream tools are often underdocumented or return
unhelpful errors, and you can't edit someone else's server. An **overlay**
patches a tool as it passes through — mark an undocumented parameter required,
constrain a value, rewrite an error into a recovery hint, strip a secret from a
response. The repo ships a config demonstrating every overlay feature against
the bundled 4-operation spec:

```sh
./target/release/minmcp help --config examples/demo-overlays.yaml acme.widgets/create   # the patched schema
./target/release/minmcp map  --config examples/demo-overlays.yaml                       # which tools an overlay touched
./target/release/minmcp lint --config examples/demo-overlays.yaml                       # best-practice smells
```

Read [`examples/demo-overlays.yaml`](../examples/demo-overlays.yaml) — it is
annotated line by line — then [Overlays](overlays.md).

## Next

- [Concepts](concepts.md) — how minification works, and the two surface modes.
- [Configuration reference](configuration.md) — every config key.
- [Overlays](overlays.md) — patch and reshape tools you don't own.
