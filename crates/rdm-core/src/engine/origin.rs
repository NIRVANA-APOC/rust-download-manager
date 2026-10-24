//! Process-wide per-origin concurrency budget.
//!
//! A single download plans its own connection count, but a desktop app usually
//! runs several downloads against the same host at once (and the batch runner
//! may run more). Independently planned plans add up, overshoot the server's
//! concurrency limit, and turn a fast link into a 429/backoff storm.
//!
//! This registry learns one cap per origin from pushbacks and shares the
//! `Retry-After` pause. Every chunk gate clamps its planned target to the
//! learned cap, so later tasks start below the limit instead of re-learning it.
//! The cap recovers one slot at a time after a quiet interval.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A learned cap is raised by one slot after this long without pushback.
const RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
/// Never recover above this many in-flight requests per origin.
const MAX_CAP: usize = 32;

#[derive(Debug, Default)]
struct OriginState {
    /// Learned maximum number of concurrent requests; `None` until the first
    /// pushback (unknown origins are unconstrained by this layer).
    cap: Option<usize>,
    /// Process-wide `Retry-After` deadline shared by every task on the origin.
    pause_until: Option<Instant>,
    last_pushback: Option<Instant>,
}

impl OriginState {
    fn recover(&mut self, now: Instant) {
        let (Some(cap), Some(last)) = (self.cap, self.last_pushback) else {
            return;
        };
        if cap >= MAX_CAP || now.saturating_duration_since(last) < RECOVERY_INTERVAL {
            return;
        }
        self.cap = Some(cap + 1);
        // Stepwise recovery: the next slot needs another quiet interval.
        self.last_pushback = Some(now);
    }
}

/// Shared budget for one origin (`scheme://host:port`).
#[derive(Debug, Default)]
pub(crate) struct OriginBudget {
    state: Mutex<OriginState>,
    /// Requests currently holding an origin permit across all tasks.
    in_flight: AtomicUsize,
}

/// RAII permit for one in-flight request against an origin.
#[derive(Debug)]
pub(crate) struct OriginGuard {
    budget: Arc<OriginBudget>,
}

impl Drop for OriginGuard {
    fn drop(&mut self) {
        self.budget.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl OriginBudget {
    /// Current learned concurrency cap, if a pushback has been observed.
    pub(crate) fn cap(&self) -> Option<usize> {
        let mut state = self.state.lock().expect("origin budget mutex poisoned");
        state.recover(Instant::now());
        state.cap
    }

    /// Take one process-wide permit for this origin.
    ///
    /// This is the cross-task limit: the per-task gate also clamps its own plan
    /// to the cap, but only this permit bounds the *sum* of all tasks on the
    /// origin. Unlearned origins are unbounded here (per-task planning is the
    /// only limit, exactly as before this feature).
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        controls: &super::Controls,
    ) -> Option<OriginGuard> {
        loop {
            if controls.stopped() {
                return None;
            }
            let cap = self.cap().unwrap_or(usize::MAX).max(1);
            let current = self.in_flight.load(Ordering::SeqCst);
            if current < cap
                && self
                    .in_flight
                    .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return Some(OriginGuard {
                    budget: Arc::clone(self),
                });
            }
            if !controls.sleep_abortable(Duration::from_millis(10)).await {
                return None;
            }
        }
    }

    /// Remaining shared `Retry-After` pause, if any.
    pub(crate) fn pause_remaining(&self, now: Instant) -> Option<Duration> {
        let state = self.state.lock().expect("origin budget mutex poisoned");
        state
            .pause_until
            .and_then(|until| until.checked_duration_since(now))
    }

