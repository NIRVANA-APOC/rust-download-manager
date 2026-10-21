//! `rdm download` — segmented download with a durable task record.
//!
//! Output modes:
//! * `text` — human progress line + final summary (legacy behaviour)
//! * `json` — one [`rdm_agent::Envelope`] whose `data` is the redacted task
//! * `ndjson` — probe/metadata/started/progress/retry/... events, then a final
//!   `result` event (same line shape as JSON mode).

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rdm_agent::{AgentErrorInfo, TaskState, TaskStore};
use rdm_core::{DownloadEvent, DownloadHandle, DownloadRequest, DownloadSummary};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::cli::{DownloadArgs, NetArgs};
use crate::output::{engine_error, invalid_request, OutputMode, Reporter};
use crate::tracker::TaskTracker;
use crate::util;

pub async fn run(mode: OutputMode, store_root: &Path, args: DownloadArgs) -> u8 {
    let quiet = args.quiet;
    let mut reporter = Reporter::new(mode, "download").quiet(quiet);

    let idempotency_key = args.idempotency_key.clone();
    let request = match args.into_request() {
        Ok(request) => request,
        Err(message) => return reporter.finish_err(invalid_request(message)),
    };
    // URL syntax/scheme first (invalid_url/exit 2), then policy (exit 4).
    if let Err(error) = util::validate_http_url(&request.url) {
        return reporter.finish_err(error);
    }
    if let Err(error) = super::validate_connections(request.connections) {
        return reporter.finish_err(error);
    }
    // Normalize before persisting/tracking: `single_stream` forces
    // connections=1 / dynamic=false / no chunk plan, and the TaskView must
    // describe the transfer that actually executes (contract §2.4).
    let request = request.normalized();

    // ---- Agent policy (only enforced when an explicit policy.json exists) ---
    let dest_dir = request
        .output_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let policy = match util::load_effective_policy(store_root) {
        Ok(policy) => policy,
        Err(error) => return reporter.finish_err(error),
    };
    let policy_warning = if policy.is_none() {
        Some(util::POLICY_ABSENT_WARNING)
    } else {
        None
    };
    // Unknown-length streams cannot be size-gated up-front: watch progress and
    // pause as soon as the download passes `max_bytes_per_task`.
    let mut unknown_length_monitor: Option<(rdm_agent::Policy, PathBuf)> = None;
    if let Some(policy) = &policy {
        if let Err(error) = util::policy_precheck(policy, &request.url, Some(&dest_dir)) {
            return reporter.finish_err(error);
        }
        // Probe once so size / overwrite gates run before any bytes hit disk.
        let meta = match rdm_core::probe(&request).await {
            Ok(meta) => meta,
            Err(error) => return reporter.finish_err(engine_error(&error)),
        };
        if let Err(error) = util::policy_gate_size(policy, meta.total_bytes) {
            return reporter.finish_err(error);
        }
        let target =
            util::candidate_target_path(&dest_dir, request.file_name.as_deref(), &meta.file_name);
        if let Err(error) = util::policy_overwrite_gate(policy, &target) {
            return reporter.finish_err(error);
        }
        if meta.total_bytes.is_none() && policy.max_bytes_per_task.is_some() {
            unknown_length_monitor = Some((policy.clone(), target));
        }
    }

    // Idempotency: reuse a completed task with the same key.
    if let Some(key) = &idempotency_key {
        let store = TaskStore::new(store_root);
        if let Ok(Some(existing)) = store.find_by_idempotency_key(key) {
            if existing.state == TaskState::Completed {
                let mut tracker = TaskTracker::adopt(store_root, existing);
                tracker.push_warning("idempotent_replay");
                reporter.set_task_id(tracker.id());
                if reporter.is_text() {
                    if let Some(result) = &tracker.record.result {
                        crate::outln!(
                            "already completed (idempotency key `{key}`): {}",
                            result.path
                        );
                    } else {
                        crate::outln!("already completed (idempotency key `{key}`)");
                    }
                }
                tracker.persist(true);
                let data = task_json(&tracker);
                return reporter.finish_ok(data);
            }
        }
    }

    let fallback_name = request
        .file_name
        .clone()
        .unwrap_or_else(|| "download".to_string());
    let mut tracker = TaskTracker::begin(
        store_root,
        "cli",
        request.clone(),
        dest_dir,
        fallback_name,
        idempotency_key,
    );
    reporter.set_task_id(tracker.id());
    let store_warning = tracker.take_warning();
    if let Some(warning) = policy_warning {
        tracker.push_warning(warning);
        if reporter.is_text() {
            crate::errln!("warning: {warning} (download continues)");
        }
    }

    let mut human = HumanOutput::new(quiet);
    if reporter.is_text() {
        if let Some(warning) = &store_warning {
            crate::errln!("warning: {warning} (download continues)");
        }
    }
    reporter.event("probe", json!({ "url": request.url }));

    let handle = DownloadHandle::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let download = rdm_core::download_with_handle(request, Some(tx), handle.clone());
    tokio::pin!(download);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut interrupted = false;
    let mut policy_violation: Option<AgentErrorInfo> = None;
    let result;
    loop {
        tokio::select! {
            biased;
            outcome = &mut download => {
                result = outcome;
                break;
            }
            Some(event) = rx.recv() => {
                observe_unknown_length(
                    &event,
                    &unknown_length_monitor,
                    &mut policy_violation,
                    &handle,
                );
                handle_event(&mut reporter, &mut tracker, &mut human, &event);
            }
            signal = &mut ctrl_c, if !interrupted => {
                match signal {
                    Ok(()) => {
                        interrupted = true;
                        handle.pause();
                        human.interrupted(reporter.is_text());
                    }
                    Err(error) => {
                        interrupted = true;
                        crate::errln!("rdm: could not listen for Ctrl-C: {error}");
                    }
                }
            }
        }
    }
    while let Ok(event) = rx.try_recv() {
        observe_unknown_length(
            &event,
            &unknown_length_monitor,
            &mut policy_violation,
            &handle,
        );
        handle_event(&mut reporter, &mut tracker, &mut human, &event);
    }

    let mut error_info: Option<AgentErrorInfo> = match &result {
        Ok(summary) => {
            tracker.apply_completed(summary);
            reporter.event("completed", summary_event_json(summary));
            if reporter.is_text() {
                human.completed(summary);
            }
            None
        }
        Err(error) => {
            let info = engine_error(error);
            match error {
                rdm_core::DownloadError::Paused { downloaded } => {
                    tracker.apply_paused(*downloaded);
                    reporter.event("paused", json!({ "downloaded": downloaded }));
                    if reporter.is_text() {
                        human.paused(*downloaded);
                    }
                }
                rdm_core::DownloadError::Cancelled { downloaded } => {
                    tracker.apply_cancelled(*downloaded);
                    reporter.event("cancelled", json!({ "downloaded": downloaded }));
                }
                _ => {
                    tracker.apply_failed(&info);
                    reporter.event(
                        "failed",
                        json!({ "error": serde_json::to_value(&info).unwrap_or(Value::Null) }),
                    );
                }
            }
            Some(info)
        }
    };
    // A streamed unknown-length response that crossed `max_bytes_per_task` was
    // paused by us; report it as a policy denial (exit 4), not a user pause.
    if let Some(violation) = policy_violation.take() {
        tracker.apply_failed(&violation);
        reporter.event(
            "failed",
            json!({ "error": serde_json::to_value(&violation).unwrap_or(Value::Null) }),
        );
        error_info = Some(violation);
    }
    tracker.persist(true);

    match error_info {
        None => {
            let data = task_json(&tracker);
            reporter.finish_ok(data)
        }
        Some(info) => reporter.finish_err(info),
    }
}

