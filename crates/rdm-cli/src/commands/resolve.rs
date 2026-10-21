//! `rdm resolve` — turn a page/share link into a verified resource manifest
//! without creating a download task (contract v1.2 §3.1/§5, `rdm-resolve`).
//!
//! Boundaries:
//! * the local recipe library lives under `<RDM state dir>/resolve/recipes`
//!   (`rdm_ipc::state_dir()` = `%LOCALAPPDATA%\RDM` on Windows);
//! * the resolver policy is derived from the effective agent policy
//!   (`policy.json` of the task store): `allowed_hosts`/`allow_private_hosts`;
//!   with no policy file the resolver's safe defaults apply (public hosts
//!   only, loopback/private denied);
//! * resolution never writes tasks and never consumes the URL, so a following
//!   `rdm download` is always required — unless the caller passes `--download`,
//!   which hands the first verified resource straight to the engine through
//!   `rdm_core::download_with_meta` (no second probe; one-time/single-stream
//!   resources stay one rangeless `GET`, `docs/architecture.md` §11.6).
//! * mode=explore uses the user's `<state>/ai.json` provider (or
//!   `RDM_AI_API_KEY`); the API key never appears in any output or error.

use std::path::{Path, PathBuf};

use rdm_agent::{AgentErrorInfo, ErrorCode, Policy};
use rdm_core::{DownloadHandle, DownloadSummary, RemoteMeta};
use rdm_resolve::{
    Budget, MethodStatus, RecipeHealth, ResolveErrorKind, ResolveMode, ResolveOutcome,
    ResolveRequest, ResolvedCredentials, ResolverService, ResourceItem, RunStatus,
};
use serde_json::{json, Value};

use crate::cli::{ResolveArgs, ResolveModeArg};
use crate::output::{engine_error, invalid_request, OutputMode, Reporter};
use crate::util;

pub async fn run(mode: OutputMode, store_root: &Path, args: ResolveArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "resolve");

    // Effective agent policy: a corrupt policy.json is an error (never
    // silently ignored), a missing one means "resolver safe defaults".
    let policy = match util::load_effective_policy(store_root) {
        Ok(policy) => policy,
        Err(error) => return reporter.finish_err(error),
    };
    let download = args.download;
    let directory = args.dir.clone();
    let request = match build_request(args) {
        Ok(request) => request,
        Err(error) => return reporter.finish_err(error),
    };
    // Exploration uses the user's own provider (`<state_dir>/ai.json` or
    // `RDM_AI_API_KEY`). The resolver only *uses* it when configured; the key
    // can never reach any output path (contract §11.9, v1.3).
    let mut resolver =
        ResolverService::with_state_dir(&rdm_ipc::state_dir(), resolver_policy(policy.as_ref()))
            .with_ai_config(rdm_ai::AiConfig::load(&rdm_ipc::state_dir()));
    let outcome = resolver.resolve(request).await;
    let mut data = serde_json::to_value(&outcome).unwrap_or(Value::Null);
    // v1.6 §8.6: additive, optional method health for library recipes. The
    // registry must be queried *after* `resolve()` (the service writes the
    // health side effect best-effort inside that call).
    let recipe_health = library_recipe_health(&resolver, &outcome);
    if let Some(health) = &recipe_health {
        if let Some(object) = data.as_object_mut() {
            object.insert(
                "recipe_health".to_string(),
                serde_json::to_value(health).unwrap_or(Value::Null),
            );
        }
    }

    if reporter.is_text() {
        print_outcome(&outcome);
        print_recipe_health_warning(&outcome, recipe_health.as_ref());
    }
    if reporter.is_ndjson() {
        reporter.event("resolve", data.clone());
    }

    match outcome_error(&outcome) {
        Some(error) => reporter.finish_err(error),
        None => {
            if !download {
                return reporter.finish_ok(data);
            }
            run_download(&mut reporter, outcome, data, directory, policy.as_ref()).await
        }
    }
}

// ---------------------------------------------------------------------------
// Method health (v1.6 §8.6)
// ---------------------------------------------------------------------------

