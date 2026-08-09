# About TOON — and why min-mcp doesn't emit it

[TOON](https://github.com/toon-format/toon) (Token-Oriented Object Notation) is
a compact text serialization of the JSON data model, designed to cut tokens in
LLM prompts. It comes up often enough in min-mcp discussions ("why not just
encode results as TOON?") that this page records what it is, what we measured,
and the decision.

**Short answer:** we built a TOON encoder, measured it four times, and removed
it. TOON's saving comes from writing each key once, which requires flat uniform
arrays — and on the payload shapes a proxy actually carries, real nested and
ragged REST responses, it falls back to a list form that costs **more** tokens
than compact JSON. Comprehension was fine (statistically indistinguishable from
JSON in our tests); the tokens weren't. So min-mcp emits compact JSON by
default.

**This is a fit finding, not a criticism.** TOON is doing what it says on the
tin; a faithful proxy just doesn't get to hand it the data it's good at.
Measurements below were taken against the published `@toon-format/cli` in
mid-2026; the format is under active development with a spec-first RFC process,
so a nested-table syntax could change our numbers — the reproduction steps are
at the bottom.

## What TOON is

Its core idea is sound and worth understanding: **write each key once**. Where
JSON repeats every field name on every array element, TOON declares a header
and follows it with rows.

Given `{"data":[{"id":"cus_1","email":"a@x.co","status":"active"}, …]}`:

```
data[3]{id,email,status}:
  cus_1,a@x.co,active
  cus_2,b@x.co,active
  cus_3,c@x.co,churned
```

Row count and field list are declared up front (`[3]{id,email,status}`), which
also gives the model a validity check. Scalars use `key: value` lines; the row
delimiter can be comma, tab, or pipe. **On this shape it works: 43 tokens vs
compact JSON's 58 — a 26% saving.**

The catch is what happens when an array *isn't* flat and uniform. TOON's
tabular form requires every element to be a flat object with identical keys.
Anything else — a nested object, an inner array, a missing key — falls back to
an indented list form:

```json
{"object":"list","has_more":false,"data":[
  {"id":"cus_1","email":"a@x.co","customer":{"plan":"pro","seats":3}},
  {"id":"cus_2","email":"b@x.co","customer":{"plan":"free","seats":1}},
  {"id":"cus_3","customer":{"plan":"pro","seats":9}}]}
```

```
object: list
has_more: false
data[3]:
  - id: cus_1
    email: a@x.co
    customer:
      plan: pro
      seats: 3
  - id: cus_2
    email: b@x.co
    customer:
      plan: free
      seats: 1
  - id: cus_3
    customer:
      plan: pro
      seats: 9
```

Every key is repeated per row again, plus `- ` markers and indentation:
**95 tokens vs compact JSON's 81 — 17% worse than the format it set out to
compress.** Two disqualifiers did that: a nested object (`customer`) and a
ragged row (`cus_3` has no email). Real REST payloads have both, almost by
definition.

Even data that is *already* a table loses. `{"columns":[...],"data":[[...]]}`
(the pandas/DB shape) becomes:

```
columns[4]: id,name,role,status
data[3]:
  - [4]: 1,Alice,Admin,Active
  - [4]: 2,Bob,User,Inactive
  - [4]: 3,Charlie,User,Active
```

**56 tokens vs 42 compact** (+33%): there is no key repetition left to remove,
so the `- [4]: ` prefixes are pure overhead.

## What we measured

Four evaluations, run months apart with different corpora and encoders. Every
number below is inlined rather than cited — this page is meant to stand on its
own.

**1. Static tokens on real API payloads** (8 large Stripe/GitHub list
responses, `o200k_base`). TOON lost to compact JSON on **every single one**,
by 1–23%; aggregate −4%. Its only "win" was against *pretty* JSON — i.e.
whitespace removal, which compact JSON already does for free and better.

**2. Live task run** (Stripe suite, one run per format). TOON failed a counting
task that both JSON formats passed, burning 161K tokens hitting the turn limit.
**We since established this was mostly not TOON's fault**: a later 7-format
study found counting accuracy is **40–60% for every format including plain
JSON**, on both a weak and a strong model — it's a model-capability wall, not a
serialization property. One run, one model, one task; we report it because it
happened, not as evidence against the format.

**3. Fresh corpus + the official encoder** (12 live Stripe/GitHub payloads,
encoded by `@toon-format/cli` rather than our own implementation, so the result
can't be blamed on us): **+10% tokens vs compact JSON aggregate, worse on all
12 payloads** (+4% to +19%). Note the baseline — **compact** JSON. TOON's own
headline compares against JSON as it is usually serialized, and against
whitespace-heavy JSON it does win here too; minifying first is free, so compact
is the baseline we hold everything to.

**4. Comprehension** (7 formats × 2 model tiers, paired questions,
programmatic ground truth):

| format | accuracy (flash-lite / haiku-4.5) | mean tokens |
|---|--:|--:|
| compact JSON | 90.1% / 87.9% | 13,843 |
| TOON | 87.9% / 86.8% | 15,641 (+13%) |

**On accuracy TOON is fine** — statistically indistinguishable from compact
JSON on both tiers (CIs overlap heavily), which is consistent with TOON's own
claim that it matches JSON's retrieval accuracy. Our finding is about *tokens*
on our data, not comprehension: it cost 13% more.

A third-party study
([improvingagents.com](https://www.improvingagents.com/blog/is-toon-good-for-table-data))
reports TOON much lower — 47.5%, indistinguishable from CSV's 44.3% and well
under Markdown-KV's 60.7%. We'd treat that with the same caution TOON's
maintainers would: it used a small model on a 1,000-row synthetic table, a
regime where *every* format degrades (nothing there cleared 61%) and where
keys-once formats lose the key-to-value locality a weak model leans on. Our own
numbers, on real payloads at realistic sizes, are the ones we act on — and they
put TOON at parity.

## Why the numbers come out that way

Not because the format is badly built — because of an unavoidable interaction:

1. **The win requires flat, non-ragged uniform arrays.** Reaching that shape
   from a real REST payload means *dropping fields* — a lossy semantic
   transform. A faithful proxy must not silently discard data
   (`get_tool_details` returns the API's own schema verbatim for the same
   reason), so the shape where TOON wins is a shape we're not allowed to
   manufacture.
2. **The two fixes are deliberately out of the format's scope.** Community PRs
   exist for both — nested dotted-column tables
   ([#296](https://github.com/toon-format/toon/pull/296), following the
   project's spec-first RFC process) and semi-uniform/ragged normalization
   ([#292](https://github.com/toon-format/toon/pull/292)) — and the latter was
   closed on a clear principle: *"TOON is a transport format… splitting
   semi-uniform arrays is a data transformation step that belongs in the
   application layer, not inside the encoder."* That's a defensible line, and
   holding it is why TOON stays a small format rather than a data-munging
   library. We agree with it — which is why we tested those two levers
   proxy-side instead of adopting the format. Applied as a *selective* hybrid
   (JSON skeleton, tables only where a table measurably wins), they turn TOON's
   +10% into −18%; rendered as markdown tables instead of a custom grammar,
   −30% at no measured comprehension cost. That result is why the shipped
   direction is "tabularize selectively inside the proxy", not "adopt TOON".
3. **Custom grammar is a structural risk.** An MCP consumer is not always a
   model. Programs read tool results too — including min-mcp's own `fields`
   projection, overlay transforms, pagination merging, and `minmcp verify`
   assertions. Compact JSON is consumable by all of them; TOON needs a
   bespoke decoder everywhere in the chain.
4. **No usable Rust dependency at the time.** TOON's own SDK is TypeScript;
   the `toon-format` crate we found (v0.5) was a TUI application pulling in
   clipboard, terminal, image-codec and syntax-highlighting crates — fine for
   its purpose, untenable for a minimal proxy. That's an ecosystem gap, not a
   format flaw, and it may well be closed by now. We hand-wrote a
   zero-dependency encoder instead (~330 lines), then deleted it when the
   measurements came in.

## The decision

**min-mcp does not emit TOON.** `result_format` offers `json` (compact, default)
and `raw` (byte-for-byte passthrough). The TOON encoder that existed in v0.1 was
removed in the leanness pass, under this repo's rule: *ship only what beat the
baseline* — the same rule that removed two of our own surface modes.

What we ship or are evaluating instead:

| option | status | tokens vs compact JSON | consumable by |
|---|---|--:|---|
| **compact JSON** | **default, shipped** | — | anything |
| `raw` | shipped (opt-out) | +0–38% | anything |
| markdown tables (`mdtab`) | research candidate, gated | **−30%** | models (custom decoder) |
| columnar split-orient JSON | research candidate | **−36%** | anything (still pure JSON) |
| TOON | **not adopted** | **+10%** on our shapes | needs a TOON decoder |

The row that matters is the last column as much as the numbers: min-mcp's own
machinery (field projection, response transforms, `verify` assertions) parses
results structurally, so a custom grammar costs us more than it would cost an
application that only feeds a model.

## When TOON *is* a reasonable choice

To be fair to it — outside a faithful proxy, TOON earns its keep when all
three hold: your arrays are **flat and uniform** (you control the schema, e.g.
an analytics extract or a report you assemble yourself), the consumer is
**only ever a model**, and you can adopt its decoder wherever the data lands.
That's a real and common situation — just not the situation of a proxy that
must relay someone else's nested REST responses byte-faithfully. Atlassian's
mcp-compressor ships a `--toonify` flag for exactly that use case; measured on
our payloads its `--toonify` flag delivered −9% on one nested body and 0% on a
live Stripe call — consistent with everything above.

## Further reading

- [TOON specification and SDK](https://github.com/toon-format/toon) — the
  format itself, its benchmarks, and the RFC process
- [Configuration](configuration.md) — the `result_format` knob min-mcp ships
- [Concepts](concepts.md) — where result shaping sits in the call path
- Independent third-party measurements:
  [format comparison](https://www.improvingagents.com/blog/best-input-data-format-for-llms/),
  [TOON for table data](https://www.improvingagents.com/blog/is-toon-good-for-table-data)
