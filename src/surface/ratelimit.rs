//! Token-bucket rate limits: per caller, per (caller, tool), and per tool
//! across callers (overlay `rate_limit`). A refused call is a `RATE_LIMITED`
//! isError result — a continuation prompt with a retry-after, not a protocol
//! error — and never reaches the upstream, the breaker, or the usage prior.
//!
//! Time is passed in explicitly so tests control the clock.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::config::Limit;
use crate::sync::lock;

/// Bound on distinct buckets (callers × tools). Past it, buckets idle for
/// longer than `IDLE_EVICT_S` are dropped; if that isn't enough, everything
/// is — a brief over-admission beats unbounded memory from a caller flood.
const MAX_BUCKETS: usize = 10_000;
const IDLE_EVICT_S: u64 = 600;

#[derive(Default)]
pub(super) struct Buckets {
    inner: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Buckets {
    /// Spend one call from bucket `key` under `limit` (burst = `calls`,
    /// refilling at `calls / per_s` per second). `Ok` to proceed, or
    /// `Err(seconds)` until the next call would fit.
    pub(super) fn take(&self, key: &str, limit: Limit, now: Instant) -> Result<(), u64> {
        let capacity = f64::from(limit.calls.max(1));
        let rate = capacity / limit.per_s.max(1) as f64;
        let mut map = lock(&self.inner);
        if map.len() >= MAX_BUCKETS && !map.contains_key(key) {
            map.retain(|_, b| now.saturating_duration_since(b.updated).as_secs() < IDLE_EVICT_S);
            if map.len() >= MAX_BUCKETS {
                map.clear();
            }
        }
        let b = map
            .entry(key.to_string())
            .or_insert(Bucket { tokens: capacity, updated: now });
        let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate).min(capacity);
        b.updated = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err((((1.0 - b.tokens) / rate).ceil() as u64).max(1))
        }
    }

    /// Put one call back (capped at capacity) — a call refused by a later
    /// limit must not stay charged to the ones that already admitted it.
    pub(super) fn refund(&self, key: &str, limit: Limit, now: Instant) {
        let capacity = f64::from(limit.calls.max(1));
        let rate = capacity / limit.per_s.max(1) as f64;
        let mut map = lock(&self.inner);
        let Some(b) = map.get_mut(key) else { return };
        let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate + 1.0).min(capacity);
        b.updated = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lim(calls: u32, per_s: u64) -> Limit {
        Limit { calls, per_s }
    }

    #[test]
    fn burst_then_refuse_then_refill() {
        let b = Buckets::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(b.take("k", lim(3, 60), t0), Ok(()));
        }
        let retry = b.take("k", lim(3, 60), t0).unwrap_err();
        assert!((1..=20).contains(&retry), "one token refills in 20s, got {retry}");
        // 20s later exactly one call fits again
        let t1 = t0 + Duration::from_secs(20);
        assert_eq!(b.take("k", lim(3, 60), t1), Ok(()));
        assert!(b.take("k", lim(3, 60), t1).is_err());
        // a long idle refills to capacity, never beyond
        let t2 = t1 + Duration::from_secs(3600);
        for _ in 0..3 {
            assert_eq!(b.take("k", lim(3, 60), t2), Ok(()));
        }
        assert!(b.take("k", lim(3, 60), t2).is_err());
    }

    #[test]
    fn buckets_are_independent_per_key() {
        let b = Buckets::default();
        let now = Instant::now();
        assert_eq!(b.take("caller:a", lim(1, 60), now), Ok(()));
        assert!(b.take("caller:a", lim(1, 60), now).is_err());
        assert_eq!(b.take("caller:b", lim(1, 60), now), Ok(()), "b is unaffected by a's exhaustion");
    }

    #[test]
    fn a_refund_restores_exactly_one_call() {
        let b = Buckets::default();
        let now = Instant::now();
        assert_eq!(b.take("k", lim(1, 60), now), Ok(()));
        assert!(b.take("k", lim(1, 60), now).is_err(), "budget spent");
        b.refund("k", lim(1, 60), now);
        assert_eq!(b.take("k", lim(1, 60), now), Ok(()), "the refunded call is available again");
        // refunds never exceed the burst capacity
        for _ in 0..5 {
            b.refund("k", lim(1, 60), now);
        }
        assert_eq!(b.take("k", lim(1, 60), now), Ok(()));
        assert!(b.take("k", lim(1, 60), now).is_err(), "capacity is still 1");
        // refunding a bucket that was never taken from is a no-op, not a credit
        b.refund("never-seen", lim(1, 60), now);
        assert_eq!(b.take("never-seen", lim(1, 60), now), Ok(()));
        assert!(b.take("never-seen", lim(1, 60), now).is_err());
    }

    #[test]
    fn zero_and_degenerate_limits_do_not_panic() {
        let b = Buckets::default();
        let now = Instant::now();
        // calls: 0 is treated as 1; per_s: 0 as 1
        assert_eq!(b.take("k", lim(0, 0), now), Ok(()));
        assert!(b.take("k", lim(0, 0), now).is_err());
    }
}
