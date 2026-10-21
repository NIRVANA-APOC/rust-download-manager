//! End-to-end tests for `rdm recipe export/import` (v1.6, architecture §11.11).
//!
//! Every test isolates `%LOCALAPPDATA%`, so the real `<state>/resolve` method
//! library and drafts are never touched. The import `--publish`/`--as-draft`
//! replay tests use the in-process loopback fixture server only.

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
        let path = std::env::temp_dir().join(format!(
            "rdm-recipe-io-{tag}-{}-{nanos}",
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

/// Run `rdm` with an isolated `%LOCALAPPDATA%` (the resolver state root).
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

fn resolve_dir(local: &Path) -> PathBuf {
    local.join("RDM").join("resolve")
}

fn recipes_dir(local: &Path) -> PathBuf {
    resolve_dir(local).join("recipes")
}

fn drafts_dir(local: &Path) -> PathBuf {
    resolve_dir(local).join("drafts")
}

fn method_path(local: &Path, id: &str, version: u32) -> PathBuf {
    recipes_dir(local).join(format!("{id}@{version}.json"))
}

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn write_policy(local: &Path) {
    let agent = local.join("RDM").join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let policy = json!({
        "schema_version": "rdm.agent/v1",
        "allowed_hosts": ["*"],
        "allowed_dirs": [local],
        "max_bytes_per_task": 1073741824,
        "require_approval_over_bytes": null,
        "max_concurrent_tasks": 2,
        "default_connections": 2,
        "allow_private_hosts": true,
        "allow_overwrite": false
    });
    write_json(&agent.join("policy.json"), &policy);
}

/// Valid but not network-replayable method (used by list/export/import tests).
fn library_recipe(id: &str, version: u32, name: &str) -> Value {
    json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": version,
        "name": name,
        "match": {"hosts": ["example.com"], "url_pattern": "^https://example\\.com/.+$"},
        "steps": [{
            "id": "fetch",
            "op": "http_request",
            "args": {"method": "GET", "url": "https://example.com/api", "max_bytes": 1024}
        }],
        "emit": {"url": "https://example.com/file.bin"},
        "tests": [{"name": "sample_input", "input_url": "https://example.com/page"}]
    })
}

/// Recipe replayable against the loopback fixture (GET input_url → assert → emit).
fn fixture_recipe(id: &str, version: u32, positive: &str, negative: &str) -> Value {
    json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": version,
        "name": "Fixture method",
        "match": {
            "hosts": ["127.0.0.1"],
            "url_pattern": r"^(?P<input_url>http://127\.0\.0\.1:[0-9]+/.+)$"
        },
        "steps": [
            {"id": "fetch", "op": "http_request",
             "args": {"method": "GET", "url": "{{input_url}}", "max_bytes": 1024}},
            {"id": "assert_url", "op": "assert",
             "args": {"kind": "matches", "value": "{{input_url}}", "pattern": "^https?://"}}
        ],
        "emit": {"url": "{{input_url}}"},
        "tests": [
            {"name": "sample_input", "input_url": positive},
            {"name": "rejects_other", "input_url": negative, "expect_no_match": true}
        ]
    })
}

fn sensitive_header_recipe(id: &str) -> Value {
    json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": 1,
        "name": "Sensitive header method",
        "match": {"hosts": ["api.example.com"]},
        "steps": [{
            "id": "fetch",
            "op": "http_request",
            "args": {"method": "GET", "url": "https://api.example.com/data",
                     "headers": {"Authorization": "Bearer SECRET-VALUE",
                                 "Accept": "application/json"}}
        }],
        "emit": {"url": "https://cdn.example.com/file.bin"}
    })
}

fn sensitive_query_recipe(id: &str) -> Value {
    json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": 1,
        "name": "Sensitive query method",
        "match": {"hosts": ["cdn.example.com"]},
        "steps": [{
            "id": "fetch",
            "op": "http_request",
            "args": {"method": "GET",
                     "url": "https://cdn.example.com/file.bin?token=deadbeefcafe"}
        }],
        "emit": {"url": "https://cdn.example.com/file.bin"}
    })
}

