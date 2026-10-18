//! End-to-end tests for the agent-facing `rdm` CLI.
//!
//! Everything is driven through the real binary (`CARGO_BIN_EXE_rdm`) against
//! the embedded fixture server; no public network or user directories are
//! touched (each test gets its own `--state-dir` and temp output dir).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use rdm_fixture_server::{Config, TestServer};
use serde_json::Value;
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn rdm_bin() -> &'static str {
    env!("CARGO_BIN_EXE_rdm")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("rdm-cli-test-{tag}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct RunOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

impl RunOutput {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|error| {
            panic!(
                "expected one JSON object, got `{}` (error: {error}; stderr: {})",
                self.stdout, self.stderr
            )
        })
    }

    fn json_lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|error| {
                    panic!(
                        "invalid NDJSON line `{line}`: {error} (stderr: {})",
                        self.stderr
                    )
                })
            })
            .collect()
    }
}

async fn run(args: &[&str], state_dir: &Path) -> RunOutput {
    run_with_stdin(args, state_dir, None).await
}

/// Run without `--state-dir`, with explicit environment overrides (used to
/// reproduce the QA `LOCALAPPDATA` isolation setup).
async fn run_with_env(args: &[&str], envs: &[(&str, String)]) -> RunOutput {
    let mut command = Command::new(rdm_bin());
    command
        .args(args)
        .env_remove("RDM_AGENT_STATE_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().await.expect("run rdm");
    RunOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

async fn run_with_stdin(args: &[&str], state_dir: &Path, stdin: Option<&str>) -> RunOutput {
    let mut command = Command::new(rdm_bin());
    command
        .args(args)
        .arg("--state-dir")
        .arg(state_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match stdin {
        Some(text) => {
            command.stdin(Stdio::piped());
            let mut child = command.spawn().expect("spawn rdm");
            {
                use tokio::io::AsyncWriteExt;
                let mut handle = child.stdin.take().expect("stdin pipe");
                handle
                    .write_all(text.as_bytes())
                    .await
                    .expect("write stdin");
            }
            let output = child.wait_with_output().await.expect("wait rdm");
            RunOutput {
                code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
        None => {
            command.stdin(Stdio::null());
            let output = command.output().await.expect("run rdm");
            RunOutput {
                code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
    }
}

async fn fixture(size: u64) -> TestServer {
    TestServer::start(Config::new(size))
        .await
        .expect("start fixture server")
}

// ---------------------------------------------------------------------------
// Policy enforcement (task-11)
// ---------------------------------------------------------------------------

fn write_policy(state_dir: &Path, policy: serde_json::Value) {
    std::fs::write(
        state_dir.join("policy.json"),
        serde_json::to_vec_pretty(&policy).unwrap(),
    )
    .unwrap();
}

/// Baseline policy that allows the loopback fixture and one output dir.
fn allowed_policy(out_dir: &Path) -> serde_json::Value {
    serde_json::json!({
        "schema_version": "rdm.agent/v1",
        "allowed_hosts": ["*"],
        "allowed_dirs": [out_dir],
        "max_bytes_per_task": 1024 * 1024,
        "require_approval_over_bytes": null,
        "max_concurrent_tasks": 2,
        "default_connections": 2,
        "allow_private_hosts": true,
        "allow_overwrite": false
    })
}

/// QA reproduction policy: only public example.com hosts, no overwrite.
fn public_only_policy(out_dir: &Path) -> serde_json::Value {
    serde_json::json!({
        "schema_version": "rdm.agent/v1",
        "allowed_hosts": ["*.example.com"],
        "allowed_dirs": [out_dir],
        "max_bytes_per_task": null,
        "require_approval_over_bytes": null,
        "max_concurrent_tasks": 2,
        "default_connections": 2,
        "allow_private_hosts": false,
        "allow_overwrite": false
    })
}

#[tokio::test]
async fn download_policy_denies_private_host() {
    let fixture = fixture(128 * 1024).await;
    let state = TempDir::new("policy-denied");
    let out = TempDir::new("policy-denied-out");
    write_policy(state.path(), public_only_policy(out.path()));

    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 4,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "policy_denied");
    assert_eq!(value["data"], Value::Null);
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    fixture.shutdown().await;
}

#[tokio::test]
async fn download_policy_allowed_succeeds() {
    let fixture = fixture(128 * 1024).await;
    let state = TempDir::new("policy-allowed");
    let out = TempDir::new("policy-allowed-out");
    write_policy(state.path(), allowed_policy(out.path()));

    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 0,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["state"], "completed");
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1);
    fixture.shutdown().await;
}

#[tokio::test]
async fn download_policy_size_denied_before_download() {
    let fixture = fixture(256 * 1024).await;
    let state = TempDir::new("policy-size");
    let out = TempDir::new("policy-size-out");
    let mut policy = allowed_policy(out.path());
    policy["max_bytes_per_task"] = serde_json::json!(1024);
    write_policy(state.path(), policy);

    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 4,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["error"]["code"], "policy_denied");
    assert_eq!(value["error"]["details"]["total_bytes"], 256 * 1024);
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    fixture.shutdown().await;
}

#[tokio::test]
async fn download_policy_needs_approval_and_does_not_download() {
    let fixture = fixture(256 * 1024).await;
    let state = TempDir::new("policy-approval");
    let out = TempDir::new("policy-approval-out");
    let mut policy = allowed_policy(out.path());
    policy["max_bytes_per_task"] = Value::Null;
    policy["require_approval_over_bytes"] = serde_json::json!(1024);
    write_policy(state.path(), policy);

    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 4,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["error"]["code"], "needs_approval");
    assert_eq!(
        value["error"]["details"]["require_approval_over_bytes"],
        1024
    );
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    fixture.shutdown().await;
}

#[tokio::test]
async fn download_policy_conflict_when_target_exists() {
    let fixture = fixture(128 * 1024).await;
    let state = TempDir::new("policy-conflict");
    let out = TempDir::new("policy-conflict-out");
    write_policy(state.path(), allowed_policy(out.path()));
    let target = out.path().join("existing.bin");
    std::fs::write(&target, b"KEEP").unwrap();

    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--name",
            "existing.bin",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 4,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["error"]["code"], "conflict");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "KEEP");
    fixture.shutdown().await;
}

