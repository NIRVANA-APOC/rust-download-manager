//! Auto-repair fallback (`docs/architecture.md` §11.5 v0.9, `docs/agent-integration.md`
//! §8.7): when `mode=auto` matches a recipe through the registry and the replay
//! fails for a *structural* reason, a configured AI provider triggers the
//! exploration path instead, the final outcome is the exploration result, the
//! drifted method still records its health failure, and one merged trace keeps
//! `recipe_replay` as its first step.
//!
//! All fixtures are ephemeral loopback servers plus the shared scripted mock LLM
//! (`tests/common/llm.rs`); no public network and no real model is used.

mod common;
mod support;

use std::path::PathBuf;

use common::llm::{LlmReply, MockLlm};
use common::{recipe_json, RecipeFixture};
use rdm_resolve::{
    list_drafts, AiConfig, Budget, MethodStatus, RecipeRegistry, ResolveErrorKind, ResolveMode,
    ResolveRequest, RunStatus, RunTrace, TraceStore,
};
use serde_json::json;
use support::{MockServer, Reply};

const API_KEY: &str = "sk-auto-repair-key-0123456789";
/// Recipe id of the drifting method used across the suite.
const DRIFT_ID: &str = "drift-auto";

// ---------------------------------------------------------------------------
// Harness
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

/// A configured provider whose endpoint refuses connections.
fn unreachable_ai_config() -> AiConfig {
    scrub_ambient_env_key();
    AiConfig {
        enabled: true,
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "scripted-model".to_owned(),
        api_key: API_KEY.to_owned(),
        timeout_secs: 1,
        // Loopback mock: not a bundled provider (no preset/no prices).
        provider: None,
        ..AiConfig::default()
    }
}

fn resolve_root(fixture: &RecipeFixture) -> PathBuf {
    fixture.state_dir().join("resolve")
}

fn runs_dir(fixture: &RecipeFixture) -> PathBuf {
    resolve_root(fixture).join("runs")
}

/// Existing `runs/*.jsonl` files (ordered) so tests can assert the delta.
fn run_files(fixture: &RecipeFixture) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(runs_dir(fixture))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn load_trace(fixture: &RecipeFixture, run_id: &str) -> RunTrace {
    TraceStore::load(&resolve_root(fixture), run_id).expect("trace must be loadable")
}

fn replay_request(url: &str, mode: ResolveMode) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        mode,
        budget: Some(Budget {
            wall_ms: 20_000,
            ..Budget::default()
        }),
        ..ResolveRequest::default()
    }
}

