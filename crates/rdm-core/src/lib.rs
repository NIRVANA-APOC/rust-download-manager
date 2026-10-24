//! RDM download engine.
//!
//! Public API contract — implemented by the engine team. GUI, CLI and the
//! native-messaging host all program against these types.
//!
//! Design summary (details in `docs/architecture.md`):
//! * probe first (HEAD, then ranged GET fallback) to learn length / range support
//! * split the file into fixed-size chunks, handed out dynamically to N workers
//!   (work stealing: a slow CDN edge never becomes the tail-latency bottleneck)
//! * every worker does an independent `Range` request and pwrite()s at its offset
//! * progress is a crash-safe `.part` file plus a JSON resume state; validators
//!   (ETag / Last-Modified / total length) are checked before resuming
//! * servers that ignore ranges transparently fall back to a single stream
//! * callers that already verified a link can hand the metadata in
//!   ([`download_with_meta`]) or demand a single rangeless request
//!   ([`DownloadRequest::single_stream`]); [`probe_detailed`] is the HEAD-only
//!   verification primitive behind that hand-off

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

pub mod error;

/// Test/benchmark hooks that are not part of the supported API.
#[doc(hidden)]
pub mod testing {
    /// Forget every learned per-origin concurrency budget.
    ///
    /// Several benchmarks run against localhost fixture servers whose
    /// ephemeral ports can be reused; without a reset a cap learned for one
    /// server could leak into the next one. Not needed by normal applications.
    pub fn reset_origin_budgets() {
        crate::engine::origin::reset_budgets();
    }
}

mod client;
mod engine;
mod event;
mod filename;
mod plan;
mod probe;
mod rate;
mod state;

pub use error::DownloadError;

/// Connections used when the caller does not specify a value.
pub const DEFAULT_CONNECTIONS: usize = 8;
/// Upper bound for a single download's connections.
pub const MAX_CONNECTIONS: usize = 32;
/// Default unit of work, in bytes.
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// Minimum chunk size we allow (bigger = less overhead, smaller = better balance).
pub const MIN_CHUNK_SIZE: u64 = 256 * 1024;
/// Default user agent.
pub const DEFAULT_USER_AGENT: &str = "rdm/0.1 (+https://github.com/rdm-download/rdm)";

fn default_connections() -> usize {
    DEFAULT_CONNECTIONS
}

fn default_true() -> bool {
    true
}

/// A download request. Serializable so the Firefox native host / IPC layer can
/// forward it without a second schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    #[serde(default)]
    pub output_dir: Option<PathBuf>,
    /// Overrides the server-derived filename.
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default = "default_connections")]
    pub connections: usize,
    #[serde(default)]
    pub chunk_size: Option<u64>,
    /// Extra request headers (case-insensitive merge).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// `Cookie` header value captured from the browser.
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// e.g. `http://127.0.0.1:7890`.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Verify the finished file.
    #[serde(default)]
    pub expected_sha256: Option<String>,
    /// Global bandwidth cap in bytes/second.
    #[serde(default)]
    pub speed_limit_bps: Option<u64>,
    #[serde(default = "default_true")]
    pub resume: bool,
    /// Accept invalid TLS certificates (debug/public mirrors only).
    #[serde(default)]
    pub insecure: bool,
    /// Adapt the number of active connections while transferring: start
    /// mid-range, add connections while throughput improves, give them back on
    /// saturation or 429/503. Missing in old JSON → `true`.
    #[serde(default = "default_true")]
    pub dynamic_connections: bool,
    /// Guarantee exactly one `GET` with no `Range` header (one-time / signed
    /// links, anti-hotlink servers that reject ranged requests).
    ///
    /// [`DownloadRequest::normalized`] forces `connections = 1`,
    /// `dynamic_connections = false` and `chunk_size = None`; `resume` keeps
    /// its value but a pre-existing `.part` is always re-downloaded from zero
    /// (a one-time link cannot be resumed). The probe also skips its ranged
    /// `GET` fallback, so no `Range` header is ever sent.
    ///
    /// [`download`] still performs its normal HEAD probe (one HEAD + one GET).
    /// Use [`download_with_meta`] when the whole hand-off must be a single
    /// request beyond the caller's own HEAD verification. Missing in old
    /// JSON → `false`.
    #[serde(default)]
    pub single_stream: bool,
    /// Additional URLs that serve the same bytes (mirrors).
    ///
    /// Workers are spread across primary + mirrors, so total throughput can
    /// exceed one origin's per-client cap; a failing mirror rotates to the next
    /// source and ultimately back to the primary. Metadata (size, validators)
    /// still comes from the primary URL; each mirror response is validated
    /// against the primary's `Content-Range` total (and ETag when present).
    /// Engine-only for now: no CLI/MCP flag exposes it yet. Missing in old
    /// JSON → empty.
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// Allow HTTP/2 multiplexing for chunked range requests.
    ///
    /// Default `false`: chunked transfers force HTTP/1.1 so every worker owns
    /// an independent TCP connection/congestion window. Set `true` for servers
    /// that cap concurrent connections (or prefer h2): all ranged GETs then
    /// share one multiplexed TLS connection, and the server negotiates h2 via
    /// ALPN (plain-HTTP fixtures fall back to h1). Missing in old JSON → false.
    #[serde(default)]
    pub prefer_http2: bool,
}

