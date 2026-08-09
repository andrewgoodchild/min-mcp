//! BM25-lite search over federated tool catalogs. The BM25 core and tokenizer
//! are ported from a Python prototype. Two behaviours differ,
//! deliberately, because the proxy indexes arbitrary MCP tools rather than
//! OpenAPI operations. First, the research verb→HTTP-method boost is generalized
//! to a verb→id-token affinity (we have no HTTP method), so "update ..." favours a
//! tool whose id contains update/patch/set over its get/list sibling. Second, the
//! usage prior (damped, never dominant) reranks by observed calls. The Rust
//! behaviour is covered by this module's own tests, not the Python query suite.

use std::collections::HashMap;

const K1: f64 = 1.5;
const B: f64 = 0.4;
/// Damping for the usage prior: score *= 1 + DAMP * ln(1 + uses).
const USAGE_DAMP: f64 = 0.15;
/// Multiplier when a query verb's affinity token appears in the tool id.
const VERB_BOOST: f64 = 1.3;
/// Multiplier when the whole normalized query is a substring of the normalized
/// tool id — "post customers" → PostCustomers. A near-exact name reference
/// should beat sibling tools that merely share resource words. (Query-rewrite
/// stage: measured candidates live in search_recall.md; this is the cheap,
/// deterministic slice of it.)
const EXACT_SUBSTRING_BOOST: f64 = 1.5;
/// Weighted first-line-of-description cap, mirroring the Python summary field.
const SUMMARY_CAP: usize = 200;

const STOP: &[&str] = &[
    "a", "an", "the", "of", "for", "to", "in", "on", "with", "and", "or", "all", "my",
];

/// (query verb synonyms) -> (id tokens that satisfy the intent). Generalizes
/// the research _VERB_HINTS method boost to tool-id vocabulary.
///
/// Note the limit: because REST APIs overload POST for both create and update,
/// `post` appears in both rows, so this separates READ from WRITE (get vs post
/// siblings) but NOT create from update on a POST/POST pair. That's acceptable
/// for a soft ranking boost — the resource words still carry the query.
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

pub fn tokenize(text: &str) -> Vec<String> {
    // camelCase boundaries -> spaces, then split on non-alphanumerics
    let mut spaced = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            if prev.is_lowercase() || prev.is_ascii_digit() {
                spaced.push(' ');
            }
        }
        spaced.push(c);
    }
    let mut out = Vec::new();
    for raw in spaced.split(|c: char| !c.is_alphanumeric()) {
        let w = raw.to_lowercase();
        if w.is_empty() || STOP.contains(&w.as_str()) {
            continue;
        }
        // crude singularization so "customers" matches "customer"
        if w.len() > 3 && w.ends_with('s') {
            out.push(w[..w.len() - 1].to_string());
        }
        out.push(w);
    }
    out
}

struct Doc {
    tool_id: String,
    tokens: HashMap<String, f64>,
    len: f64,
    /// pre-dedup token count of the id (deliberately not id_token_set.len()) —
    /// shorter ids are the canonical operations (research path-depth tiebreak)
    id_tokens: f64,
    /// deduped id token set, for verb-affinity matching (distinct from `tokens`,
    /// which also holds description words that must NOT trigger the verb boost)
    id_token_set: std::collections::HashSet<String>,
    /// normalized id (lowercase alphanumerics) for the exact-substring boost
    norm_id: String,
}

pub struct Index {
    docs: Vec<Doc>,
    idf: HashMap<String, f64>,
    avg_len: f64,
    usage: HashMap<String, u64>,
}

impl Index {
    /// `tools`: (tool_id, description) pairs. Id tokens weigh 3x, the first
    /// description line 2x, the rest 1x — ids are the precise signal.
    pub fn build(tools: &[(String, String)]) -> Self {
        let mut docs = Vec::with_capacity(tools.len());
        let mut df: HashMap<String, u64> = HashMap::new();
        for (tool_id, description) in tools {
            let mut tokens: HashMap<String, f64> = HashMap::new();
            let id_toks = tokenize(tool_id);
            for t in &id_toks {
                *tokens.entry(t.clone()).or_default() += 3.0;
            }
            // The first description line stands in for the (absent) OpenAPI
            // summary field, weighted 2x but capped so a giant one-paragraph
            // description can't dominate avg_len or the index.
            let (first, rest) = match description.split_once('\n') {
                Some((f, r)) => (f, r),
                None => (description.as_str(), ""),
            };
            for t in tokenize(&first.chars().take(SUMMARY_CAP).collect::<String>()) {
                *tokens.entry(t).or_default() += 2.0;
            }
            for t in tokenize(&rest.chars().take(300).collect::<String>()) {
                *tokens.entry(t).or_default() += 1.0;
            }
            for t in tokens.keys() {
                *df.entry(t.clone()).or_default() += 1;
            }
            let len: f64 = tokens.values().sum();
            docs.push(Doc {
                tool_id: tool_id.clone(),
                tokens,
                len,
                id_tokens: id_toks.len() as f64,
                id_token_set: id_toks.into_iter().collect(),
                norm_id: normalize_id(tool_id),
            });
        }
        let n = docs.len().max(1) as f64;
        let idf = df
            .into_iter()
            .map(|(t, c)| (t, (1.0 + n / (1.0 + c as f64)).ln()))
            .collect();
        let avg_len = if docs.is_empty() {
            1.0
        } else {
            docs.iter().map(|d| d.len).sum::<f64>() / n
        };
        Index { docs, idf, avg_len, usage: HashMap::new() }
    }

