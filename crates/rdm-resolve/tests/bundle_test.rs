//! Experience bundles + index v4 (docs/design/18-experience-v2-reusable.md
//! §2.3/§2.4/§2.5): index migration and metadata APIs, merge/related/list
//! semantics, cascading deletes and bundle replay through `resolve` and
//! `apply_experience` against a local loopback fixture.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::ScratchDir;
use rdm_resolve::{
    list_drafts, save_draft, Budget, BundleRef, ExperienceStore, Recipe, RecipeDraft,
    RecipeRegistry, ResolveErrorKind, ResolvePolicy, ResolveRequest, ResolverService, RunStatus,
    DRAFT_SCHEMA, INDEX_SCHEMA,
};
use serde_json::json;
use url::Url;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const BUNDLE_PATTERN: &str =
    "^https?://(?:bundle\\.example|127\\.0\\.0\\.1:\\d+)/v/[0-9A-Za-z]+/?(?:\\?.*)?$";

fn recipes_root(state: &Path) -> std::path::PathBuf {
    state.join("resolve").join("recipes")
}

fn resolve_root(state: &Path) -> std::path::PathBuf {
    state.join("resolve")
}

fn index_file(state: &Path) -> std::path::PathBuf {
    resolve_root(state).join("index.json")
}

/// Simple local-only recipe used by the index/merge tests.
fn static_recipe(id: &str, path: &str) -> Recipe {
    Recipe::from_json(
        &json!({
            "schema": "rdm.recipe/v1",
            "id": id,
            "version": 1,
            "name": id,
            "match": {
                "hosts": ["bundle.example"],
                "url_pattern": format!("^https?://bundle\\.example/{path}$"),
                "priority": 0
            },
            "steps": [{
                "id": "s0",
                "op": "assert",
                "args": { "kind": "equals", "value": "x", "expected": "x" }
            }],
            "emit": { "url": format!("https://cdn.bundle.example/{id}.bin") },
            "tests": [{ "name": "sample", "input_url": format!("https://bundle.example/{path}") }]
        })
        .to_string(),
    )
    .expect("valid static recipe")
}

/// One local-only experience; two of these share the same matcher scope, which
/// makes them `related` (same-class merge candidates).
fn same_scope_recipe(id: &str, name: &str) -> Recipe {
    Recipe::from_json(
        &json!({
            "schema": "rdm.recipe/v1",
            "id": id,
            "version": 1,
            "name": name,
            "match": {
                "hosts": ["example.com"],
                "url_pattern": "^https?://example\\.com/v/[0-9A-Za-z]+/?$",
                "priority": 0
            },
            "steps": [{
                "id": "s0",
                "op": "assert",
                "args": { "kind": "equals", "value": "x", "expected": "x" }
            }],
            "emit": { "url": format!("https://cdn.example.com/{id}.bin") },
            "tests": [{
                "name": "sample",
                "input_url": "https://example.com/v/abc123"
            }]
        })
        .to_string(),
    )
    .expect("valid same-scope recipe")
}

/// One loopback recipe that reads a JSON endpoint and emits its `/url`.
fn loopback_recipe(
    id: &str,
    name: &str,
    base: &str,
    api_path: &str,
    emitted_name: &str,
    kind: &str,
) -> Recipe {
    Recipe::from_json(
        &json!({
            "schema": "rdm.recipe/v1",
            "id": id,
            "version": 1,
            "name": name,
            "match": {
                "hosts": ["127.0.0.1"],
                "url_pattern": BUNDLE_PATTERN,
                "priority": 0
            },
            "steps": [
                {
                    "id": "s0",
                    "op": "http_request",
                    "args": { "method": "GET", "url": format!("{base}{api_path}"), "max_bytes": 65536 },
                    "expect": { "status_in": [200] },
                    "assign": { "o0": "body_json" }
                },
                {
                    "id": "s1",
                    "op": "json_query",
                    "args": { "from": "o0", "pointer": "/url" },
                    "assign": { "v0": "value" }
                },
                {
                    "id": "s2",
                    "op": "assert",
                    "args": { "kind": "matches", "value": "{{v0}}", "pattern": "^https?://" }
                }
            ],
            "emit": { "url": "{{v0}}", "name": emitted_name, "kind": kind },
            "tests": []
        })
        .to_string(),
    )
    .expect("valid loopback recipe")
}

