//! Scripted OpenAI-compatible mock LLM server for the exploration tests.
//!
//! Dependency-free (tokio only) and deterministic: a fresh ephemeral loopback
//! port per test and replies served from an explicit script — no public network
//! is ever used.
//!
//! The exploration loop talks to `{base_url}/chat/completions` with the OpenAI
//! chat-completions shape (`{"model","messages":[…],"temperature":0.2}`). This
//! server records every request (headers **and** JSON body, so tests can assert
//! message order, `untrusted_content` wrapping and key hygiene separately) and
//! answers with the next scripted assistant `content`.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One captured `POST /chat/completions` request.
#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    /// Raw request body (UTF-8 JSON on the wire for chat completions).
    pub body: String,
}

impl LlmRequest {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Bearer token from the `Authorization` header, without the `Bearer ` prefix.
    pub fn bearer_token(&self) -> Option<&str> {
        self.header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
    }

    /// Parse the JSON request body (panics with the captured body on malformed).
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|error| {
            panic!("mock LLM request body is not JSON: {error}\n{}", self.body)
        })
    }

    /// `messages` array of the request body (cloned values).
    pub fn messages(&self) -> Vec<serde_json::Value> {
        self.json()
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_else(|| panic!("mock LLM request has no messages array: {}", self.body))
    }

    /// Message roles, in wire order.
    pub fn roles(&self) -> Vec<String> {
        self.messages()
            .iter()
            .map(|message| {
                message
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            })
            .collect()
    }

    /// Message contents, in wire order.
    pub fn contents(&self) -> Vec<String> {
        self.messages()
            .iter()
            .map(|message| {
                message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            })
            .collect()
    }

    /// Whether the raw request body contains `needle`.
    pub fn body_contains(&self, needle: &str) -> bool {
        self.body.contains(needle)
    }

    /// Whether any header value contains `needle`.
    pub fn headers_contain(&self, needle: &str) -> bool {
        self.headers.iter().any(|(_, value)| value.contains(needle))
    }

    /// Whether the API key leaks anywhere except the `Authorization` header.
    ///
    /// Returns the list of leaking locations (`"body"`, `"header:<name>"`).
    pub fn key_leaks(&self, key: &str) -> Vec<String> {
        let mut leaks = Vec::new();
        if self.body.contains(key) {
            leaks.push("body".to_owned());
        }
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("authorization") {
                continue;
            }
            if value.contains(key) {
                leaks.push(format!("header:{name}"));
            }
        }
        leaks
    }
}

/// One scripted assistant reply.
pub enum LlmReply {
    /// Return this assistant `content` verbatim.
    Content(String),
    /// Compute the assistant `content` from the request (e.g. to echo an
    /// observation id the model could only learn from the previous tool result).
    Dynamic(Arc<dyn Fn(&LlmRequest) -> String + Send + Sync>),
    /// Answer with this HTTP status and an OpenAI-style error envelope.
    Status(u16),
    /// Sleep, then return this assistant `content` (slow-provider simulation).
    Delayed { millis: u64, content: String },
}

impl LlmReply {
    pub fn content(content: impl Into<String>) -> Self {
        Self::Content(content.into())
    }

    pub fn dynamic(build: impl Fn(&LlmRequest) -> String + Send + Sync + 'static) -> Self {
        Self::Dynamic(Arc::new(build))
    }

    pub fn status(status: u16) -> Self {
        Self::Status(status)
    }

    pub fn delayed(millis: u64, content: impl Into<String>) -> Self {
        Self::Delayed {
            millis,
            content: content.into(),
        }
    }
}

impl fmt::Debug for LlmReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Content(content) => f.debug_tuple("Content").field(content).finish(),
            Self::Dynamic(_) => f.write_str("Dynamic(..)"),
            Self::Status(status) => f.debug_tuple("Status").field(status).finish(),
            Self::Delayed { millis, content } => f
                .debug_struct("Delayed")
                .field("millis", millis)
                .field("content", content)
                .finish(),
        }
    }
}

/// Scripted chat-completions server bound to an ephemeral loopback port.
pub struct MockLlm {
    base_url: String,
    requests: Arc<Mutex<Vec<LlmRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockLlm {
    /// Start a server that serves `replies` in order.
    ///
    /// A request beyond the script is answered with HTTP 500 and an explicit
    /// `"no scripted reply left"` error body: tests that overrun their script
    /// fail loudly instead of hanging.
    pub async fn start(replies: Vec<LlmReply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock LLM server");
        let address = listener.local_addr().expect("mock LLM local addr");
        let replies: Arc<Mutex<VecDeque<LlmReply>>> = Arc::new(Mutex::new(replies.into()));
        let requests: Arc<Mutex<Vec<LlmRequest>>> = Arc::new(Mutex::new(Vec::new()));

        let task_requests = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let replies = replies.clone();
                let requests = task_requests.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    requests
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(request.clone());
                    let reply = replies
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .pop_front();
                    let result =
                        match reply {
                            Some(LlmReply::Content(content)) => {
                                write_json(&mut stream, 200, &chat_completion(&content)).await
                            }
                            Some(LlmReply::Dynamic(build)) => {
                                let content = build(&request);
                                write_json(&mut stream, 200, &chat_completion(&content)).await
                            }
                            Some(LlmReply::Status(status)) => {
                                write_json(
                                    &mut stream,
                                    status,
                                    &error_envelope(&format!("scripted HTTP {status}")),
                                )
                                .await
                            }
                            Some(LlmReply::Delayed { millis, content }) => {
                                tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
                                write_json(&mut stream, 200, &chat_completion(&content)).await
                            }
                            None => write_json(
                                &mut stream,
                                500,
                                &error_envelope(
                                    "mock LLM has no scripted reply left (unexpected extra call)",
                                ),
                            )
                            .await,
                        };
                    let _ = result;
                });
            }
        });

        Self {
            base_url: format!("http://{address}"),
            requests,
            task,
        }
    }

    /// Origin of the mock server (e.g. `http://127.0.0.1:PORT`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Base URL to put into `AiConfig::base_url`: `{origin}/v1`, so the client's
    /// endpoint is `{origin}/v1/chat/completions`.
    pub fn config_base_url(&self) -> String {
        format!("{}/v1", self.base_url)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every captured request, in arrival order.
    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests().len()
    }

    /// Request at `index` (panics with a helpful message when missing).
    pub fn request(&self, index: usize) -> LlmRequest {
        let requests = self.requests();
        assert!(
            index < requests.len(),
            "expected at least {} LLM request(s), captured {}",
            index + 1,
            requests.len()
        );
        requests[index].clone()
    }

    pub fn last_request(&self) -> LlmRequest {
        let requests = self.requests();
        assert!(
            !requests.is_empty(),
            "expected at least one LLM request, captured none"
        );
        requests[requests.len() - 1].clone()
    }

    /// Raw bodies of all captured requests.
    pub fn bodies(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.body)
            .collect()
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn chat_completion(content: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-scripted",
        "object": "chat.completion",
        "model": "scripted-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

fn error_envelope(message: &str) -> String {
    serde_json::json!({
        "error": {
            "message": message,
            "type": "mock_llm_error",
            "code": message
        }
    })
    .to_string()
}

async fn write_json(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Read one HTTP/1.1 request (headers + `Content-Length` body).
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<LlmRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 8 * 1024 * 1024 {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Some(LlmRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
