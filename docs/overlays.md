# Overlays — fix a tool you don't own

A tool description, a field doc, and an error string are all **prompts**: they're
the next thing the model reasons over, and they steer its behaviour as much as
your system prompt does. But when the tool belongs to someone else's MCP server or
a vendor's OpenAPI spec, you can't edit them at the source — your only options
have been to fork the server or paper over its quirks in every agent's system
prompt.

An **overlay** patches the surface *as it passes through the proxy*. You name a
tool id and supply the parts you want to override; the proxy merges them into what
the agent sees and into what it gets back on failure. Because overlays are part of
the surface, they're testable like any other change: `minmcp verify` runs real
calls with deterministic assertions, so changing an error hint is something you
re-run and check, not a vibe.

## Anatomy

```yaml
overlays:
  - tool: stripe.PostCustomers          # the full tool id to patch
    description: "Create a customer. Use before creating a subscription."
    fields:
      email: "The customer's real email. Never invent one — ask if unknown."
    defaults:
      body.currency: usd                # fill request fields the agent omits
    error_hints:
      - contains: "No such price"
        hint: "That price id doesn't exist. Search 'list prices' first, then retry."
    response:                           # reshape the response (see below)
      remove: ["data[].secret", "livemode"]
    binding: strong                     # fail-closed if this overlay breaks
    authored_sha: "a1b2c3d4e5f60718"    # detect upstream schema/description drift
    # preflight: false                  # exempt THIS tool from local schema validation
    # cacheable: true                   # opt this tool's results into the read cache
    # timeout_s: 10                     # per-tool call deadline (transport default: 120s)
    # breaker: { consecutive_failures: 5, cooldown_s: 60 }   # trip after N straight errors
```

Every field is optional; use only what you need.

Two per-tool switches override global settings: `preflight` (local
required/enum validation before the upstream call — **on by default**
globally; set `false` here when a spec over-declares `required`) and
`cacheable` (force a tool into — or out of — the `read_cache_ttl_s` TTL cache
regardless of its read-only signal).

