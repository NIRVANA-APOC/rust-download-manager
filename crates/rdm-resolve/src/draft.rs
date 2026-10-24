//! Deterministic recipe drafts distilled from one successful exploration
//! (`docs/architecture.md` §11.11).
//!
//! An exploration run is ephemeral: P3b keeps its *real action record* (not the
//! redacted trace), maps the evidence chain of an emitted, verified resource
//! back to recipe operators and stores a draft under
//! `<state>/resolve/drafts/<safe-id>@<version>.json`. Drafts never participate
//! in matching; publishing them requires the R1–R6 replay gates
//! ([`evaluate_publish`]) and only then enters the recipe library
//! ([`publish`]).
//!
//! Draft files may contain signed URLs from the observation (local asset, never
//! uploaded or sent to a model); the JSONL trace stays redacted. Drafts never
//! record request headers or bodies, so a `cookie`/`authorization` value can
//! never leak through this path.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::action::{parse_evidence, EvidenceRef};
use crate::contract::{
    Budget, RecipeRef, ResolveErrorKind, ResolveRequest, ResourceItem, VerificationStatus,
};
use crate::http::{HttpTool, DEFAULT_MAX_OBSERVATION_BYTES};
use crate::policy::ResolvePolicy;
use crate::recipe::{
    Recipe, RecipeEmit, RecipeLimits, RecipeMatch, RecipeStep, RecipeTest, RECIPE_SCHEMA,
};
use crate::registry::RecipeRegistry;
use crate::verify::{Verifier, VerifyHints};

/// Draft schema identifier.
pub const DRAFT_SCHEMA: &str = "rdm.recipe-draft/v1";
/// Hard cap on distilled recipe steps (defensive; current chains use ≤4).
pub const MAX_DRAFT_STEPS: usize = 24;
/// Version assigned to every distilled recipe.
pub const DRAFT_RECIPE_VERSION: u32 = 1;

/// One raw action of the exploration run kept for deterministic distillation.
///
/// This is intentionally *not* the sanitized trace: a draft needs the original
/// request URL/pointer to rebuild a replayable recipe. Headers and bodies are
/// never captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftAction {
    HttpRequest {
        method: String,
        url: String,
        max_bytes: usize,
        obs_id: Option<String>,
    },
    PageRead {
        url: String,
        obs_id: Option<String>,
    },
    JsonQuery {
        obs_id: String,
        pointer: String,
    },
    RegexCapture {
        obs_id: String,
        pattern: String,
        group: Option<usize>,
    },
    EmitManifest {
        candidates: Vec<DraftCandidate>,
    },
    /// `verify_resource` / `finish`: not expressible as recipe steps.
    Other,
}

/// One model-declared `emit_manifest.resources[]` entry (pre-verification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCandidate {
    pub url: String,
    /// `obs_X`, `obs_X#/<pointer>` or `obs_X#links/<n>`.
    pub evidence: String,
    pub name: Option<String>,
    pub one_time: bool,
    pub single_stream: bool,
}

/// A local, unpublished recipe distilled from one exploration run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeDraft {
    pub schema: String,
    pub id: String,
    pub version: u32,
    pub created_ms: u64,
    pub source_run_id: String,
    pub source_url: String,
    /// `narrow` for every automatic draft (it only knows the source sample).
    pub scope_hint: String,
    pub notes: Vec<String>,
    pub recipe: Recipe,
}

/// Result of replaying one sample URL against a candidate recipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleReplay {
    pub input_url: String,
    pub matched: bool,
    pub emitted_url: Option<String>,
    pub verification: Option<VerificationStatus>,
    pub error: Option<ResolveErrorKind>,
}

impl SampleReplay {
    /// Whether the emitted URL passed the verifier (R3/R4 criterion).
    pub fn is_downloadable(&self) -> bool {
        self.verification
            .is_some_and(VerificationStatus::is_downloadable)
    }

    fn describe(&self) -> String {
        if !self.matched {
            return format!("{}: recipe does not match this URL", self.input_url);
        }
        if let Some(kind) = self.error {
            return format!("{}: replay failed with {kind:?}", self.input_url);
        }
        match self.verification {
            Some(status) => format!("{}: emit verified as {status:?}", self.input_url),
            None => format!("{}: replay produced no verification", self.input_url),
        }
    }
}