fn drift_recipe(api_url: &str, emit_url: &str) -> String {
    recipe_json(
        DRIFT_ID,
        1,
        json!({
            "hosts": ["127.0.0.1"],
            "url_pattern": r"^http://127\.0\.0\.1:\d+/page/\d+$",
            "priority": 5
        }),
        json!([{
            "id": "api",
            "op": "http_request",
            "args": { "url": api_url },
            "expect": { "content_type_prefix": ["application/json"] },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": emit_url }),
    )
}

/// Like [`drift_recipe`] but matches by host only, so an input URL that is a
/// direct file still selects the (drifting) recipe.
fn host_only_drift_recipe(api_url: &str, emit_url: &str) -> String {
    recipe_json(
        "direct-drift",
        1,
        json!({ "hosts": ["127.0.0.1"], "priority": 5 }),
        json!([{
            "id": "api",
            "op": "http_request",
            "args": { "url": api_url },
            "expect": { "content_type_prefix": ["application/json"] },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": emit_url }),
    )
}

/// Minimal recipe: one always-passing assert step, then emit `emit_url`.
fn emit_recipe(id: &str, emit_url: &str) -> String {
    recipe_json(
        id,
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
        json!({ "url": emit_url }),
    )
}

/// Fixture site:
/// * `/page/<n>` — HTML share page linking `/files/42.bin` (index 0);
/// * `/api/<n>`  — HTML maintenance page (drifts the recipe's JSON expectation);
/// * `/files/42.bin` — binary file.
async fn drift_site() -> MockServer {
    MockServer::start(|request| {
        let host = request.header("host").unwrap_or("127.0.0.1").to_owned();
        if request.path.starts_with("/page/") {
            return Reply::html(&format!(
                "<!doctype html><html><body>\
                 <a href=\"http://{host}/files/42.bin\">Download</a>\
                 </body></html>"
            ));
        }
        if request.path.starts_with("/api/") {
            return Reply::html("<!doctype html><html>maintenance</html>");
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
// Scripted exploration helpers
// ---------------------------------------------------------------------------

fn page_read(url: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "page_read", "url": url }).to_string())
}

fn http_head(url: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "http_request", "method": "HEAD", "url": url }).to_string())
}

fn finish(reason: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "finish", "reason": reason }).to_string())
}

fn first_obs_id(body: &str) -> String {
    let regex = regex::Regex::new(r"obs_[0-9]+_[0-9]+").expect("obs id regex");
    let found = regex
        .find_iter(body)
        .map(|hit| hit.as_str())
        .next()
        .map(str::to_owned);
    found.unwrap_or_else(|| panic!("expected an observation id in the request body:\n{body}"))
}

fn last_obs_id(body: &str) -> String {
    let regex = regex::Regex::new(r"obs_[0-9]+_[0-9]+").expect("obs id regex");
    let found = regex
        .find_iter(body)
        .map(|hit| hit.as_str())
        .last()
        .map(str::to_owned);
    found.unwrap_or_else(|| panic!("expected an observation id in the request body:\n{body}"))
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

/// Emit a URL with the *newest* plain observation handle (used for the
/// unexpressible HEAD chain).
fn emit_plain_obs(file_url: impl Into<String>) -> LlmReply {
    let file_url = file_url.into();
    LlmReply::dynamic(move |request| {
        let obs = last_obs_id(&request.body);
        json!({
            "action": "emit_manifest",
            "resources": [{ "url": file_url, "evidence": obs }]
        })
        .to_string()
    })
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

fn assert_auto_repair_mentioned(
    outcome: &rdm_resolve::ResolveOutcome,
    recipe_id: &str,
    notes_required: bool,
) {
    let summary = &outcome.diagnostics.summary;
    let lower = summary.to_ascii_lowercase();
    assert!(
        lower.contains("auto-repair") || lower.contains("auto repair"),
        "summary must mention the auto-repair fallback: {summary}"
    );
    assert!(
        summary.contains(recipe_id),
        "summary must name the drifted recipe: {summary}"
    );
    assert!(
        lower.contains("recipe_stale"),
        "summary must name the failure kind: {summary}"
    );
    if notes_required {
        let manifest = outcome.manifest.as_ref().expect("successful manifest");
        assert!(
            manifest
                .notes
                .iter()
                .any(|note| note.to_ascii_lowercase().contains("auto-repair")),
            "manifest.notes must mention the auto-repair fallback: {:?}",
            manifest.notes
        );
    }
}

/// The merged auto-repair trace: `recipe_replay` first (structural error,
/// failing step id), contiguous indices, the drifted recipe in the header and
/// the exploration steps afterwards.
fn assert_auto_repair_trace(
    trace: &RunTrace,
    recipe_id: &str,
    version: u32,
    kind: &str,
    step_id: &str,
) {
    assert_eq!(
        trace.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        Some(recipe_id),
        "the trace header must record the drifted recipe"
    );
    assert_eq!(
        trace.recipe.as_ref().map(|recipe| recipe.version),
        Some(version)
    );
    let first = trace
        .steps
        .first()
        .expect("trace must keep the replay step");
    assert_eq!(first.op, "recipe_replay");
    assert_eq!(first.outcome, format!("error:{kind}"));
    assert_eq!(
        first.note.as_deref(),
        Some(step_id),
        "the replay step must name the failing step id"
    );
    assert_eq!(
        step_position(&trace.steps, "builtin_verify"),
        Some(1),
        "builtin verification follows the replay step: {:?}",
        trace
            .steps
            .iter()
            .map(|step| step.op.clone())
            .collect::<Vec<_>>()
    );
    for (position, step) in trace.steps.iter().enumerate() {
        assert_eq!(
            step.index,
            position as u32,
            "trace step indices must be contiguous from 0: {:?}",
            trace
                .steps
                .iter()
                .map(|step| step.index)
                .collect::<Vec<_>>()
        );
    }
}

fn step_position(steps: &[rdm_resolve::RunStep], op: &str) -> Option<usize> {
    steps.iter().position(|step| step.op == op)
}

// ---------------------------------------------------------------------------
// 1. Structural drift + AI: exploration repairs the run, single merged trace
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_repair_on_structural_drift_succeeds_with_a_single_merged_trace() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-success");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    let llm = MockLlm::start(vec![page_read(&page_url), emit_page_link(&file_url)]).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let runs_before = run_files(&fixture);
    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    // --- exploration result wins ----------------------------------------
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 2);
    assert_eq!(llm.request_count(), 2);
    let draft_ref = outcome.recipe.as_ref().expect("auto-repaired draft ref");
    assert_eq!(draft_ref.state, "draft");
    assert!(draft_ref.id.starts_with("draft_"));
    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
    assert_auto_repair_mentioned(&outcome, DRIFT_ID, true);

    // ...and the draft really exists on disk.
    let drafts = list_drafts(&resolve_root(&fixture));
    assert_eq!(drafts.len(), 1, "auto-repair distils one draft");
    assert_eq!(drafts[0].id, draft_ref.id);

    // --- exactly one merged trace ---------------------------------------
    let runs_after = run_files(&fixture);
    assert_eq!(
        runs_after.len(),
        runs_before.len() + 1,
        "auto-repair must persist exactly one trace: {runs_after:?}"
    );
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.status, "succeeded");
    assert_auto_repair_trace(&trace, DRIFT_ID, 1, "recipe_stale", "api");
    let ops: Vec<&str> = trace.steps.iter().map(|step| step.op.as_str()).collect();
    assert!(
        ops.contains(&"page_read") && ops.contains(&"emit_manifest"),
        "exploration steps follow the replay step: {ops:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Health: the drift keeps degrading the old method across auto-repairs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_repair_still_degrades_the_drifted_method_health() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-health");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    let llm = MockLlm::start(vec![
        page_read(&page_url),
        emit_page_link(&file_url),
        page_read(&page_url),
        emit_page_link(&file_url),
    ])
    .await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    // First run: one structural failure, status stays `untested`.
    let first = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;
    assert_eq!(first.status, RunStatus::Succeeded);
    let health = service.registry().health(DRIFT_ID);
    assert_eq!(health.status, MethodStatus::Untested);
    assert_eq!(health.consecutive_failures, 1);
    assert_eq!(
        health.last_failure.as_ref().map(|failure| failure.kind),
        Some(ResolveErrorKind::RecipeStale)
    );

    // Second run: degraded, streak 2 — exploration success does not reset it.
    let second = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;
    assert_eq!(second.status, RunStatus::Succeeded);
    let health = service.registry().health(DRIFT_ID);
    assert_eq!(health.status, MethodStatus::Degraded);
    assert_eq!(health.consecutive_failures, 2);
    assert_eq!(llm.request_count(), 4);

    // Persisted in index.json.
    let reloaded = RecipeRegistry::with_root(fixture.recipes_dir());
    assert_eq!(reloaded.health(DRIFT_ID).status, MethodStatus::Degraded);
    assert_eq!(reloaded.health(DRIFT_ID).consecutive_failures, 2);
}

