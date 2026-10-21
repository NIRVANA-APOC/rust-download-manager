//! Adaptive connection planning for dynamic multi-connection transfers.
//!
//! Everything here is pure decision logic: [`ConcurrencyPlanner`] consumes one
//! measurement window at a time (payload bytes transferred, window length and
//! the number of chunks still missing) and returns a new connection target when
//! the plan should change. The transfer layer owns the timers, the worker gate
//! and the event emission, which keeps every rule unit-testable with synthetic
//! samples instead of a real network (design-next-features §2.2):
//!
//! * initial target = `min(max, max(2, max / 2))`, capped at 8 for wide plans;
//! * a window without any byte progress never changes the target;
//! * throughput up ≥ 8 % vs. the previous window → target += max(1, target / 4),
//!   capped by `max` and by the number of chunks still missing;
//! * two consecutive windows with < 8 % gain → saturated: stop increasing
//!   (speed drops may still remove connections);
//! * throughput down ≥ 25 % → target −= 1 (never below 1);
//! * a 429/503 retry → target −= 1 and a two-window cooldown with no increases.

use std::time::Duration;

/// Minimum relative throughput gain that justifies adding a connection.
const INCREASE_RATIO: f64 = 1.08;
/// Relative throughput drop that forces giving one connection back.
const DECREASE_RATIO: f64 = 0.75;
/// Windows with < 8 % gain in a row before the plan is considered saturated.
const SATURATION_WINDOWS: u32 = 2;
/// Windows of no increases after a 429/503 retry.
const RATE_LIMIT_COOLDOWN_WINDOWS: u32 = 2;
/// The initial target never exceeds this, even for very wide plans.
const INITIAL_CAP: usize = 8;

/// Measurement window used by the transfer layer (design range: 1–1.5 s).
pub(crate) const PLANNER_WINDOW: Duration = Duration::from_millis(1200);

/// One measurement window fed to [`ConcurrencyPlanner::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowSample {
    /// Payload bytes written to the `.part` file during the window.
    pub bytes: u64,
    /// Wall-clock length of the window.
    pub interval: Duration,
    /// Chunks that still need downloading when the window ended.
    pub remaining_chunks: u64,
}

/// Pure concurrency plan state. Construct with a hard connection limit and feed
/// it windows; it only returns a value when the target actually changes.
#[derive(Debug)]
pub(crate) struct ConcurrencyPlanner {
    max: usize,
    target: usize,
    /// Throughput of the previous window, used as the comparison baseline.
    previous_bps: Option<f64>,
    /// Latched once two consecutive windows failed to improve ≥ 8 %.
    saturated: bool,
    /// Consecutive windows with less than the required improvement.
    below_improvement_windows: u32,
    /// Windows left in the 429/503 "no increase" cooldown.
    cooldown_windows: u32,
}

impl ConcurrencyPlanner {
    /// Maximum number of connections this plan may ever use.
    pub fn new(max: usize) -> Self {
        let max = max.max(1);
        Self {
            max,
            target: initial_target(max),
            previous_bps: None,
            saturated: false,
            below_improvement_windows: 0,
            cooldown_windows: 0,
        }
    }

    /// Current planned connection count.
    pub fn target(&self) -> usize {
        self.target
    }

    /// Hard connection limit.
    #[cfg(test)]
    pub fn max(&self) -> usize {
        self.max
    }

    /// Consume one measurement window. Returns `Some(new_target)` exactly when
    /// the plan changed, so the caller can emit one event per change.
    pub fn observe(&mut self, sample: WindowSample) -> Option<usize> {
        let seconds = sample.interval.as_secs_f64();
        if sample.bytes == 0 || seconds <= 0.0 || sample.remaining_chunks == 0 {
            // A stalled window (retry backoff, pause) is not evidence of
            // saturation and must not move the target in either direction.
            return None;
        }

        let bps = sample.bytes as f64 / seconds;
        let previous = self.previous_bps.replace(bps);
        let Some(previous) = previous.filter(|value| *value > 0.0) else {
            // The first measured window only establishes the comparison
            // baseline: there is nothing to compare it against yet.
            return None;
        };

        let ratio = bps / previous;
        let cooling_down = self.cooldown_windows > 0;
        if cooling_down {
            self.cooldown_windows -= 1;
        }

        if ratio <= DECREASE_RATIO {
            // Throughput collapsed: give a connection back and allow the plan
            // to grow again once the speed recovers.
            self.below_improvement_windows = 0;
            let next = self.target.saturating_sub(1).max(1);
            return self.set_target(next);
        }
        if cooling_down {
            // 429/503 cooldown: decreases are allowed above, increases are not.
            return None;
        }
        if ratio >= INCREASE_RATIO {
            if self.saturated {
                return None;
            }
            self.below_improvement_windows = 0;
            let increment = (self.target / 4).max(1);
            let remaining = sample.remaining_chunks.min(self.max as u64) as usize;
            let next = (self.target + increment).min(self.max).min(remaining);
            return self.set_target(next);
        }

        // Measured but not worth an extra connection. Two such windows in a
        // row mean the current plan already saturates the link.
        self.below_improvement_windows += 1;
        if self.below_improvement_windows >= SATURATION_WINDOWS {
            self.saturated = true;
        }
        None
    }

