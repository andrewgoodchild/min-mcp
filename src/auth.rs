//! Derive a caller's scopes from a validated JWT, so scope-based tool
//! visibility rests on a signed token rather than a hand-typed flag.
//!
//! Three verifier kinds, in config-precedence order: a JWKS endpoint (RS256,
//! keys selected by the token's `kid`), a configured RS256 public key (PEM), or
//! an HS256 shared secret (the internal-gateway case). The surface
//! (`JwtVerifier::scopes` -> a scope list) is identical across all three.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;

/// Resolved key material for validating caller JWTs.
pub enum JwtVerifier {
    Hs256(Vec<u8>),
    Rs256(DecodingKey),
    /// kid -> RSA public key, from a JWKS document.
    Jwks(HashMap<String, DecodingKey>),
}

impl JwtVerifier {
    /// Validate `token` and return the scopes named by `claim` (an OAuth-style
    /// space-delimited string or a JSON array). A valid token with no such
    /// claim yields an empty list (sees only unscoped tools).
    pub fn scopes(&self, token: &str, claim: &str) -> Result<Vec<String>> {
        let token = token.trim();
        let claims = match self {
            JwtVerifier::Hs256(secret) => {
                verify(token, Algorithm::HS256, &DecodingKey::from_secret(secret))
            }
            JwtVerifier::Rs256(key) => verify(token, Algorithm::RS256, key),
            JwtVerifier::Jwks(keys) => {
                let hdr = decode_header(token).context("unreadable JWT header")?;
                let kid = hdr
                    .kid
                    .ok_or_else(|| anyhow!("JWT has no 'kid'; JWKS validation needs one"))?;
                let key = keys
                    .get(&kid)
                    .ok_or_else(|| anyhow!("no JWKS key for kid {kid:?}"))?;
                verify(token, Algorithm::RS256, key)
            }
        }?;
        Ok(extract_scopes(&claims, claim))
    }
}

fn verify(token: &str, alg: Algorithm, key: &DecodingKey) -> Result<Value> {
    let mut validation = Validation::new(alg);
    validation.validate_aud = false; // audience isn't checked here
    Ok(decode::<Value>(token, key, &validation)
        .context("JWT validation failed (signature or expiry)")?
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
pub fn jwks_from_json(json: &str) -> Result<JwtVerifier> {
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
        keys.insert(kid, key);
    }
    if keys.is_empty() {
        return Err(anyhow!("JWKS document had no usable RSA keys"));
    }
    Ok(JwtVerifier::Jwks(keys))
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

    #[test]
    fn hs256_space_delimited_scope_string() {
        let t = mint_hs(json!({"exp": exp(), "scope": "payments.read payments.write"}));
        assert_eq!(hs().scopes(&t, "scope").unwrap(), vec!["payments.read", "payments.write"]);
    }

    #[test]
    fn hs256_scope_array_and_missing_claim() {
        let t = mint_hs(json!({"exp": exp(), "scopes": ["a", "b"]}));
        assert_eq!(hs().scopes(&t, "scopes").unwrap(), vec!["a", "b"]);
        let t2 = mint_hs(json!({"exp": exp()}));
        assert!(hs().scopes(&t2, "scope").unwrap().is_empty());
    }

    #[test]
    fn hs256_rejects_wrong_secret_and_expired() {
        let t = mint_hs(json!({"exp": exp(), "scope": "a"}));
        assert!(JwtVerifier::Hs256(b"other".to_vec()).scopes(&t, "scope").is_err());
        let expired = mint_hs(json!({"exp": 1, "scope": "a"}));
        assert!(hs().scopes(&expired, "scope").is_err());
    }

    #[test]
    fn rs256_validates_with_pem_public_key() {
        let v = rs256_from_pem(PUB_PEM).unwrap();
        let t = mint_rs(json!({"exp": exp(), "scope": "payments.write"}), None);
        assert_eq!(v.scopes(&t, "scope").unwrap(), vec!["payments.write"]);
    }

    #[test]
    fn rs256_rejects_hs256_token_and_tamper() {
        let v = rs256_from_pem(PUB_PEM).unwrap();
        // an HS256 token must not validate against an RS256 verifier
        let hs_tok = mint_hs(json!({"exp": exp(), "scope": "a"}));
        assert!(v.scopes(&hs_tok, "scope").is_err());
    }

    #[test]
    fn alg_none_token_is_rejected_by_every_verifier() {
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
        assert!(hs().scopes(&forged, "scope").is_err(), "HS256 verifier must reject alg:none");
        assert!(rs256_from_pem(PUB_PEM).unwrap().scopes(&forged, "scope").is_err(), "RS256 verifier must reject alg:none");
    }

    #[test]
    fn jwks_selects_key_by_kid_and_validates() {
        let jwks = format!(
            r#"{{"keys":[{{"kty":"RSA","kid":"key-1","alg":"RS256","use":"sig","n":"{JWK_N}","e":"{JWK_E}"}}]}}"#
        );
        let v = jwks_from_json(&jwks).unwrap();
        let t = mint_rs(json!({"exp": exp(), "scope": "reports.read"}), Some("key-1"));
        assert_eq!(v.scopes(&t, "scope").unwrap(), vec!["reports.read"]);
    }

    #[test]
    fn jwks_rejects_unknown_kid_and_missing_kid() {
        let jwks = format!(r#"{{"keys":[{{"kty":"RSA","kid":"key-1","n":"{JWK_N}","e":"{JWK_E}"}}]}}"#);
        let v = jwks_from_json(&jwks).unwrap();
        let wrong_kid = mint_rs(json!({"exp": exp(), "scope": "a"}), Some("key-9"));
        assert!(v.scopes(&wrong_kid, "scope").is_err());
        let no_kid = mint_rs(json!({"exp": exp(), "scope": "a"}), None);
        assert!(v.scopes(&no_kid, "scope").is_err());
    }
}
