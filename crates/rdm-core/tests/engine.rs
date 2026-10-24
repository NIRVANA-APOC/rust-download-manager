//! End-to-end engine tests against the deterministic local fixture server.
//!
//! Covers the nine-item correctness checklist from `docs/architecture.md` §3.6
//! plus probing edge cases (redirects, missing `Accept-Ranges`, unknown length,
//! Content-Disposition filenames).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdm_core::{
    download, download_with_handle, probe, DownloadError, DownloadEvent, DownloadHandle,
    DownloadRequest, DownloadSummary, RemoteMeta,
};
use rdm_fixture_server::{Config, TestServer};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Self-cleaning temporary directory (removed even when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rdm-core-test-{tag}-{}-{nanos}",
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

fn read_state(dir: &Path, target_name: &str) -> serde_json::Value {
    let path = dir.join(format!("{target_name}.part.json"));
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
    serde_json::from_slice(&bytes).expect("resume state is valid JSON")
}

/// Base chunk ids that are fully covered by the persisted resume state.
///
/// The kernel writes v2 states (`completed_ranges`); states written by older
/// versions carry `completed_chunks` instead. Both are accepted here so the
/// resume tests keep asserting the same public semantics.
fn completed_chunks(state: &serde_json::Value) -> Vec<u64> {
    let chunk_size = state["chunk_size"].as_u64().expect("chunk_size");
    let total_chunks = state["total_chunks"].as_u64().expect("total_chunks");
    let total_bytes = state["total_bytes"].as_u64().expect("total_bytes");
    let ranges: Vec<(u64, u64)> = match state["completed_ranges"].as_array() {
        Some(ranges) if !ranges.is_empty() => ranges
            .iter()
            .map(|pair| {
                let pair = pair.as_array().expect("range pair");
                (pair[0].as_u64().unwrap(), pair[1].as_u64().unwrap())
            })
            .collect(),
        _ => state["completed_chunks"]
            .as_array()
            .expect("completed_chunks array")
            .iter()
            .map(|value| {
                let index = value.as_u64().expect("chunk index");
                let start = index * chunk_size;
                (start, (start + chunk_size).min(total_bytes))
            })
            .collect(),
    };
    (0..total_chunks)
        .filter(|index| {
            let start = index * chunk_size;
            let end = (start + chunk_size).min(total_bytes);
            ranges.iter().any(|&(s, e)| s <= start && e >= end)
        })
        .collect()
}

fn range_start(raw: &str) -> u64 {
    raw.trim()
        .strip_prefix("bytes=")
        .and_then(|rest| rest.split('-').next())
        .and_then(|start| start.parse().ok())
        .unwrap_or_else(|| panic!("unexpected Range header `{raw}`"))
}

async fn probe_meta(server: &TestServer) -> RemoteMeta {
    probe(&DownloadRequest::new(server.file_url()))
        .await
        .expect("probe succeeds")
}

/// Compare a downloaded file with the fixture payload without dumping
/// megabytes of bytes into the test output on mismatch.
fn assert_payload_eq(path: &Path, expected: &[u8], context: &str) {
    let actual = std::fs::read(path)
        .unwrap_or_else(|error| panic!("{context}: could not read {}: {error}", path.display()));
    if actual == expected {
        return;
    }
    let limit = actual.len().min(expected.len());
    let at = (0..limit)
        .find(|index| actual[*index] != expected[*index])
        .unwrap_or(limit);
    let start = at.saturating_sub(8);
    let actual_slice = &actual[start..(at + 8).min(actual.len())];
    let expected_slice = &expected[start..(at + 8).min(expected.len())];
    panic!(
        "{context}: payload mismatch at byte {at} (actual {} bytes, expected {} bytes); \
         actual {:02x?} vs expected {:02x?}",
        actual.len(),
        expected.len(),
        actual_slice,
        expected_slice
    );
}

/// Wait until the fixture stops observing traffic, then return its counters.
///
/// After a pause the client drops in-flight responses; the server tasks may
/// still flush a few buffered bytes to half-closed sockets. Any byte-level
/// delta assertion must be taken *after* that settles, otherwise leftover
/// writes from the paused run leak into the resumed run's delta.
async fn settled_stats(server: &TestServer) -> rdm_fixture_server::Stats {
    let mut last = server.stats();
    let mut stable = 0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let now = server.stats();
        let quiet = now.body_bytes_sent == last.body_bytes_sent
            && now.requests == last.requests
            && now.range_headers.len() == last.range_headers.len();
        if quiet {
            stable += 1;
            if stable >= 3 {
                return now;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    server.stats()
}

// ---------------------------------------------------------------------------
// §3.6.1 — multi-connection output equals single-connection output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_connection_matches_single_connection() {
    let total = 6 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let expected = server.content();

    let single_dir = TempDir::new("single");
    let mut single_request = request_for(&server, single_dir.path(), 1);
    single_request.chunk_size = Some(512 * 1024);
    let single = download(single_request, None)
        .await
        .expect("single download");

    let multi_dir = TempDir::new("multi");
    let mut multi_request = request_for(&server, multi_dir.path(), 8);
    multi_request.chunk_size = Some(512 * 1024);
    let multi = download(multi_request, None).await.expect("multi download");

    assert_payload_eq(&single.path, &expected, "single-connection");
    assert_payload_eq(&multi.path, &expected, "multi-connection");
    assert_eq!(single.total_bytes, total);
    assert_eq!(multi.total_bytes, total);
    assert_eq!(single.connections, 1);
    assert_eq!(multi.connections, 8);
    assert!(single.range_supported && multi.range_supported);

    // Chunked mode must really run on HTTP/1.1 (h2 would multiplex all range
    // requests over one TCP connection and destroy the speedup).
    let version = multi
        .http_version
        .as_deref()
        .expect("summary reports the protocol");
    assert!(version.contains("1.1"), "expected HTTP/1.1, got {version}");

    let stats = server.stats();
    assert!(
        stats.responses_206 >= 24,
        "expected >= 24 ranged responses (12 + 12), got {}",
        stats.responses_206
    );
    assert!(
        stats
            .http_versions
            .iter()
            .all(|version| version == "HTTP/1.1"),
        "fixture saw unexpected protocols: {:?}",
        stats.http_versions
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.2 — server ignores Range → transparent fallback, correct file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn range_ignored_falls_back_to_single_stream() {
    let total = 2 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).without_ranges())
        .await
        .unwrap();
    let dir = TempDir::new("no-range");
    let mut request = request_for(&server, dir.path(), 4);
    request.chunk_size = Some(256 * 1024);

    let summary = download(request, None).await.expect("fallback download");
    assert!(
        !summary.range_supported,
        "fallback must be reported as range_supported=false"
    );
    assert_eq!(summary.connections, 1);
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert_eq!(summary.total_bytes, total);

    let stats = server.stats();
    assert_eq!(stats.responses_206, 0, "no ranged response expected");
    assert!(stats.responses_200 >= 1);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.3 — pause keeps verified in-flight prefixes
// ---------------------------------------------------------------------------

/// Pausing must not throw away megabytes that were already flushed and verified
/// by their `Content-Range`; with 4 MiB coalesced claims a worker can be deep
/// into a claim when the pause arrives.
#[tokio::test]
async fn pause_persists_verified_prefixes_of_in_flight_claims() {
    let total = 32 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_shared_rate(16 * 1024 * 1024))
        .await
        .unwrap();
    let dir = TempDir::new("pause-prefix");
    let mut request = request_for(&server, dir.path(), 4);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(1024 * 1024);
    request.dynamic_connections = false;

    let (paused, state) = pause_midway(request, total / 4, "payload.bin", dir.path()).await;
    assert!(paused > 0);
    let retained = state["downloaded_bytes"].as_u64().unwrap_or(0);
    assert!(
        retained > 0,
        "in-flight verified prefixes must survive a pause: {state}"
    );
    let ranges = state["completed_ranges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !ranges.is_empty(),
        "pause must persist at least one completed byte range: {state}"
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.3 — resume after pause re-downloads *only* missing chunks
// ---------------------------------------------------------------------------

/// Start `request`, wait until progress passes `pause_after`, pause, and
/// return the paused byte count plus the persisted resume state.
async fn pause_midway(
    request: DownloadRequest,
    pause_after: u64,
    target_name: &str,
    dir: &Path,
) -> (u64, serde_json::Value) {
    let handle = DownloadHandle::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task_handle = handle.clone();
    let task =
        tokio::spawn(async move { download_with_handle(request, Some(tx), task_handle).await });

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Some(DownloadEvent::Progress { downloaded, .. }) if downloaded >= pause_after => {
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
    let result = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("pause did not finish in time")
        .expect("download task panicked");

    let paused = match result {
        Err(DownloadError::Paused { downloaded }) => downloaded,
        other => panic!("expected DownloadError::Paused, got {other:?}"),
    };
    let state = read_state(dir, target_name);
    (paused, state)
}

#[tokio::test]
async fn resume_only_downloads_missing_chunks() {
    let total = 8 * 1024 * 1024u64;
    let chunk_size = 1024 * 1024u64;
    // 40 ms per 256 KiB granule => ~160 ms per chunk, leaving time to pause.
    let server = TestServer::start(Config::new(total).with_delay(40))
        .await
        .unwrap();
    let dir = TempDir::new("resume");
    let mut request = request_for(&server, dir.path(), 2);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(chunk_size);

    let (paused, state) =
        pause_midway(request.clone(), 1024 * 1024, "payload.bin", dir.path()).await;
    let completed = completed_chunks(&state);
    assert!(
        !completed.is_empty(),
        "expected at least one completed chunk before pausing"
    );
    assert!(paused > 0 && paused < total);
    assert_eq!(
        state["downloaded_bytes"].as_u64().unwrap(),
        paused,
        "state downloaded_bytes must match the paused counter"
    );
    // The partial file is pre-allocated to the full length.
    let part_len = std::fs::metadata(dir.path().join("payload.bin.part"))
        .unwrap()
        .len();
    assert_eq!(part_len, total);

    let before = settled_stats(&server).await;
    let resumed = download(request, None).await.expect("resume download");
    let after = settled_stats(&server).await;

    assert_payload_eq(&resumed.path, &server.content(), "resumed");
    assert_eq!(resumed.resumed_chunks, completed.len() as u64);
    assert!(resumed.range_supported);

    // Byte-exact assertion: only the missing chunks may hit the wire again.
    let missing = total - paused;
    let delta = after.body_bytes_sent - before.body_bytes_sent;
    assert_eq!(
        delta, missing,
        "resume re-downloaded {delta} bytes but only {missing} were missing"
    );

    let new_ranges = &after.range_headers[before.range_headers.len()..];
    let expected_missing_chunks = total.div_ceil(chunk_size) - completed.len() as u64;
    assert_eq!(
        new_ranges.len() as u64,
        expected_missing_chunks,
        "expected exactly one range request per missing chunk"
    );
    for raw in new_ranges {
        let start = range_start(raw);
        let index = start / chunk_size;
        assert!(
            !completed.contains(&index),
            "already-complete chunk {index} was re-requested"
        );
    }

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.4 — ETag change invalidates resume and restarts from zero
// ---------------------------------------------------------------------------

#[tokio::test]
async fn etag_change_invalidates_resume() {
    let total = 8 * 1024 * 1024u64;
    let chunk_size = 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_delay(40).with_etag("\"v1\""))
        .await
        .unwrap();
    let dir = TempDir::new("etag");
    let mut request = request_for(&server, dir.path(), 2);
    request.file_name = Some("payload.bin".to_string());
    request.chunk_size = Some(chunk_size);

    let (paused, state) =
        pause_midway(request.clone(), 1024 * 1024, "payload.bin", dir.path()).await;
    assert!(!completed_chunks(&state).is_empty());

    // The file "changed" on the server between the two runs.
    server.set_etag(Some("\"v2\"")).await;

    let before = settled_stats(&server).await;
    let resumed = download(request, None)
        .await
        .expect("restart after ETag change");
    let after = settled_stats(&server).await;

    assert_eq!(resumed.resumed_chunks, 0, "resume must be discarded");
    assert_eq!(std::fs::read(&resumed.path).unwrap(), server.content());
    // Probe-free resume discovers the changed validator on the first ranged
    // request: the server answers 200 and the engine restarts as one stream.
    // The fixture counts bytes written by handlers whose client aborts (the
    // extra concurrent 200 bodies), so allow a small bounded overlap.
    let served = after.body_bytes_sent - before.body_bytes_sent;
    assert!(
        served >= total,
        "a validator mismatch must re-download the whole file ({served}/{total})"
    );
    assert!(
        served < total + 512 * 1024,
        "validator mismatch wasted too many bytes: {served} vs {total}"
    );
    let new_ranges = &after.range_headers[before.range_headers.len()..];
    let new_gets = after.get_requests - before.get_requests;
    // Either the restart's first range starts at 0, or the probe-free resume
    // discovered the change via a rangeless full GET (which also covers 0).
    assert!(
        new_ranges.iter().any(|raw| range_start(raw) == 0) || new_gets as usize > new_ranges.len(),
        "restart must cover offset 0 (ranges={new_ranges:?}, gets={new_gets})"
    );
    assert!(
        after.body_bytes_sent - before.body_bytes_sent > paused,
        "restart must not trust the stale bitmap"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.5 — checksum mismatch: error, no rename, no leftover corrupt payload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checksum_mismatch_leaves_no_files() {
    let server = TestServer::start(Config::new(512 * 1024)).await.unwrap();
    let dir = TempDir::new("checksum");
    let mut request = request_for(&server, dir.path(), 4);
    request.file_name = Some("checksum.bin".to_string());
    request.expected_sha256 = Some("0".repeat(64));

    let error = download(request, None)
        .await
        .expect_err("checksum must fail");
    match &error {
        DownloadError::ChecksumMismatch { expected, actual } => {
            assert_eq!(expected, &"0".repeat(64));
            assert_eq!(actual.len(), 64);
        }
        other => panic!("expected ChecksumMismatch, got {other:?}"),
    }

    let target = dir.path().join("checksum.bin");
    assert!(!target.exists(), "mismatched payload must not be renamed");
    assert!(
        !dir.path().join("checksum.bin.part").exists(),
        "corrupt payload must be deleted, not left looking complete"
    );
    assert!(
        !dir.path().join("checksum.bin.part.json").exists(),
        "corrupt file must not leave a 'complete' resume state"
    );
    // Nothing else may linger in the output directory either.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "checksum failure should leave no files behind, found {leftovers:?}"
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.6 — 8 connections clearly faster than 1 on a throttled fixture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrency_beats_single_connection() {
    let total = 4 * 1024 * 1024u64;
    // 20 ms per 256 KiB granule (≈ 12.5 MiB/s per connection).
    let server = TestServer::start(Config::new(total).with_delay(20))
        .await
        .unwrap();

    let single_dir = TempDir::new("speed-single");
    let single_request = request_for(&server, single_dir.path(), 1);
    let started = Instant::now();
    let single = download(single_request, None)
        .await
        .expect("single download");
    let single_elapsed = started.elapsed();

    let multi_dir = TempDir::new("speed-multi");
    let multi_request = request_for(&server, multi_dir.path(), 8);
    let started = Instant::now();
    let multi = download(multi_request, None).await.expect("multi download");
    let multi_elapsed = started.elapsed();

    assert_payload_eq(&single.path, &server.content(), "single");
    assert_payload_eq(&multi.path, &server.content(), "multi");
    assert_eq!(single.connections, 1);
    assert_eq!(multi.connections, 8);

    eprintln!("concurrency: 1 connection = {single_elapsed:?}, 8 connections = {multi_elapsed:?}");
    // Wide threshold to stay robust on a busy CI machine; expected ratio ~5-8x.
    assert!(
        multi_elapsed.as_secs_f64() * 1.8 < single_elapsed.as_secs_f64(),
        "8 connections ({multi_elapsed:?}) should be at least 1.8x faster than 1 ({single_elapsed:?})"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.7 — 429 + Retry-After retry path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retries_429_with_retry_after() {
    let server = TestServer::start(Config::new(1024 * 1024).with_retry_after(2, 0))
        .await
        .unwrap();
    let dir = TempDir::new("retry");
    let mut request = request_for(&server, dir.path(), 2);
    request.chunk_size = Some(256 * 1024);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let summary = download_with_handle(request, Some(tx), DownloadHandle::new())
        .await
        .expect("download should recover from 429");
    assert_payload_eq(&summary.path, &server.content(), "payload");

    let mut retries = 0u32;
    let mut retry_reason = String::new();
    while let Ok(event) = rx.try_recv() {
        if let DownloadEvent::Retrying {
            attempt, reason, ..
        } = event
        {
            retries += 1;
            assert!(attempt >= 1);
            retry_reason = reason;
        }
    }
    assert_eq!(retries, 2, "two injected 429 responses should be retried");
    assert!(
        retry_reason.contains("429"),
        "retry reason should mention 429, got `{retry_reason}`"
    );
    assert!(server.stats().responses_429 >= 2);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.8 — filename parsing (`filename*` + UTF-8 + spaces)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_disposition_filename_is_used() {
    let server =
        TestServer::start(Config::new(2048).with_content_disposition(
            "attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87%20payload.bin",
        ))
        .await
        .unwrap();
    let dir = TempDir::new("filename");
    let request = request_for(&server, dir.path(), 1);

    let summary = download(request, None).await.expect("download");
    let name = summary
        .path
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, "中文 payload.bin");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §3.6.9 — global rate limit keeps the average under the configured budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn global_rate_limit_is_respected() {
    let total = 2 * 1024 * 1024u64;
    let limit = 1024 * 1024u64; // 1 MiB/s
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("rate");
    let mut request = request_for(&server, dir.path(), 4);
    request.chunk_size = Some(256 * 1024);
    request.speed_limit_bps = Some(limit);

    let started = Instant::now();
    let summary = download(request, None)
        .await
        .expect("rate limited download");
    let elapsed = started.elapsed();

    assert_payload_eq(&summary.path, &server.content(), "payload");
    // Loose upper bound (1.5x) as allowed by the architecture checklist.
    assert!(
        summary.avg_speed_bps <= limit as f64 * 1.5,
        "average speed {:.0} B/s exceeds 1.5x the configured {limit} B/s",
        summary.avg_speed_bps
    );
    assert!(
        elapsed >= Duration::from_millis(800),
        "transfer finished too fast ({elapsed:?}) to have applied the limit"
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Probe coverage: metadata, redirects, ranged fallback, unknown length
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_reports_metadata_and_follows_redirects() {
    let total = 64 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_etag("\"abc\"")
            .with_last_modified("Wed, 21 Oct 2015 07:28:00 GMT")
            .with_content_disposition("attachment; filename=\"report.bin\""),
    )
    .await
    .unwrap();

    let redirected = probe(&DownloadRequest::new(server.redirect_url()))
        .await
        .expect("probe through redirect");
    assert!(redirected.final_url.ends_with("/file"));
    assert_eq!(redirected.total_bytes, Some(total));
    assert!(redirected.supports_ranges);
    assert_eq!(redirected.file_name, "report.bin");
    assert_eq!(redirected.etag.as_deref(), Some("\"abc\""));
    assert_eq!(
        redirected.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert_eq!(
        redirected.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert!(
        redirected
            .http_version
            .as_deref()
            .unwrap_or_default()
            .contains("1.1"),
        "probe should report the HTTP version: {:?}",
        redirected.http_version
    );

    server.shutdown().await;
}

#[tokio::test]
async fn probe_falls_back_to_ranged_get_without_accept_ranges() {
    let total = 512 * 1024u64;
    let server = TestServer::start(Config::new(total).without_accept_ranges())
        .await
        .unwrap();

    let meta = probe_meta(&server).await;
    assert_eq!(meta.total_bytes, Some(total));
    assert!(
        meta.supports_ranges,
        "ranged GET fallback must detect range support"
    );

    let dir = TempDir::new("fallback-probe");
    let summary = download(request_for(&server, dir.path(), 4), None)
        .await
        .expect("chunked download after GET fallback");
    assert_payload_eq(&summary.path, &server.content(), "payload");
    assert!(summary.range_supported);
    server.shutdown().await;
}

#[tokio::test]
async fn unknown_length_uses_single_stream() {
    let total = 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).without_length().without_ranges())
        .await
        .unwrap();
    let meta = probe_meta(&server).await;
    assert_eq!(meta.total_bytes, None, "no Content-Length is advertised");
    assert!(!meta.supports_ranges);

    let dir = TempDir::new("unknown-length");
    let summary = download(request_for(&server, dir.path(), 4), None)
        .await
        .expect("unknown-length download");
    assert!(!summary.range_supported);
    assert_eq!(summary.connections, 1);
    assert_eq!(summary.total_bytes, total);
    assert_payload_eq(&summary.path, &server.content(), "payload");
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// §2 — dynamic concurrency planning
// ---------------------------------------------------------------------------

/// Run a download while draining its event channel, so assertions can inspect
/// `DownloadEvent::Concurrency` after completion.
async fn download_collecting(
    request: DownloadRequest,
) -> (Result<DownloadSummary, DownloadError>, Vec<DownloadEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = download_with_handle(request, Some(tx), DownloadHandle::new()).await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events)
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

#[tokio::test]
async fn dynamic_plan_emits_concurrency_and_respects_limit() {
    let total = 8 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_delay(5))
        .await
        .unwrap();
    let dir = TempDir::new("dynamic");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(256 * 1024); // 32 chunks
    request.dynamic_connections = true;

    let (result, events) = download_collecting(request).await;
    let summary = result.expect("dynamic download");

    assert_payload_eq(&summary.path, &server.content(), "dynamic payload");
    // `connections` keeps its old semantics: min(chunks, requested).
    assert_eq!(summary.connections, 8);
    // The adaptive plan starts below the ceiling but reaches real parallelism.
    assert!(
        (2..=8).contains(&summary.peak_connections),
        "peak {} outside the dynamic plan's 2..=8 range",
        summary.peak_connections
    );

    let plan = concurrency_events(&events);
    assert!(
        !plan.is_empty(),
        "expected at least one DownloadEvent::Concurrency"
    );
    assert_eq!(
        plan[0],
        (4, 0),
        "max=8 must start at half the plan (4), with no workers active yet"
    );
    for (target, active) in &plan {
        assert!(
            (1..=8).contains(target),
            "planned target {target} exceeds the 8-connection ceiling"
        );
        assert!(*active <= 8, "active workers {active} exceed the ceiling");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn static_plan_uses_all_connections_and_reports_peak() {
    let total = 8 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("static");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(256 * 1024); // 32 chunks
    request.dynamic_connections = false;

    let (result, events) = download_collecting(request).await;
    let summary = result.expect("static download");

    assert_payload_eq(&summary.path, &server.content(), "static payload");
    assert_eq!(summary.connections, 8);
    assert_eq!(
        summary.peak_connections, 8,
        "static mode must use every connection from the start"
    );

    let plan = concurrency_events(&events);
    assert_eq!(plan, vec![(8, 0)], "static mode has no plan changes");

    server.shutdown().await;
}

#[tokio::test]
async fn static_plan_peak_is_capped_by_available_chunks() {
    let total = 512 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("static-small");
    let mut request = request_for(&server, dir.path(), 8);
    request.chunk_size = Some(256 * 1024); // only 2 chunks exist
    request.dynamic_connections = false;

    let (result, events) = download_collecting(request).await;
    let summary = result.expect("small static download");

    assert_payload_eq(&summary.path, &server.content(), "small payload");
    assert_eq!(
        summary.connections, 2,
        "connections = min(chunks, requested)"
    );
    assert_eq!(summary.peak_connections, 2);
    assert_eq!(concurrency_events(&events), vec![(2, 0)]);

    server.shutdown().await;
}
