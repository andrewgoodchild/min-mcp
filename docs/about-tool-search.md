# About tool search — native, and the proxies before it

Both major agent platforms now search a tool catalog and load definitions on demand.
If you run a handful of ordinary MCP servers, **your client may already solve the
context-tax problem**, and you should know that before adopting anything. This page
sets out what they do, verified against primary sources, and where the limits
actually bite.

**Where does that leave min-mcp?** If your agent has no catalog search of its
own, min-mcp gives it the same ability from the outside — point any MCP client at
it and every upstream becomes searchable.

If your agent already has one, **don't stack two search layers**: each hides the
catalog from the other, and the agent ends up searching for the search tool.
Turn one off. min-mcp still earns its place behind a search-capable client for
the things the native layer doesn't do — fixing the tools, mounting an API that
has no MCP server, and catalogs past the deferral ceiling — just not as a second
deferral layer.

A note on provenance: I built min-mcp from the idea, and learned what the
platforms and peers had shipped only afterwards — this page is the result of
checking. At minimum, the repo is a measured, working reference for the pattern:
point your coding agent at it and build the same capability into your own stack.

Everything here was checked on 2026-08-10 against Anthropic's documentation and the
`openai/codex` source. Where a claim is inference rather than documentation, it says
so.

## Anthropic — the tool search tool

Generally available on the Claude API. You send every tool definition as usual, but
mark the ones that shouldn't occupy context with `defer_loading: true`; Claude calls
a search tool, the API returns `tool_reference` blocks, and expands them into full
definitions inline.

Two variants ship: `tool_search_tool_regex_20251119` (Claude writes Python regex)
and `tool_search_tool_bm25_20251119` (natural-language queries). Both search tool
names, descriptions, **argument names and argument descriptions**.

The documented benefit, verbatim:

> A typical multiserver setup (GitHub, Slack, Sentry, Grafana, and Splunk) can
> consume ~55k tokens in definitions before Claude does any work. Tool search
> typically reduces this by over 85 percent, loading only the 3–5 tools Claude needs
> for a given request.

Deferred tools are excluded from the system-prompt prefix, so prompt caching
survives. MCP servers configure it once on the `mcp_toolset` entry rather than per
tool. You can also supply your own retriever — a custom tool that returns
`tool_reference` blocks — which is how an embedding-based search would plug in.

Two documented limits matter:

- **Maximum 10,000 tools** with `defer_loading: true` per request.
- **You still send every definition on every request.** The docs are explicit:
  *"You still send every tool's full definition in the `tools` array on every
  request, including the deferred ones. The API needs them server-side to run the
  search."* So this reduces **context**, not request payload.

## OpenAI Codex — `tool_search` and dynamic tools

Codex CLI is open source, so this is observable rather than announced:

| what | where |
|---|---|
| the handler | `codex-rs/core/src/tools/handlers/tool_search.rs` |
| the algorithm | `bm25 = "2.3.2"` in `codex-rs/Cargo.toml` |
| the per-tool flag | `DynamicToolFunctionSpec { …, defer_loading: bool }` in `codex-rs/protocol/src/dynamic_tools.rs` |
| how it's described to the model | *"Searches over apps/connectors tool metadata with BM25 and exposes matching tools for the next model call."* |

The search text is built by walking the tool spec including its JSON Schema
(`append_schema_search_text`), the BM25 index is cached and rebuilt when the tool
registry changes, and a `limit` parameter caps results. Schema migrations
(`0004_thread_dynamic_tools` → `0019_…_defer_loading` → `0026_…_namespace`) show a
maturing feature rather than an experiment — and it has since graduated past
optional: the feature flags for it are retained only as removed no-ops, whose own
comments read *"tool_search is always enabled"* and *"MCP tools are always
deferred when tool_search is available"* (`codex-rs/features/src/lib.rs`). For
Codex users, deferred tool search is not a setting; it is the only mode.

min-mcp uses the same crate, so the honest comparison is precise. Two deliberate
differences, both measured on our side:

- **Identifier tokenization.** The crate's tokenizer segments on Unicode word
  boundaries, where `_` *joins* words — so `read_file` becomes the single opaque
  token `read_fil`, unmatchable by the query "read a file". min-mcp splits
  camelCase, snake_case, kebab-case, dotted ids and acronym runs before stemming;
  fixing this measured **+15 points recall@1** on a snake_case MCP server. No such
  splitting layer is visible in the Codex source ahead of their tokenizer — stated
  as inference from reading, not as a measurement of their binary.
- **Schema text in the index.** Codex indexes parameter names and descriptions;
  we measured that three ways on single-API corpora and it lost every time (down
  to 0.42 recall@1 at worst) — within one REST API, parameter vocabulary is nearly
  uniform (`expand`, `metadata`, `currency`), so it dilutes without discriminating.
  On a federated multi-server surface their choice may well be right; ours is the
  measured choice for the spec-mounting case min-mcp exists for.

They also carry a *fielded* BM25 (`ext/skills/src/dynamic_skill_selector/fielded_bm25.rs`)
and a shadow-mode selection experiment, because the `bm25` crate is single-field.

## The proxy generation: mcp-compressor, measured head-to-head

Before the platforms absorbed deferred loading, the same idea shipped as proxies —
and Atlassian's [mcp-compressor](https://github.com/atlassian-labs/mcp-compressor)
was the closest peer to min-mcp: sit between agent and servers, hide the catalog,
hand out schemas on demand. We cloned it, built it (commit `74674d5`, 2026-07-27),
and ran both binaries against the identical 587-tool Stripe upstream. The record is
kept here because the two designs answered one question differently — **how does the
agent find a tool?** — and that question outlived the proxies: the platforms faced
it too, and chose search.

**Their design:** two wrapper tools (`get_tool_schema`, `invoke_tool`), with the
whole catalog riding inside `get_tool_schema`'s *description* as one
`<tool>name(args): summary</tool>` line per backend tool. Four compression levels
tune how much each line carries. **Ours:** three tools and no listing at all —
backend tools grow an unseen index, and search returns ids that are directly
callable.

What the same-upstream measurements said, in brief:

- **A listing is O(N); the levels change its slope, not its shape.** ~46
  tokens/tool at the default level: 27,416 tokens upfront for 587 tools against
  min-mcp's fixed 394 (70×), still 7,886 at its `max` floor (20×). And because the
  listing lives in a tool description, any upstream change rewrites it and busts
  the provider's prompt cache.
- **Filters were the strongest lever either project had.** Their
  `--include-tools`, curated to ten operations, cut the listing to **569 tokens** —
  within 1.4× of min-mcp's surface. If you can hand-maintain the list, a filtered
  listing is nearly as cheap as search and cannot miss; what you give up is the
  long tail and the maintenance. (This result is why min-mcp's docs now present
  `filters:` as a relevance lever, not just access control.)
- **On ten tasks authored to make search miss** (zero-vocabulary-overlap
  phrasings, verified against live Stripe state, no LLM judges): search went
  **10/10 at ~20K tokens/task**; the listing arm went 9/10 at ~331K. Its failures
  were wrapper-indirection errors, not discovery — a flash-tier model calling
  listed names as if they were tools. Caveat as recorded then: a stronger model
  would likely handle the indirection; the token cost of the listing, though,
  scales with catalog size regardless of model.
- **Giant schemas:** Stripe's 107KB checkout schema came back as one 18,522-token
  tool result from their side; min-mcp's staged minification landed at 2,893 with
  all 51 top-level field names present.
- **Where they were ahead**, kept on the record: a listing cannot miss; they never
  elide a schema field; a per-deployment verbosity knob; SDKs and generated
  clients min-mcp doesn't have.

