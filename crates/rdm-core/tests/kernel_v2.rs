//! Kernel v2 regression tests: admission gating, pushback pause, wide start,
//! and the interaction between dynamic planning and the local rate limiter.
//!
//! The deterministic fixture server is the source of truth; tests assert byte
//! equality and externally observable behaviour (events, elapsed time, request
//! counts), never internal controller state.

#![allow(clippy::await_holding_lock)] // heavy_guard serializes whole async tests on purpose

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdm_core::{download, download_with_handle, DownloadEvent, DownloadHandle, DownloadRequest};
use rdm_fixture_server::{Config, TestServer};
use tokio::sync::mpsc;

/// Heavy byte-level tests each stream 8–32 MiB. Running all of them at once
/// makes timing-sensitive assertions (truncation counters, pause granularity,
/// split timing) flaky on busy machines, so they serialize on this mutex.
static HEAVY: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn heavy_guard() -> std::sync::MutexGuard<'static, ()> {
    HEAVY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rdm-kernel-v2-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn request_for(server: &TestServer, dir: &Path, connections: usize) -> DownloadRequest {
    let mut request = DownloadRequest::new(server.file_url());
    request.output_dir = Some(dir.to_path_buf());
    request.connections = connections;
    request
}

fn assert_payload_eq(path: &Path, expected: &[u8], context: &str) {
    let actual = std::fs::read(path)
        .unwrap_or_else(|error| panic!("{context}: could not read {}: {error}", path.display()));
    assert_eq!(actual, expected, "{context}: payload mismatch");
}

fn concurrency_events(events: &[DownloadEvent]) -> Vec<(usize, usize)> {
    events
        .iter()
        .filter_map(|event| match event {
            DownloadEvent::Concurrency { target, active } => Some((*target, *active)),
            _ => None,
        })
        .collect()
}

fn retry_events(events: &[DownloadEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, DownloadEvent::Retrying { .. }))
        .count()
}

async fn download_collecting(
    request: DownloadRequest,
) -> (
    Result<rdm_core::DownloadSummary, rdm_core::DownloadError>,
    Vec<DownloadEvent>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = download_with_handle(request, Some(tx), DownloadHandle::new()).await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events)
}

/// A 429 with `Retry-After: 1` must pause the origin, not just the chunk that
/// was rejected: no worker may issue a new attempt before the advertised wait.
#[tokio::test]
async fn pushback_pauses_the_origin_and_halves_the_target() {
    rdm_core::testing::reset_origin_budgets();
    let total = 4 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_retry_after(2, 1))
        .await
        .unwrap();
    let dir = TempDir::new("pushback");
    let mut request = request_for(&server, dir.path(), 4);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = true;

    let started = Instant::now();
    let (result, events) = download_collecting(request).await;
    let elapsed = started.elapsed();
    let summary = result.expect("download must recover from 429");

    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert!(
        elapsed >= Duration::from_millis(900),
        "Retry-After: 1 must be honoured (elapsed {elapsed:?})"
    );
    assert!(
        retry_events(&events) >= 2,
        "both injected 429s must be retried, events: {:?}",
        retry_events(&events)
    );
    let plan = concurrency_events(&events);
    assert!(
        plan.iter().any(|(target, _)| *target < 4),
        "pushback must shrink the plan, plan: {plan:?}"
    );
    let controller = summary
        .controller
        .as_ref()
        .expect("adaptive transfer must expose controller stats");
    assert!(
        controller.pushbacks >= 2,
        "both 429s must reach the controller: {controller:?}"
    );
    assert!(controller.final_target < 4, "{controller:?}");
    assert!(controller.min_target >= 1, "{controller:?}");
    assert!(
        controller.min_target <= controller.final_target,
        "{controller:?}"
    );

    server.shutdown().await;
}

