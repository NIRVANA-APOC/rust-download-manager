//! Draft publish gates R1–R6 (`docs/architecture.md` §11.11): sample replay,
//! the evaluate report and the publish transaction (recipe file + index +
//! draft removal, with rollback on failure).
//!
//! Every gate test runs against local fixture servers on ephemeral loopback
//! ports; no public network and no model calls are involved.

mod common;
mod support;

use common::ScratchDir;
use rdm_resolve::{
    distill, draft_file_name, drafts_root, evaluate_publish, load_draft, publish, replay_sample,
    save_draft, Budget, CredentialsHint, DraftAction, DraftCandidate, PublishReport, RecipeDraft,
    RecipeRegistry, RecipeTest, ResolveMode, ResolvePolicy, ResolveRequest, ResolverService,
    ResourceItem, RunStatus, TransferHint, Verification, VerificationMethod, VerificationStatus,
};
use support::{MockServer, Reply};
use url::Url;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `/api/42` returns `{"file":{"url":"<host>/files/42.bin"}}`; the file path
/// serves a binary; anything else is a 404.
async fn gate_site() -> MockServer {
    MockServer::start(|request| {
        let host = request.header("host").unwrap_or("127.0.0.1").to_owned();
        match request.path.as_str() {
            "/api/42" => Reply::json(&format!(
                r#"{{"file":{{"url":"http://{host}/files/42.bin"}}}}"#
            )),
            "/files/42.bin" => match request.method.as_str() {
                "HEAD" => Reply::file_head(2048).header("accept-ranges", "bytes"),
                _ => Reply::file(2048).header("accept-ranges", "bytes"),
            },
            _ => Reply::status(404),
        }
    })
    .await
}

fn budget() -> Budget {
    Budget {
        wall_ms: 20_000,
        ..Budget::default()
    }
}

fn loopback_policy() -> ResolvePolicy {
    ResolvePolicy::loopback()
}

fn verified_item(url: &str, evidence: &str) -> ResourceItem {
    ResourceItem {
        resource_id: "res_1".to_owned(),
        url: url.to_owned(),
        final_url: Some(url.to_owned()),
        name: url.rsplit('/').next().map(str::to_owned),
        kind: None,
        content_type: Some("application/octet-stream".to_owned()),
        size_bytes: Some(2048),
        supports_ranges: Some(true),
        etag: None,
        last_modified: None,
        expected_sha256: None,
        credentials: CredentialsHint::default(),
        transfer: TransferHint::default(),
        verification: Verification {
            status: VerificationStatus::Verified,
            method: VerificationMethod::BoundedGet,
            http_status: Some(200),
            final_url: Some(url.to_owned()),
            content_type: Some("application/octet-stream".to_owned()),
            size_bytes: Some(2048),
            supports_ranges: Some(true),
            etag: None,
            last_modified: None,
            evidence: vec!["HEAD -> 200".to_owned()],
            checked_ms: 0,
        },
        evidence: vec![evidence.to_owned()],
        notes: Vec::new(),
    }
}

/// Distil the JSON-chain recipe used by the gate tests from a fixture site.
fn draft_from_site(site: &MockServer, source_url: &str) -> RecipeDraft {
    let api_url = site.url("/api/42");
    let file_url = site.url("/files/42.bin");
    let actions = vec![
        DraftAction::HttpRequest {
            method: "GET".to_owned(),
            url: api_url,
            max_bytes: 65536,
            obs_id: Some("obs_0_0".to_owned()),
        },
        DraftAction::JsonQuery {
            obs_id: "obs_0_0".to_owned(),
            pointer: "/file/url".to_owned(),
        },
        DraftAction::EmitManifest {
            candidates: vec![DraftCandidate {
                url: file_url.clone(),
                evidence: "obs_0_0#/file/url".to_owned(),
                name: None,
                one_time: false,
                single_stream: false,
            }],
        },
    ];
    let emitted = vec![verified_item(&file_url, "obs_0_0#/file/url")];
    let request = ResolveRequest {
        url: source_url.to_owned(),
        ..ResolveRequest::default()
    };
    distill(&request, "run_gate", 1, &actions, &emitted).expect("the JSON chain must distil")
}

fn gate<'a>(report: &'a PublishReport, id: &str) -> &'a rdm_resolve::GateResult {
    report
        .gates
        .iter()
        .find(|gate| gate.id == id)
        .unwrap_or_else(|| panic!("missing gate {id}: {:?}", report.gates))
}

