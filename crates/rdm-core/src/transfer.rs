//! Transfer orchestration: chunked multi-connection mode, single-stream
//! fallback, retries, resume state persistence and progress reporting.
//!
//! Design references: architecture §3.3 (chunking/work stealing), §3.4
//! (.part + .part.json), §3.5 (rate limiting, progress, cancellation).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_RANGE, RANGE, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{watch, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};

use crate::client;
use crate::event::{Counters, EventSink, ProgressReporter};
use crate::filename;
use crate::plan::ChunkPlan;
use crate::probe::{self, parse_content_range};
use crate::rate::TokenBucket;
use crate::state::{self, ResumeState};
use crate::{
    DownloadError, DownloadEvent, DownloadHandle, DownloadRequest, DownloadSummary, RemoteMeta,
};

/// Total attempts per chunk / single stream.
const MAX_ATTEMPTS: u32 = 6;
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(32);
/// Persist resume state at least this often while chunks complete.
const SAVE_INTERVAL: Duration = Duration::from_millis(500);
/// ...and immediately every N completed chunks.
const SAVE_EVERY_CHUNKS: u64 = 16;
/// Polling cadence for external pause/cancel flags.
const CONTROL_POLL: Duration = Duration::from_millis(50);
/// Upper bound for a server-provided `Retry-After`.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
/// Saver tick.
const SAVER_TICK: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub(crate) async fn run(
    request: DownloadRequest,
    events: EventSink,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    let sink = events.clone();
    let result = run_inner(request, events, handle).await;
    match &result {
        Ok(_) => {}
        Err(DownloadError::Paused { downloaded }) => sink.emit(DownloadEvent::Paused {
            downloaded: *downloaded,
        }),
        Err(error) if !error.is_control_flow() => sink.emit(DownloadEvent::Failed {
            error: error.to_string(),
        }),
        Err(_) => {}
    }
    result
}

async fn run_inner(
    request: DownloadRequest,
    events: EventSink,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    let started = Instant::now();
    if handle.is_cancelled() {
        return Err(DownloadError::Cancelled { downloaded: 0 });
    }

    events.emit(DownloadEvent::Probing);
    let http = client::build(&request)?;
    let meta = probe::probe_with(&http, &request).await?;
    events.emit(DownloadEvent::Metadata(meta.clone()));
    if handle.is_cancelled() {
        return Err(DownloadError::Cancelled { downloaded: 0 });
    }

    let dir = request
        .output_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    tokio::fs::create_dir_all(&dir).await?;

    let desired = request
        .file_name
        .clone()
        .unwrap_or_else(|| meta.file_name.clone());
    let (target, file_name) = filename::resolve_target(&dir, &desired, request.resume);
    let part = state::part_path(&target);
    let state_path = state::state_path(&target);
    let limiter = request
        .speed_limit_bps
        .filter(|bps| *bps > 0)
        .map(|bps| Arc::new(TokenBucket::new(bps)));

    let ctx = Context {
        request,
        client: http,
        meta,
        target,
        file_name,
        part,
        state_path,
        events,
        handle,
        limiter,
        started,
    };

    match ctx.meta.total_bytes {
        Some(total) if ctx.meta.supports_ranges => chunked(ctx, total).await,
        Some(total) => single(ctx, Some(total), false).await,
        None => single(ctx, None, false).await,
    }
}

// ---------------------------------------------------------------------------
// Shared context
// ---------------------------------------------------------------------------

struct Context {
    request: DownloadRequest,
    client: Client,
    meta: RemoteMeta,
    target: PathBuf,
    file_name: String,
    part: PathBuf,
    state_path: PathBuf,
    events: EventSink,
    handle: DownloadHandle,
    limiter: Option<Arc<TokenBucket>>,
    started: Instant,
}

impl Context {
    fn connections(&self) -> usize {
        self.request.connections.max(1)
    }

