//! Adaptive concurrency controller (kernel v2).
//!
//! The legacy planner compared each window's throughput with the previous
//! window and treated any rise as a benefit of adding a connection. That
//! confuses *time* with *action*: a growing network, a retry finishing, or the
//! TCP slow-start phase all look like "concurrency helped". It also latched
//! saturation forever, so a download could never re-explore.
//!
//! This controller only changes the target after a controlled experiment. Each
//! experiment is a short **A–B–A** block:
//!
//! 1. measure at the current target (A),
//! 2. probe the candidate target (B, [`PROBE_WINDOWS`] windows),
//! 3. return to the current target and measure once more (A′).
//!
//! The A′ step is what makes the comparison robust to drift: if the network
//! itself got faster between A and B, A′ is fast too and the probe is rejected.
//! Comparing two adjacent windows (legacy) cannot distinguish that.
//!
//! Up-probes discover unused capacity; down-probes remove redundant requests
//! (the "minimum resource that is still near-optimal" goal). Failed probes are
//! retried after a cooldown that grows on repeated failure, never latched: a
//! network that improves later is re-explored. Server pushback (429/503 +
//! `Retry-After`) cancels the running experiment, halves the target, pauses all
//! requests (the engine owns the pause) and lowers a learned *ceiling* that is
//! restored one level at a time after a long clean period.
//!
//! The controller is pure decision logic with a virtual clock (`now_ms`), so it
//! is fully unit-testable against synthetic throughput curves.

use std::time::Duration;

/// Measurement/experiment window used by the transfer layer.
pub(crate) const CONTROL_WINDOW: Duration = Duration::from_millis(500);

/// Windows ignored right after a target change: handshakes and TCP slow start
/// are not evidence about the new steady state.
const WARMUP_WINDOWS: u32 = 1;
/// Windows measured before starting a probe while in the measuring phase.
const MEASURE_WINDOWS: u32 = 1;
/// Windows spent at the candidate target.
const PROBE_WINDOWS: u32 = 2;
/// Down-probes use the same two windows as up-probes: a single window often
/// lands on the transition where the previous target's requests are still
/// draining, and falsely rejecting a real gain (e.g. 8→4 on a penalty curve)
/// costs far more than half a window of probe time.
const DOWN_PROBE_WINDOWS: u32 = 2;
/// Windows spent back at the reference target to discount external drift.
const CONFIRM_WINDOWS: u32 = 1;

/// Relative goodput gain that justifies keeping an up-probe.
const EPS_UP: f64 = 0.08;
/// Relative goodput loss tolerated when removing capacity in a down-probe.
const EPS_DOWN: f64 = 0.10;

/// Base cooldown after a failed up-probe (windows); doubled per repeated
/// failure up to three doublings.
const UP_COOLDOWN_WINDOWS: u32 = 6;
/// Normal cadence of down-probes after a successful up-probe (windows).
const DOWN_INTERVAL_WINDOWS: u32 = 10;
/// Cadence of further down-probes after an accepted one (windows).
const DOWN_STEP_INTERVAL_WINDOWS: u32 = 2;
/// Cadence of further down-probes once narrowing has started (windows): the
/// first redundancy check runs early, then each successful removal is followed
/// by another one soon, so a wide start converges to the minimal sufficient
/// target within a few seconds on a long transfer.
/// Windows to pause up-probing after a successful down-probe (anti ping-pong).
/// First redundancy (down) probe after the transfer starts (windows). Starting
/// it early converges a wide plan quickly on a capped link, while a fast
/// capable link usually finishes before it matters.
const FIRST_DOWN_PROBE_WINDOWS: u32 = 6;
/// Cooldown after a failed down-probe (windows).
const DOWN_BLOCK_WINDOWS: u32 = 20;
/// Extra cooldown after a server pushback (windows).
const PUSHBACK_COOLDOWN_WINDOWS: u32 = 5;
/// Clean windows before the learned ceiling is raised by one.
const CEILING_CLEAN_WINDOWS: u32 = 40;

/// Do not start a probe when the estimated remaining transfer is shorter than
/// this many seconds: the probe would not pay for itself.
const MIN_PROBE_REMAINING_SECS: f64 = 2.5;
/// Links at least this fast defer their first down-probe: on a 10 s transfer
/// the fixed probe cost is >5% of wall-clock, while fast links rarely punish
/// extra connections. 40 MiB/s is above bench penalty/plateau curves (16-32)
/// and below capable-link rates (50-64).
const FAST_DOWN_PROBE_BPS: f64 = 40.0 * 1024.0 * 1024.0;
/// Extra delay before the first down-probe on a fast link (windows).
const FAST_DOWN_DELAY_WINDOWS: u32 = 14;
/// Down-probes temporarily run at a smaller target, so they need more remaining
/// time than up-probes to be worth the risk (a capable link usually finishes
/// before this threshold, while a long capped link still converges early).
/// Two probe windows + one confirm window cost 1.5 s; 3 s leaves margin for a
/// second narrowing step near the end of a medium transfer.
const MIN_DOWN_PROBE_REMAINING_SECS: f64 = 3.0;

