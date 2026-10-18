//! Durable task execution on top of the `rdm-core` engine.
//!
//! One [`TaskManager`] per MCP server process:
//! * persists every state transition through [`TaskStore`] (per-task files),
//! * enforces the policy concurrency limit with a semaphore,
//! * maps engine control flow (`Paused`/`Cancelled`) to task states instead of
//!   failures,
//! * monitors unknown-length downloads against `policy.max_bytes_per_task`,
//! * supports cross-process waits by polling the store (the CLI or another MCP
//!   server may own the task).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rdm_agent::{
    now_iso8601, AgentErrorInfo, AgentStoreError, DownloadPlan, ErrorCode, Policy, TaskEvent,
    TaskRecord, TaskState, TaskStore,
};
use rdm_core::{DownloadError, DownloadEvent, DownloadHandle, DownloadRequest, DownloadSummary};
use rdm_fixture_server::TestServer;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::time::sleep;

use crate::util;

/// Callback used to emit MCP `notifications/message` events.
pub type NotifyFn = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// Wait-poll interval. The contract asks for progress every 250-500 ms.
const WAIT_POLL: Duration = Duration::from_millis(250);
/// Control confirmation poll (pause/cancel) must be much faster than waits so
/// a short transfer cannot finish before the engine acknowledges the request.
const CONTROL_POLL: Duration = Duration::from_millis(50);
const QUEUE_POLL: Duration = Duration::from_millis(100);
const PERSIST_INTERVAL: Duration = Duration::from_millis(200);
const EVENT_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const PAUSE_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<Inner>,
}

struct Inner {
    store: TaskStore,
    runtime: tokio::runtime::Handle,
    permits: Arc<Semaphore>,
    policy: Policy,
    entries: Mutex<HashMap<String, Arc<TaskEntry>>>,
    notifier: Mutex<Option<NotifyFn>>,
}

struct TaskEntry {
    download: DownloadHandle,
    control: Mutex<Control>,
    running: AtomicBool,
    /// Set when a size monitor aborted the transfer (policy violation).
    overflow: AtomicBool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Control {
    pause: bool,
    cancel: bool,
}

impl TaskEntry {
    fn new() -> Self {
        Self {
            download: DownloadHandle::new(),
            control: Mutex::new(Control::default()),
            running: AtomicBool::new(true),
            overflow: AtomicBool::new(false),
        }
    }

    fn request_pause(&self) {
        self.control.lock().expect("control poisoned").pause = true;
        self.download.pause();
    }

    fn request_cancel(&self) {
        self.control.lock().expect("control poisoned").cancel = true;
        self.download.cancel();
    }

    fn pause_requested(&self) -> bool {
        self.control.lock().expect("control poisoned").pause
    }

