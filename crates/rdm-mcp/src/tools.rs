//! The 16 MCP tool implementations (contract §3.1).
//!
//! Every tool returns the same envelope in `structuredContent`:
//! `{schema_version, ok, task_id, data, error}` and sets `isError` on failure.
//! Tool failures are *not* JSON-RPC errors — they carry the stable
//! [`rdm_agent::AgentErrorInfo`] taxonomy so agents can branch on
//! `error.code`/`error.retryable`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rdm_agent::{
    capabilities_for, redact_url, AgentErrorInfo, DownloadPlan, ErrorCode, TaskEvent, TaskRecord,
    TaskState, SCHEMA_VERSION,
};
use rdm_core::{DownloadRequest, RemoteMeta};
use serde_json::{json, Map, Value};

use crate::server::ServerState;
use crate::util;

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Call context / results
// ---------------------------------------------------------------------------

/// Everything a tool needs in addition to its arguments: the server state,
/// the JSON-RPC call id, the optional MCP progress token, and the cancellation
/// flag for *this wait* (never for the task).
pub struct CallContext {
    pub server: Arc<ServerState>,
    pub call_id: Value,
    pub progress_token: Option<Value>,
    pub wait_cancel: Arc<AtomicBool>,
}

impl CallContext {
    pub fn wait_cancelled(&self) -> bool {
        self.wait_cancel.load(Ordering::SeqCst)
    }

    /// Emit `notifications/progress` (only when the client supplied a token).
    pub fn progress(&self, progress: u64, total: Option<u64>, message: &str) {
        let Some(token) = &self.progress_token else {
            return;
        };
        let mut params = json!({
            "progressToken": token,
            "progress": progress,
            "message": rdm_agent::redact_text(message),
        });
        if let Some(total) = total {
            params["total"] = json!(total);
        }
        self.server.writer.notify("notifications/progress", params);
    }

    pub fn progress_for(&self, record: &TaskRecord) {
        if self.progress_token.is_none() {
            return;
        }
        let view = record.view();
        let message = format!(
            "{} | {}",
            util::progress_message(&view),
            record
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        );
        self.progress(record.downloaded_bytes, record.total_bytes, &message);
    }

    pub fn log(&self, level: &str, message: &str) {
        self.server.writer.notify(
            "notifications/message",
            json!({
                "level": level,
                "logger": "rdm-mcp",
                "data": rdm_agent::redact_text(message),
            }),
        );
    }
}

#[derive(Debug)]
pub(crate) struct ToolOk {
    pub(crate) data: Value,
    pub(crate) task_id: Option<String>,
    pub(crate) summary: String,
}

impl ToolOk {
    pub(crate) fn new(data: Value, summary: impl Into<String>) -> Self {
        Self {
            data,
            task_id: None,
            summary: summary.into(),
        }
    }

    fn with_task(mut self, record: &TaskRecord) -> Self {
        self.task_id = Some(record.id.clone());
        self
    }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

fn structured_success(ok: &ToolOk) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "ok": true,
        "task_id": ok.task_id,
        "data": ok.data,
        "error": null,
    })
}

