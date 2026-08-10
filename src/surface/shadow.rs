//! Shadow selection: score alternative retrievers on real traffic, serve none of
//! them.
//!
//! The problem this solves is that a labelled query set is expensive to build and
//! small once built — and small sets lie. Our 18-query Stripe set produced an
//! 11-point recall@1 "win" that was 13 hits against 11, a difference no honest
//! reading can distinguish from noise. Growing the set helps, but authoring
//! labelled queries doesn't scale.
//!
//! So take the label from behaviour instead. On every `search_tools` call, run each
//! challenger over the same query and remember its ranked ids. When the agent then
//! calls a tool, look up what rank each challenger *would* have put that tool at,
//! and log it. The tool the agent actually chose is the label, and it costs nothing
//! to collect. (Prior art: `openai/codex`'s
//! `ext/skills/src/shadow_selection_experiment.rs`, which runs seven selectors this
//! way. A proxy is a better vantage point than an agent — it sees every client's
//! searches and calls, not one agent's.)
//!
//! **What this measures, and what it does not.** It measures *agreement with what
//! the agent did*. If retrieval never surfaced the right tool and the agent called
//! a poor substitute, that substitute counts as a hit. So shadow data is strong for
//! regression detection and for comparing challengers against the incumbent, and
//! weak as an absolute correctness signal — labelled queries still own that. Read
//! the two together.
//!
//! Off unless `shadow: true` is configured: each challenger is a full second index,
//! so it costs memory and startup time that production should not pay.

use std::collections::HashSet;

use crate::index::{Index, IndexOptions, IndexedTool};

/// Cap on remembered ranked ids per challenger. Beyond this a "miss" is reported,
/// which is the honest answer — nothing reads past a window this deep anyway.
const MAX_REMEMBERED: usize = 20;
/// Queries longer than this are skipped rather than truncated: a giant query is
/// not representative, and half of one is worse than none.
const MAX_QUERY_CHARS: usize = 2_000;

/// A challenger: a name for the telemetry, and an index built with different
/// options. Deliberately not a trait — a challenger is the *same retrieval code
/// with a switch flipped*, so there is no second implementation to drift.
struct Challenger {
    method: &'static str,
    index: Index,
}

