//! Readiness and maintenance tools: `rdm_doctor`, `rdm_selftest`, `rdm_bench`,
//! `rdm_state_maintenance` (contract §3.1).
//!
//! `selftest`/`bench` use the in-process fixture server on a random loopback
//! port, so they never touch the public network. `state_maintenance` is
//! dry-run by default and only deletes files below `policy.allowed_dirs` (plus
//! the agent state directory) when `dry_run=false`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdm_agent::{
    iso8601_in_minutes, now_iso8601, redact_url_userinfo, AgentErrorInfo, ErrorCode, TaskFilter,
    TaskState,
};
use rdm_core::{DownloadEvent, DownloadHandle, DownloadRequest, DownloadSummary};
use rdm_fixture_server::{Config, TestServer};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::tools::{CallContext, ToolOk};
use crate::util;

const SELFTEST_MIN_BYTES: u64 = 64 * 1024;
const SELFTEST_MAX_BYTES: u64 = 64 * 1024 * 1024;
const BENCH_MAX_BYTES: u64 = 256 * 1024 * 1024;

fn check(name: &str, status: &str, detail: impl Into<String>) -> Value {
    json!({ "name": name, "status": status, "detail": detail.into() })
}

fn temp_work_dir(state_root: &Path, tag: &str) -> PathBuf {
    let dir = state_root.join(tag).join(rdm_agent::new_task_id());
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// rdm_doctor
// ---------------------------------------------------------------------------

pub(crate) async fn doctor(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    let server = &ctx.server;
    let policy = &server.policy;
    let mut checks: Vec<Value> = Vec::new();

    checks.push(check(
        "version",
        "ok",
        format!(
            "rdm-mcp {} (schema {}) on {} {}",
            env!("CARGO_PKG_VERSION"),
            rdm_agent::SCHEMA_VERSION,
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    ));

    // Agent state directory: layout must be creatable (doctor runs before any
    // task in a fresh environment).
    match server.store.ensure_layout() {
        Ok(()) => checks.push(check(
            "state_dir",
            "ok",
            format!("{}", server.state_root.display()),
        )),
        Err(error) => checks.push(check(
            "state_dir",
            "error",
            format!(
                "cannot create state layout under {}: {error}",
                server.state_root.display()
            ),
        )),
    }

    // Policy file.
    let policy_path = rdm_agent::Policy::path_for(&server.state_root);
    if policy_path.exists() {
        match std::fs::read_to_string(&policy_path) {
            Ok(text) => match serde_json::from_str::<rdm_agent::Policy>(&text) {
                Ok(_) => checks.push(check(
                    "policy_file",
                    "ok",
                    format!("{} (valid JSON)", policy_path.display()),
                )),
                Err(error) => checks.push(check(
                    "policy_file",
                    "warning",
                    format!(
                        "{} contains invalid JSON ({error}); safe defaults are in effect",
                        policy_path.display()
                    ),
                )),
            },
            Err(error) => checks.push(check(
                "policy_file",
                "warning",
                format!("cannot read {}: {error}", policy_path.display()),
            )),
        }
    } else {
        checks.push(check(
            "policy_file",
            "warning",
            format!(
                "{} not found; using safe defaults (allowed_hosts={:?}, allow_private_hosts={})",
                policy_path.display(),
                policy.allowed_hosts,
                policy.allow_private_hosts
            ),
        ));
    }

    // Allowed download directories.
    for dir in &policy.allowed_dirs {
        if dir.exists() {
            checks.push(check(
                "download_dir",
                "ok",
                format!("{} exists", dir.display()),
            ));
        } else {
            checks.push(check(
                "download_dir",
                "warning",
                format!(
                    "{} does not exist yet (it will be created on download)",
                    dir.display()
                ),
            ));
        }
    }

    // Task store health.
    match server.store.list(&TaskFilter {
        limit: Some(usize::MAX),
        ..TaskFilter::default()
    }) {
        Ok(tasks) => {
            let mut counts = serde_json::Map::new();
            for state in TaskState::all() {
                let count = tasks.iter().filter(|task| task.state == *state).count();
                counts.insert(state.as_str().to_string(), json!(count));
            }
            let stale_after = iso8601_in_minutes(-24 * 60);
            let stale = tasks
                .iter()
                .filter(|task| {
                    !task.state.is_terminal() && task.updated_at.as_str() < stale_after.as_str()
                })
                .count();
            let corrupt = count_corrupt_task_files(&server.state_root);
            if corrupt > 0 {
                checks.push(check(
                    "task_store",
                    "warning",
                    format!(
                        "{} task record(s) could not be parsed; run rdm_state_maintenance",
                        corrupt
                    ),
                ));
            } else if stale > 0 {
                checks.push(check(
                    "task_store",
                    "warning",
                    format!(
                        "{} task(s) appear stale (no update for 24 h); run rdm_state_maintenance",
                        stale
                    ),
                ));
            } else {
                checks.push(check(
                    "task_store",
                    "ok",
                    format!("{} task(s): {}", tasks.len(), Value::Object(counts)),
                ));
            }
        }
        Err(error) => checks.push(check(
            "task_store",
            "error",
            format!("cannot list tasks: {error}"),
        )),
    }

    // Pending plans.
    match server.store.list_plans() {
        Ok(plans) => {
            let expired = plans.iter().filter(|plan| plan.is_expired()).count();
            if expired > 0 {
                checks.push(check(
                    "plans",
                    "warning",
                    format!("{expired} expired plan(s) can be pruned by rdm_state_maintenance"),
                ));
            } else if plans.is_empty() {
                checks.push(check("plans", "ok", "no pending plans"));
            } else {
                checks.push(check(
                    "plans",
                    "ok",
                    format!("{} pending plan(s)", plans.len()),
                ));
            }
        }
        Err(error) => checks.push(check(
            "plans",
            "warning",
            format!("cannot list plans: {error}"),
        )),
    }

    // GUI / Firefox-extension IPC is optional; its absence is a warning.
    if rdm_ipc::is_gui_alive() {
        checks.push(check(
            "gui_ipc",
            "ok",
            format!("RDM GUI reachable at {}", rdm_ipc::info_path().display()),
        ));
    } else {
        checks.push(check(
            "gui_ipc",
            "warning",
            "RDM GUI/extension host not running (optional for MCP downloads)",
        ));
    }

    // Optional network reachability (+ policy verdict) for one URL.
    if let Some(url) = util::optional_str(args, "url")? {
        util::validate_url(&url)?;
        match server.policy.check_url(&url) {
            rdm_agent::PolicyDecision::Deny { reason } => {
                checks.push(check(
                    "network",
                    "warning",
                    format!("URL blocked by policy before any request: {reason}"),
                ));
            }
            _ => {
                let request = DownloadRequest::new(url.clone());
                match tokio::time::timeout(Duration::from_secs(20), rdm_core::probe(&request)).await
                {
                    Ok(Ok(meta)) => checks.push(check(
                        "network",
                        "ok",
                        format!(
                            "{} -> {} ({} bytes, ranges={}, {})",
                            redact_url_userinfo(&url),
                            redact_url_userinfo(&meta.final_url),
                            meta.total_bytes
                                .map(|bytes| bytes.to_string())
                                .unwrap_or_else(|| "?".to_string()),
                            meta.supports_ranges,
                            meta.http_version.as_deref().unwrap_or("HTTP/?")
                        ),
                    )),
                    Ok(Err(error)) => {
                        checks.push(check("network", "error", format!("probe failed: {error}")))
                    }
                    Err(_) => checks.push(check("network", "error", "probe timed out after 20 s")),
                }
            }
        }
    } else {
        checks.push(check("network", "skipped", "no url supplied"));
    }

    let errors = checks
        .iter()
        .filter(|entry| entry["status"] == "error")
        .count();
    let warnings = checks
        .iter()
        .filter(|entry| entry["status"] == "warning")
        .count();
    let ok = errors == 0;
    let summary = json!({
        "ok": ok,
        "checks": checks.len(),
        "errors": errors,
        "warnings": warnings,
        "rdm_version": env!("CARGO_PKG_VERSION"),
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "state_root": server.state_root.display().to_string(),
        "policy_file": policy_path.display().to_string(),
        "policy_source": if policy_path.exists() { "file" } else { "defaults" },
        "allow_private_hosts": policy.allow_private_hosts,
        "allowed_hosts": policy.allowed_hosts,
        "max_concurrent_tasks": policy.max_concurrent_tasks,
        "network_checked": util::optional_str(args, "url")?.is_some(),
    });
    let text = format!(
        "doctor: {} ({} checks, {errors} error(s), {warnings} warning(s)){}",
        if ok { "ok" } else { "degraded" },
        checks.len(),
        if warnings > 0 {
            "\nfirst warning: "
        } else {
            ""
        },
    );
    let first_warning = checks
        .iter()
        .find(|entry| entry["status"] == "warning")
        .and_then(|entry| entry["detail"].as_str())
        .unwrap_or("")
        .to_string();
    let text = format!("{text}{first_warning}");
    Ok(ToolOk::new(
        json!({ "ok": ok, "checks": checks, "summary": summary }),
        text,
    ))
}

fn count_corrupt_task_files(state_root: &Path) -> usize {
    let dir = state_root.join("tasks");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut corrupt = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<rdm_agent::TaskRecord>(&text).ok())
        {
            Some(_) => {}
            None => corrupt += 1,
        }
    }
    corrupt
}

