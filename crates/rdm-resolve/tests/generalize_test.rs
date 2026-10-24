//! Reusable experience templates (docs/design/18-experience-v2-reusable.md
//! §2.2): URL abstraction, recipe templating and idempotent library migration.
//!
//! The two recipes below are desensitized static samples of the real installed
//! experiences described in design 18 §0: both were captured from
//! `https://www.bilibili.com/video/BV1e3ny6KE3m/…` (cover) and a second video
//! page (video stream), with tracking query values and credentials removed.

mod common;

use common::ScratchDir;
use rdm_resolve::{
    abstract_url, generalize_recipe, migrate_experience_library, Recipe, RecipeRegistry,
};
use serde_json::json;
use url::Url;

const SAMPLE_URL: &str =
    "https://www.bilibili.com/video/BV1e3ny6KE3m/?trackid=100&spm_id_from=page.driver";
const VIDEO_SAMPLE_URL: &str =
    "https://www.bilibili.com/video/BV1XJn26BEa7/?trackid=200&spm_id_from=page.driver";

/// Desensitized cover experience: resolves the poster image of one video.
fn cover_recipe() -> Recipe {
    Recipe::from_json(
        &json!({
            "schema": "rdm.recipe/v1",
            "id": "draft_bilibili_cover",
            "version": 1,
            "name": "B站视频封面",
            "description": "desensitized sample of the real installed cover experience",
            "match": {
                "hosts": ["www.bilibili.com"],
                "url_pattern": "^https://www\\.bilibili\\.com/video/BV1e3ny6KE3m/\\?trackid=100&spm_id_from=page.driver$",
                "priority": 0
            },
            "captures": [],
            "steps": [
                {
                    "id": "s0",
                    "op": "http_request",
                    "args": {
                        "method": "GET",
                        "url": "https://api.bilibili.com/x/web-interface/view?bvid=BV1e3ny6KE3m",
                        "max_bytes": 65536
                    },
                    "expect": { "status_in": [200] },
                    "assign": { "o0": "body_json" }
                },
                {
                    "id": "s1",
                    "op": "json_query",
                    "args": { "from": "o0", "pointer": "/data/pic" },
                    "assign": { "v0": "value" }
                },
                {
                    "id": "s2",
                    "op": "assert",
                    "args": { "kind": "matches", "value": "{{v0}}", "pattern": "^https?://" }
                }
            ],
            "emit": { "url": "{{v0}}", "name": "BV1e3ny6KE3m_cover.jpg", "kind": "image" },
            "tests": [{ "name": "sample_input", "input_url": SAMPLE_URL }]
        })
        .to_string(),
    )
    .expect("valid cover recipe")
}

