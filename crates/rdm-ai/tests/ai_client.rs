//! Integration tests for the OpenAI-compatible client and AI classification,
//! backed by the in-crate mock HTTP server (no public network, no real key).

mod support;

use std::path::PathBuf;
use std::time::Instant;

use rdm_ai::{classify, AiClient, AiError, ClassifyInput};
use support::{ai_config, local_config, MockServer, Reply};

fn classify_input(file_name: &str, existing: &[&str]) -> ClassifyInput {
    ClassifyInput {
        url: format!("https://example.com/{file_name}"),
        file_name: file_name.to_owned(),
        content_type: Some("video/mp4".to_owned()),
        root_dir: PathBuf::from("C:/Downloads"),
        existing_dirs: existing.iter().map(|dir| (*dir).to_owned()).collect(),
    }
}

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

#[tokio::test]
async fn classify_with_ai_uses_suggested_directory() {
    let server = MockServer::start(|request| {
        if request.method == "POST" {
            Reply::chat(r#"{"dir":"视频","create":true,"reason":"mp4 视频"}"#)
        } else {
            Reply::status(405, "only POST")
        }
    })
    .await;
    let outcome = rdm_ai::classify_with_ai(&ai_config(&server), &classify_input("movie.mp4", &[]))
        .await
        .expect("AI classification");
    assert_eq!(outcome.dir_name, "视频");
    assert!(outcome.ai_used);
    assert!(outcome.new_dir);
    assert_eq!(outcome.reason, "mp4 视频");
    assert_eq!(server.count_method("POST"), 1);
}

#[tokio::test]
async fn classify_with_ai_reuses_existing_directory() {
    let server = MockServer::start(|_| Reply::chat(r#"{"dir":"视频","reason":"video"}"#)).await;
    let outcome =
        rdm_ai::classify_with_ai(&ai_config(&server), &classify_input("movie.mp4", &["视频"]))
            .await
            .unwrap();
    assert_eq!(outcome.dir_name, "视频");
    assert!(!outcome.new_dir);
}

#[tokio::test]
async fn classify_with_ai_accepts_root_directory() {
    let server = MockServer::start(|_| Reply::chat(r#"{"dir":"","reason":"放根目录"}"#)).await;
    let outcome = rdm_ai::classify_with_ai(&ai_config(&server), &classify_input("movie.mp4", &[]))
        .await
        .unwrap();
    assert_eq!(outcome.dir_name, "");
    assert!(outcome.ai_used);
}

#[tokio::test]
async fn classify_with_ai_rejects_invalid_directory() {
    let server = MockServer::start(|_| {
        Reply::chat(r#"{"dir":"..\\..\\Windows","create":true,"reason":"evil"}"#)
    })
    .await;
    assert!(
        rdm_ai::classify_with_ai(&ai_config(&server), &classify_input("movie.mp4", &[]))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn classify_falls_back_to_local_on_ai_failure() {
    let server = MockServer::start(|_| Reply::status(503, "down")).await;
    let outcome = classify(&classify_input("movie.mp4", &[]), &ai_config(&server), true).await;
    assert_eq!(outcome.dir_name, "视频");
    assert!(!outcome.ai_used);
    assert!(outcome.new_dir);
}

#[tokio::test]
async fn classify_without_ai_is_local() {
    let outcome = classify(&classify_input("song.mp3", &[]), &local_config(), true).await;
    assert_eq!(outcome.dir_name, "音频");
    assert!(!outcome.ai_used);
}
