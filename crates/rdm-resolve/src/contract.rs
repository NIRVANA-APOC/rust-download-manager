//! Public contracts of the resolution runtime (`docs/architecture.md` §10.4).
//!
//! These types are the machine-readable boundary between the resolver, the
//! download engine and the CLI/MCP entry points. Everything is `serde` with
//! defaults so new optional fields stay backward compatible.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Manifest schema identifier.
pub const SCHEMA_VERSION: &str = "rdm.resolve/v1";
/// Default maximum resources per manifest.
pub const DEFAULT_MAX_RESOURCES: usize = 40;
/// Default exploration model-call budget (`docs/architecture.md` §11.5).
pub const DEFAULT_MAX_LLM_CALLS: u32 = 8;
/// Upper bound the service clamps `ResolveRequest::max_llm_calls` to.
pub const MAX_LLM_CALLS: u32 = 32;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Process-local unique identifier with a prefix, e.g. `run_17_1730000000000`.
pub fn new_id(prefix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{}_{sequence}", std::process::id())
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// How the caller wants a URL resolved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveMode {
    /// Use a recipe when one matches, otherwise explore (when enabled).
    #[default]
    Auto,
    /// Only run a known recipe; never call a model.
    RecipeOnly,
    /// Always explore, even when a recipe matches.
    Explore,
}

/// Hard limits applied to one resolution run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    pub wall_ms: u64,
    pub http_requests: u32,
    pub bytes_read: u64,
    pub redirects: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            wall_ms: 60_000,
            http_requests: 24,
            bytes_read: 4 * 1024 * 1024,
            redirects: 5,
        }
    }
}

impl Budget {
    /// Budget for deterministic recipe replay (no model, fewer requests).
    pub fn replay() -> Self {
        Self {
            wall_ms: 15_000,
            ..Self::default()
        }
    }

    /// Tighten the configured budget: never allow more than `self` allows.
    pub fn capped_by(self, cap: Budget) -> Budget {
        Budget {
            wall_ms: self.wall_ms.min(cap.wall_ms),
            http_requests: self.http_requests.min(cap.http_requests),
            bytes_read: self.bytes_read.min(cap.bytes_read),
            redirects: self.redirects.min(cap.redirects),
        }
    }
}

/// One link the caller wants to resolve.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolveRequest {
    pub url: String,
    /// Natural-language goal, e.g. "Windows x64 installer".
    pub goal: Option<String>,
    pub mode: ResolveMode,
    /// Force one recipe (debugging / replay).
    pub recipe_id: Option<String>,
    /// Overrides the defaults; always capped by the service policy.
    pub budget: Option<Budget>,
    pub max_resources: Option<usize>,
    /// v0.4: model calls allowed for exploration (1..=32, default 8).
    /// Only consulted by `explore` and the `auto` page fallback.
    pub max_llm_calls: Option<u32>,
}