    fn summary(
        &self,
        total_bytes: u64,
        sha256: Option<String>,
        resumed_chunks: u64,
        range_supported: bool,
        connections: usize,
        http_version: Option<String>,
    ) -> DownloadSummary {
        let elapsed = self.started.elapsed();
        let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        let seconds = elapsed.as_secs_f64().max(1e-6);
        DownloadSummary {
            path: self.target.clone(),
            total_bytes,
            elapsed_ms,
            avg_speed_bps: total_bytes as f64 / seconds,
            connections,
            sha256,
            resumed_chunks,
            range_supported,
            http_version,
        }
    }
}

// ---------------------------------------------------------------------------
// Pause / cancel plumbing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlOutcome {
    Paused,
    Cancelled,
}

/// Watch channel for internal stop requests (downgrade, fatal error) plus a
/// polling watcher that turns external [`DownloadHandle`] flags into stop
/// notifications so in-flight reads abort promptly.
#[derive(Clone)]
struct Controls {
    handle: DownloadHandle,
    stop: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
}

impl Controls {
    fn new(handle: DownloadHandle) -> Self {
        let (stop, stop_rx) = watch::channel(false);
        Self {
            handle,
            stop,
            stop_rx,
        }
    }

    fn request_stop(&self) {
        if !*self.stop.borrow() {
            let _ = self.stop.send(true);
        }
    }

    fn stopped(&self) -> bool {
        self.handle.is_paused() || self.handle.is_cancelled() || *self.stop.borrow()
    }

    /// Why we stopped, if it was user control flow.
    fn control_outcome(&self) -> Option<ControlOutcome> {
        if self.handle.is_cancelled() {
            Some(ControlOutcome::Cancelled)
        } else if self.handle.is_paused() {
            Some(ControlOutcome::Paused)
        } else {
            None
        }
    }

    /// Completes when a stop is requested (internal or external).
    async fn wait_stop(&self) {
        let mut rx = self.stop_rx.clone();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }

    /// Sleep that returns `false` as soon as a stop is requested.
    async fn sleep_abortable(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.stopped() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            let step = (deadline - now).min(CONTROL_POLL);
            tokio::select! {
                _ = sleep(step) => {}
                _ = self.wait_stop() => return false,
            }
        }
    }

    fn spawn_watcher(&self) -> ControlWatcher {
        let handle = self.handle.clone();
        let stop = self.stop.clone();
        let done = Arc::new(Notify::new());
        let wait_on = Arc::clone(&done);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = wait_on.notified() => break,
                    _ = sleep(CONTROL_POLL) => {
                        if handle.is_paused() || handle.is_cancelled() {
                            let _ = stop.send(true);
                            break;
                        }
                        if *stop.borrow() {
                            break;
                        }
                    }
                }
            }
        });
        ControlWatcher {
            done,
            task: Some(task),
        }
    }
}

