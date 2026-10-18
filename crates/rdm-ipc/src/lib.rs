//! Local IPC between the RDM GUI (server) and the native messaging host
//! (client), plus the shared wire types.
//!
//! Transport: TCP on 127.0.0.1 with an ephemeral port. The GUI writes
//! `%LOCALAPPDATA%\RDM\ipc.json` (mode 0600 where supported) containing the
//! port and a random per-process token; every request must echo the token.
//! Messages are single-line JSON objects (JSONL) to keep debugging trivial.
//!
//! See `docs/extension-protocol.md` for the full protocol and the Firefox
//! native-messaging framing.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Written by the GUI once its IPC listener is ready.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcInfo {
    pub protocol: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub app_version: String,
}

/// Everything the browser (or CLI) can ask the GUI to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Enqueue { item: EnqueueItem },
    Status,
    Pause { id: String },
    Resume { id: String },
    Cancel { id: String },
    OpenFolder { id: String },
    /// Bring the main window to front.
    Activate,
}

/// A download handed over by the extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueItem {
    pub download: rdm_core::DownloadRequest,
    /// `firefox`, `clipboard`, `menu`, `cli`, ...
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub source_page: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(id: u64, data: impl Serialize) -> Self {
        Self {
            id,
            ok: true,
            error: None,
            data: serde_json::to_value(data).ok(),
        }
    }

    pub fn error(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            error: Some(error.into()),
            data: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("RDM GUI is not running ({0})")]
    NotRunning(String),
    #[error("ipc i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ipc protocol error: {0}")]
    Protocol(String),
    #[error("RDM GUI rejected the request: {0}")]
    Rejected(String),
}

/// Directory that holds runtime state (`ipc.json`, logs, queue).
pub fn state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("RDM");
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home).join(".rdm");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(xdg).join("rdm");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local/state/rdm");
        }
    }
    PathBuf::from(".rdm")
}

pub fn info_path() -> PathBuf {
    state_dir().join("ipc.json")
}

pub fn read_info() -> Result<IpcInfo, IpcError> {
    let path = info_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| IpcError::NotRunning(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| IpcError::Protocol(format!("invalid {}: {e}", path.display())))
}

/// Atomically publish the GUI endpoint. On Unix the file is created with 0600.
pub fn write_info(info: &IpcInfo) -> Result<(), IpcError> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    let path = info_path();
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(info)
        .map_err(|e| IpcError::Protocol(e.to_string()))?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Best-effort liveness check: can we connect to the recorded endpoint?
pub fn is_gui_alive() -> bool {
    match read_info() {
        Ok(info) => TcpStream::connect_timeout(
            &([127, 0, 0, 1], info.port).into(),
            Duration::from_millis(300),
        )
        .is_ok(),
        Err(_) => false,
    }
}

/// Blocking client used by the native messaging host (a plain stdin/stdout
/// process, no async runtime needed).
pub fn send(request: Request, timeout: Duration) -> Result<Response, IpcError> {
    let info = read_info()?;
    let mut stream = TcpStream::connect_timeout(
        &([127, 0, 0, 1], info.port).into(),
        timeout,
    )
    .map_err(|_| IpcError::NotRunning("connection to RDM GUI failed".into()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let id = next_id();
    let envelope = Envelope {
        id,
        token: Some(info.token.clone()),
        request,
    };
    let mut line = serde_json::to_string(&envelope)
        .map_err(|e| IpcError::Protocol(e.to_string()))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)?;
    if response_line.trim().is_empty() {
        return Err(IpcError::Protocol("empty response from GUI".into()));
    }
    let response: Response = serde_json::from_str(response_line.trim())
        .map_err(|e| IpcError::Protocol(format!("invalid response: {e}")))?;
    if !response.ok {
        return Err(IpcError::Rejected(
            response.error.unwrap_or_else(|| "unknown error".into()),
        ));
    }
    Ok(response)
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip() {
        let env = Envelope {
            id: 7,
            token: Some("tok".into()),
            request: Request::Enqueue {
                item: EnqueueItem {
                    download: rdm_core::DownloadRequest::new("https://example.com/a.bin"),
                    source: "firefox".into(),
                    source_page: Some("https://example.com/".into()),
                },
            },
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"method\":\"enqueue\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 7);
    }

    #[test]
    fn state_dir_is_absolute() {
        assert!(state_dir().is_absolute());
    }
}
