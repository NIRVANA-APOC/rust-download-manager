//! Bounded JsonAction exploration loop (`docs/architecture.md` §11.9).
//!
//! The model can only act through the seven tools in [`crate::action`]; all
//! network egress goes through the shared [`HttpTool`] (policy + budget +
//! hop-by-hop redirect re-checks), every tool result is fed back wrapped in
//! `<untrusted_content>`, and a manifest is only emitted after evidence-binding
//! **and** [`crate::verify::Verifier`] approval.
//!
//! Hard stops (contract §11.9): `max_llm_calls`, the HTTP budget, two
//! consecutive rounds without progress → `no_resources`, two consecutive
//! invalid actions → `model_protocol_error`.

use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::Value;
use url::Url;

use crate::action::{
    parse_action, parse_evidence, EmitManifestAction, EvidenceRef, ExploreAction,
    HttpRequestAction, ManifestCandidate,
};
use crate::contract::{
    new_id, now_ms, CredentialsHint, Diagnostics, ResolveError, ResolveErrorKind, ResolveOutcome,
    ResolveRequest, ResourceItem, ResourceManifest, RunStatus, SourceRef, TransferHint,
    Verification, DEFAULT_MAX_RESOURCES, SCHEMA_VERSION,
};
use crate::draft::{DraftAction, DraftCandidate};
use crate::http::{HttpRequestSpec, HttpTool, Observation, DEFAULT_MAX_OBSERVATION_BYTES};
use crate::page_snapshot::{PageSnapshot, MAX_SNAPSHOT_PROMPT_LINKS};
use crate::prompt::{
    initial_user_prompt, snapshot_prompt_section, system_prompt, truncate, wrap_untrusted,
    MAX_OBSERVATIONS,
};
use crate::trace::{digest_args, error_kind_name, redact_excerpt, RunStep};
use crate::verify::{Verifier, VerifyHints};

use rdm_ai::{AiClient, AiError, ChatMessage, TokenUsage};

/// Tool outcome of one executed action.
enum Effect {
    /// Feed `summary` back to the model and keep looping.
    Feedback { summary: String, progress: bool },
    /// `emit_manifest` passed evidence binding; candidates were verified.
    Emit(EmitedManifest),
    /// `finish` — terminal typed failure.
    Finish(ResolveError),
    /// Budget exhausted / unrecoverable tool stop.
    Stop(ResolveError),
}

/// Evidence-bound candidates after verification.
struct EmitedManifest {
    resources: Vec<ResourceItem>,
    verified: usize,
    notes: Vec<String>,
}

/// One retained observation plus its page scan (bounded memory).
struct ObservationRecord {
    observation: Observation,
    links: Vec<String>,
    title: Option<String>,
    text: String,
}

impl ObservationRecord {
    fn new(observation: Observation) -> Self {
        let (links, title, text) = scan_page_parts(&observation);
        Self {
            observation,
            links,
            title,
            text,
        }
    }

    /// Synthetic observation built from a browser page snapshot (P4b-b1): the
    /// links/title come from the local bridge instead of a fetched HTML body.
    fn from_snapshot(snapshot: &PageSnapshot) -> Self {
        Self {
            observation: Observation {
                obs_id: new_id("obs"),
                request_url: snapshot.url.clone(),
                final_url: snapshot.url.clone(),
                status: 200,
                headers: BTreeMap::from([(
                    "content-type".to_owned(),
                    "text/html; charset=utf-8".to_owned(),
                )]),
                body: Vec::new(),
                truncated: false,
            },
            links: snapshot.links.clone(),
            title: snapshot.title.clone(),
            text: String::new(),
        }
    }
}

/// In-memory run state (`≤64` observations, never persisted).
#[derive(Default)]
struct ExploreState {
    observations: Vec<ObservationRecord>,
    candidates: BTreeMap<String, ()>,
    page_title: Option<String>,
    final_url: Option<String>,
}

impl ExploreState {
    fn find(&self, obs_id: &str) -> Option<&ObservationRecord> {
        self.observations
            .iter()
            .find(|record| record.observation.obs_id == obs_id)
    }

    fn observe(&mut self, observation: Observation) {
        self.final_url = Some(observation.final_url.clone());
        let record = ObservationRecord::new(observation);
        if record.title.is_some() {
            self.page_title = record.title.clone();
        }
        if self.observations.len() >= MAX_OBSERVATIONS {
            self.observations.remove(0);
        }
        self.observations.push(record);
    }

    /// Seed the state with the browser page snapshot as a synthetic observation
    /// (P4b-b1). Returns the observation id used for evidence handles.
    ///
    /// The snapshot title stays on the observation (tool feedback) but is not
    /// promoted to the run's page title: browser-snapshot metadata never leaks
    /// into the machine manifest (`manifest.source.title`).
    fn seed(&mut self, snapshot: &PageSnapshot) -> String {
        let record = ObservationRecord::from_snapshot(snapshot);
        let obs_id = record.observation.obs_id.clone();
        self.final_url = Some(snapshot.url.clone());
        self.observations.push(record);
        obs_id
    }

    /// Record a candidate URL; returns `true` when it was not seen before.
    fn note_candidate(&mut self, url: &str) -> bool {
        self.candidates.insert(url.to_owned(), ()).is_none()
    }
}

