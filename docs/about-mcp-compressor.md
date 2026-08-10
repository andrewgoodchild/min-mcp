# About mcp-compressor — the closest peer

> **Read this first — the category has moved.** This comparison was written when
> compressing a tool surface required a proxy. It no longer does: Anthropic's API
> ships a tool search tool with `defer_loading` and documents "over 85 percent"
> reduction, and `openai/codex` implements the same thing in Rust using the same
> BM25 crate min-mcp now uses. **Deferred loading and tool search have been
> absorbed into the agents themselves.** If you are choosing between min-mcp and
> mcp-compressor *for surface compression alone*, the honest answer in mid-2026 is
> that you may need neither — check what your client already does, starting with
> [About tool search](about-tool-search.md).
>
> What survives absorption is narrower and is not what either project's name
> suggests: mounting APIs that have **no MCP server to compress**, catalogs past
> the documented ceilings, and **repairing** tools rather than merely hiding them.
> Everything below the fold is still an accurate measured record of two proxies
> against the same upstream — read it as history plus a live scope difference,
> not as a live buying decision.

Atlassian's [mcp-compressor](https://github.com/atlassian-labs/mcp-compressor)
solves the same problem min-mcp originally set out to solve, with the same
top-level architecture: sit between the agent and its MCP servers, hide the full
tool catalog, hand out schemas on demand. It was the most direct comparison
available, so we cloned it, built it, and ran both binaries against the same
upstream. This page records what it is, what the measurements said, and where each
design wins.

**Short answer:** two differences, one of scope and one of design — and the scope
one is the only one that platform absorption doesn't touch. On **scope**,
mcp-compressor compresses MCP servers; min-mcp also mounts **OpenAPI specs
directly**, which is where the problem is genuinely acute and where neither a
peer proxy nor a native client feature helps — a ~590-operation Stripe or
17,531-operation Microsoft Graph spec is a document, not a server, so until
somebody writes and maintains an MCP server wrapping that API there is nothing to
compress and nothing to defer. On **design**, both defer schemas but disagree on
how the agent *finds* a tool: mcp-compressor shows it a listing of every backend
tool, min-mcp makes it search. That sets the scaling behaviour (a listing is O(N)
in backend tools, search is O(1)) and, measured live, it also decided task success.
Note where the platforms landed on that question: both chose search.

## What it is

Two wrapper tools replace the whole catalog (a third appears at `max`):

- `get_tool_schema(tool_name)` — the full schema for one backend tool
- `invoke_tool(tool_name, tool_input)` — run it
- `list_tools()` — only at `max` compression

The catalog itself rides **inside `get_tool_schema`'s description**. Here is a
real capture, mounting Stripe's 587 operations at the default `medium` level:

```
Get the input schema for a specific tool from the server toolset.

Available tools are:
<tool>stripe_DeleteAccountsAccount(path_params, query_params, body): DeleteAccountsAccount: DELETE /v1/accounts/{account} — Delete an account</tool>
<tool>stripe_DeleteAccountsAccountBankAccountsId(path_params, query_params, body): …</tool>
… 585 more lines …
```

Four **compression levels** trade per-tool verbosity:

| level | one listing line looks like | dropped |
|---|---|---|
| `low` | `<tool>name(args): full description</tool>` | nothing |
| `medium` (default) | `<tool>name(args): first sentence</tool>` | description tail |
| `high` | `<tool>name(args)</tool>` | all descriptions |
| `max` | `<tool>name</tool>` (+ a `list_tools` wrapper) | descriptions and args |

The agent's loop is: read the listing → `get_tool_schema` for the one it wants
→ `invoke_tool`. Beyond this "compressed proxy" mode it also ships CLI-mode and
code-mode generators (shell/Python/TypeScript clients talking to a local
proxy), a just-bash mode, in-process SDKs for three languages, and outbound
OAuth including interactive browser flows.

## How min-mcp differs

**Different inputs.** mcp-compressor takes MCP servers — stdio commands or
remote MCP URLs. min-mcp takes those *and* a raw OpenAPI document as a
first-class upstream kind (`spec:` + `base_url:` + `auth_env:`), converting
operations to tools in-process and deriving the call mechanics from what the
spec declares. That's the difference between compressing the servers that exist
and compressing any API with a published spec.