/// Optional health of the local method library recipe behind this outcome.
///
/// Only recipes that came from the method library (`recipe.state =
/// matched|published` **and** still present in the registry) have health.
/// Builtin direct-file verification (`recipe = null`) and exploration drafts
/// (`state = draft`) are omitted, so `recipe_health` is purely additive.
fn library_recipe_health(
    resolver: &ResolverService,
    outcome: &ResolveOutcome,
) -> Option<RecipeHealth> {
    let recipe = outcome.recipe.as_ref()?;
    if !matches!(recipe.state.as_str(), "matched" | "published") {
        return None;
    }
    // `health()` returns Untested defaults for unknown ids; only report health
    // for recipes that actually exist in the loaded library.
    resolver.registry().get(&recipe.id, None)?;
    Some(resolver.registry().health(&recipe.id))
}

fn method_status_name(status: MethodStatus) -> &'static str {
    match status {
        MethodStatus::Ok => "ok",
        MethodStatus::Degraded => "degraded",
        MethodStatus::Broken => "broken",
        MethodStatus::Untested => "untested",
    }
}

/// Text mode only: a degraded/broken method appends a warning line on
/// **stderr**, keeping `--output json` stdout exactly one JSON envelope.
fn print_recipe_health_warning(outcome: &ResolveOutcome, health: Option<&RecipeHealth>) {
    let (Some(health), Some(recipe)) = (health, outcome.recipe.as_ref()) else {
        return;
    };
    if !matches!(health.status, MethodStatus::Degraded | MethodStatus::Broken) {
        return;
    }
    let kind = health
        .last_failure
        .as_ref()
        .map(|failure| rdm_resolve::error_kind_name(&failure.kind))
        .unwrap_or_else(|| "unknown".to_string());
    crate::errln!(
        "warning: recipe {} is {} (last failure: {kind})",
        recipe.id,
        method_status_name(health.status)
    );
}

// ---------------------------------------------------------------------------
// resolve → download hand-off (`--download --dir`, architecture §11.6)
// ---------------------------------------------------------------------------

/// Hand the first verified resource to the engine.
///
/// `rdm_core::download_with_meta` reuses the resolver's verified metadata, so
/// the engine issues **no probe**: the whole path is "verification (HEAD +
/// bounded GET) → transfer (GETs)" and a one-time/single-stream resource is
/// exactly one additional rangeless `GET`.
///
/// With an explicit `policy.json` the same URL/directory/size/overwrite gates
/// as `rdm download` run *before* any byte is written: `--download` must not be
/// a way around the agent policy (security boundary, not a convenience).
async fn run_download(
    reporter: &mut Reporter,
    outcome: ResolveOutcome,
    mut data: Value,
    directory: Option<PathBuf>,
    policy: Option<&Policy>,
) -> u8 {
    let Some(item) = outcome
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.downloadable().next())
    else {
        return reporter.finish_err(no_downloadable_error(&outcome));
    };
    let directory =
        directory.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let meta = remote_meta_from_item(item);
    let request =
        item.to_download_request(Some(directory.clone()), &ResolvedCredentials::default());
    if let Some(policy) = policy {
        if let Err(error) = pre_download_policy_checks(policy, item, &meta, &request, &directory) {
            return reporter.finish_err(error);
        }
    }

    match rdm_core::download_with_meta(request, meta, None, DownloadHandle::new()).await {
        Ok(summary) => {
            let summary_json = serde_json::to_value(&summary).unwrap_or(Value::Null);
            if reporter.is_text() {
                print_download(&summary);
            }
            if reporter.is_ndjson() {
                reporter.event("download", summary_json.clone());
            }
            if let Some(object) = data.as_object_mut() {
                object.insert("download".to_string(), summary_json);
            }
            reporter.finish_ok(data)
        }
        Err(error) => {
            if reporter.is_ndjson() {
                reporter.event("download_failed", json!({ "error": error.to_string() }));
            }
            reporter.finish_err(engine_error(&error))
        }
    }
}