/// Run the exploration loop against an existing (possibly already partially
/// spent) [`HttpTool`]. Returns a complete outcome; never panics.
///
/// `trace_steps` is an optional sink filled with one sanitized
/// [`RunStep`] per executed action (used by the resolver's P3a run traces);
/// `draft_actions` receives the raw action record used by
/// [`crate::draft::distill`] (never headers/bodies).
///
/// `seed` is the optional browser page snapshot (P4b-b1): when present it
/// becomes a synthetic page observation and the snapshot `page_read` shortcut.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn explore(
    http: &mut HttpTool,
    request: &ResolveRequest,
    ai: &rdm_ai::AiConfig,
    run_id: String,
    started: Instant,
    trace_steps: &mut Vec<RunStep>,
    draft_actions: &mut Vec<DraftAction>,
    seed: Option<&PageSnapshot>,
) -> ResolveOutcome {
    let max_llm_calls = request.llm_call_limit();
    let budget = http.budget().budget();
    let mut diagnostics = Diagnostics::default();
    let mut state = ExploreState::default();
    // P4b-b1: the browser snapshot becomes the synthetic page observation.
    let seed_obs_id = seed.map(|snapshot| state.seed(snapshot));
    // Seed the draft record with the equivalent page read so a seeded run can
    // still be distilled deterministically (the snapshot *is* that page's
    // observation) instead of spending a model-assisted draft call.
    if let (Some(obs_id), Some(snapshot)) = (seed_obs_id.as_ref(), seed) {
        draft_actions.push(DraftAction::PageRead {
            url: snapshot.url.clone(),
            obs_id: Some(obs_id.clone()),
        });
    }
    let mut initial_prompt = initial_user_prompt(request, &budget, max_llm_calls);
    if let (Some(obs_id), Some(snapshot)) = (seed_obs_id.as_deref(), seed) {
        initial_prompt.push_str("\n\n");
        initial_prompt.push_str(&snapshot_prompt_section(obs_id, snapshot));
    }
    let mut messages = vec![
        ChatMessage::system(system_prompt(request, &budget, max_llm_calls)),
        ChatMessage::user(initial_prompt),
    ];
    let client = match AiClient::new(ai.clone()) {
        Ok(client) => client,
        Err(error) => {
            return fail(run_id, ai_error(&error), diagnostics, http, started, 0);
        }
    };

    let mut llm_calls: u32 = 0;
    let mut steps: u32 = 0;
    let mut invalid_streak: u32 = 0;
    let mut no_progress_streak: u32 = 0;

    loop {
        diagnostics.steps = steps;
        if llm_calls >= max_llm_calls {
            return fail(
                run_id,
                ResolveError::new(
                    ResolveErrorKind::BudgetExhausted,
                    format!("model call budget exhausted ({max_llm_calls} calls)"),
                ),
                diagnostics,
                http,
                started,
                llm_calls,
            );
        }
        if let Some(error) = budget_error(http) {
            return fail(run_id, error, diagnostics, http, started, llm_calls);
        }

        llm_calls += 1;
        let reply = client.chat_messages(&messages).await;
        // Account for this call *before* inspecting the result: a provider error
        // still billed the request, and every early-return path below must keep
        // the token usage already spent (see `Diagnostics::llm_*`).
        record_usage(&mut diagnostics, client.take_usage());
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                return fail(
                    run_id,
                    ai_error(&error),
                    diagnostics,
                    http,
                    started,
                    llm_calls,
                )
            }
        };
        // A slow model call may itself consume the wall-clock budget.
        if let Some(error) = budget_error(http) {
            return fail(run_id, error, diagnostics, http, started, llm_calls);
        }
        messages.push(ChatMessage::assistant(reply.clone()));

        let action = match parse_action(&reply) {
            Ok(action) => {
                invalid_streak = 0;
                action
            }
            Err(error) => {
                invalid_streak += 1;
                if invalid_streak >= 2 {
                    return fail(
                        run_id,
                        ResolveError::new(
                            ResolveErrorKind::ModelProtocolError,
                            format!(
                                "model produced invalid actions twice in a row; last error: {error}"
                            ),
                        ),
                        diagnostics,
                        http,
                        started,
                        llm_calls,
                    );
                }
                messages.push(ChatMessage::user(wrap_untrusted(&format!(
                    "action rejected: {error}. Reply with exactly one JSON action object."
                ))));
                no_progress_streak += 1;
                if no_progress_streak >= 2 {
                    return fail(
                        run_id,
                        no_progress_error(&state, llm_calls),
                        diagnostics,
                        http,
                        started,
                        llm_calls,
                    );
                }
                continue;
            }
        };

        steps += 1;
        let step_started = Instant::now();
        let op = action.op_name();
        let excerpt = redact_excerpt(&action.trace_summary());
        let args_digest = digest_args(&excerpt);
        let last_obs_before = state
            .observations
            .last()
            .map(|record| record.observation.obs_id.clone());
        // Keep the raw action record (bounded like the step count itself: one
        // action per model call) for deterministic draft distillation.
        let draft_index = if draft_actions.len() < crate::trace::MAX_STEPS {
            draft_actions.push(draft_action_from(&action));
            Some(draft_actions.len() - 1)
        } else {
            None
        };
        // A `page_read` of the snapshot page is answered synthetically; its
        // observation is the seeded one, so attribute the draft action to it.
        let seeded_page_read = match (&action, seed) {
            (ExploreAction::PageRead { url }, Some(snapshot)) => Url::parse(url.trim())
                .map(|parsed| snapshot.matches_url(&parsed))
                .unwrap_or(false),
            _ => false,
        };
        let effect = execute_action(http, &mut state, request, action, seed).await;
        let duration_ms = step_started.elapsed().as_millis() as u64;
        let obs_id = match state
            .observations
            .last()
            .map(|record| record.observation.obs_id.clone())
        {
            Some(obs_id) if Some(&obs_id) != last_obs_before.as_ref() => Some(obs_id),
            _ if seeded_page_read => seed_obs_id.clone(),
            _ => None,
        };
        if let (Some(index), Some(obs_id)) = (draft_index, obs_id.as_deref()) {
            if let Some(
                DraftAction::HttpRequest { obs_id: slot, .. }
                | DraftAction::PageRead { obs_id: slot, .. },
            ) = draft_actions.get_mut(index)
            {
                *slot = Some(obs_id.to_owned());
            }
        }
        let (outcome, note) = match &effect {
            Effect::Feedback { .. } => ("ok".to_owned(), None),
            Effect::Emit(emited) => (
                "ok".to_owned(),
                Some(format!(
                    "{} candidate(s), {} verified",
                    emited.resources.len(),
                    emited.verified
                )),
            ),
            Effect::Finish(error) | Effect::Stop(error) => {
                (format!("error:{}", error_kind_name(&error.kind)), None)
            }
        };
        trace_steps.push(RunStep {
            // Index across the whole trace: callers may prepend synthetic steps
            // (e.g. `builtin_verify`), so derive it from the current length.
            index: trace_steps.len() as u32,
            op: op.to_owned(),
            args_digest,
            args_excerpt: Some(excerpt),
            outcome,
            duration_ms,
            obs_id,
            note,
        });
        match effect {
            Effect::Feedback { summary, progress } => {
                messages.push(ChatMessage::user(wrap_untrusted(&summary)));
                if progress {
                    no_progress_streak = 0;
                } else {
                    no_progress_streak += 1;
                    if no_progress_streak >= 2 {
                        return fail(
                            run_id,
                            no_progress_error(&state, llm_calls),
                            diagnostics,
                            http,
                            started,
                            llm_calls,
                        );
                    }
                }
            }
            Effect::Emit(emited) => {
                diagnostics.steps = steps;
                diagnostics.llm_calls = llm_calls;
                return finish_manifest(
                    run_id,
                    request,
                    &state,
                    emited,
                    diagnostics,
                    http,
                    started,
                );
            }
            Effect::Finish(error) | Effect::Stop(error) => {
                return fail(run_id, error, diagnostics, http, started, llm_calls)
            }
        }
    }
}

