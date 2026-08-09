//! minmcp — minify your MCPs.
//!
//! `minmcp serve`   stdio MCP server proxying configured upstreams
//! `minmcp inspect` print what would be minified (counts + token estimates)

mod auth;
mod backend;
mod config;
mod exec;
mod http_upstream;
mod index;
mod jq;
mod jsonrpc;
mod lint;
mod logging;
mod oauth;
mod project;
mod rmcp_serve;
mod spec;
// note: `jsonrpc` stays — the upstream MCP clients (upstream.rs / http_upstream.rs)
// still frame JSON-RPC by hand; only the *server* side moved to rmcp.
mod surface;
mod upstream;

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use config::Config;
use surface::Surface;

#[derive(Parser)]
#[command(name = "minmcp", version, about = "Minify your MCPs", disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

/// Options shared by serve and inspect: which config, and who the caller is.
#[derive(Args)]
struct Common {
    #[arg(long, default_value = "min.yaml")]
    config: String,
    /// Scopes granted to this session (comma separated). Local-dev identity;
    /// prefer --jwt in production.
    #[arg(long, value_delimiter = ',', default_value = "")]
    scopes: Vec<String>,
    /// A caller JWT. Its scope claim (validated against the config/env secret)
    /// becomes the granted scopes, overriding --scopes.
    #[arg(long)]
    jwt: Option<String>,
    /// serve only: expose the surface over Streamable HTTP at this address
    /// (e.g. 127.0.0.1:8080) instead of stdio. Binds localhost; validates Origin.
    #[arg(long)]
    http: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the minified surface over stdio
    Serve(Common),
    /// Print surface statistics without serving
    Inspect(Common),
    /// Print the source map: every tool mapped back to its origin
    /// (METHOD /path or upstream tool), overlays applied, and a schema
    /// fingerprint. The minifier's source map / binding registry. With
    /// `--diff old-map.json`, compares against a saved map and reports which
    /// tools changed and which overlays must be re-verified (exit 1 if any).
    Map {
        #[command(flatten)]
        common: Common,
        #[arg(long)]
        diff: Option<String>,
    },
    /// Run overlay `verify:` checks against the live upstream — the dynamic
    /// third leg (detect → fix → verify). Calls each checked tool and asserts
    /// the result; exits non-zero if any check fails (CI / drift gate).
    Verify(Common),
    /// Static quality lint of every registered tool: best-practice smells over
    /// each tool's resolved definition, with per-rule aggregate stats. Findings
    /// are drafted, never applied.
    Lint {
        #[command(flatten)]
        common: Common,
        /// How many flagged tools to list as examples (0 = aggregate only).
        #[arg(long, default_value = "12")]
        sample: usize,
    },
    /// Search tools by task description (CLI access to search_tools).
    Search {
        #[command(flatten)]
        common: Common,
        /// What you are trying to do, e.g. "create a customer".
        query: String,
        #[arg(long, default_value = "10")]
        k: usize,
    },
    /// Print a tool's full description and input schema (like `--help` for a tool).
    Help {
        #[command(flatten)]
        common: Common,
        tool_id: String,
    },
    /// Call a tool by id, with optional GraphQL-style field projection.
    Call {
        #[command(flatten)]
        common: Common,
        tool_id: String,
        /// Arguments as a JSON object, e.g. '{"query_params":{"limit":3}}'.
        #[arg(long, default_value = "{}")]
        args: String,
        /// Return ONLY these response fields (comma-separated dotted paths, `[]`
        /// maps over an array), e.g. --fields 'data[].id,data[].amount,has_more'.
        #[arg(long, value_delimiter = ',')]
        fields: Vec<String>,
    },
}

/// Build the JWT verifier from config, in precedence order: a JWKS endpoint
/// (fetched once here), an RS256 public key, or an HS256 shared secret.
async fn build_verifier(cfg: &Config) -> Result<Option<auth::JwtVerifier>> {
    let a = &cfg.auth;
    if let Some(url) = &a.jwks_url {
        let json = reqwest::get(url)
            .await
            .with_context(|| format!("fetching JWKS from {url}"))?
            .text()
            .await?;
        return Ok(Some(auth::jwks_from_json(&json)?));
    }
    if let Some(pem) = a.public_key_pem()? {
        return Ok(Some(auth::rs256_from_pem(&pem)?));
    }
    if let Some(secret) = a.secret() {
        return Ok(Some(auth::JwtVerifier::Hs256(secret.into_bytes())));
    }
    Ok(None)
}

