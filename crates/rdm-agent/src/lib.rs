//! Shared agent-facing contracts for RDM.
//!
//! This crate is the single source of truth for:
//! * the JSON envelope / error taxonomy (`docs/agent-integration.md` §2)
//! * durable task records + per-task store (§2.4/§2.5)
//! * agent policy (§4)
//! * MCP/CLI tool definitions + JSON Schemas (§3.1, §5)
//!
//! Both `rdm-mcp` and the `rdm` CLI depend on this crate so their machine
//! contracts cannot drift apart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rdm_core::{DownloadError, DownloadRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCHEMA_VERSION: &str = "rdm.agent/v1";

/// MCP protocol versions this server can speak, newest first (contract §3).
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Protocol version preferred during `initialize` negotiation.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
/// Server name advertised in `serverInfo` and `capabilities`.
pub const SERVER_NAME: &str = "rdm";
/// Default approval token lifetime (contract §4).
pub const APPROVAL_TOKEN_TTL_MINUTES: i64 = 10;

pub mod tools;

pub use tools::{
    capabilities, capabilities_for, tool_by_name, tool_definitions, ToolAnnotations, ToolDefinition,
};

// ---------------------------------------------------------------------------
// Timestamps / ids
// ---------------------------------------------------------------------------

/// Current UTC time as ISO-8601 (`2026-10-19T03:20:00.123Z`).
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_from_unix(now.as_secs() as i64, now.subsec_millis())
}

/// ISO-8601 UTC timestamp `minutes` minutes in the future (negative values are
/// in the past). Used for approval-plan expiry.
pub fn iso8601_in_minutes(minutes: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64 + minutes.saturating_mul(60);
    iso8601_from_unix(secs, now.subsec_millis())
}

/// True when an ISO-8601 UTC timestamp produced by this crate is in the past.
/// Comparisons are lexicographic because every encoded timestamp uses the same
/// fixed-width UTC layout.
pub fn iso8601_expired(timestamp: &str) -> bool {
    let now = now_iso8601();
    now.as_str() > timestamp
}

fn iso8601_from_unix(secs: i64, millis: u32) -> String {
    // civil-from-days (Howard Hinnant), valid for the ranges we care about.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let sod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Stable, sortable task id: `tsk_<unix_millis>_<hex4>`.
pub fn new_task_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff;
    format!("tsk_{millis}_{seq:04x}")
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Stable error taxonomy. Do not rename existing variants; add new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidUrl,
    PolicyDenied,
    NeedsApproval,
    NotFound,
    Conflict,
    NetworkError,
    HttpStatus,
    RangeUnsupported,
    NoContentLength,
    ChecksumMismatch,
    ResumeInvalid,
    RemoteChanged,
    DiskFull,
    PermissionDenied,
    Cancelled,
    Paused,
    Timeout,
    Unsupported,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidUrl => "invalid_url",
            Self::PolicyDenied => "policy_denied",
            Self::NeedsApproval => "needs_approval",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::NetworkError => "network_error",
            Self::HttpStatus => "http_status",
            Self::RangeUnsupported => "range_unsupported",
            Self::NoContentLength => "no_content_length",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::ResumeInvalid => "resume_invalid",
            Self::RemoteChanged => "remote_changed",
            Self::DiskFull => "disk_full",
            Self::PermissionDenied => "permission_denied",
            Self::Cancelled => "cancelled",
            Self::Paused => "paused",
            Self::Timeout => "timeout",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::NetworkError | Self::Timeout | Self::ResumeInvalid | Self::RemoteChanged
        )
    }

    /// Every stable error code, in declaration order (schema/enumeration use).
    pub fn all() -> &'static [ErrorCode] {
        &[
            Self::InvalidRequest,
            Self::InvalidUrl,
            Self::PolicyDenied,
            Self::NeedsApproval,
            Self::NotFound,
            Self::Conflict,
            Self::NetworkError,
            Self::HttpStatus,
            Self::RangeUnsupported,
            Self::NoContentLength,
            Self::ChecksumMismatch,
            Self::ResumeInvalid,
            Self::RemoteChanged,
            Self::DiskFull,
            Self::PermissionDenied,
            Self::Cancelled,
            Self::Paused,
            Self::Timeout,
            Self::Unsupported,
            Self::Internal,
        ]
    }

    /// Parse the wire name (`snake_case`) back into an error code.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|code| code.as_str() == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentErrorInfo {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl AgentErrorInfo {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.retryable(),
            hint: None,
            details: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Map an engine error into the stable agent taxonomy.
    pub fn from_download_error(error: &DownloadError) -> Self {
        use DownloadError as E;
        let (code, hint): (ErrorCode, Option<&str>) = match error {
            E::Http(_) => (ErrorCode::NetworkError, Some("检查网络/代理/DNS 后重试")),
            E::Io(_) => (ErrorCode::Internal, None),
            E::InvalidUrl(_) => (ErrorCode::InvalidUrl, Some("只接受 http(s) URL")),
            E::Status { .. } => (
                ErrorCode::HttpStatus,
                Some("4xx 通常不可重试；429/5xx 可退避后重试"),
            ),
            E::NoRangeSupport => (
                ErrorCode::RangeUnsupported,
                Some("服务器忽略 Range；用 connections=1 单流下载，或更换镜像"),
            ),
            E::RemoteChanged => (
                ErrorCode::RemoteChanged,
                Some("远程文件已变化，旧 .part 状态已丢弃，可安全重下"),
            ),
            E::ChecksumMismatch { .. } => (
                ErrorCode::ChecksumMismatch,
                Some("校验失败且未生成目标文件；确认 expected_sha256 或重下"),
            ),
            E::InvalidState { .. } => (
                ErrorCode::ResumeInvalid,
                Some("恢复状态无效；删除对应 .part/.part.json 后重试"),
            ),
            E::Paused { .. } => (ErrorCode::Paused, None),
            E::Cancelled { .. } => (ErrorCode::Cancelled, None),
            E::Unsupported(_) => (ErrorCode::Unsupported, None),
            E::Other(_) => (ErrorCode::Internal, None),
        };
        let mut info = Self::new(code, redact_text(&error.to_string()));
        if let Some(hint) = hint {
            info = info.with_hint(hint);
        }
        if let E::Status { status, url } = error {
            info.details = Some(serde_json::json!({
                "status": status,
                "url": redact_url(url),
            }));
        }
        info
    }
}

impl std::fmt::Display for AgentErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