fn gate<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["gates"]
        .as_array()
        .expect("gates array")
        .iter()
        .find(|gate| gate["id"] == id)
        .unwrap_or_else(|| panic!("missing gate {id}: {report}"))
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_outputs_recipe_and_writes_force_protected_file() {
    let local = TempDir::new("export");
    write_policy(local.path());
    write_json(
        &method_path(local.path(), "example-method", 1),
        &library_recipe("example-method", 1, "v1"),
    );
    write_json(
        &method_path(local.path(), "example-method", 2),
        &library_recipe("example-method", 2, "v2"),
    );

    // No --out: just the recipe in `data` (newest version by default).
    let exported = run_local(
        &["recipe", "export", "example-method", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(exported.code, 0, "stderr: {}", exported.stderr);
    let value = exported.json();
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(value["data"]["id"], "example-method");
    assert_eq!(value["data"]["version"], 2);
    assert_eq!(value["data"]["recipe"]["id"], "example-method");
    assert!(value["data"].get("path").is_none(), "{value}");

    let v1 = run_local(
        &[
            "recipe",
            "export",
            "example-method",
            "--version",
            "1",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(v1.code, 0, "stderr: {}", v1.stderr);
    assert_eq!(v1.json()["data"]["version"], 1);

    // --out writes the exact recipe JSON and reports the path.
    let out = local.path().join("exports").join("method.json");
    let exported = run_local(
        &[
            "recipe",
            "export",
            "example-method",
            "--out",
            out.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(exported.code, 0, "stderr: {}", exported.stderr);
    let value = exported.json();
    assert_eq!(value["data"]["path"], out.display().to_string());
    assert!(out.is_file());
    let written: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(written["id"], "example-method");
    assert_eq!(written["version"], 2);
    assert_eq!(
        written, value["data"]["recipe"],
        "file must be the exported recipe"
    );

    // Existing target without --force → conflict (exit 4); --force overwrites.
    let conflict = run_local(
        &[
            "recipe",
            "export",
            "example-method",
            "--out",
            out.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(conflict.code, 4, "stderr: {}", conflict.stderr);
    assert_eq!(conflict.json()["error"]["code"], "conflict");
    std::fs::write(&out, b"stale").unwrap();
    let forced = run_local(
        &[
            "recipe",
            "export",
            "example-method",
            "--out",
            out.to_str().unwrap(),
            "--force",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(forced.code, 0, "stderr: {}", forced.stderr);
    let rewritten: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(rewritten["id"], "example-method");

    // Missing id / version → not_found exit 2.
    let missing = run_local(
        &["recipe", "export", "nope", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(missing.code, 2);
    assert_eq!(missing.json()["error"]["code"], "not_found");
    let missing_version = run_local(
        &[
            "recipe",
            "export",
            "example-method",
            "--version",
            "9",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(missing_version.code, 2);
    assert_eq!(missing_version.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn export_sensitive_content_requires_opt_in() {
    let local = TempDir::new("export-sensitive");
    write_policy(local.path());
    write_json(
        &method_path(local.path(), "sensitive-header", 1),
        &sensitive_header_recipe("sensitive-header"),
    );
    write_json(
        &method_path(local.path(), "sensitive-query", 1),
        &sensitive_query_recipe("sensitive-query"),
    );

    // Credential header → refused by default; the secret is never echoed.
    let refused = run_local(
        &["recipe", "export", "sensitive-header", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(refused.code, 2, "stderr: {}", refused.stderr);
    let value = refused.json();
    assert_eq!(value["error"]["code"], "invalid_request");
    let findings = value["error"]["details"]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{value}");
    assert_eq!(findings[0]["step"], "fetch");
    assert_eq!(findings[0]["kind"], "header");
    assert!(findings[0]["reason"]
        .as_str()
        .unwrap()
        .contains("authorization"));
    assert!(
        !refused.stdout.contains("SECRET-VALUE") && !refused.stderr.contains("SECRET-VALUE"),
        "the sensitive header value must never be printed"
    );
    assert!(
        value["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("--allow-sensitive"),
        "{value}"
    );

    // --allow-sensitive exports it unchanged.
    let allowed = run_local(
        &[
            "recipe",
            "export",
            "sensitive-header",
            "--allow-sensitive",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(allowed.code, 0, "stderr: {}", allowed.stderr);
    let allowed_value = allowed.json();
    assert_eq!(
        allowed_value["data"]["recipe"]["steps"][0]["args"]["headers"]["Authorization"],
        "Bearer SECRET-VALUE"
    );
    assert!(
        allowed.stdout.contains("SECRET-VALUE"),
        "--allow-sensitive keeps the credential content in the exported JSON"
    );

    // Sensitive query parameter name is refused too.
    let query = run_local(
        &["recipe", "export", "sensitive-query", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(query.code, 2);
    let value = query.json();
    assert_eq!(value["error"]["code"], "invalid_request");
    assert_eq!(value["error"]["details"]["findings"][0]["kind"], "query");
    assert!(value["error"]["details"]["findings"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("token"));

    // Round-trip: an allow-sensitive export cannot be imported without opt-in.
    let path = local.path().join("sensitive.json");
    let exported = run_local(
        &[
            "recipe",
            "export",
            "sensitive-header",
            "--out",
            path.to_str().unwrap(),
            "--allow-sensitive",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(exported.code, 0, "stderr: {}", exported.stderr);
    // Remove the library copy so the allow-sensitive import does not merely
    // hit the idempotent "already present" path.
    std::fs::remove_file(method_path(local.path(), "sensitive-header", 1)).unwrap();
    let refused_import = run_local(
        &[
            "recipe",
            "import",
            path.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(refused_import.code, 2);
    assert_eq!(refused_import.json()["error"]["code"], "invalid_request");
    let allowed_import = run_local(
        &[
            "recipe",
            "import",
            path.to_str().unwrap(),
            "--allow-sensitive",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(allowed_import.code, 0, "stderr: {}", allowed_import.stderr);
    assert_eq!(allowed_import.json()["data"]["imported"], true);
}

// ---------------------------------------------------------------------------
// import: default / conflicts / idempotency / dry-run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_writes_library_and_handles_conflicts_and_idempotency() {
    let local = TempDir::new("import");
    write_policy(local.path());
    let source_dir = local.path().join("incoming");
    let source = source_dir.join("method.json");
    write_json(&source, &library_recipe("imported-method", 1, "original"));

    let imported = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(imported.code, 0, "stderr: {}", imported.stderr);
    let value = imported.json();
    assert_eq!(value["data"]["imported"], true, "{value}");
    assert_eq!(value["data"]["id"], "imported-method");
    assert_eq!(value["data"]["version"], 1);
    assert_eq!(
        value["data"]["path"],
        method_path(local.path(), "imported-method", 1)
            .display()
            .to_string()
    );
    assert!(method_path(local.path(), "imported-method", 1).is_file());
    let index: Value = serde_json::from_slice(
        &std::fs::read(resolve_dir(local.path()).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        index["entries"]["imported-method"]["active_version"], 1,
        "{index}"
    );

    // Visible through the normal management commands.
    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    let list = listed.json();
    let listed_method = list["data"]["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recipe| recipe["id"] == "imported-method")
        .expect("imported method listed");
    assert_eq!(listed_method["active_version"], 1);
    assert_eq!(listed_method["available_versions"], json!([1]));
    assert_eq!(listed_method["versions"], json!({}));
    let shown = run_local(
        &["recipe", "show", "imported-method", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(shown.code, 0, "stderr: {}", shown.stderr);
    assert_eq!(shown.json()["data"]["recipe"]["name"], "original");

    // Same content again → idempotent, no error.
    let idempotent = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(idempotent.code, 0, "stderr: {}", idempotent.stderr);
    let value = idempotent.json();
    assert_eq!(value["data"]["imported"], false);
    assert_eq!(value["data"]["already_present"], true);

    // Different content for the same id+version → conflict without --force.
    write_json(&source, &library_recipe("imported-method", 1, "changed"));
    let conflict = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(conflict.code, 4, "stderr: {}", conflict.stderr);
    assert_eq!(conflict.json()["error"]["code"], "conflict");
    let shown = run_local(
        &["recipe", "show", "imported-method", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(
        shown.json()["data"]["recipe"]["name"],
        "original",
        "a conflicting import must not touch the library"
    );

    // --force overwrites the method.
    let forced = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--force",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(forced.code, 0, "stderr: {}", forced.stderr);
    assert_eq!(forced.json()["data"]["imported"], true);
    let shown = run_local(
        &["recipe", "show", "imported-method", "--output", "json"],
        local.path(),
    )
    .await;
    assert_eq!(shown.json()["data"]["recipe"]["name"], "changed");

    // --dry-run validates and reports without writing.
    let v2_source = source_dir.join("method-v2.json");
    write_json(&v2_source, &library_recipe("imported-method", 2, "v2"));
    let dry = run_local(
        &[
            "recipe",
            "import",
            v2_source.to_str().unwrap(),
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(dry.code, 0, "stderr: {}", dry.stderr);
    let value = dry.json();
    assert_eq!(value["data"]["imported"], false);
    assert_eq!(value["data"]["dry_run"], true);
    assert!(!method_path(local.path(), "imported-method", 2).exists());
    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    assert_eq!(
        listed.json()["data"]["recipes"][0]["available_versions"],
        json!([1]),
        "dry-run must not write the library"
    );

    // Real import adds v2 and promotes active_version.
    let imported_v2 = run_local(
        &[
            "recipe",
            "import",
            v2_source.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(imported_v2.code, 0, "stderr: {}", imported_v2.stderr);
    let index: Value = serde_json::from_slice(
        &std::fs::read(resolve_dir(local.path()).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        index["entries"]["imported-method"]["active_version"], 2,
        "{index}"
    );
}

// ---------------------------------------------------------------------------
// import --as-draft → publish (fixture replay closure)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_as_draft_then_publish_closes_the_loop() {
    let fixture = TestServer::start(Config::new(64 * 1024)).await.unwrap();
    let local = TempDir::new("import-draft");
    write_policy(local.path());
    let source = local.path().join("incoming").join("fixture-method.json");
    let recipe = fixture_recipe(
        "fixture-method",
        1,
        &fixture.file_url(),
        "https://example.com/not-a-download",
    );
    write_json(&source, &recipe);

    let imported = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--as-draft",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(imported.code, 0, "stderr: {}", imported.stderr);
    let value = imported.json();
    let draft_id = value["data"]["draft_id"].as_str().unwrap().to_string();
    assert!(
        draft_id.starts_with("draft_fixture-method_"),
        "unexpected draft id {draft_id}"
    );
    assert_eq!(value["data"]["recipe_id"], "fixture-method");
    assert_eq!(value["data"]["version"], 1);
    let draft_path = PathBuf::from(value["data"]["path"].as_str().unwrap());
    assert!(draft_path.is_file(), "{}", draft_path.display());
    assert!(draft_path.starts_with(drafts_dir(local.path())));
    // The method library is unchanged (as-draft writes only a draft).
    assert!(!method_path(local.path(), "fixture-method", 1).exists());

    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    let list = listed.json();
    assert!(list["data"]["recipes"].as_array().unwrap().is_empty());
    let drafts = list["data"]["drafts"].as_array().unwrap();
    assert_eq!(drafts.len(), 1, "{list}");
    assert_eq!(drafts[0]["id"], draft_id);
    assert_eq!(drafts[0]["source_url"], fixture.file_url());
    assert_eq!(drafts[0]["scope_hint"], "narrow");
    assert!(drafts[0]["source_run_id"]
        .as_str()
        .unwrap()
        .starts_with("import:"));

    // A dry-run as-draft must not write another draft.
    let dry = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--as-draft",
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(dry.code, 0, "stderr: {}", dry.stderr);
    assert_eq!(dry.json()["data"]["dry_run"], true);
    assert!(dry.json()["data"].get("path").is_none());
    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    assert_eq!(listed.json()["data"]["drafts"].as_array().unwrap().len(), 1);

    // Publish the imported draft: two fresh samples + a non-matching negative.
    let published = run_local(
        &[
            "recipe",
            "publish",
            &draft_id,
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
    assert_eq!(published.code, 0, "stderr: {}", published.stderr);
    let value = published.json();
    assert_eq!(value["data"]["published"], true, "{value}");
    assert_eq!(value["data"]["recipe"]["id"], "fixture-method");
    assert_eq!(value["data"]["scope"], "scoped");
    assert!(method_path(local.path(), "fixture-method", 1).is_file());
    let index: Value = serde_json::from_slice(
        &std::fs::read(resolve_dir(local.path()).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index["entries"]["fixture-method"]["active_version"], 1);
    let listed = run_local(&["recipe", "list", "--output", "json"], local.path()).await;
    assert!(listed.json()["data"]["drafts"]
        .as_array()
        .unwrap()
        .is_empty());

    // The published method now replays deterministically.
    let resolved = run_local(
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

// ---------------------------------------------------------------------------
// import --publish (recipe tests → R1–R6 → library)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_publish_runs_gates_and_writes_the_method() {
    let fixture = TestServer::start(Config::new(64 * 1024)).await.unwrap();
    let local = TempDir::new("import-publish");
    write_policy(local.path());
    let source = local.path().join("incoming").join("publish-method.json");
    let recipe = fixture_recipe(
        "publish-method",
        1,
        &fixture.file_url(),
        "https://example.com/not-a-download",
    );
    write_json(&source, &recipe);

    // Dry-run: required gates pass (one positive sample → R4 narrows the scope).
    let dry = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--publish",
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(dry.code, 0, "stderr: {}", dry.stderr);
    let value = dry.json();
    assert_eq!(value["data"]["imported"], false);
    assert_eq!(value["data"]["dry_run"], true);
    let report = &value["data"]["report"];
    assert_eq!(report["required_passed"], true, "{value}");
    for id in ["R1", "R2", "R3", "R5", "R6"] {
        assert_eq!(gate(report, id)["pass"], true, "gate {id}: {value}");
    }
    assert_eq!(
        gate(report, "R4")["pass"],
        false,
        "one sample only: {value}"
    );
    assert_eq!(report["scope"], "narrow");
    assert!(!method_path(local.path(), "publish-method", 1).exists());

    // Real import --publish writes the method file + index.
    let published = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--publish",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(published.code, 0, "stderr: {}", published.stderr);
    let value = published.json();
    assert_eq!(value["data"]["imported"], true, "{value}");
    assert_eq!(value["data"]["report"]["required_passed"], true);
    assert_eq!(value["data"]["recipe"]["state"], "published");
    assert!(method_path(local.path(), "publish-method", 1).is_file());
    let index: Value = serde_json::from_slice(
        &std::fs::read(resolve_dir(local.path()).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index["entries"]["publish-method"]["active_version"], 1);

    // The method is matchable right away (recipe-only replay, no model).
    let resolved = run_local(
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
    assert_eq!(resolved["data"]["recipe"]["id"], "publish-method");

    // Re-running is idempotent (no duplicate gates/writes).
    let idempotent = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--publish",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(idempotent.code, 0, "stderr: {}", idempotent.stderr);
    assert_eq!(idempotent.json()["data"]["already_present"], true);
    fixture.shutdown().await;
}

#[tokio::test]
async fn import_publish_failed_gates_write_nothing() {
    let fixture = TestServer::start(Config::new(64 * 1024)).await.unwrap();
    let local = TempDir::new("import-publish-fail");
    write_policy(local.path());
    let source = local.path().join("incoming").join("broken-method.json");
    // The positive sample is a 404: the recipe replays but the emit never verifies.
    let recipe = fixture_recipe(
        "broken-method",
        1,
        &fixture.url("/missing-404"),
        "https://example.com/not-a-download",
    );
    write_json(&source, &recipe);

    for dry_run in [true, false] {
        let mut args = vec![
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--publish",
            "--output",
            "json",
        ];
        if dry_run {
            args.insert(4, "--dry-run");
        }
        let output = run_local(&args, local.path()).await;
        assert_eq!(
            output.code, 2,
            "dry_run={dry_run} stderr: {}",
            output.stderr
        );
        let value = output.json();
        assert_eq!(value["ok"], false, "{value}");
        assert_eq!(value["error"]["code"], "resolve_failed");
        let report = value["data"]["report"].clone();
        assert_eq!(report["required_passed"], false, "{value}");
        assert_eq!(gate(&report, "R3")["pass"], false, "{value}");
        assert!(!method_path(local.path(), "broken-method", 1).exists());
    }

    // Plain --dry-run import of the same (valid) recipe succeeds without writing.
    let dry = run_local(
        &[
            "recipe",
            "import",
            source.to_str().unwrap(),
            "--dry-run",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(dry.code, 0, "stderr: {}", dry.stderr);
    assert_eq!(dry.json()["data"]["dry_run"], true);
    assert!(!method_path(local.path(), "broken-method", 1).exists());
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// import: invalid inputs / clap conflicts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_rejects_missing_invalid_and_oversized_files() {
    let local = TempDir::new("import-bad");
    write_policy(local.path());

    let missing = run_local(
        &[
            "recipe",
            "import",
            local.path().join("nope.json").to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(missing.code, 2);
    assert_eq!(missing.json()["error"]["code"], "not_found");

    let invalid_json = local.path().join("invalid.json");
    std::fs::write(&invalid_json, b"{not json").unwrap();
    let invalid = run_local(
        &[
            "recipe",
            "import",
            invalid_json.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(invalid.code, 2);
    let value = invalid.json();
    assert_eq!(value["error"]["code"], "invalid_request");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid recipe"),
        "{value}"
    );

    let invalid_recipe = local.path().join("invalid-recipe.json");
    std::fs::write(&invalid_recipe, b"{\"schema\":\"rdm.recipe/v1\"}").unwrap();
    let invalid = run_local(
        &[
            "recipe",
            "import",
            invalid_recipe.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(invalid.code, 2);
    assert_eq!(invalid.json()["error"]["code"], "invalid_request");

    let oversized = local.path().join("oversized.json");
    std::fs::write(&oversized, " ".repeat(1024 * 1024 + 1)).unwrap();
    let oversized_output = run_local(
        &[
            "recipe",
            "import",
            oversized.to_str().unwrap(),
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(oversized_output.code, 2);
    assert_eq!(oversized_output.json()["error"]["code"], "invalid_request");
    assert!(oversized_output.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("import limit"));

    // --as-draft and --publish are mutually exclusive (clap usage error, exit 2).
    let conflicts = run_local(
        &[
            "recipe",
            "import",
            invalid_recipe.to_str().unwrap(),
            "--as-draft",
            "--publish",
            "--output",
            "json",
        ],
        local.path(),
    )
    .await;
    assert_eq!(conflicts.code, 2, "stderr: {}", conflicts.stderr);
}
