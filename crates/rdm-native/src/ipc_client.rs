//! Thin, blocking wrapper around the `rdm-ipc` client used by the host.
//!
//! The IPC crate owns discovery (`ipc.json`), token handling and the JSONL
//! transport; this module only maps its error type into the native-host error
//! vocabulary and extracts the fields we forward to the extension.

use std::time::Duration;

use rdm_ipc::{EnqueueItem, IpcError, Request};

use crate::protocol::DownloadStatus;

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

/// Ask the GUI to run the AI link parser and return the GUI-side request id.
///
/// Like the download id, the request id is treated as optional (`0` when the
/// GUI accepts without one): the parse itself already happened on the GUI side.
pub fn ai_parse(request: &crate::protocol::AiParseRequest) -> Result<u64, IpcCallError> {
    let response = rdm_ipc::send(
        Request::AiParse {
            url: request.url.clone(),
            source_page: request.source_page.clone(),
            title: request.title.clone(),
            include_credentials: request.include_credentials,
            cookie: request.cookie.clone(),
            referer: request.referer.clone(),
            user_agent: request.user_agent.clone(),
            // P4b-b2: validated link snapshot (empty = no snapshot).
            page_url: request.page_url.clone(),
            page_title: request.page_title.clone(),
            page_links: request.page_links.clone(),
        },
        IPC_TIMEOUT,
    )?;
    Ok(response
        .data
        .as_ref()
        .and_then(|data| data.get("request_id"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0))
}

/// Snapshot of the GUI's download state, forwarded to the extension as a
/// `status` response (`docs/extension-protocol.md` §1.3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusSnapshot {
    pub active: u64,
    pub global_speed_bps: f64,
    pub downloads: Vec<DownloadStatus>,
}

/// Ask the GUI for a status snapshot.
///
/// This is a read-only poll and therefore never fails because of a weird
/// payload: a missing GUI becomes `Err(NotRunning)` (the host degrades it to an
/// empty snapshot frame), and everything inside `response.data` is parsed
/// tolerantly by [`parse_status_data`].
pub fn status() -> Result<StatusSnapshot, IpcCallError> {
    let response = rdm_ipc::send(Request::Status, IPC_TIMEOUT)?;
    Ok(parse_status_data(response.data.as_ref()))
}

/// Tolerant, panic-free parser for the GUI `status` payload
/// (`docs/extension-protocol.md` §2.3).
///
/// The native host, the GUI and the extension are shipped independently, so
/// every part of `data` is treated as untrusted:
///
/// * absent payload, non-object payload, or a non-array `downloads` yields
///   defaults (an empty snapshot) instead of an error;
/// * missing scalar fields fall back to their documented defaults (`total`
///   stays `None` for "unknown size");
/// * a download entry with a wrong-typed field (or without a usable string
///   `id`) is skipped instead of being forwarded half-parsed;
/// * unknown fields are ignored (forward compatibility).
pub fn parse_status_data(data: Option<&serde_json::Value>) -> StatusSnapshot {
    let Some(object) = data.and_then(serde_json::Value::as_object) else {
        return StatusSnapshot::default();
    };
    StatusSnapshot {
        active: object.get("active").and_then(json_u64).unwrap_or(0),
        global_speed_bps: object
            .get("global_speed_bps")
            .and_then(json_non_negative_f64)
            .unwrap_or(0.0),
        downloads: object
            .get("downloads")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().filter_map(parse_download_status).collect())
            .unwrap_or_default(),
    }
}

