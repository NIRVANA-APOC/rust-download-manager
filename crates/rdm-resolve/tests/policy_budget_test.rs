//! Network policy and budget enforcement (acceptance: `docs/agent-integration.md`
//! §8.2 "私网/非 http(s) → policy_denied; 预算耗尽 → budget_exhausted").

mod common;
mod support;

use rdm_resolve::contract::{ResolveErrorKind, RunStatus};
use rdm_resolve::http::HttpRequestSpec;
use rdm_resolve::{Budget, RecipeRegistry, ResolvePolicy, ResolverService};
use url::Url;

use common::{
    http_tool, loopback_policy, recipe_json, resolve_request, test_budget, RecipeFixture,
};
use support::{MockServer, Reply};

#[tokio::test]
async fn default_policy_denies_private_and_loopback_hosts() {
    let policy = ResolvePolicy::default();
    for raw in [
        "http://127.0.0.1:8080/x.bin",
        "http://127.1.2.3/x.bin",
        "http://localhost/x.bin",
        "http://10.0.0.5/x.bin",
        "http://192.168.1.10/x.bin",
        "http://169.254.10.1/x.bin",
        "http://[::1]/x.bin",
    ] {
        let error = policy
            .check_url(&Url::parse(raw).expect("valid URL"))
            .expect_err("private host must be denied");
        assert_eq!(error.kind, ResolveErrorKind::PolicyDenied, "{raw}");
    }

    // The loopback policy exists precisely for local fixture servers.
    let loopback = ResolvePolicy::loopback();
    assert!(loopback
        .check_url(&Url::parse("http://127.0.0.1:8080/x.bin").expect("valid URL"))
        .is_ok());
}

#[tokio::test]
async fn non_http_schemes_are_rejected() {
    let policy = ResolvePolicy::loopback();
    for raw in [
        "ftp://example.com/file.iso",
        "file:///C:/Windows/win.ini",
        "data:text/plain,hi",
    ] {
        let error = policy
            .check_url(&Url::parse(raw).expect("valid URL"))
            .expect_err("non-http(s) scheme must be denied");
        assert_eq!(error.kind, ResolveErrorKind::UnsupportedScheme, "{raw}");
    }
}

#[tokio::test]
async fn service_maps_policy_denial_to_policy_denied_outcome() {
    let server = MockServer::start(|_| Reply::file(16)).await;
    let mut service = ResolverService::new(RecipeRegistry::empty(), ResolvePolicy::default());

    let outcome = service
        .resolve(resolve_request(&server.url("/x.bin")))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::PolicyDenied)
    );
    assert!(outcome.manifest.is_none());
    assert_eq!(
        server.request_count(),
        0,
        "a policy-denied run must not issue any request"
    );
}

#[tokio::test]
async fn service_rejects_non_http_scheme() {
    let mut service = ResolverService::new(RecipeRegistry::empty(), ResolvePolicy::loopback());

    let outcome = service
        .resolve(resolve_request("ftp://example.com/file.iso"))
        .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::UnsupportedScheme)
    );
    assert!(outcome.manifest.is_none());
}

