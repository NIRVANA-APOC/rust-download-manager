//! Tiny self-contained mock HTTP server used by the `rdm-ai` integration tests.
//!
//! Only `tokio::net::TcpListener` is used — no external test server, no public
//! network and no real API key.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/// One canned HTTP response.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
    pub headers: Vec<(String, String)>,
    pub delay_ms: u64,
    /// Overrides `Content-Length` (useful for HEAD responses describing a file).
    pub content_length: Option<u64>,
    /// When non-empty the reply is sent with `Transfer-Encoding: chunked`, one
    /// HTTP chunk per entry (used to exercise streaming SSE replies).
    pub chunks: Vec<String>,
    /// Sleep between two streamed chunks, so the client observes separate reads.
    pub chunk_delay_ms: u64,
    /// Drop the connection after the chunks without the terminating zero chunk
    /// (a truncated chunked body: simulates a provider dying mid-stream).
    pub abort_stream: bool,
}

impl Default for Reply {
    fn default() -> Self {
        Self {
            status: 200,
            content_type: None,
            body: String::new(),
            headers: Vec::new(),
            delay_ms: 0,
            content_length: None,
            chunks: Vec::new(),
            chunk_delay_ms: 0,
            abort_stream: false,
        }
    }
}

impl Reply {
    pub fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: Some("text/html; charset=utf-8".to_owned()),
            body: body.to_owned(),
            ..Self::default()
        }
    }

    pub fn json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: Some("application/json".to_owned()),
            body: body.to_owned(),
            ..Self::default()
        }
    }

    /// OpenAI-compatible chat completion carrying `content`.
    pub fn chat(content: &str) -> Self {
        let body = serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
        });
        Self::json(&body.to_string())
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.to_owned(),
            ..Self::default()
        }
    }

    pub fn body(mut self, body: &str) -> Self {
        self.body = body.to_owned();
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn content_length(mut self, length: u64) -> Self {
        self.content_length = Some(length);
        self
    }

    pub fn delay(mut self, milliseconds: u64) -> Self {
        self.delay_ms = milliseconds;
        self
    }

    /// A `text/event-stream` reply. Every entry becomes one HTTP chunk, so with
    /// [`Reply::chunk_delay`] the client receives the events as separate reads.
    pub fn sse(chunks: Vec<String>) -> Self {
        Self {
            content_type: Some("text/event-stream".to_owned()),
            chunks,
            ..Self::default()
        }
    }

    /// Sleep `milliseconds` between two streamed chunks.
    pub fn chunk_delay(mut self, milliseconds: u64) -> Self {
        self.chunk_delay_ms = milliseconds;
        self
    }

    /// Drop the connection after the chunks, leaving the chunked body truncated
    /// (no terminating `0\r\n\r\n`): a provider that dies mid-stream.
    pub fn abort_stream(mut self) -> Self {
        self.abort_stream = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// One captured request head (+ body when present).
#[derive(Debug, Clone, Default)]
pub struct RequestInfo {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub raw: String,
}

impl RequestInfo {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

type Handler = dyn Fn(&RequestInfo) -> Reply + Send + Sync + 'static;

/// A loopback HTTP server bound to an ephemeral port.
pub struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<RequestInfo>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    pub async fn start<F>(handler: F) -> Self
    where
        F: Fn(&RequestInfo) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let requests: Arc<Mutex<Vec<RequestInfo>>> = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let task_requests = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                let requests = task_requests.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    requests
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(request.clone());
                    let reply = handler(&request);
                    if reply.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(reply.delay_ms)).await;
                    }
                    let _ = write_reply(&mut stream, &request.method, &reply).await;
                });
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            task,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub fn requests(&self) -> Vec<RequestInfo> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn count_method(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == method)
            .count()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<RequestInfo> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if header_end(&buffer).is_some() {
            break;
        }
        if buffer.len() > 1024 * 1024 {
            return None;
        }
    }
    let end = header_end(&buffer)?;
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let content_length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_end = (body_start + content_length).min(buffer.len());
    let body = String::from_utf8_lossy(&buffer[body_start..body_end]).into_owned();

    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();

    Some(RequestInfo {
        raw: format!("{head}\r\n\r\n{body}"),
        method,
        path,
        headers,
        body,
    })
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_reply(
    stream: &mut tokio::net::TcpStream,
    method: &str,
    reply: &Reply,
) -> std::io::Result<()> {
    let reason = match reply.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
    head.push_str("Connection: close\r\n");
    if let Some(content_type) = &reply.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }

    if reply.chunks.is_empty() {
        let body = reply.body.as_bytes();
        let length = reply.content_length.unwrap_or(body.len() as u64);
        head.push_str(&format!("Content-Length: {length}\r\n\r\n"));
        stream.write_all(head.as_bytes()).await?;
        if method != "HEAD" && !body.is_empty() {
            stream.write_all(body).await?;
        }
        stream.flush().await?;
        return Ok(());
    }

    // Streaming reply: every entry is one HTTP chunk, flushed before the delay
    // so the client observes it as its own read (not a buffered whole body).
    head.push_str("Transfer-Encoding: chunked\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    for chunk in &reply.chunks {
        if chunk.is_empty() {
            continue;
        }
        let framed = format!("{:x}\r\n{chunk}\r\n", chunk.len());
        stream.write_all(framed.as_bytes()).await?;
        stream.flush().await?;
        if reply.chunk_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(reply.chunk_delay_ms)).await;
        }
    }
    if reply.abort_stream {
        if reply.chunk_delay_ms == 0 {
            // Give the client time to read the delivered chunks before the FIN.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Returning without the terminating zero chunk truncates the body.
        return Ok(());
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await?;
    Ok(())
}

/// Build one `data: <payload>` SSE event (CRLF-terminated).
pub fn sse_event(payload: &str) -> String {
    format!("data: {payload}\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Helpers shared by the tests
// ---------------------------------------------------------------------------

/// Configuration pointing at `server`, with a non-empty fake API key.
pub fn ai_config(server: &MockServer) -> rdm_ai::AiConfig {
    rdm_ai::AiConfig {
        enabled: true,
        base_url: server.url("/v1"),
        model: "mock-model".to_owned(),
        api_key: "test-key-12345".to_owned(),
        timeout_secs: 5,
    }
}

/// An AI-disabled config (no key).
pub fn local_config() -> rdm_ai::AiConfig {
    rdm_ai::AiConfig::default()
}

/// Self-cleaning temporary directory.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let safe_tag: String = tag
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect();
        let path = std::env::temp_dir().join(format!(
            "rdm-ai-it-{}-{safe_tag}-{nanos}-{counter}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
