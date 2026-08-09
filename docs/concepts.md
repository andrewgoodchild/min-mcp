# Concepts

## Minification, in the JavaScript sense

min-mcp is a *minifier* for tool surfaces. Like a JS minifier, the output is:

- **radically smaller** — 587 tool definitions become 3 the agent holds in context;
- **behaviour-preserving** — the same tasks succeed and the same ones fail, because
  every original tool is still reachable through the surface;
- **source-mapped** — `get_tool_details` (and `minmcp map`) map any surface tool
  back to its origin (`METHOD /path` or an upstream tool), so a wrong-tool
  selection is traceable.

The claim ("smaller *and* behaviour-preserving") is falsifiable, and it was
checked that way — against live APIs, with a benchmark built to falsify it, not
just asserted. The compression half is reproducible here and asserted in CI
(`./bench/compression.sh`, and `tests/compression.rs`); the shipped
`minmcp verify` is the same discipline applied to your own overlays.

## The problem it solves

An agent with several MCP servers installed can spend 50,000+ tokens on tool
definitions before it reads the first prompt. At scale it is worse than
expensive: a 587-tool surface exceeds Google's 512-function-declaration cap, so
it cannot be attached to the model at all. The usual fixes each trade one problem
for another — hand-curating tools loses coverage; converting an API 1:1 produces
a surface that is unrunnable (provider caps), invalid (machine-generated schemas
get rejected), or incorrect (generated clients drop mechanics like nested-body
encoding). min-mcp keeps the whole surface reachable while showing the agent only
what it needs.

## Two axes of minification: definitions *and* responses

Context bloat has two sources, and min-mcp attacks both:

1. **Tool definitions (input side)** — the catalog the agent must hold before it
   acts. The [three-tool surface](#the-surface-three-tools) collapses 587 definitions to 3.
2. **Tool results (output side)** — a single list endpoint can return tens of KB
   of JSON, most of it fields the agent will never read. That fills context just
   as fast, and no amount of tool-definition minification touches it.

For the output side, min-mcp borrows **GraphQL's idea: let the caller name the
fields it wants.** On any `call_tool` the agent can pass a `fields` projection —
dotted paths, with `[]` to map over arrays — and min-mcp prunes the response to
exactly those, preserving structure:

```
fields: ["data[].id", "data[].amount", "has_more"]
```

The crucial difference from GraphQL: **the upstream API needs no field-selection
of its own.** min-mcp does the pruning in the proxy, so you get GraphQL-style
projection over Stripe, GitHub, or any REST API that otherwise only ever returns
everything. Two related levers:

- **caller projection (`fields`)** — the agent's per-call choice of what to keep
  (CLI: `--fields`; see [CLI](cli.md#call)).
- **overlay response transforms** — server-side `keep`/`remove`/`rename`/`set`/`jq`
  that apply to everyone: strip secrets/PII/noise, or cap a chatty endpoint. See
  [Overlays](overlays.md#what-each-part-does).

Both run before the response reaches the model, so a large result never bloats the
context in the first place.

## The surface: three tools

The agent sees `search_tools` / `get_tool_details` / `call_tool`. It searches for
the operation it needs, pulls that one schema on demand, and calls it. This is
tiered disclosure: three declarations regardless of upstream size.

- `search_tools(query)` — BM25 (`K1=1.5`, `B=0.4`) over every upstream tool, plus
  a damped usage prior (never dominant), a verb-affinity boost that favours the
  right read/write sibling (a "create" query leans to `Post…` over `Get…`), and a
  bias toward shorter/canonical ids (exact-score ties break alphabetically).
  Returns ids + one-line summaries.
- `get_tool_details(tool_id)` — the full input schema on demand (the source map).
  Very large schemas degrade in stages (prose-minified → structure-only →
  depth-pruned with explicit elision counts) so every field *name* survives the
  display budget — never a blind truncation that hides trailing fields. A
  mistyped id gets a near-miss suggestion ("did you mean …?").
- `call_tool(tool_id, arguments, fields?)` — validates against the (patched)
  schema locally first (preflight, on by default), routes to the owning
  upstream (MCP server, remote MCP server, or spec-backed HTTP call), applies
  scope checks and error overlays, and projects the response down to `fields`
  if given.

Three declarations, whatever the upstreams hold — one server or twenty, 4
operations or 17,531.

> **When not to minify.** Three meta-tools have a floor cost of their own
> (~424 tokens), so below roughly a dozen upstream tools they cost *more* than
> simply declaring everything. For that case there's `mode: passthrough`, which
> federates tools directly with no search step. The startup banner tells you
> when you're in that regime rather than leaving you to guess — a minifier that
> can't say when not to minify isn't measuring anything. See
> [Configuration](configuration.md).
>
> Two further patterns — `hotset` (a usage-promoted working set) and `pd`
> (uniform progressive disclosure) — were built, measured, and *removed*
> because tiering beat them, and `hotset` overfit on held-out tasks. Shipping
> only what beats the baseline is a design rule, not an accident.

## What else the surface does

Because everything passes through one proxy, the surface is also the natural place
to *fix* and *compose* upstreams:

- **Fix** — [overlays](overlays.md) patch descriptions, field docs, and errors,
  and reshape responses, without forking the upstream.
- **Compose** — [composites](composites.md) expose a fixed multi-step chain as one
  tool.
- **Gate** — [scopes and filters](transports-and-auth.md) decide what a caller can
  even see.

## The verification-first ethos

Every claim was **measured, not asserted** — with programmatic ground truth, no
LLM judges. Three levels of it, and where you can check each:

- **Compression** — reproducible right here: [`bench/`](../bench/) collapses a
  120-operation spec to a flat 3-tool surface and reports the ratio, deterministic
  and offline. Locked in CI ([`tests/compression.rs`](../tests/compression.rs)).
- **A fix works** — falsify it yourself with **`minmcp verify`**: real calls
  against your own upstream, deterministic assertions.
- **Agent effectiveness** (task success, error-recovery, anti-fabrication) —
  success is a state assertion: make the call, then `GET` the object and check
  it. Those runs need a model key and real API spend, so the reported numbers
  are what this repo carries — with an explicit list of what *isn't* measured in
  [Measurements](measurements.md).

This is why features that lost their measurement (TOON, `pd`, `hotset`, mined
composite discovery) were removed rather than kept on vibes.
