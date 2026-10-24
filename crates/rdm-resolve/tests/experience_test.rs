//! Resolve experience library (`crates/rdm-resolve/src/experience.rs`,
//! design 17 §2.1): list/get projections, new-version edits, delete,
//! rollback, flow summaries and the v2 -> v4 index migration.

mod common;

use std::path::PathBuf;

use common::ScratchDir;
use rdm_resolve::{
    draft, list_drafts, save_draft, ExperienceStore, MethodStatus, Recipe, RecipeDraft,
    RecipeRegistry, DRAFT_SCHEMA, INDEX_SCHEMA,
};
use serde_json::json;

fn resolve_root(scratch: &ScratchDir) -> PathBuf {
    scratch.path().join("resolve")
}

fn recipes_root(scratch: &ScratchDir) -> PathBuf {
    resolve_root(scratch).join("recipes")
}

fn index_file(scratch: &ScratchDir) -> PathBuf {
    resolve_root(scratch).join("index.json")
}

fn store(scratch: &ScratchDir) -> ExperienceStore {
    ExperienceStore::from_state_dir(scratch.path())
}

fn recipe_json(id: &str, version: u32, name: &str) -> String {
    json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": version,
        "name": name,
        "description": "fixture method",
        "match": {
            "hosts": ["example.com"],
            "url_pattern": "^https://example\\.com/first$",
            "priority": 2
        },
        "captures": [],
        "limits": { "requests": 4, "bytes_read": 2048, "wall_ms": 5000, "redirects": 2 },
        "steps": [
            {
                "id": "fetch",
                "op": "http_request",
                "args": { "method": "GET", "url": "https://example.com/api", "max_bytes": 1024 },
                "expect": { "status_in": [200, 206] },
                "assign": { "o0": "body_json" },
                "when": { "kind": "exists", "var": "o0" }
            },
            {
                "id": "pick",
                "op": "json_query",
                "args": { "from": "o0", "pointer": "/file/url" },
                "assign": { "v0": "value" }
            },
            {
                "id": "check",
                "op": "assert",
                "args": { "kind": "matches", "value": "{{v0}}", "pattern": "^https?://" },
                "on_fail": "recipe_stale"
            }
        ],
        "emit": { "url": "{{v0}}", "name": "file.bin" },
        "tests": [{ "name": "sample", "input_url": "https://example.com/first" }]
    })
    .to_string()
}

fn recipe(id: &str, version: u32, name: &str) -> Recipe {
    Recipe::from_json(&recipe_json(id, version, name)).expect("valid fixture recipe")
}

fn save_recipe_file(scratch: &ScratchDir, recipe: &Recipe) -> PathBuf {
    let registry = RecipeRegistry::with_root(recipes_root(scratch));
    registry.save(recipe).expect("save recipe file")
}

fn seed(scratch: &ScratchDir, recipes: &[Recipe]) {
    let mut registry = RecipeRegistry::with_root(recipes_root(scratch));
    for recipe in recipes {
        registry.save(recipe).expect("save recipe file");
        registry.insert(recipe.clone()).expect("insert recipe");
    }
    registry.save_index().expect("save index");
}

fn write_index(scratch: &ScratchDir, value: serde_json::Value) {
    std::fs::create_dir_all(resolve_root(scratch)).expect("resolve dir");
    std::fs::write(
        index_file(scratch),
        serde_json::to_vec_pretty(&value).expect("index JSON"),
    )
    .expect("write index");
}

fn read_index(scratch: &ScratchDir) -> serde_json::Value {
    let raw = std::fs::read_to_string(index_file(scratch)).expect("index.json exists");
    serde_json::from_str(&raw).expect("index.json is JSON")
}

