//! Outbound OAuth 2.0 client-credentials for upstreams. min-mcp fetches a bearer
//! token from the configured `token_url`, caches it, and refreshes shortly
//! before expiry — so an OAuth-protected remote MCP server can be proxied with
//! just a client_id/secret in config, no hand-minted tokens.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::config::OAuthConfig;
use crate::secrets::Secrets;
use crate::sync::lock;

pub struct OAuthClient {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    /// `${…}`-resolved; a secret, so never in Debug output.
    client_secret: SecretString,
    scope: Option<String>,
    /// The current token and when it stops being usable. A std lock: reads are
    /// the common case and must not queue behind an in-flight fetch.
    cached: Mutex<Option<(SecretString, Instant)>>,
    /// Held only while fetching, so concurrent callers share one round-trip to
    /// the token endpoint instead of each minting their own.
    fetching: tokio::sync::Mutex<()>,
}

impl OAuthClient {
    pub async fn new(cfg: &OAuthConfig, secrets: &Secrets) -> Result<Self> {
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
            client_secret: SecretString::from(secrets.expand(&cfg.client_secret).await?),
            scope: cfg.scope.clone(),
            cached: Mutex::new(None),
            fetching: tokio::sync::Mutex::new(()),
        })
    }

    /// The cached token, if it is still good.
    fn current(&self) -> Option<String> {
        let cached = lock(&self.cached);
        let (token, expiry) = cached.as_ref()?;
        (Instant::now() < *expiry).then(|| token.expose_secret().to_string())
    }

    /// A valid bearer token — cached until shortly before expiry, then refetched.
    /// Returned exposed: it goes straight into an Authorization header.
    pub async fn bearer(&self) -> Result<String> {
        if let Some(t) = self.current() {
            return Ok(t);
        }
        let _flight = self.fetching.lock().await;
        if let Some(t) = self.current() {
            return Ok(t); // another caller refreshed while we waited
        }
        let mut form = vec![
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.expose_secret()),
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
        *lock(&self.cached) = Some((SecretString::from(token.clone()), Instant::now() + lifetime));
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