// ---------------------------------------------------------------------------
// CLI envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeMeta {
    pub rdm_version: String,
    pub elapsed_ms: u64,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl Default for EnvelopeMeta {
    fn default() -> Self {
        Self {
            rdm_version: env!("CARGO_PKG_VERSION").to_string(),
            elapsed_ms: 0,
            ts: now_iso8601(),
            trace_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub schema_version: String,
    pub ok: bool,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub data: Option<Value>,
    pub error: Option<AgentErrorInfo>,
    pub meta: EnvelopeMeta,
}

impl Envelope {
    pub fn ok(command: impl Into<String>, data: impl Serialize) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            ok: true,
            command: command.into(),
            task_id: None,
            data: serde_json::to_value(data).ok(),
            error: None,
            meta: EnvelopeMeta::default(),
        }
    }

    pub fn error(command: impl Into<String>, error: AgentErrorInfo) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            ok: false,
            command: command.into(),
            task_id: None,
            data: None,
            error: Some(error),
            meta: EnvelopeMeta::default(),
        }
    }

    pub fn with_task(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Probing,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Probing => "probing",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// States in declaration order (schema/enumeration use).
    pub fn all() -> &'static [TaskState] {
        &[
            Self::Queued,
            Self::Probing,
            Self::Downloading,
            Self::Paused,
            Self::Completed,
            Self::Failed,
            Self::Cancelled,
        ]
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|state| state.as_str() == name)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskResult {
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub elapsed_ms: u64,
    pub avg_speed_bps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    #[serde(default = "schema_version_default")]
    pub schema_version: String,
    pub id: String,
    pub state: TaskState,
    #[serde(default)]
    pub source: String,
    /// Full request (may contain cookies). NEVER serialize directly to an
    /// agent-visible surface; call [`TaskRecord::redacted`] first.
    pub request: DownloadRequest,
    pub file_name: String,
    pub dest_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub connections: usize,
    pub chunk_size: Option<u64>,
    pub range_supported: Option<bool>,
    pub http_version: Option<String>,
    pub speed_bps: f64,
    pub eta_secs: Option<f64>,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub error: Option<AgentErrorInfo>,
    pub result: Option<TaskResult>,
    pub idempotency_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

fn schema_version_default() -> String {
    SCHEMA_VERSION.to_string()
}

/// Sensitive header names (case-insensitive) that must never be echoed.
pub const SENSITIVE_HEADERS: &[&str] = &["authorization", "proxy-authorization", "cookie"];

/// Object keys (case-insensitive) that always carry credentials.
pub const SENSITIVE_KEYS: &[&str] = &[
    "cookie",
    "set-cookie",
    "authorization",
    "proxy-authorization",
];

/// Remove `user:password@` from a URL while keeping it parseable. Unparseable
/// input is returned unchanged (nothing to leak by accident in that case).
pub fn redact_url(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    if parsed.username().is_empty() && parsed.password().is_none() {
        return raw.to_string();
    }
    let _ = parsed.set_password(None);
    let _ = parsed.set_username("");
    parsed.to_string()
}

/// Best-effort credential stripping for free-form text (error messages, log
/// lines). Removes `user:secret@` inside any URL-looking token while keeping
/// the rest of the message intact.
pub fn redact_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let rest = &text[index..];
        // Find the next `://` and try to redact the authority userinfo.
        if let Some(scheme_end) = rest.find("://") {
            let after = index + scheme_end + 3;
            out.push_str(&text[index..after]);
            let tail = &text[after..];
            let authority_end = tail
                .find(|ch: char| ch == '/' || ch == '?' || ch == '#' || ch.is_whitespace())
                .unwrap_or(tail.len());
            let authority = &tail[..authority_end];
            match authority.rsplit_once('@') {
                Some((_userinfo, host)) => {
                    out.push_str("<redacted>@");
                    out.push_str(host);
                }
                None => out.push_str(authority),
            }
            index = after + authority_end;
            continue;
        }
        out.push_str(rest);
        break;
    }
    out
}