    fn cancel_requested(&self) -> bool {
        self.control.lock().expect("control poisoned").cancel
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

fn store_error(error: AgentStoreError) -> AgentErrorInfo {
    match error {
        AgentStoreError::NotFound(id) => util::not_found(format!("task not found: {id}")),
        AgentStoreError::InvalidId(id) => util::invalid_request(format!("invalid id: {id}")),
        AgentStoreError::Io(error) => util::internal(format!("state store i/o error: {error}")),
        AgentStoreError::Serde(error) => util::internal(format!("task record is corrupt: {error}")),
    }
}

impl TaskManager {
    pub fn new(store: TaskStore, runtime: tokio::runtime::Handle, policy: Policy) -> Self {
        let permits = Arc::new(Semaphore::new(policy.max_concurrent_tasks.max(1)));
        Self {
            inner: Arc::new(Inner {
                store,
                runtime,
                permits,
                policy: policy.normalized(),
                entries: Mutex::new(HashMap::new()),
                notifier: Mutex::new(None),
            }),
        }
    }

    pub fn store(&self) -> &TaskStore {
        &self.inner.store
    }

    pub fn policy(&self) -> &Policy {
        &self.inner.policy
    }

    pub fn set_notifier(&self, notifier: NotifyFn) {
        *self.inner.notifier.lock().expect("notifier poisoned") = Some(notifier);
    }

    fn notify_state(&self, record: &TaskRecord) {
        let notifier = self
            .inner
            .notifier
            .lock()
            .expect("notifier poisoned")
            .clone();
        if let Some(notifier) = notifier {
            let level = match record.state {
                TaskState::Failed => "error",
                TaskState::Paused | TaskState::Cancelled => "warning",
                _ => "info",
            };
            notifier(
                "notifications/message",
                json!({
                    "level": level,
                    "logger": "rdm-mcp",
                    "data": format!(
                        "task {} -> {} ({} / {})",
                        record.id,
                        record.state.as_str(),
                        util::format_size(record.downloaded_bytes),
                        record
                            .total_bytes
                            .map(util::format_size)
                            .unwrap_or_else(|| "?".to_string()),
                    )
                }),
            );
        }
    }

    // -- lookups ---------------------------------------------------------

    pub fn get(&self, id: &str) -> Result<TaskRecord, AgentErrorInfo> {
        self.inner.store.get(id).map_err(store_error)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.inner.store.get(id).is_ok()
    }

    pub fn find_by_idempotency(&self, key: &str) -> Result<Option<TaskRecord>, AgentErrorInfo> {
        self.inner
            .store
            .find_by_idempotency_key(key)
            .map_err(store_error)
    }

    pub fn save_plan(&self, plan: &DownloadPlan) -> Result<(), AgentErrorInfo> {
        self.inner.store.save_plan(plan).map_err(store_error)
    }

    pub fn take_plan(&self, plan_id: &str) -> Result<DownloadPlan, AgentErrorInfo> {
        self.inner.store.get_plan(plan_id).map_err(store_error)
    }

    pub fn delete_plan(&self, plan_id: &str) {
        let _ = self.inner.store.delete_plan(plan_id);
    }

    pub fn active_entries(&self) -> Vec<(String, bool)> {
        self.inner
            .entries
            .lock()
            .expect("entries poisoned")
            .iter()
            .map(|(id, entry)| (id.clone(), entry.is_running()))
            .collect()
    }

    // -- creation --------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn create_task(
        &self,
        source: &str,
        request: DownloadRequest,
        dest_dir: PathBuf,
        file_name: String,
        total_bytes: Option<u64>,
        range_supported: Option<bool>,
        http_version: Option<String>,
        idempotency_key: Option<String>,
        warnings: Vec<String>,
    ) -> Result<TaskRecord, AgentErrorInfo> {
        if let Some(key) = idempotency_key.as_deref() {
            if let Some(existing) = self.find_by_idempotency(key)? {
                return Ok(existing);
            }
        }
        let mut request = request;
        request.output_dir = Some(dest_dir.clone());
        request.file_name = Some(file_name.clone());
        let mut record = TaskRecord::new(source, request, dest_dir, file_name);
        record.total_bytes = total_bytes;
        record.range_supported = range_supported;
        record.http_version = http_version;
        record.idempotency_key = idempotency_key;
        record.warnings = warnings;
        record.connections = record.request.connections.max(1);
        record.chunk_size = record.request.chunk_size;
        let entry = Arc::new(TaskEntry::new());
        self.inner.store.create(&record).map_err(store_error)?;
        self.append_event(
            &record,
            1,
            "created",
            json!({
                "url": rdm_agent::redact_url(&record.request.url),
                "dest_dir": record.dest_dir.display().to_string(),
                "file_name": record.file_name,
                "connections": record.connections,
            }),
        );
        self.inner
            .entries
            .lock()
            .expect("entries poisoned")
            .insert(record.id.clone(), Arc::clone(&entry));
        self.spawn_worker(record.clone(), entry);
        self.notify_state(&record);
        Ok(record)
    }

    /// Resume a paused/cancelled (or retryable failed) task from its `.part`.
    pub fn resume(&self, id: &str) -> Result<TaskRecord, AgentErrorInfo> {
        let mut record = self.get(id)?;
        match record.state {
            TaskState::Completed => return Ok(record),
            TaskState::Queued => {
                if let Some(entry) = self.inner.entries.lock().expect("entries poisoned").get(id) {
                    entry.control.lock().expect("control poisoned").pause = false;
                }
                return Ok(record);
            }
            TaskState::Probing | TaskState::Downloading => {
                // Already running: idempotent no-op.
                return Ok(record);
            }
            TaskState::Paused | TaskState::Cancelled => {}
            TaskState::Failed => {
                let retryable = record
                    .error
                    .as_ref()
                    .map(|error| error.retryable)
                    .unwrap_or(false);
                if !retryable {
                    return Err(util::conflict(format!(
                        "task {} failed with a non-retryable error; start a new download instead",
                        record.id
                    ))
                    .with_details(json!({ "task_id": record.id, "error": record.error })));
                }
            }
        }

        // Only one worker may own a task id at a time.
        let entry = {
            let mut entries = self.inner.entries.lock().expect("entries poisoned");
            if let Some(existing) = entries.get(id) {
                if existing.is_running() {
                    return Ok(record);
                }
            }
            let entry = Arc::new(TaskEntry::new());
            entries.insert(id.to_string(), Arc::clone(&entry));
            entry
        };
        record.state = TaskState::Probing;
        record.error = None;
        record.finished_at = None;
        record.updated_at = now_iso8601();
        self.persist(&record);
        self.append_event(&record, 0, "resumed", json!({}));
        self.spawn_worker(record.clone(), entry);
        self.notify_state(&record);
        Ok(record)
    }

    // -- control ---------------------------------------------------------

    pub async fn pause(&self, id: &str) -> Result<TaskRecord, AgentErrorInfo> {
        let mut record = self.get(id)?;
        match record.state {
            TaskState::Paused => return Ok(record),
            state if state.is_terminal() => {
                return Err(util::conflict(format!(
                    "task {} is already {}; nothing to pause",
                    id,
                    state.as_str()
                ))
                .with_details(json!({ "task_id": id, "state": state.as_str() })));
            }
            _ => {}
        }
        let Some(entry) = self
            .inner
            .entries
            .lock()
            .expect("entries poisoned")
            .get(id)
            .cloned()
        else {
            return Err(util::conflict(format!(
                "task {id} is not owned by this server process; it may be managed by the CLI or another rdm-mcp"
            ))
            .with_details(json!({ "task_id": id, "state": record.state.as_str() })));
        };
        entry.request_pause();
        let deadline = Instant::now() + PAUSE_SETTLE_TIMEOUT;
        loop {
            record = self.get(id)?;
            if record.state == TaskState::Paused {
                return Ok(record);
            }
            if record.state.is_terminal() {
                return Err(util::conflict(format!(
                    "task {} finished as {} before the pause took effect",
                    id,
                    record.state.as_str()
                ))
                .with_details(json!({ "task_id": id, "state": record.state.as_str() })));
            }
            if Instant::now() >= deadline {
                return Err(util::timeout(
                    format!("task {id} did not confirm the pause within 5 s"),
                    json!({ "task_id": id, "state": record.state.as_str() }),
                ));
            }
            sleep(CONTROL_POLL).await;
        }
    }

    pub async fn cancel(
        &self,
        id: &str,
        discard_partial: bool,
    ) -> Result<TaskRecord, AgentErrorInfo> {
        let mut record = self.get(id)?;
        match record.state {
            TaskState::Cancelled => {
                if discard_partial {
                    self.discard_partial(&record);
                }
                return Ok(record);
            }
            state if state.is_terminal() => {
                return Err(util::conflict(format!(
                    "task {} is already {}; cannot cancel",
                    id,
                    state.as_str()
                ))
                .with_details(json!({ "task_id": id, "state": state.as_str() })));
            }
            _ => {}
        }
        match self
            .inner
            .entries
            .lock()
            .expect("entries poisoned")
            .get(id)
            .cloned()
        {
            Some(entry) => entry.request_cancel(),
            None => {
                // A paused task with no live worker: transition directly.
                if record.state == TaskState::Paused {
                    self.finish_cancelled(&mut record);
                    if discard_partial {
                        self.discard_partial(&record);
                    }
                    return Ok(record);
                }
                return Err(util::conflict(format!(
                    "task {id} is not owned by this server process; it may be managed by the CLI or another rdm-mcp"
                ))
                .with_details(json!({ "task_id": id, "state": record.state.as_str() })));
            }
        }
        let deadline = Instant::now() + CANCEL_SETTLE_TIMEOUT;
        loop {
            record = self.get(id)?;
            if record.state == TaskState::Cancelled || record.state == TaskState::Failed {
                if discard_partial && record.state == TaskState::Cancelled {
                    self.discard_partial(&record);
                }
                return Ok(record);
            }
            if record.state == TaskState::Completed {
                return Err(util::conflict(format!(
                    "task {id} completed before cancellation took effect; the file was kept"
                ))
                .with_details(
                    json!({ "task_id": id, "state": "completed", "path": record.path }),
                ));
            }
            if Instant::now() >= deadline {
                return Err(util::timeout(
                    format!("task {id} did not confirm cancellation within 10 s"),
                    json!({ "task_id": id, "state": record.state.as_str() }),
                ));
            }
            sleep(CONTROL_POLL).await;
        }
    }

    fn finish_cancelled(&self, record: &mut TaskRecord) {
        record.state = TaskState::Cancelled;
        record.speed_bps = 0.0;
        record.eta_secs = None;
        record.finished_at = Some(now_iso8601());
        record.updated_at = now_iso8601();
        self.persist(record);
        self.append_event(record, 0, "cancelled", json!({}));
        self.notify_state(record);
    }

    fn discard_partial(&self, record: &TaskRecord) {
        let target = util::expected_target(&record.dest_dir, &record.file_name);
        if let Err(error) = util::remove_target_and_partials(&target) {
            tracing::warn!(task = %record.id, %error, "could not discard partial files");
        }
        // The engine may have de-duplicated the final name; also remove any
        // sibling `.part`/`.part.json` created for this exact file name.
        let mut parent = record.dest_dir.clone();
        parent.push(format!(
            "{}.part",
            util::sanitize_filename(&record.file_name)
        ));
        let _ = std::fs::remove_file(&parent);
        let _ = std::fs::remove_file(parent.with_extension("part.json"));
    }

    // -- waits -----------------------------------------------------------

    /// Wait until `id` settles (terminal state or paused) or `timeout_ms`
    /// elapses. `on_progress` is called at most every [`WAIT_POLL`].
    pub async fn wait(
        &self,
        id: &str,
        timeout_ms: u64,
        wait_cancel: Option<&AtomicBool>,
        mut on_progress: impl FnMut(&TaskRecord),
    ) -> Result<TaskRecord, AgentErrorInfo> {
        // Fail fast when the id does not exist at all.
        let mut record = self.get(id)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if wait_cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                return Err(util::cancelled(
                    format!("wait for task {id} was cancelled by the client"),
                    json!({ "task_id": id, "state": record.state.as_str() }),
                ));
            }
            record = self.get(id)?;
            on_progress(&record);
            if record.state.is_terminal() || record.state == TaskState::Paused {
                return Ok(record);
            }
            if Instant::now() >= deadline {
                return Err(util::timeout(
                    format!("task {id} did not settle within {timeout_ms} ms"),
                    json!({
                        "task_id": id,
                        "state": record.state.as_str(),
                        "downloaded_bytes": record.downloaded_bytes,
                        "total_bytes": record.total_bytes,
                    }),
                ));
            }
            sleep(WAIT_POLL).await;
        }
    }