// ---------------------------------------------------------------------------
// rdm_selftest
// ---------------------------------------------------------------------------

struct CaseResult {
    name: &'static str,
    status: &'static str,
    detail: String,
    elapsed_ms: u64,
}

impl CaseResult {
    fn to_value(&self) -> Value {
        json!({
            "name": self.name,
            "status": self.status,
            "detail": self.detail,
            "elapsed_ms": self.elapsed_ms,
        })
    }
}

fn case(name: &'static str, started: Instant, result: Result<String, String>) -> CaseResult {
    let elapsed_ms = util::elapsed_ms(started);
    match result {
        Ok(detail) => CaseResult {
            name,
            status: "pass",
            detail,
            elapsed_ms,
        },
        Err(detail) => CaseResult {
            name,
            status: "fail",
            detail,
            elapsed_ms,
        },
    }
}

fn sha256_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

async fn download_to(
    url: &str,
    dir: &Path,
    connections: usize,
    expected_sha256: Option<String>,
) -> Result<DownloadSummary, rdm_core::DownloadError> {
    let mut request = DownloadRequest::new(url);
    request.output_dir = Some(dir.to_path_buf());
    request.connections = connections;
    request.expected_sha256 = expected_sha256;
    rdm_core::download(request, None).await
}

pub(crate) async fn selftest(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    let size = util::optional_u64_bounded(
        args,
        "size_bytes",
        SELFTEST_MIN_BYTES,
        SELFTEST_MAX_BYTES,
        4 * 1024 * 1024,
    )?;
    let work = temp_work_dir(&ctx.server.state_root, "selftest");
    let started = Instant::now();
    let mut cases: Vec<CaseResult> = Vec::new();
    ctx.log("info", "selftest: starting fixture-based engine checks");

    // -- 1. multi-connection hash consistency ---------------------------
    let case_start = Instant::now();
    let result = async {
        let server = TestServer::start(Config::new(size))
            .await
            .map_err(|error| format!("cannot start fixture server: {error}"))?;
        let expected = sha256_of(&server.content());
        let dir = work.join("multi");
        let summary = download_to(&server.file_url(), &dir, 4, Some(expected.clone()))
            .await
            .map_err(|error| format!("download failed: {error}"))?;
        let stats = server.stats();
        server.shutdown().await;
        if summary.sha256.as_deref() != Some(expected.as_str()) {
            return Err(format!("sha256 mismatch: got {:?}", summary.sha256));
        }
        if summary.total_bytes != size {
            return Err(format!(
                "size mismatch: got {} expected {size}",
                summary.total_bytes
            ));
        }
        if stats.responses_206 == 0 {
            return Err("no ranged (206) responses observed".to_string());
        }
        Ok(format!(
            "{} in {} ms with {} ranged responses",
            util::format_size(summary.total_bytes),
            summary.elapsed_ms,
            stats.responses_206
        ))
    }
    .await;
    cases.push(case("multi_connection_hash", case_start, result));

    // -- 2. graceful 200 fallback when the server ignores Range ---------
    let case_start = Instant::now();
    let result = async {
        let server = TestServer::start(Config::new(size).without_ranges())
            .await
            .map_err(|error| format!("cannot start fixture server: {error}"))?;
        let expected = sha256_of(&server.content());
        let dir = work.join("fallback");
        let summary = download_to(&server.file_url(), &dir, 4, Some(expected.clone()))
            .await
            .map_err(|error| format!("download failed: {error}"))?;
        server.shutdown().await;
        if summary.range_supported {
            return Err("range_supported should be false for a 200-only server".to_string());
        }
        if summary.sha256.as_deref() != Some(expected.as_str()) {
            return Err("sha256 mismatch after fallback".to_string());
        }
        Ok(format!(
            "single stream fallback kept hash; {} bytes",
            summary.total_bytes
        ))
    }
    .await;
    cases.push(case("range_fallback", case_start, result));

    // -- 3. pause/resume transfers only the missing bytes ---------------
    let case_start = Instant::now();
    let result = resume_case(size, &work.join("resume")).await;
    cases.push(case("pause_resume_delta", case_start, result));

    // -- 4. checksum mismatch leaves no residue --------------------------
    let case_start = Instant::now();
    let result = async {
        let server = TestServer::start(Config::new(size))
            .await
            .map_err(|error| format!("cannot start fixture server: {error}"))?;
        let dir = work.join("checksum");
        let wrong = "00".repeat(32);
        let outcome = download_to(&server.file_url(), &dir, 2, Some(wrong)).await;
        server.shutdown().await;
        match outcome {
            Ok(summary) => Err(format!(
                "expected checksum_mismatch, got success at {}",
                summary.path.display()
            )),
            Err(rdm_core::DownloadError::ChecksumMismatch { .. }) => {
                let mut residue = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".part") || name.ends_with(".part.json") || name == "file"
                        {
                            residue.push(name);
                        }
                    }
                }
                if !residue.is_empty() {
                    return Err(format!(
                        "checksum failure left residue behind: {}",
                        residue.join(", ")
                    ));
                }
                Ok("checksum mismatch reported and .part state cleaned up".to_string())
            }
            Err(error) => Err(format!("unexpected error: {error}")),
        }
    }
    .await;
    cases.push(case("checksum_cleanup", case_start, result));

    cleanup(&work);
    let passed = cases.iter().filter(|case| case.status == "pass").count();
    let ok = passed == cases.len();
    let elapsed_ms = util::elapsed_ms(started);
    // Additive (v1.4, architecture §11.10): read-only recipe self-test report.
    // Recipe failures are reported only and never change `ok`/`passed`.
    let recipes = recipe_selftest_report(&rdm_ipc::state_dir());
    let data = json!({
        "ok": ok,
        "cases": cases.iter().map(CaseResult::to_value).collect::<Vec<_>>(),
        "passed": passed,
        "failed": cases.len() - passed,
        "fixture": { "transport": "loopback-random-port", "size_bytes": size },
        "elapsed_ms": elapsed_ms,
        "recipes": recipes,
    });
    let mut text = format!(
        "selftest: {} ({passed}/{} cases, {} ms)",
        if ok { "PASS" } else { "FAIL" },
        cases.len(),
        elapsed_ms
    );
    for case in &cases {
        text.push_str(&format!(
            "\n- {}: {} — {}",
            case.name, case.status, case.detail
        ));
    }
    text.push_str(&format!(
        "\n- recipes: {} checked / {} passed / {} failed / {} skipped",
        recipes["checked"].as_u64().unwrap_or(0),
        recipes["passed"].as_u64().unwrap_or(0),
        recipes["failed"].as_u64().unwrap_or(0),
        recipes["skipped"].as_u64().unwrap_or(0),
    ));
    Ok(ToolOk::new(data, text))
}