impl DownloadRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            output_dir: None,
            file_name: None,
            connections: DEFAULT_CONNECTIONS,
            chunk_size: None,
            headers: BTreeMap::new(),
            cookie: None,
            referer: None,
            user_agent: None,
            proxy: None,
            expected_sha256: None,
            speed_limit_bps: None,
            resume: true,
            insecure: false,
            dynamic_connections: true,
            single_stream: false,
            mirrors: Vec::new(),
            prefer_http2: false,
        }
    }

    /// Clamp user input into supported bounds.
    pub fn normalized(&self) -> Self {
        let mut req = self.clone();
        req.connections = req.connections.clamp(1, MAX_CONNECTIONS);
        if req.single_stream {
            // One request only: no chunk plan, no worker pool, no adaptive
            // connections. `resume` is kept as-is for API compatibility, but
            // the transfer always restarts from zero (see `transfer::single`).
            req.connections = 1;
            req.dynamic_connections = false;
            req.chunk_size = None;
        }
        req
    }
}

/// What the server tells us before the transfer starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteMeta {
    /// URL after redirects.
    pub final_url: String,
    /// Filename derived from Content-Disposition / URL path.
    pub file_name: String,
    /// `None` when the server does not send Content-Length.
    pub total_bytes: Option<u64>,
    pub supports_ranges: bool,
    pub content_type: Option<String>,
    /// Validator used to detect a changed file before resuming.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// HTTP protocol actually used for the probe (`HTTP/1.1`, `HTTP/2.0`).
    #[serde(default)]
    pub http_version: Option<String>,
}

/// Full result of a HEAD-only probe ([`probe_detailed`]).
///
/// This is the additive counterpart of [`RemoteMeta`] for callers that must
/// verify a link before downloading it (resolvers, verifiers): unlike
/// [`probe`] it never issues a fallback ranged `GET`, so a one-time link is not
/// consumed, and non-2xx responses are reported as data (with `status`) instead
/// of being turned into an error.
///
/// `headers` is the raw server response and therefore **untrusted input**: it
/// carries everything the server sent (including `Set-Cookie`); never treat its
/// values as validated metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailedMeta {
    /// URL after redirects.
    pub final_url: String,
    /// Final response status code (`200`, `404`, `403`, …).
    pub status: u16,
    /// Every response header, keyed by lowercase name. Repeated headers are
    /// joined with `", "`. Server-controlled: do not trust these values.
    pub headers: BTreeMap<String, String>,
    /// Filename derived from `Content-Disposition` / the final URL path.
    pub file_name: String,
    pub content_type: Option<String>,
    /// `Content-Length` response header (`None` when absent; never the body
    /// size hint that a HEAD response would report as `0`).
    pub content_length: Option<u64>,
    /// `Accept-Ranges: bytes` observed on the HEAD response. A missing or
    /// non-`bytes` value means "not known to support ranges": proving range
    /// support requires a `GET`, which HEAD-only probing deliberately avoids.
    pub supports_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// HTTP protocol used for the HEAD request (`HTTP/1.1`, `HTTP/2.0`).
    #[serde(default)]
    pub http_version: Option<String>,
}

/// Progress / lifecycle events emitted to the UI layer.
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Probing,
    Metadata(RemoteMeta),
    Started {
        connections: usize,
        chunk_size: u64,
        total_chunks: u64,
        /// Chunks already present in a resumed `.part` file.
        resumed_chunks: u64,
    },
    Progress {
        downloaded: u64,
        total: Option<u64>,
        speed_bps: f64,
        eta_secs: Option<f64>,
        active_workers: usize,
    },
    Retrying {
        chunk: u64,
        attempt: u32,
        delay_ms: u64,
        reason: String,
    },
    /// The dynamic connection plan changed.
    ///
    /// `target` is the planned number of concurrent workers, `active` the
    /// number of workers transferring right now. Emitted once when the plan is
    /// established and once per target change (so GUI models can track both the
    /// plan and its realised peak).
    Concurrency {
        target: usize,
        active: usize,
    },
    Completed {
        summary: DownloadSummary,
    },
    Failed {
        error: String,
    },
    Paused {
        downloaded: u64,
    },
}