/// Desensitized video experience: resolves the video stream for a second page.
/// The internal `cid` is hardcoded (it is not part of the input URL).
fn video_recipe() -> Recipe {
    Recipe::from_json(
        &json!({
            "schema": "rdm.recipe/v1",
            "id": "draft_bilibili_video",
            "version": 1,
            "name": "B站视频",
            "description": "desensitized sample of the real installed video experience",
            "match": {
                "hosts": ["www.bilibili.com"],
                "url_pattern": "^https://www\\.bilibili\\.com/video/BV1XJn26BEa7/\\?trackid=200&spm_id_from=page.driver$",
                "priority": 0
            },
            "captures": [],
            "steps": [
                {
                    "id": "s0",
                    "op": "http_request",
                    "args": {
                        "method": "GET",
                        "url": "https://api.bilibili.com/x/web-interface/view?bvid=BV1XJn26BEa7",
                        "max_bytes": 65536
                    },
                    "expect": { "status_in": [200] },
                    "assign": { "o0": "body_json" }
                },
                {
                    "id": "s1",
                    "op": "json_query",
                    "args": { "from": "o0", "pointer": "/data/cid" },
                    "assign": { "cid": "value" }
                },
                {
                    "id": "s2",
                    "op": "template",
                    "args": {
                        "template": "https://api.bilibili.com/x/player/playurl?bvid=BV1XJn26BEa7&cid=42944954746&qn=80"
                    },
                    "assign": { "api_url": "value" }
                },
                {
                    "id": "s3",
                    "op": "http_request",
                    "args": { "method": "GET", "url": "{{api_url}}", "max_bytes": 65536 },
                    "expect": { "status_in": [200] },
                    "assign": { "o1": "body_json" }
                },
                {
                    "id": "s4",
                    "op": "json_query",
                    "args": { "from": "o1", "pointer": "/data/dash/0/url" },
                    "assign": { "v0": "value" }
                },
                {
                    "id": "s5",
                    "op": "assert",
                    "args": { "kind": "matches", "value": "{{v0}}", "pattern": "^https?://" }
                }
            ],
            "emit": { "url": "{{v0}}", "name": "BV1XJn26BEa7_1080p.mp4", "kind": "video" },
            "tests": [{
                "name": "sample_input",
                "input_url": "https://www.bilibili.com/video/BV1XJn26BEa7/?trackid=200&spm_id_from=page.driver"
            }]
        })
        .to_string(),
    )
    .expect("valid video recipe")
}

#[test]
fn abstracts_bvid_and_ignores_query_and_fragment() {
    let shape = abstract_url(SAMPLE_URL).expect("abstract bilibili URL");
    assert_eq!(shape.scope, "www.bilibili.com/video");
    assert_eq!(
        shape.captures,
        vec![("bvid".to_owned(), "BV1e3ny6KE3m".to_owned())]
    );
    assert!(shape.reusable);
    assert!(
        shape
            .pattern
            .starts_with("^https?://www\\.bilibili\\.com/video/"),
        "pattern must stop at the static /video/ prefix: {}",
        shape.pattern
    );
    assert!(
        !shape.pattern.contains("BV1e3ny6KE3m"),
        "the sample value must be abstracted: {}",
        shape.pattern
    );
    assert!(shape.pattern.contains("(?P<bvid>BV[0-9A-Za-z]+)"));
    assert!(shape.pattern.contains("(?:\\?.*)?"));
}

#[test]
fn cover_recipe_becomes_a_reusable_template() {
    let report = generalize_recipe(&cover_recipe(), SAMPLE_URL);
    assert!(report.changed);
    let recipe = &report.recipe;
    recipe.validate().expect("generalized recipe validates");

    assert_eq!(recipe.matcher.hosts, vec!["www.bilibili.com".to_owned()]);
    assert_eq!(recipe.captures, vec!["bvid".to_owned()]);
    let pattern = recipe.matcher.url_pattern.as_deref().expect("pattern");
    assert!(pattern.starts_with("^https?://www\\.bilibili\\.com/video/"));
    assert!(pattern.contains("(?P<bvid>BV[0-9A-Za-z]+)"));
    assert!(!pattern.contains("BV1e3ny6KE3m"));

    let generalization = recipe.generalization.as_ref().expect("generalization");
    assert_eq!(generalization.scope, "www.bilibili.com/video");
    assert_eq!(generalization.captures, vec!["bvid".to_owned()]);
    assert!(generalization.reusable);
    assert!(generalization.leftover_hardcoded.is_empty());

    assert_eq!(
        recipe.steps[0].args["url"].as_str(),
        Some("https://api.bilibili.com/x/web-interface/view?bvid={{bvid}}")
    );
    assert_eq!(
        recipe.emit.name, None,
        "a sample-derived file name must be dropped so replay derives it from the final URL"
    );
}

