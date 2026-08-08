# min-mcp — minify your MCP servers, and fix them on the way through

![License](https://img.shields.io/badge/license-Apache--2.0-blue) ![Language](https://img.shields.io/badge/rust-2021-orange) ![MCP](https://img.shields.io/badge/MCP-2025--06--18-6E56CF)

A minifying proxy for [Model Context Protocol](https://modelcontextprotocol.io)
servers, written in Rust. Point min-mcp at your MCP servers (or straight at an
OpenAPI spec); point your agent at min-mcp. Instead of every server's full tool
catalog in every request, the agent sees a small, searchable, scope-filtered
surface — with a source map back to every original tool.

```
your agent ──► minmcp ──► server A (89 tools)
   sees            │────► server B (587 tools)
  3 tools          └────► an OpenAPI spec (1,216 operations)
```

It is a *minifier*, in the JavaScript sense: the output is radically smaller and
behaviour-preserving, and there's a source map (`get_tool_details`) back to the
full original. The claim is falsifiable, and we held it to that: the numbers
below were measured with a benchmark that ran real tasks against real APIs and
checked the resulting state — no LLM judges. (The benchmark harness is developed
privately; the reported results and the shipped `minmcp verify` are what this
repo carries.)

## Why

Two things break as teams adopt MCP at scale, and both are widely documented:

**1. The context tax.** Tool definitions are loaded into every request, unconditionally.
GitHub's MCP server alone is [~42–55K tokens of schema](https://getunblocked.com/blog/mcp-token-budget-autopsy/);
a typical [5–10 server stack burns 100,000–200,000 tokens before the user types a
character](https://agentmarketcap.ai/blog/2026/04/08/mcp-context-bloat-enterprise-scale-tool-definitions-agent-context-budget).
Priced out, that "context tax" runs into the [tens of thousands of dollars a year](https://getunblocked.com/blog/mcp-token-budget-autopsy/)
for a mid-size team — before any productive work. And a 587-tool surface simply
*can't attach* to some models (Google's 512-function-declaration cap). The usual
fixes trade one problem for another: hand-curating loses coverage; converting an
API 1:1 is unrunnable (provider caps), invalid (rejected schemas), or incorrect
(dropped nested-body encoding).

**2. The tools you depend on are broken.** You don't own the upstream servers, and
the ecosystem is rough — around [half of registry servers are effectively dead](https://rapidclaw.dev/blog/mcp-servers-dead-what-it-means-2026),
descriptions routinely diverge from behavior, and by week two of production you
hit *"errors the model can't act on… tools silently disappearing… responses
exceeding size limits."* ([StackOne](https://www.stackone.com/blog/mcp-where-its-been-where-its-going/))
You can't edit someone else's server — so today you fork it or paper over it in
every agent's prompt.

min-mcp addresses both: it **minifies** the surface (all tools stay reachable, the
agent sees a small searchable set — and it trims bloated *responses* too), and it
**fixes** the tools you don't own (patch descriptions, rewrite unhelpful errors,
reshape responses) as a versioned, drift-checked overlay instead of a fork.
→ [Concepts](docs/concepts.md)

## How it works

**1. Minify — the 3-tool surface.** In the default `three_tool` mode the agent
sees exactly three tools, regardless of how big the upstreams are:

- `search_tools(query, k)` — BM25 over every upstream tool → ranked ids + summaries
- `get_tool_details(tool_id)` — the full schema on demand (the source map)
- `call_tool(tool_id, arguments, fields)` — routes to the owning upstream, runs the
  call, and — with the optional **`fields`** filter — projects the response down to
  just the dotted paths you ask for (`["data[].id", "data[].amount"]`), GraphQL-style,
  so a big result doesn't flood the context. Omit `fields` for the whole response.

It searches for the operation it needs, pulls that one schema, calls it, and keeps
only the fields it asked for. You point min-mcp at upstreams two ways — **proxy
running MCP servers**, or **mount an OpenAPI spec directly** — through one config
and one interface.

**2. Fix — overlays.** The tools you're proxying are often broken (undocumented
required params, unhelpful errors, bloated responses), and you can't edit someone
else's server. An **overlay** patches a tool *as it passes through the proxy* — you
name a tool id and override the parts you want:

```yaml
overlays:
  - tool: github.issues/create
    description: "Create an issue. Requires owner, repo, and title."
    fields:
      body.title: { required: true, description: "issue title; ask the user, never invent" }
    error_hints:
      - contains: "404"
        hint: "Repo not found — re-check owner/repo; don't retry with a guess."
```

Overlays can rewrite descriptions and errors, patch the input schema
(mark params `required`, add enums/examples, `hide` noise), inject request
`defaults`/`headers` (with `{{uuid}}`/`{{hash}}` generators), auto-follow
pagination, and reshape responses — each a versioned, drift-checked binding you
can prove with `minmcp verify`, instead of forking the server or patching every
agent's prompt. → [Overlays](docs/overlays.md)

**3. Also.**

- **Stop fabrication, structurally.** Beyond documenting a field, `user_supplied`
  strips it from the agent's schema (injected from the session/env, so it can't be
  invented), and structured errors + opt-in `preflight:` validation hand back
  `{field, allowed_values, fix}` — which took a weak agent from **0%→100%** recovery
  on a broken call where prose guidance did nothing. [Overlays](docs/overlays.md)
- **Detect → verify.** `minmcp lint` flags best-practice smells (thin descriptions,
  undocumented-required params, confusable tools); `minmcp verify` proves a fix with
  real calls and deterministic assertions — a CI / behavioural-drift gate. Detecting
  and grading tools is crowded; *fixing a server you don't own and verifying it* isn't.
- **Compose.** A `workflows:` entry runs a fixed multi-step chain as one composite
  tool — a measured composite cut a 3-step task **6.8×** on tokens. [Composites](docs/composites.md)
- **Minify responses too.** Beyond the `fields` filter, overlays reshape any response
  server-side (strip secrets/PII/noise, rename, project) — over *any* API, even ones
  with no field-selection of their own.
- **Transports & auth.** Serve over stdio or Streamable HTTP; proxy a local
  subprocess, a remote MCP server over HTTP, or a spec — with per-upstream headers,
  outbound OAuth, and JWT-derived caller scopes. [Transports & auth](docs/transports-and-auth.md)
- **Gate visibility.** Static `filters:` drop whole APIs or tool families for
  everyone; per-caller `scopes:` hide what a JWT doesn't grant — hidden tools are
  never listed, searched, or callable. A `passthrough` mode federates tools directly
  when the surface is already small.

## Quickstart

```sh
cargo build --release
```

```sh
# a) Proxy an MCP server (edit the config for your server):
./target/release/minmcp serve --config examples/proxy-mcp-server.yaml

# b) Or mount an API spec directly — point `spec:` at any OpenAPI file
#    (e.g. Stripe's from github.com/stripe/openapi), 587 operations → 3 tools:
STRIPE_TEST_KEY=sk_test_... ./target/release/minmcp inspect --config examples/stripe-from-spec.yaml
```

Explore the surface without an agent — the CLI mirrors the three tools:

```sh
minmcp search --config examples/stripe-from-spec.yaml "create a customer"
minmcp help   --config examples/stripe-from-spec.yaml stripe.PostCustomers
```

Full walkthrough → [Getting started](docs/getting-started.md).

## Proof it works

Measured on a cheap flash-tier model, with tasks verified against live API
state — no LLM judges. The **compression numbers are reproducible right here**:
`cargo build --release && ./bench/compression.sh` (deterministic, offline, no
key), locked in CI. See [`bench/`](bench/).

- **The context tax, measured — and it scales to the extreme.** A naive 2-server
  stack (Stripe + GitHub, 1,803 tools) is **~687K tokens of tool definitions — it
  doesn't even fit** a context window; min-mcp collapses it to a flat **372 tokens**
  (**1,847×**), constant no matter how many servers you add. It even mounts
  **Microsoft Graph's 17,531 operations** (a 42 MB spec, ~3.26M raw tokens) — loaded,
  indexed, and searchable in **~1.9s** via lazy schema resolution.
- **Lossless minification.** Proxying a 587-tool Stripe server, min-mcp reproduces
  its task-success set *exactly* while collapsing 587 definitions to 3 — **58×
  fewer tokens** end-to-end — and runs where the raw surface exceeds the provider cap.
- **The Rust spec converter works on very different APIs.** Stripe **10/10** and
  GitHub (1,216 ops → 3 tools) **6/6**, with correct nested-body encoding.
- **Fix a broken server without forking it.** A third of popular MCP servers ship
  tools with undocumented parameters (agents then invent required values). One
  overlay turns GitHub's terse `issues/create` into a documented tool — `owner`/
  `repo`/`title` marked required, errors rewritten into recovery hints — as a
  drift-checked binding, no fork.
- **Composites collapse a chain to one call — 6.8× fewer tokens** at equal success.
- **The harness catches overfits — including our own.** A held-out set reversed an
  early `hotset` win; `hotset`, `pd`, and TOON were measured, then *removed*.

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | build, proxy a server, mount a spec, serve |
| [Concepts](docs/concepts.md) | the minifier model, surface modes, the source map |
| [Configuration](docs/configuration.md) | every YAML key, with examples |
| [Overlays](docs/overlays.md) | patch and reshape tools you don't own |
| [Composites](docs/composites.md) | multi-step chains as one tool, and their safety |
| [Transports & auth](docs/transports-and-auth.md) | stdio/HTTP, OAuth, JWT scopes |
| [CLI reference](docs/cli.md) | every command and flag |

## Status

v0.1: **stdio and Streamable-HTTP transports**, both served by the official MCP
SDK ([`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)) wrapping min-mcp's
surface behind its `ServerHandler` — so protocol conformance (version negotiation,
sessions, SSE, Host-header/DNS-rebinding defence) tracks the SDK, and requests are
handled concurrently (a health-check `ping` is answered while a slow `tools/call`
is in flight); **three upstream kinds** — MCP-server subprocess, remote MCP server
over HTTP, and mounted OpenAPI spec; per-upstream auth headers with `${ENV}`
expansion and **outbound OAuth**; two surface modes (`three_tool`, `passthrough`).
**Overlays** now patch descriptions/errors, the **input schema** (`required`/
example/enum/type/hide/**`user_supplied`** via dotted paths), request `defaults`,
**per-endpoint `headers`** (static + `${ENV}` + `{{uuid}}`/`{{now}}`/`{{iso8601}}`/
`{{hash}}` generators), **auto-pagination**, `retryable` error tags, **structured
errors** (`error_hints` `field:` pointer + opt-in `preflight:` local validation),
response transforms, and search `aliases` — each a drift-checked binding. Also: composite `workflows:`;
caller field projection; static include/exclude filters; JWT-derived caller scopes
(**HS256, RS256, JWKS**); path-param injection hardening; `tracing`-based leveled
logging (`MINMCP_LOG`). Tooling: `minmcp lint` (best-practice smells + stats),
`minmcp verify` (deterministic checks against the live upstream — CI/drift gate),
`minmcp map` (source map / drift diff), `minmcp search|help|call`. Not yet built: field *addition* for prose-only bodies,
async-poll and interactive-OAuth overlays, write-verified GitHub tasks.

## How it compares

The deferred-schema pattern — show a small surface, fetch a tool's full schema on
demand, then invoke it — is not unique. Atlassian's
[mcp-compressor](https://github.com/atlassian-labs/mcp-compressor) is the closest
peer: it uses the **same architecture** (a compressed surface + `get_tool_schema` /
`invoke_tool`). min-mcp differs three ways — it *ranks* tools with **BM25 search**
(`search_tools(query)`) rather than explicit name lookup; it reports **measured,
reproducible** compression (run [`bench/`](bench/) yourself) rather than qualitative
claims; and — the one thing neither it nor the broader pattern
([ACI.dev](https://github.com/aipotheosis-labs/aci), Cloudflare Code Mode, AWS
`call_aws`) does — it also **fixes** the tools it proxies.

That *fixing* side is min-mcp's real column, and it has different peers: MCP quality
graders like [mcpgrade](https://mcpgrade.com/methodology), glama's Agent-UX
validator, and security scanners like mcp-scan. They all **detect and grade**; none
patch a server they don't own, and none prove a fix works. min-mcp's `lint` shares
their detection lane (we're not first there) — but the overlay + **`verify`** loop —
*fix* a third-party tool as a drift-checked binding and *prove* it with a real
call — is the uncontested column.

## License

[Apache-2.0](LICENSE). Third-party material (the OpenAPI specs you mount, and the
Rust dependencies) is recorded in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
