//! Secret references, resolved at startup (config templates) and at call time
//! (`user_supplied` fields) from pluggable sources:
//!
//! ```text
//! ${NAME} / ${env:NAME}            the process environment (the original form)
//! ${file:/run/secrets/api-key}     a file — Kubernetes and Docker secret mounts
//! ${vault:path/to/secret#field}    HashiCorp Vault / OpenBao KV v2, via vaultrs
//! ```
//!
//! Resolved values are `SecretString`s: no Debug/Display leak, zeroed on drop,
//! and every use site says `expose_secret()` out loud. Vault reads are cached
//! for `secrets.cache_ttl_s`, so a `user_supplied` field costs one Vault
//! round-trip per TTL, not one per call. `env` and `file` are re-read every
//! time — they are local, and re-reading a mounted secret file is how rotation
//! reaches a running process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use vaultrs::client::{Client, VaultClient, VaultClientSettingsBuilder};

use crate::config::{SecretsConfig, VaultConfig};
use crate::sync::lock;

pub struct Secrets {
    /// Connected on the first `${vault:…}` reference, not at startup: a config
    /// may carry a `secrets.vault` block whose references are never reached by
    /// the command being run (`inspect`, `map`, `lint`), and those must not
    /// require Vault to be up.
    vault_cfg: Option<VaultConfig>,
    vault: tokio::sync::OnceCell<Vault>,
    cache: Mutex<HashMap<String, (Instant, SecretString)>>,
    ttl: Duration,
}

struct Vault {
    /// Behind a mutex so a re-login (which swaps the token) is single-flight.
    client: tokio::sync::Mutex<VaultClient>,
    mount: String,
    /// How to log in again when the token is rejected; None for a static token.
    login: Option<VaultLogin>,
}

enum VaultLogin {
    AppRole { mount: String, role_id: String, secret_id: SecretString },
    Kubernetes { mount: String, role: String, jwt_path: String },
}

/// One piece of a template: literal text or the inside of a `${…}`.
enum Piece<'a> {
    Lit(&'a str),
    Ref(&'a str),
}

fn pieces(template: &str) -> Result<Vec<Piece<'_>>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            out.push(Piece::Lit(&rest[..start]));
        }
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| anyhow!("unterminated ${{...}} in {template:?}"))?;
        out.push(Piece::Ref(&after[..end]));
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        out.push(Piece::Lit(rest));
    }
    Ok(out)
}

/// Split `scheme:arg`; a bare name is `env`. (Windows paths keep their drive
/// colon because only the FIRST colon splits.)
fn scheme_and_arg(reference: &str) -> (&str, &str) {
    match reference.split_once(':') {
        Some((s, a)) => (s, a),
        None => ("env", reference),
    }
}

fn env_ref(reference: &str, name: &str) -> Result<SecretString> {
    std::env::var(name)
        .map(SecretString::from)
        .map_err(|_| anyhow!("env var {name} referenced as ${{{reference}}} in the config is not set"))
}

fn file_ref(reference: &str, path: &str) -> Result<SecretString> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading secret file {path} (${{{reference}}})"))?;
    // mounted secrets often end in a newline that is not part of the value
    Ok(SecretString::from(text.trim_end_matches(['\n', '\r']).to_string()))
}

/// Env-only expansion of `${NAME}` / `${env:NAME}` — synchronous, for the few
/// places that run before any store is configured (Vault's own AppRole
/// credentials, tests). Any other scheme is an error naming the resolver.
pub fn expand_env(template: &str) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    for p in pieces(template)? {
        match p {
            Piece::Lit(s) => out.push_str(s),
            Piece::Ref(r) => match scheme_and_arg(r) {
                ("env", name) => out.push_str(env_ref(r, name)?.expose_secret()),
                (other, _) => bail!(
                    "${{{r}}}: the {other} secret scheme is not available here (only env is)"
                ),
            },
        }
    }
    Ok(out)
}

impl Secrets {
    /// The environment only — for tests that configure no store.
    #[cfg(test)]
    pub fn env_only() -> Self {
        Secrets::from_config(&SecretsConfig::default())
    }

    /// Declare the configured stores. Vault is contacted lazily (see
    /// [`Secrets::vault`]), so this cannot fail and costs nothing.
    pub fn from_config(cfg: &SecretsConfig) -> Self {
        Secrets {
            vault_cfg: cfg.vault.clone(),
            vault: tokio::sync::OnceCell::new(),
            cache: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(cfg.cache_ttl_s),
        }
    }

    /// The Vault client, connecting (and logging in) on first use. The
    /// `OnceCell` makes that single-flight: concurrent first references wait
    /// for one login rather than racing several.
    async fn vault(&self, reference: &str) -> Result<&Vault> {
        let cfg = self.vault_cfg.as_ref().ok_or_else(|| {
            anyhow!("${{{reference}}} used but no `secrets.vault` is configured")
        })?;
        self.vault.get_or_try_init(|| Vault::connect(cfg)).await
    }

