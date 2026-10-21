//! v1.6 §8.6 acceptance: `rdm resolve` reports optional method-library health
//! (`data.recipe_health`) and degraded/broken methods warn on stderr while
//! `--output json` stdout stays exactly one JSON envelope.
//!
//! Everything runs through the real `rdm` binary against the embedded fixture
//! server; each case gets an isolated `%LOCALAPPDATA%` (task store policy +
//! `<RDM>/resolve/{recipes,index.json}`), so the user profile is never touched.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use rdm_fixture_server::{Config, TestServer};
use serde_json::{json, Value};
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
            std::env::temp_dir().join(format!("rdm-health-{tag}-{}-{nanos}", std::process::id()));
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
                "expected exactly one JSON object on stdout, got `{}` (error: {error}; stderr: {})",
                self.stdout, self.stderr
            )
        })
    }
}

/// Run `rdm` with an isolated `%LOCALAPPDATA%` and no ambient AI provider.
async fn run(args: &[&str], localappdata: &Path) -> RunOutput {
    let output = Command::new(rdm_bin())
        .args(args)
        .env("LOCALAPPDATA", localappdata)
        .env_remove("RDM_AGENT_STATE_DIR")
        .env_remove("RDM_AI_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run rdm");
    RunOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Isolated `%LOCALAPPDATA%` with an allow-all host policy (loopback fixture).
fn isolated_localappdata(tag: &str) -> TempDir {
    let local = TempDir::new(tag);
    let agent = local.path().join("RDM").join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let policy = json!({
        "schema_version": "rdm.agent/v1",
        "allowed_hosts": ["*"],
        "allowed_dirs": [local.path()],
        "max_bytes_per_task": null,
        "require_approval_over_bytes": null,
        "max_concurrent_tasks": 2,
        "default_connections": 2,
        "allow_private_hosts": true,
        "allow_overwrite": false
    });
    std::fs::write(
        agent.join("policy.json"),
        serde_json::to_vec_pretty(&policy).unwrap(),
    )
    .unwrap();
    local
}

/// Library recipe that replays a fixture URL and emits it. Against the HTML
/// fixture the replay succeeds but verification yields `partial`/`not_a_file`,
/// which is a *non-structural* failure: it records `last_failure` without
/// changing a seeded health status (so all four statuses are observable).
const HTML_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "fixture-html",
  "version": 1,
  "name": "Fixture HTML page",
  "match": { "hosts": ["127.0.0.1"], "url_pattern": "^(?P<input_url>http://.+)$" },
  "limits": { "requests": 4, "bytes_read": 131072, "wall_ms": 10000, "redirects": 2 },
  "steps": [
    {
      "id": "fetch",
      "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 512 },
      "expect": { "status_in": [200] }
    }
  ],
  "emit": { "url": "{{input_url}}" }
}"#;

fn write_recipe(localappdata: &Path) {
    let recipes = localappdata.join("RDM").join("resolve").join("recipes");
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(recipes.join("fixture-html@1.json"), HTML_RECIPE).unwrap();
}

/// Seed `<RDM>/resolve/index.json` with the given health.
fn write_index(localappdata: &Path, status: &str, consecutive_failures: u32) {
    let resolve = localappdata.join("RDM").join("resolve");
    std::fs::create_dir_all(&resolve).unwrap();
    let index = json!({
        "schema": "rdm.recipe-index/v1",
        "updated_ms": 1_792_345_678_901u64,
        "entries": {
            "fixture-html": {
                "active_version": 1,
                "updated_ms": 1_792_345_678_901u64,
                "health": {
                    "status": status,
                    "last_ok_ms": null,
                    "last_failure": null,
                    "consecutive_failures": consecutive_failures,
                    "evidence": []
                }
            }
        }
    });
    std::fs::write(
        resolve.join("index.json"),
        serde_json::to_vec_pretty(&index).unwrap(),
    )
    .unwrap();
}

async fn html_fixture() -> TestServer {
    let mut config = Config::new(8 * 1024);
    config.content_type = "text/html".to_string();
    TestServer::start(config).await.expect("start fixture")
}

async fn plain_fixture() -> TestServer {
    TestServer::start(Config::new(32 * 1024))
        .await
        .expect("start fixture")
}

