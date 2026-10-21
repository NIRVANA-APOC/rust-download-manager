//! Version-level method health (`docs/architecture.md` §11.10 v0.15,
//! `docs/agent-integration.md` v2.1 §8.11).
//!
//! `index.json` is now schema `rdm.recipe-index/v2`: every method id keeps a
//! `versions: {"<version>": RecipeHealth}` map while `entry.health` stays the
//! mirror of `active_version` (old clients keep their semantics). Recording a
//! replay writes the *actually replayed* version; inserting a new version keeps
//! the old version's history but starts the new one `untested`; rolling back
//! switches the mirror; v1 index files remain readable and are upgraded on the
//! next write.

mod common;
mod support;

use std::path::PathBuf;

use common::{recipe_value, ScratchDir};
use rdm_resolve::{
    MethodStatus, Recipe, RecipeRegistry, ResolveErrorKind, ResolveMode, ResolvePolicy,
    ResolveRequest, ResolverService, RunStatus, INDEX_SCHEMA,
};
use serde_json::json;
use support::{MockServer, Reply};

// ---------------------------------------------------------------------------
// Registry fixtures
// ---------------------------------------------------------------------------

fn state_dir(scratch: &ScratchDir) -> PathBuf {
    scratch.path().to_path_buf()
}

fn resolve_root(scratch: &ScratchDir) -> PathBuf {
    state_dir(scratch).join("resolve")
}

fn recipes_root(scratch: &ScratchDir) -> PathBuf {
    resolve_root(scratch).join("recipes")
}

fn index_file(scratch: &ScratchDir) -> PathBuf {
    resolve_root(scratch).join("index.json")
}

fn registry(scratch: &ScratchDir) -> RecipeRegistry {
    RecipeRegistry::with_root(recipes_root(scratch))
}

fn reloaded(scratch: &ScratchDir) -> RecipeRegistry {
    RecipeRegistry::with_root(recipes_root(scratch))
}

fn simple_recipe(id: &str, version: u32) -> Recipe {
    recipe_value(
        id,
        version,
        json!({ "hosts": ["127.0.0.1"], "priority": 0 }),
        json!([{
            "id": "pass",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }]),
        json!({ "url": format!("https://cdn.example.com/{id}-{version}.bin") }),
    )
}

fn save_and_insert(registry: &mut RecipeRegistry, recipe: &Recipe) {
    registry.save(recipe).expect("save recipe file");
    registry.insert(recipe.clone()).expect("insert recipe");
}

fn structural_failure(registry: &mut RecipeRegistry, id: &str, version: u32, run: u64) {
    let run_id = format!("run_{run}");
    registry
        .record_failure(
            id,
            version,
            &run_id,
            ResolveErrorKind::RecipeStale,
            Some("url"),
            run,
        )
        .expect("record failure");
}

fn success(registry: &mut RecipeRegistry, id: &str, version: u32, run: u64) {
    registry
        .record_success(id, version, &format!("run_{run}"), run)
        .expect("record success");
}

fn read_index(scratch: &ScratchDir) -> serde_json::Value {
    let raw = std::fs::read_to_string(index_file(scratch)).expect("index.json exists");
    serde_json::from_str(&raw).expect("index.json is JSON")
}

fn write_recipe_file(scratch: &ScratchDir, recipe: &Recipe) {
    std::fs::create_dir_all(recipes_root(scratch)).expect("recipes dir");
    let path = recipes_root(scratch).join(format!("{}@{}.json", recipe.id, recipe.version));
    std::fs::write(
        path,
        serde_json::to_vec_pretty(recipe).expect("recipe JSON"),
    )
    .expect("write recipe file");
}

// ---------------------------------------------------------------------------
// Service-level fixture site
// ---------------------------------------------------------------------------

fn make_request(url: &str, mode: ResolveMode) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        mode,
        ..ResolveRequest::default()
    }
}

/// Site: `/api/drift` serves HTML (drifts a recipe expecting JSON),
/// `/file.bin` serves a file.
async fn service_site() -> MockServer {
    MockServer::start(|request| match request.path.as_str() {
        "/api/drift" => Reply::html("<!doctype html><html>maintenance</html>"),
        "/file.bin" => match request.method.as_str() {
            "HEAD" => Reply::file_head(1024).header("accept-ranges", "bytes"),
            _ => Reply::file(1024).header("accept-ranges", "bytes"),
        },
        "/page/42" => Reply::html("<!doctype html><html>share</html>"),
        _ => Reply::status(404),
    })
    .await
}