/// One R1–R6 gate result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    /// `R1` … `R6`.
    pub id: String,
    pub name: String,
    pub pass: bool,
    pub detail: String,
}

/// Machine-readable publish evaluation (§11.11).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishReport {
    pub draft_id: String,
    pub published: bool,
    /// R1, R2, R3 and R6 all passed: publishing is allowed.
    pub required_passed: bool,
    /// `narrow` (source sample only) or `scoped` (fresh + negative samples).
    pub scope: String,
    pub gates: Vec<GateResult>,
    pub recipe: Option<RecipeRef>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Draft storage (`<state>/resolve/drafts`)
// ---------------------------------------------------------------------------

/// `<resolve_root>/drafts`.
pub fn drafts_root(resolve_root: &Path) -> PathBuf {
    resolve_root.join("drafts")
}

/// `<safe-id>@<version>.json`.
pub fn draft_file_name(draft: &RecipeDraft) -> String {
    format!("{}@{}.json", safe_component(&draft.id), draft.version)
}

/// Atomically persist one draft (unique temporary name + rename).
pub fn save_draft(resolve_root: &Path, draft: &RecipeDraft) -> io::Result<PathBuf> {
    let root = drafts_root(resolve_root);
    std::fs::create_dir_all(&root)?;
    let path = root.join(draft_file_name(draft));
    let text = serde_json::to_string_pretty(draft)
        .map_err(|error| io::Error::other(format!("cannot serialize recipe draft: {error}")))?;
    write_atomic(&path, &text)?;
    Ok(path)
}

/// Load every readable draft, sorted by `(created_ms, id)`.
pub fn list_drafts(resolve_root: &Path) -> Vec<RecipeDraft> {
    let root = drafts_root(resolve_root);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut drafts = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::warn!(path = %path.display(), "rdm-resolve: cannot read recipe draft");
            continue;
        };
        match serde_json::from_str::<RecipeDraft>(&text) {
            Ok(draft) => drafts.push(draft),
            Err(error) => tracing::warn!(
                path = %path.display(),
                error = %error,
                "rdm-resolve: skipping invalid recipe draft"
            ),
        }
    }
    drafts.sort_by(|left, right| {
        left.created_ms
            .cmp(&right.created_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    drafts
}

/// Load the newest version of one draft id.
pub fn load_draft(resolve_root: &Path, id: &str) -> io::Result<RecipeDraft> {
    let root = drafts_root(resolve_root);
    let prefix = format!("{}@", safe_component(id));
    let entries = std::fs::read_dir(&root).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("recipe draft {id:?} not found"),
            )
        } else {
            error
        }
    })?;
    let mut newest: Option<RecipeDraft> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !name.starts_with(&prefix)
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(draft) = serde_json::from_str::<RecipeDraft>(&text) else {
            continue;
        };
        if draft.id != id {
            continue;
        }
        if newest
            .as_ref()
            .is_none_or(|current| draft.version > current.version)
        {
            newest = Some(draft);
        }
    }
    newest.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("recipe draft {id:?} not found"),
        )
    })
}