const RESOLVE_ARGS: [&str; 6] = [
    "resolve",
    "--recipe",
    "fixture-html",
    "--mode",
    "recipe-only",
    "--output",
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recipe_health_status_is_reported_for_library_recipes() {
    let fixture = html_fixture().await;
    let url = fixture.file_url();

    // (seed_index, seeded status, seeded consecutive failures)
    let cases: [(&str, u32, bool); 4] = [
        ("ok", 0, true),
        ("degraded", 2, true),
        ("broken", 3, true),
        ("untested", 0, false),
    ];
    for (status, consecutive, seed) in cases {
        let local = isolated_localappdata(&format!("health-{status}"));
        write_recipe(local.path());
        if seed {
            write_index(local.path(), status, consecutive);
        }
        let mut args = RESOLVE_ARGS.to_vec();
        args.push("json");
        args.push(&url);
        let output = run(&args, local.path()).await;
        assert_eq!(output.code, 0, "status={status} stderr: {}", output.stderr);
        let value = output.json();
        assert_eq!(value["ok"], true, "status={status}: {value}");
        assert_eq!(value["command"], "resolve");
        assert_eq!(value["data"]["status"], "partial", "status={status}");
        assert_eq!(value["data"]["recipe"]["id"], "fixture-html");
        assert_eq!(value["data"]["recipe"]["state"], "matched");
        let health = &value["data"]["recipe_health"];
        assert_eq!(
            health["status"], status,
            "seeded status must be reported (non-structural failure keeps it): {value}"
        );
        assert!(
            health["consecutive_failures"].is_number(),
            "status={status}: {value}"
        );
        assert!(health["evidence"].is_array(), "status={status}: {value}");
        assert!(health["last_ok_ms"].is_null(), "status={status}: {value}");
        // The verification failure is non-structural, so last_failure is set
        // without touching the seeded status/streak.
        assert_eq!(
            health["last_failure"]["kind"], "not_a_file",
            "status={status}: {value}"
        );
        assert!(
            health["last_failure"]["run_id"]
                .as_str()
                .unwrap()
                .starts_with("run_"),
            "status={status}: {value}"
        );
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn degraded_and_broken_text_mode_warn_on_stderr_only() {
    let fixture = html_fixture().await;
    let url = fixture.file_url();
    for (status, consecutive) in [("degraded", 2), ("broken", 3)] {
        let local = isolated_localappdata(&format!("warn-{status}"));
        write_recipe(local.path());
        write_index(local.path(), status, consecutive);

        let mut args = RESOLVE_ARGS.to_vec();
        args.push("text");
        args.push(&url);
        let output = run(&args, local.path()).await;
        assert_eq!(output.code, 0, "status={status} stderr: {}", output.stderr);
        let expected =
            format!("warning: recipe fixture-html is {status} (last failure: not_a_file)");
        assert!(
            output.stderr.contains(&expected),
            "text mode must warn on stderr, got stderr `{}`",
            output.stderr
        );
        assert!(
            !output.stdout.contains("warning:"),
            "warning must not pollute stdout: `{}`",
            output.stdout
        );

        // JSON mode keeps stdout a single parseable envelope with the field.
        let mut json_args = RESOLVE_ARGS.to_vec();
        json_args.push("json");
        json_args.push(&url);
        let json_output = run(&json_args, local.path()).await;
        assert_eq!(json_output.code, 0);
        let value = json_output.json();
        assert_eq!(value["data"]["recipe_health"]["status"], status);
        assert!(
            !json_output.stdout.contains("warning:"),
            "JSON stdout must stay clean: `{}`",
            json_output.stdout
        );
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn direct_link_has_no_recipe_health_field() {
    let fixture = plain_fixture().await;
    let local = isolated_localappdata("direct"); // no recipe, no index
    let output = run(
        &["resolve", &fixture.file_url(), "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    let value = output.json();
    assert_eq!(value["data"]["status"], "succeeded");
    assert!(value["data"]["recipe"].is_null(), "{value}");
    assert!(
        value["data"].get("recipe_health").is_none(),
        "direct-link/builtin runs must omit recipe_health: {value}"
    );
    assert!(!output.stdout.contains("recipe_health"));
    fixture.shutdown().await;
}
