//! End-to-end tests for version-level health in `rdm recipe list/show/rollback`
//! (v0.15, `docs/architecture.md` §11.10 / agent contract v2.1 §8.11).
//!
//! Every test isolates `%LOCALAPPDATA%` and seeds `<state>/resolve` directly —
//! no network, no model.

use std::path::{Path, PathBuf};
use std::process::Stdio;

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
        let path = std::env::temp_dir().join(format!(
            "rdm-recipe-version-health-{tag}-{}-{nanos}",
            std::process::id()
        ));
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
}

async fn run_local(args: &[&str], local: &Path) -> RunOutput {
    let mut command = Command::new(rdm_bin());
    command
        .args(args)
        .env_remove("RDM_AGENT_STATE_DIR")
        .env_remove("RDM_AI_API_KEY")
        .env("LOCALAPPDATA", local.to_string_lossy().into_owned())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let output = command.output().await.expect("run rdm");
    RunOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn resolve_state(local: &Path) -> PathBuf {
    local.join("RDM").join("resolve")
}

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn method(id: &str, version: u32) -> Value {
    json!({
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
        "emit": {"url": "https://example.com/file.bin"}
    })
}

/// Seed two method versions + a v2 index with per-version health map.
fn seed_versioned_library(local: &Path, active_version: u32) {
    for version in [1, 2] {
        write_json(
            &resolve_state(local)
                .join("recipes")
                .join(format!("versioned-method@{version}.json")),
            &method("versioned-method", version),
        );
    }
    let health = |status: &str, consecutive: u32| {
        json!({
            "status": status,
            "consecutive_failures": consecutive,
            "last_ok_ms": null,
            "last_failure": null,
            "evidence": []
        })
    };
    write_json(
        &resolve_state(local).join("index.json"),
        &json!({
            "schema": "rdm.recipe-index/v2",
            "updated_ms": 1,
            "entries": {
                "versioned-method": {
                    "active_version": active_version,
                    "health": health(if active_version == 2 { "ok" } else { "broken" }, 0),
                    "versions": {
                        "1": health("broken", 3),
                        "2": health("ok", 0)
                    },
                    "updated_ms": 1
                }
            }
        }),
    );
}

// ---------------------------------------------------------------------------
// list / show
// ---------------------------------------------------------------------------

#[tokio::test]
async fn version_health_is_visible_in_list_and_show() {
    let local = TempDir::new("list-show");
    seed_versioned_library(local.path(), 2);

    // ---- list --output json -------------------------------------------
    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    assert_eq!(listed.code, 0, "stderr: {}", listed.stderr);
    let list = listed.json();
    let recipes = list["data"]["recipes"].as_array().expect("recipes");
    let entry = recipes
        .iter()
        .find(|recipe| recipe["id"] == "versioned-method")
        .expect("versioned-method listed");
    assert_eq!(entry["active_version"], 2);
    assert_eq!(entry["available_versions"], json!([1, 2]));
    assert_eq!(entry["health"]["status"], "ok");
    assert_eq!(entry["versions"]["1"]["status"], "broken");
    assert_eq!(entry["versions"]["1"]["consecutive_failures"], 3);
    assert_eq!(entry["versions"]["2"]["status"], "ok");

    // ---- list text -----------------------------------------------------
    let text = run_local(&["recipe", "list", "--output", "text"], local.path()).await;
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(text.stdout.contains("available=[v1,v2]"), "{}", text.stdout);
    assert!(
        text.stdout.contains("versions=[1:broken,2:ok]"),
        "{}",
        text.stdout
    );

    // ---- show --output json -------------------------------------------
    let shown = run_local(
        &["recipe", "show", "versioned-method", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(shown.code, 0, "stderr: {}", shown.stderr);
    let show = shown.json();
    assert_eq!(show["data"]["recipe"]["version"], 2, "newest by default");
    assert_eq!(show["data"]["active_version"], 2);
    assert_eq!(show["data"]["available_versions"], json!([1, 2]));
    assert_eq!(show["data"]["health"]["status"], "ok");
    assert_eq!(show["data"]["recipe"]["versions"]["1"]["status"], "broken");
    assert_eq!(show["data"]["recipe"]["versions"]["2"]["status"], "ok");

    // ---- show text -----------------------------------------------------
    let text = run_local(
        &["recipe", "show", "versioned-method", "--output", "text"],
        local.path(),
    )
    .await;
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(
        text.stdout.contains("available:     v1,v2"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("v1: 失效 (consecutive=3)"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("v2: 正常 (consecutive=0)"),
        "{}",
        text.stdout
    );
}

// ---------------------------------------------------------------------------
// rollback mirror
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rollback_switches_the_mirror_and_keeps_version_history() {
    let local = TempDir::new("rollback");
    seed_versioned_library(local.path(), 2);

    let rolled = run_local(
        &[
            "recipe",
            "rollback",
            "versioned-method",
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
    assert_eq!(rolled["data"]["active_version"], 1);
    assert_eq!(rolled["data"]["available_versions"], json!([1, 2]));
    assert_eq!(rolled["data"]["versions"]["1"]["status"], "broken");
    assert_eq!(rolled["data"]["versions"]["2"]["status"], "ok");

    // index.json: active switched, mirror = broken(v1), history retained.
    let index: Value = serde_json::from_slice(
        &std::fs::read(resolve_state(local.path()).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        index["entries"]["versioned-method"]["active_version"], 1,
        "{index}"
    );
    assert_eq!(
        index["entries"]["versioned-method"]["health"]["status"], "broken",
        "the mirror follows the active version: {index}"
    );
    assert_eq!(
        index["entries"]["versioned-method"]["versions"]["2"]["status"], "ok",
        "{index}"
    );
    assert_eq!(index["schema"], "rdm.recipe-index/v2");

    // list reflects the new active health and still lists both versions.
    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    let list = listed.json();
    let entry = list["data"]["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recipe| recipe["id"] == "versioned-method")
        .expect("listed");
    assert_eq!(entry["active_version"], 1);
    assert_eq!(entry["health"]["status"], "broken");
    assert_eq!(entry["versions"]["2"]["status"], "ok");
}

// ---------------------------------------------------------------------------
// v1 compatibility
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v1_index_without_versions_still_lists_and_shows() {
    let local = TempDir::new("v1-compat");
    for version in [1, 2] {
        write_json(
            &resolve_state(local.path())
                .join("recipes")
                .join(format!("legacy-method@{version}.json")),
            &method("legacy-method", version),
        );
    }
    // Old schema: single health field, no `versions` map.
    write_json(
        &resolve_state(local.path()).join("index.json"),
        &json!({
            "schema": "rdm.recipe-index/v1",
            "updated_ms": 1,
            "entries": {
                "legacy-method": {
                    "active_version": 2,
                    "health": {"status": "degraded", "consecutive_failures": 2},
                    "updated_ms": 1
                }
            }
        }),
    );

    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    assert_eq!(listed.code, 0, "stderr: {}", listed.stderr);
    let list = listed.json();
    let entry = list["data"]["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recipe| recipe["id"] == "legacy-method")
        .expect("legacy-method listed");
    assert_eq!(entry["active_version"], 2);
    assert_eq!(entry["available_versions"], json!([1, 2]));
    assert_eq!(entry["health"]["status"], "degraded");
    assert_eq!(
        entry["versions"],
        json!({}),
        "v1 file has no history map yet"
    );

    let shown = run_local(
        &["recipe", "show", "legacy-method", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(shown.code, 0, "stderr: {}", shown.stderr);
    let show = shown.json();
    assert_eq!(show["data"]["health"]["status"], "degraded");
    assert_eq!(show["data"]["recipe"]["versions"], json!({}));
}
