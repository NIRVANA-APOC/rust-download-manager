//! Unit tests for deterministic draft distillation, storage and the R1–R6
//! publish gates (`docs/architecture.md` §11.11).

use super::*;
use crate::contract::{
    Budget, CredentialsHint, ResolveMode, ResolveRequest, ResourceItem, TransferHint, Verification,
};
use crate::policy::ResolvePolicy;
use crate::recipe::RecipeTest;
use crate::registry::RecipeRegistry;
use crate::test_support::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn request_for(url: &str) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        mode: ResolveMode::Auto,
        ..ResolveRequest::default()
    }
}

fn verified_item(url: &str, evidence: &str) -> ResourceItem {
    ResourceItem {
        resource_id: "res_test".to_owned(),
        url: url.to_owned(),
        final_url: None,
        name: None,
        kind: None,
        content_type: None,
        size_bytes: None,
        supports_ranges: None,
        etag: None,
        last_modified: None,
        expected_sha256: None,
        credentials: CredentialsHint::default(),
        transfer: TransferHint::default(),
        verification: Verification {
            status: VerificationStatus::Verified,
            ..Verification::default()
        },
        evidence: vec![evidence.to_owned()],
        notes: Vec::new(),
    }
}

fn json_chain_actions() -> Vec<DraftAction> {
    vec![
        DraftAction::HttpRequest {
            method: "GET".to_owned(),
            url: "https://api.example.com/d/1".to_owned(),
            max_bytes: 65_536,
            obs_id: Some("obs_meta".to_owned()),
        },
        DraftAction::JsonQuery {
            obs_id: "obs_meta".to_owned(),
            pointer: "/file/url".to_owned(),
        },
        DraftAction::EmitManifest {
            candidates: vec![DraftCandidate {
                url: "https://cdn.example.com/f.bin".to_owned(),
                evidence: "obs_meta#/file/url".to_owned(),
                name: None,
                one_time: true,
                single_stream: true,
            }],
        },
    ]
}

fn distill_json_chain() -> RecipeDraft {
    let emitted = verified_item("https://cdn.example.com/f.bin", "obs_meta#/file/url");
    distill(
        &request_for("https://api.example.com/d/1"),
        "run_json",
        1_700_000_000_000,
        &json_chain_actions(),
        &[emitted],
    )
    .expect("JSON chain must distill")
}

async fn start_site() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fixture");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task_base = base.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let base = task_base.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut chunk = [0u8; 2048];
                while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => raw.extend_from_slice(&chunk[..read]),
                    }
                }
                let text = String::from_utf8_lossy(&raw).into_owned();
                let request_line = text.lines().next().unwrap_or_default().to_owned();
                let is_head = request_line.starts_with("HEAD ");
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_owned();
                let (status, content_type, body) = if path == "/page" {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        format!(
                            "<html><body><a href=\"{base}/files/demo.bin\">demo</a></body></html>"
                        )
                        .into_bytes(),
                    )
                } else if path == "/files/demo.bin" {
                    ("200 OK", "application/octet-stream", b"demo-bytes".to_vec())
                } else {
                    ("404 Not Found", "text/plain", b"not found".to_vec())
                };
                let mut response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                if !is_head {
                    response.extend_from_slice(&body);
                }
                let _ = stream.write_all(&response).await;
            });
        }
    });
    (base, task)
}

fn page_draft(base: &str) -> RecipeDraft {
    let page_url = format!("{base}/page");
    let file_url = format!("{base}/files/demo.bin");
    let actions = vec![DraftAction::PageRead {
        url: page_url.clone(),
        obs_id: Some("obs_page".to_owned()),
    }];
    let mut item = verified_item(&file_url, "obs_page#links/0");
    item.final_url = Some(file_url);
    distill(
        &request_for(&page_url),
        "run_page",
        1_700_000_100_000,
        &actions,
        &[item],
    )
    .expect("page link chain must distill")
}