/// Run the same agent-policy pre-checks `rdm download` performs, using the
/// resolver's verified metadata instead of a fresh probe.
///
/// Order matches `rdm download`: URL/host + destination-directory first, then
/// the size gate, then the overwrite conflict gate. Errors keep the existing
/// exit-code semantics (`policy_denied`/`needs_approval`/`conflict` → exit 4).
fn pre_download_policy_checks(
    policy: &Policy,
    item: &ResourceItem,
    meta: &RemoteMeta,
    request: &rdm_core::DownloadRequest,
    dest_dir: &Path,
) -> Result<(), AgentErrorInfo> {
    util::policy_precheck(policy, &item.url, Some(dest_dir))?;
    match meta.total_bytes {
        Some(total_bytes) => util::policy_gate_size(policy, Some(total_bytes))?,
        // `rdm download` streams unknown-length transfers and enforces
        // `max_bytes_per_task` through its progress monitor; the resolver
        // hand-off has no such monitor, so it must refuse rather than silently
        // bypass the size cap.
        None => return Err(unknown_length_policy_error(policy)),
    }
    let target =
        util::candidate_target_path(dest_dir, request.file_name.as_deref(), &meta.file_name);
    util::policy_overwrite_gate(policy, &target)
}

/// Policy refusal for a verified resource whose length is unknown.
///
/// `policy_denied` (not `needs_approval`): there is no approval token flow for
/// `rdm resolve --download`, and the policy itself is not asking for approval —
/// the command simply cannot enforce the size boundary, so `rdm download`
/// (which monitors unknown-length transfers) is the supported path.
fn unknown_length_policy_error(policy: &Policy) -> AgentErrorInfo {
    AgentErrorInfo::new(
        ErrorCode::PolicyDenied,
        "size cannot be gated by policy: the resolved resource has no Content-Length",
    )
    .with_hint(
        "use `rdm download <url>` (it monitors unknown-length transfers against \
         max_bytes_per_task), or run `rdm resolve` without `--download` and gate manually",
    )
    .with_details(json!({
        "reason": "unknown_length",
        "max_bytes_per_task": policy.max_bytes_per_task,
    }))
}

/// Reconstruct the engine's [`RemoteMeta`] from the resolver's verification so
/// `download_with_meta` can skip its own probe (`docs/architecture.md` §11.6).
///
/// `http_version` is intentionally `None`: the resolver does not claim the
/// protocol the transfer will negotiate.
fn remote_meta_from_item(item: &ResourceItem) -> RemoteMeta {
    let verification = &item.verification;
    let final_url = verification
        .final_url
        .clone()
        .or_else(|| item.final_url.clone())
        .unwrap_or_else(|| item.url.clone());
    let file_name = item
        .name
        .clone()
        .or_else(|| file_name_from_url(&final_url))
        .or_else(|| file_name_from_url(&item.url))
        .unwrap_or_else(|| "download".to_string());
    RemoteMeta {
        final_url,
        file_name,
        total_bytes: item.size_bytes.or(verification.size_bytes),
        supports_ranges: item
            .supports_ranges
            .or(verification.supports_ranges)
            .unwrap_or(false),
        content_type: item
            .content_type
            .clone()
            .or_else(|| verification.content_type.clone()),
        etag: item.etag.clone().or_else(|| verification.etag.clone()),
        last_modified: item
            .last_modified
            .clone()
            .or_else(|| verification.last_modified.clone()),
        http_version: None,
    }
}

/// Last non-empty path segment of `url` (the engine's fallback file name).
fn file_name_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let segment = parsed.path_segments()?.next_back()?.trim();
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_owned())
    }
}

fn print_download(summary: &DownloadSummary) {
    crate::outln!("downloaded:    {}", summary.path.display());
    crate::outln!(
        "  size:        {} ({} bytes), {} connection(s), peak {}",
        util::format_bytes(summary.total_bytes),
        summary.total_bytes,
        summary.connections,
        summary.peak_connections,
    );
    crate::outln!(
        "  elapsed:     {} ms, {}",
        summary.elapsed_ms,
        util::format_speed(summary.avg_speed_bps),
    );
    crate::outln!(
        "  ranges:      {}; resumed chunks: {}",
        if summary.range_supported { "yes" } else { "no" },
        summary.resumed_chunks,
    );
    if let Some(sha256) = &summary.sha256 {
        crate::outln!("  sha256:      {sha256}");
    }
}

// ---------------------------------------------------------------------------
// Request / policy mapping
// ---------------------------------------------------------------------------