#[test]
fn static_emit_names_survive_generalization_but_sample_derived_ones_do_not() {
    // A pure static name (no sample value, no long internal id) is kept.
    let mut static_name = cover_recipe();
    static_name.emit.name = Some("cover.jpg".to_owned());
    let report = generalize_recipe(&static_name, SAMPLE_URL);
    assert_eq!(
        report.recipe.emit.name.as_deref(),
        Some("cover.jpg"),
        "a static file name is reusable as-is"
    );

    // A short numeric version stays static too (only ids of >=5 digits are
    // treated as internal parameters that would be wrong after a migration).
    let mut versioned = cover_recipe();
    versioned.emit.name = Some("poster-2024.jpg".to_owned());
    assert_eq!(
        generalize_recipe(&versioned, SAMPLE_URL)
            .recipe
            .emit
            .name
            .as_deref(),
        Some("poster-2024.jpg")
    );

    // A long internal id (cid / sticker hash) cannot come from the input URL:
    // the name is dropped instead of baking the old sample into the template.
    let mut internal = cover_recipe();
    internal.emit.name = Some("sticker_42944954746.webp".to_owned());
    let report = generalize_recipe(&internal, SAMPLE_URL);
    assert_eq!(report.recipe.emit.name, None);
}

#[test]
fn video_recipe_reports_the_hardcoded_cid_but_stays_reusable() {
    let report = generalize_recipe(&video_recipe(), VIDEO_SAMPLE_URL);
    let recipe = &report.recipe;
    recipe.validate().expect("generalized recipe validates");
    let generalization = recipe.generalization.as_ref().expect("generalization");
    assert!(generalization.reusable);
    assert_eq!(
        generalization.leftover_hardcoded,
        vec!["cid".to_owned()],
        "the internal cid cannot be derived from the input URL"
    );
    // Values are abstracted, the internal id is kept (never guessed away).
    assert_eq!(
        recipe.steps[0].args["url"].as_str(),
        Some("https://api.bilibili.com/x/web-interface/view?bvid={{bvid}}")
    );
    assert_eq!(
        recipe.steps[2].args["template"].as_str(),
        Some("https://api.bilibili.com/x/player/playurl?bvid={{bvid}}&cid=42944954746&qn=80")
    );
    assert_eq!(
        recipe.emit.name, None,
        "the video file name embeds the sample BV id: replay must re-derive it"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("cid")),
        "{:?}",
        report.warnings
    );
    // No URL/credential material leaks into the diagnostics.
    assert!(generalization
        .leftover_hardcoded
        .iter()
        .all(|name| !name.contains("http") && !name.contains('=')));
}

#[test]
fn generalized_recipe_matches_a_different_video_of_the_same_class() {
    let recipe = generalize_recipe(&cover_recipe(), SAMPLE_URL).recipe;
    let other =
        Url::parse("https://www.bilibili.com/video/BV1XJn26BEa7/?trackid=999").expect("url");
    let vars = recipe.matches(&other).expect("same-class URL matches");
    assert_eq!(vars.get("bvid").map(String::as_str), Some("BV1XJn26BEa7"));
    // Different hosts and different static prefixes still do not match.
    assert!(recipe
        .matches(&Url::parse("https://evil.example/video/BV1XJn26BEa7/").expect("url"))
        .is_none());
    assert!(recipe
        .matches(&Url::parse("https://www.bilibili.com/anime/BV1XJn26BEa7/").expect("url"))
        .is_none());
}

#[test]
fn numeric_uuid_and_mixed_segments_become_variables() {
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let shape = abstract_url(&format!(
        "https://example.com/u/42/item/{uuid}/abcDEF123?x=1"
    ))
    .expect("abstract");
    let names: Vec<&str> = shape
        .captures
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, vec!["id", "id2", "p1"]);
    assert_eq!(shape.scope, "example.com/u");
    assert!(shape.reusable);
}

