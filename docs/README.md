# min-mcp documentation

min-mcp is a **minifying proxy** for MCP servers and OpenAPI specs: your agent
sees one small, searchable, scope-filtered surface instead of every upstream's
full tool catalog — with a source map back to every original tool.

New here? Read [Getting started](getting-started.md), then [Concepts](concepts.md).

## Getting started
- **[Getting started](getting-started.md)** — build the binary, proxy your first
  MCP server, mount an OpenAPI spec, serve over stdio or HTTP.

## Concepts
- **[Concepts](concepts.md)** — the "minifier" model, the three-tool surface,
  response projection, the source map, and the verification-first ethos.

## Configuration & usage
- **[Configuration reference](configuration.md)** — every YAML key: upstreams
  (subprocess / remote HTTP / OpenAPI spec), filters, scopes, auth, logging.
- **[Overlays](overlays.md)** — fix a tool you don't own: patch descriptions,
  field docs, and errors; reshape responses; inject request defaults; bind safely.
- **[Composites](composites.md)** — expose a fixed multi-step chain as one tool
  (`workflows:`), and the write-safety rules that keep it honest.
- **[Transports & auth](transports-and-auth.md)** — stdio vs Streamable HTTP,
  remote MCP upstreams, outbound OAuth, JWT-derived caller scopes, security.
- **[CLI reference](cli.md)** — `serve`, `inspect`, `map`, `verify`, `lint`,
  `search`, `help`, `call`, and their flags.

## Evidence
- **[Measurements](measurements.md)** — every published number, how to reproduce
  the ones that are reproducible, and an explicit list of what *isn't* measured.

## Background — the landscape, measured
- **[About mcp-compressor](about-mcp-compressor.md)** — the closest peer to
  min-mcp (same deferred-schema architecture, a listing instead of search):
  what it is, and what a same-upstream head-to-head measured.
- **[About TOON](about-toon.md)** — the token-oriented serialization format,
  worked examples, four evaluations, and why min-mcp doesn't emit it.
