//! `rdm run` — execute a batch of downloads from a JSON plan.
//!
//! Input (contract `docs/agent-integration.md` §5):
//! ```json
//! {
//!   "schema_version": "rdm.agent/v1",
//!   "tasks": [{"url": "...", "dest_dir": "...", "file_name": "a.iso"}],
//!   "max_concurrency": 3,
//!   "wait": true,
//!   "stop_on_error": false
//! }
//! ```
//!
//! The CLI process is synchronous by design: it awaits every task before it
//! exits (a detached daemon is future work, tracked as a warning in the
//! output). Every task is persisted into the shared `TaskStore` and can be
//! inspected later with `rdm tasks` / `rdm task <id>`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rdm_agent::{AgentErrorInfo, ErrorCode, Policy, TaskState, TaskStore, TaskView};
use rdm_core::{DownloadEvent, DownloadRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Semaphore};

use crate::cli::RunArgs;
use crate::output::{engine_error, invalid_request, OutputMode, Reporter};
use crate::tracker::TaskTracker;
use crate::util;

const MAX_CONCURRENCY: usize = 32;

#[derive(Debug, Deserialize)]
struct BatchInput {
    #[serde(default)]
    schema_version: Option<String>,
    #[serde(default)]
    tasks: Vec<BatchTask>,
    #[serde(default)]
    max_concurrency: Option<usize>,
    #[serde(default)]
    wait: Option<bool>,
    #[serde(default)]
    stop_on_error: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct BatchTask {
    url: String,
    #[serde(default)]
    dest_dir: Option<PathBuf>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    connections: Option<usize>,
    #[serde(default)]
    dynamic_connections: Option<bool>,
    #[serde(default)]
    single_stream: Option<bool>,
    #[serde(default)]
    prefer_http2: Option<bool>,
    #[serde(default)]
    mirrors: Vec<String>,
    #[serde(default)]
    chunk_size: Option<u64>,
    #[serde(default)]
    expected_sha256: Option<String>,
    #[serde(default)]
    speed_limit_bps: Option<u64>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    cookie: Option<String>,
    #[serde(default)]
    referer: Option<String>,
    #[serde(default)]
    user_agent: Option<String>,
    #[serde(default)]
    proxy: Option<String>,
    #[serde(default)]
    resume: Option<bool>,
    #[serde(default)]
    insecure: Option<bool>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Debug)]
struct TaskSpec {
    index: usize,
    request: DownloadRequest,
    dest_dir: PathBuf,
    file_name: String,
    idempotency_key: Option<String>,
}

struct RunUpdate {
    task_id: String,
    kind: &'static str,
    data: Value,
}

struct TaskOutcome {
    index: usize,
    view: TaskView,
    error: Option<AgentErrorInfo>,
    reused: bool,
}

