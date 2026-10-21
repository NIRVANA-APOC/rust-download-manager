//! Draft distillation and storage (`docs/architecture.md` §11.11, schema
//! `rdm.recipe-draft/v1`).
//!
//! All tests are pure/of-fline: `distill` takes the recorded exploration
//! actions plus the verified emitted resources and must produce a deterministic
//! replayable recipe (or `None` when the chain is not expressible). Storage is
//! exercised against an isolated scratch directory under Cargo's target tmp dir.

mod common;

use common::ScratchDir;
use rdm_resolve::{
    distill, draft_file_name, drafts_root, list_drafts, load_draft, remove_draft, save_draft,
    CredentialsHint, DraftAction, DraftCandidate, RecipeDraft, ResolveRequest, ResourceItem,
    TransferHint, Verification, VerificationMethod, VerificationStatus, DRAFT_RECIPE_VERSION,
    DRAFT_SCHEMA, MAX_DRAFT_STEPS, RECIPE_SCHEMA,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PAGE_URL: &str = "http://127.0.0.1:8080/page/42";
const API_URL: &str = "http://127.0.0.1:8080/api/42";
const FILE_URL: &str = "http://127.0.0.1:8080/files/42.bin";

fn request(url: &str) -> ResolveRequest {
    ResolveRequest {
        url: url.to_owned(),
        ..ResolveRequest::default()
    }
}

fn file_name(url: &str) -> String {
    url.rsplit('/').next().unwrap_or("file.bin").to_owned()
}

fn item(url: &str, status: VerificationStatus, evidence: &str) -> ResourceItem {
    ResourceItem {
        resource_id: "res_1".to_owned(),
        url: url.to_owned(),
        final_url: Some(url.to_owned()),
        name: Some(file_name(url)),
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
            status,
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

fn verified_item(url: &str, evidence: &str) -> ResourceItem {
    item(url, VerificationStatus::Verified, evidence)
}

fn candidate(url: &str, evidence: &str) -> DraftCandidate {
    DraftCandidate {
        url: url.to_owned(),
        evidence: evidence.to_owned(),
        name: None,
        one_time: false,
        single_stream: false,
    }
}

/// JSON chain: `http_request GET api` → `json_query obs#/data/url` → emit
/// `obs#/data/url`.
fn json_chain() -> (ResolveRequest, Vec<DraftAction>, Vec<ResourceItem>) {
    (
        request(PAGE_URL),
        vec![
            DraftAction::HttpRequest {
                method: "GET".to_owned(),
                url: API_URL.to_owned(),
                max_bytes: 65536,
                obs_id: Some("obs_1_0".to_owned()),
            },
            DraftAction::JsonQuery {
                obs_id: "obs_1_0".to_owned(),
                pointer: "/data/url".to_owned(),
            },
            DraftAction::EmitManifest {
                candidates: vec![candidate(FILE_URL, "obs_1_0#/data/url")],
            },
        ],
        vec![verified_item(FILE_URL, "obs_1_0#/data/url")],
    )
}

fn json_chain_draft() -> RecipeDraft {
    let (request, actions, emitted) = json_chain();
    distill(&request, "run_1_0", 1_700_000_000_000, &actions, &emitted)
        .expect("the JSON chain must be expressible")
}

fn html_chain_draft() -> RecipeDraft {
    let actions = vec![
        DraftAction::PageRead {
            url: PAGE_URL.to_owned(),
            obs_id: Some("obs_2_0".to_owned()),
        },
        DraftAction::EmitManifest {
            candidates: vec![candidate(FILE_URL, "obs_2_0#links/1")],
        },
    ];
    let emitted = vec![verified_item(FILE_URL, "obs_2_0#links/1")];
    distill(&request(PAGE_URL), "run_2_0", 2, &actions, &emitted)
        .expect("the HTML links chain must be expressible")
}

fn step_ops(draft: &RecipeDraft) -> Vec<&str> {
    draft
        .recipe
        .steps
        .iter()
        .map(|step| step.op.as_str())
        .collect()
}

fn step_ids(draft: &RecipeDraft) -> Vec<&str> {
    draft
        .recipe
        .steps
        .iter()
        .map(|step| step.id.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. JSON chain → http_request/json_query/assert recipe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distill_json_chain_builds_the_expected_recipe() {
    let draft = json_chain_draft();

    assert_eq!(draft.schema, DRAFT_SCHEMA);
    assert!(draft.id.starts_with("draft_"), "id: {}", draft.id);
    assert_eq!(draft.version, DRAFT_RECIPE_VERSION);
    assert_eq!(draft.source_url, PAGE_URL);
    assert_eq!(draft.source_run_id, "run_1_0");
    assert_eq!(draft.created_ms, 1_700_000_000_000);
    assert_eq!(draft.scope_hint, "narrow");

    let recipe = &draft.recipe;
    assert_eq!(recipe.schema, RECIPE_SCHEMA);
    assert_eq!(recipe.id, draft.id);
    assert_eq!(recipe.version, draft.version);
    assert_eq!(recipe.matcher.hosts, vec!["127.0.0.1".to_owned()]);
    let expected_pattern = format!("^{}$", regex::escape(PAGE_URL));
    assert_eq!(
        recipe.matcher.url_pattern.as_deref(),
        Some(expected_pattern.as_str())
    );
    assert_eq!(recipe.matcher.priority, 0);

    assert_eq!(
        step_ops(&draft),
        vec!["http_request", "json_query", "assert"]
    );
    assert_eq!(step_ids(&draft), vec!["s0", "s1", "s2"]);

    let http = &recipe.steps[0];
    assert_eq!(http.args["method"], "GET");
    assert_eq!(http.args["url"], API_URL);
    assert_eq!(http.args["max_bytes"], 65536);
    assert_eq!(http.assign.get("o0").map(String::as_str), Some("body_json"));

    let query = &recipe.steps[1];
    assert_eq!(query.args["from"], "o0");
    assert_eq!(query.args["pointer"], "/data/url");
    assert_eq!(query.assign.get("v0").map(String::as_str), Some("value"));

    let assert_step = &recipe.steps[2];
    assert_eq!(assert_step.args["kind"], "matches");
    assert_eq!(assert_step.args["value"], "{{v0}}");
    assert_eq!(assert_step.args["pattern"], "^https?://");

    assert_eq!(recipe.emit.url, "{{v0}}");
    assert_eq!(recipe.emit.name.as_deref(), Some("42.bin"));
    assert!(!recipe.emit.one_time);
    assert!(!recipe.emit.single_stream);

    assert_eq!(recipe.tests.len(), 1);
    assert_eq!(recipe.tests[0].name, "sample_input");
    assert_eq!(recipe.tests[0].input_url, PAGE_URL);
    assert!(!recipe.tests[0].expect_no_match);

    recipe.validate().expect("distilled recipe must validate");
    let encoded = serde_json::to_value(&draft).expect("draft serializes");
    let decoded: RecipeDraft = serde_json::from_value(encoded).expect("draft round-trips");
    assert_eq!(
        serde_json::to_value(&decoded).expect("json"),
        serde_json::to_value(&draft).expect("json")
    );

    for needle in [
        "deterministic draft",
        "narrow match",
        "headers and bodies are not recorded",
        "R1-R6",
    ] {
        assert!(
            draft.notes.iter().any(|note| note.contains(needle)),
            "missing note {needle:?}: {:?}",
            draft.notes
        );
    }
}

// ---------------------------------------------------------------------------
// 2. HTML links chain → http_request/regex_capture/assert recipe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distill_html_links_chain_builds_a_regex_capture_recipe() {
    let draft = html_chain_draft();
    assert_eq!(draft.recipe.matcher.hosts, vec!["127.0.0.1".to_owned()]);
    assert_eq!(
        step_ops(&draft),
        vec!["http_request", "regex_capture", "assert"]
    );

    let fetch = &draft.recipe.steps[0];
    assert_eq!(fetch.args["method"], "GET");
    assert_eq!(fetch.args["url"], PAGE_URL);
    // page_read records a bounded 256 KiB observation.
    assert_eq!(fetch.args["max_bytes"], 262_144);
    assert_eq!(
        fetch.assign.get("o0").map(String::as_str),
        Some("body_text")
    );

    let capture = &draft.recipe.steps[1];
    assert_eq!(capture.args["from"], "o0");
    assert_eq!(capture.args["pattern"], regex::escape(FILE_URL));
    assert_eq!(capture.args["group"], 0);
    assert_eq!(capture.assign.get("v0").map(String::as_str), Some("value"));

    assert_eq!(draft.recipe.steps[2].args["value"], "{{v0}}");
    assert_eq!(draft.recipe.emit.url, "{{v0}}");
    assert_eq!(draft.recipe.emit.name.as_deref(), Some("42.bin"));
    draft
        .recipe
        .validate()
        .expect("distilled links recipe must validate");
}

// ---------------------------------------------------------------------------
// 3. Plain observation evidence → literal emit with a single assert step
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distill_plain_observation_uses_a_literal_emit() {
    let actions = vec![
        DraftAction::HttpRequest {
            method: "GET".to_owned(),
            url: FILE_URL.to_owned(),
            max_bytes: 262_144,
            obs_id: Some("obs_3_0".to_owned()),
        },
        DraftAction::EmitManifest {
            candidates: vec![candidate(FILE_URL, "obs_3_0")],
        },
    ];
    let emitted = vec![verified_item(FILE_URL, "obs_3_0")];
    let draft = distill(&request(PAGE_URL), "run_3_0", 3, &actions, &emitted)
        .expect("a plain observation is expressible");

    assert_eq!(step_ops(&draft), vec!["assert"]);
    let assert_step = &draft.recipe.steps[0];
    assert_eq!(assert_step.args["kind"], "matches");
    assert_eq!(assert_step.args["value"], FILE_URL);
    assert_eq!(assert_step.args["pattern"], "^https?://");
    assert_eq!(
        draft.recipe.emit.url, FILE_URL,
        "a literal URL is emitted as-is (no template variable)"
    );
}

// ---------------------------------------------------------------------------
// 4. Inexpressible chains produce `None`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distill_without_evidence_is_not_expressible() {
    let (request, actions, mut emitted) = json_chain();
    // The emitted resource is the authority for the observation handle.
    emitted[0].evidence.clear();
    assert!(distill(&request, "run_x", 1, &actions, &emitted).is_none());

    let (request, actions, mut emitted) = json_chain();
    emitted[0].evidence = vec!["not-an-obs-handle".to_owned()];
    assert!(distill(&request, "run_x", 1, &actions, &emitted).is_none());
}

#[tokio::test]
async fn distill_with_unknown_observation_is_not_expressible() {
    let (request, actions, mut emitted) = json_chain();
    emitted[0].evidence = vec!["obs_999_0#/data/url".to_owned()];
    assert!(distill(&request, "run_x", 1, &actions, &emitted).is_none());
}

#[tokio::test]
async fn distill_without_a_verified_resource_is_not_expressible() {
    let (request, actions, _) = json_chain();
    // A page candidate is not a downloadable resource: no draft.
    let page = item(FILE_URL, VerificationStatus::HtmlPage, "obs_1_0#/data/url");
    assert!(distill(&request, "run_x", 1, &actions, &[page]).is_none());
    // An empty manifest is not a success either.
    assert!(distill(&request, "run_x", 1, &actions, &[]).is_none());
}

#[tokio::test]
async fn distill_respects_the_step_cap() {
    assert_eq!(MAX_DRAFT_STEPS, 24);
    let draft = json_chain_draft();
    assert!(draft.recipe.steps.len() <= MAX_DRAFT_STEPS);
    // A defensive upper bound: even a busy action log cannot exceed the cap
    // (either it distils a bounded recipe or gives up).
    let mut busy = Vec::new();
    for index in 0..40 {
        busy.push(DraftAction::JsonQuery {
            obs_id: format!("obs_{index}_0"),
            pointer: format!("/item/{index}"),
        });
    }
    let (request, _, emitted) = json_chain();
    if let Some(draft) = distill(&request, "run_busy", 1, &busy, &emitted) {
        assert!(draft.recipe.steps.len() <= MAX_DRAFT_STEPS);
    }
}

// ---------------------------------------------------------------------------
// 5. Draft storage: save/list/load/remove under `<resolve>/drafts/`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn draft_storage_round_trip() {
    let scratch = ScratchDir::new("draft-storage");
    let resolve_root = scratch.join("resolve");
    assert_eq!(
        drafts_root(&resolve_root),
        resolve_root.join("drafts"),
        "drafts live next to the recipe library"
    );

    let draft = json_chain_draft();
    assert_eq!(
        draft_file_name(&draft),
        format!("{}@{}.json", draft.id, draft.version)
    );

    let path = save_draft(&resolve_root, &draft).expect("save draft");
    assert!(path.is_file());
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(draft_file_name(&draft).as_str())
    );
    // Atomic write: no temporary files are left behind.
    let leftovers: Vec<String> = std::fs::read_dir(drafts_root(&resolve_root))
        .expect("drafts dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");

    let listed = list_drafts(&resolve_root);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, draft.id);

    let loaded = load_draft(&resolve_root, &draft.id).expect("load draft");
    assert_eq!(
        serde_json::to_value(&loaded).expect("json"),
        serde_json::to_value(&draft).expect("json")
    );

    remove_draft(&resolve_root, &draft.id).expect("remove draft");
    assert!(!path.exists());
    assert!(list_drafts(&resolve_root).is_empty());
    // Removing an already absent draft is idempotent.
    remove_draft(&resolve_root, &draft.id).expect("idempotent remove");
    let error = load_draft(&resolve_root, &draft.id).expect_err("removed draft");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[tokio::test]
async fn list_drafts_skips_corrupt_files_and_orders_by_creation() {
    let scratch = ScratchDir::new("draft-list");
    let resolve_root = scratch.join("resolve");
    let mut drafts = Vec::new();
    for (id, created_ms) in [
        ("draft_a", 300_u64),
        ("draft_b", 100_u64),
        ("draft_c", 200_u64),
    ] {
        let mut draft = json_chain_draft();
        draft.id = id.to_owned();
        draft.recipe.id = id.to_owned();
        draft.created_ms = created_ms;
        save_draft(&resolve_root, &draft).expect("save draft");
        drafts.push(draft);
    }
    let drafts_dir = drafts_root(&resolve_root);
    std::fs::write(drafts_dir.join("draft_broken@1.json"), "{ not json").expect("corrupt draft");
    std::fs::write(drafts_dir.join("notes.txt"), "ignored").expect("non-json file");

    let listed = list_drafts(&resolve_root);
    let ids: Vec<&str> = listed.iter().map(|draft| draft.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["draft_b", "draft_c", "draft_a"],
        "corrupt files are skipped; the rest is ordered by (created_ms, id)"
    );
}

#[tokio::test]
async fn load_draft_returns_the_highest_version() {
    let scratch = ScratchDir::new("draft-versions");
    let resolve_root = scratch.join("resolve");
    let draft_v1 = json_chain_draft();
    save_draft(&resolve_root, &draft_v1).expect("save v1");
    let mut draft_v2 = draft_v1.clone();
    draft_v2.version = 2;
    draft_v2.recipe.version = 2;
    save_draft(&resolve_root, &draft_v2).expect("save v2");

    let loaded = load_draft(&resolve_root, &draft_v1.id).expect("load");
    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.recipe.version, 2);

    remove_draft(&resolve_root, &draft_v1.id).expect("remove all versions");
    assert!(load_draft(&resolve_root, &draft_v1.id).is_err());
    assert!(list_drafts(&resolve_root).is_empty());
}

