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

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

pub mod error;

mod client;
mod event;
mod filename;
mod plan;
mod probe;
mod rate;
mod state;
mod transfer;

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
        }
    }

    /// Clamp user input into supported bounds.
    pub fn normalized(&self) -> Self {
        let mut req = self.clone();
        req.connections = req.connections.clamp(1, MAX_CONNECTIONS);
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

/// Final result of a successful (or checksum-verified) download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadSummary {
    pub path: PathBuf,
    pub total_bytes: u64,
    pub elapsed_ms: u64,
    pub avg_speed_bps: f64,
    pub connections: usize,
    pub sha256: Option<String>,
    pub resumed_chunks: u64,
    pub range_supported: bool,
    /// HTTP protocol used by the transfer (`HTTP/1.1` for chunked mode —
    /// forced so N range requests really use N TCP connections).
    #[serde(default)]
    pub http_version: Option<String>,
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
        self.cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
        self.pause
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Inspect a remote resource without downloading it.
pub async fn probe(request: &DownloadRequest) -> Result<RemoteMeta, DownloadError> {
    let client = client::build(request)?;
    probe::probe_with(&client, request).await
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
    transfer::run(normalized, event::EventSink::new(events), handle).await
}