/// The process-wide origin budget learns from one task's 429s and clamps later
/// tasks on the same origin, so GUI/batch downloads stop re-learning the limit
/// and re-triggering a 429 storm.
#[tokio::test]
async fn learned_origin_budget_prevents_repeat_429_storms() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 16 * 1024 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_concurrency_limit(4)
            .with_reject_retry_after(1)
            .with_shared_rate(16 * 1024 * 1024),
    )
    .await
    .unwrap();

    // Task A learns the limit the hard way.
    let dir_a = TempDir::new("origin-a");
    let mut first = request_for(&server, dir_a.path(), 8);
    first.dynamic_connections = true;
    let a = download(first, None).await.expect("first download");
    assert_payload_eq(&a.path, &server.content(), "first payload");
    let after_a = server.stats();
    assert!(
        after_a.rejected_concurrency > 0,
        "A must trip the limit: {after_a:?}"
    );

    // Task B on the same origin inherits the learned cap. It plans 8 but the
    // shared budget + gate clamp it to the learned 4; a fixture handler for a
    // stolen/aborted request can still overlap for a moment, so a small
    // residual number of 429s is acceptable — far below A's storm.
    tokio::time::sleep(Duration::from_millis(300)).await;
    server.reset_stats();
    let dir_b = TempDir::new("origin-b");
    let mut second = request_for(&server, dir_b.path(), 8);
    second.dynamic_connections = true;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let b = download_with_handle(second, Some(tx), DownloadHandle::new())
        .await
        .expect("second download");
    assert_payload_eq(&b.path, &server.content(), "second payload");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let initial_target = events
        .iter()
        .find_map(|event| match event {
            DownloadEvent::Concurrency { target, .. } => Some(*target),
            _ => None,
        })
        .expect("B must emit an initial concurrency target");
    assert!(
        initial_target <= 4,
        "B must warm-start at the learned cap instead of re-discovering 8: {initial_target}"
    );
    let after_b = server.stats();
    assert!(
        after_b.rejected_concurrency < after_a.rejected_concurrency,
        "learned budget must cut the 429 storm: before={} after={}",
        after_a.rejected_concurrency,
        after_b.rejected_concurrency
    );
    assert!(
        after_b.rejected_concurrency <= 2,
        "at most one abort/steal overlap pair may still race the limit: {after_b:?}"
    );
    assert!(
        b.peak_connections <= 4,
        "B must clamp to the learned cap, peak={}",
        b.peak_connections
    );
    let controller = b
        .controller
        .as_ref()
        .expect("adaptive transfer must expose controller stats");
    assert!(
        controller.pushbacks <= 2,
        "B should not re-learn the limit: {controller:?}"
    );

    server.shutdown().await;
}

/// A single-stream download must also honour a positive `Retry-After` (the
/// legacy retry loop slept, but this pins the behaviour down).
#[tokio::test]
async fn single_stream_honours_positive_retry_after() {
    rdm_core::testing::reset_origin_budgets();
    let total = 1024 * 1024u64;
    // Ranges ignored => single-stream path; first GET is rejected with a
    // one-second Retry-After.
    let mut config = Config::new(total).with_retry_after(1, 1);
    config.supports_ranges = false;
    let server = TestServer::start(config).await.unwrap();
    let dir = TempDir::new("single-retry");
    let mut request = request_for(&server, dir.path(), 1);
    request.file_name = Some("single.bin".to_string());

    let started = Instant::now();
    let summary = download(request, None).await.expect("single stream");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert!(
        elapsed >= Duration::from_millis(900),
        "Retry-After: 1 must be honoured in single-stream mode (elapsed {elapsed:?})"
    );
    assert!(
        summary.controller.is_none(),
        "single-stream transfers have no adaptive controller"
    );
    assert!(!summary.range_supported);

    server.shutdown().await;
}

/// Large files start with the widest sensible plan: on a capable link there is
/// nothing to discover, so no ramp is needed. Small files keep the conservative
/// mid-range start.
#[tokio::test]
async fn large_files_start_wide_but_small_files_do_not() {
    let total = 32 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("wide");
    let mut request = request_for(&server, dir.path(), 8);
    request.dynamic_connections = true;
    let (result, events) = download_collecting(request).await;
    let summary = result.expect("wide download");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    let plan = concurrency_events(&events);
    assert_eq!(plan[0], (8, 0), "large files start wide, plan: {plan:?}");
    assert!(
        summary.peak_connections >= 4,
        "peak {}",
        summary.peak_connections
    );

    let small_total = 8 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(small_total)).await.unwrap();
    let dir = TempDir::new("narrow");
    let mut request = request_for(&server, dir.path(), 8);
    request.dynamic_connections = true;
    let (result, events) = download_collecting(request).await;
    let summary = result.expect("narrow download");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    let plan = concurrency_events(&events);
    assert_eq!(
        plan[0],
        (4, 0),
        "mid-size files below the wide-start threshold keep climbing from 4, plan: {plan:?}"
    );

    server.shutdown().await;
}

/// A user bandwidth limit makes the network concurrency irrelevant: dynamic
/// probing must not fight the token bucket.
#[tokio::test]
async fn rate_limiter_disables_dynamic_probing() {
    let total = 2 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("limited");
    let mut request = request_for(&server, dir.path(), 4);
    request.speed_limit_bps = Some(1024 * 1024);
    request.dynamic_connections = true;
    let (result, events) = download_collecting(request).await;
    let summary = result.expect("rate limited download");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    let plan = concurrency_events(&events);
    assert_eq!(
        plan,
        vec![(4, 0)],
        "no adaptive events under a local limit, plan: {plan:?}"
    );
    server.shutdown().await;
}

