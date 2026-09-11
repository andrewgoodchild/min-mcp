//! The process-wide rustls crypto provider.
//!
//! Both reqwest and vaultrs are built against rustls WITHOUT selecting a
//! provider (`rustls-no-provider`), because the alternative pulls `aws-lc-rs`
//! — a C build needing NASM on Windows x86-64, which is why this project pins
//! `ring` everywhere it can. The cost of "no provider" is that they take the
//! PROCESS DEFAULT, and building a client **panics** when there is none.
//!
//! That has bitten three times: Vault references aborted the process, and both
//! the jsonwebtoken 11 and reqwest 0.13 upgrades panicked the same way. So the
//! install lives in one place and is called from every site that builds a
//! client, rather than once in `main`: `main` is not on the path of a unit
//! test, and a provider installed only there leaves every test — and any other
//! entry point — panicking on the first client it builds.

/// Install `ring` as the process default, once. Idempotent and thread-safe.
///
/// A second call, or a provider another crate already installed, is fine: any
/// provider satisfies the requirement, and `install_default` reports the
/// conflict rather than replacing it.
pub fn install_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