Methodology honesty from the original write-up still applies: the upstream was a
spec conversion (their harder case — over prose-rich servers their levels do real
work), the live tasks used one flash-tier model, and both projects have moved
since. Treat the numbers as a snapshot of a specific day, not current standings.
The conclusion that outlived the snapshot is the section below: for a handful of
ordinary MCP servers, neither proxy is needed anymore — and what still is, no
compressor does.

## What this means for min-mcp

**Take the honest version first: if your problem is "five MCP servers cost me 55k
tokens", the platforms now handle that, and min-mcp's surface reduction is no longer
a reason on its own to add a proxy.** min-mcp uses the same algorithm — the same
crate as Codex, in fact — so there is no lexical-retrieval magic here that they lack.

Four things are genuinely outside what native tool search does.

**1. There is no MCP server to search.** Native tool search ranks tools that already
exist. Microsoft Graph's 17,531 operations, or Stripe's 589, are an OpenAPI
document, not an MCP server. min-mcp *manufactures* the tool surface from the spec —
including request-body encoding chosen from the declared media type — and only then
minifies it. Nothing in either platform does this.

**2. The 10,000-tool ceiling.** A 17,531-operation surface cannot use `defer_loading`
at all; it exceeds the documented per-request maximum. Behind min-mcp the client sees
three tools, so no client-side limit applies.

**3. Context is not payload.** Deferred definitions are still uploaded on every
request. A proxy that exposes three tools uploads three tool definitions. Whether
that matters depends on how much you care about request size and upload latency, and
it matters more the larger the catalog.

**4. Neither platform can fix a tool.** This is the durable one. Codex has
`tool_filter` for visibility (`codex-mcp/src/connection_manager/tool_catalog.rs`),
and gateways offer description overrides. But no platform lets you mark an
undocumented parameter `required`, strip a field so the model *structurally cannot*
fabricate it, turn an opaque `404` into `{field, allowed_values, fix}`, pin the
schema an overlay was written against and fail closed on drift, or prove any of it
with real calls in CI. That is what [overlays](overlays.md) and `minmcp verify` are
for, and it is the half of the problem the platforms are not converging on.

Stated carefully on point 4: we searched the Codex source for `tool_overrides`,
`description override`, `exclude_tools`, `patch schema` and `annotations override`
and found nothing. Absent code-search hits are suggestive, not proof — indexing has
gaps and we may have missed their vocabulary.

## The accuracy question, unresolved

Anthropic states that with tool search, *"selection accuracy stays high even across
thousands of tools."* Third-party evaluations circulating in mid-2026 report much
lower retrieval accuracy at that scale. We can't reconcile the two: there is no
shared corpus, no shared query set, and no shared definition of a hit.

We therefore don't publish a comparison against their numbers. min-mcp's own recall
is measured on our own harness against labelled query sets **split by query style**,
because a blended average hides the only structure that matters:

- **Verbatim queries** (phrased with the operation's own vocabulary): recall@1
  0.94–0.97 on a 589-operation Stripe surface, 1.00 on a 14-tool MCP server.
- **Adversarial paraphrases** (business language deliberately avoiding the tool's
  vocabulary): recall@10 as low as **0.00** on terse catalogs. This is not a
  ranking deficiency — a tool document is a few id tokens and a one-line summary,
  so a vocabulary-avoiding query has zero term overlap and the candidate set is
  empty. Any BM25-based tool search has this failure mode, including the
  platforms'; embeddings only dent it (we measured a small significant gain on
  paraphrases at the cost of a significant regression on verbatim queries, so we
  ship neither). The production mitigation is the loop: an empty result tells the
  agent to re-query with resource/action vocabulary, and the caller is an LLM.

Real traffic sits between the two arms; the adversarial floor is a floor, not an
average. See [Measurements](measurements.md) for the numbers and their limits.

## Sources

- [Tool search tool — Claude Docs](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool)
- [atlassian-labs/mcp-compressor](https://github.com/atlassian-labs/mcp-compressor)
- [`openai/codex` — `tool_search.rs`](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/tool_search.rs)
- [`openai/codex` — `dynamic_tools.rs`](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/dynamic_tools.rs)
