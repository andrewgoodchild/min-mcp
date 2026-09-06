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
}

impl Caller {
    pub fn new(scopes: Vec<String>, subject: Option<String>) -> Self {
        Caller { scopes, subject }
    }

    /// A caller known only by its scopes (the `--scopes` local-dev identity).
    pub fn with_scopes(scopes: Vec<String>) -> Self {
        Caller { scopes, subject: None }
    }

    /// The label audit lines carry: the subject, or `anonymous`.
    pub fn label(&self) -> &str {
        self.subject.as_deref().unwrap_or("anonymous")
    }
}
