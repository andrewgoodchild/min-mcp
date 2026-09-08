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

    // --- against a real token endpoint, in-process ---------------------------

    use crate::testserver::{self, Reply};

    fn cfg(token_url: String) -> OAuthConfig {
        OAuthConfig {
            token_url,
            client_id: "id-1".into(),
            client_secret: "shh".into(),
            scope: Some("read write".into()),
        }
    }

    async fn client(url: String) -> OAuthClient {
        OAuthClient::new(&cfg(url), &Secrets::env_only()).await.expect("build client")
    }

    #[tokio::test]
    async fn fetches_a_token_and_posts_the_client_credentials_grant() {
        let srv = testserver::spawn(|_, _| Reply::json(r#"{"access_token":"tok-1","expires_in":3600}"#)).await;
        let c = client(srv.url()).await;
        assert_eq!(c.bearer().await.unwrap(), "tok-1");

        // The form is what an OAuth server expects, secret included — and the
        // configured scope is forwarded rather than dropped.
        let body = &srv.seen()[0].body;
        for field in ["grant_type=client_credentials", "client_id=id-1", "client_secret=shh", "scope=read"] {
            assert!(body.contains(field), "form missing {field}: {body}");
        }
    }

    #[tokio::test]
    async fn a_cached_token_is_reused_instead_of_refetched() {
        // bearer() runs before EVERY upstream request, so a cache miss per call
        // would put a token round-trip in front of all of them.
        let srv = testserver::spawn(|n, _| {
            Reply::json(format!(r#"{{"access_token":"tok-{n}","expires_in":3600}}"#))
        })
        .await;
        let c = client(srv.url()).await;
        assert_eq!(c.bearer().await.unwrap(), "tok-1");
        assert_eq!(c.bearer().await.unwrap(), "tok-1", "second call must come from cache");
        assert_eq!(c.bearer().await.unwrap(), "tok-1");
        assert_eq!(srv.hits(), 1, "the token endpoint must be hit once");
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_round_trip() {
        // Single-flight: without it, a cold cache under load mints one token per
        // in-flight request and hammers the IdP.
        let srv = testserver::spawn(|n, _| {
            Reply::json(format!(r#"{{"access_token":"tok-{n}","expires_in":3600}}"#))
        })
        .await;
        let c = std::sync::Arc::new(client(srv.url()).await);
        let mut set = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            set.push(tokio::spawn(async move { c.bearer().await.unwrap() }));
        }
        let mut tokens = Vec::new();
        for t in set {
            tokens.push(t.await.unwrap());
        }
        assert_eq!(srv.hits(), 1, "8 concurrent callers must share one fetch, got {} hits", srv.hits());
        assert!(tokens.iter().all(|t| t == "tok-1"), "all callers get the same token: {tokens:?}");
    }

    #[tokio::test]
    async fn a_token_is_refetched_once_it_nears_expiry() {
        // The cache expires a minute EARLY, so `expires_in: 61` is usable for
        // about a second — enough to observe the refresh without a slow test.
        let srv = testserver::spawn(|n, _| {
            Reply::json(format!(r#"{{"access_token":"tok-{n}","expires_in":61}}"#))
        })
        .await;
        let c = client(srv.url()).await;
        assert_eq!(c.bearer().await.unwrap(), "tok-1");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(c.bearer().await.unwrap(), "tok-2", "an expiring token must be refreshed");
        assert_eq!(srv.hits(), 2);
    }

    #[tokio::test]
    async fn a_refusal_from_the_token_endpoint_names_the_status() {
        let srv = testserver::spawn(|_, _| Reply::status(401)).await;
        let err = format!("{:#}", client(srv.url()).await.bearer().await.unwrap_err());
        assert!(err.contains("401"), "the error should name the status: {err}");
    }

    #[tokio::test]
    async fn a_malformed_token_response_is_an_error_not_a_panic() {
        let srv = testserver::spawn(|_, _| Reply::json("not json at all")).await;
        assert!(client(srv.url()).await.bearer().await.is_err(), "non-JSON must not panic");

        let srv = testserver::spawn(|_, _| Reply::json(r#"{"token_type":"bearer"}"#)).await;
        let err = format!("{:#}", client(srv.url()).await.bearer().await.unwrap_err());
        assert!(err.contains("access_token"), "should say what was missing: {err}");
    }

    #[tokio::test]
    async fn an_unreachable_token_endpoint_names_the_url() {
        // Port 1 on loopback: nothing listens, so this fails to connect.
        let c = client("http://127.0.0.1:1/token".into()).await;
        let err = format!("{:#}", c.bearer().await.unwrap_err());
        assert!(err.contains("127.0.0.1:1"), "the error should name the endpoint: {err}");
    }
}