    /// Pause every locally-owned running task and wait (bounded) for the store
    /// to reflect `paused`. Called on stdin EOF so a restart can resume.
    pub async fn shutdown(&self) {
        let owned: Vec<(String, Arc<TaskEntry>)> = self
            .inner
            .entries
            .lock()
            .expect("entries poisoned")
            .iter()
            .filter(|(_, entry)| entry.is_running())
            .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
            .collect();
        if owned.is_empty() {
            return;
        }
        for (_, entry) in &owned {
            entry.request_pause();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut pending = 0;
            for (id, _) in &owned {
                if let Ok(record) = self.inner.store.get(id) {
                    if !record.state.is_terminal() && record.state != TaskState::Paused {
                        pending += 1;
                    }
                }
            }
            if pending == 0 || Instant::now() >= deadline {
                if pending > 0 {
                    tracing::warn!(
                        pending,
                        "shutdown: some tasks did not confirm pause in time"
                    );
                }
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    // -- worker plumbing --------------------------------------------------

    fn spawn_worker(&self, record: TaskRecord, entry: Arc<TaskEntry>) {
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            run_worker(inner, record, entry).await;
        });
    }

    fn persist(&self, record: &TaskRecord) {
        if let Err(error) = self.inner.store.update(record) {
            tracing::warn!(task = %record.id, %error, "task persistence failed");
        }
    }

    fn append_event(&self, record: &TaskRecord, seq: u64, kind: &str, data: Value) {
        self.append_event_ext(record, seq, kind, data);
    }

    fn append_event_ext(&self, record: &TaskRecord, seq: u64, kind: &str, data: Value) {
        let next = if seq == 0 {
            self.next_event_seq(&record.id)
        } else {
            seq
        };
        let event = TaskEvent::new(record.id.clone(), next, kind, data);
        if let Err(error) = self.inner.store.append_event(&event) {
            tracing::warn!(task = %record.id, %error, "event journal write failed");
        }
    }

    fn next_event_seq(&self, id: &str) -> u64 {
        self.inner
            .store
            .events(id, 0)
            .map(|events| {
                events
                    .iter()
                    .map(|event| event.seq)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1)
            })
            .unwrap_or(1)
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

async fn run_worker(inner: Arc<Inner>, mut record: TaskRecord, entry: Arc<TaskEntry>) {
    let mut next_seq = 1u64;
    // 1. Wait for a concurrency permit (queued state) with control checks.
    let permit = loop {
        if entry.cancel_requested() {
            entry.running.store(false, Ordering::SeqCst);
            record.state = TaskState::Cancelled;
            record.downloaded_bytes = 0;
            record.finished_at = Some(now_iso8601());
            record.updated_at = now_iso8601();
            persist(&inner, &record);
            append_event(
                &inner,
                &record,
                &mut next_seq,
                "cancelled",
                json!({"stage": "queued"}),
            );
            notify(&inner, &record);
            return;
        }
        if entry.pause_requested() {
            entry.running.store(false, Ordering::SeqCst);
            record.state = TaskState::Paused;
            record.updated_at = now_iso8601();
            persist(&inner, &record);
            append_event(
                &inner,
                &record,
                &mut next_seq,
                "paused",
                json!({"stage": "queued"}),
            );
            notify(&inner, &record);
            return;
        }
        match Arc::clone(&inner.permits).try_acquire_owned() {
            Ok(permit) => break permit,
            Err(_) => sleep(QUEUE_POLL).await,
        }
    };

    // 2. Transition to probing and run the engine.
    record.state = TaskState::Probing;
    record.error = None;
    record.updated_at = now_iso8601();
    persist(&inner, &record);
    append_event(&inner, &record, &mut next_seq, "probing", json!({}));
    notify(&inner, &record);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DownloadEvent>();
    let handle = entry.download.clone();
    let request = record.request.clone();
    let download = rdm_core::download_with_handle(request, Some(tx), handle);
    tokio::pin!(download);

    let mut last_persist = Instant::now();
    let mut last_progress_event = Instant::now();
    let mut metadata_seen = false;
    let mut rx_open = true;

    let outcome = loop {
        tokio::select! {
            result = &mut download => break result,
            maybe_event = rx.recv(), if rx_open => {
                match maybe_event {
                    Some(event) => {
                        apply_engine_event(
                            &inner,
                            &mut record,
                            &entry,
                            event,
                            &mut next_seq,
                            &mut last_persist,
                            &mut last_progress_event,
                            &mut metadata_seen,
                        );
                    }
                    None => rx_open = false,
                }
            }
        }
    };
    drop(permit);
    while let Ok(event) = rx.try_recv() {
        apply_engine_event(
            &inner,
            &mut record,
            &entry,
            event,
            &mut next_seq,
            &mut last_persist,
            &mut last_progress_event,
            &mut metadata_seen,
        );
    }
    finish(&inner, &mut record, outcome, &entry, &mut next_seq);
    entry.running.store(false, Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)]
fn apply_engine_event(
    inner: &Arc<Inner>,
    record: &mut TaskRecord,
    entry: &Arc<TaskEntry>,
    event: DownloadEvent,
    next_seq: &mut u64,
    last_persist: &mut Instant,
    last_progress_event: &mut Instant,
    metadata_seen: &mut bool,
) {
    match event {
        DownloadEvent::Probing => {}
        DownloadEvent::Metadata(meta) => {
            record.total_bytes = meta.total_bytes;
            record.range_supported = Some(meta.supports_ranges);
            record.http_version = meta.http_version.clone();
            if record.request.file_name.is_none() {
                record.file_name = meta.file_name.clone();
            }
            record.updated_at = now_iso8601();
            persist(inner, record);
            if !*metadata_seen {
                *metadata_seen = true;
                append_event(
                    inner,
                    record,
                    next_seq,
                    "metadata",
                    json!({
                        "final_url": rdm_agent::redact_url(&meta.final_url),
                        "file_name": meta.file_name,
                        "total_bytes": meta.total_bytes,
                        "range_supported": meta.supports_ranges,
                        "http_version": meta.http_version,
                    }),
                );
            }
            check_size_limit(inner, record, entry);
        }
        DownloadEvent::Started {
            connections,
            chunk_size,
            total_chunks,
            resumed_chunks,
        } => {
            record.state = TaskState::Downloading;
            record.connections = connections.max(1);
            record.chunk_size = Some(chunk_size);
            record.updated_at = now_iso8601();
            persist(inner, record);
            append_event(
                inner,
                record,
                next_seq,
                "started",
                json!({
                    "connections": connections,
                    "chunk_size": chunk_size,
                    "total_chunks": total_chunks,
                    "resumed_chunks": resumed_chunks,
                }),
            );
            notify(inner, record);
        }
        DownloadEvent::Progress {
            downloaded,
            total,
            speed_bps,
            eta_secs,
            active_workers: _,
        } => {
            record.downloaded_bytes = downloaded;
            if total.is_some() {
                record.total_bytes = total;
            }
            record.speed_bps = speed_bps;
            record.eta_secs = eta_secs;
            record.updated_at = now_iso8601();
            if last_persist.elapsed() >= PERSIST_INTERVAL {
                *last_persist = Instant::now();
                persist(inner, record);
            }
            if last_progress_event.elapsed() >= EVENT_PROGRESS_INTERVAL {
                *last_progress_event = Instant::now();
                append_event(
                    inner,
                    record,
                    next_seq,
                    "progress",
                    json!({
                        "downloaded": downloaded,
                        "total": record.total_bytes,
                        "speed_bps": speed_bps,
                        "eta_secs": eta_secs,
                    }),
                );
            }
            check_size_limit(inner, record, entry);
        }
        DownloadEvent::Retrying {
            chunk,
            attempt,
            delay_ms,
            reason,
        } => {
            append_event(
                inner,
                record,
                next_seq,
                "retry",
                json!({
                    "chunk": chunk,
                    "attempt": attempt,
                    "delay_ms": delay_ms,
                    "reason": rdm_agent::redact_text(&reason),
                }),
            );
        }
        DownloadEvent::Completed { summary } => {
            // The authoritative completion handling happens in `finish`; this
            // event is used for the final persisted snapshot.
            apply_summary(record, &summary);
        }
        DownloadEvent::Failed { error } => {
            // The typed error arrives via the download outcome; keep a redacted
            // event for diagnostics only.
            append_event(
                inner,
                record,
                next_seq,
                "engine-failed",
                json!({ "error": rdm_agent::redact_text(&error) }),
            );
        }
        DownloadEvent::Paused { downloaded } => {
            record.downloaded_bytes = downloaded;
            record.updated_at = now_iso8601();
        }
    }
}

fn check_size_limit(inner: &Arc<Inner>, record: &TaskRecord, entry: &Arc<TaskEntry>) {
    let Some(max) = inner.policy.max_bytes_per_task else {
        return;
    };
    if record.downloaded_bytes > max && !entry.overflow.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            task = %record.id,
            downloaded = record.downloaded_bytes,
            max,
            "download exceeded policy.max_bytes_per_task; cancelling"
        );
        entry.download.cancel();
    }
}

