# Changelog

All notable changes to min-mcp. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses
[semantic versioning](https://semver.org/) from 0.1.0 onward.

## [Unreleased]

### Added — the control layer for running behind a gateway

- **Per-request caller identity over HTTP.** With `auth:` configured, each
  request is authenticated on its own — `Authorization: Bearer` validated by the
  configured verifier, or identity headers from a trusted gateway
  (`auth.trusted_headers`) — and sees only what its scopes grant. One `serve
  --http` now serves many callers with different surfaces. A request with no
  identity is 401 with a `WWW-Authenticate` challenge unless
  `auth.allow_anonymous`. `auth.subject_claim` (default `sub`) names the caller.
- **Rate limits.** `rate_limits.per_caller` / `per_tool` (token buckets keyed by
  the caller) and overlay `rate_limit` (one tool across all callers). A refused
  call is a `RATE_LIMITED` isError result with a retry-after.
- **Audit stream.** Every NDJSON line carries `caller`; a `call` adds
  `latency_ms` and `result_bytes`; `rate_limited` events are logged. `log_file:
  stderr` sends the stream to stderr for container log shippers.
- **Secret references.** `${env:X}`, `${file:/run/secrets/x}`, and
  `${vault:path#field}` (HashiCorp Vault / OpenBao KV v2 via `vaultrs`; token,
  AppRole, or Kubernetes login with re-login on 403; cached `secrets.cache_ttl_s`)
  everywhere a credential appears, including `user_supplied` sources and a spec
  upstream's new `api_key`. Resolved secrets are `SecretString`s.
- **Upstream health.** A stdio upstream that exits is respawned on the next call
  (crash-loop guard: one start per second); an expired remote MCP session (404)
  is re-initialized and the request retried; `tools/list_changed` flags the
  upstream in `inspect` as `upstreams_stale`.
- `examples/enterprise.yaml`, an annotated deployment config.

### Changed

- **The surface is shared, not locked.** The catalog, index, schemas, and
  backends are read concurrently; per-call state sits behind small locks never
  held across an upstream call; the stdio client multiplexes requests by id; the
  runtime is multi-threaded. A slow upstream call now blocks only its own
  caller, instead of every session.
- `--allow-remote` is no longer needed for a non-loopback bind when `auth:`
  makes every request authenticate. A non-loopback bind now actually serves
  remote clients: rmcp's loopback-only `Host` allow-list (which 403'd every
  non-loopback `Host`) is disabled for such binds, and the `Origin` check is
  kept under `--allow-remote` but dropped when identity is enforced.
- A JWKS set refreshes itself on an unknown `kid` and every ten minutes
  (single-flight, at most once a minute), so an IdP key rotation no longer
  means 401s until restart, and a withdrawn key stops being trusted.
- `inspect` reports `upstreams_stale`.

### Fixed — from a review of the control layer

- **Audit commands no longer need the identity provider or Vault.** `build()`
  fetched JWKS and logged in to Vault for *every* subcommand, so a config with
  `auth.jwks_url` or `secrets.vault` made `inspect` / `map` / `lint` / `search`
  (and stdio `serve`, which never validates a token unless `--jwt` is given)
  fail wherever those services were unreachable. The verifier is now built only
  when a token will actually be validated, and Vault connects on the first
  `${vault:…}` reference that is really reached.
- **A refused call no longer spends the caller's wider budget.** The per-caller
  bucket was charged before the per-tool and overlay buckets were consulted, so
  hammering one rate-limited tool locked the caller out of every other tool.
  Tiers already charged are refunded when a later one refuses.
- **A scoped-out passthrough tool no longer leaks its id.** Calling a hidden
  tool by its exposed name resolved the name first and answered `unknown
  tool_id "up.GetX" — did you mean …`, handing the caller the canonical id and
  its neighbours; it is now shaped exactly like a tool that does not exist.
- **A request that carries no caller identity is refused** when identity is
  enforced, instead of falling back to the process identity (which may hold
  broader scopes than any real caller).