/// Map the shared agent policy onto the resolver's network scope.
///
/// Mirrors the checks `rdm probe`/`rdm download` enforce: the host allow-list
/// and the private/loopback opt-in. When no policy file exists the resolver
/// keeps its own safe defaults (contract §2.3/§3.1: `policy_denied` for
/// private hosts unless explicitly allowed).
fn resolver_policy(policy: Option<&Policy>) -> rdm_resolve::ResolvePolicy {
    match policy {
        Some(policy) => rdm_resolve::ResolvePolicy {
            allowed_hosts: policy.allowed_hosts.clone(),
            allow_private_hosts: policy.allow_private_hosts,
            max_redirects: 5,
        },
        None => rdm_resolve::ResolvePolicy::default(),
    }
}

fn build_request(args: ResolveArgs) -> Result<ResolveRequest, AgentErrorInfo> {
    let url = args.url.trim().to_string();
    if url.is_empty() {
        return Err(invalid_request("`url` must not be empty"));
    }
    let goal = match args.goal {
        Some(goal) => {
            let goal = goal.trim().to_string();
            if goal.is_empty() || goal.chars().count() > 512 {
                return Err(invalid_request("`--goal` must be 1..=512 characters"));
            }
            Some(goal)
        }
        None => None,
    };
    if let Some(max_resources) = args.max_resources {
        if !(1..=40).contains(&max_resources) {
            return Err(invalid_request(
                "`--max-resources` must be between 1 and 40",
            ));
        }
    }
    if let Some(max_llm_calls) = args.max_llm_calls {
        if !(1..=32).contains(&max_llm_calls) {
            return Err(invalid_request(
                "`--max-llm-calls` must be between 1 and 32",
            ));
        }
    }
    let budget = build_budget(
        args.budget_ms,
        args.budget_requests,
        args.budget_bytes,
        args.budget_redirects,
    )?;
    Ok(ResolveRequest {
        url,
        goal,
        mode: match args.mode {
            ResolveModeArg::Auto => ResolveMode::Auto,
            ResolveModeArg::RecipeOnly => ResolveMode::RecipeOnly,
            ResolveModeArg::Explore => ResolveMode::Explore,
        },
        recipe_id: args.recipe,
        budget,
        max_resources: args.max_resources,
        max_llm_calls: args.max_llm_calls,
    })
}

/// Build the per-run budget; each explicit flag is range-checked against the
/// contract (§3.1) so an agent gets a structured `invalid_request` instead of
/// a silently clamped run.
fn build_budget(
    wall_ms: Option<u64>,
    http_requests: Option<u32>,
    bytes_read: Option<u64>,
    redirects: Option<u32>,
) -> Result<Option<Budget>, AgentErrorInfo> {
    if wall_ms.is_none() && http_requests.is_none() && bytes_read.is_none() && redirects.is_none() {
        return Ok(None);
    }
    let mut budget = Budget::default();
    if let Some(value) = wall_ms {
        if !(1_000..=300_000).contains(&value) {
            return Err(invalid_request(
                "`--budget-ms` must be between 1000 and 300000",
            ));
        }
        budget.wall_ms = value;
    }
    if let Some(value) = http_requests {
        if !(1..=64).contains(&value) {
            return Err(invalid_request(
                "`--budget-requests` must be between 1 and 64",
            ));
        }
        budget.http_requests = value;
    }
    if let Some(value) = bytes_read {
        if !(65_536..=33_554_432).contains(&value) {
            return Err(invalid_request(
                "`--budget-bytes` must be between 65536 and 33554432",
            ));
        }
        budget.bytes_read = value;
    }
    if let Some(value) = redirects {
        if value > 10 {
            return Err(invalid_request(
                "`--budget-redirects` must be between 0 and 10",
            ));
        }
        budget.redirects = value;
    }
    Ok(Some(budget))
}

// ---------------------------------------------------------------------------
// Outcome → stable error taxonomy (contract §2.3)
// ---------------------------------------------------------------------------

/// `None` means the run is a success for the CLI exit-code contract
/// (`succeeded`/`partial` → exit 0). `partial` still carries a typed
/// `diagnostics.error` for the agent to inspect.
fn outcome_error(outcome: &ResolveOutcome) -> Option<AgentErrorInfo> {
    if matches!(outcome.status, RunStatus::Succeeded | RunStatus::Partial) {
        return None;
    }
    Some(typed_failure(outcome))
}