fn draft_with_actions(page_url: &str, file_url: &str, evidence: &str) -> Option<RecipeDraft> {
    let actions = vec![DraftAction::PageRead {
        url: page_url.to_owned(),
        obs_id: Some("obs_page".to_owned()),
    }];
    let mut item = verified_item(file_url, evidence);
    item.final_url = Some(file_url.to_owned());
    distill(&request_for(page_url), "run_page", 1, &actions, &[item])
}

fn gate_pass(report: &PublishReport, id: &str) -> bool {
    report
        .gates
        .iter()
        .find(|gate| gate.id == id)
        .unwrap_or_else(|| panic!("missing gate {id}"))
        .pass
}

// ---------------------------------------------------------------------------
// Deterministic distillation
// ---------------------------------------------------------------------------

#[test]
fn distill_maps_a_json_pointer_chain() {
    let draft = distill_json_chain();
    assert_eq!(draft.schema, DRAFT_SCHEMA);
    assert_eq!(draft.version, DRAFT_RECIPE_VERSION);
    assert_eq!(draft.source_run_id, "run_json");
    assert_eq!(draft.source_url, "https://api.example.com/d/1");
    assert_eq!(draft.scope_hint, "narrow");
    assert!(
        draft.id.starts_with("draft_api.example.com_"),
        "{}",
        draft.id
    );
    assert!(draft
        .notes
        .iter()
        .any(|note| note.contains("deterministic")));

    let recipe = &draft.recipe;
    assert_eq!(recipe.id, draft.id);
    assert_eq!(recipe.version, DRAFT_RECIPE_VERSION);
    recipe.validate().expect("distilled recipe validates");
    assert_eq!(recipe.matcher.hosts, vec!["api.example.com".to_owned()]);
    assert_eq!(
        recipe.matcher.url_pattern.as_deref(),
        Some(format!("^{}$", regex::escape("https://api.example.com/d/1")).as_str())
    );
    assert_eq!(recipe.tests.len(), 1);
    assert_eq!(recipe.tests[0].name, "sample_input");
    assert_eq!(recipe.tests[0].input_url, "https://api.example.com/d/1");
    assert!(!recipe.tests[0].expect_no_match);

    let ops: Vec<&str> = recipe.steps.iter().map(|step| step.op.as_str()).collect();
    assert_eq!(ops, vec!["http_request", "json_query", "assert"]);
    assert_eq!(recipe.steps[0].args["method"], "GET");
    assert_eq!(recipe.steps[0].args["url"], "https://api.example.com/d/1");
    assert_eq!(recipe.steps[0].args["max_bytes"], 65_536);
    assert_eq!(recipe.steps[0].assign["o0"], "body_json");
    assert_eq!(recipe.steps[1].args["from"], "o0");
    assert_eq!(recipe.steps[1].args["pointer"], "/file/url");
    assert_eq!(recipe.steps[1].assign["v0"], "value");
    assert_eq!(recipe.steps[2].args["kind"], "matches");
    assert_eq!(recipe.steps[2].args["value"], "{{v0}}");
    assert_eq!(recipe.steps[2].args["pattern"], "^https?://");

    assert_eq!(recipe.emit.url, "{{v0}}");
    assert_eq!(recipe.emit.name.as_deref(), Some("f.bin"));
    assert!(recipe.emit.one_time);
    assert!(recipe.emit.single_stream);
}

#[test]
fn distill_maps_a_page_link_chain() {
    let draft = draft_with_actions(
        "https://example.com/page",
        "https://cdn.example.com/files/demo.bin",
        "obs_page#links/1",
    )
    .expect("link chain must distill");
    let recipe = &draft.recipe;
    let ops: Vec<&str> = recipe.steps.iter().map(|step| step.op.as_str()).collect();
    assert_eq!(ops, vec!["http_request", "regex_capture", "assert"]);
    assert_eq!(recipe.steps[0].assign["o0"], "body_text");
    assert_eq!(recipe.steps[0].args["method"], "GET");
    assert_eq!(recipe.steps[0].args["url"], "https://example.com/page");
    assert_eq!(
        recipe.steps[1].args["pattern"],
        regex::escape("https://cdn.example.com/files/demo.bin")
    );
    assert_eq!(recipe.steps[1].args["group"], 0);
    assert_eq!(recipe.steps[1].args["from"], "o0");
    assert_eq!(recipe.steps[1].assign["v0"], "value");
    assert_eq!(recipe.steps[2].args["value"], "{{v0}}");
    assert_eq!(recipe.emit.url, "{{v0}}");
    assert_eq!(recipe.emit.name.as_deref(), Some("demo.bin"));
    assert!(!recipe.emit.one_time);
    assert!(!recipe.emit.single_stream);
}

