//! Self-test for the scripted mock LLM helper (`tests/common/llm.rs`).
//!
//! Kept separate from `explore_test.rs` so the mock infrastructure can be
//! validated independently of the exploration implementation: these tests use
//! only the raw HTTP surface (`reqwest` is already a dependency of the crate,
//! built without its `json` feature, hence the manual JSON decoding).

mod common;

use common::llm::{LlmReply, MockLlm};
use serde_json::json;

fn endpoint(llm: &MockLlm) -> String {
    format!("{}/v1/chat/completions", llm.base_url())
}

/// POST one chat-completions body and return `(status, decoded JSON body)`.
async fn post_chat(llm: &MockLlm, key: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(endpoint(llm))
        .header("Authorization", format!("Bearer {key}"))
        .body(serde_json::to_vec(&body).expect("encode request body"))
        .send()
        .await
        .expect("mock LLM request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("read mock LLM reply");
    let value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("mock LLM reply is not JSON: {error}\n{text}"));
    (status, value)
}

fn content(value: &serde_json::Value) -> &str {
    value
        .pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no assistant content in {value}"))
}

#[tokio::test]
async fn serves_scripted_content_and_records_requests() {
    let llm = MockLlm::start(vec![
        LlmReply::content("first reply"),
        LlmReply::content("second reply"),
    ])
    .await;

    let body = json!({
        "model": "scripted-model",
        "messages": [
            {"role": "system", "content": "system prompt"},
            {"role": "user", "content": "hello"},
        ],
    });
    let (status, first) = post_chat(&llm, "test-key", body.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(content(&first), "first reply");

    let (status, second) = post_chat(&llm, "test-key", body).await;
    assert_eq!(status, 200);
    assert_eq!(content(&second), "second reply");

    assert_eq!(llm.request_count(), 2);
    let request = llm.request(0);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.bearer_token(), Some("test-key"));
    assert_eq!(request.roles(), vec!["system", "user"]);
    assert_eq!(request.contents(), vec!["system prompt", "hello"]);
    assert!(request.key_leaks("test-key").is_empty(), "key leaks");
    // The configured base URL must be exactly what the mock serves.
    assert_eq!(endpoint(&llm), llm.url("/v1/chat/completions"));
}

#[tokio::test]
async fn dynamic_reply_sees_the_request() {
    let llm = MockLlm::start(vec![LlmReply::dynamic(|request| {
        format!("saw {} messages", request.messages().len())
    })])
    .await;
    let (status, response) = post_chat(
        &llm,
        "k",
        json!({"messages": [{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(content(&response), "saw 1 messages");
}

#[tokio::test]
async fn http_error_status_is_returned_to_the_client() {
    let llm = MockLlm::start(vec![LlmReply::status(500)]).await;
    let (status, body) = post_chat(&llm, "k", json!({"messages": []})).await;
    assert_eq!(status, 500);
    assert!(body
        .pointer("/error/message")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .contains("scripted HTTP 500"));
    assert_eq!(llm.request_count(), 1);
}

#[tokio::test]
async fn extra_requests_beyond_the_script_fail_loudly() {
    let llm = MockLlm::start(vec![]).await;
    let (status, body) = post_chat(&llm, "k", json!({"messages": []})).await;
    assert_eq!(status, 500);
    let message = body
        .pointer("/error/message")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert!(
        message.contains("no scripted reply left"),
        "unexpected body: {message}"
    );
}

#[tokio::test]
async fn delayed_reply_is_served_after_the_delay() {
    let llm = MockLlm::start(vec![LlmReply::delayed(120, "slow reply")]).await;
    let started = std::time::Instant::now();
    let (status, response) = post_chat(&llm, "k", json!({"messages": []})).await;
    assert_eq!(status, 200);
    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    assert_eq!(content(&response), "slow reply");
}
