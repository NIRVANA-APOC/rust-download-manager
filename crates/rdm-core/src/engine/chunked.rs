//! Multi-connection segment transfer (kernel v2).
//!
//! Differences from the legacy engine that matter for throughput:
//!
//! * **Admission is per request, not per segment.** A worker acquires a
//!   concurrency slot for exactly one HTTP attempt, releases it while sleeping
//!   on backoff, and re-acquires before a retry. Lowering the target therefore
//!   takes effect on the next attempt, including retries.
//! * **Server pushback pauses the whole origin.** 429/503 with `Retry-After`
//!   stops *all* new attempts, while in-flight bodies drain normally.
//! * **Byte-range scheduler with in-flight tail stealing.** Work is a queue of
//!   half-open ranges; an idle worker can ask a slow tail segment to split so
//!   the last bytes are finished by several connections instead of one.
//! * **The controller measures network bytes, not committed progress.**
//!   `net_rx` is monotonic; retries and rolled-back partial writes cannot make
//!   a window look faster or slower than the wire actually was.
//! * **Buffered positioned writes.** Each worker owns its file handle and
//!   coalesces body chunks into 256 KiB writes.

use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_RANGE, ETAG, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};

use crate::engine::controller::{initial_target_for, Controller, WindowSample, CONTROL_WINDOW};
use crate::engine::origin::{budget_for, OriginBudget, OriginGuard};
use crate::engine::scheduler::{
    Claim, Completion, Scheduler, COALESCE_MAX_BYTES, COALESCE_MIN_BYTES,
};
use crate::engine::{
    backoff, is_retryable_status, prepare_part, retry_after_delay, sync_all, verify_checksum,
    Context, ControlOutcome, Controls, StallDetector, GATE_POLL, MAX_ATTEMPTS, SAVER_TICK,
    SAVE_EVERY_CHUNKS, SAVE_INTERVAL, WRITE_BUFFER_BYTES,
};
use crate::event::{Counters, EventSink, ProgressReporter};
use crate::plan::ChunkPlan;
use crate::probe::parse_content_range;
use crate::rate::TokenBucket;
use crate::state::{self, ResumeState};
use crate::{ControllerStats, DownloadError, DownloadEvent, DownloadSummary};

/// How long an idle worker waits before re-checking for work.
const WORK_POLL: Duration = Duration::from_millis(100);
/// How long a thief waits for the owner to notice a split request.
const STEAL_WAIT: Duration = Duration::from_millis(250);

/// Files at least this large get a finer-grained tail: the last
/// [`TAIL_FINE_BYTES`] are scheduled as [`TAIL_FINE_PIECE`] segments so a slow
/// final region (cold CDN edge, slow mirror tail) is spread over several
/// connections instead of one big chunk.
const TAIL_FINE_MIN_TOTAL: u64 = 32 * 1024 * 1024;
const TAIL_FINE_BYTES: u64 = 8 * 1024 * 1024;
const TAIL_FINE_PIECE: u64 = 512 * 1024;

/// Turn missing byte ranges into schedule pieces.
///
/// Long ranges keep `chunk_size` granularity (the scheduler subdivides them);
/// the final [`TAIL_FINE_BYTES`] of a large file are cut into fine pieces. This
/// is a *scheduling* decision only: resume state remains the completed byte
/// ranges, so finer pieces are remembered exactly.
fn plan_work(missing: Vec<(u64, u64)>, total: u64) -> Vec<(u64, u64)> {
    let fine_start = if total >= TAIL_FINE_MIN_TOTAL {
        total.saturating_sub(TAIL_FINE_BYTES)
    } else {
        total
    };
    let mut work = Vec::new();
    for (start, end) in missing {
        if end == total && start < fine_start && fine_start - start >= 4 * TAIL_FINE_PIECE {
            work.push((start, fine_start));
            let mut cursor = fine_start;
            while cursor < total {
                let next = (cursor + TAIL_FINE_PIECE).min(total);
                work.push((cursor, next));
                cursor = next;
            }
        } else if end == total && start >= fine_start {
            let mut cursor = start;
            while cursor < total {
                let next = (cursor + TAIL_FINE_PIECE).min(total);
                work.push((cursor, next));
                cursor = next;
            }
        } else {
            work.push((start, end));
        }
    }
    work
}

/// A parked worker re-checks the gate at this cadence; release/target-change
/// notifications are the fast path.
pub(crate) struct Gate {
    target: AtomicUsize,
    stopped: AtomicBool,
    counters: Arc<Counters>,
    /// Process-wide per-origin cap learned from other tasks' pushbacks.
    origin: Option<Arc<OriginBudget>>,
    wake: Notify,
}

impl Gate {
    fn new(target: usize, counters: Arc<Counters>, origin: Option<Arc<OriginBudget>>) -> Self {
        Self {
            target: AtomicUsize::new(target.max(1)),
            stopped: AtomicBool::new(false),
            counters,
            origin,
            wake: Notify::new(),
        }
    }

    fn active(&self) -> usize {
        self.counters.active_workers.load(Ordering::SeqCst)
    }

    fn target(&self) -> usize {
        self.target.load(Ordering::SeqCst).max(1)
    }

    fn set_target(&self, target: usize) {
        self.target.store(target.max(1), Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    /// Wait until a request slot is free and take it.
    ///
    /// The returned guard releases the slot (and accounts busy time for the
    /// controller) when dropped.
    async fn acquire(&self, controls: &Controls) -> Option<ActiveGuard> {
        loop {
            if self.stopped.load(Ordering::SeqCst) || controls.stopped() {
                return None;
            }
            // Clamp on every attempt: another task may learn a smaller cap for
            // this origin while this transfer is already running.
            let cap = self
                .origin
                .as_ref()
                .and_then(|origin| origin.cap())
                .unwrap_or(usize::MAX);
            let effective = self.target().min(cap).max(1);
            if self.counters.try_enter_active(effective) {
                let mut slot = ActiveGuard {
                    counters: Arc::clone(&self.counters),
                    started: Instant::now(),
                    origin: None,
                };
                // The per-task gate bounds this transfer; the shared budget
                // bounds the *sum* across every task on the origin.
                if let Some(origin) = &self.origin {
                    match origin.acquire(controls).await {
                        Some(permit) => slot.origin = Some(permit),
                        None => {
                            drop(slot);
                            return None;
                        }
                    }
                }
                return Some(slot);
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = controls.wait_stop() => return None,
                // Lost-wakeup safety: never park indefinitely if a notification
                // raced with the atomic check above.
                _ = sleep(GATE_POLL) => {}
            }
        }
    }
}

/// RAII slot: accounts the time the request held a slot so the controller can
/// compute average concurrency.
pub(crate) struct ActiveGuard {
    counters: Arc<Counters>,
    started: Instant,
    /// Process-wide origin permit (one per in-flight HTTP request).
    origin: Option<OriginGuard>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.counters.leave_active();
        let nanos = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.counters.add_active_nanos(nanos);
    }
}

/// Origin-wide pause driven by server pushback (`Retry-After`).
struct ServerPause {
    until: StdMutex<Option<Instant>>,
}

impl ServerPause {
    fn new() -> Self {
        Self {
            until: StdMutex::new(None),
        }
    }

