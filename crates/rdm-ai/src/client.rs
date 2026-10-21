//! Minimal OpenAI-compatible chat client (`docs/design-next-features.md` §3.3).
//!
//! Requests look like:
//! `POST {endpoint}` with `Authorization: Bearer <key>` and a JSON body
//! `{"model","messages":[…],"temperature":0.2}`; replies are read as
//! `choices[0].message.content`.
//!
//! [`AiClient::chat_stream`] additionally sets `"stream": true` and consumes a
//! `text/event-stream` (SSE) reply, forwarding every `choices[0].delta.content`
//! fragment to a caller-supplied callback as soon as it arrives. A provider
//! that ignores `stream` and answers with plain JSON is handled by falling back
//! to the non-streaming parser.
//!
//! Security: the API key only ever appears in the `Authorization` header. It is
//! scrubbed from error text and never formatted into `Debug` output.

use std::fmt;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::AiConfig;

/// Upper bound for AI calls triggered from GUI actions (parse/classify), so a
/// slow provider can never block the UI for long (contract: ≤ 15s).
pub const INTERNAL_TIMEOUT_CAP_SECS: u64 = 15;

/// Copy `config` with the timeout clamped to [`INTERNAL_TIMEOUT_CAP_SECS`].
pub(crate) fn bounded_config(config: &AiConfig) -> AiConfig {
    let mut bounded = config.clone();
    bounded.timeout_secs = bounded.timeout_secs.clamp(1, INTERNAL_TIMEOUT_CAP_SECS);
    bounded
}

/// Why an AI call failed. No variant carries the API key.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// AI is disabled or has no API key.
    #[error("AI provider is not configured (enable it and set an API key)")]
    NotConfigured,
    /// Transport-level failure (DNS, connection, timeout, …).
    #[error("AI request failed: {0}")]
    Http(String),
    /// The provider answered with a non-2xx status.
    #[error("AI provider returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    /// The provider reply was not the expected JSON shape.
    #[error("AI response could not be parsed: {0}")]
    Json(String),
    /// The provider reply contained no message content.
    #[error("AI response was empty")]
    Empty,
    /// Anything else (client construction, invalid header, …).
    #[error("AI error: {0}")]
    Other(String),
}

/// Role of one message in a multi-turn chat context.
///
/// Serialized `snake_case` so it matches the OpenAI wire protocol
/// (`system`/`user`/`assistant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

impl ChatRole {
    /// Wire name used by OpenAI-compatible providers.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One message of a multi-turn chat context (`rdm-ai` v0.4 addition).
///
/// The resolver uses these to keep a bounded exploration context: a system
/// prompt, the initial user request and alternating assistant/tool turns.
/// Builders keep the call sites readable; the fields stay public so callers can
/// pattern-match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(ChatRole::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(ChatRole::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(ChatRole::Assistant, content)
    }
}

/// OpenAI-compatible chat client.
pub struct AiClient {
    config: AiConfig,
    http: reqwest::Client,
    /// 流式聊天专用 client：`timeout_secs` 只是普通请求的总超时预算，
    /// 而流式回答可能持续数分钟，因此为流式放宽总超时（见 [`STREAM_TIMEOUT_FLOOR_SECS`]）。
    http_stream: reqwest::Client,
}

/// 流式请求的总超时下限（秒）。`AiConfig::timeout_secs` 默认 30s，只适合一次性
/// 请求；长回答会被 `reqwest` 的总超时截断，因此流式请求至少给 600s。
pub const STREAM_TIMEOUT_FLOOR_SECS: u64 = 600;

/// 流式请求实际使用的总超时（`max(timeout_secs, STREAM_TIMEOUT_FLOOR_SECS)`）。
pub fn stream_timeout_secs(config: &AiConfig) -> u64 {
    config.timeout_secs.max(STREAM_TIMEOUT_FLOOR_SECS)
}