#[test]
fn list_and_get_project_versions_flow_and_metadata() {
    let scratch = ScratchDir::new("experience-list");
    save_recipe_file(&scratch, &recipe("site-a", 1, "Old A"));
    save_recipe_file(&scratch, &recipe("site-a", 2, ""));
    save_recipe_file(&scratch, &recipe("site-b", 1, "B method"));
    write_index(
        &scratch,
        json!({
            "schema": "rdm.recipe-index/v4",
            "updated_ms": 300,
            "entries": {
                "site-a": {
                    "active_version": 2,
                    "health": { "status": "ok" },
                    "versions": { "1": { "status": "degraded" }, "2": { "status": "ok" } },
                    "updated_ms": 300,
                    "created_ms": 10,
                    "source_url": "https://example.com/downloads/page",
                    "use_count": 4,
                    "success_count": 3,
                    "failure_count": 1,
                    "last_used_ms": 250,
                    "last_resources": 2,
                    "best_resources": 3,
                    "version_resources": { "1": 2, "2": 3 },
                    "version_created_ms": { "1": 10, "2": 20 }
                },
                "site-b": {
                    "active_version": 1,
                    "updated_ms": 200,
                    "created_ms": 5
                }
            }
        }),
    );

    let store = store(&scratch);
    let experiences = store.list();
    assert_eq!(
        experiences
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["site-a", "site-b"],
        "list is updated_ms descending, then id"
    );

    let first = store.get("site-a").expect("site-a experience");
    assert_eq!(first.active_version, 2);
    assert_eq!(first.name, "");
    assert_eq!(first.display_name(), "example.com/downloads/page");
    assert_eq!(first.scope_label(), "example.com · 精确地址");
    assert_eq!(first.created_ms, 10);
    assert_eq!(first.updated_ms, 300);
    assert_eq!(first.use_count, 4);
    assert_eq!(first.success_count, 3);
    assert_eq!(first.failure_count, 1);
    assert_eq!(first.last_used_ms, Some(250));
    assert_eq!(first.last_resources, 2);
    assert_eq!(first.best_resources, 3);
    assert_eq!(
        first.source_url.as_deref(),
        Some("https://example.com/downloads/page")
    );
    assert_eq!(first.health.status, MethodStatus::Ok);
    assert_eq!(first.tests.len(), 1);
    assert_eq!(first.recipe.version, 2);

    // Versions are oldest-first and keep per-version health/resources/times.
    assert_eq!(first.versions.len(), 2);
    assert_eq!(first.versions[0].version, 1);
    assert!(!first.versions[0].active);
    assert_eq!(first.versions[0].health.status, MethodStatus::Degraded);
    assert_eq!(first.versions[0].resources, 2);
    assert_eq!(first.versions[0].created_ms, Some(10));
    assert_eq!(first.versions[1].version, 2);
    assert!(first.versions[1].active);
    assert_eq!(first.versions[1].resources, 3);
    assert_eq!(first.versions[1].created_ms, Some(20));

    // Flow timeline of the active version.
    let flow = &first.flow;
    assert_eq!(flow.steps.len(), 3);
    assert_eq!(flow.steps[0].id, "fetch");
    assert_eq!(flow.steps[0].assignments, vec!["o0 ← body_json"]);
    assert_eq!(flow.steps[0].expects, vec!["status ∈ [200, 206]"]);
    assert_eq!(flow.steps[0].condition.as_deref(), Some("当 o0 存在"));
    assert_eq!(flow.steps[1].summary, "json_query /file/url");
    assert_eq!(flow.steps[2].on_fail.as_deref(), Some("recipe_stale"));
    assert_eq!(flow.captures, Vec::<String>::new());
    assert_eq!(flow.limits.requests, 4);
    assert_eq!(flow.emit.name.as_deref(), Some("file.bin"));

    assert!(store.get("missing").is_none());
}