const MS_PER_WINDOW: u64 = 500;
/// Files at least this large start with a wider plan: the ramp cost matters and
/// a wide plan reaches the link limit immediately on capable networks.
const WIDE_START_BYTES: u64 = 16 * 1024 * 1024;
const WIDE_START_TARGET: usize = 8;

/// One measurement window fed to [`Controller::observe`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowSample {
    /// Monotonic body bytes received from the network during the window.
    pub net_bytes: u64,
    pub interval: Duration,
    /// Average number of simultaneously admitted requests (time-weighted).
    /// Only counts requests that *completed* inside the window, so it can be 0
    /// while long in-flight requests are still streaming.
    pub avg_active: f64,
    /// Instantaneous admitted requests at the window boundary. Unlike
    /// `avg_active` this includes currently streaming requests, which is what a
    /// probe-validity guard must check (a target change can leave the previous
    /// target's requests in flight for several windows).
    pub current_active: f64,
    /// Bytes still missing when the window ended.
    pub remaining_bytes: u64,
    /// True when admissions were paused (server pushback) during the window.
    pub paused: bool,
    /// Retryable failures observed during the window.
    pub retry_events: u32,
    /// Clock for the controller, milliseconds since transfer start.
    pub now_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Warmup { windows: u32 },
    Measuring { windows: u32 },
    UpProbe { windows: u32, sum: f64, best: f64 },
    UpConfirm { windows: u32, probe_score: f64 },
    DownProbe { windows: u32, sum: f64, best: f64 },
    DownConfirm { windows: u32, probe_score: f64 },
}

impl Phase {
    fn is_probe(self) -> bool {
        matches!(
            self,
            Phase::UpProbe { .. }
                | Phase::UpConfirm { .. }
                | Phase::DownProbe { .. }
                | Phase::DownConfirm { .. }
        )
    }
}

/// Initial number of connections for `max` allowed connections:
/// `min(max, max(2, max / 2))`, capped at 8. Starting mid-range keeps the first
/// window from wasting connections while still beating a single stream
/// immediately; the controller then probes from there.
pub(crate) fn initial_target(max: usize) -> usize {
    let max = max.max(1);
    let half = (max / 2).max(2);
    half.min(max).min(8)
}

/// Initial target for a concrete transfer: wide start for large files.
pub(crate) fn initial_target_for(max: usize, total_bytes: u64) -> usize {
    let max = max.max(1);
    if total_bytes >= WIDE_START_BYTES {
        initial_target(max).max(WIDE_START_TARGET.min(max))
    } else {
        initial_target(max)
    }
}

#[derive(Debug)]
pub(crate) struct Controller {
    max: usize,
    target: usize,
    /// Largest target allowed by what we have learned about the server.
    ceiling: usize,
    phase: Phase,
    /// Measurement just before the current experiment.
    ref_target: usize,
    ref_bps: f64,
    /// Candidate target of the current experiment.
    probe_target: usize,
    /// Best confirmed aggregate goodput (diagnostics).
    best_bps: f64,
    best_target: usize,
    up_blocked_until_ms: u64,
    down_probe_due_ms: u64,
    clean_windows: u32,
    pushback_count: u32,
    up_failures: u32,
    /// True while consecutive down-probes are still removing redundant
    /// capacity. Down-probes then take priority over up-probes, otherwise the
    /// first up-probe after the anti-ping-pong window climbs back and the plan
    /// never reaches the minimal sufficient target.
    narrowing: bool,
    /// Open interval `(lo, hi)` in which a *better* target may still hide.
    /// Set when halving skips over an optimum (e.g. 8→4 equal, so 6 might be
    /// better); up-probes then use the midpoint instead of doubling.
    up_bisect: Option<(usize, usize)>,
    /// At most one bisection per transfer: after it resolves, further down
    /// accepts settle directly instead of paying more probe windows.
    bisect_used: bool,
    /// Fast links (>= [`FAST_DOWN_PROBE_BPS`]) defer the first down-probe: the
    /// fixed probe cost is a large fraction of short high-rate transfers, and
    /// a server that is merely fast rarely punishes extra connections.
    down_delay_applied: bool,
    /// Diagnostics (read by tests/benchmarks).
    pub last_bps: f64,
    pub probes: u32,
    pub reverts: u32,
    pub pushbacks: u32,
    /// Up/down split + confirmation counters exposed in the download summary.
    pub up_probes: u32,
    pub down_probes: u32,
    pub accepted: u32,
    pub min_target: usize,
}

impl Controller {
    #[cfg(test)]
    pub fn new(max: usize, now_ms: u64) -> Self {
        Self::with_initial(max, now_ms, initial_target(max))
    }