impl fmt::Debug for AiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AiClient {
    /// Build a client. Fails with [`AiError::NotConfigured`] when AI is off or
    /// no key is available.
    pub fn new(config: AiConfig) -> Result<Self, AiError> {
        if !config.is_configured() {
            return Err(AiError::NotConfigured);
        }
        let timeout_secs = config.timeout_secs.max(1);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(timeout_secs.min(10)))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|error| {
                AiError::Other(format!("failed to build the AI HTTP client: {error}"))
            })?;
        let stream_timeout = stream_timeout_secs(&config);
        let http_stream = reqwest::Client::builder()
            .timeout(Duration::from_secs(stream_timeout))
            .connect_timeout(Duration::from_secs(timeout_secs.min(10)))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|error| {
                AiError::Other(format!(
                    "failed to build the AI streaming HTTP client: {error}"
                ))
            })?;
        Ok(Self {
            config,
            http,
            http_stream,
        })
    }

    /// Borrow the configuration this client was built from.
    pub fn config(&self) -> &AiConfig {
        &self.config
    }

    /// Send one chat turn and return `choices[0].message.content` (trimmed).
    pub async fn chat(&self, system: &str, user: &str) -> Result<String, AiError> {
        let (key, authorization) = self.authorization()?;
        let body = self.chat_body(system, user, false)?;
        self.post_chat_body(&body, &key, authorization).await
    }

    /// Send a complete multi-turn context and return the next assistant reply
    /// (`choices[0].message.content`, trimmed).
    ///
    /// The `messages` array is serialized in caller order with its roles; no
    /// `tools`/function-calling payload is ever added (the resolver's JsonAction
    /// protocol lives in the message text, `docs/architecture.md` §11.9).
    /// HTTP, authorization, timeout, parsing and key scrubbing are shared with
    /// [`chat`](Self::chat).
    pub async fn chat_messages(&self, messages: &[ChatMessage]) -> Result<String, AiError> {
        let (key, authorization) = self.authorization()?;
        let body = self.chat_messages_body(messages)?;
        self.post_chat_body(&body, &key, authorization).await
    }

    /// POST one already-encoded chat body and parse the reply.
    async fn post_chat_body(
        &self,
        body: &[u8],
        key: &str,
        authorization: HeaderValue,
    ) -> Result<String, AiError> {
        let response = self
            .http
            .post(self.config.endpoint())
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, concat!("rdm-ai/", env!("CARGO_PKG_VERSION")))
            .body(body.to_vec())
            .send()
            .await
            .map_err(|error| AiError::Http(scrub(&describe_transport_error(&error), key)))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| AiError::Http(scrub(&describe_transport_error(&error), key)))?;
        let text = String::from_utf8_lossy(&bytes);

        if !status.is_success() {
            return Err(AiError::Status {
                status: status.as_u16(),
                body: truncate(&scrub(&text, key), 600),
            });
        }

        parse_chat_content(&text, key)
    }

    /// Send one chat turn and stream the reply.
    ///
    /// The request asks the provider for SSE (`"stream": true`). Every non-empty
    /// `choices[0].delta.content` fragment is passed to `on_delta` as soon as it
    /// is decoded, and the concatenation of all fragments is returned.
    ///
    /// Behaviour:
    /// * `Content-Type: text/event-stream` → SSE frames are parsed incrementally;
    ///   `data: [DONE]`, comments (`:`), keep-alive lines and events without
    ///   content are skipped. A provider that wrongly puts the text into
    ///   `choices[0].message.content` is tolerated.
    /// * any other content type → the reply is handled as a plain chat
    ///   completion: `on_delta` is called once with the whole text.
    /// * a failure after some text was already forwarded returns that partial
    ///   text (the UI keeps what it displayed) and only logs a key-free warning;
    ///   a failure before any text yields the underlying error.
    ///
    /// HTTP status errors and (non-streaming) JSON errors reuse the variants of
    /// [`chat`](Self::chat). The API key never appears in an error or log line.
    pub async fn chat_stream<F>(
        &self,
        system: &str,
        user: &str,
        mut on_delta: F,
    ) -> Result<String, AiError>
    where
        F: FnMut(&str),
    {
        let (key, authorization) = self.authorization()?;
        let body = self.chat_body(system, user, true)?;

        let response = self
            .http_stream
            .post(self.config.endpoint())
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, concat!("rdm-ai/", env!("CARGO_PKG_VERSION")))
            .body(body)
            .send()
            .await
            .map_err(|error| AiError::Http(scrub(&describe_transport_error(&error), &key)))?;

        let status = response.status();
        if !status.is_success() {
            let bytes = response
                .bytes()
                .await
                .map_err(|error| AiError::Http(scrub(&describe_transport_error(&error), &key)))?;
            let text = String::from_utf8_lossy(&bytes);
            return Err(AiError::Status {
                status: status.as_u16(),
                body: truncate(&scrub(&text, &key), 600),
            });
        }

        // Some "OpenAI-compatible" gateways ignore `stream` and answer with a
        // normal chat completion; only SSE replies are parsed incrementally.
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.contains("text/event-stream") {
            let bytes = response
                .bytes()
                .await
                .map_err(|error| AiError::Http(scrub(&describe_transport_error(&error), &key)))?;
            let text = String::from_utf8_lossy(&bytes);
            let content = parse_chat_content(&text, &key)?;
            on_delta(&content);
            return Ok(content);
        }

        let mut accumulated = String::new();
        let mut buffer: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        let mut done = false;

        while !done {
            let Some(chunk) = stream.next().await else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let failure = AiError::Http(scrub(&describe_transport_error(&error), &key));
                    return keep_partial(failure, accumulated);
                }
            };
            buffer.extend_from_slice(&chunk);
            // Network chunks do not respect SSE lines: only complete lines are
            // consumed, the tail stays buffered for the next chunk.
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = buffer.drain(..=newline).collect();
                line.pop(); // the `\n` itself
                let line = String::from_utf8_lossy(&line);
                match classify_sse_line(&line) {
                    SseLine::Ignore => {}
                    SseLine::Done => {
                        done = true;
                        break;
                    }
                    SseLine::Payload(payload) => match decode_sse_payload(payload, &key) {
                        Ok(Some(delta)) => {
                            accumulated.push_str(&delta);
                            on_delta(&delta);
                        }
                        Ok(None) => {}
                        Err(failure) => return keep_partial(failure, accumulated),
                    },
                }
            }
        }

        // A last line without a trailing newline is still a valid SSE line.
        if !done && !buffer.is_empty() {
            let line = String::from_utf8_lossy(&buffer);
            match classify_sse_line(&line) {
                SseLine::Ignore => {}
                SseLine::Done => {}
                SseLine::Payload(payload) => match decode_sse_payload(payload, &key) {
                    Ok(Some(delta)) => {
                        accumulated.push_str(&delta);
                        on_delta(&delta);
                    }
                    Ok(None) => {}
                    Err(failure) => return keep_partial(failure, accumulated),
                },
            }
        }

        if accumulated.is_empty() {
            return Err(AiError::Empty);
        }
        Ok(accumulated)
    }

    /// Resolve the API key and build its `Authorization` header value.
    ///
    /// `HeaderValue::from_str` errors must be reported without echoing the
    /// value, hence the tuple and the frozen message.
    fn authorization(&self) -> Result<(String, HeaderValue), AiError> {
        let key = self.config.resolved_api_key();
        if key.is_empty() {
            return Err(AiError::NotConfigured);
        }
        let authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| AiError::Other("API key is not a valid HTTP header value".into()))?;
        Ok((key, authorization))
    }

    /// Encode the chat request; `stream` adds `"stream": true`.
    fn chat_body(&self, system: &str, user: &str, stream: bool) -> Result<Vec<u8>, AiError> {
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.2,
        });
        if stream {
            body["stream"] = Value::Bool(true);
        }
        serde_json::to_vec(&body)
            .map_err(|error| AiError::Json(format!("cannot encode the AI request: {error}")))
    }

    /// Encode a multi-turn chat request, preserving the caller's message order.
    fn chat_messages_body(&self, messages: &[ChatMessage]) -> Result<Vec<u8>, AiError> {
        let messages: Vec<Value> = messages
            .iter()
            .map(|message| {
                json!({
                    "role": message.role.as_str(),
                    "content": message.content,
                })
            })
            .collect();
        let body = json!({
            "model": self.config.model,
            "messages": messages,
            "temperature": 0.2,
        });
        serde_json::to_vec(&body)
            .map_err(|error| AiError::Json(format!("cannot encode the AI request: {error}")))
    }
}