/// Recursively redact credential-bearing values in agent-visible JSON:
/// * object keys named like `cookie`/`authorization` become `"<redacted>"`
/// * values of a `headers` object lose sensitive header names
/// * url-like strings (`url`, `final_url`, `proxy`, `referer`) lose userinfo
///
/// The original value is never mutated; a sanitized copy is returned.
pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                let lowered = key.to_ascii_lowercase();
                if SENSITIVE_KEYS.contains(&lowered.as_str()) {
                    out.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else if lowered == "headers" {
                    if let Value::Object(headers) = child {
                        let mut filtered = serde_json::Map::with_capacity(headers.len());
                        for (name, header_value) in headers {
                            if SENSITIVE_HEADERS
                                .iter()
                                .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
                            {
                                continue;
                            }
                            filtered.insert(name.clone(), redact_value(header_value));
                        }
                        out.insert(key.clone(), Value::Object(filtered));
                    } else {
                        out.insert(key.clone(), redact_value(child));
                    }
                } else if matches!(lowered.as_str(), "url" | "final_url" | "proxy" | "referer") {
                    match child {
                        Value::String(text) => {
                            out.insert(key.clone(), Value::String(redact_url(text)));
                        }
                        other => {
                            out.insert(key.clone(), redact_value(other));
                        }
                    }
                } else {
                    out.insert(key.clone(), redact_value(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

/// Returns a copy of `request` with credentials stripped (for output/logs).
pub fn redact_request(request: &DownloadRequest) -> DownloadRequest {
    let mut out = request.clone();
    out.cookie = None;
    out.url = redact_url(&request.url);
    out.proxy = request.proxy.as_deref().map(redact_url);
    out.headers.retain(|name, _| {
        !SENSITIVE_HEADERS
            .iter()
            .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
    });
    out
}

impl TaskRecord {
    pub fn new(
        source: impl Into<String>,
        request: DownloadRequest,
        dest_dir: PathBuf,
        file_name: impl Into<String>,
    ) -> Self {
        let now = now_iso8601();
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            id: new_task_id(),
            state: TaskState::Queued,
            source: source.into(),
            connections: request.connections,
            chunk_size: request.chunk_size,
            file_name: file_name.into(),
            dest_dir,
            path: None,
            total_bytes: None,
            downloaded_bytes: 0,
            range_supported: None,
            http_version: None,
            speed_bps: 0.0,
            eta_secs: None,
            warnings: Vec::new(),
            error: None,
            result: None,
            idempotency_key: None,
            request,
            created_at: now.clone(),
            updated_at: now,
            finished_at: None,
        }
    }

    /// Agent-visible copy: credentials removed, request redacted.
    pub fn redacted(&self) -> Self {
        let mut out = self.clone();
        out.request = redact_request(&self.request);
        out
    }

    /// Contract §2.4 view (the exact shape CLI/MCP expose to agents).
    pub fn view(&self) -> TaskView {
        TaskView::from(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub state: TaskState,
    pub url: String,
    pub file_name: String,
    pub dest_dir: PathBuf,
    pub path: Option<PathBuf>,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub connections: usize,
    pub speed_bps: f64,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&TaskRecord> for TaskSummary {
    fn from(record: &TaskRecord) -> Self {
        Self {
            id: record.id.clone(),
            state: record.state,
            url: redact_url(&record.request.url),
            file_name: record.file_name.clone(),
            dest_dir: record.dest_dir.clone(),
            path: record.path.clone(),
            total_bytes: record.total_bytes,
            downloaded_bytes: record.downloaded_bytes,
            connections: record.connections,
            speed_bps: record.speed_bps,
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
        }
    }
}

/// Agent-facing task shape from `docs/agent-integration.md` §2.4.
///
/// This is the only task shape that may cross the MCP/CLI boundary: it drops
/// the raw [`DownloadRequest`] entirely (so cookies/headers can never leak) and
/// redacts any userinfo embedded in the URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub schema_version: String,
    pub id: String,
    pub state: TaskState,
    pub source: String,
    pub url: String,
    pub file_name: String,
    pub dest_dir: PathBuf,
    pub path: Option<PathBuf>,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub connections: usize,
    pub chunk_size: Option<u64>,
    pub range_supported: Option<bool>,
    pub http_version: Option<String>,
    pub speed_bps: f64,
    pub eta_secs: Option<f64>,
    pub warnings: Vec<String>,
    pub error: Option<AgentErrorInfo>,
    pub result: Option<TaskResult>,
    pub idempotency_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

impl From<&TaskRecord> for TaskView {
    fn from(record: &TaskRecord) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            id: record.id.clone(),
            state: record.state,
            source: record.source.clone(),
            url: redact_url(&record.request.url),
            file_name: record.file_name.clone(),
            dest_dir: record.dest_dir.clone(),
            path: record.path.clone(),
            total_bytes: record.total_bytes,
            downloaded_bytes: record.downloaded_bytes,
            connections: record.connections,
            chunk_size: record.chunk_size,
            range_supported: record.range_supported,
            http_version: record.http_version.clone(),
            speed_bps: record.speed_bps,
            eta_secs: record.eta_secs,
            warnings: record.warnings.clone(),
            error: record.error.clone(),
            result: record.result.clone(),
            idempotency_key: record.idempotency_key.clone(),
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            finished_at: record.finished_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskFilter {
    #[serde(default)]
    pub states: Option<Vec<TaskState>>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub schema_version: String,
    pub r#type: String,
    pub task_id: String,
    pub seq: u64,
    pub ts: String,
    pub data: Value,
}

impl TaskEvent {
    pub fn new(task_id: impl Into<String>, seq: u64, kind: impl Into<String>, data: Value) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            r#type: kind.into(),
            task_id: task_id.into(),
            seq,
            ts: now_iso8601(),
            data,
        }
    }
}

/// Per-task-file store under `<state_dir>/agent/`.
///
/// Layout (see docs/agent-integration.md §2.5):
/// * `tasks/<id>.json`     atomic per-task record
/// * `events/<id>.jsonl`   append-only redacted event journal
/// * `plans/<plan>.json`   pending approvals
#[derive(Debug, Clone)]
pub struct TaskStore {
    root: PathBuf,
}

impl TaskStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default store root next to the GUI/native state dir.
    ///
    /// `RDM_AGENT_STATE_DIR` overrides the location (used by tests, QA and
    /// multi-workspace setups); otherwise `%LOCALAPPDATA%\RDM\agent` is used.
    pub fn default_root() -> PathBuf {
        if let Some(dir) = std::env::var_os("RDM_AGENT_STATE_DIR") {
            return PathBuf::from(dir);
        }
        rdm_ipc::state_dir().join("agent")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Task/plan ids are also used as file names, so only a conservative
    /// character set is accepted (blocks `../` traversal from agent input).
    pub fn is_valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 160
            && id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    }

    fn checked_id(id: &str) -> Result<&str, AgentStoreError> {
        if Self::is_valid_id(id) {
            Ok(id)
        } else {
            Err(AgentStoreError::InvalidId(id.to_string()))
        }
    }

    pub fn ensure_layout(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.root.join("tasks"))?;
        std::fs::create_dir_all(self.root.join("events"))?;
        std::fs::create_dir_all(self.root.join("plans"))?;
        Ok(())
    }

    pub fn create(&self, record: &TaskRecord) -> Result<(), AgentStoreError> {
        Self::checked_id(&record.id)?;
        self.ensure_layout()?;
        self.write_record(record)
    }

    pub fn update(&self, record: &TaskRecord) -> Result<(), AgentStoreError> {
        Self::checked_id(&record.id)?;
        self.ensure_layout()?;
        self.write_record(record)
    }

    pub fn delete(&self, id: &str) -> Result<(), AgentStoreError> {
        let id = Self::checked_id(id)?;
        let path = self.root.join("tasks").join(format!("{id}.json"));
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AgentStoreError::Io(error)),
        }
    }

    pub fn get(&self, id: &str) -> Result<TaskRecord, AgentStoreError> {
        let id = Self::checked_id(id)?;
        let path = self.root.join("tasks").join(format!("{id}.json"));
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AgentStoreError::NotFound(id.to_string())
            } else {
                AgentStoreError::Io(e)
            }
        })?;
        serde_json::from_str(&text).map_err(AgentStoreError::Serde)
    }

    pub fn list(&self, filter: &TaskFilter) -> Result<Vec<TaskRecord>, AgentStoreError> {
        let dir = self.root.join("tasks");
        let mut records = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(e) => return Err(AgentStoreError::Io(e)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<TaskRecord>(&text) else {
                continue;
            };
            if let Some(states) = &filter.states {
                if !states.contains(&record.state) {
                    continue;
                }
            }
            records.push(record);
        }
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let start = filter
            .cursor
            .as_deref()
            .and_then(|cursor| records.iter().position(|r| r.id == cursor))
            .map(|index| index + 1)
            .unwrap_or(0);
        let limit = filter.limit.unwrap_or(50);
        Ok(records.into_iter().skip(start).take(limit).collect())
    }

    pub fn append_event(&self, event: &TaskEvent) -> Result<(), AgentStoreError> {
        Self::checked_id(&event.task_id)?;
        self.ensure_layout()?;
        use std::io::Write;
        let path = self
            .root
            .join("events")
            .join(format!("{}.jsonl", event.task_id));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        // Defensive redaction: callers should already build credential-free
        // event data, but the journal must never be able to leak.
        let mut event = event.clone();
        event.data = redact_value(&event.data);
        let line = serde_json::to_string(&event).map_err(AgentStoreError::Serde)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn events(&self, id: &str, limit: usize) -> Result<Vec<TaskEvent>, AgentStoreError> {
        let id = Self::checked_id(id)?;
        let path = self.root.join("events").join(format!("{id}.jsonl"));
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(AgentStoreError::Io(e)),
        };
        let mut events: Vec<TaskEvent> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if limit > 0 && events.len() > limit {
            events = events.split_off(events.len() - limit);
        }
        Ok(events)
    }

    // -- pending approval plans (contract §2.5 `plans/<plan_id>.json`) ------

    pub fn save_plan(&self, plan: &DownloadPlan) -> Result<(), AgentStoreError> {
        Self::checked_id(&plan.plan_id)?;
        self.ensure_layout()?;
        let path = self
            .root
            .join("plans")
            .join(format!("{}.json", plan.plan_id));
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(plan).map_err(AgentStoreError::Serde)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get_plan(&self, plan_id: &str) -> Result<DownloadPlan, AgentStoreError> {
        let plan_id = Self::checked_id(plan_id)?;
        let path = self.root.join("plans").join(format!("{plan_id}.json"));
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AgentStoreError::NotFound(plan_id.to_string())
            } else {
                AgentStoreError::Io(e)
            }
        })?;
        serde_json::from_str(&text).map_err(AgentStoreError::Serde)
    }

    pub fn delete_plan(&self, plan_id: &str) -> Result<(), AgentStoreError> {
        let plan_id = Self::checked_id(plan_id)?;
        let path = self.root.join("plans").join(format!("{plan_id}.json"));
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AgentStoreError::Io(error)),
        }
    }

    pub fn list_plans(&self) -> Result<Vec<DownloadPlan>, AgentStoreError> {
        let dir = self.root.join("plans");
        let mut plans = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(plans),
            Err(error) => return Err(AgentStoreError::Io(error)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(plan) = serde_json::from_str::<DownloadPlan>(&text) {
                plans.push(plan);
            }
        }
        plans.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(plans)
    }

    /// Delete expired plan files, returning how many were removed.
    pub fn prune_expired_plans(&self) -> Result<usize, AgentStoreError> {
        let plans = self.list_plans()?;
        let mut removed = 0;
        for plan in plans {
            if iso8601_expired(&plan.expires_at) {
                self.delete_plan(&plan.plan_id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<TaskRecord>, AgentStoreError> {
        let dir = self.root.join("tasks");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(AgentStoreError::Io(e)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<TaskRecord>(&text) else {
                continue;
            };
            if record.idempotency_key.as_deref() == Some(key) {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    fn write_record(&self, record: &TaskRecord) -> Result<(), AgentStoreError> {
        let path = self.root.join("tasks").join(format!("{}.json", record.id));
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(record).map_err(AgentStoreError::Serde)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentStoreError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(serde_json::Error),
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("invalid id: {0}")]
    InvalidId(String),
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default = "schema_version_default")]
    pub schema_version: String,
    #[serde(default = "default_allowed_hosts")]
    pub allowed_hosts: Vec<String>,
    #[serde(default = "default_allowed_dirs")]
    pub allowed_dirs: Vec<PathBuf>,
    #[serde(default = "default_max_bytes")]
    pub max_bytes_per_task: Option<u64>,
    #[serde(default = "default_approval_bytes")]
    pub require_approval_over_bytes: Option<u64>,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_tasks: usize,
    #[serde(default = "default_connections")]
    pub default_connections: usize,
    #[serde(default)]
    pub allow_private_hosts: bool,
    #[serde(default)]
    pub allow_overwrite: bool,
}

fn default_allowed_hosts() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_allowed_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(home) => vec![home.join("Downloads")],
        None => vec![std::env::temp_dir()],
    }
}
fn default_max_bytes() -> Option<u64> {
    Some(10 * 1024 * 1024 * 1024)
}
fn default_approval_bytes() -> Option<u64> {
    Some(1024 * 1024 * 1024)
}
fn default_max_concurrent() -> usize {
    3
}
fn default_connections() -> usize {
    rdm_core::DEFAULT_CONNECTIONS
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            allowed_hosts: default_allowed_hosts(),
            allowed_dirs: default_allowed_dirs(),
            max_bytes_per_task: default_max_bytes(),
            require_approval_over_bytes: default_approval_bytes(),
            max_concurrent_tasks: default_max_concurrent(),
            default_connections: default_connections(),
            // Safe default: loopback/private/metadata endpoints are rejected
            // unless a policy file explicitly opts in (contract §4).
            allow_private_hosts: false,
            allow_overwrite: false,
        }
    }
}

