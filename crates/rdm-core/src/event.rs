//! Event plumbing: typed sink plus the 250 ms progress reporter.
//!
//! The engine never awaits on UI consumers — `UnboundedSender::send` is
//! non-blocking and a closed channel is ignored, so a dropped GUI cannot stall
//! a transfer.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, MissedTickBehavior};

use crate::{DownloadEvent, DownloadSummary};

/// UI-side refresh cadence from architecture §3.5.
pub(crate) const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// EWMA smoothing factor for instantaneous speed.
const EWMA_ALPHA: f64 = 0.3;

#[derive(Clone, Default)]
pub(crate) struct EventSink {
    tx: Option<UnboundedSender<DownloadEvent>>,
}

impl EventSink {
    pub fn new(tx: Option<UnboundedSender<DownloadEvent>>) -> Self {
        Self { tx }
    }

    pub fn emit(&self, event: DownloadEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }

    pub fn completed(&self, summary: &DownloadSummary) {
        self.emit(DownloadEvent::Completed {
            summary: summary.clone(),
        });
    }
}

/// Atomic progress counters shared by every worker and the reporter.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Payload bytes written to the `.part` file (partial chunk writes included
    /// while in flight; retries subtract them again).
    pub downloaded: AtomicU64,
    pub completed_chunks: AtomicU64,
    pub active_workers: AtomicUsize,
    /// High-water mark of [`Counters::active_workers`]; reported as
    /// `DownloadSummary::peak_connections`.
    pub peak_workers: AtomicUsize,
}

impl Counters {
    pub fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    pub fn add_downloaded(&self, bytes: u64) {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Undo accounting for a failed/aborted attempt.
    pub fn sub_downloaded(&self, bytes: u64) {
        if bytes > 0 {
            self.downloaded.fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    pub fn set_downloaded(&self, bytes: u64) {
        self.downloaded.store(bytes, Ordering::Relaxed);
    }

    /// Try to enter the "active worker" state while `active < target`.
    ///
    /// Returns `false` when the plan is already saturated; the caller parks and
    /// retries after a release/target change.
    pub fn try_enter_active(&self, target: usize) -> bool {
        let mut active = self.active_workers.load(Ordering::SeqCst);
        loop {
            if active >= target {
                return false;
            }
            match self.active_workers.compare_exchange(
                active,
                active + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    self.peak_workers.fetch_max(active + 1, Ordering::SeqCst);
                    return true;
                }
                Err(current) => active = current,
            }
        }
    }

    /// Leave the "active worker" state.
    pub fn leave_active(&self) {
        self.active_workers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Observed peak of simultaneously active workers.
    pub fn peak(&self) -> usize {
        self.peak_workers.load(Ordering::SeqCst)
    }
}

/// 250 ms progress pump. Stops when [`ProgressReporter::stop`] is called (or
/// when dropped, best effort).
pub(crate) struct ProgressReporter {
    shutdown: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl ProgressReporter {
    pub fn start(counters: Arc<Counters>, total: Option<u64>, events: EventSink) -> Self {
        let shutdown = Arc::new(Notify::new());
        let stop = Arc::clone(&shutdown);
        let task = tokio::spawn(async move {
            let mut ticker = interval(PROGRESS_INTERVAL);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // The first tick resolves immediately; discard it so the first
            // sample covers a real interval.
            ticker.tick().await;

            let mut ewma = 0.0f64;
            let mut last_bytes = counters.downloaded();
            let mut last_at = Instant::now();
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    _ = ticker.tick() => {}
                }
                let now = Instant::now();
                let downloaded = counters.downloaded();
                let dt = now.duration_since(last_at).as_secs_f64().max(1e-6);
                let delta = downloaded.saturating_sub(last_bytes) as f64;
                let instant = delta / dt;
                ewma = if ewma <= 0.0 {
                    instant
                } else {
                    EWMA_ALPHA * instant + (1.0 - EWMA_ALPHA) * ewma
                };
                let speed = ewma.max(0.0);
                let eta = match total {
                    Some(total) if total > downloaded => {
                        Some((total - downloaded) as f64 / speed.max(1.0))
                    }
                    Some(_) => Some(0.0),
                    None => None,
                };
                events.emit(DownloadEvent::Progress {
                    downloaded,
                    total,
                    speed_bps: speed,
                    eta_secs: eta,
                    active_workers: counters.active_workers.load(Ordering::Relaxed),
                });
                last_bytes = downloaded;
                last_at = now;
            }
        });
        Self {
            shutdown,
            task: Some(task),
        }
    }

    pub async fn stop(mut self) {
        self.shutdown.notify_waiters();
        if let Some(task) = self.task.take() {
            let _ = timeout(Duration::from_secs(2), task).await;
        }
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
    }
}
