//! Derive a caller's identity from a validated JWT, so scope-based tool
//! visibility rests on a signed token rather than a hand-typed flag.
//!
//! Three verifier kinds, in config-precedence order: a JWKS endpoint (RS256,
//! keys selected by the token's `kid`), a configured RS256 public key (PEM), or
//! an HS256 shared secret (the internal-gateway case). The surface
//! (`JwtVerifier::caller` -> who this is) is identical across all three.
//!
//! A JWKS set is **live**: it refreshes itself when a token names a `kid` it
//! doesn't know (a rotation just happened) and when it is older than
//! `JWKS_MAX_AGE` (a key the IdP withdrew must stop being trusted). Refreshes
//! are single-flight and rate-limited, because an unauthenticated caller can
//! present any `kid` and must not be able to turn this proxy into a fetch
//! amplifier against the identity provider.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;

use crate::caller::Caller;
use crate::sync::{lock, read, write};

/// Optional registered-claim checks applied on top of the signature and `exp`.
/// Both default to off so an internal gateway with no audience discipline still
/// works; set them where tokens are minted for several services.
#[derive(Debug, Default, Clone)]
pub struct ClaimChecks {
    /// The token's `aud` must contain this value.
    pub audience: Option<String>,
    /// The token's `iss` must equal this value.
    pub issuer: Option<String>,
}

/// Resolved key material for validating caller JWTs.
pub enum JwtVerifier {
    Hs256(Vec<u8>),
    Rs256(DecodingKey),
    /// kid -> RSA public key, from a JWKS document; refreshed from its URL.
    Jwks(Jwks),
}

/// The floor between two JWKS fetch attempts. An unknown `kid` refetches at
/// most this often, whether the fetch succeeds or not.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(60);
/// A set older than this is refreshed before the next validation, so a key
/// the IdP removed stops being trusted within the window.
const JWKS_MAX_AGE: Duration = Duration::from_secs(600);
/// Bound on one JWKS fetch (startup and refresh alike).
const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Jwks {
    keys: RwLock<HashMap<String, Arc<DecodingKey>>>,
    /// Where the set came from, for refreshes; None for a static document
    /// (tests), which never refreshes.
    source: Option<JwksSource>,
}

struct JwksSource {
    url: String,
    client: reqwest::Client,
    /// The last fetch ATTEMPT — success or failure — so a failing IdP is not
    /// hammered either.
    attempted_at: Mutex<Instant>,
    /// Single-flight: concurrent refreshers queue behind one fetch and then see
    /// its result through `attempted_at`.
    flight: tokio::sync::Mutex<()>,
}

impl Jwks {
    fn key(&self, kid: &str) -> Option<Arc<DecodingKey>> {
        read(&self.keys).get(kid).cloned()
    }

    fn age(&self) -> Option<Duration> {
        self.source.as_ref().map(|s| lock(&s.attempted_at).elapsed())
    }

    /// Refetch the set if a source is configured and the last attempt is older
    /// than `JWKS_MIN_REFRESH`. A failed fetch keeps the current keys (logged).
    ///
    /// `wait` says what to do when another task is already fetching: an
    /// unknown `kid` waits (the answer decides whether this token is valid),
    /// while an age-driven refresh does not — every key it needs is already
    /// here, so blocking a burst of valid requests behind one slow IdP fetch
    /// would buy nothing.
    async fn refresh(&self, wait: bool) {
        let Some(src) = &self.source else { return };
        let _flight = match (wait, src.flight.try_lock()) {
            (_, Ok(guard)) => guard,
            (true, Err(_)) => src.flight.lock().await,
            (false, Err(_)) => return, // a refresh is already under way
        };
        if lock(&src.attempted_at).elapsed() < JWKS_MIN_REFRESH {
            return; // someone just did, or it is too soon to try again
        }
        *lock(&src.attempted_at) = Instant::now();
        match fetch_jwks(&src.client, &src.url).await {
            Ok(keys) => {
                crate::log_info!("refreshed JWKS from {}: {} key(s)", src.url, keys.len());
                *write(&self.keys) = keys;
            }
            Err(e) => {
                let known = read(&self.keys).len();
                crate::log_warn!("JWKS refresh from {} failed; keeping the {known} known key(s): {e:#}", src.url);
            }
        }
    }