/// Resolve the caller's granted scopes: a validated JWT wins; otherwise the
/// --scopes flag (local-dev identity).
async fn resolve_scopes(cfg: &Config, common: &Common) -> Result<Vec<String>> {
    if let Some(token) = &common.jwt {
        let verifier = build_verifier(cfg).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "--jwt given but no verifier configured (auth.jwt_secret / jwt_public_key / jwks_url)"
            )
        })?;
        return verifier.scopes(token, &cfg.auth.scope_claim);
    }
    Ok(clean(common.scopes.clone()))
}

async fn build(common: &Common) -> Result<Surface> {
    let cfg = Config::load(&common.config)?;
    let granted = resolve_scopes(&cfg, common).await?;
    Surface::build(cfg, granted).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    logging::init(); // stderr tracing subscriber, filtered by MINMCP_LOG
    let cli = Cli::parse();
    match cli.command {
        Cmd::Inspect(common) => {
            let surface = build(&common).await?;
            println!("{}", serde_json::to_string_pretty(&surface.stats())?);
            Ok(())
        }
        Cmd::Map { common, diff } => {
            let surface = build(&common).await?;
            let current = surface.source_map(false);
            match diff {
                None => {
                    println!("{}", serde_json::to_string_pretty(&current)?);
                    Ok(())
                }
                Some(old_path) => {
                    let old: Value = serde_json::from_str(
                        &std::fs::read_to_string(&old_path)
                            .with_context(|| format!("reading old map {old_path}"))?,
                    )?;
                    let report = diff_maps(&old, &current);
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    if report["breaking"].as_bool().unwrap_or(false) {
                        std::process::exit(1); // CI signal: bindings need re-verification
                    }
                    Ok(())
                }
            }
        }
        Cmd::Verify(common) => {
            let mut surface = build(&common).await?;
            let report = surface.verify().await;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report["failed"].as_u64().unwrap_or(0) > 0 {
                std::process::exit(1); // CI signal: a fix/binding no longer holds
            }
            Ok(())
        }
        Cmd::Lint { common, sample } => {
            let surface = build(&common).await?;
            println!("{}", serde_json::to_string_pretty(&surface.lint_report(sample))?);
            Ok(())
        }
        Cmd::Search { common, query, k } => {
            let surface = build(&common).await?;
            println!("{}", surface.cli_search(&query, k));
            Ok(())
        }
        Cmd::Help { common, tool_id } => {
            let surface = build(&common).await?;
            println!("{}", surface.cli_details(&tool_id));
            Ok(())
        }
        Cmd::Call { common, tool_id, args, fields } => {
            let mut surface = build(&common).await?;
            let arguments: Value =
                serde_json::from_str(&args).context("--args must be a JSON object")?;
            let result = surface.cli_call(&tool_id, arguments, &fields).await?;
            print_tool_result(&result);
            Ok(())
        }
        Cmd::Serve(common) => {
            let surface = build(&common).await?;
            print_serve_banner(&surface);
            // Both transports are served by the official MCP SDK (rmcp),
            // wrapping our Surface behind its ServerHandler.
            match &common.http {
                Some(addr) => rmcp_serve::serve_http(surface, addr).await,
                None => rmcp_serve::serve_stdio(surface).await,
            }
        }
    }
}

fn clean(scopes: Vec<String>) -> Vec<String> {
    scopes.into_iter().filter(|s| !s.is_empty()).collect()
}