// ---------------------------------------------------------------------------
// 6. Privacy: drafts never carry credentials or request headers/bodies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn draft_recipe_never_contains_credentials_or_headers() {
    let draft = json_chain_draft();
    let raw = serde_json::to_string_pretty(&draft).expect("serialize draft");
    for needle in ["authorization", "cookie", "bearer", "proxy-authorization"] {
        assert!(
            !raw.to_ascii_lowercase().contains(needle),
            "draft leaked {needle:?}: {raw}"
        );
    }
    assert!(
        !raw.contains("\"headers\""),
        "draft must not record request headers: {raw}"
    );
    // Every http_request step only carries method/url/max_bytes. Key order is
    // not part of the contract: serde_json's map is an insertion-ordered
    // IndexMap when the workspace enables `preserve_order` (GPUI does), and a
    // sorted BTreeMap otherwise.
    let Value::Object(args) = &draft.recipe.steps[0].args else {
        panic!("http_request args must be an object");
    };
    let mut keys: Vec<&str> = args.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["max_bytes", "method", "url"]);
}

// ---------------------------------------------------------------------------
// 7. Narrow match: only the exact source URL (query included) matches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn draft_recipe_matches_only_the_exact_source_url() {
    let draft = json_chain_draft();
    let recipe = &draft.recipe;
    let url = url::Url::parse(PAGE_URL).expect("url");
    assert!(recipe.matches(&url).is_some(), "the source URL must match");
    let sibling = url::Url::parse("http://127.0.0.1:8080/page/43").expect("url");
    assert!(
        recipe.matches(&sibling).is_none(),
        "a different path must not match a narrow draft"
    );
    let with_query = url::Url::parse(&format!("{PAGE_URL}?token=1")).expect("url");
    assert!(
        recipe.matches(&with_query).is_none(),
        "a different query must not match a narrow draft"
    );
    let other_host = url::Url::parse("https://example.com/page/42").expect("url");
    assert!(recipe.matches(&other_host).is_none());
}

// ---------------------------------------------------------------------------
// 8. Distillation and ids are deterministic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distill_is_deterministic() {
    let (request, actions, emitted) = json_chain();
    let first = distill(&request, "run_1", 10, &actions, &emitted).expect("draft");
    let second = distill(&request, "run_1", 10, &actions, &emitted).expect("draft");
    assert_eq!(first.id, second.id);
    assert_eq!(
        serde_json::to_value(&first.recipe).expect("json"),
        serde_json::to_value(&second.recipe).expect("json")
    );

    // A different emitted resource changes the id (it is derived from URLs).
    let other_file = "http://127.0.0.1:8080/files/other.bin";
    let other_emitted = vec![verified_item(other_file, "obs_1_0#/data/url")];
    let other = distill(&request, "run_1", 10, &actions, &other_emitted).expect("draft");
    assert_ne!(first.id, other.id);
}
