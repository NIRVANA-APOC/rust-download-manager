//! Service-level P3a/P3b wiring (`docs/architecture.md` §11.10/§11.11): a recipe
//! replay updates method health and persists a sanitized trace; exploration
//! persists a trace plus a local recipe draft (but no method health); every
//! persistence failure is best-effort and never changes the caller-visible
//! [`rdm_resolve::ResolveOutcome`].

mod common;
mod support;

use std::path::{Path, PathBuf};

use common::llm::{LlmReply, MockLlm};
use common::{recipe_json, RecipeFixture};
use rdm_resolve::{
    drafts_root, AiConfig, MethodStatus, RecipeRegistry, ResolveErrorKind, ResolveMode,
    ResolvePolicy, ResolveRequest, ResolverService, RunStatus, TraceStore, DRAFT_SCHEMA,
    TRACE_SCHEMA,
};
use serde_json::json;
use support::{MockServer, Reply};

const API_KEY: &str = "sk-p3a-scripted-key-0123456789";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Direct-file + HTML-page fixture site used by the replay tests.
async fn replay_site() -> MockServer {
    MockServer::start(|request| match request.path.as_str() {
        "/files/42.bin" => match request.method.as_str() {
            "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
            _ => Reply::file(2048).header("accept-ranges", "bytes"),
        },
        _ => Reply::status(404),
    })
    .await
}

fn replay_request(url: &str, mode: ResolveMode) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        mode,
        ..ResolveRequest::default()
    }
}

/// One-assert-step recipe that emits `file_url`; matches by host only, so the
/// input URL may carry a query/fragment that must be redacted from the trace.
fn success_recipe(file_url: &str) -> String {
    recipe_json(
        "site-file",
        1,
        json!({ "hosts": ["127.0.0.1"], "priority": 5 }),
        json!([{
            "id": "pass",
            "op": "assert",
            "args": { "kind": "equals", "value": "ok", "expected": "ok" }
        }]),
        json!({ "url": file_url }),
    )
}

/// Recipe whose only step expects JSON; a maintenance page makes it drift.
fn drift_recipe(api_url: &str) -> String {
    recipe_json(
        "drift-site",
        1,
        json!({ "hosts": ["127.0.0.1"], "priority": 5 }),
        json!([{
            "id": "api",
            "op": "http_request",
            "args": { "url": api_url },
            "expect": { "content_type_prefix": ["application/json"] },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": "https://cdn.example.com/never.bin" }),
    )
}

fn resolve_root(fixture: &RecipeFixture) -> PathBuf {
    fixture.state_dir().join("resolve")
}

fn index_file(fixture: &RecipeFixture) -> PathBuf {
    resolve_root(fixture).join("index.json")
}

fn runs_dir(fixture: &RecipeFixture) -> PathBuf {
    TraceStore::runs_dir(&resolve_root(fixture))
}

fn load_trace(fixture: &RecipeFixture, run_id: &str) -> rdm_resolve::RunTrace {
    TraceStore::load(&resolve_root(fixture), run_id).expect("trace must be loadable")
}

/// `<draft id>@<version>.json` for an outcome recipe ref.
fn draft_file_name_from(recipe: &rdm_resolve::RecipeRef) -> String {
    format!("{}@{}.json", recipe.id, recipe.version)
}

fn reloaded_registry(fixture: &RecipeFixture) -> RecipeRegistry {
    RecipeRegistry::with_root(fixture.recipes_dir())
}

fn first_line(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).expect("read trace file");
    let line = raw.lines().next().expect("trace header line");
    serde_json::from_str(line).expect("trace header JSON")
}

// ---------------------------------------------------------------------------
// Exploration harness (mirrors tests/explore_test.rs, trimmed)
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
        api_key: API_KEY.to_owned(),
        ..AiConfig::default()
    }
}

fn page_read(url: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "page_read", "url": url }).to_string())
}