/// Outcome of a policy pre-check after probing remote metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    NeedsApproval {
        reason: String,
        total_bytes: Option<u64>,
    },
    Deny {
        reason: String,
    },
}

impl PolicyDecision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }

    pub fn denied_reason(self) -> Option<String> {
        match self {
            Self::Deny { reason } => Some(reason),
            _ => None,
        }
    }
}

impl Policy {
    /// Path of the optional policy file inside an agent state root.
    pub fn path_for(agent_root: &Path) -> PathBuf {
        agent_root.join("policy.json")
    }

    /// Load `policy.json` from the given store root, or fall back to defaults.
    /// Invalid JSON also falls back to defaults (fail-safe, never fail-open to
    /// "no policy").
    pub fn load_or_default(agent_root: &Path) -> Self {
        let path = Self::path_for(agent_root);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Policy>(&text)
                .unwrap_or_default()
                .normalized(),
            Err(_) => Self::default().normalized(),
        }
    }

    /// Clamp nonsense values into the supported range.
    pub fn normalized(mut self) -> Self {
        self.max_concurrent_tasks = self.max_concurrent_tasks.clamp(1, 32);
        self.default_connections = self.default_connections.clamp(1, rdm_core::MAX_CONNECTIONS);
        self
    }

    /// True when `host` matches one of `allowed_hosts`.
    ///
    /// Supports exact names, `*` and a single leading wildcard label
    /// (`*.example.com`), which also matches `example.com` itself (a wildcard
    /// certificate for `*.example.com` covers the bare domain in DNS
    /// conventions, and agents expect the pattern to cover the apex).
    pub fn check_host(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.allowed_hosts.iter().any(|pattern| {
            let pattern = pattern.trim().to_ascii_lowercase();
            if pattern == "*" {
                return true;
            }
            if let Some(suffix) = pattern.strip_prefix("*.") {
                return host == suffix || host.ends_with(&format!(".{suffix}"));
            }
            if pattern.starts_with("*") {
                // Only a leading `*.` label wildcard is meaningful; other
                // wildcard forms are treated as literal (safe default).
                return false;
            }
            host == pattern
        })
    }

    /// True when `dir` is inside one of `allowed_dirs`.
    ///
    /// The directory does not have to exist yet: the nearest existing ancestor
    /// is canonicalized (symlinks resolved) and the remaining components are
    /// appended lexically, so `Downloads\agent\new-subdir` is allowed while
    /// `Downloads\..\..\Windows` is not.
    pub fn check_dest_dir(&self, dir: &Path) -> bool {
        let Some(candidate) = canonicalize_lenient(dir) else {
            return false;
        };
        self.allowed_dirs.iter().any(|allowed| {
            canonicalize_lenient(allowed)
                .map(|allowed| candidate.starts_with(&allowed))
                .unwrap_or(false)
        })
    }

    /// Check the URL portion of a download request (scheme / host / private).
    pub fn check_url(&self, raw_url: &str) -> PolicyDecision {
        let Ok(parsed) = url::Url::parse(raw_url) else {
            return PolicyDecision::Deny {
                reason: format!("invalid URL: {raw_url}"),
            };
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return PolicyDecision::Deny {
                reason: format!("unsupported URL scheme: {}", parsed.scheme()),
            };
        }
        let Some(host) = parsed.host_str() else {
            return PolicyDecision::Deny {
                reason: "URL has no host".to_string(),
            };
        };
        if !self.allow_private_hosts && is_private_host(host) {
            return PolicyDecision::Deny {
                reason: format!("private host `{host}` is not allowed by policy"),
            };
        }
        if !self.check_host(host) {
            return PolicyDecision::Deny {
                reason: format!("host `{host}` is not in policy.allowed_hosts"),
            };
        }
        PolicyDecision::Allow
    }

    /// Size gate applied after the remote size is known.
    pub fn gate(&self, total_bytes: Option<u64>) -> PolicyDecision {
        if let (Some(total), Some(max)) = (total_bytes, self.max_bytes_per_task) {
            if total > max {
                return PolicyDecision::Deny {
                    reason: format!("remote file is {total} bytes, policy max is {max}"),
                };
            }
        }
        if let (Some(total), Some(threshold)) = (total_bytes, self.require_approval_over_bytes) {
            if total > threshold {
                return PolicyDecision::NeedsApproval {
                    reason: format!(
                        "remote file is {total} bytes, approval threshold is {threshold}"
                    ),
                    total_bytes,
                };
            }
        }
        PolicyDecision::Allow
    }

    /// Agent-visible policy summary (absolute paths, no secrets).
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "schema_version": self.schema_version,
            "allowed_hosts": self.allowed_hosts,
            "allowed_dirs": self
                .allowed_dirs
                .iter()
                .map(|dir| canonicalize_lenient(dir).unwrap_or_else(|| dir.clone()))
                .collect::<Vec<_>>(),
            "max_bytes_per_task": self.max_bytes_per_task,
            "require_approval_over_bytes": self.require_approval_over_bytes,
            "max_concurrent_tasks": self.max_concurrent_tasks,
            "default_connections": self.default_connections,
            "allow_private_hosts": self.allow_private_hosts,
            "allow_overwrite": self.allow_overwrite,
        })
    }
}

