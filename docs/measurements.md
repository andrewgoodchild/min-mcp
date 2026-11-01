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
  load. That is three-tool mode. Passthrough has to declare every schema, so it
  takes a 7.9 GB memory peak and a 1.3 GB listing for about 335M tokens, and
  warns at startup. A schema is capped at 10,000 JSON nodes, and `$omitted`
  marks a nested reference left out to fit. Top-level fields always stay.

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
  [About tool search](about-tool-search.md).

## Tool-search recall, split by query style

`search_tools` is the surface's front door, so its quality is measured — against
labelled query sets whose answers are validated to exist on the live surface, and
**split by query style**, because averaging across styles hides the only structure
that matters:

| corpus | query style | recall@1 | recall@10 |
|---|---|---|---|
| Stripe spec, 589 operations | verbatim (operation vocabulary) | 0.94–0.97 | 1.00 |
| Stripe spec, 589 operations | adversarial paraphrase | **0.00** | **0.00–0.03** |
| filesystem MCP server, 14 tools | verbatim | 1.00 | 1.00 |
| filesystem MCP server, 14 tools | adversarial paraphrase | 0.54 | 0.69 |

The paraphrase floor is structural, not a tuning gap: a tool document is a few id
tokens plus a one-line summary, so a query that deliberately avoids that vocabulary
("give the buyer their money back" for `PostRefunds`) has zero term overlap and the
candidate set is empty. Any lexical tool search fails this way; how hard it fails
tracks how much prose the upstream provides. The shipped mitigation is the loop: an
empty result tells the agent to re-query with resource/action vocabulary, and the
caller is an LLM. Embeddings were measured as the alternative and **not shipped**:
a small significant gain on paraphrases (+15 points @1, to 0.15) at the price of a
significant regression on verbatim queries (−21 points @1, p=0.012), plus a model
download in a proxy that currently boots instantly and offline.

Four search findings shipped as code because they measured:

- **Convention-aware tokenization**: the `bm25` crate's tokenizer joins on `_`, so
  every snake_case tool name was one opaque token. Splitting identifiers before
  stemming measured **+15 points recall@1** on a snake_case MCP server (and nothing
  on Stripe, whose camelCase already split — a convention bug is invisible to a
  single-convention benchmark).
- **An exact-name tier.** 30 of 589 Stripe names ranked second to a longer sibling, so
  `GetCharges` returned `GetChargesCharge`. A query that spells a name now wins, which
  took exact-name recall@1 to 1.00 on Stripe and GitHub.
- **Simpler reranking.** The substring boost gained nothing once the exact tier existed,
  so it's gone. Verb affinity stays: synthetic queries showed no effect, but on 360 tasks
  of real agent queries it adds 2.1 points of recall@1. The short-id bias cost recall on
  long queries and helped short ones, so it now applies up to 8 words.
- **The usage prior counts successes only.** It previously counted every attempt,
  including calls rejected before reaching the upstream — so a tool could rise in
  search ranking *by failing*, the opposite of what the circuit breaker exists to
  prevent.

These numbers come from a labelled harness that lives outside this repo; what ships
*in* the repo is the instrument: set `shadow: true` and min-mcp scores alternative
retrieval configurations against your real traffic without serving them, logging
the rank each would have given the tool your agent actually called. Retrieval
claims can then be checked on your workload, not ours.

## Compared with Codex tool search

Codex's search path (`openai/codex` at `67f0994b`), ported over the same `bm25` crate
and checked against its own golden search text, ran against the real `search_tools`
ranking on the Stripe and GitHub catalogs. Each engine was scored at its shipped result
count, 8 for Codex and 10 for min-mcp. This is search only, with no model in the loop,
so reformulating a failed query isn't counted. It predates the default of 20 results.

| catalog, queries | n | Codex recall@1 | min-mcp recall@1 | Codex recall@8 | min-mcp recall@10 |
|---|--:|--:|--:|--:|--:|
| GitHub verbatim | 40 | 0.875 | 0.975 | 1.00 | 1.00 |
| GitHub paraphrase | 40 | 0.00 | 0.00 | 0.00 | 0.00 |
| Stripe verbatim | 33 | 0.848 | 0.970 | 0.939 | 1.00 |
| Stripe + GitHub in one index, Stripe verbatim | 33 | 0.758 | 0.939 | 0.879 | 1.00 |
| Stripe + GitHub in one index, GitHub verbatim | 40 | 0.875 | 1.00 | 1.00 | 1.00 |
| Stripe exact name | 589 | 0.997 | 1.00 | 1.00 | 1.00 |
| GitHub exact name | 1,216 | 0.976 | 1.00 | 1.00 | 1.00 |
| Stripe description sentence | 589 | 0.997 | 0.959 | 1.00 | 1.00 |
| GitHub description sentence | 1,216 | 0.986 | 0.989 | 1.00 | 1.00 |

