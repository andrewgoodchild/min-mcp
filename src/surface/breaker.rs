//! Per-tool circuit breaker: closed → open (cooldown) → half-open (one probe)
//! → closed/open. The structural fix for identical-retry loops (design law 6's
//! founding failure: an agent re-calling a tool that errors the same way every
//! time). State-machine semantics and test spec mined from ContextForge's
//! `circuit_breaker` plugin (consecutive-failure subset — the error-rate
//! window was dropped as unmeasured complexity).
//!
//! Time is passed in explicitly (`now: Instant`) so tests control the clock —
//! no sleeps, no flakes.

use std::time::{Duration, Instant};

use crate::config::Breaker;

/// What `check` tells the dispatcher to do with this call.
#[derive(Debug, PartialEq)]
pub(super) enum Decision {
    /// Proceed. `probe: true` means this is THE half-open probe call — its
    /// outcome decides whether the breaker closes or re-opens.
    Allow { probe: bool },
    /// Refuse locally: the breaker is open (or a probe is already in flight).
    Block { failures: u32, retry_in_s: u64 },
}

/// A probe that never reported back (its request task was cancelled between
/// `check` and `on_result` — e.g. client disconnect mid-await) is considered
/// stale after this long and its slot reclaimed; otherwise half-open would
/// block forever. Above the 120s transport ceiling, so a live probe is never
/// preempted. (ContextForge's `stale_probe_detection_resets_half_open` case.)
const STALE_PROBE_AFTER: Duration = Duration::from_secs(180);

/// Per-tool breaker state (owned by `Surface`, keyed by tool id).
#[derive(Debug, Default)]
pub(super) struct BreakerState {
    consecutive: u32,
    open_until: Option<Instant>,
    probe_in_flight: bool,
    probe_started: Option<Instant>,
}

impl BreakerState {
    /// Gate a call. Mutates state only to claim the half-open probe slot.
    pub(super) fn check(&mut self, _cfg: &Breaker, now: Instant) -> Decision {
        let Some(until) = self.open_until else {
            return Decision::Allow { probe: false };
        };
        if now < until {
            let retry_in_s = until.duration_since(now).as_secs().max(1);
            return Decision::Block { failures: self.consecutive, retry_in_s };
        }
        // Cooldown elapsed → half-open: exactly one probe at a time. A stale
        // in-flight probe (never reported back) is reclaimed, not honored.
        if self.probe_in_flight {
            let stale = self
                .probe_started
                .map(|at| now.duration_since(at) >= STALE_PROBE_AFTER)
                .unwrap_or(true);
            if !stale {
                return Decision::Block { failures: self.consecutive, retry_in_s: 1 };
            }
        }
        self.probe_in_flight = true;
        self.probe_started = Some(now);
        Decision::Allow { probe: true }
    }

