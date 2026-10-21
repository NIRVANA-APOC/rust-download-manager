//! Streaming (`chat_stream`) tests: SSE parsing, plain-JSON fallback, partial
//! replies after a mid-stream failure and API-key redaction.
//!
//! The mock server lives in `tests/support/mod.rs`; streamed replies are written
//! as HTTP/1.1 chunks with a delay between them, so a client that buffers the
//! whole body before parsing cannot pass the incremental-delivery assertions.

mod support;

use std::time::{Duration, Instant};

use rdm_ai::{AiClient, AiConfig, AiError};
use support::{ai_config, sse_event, MockServer, Reply};

/// One OpenAI-compatible streaming chunk carrying `content`.
fn delta_json(content: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}],
    })
    .to_string()
}

/// Collect the deltas (in order) together with their arrival times.
fn recording_callback<'a>(
    deltas: &'a mut Vec<String>,
    arrivals: &'a mut Vec<Instant>,
) -> impl FnMut(&str) + 'a {
    move |delta: &str| {
        deltas.push(delta.to_owned());
        arrivals.push(Instant::now());
    }
}

#[tokio::test]
async fn chat_stream_parses_sse_deltas_incrementally() {
    let server = MockServer::start(|_| {
        Reply::sse(vec![
            sse_event(&delta_json("Hel")),
            sse_event(&delta_json("lo, ")),
            sse_event(&delta_json("world!")),
            sse_event("[DONE]"),
        ])
        .chunk_delay(120)
    })
    .await;
    let client = AiClient::new(ai_config(&server)).expect("client");

    let mut deltas = Vec::new();
    let mut arrivals = Vec::new();
    let reply = client
        .chat_stream(
            "system prompt",
            "user prompt",
            recording_callback(&mut deltas, &mut arrivals),
        )
        .await
        .expect("streamed reply");

    assert_eq!(reply, "Hello, world!");
    assert_eq!(deltas, ["Hel", "lo, ", "world!"]);
    assert_eq!(arrivals.len(), 3);

    // The server flushed the chunks 120ms apart; a client that only parsed the
    // buffered body would invoke every callback at almost the same instant.
    let max_gap = arrivals
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .max()
        .expect("at least two arrivals");
    assert!(
        max_gap >= Duration::from_millis(60),
        "deltas arrived together ({max_gap:?}), the client buffered the stream"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("request JSON");
    assert_eq!(body["stream"], serde_json::Value::Bool(true));
    assert_eq!(body["model"], "mock-model");
    assert_eq!(body["messages"][0]["content"], "system prompt");
    assert_eq!(body["messages"][1]["content"], "user prompt");
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert!(request.header("authorization").is_some());
}

#[tokio::test]
async fn chat_stream_reassembles_events_split_across_network_chunks() {
    let server = MockServer::start(|_| {
        // The first chunk holds one complete event plus the beginning of the
        // second one; the second chunk closes it. A real TCP stream fragments
        // this way, so the parser must buffer partial lines.
        let second_event = sse_event(&delta_json("B"));
        let split_at = second_event.find("\"B").expect("content marker") + 2;
        let (event_head, event_tail) = second_event.split_at(split_at);
        let first_chunk = format!("{}{}", sse_event(&delta_json("A")), event_head);
        let second_chunk = format!("{event_tail}{}", sse_event("[DONE]"));
        Reply::sse(vec![first_chunk, second_chunk]).chunk_delay(40)
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    let mut deltas = Vec::new();
    let reply = client
        .chat_stream("s", "u", |delta: &str| deltas.push(delta.to_owned()))
        .await
        .unwrap();
    assert_eq!(reply, "AB");
    assert_eq!(deltas, ["A", "B"]);
}

#[tokio::test]
async fn chat_stream_handles_crlf_keep_alive_and_message_content() {
    let server = MockServer::start(|_| {
        Reply::sse(vec![
            // Comments / keep-alive lines are ignored.
            ": keep-alive\r\n\r\n".to_owned(),
            "\r\n".to_owned(),
            // A role-only first delta carries no content.
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\r\n\r\n".to_owned(),
            // No space after the `data:` field name.
            format!("data:{}\r\n\r\n", delta_json("Hi")),
            // A gateway wrongly using the non-streaming `message.content` shape.
            format!(
                "data: {}\r\n\r\n",
                serde_json::json!({"choices":[{"message":{"content":" there"}}]})
            ),
            sse_event("[DONE]"),
        ])
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    let mut deltas = Vec::new();
    let reply = client
        .chat_stream("s", "u", |delta: &str| deltas.push(delta.to_owned()))
        .await
        .unwrap();
    assert_eq!(reply, "Hi there");
    assert_eq!(deltas, ["Hi", " there"]);
}

#[tokio::test]
async fn chat_stream_accepts_stream_ending_without_done_sentinel() {
    let server = MockServer::start(|_| {
        Reply::sse(vec![
            sse_event(&delta_json("no ")),
            sse_event(&delta_json("sentinel")),
        ])
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    let mut deltas = Vec::new();
    let reply = client
        .chat_stream("s", "u", |delta: &str| deltas.push(delta.to_owned()))
        .await
        .unwrap();
    assert_eq!(reply, "no sentinel");
    assert_eq!(deltas, ["no ", "sentinel"]);
}

#[tokio::test]
async fn chat_stream_falls_back_to_plain_json_reply() {
    // A gateway that ignores `stream` and answers with a normal completion.
    let server = MockServer::start(|_| Reply::chat("  fallback reply  ")).await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    let mut deltas = Vec::new();
    let reply = client
        .chat_stream("s", "u", |delta: &str| deltas.push(delta.to_owned()))
        .await
        .unwrap();
    assert_eq!(reply, "fallback reply");
    assert_eq!(deltas, ["fallback reply"]);
}

#[tokio::test]
async fn chat_stream_falls_back_when_content_type_is_missing() {
    let server = MockServer::start(|_| {
        Reply::status(200, r#"{"choices":[{"message":{"content":"plain body"}}]}"#)
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    let mut deltas = Vec::new();
    let reply = client
        .chat_stream("s", "u", |delta: &str| deltas.push(delta.to_owned()))
        .await
        .unwrap();
    assert_eq!(reply, "plain body");
    assert_eq!(deltas, ["plain body"]);
}

#[tokio::test]
async fn chat_stream_reports_http_status_without_leaking_the_key() {
    let server =
        MockServer::start(|_| Reply::status(500, "upstream exploded: stream-status-key")).await;
    let mut config = ai_config(&server);
    config.api_key = "stream-status-key".to_owned();
    let client = AiClient::new(config).unwrap();

    let error = client
        .chat_stream("s", "u", |_delta: &str| panic!("no delta expected"))
        .await
        .unwrap_err();
    match &error {
        AiError::Status { status, body } => {
            assert_eq!(*status, 500);
            assert!(body.contains("upstream exploded"), "{body}");
            assert!(!body.contains("stream-status-key"), "{body}");
            assert!(body.contains("<redacted>"), "{body}");
        }
        other => panic!("expected Status error, got {other:?}"),
    }
    let rendered = format!("{error:?}") + &error.to_string();
    assert!(!rendered.contains("stream-status-key"), "{rendered}");
}

#[tokio::test]
async fn chat_stream_keeps_partial_text_when_connection_breaks() {
    let server = MockServer::start(|_| {
        // The provider sends one delta and then dies without the terminating
        // zero chunk: the client sees that delta, then a truncated body error.
        Reply::sse(vec![sse_event(&delta_json("partial "))])
            .chunk_delay(60)
            .abort_stream()
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    let mut deltas = Vec::new();
    let reply = client
        .chat_stream("s", "u", |delta: &str| deltas.push(delta.to_owned()))
        .await
        .expect("partial reply, not an error");
    assert_eq!(reply, "partial ");
    assert_eq!(deltas, ["partial "]);
}

#[tokio::test]
async fn chat_stream_keeps_partial_text_on_malformed_event() {
    let server = MockServer::start(|_| {
        Reply::sse(vec![
            sse_event(&delta_json("kept")),
            "data: this-is-not-json\r\n\r\n".to_owned(),
            sse_event("[DONE]"),
        ])
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    let mut deltas = Vec::new();
    let reply = client
        .chat_stream("s", "u", |delta: &str| deltas.push(delta.to_owned()))
        .await
        .expect("partial reply, not an error");
    assert_eq!(reply, "kept");
    assert_eq!(deltas, ["kept"]);
}

#[tokio::test]
async fn chat_stream_reports_malformed_event_without_partial_text() {
    let server = MockServer::start(|_| {
        // Invalid JSON that embeds the API key: the error must redact it.
        Reply::sse(vec!["data: {api_key:sse-secret-key}\r\n\r\n".to_owned()])
    })
    .await;
    let mut config = ai_config(&server);
    config.api_key = "sse-secret-key".to_owned();
    let client = AiClient::new(config).unwrap();

    let error = client
        .chat_stream("s", "u", |_delta: &str| panic!("no delta expected"))
        .await
        .unwrap_err();
    assert!(matches!(error, AiError::Json(_)), "{error:?}");
    let rendered = format!("{error:?}") + &error.to_string();
    assert!(!rendered.contains("sse-secret-key"), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
}

#[tokio::test]
async fn chat_stream_without_any_content_is_empty() {
    let server = MockServer::start(|_| {
        Reply::sse(vec![": keep-alive\r\n\r\n".to_owned(), sse_event("[DONE]")])
    })
    .await;
    let client = AiClient::new(ai_config(&server)).unwrap();

    assert!(matches!(
        client.chat_stream("s", "u", |_delta: &str| {}).await,
        Err(AiError::Empty)
    ));
}

#[tokio::test]
async fn chat_stream_requires_configuration() {
    let server = MockServer::start(|_| Reply::chat("should not be called")).await;

    let mut disabled = ai_config(&server);
    disabled.enabled = false;
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

/// Compile-time guard used by the GUI, which spawns the stream future on a
/// multi-threaded runtime and passes `&mut closure` as the callback.
#[tokio::test]
async fn chat_stream_future_is_send_with_a_send_callback() {
    fn require_send<T: Send>(_: T) {}

    let config = AiConfig {
        enabled: true,
        api_key: "compile-key".into(),
        ..AiConfig::default()
    };
    let client = AiClient::new(config).unwrap();
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut on_delta = move |delta: &str| {
        let _ = sender.send(delta.to_owned());
    };
    require_send(client.chat_stream("system", "user", &mut on_delta));
}
