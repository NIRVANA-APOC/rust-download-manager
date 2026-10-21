//! Persisted method index (`docs/architecture.md` §11.10, schema
//! `rdm.recipe-index/v1`): active_version selection/rollback, health
//! persistence and tolerant loading of a corrupt index.

mod common;

use std::path::PathBuf;

use common::{recipe_value, ScratchDir};
use rdm_resolve::{
    index_path, MethodStatus, Recipe, RecipeRegistry, ResolveErrorKind, INDEX_SCHEMA,
};
use serde_json::{json, Value};
use url::Url;

// ---------------------------------------------------------------------------
// Fixtures
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

fn load_registry(scratch: &ScratchDir) -> RecipeRegistry {
    RecipeRegistry::with_root(recipes_root(scratch))
}

fn assert_step() -> Value {
    json!([{
        "id": "s",
        "op": "assert",
        "args": { "kind": "equals", "value": "ok", "expected": "ok" }
    }])
}

fn site_recipe(id: &str, version: u32, url_pattern: &str) -> Recipe {
    recipe_value(
        id,
        version,
        json!({
            "hosts": ["127.0.0.1"],
            "url_pattern": url_pattern,
            "priority": 0
        }),
        assert_step(),
        json!({ "url": format!("https://cdn.example.com/{id}-{version}.bin") }),
    )
}

fn broad_recipe(id: &str, version: u32) -> Recipe {
    site_recipe(id, version, r"^http://127\.0\.0\.1:\d+/page/")
}

fn precise_recipe(id: &str, version: u32) -> Recipe {
    site_recipe(id, version, r"^http://127\.0\.0\.1:\d+/page/\d+/special$")
}

fn special_url() -> Url {
    Url::parse("http://127.0.0.1:9000/page/7/special").expect("valid URL")
}

fn save_and_insert(registry: &mut RecipeRegistry, recipe: &Recipe) {
    registry.save(recipe).expect("save recipe file");
    registry.insert(recipe.clone()).expect("insert recipe");
}

// ---------------------------------------------------------------------------
// 1. Documented `<state>/resolve/` layout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_path_follows_the_documented_state_layout() {
    let scratch = ScratchDir::new("index-layout");
    let registry = load_registry(&scratch);
    assert_eq!(
        registry.index_path(),
        Some(resolve_root(&scratch).join("index.json")),
        "recipes dir named `recipes` keeps index.json one level up"
    );
    assert_eq!(
        index_path(&recipes_root(&scratch)),
        resolve_root(&scratch).join("index.json")
    );

    // A custom (non-`recipes`) root keeps the index inside the root.
    let custom = scratch.join("custom-root");
    let custom_registry = RecipeRegistry::with_root(&custom);
    assert_eq!(
        custom_registry.index_path(),
        Some(custom.join("index.json"))
    );
    assert_eq!(index_path(&custom), custom.join("index.json"));

    // No root -> no path.
    assert_eq!(RecipeRegistry::empty().index_path(), None);
}

// ---------------------------------------------------------------------------
// 2. Inserting a new version promotes active_version; get(None) stays newest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn insert_promotes_active_version_but_get_none_returns_newest() {
    let scratch = ScratchDir::new("index-insert");
    let mut registry = load_registry(&scratch);
    registry.insert(broad_recipe("site", 1)).expect("insert v1");
    assert_eq!(registry.active_version("site"), Some(1));
    registry.insert(broad_recipe("site", 2)).expect("insert v2");
    assert_eq!(
        registry.active_version("site"),
        Some(2),
        "active_version = max(existing, inserted)"
    );
    assert_eq!(
        registry.get("site", None).expect("id exists").version,
        2,
        "get(_, None) keeps returning the newest version"
    );
    assert_eq!(registry.get("site", Some(1)).expect("v1").version, 1);

    // Re-inserting an older version must never regress the active version.
    registry
        .insert(broad_recipe("site", 1))
        .expect("reinsert v1");
    assert_eq!(registry.active_version("site"), Some(2));
    // A newer version promotes again.
    registry.insert(broad_recipe("site", 3)).expect("insert v3");
    assert_eq!(registry.active_version("site"), Some(3));
}

// ---------------------------------------------------------------------------
// 3. Matching only considers the active_version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_for_url_uses_only_the_active_version() {
    let scratch = ScratchDir::new("index-active-match");
    let mut registry = load_registry(&scratch);
    save_and_insert(&mut registry, &broad_recipe("site", 1));
    save_and_insert(&mut registry, &precise_recipe("site", 2));

    let url = special_url();
    assert_eq!(
        registry.find_for_url(&url).expect("match").recipe.version,
        2,
        "the newest version is active after insertion"
    );

    registry
        .set_active_version("site", 1)
        .expect("rollback to v1");
    assert_eq!(
        registry.find_for_url(&url).expect("match").recipe.version,
        1,
        "only the active version participates in matching"
    );
    assert_eq!(
        registry.get("site", None).expect("newest").version,
        2,
        "rollback only moves the index; the newest file is still there"
    );
}

// ---------------------------------------------------------------------------
// 4. set_active_version persists (atomic index rewrite + reload)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_active_version_persists_across_registry_reload() {
    let scratch = ScratchDir::new("index-rollback-persist");
    let mut registry = load_registry(&scratch);
    save_and_insert(&mut registry, &broad_recipe("site", 1));
    save_and_insert(&mut registry, &precise_recipe("site", 2));

    registry
        .set_active_version("site", 1)
        .expect("rollback to v1");
    let raw = std::fs::read_to_string(index_file(&scratch)).expect("index file written");
    let json: Value = serde_json::from_str(&raw).expect("index is JSON");
    assert_eq!(json["schema"], INDEX_SCHEMA);
    assert_eq!(json["entries"]["site"]["active_version"], 1);

    let reloaded = load_registry(&scratch);
    assert_eq!(reloaded.active_version("site"), Some(1));
    assert_eq!(
        reloaded
            .find_for_url(&special_url())
            .expect("match")
            .recipe
            .version,
        1
    );

    // The rollback did not rewrite the recipe files themselves.
    let v1_file = recipes_root(&scratch).join("site@1.json");
    let v2_file = recipes_root(&scratch).join("site@2.json");
    assert!(v1_file.is_file());
    assert!(v2_file.is_file());
}