/// Adjacent pending chunks are merged into fewer Range requests; the file must
/// still be covered exactly once at byte level.
#[tokio::test]
async fn adjacent_chunks_are_coalesced_into_fewer_requests() {
    let _heavy = heavy_guard();
    let total = 32 * 1024 * 1024u64;
    let chunk = 1024 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("coalesce");
    // One connection keeps byte accounting exact (no steal can waste a suffix).
    let mut request = request_for(&server, dir.path(), 1);
    request.chunk_size = Some(chunk);
    request.dynamic_connections = false;
    let summary = download(request, None).await.expect("coalesced download");
    assert_payload_eq(&summary.path, &server.content(), "coalesced payload");
    let stats = server.stats();
    assert_eq!(
        stats.body_bytes_sent, total,
        "coalescing must not duplicate or skip bytes"
    );
    assert!(
        stats.get_requests < total / chunk,
        "expected fewer than {} ranged GETs, got {}: {:?}",
        total / chunk,
        stats.get_requests,
        stats.range_headers
    );
    assert_eq!(stats.responses_206, stats.get_requests);
    server.shutdown().await;
}

/// A tiny file is only a handful of chunks: it finishes before the controller
/// could ever probe, so all chunks must be in flight from the start.
#[tokio::test]
async fn tiny_files_put_all_chunks_in_flight_immediately() {
    let total = 1024 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("tiny");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(256 * 1024); // exactly four chunks
    request.dynamic_connections = true;

    let (result, events) = download_collecting(request).await;
    let summary = result.expect("tiny download");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    let plan = concurrency_events(&events);
    assert_eq!(plan[0], (4, 0), "four chunks, four slots, plan: {plan:?}");
    assert_eq!(summary.peak_connections, 4, "all chunks must run at once");
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Byte-range scheduler: in-flight tail stealing and v2 resume state
// ---------------------------------------------------------------------------

/// The tail of the payload is served 16x slower by the fixture. With fixed
/// 4 MiB chunks only the last chunks are slow and every other worker is idle,
/// so the scheduler must split those tail segments and finish them in parallel.
#[tokio::test]
async fn tail_stealing_splits_slow_segments() {
    let total = 64 * 1024 * 1024u64;
    let tail = total / 10;
    let server = TestServer::start(
        Config::new(total)
            .with_shared_rate(64 * 1024 * 1024)
            .with_per_connection_rate(8 * 1024 * 1024)
            .with_slow_tail(tail)
            .with_slow_tail_factor(16),
    )
    .await
    .unwrap();
    let dir = TempDir::new("tail-steal");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(4 * 1024 * 1024); // 16 base segments
    request.dynamic_connections = false; // isolate stealing from controller churn
    let started = std::time::Instant::now();
    let summary = download(request, None).await.expect("tail download");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &server.content(), "payload");

    let stats = server.stats();
    let base_segments = total.div_ceil(4 * 1024 * 1024);
    assert!(
        stats.range_headers.len() as u64 > base_segments,
        "expected tail splits beyond {base_segments} base segments, saw {} requests",
        stats.range_headers.len()
    );
    // 6.4 MiB of slow tail: without splitting only 2 workers can serve it at
    // ~0.5 MiB/s each (≈6 s); with splitting it is a few seconds at most.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "tail took {elapsed:?}; stealing may not be working"
    );
    assert!(summary.peak_connections <= 8);

    server.shutdown().await;
}

/// Resume after a pause that happened while the tail was being split.
///
/// The v2 state records arbitrary byte ranges, so a pause in the middle of a
/// split segment must resume only the missing complement and still produce a
/// byte-identical file.
#[tokio::test]
async fn resume_after_tail_split_preserves_data() {
    let _heavy = heavy_guard();
    let total = 32 * 1024 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_shared_rate(32 * 1024 * 1024)
            .with_per_connection_rate(8 * 1024 * 1024)
            .with_slow_tail(total / 5)
            .with_slow_tail_factor(16),
    )
    .await
    .unwrap();
    let dir = TempDir::new("tail-resume");
    // Two connections keep the pause deterministic: at 2/3 progress at most two
    // 4 MiB claims can be in flight, so earlier claims are already complete.
    let mut request = request_for(&server, dir.path(), 2);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(4 * 1024 * 1024);
    request.dynamic_connections = false;

    let handle = DownloadHandle::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task_handle = handle.clone();
    let task_request = request.clone();
    let task =
        tokio::spawn(
            async move { download_with_handle(task_request, Some(tx), task_handle).await },
        );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Some(DownloadEvent::Progress { downloaded, .. }) if downloaded >= total * 2 / 3 => {
                    break
                }
                Some(_) => continue,
                None => panic!("download finished before it could be paused"),
            }
        }
    })
    .await
    .expect("timed out waiting for progress");
    handle.pause();
    let paused = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("pause did not finish")
        .expect("download task panicked");
    match paused {
        Err(rdm_core::DownloadError::Paused { downloaded }) => {
            assert!(downloaded > 0 && downloaded < total)
        }
        other => panic!("expected Paused, got {other:?}"),
    }

    let resumed = download(request, None).await.expect("resume");
    assert_payload_eq(&resumed.path, &server.content(), "resumed payload");
    assert_eq!(resumed.total_bytes, total);
    // Some work was already complete, so the resume must not be a full restart.
    assert!(
        resumed.resumed_chunks > 0,
        "resume should reuse completed chunks, summary: {resumed:?}"
    );
    server.shutdown().await;
}