fn apply_summary(record: &mut TaskRecord, summary: &DownloadSummary) {
    record.state = TaskState::Completed;
    record.total_bytes = Some(summary.total_bytes);
    record.downloaded_bytes = summary.total_bytes;
    record.speed_bps = summary.avg_speed_bps;
    record.eta_secs = Some(0.0);
    record.path = Some(summary.path.clone());
    record.range_supported = Some(summary.range_supported);
    record.http_version = summary.http_version.clone();
    record.result = Some(rdm_agent::TaskResult {
        path: summary.path.display().to_string(),
        size: summary.total_bytes,
        sha256: summary.sha256.clone(),
        elapsed_ms: summary.elapsed_ms,
        avg_speed_bps: summary.avg_speed_bps,
    });
    record.finished_at = Some(now_iso8601());
    record.updated_at = now_iso8601();
}

/// Exact bytes retained in the `.part.json` chunk bitmap. The engine's
/// `Paused`/`Cancelled` counter may count (then roll back) bytes of an
/// in-flight chunk and can even be 0 for a very early pause; the store should
/// report the data a resume will actually keep. Falls back to the engine
/// counter when the resume state file is unreadable.
fn retained_partial_bytes(record: &TaskRecord, engine_reported: u64) -> u64 {
    let target = util::expected_target(&record.dest_dir, &record.file_name);
    let (_, state_path) = util::part_paths(&target);
    std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("downloaded_bytes").and_then(Value::as_u64))
        .unwrap_or_else(|| engine_reported.max(record.downloaded_bytes))
}

