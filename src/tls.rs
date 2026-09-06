//! TLS, and optional mutual TLS, for the HTTP listener.
//!
//! This module is wiring only — every primitive is off-the-shelf `rustls`.
//! PEM loading is `rustls::pki_types`' own `PemObject` (so no `rustls-pemfile`
//! and no hand-rolled parser), and client certificates are checked by
//! `rustls::server::WebPkiClientVerifier` (so no hand-rolled chain validation).
//!
//! The crypto provider is passed **explicitly** rather than taken from rustls'
//! process default, for two reasons: the default feature set is `aws_lc_rs`,
//! which needs a C toolchain the Windows release build does not have; and
//! reaching for the process default would couple this to whatever reqwest and
//! vaultrs have installed. `ring` is already in the tree via reqwest.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use tokio_rustls::TlsAcceptor;

use crate::config::TlsConfig;

/// A built acceptor, paired with what it actually enforces.
///
/// `requires_client_cert` is reported by the thing that installs the verifier
/// rather than re-derived from config by the caller. The bind guard treats
/// mutual TLS as an authentication boundary, so a caller reading
/// `client_ca_file.is_some()` for itself would silently over-exempt a
/// non-loopback bind the day this function grows an optional-client-auth mode.
pub struct Tls {
    pub acceptor: TlsAcceptor,
    pub requires_client_cert: bool,
}

/// Build the acceptor that wraps each accepted TCP connection.
///
/// Fails at startup rather than per connection: an unreadable key or an empty
/// chain is a deployment error, and a listener that accepts and then fails
/// every handshake is worse than one that never binds.
pub fn acceptor(cfg: &TlsConfig) -> Result<Tls> {
    let certs = load_chain(&cfg.cert_file).with_context(|| format!("TLS certificate {}", cfg.cert_file))?;
    if certs.is_empty() {
        bail!("TLS certificate {} contains no certificates", cfg.cert_file);
    }
    let key = PrivateKeyDer::from_pem_file(&cfg.key_file)
        .with_context(|| format!("TLS private key {}", cfg.key_file))?;

    // One provider instance, shared by the server config and the client
    // verifier: two constructions could drift to two different cipher-suite
    // lists the day the provider choice changes.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("selecting TLS protocol versions")?;

    let builder = match &cfg.client_ca_file {
        None => builder.with_no_client_auth(),
        Some(ca_file) => {
            let mut roots = RootCertStore::empty();
            for cert in load_chain(ca_file).with_context(|| format!("client CA bundle {ca_file}"))? {
                roots.add(cert).with_context(|| format!("adding a client CA from {ca_file}"))?;
            }
            if roots.is_empty() {
                bail!("client CA bundle {ca_file} contains no certificates");
            }
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .context("building the client-certificate verifier")?;
            builder.with_client_cert_verifier(verifier)
        }
    };

    let mut server_config =
        builder.with_single_cert(certs, key).context("the TLS certificate and key do not match")?;
    // The listener speaks HTTP/1.1 only (hyper's http1 builder drives it), so
    // advertise exactly that: a client negotiating h2 would otherwise be handed
    // a connection this server cannot speak.
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Tls {
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        requires_client_cert: cfg.client_ca_file.is_some(),
    })
}

fn load_chain(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    Ok(CertificateDer::pem_file_iter(path)?.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A throwaway self-signed cert + key, PEM, generated once by `openssl` and
    /// pinned here so the test needs no toolchain and no network.
    const CERT: &str = include_str!("../tests/fixtures/tls/server.crt");
    const KEY: &str = include_str!("../tests/fixtures/tls/server.key");
    /// A perfectly valid key that is simply not `server.crt`'s — the CA's.
    const OTHER_KEY: &str = include_str!("../tests/fixtures/tls/ca.key");

    fn write(dir: &std::path::Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p.to_str().unwrap().to_string()
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("minmcp-tls-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn builds_an_acceptor_from_a_cert_and_key() {
        let d = tmpdir();
        let cfg = TlsConfig {
            cert_file: write(&d, "one.crt", CERT),
            key_file: write(&d, "one.key", KEY),
            client_ca_file: None,
        };
        assert!(acceptor(&cfg).is_ok());
    }

    #[test]
    fn mutual_tls_accepts_a_ca_bundle() {
        let d = tmpdir();
        // The server cert doubles as its own CA here: the verifier only needs a
        // parseable trust anchor, which is what this asserts.
        let cfg = TlsConfig {
            cert_file: write(&d, "two.crt", CERT),
            key_file: write(&d, "two.key", KEY),
            client_ca_file: Some(write(&d, "ca.crt", CERT)),
        };
        assert!(acceptor(&cfg).is_ok());
    }

    #[test]
    fn an_unparseable_key_is_a_startup_error() {
        let d = tmpdir();
        let cfg = TlsConfig {
            cert_file: write(&d, "three.crt", CERT),
            key_file: write(&d, "three.key", "-----BEGIN PRIVATE KEY-----\nbm90YWtleQ==\n-----END PRIVATE KEY-----\n"),
            client_ca_file: None,
        };
        assert!(acceptor(&cfg).is_err());
    }

    #[test]
    fn a_mismatched_key_is_a_startup_error() {
        // The case the name promises: both files parse, but the key is not the
        // certificate's. That is `with_single_cert`'s check, not the PEM
        // parser's, and it is the mistake an operator actually makes.
        let d = tmpdir();
        let cfg = TlsConfig {
            cert_file: write(&d, "four.crt", CERT),
            key_file: write(&d, "four.key", OTHER_KEY),
            client_ca_file: None,
        };
        assert!(acceptor(&cfg).is_err(), "a valid key that is not this certificate's must be refused");
    }

    #[test]
    fn a_missing_file_is_a_startup_error() {
        let cfg = TlsConfig {
            cert_file: "/nonexistent/server.crt".into(),
            key_file: "/nonexistent/server.key".into(),
            client_ca_file: None,
        };
        assert!(acceptor(&cfg).is_err());
    }
}
