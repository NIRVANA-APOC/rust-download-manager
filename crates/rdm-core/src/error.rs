use std::path::PathBuf;

use thiserror::Error;

/// Engine errors. `Paused` and `Cancelled` are expected control-flow outcomes.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("network error: {0}")]
    Http(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("server returned HTTP {status} for {url}")]
    Status { status: u16, url: String },

    #[error("server does not support range requests; cannot use multi-connection mode")]
    NoRangeSupport,

    #[error("the remote file changed (validator mismatch); resume state discarded")]
    RemoteChanged,

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("invalid resume state at {path}: {reason}")]
    InvalidState { path: PathBuf, reason: String },

    #[error("download paused at {downloaded} bytes")]
    Paused { downloaded: u64 },

    #[error("download cancelled at {downloaded} bytes")]
    Cancelled { downloaded: u64 },

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("{0}")]
    Other(String),
}

impl DownloadError {
    pub fn is_control_flow(&self) -> bool {
        matches!(self, Self::Paused { .. } | Self::Cancelled { .. })
    }
}
