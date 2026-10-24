//! Durable task tracking for CLI downloads: writes the shared
//! `rdm_agent::TaskRecord` into the per-task [`TaskStore`] and appends a
//! redacted event journal, so `rdm tasks` / `rdm task <id>` can inspect any
//! download started from the CLI.
//!
//! Store failures are never fatal: a download must keep working even when the
//! agent state directory is read-only. In that case the tracker degrades to
//! in-memory only and surfaces a warning.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdm_agent::{
    AgentErrorInfo, TaskEvent, TaskRecord, TaskResult, TaskState, TaskStore, TaskView,
};
use rdm_core::{DownloadSummary, RemoteMeta};
use serde_json::{json, Value};

const PROGRESS_PERSIST_INTERVAL: Duration = Duration::from_millis(500);
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(1000);

pub struct TaskTracker {
    store: Option<TaskStore>,
    pub record: TaskRecord,
    seq: u64,
    last_persist: Instant,
    last_progress_persist: Instant,
    last_progress_event: Instant,
    warning: Option<String>,
}

impl TaskTracker {
    /// Register a new task. Failure to write the store degrades gracefully.
    pub fn begin(
        store_root: &Path,
        source: &str,
        request: rdm_core::DownloadRequest,
        dest_dir: PathBuf,
        file_name: impl Into<String>,
        idempotency_key: Option<String>,
    ) -> Self {
        let mut record = TaskRecord::new(source, request, dest_dir, file_name);
        record.idempotency_key = idempotency_key;
        let mut tracker = Self {
            store: Some(TaskStore::new(store_root)),
            record,
            seq: 0,
            last_persist: Instant::now(),
            last_progress_persist: Instant::now(),
            last_progress_event: Instant::now(),
            warning: None,
        };
        tracker.event("created", json!({"source": source}));
        tracker.persist(true);
        tracker
    }