Before any tuning, the two engines tied on the 80 GitHub queries, whose labels were
written blind to both. The table shows min-mcp after the ranking changes that
comparison prompted, so its GitHub rows are no longer held out. The Stripe verbatim set
is the one min-mcp was tuned on. The filesystem corpus above was not rerun after these
changes. Codex's order is also unstable: its ties land in random order, and repeating a
query changed the top 8's membership on 18 of 80 GitHub queries. min-mcp breaks ties by
id, and its results did not change between runs.

## Search with real agent queries

Hand-written queries miss how agents really search, so this test lets models write their own.
Two models got a plain task and a search tool, with no tool names shown, on catalogs search was
never tuned on: Microsoft Graph, NetBox and GitHub's MCP server. Half the tasks are held out, and
a blind author wrote every task from the catalog alone.

| held-out, 180 tasks, 360 episodes | 10 results | 20 results |
|---|--:|--:|
| model names the right tool | 0.875 | **0.908** |
| Microsoft Graph, 17,911 tools | 0.780 | 0.839 |
| NetBox, 844 tools, mostly undescribed | 0.864 | 0.898 |
| GitHub MCP server, 131 tools | 0.976 | 0.984 |

The gain is +3.3 points, 95% CI [+0.8, +6.1]. It wasn't shared evenly: Gemini Flash Lite rose
from 0.828 to 0.906, while Haiku slipped from 0.922 to 0.911. Other changes tested on the tuning half
did not earn a place, including indexing parameter names, capping near-identical
tools, boosting name matches, and longer search guidance. Non-English tasks on a thinly described
API remain weak: first-query recall on the same 30 NetBox tasks fell from 0.733 in English to
0.433 in Spanish.

## Overlays under native tool search

With clients searching catalogs themselves, the question for repair is whether an
overlay still raises task success. We tested that on 2026-11-01, with the decision rule
written down before any episode ran: at least +10 points, a bootstrap CI above zero, and
neither model dropping.

The setup was min-mcp in `passthrough` over the real, read-only GitHub API (634 tools),
all deferred behind OpenRouter's server-side tool search, with 16 tasks answered from
live data and checked without an LLM judge. An agent that couldn't see the tasks wrote
80 overlays from `minmcp lint` findings and the spec. That gave 3 seeds × 2 models × 2
arms, or 192 episodes.

| | no overlays | overlays |
|---|---:|---:|
| verified success, pooled | 81.2% | 76.0% |
| Claude Haiku 5.5 | 87.5% | 85.4% |
| Gemini 3.5 Flash-Lite | 75.0% | 66.7% |

**No clear lift:** −5.2 points, 95% CI [−13.5, +2.1]. On the 6 tasks that reached an
overlaid tool, success was 23 of 72 in both arms. Token totals differed only because of
runaway episodes on one task, so we claim no saving.

Read it narrowly. This is one upstream, better documented than the long tail, with
cheap models, one provider's regex search, and too few tasks to resolve a small effect.
It doesn't show overlays are useless on a badly broken server. It does mean we can't
claim that overlays raise task success under native search.

The run also exposed a defect it couldn't work around: `passthrough` then capped results
at 8,000 bytes, and one task's answer sat at byte 9,077, so it failed in every episode in
both arms. The cap has since been raised to 64,000 bytes, after this run.

## The harness catches our own overfits

Three features were built, measured, and *removed* because they lost:

- **`hotset`** (promoting a usage-mined working set) won on the distribution its
  list was mined from, then **lost by 64%** on a held-out task set — promoted
  but unused declarations ride along every turn.
- **`pd`** (uniform progressive disclosure) kept one declaration per tool, which
  exceeded the provider cap and lost on both tokens and success.
- **TOON** as a result format cost *more* tokens than compact JSON on real
  nested payloads — see [About TOON](about-toon.md).
- **Field weighting by repetition** (id ×3, summary ×2 in the search index) was
  inherited from a prototype and shipped unmeasured; when finally tested it was
  *worse* than flat indexing (repetition inflates document length, which BM25
  penalises, while term-frequency saturation caps what the copies buy). Removed.

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
- **Covered by offline end-to-end fixtures, not by live systems**: the
  Streamable HTTP transport (including per-request bearer identity),
  `workflows:`, auto-pagination, `minmcp verify`, JWT scopes, upstream respawn,
  and `optional` upstreams all run over the wire in CI against scripted
  upstreams (`tests/`). Not exercised against anything real: OAuth
  client-credentials and JWKS against a live identity provider, `${vault:…}`
  against a live Vault (the reference parser and the no-store errors are
  unit-tested; the vaultrs calls are not), and trusted gateway headers behind an
  actual gateway. Blocking CI covers Linux, macOS and Windows; the shell and
  `curl` fixtures are Unix-only — see [getting started](getting-started.md).
- **`timeout_s`, `breaker`, and `rate_limits`** have unit and end-to-end tests
  proving the mechanism; their *impact* on agent token spend is not measured.
- **Concurrency is not benchmarked.** The surface is shared rather than locked
  and the stdio client multiplexes requests, so a slow call should block only
  its caller; that is asserted by design and by tests of the parts, not by a
  throughput measurement under load.
