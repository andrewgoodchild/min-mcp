# Getting started

## Install

min-mcp is a single Rust binary. Build it with a stable Rust toolchain:

```sh
git clone <this repo> && cd min-mcp
cargo build --release
# binary at ./target/release/minmcp
```

There are no runtime services to stand up — min-mcp is a proxy your agent talks
to over stdio or HTTP.

## The mental model

You point min-mcp at one or more **upstreams** (MCP servers or OpenAPI specs) via
a YAML config; you point your **agent** at min-mcp. Instead of every upstream's
full tool catalog, the agent sees three tools:

```
search_tools(query)          → ranked tool ids + one-line summaries
get_tool_details(tool_id)    → the full schema, on demand (the source map)
call_tool(tool_id, args)     → routes to the owning upstream
```

See [Concepts](concepts.md) for why this is the shape that works.

## 1. Proxy an MCP server

Create a config naming the server to proxy (see [`examples/proxy-mcp-server.yaml`](../examples/proxy-mcp-server.yaml)):

```yaml
mode: three_tool
upstreams:
  - name: myserver
    command: npx
    args: ["-y", "@some/mcp-server"]
```

Inspect the surface, then serve it:

```sh
./target/release/minmcp inspect --config myconfig.yaml   # stats, no serving
./target/release/minmcp serve   --config myconfig.yaml   # speak MCP over stdio
```

Point your MCP client at `minmcp serve --config myconfig.yaml` as the command.

## 2. Mount an OpenAPI spec directly

min-mcp can turn a raw OpenAPI document into the same 3-tool surface — no server
to run. The API key is read from a named env var, never stored in config
(see [`examples/stripe-from-spec.yaml`](../examples/stripe-from-spec.yaml)):

```yaml
mode: three_tool
upstreams:
  - name: stripe
    spec: ./stripe.json
    base_url: https://api.stripe.com
    auth_env: STRIPE_TEST_KEY
```

```sh
# download an OpenAPI spec to try — e.g. Stripe's, from github.com/stripe/openapi —
# and point the config's `spec:` at it, then:
STRIPE_TEST_KEY=sk_test_... ./target/release/minmcp inspect --config examples/stripe-from-spec.yaml
```

587 Stripe operations collapse to 3 tools. Request bodies are encoded from the
spec's declared media type (form vs JSON), so nested-body calls that break a
generated client just work.

## 3. Try the surface from the CLI

You don't need an agent to explore — the CLI mirrors the three tools:

```sh
minmcp search --config examples/stripe-from-spec.yaml "create a customer"
minmcp help   --config examples/stripe-from-spec.yaml stripe.PostCustomers
STRIPE_TEST_KEY=sk_test_... minmcp call --config examples/stripe-from-spec.yaml \
  stripe.PostCustomers --args '{"body":{"email":"a@b.com"}}'
```

See the full [CLI reference](cli.md).

## 4. Serve over HTTP instead of stdio

```sh
# Streamable HTTP transport; binds localhost, validates Origin:
minmcp serve --http 127.0.0.1:8080 --config myconfig.yaml
```

See [Transports & auth](transports-and-auth.md).

## Next

- [Concepts](concepts.md) — how minification works and why.
- [Configuration reference](configuration.md) — every config key.
- [Overlays](overlays.md) — patch and reshape tools you don't own.