/// Persist recipes + index metadata (kinds, publish richness) under `state`.
fn seed_recipes(
    state: &Path,
    recipes: &[Recipe],
    kinds: &[(&str, &[&str])],
    best: &[(&str, u32)],
    source_url: Option<&str>,
) {
    let mut registry = RecipeRegistry::with_root(recipes_root(state));
    for recipe in recipes {
        registry.save(recipe).expect("save recipe file");
        registry.insert(recipe.clone()).expect("insert recipe");
    }
    for (id, list) in kinds {
        let list: Vec<String> = list.iter().map(|kind| (*kind).to_owned()).collect();
        registry.set_resource_kinds(id, &list).expect("set kinds");
    }
    for (id, resources) in best {
        let version = registry.active_version(id).unwrap_or(1);
        registry
            .note_publish(id, version, *resources, source_url, 1_000, false)
            .expect("note publish");
    }
    registry.save_index().expect("save index");
}

fn seed_bundle(state: &Path, image: Recipe, video: Recipe) {
    let source = "https://www.bilibili.com/video/BV1e3ny6KE3m/";
    seed_recipes(
        state,
        &[image, video],
        &[("img-exp", &["image"]), ("vid-exp", &["video"])],
        &[("img-exp", 1), ("vid-exp", 1)],
        Some(source),
    );
}

fn merged_store(state: &Path) -> ExperienceStore {
    let store = ExperienceStore::from_state_dir(state);
    store.merge("img-exp", &["vid-exp"]).expect("merge bundle");
    store
}

// ---------------------------------------------------------------------------
// Loopback fixture server (no external network)
// ---------------------------------------------------------------------------

struct Site {
    base_url: String,
    _task: tokio::task::JoinHandle<()>,
}

async fn start_site() -> Site {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fixture site");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let base = base_url.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let base = base.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut chunk = [0u8; 4096];
                while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => raw.extend_from_slice(&chunk[..read]),
                    }
                }
                let head = String::from_utf8_lossy(&raw).into_owned();
                let request_line = head.lines().next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("GET").to_owned();
                let path = parts
                    .next()
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .to_owned();
                let (status, content_type, body) = match path.as_str() {
                    "/api/image" => (
                        200u16,
                        "application/json",
                        format!("{{\"url\":\"{base}/files/cover.png\"}}").into_bytes(),
                    ),
                    "/api/video" => (
                        200u16,
                        "application/json",
                        format!("{{\"url\":\"{base}/files/video.mp4\"}}").into_bytes(),
                    ),
                    "/files/cover.png" => {
                        (200u16, "image/png", vec![0x89u8, b'P', b'N', b'G', 1, 2, 3])
                    }
                    "/files/video.mp4" => (200u16, "video/mp4", vec![7u8; 256]),
                    _ => (404u16, "text/plain", b"not found".to_vec()),
                };
                let mut response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                     Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                if method != "HEAD" {
                    response.extend_from_slice(&body);
                }
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            });
        }
    });
    Site {
        base_url,
        _task: task,
    }
}

// ---------------------------------------------------------------------------
// 1. Index v4: migration + bundle/resource-kind/merged APIs
// ---------------------------------------------------------------------------

