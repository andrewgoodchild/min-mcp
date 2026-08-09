//! Tool-id hygiene: MCP-safe names and near-miss suggestions for slips.

use super::*;

/// MCP tool names must match ^[a-zA-Z0-9_-]{1,64}$ — tool ids contain dots.
/// ASCII-only: `is_alphanumeric` would keep 'é'/'ü' etc., producing a name the
/// pattern rejects (and byte-length > char-length, breaking the 64 cap).
/// Reserves `reserve` trailing chars so a disambiguating suffix can be appended
/// within the 64-char limit (the collision loop depends on this).
pub(super) fn sanitize_name(id: &str, reserve: usize) -> String {
    let cleaned = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' });
    cleaned.take(MAX_NAME_LEN.saturating_sub(reserve)).collect()
}

/// The `n` candidate strings closest to `wrong` by edit distance over a
/// normalized form (lowercase, alphanumerics only — so `stripe.PostCustomers`
/// and `stripe_postcustomers` are 0 apart). Only returns matches within a
/// distance budget of max(2, len/4): plausible slips, never wild guesses.
pub(super) fn closest_matches<'a>(wrong: &str, candidates: impl Iterator<Item = &'a str>, n: usize) -> Vec<String> {
    let wnorm = normalize_id(wrong);
    if wnorm.is_empty() {
        return Vec::new();
    }
    let max_d = 2.max(wnorm.chars().count() / 4);
    let mut scored: Vec<(usize, &str)> = Vec::new();
    for cand in candidates {
        let cnorm = normalize_id(cand);
        // length prefilter: distance is at least the length difference
        if cnorm.chars().count().abs_diff(wnorm.chars().count()) > max_d {
            continue;
        }
        let d = edit_distance(&wnorm, &cnorm);
        if d <= max_d {
            scored.push((d, cand));
        }
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    scored.into_iter().take(n).map(|(_, s)| s.to_string()).collect()
}

pub(super) use crate::index::normalize_id;

/// Plain Levenshtein over chars (two-row DP). Candidate sets are catalog-sized
/// and this only runs on the unknown-id error path.
pub(super) fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The shared ` did you mean "x" or "y"?` fragment for a mistyped name, over
/// whichever candidate set the caller holds (tool ids, exposed names). Empty
/// when nothing is plausibly close — never a wild guess.
pub(super) fn did_you_mean<'a>(wrong: &str, candidates: impl Iterator<Item = &'a str>) -> String {
    let close = closest_matches(wrong, candidates, 2);
    if close.is_empty() {
        String::new()
    } else {
        let quoted: Vec<String> = close.iter().map(|s| format!("{s:?}")).collect();
        format!(" did you mean {}?", quoted.join(" or "))
    }
}