    /// Record a call's outcome. A success closes the breaker outright; a
    /// failure counts toward the threshold — and a FAILED PROBE re-opens
    /// immediately for another full cooldown (no need to re-accumulate).
    pub(super) fn on_result(&mut self, cfg: &Breaker, is_error: bool, was_probe: bool, now: Instant) {
        if was_probe {
            self.probe_in_flight = false;
            self.probe_started = None;
        }
        if !is_error {
            self.consecutive = 0;
            self.open_until = None;
            return;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if was_probe || self.consecutive >= cfg.consecutive_failures {
            // checked_add: a config like `cooldown_s: u64::MAX` must saturate
            // (~effectively forever), not panic the dispatcher.
            self.open_until = Some(
                now.checked_add(Duration::from_secs(cfg.cooldown_s))
                    .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(failures: u32, cooldown_s: u64) -> Breaker {
        Breaker { consecutive_failures: failures, cooldown_s }
    }

    fn probe(d: Decision) -> bool {
        match d {
            Decision::Allow { probe } => probe,
            Decision::Block { .. } => panic!("expected Allow, got {d:?}"),
        }
    }

    #[test]
    fn allows_requests_when_closed() {
        let mut s = BreakerState::default();
        let now = Instant::now();
        assert_eq!(s.check(&cfg(5, 60), now), Decision::Allow { probe: false });
    }

    #[test]
    fn trips_only_at_the_consecutive_threshold() {
        let (mut s, c, now) = (BreakerState::default(), cfg(3, 60), Instant::now());
        for _ in 0..2 {
            assert!(!probe(s.check(&c, now)));
            s.on_result(&c, true, false, now);
        }
        // two failures: still closed
        assert_eq!(s.check(&c, now), Decision::Allow { probe: false });
        s.on_result(&c, true, false, now); // third trips it
        assert!(matches!(s.check(&c, now), Decision::Block { failures: 3, .. }));
    }

    #[test]
    fn success_resets_the_consecutive_count() {
        let (mut s, c, now) = (BreakerState::default(), cfg(3, 60), Instant::now());
        s.on_result(&c, true, false, now);
        s.on_result(&c, true, false, now);
        s.on_result(&c, false, false, now); // success wipes the streak
        s.on_result(&c, true, false, now);
        s.on_result(&c, true, false, now);
        assert_eq!(s.check(&c, now), Decision::Allow { probe: false }, "2 < threshold after reset");
    }

    #[test]
    fn blocks_requests_when_open_and_reports_cooldown() {
        let (mut s, c, now) = (BreakerState::default(), cfg(1, 60), Instant::now());
        s.on_result(&c, true, false, now);
        match s.check(&c, now + Duration::from_secs(10)) {
            Decision::Block { failures, retry_in_s } => {
                assert_eq!(failures, 1);
                assert!((49..=50).contains(&retry_in_s), "remaining cooldown, got {retry_in_s}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn closes_on_successful_probe() {
        let (mut s, c, now) = (BreakerState::default(), cfg(1, 60), Instant::now());
        s.on_result(&c, true, false, now); // trip
        let later = now + Duration::from_secs(61);
        assert!(probe(s.check(&c, later)), "cooldown elapsed → half-open probe");
        s.on_result(&c, false, true, later);
        assert_eq!(s.check(&c, later), Decision::Allow { probe: false }, "closed again");
    }

    #[test]
    fn reopens_for_a_full_cooldown_on_failed_probe() {
        let (mut s, c, now) = (BreakerState::default(), cfg(3, 60), Instant::now());
        for _ in 0..3 {
            s.on_result(&c, true, false, now);
        }
        let later = now + Duration::from_secs(61);
        assert!(probe(s.check(&c, later)));
        s.on_result(&c, true, true, later); // probe fails → reopen immediately
        assert!(matches!(s.check(&c, later + Duration::from_secs(30)), Decision::Block { .. }));
        // in-flight flag was cleared: after ANOTHER cooldown a new probe is allowed
        assert!(probe(s.check(&c, later + Duration::from_secs(61))));
    }

    #[test]
    fn blocks_concurrent_probes_during_half_open() {
        let (mut s, c, now) = (BreakerState::default(), cfg(1, 60), Instant::now());
        s.on_result(&c, true, false, now);
        let later = now + Duration::from_secs(61);
        assert!(probe(s.check(&c, later)), "first caller claims the probe slot");
        assert!(
            matches!(s.check(&c, later), Decision::Block { retry_in_s: 1, .. }),
            "second caller must not double-probe"
        );
    }

    #[test]
    fn stale_probe_is_reclaimed_not_honored_forever() {
        let (mut s, c, now) = (BreakerState::default(), cfg(1, 60), Instant::now());
        s.on_result(&c, true, false, now); // trip
        let half_open = now + Duration::from_secs(61);
        assert!(probe(s.check(&c, half_open)), "probe claimed");
        // ...but its on_result never arrives (request task cancelled).
        // Shortly after: still blocked (a live probe is never preempted).
        let soon = half_open + Duration::from_secs(30);
        assert!(matches!(s.check(&c, soon), Decision::Block { .. }));
        // Past the stale window: the slot is reclaimed by a NEW probe instead
        // of blocking every caller forever.
        let much_later = half_open + STALE_PROBE_AFTER + Duration::from_secs(1);
        assert!(probe(s.check(&c, much_later)), "stale probe slot reclaimed");
    }
}
