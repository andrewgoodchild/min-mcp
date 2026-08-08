# Third-party notices

This repository is licensed under Apache-2.0 ([`LICENSE`](LICENSE)). It does **not
redistribute any third-party source in-tree** — dependencies are fetched by Cargo,
and any OpenAPI spec you mount is supplied by you. This file records what those are
and where they come from.

## OpenAPI specs (you supply them, not redistributed)

min-mcp mounts an OpenAPI spec you point it at (the `spec:` path); the spec is
**not** committed to this repo and remains under its own upstream licence. Common
public ones people mount:

| spec | source | licence |
|---|---|---|
| Stripe API | [`stripe/openapi`](https://github.com/stripe/openapi) | MIT |
| GitHub REST API | [`github/rest-api-description`](https://github.com/github/rest-api-description) | MIT |
| Kubernetes | [`kubernetes/kubernetes`](https://github.com/kubernetes/kubernetes) (`api/openapi-spec`) | Apache-2.0 |

The bundled sample spec used by the offline examples
([`examples/specs/acme-store.json`](examples/specs/acme-store.json)) is original
to this repo, not a third-party document.

## Rust dependencies (fetched by Cargo)

Not redistributed in-tree; see [`Cargo.toml`](Cargo.toml) / [`Cargo.lock`](Cargo.lock)
for exact versions. Principal crates and their licences:

| crate | purpose | licence |
|---|---|---|
| `rmcp` | official MCP SDK (server transports) | MIT |
| `tokio` | async runtime | MIT |
| `hyper` / `hyper-util` | HTTP server for the Streamable transport | MIT |
| `reqwest` | HTTP client (spec-backed calls) | MIT / Apache-2.0 |
| `serde`, `serde_json`, `serde_yaml_ng` | (de)serialisation | MIT / Apache-2.0 |
| `jaq-*` | jq engine (response `jq` transform) | MIT |
| `jsonwebtoken` | JWT validation | MIT |
| `clap` | CLI parsing | MIT / Apache-2.0 |
| `tracing` / `tracing-subscriber` | logging | MIT |
| `anyhow` | error handling | MIT / Apache-2.0 |