impl ResolveRequest {
    /// Effective exploration model-call limit: default 8, clamped to 1..=32.
    pub fn llm_call_limit(&self) -> u32 {
        self.max_llm_calls
            .unwrap_or(DEFAULT_MAX_LLM_CALLS)
            .clamp(1, MAX_LLM_CALLS)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Stable machine-readable failure reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveErrorKind {
    InvalidInput,
    InvalidUrl,
    UnsupportedScheme,
    PolicyDenied,
    NeedsApproval,
    AuthRequired,
    NotFound,
    Forbidden,
    RateLimited,
    ServerError,
    NetworkError,
    Timeout,
    NotAFile,
    EmptyBody,
    UnknownLength,
    RecipeNoMatch,
    RecipeInvalid,
    RecipeStale,
    /// A recipe selector (JSON pointer / regex) found nothing.
    SelectorMiss,
    /// An `assert` step failed its predicate.
    ExpectFailed,
    RecipeBudgetExceeded,
    BudgetExhausted,
    ExploreUnavailable,
    /// No usable AI provider is configured, or the model call itself failed
    /// (v0.4 exploration; retrying later may succeed once AI is configured).
    ModelUnavailable,
    /// The model broke the JsonAction protocol twice in a row (v0.4).
    ModelProtocolError,
    /// Exploration produced no evidence-bound resource that verified (v0.4).
    NoResources,
    Internal,
}

impl ResolveErrorKind {
    /// Parse a recipe `on_fail` value.
    ///
    /// The canonical form is the `snake_case` name used by the JSON contract
    /// (`recipe_invalid`, `expect_failed`, ...); the Rust variant name
    /// (`RecipeInvalid`) is also accepted for older hand-written recipes.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "invalid_input" | "InvalidInput" => Some(Self::InvalidInput),
            "invalid_url" | "InvalidUrl" => Some(Self::InvalidUrl),
            "unsupported_scheme" | "UnsupportedScheme" => Some(Self::UnsupportedScheme),
            "policy_denied" | "PolicyDenied" => Some(Self::PolicyDenied),
            "needs_approval" | "NeedsApproval" => Some(Self::NeedsApproval),
            "auth_required" | "AuthRequired" => Some(Self::AuthRequired),
            "not_found" | "NotFound" => Some(Self::NotFound),
            "forbidden" | "Forbidden" => Some(Self::Forbidden),
            "rate_limited" | "RateLimited" => Some(Self::RateLimited),
            "server_error" | "ServerError" => Some(Self::ServerError),
            "network_error" | "NetworkError" => Some(Self::NetworkError),
            "timeout" | "Timeout" => Some(Self::Timeout),
            "not_a_file" | "NotAFile" => Some(Self::NotAFile),
            "empty_body" | "EmptyBody" => Some(Self::EmptyBody),
            "unknown_length" | "UnknownLength" => Some(Self::UnknownLength),
            "recipe_no_match" | "RecipeNoMatch" => Some(Self::RecipeNoMatch),
            "recipe_invalid" | "RecipeInvalid" => Some(Self::RecipeInvalid),
            "recipe_stale" | "RecipeStale" => Some(Self::RecipeStale),
            "selector_miss" | "SelectorMiss" => Some(Self::SelectorMiss),
            "expect_failed" | "ExpectFailed" => Some(Self::ExpectFailed),
            "recipe_budget_exceeded" | "RecipeBudgetExceeded" => Some(Self::RecipeBudgetExceeded),
            "budget_exhausted" | "BudgetExhausted" => Some(Self::BudgetExhausted),
            "explore_unavailable" | "ExploreUnavailable" => Some(Self::ExploreUnavailable),
            "model_unavailable" | "ModelUnavailable" => Some(Self::ModelUnavailable),
            "model_protocol_error" | "ModelProtocolError" => Some(Self::ModelProtocolError),
            "no_resources" | "NoResources" => Some(Self::NoResources),
            "internal" | "Internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

/// A typed resolution failure (`kind` drives retry/reporting decisions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveError {
    pub kind: ResolveErrorKind,
    pub detail: String,
}

impl ResolveError {
    pub fn new(kind: ResolveErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// Whether retrying the same input later may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind,
            ResolveErrorKind::RateLimited
                | ResolveErrorKind::ServerError
                | ResolveErrorKind::NetworkError
                | ResolveErrorKind::Timeout
                | ResolveErrorKind::ModelUnavailable
        )
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for ResolveError {}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Result of checking whether a candidate really is a downloadable file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// Bounded read confirmed a file (strongest claim).
    Verified,
    /// Only headers were inspected (used for one-time / signed URLs).
    HeadOnlyVerified,
    /// 401, or redirected to a page that looks like a login form.
    LoginRequired,
    /// 403/410, or a signed URL that expired.
    ForbiddenOrExpired,
    NotFound,
    RateLimited,
    /// The response is an HTML page, not a file.
    HtmlPage,
    /// The response is a non-file document (JSON/XML body).
    NotAFile,
    EmptyBody,
    UnknownLength,
    ServerError,
    #[default]
    NetworkError,
    /// The host/scheme is outside the resolver policy.
    PolicyDenied,
}

impl VerificationStatus {
    /// Only these two statuses may be handed to the download engine as a
    /// resolved resource.
    pub fn is_downloadable(self) -> bool {
        matches!(self, Self::Verified | Self::HeadOnlyVerified)
    }
}

/// Which probe produced the verification evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    #[default]
    HeadOnly,
    /// `GET` with a single `Range: bytes=0-N` request.
    RangedGet,
    BoundedGet,
}

/// Evidence-backed judgement about one candidate URL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Verification {
    pub status: VerificationStatus,
    pub method: VerificationMethod,
    pub http_status: Option<u16>,
    pub final_url: Option<String>,
    pub content_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub supports_ranges: Option<bool>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Human/machine readable evidence lines (no secrets, no bodies).
    pub evidence: Vec<String>,
    pub checked_ms: u64,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Credential *references* only: the manifest never contains secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CredentialsHint {
    /// Reference to a session credential resolved at download time.
    pub cookie_ref: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    /// Non-sensitive extra headers.
    pub headers: BTreeMap<String, String>,
}

/// Plain credentials resolved for one download (never persisted).
#[derive(Debug, Clone, Default)]
pub struct ResolvedCredentials {
    pub cookie: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub proxy: Option<String>,
}

/// How the engine should transfer this resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferHint {
    /// A single request must be used (one-time / signed URLs).
    pub single_stream: bool,
    pub connections: Option<usize>,
    pub dynamic_connections: Option<bool>,
    pub chunk_size: Option<u64>,
    /// `Range` support && known length && not one-time.
    pub resumable: bool,
    /// URL may be consumed by the first request.
    pub one_time: bool,
    /// When the URL stops working, in ms since the epoch.
    pub expires_ms: Option<u64>,
}