fn no_progress_error(state: &ExploreState, llm_calls: u32) -> ResolveError {
    ResolveError::new(
        ResolveErrorKind::NoResources,
        format!(
            "exploration made no progress for 2 rounds ({} observation(s) after {llm_calls} model call(s))",
            state.observations.len()
        ),
    )
}

/// Fold one drained [`TokenUsage`] into the run diagnostics (saturating).
///
/// [`rdm_ai::AiClient`] records the tokens of every model call internally; the
/// exploration loop drains that ledger right after each `chat_messages` so both
/// success and failure outcomes report what the provider actually billed. The
/// `llm_calls` counter is tracked separately (it also covers the final failed
/// call), so only the token counters and the estimate flag are merged here.
fn record_usage(diagnostics: &mut Diagnostics, usage: TokenUsage) {
    diagnostics.llm_prompt_tokens = diagnostics
        .llm_prompt_tokens
        .saturating_add(usage.prompt_tokens);
    diagnostics.llm_completion_tokens = diagnostics
        .llm_completion_tokens
        .saturating_add(usage.completion_tokens);
    diagnostics.llm_total_tokens = diagnostics
        .llm_total_tokens
        .saturating_add(usage.total_tokens);
    diagnostics.llm_estimated |= usage.estimated;
}

// ---------------------------------------------------------------------------
// Action execution
// ---------------------------------------------------------------------------

/// Project one parsed action into the raw record used by draft distillation.
///
/// Deliberately drops headers and bodies (drafts must never carry credential
/// values); `obs_id` is filled in after execution for fetch-style actions.
fn draft_action_from(action: &ExploreAction) -> DraftAction {
    match action {
        ExploreAction::HttpRequest(action) => DraftAction::HttpRequest {
            method: action.method.clone(),
            url: action.url.clone(),
            max_bytes: action.max_bytes,
            obs_id: None,
        },
        ExploreAction::PageRead { url } => DraftAction::PageRead {
            url: url.clone(),
            obs_id: None,
        },
        ExploreAction::JsonQuery { obs_id, pointer } => DraftAction::JsonQuery {
            obs_id: obs_id.clone(),
            pointer: pointer.clone(),
        },
        ExploreAction::RegexCapture {
            obs_id,
            pattern,
            group,
        } => DraftAction::RegexCapture {
            obs_id: obs_id.clone(),
            pattern: pattern.clone(),
            group: *group,
        },
        ExploreAction::EmitManifest(emit) => DraftAction::EmitManifest {
            candidates: emit
                .resources
                .iter()
                .map(|candidate| DraftCandidate {
                    url: candidate.url.clone(),
                    evidence: candidate.evidence.clone(),
                    name: candidate.name.clone(),
                    one_time: candidate.one_time,
                    single_stream: candidate.single_stream,
                })
                .collect(),
        },
        ExploreAction::VerifyResource { .. } | ExploreAction::Finish { .. } => DraftAction::Other,
    }
}

