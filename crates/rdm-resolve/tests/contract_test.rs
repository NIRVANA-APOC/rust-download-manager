//! Frozen JSON contracts: manifest/outcome/resources round-trip, backward
//! compatibility with older JSON, serde names, and the download hand-off shape.

mod common;

use rdm_resolve::{
    CredentialsHint, Diagnostics, ResolveError, ResolveErrorKind, ResolveOutcome, ResolveRequest,
    ResolvedCredentials, ResourceItem, ResourceManifest, RunStatus, SourceRef, TransferHint,
    Verification, VerificationMethod, VerificationStatus, SCHEMA_VERSION,
};

fn sample_item() -> ResourceItem {
    ResourceItem {
        resource_id: "res_fixture".to_owned(),
        url: "https://cdn.example.com/files/demo.bin?token=abc".to_owned(),
        final_url: Some("https://cdn.example.com/files/demo.bin?token=abc".to_owned()),
        name: Some("demo.bin".to_owned()),
        kind: Some("installer".to_owned()),
        content_type: Some("application/octet-stream".to_owned()),
        size_bytes: Some(4096),
        supports_ranges: Some(true),
        etag: Some("\"v1\"".to_owned()),
        last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
        expected_sha256: Some("00".repeat(32)),
        credentials: CredentialsHint {
            cookie_ref: Some("session:example".to_owned()),
            referer: Some("https://example.com/page".to_owned()),
            user_agent: None,
            headers: Default::default(),
        },
        transfer: TransferHint {
            connections: Some(6),
            ..TransferHint::default()
        },
        verification: Verification {
            status: VerificationStatus::Verified,
            method: VerificationMethod::RangedGet,
            http_status: Some(206),
            size_bytes: Some(4096),
            supports_ranges: Some(true),
            evidence: vec!["ranged-get status=206 bytes=1024".to_owned()],
            checked_ms: 1_700_000_000_000,
            ..Verification::default()
        },
        evidence: vec!["obs_1".to_owned()],
        notes: vec!["fixture".to_owned()],
    }
}

fn sample_manifest() -> ResourceManifest {
    ResourceManifest {
        schema_version: SCHEMA_VERSION.to_owned(),
        manifest_id: "mf_fixture".to_owned(),
        source: SourceRef {
            input_url: "https://example.com/d/42".to_owned(),
            final_url: Some("https://example.com/d/42".to_owned()),
            title: Some("Example".to_owned()),
            recipe_id: Some("example.com-file".to_owned()),
        },
        resources: vec![sample_item()],
        notes: vec!["resolved by recipe".to_owned()],
        created_ms: 1_700_000_000_000,
    }
}

#[test]
fn schema_version_is_frozen() {
    assert_eq!(SCHEMA_VERSION, "rdm.resolve/v1");
}

#[test]
fn manifest_round_trips_through_json() {
    let manifest = sample_manifest();

    let value = serde_json::to_value(&manifest).expect("manifest serializes");
    assert_eq!(value["schema_version"], "rdm.resolve/v1");
    assert_eq!(value["manifest_id"], "mf_fixture");
    assert_eq!(value["source"]["input_url"], "https://example.com/d/42");
    assert!(value["resources"].is_array());
    assert_eq!(value["resources"].as_array().unwrap().len(), 1);

    let text = serde_json::to_string(&manifest).expect("manifest stringifies");
    let parsed: ResourceManifest = serde_json::from_str(&text).expect("manifest deserializes");
    assert_eq!(parsed, manifest);

    let parsed_item: ResourceItem =
        serde_json::from_value(value["resources"][0].clone()).expect("resource item deserializes");
    assert_eq!(parsed_item, manifest.resources[0]);
    assert_eq!(
        parsed_item.verification.status,
        VerificationStatus::Verified
    );
    assert!(parsed_item.verification.status.is_downloadable());
}