// ---------------------------------------------------------------------------
// 3. No AI: same drift stays a recipe failure (v1.6 behaviour)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn structural_drift_without_ai_does_not_trigger_auto_repair() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-no-ai");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    let mut service = fixture.service();
    assert!(service.registry().root().is_some());

    let runs_before = run_files(&fixture);
    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeStale)
    );
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert!(outcome.manifest.is_none());
    assert!(!outcome
        .diagnostics
        .summary
        .to_ascii_lowercase()
        .contains("auto-repair"));
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());

    let runs_after = run_files(&fixture);
    assert_eq!(runs_after.len(), runs_before.len() + 1);
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.status, "failed");
    assert_eq!(trace.error_kind.as_deref(), Some("recipe_stale"));
    assert_eq!(
        trace.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        Some(DRIFT_ID)
    );
    let first = trace.steps.first().expect("trace steps");
    assert_ne!(
        first.op, "builtin_verify",
        "without auto-repair the trace is a plain recipe replay"
    );
    assert!(trace.steps.iter().any(|step| step.op == "recipe_step"));
}

// ---------------------------------------------------------------------------
// 4. Non-structural verification failures never trigger exploration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_structural_verification_failures_do_not_trigger_auto_repair() {
    let cases: [(u16, ResolveErrorKind); 3] = [
        (404, ResolveErrorKind::NotFound),
        (401, ResolveErrorKind::AuthRequired),
        (500, ResolveErrorKind::ServerError),
    ];

    for (status, expected_kind) in cases {
        let site = MockServer::start(move |_| Reply::status(status)).await;
        let page_url = site.url("/page/1");
        let file_url = site.url("/file.bin");
        let fixture = RecipeFixture::new(&format!("auto-repair-status-{status}"));
        fixture.save(&emit_recipe("emit-status", &file_url));
        let llm = MockLlm::start(Vec::new()).await;
        let mut service = fixture.service().with_ai_config(ai_config(&llm));

        let outcome = service
            .resolve(replay_request(&page_url, ResolveMode::Auto))
            .await;

        assert_eq!(
            outcome.status,
            RunStatus::Partial,
            "HTTP {status}: verification failure keeps the replay result"
        );
        assert_eq!(
            outcome.diagnostics.error.as_ref().map(|error| error.kind),
            Some(expected_kind),
            "HTTP {status}"
        );
        assert_eq!(outcome.diagnostics.llm_calls, 0, "HTTP {status}");
        assert_eq!(
            llm.request_count(),
            0,
            "HTTP {status}: the model must not be called"
        );
        assert!(!outcome
            .diagnostics
            .summary
            .to_ascii_lowercase()
            .contains("auto-repair"));
        assert!(list_drafts(&resolve_root(&fixture)).is_empty());
    }

    // An HTML page emitted as the resource is `not_a_file`, also non-structural.
    let site = MockServer::start(|_| Reply::html("<!doctype html><html>login</html>")).await;
    let page_url = site.url("/page/1");
    let file_url = site.url("/page-html");
    let fixture = RecipeFixture::new("auto-repair-html");
    fixture.save(&emit_recipe("emit-html", &file_url));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));
    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;
    assert_eq!(outcome.status, RunStatus::Partial);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::NotAFile)
    );
    assert_eq!(llm.request_count(), 0);
}