/// One download candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceItem {
    pub resource_id: String,
    /// The replayable URL (may be a short-lived signed URL).
    pub url: String,
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub supports_ranges: Option<bool>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub credentials: CredentialsHint,
    #[serde(default)]
    pub transfer: TransferHint,
    #[serde(default)]
    pub verification: Verification,
    /// References into the run trace (`step id`, observation handle).
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ResourceItem {
    /// Fill the transfer hints from the verification result.
    pub fn with_transfer_from_verification(mut self) -> Self {
        if let Some(ranges) = self.supports_ranges {
            self.transfer.resumable = ranges
                && self.size_bytes.is_some()
                && !self.transfer.one_time
                && self.verification.status == VerificationStatus::Verified;
        }
        if self.transfer.one_time || self.transfer.single_stream {
            self.transfer.resumable = false;
        }
        self
    }

    /// Convert to an engine request.
    ///
    /// A `single_stream` / one-time / non-ranged resource maps to
    /// `connections = 1`, `dynamic_connections = false`,
    /// `DownloadRequest::single_stream = true` and no resume; everything else
    /// keeps the manifest's transfer hints (`docs/architecture.md` §11.6).
    pub fn to_download_request(
        &self,
        output_dir: Option<PathBuf>,
        credentials: &ResolvedCredentials,
    ) -> rdm_core::DownloadRequest {
        let mut request = rdm_core::DownloadRequest::new(self.url.clone());
        request.output_dir = output_dir;
        request.file_name = self.name.clone();
        let ranges = self.supports_ranges.unwrap_or(false);
        let single = self.transfer.single_stream || self.transfer.one_time || !ranges;
        request.single_stream = single;
        request.connections = if single {
            1
        } else {
            self.transfer
                .connections
                .unwrap_or(rdm_core::DEFAULT_CONNECTIONS)
        };
        // A single-stream / one-time / non-ranged resource must never fan out,
        // regardless of what the manifest hint says (`docs/architecture.md` §11.6).
        request.dynamic_connections = if single {
            false
        } else {
            self.transfer.dynamic_connections.unwrap_or(true)
        };
        request.chunk_size = self.transfer.chunk_size;
        request.resume = self.transfer.resumable && !self.transfer.one_time;
        request.expected_sha256 = self.expected_sha256.clone();
        request.cookie = credentials.cookie.clone();
        request.referer = credentials
            .referer
            .clone()
            .or_else(|| self.credentials.referer.clone());
        request.user_agent = credentials
            .user_agent
            .clone()
            .or_else(|| self.credentials.user_agent.clone());
        request.proxy = credentials.proxy.clone();
        request.headers = self.credentials.headers.clone();
        request
    }
}

/// Where a manifest came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceRef {
    pub input_url: String,
    pub final_url: Option<String>,
    pub title: Option<String>,
    pub recipe_id: Option<String>,
}

