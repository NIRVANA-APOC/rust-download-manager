//! Recipe registry on-disk behavior: persistence layout, tolerant loading,
//! validation on insert and selection ordering (priority → specificity → version).

mod common;

use rdm_resolve::{Recipe, RecipeRegistry};
use serde_json::json;
use url::Url;

use common::{recipe_value, ScratchDir};

fn registry_recipe(id: &str, version: u32, pattern: &str, priority: i32) -> Recipe {
    recipe_value(
        id,
        version,
        json!({
            "hosts": ["example.com"],
            "url_pattern": pattern,
            "priority": priority
        }),
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }]),
        json!({ "url": "https://cdn.example.com/file.bin" }),
    )
}

fn example_url(path: &str) -> Url {
    Url::parse(&format!("https://example.com{path}")).expect("valid URL")
}

#[test]
fn saves_and_reloads_newest_version() {
    let scratch = ScratchDir::new("registry-reload");
    let registry = RecipeRegistry::with_root(scratch.path());
    assert_eq!(registry.root(), Some(scratch.path()));

    registry
        .save(&registry_recipe(
            "example.com-file",
            1,
            "^https://example\\.com/",
            0,
        ))
        .expect("save v1");
    registry
        .save(&registry_recipe(
            "example.com-file",
            2,
            "^https://example\\.com/d/\\d+$",
            0,
        ))
        .expect("save v2");
    assert!(scratch.join("example.com-file@1.json").is_file());
    assert!(scratch.join("example.com-file@2.json").is_file());

    let reloaded = RecipeRegistry::with_root(scratch.path());
    assert_eq!(reloaded.len(), 2);
    assert_eq!(reloaded.get("example.com-file", None).unwrap().version, 2);
    assert_eq!(
        reloaded.get("example.com-file", Some(1)).unwrap().version,
        1
    );
    assert!(reloaded.get("missing-recipe", None).is_none());
}

#[test]
fn loading_skips_invalid_recipe_files() {
    let scratch = ScratchDir::new("registry-invalid");
    std::fs::write(scratch.join("broken.json"), "{ not a recipe").expect("write broken fixture");
    std::fs::write(scratch.join("not-json.txt"), "ignored").expect("write ignored fixture");
    let registry = RecipeRegistry::with_root(scratch.path());
    registry
        .save(&registry_recipe("valid", 1, "^https://example\\.com/", 0))
        .expect("save valid recipe");

    let reloaded = RecipeRegistry::with_root(scratch.path());

    assert_eq!(reloaded.len(), 1, "invalid files are skipped, not fatal");
    assert!(reloaded.get("valid", None).is_some());
}

#[test]
fn selection_prefers_priority_then_specificity() {
    let mut registry = RecipeRegistry::empty();
    registry
        .insert(registry_recipe("loose", 1, "^https://example\\.com/", 0))
        .expect("insert loose");
    registry
        .insert(registry_recipe(
            "tight",
            1,
            "^https://example\\.com/d/\\d+$",
            0,
        ))
        .expect("insert tight");

    let hit = registry
        .find_for_url(&example_url("/d/1"))
        .expect("a match exists");
    assert_eq!(hit.recipe.id, "tight", "more specific pattern wins ties");
    assert!(hit.specificity > 0);
    assert!(registry.find_for_url(&example_url("/about")).is_some());

    registry
        .insert(registry_recipe("priority", 1, "^https://example\\.com/", 5))
        .expect("insert priority");
    let hit = registry
        .find_for_url(&example_url("/d/1"))
        .expect("a match exists");
    assert_eq!(hit.recipe.id, "priority", "priority beats specificity");
}

#[test]
fn selection_falls_back_to_newest_version() {
    let mut registry = RecipeRegistry::empty();
    registry
        .insert(registry_recipe("same", 1, "^https://example\\.com/", 0))
        .expect("insert v1");
    registry
        .insert(registry_recipe("same", 2, "^https://example\\.com/", 0))
        .expect("insert v2");

    let hit = registry
        .find_for_url(&example_url("/d/1"))
        .expect("a match exists");
    assert_eq!(hit.recipe.version, 2);
}

#[test]
fn insert_replaces_same_id_and_version() {
    let mut registry = RecipeRegistry::empty();
    registry
        .insert(registry_recipe(
            "example.com-file",
            1,
            "^https://example\\.com/",
            0,
        ))
        .expect("insert v1");
    registry
        .insert(registry_recipe(
            "example.com-file",
            1,
            "^https://example\\.com/d/",
            0,
        ))
        .expect("replace v1");

    assert_eq!(registry.len(), 1);
    assert_eq!(
        registry
            .get("example.com-file", Some(1))
            .unwrap()
            .matcher
            .url_pattern
            .as_deref(),
        Some("^https://example\\.com/d/")
    );
}

#[test]
fn insert_rejects_invalid_recipe() {
    // Deserialize without `validate` so the registry is the component under test.
    let invalid: Recipe = serde_json::from_value(json!({
        "schema": "rdm.recipe/v1",
        "id": "invalid",
        "version": 1,
        "match": { "hosts": ["example.com"] },
        "steps": [{ "id": "s", "op": "eval_js" }],
        "emit": { "url": "https://cdn.example.com/file.bin" }
    }))
    .expect("structurally deserializable");

    let mut registry = RecipeRegistry::empty();
    let error = registry
        .insert(invalid)
        .expect_err("invalid op is rejected");
    assert!(error.contains("eval_js"), "unexpected error: {error}");
    assert!(registry.is_empty());
}