pub async fn run(mode: OutputMode, store_root: &Path, args: RunArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "run");
    let started = Instant::now();

    let raw = match read_input(&args.input).await {
        Ok(raw) => raw,
        Err(message) => return reporter.finish_err(invalid_request(message)),
    };
    let input: BatchInput = match serde_json::from_str(&raw) {
        Ok(input) => input,
        Err(error) => {
            return reporter.finish_err(
                invalid_request(format!("invalid batch JSON: {error}"))
                    .with_hint("`rdm schema --output json` documents the run input shape"),
            )
        }
    };
    if let Some(version) = &input.schema_version {
        if version != rdm_agent::SCHEMA_VERSION {
            return reporter.finish_err(invalid_request(format!(
                "unsupported schema_version `{version}` (expected `{}`)",
                rdm_agent::SCHEMA_VERSION
            )));
        }
    }
    if input.tasks.is_empty() {
        return reporter.finish_err(invalid_request(
            "batch input contains no tasks (expected a non-empty `tasks` array)",
        ));
    }

    // Policy: only enforced when an explicit policy.json exists (documented
    // CLI deviation; MCP enforces the safe defaults for every call).
    let policy: Option<Policy> = match util::load_policy(store_root) {
        Ok(policy) => policy.map(Policy::normalized),
        Err(message) => {
            return reporter.finish_err(
                invalid_request(format!("invalid policy.json: {message}"))
                    .with_hint("fix `policy.json` or delete it to use built-in defaults"),
            )
        }
    };
    let mutation_warnings = Vec::new();
    let mut warnings: Vec<String> = mutation_warnings;
    if policy.is_none() {
        warnings.push(
            "policy-file-absent: default policy limits are not enforced by `rdm run`".to_string(),
        );
    }
    if !args.wait && input.wait != Some(true) {
        warnings.push(
            "cli-run-is-synchronous: all tasks are awaited before the process exits".to_string(),
        );
    }

    let max_concurrency = input.max_concurrency.unwrap_or_else(|| {
        policy
            .as_ref()
            .map(|policy| policy.max_concurrent_tasks)
            .unwrap_or(3)
    });
    if max_concurrency == 0 || max_concurrency > MAX_CONCURRENCY {
        return reporter.finish_err(invalid_request(format!(
            "max_concurrency must be between 1 and {MAX_CONCURRENCY}"
        )));
    }
    let stop_on_error = input.stop_on_error.unwrap_or(false);

    let mut specs = Vec::with_capacity(input.tasks.len());
    for (index, task) in input.tasks.into_iter().enumerate() {
        match build_spec(index, task) {
            Ok(spec) => specs.push(spec),
            Err(error) => return reporter.finish_err(error),
        }
    }

    let semaphore = Arc::new(Semaphore::new(max_concurrency));
    let abort = Arc::new(AtomicBool::new(false));
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<RunUpdate>();
    let mut join_set = tokio::task::JoinSet::new();
    for spec in specs {
        let store_root = store_root.to_path_buf();
        let policy = policy.clone();
        let semaphore = Arc::clone(&semaphore);
        let abort = Arc::clone(&abort);
        let update_tx = update_tx.clone();
        join_set.spawn(async move {
            run_one(
                spec,
                &store_root,
                policy.as_ref(),
                semaphore,
                abort,
                update_tx,
            )
            .await
        });
    }
    drop(update_tx);

    let mut outcomes: Vec<TaskOutcome> = Vec::new();
    let mut runtime_failure: Option<AgentErrorInfo> = None;
    while !join_set.is_empty() {
        tokio::select! {
            Some(update) = update_rx.recv() => handle_update(&mut reporter, update),
            Some(joined) = join_set.join_next() => {
                match joined {
                    Ok(Ok(outcome)) => {
                        if stop_on_error && outcome.error.is_some() {
                            abort.store(true, Ordering::SeqCst);
                        }
                        outcomes.push(outcome);
                    }
                    Ok(Err(error)) => {
                        runtime_failure = Some(error);
                    }
                    Err(join_error) => {
                        runtime_failure = Some(AgentErrorInfo::new(
                            ErrorCode::Internal,
                            format!("batch worker terminated unexpectedly: {join_error}"),
                        ));
                    }
                }
            }
        }
    }
    while let Ok(update) = update_rx.try_recv() {
        handle_update(&mut reporter, update);
    }
    outcomes.sort_by_key(|outcome| outcome.index);

    let completed = outcomes
        .iter()
        .filter(|outcome| outcome.view.state == TaskState::Completed)
        .count();
    let cancelled = outcomes
        .iter()
        .filter(|outcome| outcome.view.state == TaskState::Cancelled)
        .count();
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.view.state == TaskState::Failed)
        .count();
    let reused = outcomes.iter().filter(|outcome| outcome.reused).count();
    let errors: Vec<&AgentErrorInfo> = outcomes
        .iter()
        .filter_map(|outcome| outcome.error.as_ref())
        .collect();
    let only_policy_errors = !errors.is_empty()
        && errors.iter().all(|error| {
            matches!(
                error.code,
                ErrorCode::PolicyDenied | ErrorCode::NeedsApproval | ErrorCode::Conflict
            )
        });
    let exit_error: Option<AgentErrorInfo> = if let Some(error) = runtime_failure {
        Some(error)
    } else if !errors.is_empty() {
        if only_policy_errors {
            Some(
                AgentErrorInfo::new(ErrorCode::PolicyDenied, format!("{failed} task(s) denied by policy"))
                    .with_hint("relax `policy.json` (allowed_hosts/allowed_dirs/max_bytes_per_task) or use `rdm download`")
                    .with_details(json!({ "failed": failed })),
            )
        } else {
            Some(
                AgentErrorInfo::new(ErrorCode::Internal, format!("{failed} task(s) failed"))
                    .with_hint("inspect `data.tasks[].error`; retry with `rdm run --input <file>`")
                    .with_details(json!({ "failed": failed })),
            )
        }
    } else if cancelled > 0 {
        Some(
            AgentErrorInfo::new(
                ErrorCode::Cancelled,
                format!("{cancelled} task(s) cancelled"),
            )
            .with_hint("re-run without `stop_on_error` to finish the rest"),
        )
    } else {
        None
    };

    let views: Vec<TaskView> = outcomes
        .iter()
        .map(|outcome| outcome.view.clone())
        .collect();
    let data = json!({
        "tasks": views,
        "completed": completed,
        "failed": failed,
        "cancelled": cancelled,
        "reused": reused,
        "max_concurrency": max_concurrency,
        "stop_on_error": stop_on_error,
        "synchronous": true,
        "warnings": warnings,
        "duration_ms": super::download::elapsed_ms(started),
        "state_dir": store_root.display().to_string(),
    });

    if reporter.is_text() {
        for outcome in &outcomes {
            let view = &outcome.view;
            match view.state {
                TaskState::Completed => {
                    let path = view
                        .result
                        .as_ref()
                        .map(|result| result.path.clone())
                        .or_else(|| view.path.as_ref().map(|path| path.display().to_string()))
                        .unwrap_or_else(|| "<unknown>".to_string());
                    crate::outln!("[done] {}  {}  {}", view.id, view.file_name, path);
                }
                TaskState::Failed => {
                    let error = outcome
                        .error
                        .as_ref()
                        .map(|error| format!("[{}] {}", error.code.as_str(), error.message))
                        .unwrap_or_else(|| "unknown error".to_string());
                    crate::outln!("[fail] {}  {}", view.id, error);
                }
                TaskState::Cancelled => {
                    crate::outln!("[stop] {}  cancelled", view.id);
                }
                other => {
                    crate::outln!("[{}] {}", other.as_str(), view.id);
                }
            }
        }
        crate::outln!(
            "run: {completed} completed, {failed} failed, {cancelled} cancelled (max_concurrency={max_concurrency})"
        );
    }

    match exit_error {
        Some(error) => reporter.finish_report(data, Some(error)),
        None => reporter.finish_report(data, None),
    }
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