/// v1: structurally drifts (expects JSON, gets HTML).
fn drift_recipe_json(id: &str, api_url: &str) -> String {
    serde_json::json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": 1,
        "match": {
            "hosts": ["127.0.0.1"],
            "url_pattern": r"^http://127\.0\.0\.1:\d+/page/\d+$",
            "priority": 5
        },
        "steps": [{
            "id": "api",
            "op": "http_request",
            "args": { "url": api_url },
            "expect": { "content_type_prefix": ["application/json"] },
            "assign": { "body": "body_text" }
        }],
        "emit": { "url": "https://cdn.example.com/never.bin" }
    })
    .to_string()
}

/// v2: healthy (one passing assert, emits the fixture file).
fn healthy_recipe_json(id: &str, file_url: &str) -> String {
    serde_json::json!({
        "schema": "rdm.recipe/v1",
        "id": id,
        "version": 2,
        "match": {
            "hosts": ["127.0.0.1"],
            "url_pattern": r"^http://127\.0\.0\.1:\d+/page/\d+$",
            "priority": 5
        },
        "steps": [{
            "id": "pass",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }],
        "emit": { "url": file_url }
    })
    .to_string()
}

fn load_trace_state_root(fixture: &common::RecipeFixture) -> PathBuf {
    fixture.state_dir().join("resolve")
}

/// Recipe version recorded in a run trace header (failures set
/// `outcome.recipe = None` but keep the matched ref in the trace header).
fn traced_recipe_version(fixture: &common::RecipeFixture, run_id: &str) -> Option<u32> {
    let trace = rdm_resolve::TraceStore::load(&load_trace_state_root(fixture), run_id)
        .expect("run trace must load");
    trace.recipe.as_ref().map(|recipe| recipe.version)
}

// ---------------------------------------------------------------------------
// 1. Per-version isolation + active mirror
// ---------------------------------------------------------------------------

#[tokio::test]
async fn per_version_health_is_isolated_and_mirrors_the_active_version() {
    let scratch = ScratchDir::new("vhealth-isolation");
    let mut registry = registry(&scratch);
    save_and_insert(&mut registry, &simple_recipe("site", 1));

    structural_failure(&mut registry, "site", 1, 10);
    structural_failure(&mut registry, "site", 1, 20);
    let v1 = registry.health_for("site", 1);
    assert_eq!(v1.status, MethodStatus::Degraded);
    assert_eq!(v1.consecutive_failures, 2);
    assert_eq!(registry.health("site").status, MethodStatus::Degraded);

    // Inserting v2 moves active to 2 and resets the mirror; v1 history stays.
    save_and_insert(&mut registry, &simple_recipe("site", 2));
    assert_eq!(registry.active_version("site"), Some(2));
    assert_eq!(registry.health("site").status, MethodStatus::Untested);
    assert_eq!(
        registry.health_for("site", 2).status,
        MethodStatus::Untested
    );
    assert_eq!(
        registry.health_for("site", 1).status,
        MethodStatus::Degraded
    );

    success(&mut registry, "site", 2, 30);
    success(&mut registry, "site", 2, 40);
    let v2 = registry.health_for("site", 2);
    assert_eq!(v2.status, MethodStatus::Ok);
    assert_eq!(v2.consecutive_failures, 0);
    assert!(v2.last_ok_ms.is_some());
    assert_eq!(registry.health("site"), v2);
    assert_eq!(
        registry.health_for("site", 1).status,
        MethodStatus::Degraded
    );

    // Persisted across a reload.
    let reloaded = reloaded(&scratch);
    assert_eq!(reloaded.active_version("site"), Some(2));
    assert_eq!(reloaded.health_for("site", 2), v2);
    assert_eq!(
        reloaded.health_for("site", 1).status,
        MethodStatus::Degraded
    );
    assert_eq!(reloaded.health("site"), v2);
}

// ---------------------------------------------------------------------------
// 2. Rollback switches the active mirror
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rollback_switches_the_health_mirror() {
    let scratch = ScratchDir::new("vhealth-rollback");
    let mut registry = registry(&scratch);
    save_and_insert(&mut registry, &simple_recipe("site", 1));
    structural_failure(&mut registry, "site", 1, 10);
    structural_failure(&mut registry, "site", 1, 20);
    save_and_insert(&mut registry, &simple_recipe("site", 2));
    success(&mut registry, "site", 2, 30);

    registry.set_active_version("site", 1).expect("rollback");
    assert_eq!(
        registry.health("site").status,
        MethodStatus::Degraded,
        "the mirror follows the active version"
    );
    assert_eq!(registry.health("site"), registry.health_for("site", 1));
    assert_eq!(
        reloaded(&scratch).health("site").status,
        MethodStatus::Degraded
    );

    registry.set_active_version("site", 2).expect("forward");
    assert_eq!(registry.health("site").status, MethodStatus::Ok);
    assert_eq!(registry.health("site"), registry.health_for("site", 2));
    assert_eq!(reloaded(&scratch).health("site").status, MethodStatus::Ok);
    // History is untouched by rollbacks.
    assert_eq!(
        registry.health_for("site", 1).status,
        MethodStatus::Degraded
    );
}