/// A paused download already has total size and validators in its state file;
/// resuming must not pay another HEAD probe. Correctness against a changed
/// object is still guarded by `If-Range` on every ranged request.
#[tokio::test]
async fn resume_after_pause_skips_the_head_probe() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 8 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_shared_rate(8 * 1024 * 1024))
        .await
        .unwrap();
    let dir = TempDir::new("probe-free-resume");
    let mut request = request_for(&server, dir.path(), 2);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;

    let handle = DownloadHandle::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task_handle = handle.clone();
    let task_request = request.clone();
    let task =
        tokio::spawn(
            async move { download_with_handle(task_request, Some(tx), task_handle).await },
        );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Some(DownloadEvent::Progress { downloaded, .. }) if downloaded >= total / 4 => {
                    break
                }
                Some(_) => continue,
                None => panic!("download finished before it could be paused"),
            }
        }
    })
    .await
    .expect("timed out waiting for progress");
    handle.pause();
    let paused = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("pause did not finish")
        .expect("download task panicked");
    assert!(
        matches!(paused, Err(rdm_core::DownloadError::Paused { .. })),
        "expected Paused, got {paused:?}"
    );
    assert_eq!(server.stats().head_requests, 1);

    let resumed = download(request, None).await.expect("resume");
    assert_payload_eq(&resumed.path, &server.content(), "resumed payload");
    assert!(
        resumed.resumed_chunks > 0,
        "resume should reuse completed chunks, summary: {resumed:?}"
    );
    assert_eq!(
        server.stats().head_requests,
        1,
        "resume must reuse the stored metadata instead of another HEAD probe"
    );
    server.shutdown().await;
}

/// A deterministic straggler (the first payload GET is served at 1/16 rate)
/// would block a 4 MiB range for ~8 s if it stayed on one connection. The
/// scheduler must split it so idle workers finish the range in parallel.
#[tokio::test]
async fn straggler_request_is_split_across_idle_workers() {
    let _heavy = heavy_guard();
    let total = 16 * 1024 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_shared_rate(64 * 1024 * 1024)
            .with_per_connection_rate(8 * 1024 * 1024)
            .with_slow_request_ordinals([2])
            .with_slow_request_factor(16),
    )
    .await
    .unwrap();
    let dir = TempDir::new("straggler");
    let mut request = request_for(&server, dir.path(), 4);
    request.chunk_size = Some(4 * 1024 * 1024); // 4 deterministic segments
    request.dynamic_connections = false; // isolate stealing

    let started = std::time::Instant::now();
    let summary = download(request, None).await.expect("straggler download");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &server.content(), "payload");

    let stats = server.stats();
    assert!(
        stats.range_headers.len() as u64 > 4,
        "expected the slow range to be split, saw {} range requests",
        stats.range_headers.len()
    );
    // The slow body alone is 4 MiB at 0.5 MiB/s ≈ 8 s; splitting lets idle
    // workers take most of it, so the whole file must finish well before.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "straggler not split: {elapsed:?}"
    );
    server.shutdown().await;
}

