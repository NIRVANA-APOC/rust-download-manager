//! Machine-facing output layer: the JSON envelope, NDJSON event stream,
//! structured errors and the CLI exit-code contract.
//!
//! Contract: `docs/agent-integration.md` §2 (envelope §2.1, NDJSON §2.2,
//! error codes §2.3). All shared types come from `rdm_agent`; the CLI never
//! invents a parallel taxonomy.

use std::time::Instant;

use rdm_agent::{AgentErrorInfo, Envelope, EnvelopeMeta, ErrorCode, TaskEvent, SCHEMA_VERSION};
use serde::Serialize;
use serde_json::{json, Value};

/// Output format selected by `--output` (or the legacy `--json` alias).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputMode {
    /// Human-readable text (default).
    Text,
    /// One JSON envelope object for the whole invocation.
    Json,
    /// One JSON object per line: streaming events followed by a final
    /// `result` event. `--json` is an alias for this mode.
    Ndjson,
}

/// Exit code used when no error happened.
pub const EXIT_OK: u8 = 0;
/// Exit code for generic failures / invalid requests.
pub const EXIT_FAILED: u8 = 2;
/// Exit code for paused/cancelled control flow.
pub const EXIT_CONTROL_FLOW: u8 = 3;
/// Exit code for policy denials and pending approvals.
pub const EXIT_POLICY: u8 = 4;

/// Map a stable agent error code onto the CLI exit-code contract.
pub fn exit_code_for(code: ErrorCode) -> u8 {
    match code {
        ErrorCode::PolicyDenied | ErrorCode::NeedsApproval => EXIT_POLICY,
        ErrorCode::Paused | ErrorCode::Cancelled => EXIT_CONTROL_FLOW,
        _ => EXIT_FAILED,
    }
}

/// Map an engine error into the shared agent error taxonomy + exit code.
pub fn engine_error(error: &rdm_core::DownloadError) -> AgentErrorInfo {
    AgentErrorInfo::from_download_error(error)
}

/// Build the envelope `meta` block for a command that started at `started`.
pub fn envelope_meta(started: Instant) -> EnvelopeMeta {
    EnvelopeMeta {
        rdm_version: env!("CARGO_PKG_VERSION").to_string(),
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        ts: rdm_agent::now_iso8601(),
        trace_id: None,
    }
}

/// Per-invocation output context shared by every subcommand.
///
/// * `Text` — commands print their own human output; the reporter is silent.
/// * `Json` — exactly one [`Envelope`] is printed by `finish_*`.
/// * `Ndjson` — events while working, then one final `result` event.
pub struct Reporter {
    pub mode: OutputMode,
    pub command: String,
    pub task_id: Option<String>,
    pub quiet: bool,
    started: Instant,
    seq: u64,
}

impl Reporter {
    pub fn new(mode: OutputMode, command: impl Into<String>) -> Self {
        Self {
            mode,
            command: command.into(),
            task_id: None,
            quiet: false,
            started: Instant::now(),
            seq: 0,
        }
    }

    pub fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    pub fn set_task_id(&mut self, task_id: impl Into<String>) {
        self.task_id = Some(task_id.into());
    }

    pub fn is_text(&self) -> bool {
        self.mode == OutputMode::Text
    }

    pub fn is_ndjson(&self) -> bool {
        self.mode == OutputMode::Ndjson
    }

    /// Emit one NDJSON event (no-op in text/json mode). Progress events are
    /// suppressed by `--quiet`.
    pub fn event(&mut self, kind: &str, data: Value) {
        let task_id = self.task_id.clone().unwrap_or_default();
        self.event_for(&task_id, kind, data);
    }

    /// Emit one NDJSON event for an explicit task id (used by `rdm run` where
    /// several tasks stream into one process).
    pub fn event_for(&mut self, task_id: &str, kind: &str, data: Value) {
        if self.mode != OutputMode::Ndjson {
            return;
        }
        if self.quiet && kind == "progress" {
            return;
        }
        self.seq += 1;
        let event = TaskEvent::new(task_id, self.seq, kind, data);
        print_json_line(&event);
    }