// ---------------------------------------------------------------------------
// 5. set_active_version rejects unknown ids/versions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_active_version_rejects_unknown_versions() {
    let scratch = ScratchDir::new("index-rollback-invalid");
    let mut registry = load_registry(&scratch);
    save_and_insert(&mut registry, &broad_recipe("site", 1));

    let error = registry
        .set_active_version("site", 9)
        .expect_err("version 9 does not exist");
    assert!(error.contains("does not exist"), "{error}");
    assert!(registry.set_active_version("missing", 1).is_err());
    // A failed rollback leaves the persisted index untouched.
    assert_eq!(registry.active_version("site"), Some(1));
}

// ---------------------------------------------------------------------------
// 6. Health persists in the index and survives a reload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_is_persisted_in_the_index_and_survives_reload() {
    let scratch = ScratchDir::new("index-health");
    let mut registry = load_registry(&scratch);
    save_and_insert(&mut registry, &precise_recipe("site", 2));

    registry
        .record_success("site", 2, "run_ok", 100)
        .expect("record success");
    registry
        .record_failure(
            "site",
            2,
            "run_bad",
            ResolveErrorKind::RecipeStale,
            Some("url"),
            200,
        )
        .expect("record failure");

    let health = registry.health("site");
    assert_eq!(
        health.status,
        MethodStatus::Ok,
        "a success stays `ok`; the first structural failure only counts"
    );
    assert_eq!(health.consecutive_failures, 1);
    assert_eq!(health.last_ok_ms, Some(100));
    assert_eq!(
        health
            .last_failure
            .as_ref()
            .map(|failure| failure.run_id.as_str()),
        Some("run_bad")
    );

    let reloaded = load_registry(&scratch);
    assert_eq!(reloaded.health("site"), health);
    assert_eq!(
        reloaded.active_version("site"),
        Some(2),
        "health writes must not disturb active_version"
    );
    let raw = std::fs::read_to_string(index_file(&scratch)).expect("index file");
    assert!(raw.contains("\"recipe_stale\""), "{raw}");
    assert!(raw.contains("\"ok\""), "{raw}");
}

// ---------------------------------------------------------------------------
// 7. Corrupt index: recipes still load, health falls back to untested
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_index_is_ignored_but_recipes_still_load() {
    let scratch = ScratchDir::new("index-corrupt");
    let mut registry = load_registry(&scratch);
    save_and_insert(&mut registry, &broad_recipe("site", 1));
    std::fs::write(index_file(&scratch), "{ this is not json").expect("corrupt index");

    let reloaded = load_registry(&scratch);
    assert_eq!(reloaded.len(), 1, "recipes survive a corrupt index");
    assert!(reloaded.index().entries.is_empty());
    assert_eq!(reloaded.active_version("site"), None);
    assert_eq!(reloaded.health("site").status, MethodStatus::Untested);
    // Matching falls back to the newest version (which is v1 here).
    let broad = Url::parse("http://127.0.0.1:9000/page/9").expect("url");
    assert_eq!(
        reloaded.find_for_url(&broad).expect("match").recipe.version,
        1
    );
}

// ---------------------------------------------------------------------------
// 8. No root: index writes are explicit errors, never silent successes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn save_index_without_root_is_an_explicit_error() {
    let registry = RecipeRegistry::empty();
    let error = registry.save_index().expect_err("no root");
    assert!(error.to_string().contains("no root"), "{error}");

    let mut registry = RecipeRegistry::empty();
    registry.insert(broad_recipe("site", 1)).expect("insert");
    assert!(registry.record_success("site", 1, "run_1", 1).is_err());
    assert!(registry
        .record_failure("site", 1, "run_2", ResolveErrorKind::RecipeStale, None, 2)
        .is_err());
}

// ---------------------------------------------------------------------------
// 9. Old index JSON without optional fields still loads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn old_index_json_without_optional_fields_loads() {
    let scratch = ScratchDir::new("index-old");
    std::fs::create_dir_all(resolve_root(&scratch)).expect("resolve dir");
    std::fs::write(
        index_file(&scratch),
        r#"{"schema":"rdm.recipe-index/v1","entries":{"site":{"active_version":3}}}"#,
    )
    .expect("old index");

    let registry = load_registry(&scratch);
    assert_eq!(registry.active_version("site"), Some(3));
    assert_eq!(registry.health("site").status, MethodStatus::Untested);
    assert_eq!(registry.index().schema, INDEX_SCHEMA);
}

// ---------------------------------------------------------------------------
// 10. Recipes on disk without an index: newest version wins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_index_falls_back_to_the_newest_version() {
    let scratch = ScratchDir::new("index-missing");
    let registry = load_registry(&scratch);
    registry.save(&broad_recipe("site", 1)).expect("save v1");
    registry.save(&precise_recipe("site", 2)).expect("save v2");
    assert!(!index_file(&scratch).exists());

    let reloaded = load_registry(&scratch);
    assert_eq!(reloaded.len(), 2);
    assert_eq!(reloaded.active_version("site"), None);
    assert_eq!(
        reloaded
            .find_for_url(&special_url())
            .expect("match")
            .recipe
            .version,
        2,
        "without an index the matcher falls back to the newest version"
    );
}
