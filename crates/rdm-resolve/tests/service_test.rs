//! Recipe / registry / service end-to-end behavior on a local mock site.
//!
//! Covers the acceptance matrix in `docs/agent-integration.md` §8.2/§8.3:
//! deterministic replay (no model call), structural drift, forced `recipe_id`,
//! model-unavailable exploration and the manifest hand-off.

mod common;
mod support;

use rdm_resolve::contract::{ResolveErrorKind, ResolveMode, RunStatus};
use rdm_resolve::{
    Budget, RecipeRegistry, ResolvePolicy, ResolvedCredentials, ResolverService,
    VerificationMethod, VerificationStatus,
};
use serde_json::json;

use common::{recipe_json, resolve_request, test_budget, RecipeFixture, ScratchDir};
use support::{MockServer, Reply};

/// Mock site: `/api/<id>` returns JSON whose file URL is on the same origin;
/// everything else is served as a downloadable binary.
async fn mock_site() -> MockServer {
    MockServer::start(|request| {
        if request.path.starts_with("/api/") {
            let id = request.path.rsplit('/').next().unwrap_or("0");
            let host = request.header("host").unwrap_or("127.0.0.1");
            let body = json!({
                "file": { "url": format!("http://{host}/files/{id}.bin") }
            })
            .to_string();
            Reply::json(&body)
        } else {
            match request.method.as_str() {
                "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
                _ => Reply::file(2048).header("accept-ranges", "bytes"),
            }
        }
    })
    .await
}

fn site_recipe() -> String {
    recipe_json(
        "mock-site-file",
        1,
        json!({
            "hosts": ["127.0.0.1"],
            "url_pattern": "^(?P<base>http://127\\.0\\.0\\.1:\\d+)/page/(?P<id>\\d+)$",
            "priority": 10
        }),
        json!([
            {
                "id": "meta",
                "op": "http_request",
                "args": { "method": "GET", "url": "{{base}}/api/{{id}}" },
                "expect": {
                    "status_in": [200],
                    "content_type_prefix": ["application/json"]
                },
                "assign": { "meta": "body_json" }
            },
            {
                "id": "download-url",
                "op": "json_query",
                "args": { "from": "meta", "pointer": "/file/url" },
                "assign": { "download_url": "value" }
            }
        ]),
        json!({ "url": "{{download_url}}", "name": "{{id}}.bin" }),
    )
}

#[tokio::test]
async fn recipe_match_resolves_a_verified_manifest_without_llm_calls() {
    let server = mock_site().await;
    let fixture = RecipeFixture::new("recipe-e2e");
    fixture.save(&site_recipe());
    let mut service = fixture.service();
    let input_url = server.url("/page/42");
    let expected_file = format!("{}/files/42.bin", server.base_url());
    let mut request = resolve_request(&input_url);
    request.budget = Some(Budget {
        http_requests: 8,
        ..test_budget()
    });

    let outcome = service.resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.diagnostics.llm_calls, 0,
        "replay must not call a model"
    );
    assert!(outcome.diagnostics.error.is_none());
    assert_eq!(outcome.diagnostics.steps, 2);
    assert_eq!(outcome.diagnostics.http_requests, 3);

    let recipe = outcome.recipe.as_ref().expect("recipe reference");
    assert_eq!(recipe.id, "mock-site-file");
    assert_eq!(recipe.version, 1);
    assert_eq!(recipe.state, "matched");

    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.schema_version, rdm_resolve::SCHEMA_VERSION);
    assert!(!manifest.manifest_id.is_empty());
    assert_eq!(manifest.source.input_url, input_url);
    assert_eq!(manifest.source.recipe_id.as_deref(), Some("mock-site-file"));
    assert_eq!(manifest.resources.len(), 1);

    let item = &manifest.resources[0];
    assert_eq!(item.url, expected_file);
    assert_eq!(item.name.as_deref(), Some("42.bin"));
    assert_eq!(item.verification.status, VerificationStatus::Verified);
    assert!(item.verification.status.is_downloadable());
    assert_eq!(
        item.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(item.size_bytes, Some(2048));
    assert!(!item.evidence.is_empty());
    assert_eq!(manifest.downloadable().count(), 1);

    // The replay itself is a deterministic 3-request pipeline:
    // JSON metadata (GET), file HEAD probe, bounded file GET probe.
    assert_eq!(server.count_method("HEAD"), 1);
    assert_eq!(server.count_method("GET"), 2);

    // And the manifest survives the wire contract.
    let encoded = serde_json::to_value(manifest).expect("manifest serializes");
    assert_eq!(encoded["schema_version"], "rdm.resolve/v1");
    assert_eq!(
        encoded["resources"][0]["verification"]["status"],
        "verified"
    );
}

