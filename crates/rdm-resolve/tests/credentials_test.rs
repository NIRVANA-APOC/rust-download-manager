//! P4a credential bridge (`docs/architecture.md` §11.12): one-shot host-scoped
//! `HostCredentials` (cookie / extra headers) are injected into verification and
//! recipe replay, are *not* forwarded across hosts or origins, and are removed
//! before the JsonAction exploration loop. Nothing credential-shaped may ever
//! reach a trace, the outcome JSON, diagnostics or manifest notes.
//!
//! Fixtures are local MockServers that answer `401` unless the request carries
//! the expected cookie; captured request headers prove where the cookie was and
//! was not sent. No public network, no real model.

mod common;
mod support;

use std::path::PathBuf;

use common::llm::{LlmReply, MockLlm};
use common::{recipe_json, RecipeFixture};
use rdm_resolve::{
    AiConfig, Budget, HostCredentials, RecipeRegistry, ResolveErrorKind, ResolveMode,
    ResolvePolicy, ResolveRequest, ResolverService, RunStatus, TraceStore, VerificationStatus,
};
use serde_json::json;
use support::{MockServer, Reply};

/// Cookie value handed to `HostCredentials`. Must never appear in any output.
const SECRET_COOKIE: &str = "session=SUPERSECRET-abc123";
/// Extra header value injected next to the cookie (also must stay secret).
const SECRET_BEARER: &str = "Bearer SUPERSECRET-bearer-xyz";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn loopback_policy() -> ResolvePolicy {
    ResolvePolicy::loopback()
}

fn budget() -> Budget {
    Budget {
        wall_ms: 20_000,
        ..Budget::default()
    }
}

fn request(url: &str, mode: ResolveMode) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        mode,
        budget: Some(budget()),
        ..ResolveRequest::default()
    }
}

fn state_root(fixture: &RecipeFixture) -> PathBuf {
    fixture.state_dir().join("resolve")
}

fn runs_dir(fixture: &RecipeFixture) -> PathBuf {
    state_root(fixture).join("runs")
}

fn load_trace(fixture: &RecipeFixture, run_id: &str) -> rdm_resolve::RunTrace {
    TraceStore::load(&state_root(fixture), run_id).expect("trace must load")
}

/// Credentials for `host` carrying the required cookie.
fn host_credentials(host: &str) -> HostCredentials {
    HostCredentials::new().with_cookie(host, SECRET_COOKIE)
}

fn error_kind(outcome: &rdm_resolve::ResolveOutcome) -> Option<ResolveErrorKind> {
    outcome.diagnostics.error.as_ref().map(|error| error.kind)
}

/// True when the captured request carries the expected cookie.
fn saw_cookie(request: &support::RequestInfo) -> bool {
    request
        .header("cookie")
        .is_some_and(|value| value.contains(SECRET_COOKIE))
}

fn cookie_count(site: &MockServer) -> usize {
    site.requests()
        .iter()
        .filter(|request| saw_cookie(request))
        .count()
}

// ---------------------------------------------------------------------------
// Cookie-guarded fixture site
// ---------------------------------------------------------------------------

/// Handler for a site that requires `SECRET_COOKIE` on every request.
///
/// Routes:
/// * `/page/42` — HTML share page linking `/files/42.bin`;
/// * `/api/42` — JSON `{"file":{"url":"<host>/files/42.bin"}}`;
/// * `/api/drift` — HTML maintenance page (drifts recipes expecting JSON);
/// * `/files/42.bin` and `/file.bin` — 2048-byte binary;
/// * `/redirect/same` — same-origin 302 to `/file.bin`;
/// * anything else — 404.
fn guarded_handler() -> impl Fn(&support::RequestInfo) -> Reply + Send + Sync + 'static {
    move |request| {
        if !saw_cookie(request) {
            return Reply::status(401);
        }
        let host = request.header("host").unwrap_or("127.0.0.1").to_owned();
        match request.path.as_str() {
            "/page/42" => Reply::html(&format!(
                "<!doctype html><html><body>\
                 <a href=\"http://{host}/files/42.bin\">Download</a>\
                 </body></html>"
            )),
            "/api/42" => Reply::json(&format!(
                r#"{{"file":{{"url":"http://{host}/files/42.bin"}}}}"#
            )),
            "/api/drift" => Reply::html("<!doctype html><html>maintenance</html>"),
            "/files/42.bin" | "/file.bin" => match request.method.as_str() {
                "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
                _ => Reply::file(2048).header("accept-ranges", "bytes"),
            },
            "/redirect/same" => Reply::redirect("/file.bin"),
            _ => Reply::status(404),
        }
    }
}