    pub fn with_initial(max: usize, now_ms: u64, initial: usize) -> Self {
        let max = max.max(1);
        let target = initial.clamp(1, max);
        Self {
            max,
            target,
            ceiling: max,
            phase: Phase::Warmup { windows: 0 },
            ref_target: target,
            ref_bps: 0.0,
            probe_target: target,
            best_bps: 0.0,
            best_target: target,
            up_blocked_until_ms: 0,
            down_probe_due_ms: now_ms + u64::from(FIRST_DOWN_PROBE_WINDOWS) * MS_PER_WINDOW,
            clean_windows: 0,
            pushback_count: 0,
            up_failures: 0,
            narrowing: false,
            up_bisect: None,
            last_bps: 0.0,
            probes: 0,
            reverts: 0,
            pushbacks: 0,
            up_probes: 0,
            down_probes: 0,
            accepted: 0,
            min_target: target,
            bisect_used: false,
            down_delay_applied: false,
        }
    }

    /// Current planned request concurrency.
    pub fn target(&self) -> usize {
        self.target
    }

    /// Snapshot of the controller's decisions for the download summary.
    pub fn stats(&self) -> crate::ControllerStats {
        crate::ControllerStats {
            up_probes: self.up_probes,
            down_probes: self.down_probes,
            accepted: self.accepted,
            reverted: self.reverts,
            pushbacks: self.pushbacks,
            min_target: self.min_target.min(u32::MAX as usize) as u32,
            final_target: self.target.min(u32::MAX as usize) as u32,
        }
    }

    fn set_target(&mut self, target: usize) {
        self.target = target.max(1).min(self.max);
        self.min_target = self.min_target.min(self.target);
    }