// ---------------------------------------------------------------------------
// 5. A step network error is non-structural: no exploration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_network_error_does_not_trigger_auto_repair() {
    let site = MockServer::start(|_| Reply::status(404)).await;
    let page_url = site.url("/page/1");
    let file_url = site.url("/file.bin");
    let fixture = RecipeFixture::new("auto-repair-network");
    fixture.save(&recipe_json(
        "network-recipe",
        1,
        json!({
            "hosts": ["127.0.0.1"],
            "url_pattern": r"^http://127\.0\.0\.1:\d+/page/\d+$",
            "priority": 5
        }),
        json!([{
            "id": "fetch",
            "op": "http_request",
            "args": { "url": "http://127.0.0.1:1/api" },
            "assign": { "body": "body_text" }
        }]),
        json!({ "url": file_url }),
    ));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::NetworkError)
    );
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(llm.request_count(), 0);
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());
}

// ---------------------------------------------------------------------------
// 6. mode=recipe-only never repairs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recipe_only_mode_does_not_trigger_auto_repair() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-recipe-only");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::RecipeOnly))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeStale)
    );
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(llm.request_count(), 0);
}

// ---------------------------------------------------------------------------
// 7. An explicitly selected recipe never auto-repairs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_recipe_id_does_not_trigger_auto_repair() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-explicit");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let mut request = replay_request(&page_url, ResolveMode::Auto);
    request.recipe_id = Some(DRIFT_ID.to_owned());
    let outcome = service.resolve(request).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::RecipeStale)
    );
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(llm.request_count(), 0);
}

