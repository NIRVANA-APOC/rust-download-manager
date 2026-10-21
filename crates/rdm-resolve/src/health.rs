//! Per-recipe health bookkeeping (`docs/architecture.md` §11.10).
//!
//! Health is deliberately kind-aware: a recipe that stops matching a drifted
//! site (structural failure) is degraded after two runs and broken after three,
//! while credentials/rate-limits/network hiccups only record
//! [`RecipeHealth::last_failure`] and never disable the recipe — one 401 must not
//! take a whole site offline.
//!
//! The type is persisted inside `<state>/resolve/index.json` by
//! [`crate::registry::RecipeRegistry`]; every field has a `serde` default so old
//! index files stay loadable.

use serde::{Deserialize, Serialize};

use crate::contract::ResolveErrorKind;

/// Health verdict for one recipe id/version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodStatus {
    Ok,
    /// Two consecutive structural failures.
    Degraded,
    /// Three or more consecutive structural failures.
    Broken,
    /// Never run / no health recorded yet.
    #[default]
    Untested,
}

impl MethodStatus {
    /// Stable machine token (same spelling as the serialized form).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Broken => "broken",
            Self::Untested => "untested",
        }
    }

    /// Chinese label for CLI/GUI views (not part of the machine contract).
    pub fn label_zh(&self) -> &'static str {
        match self {
            Self::Ok => "正常",
            Self::Degraded => "降级",
            Self::Broken => "失效",
            Self::Untested => "未测试",
        }
    }
}

/// Reference to the run that produced the last observed failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRef {
    pub run_id: String,
    pub kind: ResolveErrorKind,
    #[serde(default)]
    pub step_id: Option<String>,
    pub observed_ms: u64,
}

/// Persisted health of one recipe version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecipeHealth {
    pub status: MethodStatus,
    pub last_ok_ms: Option<u64>,
    pub last_failure: Option<FailureRef>,
    pub consecutive_failures: u32,
    /// Free-form evidence lines reserved for later iterations.
    pub evidence: Vec<String>,
}

impl Default for RecipeHealth {
    fn default() -> Self {
        Self {
            status: MethodStatus::Untested,
            last_ok_ms: None,
            last_failure: None,
            consecutive_failures: 0,
            evidence: Vec::new(),
        }
    }
}

impl RecipeHealth {
    /// Successful replay: `ok`, failure streak reset, `last_ok_ms` updated.
    pub fn record_success(&mut self, _run_id: &str, now_ms: u64) {
        self.status = MethodStatus::Ok;
        self.consecutive_failures = 0;
        self.last_ok_ms = Some(now_ms);
    }

