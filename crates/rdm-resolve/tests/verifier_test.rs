//! Verifier matrix: status codes, HTML/login detection, one-time URLs,
//! HEAD-rejection fallback and cross-origin credential stripping.
//!
//! Every case is served by a local [`support::MockServer`]: no public network,
//! ephemeral ports, fully deterministic.

mod common;
mod support;

use rdm_resolve::http::HttpRequestSpec;
use rdm_resolve::{ResolvePolicy, VerificationMethod, VerificationStatus, VerifyHints};

use common::{http_tool, loopback_policy, test_budget, verify, verify_with};
use support::{MockServer, Reply};

#[tokio::test]
async fn direct_file_is_verified_with_evidence() {
    let server = MockServer::start(|request| match request.method.as_str() {
        "HEAD" => Reply::file_head(2048)
            .header("accept-ranges", "bytes")
            .header("etag", "\"v1\"")
            .header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
        _ => Reply::file(2048)
            .header("accept-ranges", "bytes")
            .header("etag", "\"v1\"")
            .header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
    })
    .await;
    let url = server.url("/files/demo.bin");

    let verification = verify(&url).await;

    assert_eq!(verification.status, VerificationStatus::Verified);
    assert!(verification.status.is_downloadable());
    assert!(
        !verification.evidence.is_empty(),
        "verification must carry evidence lines"
    );
    assert_eq!(verification.http_status, Some(200));
    assert_eq!(verification.size_bytes, Some(2048));
    assert_eq!(
        verification.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(verification.supports_ranges, Some(true));
    assert_eq!(verification.etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        verification.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert_eq!(verification.method, VerificationMethod::BoundedGet);
    assert_eq!(verification.final_url.as_deref(), Some(url.as_str()));
    assert!(verification.checked_ms > 0);
    assert_eq!(server.count_method("HEAD"), 1);
    assert_eq!(server.count_method("GET"), 1);
}

#[tokio::test]
async fn html_page_is_not_a_download() {
    let server =
        MockServer::start(|_| Reply::html("<!doctype html><html><body>landing</body></html>"))
            .await;

    let verification = verify(&server.url("/page")).await;

    assert_eq!(verification.status, VerificationStatus::HtmlPage);
    assert!(!verification.status.is_downloadable());
    assert_eq!(verification.http_status, Some(200));
    assert_eq!(
        server.request_count(),
        1,
        "an HTML response is rejected on HEAD, before any body read"
    );
}

#[tokio::test]
async fn html_login_redirect_maps_to_login_required() {
    let server = MockServer::start(|request| match request.path.as_str() {
        "/login" => Reply::html("<!doctype html><html><form></form></html>"),
        _ => Reply::redirect("/login"),
    })
    .await;

    let verification = verify(&server.url("/download/42")).await;

    assert_eq!(verification.status, VerificationStatus::LoginRequired);
    assert!(!verification.status.is_downloadable());
    assert!(
        verification
            .final_url
            .as_deref()
            .unwrap_or_default()
            .ends_with("/login"),
        "final_url must record the login page: {:?}",
        verification.final_url
    );
    assert_eq!(server.request_count(), 2);
    assert_eq!(server.count_method("HEAD"), 2);
    assert_eq!(server.count_method("GET"), 0);
}

#[tokio::test]
async fn http_statuses_map_to_typed_verification_status() {
    let cases = [
        (401, VerificationStatus::LoginRequired),
        (403, VerificationStatus::ForbiddenOrExpired),
        (410, VerificationStatus::ForbiddenOrExpired),
        (404, VerificationStatus::NotFound),
        (429, VerificationStatus::RateLimited),
        (500, VerificationStatus::ServerError),
        (503, VerificationStatus::ServerError),
    ];

    for (status, expected) in cases {
        let server = MockServer::start(move |_| Reply::status(status)).await;
        let verification = verify(&server.url("/file.bin")).await;
        assert_eq!(verification.status, expected, "HTTP status {status}");
        assert_eq!(
            verification.http_status,
            Some(status),
            "HTTP status {status}"
        );
        assert!(
            !verification.status.is_downloadable(),
            "HTTP status {status} must not be downloadable"
        );
        assert_eq!(
            server.request_count(),
            1,
            "HTTP status {status} must stop after the HEAD probe"
        );
    }
}

#[tokio::test]
async fn zero_length_body_is_empty_body() {
    let server = MockServer::start(|_| Reply {
        status: 200,
        content_type: Some("application/octet-stream".to_owned()),
        content_length: Some(0),
        ..Reply::default()
    })
    .await;

    let verification = verify(&server.url("/empty.bin")).await;

    assert_eq!(verification.status, VerificationStatus::EmptyBody);
    assert!(!verification.status.is_downloadable());
    assert_eq!(verification.size_bytes, Some(0));
    assert_eq!(server.count_method("GET"), 1);
}

#[tokio::test]
async fn one_time_hint_never_issues_a_get() {
    let server = MockServer::start(|request| match request.method.as_str() {
        "HEAD" => Reply::file_head(4096),
        _ => Reply::file(4096),
    })
    .await;
    let hints = VerifyHints {
        one_time: true,
        ..VerifyHints::default()
    };

    let verification = verify_with(
        &server.url("/signed/one-time.bin"),
        &hints,
        loopback_policy(),
        test_budget(),
    )
    .await;

    assert_eq!(verification.status, VerificationStatus::HeadOnlyVerified);
    assert!(verification.status.is_downloadable());
    assert_eq!(verification.method, VerificationMethod::HeadOnly);
    assert_eq!(verification.size_bytes, Some(4096));
    assert_eq!(
        server.request_count(),
        1,
        "a one-time URL must be probed with exactly one HEAD request"
    );
    assert_eq!(server.count_method("GET"), 0);
}

#[tokio::test]
async fn head_405_falls_back_to_a_bounded_get() {
    let server = MockServer::start(|request| match request.method.as_str() {
        "HEAD" => Reply::status(405).header("allow", "GET, HEAD"),
        _ => Reply::file(1024),
    })
    .await;

    let verification = verify(&server.url("/files/legacy.bin")).await;

    assert_eq!(
        server.count_method("HEAD"),
        1,
        "the body probe starts with HEAD"
    );
    assert_eq!(
        server.count_method("GET"),
        1,
        "a server that rejects HEAD (405) must be retried with one bounded GET"
    );
    assert_eq!(verification.status, VerificationStatus::Verified);
    assert!(verification.status.is_downloadable());
    assert_eq!(verification.http_status, Some(200));
}

#[tokio::test]
async fn cross_origin_redirect_drops_credentials() {
    let target = MockServer::start(|request| match request.method.as_str() {
        "HEAD" => Reply::file_head(1024),
        _ => Reply::file(1024),
    })
    .await;
    let target_url = target.url("/files/moved.bin");
    let handler_url = target_url.clone();
    let origin = MockServer::start(move |_| Reply::redirect(&handler_url)).await;

    let mut http = http_tool(loopback_policy(), test_budget());
    let spec = HttpRequestSpec::head(origin.url("/start"))
        .header("cookie", "session=top-secret")
        .header("authorization", "Bearer top-secret");
    let observation = http
        .request(spec)
        .await
        .expect("cross-origin redirect is followed");

    assert_eq!(observation.status, 200);
    assert_eq!(observation.final_url, target_url);
    assert_eq!(origin.request_count(), 1);
    assert_eq!(target.request_count(), 1);

    let first = &origin.requests()[0];
    assert_eq!(first.header("cookie"), Some("session=top-secret"));

    let second = &target.requests()[0];
    assert!(
        second.header("cookie").is_none(),
        "cookie leaked across origins: {:?}",
        second.header("cookie")
    );
    assert!(
        second.header("authorization").is_none(),
        "authorization leaked across origins: {:?}",
        second.header("authorization")
    );
}

#[tokio::test]
async fn same_origin_redirect_keeps_credentials() {
    let server = MockServer::start(|request| match request.path.as_str() {
        "/login" => Reply::file_head(512),
        _ => Reply::redirect("/login"),
    })
    .await;

    let mut http = http_tool(loopback_policy(), test_budget());
    let spec = HttpRequestSpec::head(server.url("/start")).header("cookie", "a=b");
    let observation = http.request(spec).await.expect("same-origin redirect");

    assert_eq!(observation.status, 200);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("cookie"), Some("a=b"));
    assert_eq!(
        requests[1].header("cookie"),
        Some("a=b"),
        "same-origin hops keep the caller's cookie"
    );
}

#[tokio::test]
async fn verifier_reports_policy_denied_before_touching_the_network() {
    let server = MockServer::start(|_| Reply::file(16)).await;

    let verification = verify_with(
        &server.url("/blocked.bin"),
        &VerifyHints::default(),
        ResolvePolicy::default(),
        test_budget(),
    )
    .await;

    assert_eq!(verification.status, VerificationStatus::PolicyDenied);
    assert_eq!(verification.http_status, None);
    assert!(!verification.status.is_downloadable());
    assert_eq!(
        server.request_count(),
        0,
        "a policy-denied URL must never reach the network"
    );
}

#[tokio::test]
async fn redirect_cycle_exhausts_the_redirect_budget() {
    // Every hop points at itself: the only way out is the redirect budget.
    let server = MockServer::start(|_| Reply::redirect("/loop")).await;

    let verification = verify(&server.url("/loop-a")).await;

    assert_eq!(verification.status, VerificationStatus::NetworkError);
    assert!(
        verification
            .evidence
            .iter()
            .any(|line| line.contains("BudgetExhausted")),
        "redirect-budget exhaustion must be recorded in the evidence: {:?}",
        verification.evidence
    );
}