#[test]
fn old_manifest_json_deserializes_with_defaults() {
    // A v1.1-era manifest: no notes, no optional resource fields.
    let old = serde_json::json!({
        "schema_version": "rdm.resolve/v1",
        "manifest_id": "mf_old",
        "source": { "input_url": "https://example.com/page" },
        "resources": [
            { "resource_id": "res_1", "url": "https://cdn.example.com/a.bin" }
        ],
        "created_ms": 17
    });

    let manifest: ResourceManifest =
        serde_json::from_value(old).expect("old manifest must stay readable");
    assert_eq!(manifest.resources.len(), 1);
    assert!(manifest.notes.is_empty());

    let item = &manifest.resources[0];
    assert_eq!(item.transfer, TransferHint::default());
    assert_eq!(item.credentials, CredentialsHint::default());
    assert_eq!(item.verification.status, VerificationStatus::NetworkError);
    assert_eq!(item.verification.method, VerificationMethod::HeadOnly);
    assert_eq!(item.verification.http_status, None);
    assert!(item.evidence.is_empty());
    assert!(item.notes.is_empty());
    assert!(item.supports_ranges.is_none());
}

#[test]
fn old_outcome_and_request_json_deserialize_with_defaults() {
    let old_outcome: ResolveOutcome = serde_json::from_value(serde_json::json!({
        "run_id": "run_old",
        "status": "succeeded"
    }))
    .expect("old outcome must stay readable");
    assert_eq!(old_outcome.status, RunStatus::Succeeded);
    assert!(old_outcome.manifest.is_none());
    assert!(old_outcome.recipe.is_none());
    assert_eq!(old_outcome.diagnostics.llm_calls, 0);

    let request: ResolveRequest =
        serde_json::from_value(serde_json::json!({ "url": "https://example.com/x" }))
            .expect("request defaults");
    assert_eq!(request.url, "https://example.com/x");
    assert!(request.goal.is_none());
    assert_eq!(request.mode, rdm_resolve::ResolveMode::Auto);
    assert!(request.recipe_id.is_none());
    assert!(request.budget.is_none());
    assert!(request.max_resources.is_none());

    let explicit: ResolveRequest = serde_json::from_value(serde_json::json!({
        "url": "https://example.com/x",
        "mode": "recipe_only",
        "max_resources": 3,
        "budget": { "wall_ms": 1000, "http_requests": 2, "bytes_read": 64, "redirects": 1 }
    }))
    .expect("explicit request deserializes");
    assert_eq!(explicit.mode, rdm_resolve::ResolveMode::RecipeOnly);
    assert_eq!(explicit.max_resources, Some(3));
    assert_eq!(explicit.budget.expect("budget").http_requests, 2);
}

#[test]
fn contract_enums_use_snake_case_on_the_wire() {
    let value = serde_json::to_value(VerificationStatus::HeadOnlyVerified).unwrap();
    assert_eq!(value, serde_json::json!("head_only_verified"));
    let value = serde_json::to_value(VerificationMethod::RangedGet).unwrap();
    assert_eq!(value, serde_json::json!("ranged_get"));
    let value = serde_json::to_value(ResolveErrorKind::RecipeStale).unwrap();
    assert_eq!(value, serde_json::json!("recipe_stale"));
    let value = serde_json::to_value(ResolveErrorKind::ExpectFailed).unwrap();
    assert_eq!(value, serde_json::json!("expect_failed"));
    let value = serde_json::to_value(RunStatus::BudgetExhausted).unwrap();
    assert_eq!(value, serde_json::json!("budget_exhausted"));
    let value = serde_json::to_value(rdm_resolve::ResolveMode::Explore).unwrap();
    assert_eq!(value, serde_json::json!("explore"));

    let parsed: ResolveErrorKind =
        serde_json::from_value(serde_json::json!("selector_miss")).unwrap();
    assert_eq!(parsed, ResolveErrorKind::SelectorMiss);
}

#[test]
fn failed_outcome_maps_budget_kinds_to_budget_status() {
    for (kind, expected) in [
        (
            ResolveErrorKind::BudgetExhausted,
            RunStatus::BudgetExhausted,
        ),
        (
            ResolveErrorKind::RecipeBudgetExceeded,
            RunStatus::BudgetExhausted,
        ),
        (ResolveErrorKind::RecipeNoMatch, RunStatus::Failed),
        (ResolveErrorKind::PolicyDenied, RunStatus::Failed),
        (ResolveErrorKind::Timeout, RunStatus::Failed),
    ] {
        let outcome = ResolveOutcome::failed(
            "run_fixture",
            ResolveError::new(kind, "fixture failure"),
            Diagnostics::default(),
        );
        assert_eq!(outcome.status, expected, "{kind:?}");
        assert!(outcome.manifest.is_none(), "{kind:?}");
        assert!(outcome.recipe.is_none(), "{kind:?}");
        assert_eq!(
            outcome.diagnostics.error.as_ref().map(|error| error.kind),
            Some(kind)
        );
    }
}