/// What one `search_tools` call left behind for the next `call_tool` to score.
struct Turn {
    /// method -> ranked tool ids, best first
    ranked: Vec<(&'static str, Vec<String>)>,
    /// ids the incumbent returned, so its own rank is comparable on the same row
    served: Vec<String>,
    /// tools already scored this turn — one credit per tool per search, so a loop
    /// calling the same tool ten times doesn't ten-count the retriever
    scored: HashSet<String>,
}

pub(super) struct Shadow {
    challengers: Vec<Challenger>,
    turn: Option<Turn>,
}

/// One row to log when a call reveals what the agent wanted.
pub(super) struct Observation {
    pub method: &'static str,
    pub hit: bool,
    /// 1-based rank, or None on a miss
    pub rank: Option<usize>,
}

impl Shadow {
    /// `enabled: false` builds nothing — no challenger indices, no memory, and
    /// every method below becomes a no-op.
    pub(super) fn new(corpus: &[IndexedTool], enabled: bool) -> Self {
        if !enabled {
            return Shadow { challengers: Vec::new(), turn: None };
        }
        let variants: &[(&'static str, IndexOptions)] = &[
            // Plain BM25 — what both platforms serve. Tests whether our four rerank
            // layers earn their keep on real queries, or just on our fixtures.
            ("plain_bm25", IndexOptions { rerank: false, params: false }),
            // Parameter names indexed. Lost badly on the single-API Stripe set, but
            // that is the adversarial case; on a federated surface parameter
            // vocabulary should discriminate. This is the venue to find out.
            ("params", IndexOptions { rerank: true, params: true }),
            // Both switches, to catch an interaction the singles would miss.
            ("plain_params", IndexOptions { rerank: false, params: true }),
        ];
        let challengers = variants
            .iter()
            .map(|(method, opts)| Challenger { method, index: Index::build_with(corpus, *opts) })
            .collect();
        Shadow { challengers, turn: None }
    }

    pub(super) fn enabled(&self) -> bool {
        !self.challengers.is_empty()
    }

    /// Run every challenger over a query the incumbent just answered, and remember
    /// the rankings. `served` is the incumbent's own result, kept so one log row can
    /// compare like with like.
    pub(super) fn observe_search(&mut self, query: &str, served: &[String]) {
        if !self.enabled() || query.chars().count() > MAX_QUERY_CHARS {
            self.turn = None;
            return;
        }
        let ranked = self
            .challengers
            .iter()
            .map(|c| {
                let ids = c
                    .index
                    .search(query, MAX_REMEMBERED)
                    .into_iter()
                    .map(|(id, _score)| id)
                    .collect();
                (c.method, ids)
            })
            .collect();
        self.turn = Some(Turn {
            ranked,
            served: served.iter().take(MAX_REMEMBERED).cloned().collect(),
            scored: HashSet::new(),
        });
    }

    /// The agent called `tool_id`. Score every challenger against it, plus the
    /// incumbent. Returns an empty vec when there is nothing to say — disabled, no
    /// preceding search, or this tool was already credited this turn.
    ///
    /// `eligible` gates out ids that aren't in the searchable surface at all
    /// (composites invoked directly, a hand-typed id): no retriever should be
    /// blamed for failing to rank something that was never a candidate.
    pub(super) fn observe_call(&mut self, tool_id: &str, eligible: bool) -> Vec<Observation> {
        if !eligible {
            return Vec::new();
        }
        let Some(turn) = self.turn.as_mut() else { return Vec::new() };
        if !turn.scored.insert(tool_id.to_string()) {
            return Vec::new();
        }
        let rank_in = |ids: &[String]| ids.iter().position(|id| id == tool_id).map(|i| i + 1);

        let mut out = vec![Observation {
            method: "served",
            hit: rank_in(&turn.served).is_some(),
            rank: rank_in(&turn.served),
        }];
        for (method, ids) in &turn.ranked {
            let rank = rank_in(ids);
            out.push(Observation { method, hit: rank.is_some(), rank });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Vec<IndexedTool> {
        vec![
            IndexedTool {
                id: "stripe.PostCustomers".into(),
                description: "Create a customer".into(),
                params: "email name".into(),
            },
            IndexedTool {
                id: "stripe.GetCustomers".into(),
                description: "List all customers".into(),
                params: "limit starting_after".into(),
            },
            IndexedTool {
                id: "stripe.PostPayouts".into(),
                description: "Create a payout".into(),
                params: "amount currency destination bank_account".into(),
            },
        ]
    }

    #[test]
    fn disabled_builds_nothing_and_reports_nothing() {
        let mut s = Shadow::new(&corpus(), false);
        assert!(!s.enabled());
        s.observe_search("create a customer", &["stripe.PostCustomers".to_string()]);
        assert!(s.observe_call("stripe.PostCustomers", true).is_empty());
    }

    #[test]
    fn scores_every_challenger_and_the_served_result() {
        let mut s = Shadow::new(&corpus(), true);
        s.observe_search("create a customer", &["stripe.PostCustomers".to_string()]);
        let obs = s.observe_call("stripe.PostCustomers", true);
        // one row for the incumbent plus one per challenger
        assert_eq!(obs.len(), 4, "got {:?}", obs.iter().map(|o| o.method).collect::<Vec<_>>());
        assert!(obs.iter().any(|o| o.method == "served" && o.rank == Some(1)));
        for o in &obs {
            assert!(o.hit, "{} should have found the obvious tool", o.method);
        }
    }

    /// The label is behaviour, so a tool called twice in one turn must count once —
    /// otherwise a retry loop silently inflates whichever retriever ranked it.
    #[test]
    fn one_credit_per_tool_per_search() {
        let mut s = Shadow::new(&corpus(), true);
        s.observe_search("create a customer", &["stripe.PostCustomers".to_string()]);
        assert!(!s.observe_call("stripe.PostCustomers", true).is_empty());
        assert!(s.observe_call("stripe.PostCustomers", true).is_empty(), "second call must not re-credit");
    }

    #[test]
    fn ineligible_and_searchless_calls_are_ignored() {
        let mut s = Shadow::new(&corpus(), true);
        s.observe_search("create a customer", &["stripe.PostCustomers".to_string()]);
        assert!(s.observe_call("stripe.PostCustomers", false).is_empty(), "ineligible id");

        let mut s2 = Shadow::new(&corpus(), true);
        assert!(s2.observe_call("stripe.PostCustomers", true).is_empty(), "no preceding search");
    }

    /// A miss must be recorded as a miss, not dropped — otherwise every method
    /// looks perfect and the telemetry is useless.
    #[test]
    fn a_miss_is_reported_as_a_miss() {
        let mut s = Shadow::new(&corpus(), true);
        s.observe_search("something entirely unrelated to payments", &[]);
        let obs = s.observe_call("stripe.PostPayouts", true);
        assert!(!obs.is_empty());
        assert!(obs.iter().any(|o| !o.hit && o.rank.is_none()), "expected at least one miss: {:?}",
                obs.iter().map(|o| (o.method, o.rank)).collect::<Vec<_>>());
    }

    #[test]
    fn an_absurd_query_is_skipped_rather_than_truncated() {
        let mut s = Shadow::new(&corpus(), true);
        s.observe_search(&"x".repeat(MAX_QUERY_CHARS + 1), &["stripe.PostCustomers".to_string()]);
        assert!(s.observe_call("stripe.PostCustomers", true).is_empty());
    }
}