#[test]
fn index_v4_migrates_v3_and_persists_bundle_metadata() {
    let scratch = ScratchDir::new("bundle-index-v4");
    let main = static_recipe("main", "main");
    let supplement = static_recipe("sup1", "sup1");
    let other = static_recipe("sup2", "sup2");
    let registry = RecipeRegistry::with_root(recipes_root(scratch.path()));
    for recipe in [&main, &supplement, &other] {
        registry.save(recipe).expect("save recipe");
    }
    std::fs::create_dir_all(resolve_root(scratch.path())).expect("resolve dir");
    std::fs::write(
        index_file(scratch.path()),
        serde_json::to_vec_pretty(&json!({
            "schema": "rdm.recipe-index/v3",
            "updated_ms": 5,
            "entries": {
                "main": { "active_version": 1, "updated_ms": 5 },
                "sup1": { "active_version": 1, "updated_ms": 4 },
                "sup2": { "active_version": 1, "updated_ms": 3 }
            }
        }))
        .expect("index JSON"),
    )
    .expect("write v3 index");

    let mut registry = RecipeRegistry::with_root(recipes_root(scratch.path()));
    assert_eq!(registry.index().schema, INDEX_SCHEMA);
    let entry = registry.entry("main").expect("main entry");
    assert_eq!(
        entry.created_ms, 5,
        "v3 created_ms falls back to updated_ms"
    );
    assert!(entry.resource_kinds.is_empty());
    assert!(entry.bundled.is_empty());
    assert!(entry.merged_into.is_none());

    registry
        .set_resource_kinds(
            "main",
            &["Video".to_owned(), " video ".to_owned(), "image".to_owned()],
        )
        .expect("set kinds");
    let supers = [
        BundleRef {
            id: "sup2".to_owned(),
            version: 1,
        },
        BundleRef {
            id: "sup1".to_owned(),
            version: 1,
        },
        BundleRef {
            id: "sup1".to_owned(),
            version: 1,
        },
        BundleRef {
            id: "missing".to_owned(),
            version: 9,
        },
    ];
    registry.set_bundle("main", &supers).expect("set bundle");
    registry
        .set_merged_into("sup1", Some("main"))
        .expect("mark merged");

    let entry = registry.entry("main").expect("main entry");
    assert_eq!(entry.resource_kinds, vec!["image", "video"]);
    assert_eq!(
        entry.bundled,
        vec![
            BundleRef {
                id: "missing".to_owned(),
                version: 9
            },
            BundleRef {
                id: "sup1".to_owned(),
                version: 1
            },
            BundleRef {
                id: "sup2".to_owned(),
                version: 1
            }
        ],
        "bundles are sorted and de-duplicated; missing members stay tolerated"
    );

    // `bundle_recipes` returns the main flow first and skips missing members.
    let ids: Vec<String> = registry
        .bundle_recipes("main")
        .into_iter()
        .map(|recipe| recipe.id)
        .collect();
    assert_eq!(ids, vec!["main", "sup1", "sup2"]);

    // Merged experiences disappear from the visible list and the matcher.
    assert_eq!(
        registry.visible_ids(),
        vec!["main".to_owned(), "sup2".to_owned()]
    );
    let main_url = Url::parse("https://bundle.example/main").expect("main URL");
    assert_eq!(
        registry.find_for_url(&main_url).expect("hit").recipe.id,
        "main"
    );
    let supplement_url = Url::parse("https://bundle.example/sup1").expect("sup1 URL");
    assert!(
        registry.find_for_url(&supplement_url).is_none(),
        "a merged experience never matches"
    );

    // Everything is persisted as v4 and survives a reload.
    let raw = std::fs::read_to_string(index_file(scratch.path())).expect("index file");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("index JSON");
    assert_eq!(json["schema"], INDEX_SCHEMA);
    assert_eq!(
        json["entries"]["main"]["bundled"].as_array().map(Vec::len),
        Some(3)
    );
    assert_eq!(json["entries"]["sup1"]["merged_into"], "main");
    let reloaded = RecipeRegistry::with_root(recipes_root(scratch.path()));
    assert_eq!(
        reloaded.entry("main").expect("entry").resource_kinds,
        vec!["image", "video"]
    );
    assert_eq!(
        reloaded
            .entry("sup1")
            .expect("entry")
            .merged_into
            .as_deref(),
        Some("main")
    );
}