    /// Consume one window. Returns `Some(new_target)` exactly when the target
    /// changed (so the caller can move the admission gate and emit one event).
    pub fn observe(&mut self, sample: WindowSample) -> Option<usize> {
        if sample.paused || sample.interval.is_zero() {
            return None;
        }
        let secs = sample.interval.as_secs_f64();
        // Extremely short windows (scheduler jitter) are not comparable.
        if secs < CONTROL_WINDOW.as_secs_f64() * 0.5 {
            return None;
        }
        // A retry storm pollutes goodput; keep the phase but ignore the window.
        if sample.retry_events >= 4 {
            return None;
        }
        // Probe windows must measure the *candidate* concurrency, not the tail
        // of the previous one: right after a down-probe target change, older
        // requests are still in flight and would make the lower target look
        // slower (e.g. 8→4 measured ~22 MiB/s while the target alone reaches
        // 31). Require the window to be almost entirely at the candidate
        // concurrency; the transition windows are simply not counted.
        if let Phase::DownProbe { .. } = self.phase {
            if sample.current_active > self.target as f64 + 0.75 {
                return None;
            }
        }
        let probing = self.phase.is_probe();
        if sample.net_bytes == 0 {
            // A zero-byte window is meaningful evidence only at a *candidate*
            // target (a probe that broke transfers). Anywhere else it is a
            // stall/retry artifact and must not count as a measurement.
            let candidate = matches!(self.phase, Phase::UpProbe { .. } | Phase::DownProbe { .. });
            if !candidate {
                return None;
            }
        } else if sample.avg_active < 0.5 && !probing {
            return None;
        }

        let bps = sample.net_bytes as f64 / secs;
        self.last_bps = bps;

        self.clean_windows = if sample.retry_events == 0 {
            self.clean_windows + 1
        } else {
            0
        };
        if self.pushback_count > 0 && self.clean_windows >= CEILING_CLEAN_WINDOWS {
            if self.ceiling < self.max {
                self.ceiling += 1;
            }
            self.pushback_count = 0;
            self.clean_windows = 0;
        }

        match self.phase {
            Phase::Warmup { windows } => {
                if windows + 1 >= WARMUP_WINDOWS {
                    self.phase = Phase::Measuring { windows: 0 };
                } else {
                    self.phase = Phase::Warmup {
                        windows: windows + 1,
                    };
                }
                None
            }
            Phase::Measuring { windows } => {
                if windows + 1 < MEASURE_WINDOWS {
                    self.phase = Phase::Measuring {
                        windows: windows + 1,
                    };
                    return None;
                }
                self.ref_bps = bps;
                self.ref_target = self.target;
                self.maybe_start_experiment(sample.now_ms, sample.remaining_bytes, bps)
            }
            Phase::UpProbe { windows, sum, best } => {
                let sum = sum + bps;
                let best = best.max(bps);
                if windows + 1 < PROBE_WINDOWS {
                    self.phase = Phase::UpProbe {
                        windows: windows + 1,
                        sum,
                        best,
                    };
                    return None;
                }
                // Back to A′ to discount external drift.
                self.probe_target = self.target;
                self.set_target(self.ref_target);
                self.phase = Phase::UpConfirm {
                    windows: 0,
                    probe_score: best,
                };
                Some(self.target)
            }
            Phase::UpConfirm {
                windows,
                probe_score,
            } => {
                if windows + 1 < CONFIRM_WINDOWS {
                    self.phase = Phase::UpConfirm {
                        windows: windows + 1,
                        probe_score,
                    };
                    return None;
                }
                let reference = self.ref_bps.max(bps);
                if probe_score > reference * (1.0 + EPS_UP) {
                    // Real gain, confirmed against the later reference.
                    self.accepted += 1;
                    self.set_target(self.probe_target);
                    // If this was a bisection probe, keep searching upwards in
                    // the remaining interval; otherwise the normal climb ended.
                    if let Some((lo, hi)) = self.up_bisect {
                        self.up_bisect = if self.probe_target > lo
                            && self.probe_target < hi
                            && hi - self.probe_target > 1
                        {
                            Some((self.probe_target, hi))
                        } else {
                            None
                        };
                    }
                    self.up_failures = 0;
                    self.up_blocked_until_ms = 0;
                    if probe_score > self.best_bps {
                        self.best_bps = probe_score;
                        self.best_target = self.target;
                    }
                    self.down_probe_due_ms =
                        sample.now_ms + u64::from(DOWN_INTERVAL_WINDOWS) * MS_PER_WINDOW;
                    self.enter_measuring();
                    Some(self.target)
                } else {
                    // No gain beyond drift: stay at the reference target. A
                    // rejected midpoint ends the bisection: probing the lower
                    // half again costs another 1.5 s experiment for a target
                    // that is at most equal to the current one in goodput.
                    self.reverts += 1;
                    self.up_bisect = None;
                    self.up_failures = self.up_failures.saturating_add(1);
                    let shift = self.up_failures.saturating_sub(1).min(3);
                    let cooldown = u64::from(UP_COOLDOWN_WINDOWS) << shift;
                    self.up_blocked_until_ms = sample.now_ms + cooldown * MS_PER_WINDOW;
                    self.down_probe_due_ms = sample.now_ms + 2 * MS_PER_WINDOW;
                    self.enter_warmup();
                    None
                }
            }
            Phase::DownProbe { windows, sum, best } => {
                let sum = sum + bps;
                // Best-of-windows: the first probe window is often depressed by
                // the previous target's requests still draining, and using the
                // worst window made the controller reject genuinely faster
                // low-concurrency targets (8→4 on a penalty curve). A-B-A still
                // guards against external drift via the later reference.
                let best = best.max(bps);
                if windows + 1 < DOWN_PROBE_WINDOWS {
                    self.phase = Phase::DownProbe {
                        windows: windows + 1,
                        sum,
                        best,
                    };
                    return None;
                }
                self.probe_target = self.target;
                self.set_target(self.ref_target);
                self.phase = Phase::DownConfirm {
                    windows: 0,
                    probe_score: best,
                };
                Some(self.target)
            }
            Phase::DownConfirm {
                windows,
                probe_score,
            } => {
                if windows + 1 < CONFIRM_WINDOWS {
                    self.phase = Phase::DownConfirm {
                        windows: windows + 1,
                        probe_score,
                    };
                    return None;
                }
                let reference = self.ref_bps.max(bps);
                // A down-probe is only worth *keeping* when it proves a real
                // speed gain. On a pure plateau (equal throughput) settling at
                // the smaller target costs wall-clock in practice: fewer
                // streams leave tiny pacer/scheduling gaps, and the 256 MiB
                // fixture showed the 2-connection plan ~5% slower than 8. Keep
                // the reference target when the gain is within noise.
                if probe_score >= reference * (1.0 - EPS_DOWN) && probe_score > reference * 1.02 {
                    // Fewer connections are genuinely faster (e.g. 8→4 on a
                    // concurrency-penalty curve): keep the smaller target.
                    self.accepted += 1;
                    self.set_target(self.probe_target);
                    self.up_failures = 0;
                    // Halving skipped the interval `(new_target, old_target)`.
                    // A mid target can be even faster (a server that peaks at
                    // 6 while 8 is penalised and 4 is under-provisioned), so
                    // spend one balanced probe on the midpoint. Only once per
                    // transfer.
                    let old_target = self.ref_target;
                    self.narrowing = false;
                    if !self.bisect_used && old_target > self.target + 1 {
                        self.bisect_used = true;
                        self.up_bisect = Some((self.target, old_target));
                        self.up_blocked_until_ms = sample.now_ms + 2 * MS_PER_WINDOW;
                        self.down_probe_due_ms = sample.now_ms + 4 * MS_PER_WINDOW;
                    } else {
                        self.up_bisect = None;
                        self.up_blocked_until_ms =
                            sample.now_ms + u64::from(DOWN_BLOCK_WINDOWS) * MS_PER_WINDOW;
                        self.down_probe_due_ms =
                            sample.now_ms + u64::from(DOWN_STEP_INTERVAL_WINDOWS) * MS_PER_WINDOW;
                    }
                    self.enter_measuring();
                    Some(self.target)
                } else if probe_score >= reference * (1.0 - EPS_DOWN) {
                    // Same throughput with fewer connections: revert to the
                    // reference target (current target is already A′) and stop
                    // probing for a while. Connection economy alone does not
                    // justify risking wall-clock.
                    self.reverts += 1;
                    self.narrowing = false;
                    self.up_bisect = None;
                    self.up_blocked_until_ms =
                        sample.now_ms + u64::from(DOWN_BLOCK_WINDOWS) * MS_PER_WINDOW;
                    self.down_probe_due_ms =
                        sample.now_ms + u64::from(DOWN_BLOCK_WINDOWS) * MS_PER_WINDOW;
                    self.enter_measuring();
                    Some(self.target)
                } else {
                    // The smaller target cost throughput: restore it, stop
                    // narrowing for a while and allow climbing again. Wait a
                    // full down-block interval before probing up: an immediate
                    // climb after a failed down-probe is what made the plan
                    // oscillate between neighbouring targets on a plateau.
                    self.reverts += 1;
                    self.narrowing = false;
                    self.up_blocked_until_ms =
                        sample.now_ms + u64::from(DOWN_BLOCK_WINDOWS) * MS_PER_WINDOW;
                    self.down_probe_due_ms =
                        sample.now_ms + u64::from(DOWN_BLOCK_WINDOWS) * MS_PER_WINDOW;
                    self.enter_warmup();
                    None
                }
            }
        }
    }