    /// Wrap an existing record (idempotency replay).
    pub fn adopt(store_root: &Path, record: TaskRecord) -> Self {
        Self {
            store: Some(TaskStore::new(store_root)),
            record,
            seq: 0,
            last_persist: Instant::now(),
            last_progress_persist: Instant::now(),
            last_progress_event: Instant::now(),
            warning: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    pub fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }

    /// Store-backed records with credentials stripped, ready for output.
    pub fn redacted(&self) -> TaskRecord {
        self.record.redacted()
    }

    /// Contract §2.4 agent-visible shape (drops the raw request entirely).
    pub fn view(&self) -> TaskView {
        self.redacted().view()
    }

    pub fn set_state(&mut self, state: TaskState) {
        if self.record.state != state {
            self.record.state = state;
            self.record.updated_at = rdm_agent::now_iso8601();
        }
        self.persist(true);
    }

    pub fn apply_metadata(&mut self, meta: &RemoteMeta) {
        self.record.total_bytes = meta.total_bytes;
        self.record.range_supported = Some(meta.supports_ranges);
        self.record.http_version = meta.http_version.clone();
        if self.record.request.file_name.is_none() {
            self.record.file_name = meta.file_name.clone();
        }
        if !meta.supports_ranges {
            self.push_warning("range_unsupported");
        }
        if meta.total_bytes.is_none() {
            self.push_warning("no_content_length");
        }
        self.event(
            "metadata",
            json!({
                "final_url": meta.final_url,
                "total_bytes": meta.total_bytes,
                "supports_ranges": meta.supports_ranges,
                "http_version": meta.http_version,
            }),
        );
        self.persist(true);
    }

    pub fn apply_started(&mut self, connections: usize, chunk_size: u64) {
        self.record.connections = connections;
        self.record.chunk_size = Some(chunk_size);
        self.set_state(TaskState::Downloading);
    }

    /// Track the observed dynamic-plan peak. `target` is informational (the
    /// `connections` contract keeps its `min(chunks, requested)` semantics).
    pub fn apply_concurrency(&mut self, target: usize, active: usize) {
        self.record.peak_connections = self
            .record
            .peak_connections
            .max(active.min(u32::MAX as usize) as u32);
        let _ = target;
    }

    pub fn apply_progress(
        &mut self,
        downloaded: u64,
        total: Option<u64>,
        speed_bps: f64,
        eta_secs: Option<f64>,
    ) {
        self.record.downloaded_bytes = downloaded;
        if total.is_some() {
            self.record.total_bytes = total;
        }
        self.record.speed_bps = speed_bps;
        self.record.eta_secs = eta_secs;
        self.record.updated_at = rdm_agent::now_iso8601();
        if self.last_progress_persist.elapsed() >= PROGRESS_PERSIST_INTERVAL {
            self.last_progress_persist = Instant::now();
            self.persist(false);
        }
        if self.last_progress_event.elapsed() >= PROGRESS_EVENT_INTERVAL {
            self.last_progress_event = Instant::now();
            self.event(
                "progress",
                json!({
                    "downloaded": self.record.downloaded_bytes,
                    "total": self.record.total_bytes,
                    "speed_bps": speed_bps,
                }),
            );
        }
    }

    pub fn apply_completed(&mut self, summary: &DownloadSummary) {
        self.record.state = TaskState::Completed;
        self.record.path = Some(summary.path.clone());
        self.record.total_bytes = Some(summary.total_bytes);
        self.record.downloaded_bytes = summary.total_bytes;
        self.record.speed_bps = summary.avg_speed_bps;
        self.record.eta_secs = Some(0.0);
        self.record.http_version = summary
            .http_version
            .clone()
            .or_else(|| self.record.http_version.clone());
        if !summary.range_supported {
            self.push_warning("range_unsupported");
            self.record.range_supported = Some(false);
        }
        if let Some(name) = summary.path.file_name().and_then(|name| name.to_str()) {
            self.record.file_name = name.to_string();
        }
        self.record.peak_connections = self.record.peak_connections.max(summary.peak_connections);
        self.record.result = Some(TaskResult {
            path: summary.path.display().to_string(),
            size: summary.total_bytes,
            sha256: summary.sha256.clone(),
            elapsed_ms: summary.elapsed_ms,
            avg_speed_bps: summary.avg_speed_bps,
            peak_connections: summary.peak_connections,
        });
        self.record.finished_at = Some(rdm_agent::now_iso8601());
        self.record.updated_at = rdm_agent::now_iso8601();
        self.event(
            "completed",
            json!({
                "path": summary.path.display().to_string(),
                "size": summary.total_bytes,
                "sha256": summary.sha256,
                "elapsed_ms": summary.elapsed_ms,
                "connections": summary.connections,
                "peak_connections": summary.peak_connections,
            }),
        );
        self.persist(true);
    }

    pub fn apply_failed(&mut self, error: &AgentErrorInfo) {
        self.record.state = TaskState::Failed;
        self.record.error = Some(error.clone());
        self.record.speed_bps = 0.0;
        self.record.finished_at = Some(rdm_agent::now_iso8601());
        self.record.updated_at = rdm_agent::now_iso8601();
        self.event(
            "failed",
            json!({ "error": serde_json::to_value(error).unwrap_or(Value::Null) }),
        );
        self.persist(true);
    }

    pub fn apply_paused(&mut self, downloaded: u64) {
        self.record.state = TaskState::Paused;
        self.record.downloaded_bytes = downloaded;
        self.record.speed_bps = 0.0;
        self.record.updated_at = rdm_agent::now_iso8601();
        self.event("paused", json!({ "downloaded": downloaded }));
        self.persist(true);
    }

    pub fn apply_cancelled(&mut self, downloaded: u64) {
        self.record.state = TaskState::Cancelled;
        self.record.downloaded_bytes = downloaded;
        self.record.speed_bps = 0.0;
        self.record.finished_at = Some(rdm_agent::now_iso8601());
        self.record.updated_at = rdm_agent::now_iso8601();
        self.event("cancelled", json!({ "downloaded": downloaded }));
        self.persist(true);
    }

    pub fn push_warning(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if !self
            .record
            .warnings
            .iter()
            .any(|existing| existing == &warning)
        {
            self.record.warnings.push(warning);
        }
    }

    fn event(&mut self, kind: &str, data: Value) {
        let Some(store) = &self.store else { return };
        self.seq += 1;
        let event = TaskEvent::new(self.record.id.clone(), self.seq, kind, data);
        if let Err(error) = store.append_event(&event) {
            self.degrade(format!("event journal unavailable: {error}"));
        }
    }

    /// Persist the record. `force` also updates the timestamp; non-forced
    /// calls are cheap enough for the throttled progress path.
    pub fn persist(&mut self, force: bool) {
        let Some(store) = &self.store else { return };
        if !force && self.last_persist.elapsed() < PROGRESS_PERSIST_INTERVAL {
            return;
        }
        self.last_persist = Instant::now();
        if let Err(error) = store.update(&self.record) {
            self.degrade(format!("task store unavailable: {error}"));
        }
    }

    fn degrade(&mut self, warning: String) {
        self.store = None;
        if self.warning.is_none() {
            self.warning = Some(warning);
        }
    }
}

// Keep the unused import warning away while documenting the shared types.
#[cfg(test)]
use crate::util;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_a_download_lifecycle() {
        let root = util::unique_temp_dir("rdm-cli-tracker-test");
        std::fs::create_dir_all(&root).unwrap();
        let req = rdm_core::DownloadRequest::new("http://127.0.0.1:1/file");
        let tracker = TaskTracker::begin(&root, "cli", req, root.join("out"), "file", None);
        let id = tracker.id().to_string();
        assert!(id.starts_with("tsk_"));

        let store = TaskStore::new(&root);
        let loaded = store.get(&id).unwrap();
        assert_eq!(loaded.state, TaskState::Queued);
        assert_eq!(loaded.idempotency_key, None);

        let mut tracker = TaskTracker::adopt(&root, loaded);
        tracker.apply_metadata(&RemoteMeta {
            final_url: "http://127.0.0.1:1/file".to_string(),
            file_name: "file".to_string(),
            total_bytes: Some(1024),
            supports_ranges: true,
            content_type: None,
            etag: None,
            last_modified: None,
            http_version: Some("HTTP/1.1".to_string()),
        });
        assert!(tracker.record.warnings.is_empty());
        tracker.apply_started(4, 256);
        assert_eq!(tracker.record.state, TaskState::Downloading);
        tracker.apply_progress(512, Some(1024), 1.0, Some(0.5));
        assert_eq!(tracker.record.downloaded_bytes, 512);
        tracker.apply_concurrency(4, 3);
        assert_eq!(tracker.record.peak_connections, 3);

        let summary = rdm_core::DownloadSummary {
            path: root.join("out").join("file"),
            total_bytes: 1024,
            elapsed_ms: 10,
            avg_speed_bps: 1024.0,
            connections: 4,
            peak_connections: 4,
            sha256: None,
            resumed_chunks: 0,
            range_supported: true,
            http_version: Some("HTTP/1.1".to_string()),
            controller: None,
        };
        tracker.apply_completed(&summary);
        assert_eq!(tracker.record.state, TaskState::Completed);
        assert_eq!(tracker.record.result.as_ref().unwrap().size, 1024);
        assert_eq!(tracker.record.peak_connections, 4);
        assert_eq!(tracker.record.result.as_ref().unwrap().peak_connections, 4);
        // Agent-visible view exposes the peak and the dynamic flag.
        let view = serde_json::to_value(tracker.view()).unwrap();
        assert_eq!(view["peak_connections"], 4);
        assert_eq!(view["result"]["peak_connections"], 4);
        assert_eq!(view["dynamic_connections"], true);
        // Journal contains the lifecycle events.
        let events = store.events(&id, 50).unwrap();
        let kinds: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();
        assert!(kinds.contains(&"created"));
        assert!(kinds.contains(&"completed"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn redacts_credentials_in_output() {
        let root = util::unique_temp_dir("rdm-cli-tracker-redact");
        let mut req = rdm_core::DownloadRequest::new("http://127.0.0.1:1/file");
        req.cookie = Some("session=supersecret".to_string());
        req.headers
            .insert("Authorization".to_string(), "Bearer topsecret".to_string());
        let tracker = TaskTracker::begin(&root, "cli", req, root.join("out"), "file", None);
        let visible = tracker.redacted();
        let json = serde_json::to_string(&visible).unwrap();
        assert!(!json.contains("supersecret"));
        assert!(!json.contains("topsecret"));
        assert!(visible.request.cookie.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