/// Parse a complete non-streaming chat completion body and return the trimmed
/// `choices[0].message.content`.
fn parse_chat_content(text: &str, key: &str) -> Result<String, AiError> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        AiError::Json(truncate(
            &format!("{error} (body: {})", scrub(text, key)),
            400,
        ))
    })?;
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if content.is_empty() {
        return Err(AiError::Empty);
    }
    Ok(content.to_owned())
}

/// One decoded SSE line.
enum SseLine<'a> {
    /// Empty line, comment (`:`), `event:`/`id:`/`retry:` or another field.
    Ignore,
    /// A `data:` payload that has to be parsed as JSON.
    Payload(&'a str),
    /// The `data: [DONE]` sentinel that ends the stream.
    Done,
}

/// Classify one SSE line, tolerating a trailing `\r` (CRLF streams).
fn classify_sse_line(line: &str) -> SseLine<'_> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() || line.starts_with(':') {
        return SseLine::Ignore;
    }
    let Some(rest) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let payload = rest.strip_prefix(' ').unwrap_or(rest);
    if payload.trim() == "[DONE]" {
        SseLine::Done
    } else {
        SseLine::Payload(payload)
    }
}

/// Extract one content delta from a `data:` payload.
///
/// The standard streaming shape is `choices[0].delta.content`; some compatible
/// gateways send the non-streaming `choices[0].message.content` instead, so the
/// latter is accepted as a fallback. `Ok(None)` means "no text in this event"
/// (for example a role-only first delta or a usage-only trailer).
fn decode_sse_payload(payload: &str, key: &str) -> Result<Option<String>, AiError> {
    let value: Value = serde_json::from_str(payload).map_err(|error| {
        AiError::Json(truncate(
            &format!("{error} (body: {})", scrub(payload, key)),
            400,
        ))
    })?;
    let content = value
        .pointer("/choices/0/delta/content")
        .or_else(|| value.pointer("/choices/0/message/content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if content.is_empty() {
        Ok(None)
    } else {
        Ok(Some(content.to_owned()))
    }
}

/// A mid-stream failure must not discard text the caller already displayed: log
/// a warning (the error text has the API key scrubbed) and return the partial
/// text. Without any text the underlying error is still reported.
fn keep_partial(failure: AiError, accumulated: String) -> Result<String, AiError> {
    if accumulated.is_empty() {
        return Err(failure);
    }
    tracing::warn!(error = %failure, "AI stream ended early; returning the partial reply");
    Ok(accumulated)
}

/// Replace the API key (if present) so it can never surface in an error.
fn scrub(text: &str, key: &str) -> String {
    if key.is_empty() {
        text.to_owned()
    } else {
        text.replace(key, "<redacted>")
    }
}

/// Human-readable transport failure (reqwest's `Display` already names the URL,
/// which is safe: the key travels in a header only).
fn describe_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        error.to_string()
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    crate::truncate_chars(value, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_configured_when_disabled_or_keyless() {
        let _guard = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var(crate::config::ENV_API_KEY);
        assert!(matches!(
            AiClient::new(AiConfig::default()),
            Err(AiError::NotConfigured)
        ));
        let keyless = AiConfig {
            enabled: true,
            ..AiConfig::default()
        };
        assert!(matches!(
            AiClient::new(keyless),
            Err(AiError::NotConfigured)
        ));
    }

    #[test]
    fn debug_output_does_not_leak_the_key() {
        let config = AiConfig {
            enabled: true,
            api_key: "debug-secret".into(),
            ..AiConfig::default()
        };
        let client = AiClient::new(config).unwrap();
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("debug-secret"), "{rendered}");
    }

    #[test]
    fn scrub_removes_key() {
        assert_eq!(
            scrub("bad key abc123 here", "abc123"),
            "bad key <redacted> here"
        );
        assert_eq!(scrub("nothing", ""), "nothing");
    }

    #[test]
    fn internal_timeout_is_capped() {
        let mut config = AiConfig {
            enabled: true,
            api_key: "key".into(),
            timeout_secs: 99,
            ..AiConfig::default()
        };
        assert_eq!(
            bounded_config(&config).timeout_secs,
            INTERNAL_TIMEOUT_CAP_SECS
        );
        config.timeout_secs = 0;
        assert_eq!(bounded_config(&config).timeout_secs, 1);
        config.timeout_secs = 7;
        assert_eq!(bounded_config(&config).timeout_secs, 7);
    }

    #[test]
    fn streaming_timeout_is_relaxed_for_long_replies() {
        let mut config = AiConfig {
            timeout_secs: 30,
            ..AiConfig::default()
        };
        assert_eq!(stream_timeout_secs(&config), STREAM_TIMEOUT_FLOOR_SECS);
        config.timeout_secs = 1;
        assert_eq!(stream_timeout_secs(&config), STREAM_TIMEOUT_FLOOR_SECS);
        config.timeout_secs = STREAM_TIMEOUT_FLOOR_SECS + 60;
        assert_eq!(stream_timeout_secs(&config), STREAM_TIMEOUT_FLOOR_SECS + 60);
    }

    #[test]
    fn classify_sse_line_ignores_non_data_fields() {
        assert!(matches!(classify_sse_line(""), SseLine::Ignore));
        assert!(matches!(classify_sse_line("\r"), SseLine::Ignore));
        assert!(matches!(classify_sse_line(": keep-alive"), SseLine::Ignore));
        assert!(matches!(
            classify_sse_line("event: message"),
            SseLine::Ignore
        ));
        assert!(matches!(classify_sse_line("retry: 1000"), SseLine::Ignore));
        match classify_sse_line("data: {\"a\":1}\r") {
            SseLine::Payload(payload) => assert_eq!(payload, "{\"a\":1}"),
            _ => panic!("expected a data payload"),
        }
        match classify_sse_line("data:{\"a\":1}") {
            SseLine::Payload(payload) => assert_eq!(payload, "{\"a\":1}"),
            _ => panic!("expected a data payload"),
        }
        assert!(matches!(classify_sse_line("data: [DONE]"), SseLine::Done));
        assert!(matches!(classify_sse_line("data:[DONE]\r"), SseLine::Done));
    }

    #[test]
    fn decode_sse_payload_accepts_delta_and_message_shapes() {
        let delta =
            decode_sse_payload(r#"{"choices":[{"delta":{"content":"hi"}}]}"#, "key").unwrap();
        assert_eq!(delta.as_deref(), Some("hi"));
        let message =
            decode_sse_payload(r#"{"choices":[{"message":{"content":"there"}}]}"#, "key").unwrap();
        assert_eq!(message.as_deref(), Some("there"));
        let role_only =
            decode_sse_payload(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#, "key").unwrap();
        assert_eq!(role_only, None);
        let null_content =
            decode_sse_payload(r#"{"choices":[{"delta":{"content":null}}]}"#, "key").unwrap();
        assert_eq!(null_content, None);
        // Parse failures carry the payload but never the key.
        let error = decode_sse_payload("{secret-key", "secret-key").unwrap_err();
        let rendered = format!("{error:?}") + &error.to_string();
        assert!(!rendered.contains("secret-key"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn chat_body_only_adds_stream_when_requested() {
        let config = AiConfig {
            enabled: true,
            api_key: "key".into(),
            model: "m".into(),
            ..AiConfig::default()
        };
        let client = AiClient::new(config).unwrap();
        let plain: Value = serde_json::from_slice(&client.chat_body("s", "u", false).unwrap())
            .expect("plain body");
        assert!(plain.get("stream").is_none());
        let streaming: Value = serde_json::from_slice(&client.chat_body("s", "u", true).unwrap())
            .expect("stream body");
        assert_eq!(streaming["stream"], Value::Bool(true));
        assert_eq!(streaming["model"], "m");
        assert_eq!(streaming["temperature"], 0.2);
    }

    #[test]
    fn chat_role_wire_names_are_snake_case() {
        for (role, wire) in [
            (ChatRole::System, "system"),
            (ChatRole::User, "user"),
            (ChatRole::Assistant, "assistant"),
        ] {
            assert_eq!(role.as_str(), wire);
            assert_eq!(serde_json::to_value(role).unwrap(), serde_json::json!(wire));
            let parsed: ChatRole = serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn chat_messages_body_preserves_order_and_roles() {
        let config = AiConfig {
            enabled: true,
            api_key: "key".into(),
            model: "m".into(),
            ..AiConfig::default()
        };
        let client = AiClient::new(config).unwrap();
        let body: Value = serde_json::from_slice(
            &client
                .chat_messages_body(&[
                    ChatMessage::system("sys"),
                    ChatMessage::user("first"),
                    ChatMessage::assistant("prior"),
                    ChatMessage::user("second"),
                ])
                .unwrap(),
        )
        .expect("multi-turn body");

        assert_eq!(body["model"], "m");
        assert_eq!(body["temperature"], 0.2);
        assert!(body.get("tools").is_none(), "no native tools payload");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "sys");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "first");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "prior");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "second");
    }

    /// Loopback OpenAI-compatible mock: replies with a fixed JSON body and
    /// records every decoded request body. No external network is used.
    async fn spawn_completion_server(
        status: u16,
        reply: &'static str,
        captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock completion server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let captured = captured.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let header_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|position| position + 4)
                        .unwrap_or(request.len());
                    let headers =
                        String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut body = request[header_end..].to_vec();
                    while body.len() < content_length {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => body.extend_from_slice(&chunk[..read]),
                        }
                    }
                    body.truncate(content_length);
                    let body = String::from_utf8_lossy(&body).into_owned();
                    captured
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(body);

                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        base_url
    }

    fn mock_client(base_url: &str, api_key: &str) -> AiClient {
        AiClient::new(AiConfig {
            enabled: true,
            base_url: format!("{base_url}/v1"),
            model: "mock-model".into(),
            api_key: api_key.into(),
            timeout_secs: 5,
        })
        .expect("mock client")
    }

    #[tokio::test]
    async fn chat_messages_posts_caller_order_and_parses_the_reply() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_url = spawn_completion_server(
            200,
            r#"{"choices":[{"message":{"content":"  {\"action\":\"finish\"}  "}}]}"#,
            captured.clone(),
        )
        .await;
        let client = mock_client(&base_url, "k");

        let reply = client
            .chat_messages(&[
                ChatMessage::system("sys"),
                ChatMessage::user("first"),
                ChatMessage::assistant("prior"),
                ChatMessage::user("second"),
            ])
            .await
            .expect("multi-turn reply");

        assert_eq!(reply, r#"{"action":"finish"}"#);
        let bodies = captured
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(bodies.len(), 1);
        let body: Value = serde_json::from_str(&bodies[0]).expect("captured body is JSON");
        assert_eq!(body["model"], "mock-model");
        assert!(body.get("tools").is_none());
        let messages = body["messages"].as_array().expect("messages array");
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
        assert_eq!(messages[1]["content"], "first");
        assert_eq!(messages[3]["content"], "second");
    }

    #[tokio::test]
    async fn chat_messages_errors_never_contain_the_api_key() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_url = spawn_completion_server(
            401,
            r#"{"error":"the api key secret-key is invalid"}"#,
            captured,
        )
        .await;
        let client = mock_client(&base_url, "secret-key");

        let error = client
            .chat_messages(&[ChatMessage::user("hi")])
            .await
            .expect_err("401 must fail");
        assert!(matches!(error, AiError::Status { status: 401, .. }));
        let rendered = format!("{error:?}") + &error.to_string();
        assert!(!rendered.contains("secret-key"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