/// One honest line at startup (stderr — stdout is the protocol): what got
/// minified, to what, and the estimated ratio. And the line most tools won't
/// print: when the minified surface is BIGGER than declaring everything
/// (small N), say so and recommend `mode: passthrough` — a minifier that
/// can't tell you when not to minify is marketing, not measurement.
fn print_serve_banner(surface: &crate::surface::Surface) {
    let s = surface.stats();
    let (raw, min) = (
        s["est_tokens_raw"].as_u64().unwrap_or(0),
        s["est_tokens_minified"].as_u64().unwrap_or(0),
    );
    // Large spec surfaces report raw from unresolved $ref stubs — a lower
    // bound, marked "≥"; the passthrough NOTE only fires on exact numbers.
    let exact = s["est_raw_exact"].as_bool().unwrap_or(false);
    let bound = if exact { "" } else { "≥" };
    let ratio = if min > 0 { raw as f64 / min as f64 } else { 0.0 };
    let ratio = if ratio >= 10.0 { format!("{ratio:.0}×") } else { format!("{ratio:.1}×") };
    eprintln!(
        "min-mcp: {} upstream tool(s) across {} upstream(s) → {} surface tool(s); ~{} tokens vs {bound}{} raw ({bound}{ratio})",
        s["upstream_tools"], s["upstreams_active"], s["surface_tools"], min, raw
    );
    if s["mode"] == "ThreeTool" && exact && min >= raw && raw > 0 {
        eprintln!(
            "min-mcp: NOTE — at this size the minified surface is not smaller than declaring \
             every tool (~{min} vs ~{raw} tokens); consider `mode: passthrough` until the \
             surface grows"
        );
    }
}

/// Index a source map to (tool_id -> schema_sha) and the set of tool_ids that
/// carry an overlay.
fn index_source_map(m: &Value) -> (HashMap<String, String>, HashSet<String>) {
    let mut sha = HashMap::new();
    let mut overlaid = HashSet::new();
    for t in m["tools"].as_array().into_iter().flatten() {
        if let Some(id) = t["tool_id"].as_str() {
            if let Some(s) = t["schema_sha"].as_str() {
                sha.insert(id.to_string(), s.to_string());
            }
            if !t["overlay"].is_null() {
                overlaid.insert(id.to_string());
            }
        }
    }
    (sha, overlaid)
}

/// Diff two source maps: which tools' schemas changed/were removed/added, and
/// which overlays therefore need re-verification. `breaking` is true iff an
/// overlaid tool changed or vanished.
fn diff_maps(old: &Value, cur: &Value) -> Value {
    let (old_sha, _) = index_source_map(old);
    let (cur_sha, cur_overlaid) = index_source_map(cur);
    let (mut changed, mut removed, mut added) = (Vec::new(), Vec::new(), Vec::new());
    for (id, s) in &cur_sha {
        match old_sha.get(id) {
            Some(o) if o != s => changed.push(id.clone()),
            None => added.push(id.clone()),
            _ => {}
        }
    }
    for id in old_sha.keys() {
        if !cur_sha.contains_key(id) {
            removed.push(id.clone());
        }
    }
    let mut affected: Vec<String> = changed
        .iter()
        .chain(&removed)
        .filter(|id| cur_overlaid.contains(*id))
        .cloned()
        .collect();
    for v in [&mut changed, &mut removed, &mut added, &mut affected] {
        v.sort();
    }
    let breaking = !affected.is_empty();
    json!({
        "changed": changed,
        "removed": removed,
        "added": added,
        "overlays_to_reverify": affected,
        "breaking": breaking,
    })
}

/// Print a tool result's text content to stdout (the payload the model would
/// see) — for the `minmcp call` CLI, so output pipes into `jq` etc.
fn print_tool_result(result: &Value) {
    match result.get("content").and_then(Value::as_array) {
        Some(blocks) => {
            for b in blocks {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    println!("{t}");
                } else if let Some(ty) = b.get("type").and_then(Value::as_str) {
                    // non-text block (image, audio, resource…): a placeholder so the
                    // CLI doesn't silently swallow it (real MCP clients get it whole).
                    let mime = b.get("mimeType").and_then(Value::as_str).unwrap_or("");
                    let bytes = b.get("data").and_then(Value::as_str).map(str::len).unwrap_or(0);
                    let mime = if mime.is_empty() { String::new() } else { format!(" {mime}") };
                    let bytes = if bytes > 0 { format!(", {bytes} bytes") } else { String::new() };
                    println!("[{ty}{mime}{bytes}]");
                }
            }
            if result.get("structuredContent").is_some() {
                println!("[+ structuredContent]");
            }
        }
        None => println!("{}", serde_json::to_string_pretty(result).unwrap_or_default()),
    }
}