**Different discovery.** min-mcp's surface is three tools and **no listing** —
`search_tools`, `get_tool_details`, `call_tool`. Backend tools grow
an unseen BM25 index rather than the surface, so what the agent sees is fixed:

```
search_tools("create a customer")
→ stripe.PostCustomers — PostCustomers: POST /v1/customers — Create a customer
  stripe.PostCustomerSessions — POST /v1/customer_sessions — Create a Customer Session
  … 8 more ranked hits …            (257 tokens)
```

The ids search returns are exactly what `call_tool` accepts. That detail turns
out to matter more than it sounds — see the recall result below.

## What we measured

Both binaries over the **same upstream**: min-mcp in `passthrough` mode serving
Stripe's 587 operations as a plain MCP server, so mcp-compressor compresses the
identical tool set. Tokens are `o200k_base` over the compact-serialized
`tools/list` payload — the bytes a provider actually receives.
mcp-compressor at commit `74674d5` (2026-07-27), built from source with its own
Rust CLI; it's an actively developed project, so treat every number as a
snapshot.

### How to read these numbers — where our setup flatters min-mcp

Stated up front, because a benchmark run by one of the two authors deserves the
scepticism:

- **The upstream is a spec conversion, not a hand-written MCP server.** That is
  mcp-compressor's harder case and not its target: min-mcp's converter emits
  one-line descriptions (so `low` ≈ `medium` below — an artifact of *our*
  input, not a flaw in their level knob) and container-shaped schemas, so
  **every one of their 587 listing lines reads `(path_params, query_params,
  body)`**. Over a server like Atlassian's own — distinct argument names,
  paragraph descriptions — their listing is markedly more informative than it
  looks here, and their levels do real work.
- **The live task runs used a flash-tier model** (`gemini-3.1-flash-lite`).
  Where a result depends on the model correctly handling a wrapper indirection,
  a stronger model would plausibly do better; we say so where it matters.
- **Both arms got the same protocol**: identical tasks and upstream, per-task
  best-of-two seeds for each side, one void task discarded for both after a
  harness bug.
- **We measure what a provider is billed for**, not what a human reads. That
  favours any design with a smaller upfront surface, which is the axis min-mcp
  optimises — so it is the axis on which its win is least surprising.

### Upfront surface: 70× at its default, 20× at its floor

| surface | upfront tokens | × min-mcp |
|---|--:|--:|
| naive passthrough (no proxy) | 435,381 | 1,105× |
| mcp-compressor `low` | 27,444 | 70× |
| mcp-compressor `medium` (default) | 27,416 | 70× |
| mcp-compressor `high` | 11,970 | 30× |
| mcp-compressor `max` | 7,886 | 20× |
| **min-mcp `three_tool`** | **394** | **1×** |

Three things this table hides:

**The levels change the slope, not the shape.** Every level is `tokens ≈
per_tool × N` — about 46 tokens per backend tool at `medium`. Compression picks
the slope; it cannot flatten the line. min-mcp's 394 tokens are the same for
587 tools or 17,531.

**`low` ≈ `medium` is our artifact, not theirs.** They differ by 28 tokens here
only because our converter emits one-line descriptions, so "full description"
and "first sentence" are the same string. Over the same operations served by a
Python MCP server with real prose, the levels separate properly: 41,609 vs
26,731. Judge the knob on that number, not on ours.

**`max` is smaller but still O(N).** As documented, `max` lists bare
`<tool>name</tool>` lines and adds a `list_tools` wrapper — 7,886 tokens for
587 tools, the floor of the family rather than an exit from it. Two details are
worth knowing before choosing it: the per-line `<tool>…</tool>` tags cost
**2,936 tokens wrapping 4,161 tokens of names** (delimiting one name per line
that newlines already delimit), and an agent that calls `list_tools` to recover
the argument names receives a further **11,206 tokens** — so a max-level agent
that lists once sits near ~19K, not 7.9K.

**Where the listing lives has a second cost.** It is a tool *description*, so
it is JSON-escaped inside `tools/list`, and **any upstream tool change rewrites
it — busting the provider's prompt cache** for the whole tool block. min-mcp's
394-token surface is byte-stable regardless of what upstreams do.