fn gate_passed(report: &PublishReport, id: &str) -> bool {
    gate(report, id).pass
}

fn gate_ids(report: &PublishReport) -> Vec<&str> {
    report.gates.iter().map(|gate| gate.id.as_str()).collect()
}

// ---------------------------------------------------------------------------
// 1. replay_sample: matched + verified on the first sample
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_sample_matches_and_verifies_the_first_sample() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let file_url = site.url("/files/42.bin");
    let draft = draft_from_site(&site, &source);
    let policy = loopback_policy();

    let sample = replay_sample(&draft.recipe, &source, &policy, budget()).await;

    assert!(sample.matched, "the narrow draft must match its source URL");
    assert_eq!(sample.input_url, source);
    assert!(sample.error.is_none(), "unexpected error: {sample:?}");
    assert_eq!(sample.emitted_url.as_deref(), Some(file_url.as_str()));
    assert!(
        matches!(
            sample.verification,
            Some(status) if status.is_downloadable()
        ),
        "verification: {:?}",
        sample.verification
    );
    assert!(sample.is_downloadable());

    // R2: a second call uses a fresh context and still works.
    let again = replay_sample(&draft.recipe, &source, &policy, budget()).await;
    assert!(again.is_downloadable());
}

// ---------------------------------------------------------------------------
// 2. replay_sample: a non-matching URL is reported as unmatched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_sample_reports_unmatched_urls() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let draft = draft_from_site(&site, &source);

    let sample = replay_sample(
        &draft.recipe,
        &site.url("/page/other"),
        &loopback_policy(),
        budget(),
    )
    .await;

    assert!(!sample.matched);
    assert!(sample.emitted_url.is_none());
    assert!(sample.verification.is_none());
    assert!(sample.error.is_none());
    assert!(!sample.is_downloadable());
}

// ---------------------------------------------------------------------------
// 3. replay_sample: a server failure surfaces a typed step error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_sample_surfaces_step_errors() {
    let broken = MockServer::start(|_| Reply::status(404)).await;
    let source = broken.url("/page/42");
    let draft = draft_from_site(&broken, &source);

    let sample = replay_sample(&draft.recipe, &source, &loopback_policy(), budget()).await;

    assert!(sample.matched, "the URL matches the draft's narrow pattern");
    assert!(!sample.is_downloadable());
    assert!(
        sample.error.is_some(),
        "a 404 API step must be reported as a typed error: {sample:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. evaluate_publish: single sample → narrow scope, required gates pass
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_single_sample_is_required_and_narrow() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let draft = draft_from_site(&site, &source);

    let report = evaluate_publish(&draft, &[], &[], &loopback_policy(), budget()).await;

    assert_eq!(report.draft_id, draft.id);
    assert!(!report.published, "evaluate never publishes");
    assert_eq!(report.scope, "narrow");
    assert!(report.required_passed);
    assert!(report.recipe.is_none());
    assert!(report.error.is_none());
    assert_eq!(gate_ids(&report), vec!["R1", "R2", "R3", "R4", "R5", "R6"]);
    for id in ["R1", "R2", "R3", "R6"] {
        assert!(
            gate_passed(&report, id),
            "{id} must pass: {:?}",
            gate(&report, id)
        );
    }
    assert!(
        !gate_passed(&report, "R4"),
        "one sample is not fresh enough"
    );
    assert!(
        !gate_passed(&report, "R5"),
        "there is no negative test in an auto draft"
    );
}

// ---------------------------------------------------------------------------
// 5. two successful samples + a negative → R4/R5 pass → scoped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_two_samples_and_a_negative_scopes_to_scoped() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let mut draft = draft_from_site(&site, &source);
    // Broaden the matcher so two genuinely different pages can be replayed;
    // an auto draft is intentionally narrow (exact source URL only).
    draft.recipe.matcher.url_pattern = None;
    draft.recipe.matcher.hosts = vec!["127.0.0.1".to_owned()];
    let samples = vec![source.clone(), site.url("/page/43")];
    let negatives = vec!["http://example.invalid/negative".to_owned()];

    let report = evaluate_publish(&draft, &samples, &negatives, &loopback_policy(), budget()).await;

    assert!(gate_passed(&report, "R4"), "{:?}", gate(&report, "R4"));
    assert!(gate_passed(&report, "R5"), "{:?}", gate(&report, "R5"));
    assert!(report.required_passed);
    assert_eq!(report.scope, "scoped");
}