async fn guarded_site() -> MockServer {
    MockServer::start(guarded_handler()).await
}

/// Unconditional 302 to `target` (used for the cross-origin redirect test).
async fn redirecting_site(target: String) -> MockServer {
    MockServer::start(move |_| Reply::redirect(&target)).await
}

/// A recipe that replays through the cookie-guarded JSON API:
/// GET /api/42 → json_query `/file/url` → assert → emit `{{download}}`.
fn guarded_api_recipe(site: &MockServer) -> String {
    recipe_json(
        "guarded-api",
        1,
        json!({ "hosts": ["127.0.0.1"], "priority": 5 }),
        json!([
            {
                "id": "api",
                "op": "http_request",
                "args": { "url": site.url("/api/42") },
                "assign": { "api": "body_json" }
            },
            {
                "id": "url",
                "op": "json_query",
                "args": { "from": "api", "pointer": "/file/url" },
                "assign": { "download": "value" }
            },
            {
                "id": "check",
                "op": "assert",
                "args": { "kind": "matches", "value": "{{download}}", "pattern": "^https?://" }
            }
        ]),
        json!({ "url": "{{download}}" }),
    )
}

/// A drifting recipe (expects JSON, site serves HTML) that matches any URL on
/// the host; used for the auto-repair fallback.
fn guarded_drift_recipe(site: &MockServer) -> String {
    recipe_json(
        "guarded-drift",
        1,
        json!({ "hosts": ["127.0.0.1"], "priority": 5 }),
        json!([{
            "id": "api",
            "op": "http_request",
            "args": { "url": site.url("/api/drift") },
            "expect": { "content_type_prefix": ["application/json"] },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": site.url("/never.bin") }),
    )
}

// ---------------------------------------------------------------------------
// Scripted-exploration helpers
// ---------------------------------------------------------------------------

fn scrub_ambient_env_key() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::remove_var("RDM_AI_API_KEY"));
}

fn ai_config(llm: &MockLlm) -> AiConfig {
    scrub_ambient_env_key();
    AiConfig {
        enabled: true,
        base_url: llm.config_base_url(),
        model: "scripted-model".to_owned(),
        api_key: "sk-p4a-scripted-key".to_owned(),
        ..AiConfig::default()
    }
}

fn page_read(url: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "page_read", "url": url }).to_string())
}

fn finish(reason: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "finish", "reason": reason }).to_string())
}

// ---------------------------------------------------------------------------
// 1. No credentials → the guarded direct file is a login-required failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn direct_file_without_credentials_is_login_required() {
    let site = guarded_site().await;
    let file_url = site.url("/file.bin");
    let mut service = ResolverService::new(RecipeRegistry::empty(), loopback_policy());

    let outcome = service.resolve(request(&file_url, ResolveMode::Auto)).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(error_kind(&outcome), Some(ResolveErrorKind::AuthRequired));
    assert!(outcome.manifest.is_none());
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert!(
        site.requests().iter().all(|request| !saw_cookie(request)),
        "no cookie may be sent without credentials"
    );
    assert!(!serde_json::to_string(&outcome)
        .expect("serialize outcome")
        .contains(SECRET_COOKIE));
}

// ---------------------------------------------------------------------------
// 2. Host credentials make the guarded direct file verifiable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn host_credentials_make_a_direct_file_verifiable() {
    let site = guarded_site().await;
    let file_url = site.url("/file.bin");
    let mut service = ResolverService::new(RecipeRegistry::empty(), loopback_policy())
        .with_credentials(host_credentials("127.0.0.1"));

    let outcome = service.resolve(request(&file_url, ResolveMode::Auto)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.http_requests, 2);
    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
    let item = &manifest.resources[0];
    assert_eq!(item.url, file_url);
    assert_eq!(item.size_bytes, Some(2048));
    assert_eq!(item.verification.status, VerificationStatus::Verified);
    assert!(item.verification.status.is_downloadable());
    assert_eq!(
        site.requests().len(),
        2,
        "HEAD + bounded GET: {:?}",
        site.requests()
    );
    assert_eq!(
        cookie_count(&site),
        2,
        "both verification probes must carry the cookie"
    );
}