/// Delete every stored version of one draft id (idempotent).
pub fn remove_draft(resolve_root: &Path, id: &str) -> io::Result<()> {
    let root = drafts_root(resolve_root);
    let prefix = format!("{}@", safe_component(id));
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !name.starts_with(&prefix) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deterministic distillation
// ---------------------------------------------------------------------------

/// One observation reconstructed from the raw action record.
struct ObservationSource {
    url: String,
    max_bytes: usize,
    method: String,
}

/// The evidence chain of the emitted resource.
enum Chain {
    /// The emitted URL is the observation's own request URL: emit a literal.
    Literal { url: String },
    /// Page-link evidence: fetch the page and regex-extract the target URL.
    Link {
        observation: String,
        target_url: String,
    },
    /// JSON-pointer evidence: fetch the JSON and query the pointer.
    Pointer {
        observation: String,
        pointer: String,
    },
}

/// Distill one replayable [`RecipeDraft`] from a successful exploration run.
///
/// Deterministic and model-free: only the recorded actions plus the emitted,
/// verified resources are consulted. Returns `None` when the evidence chain
/// cannot be expressed with the v1 operators (the exploration itself stays
/// successful — the draft is simply not created).
pub fn distill(
    request: &ResolveRequest,
    run_id: &str,
    created_ms: u64,
    actions: &[DraftAction],
    emitted: &[ResourceItem],
) -> Option<RecipeDraft> {
    let target = emitted
        .iter()
        .find(|item| item.verification.status.is_downloadable())?;
    let (observation, reference) = target
        .evidence
        .iter()
        .filter(|evidence| evidence.trim_start().starts_with("obs_"))
        .find_map(|evidence| parse_evidence(evidence.trim()).ok())?;

    let input_url = request.url.trim().to_owned();
    let parsed_input = Url::parse(&input_url).ok()?;
    let host = parsed_input.host_str()?.to_owned();

    let sources = observation_sources(actions);
    let chain = match reference {
        EvidenceRef::Observation => {
            let source = sources.get(&observation)?;
            if source.method != "GET" || source.url != target.url {
                return None;
            }
            Chain::Literal {
                url: target.url.clone(),
            }
        }
        EvidenceRef::Link(_) => {
            let source = sources.get(&observation)?;
            if source.method != "GET" {
                return None;
            }
            Chain::Link {
                observation,
                target_url: target.url.clone(),
            }
        }
        EvidenceRef::JsonPointer(pointer) => {
            let source = sources.get(&observation)?;
            if source.method != "GET" {
                return None;
            }
            Chain::Pointer {
                observation,
                pointer,
            }
        }
    };

    let mut steps: Vec<RecipeStep> = Vec::new();
    let mut step_index = 0usize;
    let (value_var, emit_url) = match &chain {
        Chain::Literal { url } => (None, url.clone()),
        Chain::Link {
            observation,
            target_url,
        } => {
            let source = sources.get(observation)?;
            push_http_step(
                &mut steps,
                &mut step_index,
                &source.url,
                source.max_bytes,
                "body_text",
                "o0",
            );
            let pattern = regex::escape(target_url);
            push_extraction_step(
                &mut steps,
                &mut step_index,
                "regex_capture",
                serde_json::json!({ "from": "o0", "pattern": pattern, "group": 0 }),
                "v0",
            );
            (Some("v0".to_owned()), "{{v0}}".to_owned())
        }
        Chain::Pointer {
            observation,
            pointer,
        } => {
            let source = sources.get(observation)?;
            push_http_step(
                &mut steps,
                &mut step_index,
                &source.url,
                source.max_bytes,
                "body_json",
                "o0",
            );
            push_extraction_step(
                &mut steps,
                &mut step_index,
                "json_query",
                serde_json::json!({ "from": "o0", "pointer": pointer }),
                "v0",
            );
            (Some("v0".to_owned()), "{{v0}}".to_owned())
        }
    };

    // R6 needs an assert; the distilled chain always ends with a URL sanity
    // check (literal emit asserts the literal, template emit asserts the var).
    let assert_value = value_var
        .as_deref()
        .map(|var| format!("{{{{{var}}}}}"))
        .unwrap_or_else(|| emit_url.clone());
    steps.push(RecipeStep {
        id: format!("s{step_index}"),
        op: "assert".to_owned(),
        args: serde_json::json!({
            "kind": "matches",
            "value": assert_value,
            "pattern": "^https?://",
        }),
        expect: None,
        assign: BTreeMap::new(),
        on_fail: None,
        when: None,
    });
    if steps.len() > MAX_DRAFT_STEPS {
        return None;
    }

    let final_url = target
        .final_url
        .clone()
        .unwrap_or_else(|| target.url.clone());
    let emit_name = file_name_from_url(&final_url).or_else(|| Some(final_url.clone()));
    let (one_time, single_stream) = emitted_transfer_hints(actions, target);

    let recipe = Recipe {
        schema: RECIPE_SCHEMA.to_owned(),
        id: String::new(),
        version: DRAFT_RECIPE_VERSION,
        name: String::new(),
        description: String::new(),
        matcher: RecipeMatch {
            hosts: vec![host.clone()],
            url_pattern: Some(format!("^{}$", regex::escape(&input_url))),
            priority: 0,
        },
        captures: Vec::new(),
        limits: RecipeLimits::default(),
        steps,
        emit: RecipeEmit {
            url: emit_url,
            name: emit_name,
            kind: None,
            one_time,
            single_stream,
            expires_in_secs: None,
            expected_sha256: None,
        },
        tests: vec![RecipeTest {
            name: "sample_input".to_owned(),
            input_url: input_url.clone(),
            expect_host_suffix: None,
            expect_no_match: false,
            skip: None,
        }],
        generalization: None,
    };
    let id = draft_id(&host, &input_url, &final_url);
    let recipe = Recipe {
        id: id.clone(),
        ..recipe
    };
    recipe.validate().ok()?;

    Some(RecipeDraft {
        schema: DRAFT_SCHEMA.to_owned(),
        id: id.clone(),
        version: DRAFT_RECIPE_VERSION,
        created_ms,
        source_run_id: run_id.to_owned(),
        source_url: input_url.clone(),
        scope_hint: "narrow".to_owned(),
        notes: vec![
            "deterministic draft distilled from exploration actions (no model calls)".to_owned(),
            format!("narrow match for the exact source URL: {input_url}"),
            "request headers and bodies are not recorded in drafts; replay uses URL/method only"
                .to_owned(),
            format!("publish with `rdm recipe publish {id}` (R1-R6 replay gates) before trusting"),
        ],
        recipe,
    })
}

fn push_http_step(
    steps: &mut Vec<RecipeStep>,
    step_index: &mut usize,
    url: &str,
    max_bytes: usize,
    assign_source: &str,
    var: &str,
) {
    steps.push(RecipeStep {
        id: format!("s{step_index}"),
        op: "http_request".to_owned(),
        args: serde_json::json!({
            "method": "GET",
            "url": url,
            "max_bytes": max_bytes,
        }),
        expect: None,
        assign: BTreeMap::from([(var.to_owned(), assign_source.to_owned())]),
        on_fail: None,
        when: None,
    });
    *step_index += 1;
}

fn push_extraction_step(
    steps: &mut Vec<RecipeStep>,
    step_index: &mut usize,
    op: &str,
    args: serde_json::Value,
    var: &str,
) {
    steps.push(RecipeStep {
        id: format!("s{step_index}"),
        op: op.to_owned(),
        args,
        expect: None,
        assign: BTreeMap::from([(var.to_owned(), "value".to_owned())]),
        on_fail: None,
        when: None,
    });
    *step_index += 1;
}

fn observation_sources(actions: &[DraftAction]) -> BTreeMap<String, ObservationSource> {
    let mut sources = BTreeMap::new();
    for action in actions {
        match action {
            DraftAction::HttpRequest {
                method,
                url,
                max_bytes,
                obs_id: Some(obs_id),
            } => {
                sources.insert(
                    obs_id.clone(),
                    ObservationSource {
                        url: url.clone(),
                        max_bytes: *max_bytes,
                        method: method.trim().to_ascii_uppercase(),
                    },
                );
            }
            DraftAction::PageRead {
                url,
                obs_id: Some(obs_id),
            } => {
                sources.insert(
                    obs_id.clone(),
                    ObservationSource {
                        url: url.clone(),
                        max_bytes: DEFAULT_MAX_OBSERVATION_BYTES,
                        method: "GET".to_owned(),
                    },
                );
            }
            _ => {}
        }
    }
    sources
}

/// `one_time`/`single_stream` from the last `emit_manifest` candidate that
/// matches the emitted URL, falling back to the verified resource's transfer
/// hints.
fn emitted_transfer_hints(actions: &[DraftAction], target: &ResourceItem) -> (bool, bool) {
    for action in actions.iter().rev() {
        if let DraftAction::EmitManifest { candidates } = action {
            if let Some(candidate) = candidates
                .iter()
                .find(|candidate| candidate.url == target.url)
            {
                return (candidate.one_time, candidate.single_stream);
            }
        }
    }
    (target.transfer.one_time, target.transfer.single_stream)
}

pub(crate) fn draft_id(host: &str, source_url: &str, final_url: &str) -> String {
    let digest = Sha256::digest(format!("{source_url}{final_url}").as_bytes());
    let hash = hex::encode(digest);
    format!(
        "draft_{}_{}",
        safe_component(host),
        &hash[..8.min(hash.len())]
    )
}

fn file_name_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let segment = parsed.path_segments()?.next_back()?.trim();
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_owned())
    }
}