#[test]
fn distill_plain_observation_emits_a_literal_url() {
    let actions = vec![DraftAction::HttpRequest {
        method: "GET".to_owned(),
        url: "https://cdn.example.com/f.bin".to_owned(),
        max_bytes: 1024,
        obs_id: Some("obs_file".to_owned()),
    }];
    let emitted = verified_item("https://cdn.example.com/f.bin", "obs_file");
    let draft = distill(
        &request_for("https://cdn.example.com/f.bin"),
        "run_plain",
        1,
        &actions,
        &[emitted],
    )
    .expect("plain observation must distill");
    assert_eq!(draft.recipe.steps.len(), 1);
    assert_eq!(draft.recipe.steps[0].op, "assert");
    assert_eq!(
        draft.recipe.steps[0].args["value"],
        "https://cdn.example.com/f.bin"
    );
    assert_eq!(draft.recipe.emit.url, "https://cdn.example.com/f.bin");
}

#[test]
fn distill_rejects_unexpressible_chains() {
    // No downloadable emitted resource.
    let mut not_verified = verified_item("https://cdn.example.com/f.bin", "obs_meta#/file/url");
    not_verified.verification.status = VerificationStatus::HtmlPage;
    assert!(distill(
        &request_for("https://api.example.com/d/1"),
        "run",
        1,
        &json_chain_actions(),
        &[not_verified],
    )
    .is_none());

    // No observation evidence handle at all.
    let mut no_evidence = verified_item("https://cdn.example.com/f.bin", "");
    no_evidence.evidence.clear();
    assert!(distill(
        &request_for("https://api.example.com/d/1"),
        "run",
        1,
        &json_chain_actions(),
        &[no_evidence],
    )
    .is_none());

    // Evidence names an observation the run never recorded.
    assert!(distill(
        &request_for("https://api.example.com/d/1"),
        "run",
        1,
        &json_chain_actions(),
        &[verified_item(
            "https://cdn.example.com/f.bin",
            "obs_missing#/file/url"
        )],
    )
    .is_none());

    // Plain observation whose request URL differs from the emitted URL.
    assert!(draft_with_actions(
        "https://example.com/page",
        "https://cdn.example.com/files/demo.bin",
        "obs_page",
    )
    .is_none());

    // Non-GET observation cannot be replayed by body-extraction steps.
    let head_actions = vec![DraftAction::HttpRequest {
        method: "HEAD".to_owned(),
        url: "https://api.example.com/d/1".to_owned(),
        max_bytes: 1024,
        obs_id: Some("obs_meta".to_owned()),
    }];
    assert!(distill(
        &request_for("https://api.example.com/d/1"),
        "run",
        1,
        &head_actions,
        &[verified_item(
            "https://cdn.example.com/f.bin",
            "obs_meta#/file/url"
        )],
    )
    .is_none());

    // Invalid input URL.
    assert!(distill(
        &request_for("not a url"),
        "run",
        1,
        &json_chain_actions(),
        &[verified_item(
            "https://cdn.example.com/f.bin",
            "obs_meta#/file/url"
        )],
    )
    .is_none());
}

#[test]
fn distill_output_stays_within_the_step_cap() {
    assert_eq!(MAX_DRAFT_STEPS, 24);
    for draft in [distill_json_chain(), page_draft("http://127.0.0.1:1")] {
        assert!(draft.recipe.steps.len() <= MAX_DRAFT_STEPS);
    }
}