#[test]
fn static_url_keeps_the_exact_path_but_ignores_query() {
    let shape =
        abstract_url("https://example.com/downloads/file.bin?token=secret").expect("abstract");
    assert!(!shape.reusable);
    assert!(shape.captures.is_empty());
    assert_eq!(shape.scope, "example.com/downloads/file.bin");
    assert!(shape.pattern.contains("downloads/file\\.bin"));
    assert!(shape.pattern.contains("(?:\\?.*)?"));
    assert!(!shape.pattern.contains("secret"));

    let report = generalize_recipe(&cover_recipe(), "https://example.com/downloads/file.bin");
    let generalization = report
        .recipe
        .generalization
        .as_ref()
        .expect("generalization");
    assert!(!generalization.reusable);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("精确地址")));
}

#[test]
fn invalid_sample_urls_leave_the_recipe_untouched() {
    let recipe = cover_recipe();
    let report = generalize_recipe(&recipe, "not a URL");
    assert!(!report.changed);
    assert!(report.recipe.generalization.is_none());
    assert_eq!(
        report.recipe.matcher.url_pattern,
        recipe.matcher.url_pattern
    );
    assert!(abstract_url("ftp://example.com/file").is_err());
    assert!(abstract_url("https://example.com/").is_ok());
}

#[test]
fn migration_upgrades_an_existing_library_and_is_idempotent() {
    let scratch = ScratchDir::new("generalize-migration");
    let resolve_root = scratch.join("resolve");
    let recipes_root = resolve_root.join("recipes");
    let recipe = Recipe::from_json(
        &json!({
            "schema": "rdm.recipe/v1",
            "id": "legacy-experience",
            "version": 1,
            "name": "legacy",
            "match": {
                "hosts": ["example.com"],
                "url_pattern": "^https://example\\.com/watch/abcDEF123/\\?from=x$",
                "priority": 0
            },
            "steps": [{
                "id": "s0",
                "op": "assert",
                "args": { "kind": "equals", "value": "x", "expected": "x" }
            }],
            "emit": { "url": "https://cdn.example.com/abcDEF123.bin" },
            "tests": [{
                "name": "sample",
                "input_url": "https://example.com/watch/abcDEF123/?from=x"
            }]
        })
        .to_string(),
    )
    .expect("legacy recipe");
    let mut registry = RecipeRegistry::with_root(&recipes_root);
    registry.save(&recipe).expect("save legacy");
    registry.insert(recipe).expect("insert legacy");
    registry.save_index().expect("save index");

    let report = migrate_experience_library(&resolve_root);
    assert_eq!(report.scanned, 1);
    assert_eq!(report.generalized, 1);
    assert_eq!(report.skipped, 0);
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let registry = RecipeRegistry::with_root(&recipes_root);
    assert_eq!(registry.latest_version("legacy-experience"), Some(2));
    assert_eq!(registry.active_version("legacy-experience"), Some(2));
    let migrated = registry
        .get("legacy-experience", Some(2))
        .expect("migrated v2");
    migrated.validate().expect("migrated recipe validates");
    let generalization = migrated.generalization.as_ref().expect("generalization");
    assert!(generalization.reusable);
    assert_eq!(generalization.scope, "example.com/watch");
    assert_eq!(migrated.captures, vec!["p1".to_owned()]);
    let pattern = migrated.matcher.url_pattern.as_deref().expect("pattern");
    assert!(pattern.contains("(?P<p1>"));
    assert!(!pattern.contains("abcDEF123"));

    // v1 is kept on disk for rollback.
    let v1_file = recipes_root.join("legacy-experience@1.json");
    assert!(v1_file.is_file());
    assert!(recipes_root.join("legacy-experience@2.json").is_file());

    // Second run: already generalized, nothing is versioned again.
    let again = migrate_experience_library(&resolve_root);
    assert_eq!(again.scanned, 0);
    assert_eq!(again.generalized, 0);
    assert_eq!(again.skipped, 1);
    let registry = RecipeRegistry::with_root(&recipes_root);
    assert_eq!(registry.latest_version("legacy-experience"), Some(2));
}