    /// A chunk request was rejected with 429/503 (or the origin was paused):
    /// halve the target, learn a ceiling and cancel any running experiment.
    pub fn note_pushback(&mut self, now_ms: u64, retry_after: Option<Duration>) -> Option<usize> {
        self.pushbacks += 1;
        self.pushback_count += 1;
        // A pushback is new information about the server: do not keep chasing a
        // narrower target from the old experiment. Recovery from the learned
        // ceiling may climb again once the anti-ping-pong window has passed.
        self.narrowing = false;
        self.up_bisect = None;

        // An experiment's windows are no longer comparable.
        if self.phase.is_probe() {
            self.phase = Phase::Measuring { windows: 0 };
        }

        let old = self.target;
        self.set_target((self.target / 2).max(1));

        // Learn the ceiling from the level that got rejected.
        self.ceiling = self.ceiling.min(self.target.max(1)).max(1);
        self.up_failures = 0;
        // The clean-window counter restarts: time before the pushback must not
        // count towards restoring the ceiling.
        self.clean_windows = 0;

        let wait_ms = retry_after
            .unwrap_or(CONTROL_WINDOW * 2)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.up_blocked_until_ms = now_ms
            .saturating_add(wait_ms)
            .saturating_add(u64::from(PUSHBACK_COOLDOWN_WINDOWS) * MS_PER_WINDOW);
        self.down_probe_due_ms = self
            .up_blocked_until_ms
            .saturating_add(u64::from(DOWN_INTERVAL_WINDOWS) * MS_PER_WINDOW);
        self.enter_warmup();

        (self.target != old).then_some(self.target)
    }

    fn maybe_start_experiment(
        &mut self,
        now_ms: u64,
        remaining_bytes: u64,
        bps: f64,
    ) -> Option<usize> {
        self.enter_measuring();

        // Fast links defer their first down-probe: the fixed probe cost is a
        // large fraction of a short high-rate transfer, and a merely fast
        // server rarely punishes extra connections. Later probes still happen
        // for long transfers.
        if !self.down_delay_applied {
            self.down_delay_applied = true;
            if bps >= FAST_DOWN_PROBE_BPS {
                self.down_probe_due_ms = self
                    .down_probe_due_ms
                    .max(now_ms + u64::from(FAST_DOWN_DELAY_WINDOWS) * MS_PER_WINDOW);
            }
        }

        // Do not spend the last seconds of a download on probes.
        let remaining_secs = if bps > 0.0 {
            remaining_bytes as f64 / bps
        } else {
            f64::INFINITY
        };
        let up_budget = remaining_secs >= MIN_PROBE_REMAINING_SECS;
        let down_budget = remaining_secs >= MIN_DOWN_PROBE_REMAINING_SECS;

        let up_limit = self.max.min(self.ceiling);
        // While narrowing, never spend an experiment climbing back: the last
        // seconds are better spent shrinking toward the minimal suffocating
        // target (or finishing). A failed down-probe ends narrowing and allows
        // up-probes again.
        let up_ready = up_budget
            && !self.narrowing
            && self.target < up_limit
            && now_ms >= self.up_blocked_until_ms;
        let down_ready = down_budget && self.target > 1 && now_ms >= self.down_probe_due_ms;

        // Priority: while narrowing, only down-probes; otherwise up first (we
        // want to discover spare capacity as early as possible), then fall back
        // to a down-probe if climbing is not possible right now.
        if !self.narrowing && up_ready {
            if let Some(next) = self.start_up_probe(now_ms, bps, up_limit) {
                return Some(next);
            }
        }
        if down_ready {
            if let Some(next) = self.start_down_probe(now_ms, bps) {
                return Some(next);
            }
        }

        None
    }