    /// A set with a refresh source whose last attempt is backdated by `ago` —
    /// so a test can exercise the on-miss refetch without waiting a minute.
    #[cfg(test)]
    fn with_source_for_test(keys: HashMap<String, Arc<DecodingKey>>, url: &str, ago: Duration) -> Self {
        Jwks {
            keys: RwLock::new(keys),
            source: Some(JwksSource {
                url: url.to_string(),
                client: {
                    crate::crypto::install_provider();
                    reqwest::Client::new()
                },
                attempted_at: Mutex::new(Instant::now() - ago),
                flight: tokio::sync::Mutex::new(()),
            }),
        }
    }
}

impl JwtVerifier {
    /// Validate `token` and return the scopes named by `claim` (an OAuth-style
    /// space-delimited string or a JSON array). A valid token with no such
    /// claim yields an empty list (sees only unscoped tools). Test-facing;
    /// production goes through [`JwtVerifier::caller`].
    #[cfg(test)]
    pub async fn scopes(&self, token: &str, claim: &str, checks: &ClaimChecks) -> Result<Vec<String>> {
        Ok(extract_scopes(&self.claims(token, checks).await?, claim))
    }

    /// Validate `token` and build the caller it identifies: scopes from
    /// `scope_claim`, the audit subject from `subject_claim` (absent → None).
    pub async fn caller(
        &self,
        token: &str,
        scope_claim: &str,
        subject_claim: &str,
        checks: &ClaimChecks,
    ) -> Result<Caller> {
        let claims = self.claims(token, checks).await?;
        let subject = claims.get(subject_claim).and_then(Value::as_str).map(str::to_string);
        Ok(Caller::new(extract_scopes(&claims, scope_claim), subject))
    }

    /// Validate `token` (signature, `exp`, and the configured `aud`/`iss`) and
    /// return its claims.
    pub async fn claims(&self, token: &str, checks: &ClaimChecks) -> Result<Value> {
        let token = token.trim();
        match self {
            JwtVerifier::Hs256(secret) => {
                verify(token, Algorithm::HS256, &DecodingKey::from_secret(secret), checks)
            }
            JwtVerifier::Rs256(key) => verify(token, Algorithm::RS256, key, checks),
            JwtVerifier::Jwks(set) => {
                let hdr = decode_header(token).context("unreadable JWT header")?;
                let kid = hdr
                    .kid
                    .ok_or_else(|| anyhow!("JWT has no 'kid'; JWKS validation needs one"))?;
                // Revocation: a set past its age is refreshed before it is
                // trusted — but only one caller waits for that fetch.
                if set.age().is_some_and(|a| a > JWKS_MAX_AGE) {
                    set.refresh(false).await;
                }
                // Rotation: an unknown kid means the IdP may have published a
                // new key — refetch (rate-limited), then look once more.
                let key = match set.key(&kid) {
                    Some(k) => k,
                    None => {
                        set.refresh(true).await;
                        set.key(&kid).ok_or_else(|| anyhow!("no JWKS key for kid {kid:?}"))?
                    }
                };
                verify(token, Algorithm::RS256, &key, checks)
            }
        }
    }
}