    /// A chunk request was rejected with 429/503 and will be retried. Back off
    /// immediately (at most one connection per event) and keep the target flat
    /// for the next two measurement windows.
    pub fn note_rate_limited(&mut self) -> Option<usize> {
        self.below_improvement_windows = 0;
        self.cooldown_windows = self.cooldown_windows.max(RATE_LIMIT_COOLDOWN_WINDOWS);
        let next = self.target.saturating_sub(1).max(1);
        self.set_target(next)
    }

    fn set_target(&mut self, next: usize) -> Option<usize> {
        let next = next.clamp(1, self.max);
        if next == self.target {
            None
        } else {
            self.target = next;
            Some(next)
        }
    }
}

/// Initial number of connections for `max` allowed connections:
/// `min(max, max(2, max / 2))`, capped at 8. Starting in the middle keeps the
/// first window from wasting connections while still beating a single stream
/// immediately.
pub(crate) fn initial_target(max: usize) -> usize {
    let max = max.max(1);
    let half = (max / 2).max(2);
    half.min(max).min(INITIAL_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(bytes: u64, remaining_chunks: u64) -> WindowSample {
        WindowSample {
            bytes,
            interval: Duration::from_secs(1),
            remaining_chunks,
        }
    }

    fn observe(planner: &mut ConcurrencyPlanner, bytes: u64, remaining: u64) -> Option<usize> {
        planner.observe(window(bytes, remaining))
    }

    #[test]
    fn initial_target_is_half_capped_at_eight() {
        let cases = [
            (1usize, 1usize),
            (2, 2),
            (3, 2),
            (4, 2),
            (5, 2),
            (6, 3),
            (8, 4),
            (9, 4),
            (12, 6),
            (16, 8),
            (17, 8),
            (32, 8),
        ];
        for (max, expected) in cases {
            assert_eq!(initial_target(max), expected, "max={max}");
            let planner = ConcurrencyPlanner::new(max);
            assert_eq!(planner.target(), expected, "planner max={max}");
            assert_eq!(planner.max(), max.max(1));
        }
        // Degenerate input is clamped, never zero.
        assert_eq!(initial_target(0), 1);
    }

    #[test]
    fn first_window_only_establishes_the_baseline() {
        let mut planner = ConcurrencyPlanner::new(8); // target 4
        assert_eq!(observe(&mut planner, 1000, 32), None);
        assert_eq!(planner.target(), 4);
    }

    #[test]
    fn throughput_gain_adds_connections_until_max() {
        let mut planner = ConcurrencyPlanner::new(8);
        assert_eq!(observe(&mut planner, 1000, 32), None); // baseline
        assert_eq!(observe(&mut planner, 1200, 32), Some(5)); // +20 %
        assert_eq!(observe(&mut planner, 1500, 32), Some(6)); // +25 %
        assert_eq!(observe(&mut planner, 1900, 32), Some(7)); // +27 %
        assert_eq!(observe(&mut planner, 2400, 32), Some(8)); // +26 %
                                                              // Already at the cap: further gains do not move the target.
        assert_eq!(observe(&mut planner, 3000, 32), None);
        assert_eq!(planner.target(), 8);
    }

    #[test]
    fn two_flat_windows_saturate_the_plan() {
        let mut planner = ConcurrencyPlanner::new(8);
        assert_eq!(observe(&mut planner, 1000, 32), None);
        assert_eq!(observe(&mut planner, 1200, 32), Some(5));
        // Two consecutive windows below the 8 % improvement threshold.
        assert_eq!(observe(&mut planner, 1250, 32), None); // +4.2 %
        assert_eq!(observe(&mut planner, 1300, 32), None); // +4.0 %
                                                           // Saturated: a large gain can no longer add connections...
        assert_eq!(observe(&mut planner, 2000, 32), None);
        assert_eq!(planner.target(), 5);
        // ...but a speed collapse still gives one back.
        assert_eq!(observe(&mut planner, 1000, 32), Some(4)); // −50 %
    }

    #[test]
    fn speed_drop_removes_one_connection_per_window_floor_one() {
        let mut planner = ConcurrencyPlanner::new(8); // target 4
        assert_eq!(observe(&mut planner, 1000, 32), None);
        assert_eq!(observe(&mut planner, 700, 32), Some(3)); // −30 %
        assert_eq!(observe(&mut planner, 500, 32), Some(2)); // −28.6 %
        assert_eq!(observe(&mut planner, 300, 32), Some(1)); // −40 %
        assert_eq!(observe(&mut planner, 200, 32), None); // floor 1
        assert_eq!(planner.target(), 1);
    }

    #[test]
    fn rate_limit_backs_off_and_cools_down_for_two_windows() {
        let mut planner = ConcurrencyPlanner::new(8); // target 4
        assert_eq!(planner.note_rate_limited(), Some(3));
        assert_eq!(planner.note_rate_limited(), Some(2));
        assert_eq!(observe(&mut planner, 1000, 32), None); // baseline
                                                           // Cooldown window 1 and 2: big gains are ignored.
        assert_eq!(observe(&mut planner, 1300, 32), None);
        assert_eq!(observe(&mut planner, 1700, 32), None);
        assert_eq!(planner.target(), 2);
        // Cooldown expired: the next improvement is honoured again.
        assert_eq!(observe(&mut planner, 2300, 32), Some(3));
    }

    #[test]
    fn cooldown_still_allows_a_decrease() {
        let mut planner = ConcurrencyPlanner::new(8); // target 4
        assert_eq!(planner.note_rate_limited(), Some(3));
        assert_eq!(observe(&mut planner, 1000, 32), None); // baseline
        assert_eq!(observe(&mut planner, 600, 32), Some(2)); // −40 % in cooldown
    }

    #[test]
    fn stalled_window_keeps_target_and_baseline() {
        let mut planner = ConcurrencyPlanner::new(8); // target 4
        assert_eq!(observe(&mut planner, 1000, 32), None);
        // No bytes at all: nothing to decide.
        assert_eq!(observe(&mut planner, 0, 32), None);
        assert_eq!(planner.target(), 4);
        // The stall did not corrupt the baseline: +20 % vs. the last real
        // window still adds a connection.
        assert_eq!(observe(&mut planner, 1200, 32), Some(5));
    }

    #[test]
    fn increase_is_capped_by_remaining_chunks() {
        let mut planner = ConcurrencyPlanner::new(8); // target 4
        assert_eq!(observe(&mut planner, 1000, 32), None);
        // Only four chunks left: target may not grow past them.
        assert_eq!(observe(&mut planner, 1200, 4), None);
        assert_eq!(planner.target(), 4);
        // A window with no chunks left never adjusts anything.
        assert_eq!(observe(&mut planner, 1200, 0), None);
        // Once more chunks appear (nothing consumed this window), grow again.
        assert_eq!(observe(&mut planner, 1800, 32), Some(5));
    }

    #[test]
    fn tiny_plans_never_exceed_their_limit() {
        let mut single = ConcurrencyPlanner::new(1);
        assert_eq!(single.target(), 1);
        assert_eq!(observe(&mut single, 1000, 8), None);
        assert_eq!(observe(&mut single, 5000, 8), None, "max=1 cannot grow");
        assert_eq!(single.target(), 1);

        let mut pair = ConcurrencyPlanner::new(2);
        assert_eq!(pair.target(), 2);
        assert_eq!(observe(&mut pair, 1000, 8), None);
        assert_eq!(observe(&mut pair, 5000, 8), None, "max=2 cannot grow");
        assert_eq!(pair.note_rate_limited(), Some(1));
        assert_eq!(pair.note_rate_limited(), None, "already at the floor");
    }

    #[test]
    fn target_never_drops_below_one_on_rate_limit() {
        let mut planner = ConcurrencyPlanner::new(1);
        assert_eq!(planner.note_rate_limited(), None);
        assert_eq!(planner.target(), 1);
    }
}