// ---------------------------------------------------------------------------
// 8. Auto-repair whose exploration also fails: both reasons in the summary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_repair_falls_through_when_exploration_also_fails() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-explore-fail");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    let llm = MockLlm::start(vec![finish("no candidates here")]).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(outcome.diagnostics.llm_calls, 1);
    let lower = outcome.diagnostics.summary.to_ascii_lowercase();
    assert!(
        lower.contains("auto-repair"),
        "{}",
        outcome.diagnostics.summary
    );
    assert!(
        lower.contains("recipe_stale"),
        "{}",
        outcome.diagnostics.summary
    );
    assert!(
        lower.contains("no_resources")
            || lower.contains("no candidates")
            || lower.contains("exploration"),
        "the summary must also explain the failed exploration: {}",
        outcome.diagnostics.summary
    );
    assert!(outcome.manifest.is_none());
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());

    let trace = load_trace(&fixture, &outcome.run_id);
    assert_auto_repair_trace(&trace, DRIFT_ID, 1, "recipe_stale", "api");
    // The drift is still recorded even though the repair failed.
    let health = service.registry().health(DRIFT_ID);
    assert_eq!(health.consecutive_failures, 1);
    assert_eq!(
        health.last_failure.as_ref().map(|failure| failure.kind),
        Some(ResolveErrorKind::RecipeStale)
    );
}

// ---------------------------------------------------------------------------
// 9. Repaired run whose chain is unexpressible: success but no draft
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_repair_success_without_expressible_chain_has_no_draft() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-unexpressible");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    // HEAD observation is evidence-valid but cannot be distilled into a recipe.
    let llm = MockLlm::start(vec![http_head(&file_url), emit_plain_obs(&file_url)]).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        outcome.recipe.is_none(),
        "unexpressible chain: no draft ref"
    );
    assert_auto_repair_mentioned(&outcome, DRIFT_ID, true);
    assert!(
        !outcome
            .manifest
            .as_ref()
            .expect("manifest")
            .notes
            .iter()
            .any(|note| note.contains("draft saved:")),
        "no draft was saved"
    );
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_auto_repair_trace(&trace, DRIFT_ID, 1, "recipe_stale", "api");
    assert_eq!(service.registry().health(DRIFT_ID).consecutive_failures, 1);
}

// ---------------------------------------------------------------------------
// 9b. Repaired run falling back to builtin verification: direct file wins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_repair_can_fall_back_to_builtin_direct_file_verification() {
    // The drifting recipe matches any URL on the host, including a direct file;
    // the fallback then verifies that file in place — no exploration needed.
    let site = MockServer::start(|request| {
        if request.path == "/api/1" {
            return Reply::html("<!doctype html><html>maintenance</html>");
        }
        match request.path.as_str() {
            "/direct.bin" => match request.method.as_str() {
                "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
                _ => Reply::file(2048).header("accept-ranges", "bytes"),
            },
            _ => Reply::status(404),
        }
    })
    .await;
    let input_url = site.url("/direct.bin");
    let fixture = RecipeFixture::new("auto-repair-builtin");
    fixture.save(&host_only_drift_recipe(
        &site.url("/api/1"),
        &site.url("/never.bin"),
    ));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service
        .resolve(replay_request(&input_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.diagnostics.llm_calls, 0,
        "a direct-file fallback must not call the model"
    );
    assert_eq!(llm.request_count(), 0);
    assert!(
        outcome.recipe.is_none(),
        "the builtin fallback produces neither a recipe nor a draft"
    );
    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
    assert_eq!(manifest.resources[0].url, input_url);
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.to_ascii_lowercase().contains("auto-repair")),
        "notes must mention the repaired recipe: {:?}",
        manifest.notes
    );
    assert!(
        !manifest
            .notes
            .iter()
            .any(|note| note.contains("draft saved:")),
        "no draft was produced"
    );
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());

    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.status, "succeeded");
    assert_auto_repair_trace(&trace, "direct-drift", 1, "recipe_stale", "api");
    assert_eq!(
        trace.steps.len(),
        2,
        "recipe_replay + builtin_verify: {:?}",
        trace
            .steps
            .iter()
            .map(|step| step.op.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(trace.steps[1].op, "builtin_verify");
    assert_eq!(trace.steps[1].outcome, "ok");
    assert_eq!(
        service
            .registry()
            .health("direct-drift")
            .consecutive_failures,
        1,
        "the drift is still recorded for the matched method"
    );
}

// ---------------------------------------------------------------------------
// 10. Configured but unreachable AI: the repair attempt fails cleanly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_repair_with_unreachable_ai_reports_model_unavailable() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-unreachable-ai");
    fixture.save(&drift_recipe(&site.url("/api/42"), &file_url));
    let mut service = fixture.service().with_ai_config(unreachable_ai_config());

    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::ModelUnavailable)
    );
    assert!(outcome.diagnostics.llm_calls <= 1);
    let lower = outcome.diagnostics.summary.to_ascii_lowercase();
    assert!(
        lower.contains("auto-repair"),
        "{}",
        outcome.diagnostics.summary
    );
    assert!(lower.contains(DRIFT_ID), "{}", outcome.diagnostics.summary);
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());

    let trace = load_trace(&fixture, &outcome.run_id);
    assert_auto_repair_trace(&trace, DRIFT_ID, 1, "recipe_stale", "api");
    assert_eq!(trace.error_kind.as_deref(), Some("model_unavailable"));
    assert_eq!(service.registry().health(DRIFT_ID).consecutive_failures, 1);
}