fn structured_error(error: &AgentErrorInfo, task_id: Option<&str>) -> Value {
    let task_id = task_id
        .map(str::to_owned)
        .or_else(|| {
            error
                .details
                .as_ref()
                .and_then(|details| details.get("task_id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let task_id = if task_id.is_empty() {
        Value::Null
    } else {
        Value::String(task_id)
    };
    json!({
        "schema_version": SCHEMA_VERSION,
        "ok": false,
        "task_id": task_id,
        "data": null,
        "error": error,
    })
}

fn text_result(text: String, structured: Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": is_error,
    })
}

/// Render the error text for `content[0].text`.
fn error_text(error: &AgentErrorInfo) -> String {
    let mut text = format!("[{}] {}", error.code.as_str(), error.message);
    if let Some(hint) = &error.hint {
        text.push_str("\nhint: ");
        text.push_str(hint);
    }
    if let Some(details) = &error.details {
        if let Ok(serialized) = serde_json::to_string(details) {
            if serialized.len() <= 1200 {
                text.push_str("\ndetails: ");
                text.push_str(&serialized);
            }
        }
    }
    text
}

/// Entry point used by the server for `tools/call`.
pub async fn call_tool(ctx: &CallContext, name: &str, arguments: &Value) -> Value {
    let result = match util::args_object(arguments) {
        Ok(args) => match util::reject_unknown_args(name, &args) {
            Ok(()) => dispatch(ctx, name, &args).await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    match result {
        Ok(ok) => {
            ctx.log("debug", &format!("tool {name} ok"));
            text_result(ok.summary.clone(), structured_success(&ok), false)
        }
        Err(error) => {
            let error = util::redact_error(error);
            ctx.log(
                "info",
                &format!(
                    "tool {name} failed: [{}] {}",
                    error.code.as_str(),
                    error.message
                ),
            );
            text_result(error_text(&error), structured_error(&error, None), true)
        }
    }
}

async fn dispatch(
    ctx: &CallContext,
    name: &str,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    match name {
        "rdm_capabilities" => capabilities(ctx),
        "rdm_probe" => probe(ctx, args).await,
        "rdm_download" => download(ctx, args).await,
        "rdm_plan_download" => plan_download(ctx, args).await,
        "rdm_apply_plan" => apply_plan(ctx, args).await,
        "rdm_list_tasks" => list_tasks(ctx, args),
        "rdm_get_task" => get_task(ctx, args),
        "rdm_wait_task" => wait_task(ctx, args).await,
        "rdm_pause_task" => pause_task(ctx, args).await,
        "rdm_resume_task" => resume_task(ctx, args),
        "rdm_cancel_task" => cancel_task(ctx, args).await,
        "rdm_verify_file" => verify_file(ctx, args),
        "rdm_doctor" => crate::diagnostics::doctor(ctx, args).await,
        "rdm_selftest" => crate::diagnostics::selftest(ctx, args).await,
        "rdm_bench" => crate::diagnostics::bench(ctx, args).await,
        "rdm_state_maintenance" => crate::diagnostics::state_maintenance(ctx, args),
        _ => Err(util::invalid_request(format!("unknown tool: {name}"))),
    }
}

// ---------------------------------------------------------------------------
// Shared download preparation
// ---------------------------------------------------------------------------

// The enum is a short-lived local value (never stored in a collection), so the
// variant size difference is irrelevant; boxing would only add indirection.
#[allow(clippy::large_enum_variant)]
enum Prepared {
    /// Idempotency key matched an existing task: no new transfer.
    Existing(TaskRecord),
    Ready(ReadyDownload),
    /// Policy requires the two-step approval flow.
    NeedsApproval {
        plan: DownloadPlan,
        token: String,
    },
}

struct ReadyDownload {
    request: DownloadRequest,
    dest_dir: PathBuf,
    file_name: String,
    total_bytes: Option<u64>,
    range_supported: bool,
    http_version: Option<String>,
    warnings: Vec<String>,
    /// Preserved so the task manager can deduplicate near-simultaneous calls
    /// with the same key at creation time.
    idempotency_key: Option<String>,
}

fn probe_error(error: rdm_core::DownloadError) -> AgentErrorInfo {
    util::redact_error(AgentErrorInfo::from_download_error(&error))
}

async fn probe_with_timeout(request: &DownloadRequest) -> Result<RemoteMeta, AgentErrorInfo> {
    match tokio::time::timeout(PROBE_TIMEOUT, rdm_core::probe(request)).await {
        Ok(Ok(meta)) => Ok(meta),
        Ok(Err(error)) => Err(probe_error(error)),
        Err(_) => Err(util::timeout(
            "probing the remote URL timed out after 60 s",
            json!({ "url": request.url }),
        )),
    }
}

/// Re-check the *redirected* final URL against the policy (contract §4).
pub fn check_final_url(policy: &rdm_agent::Policy, final_url: &str) -> Result<(), AgentErrorInfo> {
    if !final_url.starts_with("http://") && !final_url.starts_with("https://") {
        return Err(util::policy_denied(
            format!("redirected to a non-http(s) URL: {final_url}"),
            json!({ "stage": "final_url", "final_url": redact_url(final_url) }),
        ));
    }
    let Some(host) = util::url_host(final_url) else {
        return Err(util::policy_denied(
            "the redirected final URL has no host",
            json!({ "stage": "final_url" }),
        ));
    };
    if !policy.allow_private_hosts && rdm_agent::is_private_host(&host) {
        return Err(util::policy_denied(
            format!("redirected to private/metadata host `{host}`"),
            json!({
                "stage": "final_url",
                "host": host,
                "allow_private_hosts": false,
            }),
        ));
    }
    if !policy.check_host(&host) {
        return Err(util::policy_denied(
            format!("redirected to host `{host}` which is not in policy.allowed_hosts"),
            json!({
                "stage": "final_url",
                "host": host,
                "allowed_hosts": policy.allowed_hosts,
            }),
        ));
    }
    Ok(())
}

/// Resolve the base target and enforce `allow_overwrite`/resume rules.
fn resolve_target_conflict(
    policy: &rdm_agent::Policy,
    dest_dir: &Path,
    file_name: &str,
    resume: bool,
) -> Result<(PathBuf, Vec<String>), AgentErrorInfo> {
    let target = util::expected_target(dest_dir, file_name);
    let (part, state) = util::part_paths(&target);
    let mut warnings = Vec::new();
    if target.exists() {
        if policy.allow_overwrite {
            util::remove_target_and_partials(&target).map_err(|error| {
                AgentErrorInfo::new(
                    ErrorCode::PermissionDenied,
                    format!("cannot overwrite {}: {error}", target.display()),
                )
            })?;
            warnings.push("overwrote_existing_target".to_string());
        } else {
            return Err(util::conflict(format!(
                "target file already exists: {}",
                target.display()
            ))
            .with_details(json!({
                "path": target.display().to_string(),
                "allow_overwrite": false,
                "hint": "choose a different file_name/dest_dir, or set policy.allow_overwrite=true",
            })));
        }
    } else if resume && (part.exists() || state.exists()) {
        warnings.push("resumed_partial_target".to_string());
    } else if !resume && (part.exists() || state.exists()) {
        return Err(util::conflict(format!(
            "partial download state exists for {} but resume=false",
            target.display()
        ))
        .with_details(json!({
            "path": target.display().to_string(),
            "hint": "set resume=true to continue the partial download, or delete the .part files",
        })));
    }
    Ok((target, warnings))
}

/// Probe + policy + conflict check shared by `rdm_download` / `rdm_plan_download`.
async fn prepare_download(
    ctx: &CallContext,
    args: &Map<String, Value>,
    honor_idempotency: bool,
) -> Result<Prepared, AgentErrorInfo> {
    let policy = ctx.server.policy.clone();
    let manager = &ctx.server.manager;
    let idempotency_key = util::optional_str(args, "idempotency_key")?;
    if honor_idempotency {
        if let Some(key) = idempotency_key.as_deref() {
            if let Some(existing) = manager.find_by_idempotency(key)? {
                return Ok(Prepared::Existing(existing));
            }
        }
    }
    let mut request = util::build_request(args, &policy, true)?;
    let dest_dir = util::resolve_dest_dir(args, &policy)?;
    util::check_download_policy(&policy, &request, &dest_dir)?;
    let meta = probe_with_timeout(&request).await?;
    check_final_url(&policy, &meta.final_url)?;
    let requested_name = request
        .file_name
        .clone()
        .unwrap_or_else(|| meta.file_name.clone());
    let file_name = util::sanitize_filename(&requested_name);
    let (target, mut warnings) =
        resolve_target_conflict(&policy, &dest_dir, &file_name, request.resume)?;
    request.output_dir = Some(dest_dir.clone());
    request.file_name = Some(file_name.clone());

    match policy.gate(meta.total_bytes) {
        rdm_agent::PolicyDecision::Deny { reason } => Err(util::policy_denied(
            format!("file rejected by policy: {reason}"),
            json!({
                "stage": "size",
                "total_bytes": meta.total_bytes,
                "max_bytes_per_task": policy.max_bytes_per_task,
            }),
        )),
        rdm_agent::PolicyDecision::NeedsApproval {
            reason,
            total_bytes,
        } => {
            let plan = DownloadPlan::new(
                request,
                total_bytes,
                true,
                Some(reason),
                idempotency_key,
                policy.allow_overwrite,
                "mcp",
            );
            manager.save_plan(&plan)?;
            let token = ctx.server.issue_approval(&plan);
            Ok(Prepared::NeedsApproval { plan, token })
        }
        rdm_agent::PolicyDecision::Allow => {
            warnings.extend(conflict_warning(&target));
            Ok(Prepared::Ready(ReadyDownload {
                request,
                dest_dir,
                file_name,
                total_bytes: meta.total_bytes,
                range_supported: meta.supports_ranges,
                http_version: meta.http_version.clone(),
                warnings,
                idempotency_key,
            }))
        }
    }
}

fn conflict_warning(target: &Path) -> Vec<String> {
    let (part, state) = util::part_paths(target);
    let mut warnings = Vec::new();
    if part.exists() || state.exists() {
        warnings.push("resumed_partial_target".to_string());
    }
    warnings
}

fn start_ready_task(ctx: &CallContext, ready: ReadyDownload) -> Result<TaskRecord, AgentErrorInfo> {
    let warnings = ready.warnings;
    ctx.server.manager.create_task(
        "mcp",
        ready.request,
        ready.dest_dir,
        ready.file_name,
        ready.total_bytes,
        Some(ready.range_supported),
        ready.http_version,
        ready.idempotency_key,
        warnings,
    )
}

fn task_data(record: &TaskRecord, events: Option<Vec<TaskEvent>>) -> Value {
    let mut data = json!({ "task": record.view() });
    if let Some(events) = events {
        data["events"] = serde_json::to_value(events).unwrap_or(Value::Array(Vec::new()));
    }
    data
}

fn with_task_id(mut error: AgentErrorInfo, task_id: &str) -> AgentErrorInfo {
    match &mut error.details {
        Some(Value::Object(map)) => {
            map.entry("task_id")
                .or_insert_with(|| Value::String(task_id.to_string()));
        }
        _ => {
            error.details = Some(json!({ "task_id": task_id }));
        }
    }
    error
}

/// Wait for a freshly-created task (or return it immediately).
async fn finish_task(
    ctx: &CallContext,
    record: TaskRecord,
    wait: bool,
    wait_timeout_ms: u64,
    include_events: bool,
) -> Result<ToolOk, AgentErrorInfo> {
    if !wait {
        ctx.log(
            "info",
            &format!("task {} queued for {}", record.id, record.file_name),
        );
        let data = task_data(
            &record,
            if include_events {
                Some(Vec::new())
            } else {
                None
            },
        );
        return Ok(ToolOk::new(data, util::task_summary_text(&record.view())).with_task(&record));
    }
    ctx.log(
        "info",
        &format!(
            "waiting for task {} (timeout {} ms)",
            record.id, wait_timeout_ms
        ),
    );
    let manager = &ctx.server.manager;
    let wait_cancel = Arc::clone(&ctx.wait_cancel);
    let settled = manager
        .wait(&record.id, wait_timeout_ms, Some(&wait_cancel), |current| {
            ctx.progress_for(current);
        })
        .await
        .map_err(|error| with_task_id(error, &record.id))?;
    // Final progress line even when the task settled before the first poll.
    ctx.progress_for(&settled);
    let events = if include_events {
        Some(
            manager
                .store()
                .events(&settled.id, 50)
                .map_err(|error| util::internal(format!("cannot read events: {error}")))?,
        )
    } else {
        None
    };
    let data = task_data(&settled, events);
    Ok(ToolOk::new(data, util::task_summary_text(&settled.view())).with_task(&settled))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn capabilities(ctx: &CallContext) -> Result<ToolOk, AgentErrorInfo> {
    let capabilities = capabilities_for(&ctx.server.policy);
    let summary = format!(
        "RDM {} — 16 tools; {} concurrent tasks; allowed_dirs={}; allow_private_hosts={}",
        env!("CARGO_PKG_VERSION"),
        ctx.server.policy.max_concurrent_tasks,
        ctx.server
            .policy
            .summary()
            .get("allowed_dirs")
            .and_then(Value::as_array)
            .map(|dirs| dirs.len())
            .unwrap_or(0),
        ctx.server.policy.allow_private_hosts,
    );
    Ok(ToolOk::new(capabilities, summary))
}

async fn probe(ctx: &CallContext, args: &Map<String, Value>) -> Result<ToolOk, AgentErrorInfo> {
    let policy = &ctx.server.policy;
    let request = util::build_request(args, policy, false)?;
    util::check_policy_url(policy, &request.url)?;
    let meta = probe_with_timeout(&request).await?;
    // Report (not enforce) the final-host verdict for probes: the metadata may
    // still be useful, but the agent must know a redirect crossed the policy.
    let final_allowed = check_final_url(policy, &meta.final_url).is_ok();
    let gate = policy.gate(meta.total_bytes);
    let requires_approval = matches!(gate, rdm_agent::PolicyDecision::NeedsApproval { .. });
    let mut warnings: Vec<String> = Vec::new();
    if !final_allowed {
        warnings.push("final_url_outside_policy".to_string());
    }
    if !meta.supports_ranges {
        warnings.push("range_unsupported".to_string());
    }
    if meta.total_bytes.is_none() {
        warnings.push("no_content_length".to_string());
    }
    let data = json!({
        "url": redact_url(&request.url),
        "final_url": redact_url(&meta.final_url),
        "file_name": util::sanitize_filename(&meta.file_name),
        "total_bytes": meta.total_bytes,
        "range_supported": meta.supports_ranges,
        "content_type": meta.content_type,
        "etag": meta.etag,
        "last_modified": meta.last_modified,
        "http_version": meta.http_version,
        "policy": {
            "host_allowed": policy.check_url(&request.url).is_allowed(),
            "final_url_allowed": final_allowed,
            "requires_approval": requires_approval,
            "max_bytes_per_task": policy.max_bytes_per_task,
            "approval_threshold_bytes": policy.require_approval_over_bytes,
            "allowed_dirs": policy.summary().get("allowed_dirs").cloned().unwrap_or(Value::Null),
        },
        "warnings": warnings,
    });
    let summary = format!(
        "{} — {}; ranges={}; {}",
        util::sanitize_filename(&meta.file_name),
        meta.total_bytes
            .map(util::format_size)
            .unwrap_or_else(|| "size unknown".to_string()),
        if meta.supports_ranges { "yes" } else { "no" },
        meta.http_version.as_deref().unwrap_or("HTTP/?"),
    );
    Ok(ToolOk::new(data, summary))
}

async fn download(ctx: &CallContext, args: &Map<String, Value>) -> Result<ToolOk, AgentErrorInfo> {
    let wait = util::optional_bool(args, "wait", false)?;
    let wait_timeout_ms = util::wait_timeout_from_args(args)?;
    match prepare_download(ctx, args, true).await? {
        Prepared::Existing(existing) => {
            ctx.log(
                "info",
                &format!(
                    "idempotency key matched existing task {} — not downloading again",
                    existing.id
                ),
            );
            finish_task(ctx, existing, wait, wait_timeout_ms, false).await
        }
        Prepared::NeedsApproval { plan, token } => Err(util::needs_approval(
            format!(
                "file requires approval: {}",
                plan.reason.as_deref().unwrap_or("size over threshold")
            ),
            json!({
                "plan": plan.summary(),
                "approval_token": token,
                "approval_required": true,
                "next": "call rdm_apply_plan with plan_id and approval_token",
            }),
        )),
        Prepared::Ready(ready) => {
            let record = start_ready_task(ctx, ready)?;
            finish_task(ctx, record, wait, wait_timeout_ms, false).await
        }
    }
}

async fn plan_download(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    match prepare_download(ctx, args, false).await? {
        Prepared::Existing(existing) => Err(util::internal(format!(
            "unexpected idempotency match for task {}",
            existing.id
        ))),
        Prepared::Ready(ready) => {
            let plan = DownloadPlan::new(
                ready.request,
                ready.total_bytes,
                false,
                None,
                util::optional_str(args, "idempotency_key")?,
                ctx.server.policy.allow_overwrite,
                "mcp",
            );
            ctx.server.manager.save_plan(&plan)?;
            let data = json!({
                "plan": plan.summary(),
                "approval_token": Value::Null,
                "approval_required": false,
            });
            let summary = format!(
                "plan {}: {} -> {}\\{} ({}), no approval needed",
                plan.plan_id,
                plan.summary()
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("<url>"),
                plan.request
                    .output_dir
                    .as_ref()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default(),
                plan.request.file_name.as_deref().unwrap_or("?"),
                plan.total_bytes
                    .map(util::format_size)
                    .unwrap_or_else(|| "size unknown".to_string()),
            );
            Ok(ToolOk::new(data, summary))
        }
        Prepared::NeedsApproval { plan, token } => {
            let data = json!({
                "plan": plan.summary(),
                "approval_token": token,
                "approval_required": true,
            });
            let summary = format!(
                "plan {} requires approval: {}. Call rdm_apply_plan with this token.",
                plan.plan_id,
                plan.reason.as_deref().unwrap_or("size over threshold")
            );
            Ok(ToolOk::new(data, summary))
        }
    }
}

async fn apply_plan(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    let plan_id = util::required_str(args, "plan_id")?;
    let token = util::optional_str(args, "approval_token")?;
    let wait = util::optional_bool(args, "wait", false)?;
    let wait_timeout_ms = util::wait_timeout_from_args(args)?;
    let manager = &ctx.server.manager;
    let mut plan = manager.take_plan(&plan_id)?;
    if plan.is_expired() {
        manager.delete_plan(&plan_id);
        return Err(util::not_found(format!(
            "plan {plan_id} expired (plans live for 10 minutes); create a new plan"
        )));
    }
    if let Some(key) = util::optional_str(args, "idempotency_key")?.or(plan.idempotency_key.clone())
    {
        if let Some(existing) = manager.find_by_idempotency(&key)? {
            manager.delete_plan(&plan_id);
            return finish_task(ctx, existing, wait, wait_timeout_ms, false).await;
        }
        plan.idempotency_key = Some(key);
    }
    if plan.requires_approval {
        let token = token.ok_or_else(|| {
            util::needs_approval(
                format!("plan {plan_id} requires an approval_token"),
                json!({ "plan": plan.summary() }),
            )
        })?;
        ctx.server.consume_approval(&plan_id, &token)?;
    }

    // Re-validate against the *current* policy and remote state before starting.
    let policy = ctx.server.policy.clone();
    let dest_dir = plan
        .request
        .output_dir
        .clone()
        .ok_or_else(|| util::internal("plan has no destination directory"))?;
    util::check_download_policy(&policy, &plan.request, &dest_dir)?;
    let meta = probe_with_timeout(&plan.request).await?;
    check_final_url(&policy, &meta.final_url)?;
    match policy.gate(meta.total_bytes) {
        rdm_agent::PolicyDecision::Deny { reason } => {
            manager.delete_plan(&plan_id);
            return Err(util::policy_denied(
                format!("file is no longer allowed by policy: {reason}"),
                json!({
                    "stage": "size",
                    "total_bytes": meta.total_bytes,
                    "max_bytes_per_task": policy.max_bytes_per_task,
                }),
            ));
        }
        rdm_agent::PolicyDecision::NeedsApproval {
            reason,
            total_bytes,
        } => {
            if !plan.requires_approval {
                // Size grew past the threshold after planning: issue a new plan.
                let file_name = util::sanitize_filename(
                    plan.request.file_name.as_deref().unwrap_or(&meta.file_name),
                );
                let mut request = plan.request.clone();
                request.file_name = Some(file_name.clone());
                let new_plan = DownloadPlan::new(
                    request,
                    total_bytes,
                    true,
                    Some(reason),
                    plan.idempotency_key.clone(),
                    policy.allow_overwrite,
                    "mcp",
                );
                manager.delete_plan(&plan_id);
                manager.save_plan(&new_plan)?;
                let token = ctx.server.issue_approval(&new_plan);
                return Err(util::needs_approval(
                    "file size changed and now requires approval",
                    json!({ "plan": new_plan.summary(), "approval_token": token }),
                ));
            }
        }
        rdm_agent::PolicyDecision::Allow => {}
    }
    let plan_file_name = plan
        .request
        .file_name
        .clone()
        .unwrap_or_else(|| meta.file_name.clone());
    let file_name = util::sanitize_filename(&plan_file_name);
    let (_target, warnings) =
        resolve_target_conflict(&policy, &dest_dir, &file_name, plan.request.resume)?;
    let mut request = plan.request.clone();
    request.file_name = Some(file_name.clone());
    request.output_dir = Some(dest_dir.clone());
    let record = manager.create_task(
        "mcp",
        request,
        dest_dir,
        file_name,
        meta.total_bytes,
        Some(meta.supports_ranges),
        meta.http_version.clone(),
        plan.idempotency_key.clone(),
        warnings,
    )?;
    manager.delete_plan(&plan_id);
    ctx.log(
        "info",
        &format!("applied plan {plan_id} as task {}", record.id),
    );
    finish_task(ctx, record, wait, wait_timeout_ms, false).await
}

fn list_tasks(ctx: &CallContext, args: &Map<String, Value>) -> Result<ToolOk, AgentErrorInfo> {
    let manager = &ctx.server.manager;
    let limit = util::optional_u64_bounded(args, "limit", 1, 200, 50)? as usize;
    let cursor = util::optional_str(args, "cursor")?;
    let states = match util::optional_str_list(args, "states")? {
        None => None,
        Some(names) => {
            let mut parsed = Vec::with_capacity(names.len());
            for name in names {
                let state = TaskState::from_name(&name).ok_or_else(|| {
                    util::invalid_arg(
                        "states",
                        &format!(
                            "states from {}",
                            TaskState::all()
                                .iter()
                                .map(|state| state.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })?;
                parsed.push(state);
            }
            Some(parsed)
        }
    };
    let all = manager
        .store()
        .list(&rdm_agent::TaskFilter {
            states,
            limit: Some(usize::MAX),
            cursor: None,
        })
        .map_err(|error| util::internal(format!("cannot list tasks: {error}")))?;
    let total = all.len();
    let start = cursor
        .as_deref()
        .and_then(|cursor| all.iter().position(|record| record.id == cursor))
        .map(|index| index + 1)
        .unwrap_or(0);
    let page: Vec<&TaskRecord> = all.iter().skip(start).take(limit).collect();
    let next_cursor = if start + page.len() < total {
        page.last().map(|record| record.id.clone())
    } else {
        None
    };
    let tasks: Vec<Value> = page
        .iter()
        .map(|record| {
            serde_json::to_value(rdm_agent::TaskSummary::from(*record)).unwrap_or(Value::Null)
        })
        .collect();
    let data = json!({
        "tasks": tasks,
        "next_cursor": next_cursor,
        "total": total,
    });
    let summary = format!(
        "{total} task(s) total; returning {} from offset {start}",
        page.len()
    );
    Ok(ToolOk::new(data, summary))
}

fn get_task(ctx: &CallContext, args: &Map<String, Value>) -> Result<ToolOk, AgentErrorInfo> {
    let task_id = util::required_str(args, "task_id")?;
    let include_events = util::optional_bool(args, "include_events", false)?;
    let record = ctx.server.manager.get(&task_id)?;
    let events = if include_events {
        let limit = util::optional_u64_bounded(args, "event_limit", 0, 500, 50)? as usize;
        Some(
            ctx.server
                .manager
                .store()
                .events(&task_id, limit)
                .map_err(|error| util::internal(format!("cannot read events: {error}")))?,
        )
    } else {
        None
    };
    Ok(ToolOk::new(
        task_data(&record, events),
        util::task_summary_text(&record.view()),
    )
    .with_task(&record))
}

async fn wait_task(ctx: &CallContext, args: &Map<String, Value>) -> Result<ToolOk, AgentErrorInfo> {
    let task_id = util::required_str(args, "task_id")?;
    let timeout_ms = util::wait_timeout_ms_from_args(args, "timeout_ms")?;
    let include_events = util::optional_bool(args, "include_events", false)?;
    // Fast not-found before waiting.
    let record = ctx.server.manager.get(&task_id)?;
    let wait_cancel = Arc::clone(&ctx.wait_cancel);
    let manager = &ctx.server.manager;
    let settled = manager
        .wait(&task_id, timeout_ms, Some(&wait_cancel), |current| {
            ctx.progress_for(current);
        })
        .await
        .map_err(|error| with_task_id(error, &task_id))?;
    ctx.progress_for(&settled);
    let events = if include_events {
        let limit = util::optional_u64_bounded(args, "event_limit", 0, 500, 50)? as usize;
        Some(
            manager
                .store()
                .events(&task_id, limit)
                .map_err(|error| util::internal(format!("cannot read events: {error}")))?,
        )
    } else {
        let _ = record;
        None
    };
    Ok(ToolOk::new(
        task_data(&settled, events),
        util::task_summary_text(&settled.view()),
    )
    .with_task(&settled))
}

async fn pause_task(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    let task_id = util::required_str(args, "task_id")?;
    let record = ctx.server.manager.pause(&task_id).await?;
    Ok(ToolOk::new(
        task_data(&record, None),
        util::task_summary_text(&record.view()),
    )
    .with_task(&record))
}

fn resume_task(ctx: &CallContext, args: &Map<String, Value>) -> Result<ToolOk, AgentErrorInfo> {
    let task_id = util::required_str(args, "task_id")?;
    let record = ctx.server.manager.resume(&task_id)?;
    Ok(ToolOk::new(
        task_data(&record, None),
        util::task_summary_text(&record.view()),
    )
    .with_task(&record))
}

async fn cancel_task(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    let task_id = util::required_str(args, "task_id")?;
    let discard_partial = util::optional_bool(args, "discard_partial", false)?;
    let record = ctx.server.manager.cancel(&task_id, discard_partial).await?;
    Ok(ToolOk::new(
        task_data(&record, None),
        util::task_summary_text(&record.view()),
    )
    .with_task(&record))
}

fn verify_file(ctx: &CallContext, args: &Map<String, Value>) -> Result<ToolOk, AgentErrorInfo> {
    let expected = match util::optional_str(args, "expected_sha256")? {
        Some(value) => Some(util::validate_expected_sha256(&value)?),
        None => None,
    };
    let max_bytes = util::optional_u64(args, "max_bytes")?;
    let (path, from_task) = match util::optional_str(args, "task_id")? {
        Some(task_id) => {
            let record = ctx.server.manager.get(&task_id)?;
            let path = record
                .path
                .clone()
                .or_else(|| {
                    record
                        .result
                        .as_ref()
                        .map(|result| PathBuf::from(&result.path))
                })
                .ok_or_else(|| {
                    util::not_found(format!(
                        "task {task_id} has no completed file yet (state={})",
                        record.state.as_str()
                    ))
                })?;
            (path, true)
        }
        None => {
            let raw = util::optional_str(args, "path")?
                .ok_or_else(|| util::invalid_request("provide either task_id or path"))?;
            let path = PathBuf::from(raw);
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            if !ctx.server.policy.check_dest_dir(parent) {
                return Err(util::policy_denied(
                    format!("path is outside policy.allowed_dirs: {}", path.display()),
                    json!({
                        "path": path.display().to_string(),
                        "allowed_dirs": ctx.server.policy.summary().get("allowed_dirs").cloned().unwrap_or(Value::Null),
                    }),
                ));
            }
            (path, false)
        }
    };
    if !path.exists() {
        return Err(util::not_found(format!(
            "file not found: {}",
            path.display()
        )));
    }
    let outcome = util::sha256_file(&path, max_bytes).map_err(|error| {
        AgentErrorInfo::new(
            ErrorCode::PermissionDenied,
            format!("cannot read {}: {error}", path.display()),
        )
    })?;
    let sha256 = outcome.sha256.clone();
    let matched = match (&expected, &sha256) {
        (Some(expected), Some(actual)) if !outcome.truncated => {
            Some(expected.eq_ignore_ascii_case(actual))
        }
        _ => None,
    };
    if matched == Some(false) {
        return Err(AgentErrorInfo::new(
            ErrorCode::ChecksumMismatch,
            "file SHA-256 does not match expected_sha256",
        )
        .with_hint("The file is corrupt or incomplete; download it again.")
        .with_details(json!({
            "path": path.display().to_string(),
            "size": outcome.size,
            "sha256": sha256,
            "expected_sha256": expected,
            "truncated": outcome.truncated,
        })));
    }
    let data = json!({
        "path": path.display().to_string(),
        "size": outcome.size,
        "sha256": sha256,
        "expected_sha256": expected,
        "match": matched,
        "bytes_hashed": outcome.bytes_hashed,
        "truncated": outcome.truncated,
        "source": if from_task { "task" } else { "path" },
    });
    let summary = match (&expected, &sha256) {
        (Some(_), Some(actual)) if !outcome.truncated => {
            format!("sha256 OK ({}): {actual}", util::format_size(outcome.size))
        }
        _ => format!(
            "{} ({}), sha256 {}{}",
            path.display(),
            util::format_size(outcome.size),
            sha256.as_deref().unwrap_or("<none>"),
            if outcome.truncated {
                " (partial hash)"
            } else {
                ""
            }
        ),
    };
    Ok(ToolOk::new(data, summary))
}
