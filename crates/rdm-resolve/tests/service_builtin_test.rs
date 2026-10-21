//! `ResolveMode::Auto` builtin fallback: when no recipe matches, a direct file
//! URL is verified in-place instead of failing with `recipe_no_match`
//! (acceptance `docs/agent-integration.md` §8.2).

mod common;
mod support;

use rdm_resolve::contract::{ResolveErrorKind, RunStatus};
use rdm_resolve::{RecipeRegistry, ResolvePolicy, ResolverService, VerificationStatus};

use common::resolve_request;
use support::{MockServer, Reply};

fn empty_library_service() -> ResolverService {
    ResolverService::new(RecipeRegistry::empty(), ResolvePolicy::loopback())
}

#[tokio::test]
async fn auto_falls_back_to_builtin_direct_file_verification() {
    let server = MockServer::start(|request| match request.method.as_str() {
        "HEAD" => Reply::file_head(4096).header("accept-ranges", "bytes"),
        _ => Reply::file(4096).header("accept-ranges", "bytes"),
    })
    .await;
    let input_url = server.url("/downloads/setup.exe");

    let outcome = empty_library_service()
        .resolve(resolve_request(&input_url))
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        outcome.recipe.is_none(),
        "the builtin fallback is not a recipe"
    );
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert!(outcome.diagnostics.error.is_none());
    assert_eq!(outcome.diagnostics.steps, 0);
    assert!(
        outcome
            .diagnostics
            .summary
            .contains("builtin direct-file verification"),
        "summary must state the fallback path: {}",
        outcome.diagnostics.summary
    );

    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.schema_version, rdm_resolve::SCHEMA_VERSION);
    assert_eq!(manifest.source.input_url, input_url);
    assert!(manifest.source.recipe_id.is_none());
    assert_eq!(manifest.resources.len(), 1);
    assert_eq!(manifest.downloadable().count(), 1);

    let item = &manifest.resources[0];
    assert_eq!(item.url, input_url);
    assert_eq!(item.name.as_deref(), Some("setup.exe"));
    assert_eq!(item.verification.status, VerificationStatus::Verified);
    assert!(item.verification.status.is_downloadable());
    assert_eq!(
        item.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(item.size_bytes, Some(4096));
    assert_eq!(item.supports_ranges, Some(true));
    assert!(item.transfer.resumable);
    assert!(!item.evidence.is_empty());

    // HEAD probe + one bounded GET probe; no recipe steps.
    assert_eq!(server.count_method("HEAD"), 1);
    assert_eq!(server.count_method("GET"), 1);
}

#[tokio::test]
async fn auto_builtin_html_page_fails_as_not_a_file() {
    let server = MockServer::start(|_| Reply::html("<!doctype html><html>landing</html>")).await;

    let outcome = empty_library_service()
        .resolve(resolve_request(&server.url("/landing")))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::NotAFile)
    );
    assert!(outcome.manifest.is_none());
    assert!(outcome.recipe.is_none());
}

#[tokio::test]
async fn auto_builtin_maps_http_statuses_to_typed_errors() {
    let cases = [
        (401, ResolveErrorKind::AuthRequired),
        (403, ResolveErrorKind::Forbidden),
        (404, ResolveErrorKind::NotFound),
        (429, ResolveErrorKind::RateLimited),
        (503, ResolveErrorKind::ServerError),
    ];

    for (status, expected) in cases {
        let server = MockServer::start(move |_| Reply::status(status)).await;
        let outcome = empty_library_service()
            .resolve(resolve_request(&server.url("/download.bin")))
            .await;
        assert_eq!(outcome.status, RunStatus::Failed, "HTTP {status}");
        assert_eq!(
            outcome.diagnostics.error.as_ref().map(|error| error.kind),
            Some(expected),
            "HTTP {status}"
        );
        assert!(outcome.manifest.is_none(), "HTTP {status}");
    }
}

#[tokio::test]
async fn auto_with_explicit_missing_recipe_does_not_use_the_builtin_fallback() {
    // An explicitly requested recipe is a promise: never silently ignore it.
    let mut request = resolve_request("http://127.0.0.1:9/downloads/file.bin");
    request.recipe_id = Some("does-not-exist".to_owned());

    let outcome = empty_library_service().resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeNoMatch)
    );
    assert!(outcome.manifest.is_none());
}
