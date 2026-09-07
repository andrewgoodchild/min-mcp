# CLI reference

```
minmcp <command> [--config <path>] [options]
```

All commands share these options (via `--config`'s command group):

| flag | default | meaning |
|---|---|---|
| `--config <path>` | `min.yaml` | the config file to load |
| `--scopes a,b` | *(empty)* | granted scopes for this session (local-dev identity; prefer `--jwt`) |
| `--jwt <token>` | `$MINMCP_JWT` | the PROCESS caller's JWT; its validated scope claim overrides `--scopes`. Over HTTP with `auth:` configured each request brings its own token instead. The env var keeps it out of argv |
| `--http <addr>` | — | *(serve only)* expose over Streamable HTTP at e.g. `127.0.0.1:8080` instead of stdio. Loopback only unless every request must authenticate |
| `--allow-remote` | off | *(serve --http only)* permit a non-loopback bind WITHOUT caller identity — only behind an authenticating proxy. Not needed when `auth:` makes every request authenticate. See [Transports & auth](transports-and-auth.md#streamable-http) |

## Commands

### `serve`
Serve the minified surface as an MCP server — over stdio, or over Streamable HTTP
with `--http`. This is what your agent/MCP client connects to.

```sh
minmcp serve --config myconfig.yaml
minmcp serve --config myconfig.yaml --http 127.0.0.1:8080
minmcp serve --config myconfig.yaml --http 0.0.0.0:8080 --allow-remote   # only behind an authenticating proxy
```

### `inspect`
Print surface statistics (tool counts, modes, upstreams) without serving — a quick
sanity check on a config. `upstreams_stale` lists upstreams that announced a
catalog change since startup (restart to pick it up); `visible_after_scopes` is
for the process caller (`--jwt` / `--scopes`).

```sh
minmcp inspect --config myconfig.yaml
```

### `map`
Print the **source map**: every surface tool mapped back to its origin
(`METHOD /path` or upstream tool), which overlays were applied, and each tool's
schema fingerprint (`schema_sha`). This is the minifier's source map and the
binding registry.

```sh
minmcp map --config myconfig.yaml                 # print the current map
minmcp map --config myconfig.yaml --diff old.json # compare vs a saved map
```

With `--diff`, it compares against a saved map and reports which tools changed and
which overlays must be re-verified — **exit code 1** if anything breaking changed.
Drop it in CI to catch a vendor's breaking change before your agents do. See
[Overlays → binding](overlays.md#binding-safely-against-upstream-drift).

### `lint`
Static quality lint of every registered tool — best-practice smells (missing/thin
descriptions, undocumented params, all-optional writes, tools sharing a summary),
grounded in published taxonomies. Reports per-rule aggregate stats plus a sample of
flagged tools. Findings are *drafted*, never applied — it's detection triage and a
patch-drafter, not a breakage detector (that's `verify`).

Three rules cover **tool poisoning** — text in a definition aimed at the model
rather than at the reader:

| rule | fires on |
|---|---|
| `hidden_text` | Zero-width characters, bidi overrides, or tag characters in a name or description — a human reviewer reads one thing, the model another |
| `model_directed_instruction` | Phrases that instruct rather than describe: "ignore previous instructions", "do not tell the user", "before using this tool…" |
| `secret_solicitation` | A local credential artifact a remote tool cannot need — `~/.ssh/id_rsa`, `.aws/credentials`, `.env file` |

These same three rules run at **startup** under
[`poisoning_policy`](configuration.md), which is what the server does about them
— `warn` (default) names each flagged tool and serves it anyway, `strict`
refuses to start, `off` skips the check. The startup check resolves no schemas
(the rules read only the name and description), so unlike `minmcp lint` it is
cheap enough to run on every start.

They are matched on the definition alone, so they catch a poisoned *description*
— not a poisoned *response*, and not argument-level exfiltration. That boundary
is deliberate: min-mcp is a proxy, not a guardrail product. The rules are tuned
against real specs rather than intuition — on Stripe (589 ops) and GitHub (1,216
ops) all three fire **zero** times. Matching credential *nouns* instead of
artifacts flagged 18 GitHub tools, every one a false positive (its PAT-grant and
Actions environment-variable endpoints), which is why the rule matches paths.

That measurement is over OpenAPI specs, whose summaries never address a reader.
Hand-written MCP servers do: "before using this tool, call `search` first" is
ordinary sequencing advice there, and `model_directed_instruction` will flag it.
Treat a hit on an MCP upstream as triage, not a verdict — which is what every
rule here is.

```sh
minmcp lint --config myconfig.yaml              # aggregate stats + flagged sample
minmcp lint --config myconfig.yaml --sample 0   # aggregate only
```

Resolves every tool's schema (O(tools)), so it's opt-in — a huge spec pays the
full-resolution cost here that `serve` defers. See
[Overlays](overlays.md) for how findings map onto fixes.

### `verify`
Run every overlay's `verify:` checks against the **live upstream** — the dynamic
third leg (detect → fix → verify). Each check calls the (fully-overlaid) tool and
asserts deterministically (`status`/`is_error`/`has`/`missing`/`contains`); **exit
code 1** if any check fails. Makes real network calls; drop it in CI to catch
behavioural drift the schema fingerprint can't see, or run it to prove an overlay
actually fixes a tool before you trust it.

```sh
minmcp verify --config myconfig.yaml
```

See [Overlays → verify](overlays.md#verify--dynamic-checks-that-prove-the-fix-works).

### `search`
CLI access to `search_tools` — find the right tool by task description.

```sh
minmcp search --config myconfig.yaml "create a customer"
minmcp search --config myconfig.yaml "create a customer" --k 5
```

### `help`
Print a tool's full description and input schema (like `--help` for one tool) —
CLI access to `get_tool_details`.

```sh
minmcp help --config myconfig.yaml stripe.PostCustomers
```

### `call`
Call a tool by id, with optional GraphQL-style field projection — CLI access to
`call_tool`.

```sh
minmcp call --config myconfig.yaml stripe.PostCustomers \
  --args '{"body":{"email":"a@b.com"}}'

# return ONLY selected response fields (dotted paths; [] maps over an array):
minmcp call --config myconfig.yaml stripe.GetCharges \
  --args '{"query_params":{"limit":3}}' \
  --fields 'data[].id,data[].amount,has_more'
```

Composite tools ([workflows](composites.md)) are called the same way, by their
`id`.