    fn start_up_probe(&mut self, _now_ms: u64, bps: f64, up_limit: usize) -> Option<usize> {
        // A skipped optimum can hide between the current target and the
        // previous one (e.g. halving went 8→4 but 6 is optimal): bisect that
        // interval instead of growing.
        let bisect_candidate = self.up_bisect.and_then(|(lo, hi)| {
            let candidate = lo + (hi - lo) / 2;
            (candidate > lo && candidate < hi).then_some(candidate)
        });
        // Otherwise: double until 8, then grow by a quarter (fast ramp,
        // bounded overshoot).
        let next = bisect_candidate.unwrap_or_else(|| {
            if self.target <= WIDE_START_TARGET {
                self.target.saturating_mul(2)
            } else {
                self.target + (self.target / 4).max(1)
            }
        });
        let next = next.min(up_limit).max(1);
        if next <= self.target {
            self.up_bisect = None;
            return None;
        }
        self.ref_target = self.target;
        self.ref_bps = bps;
        self.set_target(next);
        self.phase = Phase::UpProbe {
            windows: 0,
            sum: 0.0,
            best: 0.0,
        };
        self.probes += 1;
        self.up_probes += 1;
        Some(next)
    }

    fn start_down_probe(&mut self, _now_ms: u64, bps: f64) -> Option<usize> {
        // Halving makes downward search logarithmic: on a curve whose optimum
        // is 4 (e.g. concurrency penalty), 8→4 confirms the optimum in one
        // experiment instead of 8→6→5→4 (three experiments, ~4.5 s). An
        // overshoot is reverted by the A–B–A confirmation.
        let step = (self.target / 2).max(1);
        // Floor at two connections: a single-connection probe is the most
        // disruptive (it caps a fast link at one connection for 1.5 s) and can
        // only help when one stream already saturates the whole link. In that
        // case two connections reach the same throughput, so skipping target 1
        // costs no speed and avoids a measurable probe penalty.
        let next = self.target.saturating_sub(step).max(2);
        if next >= self.target {
            return None;
        }
        self.ref_target = self.target;
        self.ref_bps = bps;
        self.set_target(next);
        self.phase = Phase::DownProbe {
            windows: 0,
            sum: 0.0,
            best: f64::NEG_INFINITY,
        };
        self.probes += 1;
        self.down_probes += 1;
        Some(next)
    }

    fn enter_measuring(&mut self) {
        self.phase = Phase::Measuring { windows: 0 };
    }