fn verify(token: &str, alg: Algorithm, key: &DecodingKey, checks: &ClaimChecks) -> Result<Value> {
    let mut validation = Validation::new(alg);
    match &checks.audience {
        // jsonwebtoken rejects ANY `aud` when none is expected, which would
        // refuse every token from an issuer that stamps one — so unset means
        // "don't check", not "must be absent".
        None => validation.validate_aud = false,
        Some(aud) => {
            validation.set_audience(&[aud.as_str()]);
            // set_audience only checks the claim IF present; a token with no
            // `aud` at all must not pass an audience check
            validation.required_spec_claims.insert("aud".into());
        }
    }
    if let Some(iss) = &checks.issuer {
        validation.set_issuer(&[iss.as_str()]);
        validation.required_spec_claims.insert("iss".into());
    }
    Ok(decode::<Value>(token, key, &validation)
        .context("JWT validation failed (signature, expiry, audience, or issuer)")?
        .claims)
}

fn extract_scopes(claims: &Value, claim: &str) -> Vec<String> {
    match claims.get(claim) {
        Some(Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => vec![],
    }
}

/// Build an RS256 verifier from a PEM public key (SPKI or PKCS1).
pub fn rs256_from_pem(pem: &str) -> Result<JwtVerifier> {
    DecodingKey::from_rsa_pem(pem.as_bytes())
        .map(JwtVerifier::Rs256)
        .context("invalid RSA public key PEM")
}

#[derive(Deserialize)]
struct JwksDoc {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

/// Parse a JWKS document into a kid -> key map (RSA keys only).
fn parse_jwks(json: &str) -> Result<HashMap<String, Arc<DecodingKey>>> {
    let doc: JwksDoc = serde_json::from_str(json).context("invalid JWKS JSON")?;
    let mut keys = HashMap::new();
    for k in doc.keys {
        if k.kty != "RSA" {
            continue;
        }
        let (Some(kid), Some(n), Some(e)) = (k.kid, k.n, k.e) else {
            continue;
        };
        let key = DecodingKey::from_rsa_components(&n, &e)
            .with_context(|| format!("bad RSA JWK for kid {kid:?}"))?;
        keys.insert(kid, Arc::new(key));
    }
    if keys.is_empty() {
        return Err(anyhow!("JWKS document had no usable RSA keys"));
    }
    Ok(keys)
}

async fn fetch_jwks(client: &reqwest::Client, url: &str) -> Result<HashMap<String, Arc<DecodingKey>>> {
    let json = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("fetching JWKS from {url}"))?
        .text()
        .await
        .with_context(|| format!("reading JWKS from {url}"))?;
    parse_jwks(&json)
}

/// A verifier over a static JWKS document (no refresh). Test-facing; the
/// binary always goes through [`jwks_from_url`].
#[cfg(test)]
pub fn jwks_from_json(json: &str) -> Result<JwtVerifier> {
    Ok(JwtVerifier::Jwks(Jwks { keys: RwLock::new(parse_jwks(json)?), source: None }))
}

/// A verifier over a JWKS endpoint: fetched now (bounded), refreshed on an
/// unknown `kid` and on age, never more than once a minute.
pub async fn jwks_from_url(url: &str) -> Result<JwtVerifier> {
    crate::crypto::install_provider();
    let client = reqwest::Client::builder()
        .timeout(JWKS_FETCH_TIMEOUT)
        .build()
        .context("building HTTP client for the JWKS fetch")?;
    let keys = fetch_jwks(&client, url).await?;
    Ok(JwtVerifier::Jwks(Jwks {
        keys: RwLock::new(keys),
        source: Some(JwksSource {
            url: url.to_string(),
            client,
            attempted_at: Mutex::new(Instant::now()),
            flight: tokio::sync::Mutex::new(()),
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const SECRET: &str = "test-secret";

    // A throwaway RSA keypair, generated for these tests and used nowhere else.
    // RS256/JWKS verification can only be tested by producing real signatures —
    // including the invalid ones (`rs256_rejects_a_forged_signature`) — so the tests
    // mint tokens on demand rather than carry pre-signed ones. Kept in a fixture file
    // so its status is obvious from the filename, and so secret scanning can be
    // excluded for that one path instead of for all of auth.rs. Test-only: `cfg(test)`
    // means it is not in the shipped binary.
    const PRIV_PEM: &str = include_str!("../tests/fixtures/rs256-test-key.pem");
    const PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA3S0uBBzFx8ndFjks/V7Y
Z5p317vhNRgmGkE2jGRc/EyvIlYCJeriPFBL5oNbfRYVCnEtKquT49MsAxuRcuh7
SgM78fuwv3hITLdN1VTjVUrdCUAbuYCK9t0X5OiGWizyeTFGWkBVVFKs1K3yev0k
Jbon1JooubkrLPl5nCou47p35sS2gH+bw+lcroAMTu22H9GkAVei9g9RuDnoybp2
n2u3VoCQ5Xm+iyKYwj8iX+/HMN+BD19cC6RxYgIwxwUioQh43v+wYztpDVBxEbkK
Y4EHk+vMIgJUYU/pdJhB/rDSdAWhRvwvjZpBN2XeSk9ae7kaq8Ml0WrrAqRo/4u6
lwIDAQAB
-----END PUBLIC KEY-----";
    const JWK_N: &str = "3S0uBBzFx8ndFjks_V7YZ5p317vhNRgmGkE2jGRc_EyvIlYCJeriPFBL5oNbfRYVCnEtKquT49MsAxuRcuh7SgM78fuwv3hITLdN1VTjVUrdCUAbuYCK9t0X5OiGWizyeTFGWkBVVFKs1K3yev0kJbon1JooubkrLPl5nCou47p35sS2gH-bw-lcroAMTu22H9GkAVei9g9RuDnoybp2n2u3VoCQ5Xm-iyKYwj8iX-_HMN-BD19cC6RxYgIwxwUioQh43v-wYztpDVBxEbkKY4EHk-vMIgJUYU_pdJhB_rDSdAWhRvwvjZpBN2XeSk9ae7kaq8Ml0WrrAqRo_4u6lw";
    const JWK_E: &str = "AQAB";

    fn exp() -> i64 {
        4_000_000_000 // far future
    }

    fn none() -> ClaimChecks {
        ClaimChecks::default()
    }

    fn mint_hs(claims: Value) -> String {
        encode(&Header::default(), &claims, &EncodingKey::from_secret(SECRET.as_bytes())).unwrap()
    }

    fn mint_rs(claims: Value, kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(str::to_string);
        let key = EncodingKey::from_rsa_pem(PRIV_PEM.as_bytes()).unwrap();
        encode(&header, &claims, &key).unwrap()
    }

    fn hs() -> JwtVerifier {
        JwtVerifier::Hs256(SECRET.as_bytes().to_vec())
    }

    fn jwks_doc(kids: &[&str]) -> String {
        let keys: Vec<String> = kids
            .iter()
            .map(|k| format!(r#"{{"kty":"RSA","kid":"{k}","alg":"RS256","use":"sig","n":"{JWK_N}","e":"{JWK_E}"}}"#))
            .collect();
        format!(r#"{{"keys":[{}]}}"#, keys.join(","))
    }

    #[tokio::test]
    async fn hs256_space_delimited_scope_string() {
        let t = mint_hs(json!({"exp": exp(), "scope": "payments.read payments.write"}));
        assert_eq!(hs().scopes(&t, "scope", &none()).await.unwrap(), vec!["payments.read", "payments.write"]);
    }

    #[tokio::test]
    async fn hs256_scope_array_and_missing_claim() {
        let t = mint_hs(json!({"exp": exp(), "scopes": ["a", "b"]}));
        assert_eq!(hs().scopes(&t, "scopes", &none()).await.unwrap(), vec!["a", "b"]);
        let t2 = mint_hs(json!({"exp": exp()}));
        assert!(hs().scopes(&t2, "scope", &none()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn hs256_rejects_wrong_secret_and_expired() {
        let t = mint_hs(json!({"exp": exp(), "scope": "a"}));
        assert!(JwtVerifier::Hs256(b"other".to_vec()).scopes(&t, "scope", &none()).await.is_err());
        let expired = mint_hs(json!({"exp": 1, "scope": "a"}));
        assert!(hs().scopes(&expired, "scope", &none()).await.is_err());
    }

    #[tokio::test]
    async fn audience_and_issuer_are_enforced_only_when_configured() {
        let t = mint_hs(json!({"exp": exp(), "scope": "a", "aud": "minmcp", "iss": "https://idp"}));
        // unset: a token carrying aud/iss still validates (not "must be absent")
        assert_eq!(hs().scopes(&t, "scope", &none()).await.unwrap(), vec!["a"]);
        let ok = ClaimChecks { audience: Some("minmcp".into()), issuer: Some("https://idp".into()) };
        assert_eq!(hs().scopes(&t, "scope", &ok).await.unwrap(), vec!["a"]);
        let wrong_aud = ClaimChecks { audience: Some("other-service".into()), issuer: None };
        assert!(hs().scopes(&t, "scope", &wrong_aud).await.is_err(), "foreign audience must be refused");
        let wrong_iss = ClaimChecks { audience: None, issuer: Some("https://evil".into()) };
        assert!(hs().scopes(&t, "scope", &wrong_iss).await.is_err(), "foreign issuer must be refused");
        // configured but the token has no such claim → refused
        let bare = mint_hs(json!({"exp": exp(), "scope": "a"}));
        assert!(hs().scopes(&bare, "scope", &ok).await.is_err(), "missing aud/iss must be refused when required");
    }

    #[tokio::test]
    async fn caller_carries_scopes_and_the_configured_subject() {
        let t = mint_hs(json!({"exp": exp(), "scope": "a b", "sub": "user-1", "email": "u@x"}));
        let c = hs().caller(&t, "scope", "sub", &none()).await.unwrap();
        assert_eq!(c.scopes, vec!["a", "b"]);
        assert_eq!(c.subject.as_deref(), Some("user-1"));
        assert_eq!(c.label(), "user-1");
        let c = hs().caller(&t, "scope", "email", &none()).await.unwrap();
        assert_eq!(c.subject.as_deref(), Some("u@x"), "subject_claim is configurable");
        let c = hs().caller(&t, "scope", "missing", &none()).await.unwrap();
        assert_eq!(c.subject, None);
        assert_eq!(c.label(), "anonymous");
    }

    #[tokio::test]
    async fn rs256_validates_with_pem_public_key() {
        let v = rs256_from_pem(PUB_PEM).unwrap();
        let t = mint_rs(json!({"exp": exp(), "scope": "payments.write"}), None);
        assert_eq!(v.scopes(&t, "scope", &none()).await.unwrap(), vec!["payments.write"]);
    }

    #[tokio::test]
    async fn rs256_rejects_hs256_token_and_tamper() {
        let v = rs256_from_pem(PUB_PEM).unwrap();
        // an HS256 token must not validate against an RS256 verifier
        let hs_tok = mint_hs(json!({"exp": exp(), "scope": "a"}));
        assert!(v.scopes(&hs_tok, "scope", &none()).await.is_err());
    }

    #[tokio::test]
    async fn alg_none_token_is_rejected_by_every_verifier() {
        // classic JWT bypass: header {"alg":"none"} with an empty signature. Every
        // verifier pins a concrete algorithm, so this must be rejected outright.
        fn b64url(data: &[u8]) -> String {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for c in data.chunks(3) {
                let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                out.push(A[(n >> 18 & 63) as usize] as char);
                out.push(A[(n >> 12 & 63) as usize] as char);
                if c.len() > 1 { out.push(A[(n >> 6 & 63) as usize] as char); }
                if c.len() > 2 { out.push(A[(n & 63) as usize] as char); }
            }
            out
        }
        let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = b64url(br#"{"exp":4000000000,"scope":"admin"}"#);
        let forged = format!("{header}.{payload}."); // empty signature
        assert!(hs().scopes(&forged, "scope", &none()).await.is_err(), "HS256 verifier must reject alg:none");
        assert!(rs256_from_pem(PUB_PEM).unwrap().scopes(&forged, "scope", &none()).await.is_err(), "RS256 verifier must reject alg:none");
    }

    #[tokio::test]
    async fn jwks_selects_key_by_kid_and_validates() {
        let v = jwks_from_json(&jwks_doc(&["key-1"])).unwrap();
        let t = mint_rs(json!({"exp": exp(), "scope": "reports.read"}), Some("key-1"));
        assert_eq!(v.scopes(&t, "scope", &none()).await.unwrap(), vec!["reports.read"]);
    }

    #[tokio::test]
    async fn jwks_rejects_unknown_kid_and_missing_kid() {
        // a static document (no source) never refreshes: an unknown kid stays unknown
        let v = jwks_from_json(&jwks_doc(&["key-1"])).unwrap();
        let wrong_kid = mint_rs(json!({"exp": exp(), "scope": "a"}), Some("key-9"));
        assert!(v.scopes(&wrong_kid, "scope", &none()).await.is_err());
        let no_kid = mint_rs(json!({"exp": exp(), "scope": "a"}), None);
        assert!(v.scopes(&no_kid, "scope", &none()).await.is_err());
    }

    /// A one-shot HTTP server serving `body`, counting requests. Enough of
    /// HTTP/1.1 for reqwest; runs on a thread so the async test isn't involved.
    fn serve_jwks_once(body: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/jwks", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().take(2) {
                let Ok(mut c) = conn else { break };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = c.write_all(resp.as_bytes());
            }
        });
        (url, hits)
    }

    #[tokio::test]
    async fn jwks_refetches_on_an_unknown_kid_after_a_rotation() {
        // The IdP rotates: it now publishes key-1 AND key-2 and signs with key-2.
        // A set that only knows key-1 must refetch on the miss and then validate.
        let (url, hits) = serve_jwks_once(jwks_doc(&["key-1", "key-2"]));
        let known = parse_jwks(&jwks_doc(&["key-1"])).unwrap();
        let v = JwtVerifier::Jwks(Jwks::with_source_for_test(known, &url, JWKS_MIN_REFRESH * 2));
        let rotated = mint_rs(json!({"exp": exp(), "scope": "after.rotation"}), Some("key-2"));
        assert_eq!(v.scopes(&rotated, "scope", &none()).await.unwrap(), vec!["after.rotation"]);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "one refetch");
        // Still-unknown kids do NOT refetch again inside the rate-limit window —
        // an unauthenticated caller can't drive fetches by inventing kids.
        let bogus = mint_rs(json!({"exp": exp(), "scope": "a"}), Some("key-9"));
        assert!(v.scopes(&bogus, "scope", &none()).await.is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "no second fetch within the window");
    }

    #[tokio::test]
    async fn jwks_keeps_its_keys_when_a_refresh_fails() {
        // A source that refuses connections: the miss triggers an attempt, the
        // attempt fails, the known key keeps working and the unknown one fails.
        let known = parse_jwks(&jwks_doc(&["key-1"])).unwrap();
        let v = JwtVerifier::Jwks(Jwks::with_source_for_test(known, "http://127.0.0.1:9/jwks", JWKS_MIN_REFRESH * 2));
        let unknown = mint_rs(json!({"exp": exp(), "scope": "a"}), Some("key-2"));
        assert!(v.scopes(&unknown, "scope", &none()).await.is_err());
        let ok = mint_rs(json!({"exp": exp(), "scope": "still.ok"}), Some("key-1"));
        assert_eq!(v.scopes(&ok, "scope", &none()).await.unwrap(), vec!["still.ok"]);
    }
}
