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

// ---------------------------------------------------------------------------
// Timestamps / ids
// ---------------------------------------------------------------------------

/// Current UTC time as ISO-8601 (`2026-10-19T03:20:00.123Z`).
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();

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
            E::Status { status, .. } => {
                let code = if *status == 429 || *status == 503 {
                    ErrorCode::HttpStatus
                } else {
                    ErrorCode::HttpStatus
                };
                (code, Some("4xx 通常不可重试；429/5xx 可退避后重试"))
            }
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
        let mut info = Self::new(code, error.to_string());
        if let Some(hint) = hint {
            info = info.with_hint(hint);
        }
        if let E::Status { status, url } = error {
            info.details = Some(serde_json::json!({ "status": status, "url": url }));
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

/// Returns a copy of `request` with credentials stripped (for output/logs).
pub fn redact_request(request: &DownloadRequest) -> DownloadRequest {
    let mut out = request.clone();
    out.cookie = None;
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
            url: record.request.url.clone(),
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
    pub fn default_root() -> PathBuf {
        rdm_ipc::state_dir().join("agent")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_layout(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.root.join("tasks"))?;
        std::fs::create_dir_all(self.root.join("events"))?;
        std::fs::create_dir_all(self.root.join("plans"))?;
        Ok(())
    }

    pub fn create(&self, record: &TaskRecord) -> Result<(), AgentStoreError> {
        self.ensure_layout()?;
        self.write_record(record)
    }

    pub fn update(&self, record: &TaskRecord) -> Result<(), AgentStoreError> {
        self.ensure_layout()?;
        self.write_record(record)
    }

    pub fn get(&self, id: &str) -> Result<TaskRecord, AgentStoreError> {
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
        self.ensure_layout()?;
        use std::io::Write;
        let path = self.root.join("events").join(format!("{}.jsonl", event.task_id));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let line = serde_json::to_string(event).map_err(AgentStoreError::Serde)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn events(&self, id: &str, limit: usize) -> Result<Vec<TaskEvent>, AgentStoreError> {
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
        if events.len() > limit {
            events = events.split_off(events.len() - limit);
        }
        Ok(events)
    }

    pub fn find_by_idempotency_key(&self, key: &str) -> Result<Option<TaskRecord>, AgentStoreError> {
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
    #[serde(default = "default_true")]
    pub allow_private_hosts: bool,
    #[serde(default)]
    pub allow_overwrite: bool,
}

fn default_allowed_hosts() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_allowed_dirs() -> Vec<PathBuf> {
    let dir = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Downloads");
    vec![dir]
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
fn default_true() -> bool {
    true
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
            allow_private_hosts: true,
            allow_overwrite: false,
        }
    }
}

/// Outcome of a policy pre-check after probing remote metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    NeedsApproval { reason: String, total_bytes: Option<u64> },
    Deny { reason: String },
}

impl Policy {
    /// Load `policy.json` from the given store root, or fall back to defaults.
    pub fn load_or_default(agent_root: &Path) -> Self {
        let path = agent_root.join("policy.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn check_host(&self, host: &str) -> bool {
        self.allowed_hosts.iter().any(|pattern| {
            pattern == "*"
                || pattern.eq_ignore_ascii_case(host)
                || pattern
                    .strip_prefix("*.")
                    .is_some_and(|suffix| host.to_ascii_lowercase().ends_with(&format!(".{}", suffix.to_ascii_lowercase())))
        })
    }

    pub fn check_dest_dir(&self, dir: &Path) -> bool {
        let Ok(canonical) = std::fs::canonicalize(dir) else {
            return false;
        };
        self.allowed_dirs.iter().any(|allowed| {
            std::fs::canonicalize(allowed)
                .map(|allowed| canonical.starts_with(allowed))
                .unwrap_or(false)
        })
    }

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
                    reason: format!("remote file is {total} bytes, approval threshold is {threshold}"),
                    total_bytes,
                };
            }
        }
        PolicyDecision::Allow
    }
}

// ---------------------------------------------------------------------------
// Tool definitions (MCP tools/list + CLI `rdm schema`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAnnotations {
    #[serde(rename = "readOnlyHint")]
    pub read_only_hint: bool,
    #[serde(rename = "destructiveHint")]
    pub destructive_hint: bool,
    #[serde(rename = "idempotentHint")]
    pub idempotent_hint: bool,
    #[serde(rename = "openWorldHint")]
    pub open_world_hint: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub title: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema")]
    pub output_schema: Value,
    pub annotations: ToolAnnotations,
}

/// Full tool catalog. Implemented by the agent-interface team; this is the
/// single source consumed by MCP `tools/list` and CLI `rdm schema`.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    // Skeleton: populated with the 16 tools in docs/agent-integration.md §3.1.
    Vec::new()
}

/// `rdm capabilities` payload (shared by CLI and MCP `rdm_capabilities`).
pub fn capabilities() -> Value {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "rdm_version": env!("CARGO_PKG_VERSION"),
        "tools": tool_definitions().iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
        "limits": {
            "max_connections": rdm_core::MAX_CONNECTIONS,
            "default_connections": rdm_core::DEFAULT_CONNECTIONS,
            "max_concurrent_tasks": Policy::default().max_concurrent_tasks,
        },
        "transports": ["mcp-stdio", "cli-json"],
        "features": [
            "range-probe", "segmented-download", "work-stealing", "resume",
            "rate-limit", "sha256-verify", "policy", "approval-token",
            "idempotency", "durable-tasks", "progress-notifications",
            "doctor", "selftest", "bench", "state-maintenance"
        ]
    })
}

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
        request.headers.insert("Authorization".into(), "Bearer x".into());
        request.headers.insert("X-Trace".into(), "ok".into());
        let redacted = redact_request(&request);
        assert!(redacted.cookie.is_none());
        assert!(!redacted.headers.keys().any(|k| k.eq_ignore_ascii_case("authorization")));
        assert_eq!(redacted.headers.get("X-Trace").map(String::as_str), Some("ok"));
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
        let mut restricted = Policy::default();
        restricted.allowed_hosts = vec!["*.example.com".into()];
        assert!(restricted.check_host("cdn.example.com"));
        assert!(!restricted.check_host("evil.test"));
        let mut tiny = Policy::default();
        tiny.require_approval_over_bytes = Some(10);
        tiny.max_bytes_per_task = Some(100);
        assert!(matches!(tiny.gate(Some(11)), PolicyDecision::NeedsApproval { .. }));
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
            .append_event(&TaskEvent::new(&record.id, 1, "created", serde_json::json!({})))
            .unwrap();
        assert_eq!(store.events(&record.id, 10).unwrap().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