    pub fn record_use(&mut self, tool_id: &str) {
        *self.usage.entry(tool_id.to_string()).or_default() += 1;
    }

    /// Id tokens implied by the query's verb words (e.g. "update" -> {update,
    /// patch, set, ...}), for the verb-affinity boost. Returned as a Vec — it
    /// is only ever iterated (a handful of tokens), never looked up.
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
        let q_tokens = tokenize(query);
        let desired = Self::desired_id_tokens(&q_tokens);
        let q_norm = normalize_id(query);
        let mut scored: Vec<(String, f64)> = Vec::new();
        for doc in &self.docs {
            let mut score = 0.0;
            for t in &q_tokens {
                let tf = *doc.tokens.get(t).unwrap_or(&0.0);
                if tf > 0.0 {
                    let norm =
                        tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * doc.len / self.avg_len));
                    score += self.idf.get(t).copied().unwrap_or(0.0) * norm;
                }
            }
            if score > 0.0 {
                // verb affinity: soft boost when the tool id carries an action
                // token the query's verb implies (favours write over read etc.)
                if desired.iter().any(|t| doc.id_token_set.contains(*t)) {
                    score *= VERB_BOOST;
                }
                // near-exact name reference: the whole normalized query appears
                // inside the normalized id ("post customers" → PostCustomers)
                if q_norm.len() >= 4 && doc.norm_id.contains(&q_norm) {
                    score *= EXACT_SUBSTRING_BOOST;
                }
                let uses = *self.usage.get(&doc.tool_id).unwrap_or(&0) as f64;
                score *= 1.0 + USAGE_DAMP * (1.0 + uses).ln();
                // near-ties break toward shorter ids (canonical operations)
                score *= 1.0 / (1.0 + 0.04 * doc.id_tokens);
                scored.push((doc.tool_id.clone(), score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Index {
        Index::build(&[
            ("stripe.PostCustomers".into(), "Create a customer".into()),
            ("stripe.GetCustomers".into(), "List all customers".into()),
            // sibling read/write pair sharing all resource words — only the
            // verb (Post vs Get) distinguishes them
            ("stripe.PostCustomersCustomer".into(), "Update a customer".into()),
            ("stripe.GetCustomersCustomer".into(), "Retrieve a customer".into()),
            ("stripe.PostCheckoutSessions".into(), "Creates a Checkout Session".into()),
            ("stripe.PostRefunds".into(), "Create a refund for a charge".into()),
            ("gh.CreateIssue".into(), "Create an issue in a repository".into()),
            ("gh.ListIssues".into(), "List issues in a repository".into()),
        ])
    }

    #[test]
    fn tokenizer_splits_camel_case_and_singularizes() {
        let t = tokenize("PostCheckoutSessions");
        assert!(t.contains(&"checkout".to_string()));
        assert!(t.contains(&"session".to_string()));
        assert!(t.contains(&"sessions".to_string()));
    }

    #[test]
    fn search_finds_the_right_tool() {
        let idx = fixture();
        assert_eq!(idx.search("create a checkout session", 3)[0].0, "stripe.PostCheckoutSessions");
        assert_eq!(idx.search("refund a charge", 3)[0].0, "stripe.PostRefunds");
        assert_eq!(idx.search("open an issue", 3)[0].0, "gh.CreateIssue");
    }

    #[test]
    fn verb_affinity_favours_write_over_read_sibling() {
        let idx = fixture();
        // both siblings match "customer"; the verb must pick the right one
        assert_eq!(
            idx.search("update a customer", 4)[0].0,
            "stripe.PostCustomersCustomer",
            "'update' must favour the write sibling over the get variant"
        );
        assert_eq!(
            idx.search("retrieve a customer", 4)[0].0,
            "stripe.GetCustomersCustomer",
            "'retrieve' must favour the read sibling"
        );
    }

    #[test]
    fn exact_name_reference_beats_sibling_resource_words() {
        let idx = fixture();
        // the whole query is a normalized substring of one id — that id must win
        // over siblings sharing the resource words
        assert_eq!(idx.search("post customers", 3)[0].0, "stripe.PostCustomers");
        assert_eq!(idx.search("get customers customer", 4)[0].0, "stripe.GetCustomersCustomer");
    }

    #[test]
    fn usage_prior_boosts_but_is_damped() {
        let mut idx = fixture();
        // "customers" matches both Post and Get variants; heavy use of Get
        // should lift it, but a hundred uses must not let an unrelated tool
        // hijack a specific query.
        for _ in 0..50 {
            idx.record_use("stripe.GetCustomers");
        }
        assert_eq!(idx.search("list customers", 2)[0].0, "stripe.GetCustomers");
        for _ in 0..100 {
            idx.record_use("stripe.GetCustomers");
        }
        assert_eq!(
            idx.search("create a checkout session", 1)[0].0,
            "stripe.PostCheckoutSessions",
            "damped usage prior must never override lexical relevance"
        );
    }
}