/// Maximum failure rows kept in the recipe report (contract: capped list).
const MAX_RECIPE_FAILURES: usize = 50;

/// Read-only recipe self-test report (`docs/architecture.md` §11.10).
///
/// Loads `<state>/resolve/recipes` (missing/bad files are tolerated by the
/// registry) and runs `Recipe::check_tests`: pure applicability checks only —
/// no network, no model, no recipe step execution. Failures are reported, never
/// folded into the engine `passed`/`failed` fields.
fn recipe_selftest_report(state_dir: &Path) -> Value {
    let dir = state_dir.join("resolve").join("recipes");
    let registry = rdm_resolve::RecipeRegistry::load(&dir);
    let mut checked = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<Value> = Vec::new();
    for recipe in registry.all() {
        checked += 1;
        for result in recipe.check_tests() {
            if result.skipped {
                skipped += 1;
            } else if result.ok {
                passed += 1;
            } else {
                failed += 1;
                if failures.len() < MAX_RECIPE_FAILURES {
                    failures.push(json!({
                        "recipe_id": recipe.id,
                        "version": recipe.version,
                        "test": result.name,
                        "message": result.message,
                    }));
                }
            }
        }
    }
    json!({
        "dir": dir.display().to_string(),
        "checked": checked,
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "failures": failures,
    })
}

