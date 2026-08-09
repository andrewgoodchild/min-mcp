# Measurements

min-mcp's claim is falsifiable — "radically smaller *and* behaviour-preserving"
— so it gets measured rather than asserted. This page collects the numbers, and
separates the ones you can reproduce from a clean clone from the ones that
needed live APIs and a model.

## Reproduce from a clean clone

No network, no credentials, no model:

```sh
./bench/compression.sh
```

```
| spec                        | tools    | raw tokens | minified | compression |
|-----------------------------|----------|-----------:|---------:|------------:|
| acme-store (bundled, 4 ops) | 4 → 3    |        498 |      424 |          1× |
| bigapi (bundled, 120 ops)   | 120 → 3  |     17,350 |      424 |         41× |
```

The same assertions run in CI as `tests/compression.rs`, so the headline can't
silently rot: the surface must stay 3 tools, and the minified token count must
stay flat as the upstream grows. Note the 4-operation row — at that size the
minifier is *not* a win, which is why `mode: passthrough` exists and why the
startup banner says so out loud.

`est_tokens_raw` is a lower bound for spec upstreams (`est_raw_exact: false`):
schemas stay as unresolved `$ref` stubs until something asks for one, so the
real "before" figure is larger than reported.

## Reproduce with a public MCP server

GitHub's official MCP server, all toolsets enabled (needs Docker and a GitHub
token; `GITHUB_TOOLSETS` defaults to a smaller set — 44 tools, 26×):

```yaml
# gh-mcp.yaml
mode: three_tool
upstreams:
  - name: github
    command: docker
    args: ["run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN", "-e", "GITHUB_TOOLSETS",
           "ghcr.io/github/github-mcp-server"]
    env:
      GITHUB_TOOLSETS: "all"
```

```sh
GITHUB_PERSONAL_ACCESS_TOKEN=ghp_… ./target/release/minmcp inspect --config gh-mcp.yaml
```

**85 tools → 3; ~20,822 tokens of definitions → ~424 (49×)** — measured
2026-08-09. That is one server; the interesting part is that the 424 doesn't
move when you add more.

## Scale

- **A naive 2-server stack** (Stripe 587 + GitHub 1,216 = 1,803 tools) is
  **~687K tokens of tool definitions — it doesn't fit a context window at all.**
  min-mcp collapses it to a flat **~424 tokens (~1,600×)**, constant however
  many upstreams you add.
- **Microsoft Graph's 17,531 operations** (a 42 MB spec, ~3.26M raw tokens)
  mount, index, and become searchable in **~1.9s**, via lazy schema
  resolution — one tool's schema is resolved when asked for, not all 17k at
  load.

## Behaviour preservation (live tasks, real APIs)

These used a cheap flash-tier model with **programmatic state verification** —
every assertion reads live API state or follows ids saved at setup; no LLM
judges anywhere.

- **Lossless relative to the upstream.** Proxying a 587-tool Stripe server,
  min-mcp reproduced its task-success set *exactly* while collapsing 587
  definitions to 3 — **58× fewer tokens** end-to-end — and ran in a
  configuration where the raw surface exceeds the provider's
  function-declaration cap.
- **The spec converter generalises.** Mounted from OpenAPI directly: Stripe
  **10/10** tasks and GitHub **6/6** read-only tasks, including the
  nested-body encoding that breaks generated clients.
- **Composites.** A `workflows:` chain collapsed a 3-step task to one call —
  **6.8× fewer tokens** at equal success.
- **Structured errors beat prose.** On a deliberately broken call, a weak agent
  recovered **0% → 100%** when the error carried `{field, allowed_values, fix}`
  instead of a prose hint.
- **Search beats an always-visible listing, end to end.** On ten tasks authored
  specifically to make search miss, the search surface scored **10/10 at ~20K
  tokens/task** against **9/10 at ~331K** for a full-catalog listing. Details:
  [About mcp-compressor](about-mcp-compressor.md).

## The harness catches our own overfits

Three features were built, measured, and *removed* because they lost:

- **`hotset`** (promoting a usage-mined working set) won on the distribution its
  list was mined from, then **lost by 64%** on a held-out task set — promoted
  but unused declarations ride along every turn.
- **`pd`** (uniform progressive disclosure) kept one declaration per tool, which
  exceeded the provider cap and lost on both tokens and success.
- **TOON** as a result format cost *more* tokens than compact JSON on real
  nested payloads — see [About TOON](about-toon.md).

Publishing those is the point: a benchmark that only ever confirms its author is
not a benchmark.

## What is *not* measured here

Read this before trusting any number above:

- **Live-task results are one model tier** (flash-class) on **one API family**
  (Stripe-shaped, plus read-only GitHub), with single runs per condition unless
  stated. They establish direction, not confidence intervals.
- **The live task harness is not in this repo** — the reproducible artefacts are
  `bench/compression.sh`, `tests/compression.rs`, and the GitHub MCP example
  above.
- **Untested in public CI**: the Streamable HTTP transport, `workflows:`,
  auto-pagination, `minmcp verify` against a live upstream, and JWT/OAuth flows
  end-to-end. Their logic is unit-tested (see `cargo test`), but there is no
  fixture exercising them over the wire, and CI runs on Linux only.
- **`timeout_s` and `breaker`** have unit and stdio end-to-end tests proving the
  mechanism; their *impact* on agent token spend is not yet measured.
