//! Who is calling.
//!
//! Over stdio the caller is the process: `--jwt` (validated) or `--scopes`,
//! resolved once at startup. Over HTTP it is derived **per request** — from a
//! validated `Authorization: Bearer` token, or from identity headers set by a
//! trusted gateway — and attached to the request before it reaches the MCP
//! handler. Every surface operation that filters by scope takes a `&Caller`,
//! so two clients of one `serve --http` see two different surfaces, and every
//! audit line says who acted.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caller {
    /// Granted scopes. With `scopes:` rules configured, a tool is visible only
    /// if one of these grants it; with no rules, everything is visible.
    pub scopes: Vec<String>,
    /// Who this is — the JWT subject (or the configured claim), a gateway
    /// identity header, or None for the process identity. Audit only; it never
    /// affects what is visible.
    pub subject: Option<String>,
    /// What the CONNECTION says about this caller when the request itself does
    /// not: the client certificate's fingerprint under mutual TLS, else the peer
    /// address. Never an identity — it names a socket, not a person — so it is
    /// used for rate-limit bucketing only, never for audit and never for scopes.
    pub origin: Option<String>,
}

impl Caller {
    pub fn new(scopes: Vec<String>, subject: Option<String>) -> Self {
        Caller { scopes, subject, origin: None }
    }

    /// A caller known only by its scopes (the `--scopes` local-dev identity).
    pub fn with_scopes(scopes: Vec<String>) -> Self {
        Caller { scopes, subject: None, origin: None }
    }

    /// Attach what the connection knows, for callers the request did not name.
    pub fn with_origin(mut self, origin: Option<String>) -> Self {
        self.origin = origin;
        self
    }

    /// The label audit lines carry: the subject, or `anonymous`.
    ///
    /// Deliberately NOT the rate-limit key — see [`Self::rate_key`]. `anonymous`
    /// is the right word for an audit line about a caller nobody named; it is
    /// the wrong key for a bucket, where it would mean "charge every unnamed
    /// caller to one budget".
    pub fn label(&self) -> &str {
        self.subject.as_deref().unwrap_or("anonymous")
    }

    /// The key rate-limit buckets are charged to: the most specific thing
    /// actually known about this caller.
    ///
    /// 1. the subject, when the request named one (a bearer's `sub`, or the
    ///    gateway's identity header) — a real per-caller key;
    /// 2. else what the connection says (`origin`) — a client-certificate
    ///    fingerprint under mutual TLS, else the peer address;
    /// 3. else `anonymous`, which is one shared bucket.
    ///
    /// Step 2 is an improvement, not a cure. It genuinely separates direct
    /// clients that present no token, and it separates certificate holders
    /// under mutual TLS. It does NOT separate callers behind a shared gateway:
    /// they share one peer address, so a gateway that forwards scopes without
    /// `trusted_headers.subject` still lands them in one bucket. Nothing
    /// observable at this layer can tell those callers apart, which is why that
    /// case is a startup warning rather than something to paper over here.
    pub fn rate_key(&self) -> &str {
        self.subject.as_deref().or(self.origin.as_deref()).unwrap_or("anonymous")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subject_keys_the_bucket_when_the_request_names_one() {
        let c = Caller::new(vec!["a".into()], Some("alice".into()));
        assert_eq!(c.rate_key(), "alice");
        assert_eq!(c.label(), "alice");
        // A connection origin never displaces a subject the request supplied.
        let c = c.with_origin(Some("peer:10.0.0.1".into()));
        assert_eq!(c.rate_key(), "alice");
    }

    #[test]
    fn the_connection_keys_the_bucket_when_the_request_does_not() {
        // This is the fix: two unnamed callers on different connections used to
        // share one `anonymous` bucket, so either could exhaust the other's.
        let a = Caller::with_scopes(vec!["x".into()]).with_origin(Some("peer:10.0.0.1".into()));
        let b = Caller::with_scopes(vec!["x".into()]).with_origin(Some("peer:10.0.0.2".into()));
        assert_ne!(a.rate_key(), b.rate_key());
        let cert = Caller::with_scopes(vec![]).with_origin(Some("cert:abc123".into()));
        assert_eq!(cert.rate_key(), "cert:abc123");
    }

    #[test]
    fn audit_never_shows_the_connection_origin() {
        // `origin` names a socket, not a person; an audit line saying
        // `peer:10.0.0.1` acted would read as an identity it is not.
        let c = Caller::with_scopes(vec![]).with_origin(Some("peer:10.0.0.1".into()));
        assert_eq!(c.label(), "anonymous");
    }

    #[test]
    fn a_caller_with_nothing_known_still_falls_back() {
        let c = Caller::with_scopes(vec![]);
        assert_eq!(c.rate_key(), "anonymous");
        assert_eq!(c.label(), "anonymous");
    }
}
