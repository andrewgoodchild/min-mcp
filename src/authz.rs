//! The external authorizer: one `POST` per tool call, answered allow or deny.
//!
//! min-mcp decides *which tools a caller can see* (scopes). Whether this caller
//! may make *this* call, with *these* arguments, is policy — a warrant
//! verifier, OPA, Cedar, a service of your own — and min-mcp is deliberately
//! not a policy engine. What it can be is the gate: it holds the caller it
//! authenticated, the resolved tool, the arguments the agent sent, and every
//! step a composite runs, none of which a gateway in front can see. So it asks.
//!
//! The contract is Envoy `ext_authz` in shape, so an authorizer written for a
//! gateway works here unchanged: any 2xx allows (unless the body says
//! `{"allow": false}`), any 4xx denies with the body as the reason, and a 5xx,
//! a timeout or no connection is an outage that `fail_open` decides — refuse
//! by default, because an authorizer that is bypassed when down is advisory,
//! and whoever can reach it can also make it unreachable.

use std::time::Duration;

use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Map, Value};

use crate::caller::Caller;
use crate::config::AuthorizerConfig;
use crate::secrets::Secrets;

/// The longest reason relayed to the agent and the audit line. A reason is a
/// sentence; past this it is a body that was never meant as one.
const REASON_MAX: usize = 500;

/// The JSON fields a denial's reason is read from, in order.
const REASON_FIELDS: [&str; 5] = ["reason", "message", "error", "detail", "code"];

/// What the authorizer is asked about. Borrowed: built on the call path, once
/// per call.
pub struct Request<'a> {
    pub caller: &'a Caller,
    pub tool_id: &'a str,
    pub upstream: &'a str,
    pub name: &'a str,
    pub read_only: Option<bool>,
    /// The arguments as the caller sent them — before overlay defaults and
    /// injected values, so a secret never leaves the proxy.
    pub arguments: &'a Value,
    /// The client's forwarded request `_meta`, if any: where a warrant or a
    /// trace context rides.
    pub meta: Option<&'a Map<String, Value>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { reason: String },
    /// No usable answer: a transport failure, a timeout, a 5xx, a redirect off
    /// the origin. Not a denial — `fail_open` says what it means.
    Unavailable { why: String },
}

pub struct Authorizer {
    client: reqwest::Client,
    url: String,
    /// Static headers, `${…}` references resolved; secrets, so never printed.
    headers: Vec<(String, SecretString)>,
    fail_open: bool,
    report_only: bool,
    include_arguments: bool,
}

impl Authorizer {
    pub async fn new(cfg: &AuthorizerConfig, secrets: &Secrets) -> Result<Self> {
        crate::crypto::install_provider();
        let mut headers = Vec::with_capacity(cfg.headers.len());
        for (k, v) in &cfg.headers {
            headers.push((k.clone(), SecretString::from(secrets.expand(v).await?)));
        }
        // The timeout IS the control: this sits in front of every call, so a
        // hung authorizer would stall the proxy. Redirects stay on the origin,
        // so the configured headers never reach a third party.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .redirect(crate::redirect::same_origin())
            .build()
            .context("building the authorizer HTTP client")?;
        Ok(Authorizer {
            client,
            url: cfg.url.clone(),
            headers,
            fail_open: cfg.fail_open,
            report_only: cfg.report_only,
            include_arguments: cfg.include_arguments,
        })
    }

    pub fn fail_open(&self) -> bool {
        self.fail_open
    }

    pub fn report_only(&self) -> bool {
        self.report_only
    }

    /// The request body. `arguments` is optional (`include_arguments`): they
    /// can carry customer data a policy about tools has no need of.
    fn body(&self, req: &Request<'_>) -> Value {
        let mut body = json!({
            "caller": {"subject": req.caller.subject, "scopes": req.caller.scopes},
            "tool": {"id": req.tool_id, "upstream": req.upstream, "name": req.name, "read_only": req.read_only},
        });
        if self.include_arguments {
            body["arguments"] = req.arguments.clone();
        }
        if let Some(m) = req.meta {
            body["meta"] = Value::Object(m.clone());
        }
        body
    }

