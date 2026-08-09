# Contributing

Thanks for looking. min-mcp is a single Rust binary with no runtime services, so
the loop is short.

## Setup

```sh
git clone https://github.com/andrewgoodchild/min-mcp && cd min-mcp
cargo build --release
```

Stable Rust; nothing else. Check it works:

```sh
./target/release/minmcp inspect --config bench/bigapi.yaml   # 120 tools → 3
```

## Run what CI runs — before you push

CI's blocking job is **clippy with warnings denied**, and `cargo test` passing
tells you nothing about it. Run both:

```sh
cargo clippy --release --all-targets -- -D warnings
cargo test --release
```

The test suite is offline and credential-free: bundled OpenAPI specs plus
scriptable POSIX-sh MCP servers under `tests/fixtures/`. Suites:

| suite | what it covers |
|---|---|
| `cargo test --bin minmcp` | unit tests, including the surface internals |
| `tests/stdio_e2e.rs` | the real binary over stdio: search → details → call, resources, breaker, timeout |
| `tests/http_e2e.rs` | Streamable HTTP: handshake, session, Origin refusal |
| `tests/features_e2e.rs` | composites, `verify`, pagination, JWT scopes, preflight opt-out |
| `tests/cli_contract.rs` | flags, exit codes, the startup banner |
| `tests/compression.rs` | the compression claim — this one is a published number, keep it honest |

Tests needing `/bin/sh` or `curl` are `cfg(unix)`-gated, so Windows runs 17 of
the 24 end-to-end tests — `http_e2e.rs` skips entirely. All three platforms are
**blocking** in CI regardless, because `release.yml` publishes a binary for each
one and none should ship from a tree nothing checked. If you port a fixture to
Windows, drop its gate.

## The bar for a change

This repo's rule is **ship only what beat the baseline** — three features
(`hotset`, `pd`, a TOON encoder) were built, measured, and removed for losing.
So:

- **A behaviour claim needs a measurement**, not an argument. If you add a
  feature that's meant to save tokens or improve success, say how you measured
  it. If you can't measure it yet, say that too — see
  [docs/measurements.md](docs/measurements.md) for the shape, including its
  "what is NOT measured" section.
- **Errors are prompts.** Anything a model reads on failure should say what
  failed, why, and what to do next. Grep for `PREFLIGHT_ERROR` /
  `BREAKER_OPEN` for the house style.
- **Normalize mechanics, never semantics.** min-mcp obeys what a spec
  *declares* (content type, auth scheme, required fields) and returns the API's
  own schemas verbatim. API-specific knowledge belongs in config/overlay data,
  never in an `if "stripe"` branch.
- **Docs live next to the change.** Every overlay key has a `### key — what it
  does` section in [docs/overlays.md](docs/overlays.md); config keys are in
  [docs/configuration.md](docs/configuration.md).

## Commits and PRs

Conventional-ish subject lines (`fix:`, `docs:`, `feat:`) and a body that
explains *why*, since the diff already shows what. Small PRs get read faster.

## Reporting a problem

Include the config (redact secrets), the command, and `MINMCP_LOG=debug` output
if it's a runtime issue. `minmcp map --config <cfg>` dumps the source map —
every tool traced back to its origin — which is usually the fastest way to see
what min-mcp thinks your upstreams contain.

## Security

Don't open a public issue for a vulnerability. min-mcp sits in the request path
between an agent and its tools, so the interesting classes are path/scope
escapes, credential exposure through logs or generated artefacts, and anything
that lets an upstream influence what other callers can see. Email the maintainer
instead.

## License

Apache-2.0. By contributing you agree your contribution is licensed under it.