#[test]
fn update_saves_a_new_version_and_keeps_old_files_and_health() {
    let scratch = ScratchDir::new("experience-update");
    let v1 = recipe("site-a", 1, "Original");
    seed(&scratch, std::slice::from_ref(&v1));
    // Give v1 a recorded health so the "old health survives" claim is testable.
    let mut registry = RecipeRegistry::with_root(recipes_root(&scratch));
    registry
        .record_success("site-a", 1, "run_ok", 100)
        .expect("record v1 health");

    let mut edited = recipe("ignored-id", 9, "Edited");
    edited.description = "edited description".to_owned();
    let experience = store(&scratch)
        .update("site-a", edited)
        .expect("update experience");
    assert_eq!(experience.id, "site-a");
    assert_eq!(experience.active_version, 2);
    assert_eq!(experience.name, "Edited");
    assert_eq!(experience.description, "edited description");
    assert_eq!(experience.recipe.id, "site-a", "recipe.id is forced to id");
    assert_eq!(experience.recipe.version, 2);
    assert_eq!(experience.versions.len(), 2);
    assert_eq!(experience.versions[0].health.status, MethodStatus::Ok);
    assert_eq!(experience.versions[1].health.status, MethodStatus::Untested);
    assert!(experience.versions[0].created_ms.is_some());

    let recipes = recipes_root(&scratch);
    assert!(recipes.join("site-a@1.json").is_file());
    assert!(recipes.join("site-a@2.json").is_file());
    let reloaded = RecipeRegistry::with_root(&recipes);
    assert_eq!(reloaded.active_version("site-a"), Some(2));
    assert_eq!(reloaded.health_for("site-a", 1).status, MethodStatus::Ok);
    assert_eq!(reloaded.latest_version("site-a"), Some(2));
    let index = read_index(&scratch);
    assert_eq!(index["schema"], INDEX_SCHEMA);
    assert!(index["entries"]["site-a"]["version_created_ms"]["2"].is_u64());

    // Invalid recipes are rejected before touching the disk.
    let invalid: Recipe = serde_json::from_value(json!({
        "schema": "rdm.recipe/v1",
        "id": "site-a",
        "version": 1,
        "match": { "hosts": ["example.com"] },
        "steps": [{ "id": "s", "op": "eval_js" }],
        "emit": { "url": "https://cdn.example.com/f.bin" }
    }))
    .expect("structurally deserializable");
    let error = store(&scratch)
        .update("site-a", invalid)
        .expect_err("invalid recipe rejected");
    assert!(error.contains("eval_js"), "{error}");
    assert_eq!(
        RecipeRegistry::with_root(&recipes).latest_version("site-a"),
        Some(2),
        "a rejected edit writes nothing"
    );

    assert!(store(&scratch)
        .update("missing", recipe("missing", 1, "x"))
        .is_err());
}

#[test]
fn delete_removes_all_versions_index_entry_and_drafts() {
    let scratch = ScratchDir::new("experience-delete");
    let v1 = recipe("site-a", 1, "A");
    let v2 = recipe("site-a", 2, "A v2");
    seed(&scratch, &[v1.clone(), v2]);
    let draft = RecipeDraft {
        schema: DRAFT_SCHEMA.to_owned(),
        id: "site-a".to_owned(),
        version: 1,
        created_ms: 1,
        source_run_id: "run_1".to_owned(),
        source_url: "https://example.com/first".to_owned(),
        scope_hint: "narrow".to_owned(),
        notes: Vec::new(),
        recipe: v1,
    };
    save_draft(&resolve_root(&scratch), &draft).expect("save same-id draft");
    assert_eq!(list_drafts(&resolve_root(&scratch)).len(), 1);

    let store = store(&scratch);
    assert!(store.delete("site-a").expect("delete experience"));
    let recipes = recipes_root(&scratch);
    assert!(!recipes.join("site-a@1.json").exists());
    assert!(!recipes.join("site-a@2.json").exists());
    assert!(read_index(&scratch)["entries"].get("site-a").is_none());
    assert!(list_drafts(&resolve_root(&scratch)).is_empty());
    assert!(store.get("site-a").is_none());
    assert!(
        !store.delete("site-a").expect("idempotent delete"),
        "false = did not exist"
    );
}