async fn read_input(source: &str) -> Result<String, String> {
    if source == "-" {
        use tokio::io::AsyncReadExt;
        let mut buffer = String::new();
        let mut stdin = tokio::io::stdin();
        stdin
            .read_to_string(&mut buffer)
            .await
            .map_err(|error| format!("cannot read batch JSON from stdin: {error}"))?;
        if buffer.trim().is_empty() {
            return Err("stdin contained no batch JSON".to_string());
        }
        Ok(buffer)
    } else {
        std::fs::read_to_string(source)
            .map_err(|error| format!("cannot read batch input `{source}`: {error}"))
    }
}

impl BatchTask {
    /// Convert one batch entry into an engine request (shared validation lives
    /// in the `download` command module).
    fn into_request(self) -> Result<DownloadRequest, String> {
        let BatchTask {
            url,
            dest_dir,
            file_name,
            connections,
            dynamic_connections,
            single_stream,
            prefer_http2,
            mirrors,
            chunk_size,
            expected_sha256,
            speed_limit_bps,
            headers,
            cookie,
            referer,
            user_agent,
            proxy,
            resume,
            insecure,
            idempotency_key: _,
        } = self;

        let mut request = DownloadRequest::new(url);
        request.output_dir = dest_dir;
        request.file_name = file_name;
        request.connections = super::download::normalize_connections(connections)?;
        if let Some(dynamic) = dynamic_connections {
            request.dynamic_connections = dynamic;
        }
        if let Some(single) = single_stream {
            request.single_stream = single;
        }
        if let Some(prefer) = prefer_http2 {
            request.prefer_http2 = prefer;
        }
        super::download::validate_mirrors(&mirrors)?;
        request.mirrors = mirrors;
        request.chunk_size = chunk_size;
        request.expected_sha256 = expected_sha256;
        request.speed_limit_bps = speed_limit_bps;
        request.headers = headers;
        request.cookie = cookie;
        request.referer = referer;
        request.user_agent = user_agent;
        request.proxy = proxy;
        if let Some(resume) = resume {
            request.resume = resume;
        }
        if let Some(insecure) = insecure {
            request.insecure = insecure;
        }
        // Keep the persisted/tracked view aligned with the executed transfer
        // (`single_stream` forces one connection, no chunk plan).
        Ok(request.normalized())
    }
}

