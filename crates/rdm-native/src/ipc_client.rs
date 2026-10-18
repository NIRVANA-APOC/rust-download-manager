//! Thin, blocking wrapper around the `rdm-ipc` client used by the host.
//!
//! The IPC crate owns discovery (`ipc.json`), token handling and the JSONL
//! transport; this module only maps its error type into the native-host error
//! vocabulary and extracts the fields we forward to the extension.

use std::time::Duration;

use rdm_ipc::{EnqueueItem, IpcError, Request};

/// Connect/write/read timeout for one GUI round-trip. The GUI answers
/// immediately; it never blocks on the download itself.
pub const IPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum IpcCallError {
    /// No `ipc.json`, stale endpoint, or connection refused: the GUI is down.
    #[error("RDM GUI is not running: {0}")]
    NotRunning(String),
    /// The GUI answered `ok:false` (bad URL, duplicate, …).
    #[error("RDM GUI rejected the request: {0}")]
    Rejected(String),
    #[error("IPC protocol error: {0}")]
    Protocol(String),
    #[error("IPC transport error: {0}")]
    Transport(String),
}

impl From<IpcError> for IpcCallError {
    fn from(err: IpcError) -> Self {
        match err {
            IpcError::NotRunning(message) => IpcCallError::NotRunning(message),
            IpcError::Rejected(message) => IpcCallError::Rejected(message),
            IpcError::Protocol(message) => IpcCallError::Protocol(message),
            IpcError::Io(err) => IpcCallError::Transport(err.to_string()),
        }
    }
}

impl IpcCallError {
    /// Whether retrying after starting the GUI could plausibly help.
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            IpcCallError::NotRunning(_) | IpcCallError::Transport(_)
        )
    }

    /// True when the GUI explicitly rejected the request (never spool those,
    /// the GUI would reject the spooled item for the same reason).
    pub fn is_rejection(&self) -> bool {
        matches!(self, IpcCallError::Rejected(_))
    }
}

/// Check whether the GUI answers a `ping`.
pub fn ping() -> Result<(), IpcCallError> {
    rdm_ipc::send(Request::Ping, IPC_TIMEOUT)?;
    Ok(())
}

/// Enqueue a handoff and return the GUI-assigned download id (`dl_…`).
///
/// The id is treated as optional: a GUI that answers `ok:true` without a
/// `download_id` still accepted the download, and losing the handoff just
/// because the id shape changed would be worse than forwarding an empty id.
pub fn enqueue(item: EnqueueItem) -> Result<String, IpcCallError> {
    let response = rdm_ipc::send(Request::Enqueue { item }, IPC_TIMEOUT)?;
    let download_id = response
        .data
        .as_ref()
        .and_then(|data| {
            data.get("download_id")
                .or_else(|| data.get("id"))
                .and_then(|value| value.as_str())
        })
        .unwrap_or_default()
        .to_string();
    Ok(download_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ipc_errors_to_codes() {
        let not_running: IpcCallError = IpcError::NotRunning("missing ipc.json".into()).into();
        assert!(not_running.is_unavailable());
        assert!(!not_running.is_rejection());

        let rejected: IpcCallError = IpcError::Rejected("invalid url".into()).into();
        assert!(rejected.is_rejection());
        assert!(!rejected.is_unavailable());

        let protocol: IpcCallError = IpcError::Protocol("bad json".into()).into();
        assert!(!protocol.is_rejection());
        assert!(!protocol.is_unavailable());
    }
}