    /// Emit a progress event without allocating a `TaskEvent` twice; keeps the
    /// contract shape (`schema_version/type/task_id/seq/ts/data`).
    pub fn progress(
        &mut self,
        downloaded: u64,
        total: Option<u64>,
        speed_bps: f64,
        eta_secs: Option<f64>,
        connections: usize,
    ) {
        if self.mode != OutputMode::Ndjson || self.quiet {
            return;
        }
        self.event(
            "progress",
            json!({
                "downloaded": downloaded,
                "total": total,
                "speed_bps": speed_bps,
                "eta_secs": eta_secs,
                "connections": connections,
            }),
        );
    }

    fn envelope(&self, ok: bool, data: Option<Value>, error: Option<AgentErrorInfo>) -> Envelope {
        Envelope {
            schema_version: SCHEMA_VERSION.to_string(),
            ok,
            command: self.command.clone(),
            task_id: self.task_id.clone(),
            data,
            error,
            meta: envelope_meta(self.started),
        }
    }

    /// Final success output. Returns the process exit code.
    pub fn finish_ok(&mut self, data: impl Serialize) -> u8 {
        let data = serde_json::to_value(data).unwrap_or(Value::Null);
        match self.mode {
            OutputMode::Text => {}
            OutputMode::Json => print_json_line(&self.envelope(true, Some(data), None)),
            OutputMode::Ndjson => {
                self.seq += 1;
                let line = json!({
                    "schema_version": SCHEMA_VERSION,
                    "type": "result",
                    "ok": true,
                    "command": self.command,
                    "task_id": self.task_id,
                    "seq": self.seq,
                    "ts": rdm_agent::now_iso8601(),
                    "data": data,
                    "error": Value::Null,
                    "meta": envelope_meta(self.started),
                });
                print_json_line(&line);
            }
        }
        EXIT_OK
    }

    /// Final report output for diagnostic commands (`doctor`, `selftest`,
    /// `bench`, `run`).
    ///
    /// Unlike `finish_err` this keeps `data` populated on failure — the whole
    /// point of a report is to carry the failing checks back to the agent. The
    /// envelope's `ok` bit mirrors the process exit code.
    pub fn finish_report(&mut self, data: impl Serialize, error: Option<AgentErrorInfo>) -> u8 {
        let data = serde_json::to_value(data).unwrap_or(Value::Null);
        let code = error
            .as_ref()
            .map(|error| exit_code_for(error.code))
            .unwrap_or(EXIT_OK);
        match self.mode {
            OutputMode::Text => {
                if let Some(error) = &error {
                    crate::errln!("error [{}]: {}", error.code.as_str(), error.message);
                    if let Some(hint) = &error.hint {
                        crate::errln!("hint: {hint}");
                    }
                }
            }
            OutputMode::Json => {
                let envelope = self.envelope(error.is_none(), Some(data), error);
                print_json_line(&envelope);
            }
            OutputMode::Ndjson => {
                if let Some(error) = &error {
                    self.seq += 1;
                    let failed = json!({
                        "schema_version": SCHEMA_VERSION,
                        "type": "failed",
                        "ok": false,
                        "command": self.command,
                        "task_id": self.task_id,
                        "seq": self.seq,
                        "ts": rdm_agent::now_iso8601(),
                        "data": { "error": error },
                    });
                    print_json_line(&failed);
                }
                self.seq += 1;
                let line = json!({
                    "schema_version": SCHEMA_VERSION,
                    "type": "result",
                    "ok": error.is_none(),
                    "command": self.command,
                    "task_id": self.task_id,
                    "seq": self.seq,
                    "ts": rdm_agent::now_iso8601(),
                    "data": data,
                    "error": error,
                    "meta": envelope_meta(self.started),
                });
                print_json_line(&line);
            }
        }
        code
    }