    /// Expand every `${…}` in `template`. The result usually IS a secret (a
    /// header value, a key) — callers store it as a `SecretString` where it
    /// lives longer than the request that uses it.
    pub async fn expand(&self, template: &str) -> Result<String> {
        let mut out = String::with_capacity(template.len());
        for p in pieces(template)? {
            match p {
                Piece::Lit(s) => out.push_str(s),
                Piece::Ref(r) => out.push_str(self.resolve_ref(r).await?.expose_secret()),
            }
        }
        Ok(out)
    }

    /// Resolve one reference — the text inside `${…}`, or a `user_supplied`
    /// source (`env:VAR`, `file:PATH`, `vault:PATH#FIELD`).
    pub async fn resolve_ref(&self, reference: &str) -> Result<SecretString> {
        match scheme_and_arg(reference) {
            ("env", name) => env_ref(reference, name),
            ("file", path) => file_ref(reference, path),
            ("vault", arg) => self.vault_ref(arg).await,
            (other, _) => bail!(
                "unknown secret scheme {other:?} in ${{{reference}}} (env, file, or vault)"
            ),
        }
    }

    /// A `user_supplied` source's value, or None (logged) when it can't be
    /// resolved or is empty — dispatch turns None into the structured
    /// `missing_user_supplied_value` error, never a fabricated success.
    pub async fn resolve_source(&self, source: &str) -> Option<String> {
        match self.resolve_ref(source).await {
            Ok(v) if !v.expose_secret().is_empty() => Some(v.expose_secret().to_string()),
            Ok(_) => {
                crate::log_warn!("user_supplied source {source:?} resolved to an empty value");
                None
            }
            Err(e) => {
                crate::log_warn!("user_supplied source {source:?} could not be resolved: {e:#}");
                None
            }
        }
    }

    async fn vault_ref(&self, arg: &str) -> Result<SecretString> {
        let (path, field) = arg
            .split_once('#')
            .ok_or_else(|| anyhow!("vault reference {arg:?} must be `path#field` (KV v2)"))?;
        let key = format!("vault:{arg}");
        if let Some(hit) = self.cached(&key) {
            return Ok(hit);
        }
        let value = self.vault(&format!("vault:{arg}")).await?.read(path, field).await?;
        lock(&self.cache).insert(key, (Instant::now(), value.clone()));
        Ok(value)
    }

    fn cached(&self, key: &str) -> Option<SecretString> {
        let cache = lock(&self.cache);
        let (at, v) = cache.get(key)?;
        (at.elapsed() < self.ttl).then(|| v.clone())
    }
}

impl Vault {
    async fn connect(cfg: &VaultConfig) -> Result<Self> {
        let mut b = VaultClientSettingsBuilder::default();
        if let Some(addr) = &cfg.address {
            // the builder's own setter panics on a malformed URL; validate first
            reqwest::Url::parse(addr).with_context(|| format!("secrets.vault.address {addr:?}"))?;
            b.address(addr);
        }
        if let Some(ns) = &cfg.namespace {
            b.set_namespace(ns.clone());
        }
        if let Some(ca) = &cfg.ca_cert {
            b.ca_certs(vec![ca.clone()]);
        }
        b.timeout(Some(Duration::from_secs(30)));
        let login = match (&cfg.auth.approle, &cfg.auth.kubernetes) {
            (Some(a), _) => Some(VaultLogin::AppRole {
                mount: a.mount.clone(),
                role_id: expand_env(&a.role_id)?,
                secret_id: SecretString::from(expand_env(&a.secret_id)?),
            }),
            (None, Some(k)) => Some(VaultLogin::Kubernetes {
                mount: k.mount.clone(),
                role: k.role.clone(),
                jwt_path: k.jwt_path.clone(),
            }),
            (None, None) => None,
        };
        if let Some(var) = &cfg.auth.token_env {
            let tok = std::env::var(var)
                .map_err(|_| anyhow!("secrets.vault.auth.token_env names {var}, which is not set"))?;
            b.token(tok);
        }
        let settings = b.build().map_err(|e| anyhow!("secrets.vault settings: {e}"))?;
        let mut client = VaultClient::new(settings).context("building the Vault client")?;
        match &login {
            Some(l) => l.apply(&mut client).await?,
            None if client.settings().token.is_empty() => bail!(
                "secrets.vault: no token (set VAULT_TOKEN or auth.token_env) and no \
                 auth.approle / auth.kubernetes login configured"
            ),
            None => {}
        }
        Ok(Vault { client: tokio::sync::Mutex::new(client), mount: cfg.mount.clone(), login })
    }