/// Result of a successful (or partially successful) resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceManifest {
    pub schema_version: String,
    pub manifest_id: String,
    pub source: SourceRef,
    pub resources: Vec<ResourceItem>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub created_ms: u64,
}

impl ResourceManifest {
    /// Only resources whose verification allows downloading.
    pub fn downloadable(&self) -> impl Iterator<Item = &ResourceItem> {
        self.resources
            .iter()
            .filter(|item| item.verification.status.is_downloadable())
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Terminal state of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    Partial,
    WaitingUser,
    Failed,
    BudgetExhausted,
    Cancelled,
}

/// Which recipe produced (or was supposed to produce) the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeRef {
    pub id: String,
    pub version: u32,
    /// `matched` = an existing recipe; `draft` = generated, not yet verified.
    pub state: String,
}

/// Counters and the typed failure for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Diagnostics {
    pub summary: String,
    pub error: Option<ResolveError>,
    pub steps: u32,
    pub http_requests: u32,
    pub bytes_read: u64,
    pub llm_calls: u32,
    /// Prompt tokens consumed by the run's LLM calls (0 when unknown).
    pub llm_prompt_tokens: u64,
    /// Completion tokens consumed by the run's LLM calls (0 when unknown).
    pub llm_completion_tokens: u64,
    /// Total tokens consumed by the run's LLM calls (0 when unknown).
    pub llm_total_tokens: u64,
    /// `true` when at least one LLM call's usage was estimated from text length.
    pub llm_estimated: bool,
    pub elapsed_ms: u64,
}

/// Everything a caller gets back from one resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveOutcome {
    pub run_id: String,
    pub status: RunStatus,
    #[serde(default)]
    pub manifest: Option<ResourceManifest>,
    #[serde(default)]
    pub recipe: Option<RecipeRef>,
    #[serde(default)]
    pub diagnostics: Diagnostics,
}