// ---------------------------------------------------------------------------
// 11. Regression: Auto without a matching recipe still explores normally
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_without_a_matching_recipe_still_explores_without_repair_step() {
    let site = drift_site().await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-no-match-regression");
    // No recipes installed: the builtin/exploration path is used.
    let llm = MockLlm::start(vec![page_read(&page_url), emit_page_link(&file_url)]).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 2);
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_eq!(trace.steps[0].op, "builtin_verify");
    assert!(
        !trace.steps.iter().any(|step| step.op == "recipe_replay"),
        "no recipe matched, so there is no replay step"
    );
    // P3b: a state-backed successful exploration records its draft ref.
    assert_eq!(
        trace.recipe.as_ref().map(|recipe| recipe.state.as_str()),
        Some("draft"),
        "no matched recipe; only the generated draft ref may appear"
    );
}

// ---------------------------------------------------------------------------
// 12. Regression: a successful recipe replay never touches the model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recipe_success_does_not_trigger_auto_repair() {
    // The API path returns the expected JSON, so the recipe replays cleanly.
    let site = MockServer::start(|request| {
        let host = request.header("host").unwrap_or("127.0.0.1").to_owned();
        match request.path.as_str() {
            path if path.starts_with("/api/") => Reply::json(&format!(
                r#"{{"file":{{"url":"http://{host}/files/42.bin"}}}}"#
            )),
            "/files/42.bin" => match request.method.as_str() {
                "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
                _ => Reply::file(2048).header("accept-ranges", "bytes"),
            },
            _ => Reply::status(404),
        }
    })
    .await;
    let page_url = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("auto-repair-success-regression");
    fixture.save(&recipe_json(
        "healthy-recipe",
        1,
        json!({
            "hosts": ["127.0.0.1"],
            "url_pattern": r"^http://127\.0\.0\.1:\d+/page/\d+$",
            "priority": 5
        }),
        json!([{
            "id": "api",
            "op": "http_request",
            "args": { "url": site.url("/api/42") },
            "expect": { "content_type_prefix": ["application/json"] },
            "assign": { "body": "body_json" }
        }]),
        json!({ "url": file_url }),
    ));
    let llm = MockLlm::start(Vec::new()).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service
        .resolve(replay_request(&page_url, ResolveMode::Auto))
        .await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(llm.request_count(), 0);
    let trace = load_trace(&fixture, &outcome.run_id);
    assert_ne!(trace.steps[0].op, "builtin_verify");
    assert_ne!(trace.steps[0].op, "recipe_replay");
    assert!(trace.steps.iter().any(|step| step.op == "recipe_step"));
    assert_eq!(
        trace.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        Some("healthy-recipe")
    );
}
