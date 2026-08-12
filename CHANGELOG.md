# Changelog

All notable changes to min-mcp. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses
[semantic versioning](https://semver.org/) from 0.1.0 onward.

## [Unreleased]

Nothing yet.

## [0.1.1] — 2026-08-12

### Search, rebuilt on measurement

- **BM25 core replaced** by the [`bm25`](https://crates.io/crates/bm25) crate (the
  engine `openai/codex` uses for its tool search), at the crate's default
  parameters — which beat our previous hand-tuned constants on the recall harness.
- **Convention-aware tokenizer.** Identifiers split on camelCase, snake_case,
  kebab-case, dots, slashes and acronym runs *before* stemming. Previously
  snake_case tool names — most of the MCP ecosystem — indexed as single opaque
  tokens (`read_file` → `read_fil`) and were unfindable by natural queries;
  fixing it measured +15 points recall@1 on a snake_case server.
- **Flat indexing.** Field weighting by repetition (id ×3, summary ×2) measured
  worse than no weighting and was removed.
- **Usage prior counts successes only.** A tool can no longer rise in search
  ranking by being called and failing — previously preflight rejections and
  upstream errors all counted as "usage".
- **`shadow: true`** — score alternative retrieval configurations against real
  traffic without serving them, logged per call as NDJSON `shadow` events. The
  instrument that produced every claim above, shipped so the claims can be
  re-checked on your workload.
- Measured and **not** shipped, with numbers in
  [docs/measurements.md](docs/measurements.md): schema/parameter-text indexing
  (dilutes single-API corpora), embedding retrieval (significant regression on
  verbatim queries), naive RRF fusion (poisoned by zero-signal lexical lists),
  and larger description caps (upstream brevity, not our caps, is the
  constraint).

### Fixed

- **MCP/HTTP results are byte-faithful by default.** `result_format` now defaults
  per upstream kind: `raw` for MCP/HTTP upstreams (their bytes are not ours to
  rewrite — compacting a pretty-printed `read_file` result mangled the file's
  real formatting and broke whitespace-sensitive `error_hints`/`verify`
  matchers), `json` for spec upstreams (min-mcp builds that envelope, so
  compacting stays free and safe). `result_format: json` remains the opt-in for
  MCP servers whose results are JSON payloads rather than documents.
- `Origin: http://[::1]` (bare IPv6 loopback, no port) was refused by the HTTP
  transport's DNS-rebinding check — the port stripper cut inside the literal.
- A timeout or transport failure on a pagination *follow-up* page discarded all
  already-fetched pages as a protocol error; it now stops pagination and returns
  the partial list with the PAGINATION notice.
- An MCP result shaped like `{"status":…,"body":…}` was mistaken for the spec
  envelope, so projections and workflow outputs silently ran against `body`
  only. Envelope unwrapping is now gated on the upstream actually being a spec
  backend, not on result shape.

### Docs

- New [About tool search](docs/about-tool-search.md): what Claude and Codex now
  do natively, verified against primary sources, and the four places their
  limits bite. The standalone mcp-compressor comparison is folded in as a
  section — same measured record, framed as the proxy generation the platforms
  absorbed — and its page retired. README reframed accordingly: fixing leads,
  minification supports.
- Filters documented as a search-quality lever, not only access control.

## [0.1.0] — 2026-08-09

First public version. A minifying proxy for MCP servers and OpenAPI specs:
your agent sees three tools instead of every upstream's catalog, and you can
patch the tools you don't own on the way through.

### Surface

- **`three_tool` mode (default)** — `search_tools` (BM25 over every upstream
  tool), `get_tool_details` (full schema on demand), `call_tool` (routes to the
  owning upstream, with optional GraphQL-style `fields` response projection).
  Constant ~424 tokens regardless of upstream size.
- **`passthrough` mode** — declare every tool by name, for surfaces small enough
  that three meta-tools cost more than the catalog. The startup banner tells you
  when you're in that regime.
- **Three upstream kinds** — MCP server subprocess, remote MCP server over
  Streamable HTTP, and a mounted OpenAPI spec (pure-Rust converter, body
  encoding chosen from the spec's declared media type).
- **Federation** — many upstreams, one search index, still three tools.
- **Staged schema minification** in `get_tool_details`: over-budget schemas
  degrade prose → structure-only → depth-pruned with explicit elision counts,
  so every field *name* survives. No blind truncation.
- **Near-miss suggestions** on an unknown tool id.
- **Prompts and resources passthrough**, namespaced and scope-gated, plus
  min-mcp's own `minmcp://tools` source-map resource.

### Fixing tools you don't own (overlays)

- Patch descriptions, and the input schema by dotted path: `required`,
  `example`, `enum`, `type`, `format`, `hide`, and `user_supplied` (strips a
  field from the agent's schema and injects it from the environment, so it
  can't be fabricated).
- **Errors as continuation prompts** — `error_hints` with a `field:` pointer
  render a structured `{field, allowed_values, fix}` error; `retryable:` gives
  an explicit transient/permanent signal.
- **`preflight`** — local required/enum validation before the upstream call,
  **on by default**, container-aware, with a per-tool opt-out.
- **Request shaping** — `defaults`, per-endpoint `headers` with `${ENV}` and
  `{{uuid}}`/`{{now}}`/`{{iso8601}}`/`{{hash}}` generators.
- **Response shaping** — declarative `remove`/`rename`/`set`/`keep` plus a jq
  escape hatch; auto-`paginate` follows a cursor and concatenates pages.
- **`timeout_s`** — per-tool call deadline; on expiry the agent is told the
  operation may or may not have completed, so a write is never blindly retried.
- **`breaker`** — per-tool circuit breaker (closed → open → half-open probe).
  While open, calls are refused locally with a recovery prompt instead of the
  agent burning turns on a tool that fails identically every time.
- **Drift-checked bindings** — `authored_sha` pins the schema an overlay was
  written against; `binding: weak|strong` chooses fail-open or fail-closed.
- **`search aliases`** — make a badly-named tool findable without changing its
  id.
- **Composites** — a `workflows:` entry runs a fixed multi-step chain as one
  tool, threading each step's outputs into the next.

### Visibility and auth

- Static `filters:` (include/exclude whole APIs or tool families, for everyone)
  and per-caller `scopes:` derived from a validated JWT (**HS256, RS256,
  JWKS**). A scoped-out tool is never listed, searched, callable, or visible in
  the source map.
- Per-upstream auth headers with `${ENV}` expansion, and **outbound OAuth**
  (client-credentials) for OAuth-protected upstreams.
- Path-parameter injection hardening: agent-supplied path params are strictly
  segment-encoded, so `..` or `/` cannot reach a different endpoint.
- HTTP serving validates **both** DNS-rebinding headers — `Host` (via rmcp) and
  `Origin` (loopback-only when present).

### Results

- **Compact JSON by default** on every path: the spec envelope embeds `body` as
  JSON rather than an escaped string, and MCP text results are compacted
  lexically (whitespace outside strings only — number literals pass through
  byte-for-byte). `result_format: raw` opts out per upstream.
- **Opt-in idempotent-read cache** (`read_cache_ttl_s`) for spec `GET`s,
  `readOnlyHint` tools, or overlay `cacheable: true` — canonical (key-order
  insensitive) cache keys, write-through invalidation, and shaping re-applied
  per call so a hit still honours that call's `fields`.

### Tooling

- `minmcp serve` (stdio or `--http`), `inspect`, `map` (source map, `--diff` for
  drift), `verify` (run overlay checks against the live upstream — a CI gate),
  `lint` (best-practice smells with per-rule stats), and `search` / `help` /
  `call` mirroring the three tools from the shell.
- NDJSON observability (`log_file`) and leveled logging via `MINMCP_LOG`.

### Not in this version

Field *addition* for prose-only bodies, async-poll and interactive-OAuth
overlays, and a `result_format` beyond `json`/`raw` (a markdown-table format is
measured and pending a comprehension gate — see
[docs/about-toon.md](docs/about-toon.md)).

### Measured, then removed

`hotset` (usage-promoted working set), `pd` (uniform progressive disclosure),
and a TOON result encoder were all built, benchmarked, and deleted for losing
to the shipped defaults. See [docs/measurements.md](docs/measurements.md).