impl ResolveOutcome {
    /// A failed run with a typed reason.
    pub fn failed(
        run_id: impl Into<String>,
        error: ResolveError,
        diagnostics: Diagnostics,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            status: match error.kind {
                ResolveErrorKind::BudgetExhausted | ResolveErrorKind::RecipeBudgetExceeded => {
                    RunStatus::BudgetExhausted
                }
                _ => RunStatus::Failed,
            },
            manifest: None,
            recipe: None,
            diagnostics: Diagnostics {
                error: Some(error),
                ..diagnostics
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_stable() {
        assert_eq!(SCHEMA_VERSION, "rdm.resolve/v1");
    }

    #[test]
    fn budget_replay_is_tighter_than_default() {
        assert!(Budget::replay().wall_ms < Budget::default().wall_ms);
        assert_eq!(
            Budget::replay().http_requests,
            Budget::default().http_requests
        );
    }

    #[test]
    fn only_verified_statuses_are_downloadable() {
        assert!(VerificationStatus::Verified.is_downloadable());
        assert!(VerificationStatus::HeadOnlyVerified.is_downloadable());
        assert!(!VerificationStatus::HtmlPage.is_downloadable());
        assert!(!VerificationStatus::LoginRequired.is_downloadable());
    }

    #[test]
    fn error_kinds_round_trip_through_names() {
        for kind in [
            ResolveErrorKind::RecipeInvalid,
            ResolveErrorKind::RecipeStale,
            ResolveErrorKind::ExpectFailed,
            ResolveErrorKind::RecipeBudgetExceeded,
            ResolveErrorKind::BudgetExhausted,
        ] {
            let json = serde_json::to_value(kind).unwrap();
            let name = json.as_str().unwrap();
            assert_eq!(ResolveErrorKind::from_name(name), Some(kind));
        }
        assert_eq!(
            ResolveErrorKind::from_name("expect_failed"),
            Some(ResolveErrorKind::ExpectFailed)
        );
        // PascalCase aliases are accepted for older hand-written recipes.
        assert_eq!(
            ResolveErrorKind::from_name("RecipeInvalid"),
            Some(ResolveErrorKind::RecipeInvalid)
        );
        assert_eq!(ResolveErrorKind::from_name("made_up"), None);
        assert_eq!(ResolveErrorKind::from_name(""), None);
    }

    #[test]
    fn v04_error_kinds_round_trip_through_names() {
        for kind in [
            ResolveErrorKind::ModelUnavailable,
            ResolveErrorKind::ModelProtocolError,
            ResolveErrorKind::NoResources,
        ] {
            let json = serde_json::to_value(kind).unwrap();
            let name = json.as_str().unwrap();
            assert_eq!(ResolveErrorKind::from_name(name), Some(kind), "{name}");
        }
        assert_eq!(
            ResolveErrorKind::from_name("ModelUnavailable"),
            Some(ResolveErrorKind::ModelUnavailable)
        );
        assert!(ResolveError::new(ResolveErrorKind::ModelUnavailable, "no key").is_transient());
        assert!(!ResolveError::new(ResolveErrorKind::ModelProtocolError, "bad").is_transient());
        assert!(!ResolveError::new(ResolveErrorKind::NoResources, "none").is_transient());
    }

    #[test]
    fn max_llm_calls_defaults_and_clamps() {
        assert_eq!(DEFAULT_MAX_LLM_CALLS, 8);
        assert_eq!(MAX_LLM_CALLS, 32);
        let mut request = ResolveRequest::default();
        assert_eq!(request.llm_call_limit(), 8, "default is 8");
        request.max_llm_calls = Some(0);
        assert_eq!(request.llm_call_limit(), 1, "clamped to the lower bound");
        request.max_llm_calls = Some(32);
        assert_eq!(request.llm_call_limit(), 32);
        request.max_llm_calls = Some(999);
        assert_eq!(request.llm_call_limit(), 32, "clamped to the upper bound");
    }

    #[test]
    fn old_request_json_without_max_llm_calls_keeps_working() {
        let request: ResolveRequest = serde_json::from_str(
            r#"{"url":"https://example.com/x","mode":"explore","max_resources":3}"#,
        )
        .unwrap();
        assert_eq!(request.max_llm_calls, None);
        assert_eq!(request.llm_call_limit(), 8);
        let encoded = serde_json::to_value(&request).unwrap();
        assert!(encoded.get("max_llm_calls").is_some());
    }

    #[test]
    fn one_time_resources_are_single_stream_and_not_resumable() {
        let item = ResourceItem {
            resource_id: "res_1".to_owned(),
            url: "https://cdn.example.com/a.bin".to_owned(),
            final_url: None,
            name: Some("a.bin".to_owned()),
            kind: None,
            content_type: Some("application/octet-stream".to_owned()),
            size_bytes: Some(10),
            supports_ranges: Some(true),
            etag: None,
            last_modified: None,
            expected_sha256: None,
            credentials: CredentialsHint::default(),
            transfer: TransferHint {
                one_time: true,
                single_stream: true,
                ..TransferHint::default()
            },
            verification: Verification {
                status: VerificationStatus::HeadOnlyVerified,
                ..Verification::default()
            },
            evidence: Vec::new(),
            notes: Vec::new(),
        }
        .with_transfer_from_verification();

        assert!(!item.transfer.resumable);
        let request = item.to_download_request(None, &ResolvedCredentials::default());
        assert_eq!(request.connections, 1);
        assert!(!request.dynamic_connections);
        assert!(!request.resume);
    }

    #[test]
    fn verified_ranged_resource_keeps_connections() {
        let item = ResourceItem {
            resource_id: "res_2".to_owned(),
            url: "https://cdn.example.com/a.bin".to_owned(),
            final_url: None,
            name: None,
            kind: None,
            content_type: Some("application/octet-stream".to_owned()),
            size_bytes: Some(1024),
            supports_ranges: Some(true),
            etag: Some("\"v1\"".to_owned()),
            last_modified: None,
            expected_sha256: None,
            credentials: CredentialsHint::default(),
            transfer: TransferHint {
                connections: Some(6),
                ..TransferHint::default()
            },
            verification: Verification {
                status: VerificationStatus::Verified,
                ..Verification::default()
            },
            evidence: Vec::new(),
            notes: Vec::new(),
        }
        .with_transfer_from_verification();

        assert!(item.transfer.resumable);
        let request = item.to_download_request(None, &ResolvedCredentials::default());
        assert_eq!(request.connections, 6);
        assert!(request.dynamic_connections);
        assert!(request.resume);
    }
}