fn finish(
    inner: &Arc<Inner>,
    record: &mut TaskRecord,
    outcome: Result<DownloadSummary, DownloadError>,
    entry: &Arc<TaskEntry>,
    next_seq: &mut u64,
) {
    match outcome {
        Ok(summary) => {
            apply_summary(record, &summary);
            persist(inner, record);
            append_event(
                inner,
                record,
                next_seq,
                "completed",
                json!({
                    "path": summary.path.display().to_string(),
                    "size": summary.total_bytes,
                    "sha256": summary.sha256,
                    "elapsed_ms": summary.elapsed_ms,
                    "avg_speed_bps": summary.avg_speed_bps,
                    "connections": summary.connections,
                    "resumed_chunks": summary.resumed_chunks,
                }),
            );
            notify(inner, record);
        }
        Err(DownloadError::Paused { downloaded }) => {
            record.state = TaskState::Paused;
            let retained = retained_partial_bytes(record, downloaded);
            record.downloaded_bytes = retained;
            record.speed_bps = 0.0;
            record.eta_secs = None;
            record.updated_at = now_iso8601();
            persist(inner, record);
            append_event(
                inner,
                record,
                next_seq,
                "paused",
                json!({ "downloaded": retained }),
            );
            notify(inner, record);
        }
        Err(DownloadError::Cancelled { downloaded }) => {
            record.downloaded_bytes = retained_partial_bytes(record, downloaded);
            if entry.overflow.load(Ordering::SeqCst) {
                record.state = TaskState::Failed;
                record.error = Some(
                    AgentErrorInfo::new(
                        ErrorCode::PolicyDenied,
                        format!(
                            "download exceeded policy.max_bytes_per_task ({} bytes)",
                            inner.policy.max_bytes_per_task.unwrap_or_default()
                        ),
                    )
                    .with_hint("Increase policy.max_bytes_per_task or download a smaller file.")
                    .with_details(json!({
                        "downloaded_bytes": downloaded,
                        "max_bytes_per_task": inner.policy.max_bytes_per_task,
                    })),
                );
                record.finished_at = Some(now_iso8601());
                persist(inner, record);
                append_event(
                    inner,
                    record,
                    next_seq,
                    "failed",
                    json!({
                        "code": record.error.as_ref().map(|error| error.code.as_str()),
                        "downloaded_bytes": downloaded,
                    }),
                );
                notify(inner, record);
            } else {
                record.state = TaskState::Cancelled;
                record.speed_bps = 0.0;
                record.eta_secs = None;
                record.finished_at = Some(now_iso8601());
                persist(inner, record);
                append_event(
                    inner,
                    record,
                    next_seq,
                    "cancelled",
                    json!({ "downloaded": downloaded }),
                );
                notify(inner, record);
            }
        }
        Err(error) => {
            record.state = TaskState::Failed;
            record.speed_bps = 0.0;
            record.eta_secs = None;
            record.error = Some(rdm_agent::AgentErrorInfo::from_download_error(&error));
            record.finished_at = Some(now_iso8601());
            persist(inner, record);
            append_event(
                inner,
                record,
                next_seq,
                "failed",
                json!({
                    "code": record.error.as_ref().map(|error| error.code.as_str()),
                    "message": record.error.as_ref().map(|error| error.message.as_str()),
                }),
            );
            notify(inner, record);
        }
    }
}

