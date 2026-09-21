# Security

min-mcp sits in the request path between an agent and the tools it calls, so it
is worth being precise about what it protects, what it does not, and what each
control assumes about your deployment.

Everything here is **off by default**. min-mcp on loopback with no `auth:` is a
development tool, and that is the right default for one. The controls below are
what you turn on when it becomes infrastructure.

> Detail lives in [Transports, identity & ops](transports-and-auth.md); this
> page is the map, and the honest limits.

## The short version

| Question | Answer |
|---|---|
| Who is calling? | `auth:` — a validated bearer, or identity headers from a gateway |
| What may they reach? | `scopes:` — default-deny once any rule exists |
| Is their token still good? | `auth.introspection` — RFC 7662 revocation |
| How often may they call? | `rate_limits` — keyed per caller |
| Is the wire protected? | `http.tls`, and `client_ca_file` for mutual TLS |
| What can one client cost me? | `http.limits`, `max_concurrent_calls`, connection deadlines |
| Are the tools themselves honest? | `poisoning_policy` — definitions checked at startup |
| Can I trust the record afterwards? | `log_hmac_key` + `minmcp audit-verify` |
| Are the dependencies clean? | `cargo audit` in CI, weekly and on every push |

## Identity is per request, not per process

Over HTTP every request is authenticated on its own, so one `serve --http`
serves many callers, each seeing only what its scopes grant. A request with no
identity is a 401 unless `auth.allow_anonymous` says otherwise.

Over **stdio** there is no per-request identity — one process, one caller, by
construction. Scopes still apply; they just come from `--jwt` or `--scopes`.

**The boundary that is easy to get wrong:** `auth.trusted_headers` believes what
a gateway tells it. That is only sound if the gateway is genuinely the sole
route to the port. Mutual TLS (`http.tls.client_ca_file`) is how you enforce
that rather than assume it — it authenticates the *channel*, so nothing else can
reach the port to set its own identity headers.

## What each control does not cover

Worth reading before you rely on one.

- **Scopes** hide tools from a caller. They are not a data-access control: a
  tool a caller *can* reach returns whatever the upstream returns.
- **Rate limits** key on the caller's subject, falling back to the client
  certificate or peer address. Callers behind one shared gateway arrive on one
  address, so without `trusted_headers.subject` they share a bucket. min-mcp
  warns at startup when that applies.
- **Revocation** costs a round trip, so verdicts are cached: `cache_ttl_s` *is*
  your revocation latency. `fail_open` is off by default, which means an
  unreachable authorization server refuses requests rather than waving them
  through.
- **`max_in_flight`** bounds HTTP request *handling*, not upstream work — its
  permit is released before the tool runs. **`max_concurrent_calls`** is the one
  that bounds concurrent upstream calls.
- **Poisoning checks** read tool *definitions* — name and description. They do
  not inspect arguments or responses, and `poisoning_policy` is `warn` by
  default, so a flagged tool is still served unless you set `strict`.
- **Tamper-evident audit** proves the record was not altered. It does not keep a
  copy: an attacker with the key can forge, and one who truncates the tail
  leaves a prefix that still verifies. Ship lines to a SIEM if you need an
  external record of how far the log had got.
- **TLS** protects the wire. Without `http.tls` the port is plaintext and
  belongs behind a gateway that terminates TLS for it.

## Deliberately not built

Not oversights — each has a reason, and a shape of deployment that answers it.

- **Per-tenant isolation in one process.** One `Surface` backs every session by
  design. Run one process per tenant behind the gateway.
- **Session-identity binding.** Scopes always come from the current request's
  token, never from session state, so a session id carries no privilege on its
  own.
- **Argument taint tracking and response scanning.** A guardrail product's job.
  For stripping fields out of a payload, overlay `response.remove` is the tool.

## Supply chain

`cargo audit` runs on every push to `main`, on any dependency change, and
**weekly** — because advisories are published against dependencies that have not
moved. It has already caught one: a rustls TLS advisory filed against a version
nothing in the repo had touched.

Vulnerabilities and yanked crates fail the build. `unmaintained` is a warning:
it is a maintenance signal rather than an exploitable flaw, and a gate that is
permanently red for something no change can fix is a gate people learn to
ignore.

## Reporting

Found something? Open an issue describing the class of problem rather than a
working exploit, and say which version you were on.