/// What the adaptive concurrency controller did during a transfer.
///
/// Purely diagnostic: callers can log/show it, but no decision depends on
/// these values. `min_target`/`final_target` are request-concurrency targets,
/// not chunk counts. `None` in [`DownloadSummary`] means the transfer used the
/// single-stream path (or dynamic planning was disabled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ControllerStats {
    /// Up-probes started (candidate higher target).
    pub up_probes: u32,
    /// Down-probes started (candidate lower target).
    pub down_probes: u32,
    /// Probes whose gain/loss survived the A–B–A confirmation and were kept.
    pub accepted: u32,
    /// Probes reverted because the A–B–A measurement did not confirm a gain.
    pub reverted: u32,
    /// Server pushbacks (429/503) that shortened the plan.
    pub pushbacks: u32,
    /// Smallest request-concurrency target used during the transfer.
    pub min_target: u32,
    /// Target at the end of the transfer.
    pub final_target: u32,
}

/// Final result of a successful (or checksum-verified) download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadSummary {
    pub path: PathBuf,
    pub total_bytes: u64,
    pub elapsed_ms: u64,
    pub avg_speed_bps: f64,
    /// Planned connection limit for this transfer. Semantics are unchanged by
    /// dynamic planning: `min(total_chunks, request.connections)` (single
    /// stream = 1).
    pub connections: usize,
    /// Realised peak number of simultaneously active workers. In static mode
    /// this equals `connections`; with dynamic planning it is the observed peak
    /// of the adaptive plan (never above `connections`).
    #[serde(default)]
    pub peak_connections: u32,
    pub sha256: Option<String>,
    pub resumed_chunks: u64,
    pub range_supported: bool,
    /// HTTP protocol used by the transfer (`HTTP/1.1` for chunked mode —
    /// forced so N range requests really use N TCP connections).
    #[serde(default)]
    pub http_version: Option<String>,
    /// Adaptive-controller diagnostics (None for single-stream transfers).
    #[serde(default)]
    pub controller: Option<ControllerStats>,
}

