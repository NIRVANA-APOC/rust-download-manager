//! `rdm recipe` — local method-library management (v1.5, `docs/architecture.md`
//! §11.11).
//!
//! Layout (root: `<rdm_ipc::state_dir()>/resolve`):
//!
//! ```text
//! recipes/<safe-id>@<version>.json   # method files (registry root)
//! index.json                         # active_version + health
//! drafts/<safe-id>@<version>.json    # pending drafts (never matched)
//! ```
//!
//! * `list`/`show`/`check` are read-only (pure matching, no network);
//! * `rollback` only rewrites `active_version` in the index;
//! * `publish` replays a draft through the R1–R6 gates (real network for the
//!   samples) and, unless `--dry-run`, writes the method file + index and
//!   removes the draft;
//! * `export`/`import` move single methods between machines. Both **refuse
//!   credential-like content by default** (`authorization`/`cookie`/
//!   `proxy-authorization` headers, or sensitive query parameter names such as
//!   `token`/`sig`/`signature`/`expires`/`key`/`secret`/`apikey`) unless
//!   `--allow-sensitive` is passed. Import can add the method to the library,
//!   convert it to a local draft (`--as-draft`), or replay the recipe's own
//!   tests through the R1–R6 gates and publish only when they pass
//!   (`--publish`).
//!
//! No AI/model is involved anywhere in this command.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rdm_agent::{AgentErrorInfo, ErrorCode, Policy};
use rdm_resolve::draft::{self, RecipeDraft, DRAFT_SCHEMA};
use rdm_resolve::{Budget, Recipe, RecipeHealth, RecipeRegistry, RecipeStep, ResolvePolicy};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::cli::{
    RecipeArgs, RecipeCheckArgs, RecipeCommand, RecipeExportArgs, RecipeImportArgs,
    RecipePublishArgs, RecipeRollbackArgs, RecipeShowArgs,
};
use crate::output::{invalid_request, OutputMode, Reporter};
use crate::util;

/// Cap on reported failure rows in `recipe check` (same shape as selftest).
const MAX_CHECK_FAILURES: usize = 50;
/// Maximum accepted size of an imported recipe file (bytes).
const MAX_IMPORT_BYTES: u64 = 1024 * 1024;
/// Header names that carry credentials (case-insensitive).
const SENSITIVE_HEADERS: &[&str] = &["authorization", "cookie", "proxy-authorization"];
/// Query parameter names that commonly carry signed/expiring credentials.
const SENSITIVE_QUERY_NAMES: &[&str] = &[
    "token",
    "sig",
    "signature",
    "expires",
    "key",
    "secret",
    "apikey",
];

/// Result of one command: either a plain failure, a success, or a report that
/// keeps `data` but exits non-zero (strict check / rejected publish).
type CommandResult = Result<(Value, Option<AgentErrorInfo>), AgentErrorInfo>;

pub async fn run(mode: OutputMode, store_root: &Path, args: RecipeArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "recipe");
    let resolve_root = resolve_dir();
    let is_text = reporter.is_text();

    let result = match args.action {
        RecipeCommand::List => list(&resolve_root, is_text),
        RecipeCommand::Show(args) => show(&resolve_root, &args, is_text),
        RecipeCommand::Check(args) => check(&resolve_root, &args, is_text),
        RecipeCommand::Rollback(args) => rollback(&resolve_root, &args, is_text),
        RecipeCommand::Publish(args) => publish(&resolve_root, store_root, &args, is_text).await,
        RecipeCommand::Export(args) => export(&resolve_root, &args, is_text),
        RecipeCommand::Import(args) => import(&resolve_root, store_root, &args, is_text).await,
    };
    match result {
        Ok((data, Some(error))) => reporter.finish_report(data, Some(error)),
        Ok((data, None)) => reporter.finish_ok(data),
        Err(error) => reporter.finish_err(error),
    }
}

fn resolve_dir() -> PathBuf {
    rdm_ipc::state_dir().join("resolve")
}

fn recipes_root(resolve_root: &Path) -> PathBuf {
    resolve_root.join("recipes")
}