    fn extend(&self, now: Instant, delay: Duration) {
        let deadline = now + delay;
        let mut slot = self.until.lock().expect("server pause mutex poisoned");
        *slot = Some(match *slot {
            Some(current) => current.max(deadline),
            None => deadline,
        });
    }

    fn active(&self) -> bool {
        self.until
            .lock()
            .expect("server pause mutex poisoned")
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    /// Wait out the pause; `false` means the transfer stopped.
    async fn wait(&self, controls: &Controls) -> bool {
        loop {
            let remaining = {
                let slot = self.until.lock().expect("server pause mutex poisoned");
                slot.and_then(|until| until.checked_duration_since(Instant::now()))
            };
            match remaining {
                None => return true,
                Some(delay) => {
                    if !controls
                        .sleep_abortable(delay.min(Duration::from_millis(200)))
                        .await
                    {
                        return false;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared transfer state
// ---------------------------------------------------------------------------

struct Chunked {
    plan: ChunkPlan,
    total: u64,
    client: Client,
    url: String,
    /// Primary URL first, then validated mirrors. Workers are assigned to one
    /// source and rotate on failure.
    sources: Vec<String>,
    /// Per-source measured bytes/micros (adaptive mirror selection) and
    /// in-flight claim counts (water-filling tie-break).
    source_bytes: Vec<AtomicU64>,
    source_micros: Vec<AtomicU64>,
    source_inflight: Vec<AtomicU32>,
    part: PathBuf,
    state_path: PathBuf,
    request_url: String,
    final_url: String,
    file_name: String,
    etag: Option<String>,
    last_modified: Option<String>,
    connections_used: usize,
    scheduler: Arc<Scheduler>,
    resumed_chunks: u64,
    counters: Arc<Counters>,
    events: EventSink,
    controls: Controls,
    gate: Arc<Gate>,
    pause: Arc<ServerPause>,
    /// Process-wide budget for the origin (`scheme://host:port`): learned
    /// concurrency cap + shared Retry-After pause across tasks.
    origin: Option<Arc<OriginBudget>>,
    pushback_tx: Option<mpsc::UnboundedSender<(u64, Option<Duration>)>>,
    limiter: Option<Arc<TokenBucket>>,
    failure: StdMutex<Option<DownloadError>>,
    http_version: StdMutex<Option<String>>,
    strict_meta: bool,
    downgrade: AtomicBool,
    /// True when the fallback single stream must not trust the advertised
    /// total (range metadata was inconsistent/empty).
    downgrade_unknown_total: AtomicBool,
    /// Valid `206` heads with empty bodies; two of them trigger the fallback.
    empty_range_bodies: AtomicU32,
    save_gate: Mutex<()>,
    last_save: StdMutex<Instant>,
    segments_since_save: AtomicU64,
    dirty: AtomicBool,
    started: Instant,
}

impl Chunked {
    fn is_stopped(&self) -> bool {
        self.controls.stopped()
            || self.downgrade.load(Ordering::SeqCst)
            || self
                .failure
                .lock()
                .expect("failure mutex poisoned")
                .is_some()
    }

    fn fail(&self, error: DownloadError) {
        let mut slot = self.failure.lock().expect("failure mutex poisoned");
        if slot.is_none() {
            *slot = Some(error);
        }
        drop(slot);
        self.controls.request_stop();
    }

    fn take_failure(&self) -> Option<DownloadError> {
        self.failure.lock().expect("failure mutex poisoned").take()
    }

    fn request_downgrade(&self) {
        self.downgrade.store(true, Ordering::SeqCst);
        self.controls.request_stop();
    }

    /// Count a valid-head/empty-body range response. After the second one the
    /// transfer falls back to a single full stream with no trusted total.
    fn note_empty_range_body(&self) -> bool {
        let seen = self.empty_range_bodies.fetch_add(1, Ordering::SeqCst) + 1;
        if seen < 2 {
            return false;
        }
        self.downgrade_unknown_total.store(true, Ordering::SeqCst);
        self.request_downgrade();
        true
    }

    /// Record one attempt's measured throughput for a source.
    fn note_source(&self, source: usize, bytes: u64, elapsed: Duration) {
        let Some(index) = self.sources.get(source).map(|_| source) else {
            return;
        };
        self.source_bytes[index].fetch_add(bytes, Ordering::Relaxed);
        self.source_micros[index].fetch_add(
            elapsed.as_micros().max(1).min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }

    /// Pick the next source for a claim.
    ///
    /// Water-filling score = measured bytes/s ÷ (in-flight claims + 1): fast
    /// mirrors get more claims, yet concurrent claims spread across sources so
    /// no source is overloaded. Unsourced (0-byte) candidates are preferred
    /// when some mirror has no measurement yet; otherwise the caller's current
    /// source is kept as a stable fallback.
    fn pick_source(&self, current: usize) -> usize {
        let count = self.sources.len();
        if count <= 1 {
            return 0;
        }
        let mut best = current.min(count - 1);
        let mut best_score = f64::NEG_INFINITY;
        let mut untried: Option<usize> = None;
        let mut untried_inflight = u32::MAX;
        for index in 0..count {
            let bytes = self.source_bytes[index].load(Ordering::Relaxed);
            let micros = self.source_micros[index].load(Ordering::Relaxed);
            let inflight = self.source_inflight[index].load(Ordering::Relaxed);
            if micros == 0 {
                if inflight < untried_inflight {
                    untried_inflight = inflight;
                    untried = Some(index);
                }
                continue;
            }
            let rate = bytes as f64 / (micros as f64 / 1_000_000.0);
            let score = rate / (f64::from(inflight) + 1.0);
            if score > best_score {
                best_score = score;
                best = index;
            }
        }
        // Keep the worker's current source while it has no measurement (the
        // initial round-robin distribution already spreads first claims across
        // origins); only redirect once some source has proven faster.
        if self.source_micros[current.min(count - 1)].load(Ordering::Relaxed) == 0 {
            return current.min(count - 1);
        }
        untried.unwrap_or(best)
    }

    fn capture_http_version(&self, version: reqwest::Version) {
        let mut slot = self
            .http_version
            .lock()
            .expect("http version mutex poisoned");
        if slot.is_none() {
            *slot = Some(format!("{version:?}"));
        }
    }

    fn http_version(&self) -> Option<String> {
        self.http_version
            .lock()
            .expect("http version mutex poisoned")
            .clone()
            .or_else(|| Some("HTTP/1.1".to_string()))
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    /// Record a server pushback: pause the whole origin and tell the controller.
    fn note_pushback(&self, delay: Duration, retry_after: Option<Duration>) {
        self.pause.extend(Instant::now(), delay);
        // Teach the process-wide budget: later tasks (and other running tasks)
        // clamp to the learned cap and share the Retry-After pause.
        if let Some(origin) = &self.origin {
            let suggested = (self.gate.target() / 2).max(1);
            origin.note_pushback(Instant::now(), suggested, Some(delay));
        }
        if let Some(tx) = &self.pushback_tx {
            let _ = tx.send((self.now_ms(), retry_after));
        }
    }

    /// Wait until both this task's pause and the shared origin-wide pause have
    /// expired. Returns `false` when the download was stopped while waiting.
    async fn wait_pauses(&self) -> bool {
        loop {
            if !self.pause.wait(&self.controls).await {
                return false;
            }
            let Some(origin) = &self.origin else {
                return true;
            };
            let Some(remaining) = origin.pause_remaining(Instant::now()) else {
                return true;
            };
            // Re-check periodically: another task may extend the shared pause.
            if !self
                .controls
                .sleep_abortable(remaining.min(Duration::from_millis(200)))
                .await
            {
                return false;
            }
        }
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    // ---- resume state -----------------------------------------------------

    async fn after_segment(&self) {
        self.mark_dirty();
        let segments = self.segments_since_save.fetch_add(1, Ordering::Relaxed) + 1;
        let due_by_time = self
            .last_save
            .lock()
            .expect("save mutex poisoned")
            .elapsed()
            >= SAVE_INTERVAL;
        if (segments >= SAVE_EVERY_CHUNKS || due_by_time) && self.save_gate.try_lock().is_ok() {
            self.save_locked().await;
        }
    }

    async fn save_if_due_by_time(&self) {
        if !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        if self
            .last_save
            .lock()
            .expect("save mutex poisoned")
            .elapsed()
            < SAVE_INTERVAL
        {
            return;
        }
        if self.save_gate.try_lock().is_ok() {
            self.save_locked().await;
        }
    }

    async fn save_force(&self) {
        let _guard = self.save_gate.lock().await;
        self.save_locked().await;
    }

    async fn save_locked(&self) {
        let ranges = self.scheduler.completed_ranges();
        let resume = ResumeState {
            version: state::STATE_VERSION,
            url: self.request_url.clone(),
            final_url: self.final_url.clone(),
            file_name: self.file_name.clone(),
            total_bytes: self.total,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            chunk_size: self.plan.chunk_size,
            total_chunks: self.plan.total_chunks,
            completed_chunks: Vec::new(),
            completed_ranges: ranges.iter().map(|(s, e)| [*s, *e]).collect(),
            downloaded_bytes: self.counters.downloaded(),
        };
        if let Err(error) = state::save_atomic(&self.state_path, &resume).await {
            tracing::warn!(%error, "could not persist resume state");
        }
        *self.last_save.lock().expect("save mutex poisoned") = Instant::now();
        self.segments_since_save.store(0, Ordering::Relaxed);
        self.dirty.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Resume helpers
// ---------------------------------------------------------------------------

/// Count base chunks that are fully covered by completed ranges (the public
/// `resumed_chunks` semantics).
fn covered_base_chunks(plan: &ChunkPlan, ranges: &[(u64, u64)]) -> u64 {
    let mut count = 0u64;
    for &(start, end) in ranges {
        let first = start / plan.chunk_size;
        let last = (end - 1) / plan.chunk_size;
        for index in first..=last {
            let (chunk_start, chunk_end) = plan.byte_range(index);
            if start <= chunk_start && end >= chunk_end {
                count += 1;
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Resume data recovered from the state file.
struct ResumeWork {
    plan: ChunkPlan,
    ranges: Vec<(u64, u64)>,
    bytes: u64,
    chunks: u64,
}

pub(crate) async fn run(ctx: Context, total: u64) -> Result<DownloadSummary, DownloadError> {
    // ---- 1. Resume discovery -------------------------------------------------
    let mut resume: Option<ResumeWork> = None;
    if ctx.request.resume {
        if let Some(saved) = state::load(&ctx.state_path).await {
            if !state::is_compatible(&saved, &ctx.request, &ctx.meta, total) {
                tracing::info!("resume validators changed; restarting from zero");
            } else if let Some(ranges) = saved.completed_byte_ranges() {
                let plan = ChunkPlan::with_chunk_size(total, saved.chunk_size);
                let bytes: u64 = ranges.iter().map(|(s, e)| e - s).sum();
                let chunks = covered_base_chunks(&plan, &ranges);
                resume = Some(ResumeWork {
                    plan,
                    ranges,
                    bytes,
                    chunks,
                });
            } else {
                tracing::warn!("resume state has invalid ranges; restarting");
            }
        }
    }

    // ---- 2. Prepare the `.part` file ----------------------------------------
    let plan = resume
        .as_ref()
        .map(|state| state.plan)
        .unwrap_or_else(|| ChunkPlan::new(total, ctx.connections(), ctx.request.chunk_size));
    prepare_part(&ctx.part, total).await?;
    if resume.is_none() {
        let _ = tokio::fs::remove_file(&ctx.state_path).await;
    }

    // ---- 3. Shared transfer state ------------------------------------------
    let (ranges, resumed_bytes, resumed_chunks) = match resume.take() {
        Some(work) => (work.ranges, work.bytes, work.chunks),
        None => (Vec::new(), 0, 0),
    };
    let missing = state::missing_ranges(&ranges, total);
    let work = plan_work(missing, total);
    let counters = Arc::new(Counters::default());
    counters.set_downloaded(resumed_bytes);
    let scheduler = Arc::new(Scheduler::new(total, ranges, work, plan.chunk_size));
    let connections_used = plan.total_chunks.clamp(1, ctx.connections() as u64) as usize;

    ctx.events.emit(DownloadEvent::Started {
        connections: connections_used,
        chunk_size: plan.chunk_size,
        total_chunks: plan.total_chunks,
        resumed_chunks,
    });

    // Adaptive planning is disabled for a single connection, a user speed
    // limit (the local token bucket, not the network, is the bottleneck) and
    // static requests.
    let dynamic = ctx.request.dynamic_connections && connections_used > 1 && ctx.limiter.is_none();
    // Share one learned concurrency budget per origin across all tasks. A cap
    // learned by an earlier task (e.g. after 429 pushback) also warms the start
    // target, so a repeat download does not re-run the full 8→4 discovery.
    let origin = budget_for(&ctx.meta.final_url);
    let learned_cap = origin.as_ref().and_then(|budget| budget.cap());
    // Small files finish before the first control window: exploring is
    // impossible and under-using chunks only adds round trips. When the whole
    // file is at most two "waves" of chunks, start with all of them in flight.
    let initial = if dynamic {
        let cover = plan.total_chunks.min(connections_used as u64) as usize;
        if plan.total_chunks <= 2 * connections_used as u64 {
            cover
        } else {
            initial_target_for(connections_used, total)
        }
    } else {
        connections_used
    };
    // Warm-start only adaptive transfers: the controller can climb back if the
    // learned cap recovers, while a static request must keep its user-requested
    // concurrency (the origin budget still clamps actual permits).
    let initial = if dynamic {
        learned_cap.map_or(initial, |cap| initial.min(cap).max(1))
    } else {
        initial
    };
    let mut controller = dynamic.then(|| Controller::with_initial(connections_used, 0, initial));
    let initial_target = controller
        .as_ref()
        .map(Controller::target)
        .unwrap_or(initial);
    ctx.events.emit(DownloadEvent::Concurrency {
        target: initial_target,
        active: 0,
    });
    let (pushback_tx, pushback_rx) = if dynamic {
        let (tx, rx) = mpsc::unbounded_channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let gate = Arc::new(Gate::new(
        initial_target,
        Arc::clone(&counters),
        origin.clone(),
    ));
    let pause = Arc::new(ServerPause::new());
    let reporter = ProgressReporter::start(Arc::clone(&counters), Some(total), ctx.events.clone());
    let controls = Controls::new(ctx.handle.clone());
    let watcher = controls.spawn_watcher();

    // Chunked transfers run on HTTP/1.1 so each concurrent request owns a real
    // TCP connection (and its own congestion window). The client comes from the
    // probe so its warm connection can serve the first range request.
    let chunk_client = ctx.client.clone();

    // Primary + validated mirrors. Workers pick a source per claim; an initial
    // untried source is preferred, then measured throughput water-fills.
    let sources = collect_sources(&ctx.meta.final_url, &ctx.request.mirrors);
    let source_count = sources.len();
    let shared = Arc::new(Chunked {
        plan,
        total,
        client: chunk_client,
        url: ctx.meta.final_url.clone(),
        sources,
        source_bytes: (0..source_count).map(|_| AtomicU64::new(0)).collect(),
        source_micros: (0..source_count).map(|_| AtomicU64::new(0)).collect(),
        source_inflight: (0..source_count).map(|_| AtomicU32::new(0)).collect(),
        part: ctx.part.clone(),
        state_path: ctx.state_path.clone(),
        request_url: ctx.request.url.clone(),
        final_url: ctx.meta.final_url.clone(),
        file_name: ctx.file_name.clone(),
        etag: ctx.meta.etag.clone(),
        last_modified: ctx.meta.last_modified.clone(),
        connections_used,
        scheduler,
        resumed_chunks,
        counters: Arc::clone(&counters),
        events: ctx.events.clone(),
        controls,
        gate: Arc::clone(&gate),
        pause: Arc::clone(&pause),
        origin,
        pushback_tx,
        limiter: ctx.limiter.clone(),
        failure: StdMutex::new(None),
        http_version: StdMutex::new(None),
        strict_meta: ctx.strict_meta,
        downgrade: AtomicBool::new(false),
        downgrade_unknown_total: AtomicBool::new(false),
        empty_range_bodies: AtomicU32::new(0),
        save_gate: Mutex::new(()),
        last_save: StdMutex::new(Instant::now()),
        segments_since_save: AtomicU64::new(0),
        dirty: AtomicBool::new(false),
        started: ctx.started,
    });

    // ---- 4. Workers + controller + saver ------------------------------------
    let controller_done = Arc::new(Notify::new());
    let controller_task = match (controller.take(), pushback_rx) {
        (Some(controller), Some(pushback_rx)) => {
            let shared = Arc::clone(&shared);
            let done = Arc::clone(&controller_done);
            Some(tokio::spawn(controller_loop(
                shared,
                controller,
                pushback_rx,
                done,
            )))
        }
        _ => None,
    };

    let saver_done = Arc::new(Notify::new());
    let saver = {
        let shared = Arc::clone(&shared);
        let done = Arc::clone(&saver_done);
        tokio::spawn(saver_loop(shared, done))
    };

    let mut workers = Vec::with_capacity(connections_used);
    for index in 0..connections_used {
        let shared = Arc::clone(&shared);
        let source_index = index % shared.sources.len().max(1);
        workers.push(tokio::spawn(worker(shared, source_index)));
    }
    for handle in workers {
        if let Err(error) = handle.await {
            shared.fail(DownloadError::Other(format!(
                "worker task terminated unexpectedly: {error}"
            )));
        }
    }

    // Wake every parked worker and stop the planning machinery.
    shared.gate.stop();
    controller_done.notify_waiters();
    let mut controller_stats: Option<ControllerStats> = None;
    if let Some(task) = controller_task {
        match timeout(Duration::from_secs(2), task).await {
            Ok(Ok(stats)) => controller_stats = Some(stats),
            Ok(Err(error)) => tracing::warn!(%error, "controller task panicked"),
            Err(_) => tracing::warn!("controller task did not stop in time"),
        }
    }
    saver_done.notify_waiters();
    let _ = timeout(Duration::from_secs(2), saver).await;
    watcher.stop().await;
    reporter.stop().await;

    // ---- 5. Outcome ---------------------------------------------------------
    if let Some(outcome) = shared.controls.control_outcome() {
        shared.save_force().await;
        let downloaded = counters.downloaded();
        return Err(match outcome {
            ControlOutcome::Paused => DownloadError::Paused { downloaded },
            ControlOutcome::Cancelled => DownloadError::Cancelled { downloaded },
        });
    }

    if shared.downgrade.load(Ordering::SeqCst) {
        let unknown_total = shared.downgrade_unknown_total.load(Ordering::SeqCst);
        let reason = if unknown_total {
            "server returned empty range bodies; falling back to a single full stream \
             without trusting the advertised size"
        } else {
            "server ignored Range for a range request; falling back to a single stream"
        };
        shared.events.emit(DownloadEvent::Retrying {
            chunk: 0,
            attempt: 0,
            delay_ms: 0,
            reason: reason.to_string(),
        });
        let _ = tokio::fs::remove_file(&ctx.state_path).await;
        if unknown_total || ctx.meta_from_cache {
            return crate::engine::single(ctx, None, false).await;
        }
        return crate::engine::single(ctx, Some(total), false).await;
    }

    if let Some(error) = shared.take_failure() {
        shared.save_force().await;
        return Err(error);
    }

    finish_chunked(&ctx, &shared, controller_stats).await
}

async fn finish_chunked(
    ctx: &Context,
    shared: &Arc<Chunked>,
    controller: Option<ControllerStats>,
) -> Result<DownloadSummary, DownloadError> {
    if !shared.scheduler.is_finished() {
        // Workers only exit before completion on stop/failure; reaching here
        // with missing bytes would mean silent corruption, so fail loudly.
        return Err(DownloadError::Other(
            "internal error: transfer ended before all ranges completed".to_string(),
        ));
    }
    sync_all(&ctx.part).await?;
    let sha256 = verify_checksum(ctx, &ctx.part).await?;
    tokio::fs::rename(&ctx.part, &ctx.target).await?;
    let _ = tokio::fs::remove_file(&ctx.state_path).await;
    let summary = ctx.summary(
        shared.total,
        sha256,
        shared.resumed_chunks,
        true,
        shared.connections_used,
        shared.counters.peak(),
        shared.http_version(),
        controller,
    );
    ctx.events.completed(&summary);
    Ok(summary)
}

async fn saver_loop(shared: Arc<Chunked>, done: Arc<Notify>) {
    let mut ticker = interval(SAVER_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = done.notified() => break,
            _ = ticker.tick() => shared.save_if_due_by_time().await,
        }
    }
}

/// Adaptive-plan driver: one window per [`CONTROL_WINDOW`], applying pushback
/// notifications immediately.
async fn controller_loop(
    shared: Arc<Chunked>,
    mut controller: Controller,
    mut pushback_rx: mpsc::UnboundedReceiver<(u64, Option<Duration>)>,
    done: Arc<Notify>,
) -> ControllerStats {
    let mut ticker = interval(CONTROL_WINDOW);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first interval tick resolves immediately; discard it.
    ticker.tick().await;

    let mut last_net = shared.counters.net_rx();
    let mut last_active_nanos = shared.counters.active_nanos();
    let mut last_retries = shared.counters.retries();
    let mut last_at = Instant::now();
    loop {
        tokio::select! {
            _ = done.notified() => break,
            message = pushback_rx.recv() => {
                match message {
                    Some((now_ms, retry_after)) => {
                        apply_target(&shared, controller.note_pushback(now_ms, retry_after));
                    }
                    None => break,
                }
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                let interval = now.saturating_duration_since(last_at);
                let net = shared.counters.net_rx();
                let active_nanos = shared.counters.active_nanos();
                let retries = shared.counters.retries();
                let avg_active = if interval.is_zero() {
                    0.0
                } else {
                    active_nanos.saturating_sub(last_active_nanos) as f64
                        / interval.as_nanos() as f64
                };
                let sample = WindowSample {
                    net_bytes: net.saturating_sub(last_net),
                    interval,
                    avg_active,
                    current_active: shared.gate.active() as f64,
                    remaining_bytes: shared.total.saturating_sub(shared.counters.downloaded()),
                    paused: shared.pause.active(),
                    retry_events: retries.saturating_sub(last_retries) as u32,
                    now_ms: shared.now_ms(),
                };
                apply_target(&shared, controller.observe(sample));
                last_net = net;
                last_active_nanos = active_nanos;
                last_retries = retries;
                last_at = now;
            }
        }
    }
    controller.stats()
}

fn apply_target(shared: &Chunked, next: Option<usize>) {
    let Some(target) = next else {
        return;
    };
    // Never plan more requests than work units that still need downloading,
    // and never exceed the process-wide learned concurrency cap for this origin.
    let cap = shared
        .origin
        .as_ref()
        .and_then(|origin| origin.cap())
        .unwrap_or(usize::MAX);
    let target = target
        .min(shared.scheduler.remaining_units())
        .min(cap)
        .max(1);
    shared.gate.set_target(target);
    shared.events.emit(DownloadEvent::Concurrency {
        target,
        active: shared.gate.active(),
    });
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

/// Primary URL first, then distinct http(s) mirrors (at most 16).
fn collect_sources(primary: &str, mirrors: &[String]) -> Vec<String> {
    let mut sources = vec![primary.to_string()];
    for raw in mirrors.iter().take(16) {
        let mirror = raw.trim();
        if mirror.is_empty() || sources.iter().any(|existing| existing == mirror) {
            continue;
        }
        if !(mirror.starts_with("http://") || mirror.starts_with("https://")) {
            continue;
        }
        sources.push(mirror.to_string());
    }
    sources
}

/// Whether in-flight tail stealing is enabled.
///
/// Stealing is on by default. `RDM_DISABLE_STEAL=1` is a benchmark/diagnostic
/// switch for A/B runs (see `docs/bench/kernel-benchmark-method.md`); it is
/// read once per process and never changes product behaviour unless set.
fn stealing_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("RDM_DISABLE_STEAL").as_deref(),
            Ok("1") | Ok("true")
        )
    })
}

async fn worker(shared: Arc<Chunked>, initial_source: usize) {
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .open(&shared.part)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            shared.fail(DownloadError::Io(error));
            return;
        }
    };
    let mut source = initial_source;

    loop {
        if shared.is_stopped() {
            return;
        }
        let Some(claim) = next_claim(&shared).await else {
            return;
        };
        if shared.sources.len() > 1 {
            source = shared.pick_source(source);
        }
        let inflight_index = source.min(shared.sources.len().saturating_sub(1));
        shared.source_inflight[inflight_index].fetch_add(1, Ordering::Relaxed);
        let outcome = download_segment(&shared, &mut file, claim, &mut source).await;
        shared.source_inflight[inflight_index].fetch_sub(1, Ordering::Relaxed);
        match outcome {
            SegmentOutcome::Done { claim, covered } => {
                match shared.scheduler.complete(claim, covered) {
                    Completion::Finished | Completion::Split { .. } => {}
                }
                shared.after_segment().await;
            }
            SegmentOutcome::Stopped => return,
            SegmentOutcome::Downgrade => {
                shared.request_downgrade();
                return;
            }
            SegmentOutcome::Failed => return,
        }
    }
}

/// Wait for a pending segment, or arrange for a tail segment to be split so the
/// idle worker has something to do. Returns `None` when the transfer stopped or
/// all bytes are on disk.
async fn next_claim(shared: &Arc<Chunked>) -> Option<Claim> {
    loop {
        if shared.is_stopped() {
            return None;
        }
        let coalesce_span = if shared.total >= COALESCE_MIN_BYTES {
            COALESCE_MAX_BYTES
        } else {
            0
        };
        if let Some(claim) = shared
            .scheduler
            .claim_with(shared.connections_used, coalesce_span)
        {
            return Some(claim);
        }
        if shared.scheduler.is_finished() {
            return None;
        }
        if stealing_enabled() && shared.scheduler.try_request_steal() {
            // The owner notices the request while streaming and re-queues the
            // suffix; wait for that notification instead of busy-looping.
            shared.scheduler.wait_for_change(STEAL_WAIT).await;
            continue;
        }
        if !shared.scheduler.has_work() {
            // All work is in flight and none of it is worth splitting yet.
            // Wake on the next completion, and periodically re-check stop.
            shared.scheduler.wait_for_change(WORK_POLL).await;
        }
    }
}

enum SegmentOutcome {
    Done { claim: Claim, covered: u64 },
    Stopped,
    Downgrade,
    Failed,
}

enum AttemptError {
    Stopped,
    Downgrade,
    /// A `206` response arrived with valid range headers but a cleanly empty
    /// body (no bytes at all). Broken CDN edges / servers that advertise range
    /// support but return zero-length parts do this; repeated occurrences mean
    /// range metadata cannot be trusted.
    EmptyRangeBody,
    Retry {
        reason: String,
        retry_after: Option<Duration>,
        status: Option<u16>,
    },
    Fatal(DownloadError),
}

/// Result of streaming one attempt's response body.
enum BodyOutcome {
    /// The whole claimed range was written.
    Complete,
    /// Another worker is taking the suffix; `covered` bytes were written.
    Split { covered: u64 },
}

async fn download_segment(
    shared: &Arc<Chunked>,
    file: &mut tokio::fs::File,
    claim: Claim,
    source: &mut usize,
) -> SegmentOutcome {
    let mut attempt: u32 = 0;
    // Bytes of this range that are already on disk and counted as progress.
    // A retry resumes from here instead of re-downloading the whole range.
    let mut resume_from: u64 = 0;

    loop {
        if shared.is_stopped() {
            // Partial bytes are not part of the persisted range state, so they
            // must not remain counted as committed progress.
            shared.counters.sub_downloaded(resume_from);
            return SegmentOutcome::Stopped;
        }
        // A steal request can arrive while this worker is between attempts
        // (backoff sleep). Bytes from earlier attempts are real progress, so
        // hand the verified prefix to the scheduler and let another request
        // (a fresh connection/edge) take the rest.
        if claim.steal_target().is_some() {
            return SegmentOutcome::Done {
                claim,
                covered: resume_from,
            };
        }
        // Honour an origin-wide server pause before competing for a slot.
        if !shared.wait_pauses().await {
            shared.counters.sub_downloaded(resume_from);
            return SegmentOutcome::Stopped;
        }
        // One slot per HTTP attempt: a retrying worker stops occupying
        // concurrency while it sleeps and must ask again before retrying.
        let guard = match shared.gate.acquire(&shared.controls).await {
            Some(guard) => guard,
            None => {
                shared.counters.sub_downloaded(resume_from);
                return SegmentOutcome::Stopped;
            }
        };
        let attempt_started = Instant::now();
        let resume_before = resume_from;
        let source_used = *source;
        let result = attempt_segment(shared, file, &claim, resume_from, source_used).await;
        let attempt_elapsed = attempt_started.elapsed();
        drop(guard);

        match result {
            Ok(BodyOutcome::Complete) => {
                shared.note_source(
                    source_used,
                    claim.len().saturating_sub(resume_before),
                    attempt_elapsed,
                );
                let covered = claim.len();
                return SegmentOutcome::Done { claim, covered };
            }
            Ok(BodyOutcome::Split { covered }) => {
                shared.note_source(
                    source_used,
                    covered.saturating_sub(resume_before),
                    attempt_elapsed,
                );
                return SegmentOutcome::Done { claim, covered };
            }
            Err((error, counted)) => {
                // `counted` is the total covered bytes of this range for this
                // attempt (including `resume_from` when it resumed).
                shared.note_source(
                    source_used,
                    counted.saturating_sub(resume_before),
                    attempt_elapsed,
                );
                let tracked = counted.max(resume_from);
                match error {
                    AttemptError::Stopped => {
                        // Preserve the verified prefix of an interrupted claim
                        // as completed progress. Pause/cancel must not throw
                        // away megabytes of already-flushed, Content-Range
                        // verified bytes; the scheduler records the prefix and
                        // keeps the suffix pending for the resume.
                        let covered = tracked.min(claim.len());
                        if covered > 0 {
                            let _ = shared.scheduler.complete(claim, covered);
                        } else {
                            claim.update_progress(0);
                        }
                        return SegmentOutcome::Stopped;
                    }
                    AttemptError::Downgrade => {
                        shared.counters.sub_downloaded(tracked);
                        claim.update_progress(0);
                        return SegmentOutcome::Downgrade;
                    }
                    AttemptError::EmptyRangeBody => {
                        // Nothing was written, so nothing to roll back.
                        shared.counters.sub_downloaded(tracked);
                        let mirror_source = *source != 0 && shared.sources.len() > 1;
                        if mirror_source {
                            // A single bad mirror must not downgrade the whole
                            // transfer: rotate to the next source instead.
                            *source = (*source + 1) % shared.sources.len();
                        } else if shared.note_empty_range_body() {
                            return SegmentOutcome::Downgrade;
                        }
                        attempt += 1;
                        if attempt >= MAX_ATTEMPTS {
                            claim.update_progress(0);
                            shared.fail(DownloadError::Http(format!(
                                "range {}-{} returned empty bodies for every attempt",
                                claim.start, claim.end
                            )));
                            return SegmentOutcome::Failed;
                        }
                        claim.update_progress(0);
                        let delay = if mirror_source {
                            // A bad edge should not cost a full backoff; rotate
                            // and retry another source almost immediately.
                            Duration::from_millis(20)
                        } else {
                            backoff(attempt)
                        };
                        shared.events.emit(DownloadEvent::Retrying {
                            chunk: claim.start,
                            attempt,
                            delay_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
                            reason: if mirror_source {
                                "mirror returned a valid 206 head with an empty body; \
                                 switching source"
                                    .to_string()
                            } else {
                                "server returned a valid 206 head with an empty body".to_string()
                            },
                        });
                        if !shared.controls.sleep_abortable(delay).await {
                            return SegmentOutcome::Stopped;
                        }
                    }
                    AttemptError::Fatal(error) => {
                        shared.counters.sub_downloaded(tracked);
                        claim.update_progress(0);
                        shared.fail(error);
                        return SegmentOutcome::Failed;
                    }
                    AttemptError::Retry {
                        reason,
                        retry_after,
                        status,
                    } => {
                        attempt += 1;
                        if attempt >= MAX_ATTEMPTS {
                            shared.counters.sub_downloaded(tracked);
                            claim.update_progress(0);
                            shared.fail(match status {
                                Some(status) => DownloadError::Status {
                                    status,
                                    url: shared.url.clone(),
                                },
                                None => DownloadError::Http(format!(
                                    "range {}-{} failed after {attempt} attempts: {reason}",
                                    claim.start, claim.end
                                )),
                            });
                            return SegmentOutcome::Failed;
                        }
                        // The bytes this attempt wrote are verified by their
                        // Content-Range, so keep them counted and resume from
                        // there on the next request.
                        resume_from = tracked.min(claim.len());
                        claim.update_progress(resume_from);
                        shared.counters.note_retry();
                        let mut rotated = false;
                        if matches!(status, Some(429) | Some(503)) {
                            shared.note_pushback(
                                retry_after.unwrap_or_else(|| backoff(attempt)),
                                retry_after,
                            );
                        } else if shared.sources.len() > 1 {
                            // Source-specific failure (404/5xx/connection): drop
                            // this source and retry on the next one immediately.
                            // Exponential backoff here would add seconds to a
                            // mirror failover for no benefit.
                            *source = (*source + 1) % shared.sources.len();
                            rotated = true;
                        }
                        let delay = retry_after.unwrap_or_else(|| {
                            if rotated {
                                Duration::from_millis(20)
                            } else {
                                backoff(attempt)
                            }
                        });
                        shared.events.emit(DownloadEvent::Retrying {
                            chunk: claim.start,
                            attempt,
                            delay_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
                            reason,
                        });
                        if !shared.controls.sleep_abortable(delay).await {
                            shared.counters.sub_downloaded(resume_from);
                            claim.update_progress(0);
                            return SegmentOutcome::Stopped;
                        }
                    }
                }
            }
        }
    }
}

async fn attempt_segment(
    shared: &Arc<Chunked>,
    file: &mut tokio::fs::File,
    claim: &Claim,
    resume_from: u64,
    source_index: usize,
) -> Result<BodyOutcome, (AttemptError, u64)> {
    let total_expected = claim.len();
    let resume_from = resume_from.min(total_expected);
    let start = claim.start + resume_from;
    let end_inclusive = claim.end - 1;
    let range_value = format!("bytes={start}-{end_inclusive}");
    shared.counters.note_request_start();
    let url = &shared.sources[source_index.min(shared.sources.len() - 1)];
    let mut request = shared.client.get(url).header(RANGE, range_value);
    // Ask the server to validate the representation on every range request.
    // `If-Range` only works with a strong validator (quoted ETag); if the
    // object changed, the server answers 200 and the engine restarts as a
    // single stream instead of assembling a mix of old and new bytes.
    if let Some(etag) = shared.etag.as_deref() {
        if etag.starts_with('"') && etag.ends_with('"') {
            request = request.header(IF_RANGE, etag);
        }
    }

    let sent_at = Instant::now();
    let response = tokio::select! {
        result = request.send() => result,
        _ = shared.controls.wait_stop() => return Err((AttemptError::Stopped, 0)),
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return Err((
                AttemptError::Retry {
                    reason: format!("request failed: {error}"),
                    retry_after: None,
                    status: None,
                },
                0,
            ))
        }
    };
    shared.counters.note_ttfb(sent_at.elapsed());

    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        shared.capture_http_version(response.version());
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
        if source_index != 0 {
            // A mirror must serve the same representation. If it advertises a
            // different ETag, its bytes cannot be mixed into the file.
            if let (Some(expected), Some(actual)) = (
                shared.etag.as_deref(),
                response
                    .headers()
                    .get(ETAG)
                    .and_then(|value| value.to_str().ok()),
            ) {
                if actual != expected {
                    return Err((
                        AttemptError::Retry {
                            reason: "mirror ETag differs from the primary; switching source"
                                .to_string(),
                            retry_after: None,
                            status: None,
                        },
                        0,
                    ));
                }
            }
        }
        if shared.strict_meta {
            if let Some(total) = content_range.and_then(|range| range.total) {
                if total != shared.total {
                    if source_index != 0 {
                        return Err((
                            AttemptError::Retry {
                                reason: "mirror reported a different size; switching source"
                                    .to_string(),
                                retry_after: None,
                                status: None,
                            },
                            0,
                        ));
                    }
                    return Err((AttemptError::Fatal(DownloadError::RemoteChanged), 0));
                }
            }
        }
        let valid = match content_range {
            Some(range) => {
                range.start == start
                    && range.end == end_inclusive
                    && range
                        .total
                        .map(|total| total == shared.total)
                        .unwrap_or(true)
            }
            None => false,
        };
        if !valid {
            return Err((
                AttemptError::Retry {
                    reason: format!("invalid Content-Range for range {start}-{end_inclusive}"),
                    retry_after: None,
                    status: Some(status.as_u16()),
                },
                0,
            ));
        }
        return stream_segment_body(shared, response, file, claim, total_expected, resume_from)
            .await;
    }

    if status == StatusCode::OK {
        if source_index != 0 && shared.sources.len() > 1 {
            // A mirror that ignores Range must not force the whole transfer
            // onto a single stream; rotate back to another source instead.
            return Err((
                AttemptError::Retry {
                    reason: "mirror ignored Range; switching source".to_string(),
                    retry_after: None,
                    status: None,
                },
                0,
            ));
        }
        // Primary ignored `Range`; the caller downgrades to a single stream.
        return Err((AttemptError::Downgrade, 0));
    }

    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
        return Err((
            AttemptError::Retry {
                reason: format!("server returned HTTP {}", status.as_u16()),
                retry_after: retry_after_delay(response.headers()),
                status: Some(status.as_u16()),
            },
            0,
        ));
    }

    if is_retryable_status(status) {
        return Err((
            AttemptError::Retry {
                reason: format!("server returned HTTP {}", status.as_u16()),
                retry_after: None,
                status: None,
            },
            0,
        ));
    }

    Err((
        if source_index != 0 && shared.sources.len() > 1 {
            // A mirror that answers 404/403/410 etc. is stale, not a reason to
            // fail the whole transfer: drop this source and rotate.
            AttemptError::Retry {
                reason: format!("mirror returned HTTP {}; switching source", status.as_u16()),
                retry_after: None,
                status: Some(status.as_u16()),
            }
        } else {
            AttemptError::Fatal(DownloadError::Status {
                status: status.as_u16(),
                url: shared.url.clone(),
            })
        },
        0,
    ))
}

/// Stream one response body into the `.part` file.
///
/// `resume_from` bytes of the claimed range are already on disk; the request
/// asked for `[claim.start + resume_from, claim.end)`. The owner checks
/// `claim.steal_target()` after every network read. When a split was requested
/// it flushes exactly up to that offset and returns [`BodyOutcome::Split`]; the
/// scheduler re-queues the suffix. Bytes already read past the split (at most
/// one write buffer) are discarded, which keeps the overlap bounded by
/// [`WRITE_BUFFER_BYTES`].
async fn stream_segment_body(
    shared: &Arc<Chunked>,
    mut response: reqwest::Response,
    file: &mut tokio::fs::File,
    claim: &Claim,
    expected: u64,
    resume_from: u64,
) -> Result<BodyOutcome, (AttemptError, u64)> {
    if let Err(error) = file.seek(SeekFrom::Start(claim.start + resume_from)).await {
        return Err((AttemptError::Fatal(DownloadError::Io(error)), resume_from));
    }

    let mut written = resume_from;
    let mut buffer: Vec<u8> = Vec::with_capacity(WRITE_BUFFER_BYTES);
    let mut stall = StallDetector::default();
    loop {
        // Split exactly at the requested offset once enough data is buffered.
        if let Some(target) = claim.steal_target() {
            let target_rel = target.saturating_sub(claim.start).min(expected);
            if written + buffer.len() as u64 >= target_rel {
                let take = (target_rel.saturating_sub(written) as usize).min(buffer.len());
                if take > 0 {
                    if let Err(error) = file.write_all(&buffer[..take]).await {
                        return Err((AttemptError::Fatal(DownloadError::Io(error)), written));
                    }
                    written += take as u64;
                    shared.counters.add_downloaded(take as u64);
                    claim.update_progress(written);
                }
                return Ok(BodyOutcome::Split { covered: written });
            }
        }

        let stall_limit = stall.timeout();
        let last_chunk = Instant::now();
        let chunk = tokio::select! {
            result = timeout(stall_limit, response.chunk()) => match result {
                Ok(result) => result,
                Err(_) => {
                    // The connection went silent mid-body (bad CDN edge, NAT
                    // timeout, half-open socket). Retry on a fresh connection
                    // instead of waiting for the 60 s read timeout.
                    return Err((
                        AttemptError::Retry {
                            reason: format!(
                                "no response body data for {}s (stalled connection)",
                                stall_limit.as_secs()
                            ),
                            retry_after: None,
                            status: None,
                        },
                        written,
                    ));
                }
            },
            _ = shared.controls.wait_stop() => return Err((AttemptError::Stopped, written)),
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                // Clean EOF without a single body byte after a valid 206: the
                // server returned an empty range body. Do not confuse this
                // with a mid-stream reset (which is an `Err` and is retried
                // with sub-chunk resume).
                if written == resume_from && buffer.is_empty() {
                    return Err((AttemptError::EmptyRangeBody, written));
                }
                break;
            }
            Err(error) => {
                return Err((
                    AttemptError::Retry {
                        reason: format!("body read failed: {error}"),
                        retry_after: None,
                        status: None,
                    },
                    written,
                ))
            }
        };
        stall.observe(last_chunk.elapsed());
        let len = bytes.len() as u64;
        shared.counters.add_net_rx(len);
        if written + buffer.len() as u64 + len > expected {
            return Err((
                AttemptError::Retry {
                    reason: format!(
                        "server sent more bytes than requested for the range ({} > {expected})",
                        written + buffer.len() as u64 + len
                    ),
                    retry_after: None,
                    status: None,
                },
                written,
            ));
        }
        if let Some(limiter) = &shared.limiter {
            tokio::select! {
                _ = limiter.acquire(len) => {}
                _ = shared.controls.wait_stop() => return Err((AttemptError::Stopped, written)),
            }
        }
        if shared.is_stopped() {
            return Err((AttemptError::Stopped, written));
        }
        buffer.extend_from_slice(&bytes);
        if buffer.len() >= WRITE_BUFFER_BYTES {
            if let Err(error) = file.write_all(&buffer).await {
                return Err((AttemptError::Fatal(DownloadError::Io(error)), written));
            }
            written += buffer.len() as u64;
            shared.counters.add_downloaded(buffer.len() as u64);
            claim.update_progress(written);
            buffer.clear();
        }
    }

    if !buffer.is_empty() {
        if let Err(error) = file.write_all(&buffer).await {
            return Err((AttemptError::Fatal(DownloadError::Io(error)), written));
        }
        written += buffer.len() as u64;
        shared.counters.add_downloaded(buffer.len() as u64);
        claim.update_progress(written);
    }

    if written == expected {
        Ok(BodyOutcome::Complete)
    } else {
        Err((
            AttemptError::Retry {
                reason: format!("short body for range: {written}/{expected} bytes"),
                retry_after: None,
                status: None,
            },
            written,
        ))
    }
}

#[cfg(test)]
mod work_planning_tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn mirrors_are_deduped_and_validated() {
        let primary = "http://a.test/file.bin".to_string();
        let sources = collect_sources(
            &primary,
            &[
                primary.clone(),
                " https://mirror.test/file.bin ".to_string(),
                "ftp://not-http/file.bin".to_string(),
                "https://mirror.test/file.bin".to_string(),
                String::new(),
            ],
        );
        assert_eq!(
            sources,
            vec![
                "http://a.test/file.bin".to_string(),
                "https://mirror.test/file.bin".to_string()
            ]
        );
    }

    #[test]
    fn small_files_keep_chunk_granularity() {
        let total = 8 * MB;
        assert_eq!(plan_work(vec![(0, total)], total), vec![(0, total)]);
    }

    #[test]
    fn large_files_get_a_fine_tail() {
        let total = 32 * MB;
        let work = plan_work(vec![(0, total)], total);
        assert_eq!(work.first().copied(), Some((0, total - 8 * MB)));
        let tail: Vec<_> = work
            .iter()
            .copied()
            .filter(|(start, _)| *start >= total - 8 * MB)
            .collect();
        assert_eq!(tail.len(), 16, "8 MiB tail in 512 KiB pieces");
        assert!(tail.iter().all(|(s, e)| e - s <= TAIL_FINE_PIECE));
        assert_eq!(*tail.last().unwrap(), (total - 512 * 1024, total));
    }

    #[test]
    fn resumed_tail_only_is_split_finely_without_a_prefix() {
        let total = 32 * MB;
        let start = total - 4 * MB + 1234;
        let work = plan_work(vec![(start, total)], total);
        assert!(work.iter().all(|(s, e)| e - s <= TAIL_FINE_PIECE));
        assert_eq!(work.first().map(|(s, _)| *s), Some(start));
        assert_eq!(work.last().map(|(_, e)| *e), Some(total));
    }

    #[test]
    fn resume_gap_before_the_fine_tail_keeps_a_prefix_piece() {
        let total = 32 * MB;
        let work = plan_work(vec![(20 * MB, total)], total);
        assert_eq!(work.first().copied(), Some((20 * MB, total - 8 * MB)));
        assert!(work.len() > 16);
    }
}