/// Probe-free resume must still detect a changed object: the cached ETag is
/// sent as `If-Range`, the server answers 200, and the engine restarts as one
/// full stream without trusting the cached total.
#[tokio::test]
async fn probe_free_resume_does_not_trust_a_changed_object() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 4 * 1024 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_etag("\"v1\"")
            .with_shared_rate(8 * 1024 * 1024),
    )
    .await
    .unwrap();
    let dir = TempDir::new("probe-free-changed");
    let mut request = request_for(&server, dir.path(), 2);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;

    let handle = DownloadHandle::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task_handle = handle.clone();
    let task_request = request.clone();
    let task =
        tokio::spawn(
            async move { download_with_handle(task_request, Some(tx), task_handle).await },
        );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Some(DownloadEvent::Progress { downloaded, .. }) if downloaded >= total / 4 => {
                    break
                }
                Some(_) => continue,
                None => panic!("download finished before it could be paused"),
            }
        }
    })
    .await
    .expect("timed out waiting for progress");
    handle.pause();
    let paused = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("pause did not finish")
        .expect("download task panicked");
    assert!(matches!(
        paused,
        Err(rdm_core::DownloadError::Paused { .. })
    ));
    let head_requests = server.stats().head_requests;
    server.set_etag(Some("\"v2\"")).await;

    let resumed = download(request, None).await.expect("resume after change");
    assert_payload_eq(&resumed.path, &server.content(), "payload");
    assert!(
        !resumed.range_supported,
        "a changed object must restart as a plain full stream"
    );
    assert_eq!(
        server.stats().head_requests,
        head_requests,
        "the resume itself must not probe"
    );
    server.shutdown().await;
}

/// Coalescing must not defeat stealing: the first request is a coalesced 4 MiB
/// claim served at 1/16 rate; idle workers must split it after the fast claims
/// are done instead of waiting ~8 s for the slow body.
#[tokio::test]
async fn coalesced_claim_is_stolen_from_a_slow_connection() {
    let _heavy = heavy_guard();
    let total = 32 * 1024 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_shared_rate(64 * 1024 * 1024)
            .with_per_connection_rate(8 * 1024 * 1024)
            .with_slow_request_ordinals([2])
            .with_slow_request_factor(16),
    )
    .await
    .unwrap();
    let dir = TempDir::new("coalesce-steal");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;

    let started = std::time::Instant::now();
    let summary = download(request, None)
        .await
        .expect("coalesced straggler download");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &server.content(), "payload");
    let stats = server.stats();
    // A steal suffix starts at the owner's split point, which is not aligned to
    // a 1 MiB base chunk; coalesced claim/request ranges are aligned.
    let chunk = 1024 * 1024u64;
    let split = stats.range_headers.iter().any(|raw| {
        let Some(rest) = raw.strip_prefix("bytes=") else {
            return false;
        };
        let Some((start, _)) = rest.split_once('-') else {
            return false;
        };
        let Ok(start) = start.parse::<u64>() else {
            return false;
        };
        start > 0 && start % chunk != 0
    });
    assert!(
        split,
        "coalesced slow claim must be split, saw {:?}",
        stats.range_headers
    );
    // The slow 4 MiB claim alone would take ~8 s; stealing must hide most of it.
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "coalesced claim not stolen: {elapsed:?}"
    );
    server.shutdown().await;
}

/// Pushback while requests are coalesced: a 429 aborts a whole coalesced Range
/// without sending body bytes; the retry must still cover the file exactly once
/// and honour `Retry-After`.
#[tokio::test]
async fn coalesced_requests_survive_pushback_without_duplicate_bytes() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 32 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_retry_after(2, 1))
        .await
        .unwrap();
    let dir = TempDir::new("coalesce-pushback");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;

    let started = std::time::Instant::now();
    let summary = download(request, None)
        .await
        .expect("coalesced pushback download");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert!(
        elapsed >= Duration::from_millis(900),
        "Retry-After: 1 must be honoured (elapsed {elapsed:?})"
    );
    let stats = server.stats();
    assert_eq!(stats.responses_429, 2, "both injected 429s must be retried");
    // A 429 carries no body, so the payload itself must not be resent. The
    // fixture still counts bytes written by handlers of stolen/aborted ranges
    // (their sockets are closed late), so allow a small bounded overlap.
    assert!(
        stats.body_bytes_sent >= total,
        "payload bytes are missing: {} < {total}",
        stats.body_bytes_sent
    );
    assert!(
        stats.body_bytes_sent < total + 4 * 1024 * 1024,
        "429 retries must not re-download payload bytes: sent {} vs total {total}",
        stats.body_bytes_sent
    );
    server.shutdown().await;
}