#[test]
fn set_active_version_rolls_back_and_persists() {
    let scratch = ScratchDir::new("experience-rollback");
    seed(
        &scratch,
        &[recipe("site-a", 1, "A v1"), recipe("site-a", 2, "A v2")],
    );
    let store = store(&scratch);
    let rolled_back = store
        .set_active_version("site-a", 1)
        .expect("rollback to v1");
    assert_eq!(rolled_back.active_version, 1);
    assert!(rolled_back.versions[0].active);
    assert!(!rolled_back.versions[1].active);
    assert_eq!(
        store.get("site-a").expect("reload").active_version,
        1,
        "rollback is persisted"
    );

    let error = store
        .set_active_version("site-a", 9)
        .expect_err("unknown version");
    assert!(error.contains("does not exist"), "{error}");
    assert!(store.set_active_version("missing", 1).is_err());
}

#[test]
fn v2_index_is_migrated_in_memory_and_rewritten_as_v4() {
    let scratch = ScratchDir::new("experience-migration");
    save_recipe_file(&scratch, &recipe("site-a", 1, "Legacy"));
    write_index(
        &scratch,
        json!({
            "schema": "rdm.recipe-index/v2",
            "updated_ms": 900,
            "entries": {
                "site-a": {
                    "active_version": 1,
                    "health": { "status": "ok" },
                    "versions": { "1": { "status": "ok" } },
                    "updated_ms": 900
                }
            }
        }),
    );
    let before = std::fs::read_to_string(index_file(&scratch)).expect("index");
    assert!(
        before.contains("rdm.recipe-index/v2"),
        "load does not rewrite"
    );

    let registry = RecipeRegistry::with_root(recipes_root(&scratch));
    assert_eq!(registry.index().schema, INDEX_SCHEMA);
    let entry = &registry.index().entries["site-a"];
    assert_eq!(entry.created_ms, 900, "created_ms falls back to updated_ms");
    assert_eq!(
        entry.source_url.as_deref(),
        Some("https://example.com/first"),
        "source_url is derived from tests[0].input_url"
    );
    assert_eq!(entry.best_resources, 0, "richness starts unknown");

    let experience = store(&scratch).get("site-a").expect("migrated experience");
    assert_eq!(experience.created_ms, 900);
    assert_eq!(
        experience.source_url.as_deref(),
        Some("https://example.com/first")
    );
    assert_eq!(experience.best_resources, 0);

    // The next write persists schema v4 plus the migrated metadata.
    let mut registry = RecipeRegistry::with_root(recipes_root(&scratch));
    registry
        .record_success("site-a", 1, "run_ok", 1000)
        .expect("record replay");
    let after = read_index(&scratch);
    assert_eq!(after["schema"], INDEX_SCHEMA);
    assert_eq!(after["schema"], "rdm.recipe-index/v4");
    assert_eq!(after["entries"]["site-a"]["created_ms"], 900);
    assert_eq!(
        after["entries"]["site-a"]["source_url"],
        "https://example.com/first"
    );
    assert_eq!(after["entries"]["site-a"]["use_count"], 1);
}

/// A v1 index with no optional fields still yields a usable experience
/// (oldest supported on-disk shape).
#[test]
fn v1_index_still_lists_an_experience() {
    let scratch = ScratchDir::new("experience-v1");
    save_recipe_file(&scratch, &recipe("site-a", 1, "Old"));
    write_index(
        &scratch,
        json!({
            "schema": "rdm.recipe-index/v1",
            "entries": { "site-a": { "active_version": 1 } }
        }),
    );
    let experience = store(&scratch).get("site-a").expect("v1 experience");
    assert_eq!(experience.active_version, 1);
    assert_eq!(experience.created_ms, 0);
    assert_eq!(experience.best_resources, 0);
    assert_eq!(experience.health.status, MethodStatus::Untested);
}

#[test]
fn root_is_the_resolve_directory() {
    let scratch = ScratchDir::new("experience-root");
    let store = ExperienceStore::new(resolve_root(&scratch));
    assert_eq!(store.root(), resolve_root(&scratch).as_path());
    assert!(list_drafts(store.root()).is_empty());
    assert_eq!(
        draft::drafts_root(store.root()),
        resolve_root(&scratch).join("drafts")
    );
}