// ---------------------------------------------------------------------------
// 3. Credentials are scoped to their host (and the host key is normalised)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn credentials_are_scoped_to_their_host() {
    let site = guarded_site().await;
    let file_url = site.url("/file.bin");

    // Credentials for a different host are never injected.
    let mut other_host = ResolverService::new(RecipeRegistry::empty(), loopback_policy())
        .with_credentials(host_credentials("example.com"));
    let outcome = other_host
        .resolve(request(&file_url, ResolveMode::Auto))
        .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(error_kind(&outcome), Some(ResolveErrorKind::AuthRequired));
    assert_eq!(
        cookie_count(&site),
        0,
        "credentials for another host must not be sent"
    );

    // Trailing-dot host normalisation: the same credentials still apply.
    let site = guarded_site().await;
    let file_url = site.url("/file.bin");
    let mut normalised = ResolverService::new(RecipeRegistry::empty(), loopback_policy())
        .with_credentials(host_credentials("127.0.0.1."));
    let outcome = normalised
        .resolve(request(&file_url, ResolveMode::Auto))
        .await;
    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "the host key must be normalised (lowercase, trailing dot trimmed)"
    );
}

// ---------------------------------------------------------------------------
// 4. Debug output never contains credential values
// ---------------------------------------------------------------------------

#[tokio::test]
async fn host_credentials_debug_does_not_leak_values() {
    let credentials =
        host_credentials("127.0.0.1").with_header("127.0.0.1", "authorization", SECRET_BEARER);
    let debug = format!("{credentials:?}");
    assert!(
        !debug.contains("SUPERSECRET"),
        "Debug leaked a value: {debug}"
    );
    assert!(!debug.contains("abc123"), "Debug leaked a value: {debug}");
    let empty = format!("{:?}", HostCredentials::default());
    assert!(
        !empty.to_ascii_lowercase().contains("cookie"),
        "empty Debug must not describe header content: {empty}"
    );
}

// ---------------------------------------------------------------------------
// 5. Recipe replay uses the credentials (and fails without them)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recipe_replay_sends_credentials() {
    // Without credentials the API step gets a 401 (not JSON) → structural fail.
    let site = guarded_site().await;
    let fixture = RecipeFixture::new("p4a-recipe-no-creds");
    fixture.save(&guarded_api_recipe(&site));
    let mut no_creds = fixture.service();
    let outcome = no_creds
        .resolve(request(&site.url("/page/42"), ResolveMode::RecipeOnly))
        .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        error_kind(&outcome),
        Some(ResolveErrorKind::RecipeStale),
        "a 401 breaks the recipe's JSON expectation"
    );
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(cookie_count(&site), 0);

    // With credentials both the API step and the file verification carry it.
    let site = guarded_site().await;
    let fixture = RecipeFixture::new("p4a-recipe-creds");
    fixture.save(&guarded_api_recipe(&site));
    let mut service = fixture
        .service()
        .with_credentials(host_credentials("127.0.0.1"));
    let outcome = service
        .resolve(request(&site.url("/page/42"), ResolveMode::RecipeOnly))
        .await;
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(
        outcome.recipe.as_ref().map(|recipe| recipe.state.as_str()),
        Some("matched")
    );
    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
    assert!(
        cookie_count(&site) >= 2,
        "API step + verification must send the cookie: {:?}",
        site.requests()
    );
    assert!(
        site.requests()
            .iter()
            .all(|request| !saw_cookie(request) || request.header("cookie") == Some(SECRET_COOKIE)),
        "only the configured cookie value is sent"
    );
}

// ---------------------------------------------------------------------------
// 6. Auto-repair fallback: builtin verification keeps the credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_repair_builtin_fallback_uses_credentials() {
    // With credentials: the drifting recipe fails, the builtin verification of
    // the direct file succeeds with the cookie, so no model is needed.
    let site = guarded_site().await;
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p4a-auto-repair-creds");
    fixture.save(&guarded_drift_recipe(&site));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture
        .service()
        .with_ai_config(ai_config(&llm))
        .with_credentials(host_credentials("127.0.0.1"));

    let outcome = service.resolve(request(&file_url, ResolveMode::Auto)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(llm.request_count(), 0);
    assert!(outcome.recipe.is_none());
    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
    assert_eq!(manifest.resources[0].size_bytes, Some(2048));
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.to_ascii_lowercase().contains("auto-repair")),
        "notes: {:?}",
        manifest.notes
    );
    let trace = load_trace(&fixture, &outcome.run_id);
    let ops: Vec<&str> = trace.steps.iter().map(|step| step.op.as_str()).collect();
    assert_eq!(
        ops,
        vec!["recipe_replay", "builtin_verify"],
        "one merged trace: replay step then the credential-backed verification"
    );
    assert_eq!(trace.steps[0].outcome, "error:recipe_stale");
    assert_eq!(trace.steps[0].note.as_deref(), Some("api"));
    assert_eq!(trace.steps[1].outcome, "ok");
    assert_eq!(
        service
            .registry()
            .health("guarded-drift")
            .consecutive_failures,
        1
    );

    // Without credentials the fallback verification is 401 → login required;
    // a login failure is not page-like, so exploration is not attempted.
    let site = guarded_site().await;
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p4a-auto-repair-no-creds");
    fixture.save(&guarded_drift_recipe(&site));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));
    let outcome = service.resolve(request(&file_url, ResolveMode::Auto)).await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(error_kind(&outcome), Some(ResolveErrorKind::AuthRequired));
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(llm.request_count(), 0);
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.steps[0].op, "recipe_replay");
    assert_eq!(trace.steps.len(), 2);
    assert_eq!(trace.steps[1].op, "builtin_verify");
    assert!(
        trace.steps[1].outcome.starts_with("error:"),
        "the fallback verification failed: {}",
        trace.steps[1].outcome
    );
}

