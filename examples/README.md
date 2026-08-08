# Example configs

Each is self-documenting; paths inside resolve relative to the file. Every key is
documented in the [configuration reference](../docs/configuration.md).

## Friction-free demos (no network, no credentials)

These mount a small **bundled** sample spec ([`specs/acme-store.json`](specs/acme-store.json))
— nothing is actually called, so you can explore the whole feature set offline:

```sh
minmcp inspect --config examples/demo-overlays.yaml                     # surface stats
minmcp map     --config examples/demo-overlays.yaml                     # source map: which tool each overlay patched
minmcp help    --config examples/demo-overlays.yaml acme.widgets/create # see the patched schema
minmcp lint    --config examples/demo-overlays.yaml                     # best-practice smells
minmcp search  --config examples/demo-overlays.yaml "look up a widget"  # search + aliases
```

| file | what it shows |
|---|---|
| `demo-overlays.yaml` | **Every overlay feature**, one per comment: `description`, `fields` (required / enum / example / **hide** / **`user_supplied`**), `defaults`, per-endpoint `headers` (with `{{uuid}}`/`{{hash}}` generators), `error_hints` (prose + structured **`field:`** + `retryable`), **`preflight`**, `paginate`, `response` (remove/rename/set/`when`), `aliases`, `verify`, and a `strong` binding. |
| `demo-workflow.yaml` | A **composite** `workflows:` tool (`orderNewWidget`) collapsing widget-create → order-create into one call, threading the created id between steps. |
| `demo-scopes.yaml` | Gate visibility: static `filters:` (drop a tool for everyone) + per-caller `scopes:` (default-deny). Try `minmcp map --config examples/demo-scopes.yaml --scopes store.write`. |

## Real-API examples

These mount a real API's OpenAPI spec, so they need the spec on disk and a
credential in the environment:

| file | what it shows |
|---|---|
| `proxy-mcp-server.yaml` | **Start here for a real server.** Template for proxying existing MCP servers behind the 3-tool surface, with overlays. |
| `stripe-from-spec.yaml` | Mount Stripe's OpenAPI spec directly — 587 operations → 3 tools. Needs `STRIPE_TEST_KEY`. |
| `github-from-spec.yaml` | Mount GitHub's OpenAPI spec — 1,216 operations → 3 tools, JSON bodies + auth. Needs `GITHUB_PAT`. |
| `github-fixups.yaml` | **Fix a broken server without forking it** — overlays documenting GitHub's terse `issues/create`, as a drift-checked binding. |
| `stripe-composite.yaml` | A **composite** over Stripe: product → price → checkout session in one call. |
| `stripe-narrow-filter.yaml` | An aggressive response-transform overlay (keeps only id/amount) — server-side response projection. |
| `oauth-upstream.yaml` | Proxy a remote MCP server over HTTP with **outbound OAuth** (client-credentials, fetched + refreshed). |

Download a spec from the API's public OpenAPI repo — e.g. Stripe
(`github.com/stripe/openapi`) or GitHub (`github.com/github/rest-api-description`)
— and point the config's `spec:` at it (big specs aren't committed here).

Secrets are never in these files — `auth_env` names the environment variable the
API key is read from.