#[tokio::test]
async fn recipe_only_without_match_reports_recipe_no_match() {
    let mut service = ResolverService::new(RecipeRegistry::empty(), ResolvePolicy::loopback());
    let mut request = resolve_request("http://127.0.0.1:9/unknown");
    request.mode = ResolveMode::RecipeOnly;

    let outcome = service.resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeNoMatch)
    );
    assert!(outcome.manifest.is_none());
    assert_eq!(outcome.diagnostics.llm_calls, 0);
}

#[tokio::test]
async fn explore_mode_without_ai_reports_model_unavailable() {
    // v0.4: exploration is no longer `explore_unavailable`; with no configured
    // provider an HTML page fails as `model_unavailable` *after* the builtin
    // direct-link verification.
    let server = MockServer::start(|_| Reply::html("<!doctype html><html>landing</html>")).await;
    let mut service = ResolverService::new(RecipeRegistry::empty(), ResolvePolicy::loopback());
    let mut request = resolve_request(&server.url("/landing"));
    request.mode = ResolveMode::Explore;

    let outcome = service.resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::ModelUnavailable)
    );
    assert!(outcome.manifest.is_none());
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert!(
        outcome.diagnostics.http_requests >= 1,
        "explore still performs the builtin direct-link verification first"
    );
    assert!(outcome.run_id.starts_with("run_"));
}

#[tokio::test]
async fn structural_drift_fails_with_recipe_stale() {
    let server =
        MockServer::start(|_| Reply::html("<!doctype html><html>maintenance</html>")).await;
    let fixture = RecipeFixture::new("recipe-drift");
    fixture.save(&recipe_json(
        "drift-recipe",
        1,
        json!({ "hosts": ["127.0.0.1"] }),
        json!([{
            "id": "api",
            "op": "http_request",
            "args": { "url": server.url("/api/item") },
            "expect": { "content_type_prefix": ["application/json"] },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": server.url("/files/item.bin") }),
    ));
    let mut service = fixture.service();

    let outcome = service
        .resolve(resolve_request(&server.url("/page/1")))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeStale),
        "a shape change must fail loudly instead of degrading silently"
    );
    assert!(outcome.manifest.is_none());
    assert_eq!(outcome.diagnostics.llm_calls, 0);
}

#[tokio::test]
async fn explicit_recipe_id_overrides_priority() {
    let server = MockServer::start(|request| match request.method.as_str() {
        "HEAD" => Reply::file_head(1024),
        _ => Reply::file(1024),
    })
    .await;
    let matcher = json!({
        "hosts": ["127.0.0.1"],
        "url_pattern": "^(?P<base>http://127\\.0\\.0\\.1:\\d+)/page/(?P<id>\\d+)$",
        "priority": 10
    });
    let steps = json!([{
        "id": "check",
        "op": "assert",
        "args": { "kind": "equals", "value": "ok", "expected": "ok" }
    }]);

    let fixture = RecipeFixture::new("recipe-forced");
    fixture.save(&recipe_json(
        "high-priority",
        1,
        matcher.clone(),
        steps.clone(),
        json!({ "url": "{{base}}/files/{{id}}-high.bin" }),
    ));
    fixture.save(&recipe_json(
        "low-priority",
        1,
        json!({
            "hosts": ["127.0.0.1"],
            "url_pattern": "^(?P<base>http://127\\.0\\.0\\.1:\\d+)/page/(?P<id>\\d+)$",
            "priority": 0
        }),
        steps,
        json!({ "url": "{{base}}/files/{{id}}-low.bin" }),
    ));
    let mut service = fixture.service();
    let input_url = server.url("/page/7");

    let automatic = service.resolve(resolve_request(&input_url)).await;
    assert_eq!(automatic.status, RunStatus::Succeeded);
    assert_eq!(automatic.recipe.as_ref().unwrap().id, "high-priority");
    assert!(
        automatic.manifest.as_ref().unwrap().resources[0]
            .url
            .ends_with("/files/7-high.bin"),
        "priority must win the automatic selection"
    );

    let mut forced = resolve_request(&input_url);
    forced.recipe_id = Some("low-priority".to_owned());
    let overridden = service.resolve(forced).await;
    assert_eq!(overridden.status, RunStatus::Succeeded);
    assert_eq!(overridden.recipe.as_ref().unwrap().id, "low-priority");
    assert!(
        overridden.manifest.as_ref().unwrap().resources[0]
            .url
            .ends_with("/files/7-low.bin"),
        "an explicit recipe_id must override priority ordering"
    );
}

#[tokio::test]
async fn state_dir_layout_is_resolve_recipes() {
    // `ResolverService::with_state_dir(root)` must read
    // `<root>/resolve/recipes/*.json`, the layout the CLI populates.
    let scratch = ScratchDir::new("state-dir-layout");
    let fixture = RecipeFixture::new("state-dir-layout-fixture");
    fixture.save(&site_recipe());
    let recipes_dir = fixture.recipes_dir();
    assert_eq!(
        recipes_dir,
        fixture.state_dir().join("resolve").join("recipes")
    );
    assert!(
        recipes_dir.join("mock-site-file@1.json").is_file(),
        "saved recipe must follow <id>@<version>.json"
    );
    let empty_service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());
    assert_eq!(
        empty_service.registry().len(),
        0,
        "scratch state dir is empty"
    );
    let seeded = fixture.service();
    assert_eq!(seeded.registry().len(), 1);
}