/// A truncated coalesced request must resume from its verified prefix: the
/// retry Range starts inside the coalesced claim, not at offset 0.
#[tokio::test]
async fn truncated_coalesced_claim_resumes_from_prefix() {
    let _heavy = heavy_guard();
    let total = 32 * 1024 * 1024u64;
    let chunk = 1024 * 1024u64;
    let cut = 1536 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_truncated_body(cut, [2])
            .with_shared_rate(64 * 1024 * 1024),
    )
    .await
    .unwrap();
    let dir = TempDir::new("coalesce-truncated");
    let mut request = request_for(&server, dir.path(), 2);
    request.chunk_size = Some(chunk);
    request.dynamic_connections = false;

    let summary = download(request, None)
        .await
        .expect("truncated coalesced download");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    let stats = server.stats();
    assert_eq!(stats.truncated_responses, 1, "exactly one truncated body");
    assert!(
        stats.get_requests < total / chunk,
        "coalescing should still cut the request count, got {}",
        stats.get_requests
    );
    assert!(
        stats.body_bytes_sent < total + cut,
        "sub-chunk resume must not re-download verified bytes: sent {} vs total {total} + cut {cut}",
        stats.body_bytes_sent
    );
    // At least one retry starts inside the first coalesced claim (its base
    // chunks are 1 MiB, the cut is 1.5 MiB).
    let resumed_inside = stats.range_headers.iter().any(|raw| {
        let Some(rest) = raw.strip_prefix("bytes=") else {
            return false;
        };
        let Some((start, _)) = rest.split_once('-') else {
            return false;
        };
        let Ok(start) = start.parse::<u64>() else {
            return false;
        };
        start > 0 && start < cut + chunk && start % chunk != 0
    });
    assert!(resumed_inside, "no suffix retry: {:?}", stats.range_headers);

    server.shutdown().await;
}

/// A dropped connection mid-body must resume from the verified prefix, not
/// re-download the whole range. The fixture truncates the first payload GET
/// after 1.5 MiB; a clean full-range retry would send `total + cut` bytes,
/// while a sub-chunk resume re-sends at most the bytes the client did not read.
#[tokio::test]
async fn truncated_body_resumes_from_verified_prefix() {
    let _heavy = heavy_guard();
    let total = 8 * 1024 * 1024u64;
    let chunk = 2 * 1024 * 1024u64;
    let cut = 1536 * 1024u64;
    let server = TestServer::start(Config::new(total).with_truncated_body(cut, [2]))
        .await
        .unwrap();
    let dir = TempDir::new("truncated-resume");
    let mut request = request_for(&server, dir.path(), 2);
    request.chunk_size = Some(chunk);
    request.dynamic_connections = false;

    let summary = download(request, None).await.expect("truncated download");
    assert_payload_eq(&summary.path, &server.content(), "payload");

    let stats = server.stats();
    assert_eq!(stats.truncated_responses, 1, "exactly one truncated body");
    // A restart-from-zero retry would add the whole `cut` bytes back. A resume
    // adds only the bytes lost in the aborted connection (< cut strictly).
    assert!(
        stats.body_bytes_sent < total + cut,
        "sub-chunk resume must not re-download verified bytes: sent {} vs total {total} + cut {cut}",
        stats.body_bytes_sent
    );
    // At least one retry asked for a suffix that starts inside a base chunk.
    let resumed = stats.range_headers.iter().any(|raw| {
        let Some(rest) = raw.strip_prefix("bytes=") else {
            return false;
        };
        let Some((start, _)) = rest.split_once('-') else {
            return false;
        };
        let Ok(start) = start.parse::<u64>() else {
            return false;
        };
        let into_chunk = start % chunk;
        start < total && into_chunk != 0 && into_chunk <= cut
    });
    assert!(
        resumed,
        "expected a resumed Range request inside the truncated chunk, got {:?}",
        stats.range_headers
    );
    server.shutdown().await;
}

/// `If-Range` with a strong ETag: when the object changes during the transfer,
/// the server answers with the full representation (200) and the engine must
/// restart as a single stream instead of stitching old and new bytes together.
#[tokio::test]
async fn changed_etag_mid_transfer_restarts_as_single_stream() {
    let total = 8 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_etag("\"v1\"").with_delay(100))
        .await
        .unwrap();
    let dir = TempDir::new("if-range");
    let mut request = request_for(&server, dir.path(), 4);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(1024 * 1024); // 8 chunks, 2 waves on 4 workers

    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        download_with_handle(request, Some(tx), DownloadHandle::new()).await
    });
    // Change the representation once the first wave is on the wire.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(DownloadEvent::Progress { downloaded, .. }) if downloaded > 0 => break,
                Some(_) => continue,
                None => panic!("download finished before it could be changed"),
            }
        }
    })
    .await
    .expect("timed out waiting for progress");
    server.set_etag(Some("\"v2\"")).await;

    let result = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("download did not finish")
        .expect("download task panicked");
    let summary = result.expect("download must recover by restarting as one stream");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert!(
        !summary.range_supported,
        "a changed object must downgrade to the single-stream path"
    );
    let mut fallback_seen = false;
    while let Ok(event) = rx.try_recv() {
        if let DownloadEvent::Retrying { reason, .. } = event {
            if reason.contains("single stream") {
                fallback_seen = true;
            }
        }
    }
    assert!(
        fallback_seen,
        "expected the Range-ignored fallback event after the ETag change"
    );
    server.shutdown().await;
}