#[tokio::test]
async fn download_policy_unknown_length_is_cancelled_when_over_budget() {
    let fixture = TestServer::start(
        Config::new(1024 * 1024)
            .without_length()
            .without_ranges()
            .with_delay(100),
    )
    .await
    .unwrap();
    let state = TempDir::new("policy-stream");
    let out = TempDir::new("policy-stream-out");
    let mut policy = allowed_policy(out.path());
    policy["max_bytes_per_task"] = serde_json::json!(128 * 1024);
    write_policy(state.path(), policy);

    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 4,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["error"]["code"], "policy_denied");
    // The completed target must not exist; a resumable .part may remain.
    assert!(!out.path().join("file").exists());
    fixture.shutdown().await;
}

#[tokio::test]
async fn probe_policy_denies_private_host() {
    let fixture = fixture(64 * 1024).await;
    let state = TempDir::new("policy-probe");
    let out = TempDir::new("policy-probe-out");
    write_policy(state.path(), public_only_policy(out.path()));

    let output = run(
        &["probe", &fixture.file_url(), "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 4,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["error"]["code"], "policy_denied");
    assert_eq!(value["data"], Value::Null);
    fixture.shutdown().await;
}

#[tokio::test]
async fn corrupt_policy_is_invalid_request() {
    let fixture = TempDir::new("policy-corrupt-fixture");
    let _ = fixture; // no server needed: policy parsing fails before networking
    let state = TempDir::new("policy-corrupt");
    std::fs::write(state.path().join("policy.json"), b"{not json").unwrap();

    let output = run(
        &["download", "http://127.0.0.1:1/file", "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 2, "stdout: {}", output.stdout);
    assert_eq!(output.json()["error"]["code"], "invalid_request");

    let output = run(
        &["probe", "http://127.0.0.1:1/file", "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 2, "stdout: {}", output.stdout);
    assert_eq!(output.json()["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn no_policy_warns_but_still_downloads() {
    let fixture = fixture(64 * 1024).await;
    let state = TempDir::new("policy-absent");
    let out = TempDir::new("policy-absent-out");

    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--name",
            "absent.bin",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    let warnings = value["data"]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .unwrap_or("")
            .starts_with("policy-file-absent")),
        "expected policy-file-absent warning, got {warnings:?}"
    );

    let output = run(
        &["probe", &fixture.file_url(), "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0);
    let value = output.json();
    assert_eq!(value["data"]["policy_enforced"], false);
    let warnings = value["data"]["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|warning| warning
        .as_str()
        .unwrap_or("")
        .starts_with("policy-file-absent")));
    fixture.shutdown().await;
}

#[tokio::test]
async fn qa_repro_isolated_localappdata_enforces_policy() {
    let fixture = fixture(128 * 1024).await;
    let local = TempDir::new("qa-localappdata");
    let state_dir = local.path().join("RDM").join("agent");
    std::fs::create_dir_all(&state_dir).unwrap();
    let out = TempDir::new("qa-localappdata-out");
    write_policy(&state_dir, public_only_policy(out.path()));

    // No --state-dir: the CLI must resolve %LOCALAPPDATA%\RDM\agent itself.
    let output = run_with_env(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        &[("LOCALAPPDATA", local.path().to_string_lossy().into_owned())],
    )
    .await;
    assert_eq!(
        output.code, 4,
        "policy from isolated LOCALAPPDATA was not enforced: stdout: {} stderr: {}",
        output.stdout, output.stderr
    );
    assert_eq!(output.json()["error"]["code"], "policy_denied");
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);

    // Removing the file restores the documented "no policy -> warning+success".
    std::fs::remove_file(state_dir.join("policy.json")).unwrap();
    let output = run_with_env(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out.path().to_str().unwrap(),
            "--name",
            "allowed.bin",
            "--output",
            "json",
        ],
        &[("LOCALAPPDATA", local.path().to_string_lossy().into_owned())],
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert_eq!(output.json()["ok"], true);
    fixture.shutdown().await;
}

fn assert_no_secret(output: &RunOutput, secret: &str) {
    assert!(
        !output.stdout.contains(secret),
        "secret leaked to stdout: {}",
        output.stdout
    );
    assert!(
        !output.stderr.contains(secret),
        "secret leaked to stderr: {}",
        output.stderr
    );
}

// ---------------------------------------------------------------------------
// capabilities / schema / output-mode compatibility
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capabilities_json_envelope() {
    let state = TempDir::new("capabilities");
    let output = run(&["capabilities", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["schema_version"], "rdm.agent/v1");
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "capabilities");
    assert_eq!(value["task_id"], Value::Null);
    assert_eq!(value["error"], Value::Null);
    assert_eq!(value["data"]["limits"]["max_connections"], 32);
    assert!(value["data"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool == "rdm_download"));
    assert!(value["data"]["features"]
        .as_array()
        .unwrap()
        .iter()
        .any(|feature| feature == "selftest"));
    assert!(value["meta"]["elapsed_ms"].is_u64());
    assert!(value["meta"]["ts"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn json_flag_is_ndjson_alias() {
    let state = TempDir::new("alias");
    let output = run(&["capabilities", "--json"], state.path()).await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let lines = output.json_lines();
    assert_eq!(lines.len(), 1, "capabilities should emit one line");
    let line = &lines[0];
    assert_eq!(line["type"], "result");
    assert_eq!(line["schema_version"], "rdm.agent/v1");
    assert_eq!(line["ok"], true);
    assert_eq!(line["command"], "capabilities");
    assert_eq!(line["seq"], 1);
    assert_eq!(line["data"]["limits"]["max_connections"], 32);

    // `--json` wins over `--output text` (documented alias precedence).
    let output = run(
        &["capabilities", "--output", "text", "--json"],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0);
    assert_eq!(output.json_lines()[0]["type"], "result");
}

#[tokio::test]
async fn schema_bundle_is_complete() {
    let state = TempDir::new("schema");
    let output = run(&["schema", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["data"]["schema_version"], "rdm.agent/v1");
    for section in ["envelope", "error", "task", "task_summary", "task_event"] {
        assert!(
            value["data"][section]["properties"].is_object(),
            "schema section `{section}` missing properties: {}",
            value["data"][section]
        );
    }
    let tools = value["data"]["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        value["data"]["tools_count"].as_u64().unwrap() as usize
    );
    assert!(tools.iter().any(|tool| tool["name"] == "rdm_download"));
    assert!(value["data"]["error"]["properties"]["code"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .any(|code| code == "invalid_url"));
    assert!(value["data"]["task"]["properties"]["state"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .any(|state| state == "completed"));
}

// ---------------------------------------------------------------------------
// doctor / selftest / bench
// ---------------------------------------------------------------------------

#[tokio::test]
async fn doctor_reports_structured_checks() {
    let state = TempDir::new("doctor");
    let output = run(&["doctor", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    let checks = value["data"]["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    let ids: Vec<&str> = checks
        .iter()
        .filter_map(|check| check["id"].as_str())
        .collect();
    for expected in [
        "env.os",
        "disk.write",
        "disk.space",
        "proxy.env",
        "policy",
        "state.dir",
        "gui.ipc",
        "firefox.native_host",
        "network.dns",
        "network.http",
        "network.range",
    ] {
        assert!(
            ids.contains(&expected),
            "missing doctor check `{expected}`: {ids:?}"
        );
    }
    for check in checks {
        let status = check["status"].as_str().unwrap();
        assert!(
            matches!(status, "pass" | "warn" | "fail" | "skip"),
            "bad status `{status}`"
        );
        assert!(check["summary"].is_string());
    }
    let summary = &value["data"]["summary"];
    let total: u64 = ["pass", "warn", "fail", "skip"]
        .iter()
        .map(|key| summary[*key].as_u64().unwrap())
        .sum();
    assert_eq!(total, checks.len() as u64);
    assert_eq!(summary["ok"], true);
}

#[tokio::test]
async fn doctor_url_checks_network() {
    let fixture = fixture(256 * 1024).await;
    let state = TempDir::new("doctor-url");
    let output = run(
        &["doctor", "--url", &fixture.file_url(), "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    let checks = value["data"]["checks"].as_array().unwrap();
    let by_id = |id: &str| {
        checks
            .iter()
            .find(|check| check["id"] == id)
            .unwrap_or_else(|| panic!("missing check {id}"))
    };
    assert_eq!(by_id("network.dns")["status"], "pass");
    assert_eq!(by_id("network.http")["status"], "pass");
    assert_eq!(by_id("network.range")["status"], "pass");
    fixture.shutdown().await;
}

#[tokio::test]
async fn selftest_passes_all_cases() {
    let state = TempDir::new("selftest");
    let output = run(&["selftest", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 0, "selftest failed: {}", output.stdout);
    let value = output.json();
    assert_eq!(value["ok"], true);
    let cases = value["data"]["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    assert_eq!(value["data"]["passed"], 4);
    assert_eq!(value["data"]["failed"], 0);
    let ids: Vec<&str> = cases
        .iter()
        .map(|case| case["id"].as_str().unwrap())
        .collect();
    for expected in [
        "multi_connection_hash",
        "range_ignored_fallback",
        "resume_missing_chunks",
        "checksum_no_residue",
    ] {
        assert!(ids.contains(&expected), "missing case {expected}");
    }
}

#[tokio::test]
async fn bench_reports_levels_and_speedup() {
    let state = TempDir::new("bench");
    let output = run(
        &[
            "bench",
            "--size",
            "1M",
            "--connections",
            "1,2",
            "--runs",
            "1",
            "--delay-ms",
            "20",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    let levels = value["data"]["levels"].as_array().unwrap();
    assert_eq!(levels.len(), 2);
    assert_eq!(levels[0]["connections"], 1);
    assert_eq!(levels[1]["connections"], 2);
    assert_eq!(value["data"]["hash_consistent"], true);
    assert_eq!(levels[0]["sha256"], levels[1]["sha256"]);
    assert!(
        levels[1]["speedup_vs_base"].as_f64().unwrap() > 1.2,
        "2 connections should beat 1 on a throttled fixture: {}",
        value["data"]
    );
}

// ---------------------------------------------------------------------------
// probe / download
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_reports_metadata_envelope() {
    let fixture = TestServer::start(
        Config::new(64 * 1024)
            .with_etag("\"v1\"")
            .with_content_disposition("attachment; filename=\"report.bin\""),
    )
    .await
    .unwrap();
    let state = TempDir::new("probe");
    let output = run(
        &["probe", &fixture.file_url(), "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "probe");
    assert_eq!(value["data"]["total_bytes"], 64 * 1024);
    assert_eq!(value["data"]["supports_ranges"], true);
    assert_eq!(value["data"]["file_name"], "report.bin");
    assert!(value["data"]["http_version"]
        .as_str()
        .unwrap()
        .contains("1.1"));
    fixture.shutdown().await;
}

#[tokio::test]
async fn probe_invalid_url_exit_2() {
    let state = TempDir::new("probe-bad");
    let output = run(&["probe", "not-a-url", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 2);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "invalid_url");
    assert!(value["error"]["hint"].is_string());
}

#[tokio::test]
async fn download_creates_queryable_task() {
    let fixture = TestServer::start(Config::new(384 * 1024)).await.unwrap();
    let state = TempDir::new("download");
    let out_dir = TempDir::new("download-out");
    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out_dir.path().to_str().unwrap(),
            "-c",
            "4",
            "--cookie",
            "session=SUPERSECRET123",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert_no_secret(&output, "SUPERSECRET123");

    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "download");
    assert_eq!(value["data"]["state"], "completed");
    assert_eq!(value["data"]["total_bytes"], 384 * 1024);
    let task_id = value["task_id"].as_str().unwrap().to_string();
    let path = value["data"]["result"]["path"].as_str().unwrap();
    assert!(Path::new(path).exists(), "downloaded file missing: {path}");

    // Task is durable and queryable.
    let detail = run(
        &["task", &task_id, "--events", "10", "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(detail.code, 0, "stderr: {}", detail.stderr);
    assert_no_secret(&detail, "SUPERSECRET123");
    let detail_value = detail.json();
    assert_eq!(detail_value["data"]["task"]["id"], task_id.as_str());
    assert_eq!(detail_value["data"]["task"]["state"], "completed");
    assert!(!detail_value["data"]["events"]
        .as_array()
        .unwrap()
        .is_empty());

    let list = run(&["tasks", "--output", "json"], state.path()).await;
    assert_eq!(list.code, 0);
    let list_value = list.json();
    assert!(list_value["data"]["count"].as_u64().unwrap() >= 1);

    fixture.shutdown().await;
}

#[tokio::test]
async fn download_ndjson_streams_contract_events() {
    let fixture = fixture(256 * 1024).await;
    let state = TempDir::new("download-ndjson");
    let out_dir = TempDir::new("download-ndjson-out");
    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out_dir.path().to_str().unwrap(),
            "-c",
            "2",
            "--json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let lines = output.json_lines();
    let types: Vec<&str> = lines
        .iter()
        .map(|line| line["type"].as_str().unwrap())
        .collect();
    for expected in ["probe", "metadata", "started", "completed", "result"] {
        assert!(
            types.contains(&expected),
            "missing event `{expected}` in {types:?}"
        );
    }
    assert_eq!(*types.last().unwrap(), "result");
    let mut last_seq = 0u64;
    let task_id = lines[0]["task_id"].as_str().unwrap();
    assert!(!task_id.is_empty());
    for line in &lines {
        assert_eq!(line["schema_version"], "rdm.agent/v1");
        assert_eq!(line["task_id"], task_id);
        let seq = line["seq"].as_u64().unwrap();
        assert!(seq > last_seq, "seq must increase");
        last_seq = seq;
        assert!(line["ts"].as_str().unwrap().ends_with('Z'));
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn download_checksum_mismatch_exit_2_no_residue() {
    let fixture = fixture(256 * 1024).await;
    let state = TempDir::new("download-checksum");
    let out_dir = TempDir::new("download-checksum-out");
    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out_dir.path().to_str().unwrap(),
            "--sha256",
            &"0".repeat(64),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 2);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "checksum_mismatch");
    let leftovers = std::fs::read_dir(out_dir.path()).unwrap().count();
    assert_eq!(leftovers, 0, "checksum failure must leave no files");
    fixture.shutdown().await;
}

#[tokio::test]
async fn download_text_mode_still_works() {
    let fixture = fixture(192 * 1024).await;
    let state = TempDir::new("download-text");
    let out_dir = TempDir::new("download-text-out");
    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out_dir.path().to_str().unwrap(),
            "--output",
            "text",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert!(output.stdout.contains("saved"), "stdout: {}", output.stdout);
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// run (batch)
// ---------------------------------------------------------------------------

fn write_batch(state_dir: &Path, value: &Value) -> PathBuf {
    let path = state_dir.join("batch.json");
    std::fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    path
}

#[tokio::test]
async fn run_batch_executes_and_redacts() {
    let fixture = fixture(256 * 1024).await;
    let state = TempDir::new("run");
    let out_dir = TempDir::new("run-out");
    let batch = serde_json::json!({
        "schema_version": "rdm.agent/v1",
        "tasks": [
            {"url": fixture.file_url(), "dest_dir": out_dir.path(), "file_name": "a.bin", "connections": 2, "idempotency_key": "cli-test-a"},
            {"url": fixture.file_url(), "dest_dir": out_dir.path(), "file_name": "b.bin", "cookie": "session=BATCHSECRET"}
        ],
        "max_concurrency": 2,
        "stop_on_error": false
    });
    let batch_path = write_batch(state.path(), &batch);
    let output = run(
        &[
            "run",
            "--input",
            batch_path.to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert_no_secret(&output, "BATCHSECRET");
    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["completed"], 2);
    assert_eq!(value["data"]["failed"], 0);
    let tasks = value["data"]["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    for task in tasks {
        assert_eq!(task["state"], "completed");
        assert!(!task["result"]["path"].as_str().unwrap().is_empty());
        assert!(
            task.get("request").is_none(),
            "raw request must not be exposed"
        );
    }
    assert!(!value["data"]["warnings"].as_array().unwrap().is_empty());

    // Re-running the batch replays the idempotent task instead of downloading.
    let output = run(
        &[
            "run",
            "--input",
            batch_path.to_str().unwrap(),
            "--wait",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["data"]["reused"], 1);

    fixture.shutdown().await;
}

#[tokio::test]
async fn run_accepts_stdin_and_rejects_invalid_json() {
    let fixture = fixture(64 * 1024).await;
    let state = TempDir::new("run-stdin");
    let out_dir = TempDir::new("run-stdin-out");
    let batch = serde_json::json!({
        "tasks": [{"url": fixture.file_url(), "dest_dir": out_dir.path()}]
    });
    let text = serde_json::to_string(&batch).unwrap();
    let output = run_with_stdin(
        &["run", "--input", "-", "--output", "json"],
        state.path(),
        Some(&text),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert_eq!(output.json()["data"]["completed"], 1);

    let output = run_with_stdin(
        &["run", "--input", "-", "--output", "json"],
        state.path(),
        Some("{not json"),
    )
    .await;
    assert_eq!(output.code, 2);
    assert_eq!(output.json()["error"]["code"], "invalid_request");

    fixture.shutdown().await;
}

#[tokio::test]
async fn run_without_input_file_exits_2() {
    let state = TempDir::new("run-missing");
    let output = run(
        &[
            "run",
            "--input",
            "definitely-missing.json",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 2);
    assert_eq!(output.json()["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn run_policy_denied_exit_4() {
    let fixture = fixture(64 * 1024).await;
    let state = TempDir::new("run-policy");
    let out_dir = TempDir::new("run-policy-out");
    std::fs::write(
        state.path().join("policy.json"),
        serde_json::json!({
            "schema_version": "rdm.agent/v1",
            "allowed_hosts": ["example.com"],
            "allowed_dirs": [out_dir.path()],
            "max_bytes_per_task": null,
            "require_approval_over_bytes": null,
            "max_concurrent_tasks": 2,
            "default_connections": 2,
            "allow_private_hosts": true,
            "allow_overwrite": false
        })
        .to_string(),
    )
    .unwrap();
    let batch = serde_json::json!({
        "tasks": [{"url": fixture.file_url(), "dest_dir": out_dir.path()}]
    });
    let batch_path = write_batch(state.path(), &batch);
    let output = run(
        &[
            "run",
            "--input",
            batch_path.to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(
        output.code, 4,
        "stderr: {}; stdout: {}",
        output.stderr, output.stdout
    );
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "policy_denied");
    assert_eq!(value["data"]["failed"], 1);
    assert_eq!(std::fs::read_dir(out_dir.path()).unwrap().count(), 0);
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// tasks / state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tasks_empty_and_not_found() {
    let state = TempDir::new("tasks-empty");
    let output = run(&["tasks", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["count"], 0);

    let output = run(&["task", "tsk_nope", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 2);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "not_found");

    let output = run(&["task", "../escape", "--output", "json"], state.path()).await;
    assert_eq!(output.code, 2);
    assert_eq!(output.json()["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn state_inspect_and_repair() {
    let state = TempDir::new("state");
    let scan = TempDir::new("state-scan");
    std::fs::write(scan.path().join("orphan.bin.part"), b"partial").unwrap();
    std::fs::write(scan.path().join("bad.bin.part"), b"partial").unwrap();
    std::fs::write(scan.path().join("bad.bin.part.json"), b"not-json").unwrap();
    std::fs::write(
        scan.path().join("orphanstate.bin.part.json"),
        br#"{"version":1}"#,
    )
    .unwrap();

    let output = run(
        &[
            "state",
            "inspect",
            "--dir",
            scan.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    let issues = value["data"]["issues"].as_array().unwrap();
    let kinds: Vec<&str> = issues
        .iter()
        .map(|issue| issue["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"orphan_part"));
    assert!(kinds.contains(&"orphan_state"));
    assert!(kinds.contains(&"corrupt_state"));
    assert_eq!(value["data"]["dry_run"], true);

    // Dry run leaves everything untouched.
    let output = run(
        &[
            "state",
            "repair",
            "--dir",
            scan.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0);
    let value = output.json();
    assert_eq!(value["data"]["dry_run"], true);
    assert_eq!(value["data"]["applied"], 0);
    assert!(value["data"]["planned"].as_u64().unwrap() >= 3);
    assert_eq!(std::fs::read_dir(scan.path()).unwrap().count(), 4);

    // Applying quarantines the problematic files.
    let output = run(
        &[
            "state",
            "repair",
            "--dir",
            scan.path().to_str().unwrap(),
            "--no-dry-run",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["data"]["dry_run"], false);
    assert!(value["data"]["applied"].as_u64().unwrap() >= 3);
    assert_eq!(value["data"]["failed"], 0);
    let quarantined = walk_files(&scan.path().join(".rdm-quarantine"));
    assert!(
        !quarantined.is_empty(),
        "quarantine dir should contain the files"
    );
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// misc contract checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_arguments_use_clap_exit_code() {
    let state = TempDir::new("clap");
    let output = run(&["definitely-not-a-command"], state.path()).await;
    assert_eq!(output.code, 2, "clap errors should exit 2");
    assert!(!output.stderr.is_empty());
}

#[tokio::test]
async fn download_rejects_bad_sha256() {
    let state = TempDir::new("bad-sha");
    let output = run(
        &[
            "download",
            "http://127.0.0.1:1/never",
            "--sha256",
            "abc",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 2);
    assert_eq!(output.json()["error"]["code"], "invalid_request");
}

// ---------------------------------------------------------------------------
// URL validation must precede policy (task-12 regression)
// ---------------------------------------------------------------------------

const BAD_URLS: [&str; 5] = [
    "ftp://example.com/x",
    "file:///C:/x",
    "example.com/x",
    "",
    "http://[broken",
];

async fn assert_invalid_url(args: &[&str], state_dir: &Path) {
    let output = run(args, state_dir).await;
    assert_eq!(
        output.code, 2,
        "expected invalid_url/exit2 for {args:?}: stdout={} stderr={}",
        output.stdout, output.stderr
    );
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(
        value["error"]["code"], "invalid_url",
        "expected invalid_url for {args:?}: {}",
        value["error"]
    );
    assert!(value["error"]["hint"].is_string());
}

#[tokio::test]
async fn probe_and_download_reject_bad_urls_as_invalid_url() {
    let state = TempDir::new("url-validation");
    for url in BAD_URLS {
        assert_invalid_url(&["probe", url, "--output", "json"], state.path()).await;
        assert_invalid_url(&["download", url, "--output", "json"], state.path()).await;
    }
}

#[tokio::test]
async fn invalid_url_wins_over_policy_denial() {
    let state = TempDir::new("url-vs-policy");
    let out = TempDir::new("url-vs-policy-out");
    write_policy(state.path(), public_only_policy(out.path()));

    // Scheme error: invalid_url (exit 2), not policy_denied (exit 4).
    assert_invalid_url(
        &["probe", "ftp://example.com/x", "--output", "json"],
        state.path(),
    )
    .await;
    assert_invalid_url(
        &[
            "download",
            "ftp://example.com/x",
            "-d",
            out.path().to_str().unwrap(),
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;

    // A syntactically valid but policy-blocked URL still yields exit 4.
    let output = run(
        &["probe", "http://127.0.0.1:1/x", "--output", "json"],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stdout: {}", output.stdout);
    assert_eq!(output.json()["error"]["code"], "policy_denied");
}

#[tokio::test]
async fn run_rejects_bad_urls_as_invalid_url() {
    let state = TempDir::new("url-validation-run");
    for url in BAD_URLS {
        let batch = serde_json::json!({
            "schema_version": "rdm.agent/v1",
            "tasks": [{"url": url}]
        });
        let batch_path = write_batch(state.path(), &batch);
        let output = run(
            &[
                "run",
                "--input",
                batch_path.to_str().unwrap(),
                "--output",
                "json",
            ],
            state.path(),
        )
        .await;
        assert_eq!(
            output.code, 2,
            "run should reject {url:?} as invalid_url: stdout={} stderr={}",
            output.stdout, output.stderr
        );
        let value = output.json();
        assert_eq!(
            value["error"]["code"], "invalid_url",
            "run {url:?}: {}",
            value["error"]
        );
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("task #0:"));
    }
}