// ---------------------------------------------------------------------------
// 6. narrow draft: a distinct second sample cannot match → R4 fails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn narrow_draft_cannot_get_a_fresh_second_sample() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let draft = draft_from_site(&site, &source);
    let samples = vec![source.clone(), site.url("/page/43")];

    let report = evaluate_publish(&draft, &samples, &[], &loopback_policy(), budget()).await;

    assert!(
        !gate_passed(&report, "R4"),
        "a narrow draft only matches its exact source URL, so a fresh sample cannot replay"
    );
    assert_eq!(report.scope, "narrow");
    assert!(
        report.required_passed,
        "R3 still passes for the source sample"
    );
}

// ---------------------------------------------------------------------------
// 7. R3 fails when the first sample cannot be replayed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_fails_r3_when_the_first_sample_cannot_replay() {
    let broken = MockServer::start(|_| Reply::status(404)).await;
    let source = broken.url("/page/42");
    let draft = draft_from_site(&broken, &source);

    let report = evaluate_publish(&draft, &[], &[], &loopback_policy(), budget()).await;

    assert!(!gate_passed(&report, "R3"), "{:?}", gate(&report, "R3"));
    assert!(!report.required_passed);
    assert!(!report.published);
    assert_eq!(report.scope, "narrow");
}

// ---------------------------------------------------------------------------
// 8. R6 fails when the recipe has no assert step
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_requires_an_assert_step() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let mut draft = draft_from_site(&site, &source);
    draft.recipe.steps.retain(|step| step.op != "assert");
    draft.recipe.validate().expect("still a valid recipe");

    let report = evaluate_publish(&draft, &[], &[], &loopback_policy(), budget()).await;

    assert!(
        gate_passed(&report, "R3"),
        "the replay itself still verifies"
    );
    assert!(!gate_passed(&report, "R6"), "{:?}", gate(&report, "R6"));
    assert!(!report.required_passed);
}

// ---------------------------------------------------------------------------
// 9. R5 passes with an `expect_no_match` recipe test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_passes_r5_with_an_expect_no_match_test() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let mut draft = draft_from_site(&site, &source);
    draft.recipe.tests.push(RecipeTest {
        name: "negative".to_owned(),
        input_url: site.url("/page/negative"),
        expect_host_suffix: None,
        expect_no_match: true,
        skip: None,
    });

    let report = evaluate_publish(
        &draft,
        std::slice::from_ref(&source),
        &[],
        &loopback_policy(),
        budget(),
    )
    .await;

    assert!(gate_passed(&report, "R5"), "{:?}", gate(&report, "R5"));
    assert!(!gate_passed(&report, "R4"));
    assert!(report.required_passed);
    assert_eq!(report.scope, "narrow");
}

// ---------------------------------------------------------------------------
// 10. R5 fails when a `--negative` URL would match the recipe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_fails_r5_when_a_negative_url_matches() {
    let site = gate_site().await;
    let source = site.url("/page/42");
    let draft = draft_from_site(&site, &source);

    let report = evaluate_publish(
        &draft,
        std::slice::from_ref(&source),
        std::slice::from_ref(&source),
        &loopback_policy(),
        budget(),
    )
    .await;

    assert!(!gate_passed(&report, "R5"), "{:?}", gate(&report, "R5"));
    assert!(
        report.required_passed,
        "R5 only decides the scope, not publishability"
    );
    assert_eq!(report.scope, "narrow");
}

// ---------------------------------------------------------------------------
// 11. publish: recipe file + index + draft removal, then replay via service
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publish_writes_the_recipe_index_and_removes_the_draft() {
    let scratch = ScratchDir::new("gates-publish");
    let resolve_root = scratch.join("resolve");
    let site = gate_site().await;
    let source = site.url("/page/42");
    let draft = draft_from_site(&site, &source);
    save_draft(&resolve_root, &draft).expect("save draft");
    let draft_path = drafts_root(&resolve_root).join(draft_file_name(&draft));
    assert!(draft_path.is_file());

    let mut report = evaluate_publish(&draft, &[], &[], &loopback_policy(), budget()).await;
    assert!(report.required_passed);
    let mut registry = RecipeRegistry::with_root(resolve_root.join("recipes"));

    let published = publish(&mut registry, &resolve_root, &draft, &mut report)
        .expect("required gates passed: publish must succeed");

    assert_eq!(published.id, draft.id);
    assert_eq!(published.version, draft.version);
    assert_eq!(published.state, "published");
    assert!(report.published);
    assert!(report.recipe.is_some());
    assert!(report.error.is_none());

    // The recipe file, the index entry and the draft removal are all committed.
    assert!(resolve_root
        .join("recipes")
        .join(draft_file_name(&draft))
        .is_file());
    assert!(resolve_root.join("index.json").is_file());
    assert!(!draft_path.exists(), "a published draft is removed");

    let reloaded = RecipeRegistry::with_root(resolve_root.join("recipes"));
    assert_eq!(reloaded.active_version(&draft.id), Some(draft.version));
    let hit = reloaded
        .find_for_url(&Url::parse(&source).expect("url"))
        .expect("the published recipe must match its source URL");
    assert_eq!(hit.recipe.id, draft.id);

    // ...and the released recipe replays without any model call.
    let mut service = ResolverService::with_state_dir(scratch.path(), ResolvePolicy::loopback());
    let outcome = service
        .resolve(ResolveRequest {
            url: source.clone(),
            mode: ResolveMode::RecipeOnly,
            ..ResolveRequest::default()
        })
        .await;
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.diagnostics.llm_calls, 0);
    assert_eq!(
        outcome.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        Some(draft.id.as_str())
    );
}

