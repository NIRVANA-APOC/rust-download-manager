//! Integration tests for the additive engine capabilities used by the
//! resolve/download hand-off:
//!
//! * `probe_detailed()` — HEAD-only, non-2xx as data, raw headers;
//! * `download_with_meta()` — no second probe, caller-verified metadata reused;
//! * `DownloadRequest::single_stream` — exactly one rangeless `GET`.
//!
//! All assertions are byte-level against the deterministic local fixture
//! server, including request counts and the raw `Range` headers it observed.

use std::path::{Path, PathBuf};

use rdm_core::{
    download, download_with_handle, download_with_meta, probe, probe_detailed, DownloadError,
    DownloadEvent, DownloadHandle, DownloadRequest, RemoteMeta,
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
            "rdm-core-detailed-{tag}-{}-{nanos}",
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

async fn meta_for(server: &TestServer) -> RemoteMeta {
    probe(&DownloadRequest::new(server.file_url()))
        .await
        .expect("probe succeeds")
}

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
    panic!(
        "{context}: payload mismatch at byte {at} (actual {} bytes, expected {} bytes)",
        actual.len(),
        expected.len()
    );
}

fn leftovers(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read output dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn drain_events(rx: &mut mpsc::UnboundedReceiver<DownloadEvent>) -> Vec<DownloadEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

// ---------------------------------------------------------------------------
// probe_detailed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_detailed_reports_head_only_metadata() {
    let total = 64 * 1024u64;
    let server = TestServer::start(
        Config::new(total)
            .with_etag("\"abc\"")
            .with_last_modified("Wed, 21 Oct 2015 07:28:00 GMT")
            .with_content_disposition("attachment; filename=\"report.bin\""),
    )
    .await
    .unwrap();

    let meta = probe_detailed(&DownloadRequest::new(server.file_url()))
        .await
        .expect("HEAD probe succeeds");

    assert_eq!(meta.status, 200);
    assert!(meta.final_url.ends_with("/file"));
    assert_eq!(meta.content_length, Some(total));
    assert_eq!(meta.file_name, "report.bin");
    assert_eq!(
        meta.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert!(meta.supports_ranges);
    assert_eq!(meta.etag.as_deref(), Some("\"abc\""));
    assert_eq!(
        meta.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert!(
        meta.http_version
            .as_deref()
            .unwrap_or_default()
            .contains("1.1"),
        "HEAD probe should report the HTTP version: {:?}",
        meta.http_version
    );
    // Raw headers are exposed with lowercase names.
    assert_eq!(
        meta.headers.get("content-type").map(String::as_str),
        Some("application/octet-stream")
    );
    assert_eq!(
        meta.headers.get("accept-ranges").map(String::as_str),
        Some("bytes")
    );
    assert_eq!(
        meta.headers.get("etag").map(String::as_str),
        Some("\"abc\"")
    );

    // HEAD-only: a probe must never fall back to a ranged GET.
    let stats = server.stats();
    assert_eq!(stats.requests, 1, "exactly one request for probe_detailed");
    assert_eq!(stats.head_requests, 1);
    assert_eq!(stats.get_requests, 0, "probe_detailed must not send a GET");
    assert!(stats.range_headers.is_empty());
    assert_eq!(stats.methods, vec!["HEAD"]);

    server.shutdown().await;
}

#[tokio::test]
async fn probe_detailed_returns_non_2xx_as_data() {
    let server = TestServer::start(Config::new(1024)).await.unwrap();

    let meta = probe_detailed(&DownloadRequest::new(server.url("/missing")))
        .await
        .expect("a 404 is data, not an error");
    assert_eq!(meta.status, 404);
    assert!(meta.final_url.ends_with("/missing"));
    assert_eq!(meta.file_name, "missing");
    assert_eq!(meta.content_type.as_deref(), Some("text/plain"));
    assert_eq!(meta.content_length, Some(b"not found".len() as u64));
    assert!(!meta.supports_ranges);

    let stats = server.stats();
    assert_eq!(stats.get_requests, 0, "404 probe is still HEAD-only");
    assert_eq!(stats.head_requests, 1);

    server.shutdown().await;
}

#[tokio::test]
async fn probe_detailed_follows_redirects_with_head() {
    let total = 32 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();

    let meta = probe_detailed(&DownloadRequest::new(server.redirect_url()))
        .await
        .expect("HEAD probe through a redirect");
    assert_eq!(meta.status, 200);
    assert!(meta.final_url.ends_with("/file"));
    assert_eq!(meta.content_length, Some(total));

    let stats = server.stats();
    assert_eq!(stats.get_requests, 0, "redirect following stays HEAD-only");
    assert_eq!(stats.head_requests, 2, "one HEAD per hop");
    assert_eq!(stats.responses_302, 1);

    server.shutdown().await;
}

#[tokio::test]
async fn probe_detailed_transport_failure_is_an_error() {
    // Port 1 is not listening: DNS/connect failures stay errors (unlike HTTP
    // status codes, which are reported in the returned metadata).
    let request = DownloadRequest::new("http://127.0.0.1:1/nope");
    let error = probe_detailed(&request)
        .await
        .expect_err("connection failure must be an error");
    assert!(
        matches!(error, DownloadError::Http(_)),
        "expected DownloadError::Http, got {error:?}"
    );

    // Invalid URLs are rejected up front, like `probe`.
    let error = probe_detailed(&DownloadRequest::new("not-a-url"))
        .await
        .expect_err("invalid URL must be an error");
    assert!(matches!(error, DownloadError::InvalidUrl(_)));
}

// ---------------------------------------------------------------------------
// download_with_meta
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_with_meta_skips_the_probe() {
    let total = 1024 * 1024u64;
    let chunk_size = 256 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let expected = server.content();

    let mut meta = meta_for(&server).await;
    // The caller-verified name must be honoured without another probe.
    meta.file_name = "meta-name.bin".to_string();

    let meta_dir = TempDir::new("meta");
    let mut meta_request = request_for(&server, meta_dir.path(), 2);
    meta_request.chunk_size = Some(chunk_size);
    meta_request.dynamic_connections = false;
    server.reset_stats();

    let summary = download_with_meta(
        meta_request.clone(),
        meta.clone(),
        None,
        DownloadHandle::new(),
    )
    .await
    .expect("download with caller-verified metadata");
    assert_payload_eq(&summary.path, &expected, "download_with_meta");
    assert_eq!(summary.total_bytes, total);
    assert_eq!(summary.connections, 2);
    assert!(summary.range_supported);
    assert_eq!(
        summary.path.file_name().unwrap().to_string_lossy(),
        "meta-name.bin",
        "meta.file_name must be reused"
    );

    let stats = server.stats();
    assert_eq!(stats.head_requests, 0, "download_with_meta must not probe");
    assert_eq!(stats.get_requests, 4, "one ranged GET per chunk");
    assert_eq!(stats.requests, 4);
    assert!(stats.methods.iter().all(|method| method == "GET"));

    // Baseline comparison: the plain `download()` performs its own HEAD probe.
    let plain_dir = TempDir::new("plain");
    let mut plain_request = request_for(&server, plain_dir.path(), 2);
    plain_request.chunk_size = Some(chunk_size);
    plain_request.dynamic_connections = false;
    server.reset_stats();
    let plain = download(plain_request, None).await.expect("plain download");
    assert_payload_eq(&plain.path, &expected, "plain download");
    let plain_stats = server.stats();
    assert_eq!(plain_stats.head_requests, 1, "download() probes once");
    assert_eq!(plain_stats.get_requests, 4);
    assert_eq!(plain_stats.requests, 5);
    assert_eq!(
        plain_stats.methods.first().map(String::as_str),
        Some("HEAD")
    );

    server.shutdown().await;
}

#[tokio::test]
async fn download_with_meta_single_stream_sends_one_rangeless_get() {
    let total = 512 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let expected = server.content();
    let meta = meta_for(&server).await;
    assert!(meta.supports_ranges);
    assert_eq!(meta.total_bytes, Some(total));

    let dir = TempDir::new("meta-single");
    let mut request = request_for(&server, dir.path(), 8);
    request.single_stream = true;
    request.dynamic_connections = true;
    request.chunk_size = Some(64 * 1024);
    server.reset_stats();

    let summary = download_with_meta(request, meta, None, DownloadHandle::new())
        .await
        .expect("single-stream download with metadata");
    assert_payload_eq(&summary.path, &expected, "single-stream payload");
    assert_eq!(summary.connections, 1);
    assert_eq!(summary.peak_connections, 1);
    assert!(!summary.range_supported, "single stream uses no ranges");
    assert_eq!(summary.total_bytes, total);

    let stats = server.stats();
    assert_eq!(
        stats.head_requests, 0,
        "metadata was supplied by the caller"
    );
    assert_eq!(stats.get_requests, 1, "exactly one GET");
    assert_eq!(stats.requests, 1);
    assert!(
        stats.range_headers.is_empty(),
        "single_stream must not send a Range header, saw {:?}",
        stats.range_headers
    );
    assert_eq!(stats.responses_206, 0);
    assert_eq!(stats.methods, vec!["GET"]);

    assert_eq!(leftovers(dir.path()), vec!["file".to_string()]);
    server.shutdown().await;
}

#[tokio::test]
async fn download_with_meta_detects_a_changed_remote_length() {
    let total = 512 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let mut meta = meta_for(&server).await;
    meta.total_bytes = Some(total + 4096);
    meta.supports_ranges = true;

    let dir = TempDir::new("meta-changed");
    let mut request = request_for(&server, dir.path(), 2);
    request.chunk_size = Some(256 * 1024);
    request.dynamic_connections = false;

    let error = download_with_meta(request, meta, None, DownloadHandle::new())
        .await
        .expect_err("a different remote length must fail");
    assert!(
        matches!(error, DownloadError::RemoteChanged),
        "expected DownloadError::RemoteChanged, got {error:?}"
    );
    let target = dir.path().join("file");
    assert!(!target.exists(), "a changed remote must not land a file");

    server.shutdown().await;
}

#[tokio::test]
async fn download_with_meta_single_stream_detects_a_changed_remote_length() {
    let total = 256 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let mut meta = meta_for(&server).await;
    // The server no longer supports ranges in the caller's (now stale) view,
    // and the verified length is wrong: the response Content-Length disagrees.
    meta.supports_ranges = false;
    meta.total_bytes = Some(total + 1);

    let dir = TempDir::new("meta-single-changed");
    let mut request = request_for(&server, dir.path(), 4);
    request.single_stream = true;

    let error = download_with_meta(request, meta, None, DownloadHandle::new())
        .await
        .expect_err("a different remote length must fail");
    assert!(
        matches!(error, DownloadError::RemoteChanged),
        "expected DownloadError::RemoteChanged, got {error:?}"
    );
    assert!(!dir.path().join("file").exists());

    server.shutdown().await;
}

#[tokio::test]
async fn download_with_meta_unknown_length_falls_back_to_single_stream() {
    let total = 512 * 1024u64;
    let server = TestServer::start(Config::new(total).without_length().without_ranges())
        .await
        .unwrap();
    let meta = meta_for(&server).await;
    assert_eq!(meta.total_bytes, None);
    assert!(!meta.supports_ranges);

    let dir = TempDir::new("meta-unknown");
    let request = request_for(&server, dir.path(), 4);
    server.reset_stats();
    let summary = download_with_meta(request, meta, None, DownloadHandle::new())
        .await
        .expect("unknown-length metadata still downloads");
    assert_payload_eq(&summary.path, &server.content(), "unknown-length payload");
    assert_eq!(summary.connections, 1);
    assert!(!summary.range_supported);
    assert_eq!(summary.total_bytes, total);

    let stats = server.stats();
    assert_eq!(stats.head_requests, 0);
    assert_eq!(stats.get_requests, 1);
    assert!(stats.range_headers.is_empty());

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// DownloadRequest::single_stream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_stream_sends_exactly_one_rangeless_get_with_all_events() {
    let total = 512 * 1024u64;
    // 2 granules x 300 ms: slow enough for the 250 ms progress pump to emit.
    let server = TestServer::start(Config::new(total).with_delay(300))
        .await
        .unwrap();
    let expected = server.content();

    let dir = TempDir::new("single-stream");
    let mut request = request_for(&server, dir.path(), 8);
    request.single_stream = true;
    request.chunk_size = Some(64 * 1024);
    request.file_name = Some("payload.bin".to_string());

    let (tx, mut rx) = mpsc::unbounded_channel();
    let summary = download_with_handle(request, Some(tx), DownloadHandle::new())
        .await
        .expect("single-stream download");
    let events = drain_events(&mut rx);

    assert_payload_eq(&summary.path, &expected, "single-stream payload");
    assert_eq!(summary.total_bytes, total);
    assert_eq!(summary.connections, 1);
    assert_eq!(summary.peak_connections, 1);
    assert!(!summary.range_supported);

    // Lifecycle events are unchanged; only the transfer strategy differs.
    assert!(
        matches!(events.first(), Some(DownloadEvent::Probing)),
        "first event must be Probing, got {:?}",
        events.first().map(event_name)
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DownloadEvent::Metadata(meta) if meta.total_bytes == Some(total))),
        "expected a Metadata event"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DownloadEvent::Started { connections: 1, .. })),
        "expected Started{{connections: 1}}"
    );
    assert!(
        events.iter().any(
            |event| matches!(event, DownloadEvent::Progress { total: Some(t), .. } if *t == total)
        ),
        "expected at least one Progress event"
    );
    assert!(
        matches!(events.last(), Some(DownloadEvent::Completed { .. })),
        "last event must be Completed, got {:?}",
        events.last().map(event_name)
    );

    // One HEAD (existing probe) + one rangeless GET, nothing else.
    let stats = server.stats();
    assert_eq!(
        stats.head_requests, 1,
        "the normal probe is still HEAD-only"
    );
    assert_eq!(stats.get_requests, 1, "single_stream sends exactly one GET");
    assert_eq!(stats.responses_206, 0);
    assert!(
        stats.range_headers.is_empty(),
        "no Range header may be sent, saw {:?}",
        stats.range_headers
    );
    assert_eq!(stats.methods, vec!["HEAD", "GET"]);

    // `.part` is renamed and its state removed.
    assert!(dir.path().join("payload.bin").exists());
    assert!(!dir.path().join("payload.bin.part").exists());
    assert!(!dir.path().join("payload.bin.part.json").exists());
    assert_eq!(leftovers(dir.path()), vec!["payload.bin".to_string()]);

    server.shutdown().await;
}