fn safe_component(text: &str) -> String {
    let mut safe: String = text
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        safe.push_str("draft");
    }
    safe
}

// ---------------------------------------------------------------------------
// Replay gates (R1–R6)
// ---------------------------------------------------------------------------

/// Replay one sample against a candidate recipe with a fresh HTTP context and
/// no model calls (R1/R2/R3 building block). Never panics and never returns
/// `Err`: failures are reported in [`SampleReplay`].
pub async fn replay_sample(
    recipe: &Recipe,
    input_url: &str,
    policy: &ResolvePolicy,
    budget: Budget,
) -> SampleReplay {
    let mut replay = SampleReplay {
        input_url: input_url.to_owned(),
        matched: false,
        emitted_url: None,
        verification: None,
        error: None,
    };
    let Ok(url) = Url::parse(input_url.trim()) else {
        replay.error = Some(ResolveErrorKind::InvalidUrl);
        return replay;
    };
    let Some(vars) = recipe.matches(&url) else {
        return replay;
    };
    replay.matched = true;

    let mut http = match HttpTool::new(policy.clone(), recipe.budget_within(budget)) {
        Ok(http) => http,
        Err(error) => {
            replay.error = Some(error.kind);
            return replay;
        }
    };
    let mut runner = crate::recipe::RecipeRunner::new(&mut http, recipe);
    runner.seed(vars);
    let output = match runner.run().await {
        Ok(output) => output,
        Err(error) => {
            replay.error = Some(error.kind);
            return replay;
        }
    };
    let verification = {
        let hints = VerifyHints {
            one_time: output.one_time,
            ..VerifyHints::default()
        };
        Verifier::new(&mut http).verify(&output.url, &hints).await
    };
    replay.emitted_url = Some(output.url);
    replay.verification = Some(verification.status);
    replay
}