**And the crossover is real:** at roughly 6–8 backend tools mcp-compressor's
surface becomes *smaller* than min-mcp's three meta-tools. Its own startup
banner is admirably honest about the small-N case — over a 3-tool server it
prints `Low 153.6% / Medium 130.8% / High 119.1% / Max 117.9%` *of the
original*. Below the crossover the right answer is no compression at all
(min-mcp's `passthrough`, whose startup banner now says so too).

### Cutting N: filters are the strongest lever either project has

The levels tune *per-tool* cost. The other lever attacks `N` itself, and it is
much more powerful: mcp-compressor's `--include-tools` / `--exclude-tools` drop
backend tools at connect time, before anything is listed. Measured on the same
587-tool upstream, keeping ten customer/charge/refund operations:

| surface | upfront tokens | × min-mcp |
|---|--:|--:|
| mcp-compressor `medium`, all 587 tools | 27,416 | 70× |
| mcp-compressor `medium`, `--include-tools` 10 tools | **569** | **1.4×** |
| min-mcp `three_tool` (any N) | 394 | 1× |

**A curated ten-tool surface costs 569 tokens — 98% less than the unfiltered
listing, and within 1.4× of min-mcp's fixed surface.** So the honest scope of
the O(N)-vs-O(1) argument is narrower than the headline suggests: *if you know
which tools your agent needs and you're willing to hand-maintain that list, a
filtered listing is nearly as cheap as search and keeps the "cannot miss"
guarantee over the tools you kept.*

What you give up is coverage — the long tail is gone rather than one search
away — and the list is a maintenance burden that drifts as the upstream
changes. That is exactly the hand-curation trade min-mcp exists to avoid, but
it is a legitimate choice, and at small N it's the better one.

min-mcp has the same lever with more reach: `filters:` in config matches
patterns rather than exact names (`stripe.*` for a whole API, `stripe.Post*`
for a family, `stripe.PostCustomers` for one tool), a fully-excluded upstream is
never even connected (so it needs no credentials), and per-caller `scopes:`
filter *on top* for a given JWT rather than globally. Theirs are exact-name CLI
flags applied to every caller alike.

### Finding a tool: the recall trade-off, measured

This is the axis where a listing should win, and the reason we tested it
properly: **a listing cannot fail to show a tool; BM25 search can miss one**,
and a search miss is a silent task failure.

So we authored ten tasks designed to make search miss — realistic phrasings
sharing as little vocabulary as possible with the target operation ("give them
a $5 store credit" → `PostCustomersCustomerBalanceTransactions`; "make sure
that pending checkout can never be completed" → expire a checkout session) —
verified against live Stripe state, no LLM judges, same upstream for both arms,
and the listing arm run with the harness's description cap *disabled* so its
mechanism arrived intact.

| condition | success | mean tokens/task | worst task |
|---|--:|--:|--:|
| **min-mcp (search)** | **10/10** | **~20K** | 48K |
| mcp-compressor (listing) | 9/10 | ~331K | 563K (a failure) |

Search never missed — including every zero-overlap phrasing. The listing arm's
losses came not from discovery but from the wrapper indirection: the model read
backend names in the listing and **called them directly as tools**
(`stripe_GetCustomers(...)` ten consecutive times in one episode, each answered
with a bare `tool not found`), then mis-nested `path_params` around the
`tool_input` envelope when it did reach `invoke_tool`.

**Read that failure mode with its caveat**: it is a weak model failing to
maintain a two-level calling convention, and a stronger model would very likely
handle it — so this is not evidence that mcp-compressor "doesn't work", and we
wouldn't claim it. What it does show is that the indirection has a cost that
scales *down* with model strength, while the listing's token cost scales *up*
with backend size. If you run frontier models over a modest catalog, that
trade may well fall the other way for you.

The transferable lesson is narrower than "search wins": **visibility isn't
affordance**. A listing shows names that are not themselves callable; search
returns ids that are valid arguments to the very next call. Either design can
close that gap — it's a prompt-surface choice, not an architectural verdict.

### On-demand schemas

Parity on a typical tool — `PostCustomers` costs 2,999 tokens through
`get_tool_schema` and 3,006 through `get_tool_details`; both pretty-print the
API's own schema.

They diverge on the monsters. Stripe's `PostCheckoutSessions` schema is 107 KB:
mcp-compressor returns all of it, **18,522 tokens in one tool result** (~9% of
a 200K context). min-mcp degrades in stages that never hide a field name —
prose-minify → structure-only → depth-prune with explicit
`nested_fields_elided` counts — landing at **2,893 tokens with all 51
top-level fields present**, `success_url` included.

Neither is strictly better in principle: theirs never elides anything; ours
never blows the budget the whole architecture exists to protect. On this schema
ours is 6.4× cheaper and complete at the top level.

### Errors

```
# mcp-compressor, unknown tool (a dot typed for an underscore)
{"code": -32603, "message": "tool not found: \"stripe.PostCustomers\""}

# min-mcp, same slip
unknown tool_id "stripe.PostCustomers" — did you mean "stripe_PostCustomers"? …
```

mcp-compressor holds the entire catalog in memory yet doesn't offer a near-miss
hint — a cheap win available to any design that knows the tool names. It also
returns tool-level failures as JSON-RPC *protocol* errors rather than `isError`
results; the MCP spec reserves protocol errors for protocol problems and
recommends `isError` for tool-execution failures, so whether the model sees the
text depends on the client. (Not a defect so much as a spec-conformance nit,
and easily changed.)

Its missing-required check is genuinely good and on by default, but it embeds
the tool's full schema in the error text (181 tokens here; at checkout scale
that's an 18K-token error), and it checks **top-level `required` only** — so on
container-style schemas, where nesting makes the check most valuable, it never
fires. min-mcp's preflight recurses into containers and answers in ~37 tokens:

```
PREFLIGHT_ERROR: {"error":"missing_required_field","field":"body.promotion.type",
"required":true,"allowed_values":["coupon"],
"fix":"set body.promotion.type to one of allowed_values, then retry"}
```

### Response size: `--toonify`

mcp-compressor offers an opt-in `--toonify` flag (off by default) that
re-encodes JSON results as TOON. Measured on a nested Stripe-shaped payload:
TOON **1,358 tokens vs compact JSON's 1,135** — on *this* shape the flag costs
20% more than simply re-serialising compactly, which is a property of the
format on nested/ragged data rather than of their implementation; on the flat
uniform arrays TOON targets it wins. On a live Stripe call the flag was a no-op
(510 → 508), because our spec executor hands back a JSON string inside an
envelope and TOON can't restructure that — our shape defeating it, not their
bug. See [About TOON](about-toon.md).

min-mcp's response levers instead: compact re-encoding by default, and
GraphQL-style field projection — `fields: ["data[].id","data[].email"]` took a
live customer list from 499 to 115 tokens (−77%).

### Federation, and what the agent never sees

`--multi-server` gives **two wrappers and one listing per backend**, so the
surface is O(servers) + O(total tools), with no cross-server search or dedup.
min-mcp stays at three tools and 394 tokens for any number of upstreams, with
one federated index across all of them.

On **access control** the two aim at different deployments. mcp-compressor is
built for one developer's agent: global `--include/--exclude` filters, and a
freshly-minted session token (constant-time compared, loopback-only) that its
generated clients present to the local proxy — written into the artifact
because the artifact is generated for that session on that machine. Its OAuth
is *outbound*, to backends. min-mcp additionally serves the multi-caller case:
scopes derived from a validated JWT, where a scoped-out tool is never listed,
searched, callable, or visible in the source map. If you're running one agent
locally, that machinery is weight you don't need.

## What actually differs

Some early differences have closed — min-mcp now has a source-map resource
(`minmcp://tools`), prompts/resources passthrough, and a startup banner that
reports honest percentages including above 100%. Those were good ideas of
theirs and are no longer distinctions. What remains:

**Where mcp-compressor is ahead**

- **A listing cannot miss.** Our recall result says search didn't miss across
  ten adversarial tasks; it does not prove it never will. Showing everything is
  the simpler guarantee, and it needs no ranking to trust.
- **It never elides a schema field.** `get_tool_schema` returns the whole
  thing, always. min-mcp trades that for a budget with staged degradation.
- **A verbosity dial.** The four levels let you tune description detail per
  deployment; min-mcp has surface *modes*, not a knob — you get one-line
  summaries in search results and that's it.
- **Reach beyond the proxy**: in-process SDKs for Rust, Python and TypeScript;
  generated CLI, Python and TypeScript clients; just-bash; interactive browser
  OAuth (min-mcp does outbound client-credentials only). min-mcp is a binary
  and a YAML file.

**Where min-mcp is ahead**

- **It mounts OpenAPI specs directly.** mcp-compressor compresses MCP servers;
  it has no spec path. That matters because the specs are where the problem is
  worst — the surfaces that *can't* be attached at all are 587-operation
  Stripe, 1,216-operation GitHub, 17,531-operation Microsoft Graph, and an
  MCP-server-only compressor can't reach them until someone else has already
  built and maintained a server wrapping that API. min-mcp treats a spec as an
  upstream kind: mount it, and the mechanics (body encoding, auth scheme,
  path-param handling) come from what the spec declares.
- **O(1) upfront surface** — 394 tokens for any N, versus a listing that grows
  at ~46 tokens per tool unless filtered, and rewrites (busting the prompt
  cache) whenever an upstream changes.
- **Federated search across upstreams** — one index, three tools, however many
  servers; theirs is two wrappers and one listing *per backend*.
- **It fixes the tools, not just their volume** — overlays patch schemas,
  errors, defaults, headers and responses as drift-checked bindings, with
  `verify` to prove a fix and `lint` to find candidates.
- **Per-caller scoping** — JWT-derived scopes, where a scoped-out tool is
  invisible to listing, search, calls and the source map. Theirs is global
  filters (see above), which every caller shares.

## Where this leaves the comparison

Both projects agreed on the important thing — the agent should not carry every
schema — and on the shape of the answer: a small surface, schemas on demand, one
invoke path. That agreement has since been ratified by the platforms, which
implemented the same shape natively and chose search over listing.

**Which means the live question is no longer "which proxy compresses better".**
It's "does anything here still need a proxy at all?" Three answers survive, and
only the last is about compression:

1. **The API has no MCP server.** A ~590-operation Stripe spec or a
   17,531-operation Microsoft Graph spec is a document. Neither a compressor nor a
   client's deferred loading can act on tools that don't exist yet; min-mcp
   manufactures the surface from the spec, with request-body encoding taken from
   the declared media type. This is the one difference absorption doesn't reach,
   and it's a scope difference rather than a quality one.
2. **The tool is broken and you can't edit it.** Neither mcp-compressor nor either
   platform lets you mark an undocumented parameter required, strip a field so it
   can't be fabricated, turn an opaque `404` into `{field, allowed_values, fix}`,
   pin the schema a patch was written against, or prove the fix with real calls in
   CI. Hiding a bad tool is not fixing it.
3. **The catalog is past the ceilings.** Anthropic documents a maximum of 10,000
   deferred tools per request, and deferred definitions are still uploaded on every
   request. Behind a three-tool surface, neither applies.

If none of those three describe your situation — you run a handful of ordinary MCP
servers and want the context bill down — then your client probably has you covered,
and mcp-compressor additionally offers SDKs and generated clients min-mcp doesn't
have.

The measurements here are honest about their limits, and one more limit now
applies: they are **historical**. They were taken against the versions of both
projects available at the time, on one upstream family (Stripe-shaped), with tokens
counted on real `tools/list` payloads, one flash-tier model for the live tasks, and
per-task best-of-two seeds. mcp-compressor's non-compressed modes
(cli/code/just-bash) were inspected, not agent-driven. Both projects have moved
since, and we have not re-run the head-to-head — so treat the numbers as a record
of a specific comparison on a specific day, not as current standings.

## Further reading

- [mcp-compressor](https://github.com/atlassian-labs/mcp-compressor) — source,
  docs, and its own blog post on the pattern
- [Concepts](concepts.md) — min-mcp's surface modes and why `three_tool` is the
  default
- [About TOON](about-toon.md) — the format behind `--toonify`, evaluated
- [Configuration](configuration.md) — `mode: passthrough` for the small-N case
