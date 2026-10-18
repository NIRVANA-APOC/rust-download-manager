//! `rdm selftest` — end-to-end readiness probe using the embedded fixture
//! server (same HTTP stack the engine uses, no network/public hosts involved).
//!
//! Four cases (contract §7 / task checklist):
//! 1. multi-connection payload hash equals the single-connection/hash reference
//! 2. server ignoring `Range` → transparent single-stream fallback, correct file
//! 3. pause + resume only re-downloads the missing chunks (byte-exact)
//! 4. `expected_sha256` mismatch leaves no files behind

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdm_core::{DownloadError, DownloadEvent, DownloadHandle, DownloadRequest};
use rdm_fixture_server::{Config, Stats, TestServer};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::cli::SelftestArgs;
use crate::output::{OutputMode, Reporter};
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CaseStatus {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
struct CaseResult {
    id: &'static str,
    status: CaseStatus,
    summary: String,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

pub async fn run(mode: OutputMode, args: SelftestArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "selftest");
    let started = Instant::now();
    let mut cases = Vec::new();

    cases.push(run_case("multi_connection_hash", &args, case_multi_connection).await);
    cases.push(run_case("range_ignored_fallback", &args, case_range_fallback).await);
    cases.push(run_case("resume_missing_chunks", &args, case_resume).await);
    cases.push(run_case("checksum_no_residue", &args, case_checksum).await);

    let passed = cases
        .iter()
        .filter(|case| case.status == CaseStatus::Pass)
        .count();
    let failed = cases.len() - passed;
    let data = json!({
        "cases": cases,
        "passed": passed,
        "failed": failed,
        "duration_ms": super::download::elapsed_ms(started),
        "transport": "loopback-http1-fixture",
        "rdm_version": env!("CARGO_PKG_VERSION"),
    });

    if reporter.is_text() {
        for case in &data["cases"].as_array().cloned().unwrap_or_default() {
            let id = case.get("id").and_then(Value::as_str).unwrap_or("?");
            let status = case.get("status").and_then(Value::as_str).unwrap_or("?");
            let summary = case.get("summary").and_then(Value::as_str).unwrap_or("");
            crate::outln!("[{status:<4}] {id:<24} {summary}");
        }
        crate::outln!("selftest: {passed} passed, {failed} failed");
    }

    if failed > 0 {
        let error = rdm_agent::AgentErrorInfo::new(
            rdm_agent::ErrorCode::Internal,
            format!("{failed} selftest case(s) failed"),
        )
        .with_hint("run `rdm selftest --output json` and inspect data.cases")
        .with_details(json!({ "passed": passed, "failed": failed }));
        reporter.finish_report(data, Some(error))
    } else {
        reporter.finish_report(data, None)
    }
}

async fn run_case<F, Fut>(id: &'static str, args: &SelftestArgs, case: F) -> CaseResult
where
    F: FnOnce(&'static str, bool) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let started = Instant::now();
    match case(id, args.keep).await {
        Ok(details) => CaseResult {
            id,
            status: CaseStatus::Pass,
            summary: format!("{} passed", id.replace('_', " ")),
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            details: Some(details),
        },
        Err(message) => CaseResult {
            id,
            status: CaseStatus::Fail,
            summary: message,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            details: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Case 1: multi connection hash equals reference payload hash
// ---------------------------------------------------------------------------

async fn case_multi_connection(_id: &'static str, keep: bool) -> Result<Value, String> {
    let total = 2 * 1024 * 1024u64;
    let server = TestServer::start(Config::new(total))
        .await
        .map_err(|error| format!("cannot start fixture server: {error}"))?;
    let expected = util::sha256_bytes(&server.content());

    let single_dir = util::create_temp_dir("selftest-hash-single")
        .map_err(|error| format!("temp dir: {error}"))?;
    let multi_dir = util::create_temp_dir("selftest-hash-multi")
        .map_err(|error| format!("temp dir: {error}"))?;

    let single = download_to(&server, &single_dir, "single.bin", 1, Some(256 * 1024)).await;
    let multi = download_to(&server, &multi_dir, "multi.bin", 8, Some(256 * 1024)).await;
    let result = (|| -> Result<Value, String> {
        let single = single?;
        let multi = multi?;
        let single_hash =
            util::sha256_file(&single.path).map_err(|error| format!("hash single: {error}"))?;
        let multi_hash =
            util::sha256_file(&multi.path).map_err(|error| format!("hash multi: {error}"))?;
        if single_hash != expected {
            return Err(format!(
                "single-connection hash {single_hash} != fixture hash {expected}"
            ));
        }
        if multi_hash != expected {
            return Err(format!(
                "multi-connection hash {multi_hash} != fixture hash {expected}"
            ));
        }
        Ok(json!({
            "sha256": expected,
            "single_ms": single.elapsed_ms,
            "multi_ms": multi.elapsed_ms,
            "single_connections": single.connections,
            "multi_connections": multi.connections,
            "http_version": multi.http_version,
        }))
    })();
    cleanup(&[single_dir, multi_dir], keep);
    server.shutdown().await;
    result
}

// ---------------------------------------------------------------------------
// Case 2: server ignores Range → single stream, correct payload
// ---------------------------------------------------------------------------

async fn case_range_fallback(_id: &'static str, keep: bool) -> Result<Value, String> {
    let total = 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).without_ranges())
        .await
        .map_err(|error| format!("cannot start fixture server: {error}"))?;
    let expected = util::sha256_bytes(&server.content());
    let dir =
        util::create_temp_dir("selftest-fallback").map_err(|error| format!("temp dir: {error}"))?;

    let result = async {
        let summary = download_to(&server, &dir, "fallback.bin", 4, Some(256 * 1024)).await?;
        let hash =
            util::sha256_file(&summary.path).map_err(|error| format!("hash payload: {error}"))?;
        if hash != expected {
            return Err(format!("fallback payload hash {hash} != {expected}"));
        }
        if summary.range_supported {
            return Err("server ignored Range but summary.range_supported is true".to_string());
        }
        Ok(json!({
            "sha256": expected,
            "range_supported": summary.range_supported,
            "connections": summary.connections,
            "http_version": summary.http_version,
        }))
    }
    .await;
    cleanup(&[dir], keep);
    server.shutdown().await;
    result
}

// ---------------------------------------------------------------------------
// Case 3: pause → resume re-downloads exactly the missing bytes
// ---------------------------------------------------------------------------

async fn case_resume(_id: &'static str, keep: bool) -> Result<Value, String> {
    let total = 4 * 1024 * 1024u64;
    let chunk_size = 1024 * 1024u64;
    let server = TestServer::start(Config::new(total).with_delay(40))
        .await
        .map_err(|error| format!("cannot start fixture server: {error}"))?;
    let expected = util::sha256_bytes(&server.content());
    let dir =
        util::create_temp_dir("selftest-resume").map_err(|error| format!("temp dir: {error}"))?;

    let result = async {
        let mut request = DownloadRequest::new(server.file_url());
        request.output_dir = Some(dir.clone());
        request.file_name = Some("resume.bin".to_string());
        request.connections = 2;
        request.chunk_size = Some(chunk_size);

        let handle = DownloadHandle::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task_handle = handle.clone();
        let request_for_task = request.clone();
        let task = tokio::spawn(async move {
            rdm_core::download_with_handle(request_for_task, Some(tx), task_handle).await
        });

        // Wait for the transfer to start, let a couple of chunks land, then
        // pause. Progress events are only emitted every 250 ms, which is too
        // coarse for a fast local fixture, so we key off `Started` instead.
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match rx.recv().await {
                    Some(DownloadEvent::Started { .. }) => break,
                    Some(_) => continue,
                    None => return Err("download ended before it could be paused".to_string()),
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|_| "timed out waiting for the download to start".to_string())??;
        tokio::time::sleep(Duration::from_millis(250)).await;

        handle.pause();
        let paused = match tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .map_err(|_| "pause timed out".to_string())?
            .map_err(|error| format!("download task panicked: {error}"))?
        {
            Err(DownloadError::Paused { downloaded }) => downloaded,
            Ok(_) => return Err("download completed before it could be paused".to_string()),
            Err(other) => return Err(format!("unexpected pause result: {other}")),
        };
        if paused == 0 {
            return Err("paused with zero downloaded bytes".to_string());
        }

        let state_path = dir.join("resume.bin.part.json");
        let state: Value = serde_json::from_slice(
            &std::fs::read(&state_path)
                .map_err(|error| format!("cannot read resume state: {error}"))?,
        )
        .map_err(|error| format!("resume state is not valid JSON: {error}"))?;
        let completed = state["completed_chunks"]
            .as_array()
            .map(|array| array.len())
            .unwrap_or(0);
        if completed == 0 {
            return Err("resume state contains no completed chunks".to_string());
        }

        // Let the fixture finish flushing bytes the paused clients aborted.
        let before = settled_stats(&server).await;
        let resumed = rdm_core::download(request, None)
            .await
            .map_err(|error| format!("resume failed: {error}"))?;
        let after = settled_stats(&server).await;

        let delta = after.body_bytes_sent - before.body_bytes_sent;
        let missing = total - paused;
        if delta != missing {
            return Err(format!(
                "resume re-downloaded {delta} bytes but only {missing} were missing"
            ));
        }
        let hash = util::sha256_file(&resumed.path)
            .map_err(|error| format!("hash resumed payload: {error}"))?;
        if hash != expected {
            return Err(format!("resumed payload hash {hash} != {expected}"));
        }
        if resumed.resumed_chunks != completed as u64 {
            return Err(format!(
                "summary.resumed_chunks {} != state completed {}",
                resumed.resumed_chunks, completed
            ));
        }
        Ok(json!({
            "sha256": expected,
            "paused_bytes": paused,
            "missing_bytes": missing,
            "resume_downloaded_bytes": delta,
            "resumed_chunks": resumed.resumed_chunks,
        }))
    }
    .await;
    cleanup(&[dir], keep);
    server.shutdown().await;
    result
}

// ---------------------------------------------------------------------------
// Case 4: checksum mismatch leaves no file behind
// ---------------------------------------------------------------------------

async fn case_checksum(_id: &'static str, keep: bool) -> Result<Value, String> {
    let total = 512 * 1024u64;
    let server = TestServer::start(Config::new(total))
        .await
        .map_err(|error| format!("cannot start fixture server: {error}"))?;
    let dir =
        util::create_temp_dir("selftest-checksum").map_err(|error| format!("temp dir: {error}"))?;

    let result = async {
        let mut request = DownloadRequest::new(server.file_url());
        request.output_dir = Some(dir.clone());
        request.file_name = Some("checksum.bin".to_string());
        request.connections = 4;
        request.expected_sha256 = Some("0".repeat(64));
        match rdm_core::download(request, None).await {
            Err(DownloadError::ChecksumMismatch { expected, actual }) => {
                if expected != "0".repeat(64) || actual.len() != 64 {
                    return Err(format!(
                        "unexpected checksum details: expected={expected} actual={actual}"
                    ));
                }
            }
            Ok(_) => return Err("checksum mismatch was not detected".to_string()),
            Err(other) => return Err(format!("unexpected error: {other}")),
        }
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .map_err(|error| format!("cannot list output dir: {error}"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        if !leftovers.is_empty() {
            return Err(format!(
                "checksum failure left files behind: {}",
                leftovers.join(", ")
            ));
        }
        Ok(json!({ "residue_files": 0 }))
    }
    .await;
    cleanup(&[dir], keep);
    server.shutdown().await;
    result
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn download_to(
    server: &TestServer,
    dir: &Path,
    file_name: &str,
    connections: usize,
    chunk_size: Option<u64>,
) -> Result<rdm_core::DownloadSummary, String> {
    let mut request = DownloadRequest::new(server.file_url());
    request.output_dir = Some(dir.to_path_buf());
    request.file_name = Some(file_name.to_string());
    request.connections = connections;
    request.chunk_size = chunk_size;
    rdm_core::download(request, None)
        .await
        .map_err(|error| format!("download failed: {error}"))
}

/// Wait until the fixture stops observing traffic (leftover writes from paused
/// clients can otherwise pollute byte-delta assertions).
async fn settled_stats(server: &TestServer) -> Stats {
    let mut last = server.stats();
    let mut stable = 0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let now = server.stats();
        if now.body_bytes_sent == last.body_bytes_sent
            && now.requests == last.requests
            && now.range_headers.len() == last.range_headers.len()
        {
            stable += 1;
            if stable >= 3 {
                return now;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    server.stats()
}

fn cleanup(dirs: &[PathBuf], keep: bool) {
    if keep {
        return;
    }
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(CaseStatus::Pass).unwrap(),
            json!("pass")
        );
        assert_eq!(
            serde_json::to_value(CaseStatus::Fail).unwrap(),
            json!("fail")
        );
    }
}
