//! Leveled logging over the ecosystem-standard [`tracing`]. The macros
//! (`log_error!` … `log_debug!`) are kept as thin wrappers with an explicit
//! `minmcp` target, so call sites are unchanged but output now flows through a
//! `tracing` subscriber — which ALSO captures rmcp's own spans (the SDK logs via
//! `tracing`). The threshold is read once from `MINMCP_LOG`:
//!
//!   MINMCP_LOG=off | error | warn | info | debug     (default: warn)
//!
//! These are for OPERATIONAL logs (connection/upstream/auth failures). Tool and
//! upstream *call* errors are NOT logged here — they are returned to the agent
//! as `isError` results by design (errors are continuation prompts). stdout is
//! reserved for the stdio MCP protocol, so everything goes to stderr.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

/// Install the global stderr subscriber, filtered by `MINMCP_LOG`. Call once at
/// startup. `off` installs nothing (so not even rmcp emits). Our crate is scoped
/// to the requested level; rmcp is kept a notch quieter so `debug` on our code
/// doesn't drown in SDK internals. Idempotent: a second call is a no-op.
pub fn init() {
    let (mine, rmcp) = match std::env::var("MINMCP_LOG").ok().as_deref().map(str::trim) {
        Some("off") => return,
        Some("error") => (LevelFilter::ERROR, LevelFilter::OFF),
        Some("info") => (LevelFilter::INFO, LevelFilter::WARN),
        Some("debug") => (LevelFilter::DEBUG, LevelFilter::INFO),
        _ => (LevelFilter::WARN, LevelFilter::WARN), // default (and explicit "warn")
    };
    // `min_mcp` is this crate's module-path root (package `min-mcp`); the macros
    // below also tag events `target: "minmcp"`, so cover both spellings.
    let filter = Targets::new()
        .with_target("min_mcp", mine)
        .with_target("minmcp", mine)
        .with_target("rmcp", rmcp);
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false),
        )
        .with(filter)
        .try_init();
}

#[macro_export]
macro_rules! log_error {
    ($($a:tt)*) => { tracing::error!(target: "minmcp", $($a)*) };
}
#[macro_export]
macro_rules! log_warn {
    ($($a:tt)*) => { tracing::warn!(target: "minmcp", $($a)*) };
}
#[macro_export]
macro_rules! log_info {
    ($($a:tt)*) => { tracing::info!(target: "minmcp", $($a)*) };
}
#[macro_export]
macro_rules! log_debug {
    ($($a:tt)*) => { tracing::debug!(target: "minmcp", $($a)*) };
}