/// Load the local method library (tolerant: bad files are skipped).
fn load_registry(resolve_root: &Path) -> RecipeRegistry {
    RecipeRegistry::with_root(recipes_root(resolve_root))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(resolve_root: &Path, is_text: bool) -> CommandResult {
    let registry = load_registry(resolve_root);
    let mut recipes: Vec<Value> = Vec::new();
    let mut ids: Vec<String> = registry
        .all()
        .iter()
        .map(|recipe| recipe.id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    for id in &ids {
        let available_versions = versions_of(&registry, id);
        recipes.push(json!({
            "id": id,
            "active_version": registry.active_version(id),
            "available_versions": available_versions,
            // v0.15: per-version health map (may be empty; additive field).
            "versions": version_health_json(&registry, id),
            "health": health_json(&registry.health(id)),
        }));
    }
    let drafts: Vec<Value> = draft::list_drafts(resolve_root)
        .iter()
        .map(draft_summary)
        .collect();

    if is_text {
        crate::outln!("method library: {}", recipes_root(resolve_root).display());
        if recipes.is_empty() {
            crate::outln!("  (no recipes)");
        }
        for recipe in &recipes {
            let id = recipe["id"].as_str().unwrap_or("?");
            let active = match recipe["active_version"].as_u64() {
                Some(version) => format!("v{version}"),
                None => "none".to_string(),
            };
            let available = recipe["available_versions"]
                .as_array()
                .map(|versions| {
                    versions
                        .iter()
                        .filter_map(Value::as_u64)
                        .map(|version| format!("v{version}"))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let health = recipe["health"]["status"].as_str().unwrap_or("untested");
            let versions = version_health_summary(&registry, id);
            crate::outln!(
                "  {id}  active={active}  available=[{available}]  health={health}  versions=[{versions}]"
            );
        }
        crate::outln!("drafts: {}", drafts.len());
        for draft in &drafts {
            crate::outln!(
                "  {} v{}  scope={}  source={}  created_ms={}",
                draft["id"].as_str().unwrap_or("?"),
                draft["version"].as_u64().unwrap_or(0),
                draft["scope_hint"].as_str().unwrap_or("?"),
                draft["source_url"].as_str().unwrap_or("?"),
                draft["created_ms"].as_u64().unwrap_or(0),
            );
        }
    }

    Ok((
        json!({
            "resolve_dir": resolve_root.display().to_string(),
            "recipes": recipes,
            "drafts": drafts,
        }),
        None,
    ))
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

fn show(resolve_root: &Path, args: &RecipeShowArgs, is_text: bool) -> CommandResult {
    let registry = load_registry(resolve_root);
    let Some(recipe) = registry.get(&args.id, args.version) else {
        return Err(not_found_recipe(&args.id, args.version));
    };
    let available_versions = versions_of(&registry, &recipe.id);
    let version_health = version_health_json(&registry, &recipe.id);
    let active_version = registry.active_version(&recipe.id);
    let health = registry.health(&recipe.id);
    if is_text {
        crate::outln!(
            "recipe:        {}@{} ({})",
            recipe.id,
            recipe.version,
            if recipe.name.trim().is_empty() {
                "-"
            } else {
                recipe.name.as_str()
            }
        );
        crate::outln!(
            "active:        {}",
            active_version
                .map(|version| format!("v{version}"))
                .unwrap_or_else(|| "none".to_string())
        );
        crate::outln!(
            "available:     {}",
            available_versions
                .iter()
                .map(|version| format!("v{version}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        crate::outln!(
            "health:        {} (consecutive_failures={}, last_ok_ms={})",
            health.status.label_zh(),
            health.consecutive_failures,
            health
                .last_ok_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
        // v0.15: one line per recorded version (ascending version order).
        if let Some(versions) = registry.health_by_version(&recipe.id) {
            for (version, health) in versions {
                crate::outln!(
                    "  v{version}: {} (consecutive={})",
                    health.status.label_zh(),
                    health.consecutive_failures
                );
            }
        }
        crate::outln!("tests:         {}", recipe.tests.len());
    }
    let mut recipe_json = serde_json::to_value(recipe).unwrap_or(Value::Null);
    if let Value::Object(object) = &mut recipe_json {
        object.insert("versions".to_owned(), version_health);
    }
    Ok((
        json!({
            "recipe": recipe_json,
            "active_version": active_version,
            "available_versions": available_versions,
            "health": health_json(&health),
        }),
        None,
    ))
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

fn check(resolve_root: &Path, args: &RecipeCheckArgs, is_text: bool) -> CommandResult {
    let registry = load_registry(resolve_root);
    let drafts = draft::list_drafts(resolve_root);
    let mut report = CheckReport::default();
    for recipe in registry.all() {
        report.record(recipe);
    }
    for draft in &drafts {
        report.record(&draft.recipe);
    }
    let data = report.to_json();
    if is_text {
        crate::outln!(
            "recipes: {} checked / {} passed / {} failed / {} skipped",
            report.checked,
            report.passed,
            report.failed,
            report.skipped,
        );
        for failure in report.failures.iter().take(5) {
            crate::outln!(
                "  - {}@{} {}: {}",
                failure["recipe_id"].as_str().unwrap_or("?"),
                failure["version"].as_u64().unwrap_or(0),
                failure["test"].as_str().unwrap_or("?"),
                failure["message"].as_str().unwrap_or(""),
            );
        }
        if args.strict && report.failed > 0 {
            crate::errln!("strict: {} recipe self-test(s) failed", report.failed);
        }
    }
    let error = if args.strict && report.failed > 0 {
        Some(
            AgentErrorInfo::new(
                ErrorCode::ResolveFailed,
                format!(
                    "{} recipe/draft self-test(s) failed under --strict",
                    report.failed
                ),
            )
            .with_hint(
                "fix the recipe `tests` blocks (or the matching rules) and re-run; without \
                 `--strict` this command is report-only",
            )
            .with_details(json!({
                "checked": report.checked,
                "passed": report.passed,
                "failed": report.failed,
                "skipped": report.skipped,
                "failures": report.failures,
            })),
        )
    } else {
        None
    };
    Ok((data, error))
}

#[derive(Default)]
struct CheckReport {
    checked: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    failures: Vec<Value>,
}

impl CheckReport {
    fn record(&mut self, recipe: &Recipe) {
        self.checked += 1;
        for result in recipe.check_tests() {
            if result.skipped {
                self.skipped += 1;
            } else if result.ok {
                self.passed += 1;
            } else {
                self.failed += 1;
                if self.failures.len() < MAX_CHECK_FAILURES {
                    self.failures.push(json!({
                        "recipe_id": recipe.id,
                        "version": recipe.version,
                        "test": result.name,
                        "message": result.message,
                    }));
                }
            }
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "checked": self.checked,
            "passed": self.passed,
            "failed": self.failed,
            "skipped": self.skipped,
            "failures": self.failures,
        })
    }
}

// ---------------------------------------------------------------------------
// rollback
// ---------------------------------------------------------------------------

fn rollback(resolve_root: &Path, args: &RecipeRollbackArgs, is_text: bool) -> CommandResult {
    if args.version == 0 {
        return Err(invalid_request("`--version` must be >= 1"));
    }
    let mut registry = load_registry(resolve_root);
    registry
        .set_active_version(&args.id, args.version)
        .map_err(|message| {
            AgentErrorInfo::new(
                ErrorCode::InvalidRequest,
                format!("cannot roll back recipe `{}`: {message}", args.id),
            )
            .with_hint("run `rdm recipe list --output json` to see existing ids/versions")
            .with_details(json!({ "id": args.id, "version": args.version }))
        })?;
    let available_versions = versions_of(&registry, &args.id);
    let version_health = version_health_json(&registry, &args.id);
    if is_text {
        crate::outln!("rolled back: {} -> active v{}", args.id, args.version);
        crate::outln!(
            "available:   {}",
            available_versions
                .iter()
                .map(|version| format!("v{version}"))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    Ok((
        json!({
            "id": args.id,
            "active_version": args.version,
            "available_versions": available_versions,
            "versions": version_health,
        }),
        None,
    ))
}

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

async fn publish(
    resolve_root: &Path,
    store_root: &Path,
    args: &RecipePublishArgs,
    is_text: bool,
) -> CommandResult {
    if args.samples.len() > 64 || args.negatives.len() > 64 {
        return Err(invalid_request(
            "at most 64 `--sample` and 64 `--negative` URLs are accepted",
        ));
    }
    let draft = load_draft(resolve_root, &args.draft_id)?;
    let samples = if args.samples.is_empty() {
        vec![draft.source_url.clone()]
    } else {
        args.samples.clone()
    };
    if samples.is_empty() {
        return Err(invalid_request(
            "no replay sample: the draft has no `source_url` and no `--sample` was given",
        ));
    }

    // Resolver network scope = effective agent policy (host allow-list +
    // private-host opt-in); no policy file keeps the resolver safe defaults.
    let policy = util::load_effective_policy(store_root)?;
    let resolve_policy = resolver_policy(policy.as_ref());
    for url in samples.iter().chain(args.negatives.iter()) {
        let parsed = url::Url::parse(url).map_err(|error| {
            AgentErrorInfo::new(
                ErrorCode::InvalidUrl,
                format!("invalid sample URL `{url}`: {error}"),
            )
            .with_hint("pass absolute http(s) URLs")
        })?;
        resolve_policy
            .check_url(&parsed)
            .map_err(|error| map_resolve_error(&error))?;
    }

    let mut report = draft::evaluate_publish(
        &draft,
        &samples,
        &args.negatives,
        &resolve_policy,
        Budget::default(),
    )
    .await;
    let mut data = serde_json::to_value(&report).unwrap_or(Value::Null);
    annotate_publish_data(&mut data, &report, args, &samples);

    if is_text {
        print_publish(&data, args.dry_run);
    }

    if !report.required_passed {
        // A replay that hit the policy is a policy denial (exit 4), not a
        // generic failed gate (exit 2).
        if let Some(detail) = policy_denial_in_report(&data) {
            return Err(AgentErrorInfo::new(
                ErrorCode::PolicyDenied,
                format!("publish replay denied by policy: {detail}"),
            )
            .with_hint(
                "add the host to `policy.json` allowed_hosts (or allow_private_hosts) and \
                 re-run `rdm recipe publish --dry-run`",
            )
            .with_details(json!({
                "draft_id": draft.id,
                "required_passed": false,
                "gates": data["gates"],
            })));
        }
        return Ok((
            data.clone(),
            Some(publish_gate_error(
                &data,
                if args.dry_run {
                    "dry-run: publish gates not satisfied"
                } else {
                    "publish gates not satisfied; nothing was written"
                },
            )),
        ));
    }
    if args.dry_run {
        return Ok((data, None));
    }

    // Non-dry-run, all required gates passed: write method file + index and
    // remove the draft (the rdm-resolve API owns the atomic sequence).
    let mut registry = load_registry(resolve_root);
    match draft::publish(&mut registry, resolve_root, &draft, &mut report) {
        Ok(reference) => {
            data = serde_json::to_value(&report).unwrap_or(Value::Null);
            annotate_publish_data(&mut data, &report, args, &samples);
            if let Some(object) = data.as_object_mut() {
                object.insert("published".to_string(), json!(true));
                object.insert(
                    "recipe".to_string(),
                    serde_json::to_value(&reference).unwrap_or(Value::Null),
                );
            }
            if is_text {
                crate::outln!(
                    "published:    {}@{} (scope={})",
                    reference.id,
                    reference.version,
                    data["scope"].as_str().unwrap_or("?"),
                );
                crate::outln!("draft removed: {}", draft.id);
            }
            Ok((data, None))
        }
        Err(error) => Err(AgentErrorInfo::new(
            ErrorCode::Internal,
            format!("publish failed while writing the method library: {error}"),
        )
        .with_hint(
            "check that `<state>/resolve/recipes` and `index.json` are writable; the in-memory \
             registry was rolled back by rdm-resolve",
        )
        .with_details(json!({ "draft_id": draft.id, "required_passed": true }))),
    }
}

/// Merge the evaluation report with the CLI-side inputs (samples/scopes live in
/// the report; the CLI only adds convenience fields when the API omits them).
fn annotate_publish_data(
    data: &mut Value,
    report: &draft::PublishReport,
    args: &RecipePublishArgs,
    samples: &[String],
) {
    if let Some(object) = data.as_object_mut() {
        object
            .entry("draft_id".to_string())
            .or_insert_with(|| json!(report.draft_id.clone()));
        object
            .entry("required_passed".to_string())
            .or_insert_with(|| json!(report.required_passed));
        object
            .entry("dry_run".to_string())
            .or_insert_with(|| json!(args.dry_run));
        object
            .entry("samples".to_string())
            .or_insert_with(|| json!(samples));
        object
            .entry("negatives".to_string())
            .or_insert_with(|| json!(args.negatives));
    }
}

fn print_publish(data: &Value, dry_run: bool) {
    crate::outln!(
        "draft:         {} (scope={}, required_passed={})",
        data["draft_id"].as_str().unwrap_or("?"),
        data["scope"].as_str().unwrap_or("?"),
        data["required_passed"].as_bool().unwrap_or(false),
    );
    for gate in data["gates"].as_array().into_iter().flatten() {
        crate::outln!(
            "[{}] {:<3} {}: {}",
            if gate["pass"].as_bool().unwrap_or(false) {
                "pass"
            } else {
                "FAIL"
            },
            gate["id"].as_str().unwrap_or("?"),
            gate["name"].as_str().unwrap_or(""),
            gate["detail"].as_str().unwrap_or(""),
        );
    }
    if dry_run {
        crate::outln!("dry-run:       nothing written");
    }
}

fn publish_gate_error(data: &Value, message: &str) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::ResolveFailed, message)
        .with_hint(
            "run `rdm recipe publish <draft_id> --dry-run --output json` and inspect `data.gates`; \
             R1/R2/R3/R6 must pass, R4/R5 decide the scope",
        )
        .with_details(json!({
            "draft_id": data["draft_id"],
            "scope": data["scope"],
            "required_passed": data["required_passed"],
            "gates": data["gates"],
        }))
}

/// Extract a policy-denied replay failure from a serialized `PublishReport`.
fn policy_denial_in_report(data: &Value) -> Option<String> {
    let mut haystacks: Vec<String> = Vec::new();
    for gate in data["gates"].as_array().into_iter().flatten() {
        if let Some(detail) = gate["detail"].as_str() {
            haystacks.push(detail.to_owned());
        }
    }
    if let Some(error) = data.get("error") {
        haystacks.push(error.to_string());
    }
    haystacks.into_iter().find(|detail| {
        detail.contains("policy_denied")
            || detail.contains("PolicyDenied")
            || detail.contains("policy denied")
    })
}

fn load_draft(resolve_root: &Path, draft_id: &str) -> Result<RecipeDraft, AgentErrorInfo> {
    draft::load_draft(resolve_root, draft_id).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => not_found_draft(draft_id),
        _ => AgentErrorInfo::new(
            ErrorCode::Internal,
            format!("cannot read draft `{draft_id}`: {error}"),
        )
        .with_hint("run `rdm recipe list --output json` to list draft ids"),
    })
}

fn resolver_policy(policy: Option<&Policy>) -> ResolvePolicy {
    match policy {
        Some(policy) => ResolvePolicy {
            allowed_hosts: policy.allowed_hosts.clone(),
            allow_private_hosts: policy.allow_private_hosts,
            max_redirects: 5,
        },
        None => ResolvePolicy::default(),
    }
}

fn map_resolve_error(error: &rdm_resolve::ResolveError) -> AgentErrorInfo {
    use rdm_resolve::ResolveErrorKind;
    let code = match error.kind {
        ResolveErrorKind::PolicyDenied => ErrorCode::PolicyDenied,
        ResolveErrorKind::InvalidUrl | ResolveErrorKind::UnsupportedScheme => ErrorCode::InvalidUrl,
        ResolveErrorKind::NetworkError => ErrorCode::NetworkError,
        ResolveErrorKind::Timeout => ErrorCode::Timeout,
        _ => ErrorCode::ResolveFailed,
    };
    AgentErrorInfo::new(code, error.detail.clone()).with_details(json!({
        "kind": error.kind,
        "retryable": error.is_transient(),
    }))
}

// ---------------------------------------------------------------------------
// Shared projections / errors
// ---------------------------------------------------------------------------

fn versions_of(registry: &RecipeRegistry, id: &str) -> Vec<u32> {
    let mut versions: Vec<u32> = registry
        .all()
        .iter()
        .filter(|recipe| recipe.id == id)
        .map(|recipe| recipe.version)
        .collect();
    versions.sort_unstable();
    versions
}

fn health_json(health: &RecipeHealth) -> Value {
    serde_json::to_value(health).unwrap_or(Value::Null)
}

/// `{"1": RecipeHealth, "2": RecipeHealth}` (v0.15 per-version health map).
fn version_health_json(registry: &RecipeRegistry, id: &str) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(versions) = registry.health_by_version(id) {
        for (version, health) in versions {
            map.insert(version.to_string(), health_json(health));
        }
    }
    Value::Object(map)
}

/// `1:ok,2:degraded` (ascending version) for text mode; empty without history.
fn version_health_summary(registry: &RecipeRegistry, id: &str) -> String {
    registry
        .health_by_version(id)
        .map(|versions| {
            versions
                .iter()
                .map(|(version, health)| format!("{version}:{}", health.status.as_str()))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

fn draft_summary(draft: &RecipeDraft) -> Value {
    let value = serde_json::to_value(draft).unwrap_or(Value::Null);
    json!({
        "id": value["id"],
        "version": value["version"],
        "source_run_id": value["source_run_id"],
        "source_url": value["source_url"],
        "scope_hint": value["scope_hint"],
        "created_ms": value["created_ms"],
    })
}

fn not_found_recipe(id: &str, version: Option<u32>) -> AgentErrorInfo {
    let message = match version {
        Some(version) => format!("recipe `{id}` version {version} not found"),
        None => format!("recipe `{id}` not found"),
    };
    AgentErrorInfo::new(ErrorCode::NotFound, message)
        .with_hint("run `rdm recipe list --output json` to see ids and versions")
        .with_details(json!({ "id": id, "version": version }))
}

fn not_found_draft(draft_id: &str) -> AgentErrorInfo {
    AgentErrorInfo::new(
        ErrorCode::NotFound,
        format!("recipe draft `{draft_id}` not found"),
    )
    .with_hint("run `rdm recipe list --output json` (drafts section) or `rdm resolve --mode explore` to create one")
    .with_details(json!({ "draft_id": draft_id }))
}

// ---------------------------------------------------------------------------
// export (v1.6): single method → JSON, refusing credential-like content
// ---------------------------------------------------------------------------

fn export(resolve_root: &Path, args: &RecipeExportArgs, is_text: bool) -> CommandResult {
    let registry = load_registry(resolve_root);
    let Some(recipe) = registry.get(&args.id, args.version) else {
        return Err(not_found_recipe(&args.id, args.version));
    };
    if !args.allow_sensitive {
        let findings = sensitive_findings(recipe);
        if !findings.is_empty() {
            return Err(sensitive_error(
                &format!("recipe `{}` v{}", recipe.id, recipe.version),
                findings,
            ));
        }
    }
    let recipe_value = serde_json::to_value(recipe).unwrap_or(Value::Null);
    if let Some(out) = &args.out {
        if out.exists() && !args.force {
            return Err(conflict_error(
                &format!("export target `{}` already exists", out.display()),
                json!({ "path": out.display().to_string() }),
            ));
        }
        let text = serde_json::to_string_pretty(&recipe_value).map_err(|error| {
            AgentErrorInfo::new(
                ErrorCode::Internal,
                format!("cannot serialize recipe: {error}"),
            )
        })?;
        write_atomic_file(out, &text).map_err(|error| {
            AgentErrorInfo::new(
                ErrorCode::PermissionDenied,
                format!("cannot write `{}`: {error}", out.display()),
            )
            .with_hint("check that the target directory exists and is writable")
        })?;
        if is_text {
            crate::outln!(
                "exported:      {} v{} -> {}",
                recipe.id,
                recipe.version,
                out.display()
            );
        }
        return Ok((
            json!({
                "id": recipe.id,
                "version": recipe.version,
                "recipe": recipe_value,
                "path": out.display().to_string(),
            }),
            None,
        ));
    }
    if is_text {
        crate::outln!(
            "{}",
            serde_json::to_string_pretty(&recipe_value).unwrap_or_default()
        );
    }
    Ok((
        json!({
            "id": recipe.id,
            "version": recipe.version,
            "recipe": recipe_value,
        }),
        None,
    ))
}

// ---------------------------------------------------------------------------
// import (v1.6): file → library / draft / R1–R6 evaluate-and-publish
// ---------------------------------------------------------------------------

async fn import(
    resolve_root: &Path,
    store_root: &Path,
    args: &RecipeImportArgs,
    is_text: bool,
) -> CommandResult {
    let recipe = read_import_file(&args.file)?;
    if !args.allow_sensitive {
        let findings = sensitive_findings(&recipe);
        if !findings.is_empty() {
            return Err(sensitive_error(
                &format!("imported recipe `{}` v{}", recipe.id, recipe.version),
                findings,
            ));
        }
    }

    if args.as_draft {
        let draft = build_import_draft(&args.file, &recipe, &[]);
        let mut data = json!({
            "draft_id": draft.id,
            "recipe_id": recipe.id,
            "version": recipe.version,
            "dry_run": args.dry_run,
        });
        if args.dry_run {
            if is_text {
                crate::outln!(
                    "dry-run:       would write draft {} (no files written)",
                    draft.id
                );
            }
            return Ok((data, None));
        }
        let path = draft::save_draft(resolve_root, &draft).map_err(|error| {
            AgentErrorInfo::new(
                ErrorCode::PermissionDenied,
                format!("cannot write recipe draft: {error}"),
            )
            .with_hint("check that `<state>/resolve/drafts` is writable")
        })?;
        if let Some(object) = data.as_object_mut() {
            object.insert("path".to_string(), json!(path.display().to_string()));
        }
        if is_text {
            crate::outln!(
                "draft written: {} -> {} (method library unchanged)",
                draft.id,
                path.display()
            );
        }
        return Ok((data, None));
    }

    let mut registry = load_registry(resolve_root);
    let existing = registry.get(&recipe.id, Some(recipe.version));
    if existing.is_some_and(|existing| same_recipe(existing, &recipe)) {
        // Idempotent import: the exact id+version already exists.
        if is_text {
            crate::outln!(
                "already present: {} v{} (no changes)",
                recipe.id,
                recipe.version
            );
        }
        return Ok((
            json!({
                "imported": false,
                "already_present": true,
                "id": recipe.id,
                "version": recipe.version,
            }),
            None,
        ));
    }
    if existing.is_some() && !args.force {
        return Err(conflict_error(
            &format!("recipe `{}` v{}", recipe.id, recipe.version),
            json!({ "id": recipe.id, "version": recipe.version }),
        ));
    }

    if args.publish {
        return publish_imported(
            store_root,
            &args.file,
            &recipe,
            args,
            &mut registry,
            is_text,
        )
        .await;
    }

    if args.dry_run {
        if is_text {
            crate::outln!(
                "dry-run:       would import {} v{} (no files written)",
                recipe.id,
                recipe.version
            );
        }
        return Ok((
            json!({
                "imported": false,
                "already_present": false,
                "dry_run": true,
                "id": recipe.id,
                "version": recipe.version,
            }),
            None,
        ));
    }
    let path = write_method(&mut registry, &recipe)?;
    if is_text {
        crate::outln!(
            "imported:      {} v{} -> {}",
            recipe.id,
            recipe.version,
            path.display()
        );
    }
    Ok((
        json!({
            "imported": true,
            "already_present": false,
            "id": recipe.id,
            "version": recipe.version,
            "path": path.display().to_string(),
        }),
        None,
    ))
}

/// `import --publish`: replay the recipe's own `tests` through R1–R6 and write
/// the method only when the required gates pass.
async fn publish_imported(
    store_root: &Path,
    file: &Path,
    recipe: &Recipe,
    args: &RecipeImportArgs,
    registry: &mut RecipeRegistry,
    is_text: bool,
) -> CommandResult {
    let samples = positive_samples(recipe);
    if samples.is_empty() {
        return Err(invalid_request(
            "`--publish` needs at least one `expect_no_match=false` test with an `input_url`",
        )
        .with_hint(
            "add a `tests` entry such as {\"name\":\"sample\",\"input_url\":\"https://host/page\"} \
             (the recipe's tests are the publish replay samples)",
        ));
    }
    let negatives = negative_samples(recipe);
    let policy = util::load_effective_policy(store_root)?;
    let resolve_policy = resolver_policy(policy.as_ref());
    for url in samples.iter().chain(negatives.iter()) {
        let parsed = url::Url::parse(url).map_err(|error| {
            AgentErrorInfo::new(
                ErrorCode::InvalidUrl,
                format!("invalid test URL `{url}`: {error}"),
            )
            .with_hint("recipe tests must use absolute http(s) URLs")
        })?;
        resolve_policy
            .check_url(&parsed)
            .map_err(|error| map_resolve_error(&error))?;
    }

    let draft = build_import_draft(file, recipe, &samples);
    let report = draft::evaluate_publish(
        &draft,
        &samples,
        &negatives,
        &resolve_policy,
        Budget::default(),
    )
    .await;
    let report_value = serde_json::to_value(&report).unwrap_or(Value::Null);
    if is_text {
        print_report_gates(&report_value);
    }
    if !report.required_passed {
        if let Some(detail) = policy_denial_in_report(&report_value) {
            return Err(AgentErrorInfo::new(
                ErrorCode::PolicyDenied,
                format!("import --publish replay denied by policy: {detail}"),
            )
            .with_hint(
                "add the host to `policy.json` allowed_hosts (or allow_private_hosts) and re-run \
                 `rdm recipe import <file> --publish --dry-run`",
            )
            .with_details(json!({ "report": report_value })));
        }
        return Ok((
            json!({
                "imported": false,
                "already_present": false,
                "report": report_value,
            }),
            Some(publish_gate_error(
                &report_value,
                "import --publish: gates not satisfied; nothing was written",
            )),
        ));
    }
    if args.dry_run {
        if is_text {
            crate::outln!("dry-run:       gates passed; nothing written");
        }
        return Ok((
            json!({
                "imported": false,
                "already_present": false,
                "dry_run": true,
                "report": report_value,
            }),
            None,
        ));
    }
    let path = write_method(registry, recipe)?;
    if is_text {
        crate::outln!(
            "published:     {} v{} -> {} (scope={})",
            recipe.id,
            recipe.version,
            path.display(),
            report.scope,
        );
    }
    Ok((
        json!({
            "imported": true,
            "already_present": false,
            "report": report_value,
            "recipe": {
                "id": recipe.id,
                "version": recipe.version,
                "state": "published",
            },
            "path": path.display().to_string(),
        }),
        None,
    ))
}

fn read_import_file(path: &Path) -> Result<Recipe, AgentErrorInfo> {
    let metadata = std::fs::metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AgentErrorInfo::new(
            ErrorCode::NotFound,
            format!("recipe file `{}` not found", path.display()),
        )
        .with_hint("pass the path to a `rdm.recipe/v1` JSON file")
        .with_details(json!({ "path": path.display().to_string() })),
        _ => AgentErrorInfo::new(
            ErrorCode::PermissionDenied,
            format!("cannot read `{}`: {error}", path.display()),
        )
        .with_hint("check the file exists and is readable"),
    })?;
    if !metadata.is_file() {
        return Err(invalid_request(format!(
            "`{}` is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_IMPORT_BYTES {
        return Err(invalid_request(format!(
            "recipe file is {} bytes; the import limit is {} bytes",
            metadata.len(),
            MAX_IMPORT_BYTES
        ))
        .with_hint("exported methods are normally well under 1 MiB"));
    }
    let text = std::fs::read_to_string(path).map_err(|error| {
        AgentErrorInfo::new(
            ErrorCode::InvalidRequest,
            format!("cannot read `{}` as UTF-8 text: {error}", path.display()),
        )
        .with_hint("recipe files are JSON text")
    })?;
    Recipe::from_json(&text).map_err(|error| {
        invalid_request(format!("invalid recipe in `{}`: {error}", path.display())).with_hint(
            "the file must be a valid `rdm.recipe/v1` JSON object (as written by \
             `rdm recipe export`)",
        )
    })
}

fn write_method(registry: &mut RecipeRegistry, recipe: &Recipe) -> Result<PathBuf, AgentErrorInfo> {
    let path = registry.save(recipe).map_err(|error| {
        AgentErrorInfo::new(
            ErrorCode::PermissionDenied,
            format!("cannot write method file: {error}"),
        )
        .with_hint("check that `<state>/resolve/recipes` is writable")
    })?;
    if let Err(error) = registry.insert(recipe.clone()) {
        let _ = std::fs::remove_file(&path);
        return Err(invalid_request(format!(
            "cannot add recipe to the library: {error}"
        )));
    }
    if let Err(error) = registry.save_index() {
        let _ = std::fs::remove_file(&path);
        return Err(AgentErrorInfo::new(
            ErrorCode::Internal,
            format!("cannot persist recipe index: {error}"),
        )
        .with_hint(
            "check that `<state>/resolve/index.json` is writable; the method file was removed again",
        ));
    }
    Ok(path)
}

fn build_import_draft(file: &Path, recipe: &Recipe, positive_samples: &[String]) -> RecipeDraft {
    let source_url = positive_samples
        .first()
        .cloned()
        .or_else(|| {
            recipe
                .tests
                .iter()
                .find(|test| test.skip.is_none())
                .map(|test| test.input_url.clone())
        })
        .unwrap_or_default();
    RecipeDraft {
        schema: DRAFT_SCHEMA.to_owned(),
        id: format!(
            "draft_{}_{}",
            safe_recipe_component(&recipe.id),
            recipe_sha8(recipe)
        ),
        version: recipe.version,
        created_ms: rdm_resolve::now_ms(),
        source_run_id: format!("import:{}", file.display()),
        source_url,
        scope_hint: "narrow".to_owned(),
        notes: vec!["imported from a file via `rdm recipe import`".to_owned()],
        recipe: recipe.clone(),
    }
}

/// Positive replay samples: non-skip tests that must match (`expect_no_match=false`).
fn positive_samples(recipe: &Recipe) -> Vec<String> {
    collect_test_urls(recipe, false)
}

/// Negative replay samples: non-skip tests that must not match.
fn negative_samples(recipe: &Recipe) -> Vec<String> {
    collect_test_urls(recipe, true)
}

fn collect_test_urls(recipe: &Recipe, expect_no_match: bool) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for test in &recipe.tests {
        if test.skip.is_some() || test.expect_no_match != expect_no_match {
            continue;
        }
        let url = test.input_url.trim();
        if url.is_empty() || urls.iter().any(|existing| existing == url) {
            continue;
        }
        urls.push(url.to_owned());
    }
    urls
}

fn same_recipe(left: &Recipe, right: &Recipe) -> bool {
    serde_json::to_value(left).ok() == serde_json::to_value(right).ok()
}

fn recipe_sha8(recipe: &Recipe) -> String {
    let bytes = serde_json::to_vec(recipe).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)[..8].to_string()
}

fn safe_recipe_component(id: &str) -> String {
    let mut out: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while out.ends_with('.') || out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("recipe");
    }
    out
}

// ---------------------------------------------------------------------------
// Sensitive-content detection (export/import)
// ---------------------------------------------------------------------------

fn sensitive_findings(recipe: &Recipe) -> Vec<Value> {
    let mut findings: Vec<Value> = Vec::new();
    collect_step_findings(&recipe.steps, &mut findings);
    if let Some(name) = sensitive_query_hit(recipe.emit.url.as_str()) {
        findings.push(json!({
            "step": "emit.url",
            "kind": "query",
            "reason": format!("emit URL contains sensitive query parameter `{name}`"),
        }));
    }
    // Stable dedupe: one finding per (step, kind) — a second sensitive query
    // name in the same step adds no new information for the operator.
    let mut seen = std::collections::BTreeSet::new();
    findings.retain(|finding| {
        let key = format!(
            "{}|{}",
            finding["step"].as_str().unwrap_or(""),
            finding["kind"].as_str().unwrap_or(""),
        );
        seen.insert(key)
    });
    findings
}

fn collect_step_findings(steps: &[RecipeStep], findings: &mut Vec<Value>) {
    for step in steps {
        if step.op == "http_request" {
            if let Some(headers) = step.args.get("headers").and_then(Value::as_object) {
                for key in headers.keys() {
                    let lower = key.to_ascii_lowercase();
                    if SENSITIVE_HEADERS.contains(&lower.as_str()) {
                        findings.push(json!({
                            "step": step.id,
                            "kind": "header",
                            "reason": format!("sets credential header `{lower}`"),
                        }));
                    }
                }
            }
            if let Some(url) = step.args.get("url").and_then(Value::as_str) {
                if let Ok(parsed) = url::Url::parse(url) {
                    for (name, _) in parsed.query_pairs() {
                        let lower = name.to_ascii_lowercase();
                        if SENSITIVE_QUERY_NAMES.contains(&lower.as_str()) {
                            findings.push(json!({
                                "step": step.id,
                                "kind": "query",
                                "reason": format!("URL query parameter `{lower}`"),
                            }));
                        }
                    }
                }
            }
        }
        let mut strings = Vec::new();
        collect_strings(&step.args, &mut strings);
        for value in strings {
            if let Some(name) = sensitive_query_hit(&value) {
                findings.push(json!({
                    "step": step.id,
                    "kind": "query",
                    "reason": format!("string value contains sensitive query parameter `{name}`"),
                }));
            }
        }
        if let Some(nested) = step.args.get("steps") {
            if let Ok(nested_steps) = serde_json::from_value::<Vec<RecipeStep>>(nested.clone()) {
                collect_step_findings(&nested_steps, findings);
            }
        }
    }
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        Value::Object(map) => map.values().for_each(|item| collect_strings(item, out)),
        _ => {}
    }
}

/// Case-insensitive scan for `name=` / `name%3D` where `name` is one of the
/// sensitive query parameter names and is not preceded by an identifier char.
fn sensitive_query_hit(value: &str) -> Option<&'static str> {
    let lower = value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for name in SENSITIVE_QUERY_NAMES {
        let mut start = 0;
        while let Some(offset) = lower[start..].find(name) {
            let position = start + offset;
            let preceded_ok = position == 0
                || !(bytes[position - 1].is_ascii_alphanumeric() || bytes[position - 1] == b'_');
            let after = &lower[position + name.len()..];
            if preceded_ok && (after.starts_with('=') || after.starts_with("%3d")) {
                return Some(name);
            }
            start = position + name.len();
        }
    }
    None
}

fn sensitive_error(context: &str, findings: Vec<Value>) -> AgentErrorInfo {
    AgentErrorInfo::new(
        ErrorCode::InvalidRequest,
        format!("{context} contains credential-like content; refusing by default"),
    )
    .with_hint(
        "review the listed steps; re-run with `--allow-sensitive` only when the content is not a \
         real credential (headers are never printed here)",
    )
    .with_details(json!({ "findings": findings }))
}

fn conflict_error(what: &str, details: Value) -> AgentErrorInfo {
    AgentErrorInfo::new(
        ErrorCode::Conflict,
        format!("{what} already exists with different content"),
    )
    .with_hint("pass `--force` to overwrite it, or bump the recipe version")
    .with_details(details)
}

fn print_report_gates(report: &Value) {
    crate::outln!(
        "gates:         required_passed={} scope={}",
        report["required_passed"].as_bool().unwrap_or(false),
        report["scope"].as_str().unwrap_or("?"),
    );
    for gate in report["gates"].as_array().into_iter().flatten() {
        crate::outln!(
            "[{}] {:<3} {}: {}",
            if gate["pass"].as_bool().unwrap_or(false) {
                "pass"
            } else {
                "FAIL"
            },
            gate["id"].as_str().unwrap_or("?"),
            gate["name"].as_str().unwrap_or(""),
            gate["detail"].as_str().unwrap_or(""),
        );
    }
}

fn write_atomic_file(path: &Path, contents: &str) -> std::io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recipe.json");
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    let result = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_dir_is_under_the_state_dir() {
        // `rdm_ipc::state_dir()` ends with `RDM`; recipe state is its `resolve/`.
        let root = resolve_dir();
        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some("resolve")
        );
        assert_eq!(
            recipes_root(&root)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("recipes")
        );
    }

    #[test]
    fn draft_summary_projects_contract_fields() {
        let raw = json!({
            "schema": "rdm.recipe-draft/v1",
            "id": "draft_example_ab12",
            "version": 1,
            "created_ms": 42,
            "source_run_id": "run_1",
            "source_url": "https://example.com/page",
            "scope_hint": "narrow",
            "notes": ["deterministic draft"],
            "recipe": {
                "schema": "rdm.recipe/v1",
                "id": "example.com-file",
                "version": 1,
                "match": {"hosts": ["example.com"]},
                "steps": [{"id": "s", "op": "assert", "args": {"kind": "matches", "pattern": "^https?://"}}],
                "emit": {"url": "https://example.com/f.bin"}
            }
        });
        let draft: RecipeDraft = serde_json::from_value(raw).expect("valid draft");
        let summary = draft_summary(&draft);
        assert_eq!(summary["id"], "draft_example_ab12");
        assert_eq!(summary["version"], 1);
        assert_eq!(summary["source_run_id"], "run_1");
        assert_eq!(summary["source_url"], "https://example.com/page");
        assert_eq!(summary["scope_hint"], "narrow");
        assert_eq!(summary["created_ms"], 42);
    }

    #[test]
    fn publish_gate_error_keeps_the_report() {
        let data = json!({
            "draft_id": "draft_x",
            "scope": "narrow",
            "required_passed": false,
            "gates": [{"id": "R3", "name": "first sample", "pass": false, "detail": "boom"}]
        });
        let error = publish_gate_error(&data, "publish gates not satisfied");
        assert_eq!(error.code, ErrorCode::ResolveFailed);
        let details = error.details.as_ref().unwrap();
        assert_eq!(details["required_passed"], false);
        assert_eq!(details["gates"], data["gates"]);
        assert_eq!(details["scope"], "narrow");
    }

    #[test]
    fn policy_denial_is_detected_in_a_report() {
        let data = json!({
            "required_passed": false,
            "gates": [
                {"id": "R1", "name": "no model", "pass": true, "detail": "ok"},
                {"id": "R3", "name": "first sample", "pass": false,
                 "detail": "replay failed: PolicyDenied: private/loopback host denied"}
            ]
        });
        assert!(policy_denial_in_report(&data).is_some());
        let clean = json!({"gates": [{"id": "R1", "pass": true, "detail": "ok"}]});
        assert!(policy_denial_in_report(&clean).is_none());
    }

    fn recipe_from(value: Value) -> Recipe {
        Recipe::from_json(&value.to_string()).expect("valid recipe")
    }

    #[test]
    fn sensitive_headers_and_query_names_are_detected() {
        let header_recipe = recipe_from(json!({
            "schema": "rdm.recipe/v1",
            "id": "sensitive-header",
            "version": 1,
            "match": {"hosts": ["example.com"]},
            "steps": [{
                "id": "fetch",
                "op": "http_request",
                "args": {"method": "GET", "url": "https://example.com/api",
                         "headers": {"Authorization": "Bearer SECRET",
                                     "Accept": "application/json"}}
            }],
            "emit": {"url": "https://example.com/file.bin"}
        }));
        let findings = sensitive_findings(&header_recipe);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["step"], "fetch");
        assert_eq!(findings[0]["kind"], "header");
        assert!(findings[0]["reason"]
            .as_str()
            .unwrap()
            .contains("authorization"));
        // The finding never echoes the header value.
        assert!(!findings[0].to_string().contains("SECRET"));

        let query_recipe = recipe_from(json!({
            "schema": "rdm.recipe/v1",
            "id": "sensitive-query",
            "version": 1,
            "match": {"hosts": ["cdn.example.com"]},
            "steps": [{
                "id": "fetch",
                "op": "http_request",
                "args": {"method": "GET",
                         "url": "https://cdn.example.com/f.bin?token=deadbeef"}
            }],
            "emit": {"url": "https://cdn.example.com/f.bin"}
        }));
        let findings = sensitive_findings(&query_recipe);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["step"], "fetch");
        assert_eq!(findings[0]["kind"], "query");
        assert!(findings[0]["reason"].as_str().unwrap().contains("token"));

        let clean = recipe_from(json!({
            "schema": "rdm.recipe/v1",
            "id": "clean",
            "version": 1,
            "match": {"hosts": ["example.com"]},
            "steps": [{"id": "fetch", "op": "http_request",
                       "args": {"method": "GET", "url": "https://example.com/page"}}],
            "emit": {"url": "https://example.com/f.bin"}
        }));
        assert!(sensitive_findings(&clean).is_empty());
    }

    #[test]
    fn sensitive_findings_recurse_into_bounded_each() {
        let recipe = recipe_from(json!({
            "schema": "rdm.recipe/v1",
            "id": "nested",
            "version": 1,
            "match": {"hosts": ["example.com"]},
            "steps": [{
                "id": "each",
                "op": "bounded_each",
                "args": {
                    "from": "items",
                    "steps": [{
                        "id": "inner",
                        "op": "http_request",
                        "args": {"method": "GET", "url": "https://example.com/x",
                                 "headers": {"cookie": "session=1"}}
                    }]
                }
            }],
            "emit": {"url": "https://example.com/f.bin"}
        }));
        let findings = sensitive_findings(&recipe);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["step"], "inner");
        assert_eq!(findings[0]["kind"], "header");
    }

    #[test]
    fn sensitive_query_hit_matches_query_shapes_only() {
        assert_eq!(sensitive_query_hit("?token=abc"), Some("token"));
        assert_eq!(sensitive_query_hit("https://x/f&sig=1"), Some("sig"));
        assert_eq!(sensitive_query_hit("https://x/f?Key=abc"), Some("key"));
        assert_eq!(sensitive_query_hit("token%3Dabc"), Some("token"));
        // Not a query parameter name (identifier boundary / no `=`).
        assert_eq!(sensitive_query_hit("https://x/monkey=business"), None);
        assert_eq!(sensitive_query_hit("tokenizer"), None);
        assert_eq!(sensitive_query_hit("token abc"), None);
    }

    #[test]
    fn import_draft_id_is_content_addressed_and_safe() {
        let recipe = recipe_from(json!({
            "schema": "rdm.recipe/v1",
            "id": "Example.COM File!",
            "version": 3,
            "match": {"hosts": ["example.com"]},
            "steps": [{"id": "fetch", "op": "http_request",
                       "args": {"method": "GET", "url": "https://example.com/x"}}],
            "emit": {"url": "https://example.com/f.bin"},
            "tests": [{"name": "sample", "input_url": "https://example.com/page"}]
        }));
        let draft = build_import_draft(Path::new("C:/tmp/method.json"), &recipe, &[]);
        assert_eq!(draft.version, 3);
        assert!(
            draft.id.starts_with("draft_example.com_file_"),
            "{}",
            draft.id
        );
        assert!(
            draft
                .id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'),
            "{}",
            draft.id
        );
        assert_eq!(draft.source_url, "https://example.com/page");
        assert_eq!(draft.source_run_id, "import:C:/tmp/method.json");
        assert_eq!(draft.scope_hint, "narrow");
        // Content-addressed: same recipe → same draft id; changed recipe → new id.
        assert_eq!(
            draft.id,
            build_import_draft(Path::new("x"), &recipe, &[]).id
        );
        let mut changed = recipe.clone();
        changed.name = "other".to_owned();
        assert_ne!(
            draft.id,
            build_import_draft(Path::new("x"), &changed, &[]).id
        );
    }

    #[test]
    fn test_samples_are_deduplicated_and_split_by_expect_no_match() {
        let recipe = recipe_from(json!({
            "schema": "rdm.recipe/v1",
            "id": "samples",
            "version": 1,
            "match": {"hosts": ["example.com"]},
            "steps": [{"id": "fetch", "op": "http_request",
                       "args": {"method": "GET", "url": "https://example.com/x"}}],
            "emit": {"url": "https://example.com/f.bin"},
            "tests": [
                {"name": "a", "input_url": "https://example.com/1"},
                {"name": "b", "input_url": "https://example.com/2"},
                {"name": "dup", "input_url": "https://example.com/1"},
                {"name": "skip", "input_url": "https://example.com/3", "skip": "login"},
                {"name": "neg", "input_url": "https://other.example/x",
                 "expect_no_match": true}
            ]
        }));
        assert_eq!(
            positive_samples(&recipe),
            vec![
                "https://example.com/1".to_string(),
                "https://example.com/2".to_string()
            ]
        );
        assert_eq!(
            negative_samples(&recipe),
            vec!["https://other.example/x".to_string()]
        );
    }
}