/// Pause mid-transfer, then resume; verify the resumed run does not re-fetch
/// already-downloaded bytes and the final hash still matches.
async fn resume_case(size: u64, dir: &Path) -> Result<String, String> {
    // Deterministic timing: one connection, and a fixture delay derived from
    // the chunk count so the transfer always lasts long enough (~600 ms) for
    // the first 250 ms progress report to arrive before completion.
    let chunk_size = (size / 4).clamp(rdm_core::MIN_CHUNK_SIZE, rdm_core::DEFAULT_CHUNK_SIZE);
    let chunks = size.div_ceil(chunk_size).max(1);
    let delay_ms = (700 / chunks).clamp(15, 150);
    let config = Config::new(size).with_delay(delay_ms);
    let server = TestServer::start(config)
        .await
        .map_err(|error| format!("cannot start fixture server: {error}"))?;
    let expected = sha256_of(&server.content());
    let mut request = DownloadRequest::new(server.file_url());
    request.output_dir = Some(dir.to_path_buf());
    request.connections = 1;
    request.expected_sha256 = Some(expected.clone());
    let handle = DownloadHandle::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let download = rdm_core::download_with_handle(request.clone(), Some(tx), handle.clone());
    tokio::pin!(download);
    let pause_threshold = (size / 4).max(64 * 1024);
    let paused_downloaded;
    loop {
        tokio::select! {
            outcome = &mut download => {
                match outcome {
                    Err(rdm_core::DownloadError::Paused { downloaded }) => {
                        paused_downloaded = downloaded;
                        break;
                    }
                    Ok(summary) => {
                        server.shutdown().await;
                        return Err(format!(
                            "finished before pause triggered ({} bytes); fixture too fast",
                            summary.total_bytes
                        ));
                    }
                    Err(error) => {
                        server.shutdown().await;
                        return Err(format!("download failed before pause: {error}"));
                    }
                }
            }
            event = rx.recv() => {
                if let Some(DownloadEvent::Progress { downloaded, .. }) = event {
                    if downloaded > pause_threshold {
                        handle.pause();
                    }
                }
            }
        }
    }
    if paused_downloaded == 0 {
        server.shutdown().await;
        return Err("paused at zero bytes; cannot verify resume delta".to_string());
    }
    let bytes_before_resume = server.stats().body_bytes_sent;
    let summary = rdm_core::download(request, None).await.map_err(|error| {
        let _ = &server;
        format!("resume failed: {error}")
    })?;
    let bytes_after = server.stats().body_bytes_sent;
    server.shutdown().await;
    if summary.sha256.as_deref() != Some(expected.as_str()) {
        return Err("sha256 mismatch after resume".to_string());
    }
    let delta = bytes_after.saturating_sub(bytes_before_resume);
    let remaining = size.saturating_sub(paused_downloaded);
    let slack = 1024 * 1024;
    if delta > remaining + slack {
        return Err(format!(
            "resume re-downloaded too much: {delta} bytes for {remaining} remaining"
        ));
    }
    Ok(format!(
        "paused at {}; resumed with only {} bytes re-sent for {} remaining (+{} ms)",
        util::format_size(paused_downloaded),
        util::format_size(delta),
        util::format_size(remaining),
        summary.elapsed_ms
    ))
}