    /// Final failure output. Returns the process exit code (2/3/4).
    pub fn finish_err(&mut self, error: AgentErrorInfo) -> u8 {
        let code = exit_code_for(error.code);
        match self.mode {
            OutputMode::Text => {
                crate::errln!("error [{}]: {}", error.code.as_str(), error.message);
                if let Some(hint) = &error.hint {
                    crate::errln!("hint: {hint}");
                }
                if let Some(details) = &error.details {
                    crate::errln!("details: {details}");
                }
            }
            OutputMode::Json => print_json_line(&self.envelope(false, None, Some(error))),
            OutputMode::Ndjson => {
                self.seq += 1;
                let error_value = serde_json::to_value(&error).unwrap_or(Value::Null);
                let failed = json!({
                    "schema_version": SCHEMA_VERSION,
                    "type": "failed",
                    "ok": false,
                    "command": self.command,
                    "task_id": self.task_id,
                    "seq": self.seq,
                    "ts": rdm_agent::now_iso8601(),
                    "data": { "error": error_value },
                });
                print_json_line(&failed);
                self.seq += 1;
                let result = json!({
                    "schema_version": SCHEMA_VERSION,
                    "type": "result",
                    "ok": false,
                    "command": self.command,
                    "task_id": self.task_id,
                    "seq": self.seq,
                    "ts": rdm_agent::now_iso8601(),
                    "data": Value::Null,
                    "error": error,
                    "meta": envelope_meta(self.started),
                });
                print_json_line(&result);
            }
        }
        code
    }
}

fn print_json_line(value: &impl Serialize) {
    match serde_json::to_string(value) {
        Ok(line) => crate::outln!("{line}"),
        Err(error) => {
            // Never fail silently: fall back to a minimal JSON error object.
            crate::outln!(
                "{{\"schema_version\":\"{SCHEMA_VERSION}\",\"ok\":false,\"error\":{{\"code\":\"internal\",\"message\":{}}}}}",
                serde_json::to_string(&format!("could not serialize output: {error}"))
                    .unwrap_or_else(|_| "\"could not serialize output\"".to_string()),
            );
        }
    }
}

/// Helper used by commands to produce a structured invalid-request error.
pub fn invalid_request(message: impl Into<String>) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::InvalidRequest, message)
        .with_hint("run `rdm <command> --help` or `rdm schema --output json`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_contract() {
        assert_eq!(exit_code_for(ErrorCode::InvalidUrl), EXIT_FAILED);
        assert_eq!(exit_code_for(ErrorCode::NetworkError), EXIT_FAILED);
        assert_eq!(exit_code_for(ErrorCode::Paused), EXIT_CONTROL_FLOW);
        assert_eq!(exit_code_for(ErrorCode::Cancelled), EXIT_CONTROL_FLOW);
        assert_eq!(exit_code_for(ErrorCode::PolicyDenied), EXIT_POLICY);
        assert_eq!(exit_code_for(ErrorCode::NeedsApproval), EXIT_POLICY);
    }

    #[test]
    fn envelope_carries_schema_version_and_command() {
        let mut reporter = Reporter::new(OutputMode::Json, "unit-test");
        reporter.set_task_id("tsk_1");
        let envelope = reporter.envelope(true, Some(json!({"a": 1})), None);
        assert_eq!(envelope.schema_version, SCHEMA_VERSION);
        assert_eq!(envelope.command, "unit-test");
        assert_eq!(envelope.task_id.as_deref(), Some("tsk_1"));
        assert!(envelope.ok);
        assert_eq!(envelope.meta.rdm_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn engine_errors_map_to_stable_codes() {
        let paused = rdm_core::DownloadError::Paused { downloaded: 5 };
        assert_eq!(engine_error(&paused).code, ErrorCode::Paused);
        let invalid = rdm_core::DownloadError::InvalidUrl("x".into());
        assert_eq!(engine_error(&invalid).code, ErrorCode::InvalidUrl);
        let checksum = rdm_core::DownloadError::ChecksumMismatch {
            expected: "a".into(),
            actual: "b".into(),
        };
        assert_eq!(engine_error(&checksum).code, ErrorCode::ChecksumMismatch);
    }
}