fn task_json(tracker: &TaskTracker) -> Value {
    let mut value = serde_json::to_value(tracker.view()).unwrap_or(Value::Null);
    if let Some(warning) = tracker.warning() {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "store_warning".to_string(),
                Value::String(warning.to_string()),
            );
        }
    }
    value
}

fn summary_event_json(summary: &DownloadSummary) -> Value {
    json!({
        "path": summary.path.display().to_string(),
        "size": summary.total_bytes,
        "total_bytes": summary.total_bytes,
        "sha256": summary.sha256,
        "elapsed_ms": summary.elapsed_ms,
        "avg_speed_bps": summary.avg_speed_bps,
        "connections": summary.connections,
        "peak_connections": summary.peak_connections,
        "resumed_chunks": summary.resumed_chunks,
        "range_supported": summary.range_supported,
        "http_version": summary.http_version,
    })
}

/// Enforce `max_bytes_per_task` for unknown-length streams by watching progress
/// and pausing the download as soon as the budget is crossed.
fn observe_unknown_length(
    event: &DownloadEvent,
    monitor: &Option<(rdm_agent::Policy, PathBuf)>,
    violation: &mut Option<AgentErrorInfo>,
    handle: &DownloadHandle,
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

fn metadata_event_json(meta: &rdm_core::RemoteMeta) -> Value {
    json!({
        "final_url": meta.final_url,
        "file_name": meta.file_name,
        "total_bytes": meta.total_bytes,
        "supports_ranges": meta.supports_ranges,
        "content_type": meta.content_type,
        "etag": meta.etag,
        "last_modified": meta.last_modified,
        "http_version": meta.http_version,
    })
}

fn handle_event(
    reporter: &mut Reporter,
    tracker: &mut TaskTracker,
    human: &mut HumanOutput,
    event: &DownloadEvent,
) {
    match event {
        DownloadEvent::Probing => {}
        DownloadEvent::Metadata(meta) => {
            tracker.apply_metadata(meta);
            reporter.event("metadata", metadata_event_json(meta));
            human.metadata(meta, reporter.is_text());
        }
        DownloadEvent::Started {
            connections,
            chunk_size,
            total_chunks,
            resumed_chunks,
        } => {
            tracker.apply_started(*connections, *chunk_size);
            reporter.event(
                "started",
                json!({
                    "connections": connections,
                    "chunk_size": chunk_size,
                    "total_chunks": total_chunks,
                    "resumed_chunks": resumed_chunks,
                }),
            );
            human.started(
                *connections,
                *chunk_size,
                *total_chunks,
                *resumed_chunks,
                reporter.is_text(),
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
            reporter.progress(*downloaded, *total, *speed_bps, *eta_secs, *active_workers);
            human.progress(
                *downloaded,
                *total,
                *speed_bps,
                *eta_secs,
                reporter.is_text(),
            );
        }
        DownloadEvent::Retrying {
            chunk,
            attempt,
            delay_ms,
            reason,
        } => {
            reporter.event(
                "retry",
                json!({
                    "chunk": chunk,
                    "attempt": attempt,
                    "delay_ms": delay_ms,
                    "reason": reason,
                }),
            );
            human.retry(*chunk, *attempt, *delay_ms, reason, reporter.is_text());
        }
        DownloadEvent::Concurrency { target, active } => {
            tracker.apply_concurrency(*target, *active);
            reporter.event(
                "concurrency",
                json!({
                    "target": target,
                    "active": active,
                }),
            );
        }
        DownloadEvent::Completed { .. }
        | DownloadEvent::Failed { .. }
        | DownloadEvent::Paused { .. } => {
            // Rendered from the final result to avoid duplicate output.
        }
    }
}

// ---------------------------------------------------------------------------
// Human progress rendering (text mode only)
// ---------------------------------------------------------------------------

struct HumanOutput {
    quiet: bool,
    tty: bool,
    progress_visible: bool,
    progress_width: usize,
    ticks: u64,
}

impl HumanOutput {
    fn new(quiet: bool) -> Self {
        Self {
            quiet,
            tty: std::io::stdout().is_terminal(),
            progress_visible: false,
            progress_width: 0,
            ticks: 0,
        }
    }

    fn enabled(&self, is_text: bool) -> bool {
        is_text && !self.quiet
    }

    fn metadata(&mut self, meta: &rdm_core::RemoteMeta, is_text: bool) {
        if !self.enabled(is_text) {
            return;
        }
        crate::outln!(
            "file: {}  size: {}  ranges: {}",
            meta.file_name,
            meta.total_bytes
                .map(util::format_bytes)
                .unwrap_or_else(|| "unknown".to_string()),
            if meta.supports_ranges { "yes" } else { "no" },
        );
    }

    fn started(
        &mut self,
        connections: usize,
        chunk_size: u64,
        total_chunks: u64,
        resumed_chunks: u64,
        is_text: bool,
    ) {
        if !self.enabled(is_text) {
            return;
        }
        if resumed_chunks > 0 {
            crate::outln!(
                "resuming: {resumed_chunks}/{total_chunks} chunks already complete, \
                 {connections} connection(s), chunk size {}",
                util::format_bytes(chunk_size)
            );
        } else {
            crate::outln!(
                "starting: {connections} connection(s), {total_chunks} chunks of {}",
                util::format_bytes(chunk_size)
            );
        }
    }

    fn progress(
        &mut self,
        downloaded: u64,
        total: Option<u64>,
        speed: f64,
        eta: Option<f64>,
        is_text: bool,
    ) {
        if !self.enabled(is_text) {
            return;
        }
        self.ticks += 1;
        let percent = match total {
            Some(total) if total > 0 => {
                format!("{:5.1}%", downloaded as f64 * 100.0 / total as f64)
            }
            _ => "  ?.?%".to_string(),
        };
        let line = format!(
            "{percent}  {} / {}  {}  ETA {}",
            util::format_bytes(downloaded),
            total
                .map(util::format_bytes)
                .unwrap_or_else(|| "?".to_string()),
            util::format_speed(speed),
            util::format_eta(eta),
        );
        if self.tty {
            crate::outprint!("\r{line}");
            let _ = std::io::stdout().flush();
            self.progress_width = line.chars().count();
            self.progress_visible = true;
        } else if self.ticks.is_multiple_of(4) {
            crate::outln!("{line}");
        }
    }

    fn retry(&mut self, chunk: u64, attempt: u32, delay_ms: u64, reason: &str, is_text: bool) {
        if !self.enabled(is_text) {
            return;
        }
        self.clear();
        crate::errln!("retrying chunk {chunk} (attempt {attempt}) in {delay_ms} ms: {reason}");
    }

    fn interrupted(&mut self, is_text: bool) {
        if !is_text {
            return;
        }
        self.clear();
        crate::errln!("interrupt received: pausing and saving resume state...");
    }

    fn completed(&mut self, summary: &DownloadSummary) {
        self.clear();
        crate::outln!(
            "saved {} ({} in {:.2}s, {}, {} connection(s){})",
            summary.path.display(),
            util::format_bytes(summary.total_bytes),
            summary.elapsed_ms as f64 / 1000.0,
            util::format_speed(summary.avg_speed_bps),
            summary.connections,
            if summary.range_supported {
                ""
            } else {
                ", single stream (server has no range support)"
            },
        );
    }

    fn paused(&mut self, downloaded: u64) {
        self.clear();
        crate::outln!(
            "paused at {} — run the same command again to resume",
            util::format_bytes(downloaded)
        );
    }

    fn clear(&mut self) {
        if self.progress_visible {
            crate::outprint!("\r{}\r", " ".repeat(self.progress_width));
            let _ = std::io::stdout().flush();
            self.progress_visible = false;
            self.progress_width = 0;
        }
    }
}

impl std::fmt::Debug for HumanOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HumanOutput")
            .field("quiet", &self.quiet)
            .field("tty", &self.tty)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

impl DownloadArgs {
    pub fn into_request(self) -> Result<DownloadRequest, String> {
        let DownloadArgs {
            url,
            dir,
            name,
            connections,
            chunk_size,
            limit,
            sha256,
            single_stream,
            idempotency_key: _,
            quiet: _,
            net,
        } = self;
        let mut request = request_from_parts(url, net)?;
        request.output_dir = dir;
        request.file_name = name;
        request.connections = connections;
        request.chunk_size = chunk_size;
        request.speed_limit_bps = limit;
        request.single_stream = single_stream;
        if let Some(expected) = sha256 {
            validate_sha256(&expected)?;
            request.expected_sha256 = Some(expected);
        }
        Ok(request)
    }
}

/// Build the engine request from URL + shared network options (also used by
/// `probe` and `run`).
pub fn request_from_parts(url: String, net: NetArgs) -> Result<DownloadRequest, String> {
    let mut request = DownloadRequest::new(url);
    request.cookie = net.cookie;
    request.referer = net.referer;
    request.user_agent = net.user_agent;
    request.proxy = net.proxy;
    request.insecure = net.insecure;
    let mut headers = BTreeMap::new();
    for raw in net.headers {
        let Some((name, value)) = raw.split_once(':') else {
            return Err(format!("invalid --header `{raw}`: expected `Name: Value`"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("invalid --header `{raw}`: empty header name"));
        }
        headers.insert(name.to_string(), value.trim().to_string());
    }
    request.headers = headers;
    Ok(request)
}

pub fn validate_sha256(value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid --sha256 `{value}`: expected 64 hexadecimal characters"
        ));
    }
    Ok(())
}