// ---------------------------------------------------------------------------
// 7. Exploration clears credentials before the JsonAction loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exploration_clears_credentials() {
    let site = guarded_site().await;
    let page_url = site.url("/page/42");
    // First exploration turn reads the page — it must 401 without the cookie.
    let llm = MockLlm::start(vec![
        page_read(&page_url),
        finish("the page requires login"),
    ])
    .await;
    let mut service = ResolverService::new(RecipeRegistry::empty(), loopback_policy())
        .with_ai_config(ai_config(&llm))
        .with_credentials(host_credentials("127.0.0.1"));

    let outcome = service
        .resolve(request(&page_url, ResolveMode::Explore))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(outcome.diagnostics.llm_calls, 2);
    assert!(!outcome.diagnostics.summary.contains(SECRET_COOKIE));

    // Builtin verification (HEAD + bounded GET) carried the cookie; the
    // exploration page_read did not.
    let requests = site.requests();
    let with_cookie = requests
        .iter()
        .filter(|request| saw_cookie(request))
        .count();
    let without_cookie = requests
        .iter()
        .filter(|request| request.header("cookie").is_none())
        .count();
    assert_eq!(
        with_cookie, 1,
        "the builtin verification HEAD keeps the credentials: {requests:?}"
    );
    assert_eq!(
        without_cookie, 1,
        "the exploration page_read must not send the cookie: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.method == "GET" && !saw_cookie(request)),
        "the cookie-less request is the exploration page_read: {requests:?}"
    );

    // The 401 is fed back to the model, but never the credential value.
    let llm_bodies = llm.bodies();
    assert!(
        llm_bodies.iter().all(|body| !body.contains(SECRET_COOKIE)),
        "exploration prompts must never contain the cookie value"
    );
    assert!(
        llm_bodies
            .iter()
            .any(|body| body.contains("401") || body.contains("status=401")),
        "the tool result should expose the 401 to the model: {llm_bodies:?}"
    );
}

// ---------------------------------------------------------------------------
// 8. Traces / outcome JSON / diagnostics never contain the secret values
// ---------------------------------------------------------------------------

