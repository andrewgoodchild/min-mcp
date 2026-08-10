# About native tool search

Both major agent platforms now search a tool catalog and load definitions on demand.
If you run a handful of ordinary MCP servers, **your client may already solve the
context-tax problem**, and you should know that before adopting anything. This page
sets out what they do, verified against primary sources, and where the limits
actually bite.

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
maturing feature rather than an experiment.

They also carry a *fielded* BM25 (`ext/skills/src/dynamic_skill_selector/fielded_bm25.rs`)
and a shadow-mode selection experiment, because the `bm25` crate is single-field.

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

We therefore don't publish a comparison. min-mcp's own recall is measured on our own
harness against a labelled query set, and the honest summary is that lexical search
finds the right tool at rank 1 roughly three times in four on a 589-operation Stripe
surface, with most remaining failures being vocabulary mismatch rather than ranking.
See [Measurements](measurements.md) for what is and isn't measured.

## Sources

- [Tool search tool — Claude Docs](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool)
- [`openai/codex` — `tool_search.rs`](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/tool_search.rs)
- [`openai/codex` — `dynamic_tools.rs`](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/dynamic_tools.rs)