/// Validate a `connections` value coming from a raw JSON batch.
pub fn normalize_connections(connections: Option<usize>) -> Result<usize, String> {
    let connections = connections.unwrap_or(rdm_core::DEFAULT_CONNECTIONS);
    if connections == 0 || connections > rdm_core::MAX_CONNECTIONS {
        return Err(format!(
            "connections must be between 1 and {} (got {connections})",
            rdm_core::MAX_CONNECTIONS
        ));
    }
    Ok(connections)
}

/// Time since an instant, saturating at u64 milliseconds (shared by batch
/// commands that report `duration_ms`).
pub fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_sha256() {
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256("abc").is_err());
        assert!(validate_sha256(&"z".repeat(64)).is_err());
    }

    #[test]
    fn normalizes_connections() {
        assert_eq!(
            normalize_connections(None).unwrap(),
            rdm_core::DEFAULT_CONNECTIONS
        );
        assert_eq!(normalize_connections(Some(1)).unwrap(), 1);
        assert_eq!(normalize_connections(Some(32)).unwrap(), 32);
        assert!(normalize_connections(Some(0)).is_err());
        assert!(normalize_connections(Some(33)).is_err());
    }

    #[test]
    fn parses_headers_and_rejects_bad_ones() {
        let net = NetArgs {
            headers: vec!["Accept-Language: zh-CN".to_string()],
            ..NetArgs::default()
        };
        let request = request_from_parts("https://example.com/a.bin".to_string(), net).unwrap();
        assert_eq!(
            request.headers.get("Accept-Language").map(String::as_str),
            Some("zh-CN")
        );

        let net = NetArgs {
            headers: vec!["broken".to_string()],
            ..NetArgs::default()
        };
        assert!(request_from_parts("https://example.com/a.bin".to_string(), net).is_err());
    }
}