/// `--download` needs at least one resource the engine may consume; a
/// resolution that only produced unverified candidates is a failure even
/// though plain `rdm resolve` would exit 0 with `status=partial`.
fn no_downloadable_error(outcome: &ResolveOutcome) -> AgentErrorInfo {
    let Some(error) = outcome.diagnostics.error.as_ref() else {
        return AgentErrorInfo::new(
            ErrorCode::ResolveFailed,
            format!(
                "--download: resolution ended with status {:?} and no downloadable resource",
                outcome.status
            ),
        )
        .with_hint(
            "Re-run without `--download` to inspect `data.manifest.resources`, or resolve a \
             different URL/recipe whose verification is `verified`/`head_only_verified`.",
        )
        .with_details(json!({
            "kind": "no_resources",
            "retryable": false,
            "run_id": outcome.run_id,
            "status": outcome.status,
        }));
    };
    let mut info = typed_failure(outcome);
    info.message = format!("--download: no downloadable resource ({})", error.detail);
    info.hint = Some(
        "Re-run without `--download` to inspect `data.manifest.resources`, or fix the \
         recipe/URL so a resource verifies as `verified`/`head_only_verified`."
            .to_string(),
    );
    info
}

/// Project a non-success [`ResolveOutcome`] onto the stable agent error
/// taxonomy (contract §2.3).
fn typed_failure(outcome: &ResolveOutcome) -> AgentErrorInfo {
    let diagnostics = &outcome.diagnostics;
    let Some(error) = diagnostics.error.as_ref() else {
        // Defensive: a non-success status without a typed error must still be
        // a stable resolve_failed, never a silent ok.
        return AgentErrorInfo::new(
            ErrorCode::ResolveFailed,
            format!("resolution ended with status {:?}", outcome.status),
        )
        .with_hint("Re-run with `--output json` to inspect the full ResolveOutcome.")
        .with_details(json!({
            "kind": "internal",
            "retryable": false,
            "run_id": outcome.run_id,
            "status": outcome.status,
        }));
    };

    let code = match error.kind {
        ResolveErrorKind::InvalidUrl | ResolveErrorKind::UnsupportedScheme => ErrorCode::InvalidUrl,
        ResolveErrorKind::PolicyDenied => ErrorCode::PolicyDenied,
        ResolveErrorKind::NetworkError => ErrorCode::NetworkError,
        ResolveErrorKind::Timeout => ErrorCode::Timeout,
        _ => ErrorCode::ResolveFailed,
    };
    let retryable = error.is_transient();
    let hint = match error.kind {
        ResolveErrorKind::RecipeNoMatch => {
            "No local recipe matched. Add one under <state_dir>/resolve/recipes, or resolve with \
             `--recipe <id>` after inspecting `rdm capabilities`."
        }
        ResolveErrorKind::RecipeStale => {
            "The recipe no longer matches the site (structure drift). Update the recipe or use \
             `--mode explore`."
        }
        ResolveErrorKind::ExploreUnavailable | ResolveErrorKind::ModelUnavailable => {
            "mode=explore needs a configured AI provider: set `enabled=true` + `api_key` in \
             <state_dir>/ai.json (or RDM_AI_API_KEY). Without it use `--mode auto`/`recipe-only`."
        }
        ResolveErrorKind::ModelProtocolError => {
            "The configured model did not return a valid JsonAction (after retrying). Check the \
             provider/model in <state_dir>/ai.json, or raise `--max-llm-calls`."
        }
        ResolveErrorKind::NoResources => {
            "Exploration found no verifiable resource. Try a more specific `--goal`, raise \
             `--budget-*`, or install a recipe for the site."
        }
        ResolveErrorKind::NotAFile | ResolveErrorKind::EmptyBody => {
            "The URL returned a page/JSON/empty body instead of a file. Resolve the share page with \
             a recipe, or use `rdm probe` on a direct file URL."
        }
        ResolveErrorKind::BudgetExhausted | ResolveErrorKind::RecipeBudgetExceeded => {
            "Raise `--budget-ms/--budget-requests/--budget-bytes` or simplify the input URL."
        }
        ResolveErrorKind::PolicyDenied => {
            "Add the host to policy.json allowed_hosts, or set allow_private_hosts=true."
        }
        _ => "Inspect `data.diagnostics` in the JSON envelope; re-run `rdm resolve --output json`.",
    };
    let mut message = if diagnostics.summary.trim().is_empty() {
        error.detail.clone()
    } else {
        diagnostics.summary.clone()
    };
    if !message.contains(&error.detail) {
        message = format!("{message} ({})", error.detail);
    }
    let mut info = AgentErrorInfo::new(code, message)
        .with_hint(hint)
        .with_details(json!({
            "kind": error.kind,
            "retryable": retryable,
            "run_id": outcome.run_id,
            "status": outcome.status,
            "recipe": outcome.recipe,
            "diagnostics": diagnostics,
        }));
    // `resolve_failed` is not retryable by default; transient resolver kinds
    // advertise `retryable=true` in both the code and details (contract §2.3).
    info.retryable = retryable;
    info
}