#[test]
fn draft_json_never_carries_credentials_or_headers() {
    let draft = distill_json_chain();
    let json = serde_json::to_string(&draft).expect("serialize draft");
    let lower = json.to_ascii_lowercase();
    assert!(!lower.contains("authorization"), "{json}");
    assert!(!lower.contains("cookie"), "{json}");
    assert!(!lower.contains("proxy-authorization"), "{json}");
    // DraftAction never captures headers, so no recipe step can carry one.
    for step in &draft.recipe.steps {
        assert!(step.args.get("headers").is_none(), "{:?}", step.args);
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[test]
fn drafts_round_trip_save_list_load_and_remove() {
    let dir = TempDir::new("draft-storage");
    let resolve_root = dir.path();
    let mut older = distill_json_chain();
    older.created_ms = 100;
    let mut newer = draft_with_actions(
        "https://example.com/page",
        "https://cdn.example.com/files/demo.bin",
        "obs_page#links/1",
    )
    .expect("second draft");
    newer.created_ms = 200;
    newer.id = "draft_example.com_22222222".to_owned();
    newer.recipe.id = newer.id.clone();

    let older_path = save_draft(resolve_root, &older).expect("save older");
    assert_eq!(
        older_path.file_name().and_then(|name| name.to_str()),
        Some(draft_file_name(&older).as_str())
    );
    save_draft(resolve_root, &newer).expect("save newer");
    // A second version of the same draft id.
    let mut v2 = older.clone();
    v2.version = 2;
    v2.recipe.version = 2;
    save_draft(resolve_root, &v2).expect("save v2");

    let listed = list_drafts(resolve_root);
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!(listed[0].created_ms, 100);
    assert_eq!(listed[1].created_ms, 100);
    assert_eq!(listed[2].created_ms, 200);

    let loaded = load_draft(resolve_root, &older.id).expect("load");
    assert_eq!(loaded.version, 2, "load_draft picks the highest version");
    assert_eq!(loaded.recipe.version, 2);
    assert!(load_draft(resolve_root, "draft_missing").is_err());

    remove_draft(resolve_root, &older.id).expect("remove");
    assert!(load_draft(resolve_root, &older.id).is_err());
    assert_eq!(list_drafts(resolve_root).len(), 1);
    remove_draft(resolve_root, &older.id).expect("remove is idempotent");
}

#[test]
fn list_drafts_skips_corrupt_files() {
    let dir = TempDir::new("draft-corrupt");
    let resolve_root = dir.path();
    let draft = distill_json_chain();
    save_draft(resolve_root, &draft).expect("save");
    let root = drafts_root(resolve_root);
    std::fs::write(root.join("broken@1.json"), "{ not json").expect("write broken");
    std::fs::write(root.join("ignored.txt"), "not a draft").expect("write ignored");
    let listed = list_drafts(resolve_root);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, draft.id);
}

// ---------------------------------------------------------------------------
// Replay + R1–R6 gates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_sample_reports_unmatched_invalid_and_network_failures() {
    let recipe = distill_json_chain().recipe;
    let policy = ResolvePolicy::loopback();

    let unmatched = replay_sample(
        &recipe,
        "https://other.example.com/x",
        &policy,
        Budget::default(),
    )
    .await;
    assert!(!unmatched.matched);
    assert!(unmatched.error.is_none());
    assert!(unmatched.verification.is_none());

    let invalid = replay_sample(&recipe, "not a url", &policy, Budget::default()).await;
    assert!(!invalid.matched);
    assert_eq!(invalid.error, Some(ResolveErrorKind::InvalidUrl));

    let mut broken = distill_json_chain().recipe;
    broken.steps[0].args["url"] = serde_json::json!("http://127.0.0.1:1/api");
    let failed = replay_sample(
        &broken,
        "https://api.example.com/d/1",
        &policy,
        Budget::default(),
    )
    .await;
    assert!(failed.matched);
    assert_eq!(failed.error, Some(ResolveErrorKind::NetworkError));
    assert!(failed.verification.is_none());
}