// ---------------------------------------------------------------------------
// 2. ExperienceStore: merge, related/list and cascading delete
// ---------------------------------------------------------------------------

#[test]
fn merge_bundles_flows_metadata_and_delete_cascades() {
    let scratch = ScratchDir::new("bundle-merge");
    let image = same_scope_recipe("img-exp", "封面图");
    let video = same_scope_recipe("vid-exp", "视频");
    seed_bundle(scratch.path(), image, video);
    // The merge must not rewrite any flow JSON.
    let video_file = recipes_root(scratch.path()).join("vid-exp@1.json");
    let video_before = std::fs::read_to_string(&video_file).expect("video recipe file");

    let store = ExperienceStore::from_state_dir(scratch.path());
    let listed = store.list();
    assert_eq!(listed.len(), 2);
    let image_experience = store.get("img-exp").expect("image experience");
    assert_eq!(image_experience.resource_kinds, vec!["image"]);
    assert_eq!(image_experience.flows.len(), 1);
    assert!(image_experience.flows[0].primary);
    assert_eq!(image_experience.flows[0].resources, 1);
    assert_eq!(
        image_experience.related,
        vec!["vid-exp".to_owned()],
        "same-scope experiences are related"
    );

    let merged = store.merge("img-exp", &["vid-exp"]).expect("merge");
    assert_eq!(merged.id, "img-exp");
    assert_eq!(merged.best_resources, 2, "target + source richness");
    assert_eq!(merged.resource_kinds, vec!["image", "video"]);
    assert_eq!(merged.merged_into, None);
    assert_eq!(merged.flows.len(), 2);
    assert!(merged.flows[0].primary);
    assert_eq!(merged.flows[0].recipe_id, "img-exp");
    assert_eq!(merged.flows[1].recipe_id, "vid-exp");
    assert_eq!(merged.flows[1].name, "视频");
    assert_eq!(merged.flows[1].resources, 1);
    assert!(!merged.flows[1].primary);
    assert!(merged.related.is_empty());

    assert_eq!(
        std::fs::read_to_string(&video_file).expect("video recipe file"),
        video_before,
        "merge only touches index metadata, never flow JSON"
    );
    assert_eq!(store.list().len(), 1, "merged sources are hidden");
    let source = store.get("vid-exp").expect("merged source detail");
    assert_eq!(source.merged_into.as_deref(), Some("img-exp"));
    assert!(source.related.is_empty());

    // Merging a source twice is an explicit error.
    assert!(store.merge("img-exp", &["vid-exp"]).is_err());
    assert!(store.merge("missing", &["img-exp"]).is_err());

    // Drafts of the target and of every cascaded source are removed too.
    for id in ["img-exp", "vid-exp"] {
        let draft = RecipeDraft {
            schema: DRAFT_SCHEMA.to_owned(),
            id: id.to_owned(),
            version: 1,
            created_ms: 1,
            source_run_id: "run_1".to_owned(),
            source_url: "https://bundle.example/v/1".to_owned(),
            scope_hint: "narrow".to_owned(),
            notes: Vec::new(),
            recipe: store.get(id).expect("experience").recipe,
        };
        save_draft(&resolve_root(scratch.path()), &draft).expect("save draft");
    }
    assert_eq!(list_drafts(&resolve_root(scratch.path())).len(), 2);

    assert!(store.delete("img-exp").expect("cascade delete"));
    assert!(store.list().is_empty());
    assert!(store.get("img-exp").is_none());
    assert!(store.get("vid-exp").is_none());
    let recipes = recipes_root(scratch.path());
    for id in ["img-exp", "vid-exp"] {
        assert!(
            !recipes.join(format!("{id}@1.json")).exists(),
            "{id} recipe file must be removed"
        );
        assert!(!recipes.join(format!("{id}@2.json")).exists());
    }
    assert!(list_drafts(&resolve_root(scratch.path())).is_empty());
    let registry = RecipeRegistry::with_root(recipes);
    assert!(registry.index().entries.is_empty());
    assert!(!store.delete("img-exp").expect("idempotent delete"));
}