#[tokio::test]
async fn connections_equals_one_without_single_stream_still_uses_ranges() {
    // Regression guard: `connections = 1` is *not* a single-request promise.
    let total = 1024 * 1024u64;
    let chunk_size = 256 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let dir = TempDir::new("connections-one");
    let mut request = request_for(&server, dir.path(), 1);
    request.chunk_size = Some(chunk_size);
    request.dynamic_connections = false;

    let summary = download(request, None)
        .await
        .expect("one-connection chunked download");
    assert_payload_eq(&summary.path, &server.content(), "one-connection payload");

    let stats = server.stats();
    assert_eq!(stats.get_requests, 4, "one Range GET per chunk");
    assert_eq!(stats.range_headers.len(), 4);
    assert_eq!(stats.responses_206, 4, "chunked mode really used ranges");
    assert_eq!(summary.connections, 1);
    assert!(summary.range_supported);

    server.shutdown().await;
}

#[tokio::test]
async fn single_stream_restarts_from_zero_over_a_stale_part() {
    let total = 512 * 1024u64;
    let server = TestServer::start(Config::new(total)).await.unwrap();
    let expected = server.content();
    let dir = TempDir::new("single-stream-resume");

    // Simulate an interrupted previous run: a pre-allocated `.part` plus its
    // resume state. A one-time link cannot be resumed, so the engine must
    // re-download from zero and discard the stale state.
    let target = dir.path().join("payload.bin");
    std::fs::write(dir.path().join("payload.bin.part"), vec![0xAAu8; 4096]).unwrap();
    std::fs::write(dir.path().join("payload.bin.part.json"), b"{}").unwrap();
    assert!(!target.exists());

    let mut request = request_for(&server, dir.path(), 2);
    request.file_name = Some("payload.bin".to_string());
    request.single_stream = true;
    request.resume = true;

    let summary = download(request, None)
        .await
        .expect("single-stream download over stale part");
    assert_payload_eq(&summary.path, &expected, "restarted payload");
    assert_eq!(summary.total_bytes, total);
    assert_eq!(summary.resumed_chunks, 0);

    assert!(target.exists());
    assert!(!dir.path().join("payload.bin.part").exists());
    assert!(!dir.path().join("payload.bin.part.json").exists());
    assert_eq!(leftovers(dir.path()), vec!["payload.bin".to_string()]);

    // The whole payload crossed the wire again: no bytes were reused.
    let stats = server.stats();
    assert_eq!(stats.get_requests, 1);
    assert_eq!(stats.body_bytes_sent, total);
    assert!(stats.range_headers.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn single_stream_never_sends_a_range_header_even_while_probing() {
    // A server that omits `Accept-Ranges` normally forces the probe to send a
    // one-byte ranged GET. `single_stream` must skip that fallback: otherwise a
    // one-time/anti-hotlink link would receive a Range request.
    let total = 256 * 1024u64;
    let server = TestServer::start(Config::new(total).without_accept_ranges())
        .await
        .unwrap();
    let expected = server.content();
    let dir = TempDir::new("single-stream-probe-guard");
    let mut request = request_for(&server, dir.path(), 4);
    request.single_stream = true;

    let summary = download(request, None)
        .await
        .expect("single-stream download without Accept-Ranges");
    assert_payload_eq(&summary.path, &expected, "single-stream payload");
    assert_eq!(summary.total_bytes, total);
    assert_eq!(summary.connections, 1);

    let stats = server.stats();
    assert_eq!(stats.head_requests, 1);
    assert_eq!(stats.get_requests, 1, "only the payload GET is allowed");
    assert!(
        stats.range_headers.is_empty(),
        "single_stream must never send a Range header, saw {:?}",
        stats.range_headers
    );
    assert_eq!(stats.responses_206, 0);
    assert_eq!(stats.methods, vec!["HEAD", "GET"]);

    // Contrast: without `single_stream`, the same fixture still forces the
    // one-byte ranged probe fallback.
    let meta = probe(&DownloadRequest::new(server.file_url()))
        .await
        .expect("ranged fallback probe");
    assert!(meta.supports_ranges);
    let stats = server.stats();
    assert_eq!(
        stats.range_headers,
        vec!["bytes=0-0".to_string()],
        "the non-single-stream probe uses a ranged GET"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Event naming helper (DownloadEvent is not Display)
// ---------------------------------------------------------------------------

fn event_name(event: &DownloadEvent) -> &'static str {
    match event {
        DownloadEvent::Probing => "Probing",
        DownloadEvent::Metadata(_) => "Metadata",
        DownloadEvent::Started { .. } => "Started",
        DownloadEvent::Progress { .. } => "Progress",
        DownloadEvent::Retrying { .. } => "Retrying",
        DownloadEvent::Concurrency { .. } => "Concurrency",
        DownloadEvent::Completed { .. } => "Completed",
        DownloadEvent::Failed { .. } => "Failed",
        DownloadEvent::Paused { .. } => "Paused",
    }
}
