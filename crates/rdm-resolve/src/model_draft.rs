//! Model-assisted recipe drafts (`docs/architecture.md` §11.11, P3b-b).
//!
//! When the deterministic [`crate::draft::distill`] cannot express a successful
//! exploration's action chain as a recipe, the resolver may spend **one**
//! additional model call to ask the user's configured provider for an
//! `rdm.recipe/v1` draft. The model output is treated as untrusted data:
//!
//! * the prompt only contains the (redacted) source URL, up to 40 sanitized
//!   action lines and the observed emit URLs — never headers, bodies, cookies
//!   or API keys;
//! * the generated recipe is parsed, matched against the source URL and scanned
//!   for credential headers / sensitive query parameter names before it is
//!   accepted;
//! * it is saved as a **draft** (never published): its `id`/`version` are forced,
//!   an `assert` is appended when missing, `sample_input` is filled in when
//!   `tests` is empty, and `scope_hint` stays `narrow`;
//! * publishing still has to pass the R1–R6 replay gates
//!   ([`crate::draft::evaluate_publish`]), so a hallucinated URL is rejected at
//!   replay time as well.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use url::Url;

use crate::contract::{ResolveRequest, ResourceItem};
use crate::draft::{
    draft_id, DraftAction, RecipeDraft, DRAFT_RECIPE_VERSION, DRAFT_SCHEMA, MAX_DRAFT_STEPS,
};
use crate::recipe::{Recipe, RecipeStep, RecipeTest, RECIPE_SCHEMA};
use crate::trace::{redact_excerpt, redact_url};
use rdm_ai::{AiClient, ChatMessage};

/// Maximum number of action summary lines sent to the model.
pub const MAX_PROMPT_ACTIONS: usize = 40;

/// Query parameter names that must never appear in a model-generated draft.
const SENSITIVE_QUERY_NAMES: [&str; 7] = [
    "token",
    "sig",
    "signature",
    "expires",
    "key",
    "secret",
    "apikey",
];

/// Header names that would leak credentials if replayed from a draft.
const CREDENTIAL_HEADERS: [&str; 3] = ["authorization", "cookie", "proxy-authorization"];