// ---------------------------------------------------------------------------
// 3. Bundle replay: resolve hit expansion + apply_experience
// ---------------------------------------------------------------------------

fn image_recipe(site: &Site) -> Recipe {
    loopback_recipe(
        "img-exp",
        "封面图",
        &site.base_url,
        "/api/image",
        "cover.png",
        "image",
    )
}

fn video_recipe(site: &Site) -> Recipe {
    loopback_recipe(
        "vid-exp",
        "视频",
        &site.base_url,
        "/api/video",
        "video.mp4",
        "video",
    )
}

fn broken_video_recipe(site: &Site) -> Recipe {
    loopback_recipe(
        "vid-exp",
        "视频",
        &site.base_url,
        "/api/missing",
        "video.mp4",
        "video",
    )
}

#[tokio::test]
async fn resolve_hit_expands_a_merged_bundle_without_ai() {
    let site = start_site().await;
    let scratch = ScratchDir::new("bundle-resolve");
    seed_bundle(scratch.path(), image_recipe(&site), video_recipe(&site));
    merged_store(scratch.path());
    let mut service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());
    let url = format!("{}/v/BV1e3ny6KE3m", site.base_url);

    let outcome = service
        .resolve(ResolveRequest {
            url,
            ..ResolveRequest::default()
        })
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    let manifest = outcome.manifest.expect("bundle manifest");
    assert_eq!(manifest.source.recipe_id.as_deref(), Some("img-exp"));
    assert_eq!(manifest.resources.len(), 2);
    let urls: BTreeSet<&str> = manifest
        .resources
        .iter()
        .map(|resource| resource.url.as_str())
        .collect();
    assert!(urls.iter().any(|url| url.ends_with("/files/cover.png")));
    assert!(urls.iter().any(|url| url.ends_with("/files/video.mp4")));
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.contains("本地解析经验命中：img-exp@v1")),
        "{:?}",
        manifest.notes
    );
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.contains("经验组：主流程 + 1 个补充流程（共 2 个流程）")),
        "{:?}",
        manifest.notes
    );
    // Both flows recorded success (usage/health).
    let store = ExperienceStore::from_state_dir(scratch.path());
    assert_eq!(store.get("img-exp").expect("target").use_count, 1);
    assert_eq!(store.get("vid-exp").expect("source").use_count, 1);
}

#[tokio::test]
async fn apply_experience_replays_main_and_supplementary_flows() {
    let site = start_site().await;
    let scratch = ScratchDir::new("bundle-apply");
    seed_bundle(scratch.path(), image_recipe(&site), video_recipe(&site));
    merged_store(scratch.path());
    let service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());
    // A *different* URL of the same class proves scope reuse.
    let url = format!("{}/v/BV1XJn26BEa7/?trackid=9", site.base_url);

    let outcome = service
        .apply_experience("img-exp", &url, Budget::replay())
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    let manifest = outcome.manifest.expect("manifest");
    assert_eq!(manifest.source.recipe_id.as_deref(), Some("img-exp"));
    assert_eq!(manifest.resources.len(), 2);
    assert!(
        manifest.notes[0].contains("使用解析经验 img-exp@v1"),
        "{:?}",
        manifest.notes
    );
    assert!(manifest
        .notes
        .iter()
        .any(|note| note.contains("经验组：主流程 + 1 个补充流程")));
    assert!(outcome.diagnostics.http_requests >= 2);
    let store = ExperienceStore::from_state_dir(scratch.path());
    assert_eq!(store.get("img-exp").expect("target").use_count, 1);
    assert_eq!(store.get("vid-exp").expect("source").use_count, 1);
}