    /// Record one failed replay.
    ///
    /// * Structural kinds (`recipe_stale`, `selector_miss`, `expect_failed`,
    ///   `recipe_invalid`) increment the streak: the 2nd → [`MethodStatus::Degraded`],
    ///   the 3rd (and later) → [`MethodStatus::Broken`].
    /// * Every other kind only records [`Self::last_failure`]; the status and
    ///   the streak stay untouched.
    pub fn record_failure(
        &mut self,
        run_id: &str,
        kind: &ResolveErrorKind,
        step_id: Option<&str>,
        now_ms: u64,
    ) {
        self.last_failure = Some(FailureRef {
            run_id: run_id.to_owned(),
            kind: *kind,
            step_id: step_id.map(str::to_owned),
            observed_ms: now_ms,
        });
        if !is_structural(kind) {
            return;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.status = match self.consecutive_failures {
            0 | 1 => self.status,
            2 => MethodStatus::Degraded,
            _ => MethodStatus::Broken,
        };
    }
}

/// Whether a failure means "the recipe no longer matches reality".
pub fn is_structural(kind: &ResolveErrorKind) -> bool {
    matches!(
        kind,
        ResolveErrorKind::RecipeStale
            | ResolveErrorKind::SelectorMiss
            | ResolveErrorKind::ExpectFailed
            | ResolveErrorKind::RecipeInvalid
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health() -> RecipeHealth {
        RecipeHealth::default()
    }

    #[test]
    fn default_is_untested_and_serializes_snake_case() {
        let health = health();
        assert_eq!(health.status, MethodStatus::Untested);
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.last_failure.is_none());
        let json = serde_json::to_value(&health).unwrap();
        assert_eq!(json["status"], "untested");
        assert_eq!(json["consecutive_failures"], 0);
        assert_eq!(json["evidence"], serde_json::json!([]));

        let empty: RecipeHealth = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, health);

        for (status, name) in [
            (MethodStatus::Ok, "ok"),
            (MethodStatus::Degraded, "degraded"),
            (MethodStatus::Broken, "broken"),
            (MethodStatus::Untested, "untested"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), name);
        }
    }

    #[test]
    fn structural_failures_degrade_on_the_second_and_break_on_the_third() {
        let mut health = health();
        health.record_failure("run_1", &ResolveErrorKind::RecipeStale, Some("url"), 10);
        assert_eq!(health.status, MethodStatus::Untested);
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(
            health.last_failure.as_ref().map(|failure| failure.kind),
            Some(ResolveErrorKind::RecipeStale)
        );

        health.record_failure("run_2", &ResolveErrorKind::SelectorMiss, None, 20);
        assert_eq!(health.status, MethodStatus::Degraded);
        assert_eq!(health.consecutive_failures, 2);

        health.record_failure("run_3", &ResolveErrorKind::ExpectFailed, None, 30);
        assert_eq!(health.status, MethodStatus::Broken);
        assert_eq!(health.consecutive_failures, 3);

        // Stays broken and keeps counting.
        health.record_failure("run_4", &ResolveErrorKind::RecipeInvalid, None, 40);
        assert_eq!(health.status, MethodStatus::Broken);
        assert_eq!(health.consecutive_failures, 4);
        assert_eq!(
            health
                .last_failure
                .as_ref()
                .map(|failure| failure.observed_ms),
            Some(40)
        );
    }

    #[test]
    fn success_resets_status_and_streak() {
        let mut health = health();
        health.record_failure("run_1", &ResolveErrorKind::RecipeStale, None, 10);
        health.record_failure("run_2", &ResolveErrorKind::RecipeStale, None, 20);
        assert_eq!(health.status, MethodStatus::Degraded);

        health.record_success("run_3", 30);
        assert_eq!(health.status, MethodStatus::Ok);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.last_ok_ms, Some(30));
        // The last failure reference is kept for diagnostics.
        assert!(health.last_failure.is_some());

        // One structural failure after success keeps `ok` until the threshold.
        health.record_failure("run_4", &ResolveErrorKind::RecipeStale, None, 40);
        assert_eq!(health.status, MethodStatus::Ok);
        assert_eq!(health.consecutive_failures, 1);
        health.record_failure("run_5", &ResolveErrorKind::RecipeStale, None, 50);
        assert_eq!(health.status, MethodStatus::Degraded);
    }

    #[test]
    fn non_structural_failures_never_degrade_or_count() {
        let mut health = health();
        health.record_success("run_1", 10);
        assert_eq!(health.status, MethodStatus::Ok);

        for (index, kind) in [
            ResolveErrorKind::AuthRequired,
            ResolveErrorKind::Forbidden,
            ResolveErrorKind::NotFound,
            ResolveErrorKind::RateLimited,
            ResolveErrorKind::ServerError,
            ResolveErrorKind::NetworkError,
            ResolveErrorKind::Timeout,
            ResolveErrorKind::BudgetExhausted,
            ResolveErrorKind::PolicyDenied,
            ResolveErrorKind::ModelUnavailable,
        ]
        .into_iter()
        .enumerate()
        {
            health.record_failure(
                &format!("run_{}", index + 2),
                &kind,
                Some("fetch"),
                100 + index as u64,
            );
            assert_eq!(health.status, MethodStatus::Ok, "{kind:?}");
            assert_eq!(health.consecutive_failures, 0, "{kind:?}");
            assert_eq!(
                health.last_failure.as_ref().map(|failure| failure.kind),
                Some(kind)
            );
        }
    }

    #[test]
    fn structural_kinds_are_exactly_the_drift_kinds() {
        for kind in [
            ResolveErrorKind::RecipeStale,
            ResolveErrorKind::SelectorMiss,
            ResolveErrorKind::ExpectFailed,
            ResolveErrorKind::RecipeInvalid,
        ] {
            assert!(is_structural(&kind), "{kind:?}");
        }
        for kind in [
            ResolveErrorKind::AuthRequired,
            ResolveErrorKind::NotFound,
            ResolveErrorKind::RateLimited,
            ResolveErrorKind::NetworkError,
            ResolveErrorKind::Timeout,
            ResolveErrorKind::BudgetExhausted,
            ResolveErrorKind::PolicyDenied,
            ResolveErrorKind::ModelUnavailable,
            ResolveErrorKind::Internal,
        ] {
            assert!(!is_structural(&kind), "{kind:?}");
        }
    }

    #[test]
    fn failure_ref_serializes_kind_as_snake_case() {
        let health = RecipeHealth {
            status: MethodStatus::Degraded,
            last_ok_ms: Some(1),
            last_failure: Some(FailureRef {
                run_id: "run_1".to_owned(),
                kind: ResolveErrorKind::RecipeStale,
                step_id: Some("url".to_owned()),
                observed_ms: 5,
            }),
            consecutive_failures: 2,
            evidence: Vec::new(),
        };
        let json = serde_json::to_value(&health).unwrap();
        assert_eq!(json["status"], "degraded");
        assert_eq!(json["last_failure"]["kind"], "recipe_stale");
        assert_eq!(json["last_failure"]["step_id"], "url");
        let round_tripped: RecipeHealth = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped, health);
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(MethodStatus::Ok.label_zh(), "正常");
        assert_eq!(MethodStatus::Degraded.label_zh(), "降级");
        assert_eq!(MethodStatus::Broken.label_zh(), "失效");
        assert_eq!(MethodStatus::Untested.label_zh(), "未测试");
    }
}