#[tokio::test]
async fn traces_and_outcomes_never_leak_credentials() {
    let site = guarded_site().await;
    let file_url = site.url("/file.bin");
    let fixture = RecipeFixture::new("p4a-redaction");
    let credentials =
        host_credentials("127.0.0.1").with_header("127.0.0.1", "authorization", SECRET_BEARER);
    let mut service = fixture.service().with_credentials(credentials);

    let outcome = service.resolve(request(&file_url, ResolveMode::Auto)).await;
    assert_eq!(outcome.status, RunStatus::Succeeded);

    // ResolveOutcome JSON.
    let encoded = serde_json::to_string(&outcome).expect("serialize outcome");
    for secret in [SECRET_COOKIE, "SUPERSECRET", "abc123", SECRET_BEARER] {
        assert!(
            !encoded.contains(secret),
            "outcome leaked {secret:?}: {encoded}"
        );
    }

    // Persisted trace files (raw bytes).
    let mut trace_files = 0usize;
    for entry in std::fs::read_dir(runs_dir(&fixture)).expect("runs dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        trace_files += 1;
        let raw = std::fs::read_to_string(&path).expect("read trace");
        for secret in [SECRET_COOKIE, "SUPERSECRET", "abc123", SECRET_BEARER] {
            assert!(
                !raw.contains(secret),
                "trace {} leaked {secret:?}: {raw}",
                path.display()
            );
        }
    }
    assert_eq!(trace_files, 1, "the run must persist exactly one trace");

    // Diagnostics and notes are clean too.
    assert!(!outcome.diagnostics.summary.contains("SUPERSECRET"));
    if let Some(error) = &outcome.diagnostics.error {
        assert!(!error.detail.contains("SUPERSECRET"));
    }
    if let Some(manifest) = &outcome.manifest {
        for note in &manifest.notes {
            assert!(!note.contains("SUPERSECRET"), "note leaked: {note}");
        }
        for item in &manifest.resources {
            assert!(!item.url.contains("SUPERSECRET"));
            assert!(item.credentials.cookie_ref.is_none());
            assert!(item.credentials.headers.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Cross-origin redirects never forward credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_origin_redirect_drops_credentials() {
    let target = guarded_site().await;
    let target_url = target.url("/file.bin");
    let origin = redirecting_site(target_url.clone()).await;
    let start_url = origin.url("/start");
    let mut service = ResolverService::new(RecipeRegistry::empty(), loopback_policy())
        .with_credentials(host_credentials("127.0.0.1"));

    let outcome = service
        .resolve(request(&start_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(error_kind(&outcome), Some(ResolveErrorKind::AuthRequired));
    assert!(
        cookie_count(&origin) >= 1,
        "the initial request keeps the credentials"
    );
    assert_eq!(
        cookie_count(&target),
        0,
        "a redirect to another origin must strip the cookie: {:?}",
        target.requests()
    );
}

// ---------------------------------------------------------------------------
// 10. Same-origin redirects keep the credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_origin_redirect_keeps_credentials() {
    let site = guarded_site().await;
    let start_url = site.url("/redirect/same");
    let mut service = ResolverService::new(RecipeRegistry::empty(), loopback_policy())
        .with_credentials(host_credentials("127.0.0.1"));

    let outcome = service
        .resolve(request(&start_url, ResolveMode::Auto))
        .await;

    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "same-origin redirect keeps the cookie: {}",
        outcome.diagnostics.summary
    );
    let requests = site.requests();
    assert!(requests
        .iter()
        .any(|request| request.path == "/redirect/same"));
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/file.bin" && saw_cookie(request)),
        "the redirected hop carries the cookie: {requests:?}"
    );
}

// ---------------------------------------------------------------------------
// 12. A recipe that emits a guarded file is partial without the cookie
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recipe_emitting_a_guarded_file_is_partial_without_credentials() {
    // The replay itself succeeds (one always-passing step); only the verifier
    // of the emitted URL hits the 401 — a non-structural, partial result.
    let site = guarded_site().await;
    let fixture = RecipeFixture::new("p4a-recipe-guarded-emit");
    fixture.save(&recipe_json(
        "guarded-emit",
        1,
        json!({
            "hosts": ["127.0.0.1"],
            "url_pattern": r"^http://127\.0\.0\.1:\d+/page/\d+$",
            "priority": 5
        }),
        json!([{
            "id": "pass",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }]),
        json!({ "url": site.url("/file.bin") }),
    ));

    let mut no_creds = fixture.service();
    let outcome = no_creds
        .resolve(request(&site.url("/page/1"), ResolveMode::RecipeOnly))
        .await;
    assert_eq!(outcome.status, RunStatus::Partial);
    assert_eq!(error_kind(&outcome), Some(ResolveErrorKind::AuthRequired));
    let manifest = outcome.manifest.as_ref().expect("partial keeps candidates");
    assert_eq!(manifest.resources.len(), 1);
    assert_eq!(manifest.downloadable().count(), 0);

    let mut with_creds = fixture
        .service()
        .with_credentials(host_credentials("127.0.0.1"));
    let outcome = with_creds
        .resolve(request(&site.url("/page/1"), ResolveMode::RecipeOnly))
        .await;
    assert_eq!(outcome.status, RunStatus::Succeeded);
    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
}

// ---------------------------------------------------------------------------
// 11. Empty credentials behave exactly like no credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_credentials_behave_like_no_credentials() {
    let site = guarded_site().await;
    let file_url = site.url("/file.bin");
    let mut service = ResolverService::new(RecipeRegistry::empty(), loopback_policy())
        .with_credentials(HostCredentials::default());

    let outcome = service.resolve(request(&file_url, ResolveMode::Auto)).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(error_kind(&outcome), Some(ResolveErrorKind::AuthRequired));
    assert_eq!(cookie_count(&site), 0);
}
