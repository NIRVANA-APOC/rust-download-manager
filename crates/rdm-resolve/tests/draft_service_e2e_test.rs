//! End-to-end P3b closed loop (`docs/architecture.md` §11.11): a successful
//! exploration distils a local draft, the draft passes the R1–R6 publish gates
//! and, once published, the same URL replays deterministically without any
//! model call.
//!
//! The exploration provider is the shared scripted mock LLM
//! (`tests/common/llm.rs`); the target site is a local fixture server. No public
//! network is ever used.

mod common;
mod support;

use std::path::PathBuf;

use common::llm::{LlmReply, MockLlm};
use common::RecipeFixture;
use rdm_resolve::{
    drafts_root, evaluate_publish, list_drafts, load_draft, publish, Budget, RecipeRegistry,
    ResolveMode, ResolvePolicy, ResolveRequest, ResolverService, RunStatus, DRAFT_SCHEMA,
};
use serde_json::json;
use support::{MockServer, Reply};

const API_KEY: &str = "sk-p3b-scripted-key-0123456789";

// ---------------------------------------------------------------------------
// Scripted exploration harness
// ---------------------------------------------------------------------------

fn scrub_ambient_env_key() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::remove_var("RDM_AI_API_KEY"));
}

fn ai_config(llm: &MockLlm) -> rdm_resolve::AiConfig {
    scrub_ambient_env_key();
    rdm_resolve::AiConfig {
        enabled: true,
        base_url: llm.config_base_url(),
        model: "scripted-model".to_owned(),
        api_key: API_KEY.to_owned(),
        ..rdm_resolve::AiConfig::default()
    }
}

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

/// Emit one of the page's scanned links (`links/<index>`) through the newest
/// observation.
fn emit_page_link(file_url: impl Into<String>, link_index: usize) -> LlmReply {
    let file_url = file_url.into();
    LlmReply::dynamic(move |request| {
        let obs = first_obs_id(&request.body);
        json!({
            "action": "emit_manifest",
            "resources": [{
                "url": file_url,
                "evidence": format!("{obs}#links/{link_index}"),
            }]
        })
        .to_string()
    })
}

/// Emit a URL with a plain `obs_<id>` evidence handle.
fn emit_plain_obs(file_url: impl Into<String>) -> LlmReply {
    let file_url = file_url.into();
    LlmReply::dynamic(move |request| {
        let obs = first_obs_id(&request.body);
        json!({
            "action": "emit_manifest",
            "resources": [{ "url": file_url, "evidence": obs }]
        })
        .to_string()
    })
}

/// Fixture site: `/page` links `/files/42.bin` (index 0) and `/login` (index 1).
async fn share_site() -> MockServer {
    MockServer::start(|request| {
        let host = request.header("host").unwrap_or("127.0.0.1").to_owned();
        if request.path == "/page" {
            return Reply::html(&format!(
                "<!doctype html><html><body>\
                 <a href=\"http://{host}/files/42.bin\">Download</a>\
                 <a href=\"http://{host}/login\">Login</a>\
                 </body></html>"
            ));
        }
        match request.path.as_str() {
            "/files/42.bin" => match request.method.as_str() {
                "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
                _ => Reply::file(2048).header("accept-ranges", "bytes"),
            },
            "/login" => Reply::html("<!doctype html><html><body>sign in</body></html>"),
            _ => Reply::status(404),
        }
    })
    .await
}

fn explore_request(url: &str) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        mode: ResolveMode::Explore,
        budget: Some(Budget {
            wall_ms: 20_000,
            ..Budget::default()
        }),
        ..ResolveRequest::default()
    }
}

fn resolve_root(fixture: &RecipeFixture) -> PathBuf {
    fixture.state_dir().join("resolve")
}

fn draft_path(fixture: &RecipeFixture, id: &str, version: u32) -> PathBuf {
    drafts_root(&resolve_root(fixture)).join(format!("{id}@{version}.json"))
}

