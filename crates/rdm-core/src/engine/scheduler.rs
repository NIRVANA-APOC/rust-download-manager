//! Work scheduler with in-flight tail stealing.
//!
//! The kernel downloads byte ranges, not "chunk #17": a worker owns a segment
//! `[start, end)` and may have it split when it becomes the tail. The split is
//! negotiated cooperatively:
//!
//! 1. An idle worker (`try_request_steal`) picks the segment with the largest
//!    remaining bytes and publishes a split offset in `steal_at`.
//! 2. The owner checks `steal_at` while streaming the body; it stops at that
//!    offset, reports the completed prefix and the scheduler re-queues the
//!    suffix.
//! 3. The idle worker claims the suffix (or any other pending segment).
//!
//! This turns "one slow connection holds the last 4 MiB" into "every idle
//! worker helps finish the slow tail", which is what Gopeed/axel/IDM do with
//! their own segment stealing. Splits are bounded: a segment must have at least
//! [`MIN_STEAL_REMAINING`] bytes left and neither half may fall below
//! [`MIN_STEAL_BYTES`], so the extra request overhead stays small.
//!
//! The scheduler owns no I/O and no async I/O state; it is pure bookkeeping and
//! unit-tested with synthetic claims.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

/// Only split a segment that still has at least this much left.
pub(crate) const MIN_STEAL_REMAINING: u64 = 2 * 1024 * 1024;
/// A single request may merge adjacent pending segments up to this span.
///
/// 4 MiB keeps one request short enough for the 15 s stall timeout and for the
/// controller's 500 ms windows, while cutting round trips on high-RTT links.
pub(crate) const COALESCE_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Coalescing only pays off when the transfer has enough waves to amortise it;
/// on small files the extra parallelism of per-chunk requests is more valuable
/// (and pause/resume granularity matters more).
pub(crate) const COALESCE_MIN_BYTES: u64 = 16 * 1024 * 1024;
/// The owner keeps `remaining / STEAL_FRACTION_DIVISOR` (and hands the rest
/// to the thief, which re-issues it as a fresh request).
const STEAL_FRACTION_DIVISOR: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    start: u64,
    end: u64,
}

struct Active {
    id: u64,
    start: u64,
    end: u64,
    progress: Arc<AtomicU64>,
    steal_at: Arc<AtomicU64>,
}

pub(crate) struct Scheduler {
    total: u64,
    state: Mutex<SchedState>,
    notify: Notify,
}

struct SchedState {
    pending: VecDeque<Segment>,
    active: Vec<Active>,
    done: Vec<(u64, u64)>,
    done_bytes: u64,
    next_id: u64,
}

/// A worker's ownership of one byte range.
pub(crate) struct Claim {
    pub id: u64,
    pub start: u64,
    pub end: u64,
    progress: Arc<AtomicU64>,
    steal_at: Arc<AtomicU64>,
}

impl Claim {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Bytes covered so far (reported by the owner after each flush).
    pub fn update_progress(&self, covered: u64) {
        self.progress
            .store(covered.min(self.len()), Ordering::Release);
    }