    pub async fn decide(&self, req: &Request<'_>) -> Decision {
        let mut rb = self.client.post(&self.url).json(&self.body(req));
        for (k, v) in &self.headers {
            rb = rb.header(k, v.expose_secret());
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Decision::Unavailable { why: "timed out".into() },
            Err(e) => return Decision::Unavailable { why: e.to_string() },
        };
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        classify(status, &text)
    }
}

/// Envoy `ext_authz` semantics, read off the status; the body refines a 2xx
/// and names the reason for a 4xx.
fn classify(status: u16, body: &str) -> Decision {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    match status {
        200..=299 => match parsed.as_ref().and_then(|v| v.get("allow")).and_then(Value::as_bool) {
            Some(false) => Decision::Deny { reason: reason_of(parsed.as_ref(), body, "denied by the authorizer") },
            _ => Decision::Allow,
        },
        400..=499 => Decision::Deny { reason: reason_of(parsed.as_ref(), body, &format!("HTTP {status}")) },
        _ => Decision::Unavailable { why: format!("HTTP {status}") },
    }
}

/// The reason an authorizer gave: a known string field in a JSON body, else a
/// plain-text body, else `fallback`. One line, bounded — it lands in the
/// agent's result and in an audit line.
fn reason_of(parsed: Option<&Value>, body: &str, fallback: &str) -> String {
    let from_json = parsed.and_then(|v| REASON_FIELDS.iter().find_map(|k| v.get(*k).and_then(Value::as_str)));
    let plain = (parsed.is_none()).then(|| body.trim()).filter(|t| !t.is_empty());
    let reason = from_json.map(str::trim).filter(|s| !s.is_empty()).or(plain).unwrap_or(fallback);
    let one_line = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(REASON_MAX) {
        Some((cut, _)) => format!("{}…", &one_line[..cut]),
        None => one_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(d: Decision) -> String {
        match d {
            Decision::Deny { reason } => reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn a_2xx_allows_unless_the_body_says_otherwise() {
        assert_eq!(classify(200, ""), Decision::Allow);
        assert_eq!(classify(200, r#"{"allow": true}"#), Decision::Allow);
        assert_eq!(classify(204, "not json"), Decision::Allow, "ext_authz: the status decides");
        assert_eq!(deny(classify(200, r#"{"allow": false, "reason": "staging only"}"#)), "staging only");
        assert_eq!(deny(classify(200, r#"{"allow": false}"#)), "denied by the authorizer");
    }

    #[test]
    fn a_4xx_denies_with_the_bodys_reason() {
        assert_eq!(deny(classify(403, r#"{"reason": "cluster must match staging-*"}"#)), "cluster must match staging-*");
        assert_eq!(deny(classify(403, r#"{"code": "constraint_violation"}"#)), "constraint_violation");
        assert_eq!(deny(classify(401, "missing warrant\n")), "missing warrant");
        assert_eq!(deny(classify(400, r#"{"allow": false}"#)), "HTTP 400", "json without a reason field");
        assert_eq!(deny(classify(404, "")), "HTTP 404");
    }

    #[test]
    fn a_5xx_or_odd_status_is_an_outage_not_a_denial() {
        assert_eq!(classify(503, "down"), Decision::Unavailable { why: "HTTP 503".into() });
        assert_eq!(classify(304, ""), Decision::Unavailable { why: "HTTP 304".into() });
    }

    #[test]
    fn a_reason_is_one_bounded_line() {
        let messy = format!("{{\"reason\": \"line one\\n   line two {}\"}}", "x".repeat(600));
        let r = deny(classify(403, &messy));
        assert!(r.starts_with("line one line two x"), "{r}");
        assert!(r.ends_with('…'));
        assert_eq!(r.chars().count(), REASON_MAX + 1);
    }
}