// ---------------------------------------------------------------------------
// 1. distill → publish → replay closed loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explore_distills_a_draft_that_publishes_and_replays_without_a_model() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let llm = MockLlm::start(vec![page_read(&page_url), emit_page_link(&file_url, 0)]).await;
    let fixture = RecipeFixture::new("p3b-e2e");
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url)).await;

    // --- exploration succeeded and produced a draft ---------------------
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 2);
    let manifest = outcome.manifest.as_ref().expect("manifest");
    assert_eq!(manifest.downloadable().count(), 1);
    let draft_ref = outcome.recipe.as_ref().expect("draft recipe ref");
    assert_eq!(draft_ref.state, "draft");
    assert!(draft_ref.id.starts_with("draft_"), "{}", draft_ref.id);
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.contains("draft saved:")),
        "manifest must point at the saved draft: {:?}",
        manifest.notes
    );

    let path = draft_path(&fixture, &draft_ref.id, draft_ref.version);
    assert!(path.is_file(), "missing draft file {path:?}");
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read draft"))
            .expect("draft is JSON");
    assert_eq!(raw["schema"], DRAFT_SCHEMA);
    assert_eq!(raw["id"], draft_ref.id);
    assert_eq!(raw["version"], draft_ref.version);
    assert_eq!(raw["recipe"]["schema"], "rdm.recipe/v1");
    let raw_text = std::fs::read_to_string(&path).expect("read draft");
    assert!(
        !raw_text.to_ascii_lowercase().contains("authorization")
            && !raw_text.to_ascii_lowercase().contains("cookie"),
        "draft leaked credentials: {raw_text}"
    );

    // --- the draft passes the publish gates -----------------------------
    let draft = load_draft(&resolve_root(&fixture), &draft_ref.id).expect("load draft");
    assert_eq!(draft.source_url, page_url);
    let report = evaluate_publish(
        &draft,
        std::slice::from_ref(&page_url),
        &[],
        &ResolvePolicy::loopback(),
        Budget {
            wall_ms: 20_000,
            ..Budget::default()
        },
    )
    .await;
    assert!(
        report.required_passed,
        "the distilled draft must pass R1/R2/R3/R6: {:?}",
        report.gates
    );
    assert_eq!(report.scope, "narrow");

    // --- publish commits it into the recipe library ---------------------
    let mut registry = RecipeRegistry::with_root(fixture.recipes_dir());
    let mut report = report;
    let published = publish(&mut registry, &resolve_root(&fixture), &draft, &mut report)
        .expect("publish must succeed");
    assert!(report.published);
    assert_eq!(published.state, "published");
    assert!(!path.exists(), "the published draft is removed");
    assert!(fixture
        .recipes_dir()
        .join(format!("{}@{}.json", draft_ref.id, draft_ref.version))
        .is_file());
    assert!(resolve_root(&fixture).join("index.json").is_file());
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());

    // --- the released recipe replays without any model call -------------
    let mut replay_service =
        ResolverService::with_state_dir(fixture.state_dir(), ResolvePolicy::loopback());
    let replayed = replay_service
        .resolve(ResolveRequest {
            url: page_url.clone(),
            mode: ResolveMode::RecipeOnly,
            ..ResolveRequest::default()
        })
        .await;
    assert_eq!(replayed.status, RunStatus::Succeeded);
    assert_eq!(
        replayed.diagnostics.llm_calls, 0,
        "replay must never call the model"
    );
    assert_eq!(
        replayed.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        Some(draft_ref.id.as_str())
    );
    assert_eq!(
        replayed.recipe.as_ref().map(|recipe| recipe.state.as_str()),
        Some("matched")
    );
}

// ---------------------------------------------------------------------------
// 2. Successful exploration with an unexpressible chain → no draft
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explore_success_without_an_expressible_chain_writes_no_draft() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    // A HEAD observation is evidence-valid but cannot be replayed as a recipe
    // step (draft distillation requires GET observations).
    let llm = MockLlm::start(vec![http_head(&file_url), emit_plain_obs(&file_url)]).await;
    let fixture = RecipeFixture::new("p3b-unexpressible");
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        outcome.recipe.is_none(),
        "an inexpressible action chain must not produce a draft: {:?}",
        outcome.recipe
    );
    assert!(
        !outcome
            .manifest
            .as_ref()
            .expect("manifest")
            .notes
            .iter()
            .any(|note| note.contains("draft saved:")),
        "no draft note when nothing was saved"
    );
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());
}

// ---------------------------------------------------------------------------
// 3. Failed exploration → no draft
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_exploration_writes_no_draft() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let llm = MockLlm::start(vec![finish("no download link here")]).await;
    let fixture = RecipeFixture::new("p3b-failed");
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url)).await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.recipe.is_none());
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());
}

// ---------------------------------------------------------------------------
// 4. Service without a state root never produces a draft
// ---------------------------------------------------------------------------

#[tokio::test]
async fn service_without_state_root_never_distils_a_draft() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let file_url = site.url("/files/42.bin");
    let llm = MockLlm::start(vec![page_read(&page_url), emit_page_link(&file_url, 0)]).await;
    let mut service = ResolverService::new(RecipeRegistry::empty(), ResolvePolicy::loopback())
        .with_ai_config(ai_config(&llm));
    assert!(service.trace_root().is_none());

    let outcome = service.resolve(explore_request(&page_url)).await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        outcome.recipe.is_none(),
        "a state-less service has nowhere to persist a draft"
    );
}

// ---------------------------------------------------------------------------
// 5. Partial exploration (no downloadable resource) → no draft
// ---------------------------------------------------------------------------

#[tokio::test]
async fn partial_exploration_writes_no_draft() {
    let site = share_site().await;
    let page_url = site.url("/page");
    let login_url = site.url("/login");
    // links/1 is the login page: evidence-valid, but not a downloadable file.
    let llm = MockLlm::start(vec![page_read(&page_url), emit_page_link(&login_url, 1)]).await;
    let fixture = RecipeFixture::new("p3b-partial");
    let mut service = fixture.service().with_ai_config(ai_config(&llm));

    let outcome = service.resolve(explore_request(&page_url)).await;

    assert_eq!(outcome.status, RunStatus::Partial);
    assert!(outcome.recipe.is_none(), "partial runs produce no draft");
    assert!(list_drafts(&resolve_root(&fixture)).is_empty());
}
