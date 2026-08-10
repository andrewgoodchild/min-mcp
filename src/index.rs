//! Lexical tool search over federated tool catalogs.
//!
//! The BM25 core is the `bm25` crate — the same crate `openai/codex` uses for its
//! own `tool_search`. Everything layered above it is ours, because the crate is a
//! single-field text engine that knows nothing about tool ids:
//!
//! 1. **camelCase-aware tokenizing** (`ToolTokenizer`). Mandatory, not cosmetic:
//!    the crate's tokenizer splits on whitespace and punctuation only, so
//!    `PostCheckoutSessions` tokenizes to the single token `postcheckoutsess` and
//!    every OpenAPI `operationId` becomes unsearchable. We split first, then
//!    delegate — which also buys the crate's stemming, stop-word removal and
//!    unicode normalization, things the previous hand-rolled tokenizer only
//!    approximated with a 12-word stop list and an `ends_with('s')` heuristic.
//! 2. **Field weighting by repetition** — ids matter more than prose, but the
//!    crate indexes one flat field, so weight is expressed by repeating text.
//! 3. **Four rerank layers** over the crate's candidates: verb affinity, exact
//!    substring, a damped usage prior, and a short-id tiebreak. These operate on
//!    *raw* (unstemmed) tokens, because the stemmer maps `create`→`creat` and
//!    `update`→`updat`, which would never match the affinity table's literals.

use std::collections::{HashMap, HashSet};

use bm25::{Language, SearchEngine, SearchEngineBuilder, Tokenizer};

/// BM25 parameters. These are the `bm25` crate's defaults, deliberately: the
/// previous implementation used k1=1.5/b=0.4, and which pair is better on tool
/// catalogs is a measurement, not a preference. Both are settable on the builder,
/// so the comparison is a two-line change — see research search_recall.
const K1: f32 = 1.2;
const B: f32 = 0.75;

/// Damping for the usage prior: score *= 1 + DAMP * ln(1 + uses).
const USAGE_DAMP: f64 = 0.15;
/// Multiplier when a query verb's affinity token appears in the tool id.
const VERB_BOOST: f64 = 1.3;
/// Multiplier when the whole normalized query is a substring of the normalized
/// tool id — "post customers" → PostCustomers. A near-exact name reference
/// should beat sibling tools that merely share resource words.
const EXACT_SUBSTRING_BOOST: f64 = 1.5;
/// Weighted first-line-of-description cap, standing in for the (absent) OpenAPI
/// summary field, so one giant paragraph can't dominate the index.
const SUMMARY_CAP: usize = 200;
const BODY_CAP: usize = 300;
/// Field weights, expressed as repetition counts because the crate is single-field.
const ID_REPEATS: usize = 3;
const SUMMARY_REPEATS: usize = 2;
/// Cap on text harvested from a tool's input schema, when a variant indexes it.
/// Tight on purpose: Stripe operation schemas run to thousands of characters and
/// an uncapped dump inflates document length enough to dilute the id signal.
const PARAMS_CAP: usize = 200;
/// How deep to walk nested schemas for parameter names.
const PARAMS_DEPTH: usize = 3;

/// Which retrieval behaviours an [`Index`] has switched on.
///
/// The shipped surface uses [`IndexOptions::default`]. The point of the struct is
/// that a shadow challenger is *the same code with a different switch*, not a
/// second retriever implementation to keep in sync — see `surface/shadow.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexOptions {
    /// Apply the four rerank layers (verb affinity, exact substring, usage prior,
    /// short-id tiebreak). Off = plain BM25, which is what Codex serves.
    pub rerank: bool,
    /// Index parameter names from the input schema, weighted below the description.
    /// Rejected on a single-API benchmark (it cost 16 points of recall@1 on Stripe,
    /// whose operations nearly all share `expand`/`metadata`/`customer`), but both
    /// platforms do it for *federated* surfaces where parameter vocabulary actually
    /// discriminates. Shadow mode on real multi-upstream traffic is the right venue
    /// to settle that.
    pub params: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self { rerank: true, params: false }
    }
}

/// (query verb synonyms) -> (id tokens that satisfy the intent).
///
/// Note the limit: because REST APIs overload POST for both create and update,
/// `post` appears in both rows, so this separates READ from WRITE (get vs post
/// siblings) but NOT create from update on a POST/POST pair. That's acceptable
/// for a soft ranking boost — the resource words still carry the query. Explicit
/// effect typing measured better; see research effect_typing.
const VERB_AFFINITY: &[(&[&str], &[&str])] = &[
    (&["create", "add", "new", "make"], &["create", "post", "add", "new"]),
    (&["update", "change", "edit", "set", "modify"], &["update", "patch", "put", "post", "set", "modify"]),
    (&["delete", "remove", "cancel", "destroy"], &["delete", "remove", "cancel", "destroy"]),
    (&["list", "get", "find", "search", "show", "retrieve"], &["list", "get", "find", "search", "show", "retrieve"]),
];