/// Best-effort absolute + canonical path for a directory that may not exist.
fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    // Find the longest existing prefix, canonicalize it, then re-apply the
    // remaining components lexically (`.` skipped, `..` pops).
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = absolute.as_path();
    let canonical_prefix = loop {
        if cursor.exists() {
            break std::fs::canonicalize(cursor).ok()?;
        }
        let file_name = cursor.file_name()?.to_os_string();
        rest.push(file_name);
        cursor = cursor.parent()?;
    };
    let mut out = canonical_prefix;
    for component in rest.into_iter().rev() {
        if component == "." {
            continue;
        }
        if component == ".." {
            out.pop();
            continue;
        }
        out.push(component);
    }
    Some(out)
}

/// Cloud metadata service hostnames (SSRF targets even when they resolve to a
/// public-looking address via a hosts file).
pub const METADATA_HOSTS: &[&str] = &[
    "metadata",
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
    "metadata.azure.internal",
];

/// True for loopback / RFC1918 / link-local / cloud-metadata / unspecified
/// hosts. Used to enforce the `allow_private_hosts=false` safe default.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if METADATA_HOSTS.contains(&host.as_str()) || host.ends_with(".internal") {
        return true;
    }
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        // Other hostnames are not classified as private (DNS pinning is out of
        // scope for a local policy; the engine re-checks the final URL host).
        return false;
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            // `is_link_local` also covers the 169.254.169.254 metadata service.
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

// ---------------------------------------------------------------------------
// Approval plans / tokens (contract §4)
// ---------------------------------------------------------------------------

/// Stable, sortable plan id: `plan_<unix_millis>_<hex4>`.
pub fn new_plan_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff;
    format!("plan_{millis}_{seq:04x}")
}

/// Opaque single-use approval token (32 hex chars, 128 bits of entropy).
pub fn new_approval_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// Opaque trace id used to correlate agent calls with logs.
pub fn new_trace_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 6];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("tr_{}", hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// A pending download plan produced by `rdm_plan_download` / `rdm_download`
/// when policy requires explicit approval. Persisted at
/// `<agent root>/plans/<plan_id>.json` (contains the full request, including
/// credentials, which is why only [`DownloadPlan::summary`] is agent-visible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadPlan {
    #[serde(default = "schema_version_default")]
    pub schema_version: String,
    pub plan_id: String,
    /// Full request to execute on approval (local state dir only).
    pub request: DownloadRequest,
    pub total_bytes: Option<u64>,
    pub requires_approval: bool,
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub source: String,
    /// Policy `allow_overwrite` snapshot at planning time.
    #[serde(default)]
    pub allow_overwrite: bool,
}

impl DownloadPlan {
    pub fn new(
        request: DownloadRequest,
        total_bytes: Option<u64>,
        requires_approval: bool,
        reason: Option<String>,
        idempotency_key: Option<String>,
        allow_overwrite: bool,
        source: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            plan_id: new_plan_id(),
            request,
            total_bytes,
            requires_approval,
            reason,
            created_at: now_iso8601(),
            expires_at: iso8601_in_minutes(APPROVAL_TOKEN_TTL_MINUTES),
            idempotency_key,
            source: source.into(),
            allow_overwrite,
        }
    }

    pub fn is_expired(&self) -> bool {
        iso8601_expired(&self.expires_at)
    }

    /// Agent-visible plan shape: request dropped, only operational fields.
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "plan_id": self.plan_id,
            "url": redact_url(&self.request.url),
            "dest_dir": self.request.output_dir,
            "file_name": self.request.file_name,
            "connections": self.request.connections,
            "chunk_size": self.request.chunk_size,
            "expected_sha256": self.request.expected_sha256,
            "speed_limit_bps": self.request.speed_limit_bps,
            "total_bytes": self.total_bytes,
            "requires_approval": self.requires_approval,
            "reason": self.reason,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "idempotency_key": self.idempotency_key,
        })
    }
}

/// In-memory single-use approval token (never persisted; contract §4).
#[derive(Debug, Clone)]
pub struct ApprovalTokenRecord {
    pub token: String,
    pub plan_id: String,
    pub expires_at: String,
}

impl ApprovalTokenRecord {
    pub fn for_plan(plan: &DownloadPlan) -> Self {
        Self {
            token: new_approval_token(),
            plan_id: plan.plan_id.clone(),
            expires_at: plan.expires_at.clone(),
        }
    }

    pub fn is_expired(&self) -> bool {
        iso8601_expired(&self.expires_at)
    }
}