// ---------------------------------------------------------------------------
// Human-readable output
// ---------------------------------------------------------------------------

fn print_outcome(outcome: &ResolveOutcome) {
    crate::outln!("status:        {:?}", outcome.status);
    crate::outln!("run id:        {}", outcome.run_id);
    if let Some(recipe) = &outcome.recipe {
        crate::outln!("recipe:        {}@{}", recipe.id, recipe.version);
    }
    if !outcome.diagnostics.summary.is_empty() {
        crate::outln!("summary:       {}", outcome.diagnostics.summary);
    }
    if let Some(manifest) = &outcome.manifest {
        crate::outln!(
            "resources:     {} (manifest {})",
            manifest.resources.len(),
            manifest.manifest_id
        );
        for resource in &manifest.resources {
            crate::outln!("  - {}", resource.url);
            crate::outln!(
                "    verification: {:?}; size: {}; ranges: {}",
                resource.verification.status,
                match resource.size_bytes {
                    Some(bytes) => format!("{} ({} bytes)", util::format_bytes(bytes), bytes),
                    None => "unknown".to_string(),
                },
                match resource.supports_ranges {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                }
            );
            crate::outln!(
                "    transfer: single_stream={} one_time={} resumable={}",
                resource.transfer.single_stream,
                resource.transfer.one_time,
                resource.transfer.resumable
            );
        }
    }
    crate::outln!(
        "diagnostics:   {} step(s), {} request(s), {} bytes, {} ms, llm_calls={}",
        outcome.diagnostics.steps,
        outcome.diagnostics.http_requests,
        outcome.diagnostics.bytes_read,
        outcome.diagnostics.elapsed_ms,
        outcome.diagnostics.llm_calls
    );
    if let Some(error) = &outcome.diagnostics.error {
        crate::errln!("resolve error: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdm_resolve::{Diagnostics, RecipeRef, ResolveError};

    fn diagnostic(kind: ResolveErrorKind) -> Diagnostics {
        Diagnostics {
            error: Some(ResolveError::new(kind, "boom")),
            ..Diagnostics::default()
        }
    }

    fn failed_outcome(kind: ResolveErrorKind) -> ResolveOutcome {
        ResolveOutcome::failed("run_1", ResolveError::new(kind, "boom"), diagnostic(kind))
    }

    #[test]
    fn budget_flags_are_range_checked() {
        assert!(build_budget(None, None, None, None).unwrap().is_none());
        let budget = build_budget(Some(5_000), Some(3), Some(65_536), Some(1))
            .unwrap()
            .unwrap();
        assert_eq!(budget.wall_ms, 5_000);
        assert_eq!(budget.http_requests, 3);
        assert_eq!(budget.bytes_read, 65_536);
        assert_eq!(budget.redirects, 1);
        for (wall, requests, bytes, redirects) in [
            (Some(999), None, None, None),
            (None, Some(0), None, None),
            (None, None, Some(1024), None),
            (None, None, None, Some(11)),
        ] {
            let error = build_budget(wall, requests, bytes, redirects).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
        }
    }

    #[test]
    fn failed_outcomes_map_to_stable_codes() {
        assert_eq!(
            outcome_error(&failed_outcome(ResolveErrorKind::InvalidUrl))
                .unwrap()
                .code,
            ErrorCode::InvalidUrl
        );
        assert_eq!(
            outcome_error(&failed_outcome(ResolveErrorKind::UnsupportedScheme))
                .unwrap()
                .code,
            ErrorCode::InvalidUrl
        );
        assert_eq!(
            outcome_error(&failed_outcome(ResolveErrorKind::PolicyDenied))
                .unwrap()
                .code,
            ErrorCode::PolicyDenied
        );
        assert_eq!(
            outcome_error(&failed_outcome(ResolveErrorKind::NetworkError))
                .unwrap()
                .code,
            ErrorCode::NetworkError
        );
        let resolve_failed =
            outcome_error(&failed_outcome(ResolveErrorKind::RecipeNoMatch)).unwrap();
        assert_eq!(resolve_failed.code, ErrorCode::ResolveFailed);
        assert!(!resolve_failed.retryable);
        assert_eq!(
            resolve_failed.details.as_ref().unwrap()["kind"],
            "recipe_no_match"
        );
        assert!(resolve_failed.details.as_ref().unwrap()["run_id"].is_string());
    }

    #[test]
    fn partial_and_succeeded_are_success_for_exit_codes() {
        let partial = ResolveOutcome {
            run_id: "run_1".to_string(),
            status: RunStatus::Partial,
            manifest: None,
            recipe: Some(RecipeRef {
                id: "r".to_string(),
                version: 1,
                state: "matched".to_string(),
            }),
            diagnostics: Diagnostics::default(),
        };
        assert!(outcome_error(&partial).is_none());
        let mut succeeded = partial;
        succeeded.status = RunStatus::Succeeded;
        assert!(outcome_error(&succeeded).is_none());
    }

    /// `--download` must fail for a `partial` run that has no downloadable
    /// resource even though plain `rdm resolve` treats `partial` as success.
    #[test]
    fn partial_without_downloadable_resource_fails_download() {
        let mut partial = ResolveOutcome {
            run_id: "run_2".to_string(),
            status: RunStatus::Partial,
            manifest: None,
            recipe: None,
            diagnostics: diagnostic(ResolveErrorKind::NotAFile),
        };
        partial.diagnostics.summary = "candidate did not verify".to_string();
        let error = no_downloadable_error(&partial);
        assert_eq!(error.code, ErrorCode::ResolveFailed);
        assert!(error
            .message
            .starts_with("--download: no downloadable resource"));
        assert_eq!(
            error.details.as_ref().unwrap()["kind"],
            "not_a_file",
            "{error:?}"
        );

        // No typed error at all: still a stable resolve_failed, never ok.
        partial.diagnostics.error = None;
        let error = no_downloadable_error(&partial);
        assert_eq!(error.code, ErrorCode::ResolveFailed);
        assert_eq!(error.details.as_ref().unwrap()["kind"], "no_resources");
    }

    #[test]
    fn max_llm_calls_flag_is_range_checked() {
        assert!(build_request(resolve_args("https://example.com/a", Some(1))).is_ok());
        assert!(build_request(resolve_args("https://example.com/a", Some(32))).is_ok());
        let error = build_request(resolve_args("https://example.com/a", Some(0))).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        let error = build_request(resolve_args("https://example.com/a", Some(33))).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn remote_meta_is_reconstructed_from_verification() {
        let item: ResourceItem = serde_json::from_value(json!({
            "resource_id": "res_1",
            "url": "https://cdn.example.com/a/file.bin?token=1",
            "final_url": "https://cdn.example.com/a/redirected.bin",
            "size_bytes": 4096,
            "supports_ranges": true,
            "verification": {
                "status": "verified",
                "method": "ranged_get",
                "final_url": "https://cdn.example.com/final.bin",
                "size_bytes": 4096,
                "supports_ranges": true,
                "content_type": "application/octet-stream",
                "etag": "\"v2\"",
                "last_modified": "Wed, 21 Oct 2015 07:28:00 GMT"
            },
            "transfer": {}
        }))
        .unwrap();
        let meta = remote_meta_from_item(&item);
        // Verification metadata supplies the final URL (and is never overridden
        // by a stale manifest field).
        assert_eq!(meta.final_url, "https://cdn.example.com/final.bin");
        // ...and the file name falls back to the final URL's last segment.
        assert_eq!(meta.file_name, "final.bin");
        assert_eq!(meta.total_bytes, Some(4096));
        assert!(meta.supports_ranges);
        assert_eq!(
            meta.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(meta.etag.as_deref(), Some("\"v2\""));
        assert_eq!(
            meta.last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
        // The transfer negotiates its own HTTP version; the resolver does not
        // claim one.
        assert!(meta.http_version.is_none());
    }

    #[test]
    fn unknown_length_under_policy_is_refused_not_silently_allowed() {
        let policy = Policy::default();
        let error = unknown_length_policy_error(&policy);
        assert_eq!(error.code, ErrorCode::PolicyDenied);
        assert_eq!(error.details.as_ref().unwrap()["reason"], "unknown_length");
        assert!(
            error.hint.as_deref().unwrap().contains("rdm download"),
            "{error:?}"
        );
    }

    #[test]
    fn file_name_from_url_uses_the_last_segment() {
        assert_eq!(
            file_name_from_url("https://cdn.example.com/a/b/file.bin?token=1").as_deref(),
            Some("file.bin")
        );
        assert_eq!(file_name_from_url("https://cdn.example.com/"), None);
        assert_eq!(file_name_from_url("not a url"), None);
    }

    fn resolve_args(url: &str, max_llm_calls: Option<u32>) -> ResolveArgs {
        ResolveArgs {
            url: url.to_string(),
            goal: None,
            mode: ResolveModeArg::Auto,
            recipe: None,
            max_resources: None,
            budget_ms: None,
            budget_requests: None,
            budget_bytes: None,
            budget_redirects: None,
            max_llm_calls,
            download: false,
            dir: None,
        }
    }

    // -- v1.6 §8.6 method health -------------------------------------------

    const LIBRARY_RECIPE: &str = r#"{
        "schema": "rdm.recipe/v1",
        "id": "r1",
        "version": 1,
        "match": { "hosts": ["example.com"] },
        "steps": [
            { "id": "s", "op": "http_request", "args": { "url": "https://example.com/a" } }
        ],
        "emit": { "url": "https://example.com/a" }
    }"#;

    fn service_with_recipe() -> ResolverService {
        let recipe = rdm_resolve::Recipe::from_json(LIBRARY_RECIPE).unwrap();
        let mut registry = rdm_resolve::RecipeRegistry::empty();
        registry.insert(recipe).unwrap();
        ResolverService::new(registry, rdm_resolve::ResolvePolicy::default())
    }

    fn outcome_with_recipe(state: Option<&str>) -> ResolveOutcome {
        ResolveOutcome {
            run_id: "run_1".to_string(),
            status: RunStatus::Partial,
            manifest: None,
            recipe: state.map(|state| RecipeRef {
                id: "r1".to_string(),
                version: 1,
                state: state.to_string(),
            }),
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn library_recipe_health_only_for_library_recipes() {
        let service = service_with_recipe();
        // matched/published recipes from the library report health (Untested
        // when nothing has been recorded yet).
        for state in ["matched", "published"] {
            let health = library_recipe_health(&service, &outcome_with_recipe(Some(state)))
                .unwrap_or_else(|| panic!("{state} recipe must report health"));
            assert_eq!(health.status, MethodStatus::Untested);
        }
        // Direct-link (recipe = null) and exploration drafts are omitted.
        assert!(library_recipe_health(&service, &outcome_with_recipe(None)).is_none());
        assert!(library_recipe_health(&service, &outcome_with_recipe(Some("draft"))).is_none());
        // A matched reference whose recipe is not in the library is omitted.
        let mut unknown = outcome_with_recipe(Some("matched"));
        unknown.recipe.as_mut().unwrap().id = "not-in-library".to_string();
        assert!(library_recipe_health(&service, &unknown).is_none());
    }

    #[test]
    fn method_status_names_are_snake_case() {
        assert_eq!(method_status_name(MethodStatus::Ok), "ok");
        assert_eq!(method_status_name(MethodStatus::Degraded), "degraded");
        assert_eq!(method_status_name(MethodStatus::Broken), "broken");
        assert_eq!(method_status_name(MethodStatus::Untested), "untested");
    }
}