#[test]
fn one_time_head_only_item_becomes_a_single_stream_download() {
    // Regression for frozen semantics (docs/architecture.md §11.6): an explicit
    // `dynamic_connections = Some(true)` on a one-time item must NOT override
    // the mandatory single-stream hand-off.
    let item = ResourceItem {
        transfer: TransferHint {
            one_time: true,
            single_stream: true,
            connections: Some(8),
            dynamic_connections: Some(true),
            ..TransferHint::default()
        },
        verification: Verification {
            status: VerificationStatus::HeadOnlyVerified,
            ..Verification::default()
        },
        ..sample_item()
    }
    .with_transfer_from_verification();

    assert!(!item.transfer.resumable);
    let request = item.to_download_request(None, &ResolvedCredentials::default());
    assert_eq!(request.connections, 1);
    assert!(request.single_stream, "one-time hand-off is single-stream");
    assert!(
        !request.dynamic_connections,
        "single-stream hand-off must force dynamic_connections=false"
    );
    assert!(!request.resume);
    assert_eq!(request.chunk_size, None);
}

#[test]
fn single_stream_hint_forces_one_connection_even_with_ranges() {
    // docs/architecture.md §11.6: `single_stream` → connections=1,
    // dynamic_connections=false, resume=false, regardless of Range support.
    let item = ResourceItem {
        transfer: TransferHint {
            single_stream: true,
            connections: Some(8),
            ..TransferHint::default()
        },
        ..sample_item()
    }
    .with_transfer_from_verification();

    let request = item.to_download_request(None, &ResolvedCredentials::default());
    assert_eq!(request.connections, 1);
    assert!(request.single_stream);
    assert!(!request.dynamic_connections);
    assert!(!request.resume);
}

#[test]
fn item_without_range_support_is_forced_single_stream() {
    let item = ResourceItem {
        supports_ranges: None,
        transfer: TransferHint {
            connections: Some(8),
            ..TransferHint::default()
        },
        ..sample_item()
    }
    .with_transfer_from_verification();

    let request = item.to_download_request(None, &ResolvedCredentials::default());
    assert_eq!(request.connections, 1);
    assert!(request.single_stream);
    assert!(!request.dynamic_connections);
    assert!(!request.resume);
}

#[test]
fn verified_ranged_item_keeps_configured_connections() {
    let item = ResourceItem {
        transfer: TransferHint {
            connections: Some(6),
            ..TransferHint::default()
        },
        ..sample_item()
    }
    .with_transfer_from_verification();

    assert!(item.transfer.resumable);
    let request = item.to_download_request(None, &ResolvedCredentials::default());
    assert_eq!(request.connections, 6);
    assert!(
        !request.single_stream,
        "a ranged, resumable resource keeps the multi-connection path"
    );
    assert!(request.dynamic_connections);
    assert!(request.resume);
    assert_eq!(request.expected_sha256, item.expected_sha256);
    assert_eq!(request.file_name.as_deref(), Some("demo.bin"));
    assert_eq!(request.referer, Some("https://example.com/page".to_owned()));
}

#[test]
fn resolved_credentials_override_manifest_hints() {
    let item = sample_item();
    let credentials = ResolvedCredentials {
        cookie: Some("session=fresh".to_owned()),
        referer: Some("https://example.com/override".to_owned()),
        user_agent: Some("fixture-agent/1.0".to_owned()),
        proxy: Some("http://127.0.0.1:3128".to_owned()),
    };

    let request = item.to_download_request(None, &credentials);

    assert_eq!(request.cookie.as_deref(), Some("session=fresh"));
    assert_eq!(
        request.referer.as_deref(),
        Some("https://example.com/override"),
        "explicit credentials win over manifest hints"
    );
    assert_eq!(request.user_agent.as_deref(), Some("fixture-agent/1.0"));
    assert_eq!(request.proxy.as_deref(), Some("http://127.0.0.1:3128"));
}