// ---------------------------------------------------------------------------
// 3. A newly inserted version starts untested (no inheritance)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_version_does_not_inherit_the_old_health() {
    let scratch = ScratchDir::new("vhealth-new-version");
    let mut registry = registry(&scratch);
    save_and_insert(&mut registry, &simple_recipe("site", 1));
    success(&mut registry, "site", 1, 10);
    assert_eq!(registry.health("site").status, MethodStatus::Ok);

    save_and_insert(&mut registry, &simple_recipe("site", 2));
    assert_eq!(
        registry.health("site").status,
        MethodStatus::Untested,
        "a new version does not inherit the previous version's success"
    );
    assert_eq!(registry.health_for("site", 1).status, MethodStatus::Ok);
    assert_eq!(
        registry.health_for("site", 2).status,
        MethodStatus::Untested
    );
    // insert() promotes the active version in memory only; persist it first.
    registry.save_index().expect("save index");
    // ...and the same holds after a reload.
    let reloaded = reloaded(&scratch);
    assert_eq!(reloaded.health("site").status, MethodStatus::Untested);
    assert_eq!(reloaded.health_for("site", 1).status, MethodStatus::Ok);
}

// ---------------------------------------------------------------------------
// 4. Re-inserting the same version never resets its health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reinserting_the_same_version_keeps_its_health() {
    let scratch = ScratchDir::new("vhealth-reinsert");
    let mut registry = registry(&scratch);
    let recipe = simple_recipe("site", 1);
    save_and_insert(&mut registry, &recipe);
    structural_failure(&mut registry, "site", 1, 10);
    structural_failure(&mut registry, "site", 1, 20);
    assert_eq!(
        registry.health_for("site", 1).status,
        MethodStatus::Degraded
    );

    registry.insert(recipe).expect("re-insert same version");
    assert_eq!(registry.active_version("site"), Some(1));
    assert_eq!(
        registry.health_for("site", 1).status,
        MethodStatus::Degraded,
        "re-inserting the same version must not reset its history"
    );
    assert_eq!(registry.health("site").status, MethodStatus::Degraded);
}

// ---------------------------------------------------------------------------
// 5. index.json: schema v2 + versions map + active mirror
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_json_uses_schema_v2_and_persists_the_versions_map() {
    let scratch = ScratchDir::new("vhealth-json");
    let mut registry = registry(&scratch);
    save_and_insert(&mut registry, &simple_recipe("site", 1));
    structural_failure(&mut registry, "site", 1, 10);
    structural_failure(&mut registry, "site", 1, 20);
    save_and_insert(&mut registry, &simple_recipe("site", 2));
    success(&mut registry, "site", 2, 30);

    let index = read_index(&scratch);
    assert_eq!(index["schema"], INDEX_SCHEMA);
    assert_eq!(index["schema"], "rdm.recipe-index/v2");
    let entry = &index["entries"]["site"];
    assert_eq!(entry["active_version"], 2);
    let versions = entry["versions"]
        .as_object()
        .expect("versions map is an object");
    assert_eq!(versions.len(), 2, "versions: {versions:?}");
    assert_eq!(versions["1"]["status"], "degraded");
    assert_eq!(versions["1"]["consecutive_failures"], 2);
    assert_eq!(versions["2"]["status"], "ok");
    assert_eq!(versions["2"]["consecutive_failures"], 0);
    // entry.health is the mirror of the active version (v2).
    assert_eq!(entry["health"]["status"], "ok");
    assert_eq!(entry["health"]["consecutive_failures"], 0);

    let reloaded = reloaded(&scratch);
    assert_eq!(reloaded.health("site"), reloaded.health_for("site", 2));
    assert_eq!(
        reloaded.health_for("site", 1).status,
        MethodStatus::Degraded
    );
}

