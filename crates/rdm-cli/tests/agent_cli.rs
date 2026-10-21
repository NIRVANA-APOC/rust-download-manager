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
        // Never let a developer's real AI key leak into the isolated tests.
        .env_remove("RDM_AI_API_KEY")
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

/// Baseline policy with several allowed directories (resolver recipes live
/// under `<LOCALAPPDATA>/RDM`, the `--download --dir` target is separate).
fn allowed_policy_with_dirs(dirs: &[&Path]) -> serde_json::Value {
    let mut policy = allowed_policy(dirs.first().copied().unwrap_or(Path::new(".")));
    policy["allowed_dirs"] = serde_json::json!(dirs);
    policy
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
    assert!(
        value["data"]["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "dynamic-connections"),
        "capabilities.features should advertise dynamic-connections"
    );
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

// ---------------------------------------------------------------------------
// selftest: read-only recipe self-test report (v1.4, architecture §11.10)
// ---------------------------------------------------------------------------

/// Recipe whose pure matching self-tests pass (one match, one no-match, one skip).
const SELFTEST_PASS_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "selftest-pass",
  "version": 1,
  "name": "Selftest passing recipe",
  "match": { "hosts": ["example.com"], "url_pattern": "^https://example\\.com/file/(?P<id>[0-9]+)$" },
  "steps": [
    { "id": "fetch", "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 1024 } }
  ],
  "emit": { "url": "{{input_url}}" },
  "tests": [
    { "name": "matches-file-url", "input_url": "https://example.com/file/42" },
    { "name": "rejects-other-host", "input_url": "https://mirror.example.net/file/42",
      "expect_no_match": true },
    { "name": "skipped-needs-login", "input_url": "https://example.com/file/43",
      "skip": "requires login" }
  ]
}"#;

/// Recipe with one deliberately failing `expect_no_match` self-test.
const SELFTEST_FAILING_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "selftest-failing",
  "version": 2,
  "name": "Selftest failing recipe",
  "match": { "hosts": ["fail.example"], "url_pattern": "^https://fail\\.example/" },
  "steps": [
    { "id": "fetch", "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 1024 } }
  ],
  "emit": { "url": "{{input_url}}" },
  "tests": [
    { "name": "must-not-match", "input_url": "https://fail.example/file/1",
      "expect_no_match": true }
  ]
}"#;

fn write_selftest_recipes(local: &Path) {
    let recipes = local.join("RDM").join("resolve").join("recipes");
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(recipes.join("selftest-pass@1.json"), SELFTEST_PASS_RECIPE).unwrap();
    std::fs::write(
        recipes.join("selftest-failing@2.json"),
        SELFTEST_FAILING_RECIPE,
    )
    .unwrap();
}

fn localappdata_env(local: &Path) -> Vec<(&'static str, String)> {
    vec![("LOCALAPPDATA", local.to_string_lossy().into_owned())]
}

#[tokio::test]
async fn selftest_reports_recipe_self_tests_without_changing_engine_result() {
    let local = TempDir::new("selftest-recipes");
    write_selftest_recipes(local.path());
    let output = run_with_env(
        &["selftest", "--output", "json"],
        &localappdata_env(local.path()),
    )
    .await;
    assert_eq!(
        output.code, 0,
        "selftest failed: {}\n{}",
        output.stdout, output.stderr
    );
    let value = output.json();
    // Engine result is untouched by recipe failures.
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["passed"], 4);
    assert_eq!(value["data"]["failed"], 0);
    assert_eq!(value["data"]["cases"].as_array().unwrap().len(), 4);
    let recipes = &value["data"]["recipes"];
    let expected_dir = local.path().join("RDM").join("resolve").join("recipes");
    assert_eq!(recipes["dir"], expected_dir.display().to_string());
    assert_eq!(recipes["checked"], 2, "{value}");
    assert_eq!(recipes["passed"], 2, "{value}");
    assert_eq!(recipes["failed"], 1, "{value}");
    assert_eq!(recipes["skipped"], 1, "{value}");
    let failures = recipes["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "{value}");
    assert_eq!(failures[0]["recipe_id"], "selftest-failing");
    assert_eq!(failures[0]["version"], 2);
    assert_eq!(failures[0]["test"], "must-not-match");
    assert!(
        failures[0]["message"]
            .as_str()
            .unwrap()
            .contains("must not match but did"),
        "{value}"
    );

    // Text mode prints the one-line report plus the failure row.
    let text = run_with_env(
        &["selftest", "--output", "text"],
        &localappdata_env(local.path()),
    )
    .await;
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(
        text.stdout
            .contains("recipes: 2 checked / 2 passed / 1 failed / 1 skipped"),
        "text report missing: {}",
        text.stdout
    );
    assert!(
        text.stdout.contains("selftest-failing"),
        "text mode must list up to five failures: {}",
        text.stdout
    );
}

#[tokio::test]
async fn selftest_without_recipe_library_reports_checked_zero() {
    let local = TempDir::new("selftest-no-recipes");
    let output = run_with_env(
        &["selftest", "--output", "json"],
        &localappdata_env(local.path()),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["passed"], 4);
    let recipes = &value["data"]["recipes"];
    assert_eq!(recipes["checked"], 0, "{value}");
    assert_eq!(recipes["passed"], 0);
    assert_eq!(recipes["failed"], 0);
    assert_eq!(recipes["skipped"], 0);
    assert!(recipes["failures"].as_array().unwrap().is_empty());
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

// ---------------------------------------------------------------------------
// resolve (v1.2 link resolution: recipe replay + built-in verification)
// ---------------------------------------------------------------------------

/// Minimal direct-link recipe: match the fixture host, fetch the input URL
/// once and emit it as the resource; the resolver's verifier does the rest.
const DIRECT_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "fixture-direct",
  "version": 1,
  "name": "Fixture direct link",
  "match": { "hosts": ["127.0.0.1"], "url_pattern": "^(?P<input_url>http://.+)$" },
  "limits": { "requests": 8, "bytes_read": 262144, "wall_ms": 15000, "redirects": 3 },
  "steps": [
    {
      "id": "fetch",
      "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 1024 },
      "expect": { "status_in": [200, 206] }
    }
  ],
  "emit": { "url": "{{input_url}}" }
}"#;

fn write_local_policy(local: &Path, policy: Value) {
    let agent = local.join("RDM").join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("policy.json"),
        serde_json::to_vec_pretty(&policy).unwrap(),
    )
    .unwrap();
}

fn write_resolve_recipe(local: &Path) {
    let recipes = local.join("RDM").join("resolve").join("recipes");
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(recipes.join("fixture-direct@1.json"), DIRECT_RECIPE).unwrap();
}

/// Run `rdm` with an isolated `%LOCALAPPDATA%`: both the task store
/// (`<local>/RDM/agent`) and the resolver recipe library
/// (`<local>/RDM/resolve/recipes`) live under it, so the real user profile is
/// never touched.
async fn run_resolve(args: &[&str], local: &Path) -> RunOutput {
    run_with_env(
        args,
        &[("LOCALAPPDATA", local.to_string_lossy().into_owned())],
    )
    .await
}

#[tokio::test]
async fn resolve_fixture_direct_link_succeeds() {
    let fixture = fixture(96 * 1024).await;
    let local = TempDir::new("resolve-ok");
    write_local_policy(local.path(), allowed_policy(local.path()));
    write_resolve_recipe(local.path());

    let output = run_resolve(
        &["resolve", &fixture.file_url(), "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "resolve");
    assert_eq!(value["error"], Value::Null);
    let data = &value["data"];
    assert_eq!(data["status"], "succeeded");
    assert!(data["run_id"].as_str().unwrap().starts_with("run_"));
    assert_eq!(data["recipe"]["id"], "fixture-direct");
    assert_eq!(data["diagnostics"]["llm_calls"], 0);
    let manifest = &data["manifest"];
    assert_eq!(manifest["schema_version"], "rdm.resolve/v1");
    let resources = manifest["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["url"], fixture.file_url());
    assert_eq!(resources[0]["verification"]["status"], "verified");
    assert_eq!(resources[0]["size_bytes"], 96 * 1024);
    assert_eq!(resources[0]["supports_ranges"], true);
    assert_eq!(resources[0]["transfer"]["resumable"], true);
    // Resolution is read-only: it must not create a task.
    let tasks = run_resolve(&["tasks", "--output", "json"], local.path()).await;
    let tasks_value = tasks.json();
    assert_eq!(tasks_value["ok"], true);
    assert!(
        tasks_value["data"]["tasks"].as_array().unwrap().is_empty(),
        "resolve must not create tasks: {tasks_value}"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_invalid_url_is_invalid_url_exit_2() {
    let local = TempDir::new("resolve-bad-url");
    write_local_policy(local.path(), allowed_policy(local.path()));
    let output = run_resolve(&["resolve", "not-a-url", "--output", "json"], local.path()).await;
    assert_eq!(output.code, 2);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "invalid_url");
    assert_eq!(value["data"], Value::Null);
}

#[tokio::test]
async fn resolve_without_recipe_is_resolve_failed() {
    // A fixture HTML page with no matching recipe must fail loudly as
    // resolve_failed/not_a_file (the built-in direct verifier rejects pages) —
    // never silently succeed.
    let mut config = Config::new(16 * 1024);
    config.content_type = "text/html".to_string();
    let fixture = TestServer::start(config).await.unwrap();
    let local = TempDir::new("resolve-no-recipe");
    write_local_policy(local.path(), allowed_policy(local.path()));
    let html = run_resolve(
        &["resolve", &fixture.file_url(), "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(html.code, 2, "stderr: {}", html.stderr);
    let value = html.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "resolve_failed");
    assert_eq!(value["error"]["details"]["kind"], "not_a_file");
    assert!(value["error"]["details"]["run_id"]
        .as_str()
        .unwrap()
        .starts_with("run_"));
    assert_eq!(value["error"]["retryable"], false);

    // `recipe-only` promises no built-in fallback: an empty recipe library
    // yields recipe_no_match without touching the network.
    let recipe_only = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--mode",
            "recipe-only",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(recipe_only.code, 2, "stderr: {}", recipe_only.stderr);
    let value = recipe_only.json();
    assert_eq!(value["error"]["code"], "resolve_failed");
    assert_eq!(value["error"]["details"]["kind"], "recipe_no_match");
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_builtin_direct_verification_succeeds_without_recipe() {
    // Contract §8.2: `auto` mode verifies a direct file itself when no recipe
    // matches (no model, no task, no recipe library needed).
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("resolve-builtin");
    write_local_policy(local.path(), allowed_policy(local.path()));
    let output = run_resolve(
        &["resolve", &fixture.file_url(), "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    let data = &value["data"];
    assert_eq!(data["status"], "succeeded");
    assert_eq!(data["recipe"], Value::Null, "builtin path has no recipe");
    assert_eq!(data["diagnostics"]["llm_calls"], 0);
    assert_eq!(
        data["manifest"]["resources"][0]["verification"]["status"],
        "verified"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_policy_denied_private_host_exit_4() {
    let fixture = fixture(32 * 1024).await;
    let local = TempDir::new("resolve-policy");
    write_local_policy(local.path(), public_only_policy(local.path()));
    let output = run_resolve(
        &["resolve", &fixture.file_url(), "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "policy_denied");
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// resolve --download (v1.3: validated metadata handed to the engine)
// ---------------------------------------------------------------------------

fn output_dir_arg(out: &TempDir) -> &str {
    out.path().to_str().unwrap()
}

#[tokio::test]
async fn resolve_download_reuses_verified_meta_without_a_second_probe() {
    // Auto mode verifies the direct link (HEAD + bounded ranged GET); the
    // hand-off must then transfer with `download_with_meta` — no second probe.
    let fixture = fixture(96 * 1024).await;
    let local = TempDir::new("resolve-download");
    let out = TempDir::new("resolve-download-out");
    write_local_policy(
        local.path(),
        allowed_policy_with_dirs(&[local.path(), out.path()]),
    );

    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true, "{value}");
    // The ResolveOutcome fields stay at the top level of `data`.
    assert_eq!(value["data"]["status"], "succeeded");
    assert!(value["data"]["manifest"]["resources"].is_array());
    let download = &value["data"]["download"];
    assert!(download.is_object(), "data.download missing: {value}");
    let path = PathBuf::from(download["path"].as_str().expect("download.path"));
    assert!(path.is_file(), "{}", path.display());
    assert_eq!(std::fs::read(&path).unwrap(), fixture.content());
    assert_eq!(download["total_bytes"], 96 * 1024);
    assert_eq!(download["connections"], 1);
    assert_eq!(download["range_supported"], true);

    // Exactly: verifier HEAD, verifier ranged GET, transfer GET. A second
    // probe would add a HEAD between the verification and the transfer.
    let stats = fixture.stats();
    assert_eq!(stats.methods, vec!["HEAD", "GET", "GET"], "{stats:?}");
    assert_eq!(stats.head_requests, 1, "no second probe: {stats:?}");
    fixture.shutdown().await;

    // Nothing extra was created next to the payload.
    assert_eq!(walk_files(out.path()), vec![path.clone()]);
}

#[tokio::test]
async fn resolve_download_single_stream_is_one_rangeless_get() {
    // A server without Range support maps to `single_stream=true`: the
    // transfer is exactly one GET and it must not carry a Range header.
    let mut config = Config::new(64 * 1024);
    config.supports_ranges = false;
    let fixture = TestServer::start(config).await.unwrap();
    let local = TempDir::new("resolve-single-stream");
    let out = TempDir::new("resolve-single-stream-out");
    write_local_policy(
        local.path(),
        allowed_policy_with_dirs(&[local.path(), out.path()]),
    );

    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true, "{value}");
    let item = &value["data"]["manifest"]["resources"][0];
    // The manifest hint keeps the recipe/transfer facts; the *effective*
    // single-stream shape is derived by `to_download_request` from the missing
    // Range support (`supports_ranges=false` ⇒ single_stream + connections=1).
    assert_eq!(item["supports_ranges"], false);
    assert_eq!(item["transfer"]["resumable"], false);
    let download = &value["data"]["download"];
    assert_eq!(download["connections"], 1);
    assert_eq!(download["range_supported"], false);
    let path = PathBuf::from(download["path"].as_str().expect("download.path"));
    assert_eq!(std::fs::read(&path).unwrap(), fixture.content());

    // verifier HEAD + verifier ranged GET + the single rangeless transfer GET.
    let stats = fixture.stats();
    assert_eq!(stats.methods, vec!["HEAD", "GET", "GET"], "{stats:?}");
    assert_eq!(stats.head_requests, 1, "no second probe: {stats:?}");
    assert_eq!(
        stats.range_headers.len(),
        1,
        "only the verifier may send a Range header (transfer is single-stream): {stats:?}"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_download_without_a_downloadable_resource_creates_nothing() {
    // A recipe that emits an HTML page verifies as `html_page`: the run is
    // `partial` (exit 0 without --download) but `--download` must fail and
    // must not create a file.
    let mut config = Config::new(8 * 1024);
    config.content_type = "text/html".to_string();
    let fixture = TestServer::start(config).await.unwrap();
    let local = TempDir::new("resolve-download-partial");
    let out = TempDir::new("resolve-download-partial-out");
    write_local_policy(
        local.path(),
        allowed_policy_with_dirs(&[local.path(), out.path()]),
    );
    write_resolve_recipe(local.path());

    let plain = run_resolve(
        &["resolve", &fixture.file_url(), "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(plain.code, 0, "stderr: {}", plain.stderr);
    assert_eq!(plain.json()["data"]["status"], "partial");

    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 2, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "resolve_failed");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no downloadable resource"),
        "{value}"
    );
    assert_eq!(value["error"]["details"]["kind"], "not_a_file");
    assert!(
        walk_files(out.path()).is_empty(),
        "a failed --download must not create files"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_download_text_mode_prints_summary() {
    let fixture = fixture(32 * 1024).await;
    let local = TempDir::new("resolve-download-text");
    let out = TempDir::new("resolve-download-text-out");
    write_local_policy(
        local.path(),
        allowed_policy_with_dirs(&[local.path(), out.path()]),
    );
    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "text",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert!(
        output.stdout.contains("downloaded:"),
        "text mode must print a download summary: {}",
        output.stdout
    );
    assert!(output.stdout.contains("status:        Succeeded"));
    assert!(!walk_files(out.path()).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_download_policy_denied_outside_allowed_dirs() {
    // `--download` must not be a way around the agent policy: the same
    // destination-directory gate as `rdm download` runs before any byte lands.
    let fixture = fixture(32 * 1024).await;
    let local = TempDir::new("resolve-dl-policy-dir");
    let out = TempDir::new("resolve-dl-policy-dir-out");
    // allowed_dirs contains only the state-dir root, NOT the output directory.
    write_local_policy(local.path(), allowed_policy(local.path()));
    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "policy_denied");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside policy.allowed_dirs"),
        "{value}"
    );
    assert!(
        walk_files(out.path()).is_empty(),
        "a policy-denied --download must not create files"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_download_policy_denied_over_max_bytes_per_task() {
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("resolve-dl-policy-size");
    let out = TempDir::new("resolve-dl-policy-size-out");
    let mut policy = allowed_policy_with_dirs(&[local.path(), out.path()]);
    policy["max_bytes_per_task"] = serde_json::json!(1024);
    write_local_policy(local.path(), policy);
    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["error"]["code"], "policy_denied");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("size denied by policy"),
        "{value}"
    );
    assert_eq!(value["error"]["details"]["total_bytes"], 64 * 1024);
    assert!(walk_files(out.path()).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_download_policy_needs_approval_over_threshold() {
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("resolve-dl-policy-approval");
    let out = TempDir::new("resolve-dl-policy-approval-out");
    let mut policy = allowed_policy_with_dirs(&[local.path(), out.path()]);
    policy["require_approval_over_bytes"] = serde_json::json!(1024);
    write_local_policy(local.path(), policy);
    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["error"]["code"], "needs_approval");
    assert_eq!(value["error"]["details"]["total_bytes"], 64 * 1024);
    assert!(walk_files(out.path()).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_download_policy_conflict_when_target_exists() {
    // `allow_overwrite=false`: the overwrite gate must run before the transfer,
    // so an existing target is a `conflict` and no bytes are written.
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("resolve-dl-policy-conflict");
    let out = TempDir::new("resolve-dl-policy-conflict-out");
    write_local_policy(
        local.path(),
        allowed_policy_with_dirs(&[local.path(), out.path()]),
    );
    let candidate = out.path().join("file");
    std::fs::write(&candidate, b"existing").unwrap();
    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["error"]["code"], "conflict");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already exists"),
        "{value}"
    );
    // The pre-existing file was not overwritten.
    assert_eq!(std::fs::read(&candidate).unwrap(), b"existing");
    assert_eq!(walk_files(out.path()), vec![candidate]);
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_download_refuses_unknown_length_under_policy() {
    // No Content-Length and no Range: the resource still verifies, but its
    // length is unknown. `rdm download` monitors such streams against
    // max_bytes_per_task; `--download` has no monitor, so under a policy it
    // refuses instead of silently bypassing the size cap.
    let mut config = Config::new(64 * 1024);
    config.no_content_length = true;
    config.supports_ranges = false;
    let fixture = TestServer::start(config).await.unwrap();
    let local = TempDir::new("resolve-dl-unknown-length");
    let out = TempDir::new("resolve-dl-unknown-length-out");
    write_local_policy(
        local.path(),
        allowed_policy_with_dirs(&[local.path(), out.path()]),
    );

    // Plain resolve still succeeds with an unknown size (read-only).
    let plain = run_resolve(
        &["resolve", &fixture.file_url(), "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(plain.code, 0, "stderr: {}", plain.stderr);
    let plain_value = plain.json();
    assert_eq!(plain_value["data"]["status"], "succeeded");
    assert!(
        plain_value["data"]["manifest"]["resources"][0]["size_bytes"].is_null(),
        "{plain_value}"
    );

    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--download",
            "--dir",
            output_dir_arg(&out),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["error"]["code"], "policy_denied");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no Content-Length"),
        "{value}"
    );
    assert_eq!(value["error"]["details"]["reason"], "unknown_length");
    assert!(
        value["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("rdm download"),
        "the hint must point at the streaming-monitor command: {value}"
    );
    assert!(walk_files(out.path()).is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_rejects_out_of_range_max_llm_calls() {
    let local = TempDir::new("resolve-llm-range");
    write_local_policy(local.path(), allowed_policy(local.path()));
    for value in ["0", "33"] {
        let output = run_resolve(
            &[
                "resolve",
                "https://example.com/share/abc",
                "--max-llm-calls",
                value,
                "--output",
                "json",
            ],
            local.path(),
        )
        .await;
        assert_eq!(output.code, 2, "stderr: {}", output.stderr);
        let envelope = output.json();
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["code"], "invalid_request");
    }
}

// ---------------------------------------------------------------------------
// resolve --mode explore (v1.3: user-configured AI, model_unavailable, privacy)
// ---------------------------------------------------------------------------

/// Write the user's AI provider config (`<LOCALAPPDATA>/RDM/ai.json`) so the
/// resolver can reach the scripted mock endpoint.
fn write_ai_config(local: &Path, base_url: &str, api_key: &str) {
    let state = local.join("RDM");
    std::fs::create_dir_all(&state).unwrap();
    let config = serde_json::json!({
        "enabled": true,
        "base_url": base_url,
        "model": "mock-model",
        "api_key": api_key,
        "timeout_secs": 10
    });
    std::fs::write(
        state.join("ai.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
}

/// Minimal share page that links to the fixture's direct file URL.
async fn spawn_share_page(file_url: &str) -> (String, tokio::sync::watch::Sender<bool>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind share page");
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/share/abc");
    let html = format!(
        "<!doctype html><html><head><title>Share page</title></head><body>\
         <h1>RDM fixture share page</h1><a href=\"{file_url}\">download the file</a></body></html>"
    );
    let (shutdown, mut rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = rx.changed() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                break;
            };
            let html = html.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    head.extend_from_slice(&chunk[..read]);
                    if head.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let is_head = head.starts_with(b"HEAD ");
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    html.len()
                )
                .into_bytes();
                if !is_head {
                    response.extend_from_slice(html.as_bytes());
                }
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            });
        }
    });
    (url, shutdown)
}

/// Scripted OpenAI-compatible mock (`POST /chat/completions`).
///
/// Turn 1 asks the resolver to `page_read` the share page; every later turn
/// emits the fixture file URL with the observation id it extracted from the
/// conversation (evidence binding requires a real observation).
struct MockLlm {
    base_url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl MockLlm {
    async fn start(share_url: &str, file_url: &str) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock llm");
        let addr = listener.local_addr().unwrap();
        let share_url = share_url.to_string();
        let file_url = file_url.to_string();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let (shutdown, mut rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = rx.changed() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, _)) = accepted else {
                    break;
                };
                let recorded = std::sync::Arc::clone(&recorded);
                let share_url = share_url.clone();
                let file_url = file_url.clone();
                tokio::spawn(async move {
                    let body = read_http_body(&mut stream).await;
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    recorded.lock().unwrap().push(request.clone());
                    let content = scripted_reply(&request, &share_url, &file_url);
                    let payload = serde_json::json!({
                        "choices": [{"message": {"role": "assistant", "content": content}}]
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    use tokio::io::AsyncWriteExt;
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
            shutdown,
        }
    }

    fn recorded_requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn shutdown(self) {
        let _ = self.shutdown.send(true);
    }
}

async fn read_http_body(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return buffer;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..position]).to_ascii_lowercase();
            let expected = head
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while buffer.len() < position + 4 + expected {
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return buffer[position + 4..].to_vec();
        }
    }
}

fn scripted_reply(request: &Value, share_url: &str, file_url: &str) -> String {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let assistant_turns = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .count();
    if assistant_turns == 0 {
        return serde_json::json!({"action": "page_read", "url": share_url}).to_string();
    }
    let conversation = serde_json::to_string(request).unwrap_or_default();
    match find_observation_id(&conversation) {
        Some(observation) => serde_json::json!({
            "action": "emit_manifest",
            "resources": [{
                "url": file_url,
                "evidence": format!("{observation}#links/0"),
                "name": "file"
            }],
            "notes": ["scripted mock"]
        })
        .to_string(),
        None => serde_json::json!({
            "action": "finish",
            "reason": "mock saw no observation id",
            "kind": "no_resources"
        })
        .to_string(),
    }
}

/// Extract the first real `obs_<pid>_<seq>` id from the conversation.
///
/// The system prompt itself mentions the placeholder forms `obs_...` and
/// `obs_<id>`, so only ids whose trailing segments are numeric count.
fn find_observation_id(conversation: &str) -> Option<String> {
    let mut search = conversation;
    while let Some(start) = search.find("obs_") {
        let rest = &search[start..];
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        let parts: Vec<&str> = candidate.split('_').collect();
        if parts.len() >= 3
            && parts[1..]
                .iter()
                .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        {
            return Some(candidate.to_string());
        }
        search = &rest[end..];
    }
    None
}

#[tokio::test]
async fn resolve_explore_with_configured_ai_succeeds_without_leaking_the_key() {
    let fixture = fixture(64 * 1024).await;
    let (share_url, share_shutdown) = spawn_share_page(&fixture.file_url()).await;
    let mock = MockLlm::start(&share_url, &fixture.file_url()).await;
    let local = TempDir::new("resolve-explore-ai");
    write_local_policy(local.path(), allowed_policy(local.path()));
    write_ai_config(local.path(), &mock.base_url, "test-key-123456");

    let output = run_resolve(
        &[
            "resolve",
            &share_url,
            "--mode",
            "explore",
            "--max-llm-calls",
            "4",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(
        output.code, 0,
        "stdout: {}\nstderr: {}",
        output.stdout, output.stderr
    );
    let value = output.json();
    assert_eq!(value["ok"], true, "{value}");
    let data = &value["data"];
    assert_eq!(data["status"], "succeeded", "{value}");
    // v1.5: a successful, expressible exploration deterministically distills a
    // local draft (`recipe.state=="draft"`); drafts never participate in matching.
    assert_eq!(data["recipe"]["state"], "draft", "{value}");
    assert!(
        data["recipe"]["id"].as_str().unwrap().starts_with("draft_"),
        "{value}"
    );
    let draft_files = walk_files(&local.path().join("RDM").join("resolve").join("drafts"));
    assert_eq!(draft_files.len(), 1, "{draft_files:?}");
    let draft_text = std::fs::read_to_string(&draft_files[0]).unwrap();
    assert!(
        draft_text.contains("rdm.recipe-draft/v1"),
        "draft file must use the draft schema: {draft_text}"
    );
    // The draft is a local asset, but it must never contain the API key either.
    assert!(
        !draft_text.contains("test-key-123456"),
        "draft leaked the key"
    );
    let resources = data["manifest"]["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["url"], fixture.file_url());
    assert_eq!(resources[0]["verification"]["status"], "verified");
    let llm_calls = data["diagnostics"]["llm_calls"].as_u64().unwrap();
    assert!(
        (2..=4).contains(&llm_calls),
        "explore must record real llm calls, got {llm_calls}: {value}"
    );

    // ---- privacy: the API key never appears in output or prompt bodies ----
    assert!(
        !output.stdout.contains("test-key-123456"),
        "stdout leaked the API key"
    );
    assert!(
        !output.stderr.contains("test-key-123456"),
        "stderr leaked the API key: {}",
        output.stderr
    );
    let requests = mock.recorded_requests();
    assert!(!requests.is_empty(), "mock LLM saw no request");
    for (index, request) in requests.iter().enumerate() {
        let text = serde_json::to_string(request).unwrap();
        assert!(
            !text.contains("test-key-123456"),
            "the API key must not be sent in the prompt/body: {text}"
        );
        if index > 0 {
            assert!(
                text.contains("untrusted_content"),
                "tool results must be fed back as untrusted_content: {text}"
            );
        }
    }

    mock.shutdown();
    let _ = share_shutdown.send(true);
    fixture.shutdown().await;
}

#[tokio::test]
async fn resolve_explore_without_ai_config_is_model_unavailable() {
    // A page (HTML) forces the explore path; without ai.json the resolver must
    // fail with model_unavailable and make no model call.
    let mut config = Config::new(8 * 1024);
    config.content_type = "text/html".to_string();
    let fixture = TestServer::start(config).await.unwrap();
    let local = TempDir::new("resolve-explore-noai");
    write_local_policy(local.path(), allowed_policy(local.path()));
    let output = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--mode",
            "explore",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 2, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "resolve_failed");
    assert_eq!(
        value["error"]["details"]["kind"], "model_unavailable",
        "explore without AI must be model_unavailable: {value}"
    );
    fixture.shutdown().await;
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
    // Dynamic-connection contract fields are agent-visible.
    assert_eq!(value["data"]["dynamic_connections"], true);
    assert!(value["data"]["peak_connections"].as_u64().unwrap() >= 1);
    assert!(
        value["data"]["result"]["peak_connections"]
            .as_u64()
            .unwrap()
            >= 1
    );

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
    // The task view carries the dynamic-connection contract fields.
    assert_eq!(detail_value["data"]["task"]["dynamic_connections"], true);
    assert!(
        detail_value["data"]["task"]["peak_connections"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        detail_value["data"]["task"]["result"]["peak_connections"]
            .as_u64()
            .unwrap()
            >= 1
    );
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
    // The completed event reports the realised concurrency peak; the final
    // TaskView exposes both the peak and the dynamic-planning flag.
    let completed = lines
        .iter()
        .find(|line| line["type"] == "completed")
        .expect("completed event");
    assert!(completed["data"]["peak_connections"].as_u64().unwrap() >= 1);
    assert!(completed["data"]["connections"].as_u64().unwrap() >= 1);
    let result = lines.last().unwrap();
    assert!(result["data"]["peak_connections"].as_u64().unwrap() >= 1);
    assert_eq!(result["data"]["dynamic_connections"], true);
    assert!(
        result["data"]["result"]["peak_connections"]
            .as_u64()
            .unwrap()
            >= 1
    );
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

#[tokio::test]
async fn download_single_stream_uses_one_plain_get() {
    // v1.2 / engine `single_stream`: exactly one HEAD probe + one plain `GET`
    // with no `Range` header — the shape one-time/signed links and anti-hotlink
    // servers require.
    let fixture = fixture(128 * 1024).await;
    let state = TempDir::new("single-stream");
    let out_dir = TempDir::new("single-stream-out");
    let output = run(
        &[
            "download",
            &fixture.file_url(),
            "-d",
            out_dir.path().to_str().unwrap(),
            "--single-stream",
            "--output",
            "json",
        ],
        state.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["state"], "completed");
    assert_eq!(value["data"]["connections"], 1);
    assert_eq!(value["data"]["dynamic_connections"], false);
    let stats = fixture.stats();
    assert_eq!(
        stats.methods,
        vec!["HEAD".to_string(), "GET".to_string()],
        "single_stream must be one HEAD probe + one plain GET: {stats:?}"
    );
    assert!(
        stats.range_headers.is_empty(),
        "single_stream must never send a Range header: {:?}",
        stats.range_headers
    );
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
            {"url": fixture.file_url(), "dest_dir": out_dir.path(), "file_name": "a.bin", "connections": 2, "dynamic_connections": false, "idempotency_key": "cli-test-a"},
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
        // Per-task dynamic flag is preserved from the batch input, and the
        // realised peak is reported in both the view and its result.
        assert!(task["peak_connections"].as_u64().unwrap() >= 1);
        assert!(task["result"]["peak_connections"].as_u64().unwrap() >= 1);
    }
    assert_eq!(tasks[0]["dynamic_connections"], false);
    assert_eq!(tasks[1]["dynamic_connections"], true);
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
async fn run_batch_single_stream_is_passed_through() {
    let fixture = fixture(64 * 1024).await;
    let state = TempDir::new("run-single-stream");
    let out_dir = TempDir::new("run-single-stream-out");
    let batch_path = write_batch(
        state.path(),
        &serde_json::json!({
            "tasks": [{
                "url": fixture.file_url(),
                "dest_dir": out_dir.path(),
                "file_name": "single.bin",
                "single_stream": true
            }]
        }),
    );
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
    assert_eq!(value["data"]["completed"], 1);
    assert_eq!(value["data"]["tasks"][0]["state"], "completed");
    let stats = fixture.stats();
    assert_eq!(
        stats.methods,
        vec!["HEAD".to_string(), "GET".to_string()],
        "run task single_stream must be passed through: {stats:?}"
    );
    assert!(
        stats.range_headers.is_empty(),
        "run task single_stream must not send Range headers: {:?}",
        stats.range_headers
    );
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

// ---------------------------------------------------------------------------
// rdm recipe (v1.5 method-library management, architecture §11.11)
// ---------------------------------------------------------------------------

fn resolve_state(local: &Path) -> PathBuf {
    local.join("RDM").join("resolve")
}

fn write_resolve_file(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

/// Minimal valid recipe used only by `rdm recipe check`/`list`/`show` (the
/// steps are never executed by those commands — only `tests` matching runs).
fn manageable_recipe(id: &str, version: u32, tests: Value) -> Value {
    serde_json::json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": version,
        "name": format!("{id} v{version}"),
        "match": {"hosts": ["127.0.0.1"], "url_pattern": "^http://127\\.0\\.0\\.1:[0-9]+/.+$"},
        "steps": [{
            "id": "fetch",
            "op": "http_request",
            "args": {"method": "GET", "url": "http://127.0.0.1:1/file", "max_bytes": 1024}
        }],
        "emit": {"url": "https://example.com/file.bin"},
        "tests": tests
    })
}

fn write_recipe_index(local: &Path, entries: Value) {
    write_resolve_file(
        &resolve_state(local).join("index.json"),
        &serde_json::json!({
            "schema": "rdm.recipe-index/v1",
            "updated_ms": 1,
            "entries": entries
        }),
    );
}

fn recipe_file(local: &Path, id: &str, version: u32) -> PathBuf {
    resolve_state(local)
        .join("recipes")
        .join(format!("{id}@{version}.json"))
}

fn draft_file(local: &Path, draft_id: &str) -> PathBuf {
    resolve_state(local)
        .join("drafts")
        .join(format!("{draft_id}@1.json"))
}

/// Draft whose recipe replays against the loopback fixture: fetch `input_url`,
/// assert it looks like a URL, emit it (R1/R2/R3/R6 can pass; R4/R5 need
/// `--sample`/`--negative`).
fn write_fixture_publish_draft(
    local: &Path,
    draft_id: &str,
    recipe_id: &str,
    source_url: &str,
) -> PathBuf {
    let draft = serde_json::json!({
        "schema": "rdm.recipe-draft/v1",
        "id": draft_id,
        "version": 1,
        "created_ms": 11,
        "source_run_id": "run_publish_1",
        "source_url": source_url,
        "scope_hint": "narrow",
        "notes": ["fixture draft for publish gates"],
        "recipe": {
            "schema": "rdm.recipe/v1",
            "id": recipe_id,
            "version": 1,
            "name": "Fixture direct",
            "match": {
                "hosts": ["127.0.0.1"],
                "url_pattern": "^(?P<input_url>http://127\\.0\\.0\\.1:[0-9]+/(?:file|redirect))$"
            },
            "steps": [
                {"id": "fetch", "op": "http_request",
                 "args": {"method": "GET", "url": "{{input_url}}", "max_bytes": 1024}},
                {"id": "assert_url", "op": "assert",
                 "args": {"kind": "matches", "value": "{{input_url}}",
                          "pattern": "^https?://"}}
            ],
            "emit": {"url": "{{input_url}}"},
            "tests": [{"name": "sample_input", "input_url": source_url}]
        }
    });
    let path = draft_file(local, draft_id);
    write_resolve_file(&path, &draft);
    path
}

fn gate<'a>(data: &'a Value, id: &str) -> &'a Value {
    data["gates"]
        .as_array()
        .expect("gates array")
        .iter()
        .find(|gate| gate["id"] == id)
        .unwrap_or_else(|| panic!("missing gate {id}: {data}"))
}

#[tokio::test]
async fn recipe_list_show_check_and_rollback() {
    let local = TempDir::new("recipe-manage");
    write_local_policy(local.path(), allowed_policy(local.path()));
    // Two versions of one method; v1 has a matching + a skipped test.
    write_resolve_file(
        &recipe_file(local.path(), "fixture-method", 1),
        &manageable_recipe(
            "fixture-method",
            1,
            serde_json::json!([
                {"name": "matches", "input_url": "http://127.0.0.1:1/file"},
                {"name": "skip-needs-login", "input_url": "http://127.0.0.1:1/file",
                 "skip": "needs login"}
            ]),
        ),
    );
    write_resolve_file(
        &recipe_file(local.path(), "fixture-method", 2),
        &manageable_recipe(
            "fixture-method",
            2,
            serde_json::json!([{"name": "matches", "input_url": "http://127.0.0.1:1/file"}]),
        ),
    );
    write_recipe_index(
        local.path(),
        serde_json::json!({
            "fixture-method": {
                "active_version": 2,
                "health": {"status": "ok", "consecutive_failures": 0},
                "updated_ms": 1
            }
        }),
    );
    // A draft with one deliberately failing `expect_no_match` test.
    let failing_draft = serde_json::json!({
        "schema": "rdm.recipe-draft/v1",
        "id": "draft_bad_0001",
        "version": 1,
        "created_ms": 5,
        "source_run_id": "run_manage_2",
        "source_url": "http://127.0.0.1:1/page",
        "scope_hint": "narrow",
        "notes": [],
        "recipe": manageable_recipe(
            "draft-method",
            1,
            serde_json::json!([{"name": "must-not-match",
                                "input_url": "http://127.0.0.1:1/file",
                                "expect_no_match": true}])
        )
    });
    write_resolve_file(&draft_file(local.path(), "draft_bad_0001"), &failing_draft);

    // ---- list ---------------------------------------------------------
    let listed = run_resolve(&["recipe", "list", "--output", "json"], local.path()).await;
    assert_eq!(listed.code, 0, "stderr: {}", listed.stderr);
    let list = listed.json();
    assert_eq!(list["command"], "recipe");
    let data = &list["data"];
    assert_eq!(
        data["resolve_dir"],
        resolve_state(local.path()).display().to_string()
    );
    let recipes = data["recipes"].as_array().unwrap();
    let method = recipes
        .iter()
        .find(|recipe| recipe["id"] == "fixture-method")
        .expect("fixture-method listed");
    assert_eq!(method["active_version"], 2);
    assert_eq!(method["available_versions"], serde_json::json!([1, 2]));
    // v0.15: per-version health map (empty here: the fixture index has no
    // `versions` history yet).
    assert_eq!(method["versions"], serde_json::json!({}));
    assert_eq!(method["health"]["status"], "ok");
    let drafts = data["drafts"].as_array().unwrap();
    assert_eq!(drafts.len(), 1, "{list}");
    assert_eq!(drafts[0]["id"], "draft_bad_0001");
    assert_eq!(drafts[0]["source_url"], "http://127.0.0.1:1/page");
    assert_eq!(drafts[0]["scope_hint"], "narrow");
    assert_eq!(drafts[0]["created_ms"], 5);
    // text mode prints the table.
    let text = run_resolve(&["recipe", "list", "--output", "text"], local.path()).await;
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(text.stdout.contains("fixture-method"), "{}", text.stdout);
    assert!(text.stdout.contains("draft_bad_0001"), "{}", text.stdout);

    // ---- show ---------------------------------------------------------
    let shown = run_resolve(
        &["recipe", "show", "fixture-method", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(shown.code, 0, "stderr: {}", shown.stderr);
    let show = shown.json();
    assert_eq!(show["data"]["recipe"]["version"], 2, "newest by default");
    assert_eq!(show["data"]["active_version"], 2);
    assert_eq!(
        show["data"]["available_versions"],
        serde_json::json!([1, 2])
    );
    assert_eq!(show["data"]["recipe"]["versions"], serde_json::json!({}));
    assert_eq!(show["data"]["health"]["status"], "ok");
    let shown_v1 = run_resolve(
        &[
            "recipe",
            "show",
            "fixture-method",
            "--version",
            "1",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(shown_v1.code, 0, "stderr: {}", shown_v1.stderr);
    assert_eq!(shown_v1.json()["data"]["recipe"]["version"], 1);
    let missing = run_resolve(
        &["recipe", "show", "nope", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(missing.code, 2);
    assert_eq!(missing.json()["error"]["code"], "not_found");

    // ---- check --------------------------------------------------------
    let checked = run_resolve(&["recipe", "check", "--output", "json"], local.path()).await;
    assert_eq!(checked.code, 0, "report-only check must exit 0");
    let check = checked.json();
    assert_eq!(check["data"]["checked"], 3, "2 recipes + 1 draft: {check}");
    assert_eq!(check["data"]["passed"], 2);
    assert_eq!(check["data"]["failed"], 1);
    assert_eq!(check["data"]["skipped"], 1);
    let failures = check["data"]["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["recipe_id"], "draft-method");
    assert_eq!(failures[0]["test"], "must-not-match");
    let strict = run_resolve(
        &["recipe", "check", "--strict", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(strict.code, 2, "--strict must fail on recipe test failures");
    let strict = strict.json();
    assert_eq!(strict["ok"], false);
    assert_eq!(strict["error"]["code"], "resolve_failed");
    assert_eq!(strict["data"]["failed"], 1);

    // ---- rollback -----------------------------------------------------
    let rolled = run_resolve(
        &[
            "recipe",
            "rollback",
            "fixture-method",
            "--version",
            "1",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(rolled.code, 0, "stderr: {}", rolled.stderr);
    let rolled = rolled.json();
    assert_eq!(rolled["data"]["id"], "fixture-method");
    assert_eq!(rolled["data"]["active_version"], 1);
    assert_eq!(
        rolled["data"]["available_versions"],
        serde_json::json!([1, 2])
    );
    // Rolling back solidifies the previous active version's (v1-file) health
    // into the per-version map; the target version has no history yet.
    assert_eq!(rolled["data"]["versions"]["2"]["status"], "ok");
    let index: Value = serde_json::from_slice(
        &std::fs::read(resolve_state(local.path()).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        index["entries"]["fixture-method"]["active_version"], 1,
        "rollback must persist active_version in index.json: {index}"
    );
    let bad_rollback = run_resolve(
        &[
            "recipe",
            "rollback",
            "fixture-method",
            "--version",
            "9",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(bad_rollback.code, 2);
    assert_eq!(bad_rollback.json()["error"]["code"], "invalid_request");
    let missing_draft = run_resolve(
        &[
            "recipe",
            "publish",
            "draft_nope",
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(missing_draft.code, 2);
    assert_eq!(missing_draft.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn recipe_publish_dry_run_reports_gates_without_writing() {
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("recipe-publish-dry");
    write_local_policy(local.path(), allowed_policy(local.path()));
    let draft = write_fixture_publish_draft(
        local.path(),
        "draft_fixture_0001",
        "fixture-method",
        &fixture.file_url(),
    );

    // Narrow (default): only the source sample → R4/R5 fail, R1/R2/R3/R6 pass.
    let output = run_resolve(
        &[
            "recipe",
            "publish",
            "draft_fixture_0001",
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    let data = &value["data"];
    assert_eq!(data["draft_id"], "draft_fixture_0001", "{value}");
    assert_eq!(data["published"], false);
    assert_eq!(data["required_passed"], true);
    assert_eq!(data["scope"], "narrow");
    assert_eq!(data["dry_run"], true);
    assert_eq!(gate(data, "R3")["pass"], true, "{value}");
    assert_eq!(gate(data, "R6")["pass"], true);
    assert_eq!(gate(data, "R4")["pass"], false);
    assert_eq!(gate(data, "R5")["pass"], false);
    // Dry-run wrote nothing: no method file, draft still there.
    assert!(!recipe_file(local.path(), "fixture-method", 1).exists());
    assert!(draft.exists());

    // Scoped: two fresh samples + a non-matching negative → all gates pass.
    let scoped = run_resolve(
        &[
            "recipe",
            "publish",
            "draft_fixture_0001",
            "--sample",
            &fixture.file_url(),
            "--sample",
            &fixture.redirect_url(),
            "--negative",
            "https://example.com/not-a-download",
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(scoped.code, 0, "stderr: {}", scoped.stderr);
    let scoped = scoped.json();
    let scoped_data = &scoped["data"];
    assert_eq!(scoped_data["required_passed"], true, "{scoped}");
    assert_eq!(scoped_data["scope"], "scoped");
    assert_eq!(scoped_data["samples"].as_array().unwrap().len(), 2);
    for id in ["R1", "R2", "R3", "R4", "R5", "R6"] {
        assert_eq!(gate(scoped_data, id)["pass"], true, "gate {id}: {scoped}");
    }
    assert!(!recipe_file(local.path(), "fixture-method", 1).exists());
    fixture.shutdown().await;
}

#[tokio::test]
async fn recipe_publish_writes_method_index_and_removes_draft() {
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("recipe-publish-ok");
    write_local_policy(local.path(), allowed_policy(local.path()));
    let draft_path = write_fixture_publish_draft(
        local.path(),
        "draft_fixture_0002",
        "fixture-method",
        &fixture.file_url(),
    );

    let output = run_resolve(
        &[
            "recipe",
            "publish",
            "draft_fixture_0002",
            "--sample",
            &fixture.file_url(),
            "--sample",
            &fixture.redirect_url(),
            "--negative",
            "https://example.com/not-a-download",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    let data = &value["data"];
    assert_eq!(data["published"], true, "{value}");
    assert_eq!(data["scope"], "scoped");
    assert_eq!(data["recipe"]["id"], "fixture-method");
    assert_eq!(data["recipe"]["version"], 1);
    assert_eq!(data["recipe"]["state"], "published");

    // Method file + persisted index + draft removal (publish owns the sequence).
    let method = recipe_file(local.path(), "fixture-method", 1);
    assert!(
        method.is_file(),
        "method file missing: {}",
        method.display()
    );
    let index: Value = serde_json::from_slice(
        &std::fs::read(resolve_state(local.path()).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        index["entries"]["fixture-method"]["active_version"], 1,
        "{index}"
    );
    assert!(!draft_path.exists(), "draft must be removed after publish");

    // The published method now matches the site (recipe-only replay, no model).
    let resolved = run_resolve(
        &[
            "resolve",
            &fixture.file_url(),
            "--mode",
            "recipe-only",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(resolved.code, 0, "stderr: {}", resolved.stderr);
    let resolved = resolved.json();
    assert_eq!(resolved["data"]["status"], "succeeded", "{resolved}");
    assert_eq!(resolved["data"]["recipe"]["id"], "fixture-method");
    fixture.shutdown().await;
}

#[tokio::test]
async fn recipe_publish_failed_required_gate_writes_nothing() {
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("recipe-publish-fail");
    write_local_policy(local.path(), allowed_policy(local.path()));
    let draft_path = write_fixture_publish_draft(
        local.path(),
        "draft_fixture_0003",
        "fixture-method",
        &fixture.file_url(),
    );

    for dry_run in [true, false] {
        let mut args = vec![
            "recipe",
            "publish",
            "draft_fixture_0003",
            "--sample",
            "https://example.com/page",
            "--output",
            "json",
        ];
        if dry_run {
            args.insert(3, "--dry-run");
        }
        let output = run_resolve(&args, local.path()).await;
        assert_eq!(
            output.code, 2,
            "dry_run={dry_run} stderr: {}",
            output.stderr
        );
        let value = output.json();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "resolve_failed");
        assert_eq!(value["data"]["required_passed"], false, "{value}");
        assert_eq!(gate(&value["data"], "R3")["pass"], false, "{value}");
        assert_eq!(
            value["data"]["published"], false,
            "nothing may be published: {value}"
        );
        assert!(!recipe_file(local.path(), "fixture-method", 1).exists());
        assert!(draft_path.exists(), "failed publish keeps the draft");
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn recipe_publish_r5_failure_scopes_down_to_narrow() {
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("recipe-publish-r5");
    write_local_policy(local.path(), allowed_policy(local.path()));
    write_fixture_publish_draft(
        local.path(),
        "draft_fixture_0004",
        "fixture-method",
        &fixture.file_url(),
    );
    // R4 passes (two fresh samples) but the negative URL matches the recipe →
    // R5 fails → scope narrows, publishing is still allowed (required gates ok).
    let output = run_resolve(
        &[
            "recipe",
            "publish",
            "draft_fixture_0004",
            "--sample",
            &fixture.file_url(),
            "--sample",
            &fixture.redirect_url(),
            "--negative",
            &fixture.file_url(),
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    let data = &value["data"];
    assert_eq!(data["required_passed"], true, "{value}");
    assert_eq!(gate(data, "R4")["pass"], true, "{value}");
    assert_eq!(gate(data, "R5")["pass"], false, "{value}");
    assert_eq!(data["scope"], "narrow", "{value}");
    fixture.shutdown().await;
}

#[tokio::test]
async fn recipe_publish_policy_denied_exits_4() {
    let fixture = fixture(64 * 1024).await;
    let local = TempDir::new("recipe-publish-denied");
    // No policy file → resolver safe defaults deny loopback/private hosts.
    write_fixture_publish_draft(
        local.path(),
        "draft_fixture_0005",
        "fixture-method",
        &fixture.file_url(),
    );
    let output = run_resolve(
        &[
            "recipe",
            "publish",
            "draft_fixture_0005",
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 4, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "policy_denied");
    assert!(
        value["error"]["details"]["kind"] == "policy_denied"
            || value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("denied"),
        "{value}"
    );
    assert!(!recipe_file(local.path(), "fixture-method", 1).exists());
    fixture.shutdown().await;
}