fn persist(inner: &Inner, record: &TaskRecord) {
    if let Err(error) = inner.store.update(record) {
        tracing::warn!(task = %record.id, %error, "task persistence failed");
    }
}

fn append_event(inner: &Inner, record: &TaskRecord, next_seq: &mut u64, kind: &str, data: Value) {
    let event = TaskEvent::new(record.id.clone(), *next_seq, kind, data);
    *next_seq += 1;
    if let Err(error) = inner.store.append_event(&event) {
        tracing::warn!(task = %record.id, %error, "event journal write failed");
    }
}

fn notify(inner: &Inner, record: &TaskRecord) {
    let notifier = inner.notifier.lock().expect("notifier poisoned").clone();
    if let Some(notifier) = notifier {
        let level = match record.state {
            TaskState::Failed => "error",
            TaskState::Paused | TaskState::Cancelled => "warning",
            _ => "info",
        };
        notifier(
            "notifications/message",
            json!({
                "level": level,
                "logger": "rdm-mcp",
                "data": format!(
                    "task {} -> {} ({} / {})",
                    record.id,
                    record.state.as_str(),
                    util::format_size(record.downloaded_bytes),
                    record
                        .total_bytes
                        .map(util::format_size)
                        .unwrap_or_else(|| "?".to_string()),
                )
            }),
        );
    }
}

/// True when the download source must be treated as "unknown length" (so the
/// caller can pre-warn). Kept public for diagnostics.
pub fn is_unknown_length(record: &TaskRecord) -> bool {
    record.total_bytes.is_none()
}

/// Fixture server alias re-exported for integration tests.
pub type FixtureServer = TestServer;

#[cfg(test)]
mod tests {
    use super::*;
    use rdm_agent::PolicyDecision;
    use rdm_fixture_server::{Config, TestServer};
    use std::path::Path;

    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let index = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rdm-mcp-manager-{tag}-{}-{index}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn policy_for(root: &Path, max_concurrent: usize) -> Policy {
        Policy {
            allowed_hosts: vec!["127.0.0.1".into()],
            allowed_dirs: vec![root.join("downloads")],
            allow_private_hosts: true,
            max_concurrent_tasks: max_concurrent,
            allow_overwrite: false,
            ..Policy::default()
        }
    }

    fn manager_for(root: &Path, policy: Policy) -> TaskManager {
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        TaskManager::new(
            TaskStore::new(root),
            tokio::runtime::Handle::current(),
            policy,
        )
    }