// ---------------------------------------------------------------------------
// rdm_bench
// ---------------------------------------------------------------------------

pub(crate) async fn bench(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    let size = util::optional_u64_bounded(
        args,
        "size_bytes",
        64 * 1024,
        BENCH_MAX_BYTES,
        16 * 1024 * 1024,
    )?;
    let mut connections = match util::optional_u64_list(args, "connections")? {
        Some(values) => values
            .into_iter()
            .map(|value| {
                if !(1..=32).contains(&value) {
                    Err(util::invalid_arg("connections", "integers in 1..=32"))
                } else {
                    Ok(value as usize)
                }
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => vec![1, 8],
    };
    if connections.is_empty() {
        connections = vec![1];
    }
    connections.truncate(8);
    let repeats = util::optional_u64_bounded(args, "repeats", 1, 7, 3)? as usize;

    let work = temp_work_dir(&ctx.server.state_root, "bench");
    let server = TestServer::start(Config::new(size))
        .await
        .map_err(|error| util::internal(format!("cannot start fixture server: {error}")))?;
    let base_url = server.file_url();
    let mut runs: Vec<Value> = Vec::new();
    let mut medians: Vec<Value> = Vec::new();
    for &connections in &connections {
        let mut samples: Vec<(u64, f64)> = Vec::new();
        for repeat in 0..repeats {
            let dir = work.join(format!("c{connections}-r{repeat}"));
            let started = Instant::now();
            match download_to(&base_url, &dir, connections, None).await {
                Ok(summary) => {
                    let elapsed = util::elapsed_ms(started);
                    samples.push((elapsed, summary.avg_speed_bps));
                    runs.push(json!({
                        "connections": connections,
                        "repeat": repeat,
                        "elapsed_ms": elapsed,
                        "avg_speed_bps": summary.avg_speed_bps,
                        "bytes": summary.total_bytes,
                        "status": "ok",
                    }));
                }
                Err(error) => {
                    runs.push(json!({
                        "connections": connections,
                        "repeat": repeat,
                        "status": "error",
                        "error": error.to_string(),
                    }));
                }
            }
        }
        let mut speeds: Vec<f64> = samples.iter().map(|(_, speed)| *speed).collect();
        speeds.sort_by(|a, b| a.total_cmp(b));
        let median_speed = if speeds.is_empty() {
            0.0
        } else {
            speeds[speeds.len() / 2]
        };
        let mut elapsed: Vec<u64> = samples.iter().map(|(elapsed, _)| *elapsed).collect();
        elapsed.sort_unstable();
        let median_elapsed = elapsed.get(elapsed.len() / 2).copied().unwrap_or(0);
        medians.push(json!({
            "connections": connections,
            "runs": samples.len(),
            "median_avg_speed_bps": median_speed,
            "median_elapsed_ms": median_elapsed,
        }));
        ctx.log(
            "info",
            &format!(
                "bench finished: {connections} connection(s), median {}/s",
                util::format_size(median_speed.max(0.0) as u64)
            ),
        );
    }
    server.shutdown().await;
    cleanup(&work);

    let baseline = medians
        .first()
        .and_then(|value| value["median_avg_speed_bps"].as_f64())
        .unwrap_or(0.0);
    let best = medians
        .iter()
        .filter_map(|value| value["median_avg_speed_bps"].as_f64())
        .fold(0.0f64, f64::max);
    let best_connections = medians
        .iter()
        .max_by(|a, b| {
            a["median_avg_speed_bps"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["median_avg_speed_bps"].as_f64().unwrap_or(0.0))
        })
        .and_then(|value| value["connections"].as_u64())
        .unwrap_or(0);
    let comparison = json!({
        "baseline_connections": connections.first().copied().unwrap_or(1),
        "baseline_median_avg_speed_bps": baseline,
        "best_connections": best_connections,
        "best_median_avg_speed_bps": best,
        "speedup": if baseline > 0.0 { best / baseline } else { 0.0 },
        "repeats": repeats,
        "note": "Loopback numbers measure this machine/engine, not the public internet; multi-connection is not always faster.",
    });
    let data = json!({
        "fixture": { "url": base_url, "size_bytes": size, "transport": "loopback-random-port" },
        "size_bytes": size,
        "repeats": repeats,
        "runs": runs,
        "medians": medians,
        "comparison": comparison,
    });
    let text = format!(
        "bench {}: best {} connection(s) at {}/s (speedup {:.2}x over {} connection(s)); per-run data included",
        util::format_size(size),
        best_connections,
        util::format_size(best.max(0.0) as u64),
        if baseline > 0.0 { best / baseline } else { 0.0 },
        connections.first().copied().unwrap_or(1),
    );
    Ok(ToolOk::new(data, text))
}

// ---------------------------------------------------------------------------
// rdm_state_maintenance
// ---------------------------------------------------------------------------

pub(crate) fn state_maintenance(
    ctx: &CallContext,
    args: &Map<String, Value>,
) -> Result<ToolOk, AgentErrorInfo> {
    let dry_run = util::optional_bool(args, "dry_run", true)?;
    let stale_hours = util::optional_u64_bounded(args, "stale_task_hours", 1, 8760, 24)?;
    let max_scan_files =
        util::optional_u64_bounded(args, "max_scan_files", 1, 200_000, 20_000)? as usize;
    let policy = &ctx.server.policy;
    let store = &ctx.server.store;
    let mut findings: Vec<Value> = Vec::new();
    let mut repaired = 0usize;
    let mut skipped = 0usize;
    let mut scanned = 0usize;

    let record_finding =
        |findings: &mut Vec<Value>, kind: &str, path: &Path, detail: &str, action: &str| {
            findings.push(json!({
                "kind": kind,
                "path": path.display().to_string(),
                "detail": detail,
                "action": action,
            }));
        };

    // 1. Corrupt task records.
    let tasks_dir = ctx.server.state_root.join("tasks");
    if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            scanned += 1;
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<rdm_agent::TaskRecord>(&text).ok());
            if parsed.is_none() {
                let action = if dry_run {
                    "would_quarantine"
                } else {
                    let corrupt = path.with_extension("json.corrupt");
                    let ok = std::fs::rename(&path, &corrupt).is_ok();
                    if ok {
                        repaired += 1;
                        "quarantined"
                    } else {
                        skipped += 1;
                        "quarantine_failed"
                    }
                };
                record_finding(
                    &mut findings,
                    "corrupt_task",
                    &path,
                    "task record is not valid JSON",
                    action,
                );
            }
        }
    }

    // 2. Stale non-terminal tasks.
    let stale_before = iso8601_in_minutes(-(stale_hours as i64) * 60);
    match store.list(&TaskFilter {
        limit: Some(usize::MAX),
        ..TaskFilter::default()
    }) {
        Ok(tasks) => {
            scanned += tasks.len();
            for task in tasks {
                if task.state.is_terminal() || task.updated_at.as_str() >= stale_before.as_str() {
                    continue;
                }
                let action = if dry_run {
                    "would_mark_failed"
                } else {
                    let mut record = task.clone();
                    record.state = TaskState::Failed;
                    record.error = Some(
                        AgentErrorInfo::new(
                            ErrorCode::ResumeInvalid,
                            "task is stale (no update for more than the configured threshold)",
                        )
                        .with_hint("Use rdm_resume_task to continue from the saved .part state.")
                        .with_details(
                            json!({ "updated_at": task.updated_at, "stale_hours": stale_hours }),
                        ),
                    );
                    record.speed_bps = 0.0;
                    record.eta_secs = None;
                    record.finished_at = Some(now_iso8601());
                    record.updated_at = now_iso8601();
                    match ctx.server.manager.store().update(&record) {
                        Ok(()) => {
                            repaired += 1;
                            let _ = ctx.server.manager.store().append_event(
                                &rdm_agent::TaskEvent::new(
                                    record.id.clone(),
                                    next_event_seq(&record.id, store),
                                    "stale",
                                    json!({ "stale_hours": stale_hours }),
                                ),
                            );
                            "marked_failed"
                        }
                        Err(_) => {
                            skipped += 1;
                            "mark_failed_failed"
                        }
                    }
                };
                record_finding(
                    &mut findings,
                    "stale_task",
                    Path::new(&task.id),
                    &format!(
                        "task {} has not been updated since {}",
                        task.id, task.updated_at
                    ),
                    action,
                );
            }
        }
        Err(error) => {
            return Err(util::internal(format!("cannot list tasks: {error}")));
        }
    }

    // 3. Expired plans.
    match store.list_plans() {
        Ok(plans) => {
            scanned += plans.len();
            for plan in plans {
                if !plan.is_expired() {
                    continue;
                }
                let action = if dry_run {
                    "would_delete"
                } else {
                    match store.delete_plan(&plan.plan_id) {
                        Ok(()) => {
                            repaired += 1;
                            "deleted"
                        }
                        Err(_) => {
                            skipped += 1;
                            "delete_failed"
                        }
                    }
                };
                record_finding(
                    &mut findings,
                    "expired_plan",
                    &ctx.server
                        .state_root
                        .join("plans")
                        .join(format!("{}.json", plan.plan_id)),
                    "approval plan expired",
                    action,
                );
            }
        }
        Err(error) => {
            return Err(util::internal(format!("cannot list plans: {error}")));
        }
    }

    // 4. Orphan/corrupt `.part` state under the allowed download dirs.
    for dir in &policy.allowed_dirs {
        if !dir.exists() {
            continue;
        }
        let mut files = Vec::new();
        scanned += util::walk_files(dir, max_scan_files, 8, &mut files);
        for path in &files {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if let Some(base) = name.strip_suffix(".part.json") {
                let data_file = path.with_file_name(format!("{base}.part"));
                let valid = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .is_some();
                if !valid {
                    let action = repair_delete(dry_run, path, &mut repaired, &mut skipped);
                    record_finding(
                        &mut findings,
                        "corrupt_state",
                        path,
                        "resume state is not valid JSON",
                        action,
                    );
                } else if !data_file.exists() {
                    let action = repair_delete(dry_run, path, &mut repaired, &mut skipped);
                    record_finding(
                        &mut findings,
                        "orphan_state",
                        path,
                        "resume state has no matching .part payload",
                        action,
                    );
                }
            } else if name.ends_with(".part") {
                let state_file = path.with_file_name(format!("{name}.json"));
                if !state_file.exists() {
                    let action = repair_delete(dry_run, path, &mut repaired, &mut skipped);
                    record_finding(
                        &mut findings,
                        "orphan_part",
                        path,
                        "partial payload has no matching .part.json state",
                        action,
                    );
                }
            }
        }
    }

    let data = json!({
        "dry_run": dry_run,
        "findings": findings,
        "repaired": repaired,
        "skipped": skipped,
        "scanned": scanned,
        "stale_task_hours": stale_hours,
        "max_scan_files": max_scan_files,
    });
    let text = format!(
        "state maintenance ({}): {} finding(s), {repaired} repaired, {skipped} skipped, {scanned} entries scanned",
        if dry_run { "dry-run" } else { "repair" },
        findings.len()
    );
    Ok(ToolOk::new(data, text))
}