async fn execute_action(
    http: &mut HttpTool,
    state: &mut ExploreState,
    request: &ResolveRequest,
    action: ExploreAction,
    seed: Option<&PageSnapshot>,
) -> Effect {
    match action {
        ExploreAction::HttpRequest(action) => run_http_request(http, state, action).await,
        ExploreAction::PageRead { url } => run_page_read(http, state, &url, seed).await,
        ExploreAction::JsonQuery { obs_id, pointer } => run_json_query(state, &obs_id, &pointer),
        ExploreAction::RegexCapture {
            obs_id,
            pattern,
            group,
        } => run_regex_capture(state, &obs_id, &pattern, group),
        ExploreAction::VerifyResource { url, one_time } => {
            run_verify_resource(http, state, &url, one_time).await
        }
        ExploreAction::EmitManifest(emit) => run_emit_manifest(http, state, request, emit).await,
        ExploreAction::Finish { reason, kind } => Effect::Finish(ResolveError::new(
            kind,
            format!("exploration finished: {reason}"),
        )),
    }
}

async fn run_http_request(
    http: &mut HttpTool,
    state: &mut ExploreState,
    action: HttpRequestAction,
) -> Effect {
    let mut spec = HttpRequestSpec::get(action.url).max_bytes(action.max_bytes);
    spec.method = action.method;
    spec.headers = action.headers.into_iter().collect();
    spec.body = action.body;
    match http.request(spec).await {
        Ok(observation) => {
            let obs_id = observation.obs_id.clone();
            state.observe(observation);
            let Some(record) = state.find(&obs_id) else {
                return feedback_error("observation was evicted immediately");
            };
            let summary = format_observation_summary("http_request", record);
            Effect::Feedback {
                summary,
                progress: true,
            }
        }
        Err(error) => tool_error_effect(error, "http_request"),
    }
}

fn format_snapshot_observation(record: &ObservationRecord) -> String {
    let mut summary = format!(
        "page_read: browser page snapshot (untrusted data) obs {} status={} (no network request)\ntitle: {}",
        record.observation.obs_id,
        record.observation.status,
        record.title.as_deref().unwrap_or("(none)")
    );
    summary.push_str(&format!("\nlinks ({}):", record.links.len()));
    for (index, link) in record
        .links
        .iter()
        .take(MAX_SNAPSHOT_PROMPT_LINKS)
        .enumerate()
    {
        summary.push_str(&format!("\n#{index} {link}"));
    }
    if record.links.len() > MAX_SNAPSHOT_PROMPT_LINKS {
        summary.push_str(&format!(
            "\n… {} more link(s) not shown",
            record.links.len() - MAX_SNAPSHOT_PROMPT_LINKS
        ));
    }
    summary.push_str(&format!(
        "\nsource: browser page snapshot; use evidence handles obs_<id>#links/N (obs id {})",
        record.observation.obs_id
    ));
    summary
}

async fn run_page_read(
    http: &mut HttpTool,
    state: &mut ExploreState,
    url: &str,
    seed: Option<&PageSnapshot>,
) -> Effect {
    // P4b-b1: the browser snapshot *is* the page — no network request, no HTTP
    // budget consumed, and the existing synthetic observation is reused.
    if let Some(seed) = seed {
        if let Ok(parsed) = Url::parse(url.trim()) {
            if seed.matches_url(&parsed) {
                let Some(record) = state
                    .observations
                    .iter()
                    .find(|record| record.observation.request_url == seed.url)
                else {
                    return feedback_error("page snapshot observation is not available");
                };
                let summary = format_snapshot_observation(record);
                return Effect::Feedback {
                    summary,
                    progress: true,
                };
            }
        }
    }
    let spec = HttpRequestSpec::get(url).max_bytes(DEFAULT_MAX_OBSERVATION_BYTES);
    match http.request(spec).await {
        Ok(observation) => {
            let obs_id = observation.obs_id.clone();
            state.observe(observation);
            let Some(record) = state.find(&obs_id) else {
                return feedback_error("observation was evicted immediately");
            };
            let summary = format_observation_summary("page_read", record);
            Effect::Feedback {
                summary,
                progress: true,
            }
        }
        Err(error) => tool_error_effect(error, "page_read"),
    }
}

fn run_json_query(state: &ExploreState, obs_id: &str, pointer: &str) -> Effect {
    let Some(record) = state.find(obs_id) else {
        return feedback_error(&format!(
            "json_query: unknown observation {obs_id:?}; use an obs_ id from a tool result"
        ));
    };
    let Some(value) = record.observation.body_json() else {
        return feedback_error(&format!(
            "json_query: observation {obs_id} has no JSON body"
        ));
    };
    let Some(target) = value.pointer(pointer) else {
        return feedback_error(&format!(
            "json_query: pointer {pointer:?} does not exist in observation {obs_id}"
        ));
    };
    let rendered = serde_json::to_string(target).unwrap_or_default();
    Effect::Feedback {
        summary: format!(
            "json_query obs {obs_id} pointer {pointer:?} => {} {}",
            json_type(target),
            truncate(&rendered, 1500)
        ),
        progress: true,
    }
}

fn run_regex_capture(
    state: &ExploreState,
    obs_id: &str,
    pattern: &str,
    group: Option<usize>,
) -> Effect {
    let Some(record) = state.find(obs_id) else {
        return feedback_error(&format!(
            "regex_capture: unknown observation {obs_id:?}; use an obs_ id from a tool result"
        ));
    };
    let regex = match regex::Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => {
            return feedback_error(&format!("regex_capture: invalid regex: {error}"));
        }
    };
    let text = record.observation.body_text();
    let Some(captures) = regex.captures(&text) else {
        return feedback_error(&format!(
            "regex_capture: pattern {pattern:?} did not match observation {obs_id}"
        ));
    };
    let matched = match group {
        Some(group) => captures.get(group),
        None => captures.get(1).or_else(|| captures.get(0)),
    };
    let Some(matched) = matched else {
        return feedback_error(&format!(
            "regex_capture: group {} did not participate in the match",
            group.unwrap_or(1)
        ));
    };
    Effect::Feedback {
        summary: format!(
            "regex_capture obs {obs_id} pattern {pattern:?} => {:?}",
            truncate(matched.as_str(), 1500)
        ),
        progress: true,
    }
}