/// Controls a running download from another task (UI thread, signal handler…).
#[derive(Debug, Clone, Default)]
pub struct DownloadHandle {
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pause: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DownloadHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn pause(&self) {
        self.pause.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.cancel
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.pause.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Inspect a remote resource without downloading it.
pub async fn probe(request: &DownloadRequest) -> Result<RemoteMeta, DownloadError> {
    let client = client::build(request)?;
    probe::probe_with(&client, request).await
}

/// Inspect a remote resource with a single `HEAD` request.
///
/// Additive counterpart of [`probe`]: it never issues a `GET` (so a one-time or
/// short-lived signed link is not consumed), returns non-2xx responses as data
/// with `status` instead of errors, and exposes the raw response headers.
///
/// Only transport failures (DNS, connect, timeout, TLS) are errors. Range
/// support is reported from `Accept-Ranges` only — HEAD cannot prove it.
pub async fn probe_detailed(request: &DownloadRequest) -> Result<DetailedMeta, DownloadError> {
    let client = client::build(request)?;
    probe::probe_detailed_with(&client, request).await
}

/// Download a file with multiple connections.
///
/// `events` may be `None` for fire-and-forget use. Errors `Paused` and
/// `Cancelled` are control-flow results, not failures: resume by calling this
/// function again with the same request — the `.part` state is picked up.
pub async fn download(
    request: DownloadRequest,
    events: Option<UnboundedSender<DownloadEvent>>,
) -> Result<DownloadSummary, DownloadError> {
    download_with_handle(request, events, DownloadHandle::new()).await
}

/// Same as [`download`], but with an externally controlled handle.
pub async fn download_with_handle(
    request: DownloadRequest,
    events: Option<UnboundedSender<DownloadEvent>>,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    let normalized = request.normalized();
    engine::run(normalized, event::EventSink::new(events), handle).await
}

/// Download using remote metadata that the caller already verified.
///
/// Identical to [`download_with_handle`] except that the engine **skips its own
/// probe** and reuses `meta` (`final_url` / `file_name` / `total_bytes` /
/// `supports_ranges` / `etag` / `last_modified` / `http_version`). This is what
/// makes [`probe_detailed`] + download safe for one-time/signed URLs: the
/// validate-then-download hand-off consumes one request each instead of letting
/// the engine probe again.
///
/// The engine still enforces the validated length: when a response disagrees
/// with `meta.total_bytes` the transfer fails with
/// [`DownloadError::RemoteChanged`] instead of silently writing a different
/// file. The TOCTOU window between verification and download remains the
/// caller's responsibility.
pub async fn download_with_meta(
    request: DownloadRequest,
    meta: RemoteMeta,
    events: Option<UnboundedSender<DownloadEvent>>,
    handle: DownloadHandle,
) -> Result<DownloadSummary, DownloadError> {
    let normalized = request.normalized();
    engine::run_with_meta(normalized, meta, event::EventSink::new(events), handle).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_request_json_defaults_to_dynamic_connections() {
        // Old GUI/native/IPC payloads predate `dynamic_connections`; they must
        // keep deserializing and behave like a modern default request.
        let request: DownloadRequest =
            serde_json::from_str(r#"{"url":"https://example.com/file.bin"}"#)
                .expect("legacy request JSON parses");
        assert_eq!(request.connections, DEFAULT_CONNECTIONS);
        assert!(request.dynamic_connections);
    }

    #[test]
    fn request_roundtrip_keeps_dynamic_flag() {
        let mut request = DownloadRequest::new("https://example.com/file.bin");
        request.dynamic_connections = false;
        let json = serde_json::to_string(&request).unwrap();
        let decoded: DownloadRequest = serde_json::from_str(&json).unwrap();
        assert!(!decoded.dynamic_connections);
    }

    #[test]
    fn legacy_summary_json_defaults_peak_connections() {
        // `peak_connections` is additive: old persisted JSON stays readable.
        let summary: DownloadSummary = serde_json::from_str(
            r#"{"path":"file.bin","total_bytes":4,"elapsed_ms":1,"avg_speed_bps":4.0,
                "connections":2,"sha256":null,"resumed_chunks":0,"range_supported":true}"#,
        )
        .expect("legacy summary JSON parses");
        assert_eq!(summary.peak_connections, 0);
    }

    #[test]
    fn legacy_request_json_defaults_single_stream_to_false() {
        // `single_stream` is additive: old GUI/native/IPC payloads keep working
        // and keep their multi-connection behavior.
        let request: DownloadRequest =
            serde_json::from_str(r#"{"url":"https://example.com/file.bin"}"#)
                .expect("legacy request JSON parses");
        assert!(!request.single_stream);
        let normalized = request.normalized();
        assert!(!normalized.single_stream);
        assert_eq!(normalized.connections, DEFAULT_CONNECTIONS);
        assert!(normalized.dynamic_connections);
    }

    #[test]
    fn single_stream_normalizes_to_one_request() {
        let mut request = DownloadRequest::new("https://example.com/file.bin");
        request.single_stream = true;
        request.connections = 16;
        request.chunk_size = Some(1024);
        request.resume = true;

        let normalized = request.normalized();
        assert!(normalized.single_stream);
        assert_eq!(normalized.connections, 1);
        assert!(!normalized.dynamic_connections);
        assert_eq!(normalized.chunk_size, None);
        // Resume keeps its value; the transfer restarts from zero instead.
        assert!(normalized.resume);
    }

    #[test]
    fn single_stream_roundtrips_through_json() {
        let mut request = DownloadRequest::new("https://example.com/file.bin");
        request.single_stream = true;
        let json = serde_json::to_string(&request).unwrap();
        let decoded: DownloadRequest = serde_json::from_str(&json).unwrap();
        assert!(decoded.single_stream);
    }

    #[test]
    fn detailed_meta_roundtrips_through_json() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "content-type".to_string(),
            "application/octet-stream".into(),
        );
        headers.insert("set-cookie".to_string(), "a=1".into());
        let meta = DetailedMeta {
            final_url: "https://cdn.example.com/file.bin".into(),
            status: 200,
            headers,
            file_name: "file.bin".into(),
            content_type: Some("application/octet-stream".into()),
            content_length: Some(42),
            supports_ranges: true,
            etag: Some("\"v1\"".into()),
            last_modified: None,
            http_version: Some("HTTP/1.1".into()),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let decoded: DetailedMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.status, 200);
        assert_eq!(decoded.content_length, Some(42));
        assert_eq!(
            decoded.headers.get("set-cookie").map(String::as_str),
            Some("a=1")
        );
    }
}