#[tokio::test]
async fn explore_mode_does_not_replay_matching_recipes() {
    // Exploration never replays a matching recipe: only the builtin
    // verification and the model-driven loop run. With no provider configured
    // the run ends as `model_unavailable` after the page itself was fetched.
    let server = MockServer::start(|_| Reply::html("<!doctype html><html>share page</html>")).await;
    let fixture = RecipeFixture::new("explore-with-recipe");
    fixture.save(&site_recipe());
    let mut request = resolve_request(&server.url("/page/42"));
    request.mode = ResolveMode::Explore;

    let outcome = fixture.service().resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::ModelUnavailable)
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| !request.path.starts_with("/api/")),
        "explore must not replay the matching recipe's API step"
    );
}

#[tokio::test]
async fn one_time_recipe_emits_a_head_only_single_stream_resource() {
    let server = MockServer::start(|request| match request.method.as_str() {
        "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
        _ => Reply::file(2048),
    })
    .await;
    let fixture = RecipeFixture::new("recipe-one-time");
    fixture.save(&recipe_json(
        "one-time-recipe",
        1,
        json!({ "hosts": ["127.0.0.1"] }),
        json!([{
            "id": "check",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }]),
        json!({
            "url": server.url("/signed/file.bin"),
            "name": "file.bin",
            "one_time": true,
            "single_stream": true
        }),
    ));

    let outcome = fixture
        .service()
        .resolve(resolve_request(&server.url("/page/1")))
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    let item = &outcome.manifest.as_ref().expect("manifest").resources[0];
    assert_eq!(
        item.verification.status,
        VerificationStatus::HeadOnlyVerified
    );
    assert_eq!(item.verification.method, VerificationMethod::HeadOnly);
    assert!(item.transfer.one_time);
    assert!(item.transfer.single_stream);
    assert!(!item.transfer.resumable);
    assert_eq!(server.count_method("HEAD"), 1);
    assert_eq!(
        server.count_method("GET"),
        0,
        "one-time URLs must never be probed with a GET"
    );

    let request = item.to_download_request(None, &ResolvedCredentials::default());
    assert_eq!(request.connections, 1);
    assert!(request.single_stream);
    assert!(!request.dynamic_connections);
    assert!(!request.resume);
}

#[tokio::test]
async fn recipe_only_runs_a_matching_recipe() {
    let server = mock_site().await;
    let fixture = RecipeFixture::new("recipe-only-match");
    fixture.save(&site_recipe());
    let mut request = resolve_request(&server.url("/page/42"));
    request.mode = ResolveMode::RecipeOnly;

    let outcome = fixture.service().resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.recipe.as_ref().unwrap().id, "mock-site-file");
    assert_eq!(outcome.diagnostics.llm_calls, 0);
}

#[tokio::test]
async fn replay_is_deterministic_across_runs() {
    let server = mock_site().await;
    let fixture = RecipeFixture::new("recipe-deterministic");
    fixture.save(&site_recipe());
    let mut service = fixture.service();
    let input_url = server.url("/page/42");

    let first = service.resolve(resolve_request(&input_url)).await;
    let second = service.resolve(resolve_request(&input_url)).await;

    assert_eq!(first.status, RunStatus::Succeeded);
    assert_eq!(second.status, RunStatus::Succeeded);
    for outcome in [&first, &second] {
        assert_ne!(outcome.run_id, "");
        assert_eq!(outcome.diagnostics.steps, 2);
        assert_eq!(outcome.diagnostics.http_requests, 3);
        assert_eq!(outcome.diagnostics.llm_calls, 0);
        assert!(outcome.diagnostics.bytes_read > 0);
    }
    let first_item = &first.manifest.as_ref().unwrap().resources[0];
    let second_item = &second.manifest.as_ref().unwrap().resources[0];
    assert_eq!(first_item.url, second_item.url);
    assert_eq!(first_item.name, second_item.name);
    assert_eq!(
        first_item.verification.status,
        second_item.verification.status
    );
    assert_eq!(first.manifest.as_ref().unwrap().source.input_url, input_url);
    assert_ne!(
        first.manifest.as_ref().unwrap().manifest_id,
        second.manifest.as_ref().unwrap().manifest_id,
        "each run gets a fresh manifest id"
    );
}