#[tokio::test]
async fn evaluate_publish_required_gates_pass_but_scope_is_narrow() {
    let (base, task) = start_site().await;
    let draft = page_draft(&base);
    let report = evaluate_publish(
        &draft,
        &[],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    task.abort();

    assert!(!report.published);
    assert!(report.error.is_none());
    assert_eq!(report.draft_id, draft.id);
    let ids: Vec<&str> = report.gates.iter().map(|gate| gate.id.as_str()).collect();
    assert_eq!(ids, vec!["R1", "R2", "R3", "R4", "R5", "R6"]);
    assert!(gate_pass(&report, "R1"));
    assert!(gate_pass(&report, "R2"));
    assert!(gate_pass(&report, "R3"), "{:?}", report.gates);
    assert!(gate_pass(&report, "R6"));
    assert!(
        !gate_pass(&report, "R4"),
        "only the source sample is replayed"
    );
    assert!(
        !gate_pass(&report, "R5"),
        "no negatives and no expect_no_match test"
    );
    assert!(report.required_passed);
    assert_eq!(report.scope, "narrow");
}

#[tokio::test]
async fn evaluate_publish_scopes_with_fresh_and_negative_samples() {
    let (base, task) = start_site().await;
    let draft = page_draft(&base);
    let source = draft.source_url.clone();
    let report = evaluate_publish(
        &draft,
        &[source.clone(), source],
        &[format!("{base}/other")],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    task.abort();

    assert!(gate_pass(&report, "R3"), "{:?}", report.gates);
    assert!(gate_pass(&report, "R4"), "two fresh samples both verify");
    assert!(gate_pass(&report, "R5"), "negative sample does not match");
    assert!(report.required_passed);
    assert_eq!(report.scope, "scoped");
}

#[tokio::test]
async fn evaluate_publish_fails_r3_for_a_non_matching_sample() {
    let (base, task) = start_site().await;
    let draft = page_draft(&base);
    let report = evaluate_publish(
        &draft,
        &["http://127.0.0.1:1/other".to_owned()],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    task.abort();

    assert!(!gate_pass(&report, "R3"));
    assert!(!report.required_passed);
    assert_eq!(report.scope, "narrow");
}

#[tokio::test]
async fn evaluate_publish_fails_r6_without_an_assert_step() {
    let (base, task) = start_site().await;
    let mut draft = page_draft(&base);
    draft.recipe.steps.retain(|step| step.op != "assert");
    let report = evaluate_publish(
        &draft,
        &[],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    task.abort();

    assert!(!gate_pass(&report, "R6"));
    assert!(!report.required_passed);
}

#[tokio::test]
async fn evaluate_publish_uses_recipe_tests_for_r5_fallback() {
    let (base, task) = start_site().await;
    let mut draft = page_draft(&base);
    draft.recipe.tests.push(RecipeTest {
        name: "must-not-match".to_owned(),
        input_url: format!("{base}/other"),
        expect_host_suffix: None,
        expect_no_match: true,
        skip: None,
    });
    let report = evaluate_publish(
        &draft,
        &[],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    task.abort();

    assert!(gate_pass(&report, "R5"), "{:?}", report.gates);
    assert!(!gate_pass(&report, "R4"));
    assert_eq!(report.scope, "narrow");
}

// ---------------------------------------------------------------------------
// Publish
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publish_writes_the_library_and_removes_the_draft() {
    let (base, task) = start_site().await;
    let dir = TempDir::new("draft-publish");
    let resolve_root = dir.path().join("resolve");
    let draft = page_draft(&base);
    save_draft(&resolve_root, &draft).expect("save draft");
    let mut registry = RecipeRegistry::with_root(resolve_root.join("recipes"));

    let mut report = evaluate_publish(
        &draft,
        &[],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    assert!(report.required_passed);
    let reference =
        publish(&mut registry, &resolve_root, &draft, &mut report).expect("publish succeeds");
    assert_eq!(reference.id, draft.id);
    assert_eq!(reference.version, 1);
    assert_eq!(reference.state, "published");
    assert!(report.published);
    assert_eq!(
        report.recipe.as_ref().map(|recipe| recipe.id.as_str()),
        Some(draft.id.as_str())
    );

    assert!(resolve_root
        .join("recipes")
        .join(format!("{}@1.json", draft.id))
        .is_file());
    assert_eq!(
        registry.get(&draft.id, None).map(|recipe| recipe.version),
        Some(1)
    );
    assert_eq!(registry.active_version(&draft.id), Some(1));
    assert!(
        load_draft(&resolve_root, &draft.id).is_err(),
        "published draft file must be removed"
    );
    // Registry index was persisted.
    assert!(resolve_root.join("index.json").is_file());

    // Idempotent: publishing the same draft again succeeds without duplicates.
    let mut second = evaluate_publish(
        &draft,
        &[],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    let again =
        publish(&mut registry, &resolve_root, &draft, &mut second).expect("publish is idempotent");
    assert_eq!(again.id, draft.id);
    task.abort();
}

#[tokio::test]
async fn publish_refuses_when_required_gates_failed() {
    let (base, task) = start_site().await;
    let dir = TempDir::new("draft-publish-refused");
    let resolve_root = dir.path().join("resolve");
    let draft = page_draft(&base);
    save_draft(&resolve_root, &draft).expect("save draft");
    let mut registry = RecipeRegistry::with_root(resolve_root.join("recipes"));

    let mut report = evaluate_publish(
        &draft,
        &["http://127.0.0.1:1/other".to_owned()],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    assert!(!report.required_passed);
    let error = publish(&mut registry, &resolve_root, &draft, &mut report)
        .expect_err("publish must be refused");
    task.abort();

    assert!(error.to_string().contains("required"));
    assert!(!report.published);
    assert!(report.error.is_some());
    assert!(registry.get(&draft.id, None).is_none());
    assert!(
        load_draft(&resolve_root, &draft.id).is_ok(),
        "draft is kept"
    );
}

#[tokio::test]
async fn publish_reports_registry_write_failures() {
    let (base, task) = start_site().await;
    let dir = TempDir::new("draft-publish-failure");
    let resolve_root = dir.path().join("resolve");
    let draft = page_draft(&base);
    save_draft(&resolve_root, &draft).expect("save draft");
    let mut registry = RecipeRegistry::empty();

    let mut report = evaluate_publish(
        &draft,
        &[],
        &[],
        &ResolvePolicy::loopback(),
        Budget::default(),
    )
    .await;
    task.abort();
    assert!(report.required_passed);
    let error =
        publish(&mut registry, &resolve_root, &draft, &mut report).expect_err("no registry root");
    assert!(error.to_string().contains("no root"), "{error}");
    assert!(!report.published);
    assert!(report
        .error
        .as_deref()
        .is_some_and(|error| error.contains("cannot save recipe file")));
    assert!(load_draft(&resolve_root, &draft.id).is_ok());
}

#[test]
fn publish_report_and_gate_serialize_for_the_cli_contract() {
    let report = PublishReport {
        draft_id: "draft_example.com_ab12cd34".to_owned(),
        published: false,
        required_passed: true,
        scope: "narrow".to_owned(),
        gates: vec![GateResult {
            id: "R1".to_owned(),
            name: "no model calls".to_owned(),
            pass: true,
            detail: "structural".to_owned(),
        }],
        recipe: None,
        error: None,
    };
    let json = serde_json::to_value(&report).expect("serialize report");
    for key in [
        "draft_id",
        "published",
        "required_passed",
        "scope",
        "gates",
        "recipe",
        "error",
    ] {
        assert!(json.get(key).is_some(), "missing {key}: {json}");
    }
    assert_eq!(json["scope"], "narrow");
    assert_eq!(json["gates"][0]["id"], "R1");
    let round_tripped: PublishReport = serde_json::from_value(json).expect("deserialize report");
    assert_eq!(round_tripped.draft_id, report.draft_id);
}

#[test]
fn draft_file_name_is_filesystem_safe() {
    let draft = distill_json_chain();
    let name = draft_file_name(&draft);
    assert!(name.ends_with("@1.json"), "{name}");
    assert!(name.starts_with("draft_api.example.com_"), "{name}");
    let mut weird = draft;
    weird.id = "draft_a/b:c*?.json".to_owned();
    let name = draft_file_name(&weird);
    assert!(!name.contains('/'), "{name}");
    assert!(!name.contains(':'), "{name}");
    assert!(!name.contains('*'), "{name}");
    assert!(!name.contains('?'), "{name}");
}