    async fn read(&self, path: &str, field: &str) -> Result<SecretString> {
        use vaultrs::error::ClientError;
        let mut client = self.client.lock().await;
        let read = vaultrs::kv2::read::<HashMap<String, Value>>(&*client, &self.mount, path).await;
        let data = match (read, &self.login) {
            (Ok(d), _) => d,
            // token expired or revoked: log in again once, then retry
            (Err(ClientError::APIError { code: 403, .. }), Some(l)) => {
                crate::log_warn!("Vault rejected the token (403); logging in again");
                l.apply(&mut client).await?;
                vaultrs::kv2::read::<HashMap<String, Value>>(&*client, &self.mount, path)
                    .await
                    .with_context(|| format!("reading Vault secret {}/{path} after re-login", self.mount))?
            }
            (Err(e), _) => {
                return Err(anyhow!(e))
                    .with_context(|| format!("reading Vault secret {}/{path}", self.mount))
            }
        };
        let v = data.get(field).ok_or_else(|| {
            let mut keys: Vec<&String> = data.keys().collect();
            keys.sort();
            anyhow!("Vault secret {}/{path} has no field {field:?} (fields: {keys:?})", self.mount)
        })?;
        let s = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        Ok(SecretString::from(s))
    }
}

impl VaultLogin {
    async fn apply(&self, client: &mut VaultClient) -> Result<()> {
        let info = match self {
            VaultLogin::AppRole { mount, role_id, secret_id } => {
                vaultrs::auth::approle::login(&*client, mount, role_id, secret_id.expose_secret())
                    .await
                    .context("Vault AppRole login")?
            }
            VaultLogin::Kubernetes { mount, role, jwt_path } => {
                let jwt = std::fs::read_to_string(jwt_path)
                    .with_context(|| format!("reading the service-account JWT at {jwt_path}"))?;
                vaultrs::auth::kubernetes::login(&*client, mount, role, jwt.trim())
                    .await
                    .context("Vault Kubernetes login")?
            }
        };
        client.set_token(&info.client_token);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_env_substitutes_and_errors_on_missing() {
        std::env::set_var("MINMCP_TEST_TOKEN", "sekret");
        assert_eq!(expand_env("Bearer ${MINMCP_TEST_TOKEN}").unwrap(), "Bearer sekret");
        assert_eq!(expand_env("Bearer ${env:MINMCP_TEST_TOKEN}").unwrap(), "Bearer sekret");
        assert_eq!(expand_env("no vars here").unwrap(), "no vars here");
        assert!(expand_env("${MINMCP_DEFINITELY_UNSET_VAR_XYZ}").is_err());
        assert!(expand_env("${unterminated").is_err());
        // other schemes need the async resolver; the sync path says so
        let e = expand_env("${vault:secret/x#k}").unwrap_err();
        assert!(e.to_string().contains("vault"), "{e}");
        std::env::remove_var("MINMCP_TEST_TOKEN");
    }

    #[tokio::test]
    async fn file_refs_read_and_trim_a_mounted_secret() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("minmcp_secret_{}", std::process::id()));
        std::fs::write(&p, "s3cr3t\n").unwrap();
        let s = Secrets::env_only();
        let out = s.expand(&format!("Bearer ${{file:{}}}", p.display())).await.unwrap();
        assert_eq!(out, "Bearer s3cr3t", "trailing newline is not part of the value");
        assert!(s.expand("${file:/nonexistent/minmcp/secret}").await.is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn vault_refs_without_a_store_and_unknown_schemes_are_errors() {
        let s = Secrets::env_only();
        let e = s.expand("${vault:secret/app#key}").await.unwrap_err();
        assert!(format!("{e:#}").contains("secrets.vault"), "{e:#}");
        // a config CARRYING a vault block never contacts it unless a reference
        // is reached — every non-vault expansion still resolves
        let cfg: crate::config::SecretsConfig = serde_yaml::from_str(
            "vault:\n  address: http://127.0.0.1:9\n  auth: {token_env: MINMCP_TEST_NO_SUCH}\n",
        )
        .unwrap();
        let lazy = Secrets::from_config(&cfg);
        std::env::set_var("MINMCP_TEST_LAZY", "ok");
        assert_eq!(lazy.expand("${MINMCP_TEST_LAZY}").await.unwrap(), "ok");
        std::env::remove_var("MINMCP_TEST_LAZY");
        let e = s.expand("${vault:no-field-separator}").await.unwrap_err();
        assert!(format!("{e:#}").contains("path#field"), "{e:#}");
        let e = s.expand("${aws:arn}").await.unwrap_err();
        assert!(format!("{e:#}").contains("unknown secret scheme"), "{e:#}");
    }

    #[tokio::test]
    async fn resolve_source_is_none_for_unset_empty_or_unknown() {
        let s = Secrets::env_only();
        assert_eq!(s.resolve_source("literal:x").await, None);
        assert_eq!(s.resolve_source("env:__minmcp_definitely_unset__").await, None);
        std::env::set_var("MINMCP_TEST_EMPTY", "");
        assert_eq!(s.resolve_source("env:MINMCP_TEST_EMPTY").await, None, "empty is missing");
        std::env::set_var("MINMCP_TEST_REGION", "eu");
        assert_eq!(s.resolve_source("env:MINMCP_TEST_REGION").await.as_deref(), Some("eu"));
        std::env::remove_var("MINMCP_TEST_EMPTY");
        std::env::remove_var("MINMCP_TEST_REGION");
    }
}