#[tokio::test]
async fn apply_experience_rejects_a_non_matching_url_without_network() {
    let site = start_site().await;
    let scratch = ScratchDir::new("bundle-apply-mismatch");
    seed_bundle(scratch.path(), image_recipe(&site), video_recipe(&site));
    merged_store(scratch.path());
    let service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());

    let outcome = service
        .apply_experience(
            "img-exp",
            &format!("{}/other/thing", site.base_url),
            Budget::replay(),
        )
        .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeNoMatch)
    );
    assert!(outcome.manifest.is_none());
    assert_eq!(outcome.diagnostics.http_requests, 0);

    let unknown = service
        .apply_experience(
            "missing",
            &format!("{}/v/1", site.base_url),
            Budget::replay(),
        )
        .await;
    assert_eq!(
        unknown.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeNoMatch)
    );
}

#[tokio::test]
async fn apply_experience_keeps_succeeding_when_one_flow_fails() {
    let site = start_site().await;
    let scratch = ScratchDir::new("bundle-apply-partial");
    seed_bundle(
        scratch.path(),
        image_recipe(&site),
        broken_video_recipe(&site),
    );
    merged_store(scratch.path());
    let service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());

    let outcome = service
        .apply_experience(
            "img-exp",
            &format!("{}/v/BV1e3ny6KE3m", site.base_url),
            Budget::replay(),
        )
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    let manifest = outcome.manifest.expect("manifest");
    assert_eq!(manifest.resources.len(), 1);
    assert!(manifest.resources[0].url.ends_with("/files/cover.png"));
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.contains("1 个流程回放失败，已跳过")),
        "{:?}",
        manifest.notes
    );
    // The failed supplementary flow still records a failure (usage/health).
    let store = ExperienceStore::from_state_dir(scratch.path());
    let failed = store.get("vid-exp").expect("source");
    assert_eq!(failed.failure_count, 1);
    assert_eq!(failed.success_count, 0);
}

#[tokio::test]
async fn bundle_replay_fails_only_when_every_flow_fails() {
    let site = start_site().await;
    let scratch = ScratchDir::new("bundle-all-failed");
    seed_bundle(
        scratch.path(),
        loopback_recipe(
            "img-exp",
            "封面图",
            &site.base_url,
            "/api/missing",
            "cover.png",
            "image",
        ),
        broken_video_recipe(&site),
    );
    merged_store(scratch.path());
    // No AI provider is configured: an all-flow failure never falls back.
    let mut service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());

    let outcome = service
        .resolve(ResolveRequest {
            url: format!("{}/v/BV1e3ny6KE3m", site.base_url),
            ..ResolveRequest::default()
        })
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeStale)
    );
    assert!(outcome.manifest.is_none());
    let store = ExperienceStore::from_state_dir(scratch.path());
    assert_eq!(store.get("img-exp").expect("target").failure_count, 1);
    assert_eq!(store.get("vid-exp").expect("source").failure_count, 1);
}

#[tokio::test]
async fn bundle_replay_dedupes_resources_by_url() {
    let site = start_site().await;
    let scratch = ScratchDir::new("bundle-dedupe");
    // Both flows emit the very same file URL.
    seed_bundle(
        scratch.path(),
        image_recipe(&site),
        loopback_recipe(
            "vid-exp",
            "视频",
            &site.base_url,
            "/api/image",
            "video.mp4",
            "video",
        ),
    );
    merged_store(scratch.path());
    let service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());

    let outcome = service
        .apply_experience(
            "img-exp",
            &format!("{}/v/BV1e3ny6KE3m", site.base_url),
            Budget::replay(),
        )
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    let manifest = outcome.manifest.expect("manifest");
    assert_eq!(
        manifest.resources.len(),
        1,
        "duplicate emitted URLs are merged: {:?}",
        manifest
            .resources
            .iter()
            .map(|resource| resource.url.as_str())
            .collect::<Vec<_>>()
    );
}
