//! RDM link resolution runtime.
//!
//! Three execution paths share one contract:
//! * **recipe replay** — deterministic, model-free, bounded by [`Budget`];
//! * **builtin direct-file verification** — HEAD/bounded-GET check of the input
//!   URL itself when no recipe matches;
//! * **exploration** — a bounded JsonAction loop against the user's configured
//!   OpenAI-compatible provider (`docs/architecture.md` §11.9); the manifest is
//!   still decided by evidence binding + [`Verifier`], never by the model.
//!
//! Contract: `docs/architecture.md` §11 and `docs/research/link-resolution-framework.md`.
//!
//! v0.5 (P3a, §11.10) adds observable method health: sanitized run traces
//! ([`trace`]), [`health::RecipeHealth`] and the persisted
//! [`registry::RecipeIndex`] (`active_version` + health). Every write is
//! best-effort: a trace/index I/O failure is logged and never changes a
//! [`ResolveOutcome`].
//!
//! v0.6 (P3b, §11.11) adds deterministic recipe drafts ([`draft`]): a
//! successful exploration can be distilled into a local `rdm.recipe-draft/v1`
//! asset, and publishing it requires the model-free R1–R6 replay gates.

pub mod action;
pub mod contract;
pub mod credentials;
pub mod draft;
pub mod experience;
pub mod explore;
pub mod generalize;
pub mod health;
pub mod http;
pub mod model_draft;
pub mod page_snapshot;
pub mod policy;
pub mod prompt;
pub mod recipe;
pub mod registry;
pub mod service;
pub mod trace;
pub mod verify;

#[cfg(test)]
mod draft_tests;
#[cfg(test)]
mod test_support;

pub use contract::{
    new_id, now_ms, Budget, CredentialsHint, Diagnostics, RecipeRef, ResolveError,
    ResolveErrorKind, ResolveMode, ResolveOutcome, ResolveRequest, ResolvedCredentials,
    ResourceItem, ResourceManifest, RunStatus, SourceRef, TransferHint, Verification,
    VerificationMethod, VerificationStatus, DEFAULT_MAX_LLM_CALLS, DEFAULT_MAX_RESOURCES,
    MAX_LLM_CALLS, SCHEMA_VERSION,
};
pub use credentials::{
    HostCredentials, MAX_CREDENTIAL_HEADERS_PER_HOST, MAX_CREDENTIAL_HOSTS,
    MAX_CREDENTIAL_VALUE_BYTES,
};
pub use draft::{
    distill, draft_file_name, drafts_root, evaluate_publish, list_drafts, load_draft, publish,
    remove_draft, replay_sample, save_draft, DraftAction, DraftCandidate, GateResult,
    PublishReport, RecipeDraft, SampleReplay, DRAFT_RECIPE_VERSION, DRAFT_SCHEMA, MAX_DRAFT_STEPS,
};
pub use experience::{
    Experience, ExperienceFlow, ExperienceFlowView, ExperienceStore, ExperienceVersion, FlowEmit,
    FlowStep,
};
pub use generalize::{
    abstract_url, generalize_recipe, migrate_experience_library, GeneralizeReport, MigrationReport,
    UrlShape,
};
pub use health::{is_structural, FailureRef, MethodStatus, RecipeHealth};
pub use http::{HttpRequestSpec, HttpTool, Observation};
pub use model_draft::{draft_messages, parse_and_validate, validate_generated, MAX_PROMPT_ACTIONS};
pub use page_snapshot::{
    PageSnapshot, MAX_SNAPSHOT_LINK, MAX_SNAPSHOT_LINKS, MAX_SNAPSHOT_PROMPT_LINKS,
    MAX_SNAPSHOT_TITLE, MAX_SNAPSHOT_URL,
};
pub use policy::{is_private_host, BudgetTracker, ResolvePolicy};
pub use recipe::{
    Recipe, RecipeEmit, RecipeError, RecipeGeneralization, RecipeLimits, RecipeMatch, RecipeOutput,
    RecipeRunner, RecipeStep, RecipeTest, RecipeTestResult, StepWhen, Var,
    DEFAULT_BOUNDED_EACH_MAX, MAX_BOUNDED_EACH_DEPTH, MAX_BOUNDED_EACH_ITEMS, RECIPE_SCHEMA,
    SUPPORTED_OPS,
};
pub use registry::{
    index_path, BundleRef, MatchHit, RecipeIndex, RecipeIndexEntry, RecipeRegistry, INDEX_SCHEMA,
};
pub use service::ResolverService;
pub use trace::{
    digest_args, error_kind_name, redact_excerpt, redact_url, RunStep, RunTrace, TraceStore,
    MAX_BYTES as TRACE_MAX_BYTES, MAX_EXCERPT_CHARS, MAX_STEPS, RETENTION_RUNS, TRACE_SCHEMA,
};
pub use verify::{Verifier, VerifyHints};

/// The exploration provider config type, re-exported so callers (CLI/MCP and
/// integration tests) do not need their own `rdm-ai` dependency just to call
/// [`ResolverService::with_ai_config`].
pub use rdm_ai::AiConfig;