// ---------------------------------------------------------------------------
// 6. v1 index migration: old health is the active version's health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v1_index_is_read_as_the_active_versions_health() {
    let scratch = ScratchDir::new("vhealth-migration");
    write_recipe_file(&scratch, &simple_recipe("site", 1));
    write_recipe_file(&scratch, &simple_recipe("site", 2));
    std::fs::create_dir_all(resolve_root(&scratch)).expect("resolve dir");
    std::fs::write(
        index_file(&scratch),
        r#"{"schema":"rdm.recipe-index/v1","updated_ms":1,"entries":{"site":{"active_version":1,"health":{"status":"broken","last_ok_ms":null,"last_failure":null,"consecutive_failures":3,"evidence":[]}}}}"#,
    )
    .expect("old v1 index");

    let mut registry = registry(&scratch);
    assert_eq!(registry.len(), 2, "recipes still load");
    assert_eq!(registry.health("site").status, MethodStatus::Broken);
    assert_eq!(registry.health_for("site", 1).status, MethodStatus::Broken);
    assert_eq!(
        registry.health_for("site", 2).status,
        MethodStatus::Untested,
        "old files have no per-version records for other versions"
    );
    // Loading alone must not rewrite the file.
    let raw = std::fs::read_to_string(index_file(&scratch)).expect("index");
    assert!(raw.contains("\"rdm.recipe-index/v1\""));

    // Inserting the already-loaded v2 file promotes it to active and fixes the
    // old active health as versions[1] (v1-migration semantics).
    let v2 = registry.get("site", Some(2)).expect("v2 loaded").clone();
    registry.insert(v2).expect("promote v2");
    assert_eq!(registry.active_version("site"), Some(2));
    assert_eq!(registry.health_for("site", 1).status, MethodStatus::Broken);
    assert_eq!(registry.health("site").status, MethodStatus::Untested);

    // Recording the (now active) v2 writes v2 schema while v1's history stays.
    success(&mut registry, "site", 2, 10);
    assert_eq!(registry.health_for("site", 2).status, MethodStatus::Ok);
    assert_eq!(
        registry.health("site").status,
        MethodStatus::Ok,
        "the mirror follows the newly active v2"
    );
    let index = read_index(&scratch);
    assert_eq!(index["schema"], "rdm.recipe-index/v2");
    assert_eq!(
        index["entries"]["site"]["versions"]["1"]["status"],
        "broken"
    );
    assert_eq!(index["entries"]["site"]["versions"]["2"]["status"], "ok");

    // Rolling forward switches the mirror to v2 and keeps v1's history.
    registry.set_active_version("site", 2).expect("activate v2");
    assert_eq!(registry.health("site").status, MethodStatus::Ok);
    let reloaded = reloaded(&scratch);
    assert_eq!(reloaded.active_version("site"), Some(2));
    assert_eq!(reloaded.health("site").status, MethodStatus::Ok);
    assert_eq!(reloaded.health_for("site", 1).status, MethodStatus::Broken);
}

// ---------------------------------------------------------------------------
// 7. Unknown versions / missing index entries default to untested
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_versions_default_to_untested() {
    let scratch = ScratchDir::new("vhealth-unknown");
    let mut registry = registry(&scratch);
    save_and_insert(&mut registry, &simple_recipe("site", 1));
    success(&mut registry, "site", 1, 10);

    let unknown = registry.health_for("site", 99);
    assert_eq!(unknown, rdm_resolve::RecipeHealth::default());
    assert_eq!(unknown.status, MethodStatus::Untested);
    assert_eq!(
        registry.health_for("missing-id", 1).status,
        MethodStatus::Untested
    );

    // Recording an unknown version of a known id only adds that version.
    structural_failure(&mut registry, "site", 99, 20);
    assert_eq!(registry.health_for("site", 99).consecutive_failures, 1);
    assert_eq!(
        registry.health_for("site", 99).status,
        MethodStatus::Untested,
        "one structural failure is not enough to degrade"
    );
    assert_eq!(
        registry.health("site").status,
        MethodStatus::Ok,
        "the active (v1) mirror is untouched"
    );
}

// ---------------------------------------------------------------------------
// 8. Corrupt index / missing versions field are tolerated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_index_is_ignored_but_recipes_still_load() {
    let scratch = ScratchDir::new("vhealth-corrupt");
    write_recipe_file(&scratch, &simple_recipe("site", 1));
    std::fs::create_dir_all(resolve_root(&scratch)).expect("resolve dir");
    std::fs::write(index_file(&scratch), "{ this is not json").expect("corrupt index");

    let registry = registry(&scratch);
    assert_eq!(registry.len(), 1, "recipe files survive");
    assert!(registry.index().entries.is_empty());
    assert_eq!(registry.health("site").status, MethodStatus::Untested);
    let url = url::Url::parse("http://127.0.0.1:9000/page/1").expect("url");
    assert!(
        registry.find_for_url(&url).is_some(),
        "matching falls back to the newest file without an index"
    );
}

