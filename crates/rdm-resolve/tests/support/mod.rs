//! Minimal loopback HTTP/1.1 server for `rdm-resolve` tests.
//!
//! Deliberately dependency-free (tokio only) and fully deterministic: every
//! test declares its own routes, so no public network is ever used.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One canned response.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    /// Overrides `Content-Length` (needed for HEAD responses describing a file).
    pub content_length: Option<u64>,
}

impl Reply {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    pub fn file(len: usize) -> Self {
        Self {
            status: 200,
            content_type: Some("application/octet-stream".to_owned()),
            body: vec![b'a'; len],
            ..Self::default()
        }
    }

    /// `HEAD` reply for a file: headers only, explicit length.
    pub fn file_head(len: u64) -> Self {
        Self {
            status: 200,
            content_type: Some("application/octet-stream".to_owned()),
            content_length: Some(len),
            ..Self::default()
        }
    }

    pub fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: Some("text/html; charset=utf-8".to_owned()),
            body: body.as_bytes().to_vec(),
            ..Self::default()
        }
    }

    pub fn json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: Some("application/json".to_owned()),
            body: body.as_bytes().to_vec(),
            ..Self::default()
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".to_owned(), location.to_owned())],
            ..Self::default()
        }
    }

    /// `206` reply for `Range: bytes=0-<len-1>` over a `total`-byte resource.
    pub fn partial(total: u64, len: usize) -> Self {
        Self {
            status: 206,
            content_type: Some("application/octet-stream".to_owned()),
            body: vec![b'a'; len],
            headers: vec![(
                "Content-Range".to_owned(),
                format!("bytes 0-{}/{total}", len.saturating_sub(1)),
            )],
            ..Self::default()
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// One captured request.
#[derive(Debug, Clone, Default)]
pub struct RequestInfo {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl RequestInfo {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

type Handler = dyn Fn(&RequestInfo) -> Reply + Send + Sync + 'static;

/// Server bound to an ephemeral loopback port.
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
        let address = listener.local_addr().unwrap();
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
                    let request = match read_request(&mut stream).await {
                        Some(request) => request,
                        None => return,
                    };
                    requests
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(request.clone());
                    let reply = handler(&request);
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
        self.requests().len()
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
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buffer).into_owned();
    let (request_line, rest) = head.split_once("\r\n")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let headers = rest
        .lines()
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Some(RequestInfo {
        method,
        path,
        headers,
    })
}

async fn write_reply(
    stream: &mut tokio::net::TcpStream,
    method: &str,
    reply: &Reply,
) -> std::io::Result<()> {
    let reason = match reply.status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nConnection: close\r\n",
        reply.status
    );
    if let Some(content_type) = &reply.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let length = reply.content_length.unwrap_or(reply.body.len() as u64);
    head.push_str(&format!("Content-Length: {length}\r\n\r\n"));
    stream.write_all(head.as_bytes()).await?;
    if method != "HEAD" && !reply.body.is_empty() {
        stream.write_all(&reply.body).await?;
    }
    stream.flush().await
}