fn repair_delete(
    dry_run: bool,
    path: &Path,
    repaired: &mut usize,
    skipped: &mut usize,
) -> &'static str {
    if dry_run {
        return "would_delete";
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            *repaired += 1;
            "deleted"
        }
        Err(_) => {
            *skipped += 1;
            "delete_failed"
        }
    }
}

fn next_event_seq(task_id: &str, store: &rdm_agent::TaskStore) -> u64 {
    store
        .events(task_id, 0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_and_cases_serialize() {
        let value = check("state_dir", "ok", "fine");
        assert_eq!(value["name"], "state_dir");
        assert_eq!(value["status"], "ok");
        let passed = case("multi", Instant::now(), Ok("done".to_string()));
        assert_eq!(passed.status, "pass");
        assert_eq!(passed.to_value()["detail"], "done");
        let failed = case("multi", Instant::now(), Err("boom".to_string()));
        assert_eq!(failed.status, "fail");
    }

    #[test]
    fn sha256_helper_is_sane() {
        assert_eq!(
            sha256_of(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn repair_delete_respects_dry_run() {
        let dir = std::env::temp_dir().join(format!("rdm-mcp-diag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.part");
        std::fs::write(&path, b"x").unwrap();
        let mut repaired = 0;
        let mut skipped = 0;
        assert_eq!(
            repair_delete(true, &path, &mut repaired, &mut skipped),
            "would_delete"
        );
        assert!(path.exists());
        assert_eq!(
            repair_delete(false, &path, &mut repaired, &mut skipped),
            "deleted"
        );
        assert!(!path.exists());
        assert_eq!((repaired, skipped), (1, 0));
        std::fs::remove_dir_all(&dir).ok();
    }
}