fn build_spec(index: usize, task: BatchTask) -> Result<TaskSpec, AgentErrorInfo> {
    let idempotency_key = task.idempotency_key.clone();
    let mut request = task
        .into_request()
        .map_err(|message| invalid_request(format!("task #{index}: {message}")))?;

    // URL syntax/scheme first (invalid_url/exit 2), then policy (exit 4).
    util::validate_http_url(&request.url).map_err(|error| error_with_task_index(error, index))?;

    if let Some(expected) = &request.expected_sha256 {
        super::download::validate_sha256(expected)
            .map_err(|message| invalid_request(format!("task #{index}: {message}")))?;
    }

    let dest_dir = request
        .output_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if request.output_dir.is_none() {
        request.output_dir = Some(dest_dir.clone());
    }
    let file_name = request
        .file_name
        .clone()
        .unwrap_or_else(|| file_name_from_url(&request.url));

    Ok(TaskSpec {
        index,
        request,
        dest_dir,
        file_name,
        idempotency_key,
    })
}

/// Prefix a validation error with the batch task index while keeping its
/// stable error code (`invalid_url` must not become policy_denied).
fn error_with_task_index(mut error: AgentErrorInfo, index: usize) -> AgentErrorInfo {
    error.message = format!("task #{index}: {}", error.message);
    error
}

fn file_name_from_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "download".to_string();
    };
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|segments| segments.collect())
        .unwrap_or_default();
    segments
        .iter()
        .rev()
        .find(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .unwrap_or_else(|| "download".to_string())
}

// ---------------------------------------------------------------------------
// One task
// ---------------------------------------------------------------------------

async fn run_one(
    spec: TaskSpec,
    store_root: &Path,
    policy: Option<&Policy>,
    semaphore: Arc<Semaphore>,
    abort: Arc<AtomicBool>,
    updates: mpsc::UnboundedSender<RunUpdate>,
) -> Result<TaskOutcome, AgentErrorInfo> {
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| AgentErrorInfo::new(ErrorCode::Internal, "concurrency limiter closed"))?;

    let mut tracker = TaskTracker::begin(
        store_root,
        "cli",
        spec.request.clone(),
        spec.dest_dir.clone(),
        spec.file_name.clone(),
        spec.idempotency_key.clone(),
    );
    let task_id = tracker.id().to_string();
    send_update(
        &updates,
        &task_id,
        "queued",
        json!({ "url": spec.request.url }),
    );

    // stop_on_error: tasks that never started are marked cancelled.
    if abort.load(Ordering::SeqCst) {
        tracker.apply_cancelled(0);
        send_update(
            &updates,
            &task_id,
            "cancelled",
            json!({ "reason": "stop_on_error" }),
        );
        return Ok(TaskOutcome {
            index: spec.index,
            view: tracker.view(),
            error: None,
            reused: false,
        });
    }

    // Idempotency replay.
    if let Some(key) = &spec.idempotency_key {
        let store = TaskStore::new(store_root);
        if let Ok(Some(existing)) = store.find_by_idempotency_key(key) {
            if existing.state == TaskState::Completed {
                let mut tracker = TaskTracker::adopt(store_root, existing);
                tracker.push_warning("idempotent_replay");
                tracker.persist(true);
                send_update(
                    &updates,
                    &task_id,
                    "completed",
                    json!({
                        "reused": true,
                        "path": tracker.record.path,
                        "connections": tracker.record.connections,
                        "peak_connections": tracker.record.peak_connections,
                    }),
                );
                return Ok(TaskOutcome {
                    index: spec.index,
                    view: tracker.view(),
                    error: None,
                    reused: true,
                });
            }
        }
    }

    // Policy pre-checks (only when an explicit policy.json is present).
    let mut unknown_length_monitor: Option<(rdm_agent::Policy, PathBuf)> = None;
    if let Some(policy) = policy {
        match policy_precheck(policy, &spec.request, &spec.dest_dir).await {
            Ok(monitor) => unknown_length_monitor = monitor,
            Err(info) => {
                tracker.apply_failed(&info);
                send_update(
                    &updates,
                    &task_id,
                    "failed",
                    json!({ "error": serde_json::to_value(&info).unwrap_or(Value::Null) }),
                );
                return Ok(TaskOutcome {
                    index: spec.index,
                    view: tracker.view(),
                    error: Some(info),
                    reused: false,
                });
            }
        }
    }

    // Execute the download.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let request = spec.request.clone();
    let handle = rdm_core::DownloadHandle::new();
    let download = rdm_core::download_with_handle(request, Some(event_tx), handle.clone());
    tokio::pin!(download);

    let mut policy_violation: Option<AgentErrorInfo> = None;
    let result;
    loop {
        tokio::select! {
            biased;
            outcome = &mut download => {
                result = outcome;
                break;
            }
            Some(event) = event_rx.recv() => {
                enforce_unknown_length_limit(
                    &event,
                    &unknown_length_monitor,
                    &mut policy_violation,
                    &handle,
                );
                handle_engine_event(&mut tracker, &updates, &task_id, &event);
            }
        }
    }
    while let Ok(event) = event_rx.try_recv() {
        enforce_unknown_length_limit(
            &event,
            &unknown_length_monitor,
            &mut policy_violation,
            &handle,
        );
        handle_engine_event(&mut tracker, &updates, &task_id, &event);
    }

    let mut error = match result {
        Ok(summary) => {
            tracker.apply_completed(&summary);
            send_update(
                &updates,
                &task_id,
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
            None
        }
        Err(error) => {
            let info = engine_error(&error);
            match &error {
                rdm_core::DownloadError::Paused { downloaded } => {
                    tracker.apply_paused(*downloaded);
                    send_update(
                        &updates,
                        &task_id,
                        "paused",
                        json!({ "downloaded": downloaded }),
                    );
                }
                rdm_core::DownloadError::Cancelled { downloaded } => {
                    tracker.apply_cancelled(*downloaded);
                    send_update(
                        &updates,
                        &task_id,
                        "cancelled",
                        json!({ "downloaded": downloaded }),
                    );
                }
                _ => {
                    tracker.apply_failed(&info);
                    send_update(
                        &updates,
                        &task_id,
                        "failed",
                        json!({ "error": serde_json::to_value(&info).unwrap_or(Value::Null) }),
                    );
                }
            }
            Some(info)
        }
    };
    if let Some(violation) = policy_violation.take() {
        tracker.apply_failed(&violation);
        send_update(
            &updates,
            &task_id,
            "failed",
            json!({ "error": serde_json::to_value(&violation).unwrap_or(Value::Null) }),
        );
        error = Some(violation);
    }
    tracker.persist(true);

    Ok(TaskOutcome {
        index: spec.index,
        view: tracker.view(),
        error,
        reused: false,
    })
}

