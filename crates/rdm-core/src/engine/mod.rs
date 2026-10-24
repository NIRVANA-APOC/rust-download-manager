//! Kernel v2 orchestration: probe → dispatch → chunked or single-stream →
//! completion. This module owns the transfer context, pause/cancel plumbing,
//! metadata probing dispatch and the streaming fallback; the multi-connection
//! engine lives in [`chunked`] and the adaptive policy in [`controller`].

pub(crate) mod chunked;
pub(crate) mod controller;
pub(crate) mod origin;
pub(crate) mod scheduler;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::RETRY_AFTER;
use reqwest::{Client, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::client;
use crate::event::{Counters, EventSink, ProgressReporter};
use crate::filename;
use crate::probe;
use crate::rate::TokenBucket;
use crate::state;
use crate::{
    ControllerStats, DownloadError, DownloadEvent, DownloadHandle, DownloadRequest,
    DownloadSummary, RemoteMeta,
};

/// Total attempts per chunk / single stream.
pub(crate) const MAX_ATTEMPTS: u32 = 6;
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(32);
/// Upper bound for a server-provided `Retry-After`.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
/// Polling cadence for external pause/cancel flags.
const CONTROL_POLL: Duration = Duration::from_millis(50);
/// Persist resume state at least this often while chunks complete.
pub(crate) const SAVE_INTERVAL: Duration = Duration::from_millis(500);
/// ...and immediately every N completed chunks.
pub(crate) const SAVE_EVERY_CHUNKS: u64 = 16;
/// Saver tick.
pub(crate) const SAVER_TICK: Duration = Duration::from_millis(200);
/// A parked worker re-checks the gate at this cadence.
pub(crate) const GATE_POLL: Duration = Duration::from_millis(10);
/// Maximum gap between two body chunks before an attempt is considered stalled
/// and retried on a fresh connection.
pub(crate) const STALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Floor for the adaptive stall timeout: on a fast link a dead connection is
/// detected in ~3 s instead of waiting the full 15 s.
pub(crate) const MIN_STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Adaptive inter-chunk stall detector.
///
/// Real transfers often have very small gaps between chunks; waiting a flat
/// 15 s after a peer goes silent wastes that time on every mid-transfer stall.
/// The detector tracks an EWMA of observed gaps and uses `6 × gap` clamped to
/// `[3 s, 15 s]`: fast links fail over quickly, slow but healthy links keep a
/// wide window so no legitimate pause is mistaken for a stall.
#[derive(Debug, Default)]
pub(crate) struct StallDetector {
    gap: Option<Duration>,
}

impl StallDetector {
    pub(crate) fn timeout(&self) -> Duration {
        match self.gap {
            None => STALL_TIMEOUT,
            Some(gap) => (gap * 6).clamp(MIN_STALL_TIMEOUT, STALL_TIMEOUT),
        }
    }

    pub(crate) fn observe(&mut self, gap: Duration) {
        self.gap = Some(match self.gap {
            None => gap,
            Some(previous) => previous.mul_f64(0.7) + gap.mul_f64(0.3),
        });
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub(crate) async fn run(
    request: DownloadRequest,
    events: EventSink,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    run_entry(request, None, events, handle).await
}

pub(crate) async fn run_with_meta(
    request: DownloadRequest,
    meta: RemoteMeta,
    events: EventSink,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    run_entry(request, Some(meta), events, handle).await
}

async fn run_entry(
    request: DownloadRequest,
    preset_meta: Option<RemoteMeta>,
    events: EventSink,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    let sink = events.clone();
    let result = run_inner(request, preset_meta, events, handle).await;
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
    preset_meta: Option<RemoteMeta>,
    events: EventSink,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    let started = Instant::now();
    if handle.is_cancelled() {
        return Err(DownloadError::Cancelled { downloaded: 0 });
    }

    let strict_meta = preset_meta.is_some();
    if !strict_meta {
        events.emit(DownloadEvent::Probing);
    }
    // Multi-connection transfers normally force HTTP/1.1 for their ranged GETs
    // so each worker owns a real TCP congestion window. `prefer_http2` opts into
    // multiplexed h2 (one TLS connection, many streams) for servers that cap
    // connections. Probing with the *same* client lets the probe's TCP/TLS
    // connection be reused by the first chunk instead of opening a second pool.
    let http1 = !request.single_stream && request.connections > 1 && !request.prefer_http2;
    let http = if http1 {
        client::build_http1(&request)?
    } else {
        client::build(&request)?
    };
    let mut meta_from_cache = false;
    let meta = match preset_meta {
        Some(meta) => {
            events.emit(DownloadEvent::Metadata(meta.clone()));
            meta
        }
        None => {
            // A paused download already knows everything the HEAD probe would
            // return; skipping it saves a full round trip on resume.
            if let Some(cached) = probe_free_resume_meta(&request).await {
                tracing::debug!("resuming from cached metadata; skipping HEAD probe");
                meta_from_cache = true;
                events.emit(DownloadEvent::Metadata(cached.clone()));
                cached
            } else {
                let meta = probe::probe_with(&http, &request).await?;
                events.emit(DownloadEvent::Metadata(meta.clone()));
                meta
            }
        }
    };
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
        strict_meta,
        meta_from_cache,
    };

    match ctx.meta.total_bytes {
        Some(total) if ctx.meta.supports_ranges && !ctx.request.single_stream => {
            chunked::run(ctx, total).await
        }
        Some(total) => single(ctx, Some(total), false).await,
        None => single(ctx, None, false).await,
    }
}

// ---------------------------------------------------------------------------
// Shared context
// ---------------------------------------------------------------------------

/// Reconstruct the probe metadata from an existing `.part.json`, so a resumed
/// download does not pay another HEAD round trip.
///
/// Only exact-URL matches whose `.part` file still exists are accepted; any
/// suspicious state (corrupt, wrong URL, changed name) falls back to a live
/// probe. The cached validator still guards correctness: each range request
/// carries `If-Range`, so a changed object downgrades to a plain full stream
/// (and does not trust the cached total).
async fn probe_free_resume_meta(request: &DownloadRequest) -> Option<RemoteMeta> {
    if !request.resume || request.single_stream {
        return None;
    }
    let dir = request
        .output_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(name) = &request.file_name {
        let target = dir.join(filename::sanitize(name));
        candidates.push(state::state_path(&target));
    } else {
        let mut entries = tokio::fs::read_dir(&dir).await.ok()?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.to_string_lossy().ends_with(".part.json") {
                candidates.push(path);
            }
        }
    }
    for state_path in candidates {
        let Some(saved) = state::load(&state_path).await else {
            continue;
        };
        if saved.url != request.url || saved.final_url.is_empty() || saved.total_bytes == 0 {
            continue;
        }
        if saved.completed_byte_ranges().is_none() {
            continue;
        }
        if let Some(requested) = request.file_name.as_deref() {
            if requested != saved.file_name {
                continue;
            }
        }
        let state_string = state_path.to_string_lossy();
        let Some(target) = state_string.strip_suffix(".part.json") else {
            continue;
        };
        if !state::part_path(Path::new(target)).exists() {
            continue;
        }
        return Some(RemoteMeta {
            final_url: saved.final_url,
            file_name: saved.file_name,
            total_bytes: Some(saved.total_bytes),
            supports_ranges: true,
            content_type: None,
            etag: saved.etag,
            last_modified: saved.last_modified,
            http_version: None,
        });
    }
    None
}

pub(crate) struct Context {
    pub(crate) request: DownloadRequest,
    pub(crate) client: Client,
    pub(crate) meta: RemoteMeta,
    pub(crate) target: PathBuf,
    pub(crate) file_name: String,
    pub(crate) part: PathBuf,
    pub(crate) state_path: PathBuf,
    pub(crate) events: EventSink,
    pub(crate) handle: DownloadHandle,
    pub(crate) limiter: Option<Arc<TokenBucket>>,
    pub(crate) started: Instant,
    /// `meta` came from the caller: a transfer that disagrees with its
    /// `total_bytes` is a changed remote, not a retryable protocol oddity.
    pub(crate) strict_meta: bool,
    /// `meta` was reconstructed from a resume state instead of a live probe.
    /// If the server then ignores `Range` (e.g. the object changed), the
    /// fallback stream must not trust the cached total.
    pub(crate) meta_from_cache: bool,
}

impl Context {
    pub(crate) fn connections(&self) -> usize {
        self.request.connections.max(1)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn summary(
        &self,
        total_bytes: u64,
        sha256: Option<String>,
        resumed_chunks: u64,
        range_supported: bool,
        connections: usize,
        peak_connections: usize,
        http_version: Option<String>,
        controller: Option<ControllerStats>,
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
            peak_connections: peak_connections.min(u32::MAX as usize) as u32,
            sha256,
            resumed_chunks,
            range_supported,
            http_version,
            controller,
        }
    }
}

// ---------------------------------------------------------------------------
// Pause / cancel plumbing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlOutcome {
    Paused,
    Cancelled,
}