// ---------------------------------------------------------------------------
// Tool definitions live in `tools.rs` and are re-exported above:
//   tool_definitions(), tool_by_name(), capabilities(), capabilities_for()
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(ErrorCode::InvalidUrl.as_str(), "invalid_url");
        assert!(ErrorCode::NetworkError.retryable());
        assert!(!ErrorCode::ChecksumMismatch.retryable());
    }

    #[test]
    fn redaction_strips_credentials() {
        let mut request = DownloadRequest::new("https://example.com/a.bin");
        request.cookie = Some("session=secret".into());
        request
            .headers
            .insert("Authorization".into(), "Bearer x".into());
        request.headers.insert("X-Trace".into(), "ok".into());
        let redacted = redact_request(&request);
        assert!(redacted.cookie.is_none());
        assert!(!redacted
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization")));
        assert_eq!(
            redacted.headers.get("X-Trace").map(String::as_str),
            Some("ok")
        );
    }

    #[test]
    fn timestamp_has_iso_shape() {
        let now = now_iso8601();
        assert_eq!(now.len(), 24);
        assert!(now.ends_with('Z'));
        assert_eq!(&now[4..5], "-");
    }

    #[test]
    fn policy_host_and_size_gate() {
        let policy = Policy::default();
        assert!(policy.check_host("example.com"));
        assert!(policy.check_host("anything.test"));
        let restricted = Policy {
            allowed_hosts: vec!["*.example.com".into()],
            ..Policy::default()
        };
        assert!(restricted.check_host("cdn.example.com"));
        assert!(!restricted.check_host("evil.test"));
        let tiny = Policy {
            require_approval_over_bytes: Some(10),
            max_bytes_per_task: Some(100),
            ..Policy::default()
        };
        assert!(matches!(
            tiny.gate(Some(11)),
            PolicyDecision::NeedsApproval { .. }
        ));
        assert!(matches!(tiny.gate(Some(101)), PolicyDecision::Deny { .. }));
    }

    #[test]
    fn task_store_roundtrip() {
        let root = std::env::temp_dir().join(format!("rdm-agent-test-{}", std::process::id()));
        let store = TaskStore::new(&root);
        let request = DownloadRequest::new("https://example.com/a.bin");
        let record = TaskRecord::new("test", request, root.join("out"), "a.bin");
        store.create(&record).unwrap();
        let loaded = store.get(&record.id).unwrap();
        assert_eq!(loaded.id, record.id);
        assert_eq!(loaded.state, TaskState::Queued);
        assert_eq!(store.list(&TaskFilter::default()).unwrap().len(), 1);
        store
            .append_event(&TaskEvent::new(
                &record.id,
                1,
                "created",
                serde_json::json!({}),
            ))
            .unwrap();
        assert_eq!(store.events(&record.id, 10).unwrap().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    // -- helpers ----------------------------------------------------------

    fn temp_root(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let index = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rdm-agent-{tag}-{}-{index}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_request(url: &str) -> DownloadRequest {
        DownloadRequest::new(url)
    }

    // -- ids / timestamps -------------------------------------------------

    #[test]
    fn ids_are_unique_and_well_formed() {
        let mut task_ids = std::collections::HashSet::new();
        for _ in 0..500 {
            let id = new_task_id();
            assert!(id.starts_with("tsk_"), "unexpected id {id}");
            assert!(TaskStore::is_valid_id(&id));
            assert!(task_ids.insert(id));
        }
        let plan_id = new_plan_id();
        assert!(plan_id.starts_with("plan_"));
        assert!(TaskStore::is_valid_id(&plan_id));
        let token = new_approval_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(new_approval_token(), token);
        let trace = new_trace_id();
        assert!(trace.starts_with("tr_"));
    }

    #[test]
    fn iso8601_helpers_agree() {
        let now = now_iso8601();
        assert_eq!(now.len(), 24);
        assert!(iso8601_expired(&iso8601_in_minutes(-5)));
        assert!(!iso8601_expired(&iso8601_in_minutes(10)));
        assert_eq!(iso8601_in_minutes(10).len(), 24);
    }

    #[test]
    fn error_and_state_names_roundtrip() {
        for code in ErrorCode::all() {
            assert_eq!(ErrorCode::from_name(code.as_str()), Some(*code));
        }
        assert!(ErrorCode::from_name("nope").is_none());
        for state in TaskState::all() {
            assert_eq!(TaskState::from_name(state.as_str()), Some(*state));
        }
        assert!(TaskState::from_name("nope").is_none());
        assert!(TaskState::Completed.is_terminal());
        assert!(!TaskState::Paused.is_terminal());
    }

    // -- redaction --------------------------------------------------------

    #[test]
    fn redact_url_strips_userinfo_only() {
        assert_eq!(
            redact_url("https://user:pass@example.com/a.bin?x=1"),
            "https://example.com/a.bin?x=1"
        );
        assert_eq!(
            redact_url("https://example.com/a.bin"),
            "https://example.com/a.bin"
        );
        // Unparseable strings are returned verbatim (no partial leak possible).
        assert_eq!(redact_url("not a url"), "not a url");
    }

    #[test]
    fn redact_text_scrubs_embedded_url_credentials() {
        let text = "request to https://user:secret@example.com/a?b=1 failed; retry https://x:y@h/p";
        let redacted = redact_text(text);
        assert!(!redacted.contains("user:secret"));
        assert!(!redacted.contains("x:y@"));
        assert!(redacted.contains("example.com/a?b=1"));
        assert!(redacted.contains("<redacted>@h/p"));
        assert_eq!(redact_text("plain error"), "plain error");
    }

    #[test]
    fn redact_request_strips_cookie_headers_and_proxy_credentials() {
        let mut request = sample_request("https://user:secret@example.com/a.bin");
        request.cookie = Some("session=topsecret".into());
        request.proxy = Some("http://proxy-user:proxy-pass@127.0.0.1:7890".into());
        request
            .headers
            .insert("authorization".into(), "Bearer topsecret".into());
        request.headers.insert("Cookie".into(), "a=b".into());
        request
            .headers
            .insert("Proxy-Authorization".into(), "Basic topsecret".into());
        request
            .headers
            .insert("Accept-Language".into(), "zh-CN".into());
        let redacted = redact_request(&request);
        let serialized = serde_json::to_string(&redacted).unwrap();
        assert!(!serialized.contains("topsecret"));
        assert!(!serialized.contains("proxy-pass"));
        assert!(!serialized.contains("user:secret"));
        assert!(redacted.cookie.is_none());
        assert_eq!(
            redacted.headers.get("Accept-Language").map(String::as_str),
            Some("zh-CN")
        );
    }

    #[test]
    fn redact_value_walks_nested_structures() {
        let payload = serde_json::json!({
            "url": "https://u:p@example.com/x",
            "headers": {
                "Cookie": "secret=1",
                "Authorization": "Bearer x",
                "Accept": "text/plain"
            },
            "cookie": "secret=2",
            "nested": [{ "proxy": "http://a:b@proxy:8080", "ok": true }]
        });
        let redacted = redact_value(&payload);
        let text = redacted.to_string();
        assert!(!text.contains("secret=1"));
        assert!(!text.contains("secret=2"));
        assert!(!text.contains("Bearer x"));
        assert!(!text.contains("u:p@"));
        assert!(!text.contains("a:b@"));
        assert_eq!(redacted["headers"]["Accept"], "text/plain");
        assert_eq!(redacted["nested"][0]["ok"], true);
        assert_eq!(redacted["cookie"], "<redacted>");
    }

    #[test]
    fn task_view_drops_request_and_redacts_url() {
        let mut request = sample_request("https://user:pass@example.com/a.bin");
        request.cookie = Some("secret".into());
        let record = TaskRecord::new("mcp", request, PathBuf::from("C:\\tmp"), "a.bin");
        let view = record.view();
        let serialized = serde_json::to_string(&view).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("user:pass"));
        assert!(serialized.contains("tsk_"));
        assert!(!serialized.contains("cookie"));
        // TaskView contains the contract §2.4 fields.
        for key in [
            "schema_version",
            "id",
            "state",
            "source",
            "url",
            "file_name",
            "dest_dir",
            "path",
            "total_bytes",
            "downloaded_bytes",
            "connections",
            "chunk_size",
            "range_supported",
            "http_version",
            "speed_bps",
            "eta_secs",
            "warnings",
            "error",
            "result",
            "idempotency_key",
            "created_at",
            "updated_at",
            "finished_at",
        ] {
            assert!(view_has_field(&serialized, key), "TaskView missing {key}");
        }
    }

    fn view_has_field(serialized: &str, key: &str) -> bool {
        serialized.contains(&format!("\"{key}\":"))
    }

    // -- policy -----------------------------------------------------------

    #[test]
    fn policy_defaults_match_contract() {
        let policy = Policy::default();
        assert_eq!(policy.allowed_hosts, vec!["*".to_string()]);
        assert_eq!(policy.allowed_dirs.len(), 1);
        assert!(policy.allowed_dirs[0].ends_with("Downloads"));
        assert_eq!(policy.max_bytes_per_task, Some(10 * 1024 * 1024 * 1024));
        assert_eq!(policy.require_approval_over_bytes, Some(1024 * 1024 * 1024));
        assert_eq!(policy.max_concurrent_tasks, 3);
        assert_eq!(policy.default_connections, rdm_core::DEFAULT_CONNECTIONS);
        assert!(!policy.allow_overwrite);
        // Safe default reversed in contract v1: private hosts are denied
        // unless explicitly enabled by a policy file.
        assert!(!policy.allow_private_hosts);
        let normalized = Policy {
            max_concurrent_tasks: 0,
            default_connections: 0,
            ..Policy::default()
        }
        .normalized();
        assert_eq!(normalized.max_concurrent_tasks, 1);
        assert_eq!(normalized.default_connections, 1);
        assert_eq!(normalized.summary()["schema_version"], SCHEMA_VERSION);
    }

    #[test]
    fn policy_host_globs_are_precise() {
        let mut policy = Policy {
            allowed_hosts: vec!["*.example.com".into()],
            ..Policy::default()
        };
        assert!(policy.check_host("example.com"));
        assert!(policy.check_host("cdn.example.com"));
        assert!(policy.check_host("CDN.EXAMPLE.COM."));
        assert!(!policy.check_host("evil-example.com"));
        assert!(!policy.check_host("example.com.evil.test"));
        assert!(!policy.check_host(""));
        policy.allowed_hosts = vec!["files.example.org".into()];
        assert!(policy.check_host("files.example.org"));
        assert!(!policy.check_host("cdn.files.example.org"));
    }

    #[test]
    fn policy_dest_dir_allows_uncreated_descendants_only_inside_roots() {
        let root = temp_root("policy-dirs");
        let allowed = root.join("allowed");
        let existing_child = allowed.join("existing");
        std::fs::create_dir_all(&existing_child).unwrap();
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let policy = Policy {
            allowed_dirs: vec![allowed.clone()],
            ..Policy::default()
        };
        assert!(policy.check_dest_dir(&allowed));
        assert!(policy.check_dest_dir(&existing_child));
        // Works before the download creates the directory.
        assert!(policy.check_dest_dir(&allowed.join("new").join("deeper")));
        assert!(!policy.check_dest_dir(&outside));
        assert!(!policy.check_dest_dir(&root.join("allowed-sibling")));
        // `..` cannot escape the root.
        assert!(!policy.check_dest_dir(&allowed.join("..").join("outside")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn policy_url_checks_scheme_host_and_private_ranges() {
        let policy = Policy::default();
        assert!(matches!(
            policy.check_url("https://example.com/x"),
            PolicyDecision::Allow
        ));
        assert!(matches!(
            policy.check_url("ftp://example.com/x"),
            PolicyDecision::Deny { .. }
        ));
        assert!(matches!(
            policy.check_url("not a url"),
            PolicyDecision::Deny { .. }
        ));
        let restricted = Policy {
            allowed_hosts: vec!["*.example.com".into()],
            ..Policy::default()
        };
        assert!(matches!(
            restricted.check_url("https://cdn.example.com/x"),
            PolicyDecision::Allow
        ));
        assert!(matches!(
            restricted.check_url("https://evil.test/x"),
            PolicyDecision::Deny { .. }
        ));
        // Safe default: private/loopback/metadata endpoints are rejected even
        // though allowed_hosts is `*` (contract §4).
        assert!(matches!(
            policy.check_url("http://127.0.0.1:8080/file"),
            PolicyDecision::Deny { .. }
        ));
        assert!(matches!(
            policy.check_url("http://localhost:8080/file"),
            PolicyDecision::Deny { .. }
        ));
        assert!(matches!(
            policy.check_url("http://192.168.1.5/file"),
            PolicyDecision::Deny { .. }
        ));
        assert!(matches!(
            policy.check_url("http://169.254.169.254/latest/meta-data"),
            PolicyDecision::Deny { .. }
        ));
        assert!(matches!(
            policy.check_url("http://metadata.google.internal/computeMetadata/v1"),
            PolicyDecision::Deny { .. }
        ));
        assert!(matches!(
            policy.check_url("https://example.com/file"),
            PolicyDecision::Allow
        ));
        // Explicit opt-in: allow_private_hosts=true AND the host must match
        // allowed_hosts.
        let mut opt_in = Policy {
            allow_private_hosts: true,
            ..Policy::default()
        };
        opt_in.allowed_hosts = vec!["127.0.0.1".into()];
        assert!(matches!(
            opt_in.check_url("http://127.0.0.1:8080/file"),
            PolicyDecision::Allow
        ));
        assert!(matches!(
            opt_in.check_url("http://localhost:8080/file"),
            PolicyDecision::Deny { .. }
        ));
        opt_in.allowed_hosts = vec!["*".into()];
        assert!(matches!(
            opt_in.check_url("http://192.168.1.5/file"),
            PolicyDecision::Allow
        ));
        assert!(is_private_host("::1"));
        assert!(is_private_host("10.1.2.3"));
        assert!(is_private_host("169.254.169.254"));
        assert!(is_private_host("fe80::1"));
        assert!(is_private_host("metadata.google.internal"));
        assert!(is_private_host("something.internal"));
        assert!(!is_private_host("example.com"));
        assert!(!is_private_host("8.8.8.8"));
    }

    #[test]
    fn policy_file_loading_is_fail_safe() {
        let root = temp_root("policy-file");
        // Missing file -> defaults.
        assert_eq!(Policy::load_or_default(&root).max_concurrent_tasks, 3);
        // Invalid JSON -> defaults, not "allow everything with 0 limits".
        std::fs::write(root.join("policy.json"), b"{not json").unwrap();
        let loaded = Policy::load_or_default(&root);
        assert_eq!(loaded.max_concurrent_tasks, 3);
        assert_eq!(loaded.allowed_hosts, vec!["*".to_string()]);
        // Valid partial file: missing fields keep their defaults.
        std::fs::write(
            root.join("policy.json"),
            br#"{"allowed_hosts":["127.0.0.1"],"max_concurrent_tasks":1}"#,
        )
        .unwrap();
        let loaded = Policy::load_or_default(&root);
        assert_eq!(loaded.allowed_hosts, vec!["127.0.0.1".to_string()]);
        assert_eq!(loaded.max_concurrent_tasks, 1);
        assert_eq!(loaded.default_connections, rdm_core::DEFAULT_CONNECTIONS);
        assert!(!loaded.allow_overwrite);
        assert!(!loaded.allow_private_hosts);
        // Explicit opt-in is honoured and enables loopback fixtures.
        std::fs::write(
            root.join("policy.json"),
            br#"{"allowed_hosts":["127.0.0.1"],"allow_private_hosts":true}"#,
        )
        .unwrap();
        let loaded = Policy::load_or_default(&root);
        assert!(loaded.allow_private_hosts);
        assert!(matches!(
            loaded.check_url("http://127.0.0.1:9/file"),
            PolicyDecision::Allow
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    // -- task store -------------------------------------------------------

    #[test]
    fn store_rejects_path_traversal_ids() {
        let root = temp_root("store-ids");
        let store = TaskStore::new(&root);
        for bad in ["../escape", "a/b", "a\\b", "", ".", "..", "a b", "a\0b"] {
            assert!(!TaskStore::is_valid_id(bad), "{bad:?} should be rejected");
            assert!(store.get(bad).is_err());
            assert!(store.events(bad, 10).is_err());
        }
        assert!(TaskStore::is_valid_id("tsk_1792345678901_a1b2"));
        assert!(TaskStore::is_valid_id("plan_1_dead"));
        let record = TaskRecord::new(
            "test",
            sample_request("https://example.com/a"),
            root.clone(),
            "a",
        );
        store.create(&record).unwrap();
        assert!(matches!(
            store.get("missing").unwrap_err(),
            AgentStoreError::NotFound(_)
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn store_plans_roundtrip_and_prune() {
        let root = temp_root("store-plans");
        let store = TaskStore::new(&root);
        let request = sample_request("https://example.com/big.iso");
        let mut plan = DownloadPlan::new(
            request,
            Some(42),
            true,
            Some("big".into()),
            None,
            false,
            "mcp",
        );
        // Store a second, expired plan.
        let mut expired = plan.clone();
        expired.plan_id = new_plan_id();
        expired.expires_at = iso8601_in_minutes(-5);
        plan.expires_at = iso8601_in_minutes(10);
        store.save_plan(&plan).unwrap();
        store.save_plan(&expired).unwrap();
        assert_eq!(store.list_plans().unwrap().len(), 2);
        let loaded = store.get_plan(&plan.plan_id).unwrap();
        assert_eq!(loaded.plan_id, plan.plan_id);
        assert_eq!(loaded.total_bytes, Some(42));
        assert!(!loaded.is_expired());
        assert!(store.get_plan(&expired.plan_id).unwrap().is_expired());
        assert_eq!(store.prune_expired_plans().unwrap(), 1);
        assert_eq!(store.list_plans().unwrap().len(), 1);
        store.delete_plan(&plan.plan_id).unwrap();
        assert!(matches!(
            store.get_plan(&plan.plan_id).unwrap_err(),
            AgentStoreError::NotFound(_)
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn store_events_are_defensively_redacted() {
        let root = temp_root("store-events");
        let store = TaskStore::new(&root);
        let record = TaskRecord::new(
            "test",
            sample_request("https://example.com/a"),
            root.clone(),
            "a",
        );
        store.create(&record).unwrap();
        store
            .append_event(&TaskEvent::new(
                &record.id,
                1,
                "progress",
                serde_json::json!({
                    "cookie": "session=topsecret",
                    "headers": { "Authorization": "Bearer topsecret", "X-Trace": "ok" },
                    "downloaded": 10
                }),
            ))
            .unwrap();
        let raw = std::fs::read_to_string(root.join("events").join(format!("{}.jsonl", record.id)))
            .unwrap();
        assert!(!raw.contains("topsecret"));
        assert!(raw.contains("<redacted>"));
        assert!(raw.contains("\"X-Trace\""));
        let events = store.events(&record.id, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["cookie"], "<redacted>");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn store_filters_and_paginates() {
        let root = temp_root("store-filter");
        let store = TaskStore::new(&root);
        let mut ids = Vec::new();
        for index in 0..5 {
            let mut record = TaskRecord::new(
                "test",
                sample_request(&format!("https://example.com/{index}")),
                root.clone(),
                format!("{index}.bin"),
            );
            if index % 2 == 0 {
                record.state = TaskState::Completed;
            }
            ids.push(record.id.clone());
            store.create(&record).unwrap();
        }
        let completed = store
            .list(&TaskFilter {
                states: Some(vec![TaskState::Completed]),
                ..TaskFilter::default()
            })
            .unwrap();
        assert_eq!(completed.len(), 3);
        let first_page = store
            .list(&TaskFilter {
                limit: Some(2),
                ..TaskFilter::default()
            })
            .unwrap();
        assert_eq!(first_page.len(), 2);
        let second_page = store
            .list(&TaskFilter {
                limit: Some(2),
                cursor: Some(first_page[1].id.clone()),
                ..TaskFilter::default()
            })
            .unwrap();
        assert_eq!(second_page.len(), 2);
        assert_ne!(first_page[0].id, second_page[0].id);
        let found = store.find_by_idempotency_key("missing").unwrap();
        assert!(found.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn error_info_maps_engine_errors_and_redacts() {
        let mut request = DownloadRequest::new("https://example.com/a");
        request.cookie = Some("x".into());
        let request = redact_request(&request);
        assert!(request.cookie.is_none());
        let info = AgentErrorInfo::from_download_error(&DownloadError::Status {
            status: 404,
            url: "https://user:pass@example.com/a".to_string(),
        });
        assert_eq!(info.code, ErrorCode::HttpStatus);
        let details = info.details.unwrap();
        assert_eq!(details["status"], 404);
        assert!(!details["url"].as_str().unwrap().contains("user:pass"));
        assert!(!info.retryable);
        let paused = AgentErrorInfo::from_download_error(&DownloadError::Paused { downloaded: 5 });
        assert_eq!(paused.code, ErrorCode::Paused);
        assert!(!paused.retryable);
        let network = AgentErrorInfo::from_download_error(&DownloadError::Http("dns".to_string()));
        assert!(network.retryable);
        assert!(network.hint.is_some());
    }
}