async fn run_verify_resource(
    http: &mut HttpTool,
    state: &mut ExploreState,
    url: &str,
    one_time: bool,
) -> Effect {
    let hints = VerifyHints {
        one_time,
        ..VerifyHints::default()
    };
    let verification = Verifier::new(http).verify(url, &hints).await;
    let progress = state.note_candidate(url);
    Effect::Feedback {
        summary: format!(
            "verify_resource {url}: status={:?} method={:?} http_status={:?} content-type={:?} \
             size={:?} ranges={:?}",
            verification.status,
            verification.method,
            verification.http_status,
            verification.content_type,
            verification.size_bytes,
            verification.supports_ranges
        ),
        progress,
    }
}

async fn run_emit_manifest(
    http: &mut HttpTool,
    state: &mut ExploreState,
    request: &ResolveRequest,
    emit: EmitManifestAction,
) -> Effect {
    let max_resources = request
        .max_resources
        .unwrap_or(DEFAULT_MAX_RESOURCES)
        .max(1);
    if emit.resources.len() > max_resources {
        return feedback_error(&format!(
            "emit_manifest rejected: {} resources exceed max_resources={max_resources}; \
             emit at most {max_resources}",
            emit.resources.len()
        ));
    }

    // 1) Evidence binding: every URL must be traceable to a real observation.
    let mut binding_errors = Vec::new();
    for candidate in &emit.resources {
        if let Err(error) = bind_evidence(state, &candidate.url, &candidate.evidence) {
            binding_errors.push(error);
        }
    }
    if !binding_errors.is_empty() {
        return Effect::Feedback {
            summary: format!(
                "emit_manifest rejected: evidence check failed for {} of {} resource(s):\n- {}",
                binding_errors.len(),
                emit.resources.len(),
                binding_errors.join("\n- ")
            ),
            progress: false,
        };
    }
    if let Some(error) = budget_error(http) {
        return Effect::Stop(error);
    }

    // 2) Verifier judgement: only real files reach the manifest.
    let mut resources = Vec::with_capacity(emit.resources.len());
    let mut verified = 0usize;
    let mut notes = emit.notes;
    for candidate in &emit.resources {
        state.note_candidate(&candidate.url);
        let hints = VerifyHints {
            one_time: candidate.one_time,
            ..VerifyHints::default()
        };
        let verification = Verifier::new(http).verify(&candidate.url, &hints).await;
        if verification.status.is_downloadable() {
            verified += 1;
        } else {
            notes.push(format!(
                "{} failed verification: {:?}",
                candidate.url, verification.status
            ));
        }
        resources.push(build_resource_item(candidate, verification));
    }
    if verified == 0 {
        // Distinguish "no candidate is a file" from "the budget cut verification
        // short": the latter must be reported as budget_exhausted, not partial.
        if let Some(error) = budget_error(http) {
            return Effect::Stop(error);
        }
    }
    Effect::Emit(EmitedManifest {
        resources,
        verified,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Evidence binding
// ---------------------------------------------------------------------------

/// Check that `url` really appears in the observation named by `evidence`.
fn bind_evidence(state: &ExploreState, url: &str, evidence: &str) -> Result<(), String> {
    let (obs_id, reference) = parse_evidence(evidence)?;
    let Some(record) = state.find(&obs_id) else {
        return Err(format!(
            "{url}: evidence {evidence:?} names an unknown observation {obs_id:?}; \
             use an obs_ id from a tool result"
        ));
    };
    let observation = &record.observation;
    match reference {
        EvidenceRef::Observation => {
            if observation.request_url == url || observation.final_url == url {
                return Ok(());
            }
            if !observation.body.is_empty() && observation.body_text().contains(url) {
                return Ok(());
            }
            if record.links.iter().any(|link| link == url) {
                return Ok(());
            }
            Err(format!(
                "{url}: does not appear in observation {obs_id} (request/final URL, body text \
                 or scanned page links)"
            ))
        }
        EvidenceRef::JsonPointer(pointer) => {
            let Some(value) = observation.body_json() else {
                return Err(format!("{url}: observation {obs_id} has no JSON body"));
            };
            let Some(target) = value.pointer(&pointer) else {
                return Err(format!(
                    "{url}: pointer {pointer:?} does not exist in observation {obs_id}"
                ));
            };
            match target.as_str() {
                Some(text) if text == url => Ok(()),
                Some(text) => Err(format!(
                    "{url}: pointer {pointer:?} in observation {obs_id} is {text:?}"
                )),
                None => Err(format!(
                    "{url}: pointer {pointer:?} in observation {obs_id} is not a string URL"
                )),
            }
        }
        EvidenceRef::Link(index) => match record.links.get(index) {
            Some(link) if link == url => Ok(()),
            Some(link) => Err(format!(
                "{url}: scanned link #{index} of observation {obs_id} is {link:?}"
            )),
            None => Err(format!(
                "{url}: observation {obs_id} has {} scanned link(s); index {index} is out of range",
                record.links.len()
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Projection helpers
// ---------------------------------------------------------------------------

fn build_resource_item(candidate: &ManifestCandidate, verification: Verification) -> ResourceItem {
    let final_url = verification.final_url.clone();
    let raw_name = candidate
        .name
        .clone()
        .or_else(|| {
            final_url
                .as_deref()
                .and_then(file_name_from_url)
                .or_else(|| file_name_from_url(&candidate.url))
        })
        .unwrap_or_default();
    // Exploration models frequently emit a page title as `name` (no suffix).
    // Normalize with verified Content-Type / URL / kind so downstream CLI/MCP/
    // GUI consumers all receive a real filename (`标题 (720P).mp4`).
    let name = rdm_ai::ensure_file_extension(
        &raw_name,
        verification.content_type.as_deref(),
        final_url.as_deref().or(Some(candidate.url.as_str())),
        candidate.kind.as_deref(),
    );
    let notes = if verification.status.is_downloadable() {
        Vec::new()
    } else {
        vec![format!("verification failed: {:?}", verification.status)]
    };
    ResourceItem {
        resource_id: new_id("res"),
        url: candidate.url.clone(),
        final_url,
        name: Some(name),
        kind: candidate.kind.clone(),
        content_type: verification.content_type.clone(),
        size_bytes: verification.size_bytes,
        supports_ranges: verification.supports_ranges,
        etag: verification.etag.clone(),
        last_modified: verification.last_modified.clone(),
        expected_sha256: None,
        credentials: CredentialsHint::default(),
        transfer: TransferHint {
            one_time: candidate.one_time,
            single_stream: candidate.single_stream || candidate.one_time,
            ..TransferHint::default()
        },
        evidence: vec![candidate.evidence.clone()],
        notes,
        verification,
    }
    .with_transfer_from_verification()
}

fn finish_manifest(
    run_id: String,
    request: &ResolveRequest,
    state: &ExploreState,
    emited: EmitedManifest,
    mut diagnostics: Diagnostics,
    http: &HttpTool,
    started: Instant,
) -> ResolveOutcome {
    let max_resources = request
        .max_resources
        .unwrap_or(DEFAULT_MAX_RESOURCES)
        .max(1);
    let (mut resources, verified) = if emited.verified > 0 {
        (
            emited
                .resources
                .into_iter()
                .filter(|item| item.verification.status.is_downloadable())
                .collect::<Vec<_>>(),
            emited.verified,
        )
    } else {
        (emited.resources, 0)
    };
    resources.truncate(max_resources);

    let mut notes = emited.notes;
    if verified == 0 {
        notes.push("no emitted candidate passed verification".to_owned());
    }
    let manifest = ResourceManifest {
        schema_version: SCHEMA_VERSION.to_owned(),
        manifest_id: new_id("mf"),
        source: SourceRef {
            input_url: request.url.clone(),
            final_url: state
                .final_url
                .clone()
                .or_else(|| Some(request.url.trim().to_owned())),
            title: state.page_title.clone(),
            recipe_id: None,
        },
        resources,
        notes,
        created_ms: now_ms(),
    };

    let status = if verified > 0 {
        RunStatus::Succeeded
    } else {
        RunStatus::Partial
    };
    let resource_count = manifest.resources.len();
    let downloadable = manifest.downloadable().count();
    diagnostics.http_requests = http.budget().requests();
    diagnostics.bytes_read = http.budget().bytes();
    diagnostics.elapsed_ms = started.elapsed().as_millis() as u64;
    if verified > 0 {
        diagnostics.summary = format!(
            "exploration resolved {downloadable} verified resource(s) from {resource_count} \
             candidate(s) using {} model call(s)",
            diagnostics.llm_calls
        );
    } else {
        diagnostics.error = Some(ResolveError::new(
            ResolveErrorKind::NoResources,
            format!("exploration emitted {resource_count} candidate(s) but 0 passed verification"),
        ));
        diagnostics.summary = format!(
            "exploration produced {resource_count} candidate(s); 0 verified ({} model call(s))",
            diagnostics.llm_calls
        );
    }
    ResolveOutcome {
        run_id,
        status,
        manifest: Some(manifest),
        recipe: None,
        diagnostics,
    }
}

fn fail(
    run_id: String,
    error: ResolveError,
    mut diagnostics: Diagnostics,
    http: &HttpTool,
    started: Instant,
    llm_calls: u32,
) -> ResolveOutcome {
    diagnostics.llm_calls = llm_calls;
    diagnostics.http_requests = http.budget().requests();
    diagnostics.bytes_read = http.budget().bytes();
    diagnostics.elapsed_ms = started.elapsed().as_millis() as u64;
    diagnostics.summary = format!("exploration failed: {error}");
    ResolveOutcome::failed(run_id, error, diagnostics)
}

fn feedback_error(message: &str) -> Effect {
    Effect::Feedback {
        summary: message.to_owned(),
        progress: false,
    }
}

fn tool_error_effect(error: ResolveError, tool: &str) -> Effect {
    if error.kind == ResolveErrorKind::BudgetExhausted {
        Effect::Stop(error)
    } else {
        Effect::Feedback {
            summary: format!("{tool} failed: {error}"),
            progress: false,
        }
    }
}

fn budget_error(http: &HttpTool) -> Option<ResolveError> {
    let tracker = http.budget();
    let budget = tracker.budget();
    if tracker.requests() >= budget.http_requests {
        return Some(ResolveError::new(
            ResolveErrorKind::BudgetExhausted,
            format!(
                "http request budget exhausted ({} requests)",
                tracker.requests()
            ),
        ));
    }
    if tracker.bytes() >= budget.bytes_read {
        return Some(ResolveError::new(
            ResolveErrorKind::BudgetExhausted,
            format!("byte budget exhausted ({} bytes)", tracker.bytes()),
        ));
    }
    tracker.check_deadline().err()
}

fn ai_error(error: &AiError) -> ResolveError {
    match error {
        AiError::NotConfigured => ResolveError::new(
            ResolveErrorKind::ModelUnavailable,
            "AI provider is not configured (enable it and set an API key)",
        ),
        AiError::Http(detail) => ResolveError::new(
            ResolveErrorKind::ModelUnavailable,
            format!("AI request failed: {detail}"),
        ),
        AiError::Status { status, body } => ResolveError::new(
            ResolveErrorKind::ModelUnavailable,
            format!("AI provider returned HTTP {status}: {body}"),
        ),
        AiError::Empty => ResolveError::new(
            ResolveErrorKind::ModelUnavailable,
            "AI provider returned an empty reply",
        ),
        AiError::Json(detail) => ResolveError::new(
            ResolveErrorKind::ModelProtocolError,
            format!("AI provider reply was not a valid chat completion: {detail}"),
        ),
        AiError::Other(detail) => ResolveError::new(
            ResolveErrorKind::ModelProtocolError,
            format!("AI client error: {detail}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn scan_page_parts(observation: &Observation) -> (Vec<String>, Option<String>, String) {
    if observation.body.is_empty() {
        return (Vec::new(), None, String::new());
    }
    let content_type_is_page = observation.content_type().is_some_and(|content_type| {
        let content_type = content_type.to_ascii_lowercase();
        content_type.contains("html")
            || content_type.contains("xml")
            || content_type.starts_with("text/")
    });
    if !observation.looks_like_page() && !content_type_is_page {
        return (Vec::new(), None, String::new());
    }
    let base = Url::parse(&observation.final_url).or_else(|_| Url::parse(&observation.request_url));
    let Ok(base) = base else {
        return (Vec::new(), None, String::new());
    };
    let page = rdm_ai::html::scan_page(&observation.body_text(), &base);
    (page.links, page.title, page.text)
}

fn format_observation_summary(label: &str, record: &ObservationRecord) -> String {
    let observation = &record.observation;
    let mut summary = format!(
        "observation {}: {label} status={} content-type={} bytes={} truncated={} final-url={}",
        observation.obs_id,
        observation.status,
        observation.content_type().unwrap_or("-"),
        observation.body.len(),
        observation.truncated,
        observation.final_url
    );
    if let Some(title) = &record.title {
        summary.push_str(&format!("\ntitle: {}", truncate(title, 200)));
    }
    if !record.links.is_empty() {
        summary.push_str(&format!("\nscanned links ({}):", record.links.len()));
        for (index, link) in record.links.iter().enumerate() {
            if summary.chars().count() > 1200 {
                summary.push_str("\n… more links truncated");
                break;
            }
            summary.push_str(&format!("\n[{index}] {link}"));
        }
    }
    if !record.text.is_empty() {
        summary.push_str("\ntext: ");
        summary.push_str(&truncate(&record.text, 400));
    } else if !observation.body.is_empty() {
        summary.push_str("\nbody-preview: ");
        summary.push_str(&truncate(&observation.body_text(), 800));
    }
    summary
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn verified_video(content_type: &str, final_url: &str) -> Verification {
        Verification {
            status: crate::contract::VerificationStatus::Verified,
            method: crate::contract::VerificationMethod::RangedGet,
            http_status: Some(206),
            final_url: Some(final_url.to_owned()),
            content_type: Some(content_type.to_owned()),
            size_bytes: Some(45_932_387),
            supports_ranges: Some(true),
            etag: None,
            last_modified: None,
            evidence: vec!["GET range 0-1023 -> 206 video/mp4".to_owned()],
            checked_ms: 0,
        }
    }

    #[test]
    fn exploration_resource_name_gets_extension_from_verified_content_type() {
        // Real Bilibili case: the explorer emitted the page title as `name`.
        let candidate = ManifestCandidate {
            url: "https://cdn.example.com/upgcxcode/05/11/x/stream?e=sig".to_owned(),
            evidence: "obs_1#/data/durl/0/url".to_owned(),
            name: Some("饿急眼了煮两包泡面 (720P)".to_owned()),
            kind: Some("video".to_owned()),
            one_time: false,
            single_stream: false,
        };
        let verification = verified_video("video/mp4", &candidate.url);
        let item = build_resource_item(&candidate, verification);
        assert_eq!(item.name.as_deref(), Some("饿急眼了煮两包泡面 (720P).mp4"));
        assert_eq!(item.content_type.as_deref(), Some("video/mp4"));
    }

    #[test]
    fn exploration_resource_name_uses_signed_url_suffix_when_no_content_type() {
        let candidate = ManifestCandidate {
            url: "https://cdn.example.com/42924311105-1-192.mp4?e=sig".to_owned(),
            evidence: "obs_1".to_owned(),
            name: Some("标题 (1080P)".to_owned()),
            kind: Some("video".to_owned()),
            one_time: false,
            single_stream: false,
        };
        let mut verification = verified_video("", &candidate.url);
        verification.content_type = None;
        let item = build_resource_item(&candidate, verification);
        assert_eq!(item.name.as_deref(), Some("标题 (1080P).mp4"));
    }

    #[test]
    fn exploration_resource_name_keeps_existing_extension() {
        let candidate = ManifestCandidate {
            url: "https://cdn.example.com/a.mp4".to_owned(),
            evidence: "obs_1".to_owned(),
            name: Some("already.mp4".to_owned()),
            kind: Some("video".to_owned()),
            one_time: false,
            single_stream: false,
        };
        let item = build_resource_item(&candidate, verified_video("video/mp4", &candidate.url));
        assert_eq!(item.name.as_deref(), Some("already.mp4"));
    }

    fn observation(obs_id: &str, request_url: &str, final_url: &str, body: &[u8]) -> Observation {
        Observation {
            obs_id: obs_id.to_owned(),
            request_url: request_url.to_owned(),
            final_url: final_url.to_owned(),
            status: 200,
            headers: Default::default(),
            body: body.to_vec(),
            truncated: false,
        }
    }

    fn state_with(observations: Vec<Observation>) -> ExploreState {
        let mut state = ExploreState::default();
        for observation in observations {
            state.observe(observation);
        }
        state
    }

    #[test]
    fn evidence_matches_request_and_final_url() {
        let state = state_with(vec![observation(
            "obs_1",
            "https://example.com/start",
            "https://example.com/final",
            b"nothing here",
        )]);
        assert!(bind_evidence(&state, "https://example.com/start", "obs_1").is_ok());
        assert!(bind_evidence(&state, "https://example.com/final", "obs_1").is_ok());
        assert!(bind_evidence(&state, "https://example.com/other", "obs_1").is_err());
    }

    #[test]
    fn evidence_matches_body_text_and_page_links() {
        let html = br#"<html><head><title>Downloads</title></head><body>
            <a href="/files/a.bin">a</a>
            <a href="https://cdn.example.com/b.bin">b</a>
          </body></html>"#;
        let state = state_with(vec![observation(
            "obs_2",
            "https://example.com/page",
            "https://example.com/page",
            html,
        )]);
        // Direct body-text match.
        assert!(bind_evidence(&state, "https://cdn.example.com/b.bin", "obs_2").is_ok());
        // Page link index (relative links were resolved against the final URL).
        assert!(bind_evidence(&state, "https://example.com/files/a.bin", "obs_2#links/0").is_ok());
        assert!(bind_evidence(&state, "https://cdn.example.com/b.bin", "obs_2#links/1").is_ok());
        // Wrong index / wrong URL / fabricated URL are rejected.
        assert!(bind_evidence(&state, "https://example.com/files/a.bin", "obs_2#links/1").is_err());
        assert!(bind_evidence(&state, "https://evil.example/x.bin", "obs_2#links/0").is_err());
        assert!(bind_evidence(&state, "https://evil.example/x.bin", "obs_2").is_err());
    }

    #[test]
    fn evidence_matches_json_pointer_values() {
        let body = br#"{"file":{"url":"https://cdn.example.com/a.bin"},"count":3}"#;
        let state = state_with(vec![observation(
            "obs_3",
            "https://api.example.com/meta",
            "https://api.example.com/meta",
            body,
        )]);
        assert!(bind_evidence(&state, "https://cdn.example.com/a.bin", "obs_3#/file/url").is_ok());
        assert!(bind_evidence(&state, "https://cdn.example.com/a.bin", "obs_3#/file").is_err());
        assert!(bind_evidence(&state, "https://cdn.example.com/a.bin", "obs_3#/count").is_err());
        assert!(bind_evidence(&state, "https://cdn.example.com/a.bin", "obs_3#/missing").is_err());
    }

    #[test]
    fn evidence_requires_a_known_observation() {
        let state = state_with(vec![observation(
            "obs_4",
            "https://example.com/x",
            "https://example.com/x",
            b"x",
        )]);
        let error = bind_evidence(&state, "https://example.com/x", "obs_999").unwrap_err();
        assert!(error.contains("unknown observation"), "{error}");
        let error = bind_evidence(&state, "https://example.com/x", "nope").unwrap_err();
        assert!(error.contains("obs_"), "{error}");
    }

    #[test]
    fn observation_retention_is_bounded() {
        let mut state = ExploreState::default();
        for index in 0..(MAX_OBSERVATIONS + 10) {
            state.observe(observation(
                &format!("obs_{index}"),
                "https://example.com/page",
                "https://example.com/page",
                b"<html><body>x</body></html>",
            ));
        }
        assert_eq!(state.observations.len(), MAX_OBSERVATIONS);
        // The oldest entries were evicted.
        assert!(state.find("obs_0").is_none());
        assert!(state
            .find(&format!("obs_{}", MAX_OBSERVATIONS + 9))
            .is_some());
    }

    #[test]
    fn ai_errors_map_to_contract_kinds() {
        assert_eq!(
            ai_error(&AiError::NotConfigured).kind,
            ResolveErrorKind::ModelUnavailable
        );
        assert_eq!(
            ai_error(&AiError::Status {
                status: 401,
                body: "nope".to_owned()
            })
            .kind,
            ResolveErrorKind::ModelUnavailable
        );
        assert_eq!(
            ai_error(&AiError::Empty).kind,
            ResolveErrorKind::ModelUnavailable
        );
        assert_eq!(
            ai_error(&AiError::Json("bad".to_owned())).kind,
            ResolveErrorKind::ModelProtocolError
        );
    }

    #[test]
    fn token_usage_accumulates_across_model_calls() {
        let mut diagnostics = Diagnostics::default();
        record_usage(&mut diagnostics, TokenUsage::single(120, 30));
        record_usage(
            &mut diagnostics,
            TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 10,
                calls: 1,
                estimated: true,
            },
        );
        assert_eq!(diagnostics.llm_prompt_tokens, 127);
        assert_eq!(diagnostics.llm_completion_tokens, 33);
        assert_eq!(diagnostics.llm_total_tokens, 160);
        assert!(diagnostics.llm_estimated);

        // A zero usage (e.g. `NotConfigured` before any HTTP call) changes nothing.
        record_usage(&mut diagnostics, TokenUsage::ZERO);
        assert_eq!(diagnostics.llm_total_tokens, 160);
        assert_eq!(diagnostics.llm_prompt_tokens, 127);
    }
}