struct ControlWatcher {
    done: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl ControlWatcher {
    async fn stop(mut self) {
        self.done.notify_waiters();
        if let Some(task) = self.task.take() {
            let _ = timeout(Duration::from_secs(1), task).await;
        }
    }
}

impl Drop for ControlWatcher {
    fn drop(&mut self) {
        self.done.notify_waiters();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Chunked transfer
// ---------------------------------------------------------------------------

struct ResumePlan {
    plan: ChunkPlan,
    completed: Vec<bool>,
    bytes: u64,
    chunks: u64,
}

struct ChunkTransfer {
    plan: ChunkPlan,
    total: u64,
    client: Client,
    url: String,
    part: PathBuf,
    state_path: PathBuf,
    request_url: String,
    final_url: String,
    file_name: String,
    etag: Option<String>,
    last_modified: Option<String>,
    connections_used: usize,
    completed: Vec<AtomicBool>,
    next: AtomicU64,
    completed_bytes: AtomicU64,
    resumed_chunks: u64,
    counters: Arc<Counters>,
    events: EventSink,
    controls: Controls,
    limiter: Option<Arc<TokenBucket>>,
    failure: StdMutex<Option<DownloadError>>,
    /// Protocol observed on the first successful ranged response; must be
    /// HTTP/1.1 because chunked mode uses an h1-only client.
    http_version: StdMutex<Option<String>>,
    downgrade: AtomicBool,
    save_gate: Mutex<()>,
    last_save: StdMutex<Instant>,
    chunks_since_save: AtomicU64,
    dirty: AtomicBool,
}

impl ChunkTransfer {
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

    fn capture_http_version(&self, version: reqwest::Version) {
        let mut slot = self
            .http_version
            .lock()
            .expect("http version mutex poisoned");
        if slot.is_none() {
            *slot = probe::version_string(version);
        }
    }

    fn http_version(&self) -> Option<String> {
        self.http_version
            .lock()
            .expect("http version mutex poisoned")
            .clone()
            .or_else(|| Some("HTTP/1.1".to_string()))
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Persist after a completed chunk: honours the 16-chunk and 500 ms rules.
    async fn after_chunk(&self) {
        self.mark_dirty();
        let chunks = self.chunks_since_save.fetch_add(1, Ordering::Relaxed) + 1;
        let due_by_time = self
            .last_save
            .lock()
            .expect("save mutex poisoned")
            .elapsed()
            >= SAVE_INTERVAL;
        if (chunks >= SAVE_EVERY_CHUNKS || due_by_time) && self.save_gate.try_lock().is_ok() {
            self.save_locked().await;
        }
    }

    /// Periodic save driven by the saver task (500 ms rule).
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

    /// Force a save (pause/cancel/failure paths).
    async fn save_force(&self) {
        let _guard = self.save_gate.lock().await;
        self.save_locked().await;
    }

    async fn save_locked(&self) {
        let completed: Vec<u64> = self
            .completed
            .iter()
            .enumerate()
            .filter_map(|(index, flag)| flag.load(Ordering::Relaxed).then_some(index as u64))
            .collect();
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
            completed_chunks: completed,
            downloaded_bytes: self.completed_bytes.load(Ordering::Relaxed),
        };
        if let Err(error) = state::save_atomic(&self.state_path, &resume).await {
            tracing::warn!(%error, "could not persist resume state");
        }
        *self.last_save.lock().expect("save mutex poisoned") = Instant::now();
        self.chunks_since_save.store(0, Ordering::Relaxed);
        self.dirty.store(false, Ordering::Relaxed);
    }
}

async fn chunked(ctx: Context, total: u64) -> Result<DownloadSummary, DownloadError> {
    // ---- 1. Resume discovery -------------------------------------------------
    let mut resume: Option<ResumePlan> = None;
    if ctx.request.resume {
        if let Some(saved) = state::load(&ctx.state_path).await {
            if !state::is_compatible(&saved, &ctx.request, &ctx.meta, total) {
                tracing::info!("resume validators changed; restarting from zero");
            } else {
                let plan = ChunkPlan::with_chunk_size(total, saved.chunk_size);
                if plan.total_chunks == saved.total_chunks {
                    let mut completed = vec![false; plan.total_chunks as usize];
                    let mut bytes = 0u64;
                    let mut chunks = 0u64;
                    let mut valid = true;
                    for &index in &saved.completed_chunks {
                        if index >= plan.total_chunks {
                            valid = false;
                            break;
                        }
                        let slot = &mut completed[index as usize];
                        if !*slot {
                            *slot = true;
                            bytes += plan.chunk_len(index);
                            chunks += 1;
                        }
                    }
                    if valid {
                        resume = Some(ResumePlan {
                            plan,
                            completed,
                            bytes,
                            chunks,
                        });
                    } else {
                        tracing::warn!("resume state references out-of-range chunks; restarting");
                    }
                }
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
    let (completed, resumed_bytes, resumed_chunks) = match resume.take() {
        Some(state) => (state.completed, state.bytes, state.chunks),
        None => (vec![false; plan.total_chunks as usize], 0, 0),
    };
    let counters = Arc::new(Counters::default());
    counters.set_downloaded(resumed_bytes);
    counters
        .completed_chunks
        .store(resumed_chunks, Ordering::Relaxed);
    let connections_used = plan.total_chunks.clamp(1, ctx.connections() as u64) as usize;

    ctx.events.emit(DownloadEvent::Started {
        connections: connections_used,
        chunk_size: plan.chunk_size,
        total_chunks: plan.total_chunks,
        resumed_chunks,
    });

    let reporter = ProgressReporter::start(Arc::clone(&counters), Some(total), ctx.events.clone());
    let controls = Controls::new(ctx.handle.clone());
    let watcher = controls.spawn_watcher();

    // Critical: chunked transfers run on HTTP/1.1 so each worker owns a real
    // TCP connection. On h2, all streams would share one congestion window and
    // multi-connection throughput gains would be an illusion.
    let chunk_client = client::build_http1(&ctx.request)?;

    let shared = Arc::new(ChunkTransfer {
        plan,
        total,
        client: chunk_client,
        url: ctx.meta.final_url.clone(),
        part: ctx.part.clone(),
        state_path: ctx.state_path.clone(),
        request_url: ctx.request.url.clone(),
        final_url: ctx.meta.final_url.clone(),
        file_name: ctx.file_name.clone(),
        etag: ctx.meta.etag.clone(),
        last_modified: ctx.meta.last_modified.clone(),
        connections_used,
        completed: completed.into_iter().map(AtomicBool::new).collect(),
        next: AtomicU64::new(0),
        completed_bytes: AtomicU64::new(resumed_bytes),
        resumed_chunks,
        counters: Arc::clone(&counters),
        events: ctx.events.clone(),
        controls,
        limiter: ctx.limiter.clone(),
        failure: StdMutex::new(None),
        http_version: StdMutex::new(None),
        downgrade: AtomicBool::new(false),
        save_gate: Mutex::new(()),
        last_save: StdMutex::new(Instant::now()),
        chunks_since_save: AtomicU64::new(0),
        dirty: AtomicBool::new(false),
    });

    // ---- 4. Workers + saver -------------------------------------------------
    let saver_done = Arc::new(Notify::new());
    let saver = {
        let shared = Arc::clone(&shared);
        let done = Arc::clone(&saver_done);
        tokio::spawn(saver_loop(shared, done))
    };

    let mut workers = Vec::with_capacity(connections_used);
    for _ in 0..connections_used {
        let shared = Arc::clone(&shared);
        workers.push(tokio::spawn(worker(shared)));
    }
    for handle in workers {
        if let Err(error) = handle.await {
            shared.fail(DownloadError::Other(format!(
                "worker task terminated unexpectedly: {error}"
            )));
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
        shared.events.emit(DownloadEvent::Retrying {
            chunk: 0,
            attempt: 0,
            delay_ms: 0,
            reason: "server ignored Range for a chunk request; falling back to a single stream"
                .to_string(),
        });
        let _ = tokio::fs::remove_file(&ctx.state_path).await;
        return single(ctx, Some(total), false).await;
    }

    if let Some(error) = shared.take_failure() {
        shared.save_force().await;
        return Err(error);
    }

    finish_chunked(&ctx, &shared).await
}

async fn finish_chunked(
    ctx: &Context,
    shared: &Arc<ChunkTransfer>,
) -> Result<DownloadSummary, DownloadError> {
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
        shared.http_version(),
    );
    ctx.events.completed(&summary);
    Ok(summary)
}

async fn saver_loop(shared: Arc<ChunkTransfer>, done: Arc<Notify>) {
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

async fn worker(shared: Arc<ChunkTransfer>) {
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

    loop {
        if shared.is_stopped() {
            return;
        }
        let index = shared.next.fetch_add(1, Ordering::SeqCst);
        if index >= shared.plan.total_chunks {
            return;
        }
        if shared.completed[index as usize].load(Ordering::SeqCst) {
            continue;
        }

        shared
            .counters
            .active_workers
            .fetch_add(1, Ordering::SeqCst);
        let outcome = fetch_chunk(&shared, &mut file, index).await;
        shared
            .counters
            .active_workers
            .fetch_sub(1, Ordering::SeqCst);

        match outcome {
            ChunkOutcome::Done => {
                shared.completed[index as usize].store(true, Ordering::SeqCst);
                shared
                    .completed_bytes
                    .fetch_add(shared.plan.chunk_len(index), Ordering::SeqCst);
                shared
                    .counters
                    .completed_chunks
                    .fetch_add(1, Ordering::SeqCst);
                shared.after_chunk().await;
            }
            ChunkOutcome::Stopped => return,
            ChunkOutcome::Downgrade => {
                shared.request_downgrade();
                return;
            }
            ChunkOutcome::Failed => return,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkOutcome {
    Done,
    Stopped,
    Downgrade,
    Failed,
}

enum AttemptError {
    Stopped,
    Downgrade,
    Retry {
        reason: String,
        retry_after: Option<Duration>,
        status: Option<u16>,
    },
    Fatal(DownloadError),
}

async fn fetch_chunk(
    shared: &Arc<ChunkTransfer>,
    file: &mut tokio::fs::File,
    index: u64,
) -> ChunkOutcome {
    let (start, end) = shared.plan.byte_range(index);
    let expected = end - start;
    let mut attempt: u32 = 0;

    loop {
        if shared.is_stopped() {
            return ChunkOutcome::Stopped;
        }
        attempt += 1;
        match attempt_chunk(shared, file, index, start, expected).await {
            Ok(()) => return ChunkOutcome::Done,
            Err((error, counted)) => {
                shared.counters.sub_downloaded(counted);
                match error {
                    AttemptError::Stopped => return ChunkOutcome::Stopped,
                    AttemptError::Downgrade => return ChunkOutcome::Downgrade,
                    AttemptError::Fatal(error) => {
                        shared.fail(error);
                        return ChunkOutcome::Failed;
                    }
                    AttemptError::Retry {
                        reason,
                        retry_after,
                        status,
                    } => {
                        if attempt >= MAX_ATTEMPTS {
                            shared.fail(match status {
                                Some(status) => DownloadError::Status {
                                    status,
                                    url: shared.url.clone(),
                                },
                                None => DownloadError::Http(format!(
                                    "chunk {index} failed after {attempt} attempts: {reason}"
                                )),
                            });
                            return ChunkOutcome::Failed;
                        }
                        let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                        shared.events.emit(DownloadEvent::Retrying {
                            chunk: index,
                            attempt,
                            delay_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
                            reason,
                        });
                        if !shared.controls.sleep_abortable(delay).await {
                            return ChunkOutcome::Stopped;
                        }
                    }
                }
            }
        }
    }
}

async fn attempt_chunk(
    shared: &Arc<ChunkTransfer>,
    file: &mut tokio::fs::File,
    index: u64,
    start: u64,
    expected: u64,
) -> Result<(), (AttemptError, u64)> {
    let end_inclusive = start + expected - 1;
    let range_value = format!("bytes={start}-{end_inclusive}");
    let request = shared.client.get(&shared.url).header(RANGE, range_value);

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

    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        shared.capture_http_version(response.version());
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
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
                    reason: format!("invalid Content-Range for chunk {index}"),
                    retry_after: None,
                    status: Some(status.as_u16()),
                },
                0,
            ));
        }
        return stream_chunk_body(shared, response, file, start, expected).await;
    }

    if status == StatusCode::OK {
        // Server ignored `Range`; the caller downgrades to a single stream.
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
        AttemptError::Fatal(DownloadError::Status {
            status: status.as_u16(),
            url: shared.url.clone(),
        }),
        0,
    ))
}

async fn stream_chunk_body(
    shared: &Arc<ChunkTransfer>,
    mut response: reqwest::Response,
    file: &mut tokio::fs::File,
    start: u64,
    expected: u64,
) -> Result<(), (AttemptError, u64)> {
    if let Err(error) = file.seek(std::io::SeekFrom::Start(start)).await {
        return Err((AttemptError::Fatal(DownloadError::Io(error)), 0));
    }

    let mut written = 0u64;
    loop {
        let chunk = tokio::select! {
            result = response.chunk() => result,
            _ = shared.controls.wait_stop() => return Err((AttemptError::Stopped, written)),
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
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
        let len = bytes.len() as u64;
        if written + len > expected {
            return Err((
                AttemptError::Retry {
                    reason: format!(
                        "server sent more bytes than requested for the chunk ({} > {expected})",
                        written + len
                    ),
                    retry_after: None,
                    status: None,
                },
                written,
            ));
        }
        if let Some(limiter) = &shared.limiter {
            limiter.acquire(len).await;
        }
        if shared.is_stopped() {
            return Err((AttemptError::Stopped, written));
        }
        if let Err(error) = file.write_all(&bytes).await {
            return Err((AttemptError::Fatal(DownloadError::Io(error)), written));
        }
        written += len;
        shared.counters.add_downloaded(len);
    }

    if written != expected {
        return Err((
            AttemptError::Retry {
                reason: format!("short body for chunk: {written}/{expected} bytes"),
                retry_after: None,
                status: None,
            },
            written,
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Single-stream transfer (no ranges, or server ignored Range)
// ---------------------------------------------------------------------------

enum SingleError {
    Stopped,
    Retry {
        reason: String,
        retry_after: Option<Duration>,
        status: Option<u16>,
    },
    Fatal(DownloadError),
}

async fn single(
    ctx: Context,
    total: Option<u64>,
    range_supported: bool,
) -> Result<DownloadSummary, DownloadError> {
    let counters = Arc::new(Counters::default());
    ctx.events.emit(DownloadEvent::Started {
        connections: 1,
        chunk_size: total.unwrap_or(0),
        total_chunks: u64::from(total.is_some()),
        resumed_chunks: 0,
    });
    let reporter = ProgressReporter::start(Arc::clone(&counters), total, ctx.events.clone());
    let controls = Controls::new(ctx.handle.clone());
    let watcher = controls.spawn_watcher();

    let mut attempt: u32 = 0;
    let http_version: Option<String>;
    loop {
        if let Some(outcome) = controls.control_outcome() {
            let downloaded = counters.downloaded();
            watcher.stop().await;
            reporter.stop().await;
            return Err(match outcome {
                ControlOutcome::Paused => DownloadError::Paused { downloaded },
                ControlOutcome::Cancelled => DownloadError::Cancelled { downloaded },
            });
        }
        attempt += 1;
        match single_attempt(&ctx, &counters, total, &controls).await {
            Ok(version) => {
                http_version = probe::version_string(version);
                break;
            }
            Err(SingleError::Stopped) => {
                let downloaded = counters.downloaded();
                watcher.stop().await;
                reporter.stop().await;
                let outcome = controls.control_outcome();
                return Err(match outcome {
                    Some(ControlOutcome::Cancelled) | None => {
                        DownloadError::Cancelled { downloaded }
                    }
                    Some(ControlOutcome::Paused) => DownloadError::Paused { downloaded },
                });
            }
            Err(SingleError::Fatal(error)) => {
                watcher.stop().await;
                reporter.stop().await;
                return Err(error);
            }
            Err(SingleError::Retry {
                reason,
                retry_after,
                status,
            }) => {
                if attempt >= MAX_ATTEMPTS {
                    watcher.stop().await;
                    reporter.stop().await;
                    return Err(match status {
                        Some(status) => DownloadError::Status {
                            status,
                            url: ctx.meta.final_url.clone(),
                        },
                        None => DownloadError::Http(format!(
                            "single-stream download failed after {attempt} attempts: {reason}"
                        )),
                    });
                }
                let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                ctx.events.emit(DownloadEvent::Retrying {
                    chunk: 0,
                    attempt,
                    delay_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
                    reason,
                });
                if !controls.sleep_abortable(delay).await {
                    continue;
                }
            }
        }
    }

    watcher.stop().await;
    reporter.stop().await;

    let downloaded = counters.downloaded();
    sync_all(&ctx.part).await?;
    let sha256 = verify_checksum(&ctx, &ctx.part).await?;
    tokio::fs::rename(&ctx.part, &ctx.target).await?;
    let _ = tokio::fs::remove_file(&ctx.state_path).await;
    let summary = ctx.summary(downloaded, sha256, 0, range_supported, 1, http_version);
    ctx.events.completed(&summary);
    Ok(summary)
}

async fn single_attempt(
    ctx: &Context,
    counters: &Arc<Counters>,
    total: Option<u64>,
    controls: &Controls,
) -> Result<reqwest::Version, SingleError> {
    let request = ctx.client.get(&ctx.meta.final_url);
    let response = tokio::select! {
        result = request.send() => result,
        _ = controls.wait_stop() => return Err(SingleError::Stopped),
    };
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            return Err(SingleError::Retry {
                reason: format!("request failed: {error}"),
                retry_after: None,
                status: None,
            })
        }
    };

    let status = response.status();
    if !status.is_success() {
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            return Err(SingleError::Retry {
                reason: format!("server returned HTTP {}", status.as_u16()),
                retry_after: retry_after_delay(response.headers()),
                status: Some(status.as_u16()),
            });
        }
        if is_retryable_status(status) {
            return Err(SingleError::Retry {
                reason: format!("server returned HTTP {}", status.as_u16()),
                retry_after: None,
                status: None,
            });
        }
        return Err(SingleError::Fatal(DownloadError::Status {
            status: status.as_u16(),
            url: ctx.meta.final_url.clone(),
        }));
    }

    let http_version = response.version();
    let mut file = tokio::fs::File::create(&ctx.part)
        .await
        .map_err(|error| SingleError::Fatal(DownloadError::Io(error)))?;
    counters.set_downloaded(0);
    counters.completed_chunks.store(0, Ordering::Relaxed);

    loop {
        let chunk = tokio::select! {
            result = response.chunk() => result,
            _ = controls.wait_stop() => return Err(SingleError::Stopped),
        };
        let bytes = match chunk {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(error) => {
                return Err(SingleError::Retry {
                    reason: format!("body read failed: {error}"),
                    retry_after: None,
                    status: None,
                })
            }
        };
        let len = bytes.len() as u64;
        if let Some(expected) = total {
            if counters.downloaded() + len > expected {
                return Err(SingleError::Retry {
                    reason: format!("server sent more than the advertised {expected} bytes"),
                    retry_after: None,
                    status: None,
                });
            }
        }
        if let Some(limiter) = &ctx.limiter {
            limiter.acquire(len).await;
        }
        if controls.stopped() {
            return Err(SingleError::Stopped);
        }
        if let Err(error) = file.write_all(&bytes).await {
            return Err(SingleError::Fatal(DownloadError::Io(error)));
        }
        counters.add_downloaded(len);
    }

    if let Some(expected) = total {
        if counters.downloaded() != expected {
            return Err(SingleError::Retry {
                reason: format!(
                    "single-stream length mismatch: got {} of {expected} bytes",
                    counters.downloaded()
                ),
                retry_after: None,
                status: None,
            });
        }
    }
    Ok(http_version)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn prepare_part(part: &Path, total: u64) -> Result<(), DownloadError> {
    // Deliberately do not truncate: a resumed `.part` keeps its completed
    // chunks, and `set_len` normalises the length either way.
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)
        .await?;
    file.set_len(total).await?;
    Ok(())
}

async fn sync_all(path: &Path) -> Result<(), DownloadError> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await?;
    file.sync_all().await?;
    Ok(())
}

async fn verify_checksum(ctx: &Context, path: &Path) -> Result<Option<String>, DownloadError> {
    let Some(expected) = ctx.request.expected_sha256.as_deref() else {
        return Ok(None);
    };
    let actual = sha256_file(path).await?;
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        // Never leave a complete-looking-but-corrupt payload behind: drop the
        // `.part` payload *and* the state bitmap (which would otherwise call it
        // complete on the next run). The caller only learns the expected and
        // actual digests from the error.
        let _ = tokio::fs::remove_file(path).await;
        let _ = tokio::fs::remove_file(&ctx.state_path).await;
        return Err(DownloadError::ChecksumMismatch {
            expected: expected.trim().to_ascii_lowercase(),
            actual,
        });
    }
    Ok(Some(actual))
}

async fn sha256_file(path: &Path) -> Result<String, DownloadError> {
    use sha2::{Digest, Sha256};

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Exponential backoff with ±20 % jitter, capped at 32 s.
fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    let exponential = BASE_BACKOFF.saturating_mul(1u32 << shift).min(MAX_BACKOFF);
    let jitter = 0.8 + rand::random::<f64>() * 0.4;
    Duration::from_secs_f64((exponential.as_secs_f64() * jitter).max(0.05))
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    let wait = date
        .duration_since(std::time::SystemTime::now())
        .unwrap_or(Duration::ZERO);
    Some(wait.min(MAX_RETRY_AFTER))
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 500 | 502 | 504)
}