// ---------------------------------------------------------------------------
// 12. publish refuses to write when a required gate failed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publish_refuses_when_required_gates_fail() {
    let scratch = ScratchDir::new("gates-refused");
    let resolve_root = scratch.join("resolve");
    let broken = MockServer::start(|_| Reply::status(404)).await;
    let source = broken.url("/page/42");
    let draft = draft_from_site(&broken, &source);
    save_draft(&resolve_root, &draft).expect("save draft");

    let mut report = evaluate_publish(&draft, &[], &[], &loopback_policy(), budget()).await;
    assert!(!report.required_passed);
    let mut registry = RecipeRegistry::with_root(resolve_root.join("recipes"));

    let error = publish(&mut registry, &resolve_root, &draft, &mut report)
        .expect_err("a failed R3 must block the publish");

    assert!(!report.published);
    assert!(report.error.is_some(), "publish must explain the refusal");
    assert!(!error.to_string().is_empty());
    assert!(drafts_root(&resolve_root)
        .join(draft_file_name(&draft))
        .is_file());
    assert!(registry.get(&draft.id, None).is_none());
    assert!(!resolve_root.join("index.json").exists());
}

// ---------------------------------------------------------------------------
// 13. evaluate_publish is side-effect free (dry-run semantics)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_publish_never_writes_the_library() {
    let scratch = ScratchDir::new("gates-dry-run");
    let resolve_root = scratch.join("resolve");
    let site = gate_site().await;
    let source = site.url("/page/42");
    let draft = draft_from_site(&site, &source);
    save_draft(&resolve_root, &draft).expect("save draft");

    let report = evaluate_publish(&draft, &[], &[], &loopback_policy(), budget()).await;
    assert!(report.required_passed);

    assert!(drafts_root(&resolve_root)
        .join(draft_file_name(&draft))
        .is_file());
    assert!(!resolve_root.join("recipes").exists(), "no recipe file yet");
    assert!(!resolve_root.join("index.json").exists(), "no index yet");
    assert_eq!(
        load_draft(&resolve_root, &draft.id).expect("draft").id,
        draft.id
    );
}

// ---------------------------------------------------------------------------
// 14. publish reports I/O failures and keeps the draft
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publish_io_failure_is_reported_and_rolled_back() {
    let scratch = ScratchDir::new("gates-io-failure");
    let resolve_root = scratch.join("resolve");
    let site = gate_site().await;
    let source = site.url("/page/42");
    let draft = draft_from_site(&site, &source);
    save_draft(&resolve_root, &draft).expect("save draft");

    // A regular file blocks the recipe directory creation.
    let blocked = scratch.join("blocked");
    std::fs::write(&blocked, b"this is a file, not a directory").expect("blocking file");
    let broken_recipes = blocked.join("resolve").join("recipes");
    let mut registry = RecipeRegistry::with_root(&broken_recipes);

    let mut report = evaluate_publish(&draft, &[], &[], &loopback_policy(), budget()).await;
    assert!(report.required_passed);
    let error = publish(&mut registry, &resolve_root, &draft, &mut report)
        .expect_err("an unwritable recipe root must fail the publish");

    assert!(!report.published);
    assert!(report.error.is_some(), "{error}");
    assert!(
        drafts_root(&resolve_root)
            .join(draft_file_name(&draft))
            .is_file(),
        "the draft survives a failed publish"
    );
    assert!(registry.get(&draft.id, None).is_none());
    assert!(blocked.is_file());
}
