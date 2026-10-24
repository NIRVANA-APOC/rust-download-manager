//! Integration tests for the OpenAI-compatible client: chat completions and
//! live model listing (`GET {base_url}/models`), backed by the in-crate mock
//! HTTP server (no public network, no real key).

mod support;

use std::time::Instant;

use rdm_ai::{AiClient, AiError};
use support::{ai_config, local_config, MockServer, Reply};

#[tokio::test]
async fn chat_sends_openai_compatible_request() {
    let server = MockServer::start(|request| {
        if request.method == "POST" && request.path == "/v1/chat/completions" {
            Reply::chat("  hello from mock  ")
        } else {
            Reply::status(404, "not found")
        }
    })
    .await;

    let client = AiClient::new(ai_config(&server)).expect("client");
    let reply = client.chat("system prompt", "user prompt").await.unwrap();
    assert_eq!(reply, "hello from mock");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    let authorization = request.header("authorization").expect("auth header");
    assert!(authorization.starts_with("Bearer "), "{authorization}");
    assert_eq!(request.header("content-type"), Some("application/json"));

    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "mock-model");
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "system prompt");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "user prompt");
}

#[tokio::test]
async fn chat_reports_status_without_leaking_the_key() {
    let server = MockServer::start(|_| Reply::status(500, "upstream exploded")).await;
    let mut config = ai_config(&server);
    config.api_key = "status-secret-key".to_owned();

    let client = AiClient::new(config).unwrap();
    let error = client.chat("s", "u").await.unwrap_err();
    match &error {
        AiError::Status { status, body } => {
            assert_eq!(*status, 500);
            assert!(body.contains("upstream exploded"), "{body}");
        }
        other => panic!("expected Status error, got {other:?}"),
    }
    let rendered = format!("{error:?}") + &error.to_string();
    assert!(!rendered.contains("status-secret-key"), "{rendered}");
}

#[tokio::test]
async fn chat_rejects_malformed_json() {
    let server = MockServer::start(|_| Reply::json("this is not json")).await;
    let client = AiClient::new(ai_config(&server)).unwrap();
    assert!(matches!(client.chat("s", "u").await, Err(AiError::Json(_))));
}

#[tokio::test]
async fn chat_rejects_empty_content() {
    let server = MockServer::start(|_| {
        Reply::json(r#"{"choices":[{"message":{"role":"assistant","content":"   "}}]}"#)
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();
    assert!(matches!(client.chat("s", "u").await, Err(AiError::Empty)));
}

#[tokio::test]
async fn chat_requires_configuration() {
    let server = MockServer::start(|_| Reply::chat("should not be called")).await;
    let disabled = local_config();
    assert!(matches!(
        AiClient::new(disabled),
        Err(AiError::NotConfigured)
    ));

    let mut keyless = ai_config(&server);
    keyless.api_key.clear();
    assert!(matches!(
        AiClient::new(keyless),
        Err(AiError::NotConfigured)
    ));
    assert_eq!(server.request_count(), 0);
}

#[tokio::test]
async fn chat_timeout_is_enforced() {
    let server = MockServer::start(|_| Reply::chat("too late").delay(1500)).await;
    let mut config = ai_config(&server);
    config.timeout_secs = 1;
    let client = AiClient::new(config).unwrap();

    let started = Instant::now();
    let error = client.chat("s", "u").await.unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(error, AiError::Http(_)), "{error:?}");
    assert!(
        elapsed.as_millis() >= 800 && elapsed.as_millis() < 3000,
        "unexpected timeout latency: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Live model listing (`GET {base_url}/models`, design 16 §5.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_models_gets_the_live_catalog_with_bearer_auth() {
    let server = MockServer::start(|request| {
        if request.method == "GET" && request.path == "/v1/models" {
            Reply::json(
                r#"{"object":"list","data":[
                    {"id":"deepseek-v4-pro","object":"model"},
                    {"id":"deepseek-flash","object":"model"},
                    {"id":"deepseek-flash","object":"model"},
                    {"id":"DeepSeek-Reasoner","object":"model"}
                ]}"#,
            )
        } else {
            Reply::status(404, "not found")
        }
    })
    .await;

    let client = AiClient::new(ai_config(&server)).expect("client");
    let models = client.list_models().await.expect("live model list");
    // Sorted case-insensitively by model id, duplicates removed.
    assert_eq!(
        models,
        ["deepseek-flash", "DeepSeek-Reasoner", "deepseek-v4-pro"]
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/v1/models");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer test-key-12345")
    );
    let user_agent = request.header("user-agent").expect("user-agent header");
    assert!(user_agent.starts_with("rdm-ai/"), "{user_agent}");
}

#[tokio::test]
async fn list_models_normalizes_models_and_trailing_slash_bases() {
    let server = MockServer::start(|request| {
        if request.method == "GET" && request.path == "/v1/models" {
            Reply::json(r#"{"data":[{"id":"m1"}]}"#)
        } else {
            Reply::status(404, "not found")
        }
    })
    .await;

    for base in [
        server.url("/v1"),
        server.url("/v1/"),
        server.url("/v1/models"),
    ] {
        let mut config = ai_config(&server);
        config.base_url = base;
        let client = AiClient::new(config).expect("client");
        assert_eq!(
            client.list_models().await.expect("models"),
            ["m1".to_owned()]
        );
    }
}

#[tokio::test]
async fn list_models_reports_status_without_leaking_the_key() {
    let server = MockServer::start(|_| {
        Reply::status(
            401,
            r#"{"error":"the api key status-secret-key is invalid"}"#,
        )
    })
    .await;
    let mut config = ai_config(&server);
    config.api_key = "status-secret-key".to_owned();
    let client = AiClient::new(config).unwrap();

    let error = client.list_models().await.expect_err("401 must fail");
    match &error {
        AiError::Status { status, body } => {
            assert_eq!(*status, 401);
            assert!(body.contains("<redacted>"), "{body}");
        }
        other => panic!("expected Status error, got {other:?}"),
    }
    let rendered = format!("{error:?}") + &error.to_string();
    assert!(!rendered.contains("status-secret-key"), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
}

#[tokio::test]
async fn list_models_rejects_malformed_json_and_accepts_empty_lists() {
    let malformed = MockServer::start(|_| Reply::json("this is not json")).await;
    let client = AiClient::new(ai_config(&malformed)).unwrap();
    assert!(matches!(client.list_models().await, Err(AiError::Json(_))));

    let empty = MockServer::start(|_| Reply::json(r#"{"object":"list","data":[]}"#)).await;
    let client = AiClient::new(ai_config(&empty)).unwrap();
    assert!(client.list_models().await.expect("empty list").is_empty());
}
