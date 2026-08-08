# Composites — a multi-step chain as one tool

A `workflows:` entry exposes a **fixed sequence of upstream calls as a single
composite tool** — the linear subset of the
[Arazzo](https://spec.openapis.org/arazzo/latest.html) workflow spec (no
branching, no success-criteria DSL), which covers most of the chains agents
actually run. The agent sees one tool and calls it once; min-mcp runs the steps
internally, threading each step's output into the next.

Measured: exposing one composite cut a 3-step Stripe task **6.8× on tokens** (and
collapsed the token variance ~40×) at equal success.

## Anatomy

```yaml
workflows:
  - id: createCheckoutForNewProduct        # what the agent calls
    description: "Create a product, a one-time price, and a checkout session in one call."
    inputs:                                # JSON Schema of the composite's inputs
      type: object
      required: [name, amount]
      properties:
        name:   { type: string }
        amount: { type: integer, description: price in cents }
    steps:
      - id: product
        tool: stripe.PostProducts
        input: { body: { name: "$inputs.name" } }
        output: { id: id }                 # keep response `id` as steps.product.id
      - id: price
        tool: stripe.PostPrices
        input:
          body:
            product: "$steps.product.id"   # thread the previous step's output
            unit_amount: "$inputs.amount"
            currency: usd                  # constants are hardcoded
        output: { id: id }
      - id: session
        tool: stripe.PostCheckoutSessions
        input:
          body:
            mode: payment
            line_items: [ { price: "$steps.price.id", quantity: 1 } ]
        output: { url: url }
    outputs:                               # what the composite returns
      url: "$steps.session.url"
```

## Runtime expressions

String values in a step `input` (and in `outputs`) may reference:

- `$inputs.<path>` — a field of the composite's own inputs. An **omitted** input
  resolves to `null` and is dropped (not sent as a literal), so optional inputs
  just disappear.
- `$steps.<stepId>.<name>` — a value a prior step extracted via its `output` map.
  A missing step output keeps its literal, so an authoring bug is visible rather
  than silently null.

`output: { name: "dotted.path" }` extracts fields from a step's response body
(e.g. `{ id: id }` keeps `body.id` as `$steps.<id>.id`).

## Authoring guidance

Decide, for each step argument, whether it is:

- **constant** — the same every time → hardcode it in the step (`currency: usd`);
- **threaded** — comes from a prior step's output → `$steps.<id>.<field>`;
- **free** — varies per call → expose it as a composite `input` and reference
  `$inputs.x`.

Keeping the exposed inputs to just the *free* bits is what makes the composite
easy for the agent to call correctly.

## Safety: a composite is not a transaction

On a step failure the composite **aborts fail-fast** and returns
`workflow "<id>" failed at step "<step>": <error>` (`isError: true`); remaining
steps are skipped. But steps that already ran are **not rolled back** — there is
no compensation/saga logic and no idempotency keys. The hazard is entirely about
*writes that already committed*, and it is strictly a multi-write problem:

| class | definition | safe? |
|---|---|---|
| **read-only** | no writes | ✅ nothing to strand |
| **single-terminal-write** | one write, and it is the last step | ✅ a failure means the write never happened |
| **mid-chain-write** | one write, not last | ⚠️ reorder the write last, or use idempotency keys |
| **multi-write** | ≥2 writes | ⚠️ partial completion strands orphans — needs idempotency keys / verification |

The residual edge even a single-terminal-write can't dodge is the universal
**lost-response** case (write commits, response lost in transit → a retry
duplicates); only idempotency keys close that, and it's a property of the
underlying API, not of composites.

This hazard is **composite-only**: an [overlay](overlays.md) is a single-call
transform, so it never issues a second write and is always trivially write-safe.
Composition — issuing call #2 — is what introduces transactionality at all.

For write-chains (like the checkout composite above, which has three writes),
verify the composite against the live API before trusting it. See
[`examples/stripe-composite.yaml`](../examples/stripe-composite.yaml).

## What's not built

Full Arazzo (branching, `successCriteria`, retries, nested workflows), rollback /
compensation, and idempotency-key threading are deliberately out of scope for the
linear subset. A prior experiment in *auto-discovering* composites from agent
traces was investigated and cut.