/// A server that advertises range support but answers some valid `206` heads
/// with an empty body must not be retried forever: after two such responses the
/// engine falls back to one full stream and does not trust the advertised
/// total. Real jsDelivr edges exhibited this exact failure mode.
#[tokio::test]
async fn empty_range_bodies_fall_back_to_single_full_stream() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 8 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_empty_range_bodies([2, 3]))
        .await
        .unwrap();
    let dir = TempDir::new("empty-range-fallback");
    let mut request = request_for(&server, dir.path(), 4);
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;

    let summary = download(request, None)
        .await
        .expect("empty range bodies must trigger a single-stream fallback");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert_eq!(summary.total_bytes, total);
    assert!(
        !summary.range_supported,
        "the fallback stream has no verified range metadata"
    );
    let stats = server.stats();
    assert_eq!(
        stats.empty_range_bodies, 2,
        "both injected empty range bodies must be observed before falling back"
    );
    assert_eq!(stats.head_requests, 1);
    assert!(
        stats.body_bytes_sent >= total,
        "the full payload must be streamed once: {}",
        stats.body_bytes_sent
    );
    server.shutdown().await;
}

/// A half-open connection (server goes silent without closing) must be
/// abandoned by the adaptive stall detector in a few seconds, not by the flat
/// 15 s timeout. The retry resumes from the received prefix.
#[tokio::test]
async fn adaptive_stall_detector_recovers_a_half_open_connection() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 4 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_stalled_body(128 * 1024, [2]))
        .await
        .unwrap();
    let dir = TempDir::new("stall-detector");
    let mut request = request_for(&server, dir.path(), 1);
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;
    request.file_name = Some("stalled.bin".to_string());

    let started = Instant::now();
    let summary = download(request, None)
        .await
        .expect("a stalled connection must be retried, not failed");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert_eq!(summary.total_bytes, total);
    assert!(
        elapsed < Duration::from_secs(8),
        "adaptive stall detection must beat the flat 15 s timeout (elapsed {elapsed:?})"
    );
    assert_eq!(server.stats().stalled_responses, 1);
    server.shutdown().await;
}

/// Mirrors are additional origins serving the same bytes. With two 4 MiB/s
/// origins, an 8 MiB transfer should finish in roughly half the single-origin
/// time and both servers must actually serve part of the file.
#[tokio::test]
async fn mirrors_split_the_transfer_across_origins() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 8 * 1024 * 1024u64;
    let primary = TestServer::start(Config::new(total).with_shared_rate(4 * 1024 * 1024))
        .await
        .unwrap();
    let mirror = TestServer::start(Config::new(total).with_shared_rate(4 * 1024 * 1024))
        .await
        .unwrap();
    assert_eq!(primary.content(), mirror.content(), "fixtures must agree");

    let single_dir = TempDir::new("mirror-single");
    let mut single = request_for(&primary, single_dir.path(), 8);
    single.chunk_size = Some(1024 * 1024);
    single.dynamic_connections = false;
    let started = Instant::now();
    let single_summary = download(single, None).await.expect("single origin");
    let single_elapsed = started.elapsed();
    assert_payload_eq(&single_summary.path, &primary.content(), "single payload");

    let mirror_dir = TempDir::new("mirror-multi");
    let mut multi = request_for(&primary, mirror_dir.path(), 8);
    multi.chunk_size = Some(1024 * 1024);
    multi.dynamic_connections = false;
    multi.mirrors = vec![mirror.file_url()];
    let started = Instant::now();
    let multi_summary = download(multi, None).await.expect("mirrored download");
    let multi_elapsed = started.elapsed();
    assert_payload_eq(&multi_summary.path, &primary.content(), "mirror payload");

    let primary_bytes = primary.stats().body_bytes_sent;
    let mirror_bytes = mirror.stats().body_bytes_sent;
    assert!(
        primary_bytes >= total / 4 && mirror_bytes >= total / 4,
        "both origins must serve part of the file: primary={primary_bytes} mirror={mirror_bytes}"
    );
    assert!(
        multi_elapsed < single_elapsed.mul_f64(0.85),
        "two origins should halve the shared-cap time: single={single_elapsed:?} multi={multi_elapsed:?}"
    );
    primary.shutdown().await;
    mirror.shutdown().await;
}

/// A dead mirror must be abandoned and the transfer must finish on the
/// primary: workers rotate sources after connection-level failures.
#[tokio::test]
async fn dead_mirror_rotates_back_to_the_primary() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 4 * 1024 * 1024u64;
    let primary = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("mirror-dead");
    let mut request = request_for(&primary, dir.path(), 8);
    request.chunk_size = Some(512 * 1024);
    request.dynamic_connections = false;
    request.mirrors = vec!["http://127.0.0.1:1/dead.bin".to_string()];

    let started = Instant::now();
    let summary = download(request, None)
        .await
        .expect("a dead mirror must not fail the transfer");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &primary.content(), "payload");
    assert!(
        elapsed < Duration::from_secs(10),
        "fallback took too long: {elapsed:?}"
    );
    assert!(primary.stats().body_bytes_sent >= total);
    primary.shutdown().await;
}