/// Policy pre-checks shared with `rdm download` (URL, destination, probe-time
/// size gate, overwrite gate). Returns an unknown-length monitor when the
/// remote size is unavailable but `max_bytes_per_task` needs streaming
/// enforcement.
async fn policy_precheck(
    policy: &Policy,
    request: &DownloadRequest,
    dest_dir: &Path,
) -> Result<Option<(Policy, PathBuf)>, AgentErrorInfo> {
    util::policy_precheck(policy, &request.url, Some(dest_dir))?;

    // Size gate needs the remote size: probe first (cheap HEAD).
    let meta = rdm_core::probe(request)
        .await
        .map_err(|error| engine_error(&error))?;
    util::policy_gate_size(policy, meta.total_bytes)?;
    let target =
        util::candidate_target_path(dest_dir, request.file_name.as_deref(), &meta.file_name);
    util::policy_overwrite_gate(policy, &target)?;

    if meta.total_bytes.is_none() && policy.max_bytes_per_task.is_some() {
        Ok(Some((policy.clone(), target)))
    } else {
        Ok(None)
    }
}

/// Enforce `max_bytes_per_task` while streaming an unknown-length response.
fn enforce_unknown_length_limit(
    event: &DownloadEvent,
    monitor: &Option<(Policy, PathBuf)>,
    violation: &mut Option<AgentErrorInfo>,
    handle: &rdm_core::DownloadHandle,
) {
    if violation.is_some() {
        return;
    }
    let Some((policy, target)) = monitor else {
        return;
    };
    let DownloadEvent::Progress { downloaded, .. } = event else {
        return;
    };
    if let Some(max) = policy.max_bytes_per_task {
        if *downloaded > max {
            *violation = Some(util::policy_size_exceeded_error(
                policy,
                *downloaded,
                target,
            ));
            handle.pause();
        }
    }
}