/// Evaluate the R1–R6 publish gates without touching the library or drafts.
///
/// `samples` are positive URLs used for the R3/R4 replays (empty → the draft's
/// `source_url`); `negatives` are URLs the recipe must **not** match (empty →
/// fall back to `recipe.tests` `expect_no_match` entries).
pub async fn evaluate_publish(
    draft: &RecipeDraft,
    samples: &[String],
    negatives: &[String],
    policy: &ResolvePolicy,
    budget: Budget,
) -> PublishReport {
    let sample_urls: Vec<String> = if samples.is_empty() {
        vec![draft.source_url.clone()]
    } else {
        samples.to_vec()
    };
    let mut replays = Vec::with_capacity(sample_urls.len());
    for sample in &sample_urls {
        replays.push(replay_sample(&draft.recipe, sample, policy, budget).await);
    }

    let mut gates = Vec::with_capacity(6);
    // R1: the replay path only builds HttpTool/RecipeRunner/Verifier.
    gates.push(GateResult {
        id: "R1".to_owned(),
        name: "no model calls".to_owned(),
        pass: true,
        detail: "replay path constructs HttpTool/RecipeRunner/Verifier only; no AiClient"
            .to_owned(),
    });
    // R2: every replay above got a fresh HttpTool + variable context.
    gates.push(GateResult {
        id: "R2".to_owned(),
        name: "fresh replay context".to_owned(),
        pass: true,
        detail: format!(
            "each of {} sample replay(s) used a fresh HttpTool and fresh recipe variables",
            replays.len()
        ),
    });
    // R3: first sample replays into a downloadable emitted URL.
    let first_ok = replays.first().is_some_and(SampleReplay::is_downloadable);
    gates.push(GateResult {
        id: "R3".to_owned(),
        name: "first sample verifies".to_owned(),
        pass: first_ok,
        detail: replays
            .first()
            .map(SampleReplay::describe)
            .unwrap_or_else(|| "no sample URL available".to_owned()),
    });
    // R4: at least two fresh samples, all verified.
    let fresh_ok = sample_urls.len() >= 2 && replays.iter().all(SampleReplay::is_downloadable);
    gates.push(GateResult {
        id: "R4".to_owned(),
        name: "fresh samples".to_owned(),
        pass: fresh_ok,
        detail: format!(
            "{} sample(s); {}/{} verified",
            sample_urls.len(),
            replays
                .iter()
                .filter(|replay| replay.is_downloadable())
                .count(),
            replays.len()
        ),
    });
    // R5: explicit negatives all non-matching, or recipe `expect_no_match` tests.
    let mut negative_replays = Vec::with_capacity(negatives.len());
    for negative in negatives {
        negative_replays.push(replay_sample(&draft.recipe, negative, policy, budget).await);
    }
    let (negative_ok, negative_detail) = if negatives.is_empty() {
        let has_expect_no_match = draft.recipe.tests.iter().any(|test| test.expect_no_match);
        let tests = draft.recipe.check_tests();
        let all_ok = !tests.is_empty() && tests.iter().all(|result| result.ok);
        (
            has_expect_no_match && all_ok,
            format!(
                "no explicit negatives; expect_no_match present={has_expect_no_match}, \
                 check_tests ok={all_ok} ({} test(s))",
                tests.len()
            ),
        )
    } else {
        let all_ignored = negative_replays.iter().all(|replay| !replay.matched);
        (
            all_ignored,
            format!(
                "{} negative sample(s); {} not matched",
                negative_replays.len(),
                negative_replays
                    .iter()
                    .filter(|replay| !replay.matched)
                    .count()
            ),
        )
    };
    gates.push(GateResult {
        id: "R5".to_owned(),
        name: "negative samples".to_owned(),
        pass: negative_ok,
        detail: negative_detail,
    });
    // R6: at least one top-level `assert` step (nested bounded_each steps are
    // not inspected in this slice).
    let assert_count = draft
        .recipe
        .steps
        .iter()
        .filter(|step| step.op == "assert")
        .count();
    gates.push(GateResult {
        id: "R6".to_owned(),
        name: "assertion present".to_owned(),
        pass: assert_count > 0,
        detail: format!("{assert_count} top-level assert step(s) (nested steps not inspected)"),
    });

    let gate = |id: &str| {
        gates
            .iter()
            .find(|gate| gate.id == id)
            .is_some_and(|gate| gate.pass)
    };
    let required_passed = gate("R1") && gate("R2") && gate("R3") && gate("R6");
    let scope = if gate("R4") && gate("R5") {
        "scoped"
    } else {
        "narrow"
    };
    PublishReport {
        draft_id: draft.id.clone(),
        published: false,
        required_passed,
        scope: scope.to_owned(),
        gates,
        recipe: None,
        error: None,
    }
}

