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

/// Install the process-wide rustls crypto provider that `vaultrs` needs.
///
/// vaultrs is built with `rustls-no-provider` so it shares the `ring` provider
/// already in the tree rather than dragging in aws-lc (a C build on every
/// platform). The catch: "no provider" means it takes the PROCESS DEFAULT, and
/// reqwest **panics** while building its client when no default is installed.
/// Nothing installed one, so every `${vault:…}` reference aborted the process
/// instead of resolving — the feature could not work at all.
///
/// Called on the Vault path only, so a deployment that never references Vault
/// pays nothing. `install_default` errors if a provider is already installed,
/// which is not a problem here: any provider will do, and it must not be racy,
/// hence the `Once`.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Vault {
    async fn connect(cfg: &VaultConfig) -> Result<Self> {
        install_crypto_provider();
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

    // --- Vault, against a mock of its HTTP API ------------------------------
    //
    // `${vault:…}` resolution, the AppRole/Kubernetes logins, and the re-login
    // on 403 are the credential path — the code most worth getting right and,
    // until now, the least covered, because it needs a Vault to talk to.
    // vaultrs speaks plain JSON over HTTP, so an in-process server is enough.

    use crate::testserver::{self, Received, Reply};

    /// Vault wraps every reply in the same envelope, and vaultrs requires the
    /// whole of it — `request_id`, `lease_id`, `lease_duration` and `renewable`
    /// are not optional in its `EndpointResult`, so a trimmed mock fails to
    /// deserialise rather than failing the assertion under test.
    fn envelope(data: &str, auth: &str) -> String {
        format!(
            r#"{{"request_id":"req-1","lease_id":"","renewable":false,"lease_duration":0,
                 "data":{data},"wrap_info":null,"warnings":null,"auth":{auth}}}"#
        )
    }

    /// KV v2 read response for `{field: value}` pairs.
    fn kv2(fields: &[(&str, &str)]) -> String {
        let inner: Vec<String> = fields.iter().map(|(k, v)| format!(r#""{k}":"{v}""#)).collect();
        // The metadata block is required in full: vaultrs' SecretVersionMetadata
        // has no optional fields beyond custom_metadata.
        let data = format!(
            r#"{{"data":{{{}}},"metadata":{{"created_time":"2026-01-01T00:00:00Z",
                 "deletion_time":"","custom_metadata":null,"destroyed":false,"version":1}}}}"#,
            inner.join(",")
        );
        envelope(&data, "null")
    }

    /// A successful Vault login response.
    fn login_ok(token: &str) -> String {
        let auth = format!(
            r#"{{"client_token":"{token}","accessor":"acc","policies":["default"],
                 "token_policies":["default"],"metadata":{{}},"lease_duration":3600,
                 "renewable":true,"entity_id":"","token_type":"service","orphan":false}}"#
        );
        envelope("null", &auth)
    }

    fn vault_cfg(addr: String, auth: crate::config::VaultAuth) -> SecretsConfig {
        SecretsConfig {
            vault: Some(crate::config::VaultConfig {
                address: Some(addr),
                namespace: None,
                mount: "secret".into(),
                ca_cert: None,
                auth,
            }),
            cache_ttl_s: 300,
        }
    }

    /// Auth by a token taken from a uniquely-named env var, so parallel tests
    /// cannot race each other through the process environment.
    fn token_auth(tag: &str) -> (crate::config::VaultAuth, String) {
        let var = format!("MINMCP_TEST_VAULT_TOKEN_{tag}");
        std::env::set_var(&var, "root-token");
        (
            crate::config::VaultAuth { token_env: Some(var.clone()), approle: None, kubernetes: None },
            var,
        )
    }

    #[tokio::test]
    async fn a_vault_reference_reads_the_named_field() {
        let srv = testserver::spawn(|_, _| Reply::json(kv2(&[("api_key", "sk-live-1"), ("other", "x")]))).await;
        let (auth, var) = token_auth("read");
        let s = Secrets::from_config(&vault_cfg(srv.url(), auth));

        assert_eq!(s.expand("${vault:acme/prod#api_key}").await.unwrap(), "sk-live-1");
        // The path is the KV v2 data path under the configured mount.
        let read = srv.seen().into_iter().find(|r| r.method == "GET").expect("a KV read");
        // (vaultrs appends an empty query string, hence the trim.)
        assert_eq!(
            read.path.trim_end_matches('?'),
            "/v1/secret/data/acme/prod",
            "unexpected Vault path: {}",
            read.path
        );
        assert_eq!(read.header("x-vault-token"), Some("root-token"), "the token must be sent");
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn a_missing_field_names_the_fields_that_do_exist() {
        // The failure an operator actually hits: right secret, wrong key. The
        // error has to say what IS there, or debugging is guesswork.
        let srv = testserver::spawn(|_, _| Reply::json(kv2(&[("username", "u"), ("password", "p")]))).await;
        let (auth, var) = token_auth("missing");
        let s = Secrets::from_config(&vault_cfg(srv.url(), auth));
        let err = format!("{:#}", s.expand("${vault:acme/prod#api_key}").await.unwrap_err());
        assert!(err.contains("api_key"), "should name the field asked for: {err}");
        assert!(err.contains("username") && err.contains("password"), "should list what exists: {err}");
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn a_read_is_cached_for_the_ttl() {
        // A `user_supplied` field resolves per call; without the cache that is
        // one Vault round-trip per tool call.
        let srv = testserver::spawn(|_, _| Reply::json(kv2(&[("k", "v")]))).await;
        let (auth, var) = token_auth("cache");
        let s = Secrets::from_config(&vault_cfg(srv.url(), auth));
        for _ in 0..3 {
            assert_eq!(s.expand("${vault:a/b#k}").await.unwrap(), "v");
        }
        let reads = srv.seen().iter().filter(|r| r.method == "GET").count();
        assert_eq!(reads, 1, "three resolutions of one reference should read Vault once");
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn approle_logs_in_then_reads() {
        let srv = testserver::spawn(|_, got: &Received| {
            if got.path.contains("/auth/approle/login") {
                return Reply::json(login_ok("approle-token"));
            }
            Reply::json(kv2(&[("k", "from-approle")]))
        })
        .await;
        let auth = crate::config::VaultAuth {
            token_env: None,
            approle: Some(crate::config::VaultAppRole {
                mount: "approle".into(),
                role_id: "role-1".into(),
                secret_id: "secret-1".into(),
            }),
            kubernetes: None,
        };
        let s = Secrets::from_config(&vault_cfg(srv.url(), auth));
        assert_eq!(s.expand("${vault:a/b#k}").await.unwrap(), "from-approle");

        let seen = srv.seen();
        let login = seen.iter().find(|r| r.path.contains("login")).expect("a login");
        assert!(login.body.contains("role-1") && login.body.contains("secret-1"), "credentials: {}", login.body);
        // The token the login returned is what the read carries.
        let read = seen.iter().find(|r| r.method == "GET").expect("a read");
        assert_eq!(read.header("x-vault-token"), Some("approle-token"));
    }

    #[tokio::test]
    async fn kubernetes_login_sends_the_service_account_jwt() {
        let dir = std::env::temp_dir();
        let jwt = dir.join(format!("minmcp_sa_jwt_{}", std::process::id()));
        std::fs::write(&jwt, "  eyJhbGciOi.fake.jwt  \n").unwrap();

        let srv = testserver::spawn(|_, got: &Received| {
            if got.path.contains("/auth/kubernetes/login") {
                return Reply::json(login_ok("k8s-token"));
            }
            Reply::json(kv2(&[("k", "from-k8s")]))
        })
        .await;
        let auth = crate::config::VaultAuth {
            token_env: None,
            approle: None,
            kubernetes: Some(crate::config::VaultKubernetes {
                mount: "kubernetes".into(),
                role: "minmcp".into(),
                jwt_path: jwt.to_string_lossy().into_owned(),
            }),
        };
        let s = Secrets::from_config(&vault_cfg(srv.url(), auth));
        assert_eq!(s.expand("${vault:a/b#k}").await.unwrap(), "from-k8s");

        let seen = srv.seen();
        let login = seen.iter().find(|r| r.path.contains("login")).expect("a login");
        assert!(login.body.contains("eyJhbGciOi.fake.jwt"), "the JWT must be sent: {}", login.body);
        assert!(!login.body.contains("  eyJ"), "and trimmed, or Vault rejects it");
        let _ = std::fs::remove_file(&jwt);
    }

    #[tokio::test]
    async fn an_expired_token_is_renewed_once_and_the_read_retried() {
        // A long-lived proxy outlives its Vault lease. Without this, every
        // secret resolution fails from the moment the token expires until
        // someone restarts the process.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = std::sync::Arc::new(AtomicUsize::new(0));
        let r2 = reads.clone();
        let srv = testserver::spawn(move |_, got: &Received| {
            if got.path.contains("/auth/approle/login") {
                return Reply::json(login_ok("fresh-token"));
            }
            // The first read is rejected as if the token had expired.
            if r2.fetch_add(1, Ordering::SeqCst) == 0 {
                return Reply { status: 403, ..Reply::json(r#"{"errors":["permission denied"]}"#) };
            }
            Reply::json(kv2(&[("k", "after-relogin")]))
        })
        .await;
        let auth = crate::config::VaultAuth {
            token_env: None,
            approle: Some(crate::config::VaultAppRole {
                mount: "approle".into(),
                role_id: "r".into(),
                secret_id: "s".into(),
            }),
            kubernetes: None,
        };
        let s = Secrets::from_config(&vault_cfg(srv.url(), auth));
        assert_eq!(s.expand("${vault:a/b#k}").await.unwrap(), "after-relogin", "a 403 should recover");

        // Exactly two logins: the one at connect, and the one after the 403.
        let logins = srv.seen().iter().filter(|r| r.path.contains("login")).count();
        assert_eq!(logins, 2, "one re-login, not a loop");
    }

    #[tokio::test]
    async fn a_403_without_a_login_configured_is_reported_not_retried() {
        // Token auth has nothing to re-login WITH, so the 403 must surface.
        let srv = testserver::spawn(|_, _| Reply { status: 403, ..Reply::json(r#"{"errors":["denied"]}"#) }).await;
        let (auth, var) = token_auth("no_relogin");
        let s = Secrets::from_config(&vault_cfg(srv.url(), auth));
        let err = format!("{:#}", s.expand("${vault:a/b#k}").await.unwrap_err());
        assert!(err.contains("secret/a/b"), "should name the secret it failed to read: {err}");
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn vault_misconfiguration_fails_with_a_pointed_message() {
        // A malformed address: the vaultrs builder panics on this, so it is
        // validated first — a panic here would take the process down at startup.
        let cfg = vault_cfg("not a url".into(), crate::config::VaultAuth::default());
        let err = format!("{:#}", Secrets::from_config(&cfg).expand("${vault:a/b#k}").await.unwrap_err());
        assert!(err.contains("address"), "should name the setting: {err}");

        // No token and no login configured at all.
        let srv = testserver::spawn(|_, _| Reply::json(kv2(&[("k", "v")]))).await;
        let cfg = vault_cfg(srv.url(), crate::config::VaultAuth::default());
        let err = format!("{:#}", Secrets::from_config(&cfg).expand("${vault:a/b#k}").await.unwrap_err());
        assert!(err.contains("token") || err.contains("approle"), "should say how to authenticate: {err}");

        // A reference missing its `#field`.
        let (auth, var) = token_auth("shape");
        let s = Secrets::from_config(&vault_cfg("http://127.0.0.1:1".into(), auth));
        let err = format!("{:#}", s.expand("${vault:no-hash}").await.unwrap_err());
        assert!(err.contains("path#field"), "should teach the shape: {err}");
        std::env::remove_var(var);
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