#[tokio::test]
async fn http_request_budget_exhausts_after_one_request() {
    let server = MockServer::start(|_| Reply::file(8)).await;
    let mut http = http_tool(
        loopback_policy(),
        Budget {
            http_requests: 1,
            ..test_budget()
        },
    );

    assert!(http
        .request(HttpRequestSpec::get(server.url("/first")))
        .await
        .is_ok());
    let error = http
        .request(HttpRequestSpec::get(server.url("/second")))
        .await
        .expect_err("request budget must stop the second request");

    assert_eq!(error.kind, ResolveErrorKind::BudgetExhausted);
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn byte_budget_stops_reading() {
    let server = MockServer::start(|_| Reply::file(100)).await;
    let mut http = http_tool(
        loopback_policy(),
        Budget {
            bytes_read: 4,
            ..test_budget()
        },
    );

    let error = http
        .request(HttpRequestSpec::get(server.url("/big.bin")).max_bytes(1024))
        .await
        .expect_err("byte budget must stop the body read");

    assert_eq!(error.kind, ResolveErrorKind::BudgetExhausted);
}

#[tokio::test]
async fn redirect_budget_stops_the_hop_chain() {
    let server = MockServer::start(|_| Reply::redirect("/next")).await;
    let mut http = http_tool(
        loopback_policy(),
        Budget {
            redirects: 0,
            ..test_budget()
        },
    );

    let error = http
        .request(HttpRequestSpec::head(server.url("/start")))
        .await
        .expect_err("redirect budget must stop the first hop");

    assert_eq!(error.kind, ResolveErrorKind::BudgetExhausted);
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn service_reports_http_budget_exhaustion_instead_of_success() {
    let server = MockServer::start(|_| Reply::json("{\"ok\":true}")).await;
    let fixture = RecipeFixture::new("budget-http");
    fixture.save(&recipe_json(
        "budget-http-recipe",
        1,
        serde_json::json!({ "hosts": ["127.0.0.1"] }),
        serde_json::json!([{
            "id": "fetch",
            "op": "http_request",
            "args": { "url": server.url("/api/item") },
            "assign": { "body": "body_text" }
        }]),
        serde_json::json!({ "url": server.url("/files/item.bin") }),
    ));
    let mut request = resolve_request(&server.url("/page/1"));
    request.budget = Some(Budget {
        http_requests: 0,
        ..test_budget()
    });

    let outcome = fixture.service().resolve(request).await;

    assert_eq!(outcome.status, RunStatus::BudgetExhausted);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::BudgetExhausted)
    );
    assert!(
        outcome.manifest.is_none(),
        "budget exhaustion is not success"
    );
    assert_eq!(
        server.request_count(),
        0,
        "a zero request budget must not send anything"
    );
}

#[tokio::test]
async fn service_reports_byte_budget_exhaustion_instead_of_success() {
    let server = MockServer::start(|_| Reply::json("{\"value\":\"a long enough body\"}")).await;
    let fixture = RecipeFixture::new("budget-bytes");
    fixture.save(&recipe_json(
        "budget-bytes-recipe",
        1,
        serde_json::json!({ "hosts": ["127.0.0.1"] }),
        serde_json::json!([{
            "id": "fetch",
            "op": "http_request",
            "args": { "url": server.url("/api/item") },
            "assign": { "body": "body_text" }
        }]),
        serde_json::json!({ "url": server.url("/files/item.bin") }),
    ));
    let mut request = resolve_request(&server.url("/page/1"));
    request.budget = Some(Budget {
        bytes_read: 2,
        ..test_budget()
    });

    let outcome = fixture.service().resolve(request).await;

    assert_eq!(outcome.status, RunStatus::BudgetExhausted);
    assert_eq!(
        outcome.diagnostics.error.as_ref().map(|error| error.kind),
        Some(ResolveErrorKind::BudgetExhausted)
    );
    assert!(outcome.manifest.is_none());
}

#[tokio::test]
async fn budget_tracker_counts_requests_and_bytes() {
    let mut tracker = rdm_resolve::BudgetTracker::new(Budget {
        http_requests: 2,
        bytes_read: 16,
        redirects: 1,
        wall_ms: 60_000,
    });

    tracker.take_request().expect("first request fits");
    tracker.take_request().expect("second request fits");
    assert_eq!(tracker.requests(), 2);
    assert_eq!(
        tracker.take_request().expect_err("budget is spent").kind,
        ResolveErrorKind::BudgetExhausted
    );
    tracker.add_bytes(8).expect("within byte budget");
    assert_eq!(tracker.bytes(), 8);
    assert_eq!(
        tracker.add_bytes(9).expect_err("byte cap exceeded").kind,
        ResolveErrorKind::BudgetExhausted
    );
}

#[test]
fn wall_clock_budget_expires_into_budget_exhausted() {
    let tracker = rdm_resolve::BudgetTracker::new(Budget {
        wall_ms: 1,
        ..Budget::default()
    });
    std::thread::sleep(std::time::Duration::from_millis(5));

    let error = tracker
        .check_deadline()
        .expect_err("the wall-clock deadline has passed");
    assert_eq!(error.kind, ResolveErrorKind::BudgetExhausted);
    assert!(error.detail.contains("wall-clock"));
}
