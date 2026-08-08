# Benchmark

A **deterministic, offline** benchmark you can run yourself — no network, no
credentials, no LLM. It reproduces min-mcp's headline claim: a large tool surface
collapses to a small, constant one.

## Run it

```sh
cargo build --release
./bench/compression.sh
```

Example output:

| spec | tools | raw tokens | minified | compression |
|---|---|---:|---:|---:|
| acme-store (bundled, 4 ops) | 4 → 3 | 498 | 372 | 1× |
| bigapi (bundled, 120 ops) | 120 → 3 | 17,350 | 372 | 47× |

## What it measures

`minmcp inspect` reports two numbers for any mounted spec:

- **`est_tokens_raw`** — the token cost if every operation were its own MCP tool
  (the naive one-tool-per-endpoint baseline every client pays today).
- **`est_tokens_minified`** — the cost of the 3-tool surface the agent actually
  sees (`search_tools` / `get_tool_details` / `call_tool`).

The ratio is the compression. The load-bearing property: **minified cost is ~flat
(372 tokens here) no matter how big the upstream is** — 4 ops or 120 ops, the
agent pays the same — whereas the naive cost scales with the operation count.
[`bench/specs/bigapi.json`](specs/bigapi.json) is a synthetic 120-operation spec
bundled so this runs with nothing but the repo.

Reproduce the headline number on a real API by downloading its OpenAPI spec (e.g.
Stripe's 587 operations from `github.com/stripe/openapi`), placing it next to
`examples/stripe-from-spec.yaml` as `stripe.json`, and uncommenting the `stripe`
line in `compression.sh`.

This measurement is also locked in CI — see [`tests/compression.rs`](../tests/compression.rs).

## Scope (what this does and doesn't cover)

- ✅ **Compression** (this benchmark) — deterministic, reproducible by anyone.
- ✅ **A fix works** — falsify per-overlay claims yourself with **`minmcp verify`**,
  which makes real calls against your own upstream and asserts on the result.
- ⚠️ **Agent effectiveness** (task success, error-recovery, anti-fabrication rates
  reported in the README) were measured with a private harness that drives a real
  LLM against real APIs — reproducing those needs your own model key and a few
  dollars of spend, so that harness isn't shipped here. Those numbers are reported,
  not reproducible from this repo.