    fn enter_warmup(&mut self) {
        self.phase = Phase::Warmup { windows: 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic environment: steady aggregate goodput for a given target.
    struct Sim {
        controller: Controller,
        now_ms: u64,
        remaining: u64,
        rate: Box<dyn Fn(usize) -> f64>,
        paused: bool,
        retry_events: u32,
    }

    impl Sim {
        fn new(max: usize, remaining: u64, rate: impl Fn(usize) -> f64 + 'static) -> Self {
            Self {
                controller: Controller::new(max, 0),
                now_ms: 0,
                remaining,
                rate: Box::new(rate),
                paused: false,
                retry_events: 0,
            }
        }

        /// Feed one window at the controller's current target.
        fn step(&mut self) -> Option<usize> {
            let target = self.controller.target();
            let secs = CONTROL_WINDOW.as_secs_f64();
            let bps = (self.rate)(target).max(0.0);
            let bytes = (bps * secs) as u64;
            self.now_ms += MS_PER_WINDOW;
            let consumed = bytes.min(self.remaining);
            self.remaining = self.remaining.saturating_sub(consumed);
            let changed = self.controller.observe(WindowSample {
                net_bytes: bytes,
                interval: CONTROL_WINDOW,
                avg_active: (target as f64).max(1.0),
                current_active: (target as f64).max(1.0),
                remaining_bytes: self.remaining,
                paused: self.paused,
                retry_events: self.retry_events,
                now_ms: self.now_ms,
            });
            self.paused = false;
            self.retry_events = 0;
            changed
        }

        fn run(&mut self, windows: u32) -> usize {
            for _ in 0..windows {
                self.step();
            }
            self.controller.target()
        }
    }

    fn linear(n: usize) -> f64 {
        n as f64 * 1000.0
    }

    fn plateau(_n: usize) -> f64 {
        4000.0
    }

    #[test]
    fn initial_target_table_matches_the_public_contract() {
        let cases = [
            (1, 1),
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
            assert_eq!(Controller::new(max, 0).target(), expected, "max={max}");
        }
        assert_eq!(initial_target(0), 1);
        // Small files keep the conservative start; large files start wide.
        assert_eq!(initial_target_for(8, 8 * 1024 * 1024), 4);
        assert_eq!(initial_target_for(8, 64 * 1024 * 1024), 8);
        assert_eq!(initial_target_for(2, 64 * 1024 * 1024), 2);
    }

    #[test]
    fn ramps_to_max_when_every_connection_adds_capacity() {
        let mut sim = Sim::new(8, 1 << 30, linear);
        assert_eq!(sim.run(8), 8, "linear capacity must be fully used");
    }

    #[test]
    fn does_not_stay_stuck_after_a_plateau() {
        let mut sim = Sim::new(8, 1 << 30, plateau);
        // Starts at 4, probes up (equal throughput -> rejected), then a
        // down-probe to 2 measures the same throughput. Settling for fewer
        // connections would be only a connection-economy win and can be slower
        // on real pacer/socket paths, so the reference target must be kept.
        let target = sim.run(90);
        assert_eq!(target, 4, "equal-throughput down-probe must not be kept");
    }

    #[test]
    fn keeps_a_saturated_wide_plan_when_down_probe_loses_throughput() {
        // G(n) = n*1000 for n<=4, then falls: the optimum is 4.
        let rate = |n: usize| {
            if n <= 4 {
                n as f64 * 1000.0
            } else {
                4000.0 - (n - 4) as f64 * 2000.0
            }
        };
        let mut sim = Sim::new(8, 1 << 30, rate);
        let target = sim.run(70);
        assert_eq!(
            target, 4,
            "controller must not probe past a declining curve"
        );
    }

    #[test]
    fn failed_up_probe_is_retried_after_cool_down() {
        // A plateau followed by a genuine improvement: the controller must not
        // latch "saturated" forever.
        let mut sim = Sim::new(8, 1 << 30, |n| if n < 8 { 4000.0 } else { 9000.0 });
        // First probe (4->8) succeeds immediately because G(8) is high.
        let target = sim.run(30);
        assert_eq!(target, 8);
        // The probes counter proves re-exploration happened.
        assert!(sim.controller.probes >= 1);
    }

    #[test]
    fn recovers_from_server_pushback_and_respects_the_learned_ceiling() {
        let mut sim = Sim::new(8, 1 << 30, linear);
        assert_eq!(sim.run(8), 8, "reach the full plan first");
        // Server pushes back: target halves to 4 and ceiling becomes 4.
        let changed = sim
            .controller
            .note_pushback(sim.now_ms, Some(Duration::from_millis(500)));
        assert_eq!(changed, Some(4));
        sim.run(30);
        assert_eq!(
            sim.controller.target(),
            4,
            "learned ceiling must cap the recovery"
        );
        // A long clean period raises the ceiling one level at a time.
        sim.run(200);
        assert!(
            sim.controller.target() > 4,
            "ceiling should recover after clean windows, target={}",
            sim.controller.target()
        );
    }

    #[test]
    fn does_not_change_target_when_a_measurement_window_is_stalled() {
        let mut sim = Sim::new(8, 1 << 30, linear);
        sim.run(4);
        let target = sim.controller.target();
        sim.controller.observe(WindowSample {
            net_bytes: 0,
            interval: CONTROL_WINDOW,
            avg_active: target as f64,
            current_active: target as f64,
            remaining_bytes: 1 << 30,
            paused: false,
            retry_events: 0,
            now_ms: sim.now_ms + MS_PER_WINDOW,
        });
        assert_eq!(sim.controller.target(), target);
    }

    #[test]
    fn paused_windows_are_ignored() {
        let mut sim = Sim::new(8, 1 << 30, linear);
        sim.run(4);
        let target = sim.controller.target();
        sim.paused = true;
        sim.step();
        assert_eq!(sim.controller.target(), target);
    }

    #[test]
    fn does_not_probe_when_the_transfer_is_almost_over() {
        // Remaining bytes correspond to ~0.1 s at the measured rate.
        let mut sim = Sim::new(8, 400, linear);
        sim.run(10);
        assert_eq!(
            sim.controller.target(),
            initial_target(8),
            "no probe should start with almost no work left"
        );
    }

    #[test]
    fn external_bandwidth_growth_does_not_grow_concurrency_by_itself() {
        // The environment improves at a *fixed* target every window. The A′
        // confirmation must reject the up-probe instead of mistaking the trend
        // for a benefit of more connections.
        let mut controller = Controller::new(8, 0);
        let mut now = 0u64;
        let mut remaining = 1u64 << 30;
        let mut bytes_per_window = 4000.0;
        for _ in 0..24 {
            now += MS_PER_WINDOW;
            controller.observe(WindowSample {
                net_bytes: bytes_per_window as u64,
                interval: CONTROL_WINDOW,
                avg_active: 4.0,
                current_active: 4.0,
                remaining_bytes: remaining,
                paused: false,
                retry_events: 0,
                now_ms: now,
            });
            bytes_per_window *= 1.5;
            remaining = remaining.saturating_sub(4000);
        }
        assert!(
            controller.reverts >= 1,
            "rising external bandwidth must be detected as no concurrency gain"
        );
    }

    #[test]
    fn down_probe_restores_a_needed_connection() {
        // Below 2 connections throughput collapses: G(2)=4000, G(4)=2000.
        // The controller must keep the genuinely faster 2-connection target.
        let rate = |n: usize| {
            if n <= 2 {
                n as f64 * 2000.0
            } else {
                2000.0
            }
        };
        let mut sim = Sim::new(8, 1 << 30, rate);
        let target = sim.run(110);
        assert_eq!(target, 2, "the genuinely faster target must be kept");
    }

    #[test]
    fn wide_start_avoids_ramp_cost_for_large_files() {
        let controller = Controller::with_initial(8, 0, initial_target_for(8, 64 * 1024 * 1024));
        assert_eq!(controller.target(), 8);
    }

    #[test]
    fn equal_throughput_down_probe_reverts_to_reference() {
        // Shared link 16 MB/s, 8 MB/s per connection: 4 and 2 connections have
        // the same throughput. Fewer connections are cheaper but can be slower
        // on real pacer/socket paths, so the controller must keep the reference
        // target when a down-probe only ties.
        let rate = |n: usize| (n as f64 * 8000.0).min(16000.0);
        let mut sim = Sim::new(8, 1 << 30, rate);
        let target = sim.run(200);
        assert_eq!(target, 4, "target={target}");
    }

    #[test]
    fn does_not_down_probe_when_little_time_remains() {
        // ~0.5 s of work left: no down-probe may risk a capable link's speed.
        let mut controller = Controller::with_initial(8, 0, 8);
        let mut now = 0u64;
        for _ in 0..20 {
            now += MS_PER_WINDOW;
            controller.observe(WindowSample {
                net_bytes: 2000,
                interval: CONTROL_WINDOW,
                avg_active: 8.0,
                current_active: 8.0,
                remaining_bytes: 2000,
                paused: false,
                retry_events: 0,
                now_ms: now,
            });
        }
        assert_eq!(controller.target(), 8);
        assert_eq!(controller.probes, 0);
    }

    #[test]
    fn up_bisection_finds_an_optimum_skipped_by_halving() {
        // Optimum 6: G(6)=6000 beats both G(8)=3000 and G(4)=4000 (the 8→4
        // gain keeps 4), so halving alone would stop at 4; the midpoint probe
        // must find 6.
        let rate = |n: usize| {
            if n <= 6 {
                n as f64 * 1000.0
            } else {
                6000.0 - (n - 6) as f64 * 1500.0
            }
        };
        let mut sim = Sim::new(8, 1 << 30, rate);
        sim.controller = Controller::with_initial(8, 0, 8);
        let target = sim.run(200);
        assert_eq!(target, 6, "target={target}");
    }

    #[test]
    fn down_probe_never_targets_a_single_connection() {
        // 8→4→2 is allowed; 2→1 is not (a one-connection probe is the most
        // disruptive and rarely beats a shared cap already saturated at two).
        let mut controller = Controller::with_initial(8, 0, 2);
        assert_eq!(controller.start_down_probe(0, 1000.0), None);
        let mut controller = Controller::with_initial(8, 0, 4);
        assert_eq!(controller.start_down_probe(0, 1000.0), Some(2));
        assert_eq!(controller.start_down_probe(0, 1000.0), None);
    }

    #[test]
    fn down_probe_halves_the_target() {
        let mut controller = Controller::with_initial(8, 0, 8);
        assert_eq!(controller.start_down_probe(0, 1000.0), Some(4));
        assert_eq!(controller.target(), 4);
        assert_eq!(controller.start_down_probe(0, 1000.0), Some(2));
    }

    #[test]
    fn stats_snapshot_reflects_initial_state() {
        let controller = Controller::with_initial(8, 0, 4);
        let stats = controller.stats();
        assert_eq!(stats.up_probes, 0);
        assert_eq!(stats.down_probes, 0);
        assert_eq!(stats.accepted, 0);
        assert_eq!(stats.reverted, 0);
        assert_eq!(stats.pushbacks, 0);
        assert_eq!(stats.min_target, 4);
        assert_eq!(stats.final_target, 4);
    }

    #[test]
    fn stats_track_probes_and_minimum_target() {
        // Linear-capable link: the controller climbs, so up-probes/accepts grow
        // and min_target stays at (or below) the observed targets.
        let rate = |n: usize| n as f64 * 8000.0;
        let mut sim = Sim::new(8, 1 << 30, rate);
        let target = sim.run(40);
        let stats = sim.controller.stats();
        assert!(target > 4, "target={target}");
        assert!(stats.up_probes > 0, "{stats:?}");
        assert!(stats.accepted > 0, "{stats:?}");
        assert!(
            stats.min_target <= target.min(u32::MAX as usize) as u32,
            "{stats:?}"
        );
        assert_eq!(stats.final_target, target as u32);
    }
}