// ---------------------------------------------------------------------------
// 9. Service-level: real replays write the replayed version's health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn service_records_health_for_the_version_it_replayed() {
    let site = service_site().await;
    let fixture = common::RecipeFixture::new("vhealth-service");
    let page_url = site.url("/page/42");
    let file_url = site.url("/file.bin");

    // v1 drifts on both runs.
    fixture.save(&drift_recipe_json("versioned", &site.url("/api/drift")));
    let mut service = fixture.service();
    for _ in 0..2 {
        let outcome = service
            .resolve(make_request(&page_url, ResolveMode::RecipeOnly))
            .await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(
            outcome.recipe.is_none(),
            "failures keep the recipe ref in the trace header, not the outcome"
        );
        assert_eq!(traced_recipe_version(&fixture, &outcome.run_id), Some(1));
    }
    assert_eq!(
        service.registry().health_for("versioned", 1).status,
        MethodStatus::Degraded
    );
    assert_eq!(
        service.registry().health("versioned").status,
        MethodStatus::Degraded
    );

    // Add a healthy v2 and replay it (this promotes active_version to 2).
    let v2 = fixture.save(&healthy_recipe_json("versioned", &file_url));
    let mut service = fixture.service();
    service
        .registry_mut()
        .insert(v2)
        .expect("insert v2 into a reloaded registry");
    let outcome = service
        .resolve(make_request(&page_url, ResolveMode::RecipeOnly))
        .await;
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.recipe.as_ref().map(|recipe| recipe.version),
        Some(2)
    );
    assert_eq!(
        service.registry().health_for("versioned", 2).status,
        MethodStatus::Ok
    );
    assert_eq!(
        service.registry().health("versioned").status,
        MethodStatus::Ok
    );
    assert_eq!(
        service.registry().health_for("versioned", 1).status,
        MethodStatus::Degraded,
        "the v1 history survives the v2 replay"
    );

    // Persisted for the next process.
    let reloaded = RecipeRegistry::with_root(fixture.recipes_dir());
    assert_eq!(reloaded.active_version("versioned"), Some(2));
    assert_eq!(reloaded.health_for("versioned", 2).status, MethodStatus::Ok);
    assert_eq!(
        reloaded.health_for("versioned", 1).status,
        MethodStatus::Degraded
    );
    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(load_trace_state_root(&fixture).join("index.json"))
            .expect("index"),
    )
    .expect("index JSON");
    assert_eq!(raw["schema"], "rdm.recipe-index/v2");
    assert!(raw["entries"]["versioned"]["versions"]["1"].is_object());
    assert!(raw["entries"]["versioned"]["versions"]["2"].is_object());
}

// ---------------------------------------------------------------------------
// 10. Service-level rollback: the replayed version is the rolled-back one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn service_rollback_records_health_on_the_target_version() {
    let site = service_site().await;
    let fixture = common::RecipeFixture::new("vhealth-service-rollback");
    let page_url = site.url("/page/42");
    let file_url = site.url("/file.bin");
    fixture.save(&drift_recipe_json("versioned", &site.url("/api/drift")));
    fixture.save(&healthy_recipe_json("versioned", &file_url));

    // Activate v1 through the persisted index, then let a fresh service load it.
    let mut registry = RecipeRegistry::with_root(fixture.recipes_dir());
    registry
        .set_active_version("versioned", 1)
        .expect("activate v1");
    drop(registry);
    let mut service =
        ResolverService::with_state_dir(fixture.state_dir(), ResolvePolicy::loopback());

    // A fresh `with_state_dir` reads index.json: active_version should be 1.
    assert_eq!(service.registry().active_version("versioned"), Some(1));
    let outcome = service
        .resolve(make_request(&page_url, ResolveMode::RecipeOnly))
        .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.recipe.is_none());
    assert_eq!(
        traced_recipe_version(&fixture, &outcome.run_id),
        Some(1),
        "the rolled-back version is the one that replays"
    );
    assert_eq!(
        service
            .registry()
            .health_for("versioned", 1)
            .consecutive_failures,
        1
    );
    assert_eq!(
        service.registry().health("versioned"),
        service.registry().health_for("versioned", 1),
        "the mirror follows the rollback target"
    );

    let reloaded = RecipeRegistry::with_root(fixture.recipes_dir());
    assert_eq!(reloaded.active_version("versioned"), Some(1));
    assert_eq!(reloaded.health_for("versioned", 1).consecutive_failures, 1);
    assert_eq!(reloaded.health("versioned").status, MethodStatus::Untested);
}