/// Watch channel for internal stop requests (downgrade, fatal error) plus a
/// polling watcher that turns external [`DownloadHandle`] flags into stop
/// notifications so in-flight reads abort promptly.
#[derive(Clone)]
pub(crate) struct Controls {
    handle: DownloadHandle,
    stop: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
}

impl Controls {
    pub(crate) fn new(handle: DownloadHandle) -> Self {
        let (stop, stop_rx) = watch::channel(false);
        Self {
            handle,
            stop,
            stop_rx,
        }
    }

    pub(crate) fn request_stop(&self) {
        if !*self.stop.borrow() {
            let _ = self.stop.send(true);
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        self.handle.is_paused() || self.handle.is_cancelled() || *self.stop.borrow()
    }

    pub(crate) fn control_outcome(&self) -> Option<ControlOutcome> {
        if self.handle.is_cancelled() {
            Some(ControlOutcome::Cancelled)
        } else if self.handle.is_paused() {
            Some(ControlOutcome::Paused)
        } else {
            None
        }
    }

    pub(crate) async fn wait_stop(&self) {
        let mut rx = self.stop_rx.clone();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }

    /// Sleep that returns `false` as soon as a stop is requested.
    pub(crate) async fn sleep_abortable(&self, duration: Duration) -> bool {
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

    pub(crate) fn spawn_watcher(&self) -> ControlWatcher {
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

pub(crate) struct ControlWatcher {
    done: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl ControlWatcher {
    pub(crate) async fn stop(mut self) {
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
// Single-stream transfer (no ranges, server ignored Range, unknown length)
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

pub(crate) async fn single(
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
                    Some(ControlOutcome::Paused) => DownloadError::Paused { downloaded },
                    _ => DownloadError::Cancelled { downloaded },
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
    let summary = ctx.summary(
        downloaded,
        sha256,
        0,
        range_supported,
        1,
        1,
        http_version,
        None,
    );
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
    // Caller-verified metadata: the advertised length is a contract. Check it
    // before touching the `.part` file so a changed remote never truncates a
    // previous payload.
    if ctx.strict_meta && status == StatusCode::OK {
        if let (Some(expected), Some(advertised)) = (total, response.content_length()) {
            if advertised != expected {
                return Err(SingleError::Fatal(DownloadError::RemoteChanged));
            }
        }
    }
    let mut file = tokio::fs::File::create(&ctx.part)
        .await
        .map_err(|error| SingleError::Fatal(DownloadError::Io(error)))?;
    counters.set_downloaded(0);
    counters
        .completed_chunks
        .store(0, std::sync::atomic::Ordering::Relaxed);

    let mut write_buffer = Vec::with_capacity(crate::engine::WRITE_BUFFER_BYTES);
    let mut received = 0u64;
    let mut stall = StallDetector::default();
    loop {
        let stall_limit = stall.timeout();
        let last_chunk = std::time::Instant::now();
        let chunk = tokio::select! {
            result = timeout(stall_limit, response.chunk()) => match result {
                Ok(result) => result,
                Err(_) => {
                    return Err(SingleError::Retry {
                        reason: format!(
                            "no response body data for {}s (stalled connection)",
                            stall_limit.as_secs()
                        ),
                        retry_after: None,
                        status: None,
                    })
                }
            },
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
        stall.observe(last_chunk.elapsed());
        let len = bytes.len() as u64;
        received += len;
        counters.add_net_rx(len);
        if let Some(expected) = total {
            if received > expected {
                if ctx.strict_meta {
                    return Err(SingleError::Fatal(DownloadError::RemoteChanged));
                }
                return Err(SingleError::Retry {
                    reason: format!("server sent more than the advertised {expected} bytes"),
                    retry_after: None,
                    status: None,
                });
            }
        }
        if let Some(limiter) = &ctx.limiter {
            tokio::select! {
                _ = limiter.acquire(len) => {}
                _ = controls.wait_stop() => return Err(SingleError::Stopped),
            }
        }
        if controls.stopped() {
            return Err(SingleError::Stopped);
        }
        write_buffer.extend_from_slice(&bytes);
        if write_buffer.len() >= crate::engine::WRITE_BUFFER_BYTES {
            if let Err(error) = file.write_all(&write_buffer).await {
                return Err(SingleError::Fatal(DownloadError::Io(error)));
            }
            counters.add_downloaded(write_buffer.len() as u64);
            write_buffer.clear();
        }
    }
    if !write_buffer.is_empty() {
        if let Err(error) = file.write_all(&write_buffer).await {
            return Err(SingleError::Fatal(DownloadError::Io(error)));
        }
        counters.add_downloaded(write_buffer.len() as u64);
    }

    if let Some(expected) = total {
        if counters.downloaded() != expected {
            if ctx.strict_meta {
                return Err(SingleError::Fatal(DownloadError::RemoteChanged));
            }
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
// Helpers shared with the chunked engine
// ---------------------------------------------------------------------------

/// Write coalescing threshold: fewer, larger positioned writes.
pub(crate) const WRITE_BUFFER_BYTES: usize = 256 * 1024;

pub(crate) async fn prepare_part(part: &Path, total: u64) -> Result<(), DownloadError> {
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

pub(crate) async fn sync_all(path: &Path) -> Result<(), DownloadError> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await?;
    file.sync_all().await?;
    Ok(())
}

pub(crate) async fn verify_checksum(
    ctx: &Context,
    path: &Path,
) -> Result<Option<String>, DownloadError> {
    let Some(expected) = ctx.request.expected_sha256.as_deref() else {
        return Ok(None);
    };
    let actual = sha256_file(path).await?;
    if !actual.eq_ignore_ascii_case(expected.trim()) {
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
pub(crate) fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    let exponential = BASE_BACKOFF.saturating_mul(1u32 << shift).min(MAX_BACKOFF);
    let jitter = 0.8 + rand::random::<f64>() * 0.4;
    Duration::from_secs_f64((exponential.as_secs_f64() * jitter).max(0.05))
}

pub(crate) fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
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

pub(crate) fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 500 | 502 | 504)
}