Two more guard a flaky or slow tool — **`timeout_s`** and **`breaker`** — see
[their section below](#timeout_s-and-breaker--guard-a-slow-or-flaky-tool).

## What each part does

### `description` and `fields`
`description` replaces the tool's one-line summary. `fields` patch individual
fields inside the input schema, addressed by a **dotted path through
`properties`** (`body.currency`, `path_params.owner`, or a bare `owner` for a
flat MCP tool) — so nested API params are reachable, not just the envelope.

A field value is either a **string** (shorthand for a description — this is where
you defuse the *mandatory-field hallucination* trap) or a **map** — a full patch
of the field's schema:

```yaml
fields:
  owner: "the repository owner login"     # string shorthand = description
  title:                                   # or a structured patch:
    required: true                         # add it to the object's `required` list
    example: "Bug: login fails"
    description: "issue title; ask the user, never invent"
  legacy_flag: { hide: true }              # drop an obsolete field from the schema
```

Supported keys: `description`, `required` (add `true` / remove `false` — the fix
for the *undocumented-required* trap, where the schema says optional but the API
400s), `example`, `enum`, `type`, `format`, `hide`, and `user_supplied`. Patches
are applied to the resolved schema at load; a path that no longer exists upstream
is reported `broken` (see *binding safely*), so a fix can't silently stop applying.

**`user_supplied` — structural anti-fabrication.** Mark a required field whose
value belongs to the *user/session*, not the agent. min-mcp **strips it from the
agent-facing schema** (so the agent can neither set nor *invent* it) and
**injects it at call time** from the named source:

```yaml
fields:
  body.account_id: { user_supplied: "env:ACCOUNT_ID" }   # the agent never sees this field
```

The value comes from the environment/session, so fabrication is impossible *by
construction* — the model can't guess a field it can't see, and any value it
tries to pass is overwritten. If the source can't be resolved at call time the
call fails with a clear `missing_user_supplied_value` error, never a fabricated
success. This is the model-independent counterpart to a `description` that merely
*asks* the agent not to guess (which only capable models heed).

> **Limit:** `fields` patches *existing* fields. If an upstream documents its
> request body only in prose (an empty `body` schema), there is no field to mark
> required — that needs field *addition*, which overlays don't do yet.

### `paginate` — auto-follow pagination (spec upstreams)
81% of list endpoints paginate; without help the agent hand-rolls cursor loops and
truncates. `paginate` makes the proxy follow the cursor and concatenate, so the
agent gets one complete result:

```yaml
overlays:
  - tool: stripe.GetCharges
    paginate:
      items:  "data"                    # array to accumulate
      cursor: "data[last].id"           # next cursor (a `[last]` segment = last item)
      into:   "query_params.starting_after"   # where to write it for the next page
      more:   "has_more"                # optional gate; page while true
      max_pages: 10                     # safety cap
```

Covers cursor-by-last-item (Stripe) and next-token (`cursor: "next_cursor"`,
`into: "query_params.cursor"`, omit `more`) styles. The merged result has its item
array replaced by the concatenation and `more` set false.

### `timeout_s` and `breaker` — guard a slow or flaky tool

A broken upstream doesn't just fail — it *drains the agent*. Two failure modes
cost real money and turns, both measured: a tool that hangs eats the whole turn
budget in silence, and a tool that fails identically every call invites the
agent to retry it forever (our benchmarks caught 15 wasted turns in one episode
and 562K tokens in another). These two keys make the proxy handle both, so the
agent never has to.

```yaml
overlays:
  - tool: flaky.search
    timeout_s: 10                  # bound THIS tool's call (transport default: 120s)
    breaker:
      consecutive_failures: 5      # straight errors that trip it open (default 5)
      cooldown_s: 60               # how long it stays open before a probe (default 60)
```

**`timeout_s`** bounds one upstream call. On expiry the agent gets an isError
that states the ambiguity honestly:

```
TIMEOUT: flaky.search did not respond within 10s. The operation may or may not
have completed upstream — for writes, check state before retrying.
```

That wording is deliberate: a timed-out *write* may have committed, so the
agent must verify state rather than blindly retry. The deadline is applied
inside the backend, so a stdio request is never cancelled mid-write (a
half-written frame would corrupt the next request).

**`breaker`** is a per-tool circuit breaker with the standard three states.
Closed: calls pass through. After `consecutive_failures` straight errors it
trips **open** and calls are refused *locally* — no upstream round-trip — with
a message that is itself a continuation prompt:

```
BREAKER_OPEN: flaky.search has failed 5 time(s) in a row and is paused for ~42s.
Do not retry it now — use a different tool or report what is failing.
```

After `cooldown_s` it goes **half-open** and lets exactly one probe call
through (concurrent callers are still refused): success closes the breaker,
failure re-opens it for another full cooldown. This is design law 6 —
*errors are continuation prompts* — made structural rather than advisory.

Details worth knowing:

- **Timeouts count as failures**, so the two compose: a hanging tool trips its
  own breaker and stops being called.
- **Any failure counts**, including transport errors and upstream 5xx — not
  just tool-level `isError` results.
- State is **per tool and per process**; a sibling tool on the same upstream is
  unaffected, and nothing persists across restarts.
- **Cache hits bypass both** — a result served from `read_cache_ttl_s` is not a
  call, so it neither times out nor counts toward the breaker.
- Both are **opt-in per tool**. There is no global default: a breaker on a tool
  that legitimately errors often (a `search` that returns 404 for "not found")
  would do harm, so you name the tools you want guarded.

### `verify` — dynamic checks that prove the fix works
The third leg of the loop — **detect (lint) → fix (overlay) → verify**. Each check
calls the tool (through the full overlay path — headers, defaults, field-patches
and response transforms all apply, so you verify the *fixed* tool the agent sees)
and asserts on the result. Assertions are deterministic (no LLM judge):

```yaml
overlays:
  - tool: api.createItem
    fields: { body.name: { required: true } }
    verify:
      - name: "omitting name is rejected"
        arguments: { body: {} }
        expect: { status: 400 }              # negative case
      - name: "with name it succeeds"
        arguments: { body: { name: "widget" } }
        expect: { status: 200, has: ["id"] } # positive case
```

`expect` supports `status` (spec upstreams), `is_error`, `has`/`missing` (dotted
response-payload paths), and `contains` (substring). Run with **`minmcp verify`** —
it makes real upstream calls and exits non-zero if any check fails. The *same*
block is reusable: prove a fix at author time, guard **behavioural** drift in CI
(which the schema fingerprint can't see), and gate an agent-proposed patch before
accepting it.

### `headers` — per-operation request headers (spec upstreams)
Fill a header the spec *requires* but the mount doesn't supply — and, crucially,
one whose value **differs per endpoint**, which the upstream-wide `headers:` can't
express. The motivating case is the Australian CDR `x-v` API-version header
(required, and different for each resource):

```yaml
overlays:
  - tool: cdr.listBankingAccounts
    headers:
      x-v: "1"                              # required, per-endpoint (static)
      x-fapi-interaction-id: "{{uuid}}"     # fresh UUIDv4 per request (dynamic)
      x-fapi-auth-date: "{{iso8601}}"       # UTC RFC-3339 timestamp per request
      authorization: "Bearer ${CDR_TOKEN}"  # ${ENV} expanded once, at load
```

Values resolve in two phases: **`${ENV}` at load** (fail-loud, keeps secrets out
of config), and **generators on every call** — `{{uuid}}`, `{{now}}` / `{{now_ms}}`
(unix epoch), `{{iso8601}}` (UTC), and `{{hash}}` (a stable key derived from the
request content — an idempotency key that de-dups an agent retry of the same call). Per-tool headers apply to spec (HTTP) upstreams
and win over an upstream-wide `headers:`; MCP upstreams ignore them.

### `aliases` — discovery names
Extra phrases indexed for `search_tools` only, so a poorly-named tool
(`get_user`) is findable by an outcome (`find_user_by_email`). The tool id and
routing are unchanged — this affects search ranking, nothing else.

```yaml
aliases: ["find a user by their email address"]
```

### `defaults` — request-side input defaults
Dotted paths filled in **before** the upstream call, for any path the agent
omitted. `body.currency: usd` lands the value in the right place. The agent may
still override — defaults only fill what's absent.

### `error_hints` — errors as continuation prompts
Rewrite raw upstream errors into recovery instructions before they reach the
model. A raw `No such price: price_xxx` derails an agent; the same failure
carrying *what failed, why, and what to do next* becomes something it can act on.
Fleet-wide hints (top-level `error_hints:`) cover cross-cutting failures (auth,
rate limits); per-tool hints stack on top. Add `retryable: true`/`false` to a hint
to give the agent an explicit, structured retry signal (transient 429/5xx vs a
permanent 4xx) instead of leaving it to guess:

```yaml
error_hints:
  - contains: "429"
    hint: "rate limited"
    retryable: true      # -> appends "RETRYABLE: true (transient — wait and retry)"
```

**Prefer a structured error over prose.** Add a `field:` pointer and, on a
matching error, min-mcp renders a machine-shaped block —
`{error, field, allowed_values, description, fix}` — from that field's *patched*
schema, instead of the prose `hint`. One source of truth (the codes/meanings come
from the `fields` patch, not a duplicated sentence):

```yaml
fields:
  body.zone: { enum: [z-1, z-2, z-3], description: "region: z-1=us-east, z-2=us-west, z-3=eu", required: true }
error_hints:
  - contains: "ERR_ZONE"
    hint: "the zone is invalid"     # fallback if the field can't be rendered
    field: body.zone                # -> renders the structured error instead
```

This matters: a prose hint is a **capability-gated** fix — weak agents ignore it —
whereas the structured shape recovers even weak agents (measured 0%→100% task
success, at a fraction of the tokens). Preflight is on by default and pairs with this
([configuration](configuration.md)) to return the *same* structured error
**before** the upstream call, when a required/enum constraint is violated — no
round-trip, no opaque 400 to thrash on.

### `response` — reshape what comes back
A declarative JSON transform applied to every response before it reaches the model
— JSON-Patch-like mutation (remove/rename/set) plus JSONPath/GraphQL-like selection
(keep), with a jq escape hatch. Paths are dotted; `[]` maps over an array element.
Applied in order keep → remove → rename → set → jq:

```yaml
response:
  keep:   ["object", "has_more", "data[].id", "data[].amount"]  # allowlist (aggressive)
  remove: ["data[].secret", "livemode"]                          # denylist
  rename: { "data[].balance_transaction": "txn" }                # key -> new key
  set:    { "source": "min-mcp" }                                # add/replace (unconditional upsert)
  jq:     ".data | map({id, amount})"                            # escape hatch
  when:   error                                                  # always (default) | success | error
```

- `when` gates the whole transform on the result's `isError` — e.g. `when: error`
  strips a verbose error body while leaving successful responses untouched.

- `keep` is an allowlist — it drops everything else, so it can remove context the
  agent needed; use with care. It is **never applied to error responses** (its
  paths are tuned to the success shape, so it would blank the error out and leave
  the agent flying blind) unless you set `when: error` explicitly. `remove`/
  `rename`/`set` still run on errors — so secrets are stripped from error bodies.
- `remove` is the safe default for stripping secrets / PII / noise.
- `jq` runs last, for reshaping the declarative ops can't express.

> Callers can *also* narrow a response per-call with GraphQL-style field
> projection (`--fields` on the CLI, a `fields` argument on `call_tool`). Overlay
> `response` transforms are server-side and apply to everyone; `fields` is the
> caller's choice for one call. Both run before the agent-budget truncation.

> min-mcp is a proxy, not a security product — argument-level injection blocking,
> poisoning scanners, and secret redaction are deliberately **out of scope** (that's
> what dedicated security tools are for). The overlay's job is to *fix*
> a tool you don't own — patch its description, its errors, and reshape its response
> — not to be a WAF. (An earlier experiment in taint-blocking + description scanning
> was cut for exactly this reason.)

## Binding safely against upstream drift

Overlays are *bindings* to someone else's schema, and that schema can change. Two
mechanisms keep them honest:

### Strength — `binding: weak | strong`
The consequence of a broken overlay differs by kind: a PII-strip that silently
stops applying is a data leak; a description patch that stops applying is
cosmetic. So strength is per-overlay:

- `weak` — if broken, skip the broken parts and keep serving (fail open).
- `strong` — if broken, refuse to start (fail closed).

The default comes from the config-wide `binding_policy` (`warn`→weak,
`strict`→strong).

### Drift detection — `authored_sha`
Run `minmcp map` to print each tool's `schema_sha` (a stable fingerprint over the
raw upstream **description *and* input schema** — so a rug-pull that changes only
the description is caught, not just a schema-shape change). Copy it into the
overlay's `authored_sha`. Now min-mcp can tell whether the live tool still matches:

- **ok** — matches; overlay applies.
- **changed** — schema moved but the overlay's contract still holds (a warning).
- **broken** — the tool is gone, or a field the overlay patches no longer exists.

`minmcp map --diff old-map.json` compares a saved map against the current upstream
and reports which overlays must be re-verified (exit 1 on a breaking change) —
drop it in CI to catch a vendor's breaking change before your agents do. See the
[CLI reference](cli.md).

## Why this is first-class

It keeps these prompt-surfaces **single-sourced and versioned in one file**
instead of copied into every agent — and, because they're part of the surface,
they're testable. See [`examples/stripe-narrow-filter.yaml`](../examples/stripe-narrow-filter.yaml)
for a worked response-transform overlay.