/// Publish a draft that passed the required R1/R2/R3/R6 gates.
///
/// Order: save the recipe file → insert into the library → persist the index →
/// remove the draft. Failures are reported through `report.error` and returned
/// as `io::Error`; publishing the same draft again is idempotent.
pub fn publish(
    registry: &mut RecipeRegistry,
    resolve_root: &Path,
    draft: &RecipeDraft,
    report: &mut PublishReport,
) -> io::Result<RecipeRef> {
    if !report.required_passed {
        report.published = false;
        report.error = Some(
            "publish gates not passed (R1/R2/R3/R6 are required); run with --strict or fix the draft"
                .to_owned(),
        );
        return Err(io::Error::other(
            "publish gates not passed (R1/R2/R3/R6 are required)",
        ));
    }
    let recipe_path = match registry.save(&draft.recipe) {
        Ok(path) => path,
        Err(error) => {
            report.published = false;
            report.error = Some(format!("cannot save recipe file: {error}"));
            return Err(error);
        }
    };
    if let Err(error) = registry.insert(draft.recipe.clone()) {
        let _ = std::fs::remove_file(&recipe_path);
        report.published = false;
        report.error = Some(format!("cannot insert recipe: {error}"));
        return Err(io::Error::other(format!("cannot insert recipe: {error}")));
    }
    if let Err(error) = registry.save_index() {
        let _ = std::fs::remove_file(&recipe_path);
        report.published = false;
        report.error = Some(format!("cannot persist recipe index: {error}"));
        return Err(error);
    }
    if let Err(error) = remove_draft(resolve_root, &draft.id) {
        // The recipe is already in the library; the stale draft is only
        // cosmetic, so report it without unpublishing.
        tracing::warn!(
            draft = %draft.id,
            error = %error,
            "rdm-resolve: published recipe but could not remove draft"
        );
        report.error = Some(format!("published but could not remove draft: {error}"));
    }
    let recipe_ref = RecipeRef {
        id: draft.recipe.id.clone(),
        version: draft.recipe.version,
        state: "published".to_owned(),
    };
    report.published = true;
    report.recipe = Some(recipe_ref.clone());
    Ok(recipe_ref)
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("draft");
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    let result = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
