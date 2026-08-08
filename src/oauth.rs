//! Outbound OAuth 2.0 client-credentials for upstreams. min-mcp fetches a bearer
//! token from the configured `token_url`, caches it, and refreshes shortly
//! before expiry — so an OAuth-protected remote MCP server can be proxied with
//! just a client_id/secret in config, no hand-minted tokens.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::config::{expand_env, OAuthConfig};

pub struct OAuthClient {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String, // env-expanded
    scope: Option<String>,
    cached: Option<(String, Instant)>,
}

impl OAuthClient {
    pub fn new(cfg: &OAuthConfig) -> Result<Self> {
        // Bound the token fetch: bearer() is awaited before every HTTP-upstream
        // request, so a hung token endpoint would stall min-mcp indefinitely
        // (reqwest has no default timeout). Token endpoints respond fast.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building OAuth HTTP client")?;
        Ok(OAuthClient {
            client,
            token_url: cfg.token_url.clone(),
            client_id: cfg.client_id.clone(),
            client_secret: expand_env(&cfg.client_secret)?,
            scope: cfg.scope.clone(),
            cached: None,
        })
    }

    /// A valid bearer token — cached until shortly before expiry, then refetched.
    pub async fn bearer(&mut self) -> Result<String> {
        if let Some((token, expiry)) = &self.cached {
            if Instant::now() < *expiry {
                return Ok(token.clone());
            }
        }
        let mut form = vec![
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
        ];
        if let Some(scope) = &self.scope {
            form.push(("scope", scope.as_str()));
        }
        let resp = self
            .client
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .with_context(|| format!("requesting OAuth token from {}", self.token_url))?;
        if !resp.status().is_success() {
            bail!("OAuth token endpoint {} returned {}", self.token_url, resp.status());
        }
        let body: Value = resp.json().await.context("OAuth token response was not JSON")?;
        let (token, ttl) = parse_token_response(&body)?;
        // refresh a minute early to avoid using a token that expires mid-flight
        let lifetime = Duration::from_secs(ttl.saturating_sub(60).max(1));
        self.cached = Some((token.clone(), Instant::now() + lifetime));
        Ok(token)
    }
}

/// Pull `(access_token, expires_in_secs)` from a token response (expires_in
/// defaults to 3600 when absent, per common practice).
pub fn parse_token_response(body: &Value) -> Result<(String, u64)> {
    let token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OAuth token response missing 'access_token'"))?
        .to_string();
    let ttl = body.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
    Ok((token, ttl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_token_and_ttl() {
        let (t, ttl) = parse_token_response(&json!({"access_token": "abc", "expires_in": 900})).unwrap();
        assert_eq!(t, "abc");
        assert_eq!(ttl, 900);
    }

    #[test]
    fn ttl_defaults_when_absent_and_errors_without_token() {
        let (_, ttl) = parse_token_response(&json!({"access_token": "x"})).unwrap();
        assert_eq!(ttl, 3600);
        assert!(parse_token_response(&json!({"token_type": "bearer"})).is_err());
    }
}