    fn request(url: &str) -> DownloadRequest {
        DownloadRequest::new(url)
    }

    fn request_with_sha(url: &str, sha256: &str) -> DownloadRequest {
        let mut request = DownloadRequest::new(url);
        request.expected_sha256 = Some(sha256.to_string());
        // Small chunks make pause/cancel observable within a few completed
        // chunks regardless of machine speed.
        request.chunk_size = Some(rdm_core::MIN_CHUNK_SIZE);
        request
    }

    fn content_sha256(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }

    async fn wait_settled(manager: &TaskManager, id: &str) -> TaskRecord {
        manager
            .wait(id, 60_000, None, |_| {})
            .await
            .expect("task should settle")
    }

    #[tokio::test]
    async fn completes_multi_connection_download() {
        let root = temp_root("complete");
        let server = TestServer::start(Config::new(2 * 1024 * 1024))
            .await
            .unwrap();
        let manager = manager_for(&root, policy_for(&root, 2));
        let expected = content_sha256(&server.content());
        let record = manager
            .create_task(
                "test",
                request_with_sha(&server.file_url(), &expected),
                root.join("downloads"),
                "file.bin".to_string(),
                Some(server.size()),
                Some(true),
                None,
                None,
                Vec::new(),
            )
            .unwrap();
        let settled = wait_settled(&manager, &record.id).await;
        assert_eq!(settled.state, TaskState::Completed);
        let result = settled.result.as_ref().unwrap();
        assert_eq!(result.sha256.as_deref(), Some(expected.as_str()));
        assert_eq!(result.size, server.size());
        assert!(Path::new(&result.path).exists());
        // The engine really used multiple range requests.
        let stats = server.stats();
        assert!(
            stats.responses_206 > 1,
            "expected ranged requests: {stats:?}"
        );
        let loaded = manager.store().get(&record.id).unwrap();
        assert_eq!(loaded.state, TaskState::Completed);
        let events = manager.store().events(&record.id, 100).unwrap();
        assert!(events.iter().any(|event| event.r#type == "completed"));
        server.shutdown().await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn pause_then_resume_does_not_restart_from_zero() {
        let root = temp_root("pause");
        let size = 8 * 1024 * 1024;
        let config = Config::new(size).with_delay(50).with_delay_every(64 * 1024);
        let server = TestServer::start(config).await.unwrap();
        let manager = manager_for(&root, policy_for(&root, 2));
        let expected = content_sha256(&server.content());
        let record = manager
            .create_task(
                "test",
                request_with_sha(&server.file_url(), &expected),
                root.join("downloads"),
                "file.bin".to_string(),
                Some(size),
                Some(true),
                None,
                None,
                Vec::new(),
            )
            .unwrap();
        // Wait until some bytes have been transferred, then pause.
        let paused = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let current = manager.get(&record.id).unwrap();
                if current.downloaded_bytes > 256 * 1024 && current.state == TaskState::Downloading
                {
                    return manager.pause(&record.id).await;
                }
                if current.state.is_terminal() {
                    return Ok(current);
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("pause loop timed out")
        .expect("pause should succeed");
        assert_eq!(paused.state, TaskState::Paused);
        assert!(paused.downloaded_bytes > 0);
        let bytes_before_resume = server.stats().body_bytes_sent;
        let resumed = manager.resume(&record.id).unwrap();
        assert!(!resumed.state.is_terminal());
        let settled = wait_settled(&manager, &record.id).await;
        assert_eq!(settled.state, TaskState::Completed);
        assert_eq!(
            settled.result.as_ref().unwrap().sha256.as_deref(),
            Some(expected.as_str())
        );
        // Resume must not re-fetch the whole file: bytes sent after the pause
        // are bounded by the not-yet-downloaded remainder plus retry slack.
        let delta = server.stats().body_bytes_sent - bytes_before_resume;
        let remaining = size - paused.downloaded_bytes;
        assert!(
            delta <= remaining + 1024 * 1024,
            "resume re-downloaded too much: delta={delta} remaining={remaining}"
        );
        server.shutdown().await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn cancel_keeps_partial_and_resume_completes() {
        let root = temp_root("cancel");
        let size = 8 * 1024 * 1024;
        let config = Config::new(size).with_delay(50).with_delay_every(64 * 1024);
        let server = TestServer::start(config).await.unwrap();
        let manager = manager_for(&root, policy_for(&root, 2));
        let expected = content_sha256(&server.content());
        let record = manager
            .create_task(
                "test",
                request_with_sha(&server.file_url(), &expected),
                root.join("downloads"),
                "file.bin".to_string(),
                Some(size),
                Some(true),
                None,
                None,
                Vec::new(),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let current = manager.get(&record.id).unwrap();
                if current.downloaded_bytes > 256 * 1024 {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let cancelled = manager.cancel(&record.id, false).await.unwrap();
        assert_eq!(cancelled.state, TaskState::Cancelled);
        let target = util::expected_target(&cancelled.dest_dir, &cancelled.file_name);
        let (part, state) = util::part_paths(&target);
        assert!(
            part.exists() || state.exists(),
            "partial state should be kept"
        );
        let resumed = manager.resume(&record.id).unwrap();
        assert_eq!(resumed.state, TaskState::Probing);
        let settled = wait_settled(&manager, &record.id).await;
        assert_eq!(settled.state, TaskState::Completed);
        assert_eq!(
            settled.result.as_ref().unwrap().sha256.as_deref(),
            Some(expected.as_str())
        );
        server.shutdown().await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn idempotency_key_returns_existing_task() {
        let root = temp_root("idem");
        let server = TestServer::start(Config::new(1024 * 1024)).await.unwrap();
        let manager = manager_for(&root, policy_for(&root, 2));
        let make = |key: &str| {
            manager
                .create_task(
                    "test",
                    request(&server.file_url()),
                    root.join("downloads"),
                    "file.bin".to_string(),
                    Some(server.size()),
                    Some(true),
                    None,
                    Some(key.to_string()),
                    Vec::new(),
                )
                .unwrap()
        };
        let first = make("agent-run-42");
        let second = make("agent-run-42");
        assert_eq!(first.id, second.id);
        assert_eq!(
            manager
                .store()
                .list(&rdm_agent::TaskFilter::default())
                .unwrap()
                .len(),
            1
        );
        let settled = wait_settled(&manager, &first.id).await;
        assert_eq!(settled.state, TaskState::Completed);
        server.shutdown().await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn concurrency_limit_queues_tasks() {
        let root = temp_root("queue");
        let size = 4 * 1024 * 1024;
        let config = Config::new(size).with_delay(20);
        let server = TestServer::start(config).await.unwrap();
        let manager = manager_for(&root, policy_for(&root, 1));
        let mut ids = Vec::new();
        for index in 0..2 {
            let record = manager
                .create_task(
                    "test",
                    request(&server.file_url()),
                    root.join("downloads"),
                    format!("file-{index}.bin"),
                    Some(size),
                    Some(true),
                    None,
                    None,
                    Vec::new(),
                )
                .unwrap();
            ids.push(record.id);
        }
        // The second task is queued while the first holds the only permit.
        let mut saw_queued = false;
        for _ in 0..300 {
            let second = manager.get(&ids[1]).unwrap();
            if second.state == TaskState::Queued {
                saw_queued = true;
            }
            if manager.get(&ids[0]).unwrap().state.is_terminal() {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(saw_queued, "second task should have been queued");
        let first = wait_settled(&manager, &ids[0]).await;
        let second = wait_settled(&manager, &ids[1]).await;
        assert_eq!(first.state, TaskState::Completed);
        assert_eq!(second.state, TaskState::Completed);
        assert_eq!(first.result.unwrap().size, size);
        server.shutdown().await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn unknown_length_over_limit_fails_policy_denied() {
        let root = temp_root("unknown");
        let size = 4 * 1024 * 1024;
        let config = Config::new(size)
            .without_length()
            .without_ranges()
            .with_delay(20);
        let server = TestServer::start(config).await.unwrap();
        let mut policy = policy_for(&root, 2);
        policy.max_bytes_per_task = Some(512 * 1024);
        let manager = manager_for(&root, policy);
        let record = manager
            .create_task(
                "test",
                request(&server.file_url()),
                root.join("downloads"),
                "unknown.bin".to_string(),
                None,
                None,
                None,
                None,
                Vec::new(),
            )
            .unwrap();
        let settled = wait_settled(&manager, &record.id).await;
        assert_eq!(settled.state, TaskState::Failed);
        let error = settled.error.as_ref().unwrap();
        assert_eq!(error.code, ErrorCode::PolicyDenied);
        assert!(error.message.contains("max_bytes_per_task"));
        server.shutdown().await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn invalid_control_transitions_are_conflicts() {
        let root = temp_root("transitions");
        let server = TestServer::start(Config::new(512 * 1024)).await.unwrap();
        let manager = manager_for(&root, policy_for(&root, 2));
        let record = manager
            .create_task(
                "test",
                request(&server.file_url()),
                root.join("downloads"),
                "file.bin".to_string(),
                Some(server.size()),
                Some(true),
                None,
                None,
                Vec::new(),
            )
            .unwrap();
        let settled = wait_settled(&manager, &record.id).await;
        assert_eq!(settled.state, TaskState::Completed);
        let pause_error = manager.pause(&record.id).await.unwrap_err();
        assert_eq!(pause_error.code, ErrorCode::Conflict);
        let cancel_error = manager.cancel(&record.id, false).await.unwrap_err();
        assert_eq!(cancel_error.code, ErrorCode::Conflict);
        // Resuming a completed task is an idempotent no-op.
        let resumed = manager.resume(&record.id).unwrap();
        assert_eq!(resumed.state, TaskState::Completed);
        // Unknown ids are not-found.
        assert_eq!(
            manager.get("tsk_missing").unwrap_err().code,
            ErrorCode::NotFound
        );
        // The plan helpers reject traversal ids.
        assert!(manager.take_plan("../escape").is_err());
        server.shutdown().await;
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn policy_gate_helpers_are_exposed() {
        let root = temp_root("gate");
        let policy = policy_for(&root, 1);
        assert!(matches!(
            policy.check_url("http://127.0.0.1:9/x"),
            PolicyDecision::Allow
        ));
        assert!(matches!(
            policy.check_url("https://example.com/x"),
            PolicyDecision::Deny { .. }
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn error_maps_store_failures() {
        let error = store_error(AgentStoreError::NotFound("x".into()));
        assert_eq!(error.code, ErrorCode::NotFound);
        let error = store_error(AgentStoreError::InvalidId("../x".into()));
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
}