fn first_obs_id(body: &str) -> String {
    let regex = regex::Regex::new(r"obs_[0-9]+_[0-9]+").expect("obs id regex");
    let found = regex
        .find_iter(body)
        .map(|hit| hit.as_str())
        .next()
        .map(str::to_owned);
    found.unwrap_or_else(|| panic!("no observation id in request body:\n{body}"))
}

fn emit_page_link(file_url: impl Into<String>) -> LlmReply {
    let file_url = file_url.into();
    LlmReply::dynamic(move |request| {
        let obs = first_obs_id(&request.body);
        json!({
            "action": "emit_manifest",
            "resources": [{
                "url": file_url,
                "evidence": format!("{obs}#links/0"),
            }]
        })
        .to_string()
    })
}

async fn explore_site() -> MockServer {
    MockServer::start(|request| {
        let host = request.header("host").unwrap_or("127.0.0.1");
        if request.path == "/page" {
            return Reply::html(&format!(
                "<!doctype html><html><body>\
                 <a href=\"http://{host}/files/42.bin\">Download</a>\
                 </body></html>"
            ));
        }
        match request.path.as_str() {
            "/files/42.bin" => match request.method.as_str() {
                "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
                _ => Reply::file(2048).header("accept-ranges", "bytes"),
            },
            _ => Reply::status(404),
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// 1. Recipe success: trace + health + active_version are all persisted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recipe_success_persists_a_sanitized_trace_and_ok_health() {
    let site = replay_site().await;
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p3a-recipe-success");
    fixture.save(&success_recipe(&file_url));
    let mut service = fixture.service();

    let input = format!("{}/page/42?token=SECRET&sig=SECRET#frag", site.base_url());
    let outcome = service
        .resolve(replay_request(&input, ResolveMode::RecipeOnly))
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(outcome.diagnostics.error.is_none());
    assert_eq!(outcome.diagnostics.steps, 1);

    // --- trace file -----------------------------------------------------
    let run_id = outcome.run_id.clone();
    let trace_path = runs_dir(&fixture).join(format!("{run_id}.jsonl"));
    assert!(trace_path.is_file(), "missing trace {trace_path:?}");
    let header = first_line(&trace_path);
    assert_eq!(header["schema"], TRACE_SCHEMA);
    assert_eq!(header["run_id"], run_id);
    assert_eq!(header["mode"], "recipe");
    assert_eq!(header["status"], "succeeded");
    assert_eq!(header["recipe"]["id"], "site-file");
    assert_eq!(header["recipe"]["version"], 1);
    assert_eq!(header["llm_calls"], 0);
    assert_eq!(header["http_requests"], outcome.diagnostics.http_requests);
    let redacted = format!("{}/page/42", site.base_url());
    assert_eq!(header["source_url"], redacted);
    let raw = std::fs::read_to_string(&trace_path).expect("read trace");
    assert!(!raw.contains("SECRET"), "input token leaked: {raw}");
    assert!(!raw.contains('?'), "query marker leaked: {raw}");

    let trace = load_trace(&fixture, &run_id);
    assert_eq!(trace.steps.len(), outcome.diagnostics.steps as usize);
    assert!(trace.steps.iter().all(|step| step.op == "recipe_step"));
    assert!(trace.steps.iter().all(|step| step.outcome == "ok"));

    // --- health + index -------------------------------------------------
    let health = service.registry().health("site-file");
    assert_eq!(health.status, MethodStatus::Ok);
    assert_eq!(health.consecutive_failures, 0);
    assert!(health.last_ok_ms.is_some());
    assert_eq!(service.registry().active_version("site-file"), Some(1));
    assert!(index_file(&fixture).is_file());

    let reloaded = reloaded_registry(&fixture);
    assert_eq!(reloaded.health("site-file").status, MethodStatus::Ok);
    assert_eq!(reloaded.active_version("site-file"), Some(1));
}

// ---------------------------------------------------------------------------
// 2. Structural drift: trace records the failure and health degrades/broken
// ---------------------------------------------------------------------------

#[tokio::test]
async fn structural_drift_degrades_then_breaks_method_health() {
    let site = MockServer::start(|_| Reply::html("<!doctype html><html>maintenance</html>")).await;
    let fixture = RecipeFixture::new("p3a-structural-drift");
    fixture.save(&drift_recipe(&site.url("/api/item/42")));
    let mut service = fixture.service();
    let input = site.url("/page/42");

    let mut outcomes = Vec::new();
    let mut healths = Vec::new();
    for _ in 0..3 {
        outcomes.push(
            service
                .resolve(replay_request(&input, ResolveMode::RecipeOnly))
                .await,
        );
        // Snapshot the health right after this run (later runs would advance it).
        healths.push(service.registry().health("drift-site"));
    }

    for outcome in &outcomes {
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(
            outcome.diagnostics.error.as_ref().map(|error| error.kind),
            Some(ResolveErrorKind::RecipeStale)
        );
    }

    // Health: first structural failure keeps `untested`, 2nd degrades, 3rd breaks.
    let expected = [
        (MethodStatus::Untested, 1),
        (MethodStatus::Degraded, 2),
        (MethodStatus::Broken, 3),
    ];
    for (index, (status, streak)) in expected.into_iter().enumerate() {
        let outcome = &outcomes[index];
        let run_id = outcome.run_id.clone();
        let trace = load_trace(&fixture, &run_id);
        assert_eq!(trace.status, "failed");
        assert_eq!(trace.error_kind.as_deref(), Some("recipe_stale"));
        assert_eq!(
            trace.recipe.as_ref().map(|recipe| recipe.id.as_str()),
            Some("drift-site")
        );
        assert_eq!(
            trace.steps.len(),
            outcome.diagnostics.steps as usize + 1,
            "failed replay = executed steps + one error step"
        );
        let last = trace.steps.last().expect("error step");
        assert_eq!(last.op, "recipe_step");
        assert_eq!(last.outcome, "error:recipe_stale");
        assert_eq!(last.note.as_deref(), Some("api"));

        let health = &healths[index];
        assert_eq!(health.status, status, "run {}", index + 1);
        assert_eq!(health.consecutive_failures, streak, "run {}", index + 1);
        assert_eq!(
            health.last_failure.as_ref().map(|failure| failure.kind),
            Some(ResolveErrorKind::RecipeStale)
        );
    }

    let reloaded = reloaded_registry(&fixture);
    assert_eq!(reloaded.health("drift-site").status, MethodStatus::Broken);
    assert_eq!(reloaded.health("drift-site").consecutive_failures, 3);
}

// ---------------------------------------------------------------------------
// 3. Non-structural failure (403 on the emitted resource) never degrades
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_structural_failure_does_not_degrade_health() {
    let site = MockServer::start(|_| Reply::status(403)).await;
    let fixture = RecipeFixture::new("p3a-non-structural");
    fixture.save(&success_recipe(&site.url("/private.bin")));
    let mut service = fixture.service();
    let input = site.url("/page/1");

    for run in 1..=3 {
        let outcome = service
            .resolve(replay_request(&input, ResolveMode::RecipeOnly))
            .await;
        assert_eq!(outcome.status, RunStatus::Partial, "run {run}");
        assert_eq!(
            outcome.diagnostics.error.as_ref().map(|error| error.kind),
            Some(ResolveErrorKind::Forbidden),
            "a 403 candidate is a forbidden failure, not recipe drift"
        );

        let trace = load_trace(&fixture, &outcome.run_id);
        assert_eq!(trace.status, "partial");
        assert_eq!(trace.error_kind.as_deref(), Some("forbidden"));

        let health = service.registry().health("site-file");
        assert_eq!(
            health.status,
            MethodStatus::Untested,
            "run {run}: a 403 must not disable the recipe"
        );
        assert_eq!(health.consecutive_failures, 0, "run {run}");
        assert_eq!(
            health.last_failure.as_ref().map(|failure| failure.kind),
            Some(ResolveErrorKind::Forbidden),
            "run {run}: last_failure still records the kind"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Exploration: trace + local draft (P3b), but no recipe health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exploration_writes_a_trace_and_a_draft_but_no_recipe_health() {
    let site = explore_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let llm = MockLlm::start(vec![page_read(&page_url), emit_page_link(&file_url)]).await;
    let fixture = RecipeFixture::new("p3a-explore-trace");
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let mut request = replay_request(&page_url, ResolveMode::Explore);
    request.budget = Some(rdm_resolve::Budget {
        wall_ms: 20_000,
        ..rdm_resolve::Budget::default()
    });
    let outcome = service.resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 2);
    assert_eq!(outcome.diagnostics.steps, 2);

    // P3b: a successful, expressible exploration distils a local draft and the
    // outcome points at it (state="draft").
    let draft_ref = outcome
        .recipe
        .as_ref()
        .expect("a state-backed exploration must expose its draft");
    assert_eq!(draft_ref.state, "draft");
    assert!(draft_ref.id.starts_with("draft_"));
    let draft_path = drafts_root(&resolve_root(&fixture)).join(draft_file_name_from(draft_ref));
    assert!(draft_path.is_file(), "missing draft {draft_path:?}");
    let draft_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&draft_path).expect("read draft"))
            .expect("draft JSON");
    assert_eq!(draft_json["schema"], DRAFT_SCHEMA);
    assert!(outcome
        .manifest
        .as_ref()
        .expect("manifest")
        .notes
        .iter()
        .any(|note| note.contains("draft saved:")));

    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.mode, "explore");
    assert_eq!(trace.status, "succeeded");
    // The trace records the draft reference (report.recipe falls back to the
    // outcome's draft ref) — exploration still has no *matched* recipe.
    assert_eq!(
        trace.recipe.as_ref().map(|recipe| recipe.state.as_str()),
        Some("draft"),
        "trace must record the generated draft ref"
    );
    assert_eq!(
        trace.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        Some(draft_ref.id.as_str())
    );
    assert_eq!(trace.llm_calls, 2);
    assert_eq!(trace.http_requests, outcome.diagnostics.http_requests);
    assert_eq!(
        trace.steps.len(),
        1 + outcome.diagnostics.steps as usize,
        "one builtin_verify step + one step per executed action"
    );
    assert_eq!(trace.steps[0].op, "builtin_verify");
    let ops: Vec<&str> = trace.steps.iter().map(|step| step.op.as_str()).collect();
    assert_eq!(ops, vec!["builtin_verify", "page_read", "emit_manifest"]);

    // Exploration never produces method health, hence no index file.
    assert!(
        !index_file(&fixture).exists(),
        "no recipe health for explore"
    );
    assert!(service.registry().index().entries.is_empty());
    assert!(reloaded_registry(&fixture).index().entries.is_empty());
}

// ---------------------------------------------------------------------------
// 5. `ResolverService::new` has no state root: nothing is written
// ---------------------------------------------------------------------------

#[tokio::test]
async fn service_without_state_dir_persists_nothing() {
    let site = replay_site().await;
    let file_url = site.url("/files/42.bin");
    let service = ResolverService::new(RecipeRegistry::empty(), ResolvePolicy::loopback());
    assert!(service.trace_root().is_none());
    assert!(service.registry().root().is_none());

    let mut service = service;
    let outcome = service
        .resolve(replay_request(&file_url, ResolveMode::Auto))
        .await;
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(outcome.manifest.is_some());
    // Nothing else to assert on disk: the service has no root at all.
}

// ---------------------------------------------------------------------------
// 6. Unwritable trace root only warns: outcome and health are unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unwritable_trace_root_does_not_change_the_outcome() {
    let site = replay_site().await;
    let file_url = site.url("/files/42.bin");

    // Baseline: same run with a writable state dir.
    let baseline_fixture = RecipeFixture::new("p3a-trace-baseline");
    baseline_fixture.save(&success_recipe(&file_url));
    let mut baseline_service = baseline_fixture.service();
    let baseline = baseline_service
        .resolve(replay_request(
            &site.url("/page/42"),
            ResolveMode::RecipeOnly,
        ))
        .await;

    // Blocked: the trace root is a regular file.
    let blocked_fixture = RecipeFixture::new("p3a-trace-blocked");
    blocked_fixture.save(&success_recipe(&file_url));
    let blocked_root = blocked_fixture.state_dir().join("blocked-trace-root");
    std::fs::write(&blocked_root, b"this is a file, not a directory").expect("blocking file");
    let mut blocked_service = blocked_fixture.service().with_trace_root(&blocked_root);
    let blocked = blocked_service
        .resolve(replay_request(
            &site.url("/page/42"),
            ResolveMode::RecipeOnly,
        ))
        .await;

    assert_eq!(blocked.status, baseline.status);
    assert_eq!(
        blocked.diagnostics.error.as_ref().map(|error| error.kind),
        baseline.diagnostics.error.as_ref().map(|error| error.kind)
    );
    assert_eq!(
        blocked.diagnostics.llm_calls,
        baseline.diagnostics.llm_calls
    );
    assert_eq!(
        blocked.diagnostics.http_requests,
        baseline.diagnostics.http_requests
    );
    assert_eq!(
        blocked.diagnostics.bytes_read,
        baseline.diagnostics.bytes_read
    );
    assert_eq!(blocked.diagnostics.steps, baseline.diagnostics.steps);
    assert_eq!(
        blocked.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        baseline.recipe.as_ref().map(|recipe| recipe.id.as_str())
    );
    let blocked_manifest = blocked.manifest.as_ref().expect("manifest");
    let baseline_manifest = baseline.manifest.as_ref().expect("manifest");
    assert_eq!(
        blocked_manifest.resources.len(),
        baseline_manifest.resources.len()
    );
    assert_eq!(
        blocked_manifest.resources[0].url,
        baseline_manifest.resources[0].url
    );

    assert!(blocked_root.is_file(), "the blocking file must stay a file");
    assert!(
        !runs_dir(&blocked_fixture).exists(),
        "no runs directory can be created under a file"
    );
    // Health persistence is independent from the trace root: still recorded.
    assert!(index_file(&blocked_fixture).is_file());
    assert_eq!(
        blocked_service.registry().health("site-file").status,
        MethodStatus::Ok
    );
}

// ---------------------------------------------------------------------------
// 7. Auto builtin direct-file run: one builtin_verify trace step, no index
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_builtin_success_trace_has_one_builtin_verify_step() {
    let site = replay_site().await;
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p3a-auto-builtin");
    let mut service = fixture.service();

    let mut request = replay_request(&file_url, ResolveMode::Auto);
    request.max_resources = Some(1);
    let outcome = service.resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(outcome.recipe.is_none());
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.mode, "auto");
    assert_eq!(trace.status, "succeeded");
    assert!(trace.recipe.is_none());
    assert_eq!(trace.steps.len(), 1);
    assert_eq!(trace.steps[0].op, "builtin_verify");
    assert_eq!(trace.steps[0].outcome, "ok");
    // Builtin verification never updates recipe health -> no index entry/file.
    assert!(!index_file(&fixture).exists());
}

// ---------------------------------------------------------------------------
// 8. Recipe no-match failure: trace records the typed error, no recipe ref
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recipe_no_match_trace_records_the_typed_error() {
    let site = replay_site().await;
    let fixture = RecipeFixture::new("p3a-no-match");
    let mut service = fixture.service();

    let outcome = service
        .resolve(replay_request(
            &site.url("/page/42"),
            ResolveMode::RecipeOnly,
        ))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeNoMatch)
    );
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.status, "failed");
    assert_eq!(trace.error_kind.as_deref(), Some("recipe_no_match"));
    assert!(trace.recipe.is_none(), "no recipe was selected");
    assert!(trace.steps.is_empty());
}