fn parse_download_status(entry: &serde_json::Value) -> Option<DownloadStatus> {
    let object = entry.as_object()?;
    // `id` is the only identity field: an entry without a usable string id can
    // never be matched against the extension's recent-download list, so drop
    // it instead of forwarding an empty id.
    let id = object.get("id").and_then(serde_json::Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    Some(DownloadStatus {
        id: id.to_string(),
        file_name: optional_string(object, "file_name")?.unwrap_or_default(),
        state: optional_string(object, "state")?.unwrap_or_else(|| "unknown".to_string()),
        downloaded: optional_u64(object, "downloaded")?.unwrap_or(0),
        total: optional_u64(object, "total")?,
        speed_bps: optional_f64(object, "speed_bps")?.unwrap_or(0.0),
        connections: optional_u64(object, "connections")?.unwrap_or(0),
        // Additive v0.2 thread counts: an old GUI has none of them, and a
        // malformed value must never drop the whole download entry (the
        // extension renders 0 as "unknown"). So these are read leniently.
        active_connections: lenient_u64(object, "active_connections"),
        target_connections: lenient_u64(object, "target_connections"),
        peak_connections: lenient_u64(object, "peak_connections"),
    })
}

/// `Ok(None)` = absent/null (use the default); `None` = present but unusable
/// (the whole download entry must be skipped).
fn optional_string(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<Option<String>> {
    match object.get(key) {
        None | Some(serde_json::Value::Null) => Some(None),
        Some(value) => value.as_str().map(|value| Some(value.to_string())),
    }
}

fn optional_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<Option<u64>> {
    match object.get(key) {
        None | Some(serde_json::Value::Null) => Some(None),
        Some(value) => json_u64(value).map(Some),
    }
}

fn optional_f64(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<Option<f64>> {
    match object.get(key) {
        None | Some(serde_json::Value::Null) => Some(None),
        Some(value) => json_non_negative_f64(value).map(Some),
    }
}

/// Lenient read of an additive count field (the v0.2 thread counts).
///
/// Unlike [`optional_u64`], a bad value never makes the caller skip the whole
/// download entry: absent, `null`, non-numeric, negative, fractional or
/// overflowing values all degrade to `0` ("unknown" for the extension).
fn lenient_u64(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    object.get(key).and_then(json_u64).unwrap_or(0)
}

/// Coerce a JSON number to `u64`. Integers are exact; integral floats are
/// accepted (a GUI could compute byte counts in floating point), while
/// fractional, negative and non-finite values are rejected.
fn json_u64(value: &serde_json::Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    let value = value.as_f64()?;
    // Strictly below 2^64: `u64::MAX as f64` rounds up to 2^64, so `<=` would
    // let an overflowing float saturate to u64::MAX instead of falling back.
    let upper_bound = u64::MAX as f64;
    if value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value < upper_bound {
        Some(value as u64)
    } else {
        None
    }
}

fn json_non_negative_f64(value: &serde_json::Value) -> Option<f64> {
    let value = value.as_f64()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> StatusSnapshot {
        parse_status_data(Some(&value))
    }

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

    #[test]
    fn parses_full_status_payload() {
        let snapshot = parse(json!({
            "downloads": [{
                "id": "dl_000001",
                "file_name": "a.bin",
                "state": "downloading",
                "downloaded": 1_048_576u64,
                "total": 10_485_760u64,
                "speed_bps": 524_288.0,
                "connections": 8u64,
                "active_connections": 6u64,
                "target_connections": 12u64,
                "peak_connections": 16u64,
            }],
            "global_speed_bps": 1_048_576.0,
            "active": 2u64,
        }));
        assert_eq!(
            snapshot,
            StatusSnapshot {
                active: 2,
                global_speed_bps: 1_048_576.0,
                downloads: vec![DownloadStatus {
                    id: "dl_000001".to_string(),
                    file_name: "a.bin".to_string(),
                    state: "downloading".to_string(),
                    downloaded: 1_048_576,
                    total: Some(10_485_760),
                    speed_bps: 524_288.0,
                    connections: 8,
                    active_connections: 6,
                    target_connections: 12,
                    peak_connections: 16,
                }],
            }
        );
    }

    #[test]
    fn missing_or_non_object_data_yields_empty_snapshot() {
        assert_eq!(parse_status_data(None), StatusSnapshot::default());
        for value in [json!(null), json!([]), json!("nope"), json!(42)] {
            assert_eq!(
                parse(value.clone()),
                StatusSnapshot::default(),
                "payload: {value}"
            );
        }
        assert_eq!(
            parse(json!({})),
            StatusSnapshot::default(),
            "a GUI answering ok:true without data must not be treated as an error"
        );
    }

    #[test]
    fn missing_download_fields_use_defaults() {
        let snapshot = parse(json!({ "downloads": [{ "id": "dl_1" }] }));
        assert_eq!(snapshot.active, 0);
        assert_eq!(snapshot.global_speed_bps, 0.0);
        let download = &snapshot.downloads[0];
        assert_eq!(download.id, "dl_1");
        assert_eq!(download.file_name, "");
        assert_eq!(download.state, "unknown");
        assert_eq!(download.downloaded, 0);
        assert_eq!(download.total, None);
        assert_eq!(download.speed_bps, 0.0);
        assert_eq!(download.connections, 0);
        assert_eq!(download.active_connections, 0);
        assert_eq!(download.target_connections, 0);
        assert_eq!(download.peak_connections, 0);
    }

    #[test]
    fn thread_count_fields_default_to_zero_for_old_or_bad_gui() {
        let snapshot = parse(json!({
            "downloads": [
                // Old GUI: the three additive fields are absent entirely.
                {"id": "dl_old", "file_name": "old.bin", "connections": 8u64},
                // Bad types / negative / fractional: degrade to 0, keep entry.
                {
                    "id": "dl_bad",
                    "active_connections": "many",
                    "target_connections": -3,
                    "peak_connections": 1.5,
                },
                // Overflowing float (2^64) must not saturate to u64::MAX.
                {"id": "dl_overflow", "active_connections": 1.844_674_407_370_955_2e19},
                // Normal values pass through untouched.
                {
                    "id": "dl_ok",
                    "active_connections": 6u64,
                    "target_connections": 12u64,
                    "peak_connections": 16u64,
                },
            ],
        }));
        assert_eq!(
            snapshot.downloads.len(),
            4,
            "bad thread counts must never drop a download entry: {snapshot:?}"
        );
        for (index, id) in ["dl_old", "dl_bad", "dl_overflow"].iter().enumerate() {
            let download = &snapshot.downloads[index];
            assert_eq!(&download.id, id);
            assert_eq!(download.active_connections, 0, "{id} active_connections");
            assert_eq!(download.target_connections, 0, "{id} target_connections");
            assert_eq!(download.peak_connections, 0, "{id} peak_connections");
        }
        let ok = &snapshot.downloads[3];
        assert_eq!(ok.id, "dl_ok");
        assert_eq!(ok.active_connections, 6);
        assert_eq!(ok.target_connections, 12);
        assert_eq!(ok.peak_connections, 16);
    }

    #[test]
    fn total_null_and_zero_values_are_preserved() {
        let snapshot = parse(json!({
            "downloads": [{
                "id": "dl_2",
                "file_name": "b.bin",
                "state": "paused",
                "downloaded": 0u64,
                "total": null,
                "speed_bps": 0.0,
                "connections": 4u64,
            }],
        }));
        let download = &snapshot.downloads[0];
        assert_eq!(download.total, None, "unknown size must stay null/None");
        assert_eq!(download.downloaded, 0);
        assert_eq!(download.speed_bps, 0.0);
    }

    #[test]
    fn non_array_downloads_and_bad_top_level_fields_fall_back() {
        let snapshot = parse(json!({
            "downloads": "not an array",
            "active": "many",
            "global_speed_bps": "fast",
        }));
        assert_eq!(snapshot, StatusSnapshot::default());

        let snapshot = parse(json!({ "downloads": null, "active": 3u64 }));
        assert_eq!(snapshot.active, 3);
        assert!(snapshot.downloads.is_empty());
    }

    #[test]
    fn invalid_top_level_numbers_fall_back_to_zero() {
        let snapshot = parse(json!({ "active": -1, "global_speed_bps": "x" }));
        assert_eq!(snapshot.active, 0);
        assert_eq!(snapshot.global_speed_bps, 0.0);

        let snapshot = parse(json!({ "active": 2.0, "global_speed_bps": 1024 }));
        assert_eq!(snapshot.active, 2, "integral floats are accepted");
        assert_eq!(snapshot.global_speed_bps, 1024.0);
    }

    #[test]
    fn skips_malformed_download_entries_but_keeps_good_ones() {
        let snapshot = parse(json!({
            "downloads": [
                null,
                "dl",
                7,
                {},
                {"id": 12, "file_name": "numeric-id.bin"},
                {"id": "", "file_name": "empty-id.bin"},
                {"id": "dl_bad_total", "total": "huge"},
                {"id": "dl_bad_speed", "speed_bps": "fast"},
                {"id": "dl_bad_state", "state": 3},
                {"id": "dl_bad_connections", "connections": "lots"},
                {
                    "id": "dl_ok",
                    "file_name": "ok.bin",
                    "state": "completed",
                    "downloaded": 10u64,
                    "total": 10u64,
                    "speed_bps": 0.0,
                    "connections": 2u64,
                    "unknown_field": {"ignored": true},
                },
            ],
            "active": 1u64,
        }));
        assert_eq!(
            snapshot.downloads.len(),
            1,
            "malformed entries must be skipped, not fatal: {snapshot:?}"
        );
        let download = &snapshot.downloads[0];
        assert_eq!(download.id, "dl_ok");
        assert_eq!(download.file_name, "ok.bin");
        assert_eq!(download.state, "completed");
        assert_eq!(download.total, Some(10));
    }

    #[test]
    fn accepts_integral_float_byte_counts() {
        let snapshot = parse(json!({
            "downloads": [{
                "id": "dl_float",
                "downloaded": 1_048_576.0,
                "total": 10_485_760.0,
                "connections": 8.0,
            }],
        }));
        let download = &snapshot.downloads[0];
        assert_eq!(download.downloaded, 1_048_576);
        assert_eq!(download.total, Some(10_485_760));
        assert_eq!(download.connections, 8);
    }

    #[test]
    fn rejects_fractional_negative_and_non_finite_numbers() {
        let snapshot = parse(json!({
            "downloads": [
                {"id": "dl_fractional", "downloaded": 1.5},
                {"id": "dl_negative", "downloaded": -1},
                {"id": "dl_negative_speed", "speed_bps": -2.0},
            ],
        }));
        assert!(
            snapshot.downloads.is_empty(),
            "invalid numbers must drop the entry: {snapshot:?}"
        );
    }
}