/// Lowercase alphanumerics only — the ONE normal form shared by the search
/// exact-substring boost and the near-miss matcher (they must agree).
pub(crate) fn normalize_id(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Insert spaces at camelCase boundaries so a downstream whitespace tokenizer can
/// see the words. `PostCheckoutSessions` -> `Post Checkout Sessions`.
fn split_camel(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            if prev.is_lowercase() || prev.is_ascii_digit() {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out
}

/// Lowercased words with camelCase split, but **no stemming and no stop-word
/// removal** — the vocabulary the rerank layers match against. Kept separate from
/// the BM25 tokenizer on purpose: the stemmer turns `create` into `creat`, so
/// VERB_AFFINITY's literals only work against raw tokens.
fn raw_tokens(text: &str) -> Vec<String> {
    split_camel(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Parameter names harvested from a tool's input schema, breadth-first so the
/// top-level parameters — the ones a caller actually names — survive the cap.
///
/// Only used when [`IndexOptions::params`] is set. Anthropic's tool search covers
/// "argument names and argument descriptions" and Codex walks the schema in
/// `append_schema_search_text`; descriptions are deliberately excluded here because
/// including them was the variant that lost worst on Stripe.
pub fn schema_param_text(schema: &serde_json::Value) -> String {
    let mut out = String::new();
    let mut level = vec![schema];
    for _ in 0..PARAMS_DEPTH {
        if out.len() >= PARAMS_CAP || level.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for node in level {
            let Some(obj) = node.as_object() else { continue };
            if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
                for (name, sub) in props {
                    if out.len() < PARAMS_CAP {
                        if !out.is_empty() {
                            out.push(' ');
                        }
                        out.push_str(name);
                    }
                    next.push(sub);
                }
            }
            for key in ["items", "additionalProperties"] {
                if let Some(sub) = obj.get(key) {
                    next.push(sub);
                }
            }
            for key in ["anyOf", "oneOf", "allOf"] {
                if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                    next.extend(arr.iter());
                }
            }
        }
        level = next;
    }
    out
}

/// camelCase-aware tokenizer: split boundaries, then hand off to the crate's
/// tokenizer for stemming, stop words and unicode normalization.
pub struct ToolTokenizer {
    inner: bm25::DefaultTokenizer,
}

impl Default for ToolTokenizer {
    fn default() -> Self {
        Self {
            inner: bm25::DefaultTokenizer::builder()
                .language_mode(Language::English)
                .normalization(true)
                .stopwords(true)
                .stemming(true)
                .build(),
        }
    }
}

impl Tokenizer for ToolTokenizer {
    fn tokenize(&self, input_text: &str) -> Vec<String> {
        self.inner.tokenize(&split_camel(input_text))
    }
}

/// Per-tool facts the rerank layers need and the crate doesn't keep.
struct Meta {
    /// pre-dedup id token count (deliberately not `id_token_set.len()`) — shorter
    /// ids are the canonical operations
    id_token_count: f64,
    /// deduped raw id tokens, for verb-affinity matching. Only id tokens: a
    /// description containing "create" must not trigger the boost.
    id_token_set: HashSet<String>,
    norm_id: String,
}

/// One entry in the search corpus. A struct rather than a tuple because the texts
/// carry different weights and must not be transposed by accident.
#[derive(Clone)]
pub struct IndexedTool {
    pub id: String,
    pub description: String,
    /// Parameter names from the input schema; only indexed when
    /// [`IndexOptions::params`] is set. Empty for composites.
    pub params: String,
}

pub struct Index {
    engine: SearchEngine<String, u32, ToolTokenizer>,
    meta: HashMap<String, Meta>,
    usage: HashMap<String, u64>,
    opts: IndexOptions,
}

impl Index {
    /// The shipped surface's index: default options.
    pub fn build(tools: &[IndexedTool]) -> Self {
        Self::build_with(tools, IndexOptions::default())
    }

    pub fn build_with(tools: &[IndexedTool], opts: IndexOptions) -> Self {
        let mut docs: Vec<bm25::Document<String>> = Vec::with_capacity(tools.len());
        let mut meta = HashMap::with_capacity(tools.len());
        for IndexedTool { id: tool_id, description, params } in tools {
            let id_toks = raw_tokens(tool_id);
            meta.insert(
                tool_id.clone(),
                Meta {
                    id_token_count: id_toks.len() as f64,
                    id_token_set: id_toks.into_iter().collect(),
                    norm_id: normalize_id(tool_id),
                },
            );
            let params = if opts.params { params.as_str() } else { "" };
            docs.push(bm25::Document::new(
                tool_id.clone(),
                weighted_contents(tool_id, description, params),
            ));
        }

        // `with_tokenizer_and_documents` fits avgdl to this corpus. An empty
        // corpus would leave avgdl at 0 and produce NaN scores, so fall back to a
        // fixed avgdl — an empty index simply never matches anything.
        let engine = if docs.is_empty() {
            SearchEngineBuilder::<String, u32, ToolTokenizer>::with_avgdl(1.0)
                .tokenizer(ToolTokenizer::default())
                .k1(K1)
                .b(B)
                .build()
        } else {
            SearchEngineBuilder::<String, u32, ToolTokenizer>::with_tokenizer_and_documents(
                ToolTokenizer::default(),
                docs,
            )
            .k1(K1)
            .b(B)
            .build()
        };

        Index { engine, meta, usage: HashMap::new(), opts }
    }

    pub fn record_use(&mut self, tool_id: &str) {
        *self.usage.entry(tool_id.to_string()).or_default() += 1;
    }

    /// Id tokens implied by the query's verb words (e.g. "update" -> {update,
    /// patch, set, ...}). Returned as a Vec — it is only ever iterated (a handful
    /// of tokens), never looked up.
    fn desired_id_tokens(q_tokens: &[String]) -> Vec<&'static str> {
        let mut want: Vec<&'static str> = Vec::new();
        for w in q_tokens {
            for (verbs, id_toks) in VERB_AFFINITY {
                if verbs.contains(&w.as_str()) {
                    for t in *id_toks {
                        if !want.contains(t) {
                            want.push(t);
                        }
                    }
                }
            }
        }
        want
    }

    pub fn search(&self, query: &str, k: usize) -> Vec<(String, f64)> {
        let desired = Self::desired_id_tokens(&raw_tokens(query));
        let q_norm = normalize_id(query);

        // Ask for every match, not the top k: the rerank layers below can promote
        // a candidate the lexical stage ranked outside k, and truncating first
        // would make those promotions unreachable. `matches` already restricts to
        // documents sharing a query token, so this is not a full scan.
        let mut scored: Vec<(String, f64)> = Vec::new();
        for hit in self.engine.search(query, None) {
            let score = hit.score as f64;
            // The crate's IDF is the Robertson form, (N - df + 0.5)/(df + 0.5),
            // which goes NEGATIVE for a token present in more than half the
            // corpus — "get" across a REST catalog, say. Multiplying a negative
            // score by a boost makes it worse, inverting every layer below, so
            // non-positive matches are dropped exactly as before.
            if score <= 0.0 {
                continue;
            }
            let tool_id = hit.document.id;
            let mut score = score;
            if !self.opts.rerank {
                // plain BM25, which is what both platforms serve. Kept reachable so a
                // shadow challenger can show whether our layers earn their keep.
                scored.push((tool_id, score));
                continue;
            }
            if let Some(m) = self.meta.get(&tool_id) {
                // verb affinity: soft boost when the tool id carries an action
                // token the query's verb implies (favours write over read etc.)
                if desired.iter().any(|t| m.id_token_set.contains(*t)) {
                    score *= VERB_BOOST;
                }
                // near-exact name reference: the whole normalized query appears
                // inside the normalized id ("post customers" → PostCustomers)
                if q_norm.len() >= 4 && m.norm_id.contains(&q_norm) {
                    score *= EXACT_SUBSTRING_BOOST;
                }
                // near-ties break toward shorter ids (canonical operations)
                score *= 1.0 / (1.0 + 0.04 * m.id_token_count);
            }
            let uses = *self.usage.get(&tool_id).unwrap_or(&0) as f64;
            score *= 1.0 + USAGE_DAMP * (1.0 + uses).ln();
            scored.push((tool_id, score));
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }
}

/// Field weighting for a single-field engine: the id counts most, the first
/// description line next, the remainder least. Expressed by repeating the text,
/// which is approximate — BM25's term-frequency saturation means three copies is
/// less than three times the weight — but it is the only lever a flat index
/// offers, and the ordering it produces is what the recall harness measures.
fn weighted_contents(tool_id: &str, description: &str, params: &str) -> String {
    let (first, rest) = match description.split_once('\n') {
        Some((f, r)) => (f, r),
        None => (description, ""),
    };
    let summary: String = first.chars().take(SUMMARY_CAP).collect();
    let body: String = rest.chars().take(BODY_CAP).collect();

    let mut out = String::with_capacity(tool_id.len() * ID_REPEATS + summary.len() * SUMMARY_REPEATS + body.len() + 8);
    for _ in 0..ID_REPEATS {
        out.push_str(tool_id);
        out.push(' ');
    }
    for _ in 0..SUMMARY_REPEATS {
        out.push_str(&summary);
        out.push(' ');
    }
    out.push_str(&body);
    if !params.is_empty() {
        // weighted 1x, the lowest: most voluminous field, least precise
        out.push(' ');
        out.push_str(params);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, description: &str) -> IndexedTool {
        IndexedTool { id: id.into(), description: description.into(), params: String::new() }
    }

    fn fixture() -> Index {
        Index::build(&[
            t("stripe.PostCustomers", "Create a customer"),
            t("stripe.GetCustomers", "List all customers"),
            // sibling read/write pair sharing all resource words — only the
            // verb (Post vs Get) distinguishes them
            t("stripe.PostCustomersCustomer", "Update a customer"),
            t("stripe.GetCustomersCustomer", "Retrieve a customer"),
            t("stripe.PostCheckoutSessions", "Creates a Checkout Session"),
            t("stripe.PostRefunds", "Create a refund for a charge"),
            t("gh.CreateIssue", "Create an issue in a repository"),
            t("gh.ListIssues", "List issues in a repository"),
        ])
    }

    /// The reason ToolTokenizer exists. Without the camelCase pass the crate's
    /// tokenizer returns one opaque token and operationIds stop being searchable.
    #[test]
    fn tokenizer_splits_camel_case_before_stemming() {
        let t = ToolTokenizer::default();
        let toks = t.tokenize("PostCheckoutSessions");
        assert!(toks.len() > 1, "camelCase must split, got {toks:?}");
        assert!(toks.iter().any(|w| w.starts_with("checkout")), "{toks:?}");
        assert!(toks.iter().any(|w| w.starts_with("session")), "{toks:?}");
        // and the bare crate tokenizer really does collapse it, so this is load-bearing
        let bare = bm25::DefaultTokenizer::builder().build();
        assert_eq!(Tokenizer::tokenize(&bare, "PostCheckoutSessions").len(), 1);
    }

    /// Stemming is what the crate buys us over the old `ends_with('s')` hack:
    /// plural and singular must land on the same term.
    #[test]
    fn stemming_unifies_plural_and_singular() {
        let t = ToolTokenizer::default();
        assert_eq!(t.tokenize("customers"), t.tokenize("customer"));
    }

    /// The rerank layers must not be stemmed: `create` -> `creat` would never
    /// match VERB_AFFINITY's literals.
    #[test]
    fn raw_tokens_are_unstemmed_so_verb_affinity_still_matches() {
        assert!(raw_tokens("create a Customer").contains(&"create".to_string()));
        let want = Index::desired_id_tokens(&raw_tokens("create a customer"));
        assert!(want.contains(&"post"), "{want:?}");
    }

    #[test]
    fn search_finds_the_right_tool() {
        let idx = fixture();
        assert_eq!(idx.search("create a checkout session", 3)[0].0, "stripe.PostCheckoutSessions");
        assert_eq!(idx.search("refund a charge", 3)[0].0, "stripe.PostRefunds");
    }

    #[test]
    fn verb_affinity_favours_write_over_read_sibling() {
        let idx = fixture();
        // both siblings match "customer"; the verb must pick the right one
        let top = idx.search("create a customer", 4)[0].0.clone();
        assert_eq!(top, "stripe.PostCustomers", "write sibling should win");
        let top = idx.search("list customers", 4)[0].0.clone();
        assert_eq!(top, "stripe.GetCustomers", "read sibling should win");
    }

    #[test]
    fn exact_substring_beats_resource_word_siblings() {
        let idx = fixture();
        assert_eq!(idx.search("PostRefunds", 3)[0].0, "stripe.PostRefunds");
    }

    #[test]
    fn usage_prior_boosts_but_is_damped() {
        let mut idx = fixture();
        for _ in 0..50 {
            idx.record_use("stripe.GetCustomers");
        }
        // 50 uses of the read sibling must not flip a clearly-write query
        assert_eq!(
            idx.search("create a customer", 4)[0].0,
            "stripe.PostCustomers",
            "damped usage prior must never override lexical relevance"
        );
    }

    #[test]
    fn empty_index_never_matches_and_does_not_panic() {
        let idx = Index::build(&[]);
        assert!(idx.search("anything at all", 5).is_empty());
    }

    #[test]
    fn k_is_respected() {
        let idx = fixture();
        assert!(idx.search("customer", 2).len() <= 2);
    }
}
