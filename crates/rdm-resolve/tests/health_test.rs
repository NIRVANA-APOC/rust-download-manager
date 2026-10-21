//! Recipe health (`docs/architecture.md` §11.10): structural failures degrade
//! after two runs and break after three, while credentials/rate-limits/network
//! hiccups only record `last_failure` and never disable a recipe.

use rdm_resolve::{is_structural, FailureRef, MethodStatus, RecipeHealth, ResolveErrorKind};

// ---------------------------------------------------------------------------
// 1. The structural list is exactly the drift list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn structural_kinds_are_exactly_the_drift_kinds() {
    let structural = [
        ResolveErrorKind::RecipeStale,
        ResolveErrorKind::SelectorMiss,
        ResolveErrorKind::ExpectFailed,
        ResolveErrorKind::RecipeInvalid,
    ];
    for kind in structural {
        assert!(is_structural(&kind), "{kind:?} must be structural");
    }

    let non_structural = [
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
        ResolveErrorKind::ModelProtocolError,
        ResolveErrorKind::NoResources,
        ResolveErrorKind::RecipeNoMatch,
        ResolveErrorKind::NotAFile,
        ResolveErrorKind::EmptyBody,
        ResolveErrorKind::UnknownLength,
        ResolveErrorKind::InvalidUrl,
        ResolveErrorKind::UnsupportedScheme,
        ResolveErrorKind::NeedsApproval,
        ResolveErrorKind::InvalidInput,
        ResolveErrorKind::ExploreUnavailable,
        ResolveErrorKind::Internal,
    ];
    for kind in non_structural {
        assert!(!is_structural(&kind), "{kind:?} must not degrade a recipe");
    }
}

// ---------------------------------------------------------------------------
// 2. First structural failure keeps the status, the second degrades, the
//    third breaks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn structural_failures_degrade_on_the_second_and_break_on_the_third() {
    let mut health = RecipeHealth::default();
    assert_eq!(health.status, MethodStatus::Untested);

    health.record_failure("run_1", &ResolveErrorKind::RecipeStale, Some("url"), 10);
    assert_eq!(
        health.status,
        MethodStatus::Untested,
        "one structural failure is not enough to degrade a method"
    );
    assert_eq!(health.consecutive_failures, 1);
    assert_eq!(health.last_ok_ms, None);
    let failure = health.last_failure.as_ref().expect("failure recorded");
    assert_eq!(failure.run_id, "run_1");
    assert_eq!(failure.kind, ResolveErrorKind::RecipeStale);
    assert_eq!(failure.step_id.as_deref(), Some("url"));
    assert_eq!(failure.observed_ms, 10);

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

// ---------------------------------------------------------------------------
// 3. Success -> ok, streak reset, last_ok updated, last_failure kept
// ---------------------------------------------------------------------------

#[tokio::test]
async fn success_resets_status_and_streak() {
    let mut health = RecipeHealth::default();
    health.record_failure("run_1", &ResolveErrorKind::RecipeStale, None, 10);
    health.record_failure("run_2", &ResolveErrorKind::RecipeStale, None, 20);
    assert_eq!(health.status, MethodStatus::Degraded);

    health.record_success("run_3", 30);
    assert_eq!(health.status, MethodStatus::Ok);
    assert_eq!(health.consecutive_failures, 0);
    assert_eq!(health.last_ok_ms, Some(30));
    assert!(
        health.last_failure.is_some(),
        "the last failure reference is kept for diagnostics"
    );

    // After a success a single structural failure keeps `ok` until threshold.
    health.record_failure("run_4", &ResolveErrorKind::RecipeStale, None, 40);
    assert_eq!(health.status, MethodStatus::Ok);
    assert_eq!(health.consecutive_failures, 1);
    health.record_failure("run_5", &ResolveErrorKind::RecipeStale, None, 50);
    assert_eq!(health.status, MethodStatus::Degraded);
}

// ---------------------------------------------------------------------------
// 4. Non-structural failures never degrade or count
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_structural_failures_only_record_last_failure() {
    let mut health = RecipeHealth::default();
    health.record_success("run_ok", 10);
    assert_eq!(health.status, MethodStatus::Ok);

    let kinds = [
        ResolveErrorKind::AuthRequired,
        ResolveErrorKind::RateLimited,
        ResolveErrorKind::NetworkError,
        ResolveErrorKind::ServerError,
        ResolveErrorKind::Timeout,
        ResolveErrorKind::BudgetExhausted,
        ResolveErrorKind::PolicyDenied,
        ResolveErrorKind::ModelUnavailable,
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        health.record_failure(
            &format!("run_{index}"),
            &kind,
            Some("fetch"),
            100 + index as u64,
        );
        assert_eq!(health.status, MethodStatus::Ok, "{kind:?} must not degrade");
        assert_eq!(
            health.consecutive_failures, 0,
            "{kind:?} must not count towards the streak"
        );
        assert_eq!(
            health.last_failure.as_ref().map(|failure| failure.kind),
            Some(kind)
        );
        assert_eq!(
            health
                .last_failure
                .as_ref()
                .map(|failure| failure.run_id.as_str()),
            Some(format!("run_{index}").as_str())
        );
    }
    // A success after these hiccups still works and a structural failure counts
    // from zero.
    health.record_failure(
        "run_first_structural",
        &ResolveErrorKind::RecipeStale,
        None,
        999,
    );
    assert_eq!(health.status, MethodStatus::Ok);
    assert_eq!(health.consecutive_failures, 1);
}

// ---------------------------------------------------------------------------
// 5. Serde contract: snake_case statuses and full round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serde_uses_snake_case_and_round_trips() {
    for (status, name) in [
        (MethodStatus::Ok, "ok"),
        (MethodStatus::Degraded, "degraded"),
        (MethodStatus::Broken, "broken"),
        (MethodStatus::Untested, "untested"),
    ] {
        assert_eq!(
            serde_json::to_value(status).expect("serialize status"),
            name
        );
    }

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
        evidence: vec!["step url missed".to_owned()],
    };
    let json = serde_json::to_value(&health).expect("serialize health");
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["consecutive_failures"], 2);
    assert_eq!(json["last_failure"]["kind"], "recipe_stale");
    assert_eq!(json["last_failure"]["run_id"], "run_1");
    assert_eq!(json["last_failure"]["step_id"], "url");
    let round_tripped: RecipeHealth = serde_json::from_value(json).expect("deserialize");
    assert_eq!(round_tripped, health);

    // Every field has a serde default: `{}` is the untested health.
    let default: RecipeHealth = serde_json::from_str("{}").expect("default health");
    assert_eq!(default, RecipeHealth::default());
    assert_eq!(default.status, MethodStatus::Untested);
    assert_eq!(
        serde_json::to_value(&default).expect("json")["status"],
        "untested"
    );
}

// ---------------------------------------------------------------------------
// 6. Chinese labels (non-contract, human-facing)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_labels_are_human_readable() {
    assert_eq!(MethodStatus::Ok.label_zh(), "正常");
    assert_eq!(MethodStatus::Degraded.label_zh(), "降级");
    assert_eq!(MethodStatus::Broken.label_zh(), "失效");
    assert_eq!(MethodStatus::Untested.label_zh(), "未测试");
}