    /// Absolute offset at which the owner must stop because another worker is
    /// taking the suffix.
    pub fn steal_target(&self) -> Option<u64> {
        let value = self.steal_at.load(Ordering::Acquire);
        (value != 0).then_some(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    /// The whole claimed range is on disk.
    Finished,
    /// A prefix was completed; the suffix was re-queued for another worker.
    Split { completed: u64 },
}

impl Scheduler {
    /// Build a scheduler for `total` bytes with `missing` ranges to download.
    ///
    /// `done` seeds ranges recovered from a resume state; they count towards
    /// completion and are included in [`Scheduler::completed_ranges`] again.
    /// Long missing ranges are subdivided into `chunk_size` pieces so the
    /// initial work granularity matches the chunk plan (and v1 resume gaps stay
    /// aligned).
    pub(crate) fn new(
        total: u64,
        done: Vec<(u64, u64)>,
        missing: Vec<(u64, u64)>,
        chunk_size: u64,
    ) -> Self {
        let chunk_size = chunk_size.max(1);
        let done = crate::state::merge_ranges(done);
        let done_bytes: u64 = done.iter().map(|(start, end)| end - start).sum();
        let mut pending = VecDeque::new();
        for (start, end) in missing {
            let mut cursor = start;
            while cursor < end {
                let next = (cursor + chunk_size).min(end);
                pending.push_back(Segment {
                    start: cursor,
                    end: next,
                });
                cursor = next;
            }
        }
        Self {
            total,
            state: Mutex::new(SchedState {
                pending,
                active: Vec::new(),
                done,
                done_bytes,
                next_id: 1,
            }),
            notify: Notify::new(),
        }
    }

    /// Take the next pending segment, if any.
    #[cfg(test)]
    pub(crate) fn claim(&self) -> Option<Claim> {
        self.claim_with(usize::MAX, 0)
    }

    /// Take the next pending segment, merging adjacent pending segments into one
    /// request while that does not starve the scheduler.
    ///
    /// `keep_pending` is the number of pending segments that must remain
    /// unmerged (usually the worker count); `max_span == 0` disables merging.
    /// Merging cuts one round trip per absorbed segment, which matters most on
    /// high-RTT links, while the unmerged tail keeps stealing/resume granularity.
    pub(crate) fn claim_with(&self, keep_pending: usize, max_span: u64) -> Option<Claim> {
        let mut state = self.state.lock().expect("scheduler mutex poisoned");
        let mut segment = state.pending.pop_front()?;
        while max_span > 0 && state.pending.len() > keep_pending {
            let Some(next) = state.pending.front() else {
                break;
            };
            let span = segment.end - segment.start;
            let next_len = next.end - next.start;
            if next.start != segment.end || span.saturating_add(next_len) > max_span {
                break;
            }
            segment.end = next.end;
            state.pending.pop_front();
        }
        let id = state.next_id;
        state.next_id += 1;
        let progress = Arc::new(AtomicU64::new(0));
        let steal_at = Arc::new(AtomicU64::new(0));
        state.active.push(Active {
            id,
            start: segment.start,
            end: segment.end,
            progress: Arc::clone(&progress),
            steal_at: Arc::clone(&steal_at),
        });
        Some(Claim {
            id,
            start: segment.start,
            end: segment.end,
            progress,
            steal_at,
        })
    }

    /// Ask the owner of the largest remaining segment to hand over its suffix.
    ///
    /// Returns `true` when a split request was published. The caller then waits
    /// for work (the owner re-queues the suffix when it notices), rather than
    /// assuming an immediate claim.
    pub(crate) fn try_request_steal(&self) -> bool {
        let state = self.state.lock().expect("scheduler mutex poisoned");
        let mut best: Option<(usize, u64)> = None;
        for (index, active) in state.active.iter().enumerate() {
            let progress = active.progress.load(Ordering::Acquire);
            let remaining = active
                .end
                .saturating_sub(active.start)
                .saturating_sub(progress);
            if remaining < MIN_STEAL_REMAINING {
                continue;
            }
            match best {
                Some((_, best_remaining)) if remaining <= best_remaining => {}
                _ => best = Some((index, remaining)),
            }
        }
        let Some((index, remaining)) = best else {
            return false;
        };
        let active = &state.active[index];
        let progress = active.progress.load(Ordering::Acquire);
        // Hand almost all remaining bytes to the thief: the owner only keeps a
        // small slice so it finishes quickly, and the suffix is re-issued as a
        // fresh request (a fresh connection/edge is what makes stealing pay).
        let kept = (remaining / STEAL_FRACTION_DIVISOR).max(1);
        let split = active.start + progress + kept;
        if kept < 1024 || split <= active.start || split >= active.end {
            return false;
        }
        active
            .steal_at
            .compare_exchange(0, split, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Record how much of the claim is on disk. `completed` is the number of
    /// bytes from `claim.start`; when a split was requested the owner stops at
    /// or before its split offset, and the suffix goes back to the queue.
    pub(crate) fn complete(&self, claim: Claim, completed: u64) -> Completion {
        let mut state = self.state.lock().expect("scheduler mutex poisoned");
        let covered = completed.min(claim.len());
        let taken = state.active.iter().position(|active| active.id == claim.id);
        if let Some(index) = taken {
            state.active.swap_remove(index);
        }
        let prefix_end = claim.start + covered;
        if covered > 0 {
            state.done.push((claim.start, prefix_end));
            state.done_bytes += covered;
        }
        if prefix_end < claim.end {
            // Splitting happens only at the tail; put the suffix at the front
            // so an idle worker grabs it before any older pending segment.
            state.pending.push_front(Segment {
                start: prefix_end,
                end: claim.end,
            });
            if prefix_end == claim.start {
                self.notify.notify_waiters();
                return Completion::Split { completed: 0 };
            }
            self.notify.notify_waiters();
            Completion::Split { completed: covered }
        } else {
            self.notify.notify_waiters();
            Completion::Finished
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        let state = self.state.lock().expect("scheduler mutex poisoned");
        state.done_bytes >= self.total && state.active.is_empty() && state.pending.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn done_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("scheduler mutex poisoned")
            .done_bytes
    }

    /// Remaining work units (pending segments + active segments), at least 1.
    pub(crate) fn remaining_units(&self) -> usize {
        let state = self.state.lock().expect("scheduler mutex poisoned");
        (state.pending.len() + state.active.len()).max(1)
    }

    pub(crate) fn has_work(&self) -> bool {
        let state = self.state.lock().expect("scheduler mutex poisoned");
        !state.pending.is_empty()
    }

    /// Sorted, merged completed byte ranges (for the resume state).
    pub(crate) fn completed_ranges(&self) -> Vec<(u64, u64)> {
        let state = self.state.lock().expect("scheduler mutex poisoned");
        crate::state::merge_ranges(state.done.clone())
    }

    /// Wait until some worker completes or enqueues work. Returns early after
    /// `timeout` so callers can re-check stop flags.
    pub(crate) async fn wait_for_change(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.notify.notified()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(total: u64) -> Vec<(u64, u64)> {
        vec![(0, total)]
    }

    #[test]
    fn initial_work_is_subdivided_into_chunks() {
        let scheduler = Scheduler::new(10, Vec::new(), full(10), 4);
        let mut ranges = Vec::new();
        while let Some(claim) = scheduler.claim() {
            ranges.push((claim.start, claim.end));
        }
        assert_eq!(ranges, vec![(0, 4), (4, 8), (8, 10)]);
    }

    #[test]
    fn missing_ranges_are_scheduled() {
        let scheduler = Scheduler::new(32, Vec::new(), vec![(4, 8), (16, 30)], 4);
        let mut ranges = Vec::new();
        while let Some(claim) = scheduler.claim() {
            ranges.push((claim.start, claim.end));
        }
        assert_eq!(ranges, vec![(4, 8), (16, 20), (20, 24), (24, 28), (28, 30)]);
    }

    #[test]
    fn completion_is_out_of_order_and_merged_for_state() {
        let scheduler = Scheduler::new(8, Vec::new(), full(8), 2);
        let a = scheduler.claim().unwrap();
        let b = scheduler.claim().unwrap();
        let c = scheduler.claim().unwrap();
        let d = scheduler.claim().unwrap();
        assert_eq!(scheduler.complete(c, 2), Completion::Finished); // [4,6)
        assert_eq!(scheduler.complete(a, 2), Completion::Finished); // [0,2)
        assert!(!scheduler.is_finished());
        assert_eq!(scheduler.completed_ranges(), vec![(0, 2), (4, 6)]);
        assert_eq!(scheduler.complete(b, 2), Completion::Finished); // [2,4)
        assert_eq!(scheduler.completed_ranges(), vec![(0, 6)]);
        assert_eq!(scheduler.done_bytes(), 6);
        assert_eq!(scheduler.complete(d, 2), Completion::Finished);
        assert!(scheduler.is_finished());
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn coalesces_adjacent_segments_when_pending_is_plentiful() {
        let scheduler = Scheduler::new(8 * MB, Vec::new(), full(8 * MB), MB);
        // 8 pending segments of 1 MiB, at most 4 MiB per request, and keep at
        // least 2 pending segments untouched for stealing/resume granularity.
        let first = scheduler.claim_with(2, 4 * MB).unwrap();
        assert_eq!((first.start, first.end), (0, 4 * MB));
        let second = scheduler.claim_with(2, 4 * MB).unwrap();
        assert_eq!((second.start, second.end), (4 * MB, 6 * MB));
        let third = scheduler.claim_with(2, 4 * MB).unwrap();
        assert_eq!((third.start, third.end), (6 * MB, 7 * MB));
        let fourth = scheduler.claim_with(2, 4 * MB).unwrap();
        assert_eq!((fourth.start, fourth.end), (7 * MB, 8 * MB));
        assert!(scheduler.claim_with(2, 4 * MB).is_none());
    }

    #[test]
    fn claim_with_zero_span_never_merges() {
        let scheduler = Scheduler::new(4 * MB, Vec::new(), full(4 * MB), MB);
        let first = scheduler.claim_with(0, 0).unwrap();
        assert_eq!((first.start, first.end), (0, MB));
    }

    #[test]
    fn steal_returns_the_suffix_to_the_queue() {
        let scheduler = Scheduler::new(4 * MB, Vec::new(), full(4 * MB), 4 * MB);
        let owner = scheduler.claim().unwrap(); // [0, 4 MiB)
        owner.update_progress(MB); // 1 MiB done, 3 MiB remaining
        assert!(scheduler.try_request_steal());
        let target = owner.steal_target().expect("split requested");
        // The owner keeps 1/8 of the remaining 3 MiB: 1 MiB + 384 KiB.
        assert_eq!(target, MB + (3 * MB) / 8);
        // No pending work: the suffix is only queued after the owner reports.
        assert!(scheduler.claim().is_none());
        let covered = target - owner.start;
        assert_eq!(
            scheduler.complete(owner, covered),
            Completion::Split { completed: covered }
        );
        let suffix = scheduler.claim().expect("stolen suffix is pending");
        assert_eq!((suffix.start, suffix.end), (target, 4 * MB));
        assert_eq!(scheduler.completed_ranges(), vec![(0, target)]);
    }

    #[test]
    fn small_remaining_segments_are_not_split() {
        let scheduler = Scheduler::new(8 * MB, Vec::new(), full(8 * MB), MB);
        let owner = scheduler.claim().unwrap(); // 1 MiB segment
        owner.update_progress(0);
        assert!(
            !scheduler.try_request_steal(),
            "1 MiB is below the 2 MiB threshold"
        );
    }

    #[test]
    fn a_steal_before_any_progress_requeues_the_whole_segment() {
        let scheduler = Scheduler::new(4 * MB, Vec::new(), full(4 * MB), 4 * MB);
        let owner = scheduler.claim().unwrap();
        assert!(scheduler.try_request_steal());
        assert_eq!(
            scheduler.complete(owner, 0),
            Completion::Split { completed: 0 }
        );
        let retry = scheduler.claim().expect("whole segment requeued");
        assert_eq!((retry.start, retry.end), (0, 4 * MB));
        assert_eq!(scheduler.done_bytes(), 0);
    }
}