/// Build the two chat messages used for one model-assisted draft call.
///
/// Everything is sanitized: URLs go through [`redact_url`] (query/fragment
/// dropped), free-form strings go through [`redact_excerpt`], and the action
/// summaries never contain headers or bodies (the [`DraftAction`] record does
/// not capture them in the first place).
pub fn draft_messages(request: &ResolveRequest, actions: &[DraftAction]) -> Vec<ChatMessage> {
    let source_url = redact_url(request.url.trim());
    let observed_urls = observed_emit_urls(actions);
    let mut lines: Vec<String> = actions
        .iter()
        .take(MAX_PROMPT_ACTIONS)
        .enumerate()
        .map(|(index, action)| format!("{}. {}", index + 1, action_summary(action)))
        .collect();
    if actions.len() > MAX_PROMPT_ACTIONS {
        lines.pop();
        lines.push(format!(
            "{}. … {} more action(s) omitted",
            MAX_PROMPT_ACTIONS,
            actions.len() - (MAX_PROMPT_ACTIONS - 1)
        ));
    }
    let actions_block = if lines.is_empty() {
        "(no actions recorded)".to_owned()
    } else {
        lines.join("\n")
    };
    let urls_block = if observed_urls.is_empty() {
        "(none)".to_owned()
    } else {
        observed_urls
            .iter()
            .map(|url| format!("- {url}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let system = format!(
        "You convert one successful link-exploration run into a single replayable RDM recipe.\n\
         Reply with exactly one JSON object and nothing else (no prose, no markdown fences).\n\
         The object must follow schema {RECIPE_SCHEMA} with `id`, `version`, `match`, `steps`, `emit` \
         and optional `tests` fields. Rules:\n\
         - allowed step ops: http_request, json_query, regex_capture, url_join, template, assert, \
           branch, bounded_each;\n\
         - at most {MAX_DRAFT_STEPS} steps and at least one top-level assert step;\n\
         - never include request headers or request bodies; never include cookies, authorization \
           tokens or API keys, and never put signed/token query parameters into URLs;\n\
         - `emit.url` must be one of the observed emit URLs (or a {{variable}} derived from a step);\n\
         - `match.hosts` should contain the source host and `match.url_pattern` should narrow the \
           recipe to the source URL shape;\n\
         - variables are only produced by step `assign`; templates support {{{{var}}}} placeholders;\n\
         - `emit.name` MUST include a real file suffix (derive it from Content-Type or the emitted URL;\
           never a bare page title).\n\
         If unsure, emit a minimal recipe: one http_request step, one assert step, and the observed \
         emit URL."
    );
    let user = format!(
        "SOURCE URL (redacted): {source_url}\n\
         OBSERVED EMIT URLS (redacted, verified during the run):\n{urls_block}\n\
         ACTIONS (sanitized; URLs have query/fragment removed):\n{actions_block}\n\n\
         Produce the recipe JSON now."
    );
    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

/// One sanitized action line for the prompt.
fn action_summary(action: &DraftAction) -> String {
    match action {
        DraftAction::HttpRequest {
            method,
            url,
            max_bytes,
            ..
        } => format!(
            "http_request {method} {} max_bytes={max_bytes}",
            redact_url(url)
        ),
        DraftAction::PageRead { url, .. } => format!("page_read GET {}", redact_url(url)),
        DraftAction::JsonQuery { obs_id, pointer } => {
            format!("json_query {obs_id} {pointer:?}")
        }
        DraftAction::RegexCapture {
            obs_id,
            pattern,
            group,
        } => format!(
            "regex_capture {obs_id} pattern={:?} group={group:?}",
            redact_excerpt(pattern)
        ),
        DraftAction::EmitManifest { candidates } => {
            let urls: Vec<String> = candidates
                .iter()
                .map(|candidate| redact_url(&candidate.url))
                .collect();
            format!(
                "emit_manifest {} candidate(s): {}",
                urls.len(),
                urls.join(", ")
            )
        }
        DraftAction::Other => "other".to_owned(),
    }
}

/// Observed emit URLs (redacted, deduplicated, insertion order).
fn observed_emit_urls(actions: &[DraftAction]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut urls = Vec::new();
    for action in actions {
        if let DraftAction::EmitManifest { candidates } = action {
            for candidate in candidates {
                let redacted = redact_url(&candidate.url);
                if seen.insert(redacted.clone()) {
                    urls.push(redacted);
                }
            }
        }
    }
    urls
}

/// Parse one model reply and validate/force it into a [`RecipeDraft`].
///
/// Unparseable output, a recipe that does not match the source URL, credential
/// headers, sensitive query parameters or more than [`MAX_DRAFT_STEPS`] steps
/// are all rejected with a human-readable reason (the caller only logs it).
/// Returns no draft when the reply has no JSON object or the JSON is not a
/// valid `rdm.recipe/v1` recipe.
pub fn parse_and_validate(
    reply: &str,
    request: &ResolveRequest,
    run_id: &str,
    created_ms: u64,
    actions: &[DraftAction],
    emitted: &[ResourceItem],
) -> Result<Option<RecipeDraft>, String> {
    let Some(json) = rdm_ai::extract_json_object(reply) else {
        return Ok(None);
    };
    let Ok(recipe) = Recipe::from_json(json) else {
        return Ok(None);
    };
    validate_generated(recipe, request, run_id, created_ms, actions, emitted).map(Some)
}

/// Spend one model call to generate a draft. `Err` is reserved for transport /
/// client failures; unusable model output yields `Ok(None)`.
///
/// Kept as the compatibility wrapper for the original signature; callers that
/// need the token accounting of the attempt (the resolver's own
/// `service::attach_draft` does, so `Diagnostics::llm_calls` and the token
/// counters stay in sync) use [`generate_with_usage`] instead.
pub async fn generate(
    ai: &rdm_ai::AiConfig,
    request: &ResolveRequest,
    run_id: &str,
    created_ms: u64,
    actions: &[DraftAction],
    emitted: &[ResourceItem],
) -> Result<Option<RecipeDraft>, String> {
    generate_with_usage(ai, request, run_id, created_ms, actions, emitted)
        .await
        .0
}

/// Like [`generate`], but also returns the token usage of the attempt.
///
/// The usage is drained from the client's ledger **before** the reply is
/// inspected, so it is returned for every path the provider could bill:
/// success, unusable output and transport/parse errors. A client construction
/// failure (no request sent) returns [`rdm_ai::TokenUsage::ZERO`].
pub async fn generate_with_usage(
    ai: &rdm_ai::AiConfig,
    request: &ResolveRequest,
    run_id: &str,
    created_ms: u64,
    actions: &[DraftAction],
    emitted: &[ResourceItem],
) -> (Result<Option<RecipeDraft>, String>, rdm_ai::TokenUsage) {
    let client = match AiClient::new(ai.clone()) {
        Ok(client) => client,
        Err(error) => {
            return (
                Err(format!("cannot create AI client: {error}")),
                rdm_ai::TokenUsage::ZERO,
            )
        }
    };
    let messages = draft_messages(request, actions);
    let reply = client.chat_messages(&messages).await;
    // A failed call is still billed: drain the ledger before mapping the error.
    let usage = client.take_usage();
    let outcome = match reply {
        Ok(reply) => parse_and_validate(&reply, request, run_id, created_ms, actions, emitted),
        Err(error) => Err(format!("model-assisted draft call failed: {error}")),
    };
    (outcome, usage)
}

/// Validate a model-generated [`Recipe`] and force it into a sanitized
/// [`RecipeDraft`] (pure; unit-testable without a network).
pub fn validate_generated(
    mut recipe: Recipe,
    request: &ResolveRequest,
    run_id: &str,
    created_ms: u64,
    _actions: &[DraftAction],
    emitted: &[ResourceItem],
) -> Result<RecipeDraft, String> {
    let source_url = request.url.trim().to_owned();
    let source = Url::parse(&source_url)
        .map_err(|error| format!("invalid source URL {source_url:?}: {error}"))?;
    if recipe.matches(&source).is_none() {
        return Err(format!(
            "generated recipe does not match the source URL {source_url:?}"
        ));
    }
    if recipe.steps.len() > MAX_DRAFT_STEPS {
        return Err(format!(
            "generated recipe has {} steps (limit {MAX_DRAFT_STEPS})",
            recipe.steps.len()
        ));
    }
    reject_credential_headers(&recipe)?;
    reject_sensitive_query_parameters(&recipe)?;

    let target = emitted
        .iter()
        .find(|item| item.verification.status.is_downloadable())
        .ok_or_else(|| "no verified resource to base the draft on".to_owned())?;
    let final_url = target
        .final_url
        .clone()
        .unwrap_or_else(|| target.url.clone());
    let host = source
        .host_str()
        .ok_or_else(|| "source URL has no host".to_owned())?
        .to_owned();
    let id = draft_id(&host, &source_url, &final_url);

    // Forced identity: a model can never choose an id/version that would let it
    // overwrite an existing recipe.
    recipe.id = id.clone();
    recipe.version = DRAFT_RECIPE_VERSION;

    // R6: the draft must contain at least one top-level assert.
    if !recipe.steps.iter().any(|step| step.op == "assert") {
        if recipe.steps.len() >= MAX_DRAFT_STEPS {
            return Err(format!(
                "cannot append a required assert: already at the {MAX_DRAFT_STEPS}-step limit"
            ));
        }
        let step_id = unique_step_id(&recipe.steps, "auto_assert");
        recipe.steps.push(RecipeStep {
            id: step_id,
            op: "assert".to_owned(),
            args: serde_json::json!({
                "kind": "matches",
                "value": recipe.emit.url.clone(),
                "pattern": "^https?://",
            }),
            expect: None,
            assign: BTreeMap::new(),
            on_fail: None,
            when: None,
        });
    }
    // Tests: keep model-provided ones, otherwise fill the positive sample.
    if recipe.tests.is_empty() {
        recipe.tests.push(RecipeTest {
            name: "sample_input".to_owned(),
            input_url: source_url.clone(),
            expect_host_suffix: None,
            expect_no_match: false,
            skip: None,
        });
    }
    recipe
        .validate()
        .map_err(|error| format!("generated recipe is invalid: {error}"))?;

    let notes = vec![
        "model-assisted draft from exploration".to_owned(),
        "review before publishing".to_owned(),
        format!("source_run_id: {run_id}"),
    ];
    Ok(RecipeDraft {
        schema: DRAFT_SCHEMA.to_owned(),
        id,
        version: DRAFT_RECIPE_VERSION,
        created_ms,
        source_run_id: run_id.to_owned(),
        source_url,
        scope_hint: "narrow".to_owned(),
        notes,
        recipe,
    })
}

fn unique_step_id(steps: &[RecipeStep], base: &str) -> String {
    if !steps.iter().any(|step| step.id == base) {
        return base.to_owned();
    }
    for index in 1..=steps.len() + 2 {
        let candidate = format!("{base}_{index}");
        if !steps.iter().any(|step| step.id == candidate) {
            return candidate;
        }
    }
    format!("{base}_{}", steps.len() + 3)
}

fn reject_credential_headers(recipe: &Recipe) -> Result<(), String> {
    for step in &recipe.steps {
        if step.op != "http_request" {
            continue;
        }
        let Some(headers) = step.args.get("headers").and_then(Value::as_object) else {
            continue;
        };
        for name in headers.keys() {
            let lower = name.trim().to_ascii_lowercase();
            if CREDENTIAL_HEADERS.iter().any(|blocked| *blocked == lower) {
                return Err(format!(
                    "generated recipe includes the forbidden credential header {name:?}"
                ));
            }
        }
    }
    Ok(())
}

fn reject_sensitive_query_parameters(recipe: &Recipe) -> Result<(), String> {
    let value = serde_json::to_value(recipe)
        .map_err(|error| format!("cannot inspect generated recipe: {error}"))?;
    let mut strings = Vec::new();
    collect_strings(&value, &mut strings);
    for text in strings {
        if let Some(name) = sensitive_query_name(&text) {
            return Err(format!(
                "generated recipe contains the sensitive query parameter {name:?}"
            ));
        }
    }
    Ok(())
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

/// First sensitive query parameter name found in `text` (absolute or relative
/// URL-ish strings), using token-boundary matching (`access_token` matches
/// `token`, `monkey` does not match `key`).
fn sensitive_query_name(text: &str) -> Option<String> {
    let queries: Vec<String> = if let Ok(url) = Url::parse(text) {
        url.query_pairs()
            .map(|(name, _)| name.to_string())
            .collect()
    } else if let Some((_, query)) = text.split_once('?') {
        query
            .split('#')
            .next()
            .unwrap_or_default()
            .split('&')
            .filter_map(|pair| pair.split_once('=').map(|(name, _)| name.to_owned()))
            .collect()
    } else {
        return None;
    };
    for name in queries {
        let lower = name.trim().to_ascii_lowercase();
        if lower.is_empty() {
            continue;
        }
        let mut matches = false;
        for sensitive in SENSITIVE_QUERY_NAMES {
            if lower == sensitive {
                matches = true;
                break;
            }
            if lower
                .split(|character: char| !character.is_ascii_alphanumeric())
                .any(|part| part == sensitive)
            {
                matches = true;
                break;
            }
        }
        if matches {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{
        CredentialsHint, ResolveMode, TransferHint, Verification, VerificationStatus,
    };
    use crate::draft::DraftCandidate;

    fn request_for(url: &str) -> ResolveRequest {
        ResolveRequest {
            url: url.to_owned(),
            mode: ResolveMode::Auto,
            ..ResolveRequest::default()
        }
    }

    fn verified_item(url: &str) -> ResourceItem {
        ResourceItem {
            resource_id: "res_test".to_owned(),
            url: url.to_owned(),
            final_url: Some(url.to_owned()),
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
            evidence: vec!["obs_page#links/0".to_owned()],
            notes: Vec::new(),
        }
    }

    fn actions(page_url: &str, file_url: &str) -> Vec<DraftAction> {
        vec![
            DraftAction::PageRead {
                url: page_url.to_owned(),
                obs_id: Some("obs_page".to_owned()),
            },
            DraftAction::EmitManifest {
                candidates: vec![DraftCandidate {
                    url: file_url.to_owned(),
                    evidence: "obs_page#links/0".to_owned(),
                    name: None,
                    one_time: false,
                    single_stream: false,
                }],
            },
        ]
    }

    fn model_recipe_json(source_url: &str, emit_url: &str, steps: Value) -> String {
        serde_json::json!({
            "schema": RECIPE_SCHEMA,
            "id": "model-picked-id",
            "version": 99,
            "match": { "hosts": [Url::parse(source_url).unwrap().host_str().unwrap()] },
            "steps": steps,
            "emit": { "url": emit_url }
        })
        .to_string()
    }

    fn http_step(url: &str) -> Value {
        serde_json::json!({
            "id": "fetch",
            "op": "http_request",
            "args": { "method": "GET", "url": url }
        })
    }

    fn assert_step() -> Value {
        serde_json::json!({
            "id": "check",
            "op": "assert",
            "args": { "kind": "matches", "value": "https://cdn.example.com/f.bin", "pattern": "^https?://" }
        })
    }

    #[test]
    fn prompt_is_sanitized_and_bounded() {
        let page_url = "https://example.com/page?token=super-secret#frag";
        let file_url = "https://cdn.example.com/f.bin?signature=signed-secret";
        let mut recorded = actions(page_url, file_url);
        recorded.push(DraftAction::HttpRequest {
            method: "GET".to_owned(),
            url: "https://api.example.com/data?key=api-secret".to_owned(),
            max_bytes: 4096,
            obs_id: Some("obs_api".to_owned()),
        });
        let messages = draft_messages(&request_for(page_url), &recorded);
        assert_eq!(messages.len(), 2);
        let all = format!("{}\n{}", messages[0].content, messages[1].content);
        for secret in ["super-secret", "signed-secret", "api-secret"] {
            assert!(!all.contains(secret), "prompt leaked {secret}: {all}");
        }
        assert!(all.contains("https://example.com/page"), "{all}");
        assert!(all.contains("https://cdn.example.com/f.bin"), "{all}");
        assert!(!all.to_ascii_lowercase().contains("bearer "), "{all}");
        // Action lines are bounded.
        let user_lines: Vec<&str> = messages[1].content.lines().collect();
        let action_lines = user_lines
            .iter()
            .filter(|line| line.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .count();
        assert!(action_lines <= MAX_PROMPT_ACTIONS, "{action_lines}");
    }

    #[test]
    fn prompt_truncates_long_action_lists_keeping_the_limit() {
        let mut recorded = Vec::new();
        for index in 0..(MAX_PROMPT_ACTIONS + 10) {
            recorded.push(DraftAction::JsonQuery {
                obs_id: format!("obs_{index}"),
                pointer: format!("/items/{index}"),
            });
        }
        let messages = draft_messages(&request_for("https://example.com/page"), &recorded);
        let number_lines = messages[1]
            .content
            .lines()
            .filter(|line| line.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .count();
        assert_eq!(number_lines, MAX_PROMPT_ACTIONS);
        assert!(
            messages[1].content.contains("omitted"),
            "{}",
            messages[1].content
        );
    }

    #[test]
    fn validate_forces_identity_and_fills_assert_and_tests() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let recipe = Recipe::from_json(&model_recipe_json(
            source,
            file,
            serde_json::json!([http_step(source)]),
        ))
        .expect("valid model recipe");
        let draft = validate_generated(
            recipe,
            &request_for(source),
            "run_model_1",
            1_700_000_000_000,
            &actions(source, file),
            &[verified_item(file)],
        )
        .expect("draft");

        assert_eq!(draft.schema, DRAFT_SCHEMA);
        assert_eq!(draft.scope_hint, "narrow");
        assert_eq!(draft.source_url, source);
        assert_eq!(draft.source_run_id, "run_model_1");
        assert_eq!(draft.version, DRAFT_RECIPE_VERSION);
        assert!(draft.id.starts_with("draft_example.com_"), "{}", draft.id);
        assert_eq!(draft.recipe.id, draft.id);
        assert_eq!(draft.recipe.version, DRAFT_RECIPE_VERSION);
        assert_eq!(draft.notes[0], "model-assisted draft from exploration");
        assert!(draft.notes[1].contains("review"));
        assert!(draft.notes.iter().any(|note| note.contains("run_model_1")));
        // Assert appended because the model only emitted a fetch step.
        assert_eq!(
            draft.recipe.steps.last().map(|step| step.op.as_str()),
            Some("assert")
        );
        // Positive sample test filled in.
        assert_eq!(draft.recipe.tests.len(), 1);
        assert_eq!(draft.recipe.tests[0].name, "sample_input");
        assert_eq!(draft.recipe.tests[0].input_url, source);
        draft.recipe.validate().expect("draft recipe is valid");
    }

    #[test]
    fn validate_keeps_model_assert_and_tests() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let mut value: Value = serde_json::from_str(&model_recipe_json(
            source,
            file,
            serde_json::json!([assert_step()]),
        ))
        .unwrap();
        value["tests"] =
            serde_json::json!([{ "name": "model-test", "input_url": "https://example.com/other" }]);
        let recipe = Recipe::from_json(&value.to_string()).expect("valid recipe");
        let draft = validate_generated(
            recipe,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .expect("draft");
        assert_eq!(draft.recipe.steps.len(), 1);
        assert_eq!(draft.recipe.steps[0].op, "assert");
        assert_eq!(draft.recipe.tests.len(), 1);
        assert_eq!(draft.recipe.tests[0].name, "model-test");
    }

    #[test]
    fn validate_rejects_non_matching_recipes() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let value = serde_json::json!({
            "schema": RECIPE_SCHEMA,
            "id": "x",
            "version": 1,
            "match": { "hosts": ["other.example.com"] },
            "steps": [assert_step()],
            "emit": { "url": file }
        });
        let recipe = Recipe::from_json(&value.to_string()).expect("valid recipe");
        let error = validate_generated(
            recipe,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .expect_err("must reject");
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn validate_rejects_credential_headers() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let steps = serde_json::json!([{
            "id": "fetch",
            "op": "http_request",
            "args": {
                "method": "GET",
                "url": source,
                "headers": { "Authorization": "Bearer secret" }
            }
        }]);
        let recipe = Recipe::from_json(&model_recipe_json(source, file, steps)).expect("recipe");
        let error = validate_generated(
            recipe,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .expect_err("must reject");
        assert!(error.contains("credential header"), "{error}");
    }

    #[test]
    fn validate_rejects_sensitive_query_parameters() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let steps = serde_json::json!([{
            "id": "fetch",
            "op": "http_request",
            "args": { "method": "GET", "url": "https://api.example.com/d?access_token=secret" }
        }]);
        let recipe = Recipe::from_json(&model_recipe_json(source, file, steps)).expect("recipe");
        let error = validate_generated(
            recipe,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .expect_err("must reject");
        assert!(error.contains("sensitive query parameter"), "{error}");
    }

    #[test]
    fn sensitive_query_name_uses_token_boundaries() {
        assert_eq!(
            sensitive_query_name("https://x/a?access_token=1").as_deref(),
            Some("access_token")
        );
        assert_eq!(
            sensitive_query_name("https://x/a?apikey=1").as_deref(),
            Some("apikey")
        );
        assert_eq!(sensitive_query_name("https://x/a?monkey=1"), None);
        assert_eq!(sensitive_query_name("https://x/a?page=2"), None);
        assert_eq!(
            sensitive_query_name("/relative?a=1&signature=2").as_deref(),
            Some("signature")
        );
    }

    #[test]
    fn validate_rejects_too_many_steps() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let steps: Vec<Value> = (0..=MAX_DRAFT_STEPS)
            .map(|index| {
                serde_json::json!({
                    "id": format!("s{index}"),
                    "op": "assert",
                    "args": { "kind": "matches", "value": "https://example.com/", "pattern": "^https?://" }
                })
            })
            .collect();
        let recipe = Recipe::from_json(&model_recipe_json(source, file, Value::Array(steps)))
            .expect("recipe");
        let error = validate_generated(
            recipe,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .expect_err("must reject");
        assert!(error.contains("steps"), "{error}");
    }

    #[test]
    fn parse_and_validate_returns_none_for_unusable_output() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let request = request_for(source);
        let recorded = actions(source, file);
        let emitted = [verified_item(file)];

        // No JSON object at all.
        assert!(parse_and_validate(
            "I cannot help with that",
            &request,
            "run",
            1,
            &recorded,
            &emitted
        )
        .expect("not a transport error")
        .is_none());
        // JSON, but not a valid recipe.
        assert!(
            parse_and_validate("{\"hello\":1}", &request, "run", 1, &recorded, &emitted)
                .expect("not a transport error")
                .is_none()
        );
        // Unknown operator fails Recipe::from_json validation.
        let unknown = model_recipe_json(
            source,
            file,
            serde_json::json!([{ "id": "x", "op": "made_up" }]),
        );
        assert!(
            parse_and_validate(&unknown, &request, "run", 1, &recorded, &emitted)
                .expect("not a transport error")
                .is_none()
        );
    }

    #[test]
    fn parse_and_validate_accepts_a_valid_reply() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let reply = format!(
            "```json\n{}\n```",
            model_recipe_json(
                source,
                file,
                serde_json::json!([http_step(source), assert_step()])
            )
        );
        let draft = parse_and_validate(
            &reply,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .expect("not a transport error")
        .expect("draft");
        assert_eq!(draft.recipe.id, draft.id);
    }

    // ------------------------------------------------------------------
    // Token accounting of the model-assisted draft call (P3b-b)
    // ------------------------------------------------------------------

    /// Minimal loopback OpenAI-compatible server: answer one drained chat
    /// request with `body` (HTTP `status`), then close.
    fn spawn_chat_mock(status: u16, body: String) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind mock AI");
        let address = listener.local_addr().expect("mock AI address");
        let base_url = format!("http://{address}/v1");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept mock AI request");
            let mut raw = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                raw.extend_from_slice(&chunk[..read]);
                let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&raw[..position]).to_ascii_lowercase();
                let content_length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() >= position + 4 + content_length {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        (base_url, handle)
    }

    fn mock_ai_config(base_url: &str) -> rdm_ai::AiConfig {
        rdm_ai::AiConfig {
            enabled: true,
            base_url: base_url.to_owned(),
            model: "mock-model".to_owned(),
            api_key: "test-key".to_owned(),
            timeout_secs: 5,
            // Loopback mock: not a bundled provider (no preset/no prices).
            provider: None,
            ..rdm_ai::AiConfig::default()
        }
    }

    /// Wrap `content` in an OpenAI-compatible completion, optionally with usage.
    fn chat_completion(content: &str, usage: Option<Value>) -> String {
        let mut body = serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
        });
        if let Some(usage) = usage {
            body["usage"] = usage;
        }
        body.to_string()
    }

    #[tokio::test]
    async fn generate_with_usage_returns_provider_usage_on_success() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let recipe_json = model_recipe_json(
            source,
            file,
            serde_json::json!([http_step(source), assert_step()]),
        );
        let reply = chat_completion(
            &recipe_json,
            Some(serde_json::json!({
                "prompt_tokens": 21,
                "completion_tokens": 9,
                "total_tokens": 30
            })),
        );
        let (base_url, server) = spawn_chat_mock(200, reply);
        let ai = mock_ai_config(&base_url);
        let (outcome, usage) = generate_with_usage(
            &ai,
            &request_for(source),
            "run_model_usage",
            7,
            &actions(source, file),
            &[verified_item(file)],
        )
        .await;
        server.join().expect("mock AI thread");

        let draft = outcome.expect("no transport error").expect("valid draft");
        assert_eq!(draft.source_run_id, "run_model_usage");
        assert_eq!(usage.prompt_tokens, 21);
        assert_eq!(usage.completion_tokens, 9);
        assert_eq!(usage.total_tokens, 30);
        assert_eq!(usage.calls, 1);
        assert!(!usage.estimated, "provider usage must stay exact");
    }

    #[tokio::test]
    async fn generate_with_usage_keeps_the_usage_of_an_unusable_reply() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        // The provider answered (and billed) but produced no JSON recipe.
        let reply = chat_completion(
            "I cannot help with that",
            Some(serde_json::json!({
                "prompt_tokens": 9,
                "completion_tokens": 3,
                "total_tokens": 12
            })),
        );
        let (base_url, server) = spawn_chat_mock(200, reply);
        let ai = mock_ai_config(&base_url);
        let (outcome, usage) = generate_with_usage(
            &ai,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .await;
        server.join().expect("mock AI thread");

        assert!(
            outcome
                .expect("unusable output is not a transport error")
                .is_none(),
            "an unusable reply yields no draft"
        );
        // …but the tokens the provider already charged are still reported.
        assert_eq!(usage.total_tokens, 12);
        assert_eq!(usage.calls, 1);
        assert!(!usage.estimated);
    }

    #[tokio::test]
    async fn generate_with_usage_estimates_when_the_provider_omits_usage() {
        let source = "https://example.com/page";
        let file = "https://cdn.example.com/f.bin";
        let reply = chat_completion("not a recipe", None);
        let (base_url, server) = spawn_chat_mock(200, reply);
        let ai = mock_ai_config(&base_url);
        let (outcome, usage) = generate_with_usage(
            &ai,
            &request_for(source),
            "run",
            1,
            &actions(source, file),
            &[verified_item(file)],
        )
        .await;
        server.join().expect("mock AI thread");

        assert!(outcome.expect("no transport error").is_none());
        assert!(usage.estimated, "missing provider usage must be estimated");
        assert_eq!(usage.calls, 1);
        assert!(usage.total_tokens > 0);
    }
}