- **A dying stdio upstream fails its in-flight call immediately.** A request
  that registered between the reader draining its waiters and the child's pipe
  closing waited out the full deadline for a reply that could never arrive.
- **Concurrent recovery is single-flight.** Two callers hitting an expired
  remote MCP session each re-initialized, and one cleared the other's fresh
  session id; the same for OAuth token refresh. A healthy call now takes no
  lock a recovery could be holding, and a respawn no longer blocks callers from
  reading the live connection.
- SSE replies match their request id by the same tolerant rule the stdio client
  uses, so a server echoing `"7"` for `7` is matched rather than falling
  through to the first result-bearing frame.

### Security

- **HTTP serving is loopback-only.** `serve --http` refuses a non-loopback
  bind (`0.0.0.0`, a LAN address) unless `--allow-remote` is passed. The
  transport has no inbound authentication and one scope set per process, so a
  reachable port handed every client this process's upstream credentials — and
  the CLI help said "binds localhost" while binding whatever it was given.
- `auth.audience` / `auth.issuer` — optional `aud` / `iss` checks on caller
  JWTs. Both were unchecked, so a token minted for another service validated.
- `--jwt` can be supplied as `MINMCP_JWT`, keeping the token out of argv.
- The JWKS fetch at startup is bounded (30s), like the OAuth token fetch.

### Fixed

- **Number literals survive every path.** serde_json's `arbitrary_precision`
  is on: a 128-bit id or a 23-digit decimal is no longer rewritten through f64
  by the spec envelope (default `json`), `fields` projection, overlay
  `response` transforms, or the pagination merge. The MCP path's lexical
  compactor already guarded this; the other paths did not. (jq programs still
  compute in f64, as jq does.)
- An MCP upstream's transport failure (subprocess died, remote non-2xx) reached
  the agent as a JSON-RPC protocol error, while a spec upstream's identical
  failure was an `isError` result. Both are now `UPSTREAM_ERROR` results with
  the write-safety guidance, so breaker, cache-bust, and error hints apply
  alike.
- A request body declaring form-encoding before JSON showed the JSON schema
  but was sent form-encoded: schema and encoding now pick the same media type.
- Protocol error messages carried only the outermost context; the cause chain
  is included.
- A transient `accept()` failure ended the HTTP server; it is logged and the
  loop continues.
- `search_tools` with `k: 0` reported "no matches"; it now uses the default.
- Composite tools hidden by scopes from `search_tools` were still reachable via
  `get_tool_details` / `call_tool`; the three now agree.
- The duplicate-upstream error printed runs of literal spaces; the `${VAR}`
  and missing-`command` errors named the wrong thing.

### Changed

- **Config is strict.** Unknown or misspelled keys anywhere in the file are a
  startup error naming the key (they were silently ignored). Duplicate overlay
  `tool`s and workflow `id`s, and an upstream setting more than one of
  `command` / `url` / `spec`, are rejected too.
- `inspect` / `map` report `"mode": "three_tool"` (the config spelling)
  instead of the Rust variant name `"ThreeTool"`.
- `MINMCP_LOG=trace` is accepted.
- jq programs are compiled once per process instead of on every call.
- Searches no longer serialize on the usage prior, the audit line's size
  measurement is skipped when no sink is configured, and a per-request params
  clone on the HTTP upstream's hot path is gone.
- One poison-tolerant lock helper (`crate::sync`) replaces three copies and
  ~15 inline spellings; `tests/common` replaces per-suite copies of the fixture
  tokens and spawn helpers, and the HTTP suite's hardcoded ports (a real
  cross-test race) are now OS-assigned.
- `rust-version = "1.91"` declares the MSRV; release binaries are stripped and
  built with `codegen-units = 1`.

### Added

- `optional: true` on an upstream — skip it with a warning when it can't be
  spawned, connected, or listed, instead of failing the whole proxy.

### Docs

- Transports: the claim that every request runs on its own task is corrected —
  surface requests are serialized behind one lock, and `timeout_s` is the
  mitigation. The HTTP transport's no-auth, per-process-scope model is stated.
- Configuration: the examples list no longer names a file that isn't shipped.

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