/// A mirror that ignores Range must be rotated away instead of triggering the
/// whole-transfer single-stream downgrade that a primary would cause.
#[tokio::test]
async fn mirror_ignoring_range_does_not_downgrade_the_transfer() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 8 * 1024 * 1024u64;
    let primary = TestServer::start(Config::new(total)).await.unwrap();
    let mirror = TestServer::start(Config::new(total).without_ranges())
        .await
        .unwrap();
    let dir = TempDir::new("mirror-no-range");
    let mut request = request_for(&primary, dir.path(), 8);
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;
    request.mirrors = vec![mirror.file_url()];

    let summary = download(request, None).await.expect("mirror rotation");
    assert_payload_eq(&summary.path, &primary.content(), "payload");
    assert!(
        summary.range_supported,
        "the primary still ranges, so the transfer must stay chunked"
    );
    primary.shutdown().await;
    mirror.shutdown().await;
}

/// Static source assignment would let a slow mirror hold half the claims and
/// dominate wall-clock; adaptive per-claim water-filling (plus stealing) must
/// shift the bulk of the remaining ranges to the fast origin.
#[tokio::test]
async fn adaptive_mirror_selection_avoids_a_slow_origin() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 16 * 1024 * 1024u64;
    let fast = TestServer::start(Config::new(total).with_shared_rate(16 * 1024 * 1024))
        .await
        .unwrap();
    let slow = TestServer::start(Config::new(total).with_shared_rate(2 * 1024 * 1024))
        .await
        .unwrap();
    assert_eq!(fast.content(), slow.content(), "fixtures must agree");
    let dir = TempDir::new("mirror-adaptive");
    let mut request = request_for(&fast, dir.path(), 8);
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;
    request.mirrors = vec![slow.file_url()];

    let started = Instant::now();
    let summary = download(request, None)
        .await
        .expect("adaptive mirror download");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &fast.content(), "payload");
    let fast_bytes = fast.stats().body_bytes_sent;
    let slow_bytes = slow.stats().body_bytes_sent;
    assert!(slow_bytes > 0, "the slow origin must be used at least once");
    assert!(
        slow_bytes < total / 2,
        "adaptive selection must shift most ranges to the fast origin: fast={fast_bytes} slow={slow_bytes}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "a slow mirror must not dominate the transfer: {elapsed:?} (fast={fast_bytes} slow={slow_bytes})"
    );
    fast.shutdown().await;
    slow.shutdown().await;
}

/// A stale mirror (404) must be dropped and the transfer must finish on the
/// primary instead of failing the whole download.
#[tokio::test]
async fn stale_mirror_404_rotates_to_the_primary() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 4 * 1024 * 1024u64;
    let primary = TestServer::start(Config::new(total)).await.unwrap();
    let stale = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("mirror-404");
    let mut request = request_for(&primary, dir.path(), 8);
    request.chunk_size = Some(512 * 1024);
    request.dynamic_connections = false;
    request.mirrors = vec![stale.url("/missing.bin")];

    let started = Instant::now();
    let summary = download(request, None)
        .await
        .expect("a stale mirror must not fail the transfer");
    let elapsed = started.elapsed();
    assert_payload_eq(&summary.path, &primary.content(), "payload");
    assert!(
        stale.stats().responses_404 > 0,
        "the stale mirror must have been attempted"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "mounting a stale mirror must fail over quickly: {elapsed:?}"
    );
    assert!(primary.stats().body_bytes_sent >= total);
    primary.shutdown().await;
    stale.shutdown().await;
}

/// `prefer_http2` only *allows* multiplexing; a plain-HTTP fixture has no h2,
/// so the transfer must fall back to HTTP/1.1 with identical bytes.
#[tokio::test]
async fn prefer_http2_falls_back_to_h1_on_a_plain_http_fixture() {
    let _heavy = heavy_guard();
    rdm_core::testing::reset_origin_budgets();
    let total = 4 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("prefer-http2");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(512 * 1024);
    request.dynamic_connections = false;
    request.prefer_http2 = true;

    let summary = download(request, None).await.expect("h2 flag on h1 origin");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert!(summary.range_supported);
    assert_eq!(
        summary.http_version.as_deref(),
        Some("HTTP/1.1"),
        "plain HTTP has no h2 handshake; the engine must continue over h1"
    );
    server.shutdown().await;
}
