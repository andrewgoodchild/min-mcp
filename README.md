# min-mcp — fix the MCP servers you don't own, and minify them on the way through

[![CI](https://github.com/andrewgoodchild/min-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/andrewgoodchild/min-mcp/actions/workflows/ci.yml) ![License](https://img.shields.io/badge/license-Apache--2.0-blue) ![Language](https://img.shields.io/badge/rust-2021-orange) ![MCP](https://img.shields.io/badge/MCP-2025--06--18-6E56CF)

A repair-and-minification proxy for [Model Context Protocol](https://modelcontextprotocol.io)
servers, written in Rust. You don't own the servers your agents depend on, and you
can't edit them — so today you fork them, or paper over their flaws in every
agent's prompt. min-mcp patches them *as they pass through*: a versioned,
drift-checked **overlay** you can prove with `minmcp verify` in CI. On the way
through it also minifies the surface, so a 17,000-operation API arrives as three
searchable tools instead of a context bill. And it runs where your agents run:
behind the gateway you already have, with a caller identity per request, scopes
and rate limits per caller, and an audit line per call.

```
                          ┌─ overlay: patch schemas, rewrite errors, reshape responses
                          │           (drift-checked, verified in CI)
your gateway ──► minmcp ──► GitHub's MCP server (85 tools)
 (bearer or        │    │────► Stripe's OpenAPI spec (589 operations)
 identity headers) │    └────► Microsoft Graph's OpenAPI spec (17,531 operations)
                   └─ per request: who → which tools → how many calls → audit line
```

## Why

Two things break as teams adopt MCP at scale. One of them is now being solved for
you; the other isn't.

**1. The tools you depend on are broken, and you can't edit them.** The ecosystem
is rough — around [half of registry servers are effectively dead](https://rapidclaw.dev/blog/mcp-servers-dead-what-it-means-2026),
descriptions routinely diverge from behavior, and by week two of production you
hit *"errors the model can't act on… tools silently disappearing… responses
exceeding size limits."* ([StackOne](https://www.stackone.com/blog/mcp-where-its-been-where-its-going/))
Forking someone else's server means owning their release cadence forever; prompt
patches rot silently and can't be tested. Nothing in the platforms addresses this:
clients can now *find* tools well, but none of them can *fix* one.

**2. The context tax — increasingly handled by your client.** Tool definitions are
loaded into every request, and a typical [5–10 server stack burns 100,000–200,000
tokens before the user types a character](https://agentmarketcap.ai/blog/2026/04/08/mcp-context-bloat-enterprise-scale-tool-definitions-agent-context-budget).
Both major clients now defer loading and search on demand, so if you run a handful
of ordinary MCP servers, **your client may already cover this** — see
[About tool search](docs/about-tool-search.md) for what they do and where the
limits actually bite. min-mcp still matters at the top end: mounting an API that
has *no MCP server at all*, and catalogs past the size a client can defer.

min-mcp does both, in that order: it **fixes** the tools you don't own (patch
descriptions and schemas, rewrite unhelpful errors, reshape responses) as a
drift-checked binding instead of a fork, and it **minifies** the surface so every
tool stays reachable through a small searchable set.
→ [Concepts](docs/concepts.md)

## How it works

**1. Fix — overlays.** The tools you're proxying are often broken (undocumented
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

Overlays can rewrite descriptions and errors, patch the input schema (mark params
`required`, add enums/examples, `hide` noise), inject request `defaults`/`headers`
(with `{{uuid}}`/`{{hash}}` generators), guard a flaky tool (`timeout_s`,
`breaker`), auto-follow pagination, and reshape responses — each a versioned,
drift-checked binding you can prove with `minmcp verify`, instead of forking the
server or patching every agent's prompt. → [Overlays](docs/overlays.md)

**2. Minify — the 3-tool surface.** In the default `three_tool` mode the agent
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
and one interface. Search is BM25 over the [`bm25`](https://crates.io/crates/bm25)
crate behind a convention-aware tokenizer: `PostCheckoutSessions`, `read_file`, and
`list-pull-requests` are all findable by their words.

**3. Run it behind your gateway.** One `minmcp serve --http` serves many callers.
Each request is identified on its own — a bearer validated here (HS256 / RS256 /
JWKS, with `aud` / `iss` checks), or identity headers from a gateway that already
did the work — and sees only what its scopes grant: hidden tools are never listed,
searched, or callable. Token-bucket **rate limits** per caller, per caller-and-tool,
and per tool across callers refuse with a retry-after the agent can act on. Every
search, lookup, and call writes an NDJSON **audit line** naming the caller, the
tool, its origin, the outcome, latency, and size — to a file or to stderr for your
log shipper. Credentials are **references** into the environment, mounted secret
files, or HashiCorp Vault, never values in the config. A dead upstream is
respawned, not fatal. → [Transports & auth](docs/transports-and-auth.md),
[`examples/enterprise.yaml`](examples/enterprise.yaml)

**4. Also.**
- **Minify responses too.** Beyond the `fields` filter, overlays reshape any response
  server-side (strip secrets/PII/noise, rename, project) — over *any* API, even ones
  with no field-selection of their own.
- **Detect → verify.** `minmcp lint` flags best-practice smells (thin descriptions,
  undocumented-required params, confusable tools); `minmcp verify` proves a fix with
  real calls and deterministic assertions — a CI / behavioural-drift gate.
- **Compose.** A `workflows:` entry runs a fixed multi-step chain as one composite
  tool — a measured composite cut a 3-step task **6.8×** on tokens. [Composites](docs/composites.md)
- **Transports & auth.** Serve over stdio or Streamable HTTP; proxy a local
  subprocess, a remote MCP server over HTTP, or a spec — with per-upstream headers,
  outbound OAuth, and JWT-derived caller scopes. [Transports & auth](docs/transports-and-auth.md)
- **Gate visibility.** Static `filters:` drop whole APIs or tool families for
  everyone; per-caller `scopes:` hide what a caller's identity doesn't grant. A
  `passthrough` mode federates tools directly when the surface is already small.

## Quickstart

You need [Rust](https://rustup.rs) (stable) and a clone; there's nothing else to
install and no service to run:

```sh
git clone https://github.com/andrewgoodchild/min-mcp && cd min-mcp
cargo build --release          # a few minutes from cold; binary at ./target/release/minmcp
```

Prefer not to build? Prebuilt binaries for Linux, Apple Silicon macOS, and
Windows are on the [Releases](https://github.com/andrewgoodchild/min-mcp/releases)
page, or `cargo install --git https://github.com/andrewgoodchild/min-mcp` puts
`minmcp` on your PATH (Rust 1.91 or newer).

Everything below runs from the repo root. See it work on the bundled
120-operation spec — no network, no credentials:

```sh
./target/release/minmcp inspect --config bench/bigapi.yaml
# 120 upstream tools → 3 surface tools; ~17,350 tokens of definitions → ~424
```

Explore that surface without an agent — the CLI mirrors the three tools:

```sh
./target/release/minmcp search --config bench/bigapi.yaml "create a widget"
./target/release/minmcp help   --config bench/bigapi.yaml "big.widgets/create"
```

And see the headline feature — **fixing a tool you don't own** — on the bundled
demo config, annotated line by line (`examples/demo-overlays.yaml`):

```sh
./target/release/minmcp help --config examples/demo-overlays.yaml acme.widgets/create
# the schema the agent sees: overlay-patched — undocumented param now required,
# enum constrained, an env-injected field stripped so the agent can't fabricate it
./target/release/minmcp lint --config examples/demo-overlays.yaml   # find what needs fixing
./target/release/minmcp map  --config examples/demo-overlays.yaml   # which tools an overlay touched
```

Or point it at a real public MCP server — GitHub's official one, all toolsets
enabled (needs Docker and a GitHub token):

```sh
GITHUB_PERSONAL_ACCESS_TOKEN=ghp_... \
  ./target/release/minmcp inspect --config examples/github-mcp-server.yaml
# 85 upstream tools → 3 surface tools; ~20,822 tokens → ~424  (49×)
```

Then point it at your own upstreams — MCP servers you already run, or a spec:

```sh
# a) Proxy MCP servers (copy the template and edit it for your server):
./target/release/minmcp serve --config examples/proxy-mcp-server.yaml

# b) Or mount an OpenAPI spec directly — every operation in Stripe's spec → 3 tools.
#    Download it from github.com/stripe/openapi next to the config as stripe.json:
STRIPE_TEST_KEY=sk_test_... ./target/release/minmcp inspect --config examples/stripe-from-spec.yaml
```

And the deployment shape — behind a gateway, many callers, one process. The
bundled config trusts identity headers a gateway would set; a request without
them is refused, and two callers with different scopes see two different surfaces:

```sh
./target/release/minmcp serve --http 0.0.0.0:8080 --config examples/enterprise.yaml
# accepted without --allow-remote: every request must carry identity
curl -s -o /dev/null -w '%{http_code}\n' -d '{...initialize...}' http://localhost:8080/   # 401
curl -s -H 'X-Auth-Scopes: store.read' -H 'X-Auth-Subject: alice' ...                 # sees list/get
# stderr: {"ts_ms":…,"event":"search","caller":"alice","query":"widget","k":10}
```

Full walkthrough → [Getting started](docs/getting-started.md).

## Proof it works

The minification claim is falsifiable, so it gets measured (the fixing side —
structured errors taking a weak agent 0%→100%, a composite cutting a task 6.8× —
is measured too; see [Measurements](docs/measurements.md)):

- **85 tools → 3** proxying GitHub's official MCP server: ~20,822 tokens of
  definitions → **~424** (49×). Add more upstreams and the 424 does not move.
- **A Stripe + GitHub stack (1,803 tools) is ~687K tokens** — it does not fit a
  context window; min-mcp serves it in ~424.
- **Behaviour preserved:** proxying a 587-tool Stripe server reproduced its
  task-success set *exactly* at **58× fewer tokens**, verified against live API
  state with no LLM judges.
- **Reproducible right here:** `./bench/compression.sh` (offline, no key),
  asserted in CI.

Full numbers, reproduction steps, and what is *not* measured →
[Measurements](docs/measurements.md).

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | build, proxy a server, mount a spec, serve |
| [Concepts](docs/concepts.md) | the minifier model, the three-tool surface, the source map |
| [Configuration](docs/configuration.md) | every YAML key, with examples |
| [Overlays](docs/overlays.md) | patch and reshape tools you don't own |
| [Composites](docs/composites.md) | multi-step chains as one tool, and their safety |
| [Transports, identity & ops](docs/transports-and-auth.md) | stdio/HTTP, per-request caller identity (JWT or gateway headers), rate limits, the audit stream, secrets (env/file/Vault), upstream health, what is deliberately not built |
| [CLI reference](docs/cli.md) | every command and flag |
| [About tool search](docs/about-tool-search.md) | **read this first** — what Claude and Codex do natively, Atlassian's peer proxy measured head-to-head, and when min-mcp fits alongside an agent with built-in catalog search |
| [About TOON](docs/about-toon.md) | what it is, what we measured, why we don't emit it |
| [Measurements](docs/measurements.md) | every number, how to reproduce it, and what isn't measured |

## License

[Apache-2.0](LICENSE). Third-party material (the OpenAPI specs you mount, and the
Rust dependencies) is recorded in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