fn handle_engine_event(
    tracker: &mut TaskTracker,
    updates: &mpsc::UnboundedSender<RunUpdate>,
    task_id: &str,
    event: &DownloadEvent,
) {
    match event {
        DownloadEvent::Probing => {}
        DownloadEvent::Metadata(meta) => {
            tracker.apply_metadata(meta);
            send_update(
                updates,
                task_id,
                "metadata",
                json!({
                    "final_url": meta.final_url,
                    "file_name": meta.file_name,
                    "total_bytes": meta.total_bytes,
                    "supports_ranges": meta.supports_ranges,
                }),
            );
        }
        DownloadEvent::Started {
            connections,
            chunk_size,
            total_chunks,
            resumed_chunks,
        } => {
            tracker.apply_started(*connections, *chunk_size);
            send_update(
                updates,
                task_id,
                "started",
                json!({
                    "connections": connections,
                    "chunk_size": chunk_size,
                    "total_chunks": total_chunks,
                    "resumed_chunks": resumed_chunks,
                }),
            );
        }
        DownloadEvent::Progress {
            downloaded,
            total,
            speed_bps,
            eta_secs,
            active_workers,
        } => {
            tracker.apply_progress(*downloaded, *total, *speed_bps, *eta_secs);
            send_update(
                updates,
                task_id,
                "progress",
                json!({
                    "downloaded": downloaded,
                    "total": total,
                    "speed_bps": speed_bps,
                    "eta_secs": eta_secs,
                    "connections": active_workers,
                }),
            );
        }
        DownloadEvent::Retrying {
            chunk,
            attempt,
            delay_ms,
            reason,
        } => {
            send_update(
                updates,
                task_id,
                "retry",
                json!({
                    "chunk": chunk,
                    "attempt": attempt,
                    "delay_ms": delay_ms,
                    "reason": reason,
                }),
            );
        }
        DownloadEvent::Concurrency { target, active } => {
            tracker.apply_concurrency(*target, *active);
            send_update(
                updates,
                task_id,
                "concurrency",
                json!({
                    "target": target,
                    "active": active,
                }),
            );
        }
        DownloadEvent::Completed { .. }
        | DownloadEvent::Failed { .. }
        | DownloadEvent::Paused { .. } => {}
    }
}

fn send_update(
    updates: &mpsc::UnboundedSender<RunUpdate>,
    task_id: &str,
    kind: &'static str,
    data: Value,
) {
    let _ = updates.send(RunUpdate {
        task_id: task_id.to_string(),
        kind,
        data,
    });
}

fn handle_update(reporter: &mut Reporter, update: RunUpdate) {
    if reporter.is_text() {
        match update.kind {
            "started" => {
                let connections = update.data["connections"].as_u64().unwrap_or(0);
                crate::outln!("[start] {}  {connections} connection(s)", update.task_id);
            }
            "failed" => {
                let message = update.data["error"]["message"].as_str().unwrap_or("failed");
                crate::outln!("[fail] {}  {message}", update.task_id);
            }
            _ => {}
        }
        return;
    }
    reporter.event_for(&update.task_id, update.kind, update.data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_file_name_from_url() {
        assert_eq!(
            file_name_from_url("https://example.com/files/My%20File.bin?token=x"),
            "My%20File.bin"
        );
        assert_eq!(file_name_from_url("https://example.com/"), "download");
        assert_eq!(file_name_from_url("not a url"), "download");
    }

    #[test]
    fn parses_minimal_batch_input() {
        let input: BatchInput = serde_json::from_str(
            r#"{"schema_version":"rdm.agent/v1","tasks":[{"url":"https://example.com/a.bin"}]}"#,
        )
        .unwrap();
        assert_eq!(input.tasks.len(), 1);
        assert_eq!(input.tasks[0].url, "https://example.com/a.bin");
        assert!(input.max_concurrency.is_none());
    }

    #[test]
    fn rejects_empty_task_list_later_but_parses() {
        let input: BatchInput = serde_json::from_str(r#"{"tasks":[]}"#).unwrap();
        assert!(input.tasks.is_empty());
    }

    #[test]
    fn build_spec_defaults_dest_and_name() {
        let spec = build_spec(
            0,
            BatchTask {
                url: "https://example.com/file.iso".to_string(),
                ..BatchTask::default()
            },
        )
        .unwrap();
        assert_eq!(spec.file_name, "file.iso");
        assert!(spec.request.output_dir.is_some());
    }

    #[test]
    fn build_spec_rejects_bad_connections() {
        let error = build_spec(
            3,
            BatchTask {
                url: "https://example.com/file.iso".to_string(),
                connections: Some(0),
                ..BatchTask::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("task #3"));
    }
}