    /// Learn from a 429/503: lower the cap and share the pause. `suggested` is
    /// the caller's post-backoff plan (usually `target / 2`).
    pub(crate) fn note_pushback(
        &self,
        now: Instant,
        suggested: usize,
        retry_after: Option<Duration>,
    ) {
        let mut state = self.state.lock().expect("origin budget mutex poisoned");
        let suggested = suggested.max(1);
        state.cap = Some(match state.cap {
            Some(current) => current.min(suggested),
            None => suggested,
        });
        state.last_pushback = Some(now);
        if let Some(delay) = retry_after {
            let deadline = now + delay;
            state.pause_until = Some(match state.pause_until {
                Some(current) => current.max(deadline),
                None => deadline,
            });
        }
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<OriginBudget>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<OriginBudget>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop every learned budget. Benchmarks and deterministic localhost tests
/// create short-lived servers whose ephemeral ports can be reused, which would
/// otherwise leak a learned cap from one sample into the next.
pub(crate) fn reset_budgets() {
    registry()
        .lock()
        .expect("origin registry mutex poisoned")
        .clear();
}

/// Origin key for a URL: `scheme://host:port` (default port filled in).
pub(crate) fn origin_key(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    Some(format!("{}://{}:{}", parsed.scheme(), host, port))
}

/// Budget for the origin serving `url`. Every task on the same origin gets the
/// same object for the lifetime of the process.
pub(crate) fn budget_for(url: &str) -> Option<Arc<OriginBudget>> {
    let key = origin_key(url)?;
    let mut entries = registry().lock().expect("origin registry mutex poisoned");
    Some(Arc::clone(
        entries
            .entry(key)
            .or_insert_with(|| Arc::new(OriginBudget::default())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_key_normalises_default_ports() {
        assert_eq!(
            origin_key("https://example.com/a/b?x=1").as_deref(),
            Some("https://example.com:443")
        );
        assert_eq!(
            origin_key("http://127.0.0.1:8080/file").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(origin_key("not-a-url"), None);
    }

    #[test]
    fn pushback_lowers_the_cap_and_pauses_the_origin() {
        let budget = OriginBudget::default();
        let now = Instant::now();
        assert_eq!(budget.cap(), None);
        budget.note_pushback(now, 4, Some(Duration::from_secs(1)));
        assert_eq!(budget.cap(), Some(4));
        assert_eq!(budget.pause_remaining(now).map(|d| d.as_secs()), Some(1));
        // A smaller suggestion wins; a larger one must not undo the learning.
        budget.note_pushback(now, 2, None);
        assert_eq!(budget.cap(), Some(2));
        budget.note_pushback(now, 8, None);
        assert_eq!(budget.cap(), Some(2));
        // The shared pause never shrinks.
        assert!(budget.pause_remaining(now).is_some());
    }

    #[test]
    fn cap_recovers_one_slot_after_a_quiet_interval() {
        let start = Instant::now();
        // Directly exercise the recovery helper (Instant cannot be mocked in
        // the public path without a chrono dependency).
        let mut state = OriginState {
            cap: Some(2),
            pause_until: None,
            last_pushback: Some(start),
        };
        state.recover(start + RECOVERY_INTERVAL / 2);
        assert_eq!(state.cap, Some(2), "too early to recover");
        state.recover(start + RECOVERY_INTERVAL + Duration::from_secs(1));
        assert_eq!(state.cap, Some(3));
        state.recover(start + RECOVERY_INTERVAL * 4);
        assert_eq!(state.cap, Some(4), "each quiet interval adds one slot");
    }

    #[test]
    fn same_origin_shares_one_budget() {
        let a = budget_for("http://127.0.0.1:9/a").expect("key");
        let b = budget_for("http://127.0.0.1:9/b").expect("key");
        assert!(Arc::ptr_eq(&a, &b));
        let c = budget_for("http://127.0.0.1:10/a").expect("key");
        assert!(!Arc::ptr_eq(&a, &c));
    }

    #[tokio::test]
    async fn permits_bound_the_sum_across_concurrent_tasks() {
        use std::sync::atomic::AtomicUsize;

        use crate::engine::Controls;

        let budget = Arc::new(OriginBudget::default());
        budget.note_pushback(Instant::now(), 3, None);
        let controls = Arc::new(Controls::new(crate::DownloadHandle::new()));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let budget = Arc::clone(&budget);
            let controls = Arc::clone(&controls);
            let peak = Arc::clone(&peak);
            tasks.push(tokio::spawn(async move {
                let permit = budget
                    .acquire(&controls)
                    .await
                    .expect("permit while running");
                peak.fetch_max(
                    budget.in_flight.load(std::sync::atomic::Ordering::SeqCst),
                    std::sync::atomic::Ordering::SeqCst,
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(permit);
            }));
        }
        for task in tasks {
            task.await.expect("task");
        }
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the shared cap must bound the sum of all tasks"
        );
        assert_eq!(
            budget.in_flight.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "every permit must be returned"
        );
    }
}
