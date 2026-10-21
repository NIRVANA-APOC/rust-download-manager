//! Local HTTP/1.1 fixture server used by `rdm-core` integration tests and the
//! benchmark. Hand-written on top of tokio (no axum/hyper) so the test surface
//! stays small and fully controllable.
//!
//! The server exposes:
//! * `GET|HEAD /file`      — the configured payload (Range aware).
//! * `GET|HEAD /redirect`  — `302` to `/file`.
//!
//! Everything a download engine needs to be exercised is configurable:
//! Range support (or deliberately ignoring `Range`), missing `Content-Length`,
//! per-N-bytes artificial latency, validators (`ETag` / `Last-Modified`),
//! `Content-Disposition`, request-abort injection, `429`/`503` + `Retry-After`
//! injection. Request/byte counters let tests assert "only missing chunks were
//! re-downloaded".

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, RwLock};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};

/// Size of the deterministic pseudo-random content block used to synthesise
/// payload bytes. Content repeats this block, which keeps generation cheap.
pub const CONTENT_BLOCK_SIZE: usize = 64 * 1024;

/// Default granule size for artificial latency.
pub const DEFAULT_DELAY_EVERY_BYTES: u64 = 256 * 1024;

/// Idle keep-alive timeout for a connection (also bounds shutdown drain).
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum accepted size of a request head.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Injection: reject the first `first_requests` matched requests with
/// `status` and a `Retry-After: retry_after_secs` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitInjection {
    pub first_requests: u64,
    pub retry_after_secs: u64,
    pub status: u16,
}

/// Server behaviour. Construct with [`Config::new`] and adjust fields (or use
/// the chainable helpers) before calling [`TestServer::start`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Payload size in bytes.
    pub size: u64,
    /// Seed for the deterministic content generator.
    pub seed: u64,
    /// When `false` the server replies `200` with the whole payload even if
    /// the request carried a `Range` header (mirrors CDNs that strip ranges).
    pub supports_ranges: bool,
    /// When `true` no `Content-Length` is sent and bodies use chunked encoding.
    pub no_content_length: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
    pub content_type: String,
    /// Latency injected before every `delay_every_bytes` granule of body.
    pub delay_ms: u64,
    /// Granule size for [`Config::delay_ms`]; `0` with `delay_ms > 0` means one
    /// delay per response.
    pub delay_every_bytes: u64,
    /// Close the connection without a response on these request ordinals
    /// (1-based, counted across every request the server receives).
    pub abort_on_requests: Vec<u64>,
    /// Reject some early GET requests with `429`/`503` + `Retry-After`.
    pub rate_limit: Option<RateLimitInjection>,
    /// Also apply the rate-limit injection to `HEAD` requests (default false,
    /// so probes succeed and only the transfer retries).
    pub retry_apply_to_head: bool,
    /// Omit the `Accept-Ranges` header entirely (forces engines to probe range
    /// support with a ranged `GET`).
    pub omit_accept_ranges: bool,
    /// Path serving the payload.
    pub file_path: String,
    /// Path issuing the redirect.
    pub redirect_path: String,
    /// When set, every `GET`/`HEAD` request must carry a `Cookie` header whose
    /// value contains this exact substring (e.g. `"session=abc"`); otherwise the
    /// server replies `401` with a `text/plain` body `"cookie required"`.
    /// `None` (the default) keeps the previous behaviour.
    pub required_cookie: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            size: 1024 * 1024,
            seed: 0x5EED_1234,
            supports_ranges: true,
            no_content_length: false,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_type: "application/octet-stream".to_string(),
            delay_ms: 0,
            delay_every_bytes: DEFAULT_DELAY_EVERY_BYTES,
            abort_on_requests: Vec::new(),
            rate_limit: None,
            retry_apply_to_head: false,
            omit_accept_ranges: false,
            file_path: "/file".to_string(),
            redirect_path: "/redirect".to_string(),
            required_cookie: None,
        }
    }
}

impl Config {
    pub fn new(size: u64) -> Self {
        Self {
            size,
            ..Self::default()
        }
    }

    pub fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    pub fn with_delay_every(mut self, bytes: u64) -> Self {
        self.delay_every_bytes = bytes;
        self
    }

    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = Some(etag.into());
        self
    }

    pub fn with_last_modified(mut self, value: impl Into<String>) -> Self {
        self.last_modified = Some(value.into());
        self
    }

    pub fn with_content_disposition(mut self, value: impl Into<String>) -> Self {
        self.content_disposition = Some(value.into());
        self
    }

    /// Ignore `Range` headers (always reply `200` with the full payload).
    pub fn without_ranges(mut self) -> Self {
        self.supports_ranges = false;
        self
    }

    /// Omit `Content-Length`, stream bodies with chunked encoding.
    pub fn without_length(mut self) -> Self {
        self.no_content_length = true;
        self
    }

    /// Abort the connection on the given 1-based request ordinal(s).
    pub fn with_abort_on(mut self, requests: impl IntoIterator<Item = u64>) -> Self {
        self.abort_on_requests = requests.into_iter().collect();
        self
    }

    /// Omit `Accept-Ranges` so clients must verify range support with a
    /// one-byte ranged GET.
    pub fn without_accept_ranges(mut self) -> Self {
        self.omit_accept_ranges = true;
        self
    }

    /// Reject the first `first_requests` GETs with `429` + `Retry-After`.
    pub fn with_retry_after(mut self, first_requests: u64, retry_after_secs: u64) -> Self {
        self.rate_limit = Some(RateLimitInjection {
            first_requests,
            retry_after_secs,
            status: 429,
        });
        self
    }

    /// Require a cookie: `GET`/`HEAD` requests whose `Cookie` header does not
    /// contain `value` get `401 text/plain` + body `"cookie required"` (P4a).
    pub fn with_required_cookie(mut self, value: impl Into<String>) -> Self {
        self.required_cookie = Some(value.into());
        self
    }
}

/// Snapshot of what the server observed. All counters are cumulative since
/// start (or since [`TestServer::reset_stats`]).
#[derive(Debug, Clone, Default)]
pub struct Stats {
    /// Every request head received (including aborted ones).
    pub requests: u64,
    pub head_requests: u64,
    pub get_requests: u64,
    pub responses_200: u64,
    pub responses_206: u64,
    pub responses_302: u64,
    pub responses_404: u64,
    pub responses_416: u64,
    pub responses_429: u64,
    /// Connections closed without sending a response (abort injection).
    pub aborted_connections: u64,
    /// Raw `Range` header values in arrival order.
    pub range_headers: Vec<String>,
    /// Request methods in arrival order (`HEAD`, `GET`, …). Lets tests assert
    /// an exact request sequence (e.g. "one rangeless GET, no probe").
    pub methods: Vec<String>,
    /// HTTP version token from each request line (always `HTTP/1.1` for this
    /// fixture; lets tests prove the engine used h1 for chunked transfers).
    pub http_versions: Vec<String>,
    /// Payload bytes actually written to sockets (headers excluded).
    pub body_bytes_sent: u64,
}

struct ServerState {
    config: RwLock<Config>,
    stats: Mutex<Stats>,
    /// Deterministic content block derived from the initial seed.
    block: Vec<u8>,
    /// Request ordinal counter.
    request_seq: AtomicU64,
    /// Abort ordinals already consumed.
    abort_used: Mutex<HashSet<u64>>,
    /// Rate-limit rejections already served.
    rate_limited_served: AtomicU64,
}

impl ServerState {
    fn new(config: Config) -> Self {
        let block = content_block(config.seed);
        Self {
            config: RwLock::new(config),
            stats: Mutex::new(Stats::default()),
            block,
            request_seq: AtomicU64::new(0),
            abort_used: Mutex::new(HashSet::new()),
            rate_limited_served: AtomicU64::new(0),
        }
    }

    fn stats(&self) -> Stats {
        self.stats.lock().expect("stats mutex poisoned").clone()
    }

    fn reset_stats(&self) {
        *self.stats.lock().expect("stats mutex poisoned") = Stats::default();
    }

    fn with_stats<R>(&self, f: impl FnOnce(&mut Stats) -> R) -> R {
        let mut guard = self.stats.lock().expect("stats mutex poisoned");
        f(&mut guard)
    }
}

/// A running fixture server. Dropping it stops the listener (best effort);
/// prefer awaiting [`TestServer::shutdown`] so the port is released before the
/// test continues.
pub struct TestServer {
    addr: SocketAddr,
    state: Arc<ServerState>,
    file_path: String,
    redirect_path: String,
    size: u64,
    seed: u64,
    shutdown: watch::Sender<bool>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl TestServer {
    /// Bind `127.0.0.1:0` and start serving. The returned handle owns the
    /// listener; call [`TestServer::shutdown`] to stop it deterministically.
    pub async fn start(config: Config) -> std::io::Result<Self> {
        Self::start_at(SocketAddr::from(([127, 0, 0, 1], 0)), config).await
    }

    /// Like [`TestServer::start`] but on an explicit loopback address.
    pub async fn start_at(addr: SocketAddr, config: Config) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let file_path = config.file_path.clone();
        let redirect_path = config.redirect_path.clone();
        let size = config.size;
        let seed = config.seed;
        let state = Arc::new(ServerState::new(config));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let accept_state = Arc::clone(&state);
        let join = tokio::spawn(async move {
            accept_loop(listener, accept_state, shutdown_rx).await;
        });
        Ok(Self {
            addr,
            state,
            file_path,
            redirect_path,
            size,
            seed,
            shutdown,
            join: Some(join),
        })
    }

    /// Bound address (loopback).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    /// URL of the payload endpoint.
    pub fn file_url(&self) -> String {
        self.url(&self.file_path)
    }

    /// URL of the redirect endpoint.
    pub fn redirect_url(&self) -> String {
        self.url(&self.redirect_path)
    }

    pub fn stats(&self) -> Stats {
        self.state.stats()
    }

    pub fn reset_stats(&self) {
        self.state.reset_stats();
    }

    /// Current config clone.
    pub async fn config(&self) -> Config {
        self.state.config.read().await.clone()
    }

    /// Mutate config while the server is running (e.g. flip the `ETag` between
    /// a paused run and its resume). Size/seed changes are not supported.
    pub async fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut guard = self.state.config.write().await;
        f(&mut guard);
    }

    pub async fn set_etag(&self, etag: Option<&str>) {
        self.update_config(|c| c.etag = etag.map(str::to_owned))
            .await;
    }

    /// Full payload bytes for assertions, generated with the same algorithm
    /// the server streams.
    pub fn content(&self) -> Vec<u8> {
        content_bytes(self.size, self.seed)
    }

    /// Configured payload size.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Stop accepting connections, close the listener and drain handlers.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(join) = self.join.take() {
            let _ = timeout(Duration::from_secs(3), join).await;
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Content generation
// ---------------------------------------------------------------------------

/// Deterministic 64 KiB content block derived from `seed`.
pub fn content_block(seed: u64) -> Vec<u8> {
    let mut block = Vec::with_capacity(CONTENT_BLOCK_SIZE);
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    if x == 0 {
        x = 0xDEAD_BEEF_CAFE_F00D;
    }
    while block.len() < CONTENT_BLOCK_SIZE {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        block.extend_from_slice(&x.to_le_bytes());
    }
    block.truncate(CONTENT_BLOCK_SIZE);
    block
}

/// Deterministic full payload for `size` bytes derived from `seed`.
pub fn content_bytes(size: u64, seed: u64) -> Vec<u8> {
    let block = content_block(seed);
    let mut out = vec![0u8; size as usize];
    fill_from_block(&block, 0, &mut out);
    out
}

fn fill_from_block(block: &[u8], offset: u64, out: &mut [u8]) {
    let blen = block.len();
    let mut done = 0usize;
    while done < out.len() {
        let start = ((offset % blen as u64) as usize + done) % blen;
        let n = (blen - start).min(out.len() - done);
        out[done..done + n].copy_from_slice(&block[start..start + n]);
        done += n;
    }
}

// ---------------------------------------------------------------------------
// HTTP/1.1 plumbing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    ordinal: u64,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn path(&self) -> &str {
        let target = if let Some(rest) = self
            .target
            .strip_prefix("http://")
            .or_else(|| self.target.strip_prefix("https://"))
        {
            match rest.find('/') {
                Some(idx) => &rest[idx..],
                None => "/",
            }
        } else {
            self.target.as_str()
        };
        match target.find('?') {
            Some(idx) => &target[..idx],
            None => target,
        }
    }

    fn wants_keep_alive(&self) -> bool {
        match self.header("connection") {
            Some(value) => !value.eq_ignore_ascii_case("close"),
            None => self.version != "HTTP/1.0",
        }
    }
}

async fn read_request(reader: &mut BufReader<OwnedReadHalf>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    if line.trim().is_empty() {
        // Tolerate a stray CRLF between requests.
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    if method.is_empty() || target.is_empty() {
        return Ok(None);
    }

    let mut headers = Vec::new();
    let mut total = line.len();
    loop {
        let mut header_line = String::new();
        let n = reader.read_line(&mut header_line).await?;
        if n == 0 {
            return Ok(None);
        }
        total += n;
        if total > MAX_HEAD_BYTES {
            return Ok(None);
        }
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    Ok(Some(Request {
        method,
        target,
        version,
        headers,
        ordinal: 0,
    }))
}

async fn write_head(
    writer: &mut OwnedWriteHalf,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes()).await
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<ServerState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((socket, _peer)) => {
                        let state = Arc::clone(&state);
                        let shutdown = shutdown.clone();
                        connections.spawn(async move {
                            let _ = handle_connection(socket, state, shutdown).await;
                        });
                    }
                    Err(_) => sleep(Duration::from_millis(10)).await,
                }
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                let _ = joined;
            }
        }
    }
    // Close the listening socket before waiting for handlers.
    drop(listener);
    let drain = async { while connections.join_next().await.is_some() {} };
    let _ = timeout(Duration::from_secs(2), drain).await;
}

async fn handle_connection(
    socket: TcpStream,
    state: Arc<ServerState>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let _ = socket.set_nodelay(true);
    let (read_half, mut write_half) = socket.into_split();
    let mut reader = BufReader::with_capacity(16 * 1024, read_half);

    loop {
        let request = tokio::select! {
            _ = shutdown.changed() => break,
            result = timeout(CONNECTION_IDLE_TIMEOUT, read_request(&mut reader)) => {
                match result {
                    Ok(Ok(Some(mut request))) => {
                        request.ordinal = state.request_seq.fetch_add(1, Ordering::SeqCst) + 1;
                        request
                    }
                    _ => break,
                }
            }
        };

        let keep_alive = request.wants_keep_alive();
        let served = tokio::select! {
            _ = shutdown.changed() => break,
            result = serve(&mut write_half, &state, &request) => result,
        };
        match served {
            Ok(true) if keep_alive => continue,
            _ => break,
        }
    }

    let _ = write_half.shutdown().await;
    Ok(())
}

/// Serve one request. Returns `Ok(true)` when the connection can be reused.
async fn serve(
    writer: &mut OwnedWriteHalf,
    state: &Arc<ServerState>,
    request: &Request,
) -> std::io::Result<bool> {
    state.with_stats(|s| {
        s.requests += 1;
        s.methods.push(request.method.clone());
        s.http_versions.push(request.version.clone());
        match request.method.as_str() {
            "HEAD" => s.head_requests += 1,
            "GET" => s.get_requests += 1,
            _ => {}
        }
    });

    let config = state.config.read().await.clone();

    // Connection-abort injection: close without a single byte of response.
    let consumed_abort = {
        let mut used = state.abort_used.lock().expect("abort mutex poisoned");
        config.abort_on_requests.contains(&request.ordinal) && used.insert(request.ordinal)
    };
    if consumed_abort {
        state.with_stats(|s| s.aborted_connections += 1);
        return Ok(false);
    }

    let path = request.path();
    let is_file = path == config.file_path;
    let is_redirect = path == config.redirect_path;

    if !is_file && !is_redirect {
        state.with_stats(|s| s.responses_404 += 1);
        return write_404(writer).await;
    }

    if request.method != "GET" && request.method != "HEAD" {
        return write_status(writer, 405, "Method Not Allowed", &[], 0).await;
    }

    // Required-cookie mode (P4a): a GET/HEAD without the configured cookie
    // substring is rejected before any payload/redirect handling.
    if let Some(required) = &config.required_cookie {
        let present = request
            .header("cookie")
            .is_some_and(|value| value.contains(required.as_str()));
        if !present {
            return write_cookie_required(writer, request.method == "HEAD").await;
        }
    }

    if is_redirect {
        state.with_stats(|s| s.responses_302 += 1);
        let location = config.file_path.clone();
        write_head(
            writer,
            302,
            "Found",
            &[
                ("Location", location),
                ("Content-Length", "0".to_string()),
                ("Connection", "keep-alive".to_string()),
            ],
        )
        .await?;
        return Ok(true);
    }

    serve_file(writer, state, request, &config).await
}

async fn serve_file(
    writer: &mut OwnedWriteHalf,
    state: &Arc<ServerState>,
    request: &Request,
    config: &Config,
) -> std::io::Result<bool> {
    // 429/503 injection.
    if let Some(injection) = config.rate_limit {
        let applies = request.method == "GET" || config.retry_apply_to_head;
        if applies {
            let served = state.rate_limited_served.fetch_add(1, Ordering::SeqCst);
            if served < injection.first_requests {
                let status = if injection.status == 0 {
                    429
                } else {
                    injection.status
                };
                let (code, reason) = if status == 503 {
                    (503, "Service Unavailable")
                } else {
                    (429, "Too Many Requests")
                };
                state.with_stats(|s| s.responses_429 += 1);
                write_head(
                    writer,
                    code,
                    reason,
                    &[
                        ("Retry-After", injection.retry_after_secs.to_string()),
                        ("Content-Length", "0".to_string()),
                        ("Connection", "keep-alive".to_string()),
                    ],
                )
                .await?;
                return Ok(true);
            }
        }
    }

    let size = config.size;
    let range = request.header("range").map(str::to_owned);
    if let Some(raw) = &range {
        state.with_stats(|s| s.range_headers.push(raw.clone()));
    }
    let chunked = config.no_content_length;

    let mut common_headers: Vec<(&str, String)> = Vec::new();
    common_headers.push(("Content-Type", config.content_type.clone()));
    if let Some(disposition) = &config.content_disposition {
        common_headers.push(("Content-Disposition", disposition.clone()));
    }
    if let Some(etag) = &config.etag {
        common_headers.push(("ETag", etag.clone()));
    }
    if let Some(last_modified) = &config.last_modified {
        common_headers.push(("Last-Modified", last_modified.clone()));
    }
    common_headers.push((
        "Accept-Ranges",
        if config.supports_ranges {
            "bytes".to_string()
        } else {
            "none".to_string()
        },
    ));
    if config.omit_accept_ranges {
        common_headers.pop();
    }
    common_headers.push(("Connection", "keep-alive".to_string()));

    // HEAD: headers only, never a body.
    if request.method == "HEAD" {
        let mut headers = common_headers;
        if !chunked {
            headers.push(("Content-Length", size.to_string()));
        }
        state.with_stats(|s| s.responses_200 += 1);
        write_head(writer, 200, "OK", &headers).await?;
        return Ok(true);
    }

    // Resolve the byte range the client asked for.
    let mut resolved: Option<(u64, u64)> = None;
    if let Some(raw) = range.as_deref() {
        if let Some((start, end)) = parse_range_header(raw, size) {
            if start >= size || end >= size {
                state.with_stats(|s| s.responses_416 += 1);
                write_head(
                    writer,
                    416,
                    "Range Not Satisfiable",
                    &[
                        ("Content-Range", format!("bytes */{size}")),
                        ("Content-Length", "0".to_string()),
                        ("Connection", "keep-alive".to_string()),
                    ],
                )
                .await?;
                return Ok(true);
            }
            if config.supports_ranges {
                resolved = Some((start, end));
            }
        }
    }

    let (status, start, end) = match resolved {
        Some((start, end)) => (206, start, end),
        None => {
            if size == 0 {
                (200, 0, 0)
            } else {
                (200, 0, size - 1)
            }
        }
    };
    let length = if status == 206 { end - start + 1 } else { size };

    let mut headers = common_headers;
    if status == 206 {
        headers.push(("Content-Range", format!("bytes {start}-{end}/{size}")));
        if !chunked {
            headers.push(("Content-Length", length.to_string()));
        }
    } else if !chunked {
        headers.push(("Content-Length", length.to_string()));
    }
    if chunked {
        headers.push(("Transfer-Encoding", "chunked".to_string()));
    }

    state.with_stats(|s| {
        if status == 206 {
            s.responses_206 += 1;
        } else {
            s.responses_200 += 1;
        }
    });

    let reason = if status == 206 {
        "Partial Content"
    } else {
        "OK"
    };
    write_head(writer, status, reason, &headers).await?;
    send_body(writer, state, config, start, length, chunked).await?;
    Ok(true)
}

async fn write_404(writer: &mut OwnedWriteHalf) -> std::io::Result<bool> {
    const BODY: &[u8] = b"not found";
    write_head(
        writer,
        404,
        "Not Found",
        &[
            ("Content-Type", "text/plain".to_string()),
            ("Content-Length", BODY.len().to_string()),
            ("Connection", "keep-alive".to_string()),
        ],
    )
    .await?;
    writer.write_all(BODY).await?;
    Ok(true)
}

/// `401` used by the P4a required-cookie mode.
async fn write_cookie_required(
    writer: &mut OwnedWriteHalf,
    head_only: bool,
) -> std::io::Result<bool> {
    const BODY: &[u8] = b"cookie required";
    write_head(
        writer,
        401,
        "Unauthorized",
        &[
            ("Content-Type", "text/plain".to_string()),
            ("Content-Length", BODY.len().to_string()),
            ("Connection", "keep-alive".to_string()),
        ],
    )
    .await?;
    if !head_only {
        writer.write_all(BODY).await?;
        writer.flush().await?;
    }
    Ok(true)
}

async fn write_status(
    writer: &mut OwnedWriteHalf,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
    body_len: usize,
) -> std::io::Result<bool> {
    let mut all: Vec<(&str, String)> = headers.to_vec();
    all.push(("Content-Length", body_len.to_string()));
    all.push(("Connection", "keep-alive".to_string()));
    write_head(writer, status, reason, &all).await?;
    Ok(true)
}

/// Stream `length` payload bytes starting at `start`, optionally with chunked
/// framing and configured per-granule latency. Cancelled promptly on shutdown.
async fn send_body(
    writer: &mut OwnedWriteHalf,
    state: &Arc<ServerState>,
    config: &Config,
    start: u64,
    length: u64,
    chunked: bool,
) -> std::io::Result<()> {
    let granule = if config.delay_ms > 0 && config.delay_every_bytes > 0 {
        config.delay_every_bytes
    } else {
        length.max(1)
    };
    let mut sent = 0u64;
    let mut buf = vec![0u8; CONTENT_BLOCK_SIZE];
    while sent < length {
        // One latency unit per granule (aligned to this response's start).
        if config.delay_ms > 0 {
            sleep(Duration::from_millis(config.delay_ms)).await;
        }
        let granule_len = granule.min(length - sent);
        let mut in_granule = 0u64;
        while in_granule < granule_len {
            let want = CONTENT_BLOCK_SIZE
                .min((granule_len - in_granule) as usize)
                .max(1);
            let piece = &mut buf[..want];
            fill_from_block(&state.block, start + sent, piece);

            if chunked {
                writer.write_all(format!("{want:x}\r\n").as_bytes()).await?;
                writer.write_all(piece).await?;
                writer.write_all(b"\r\n").await?;
            } else {
                writer.write_all(piece).await?;
            }
            sent += want as u64;
            in_granule += want as u64;
            state.with_stats(|s| s.body_bytes_sent += want as u64);
        }
    }
    if chunked {
        writer.write_all(b"0\r\n\r\n").await?;
    }
    writer.flush().await?;
    Ok(())
}

/// Parse a single-range `Range` header. Returns the inclusive byte range.
fn parse_range_header(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?.trim();
    if size == 0 {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;
    if start_s.is_empty() {
        // Suffix range: last N bytes.
        let n: u64 = end_s.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        let start = size.saturating_sub(n);
        return Some((start, size - 1));
    }
    let start: u64 = start_s.trim().parse().ok()?;
    let end = if end_s.trim().is_empty() {
        size - 1
    } else {
        end_s.trim().parse::<u64>().ok()?.min(size - 1)
    };
    if start > end {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_ranges() {
        assert_eq!(parse_range_header("bytes=0-0", 100), Some((0, 0)));
        assert_eq!(parse_range_header("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range_header("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range_header("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range_header("bytes=0-500", 100), Some((0, 99)));
        assert_eq!(parse_range_header("nibbles=0-5", 100), None);
        assert_eq!(parse_range_header("bytes=20-10", 100), None);
    }

    #[test]
    fn content_is_deterministic_and_block_aligned() {
        let size = CONTENT_BLOCK_SIZE as u64 + 8;
        let a = content_bytes(size, 7);
        let b = content_bytes(size, 7);
        assert_eq!(a, b);
        assert_ne!(a, content_bytes(size, 8));
        let block = content_block(7);
        assert_eq!(&a[..256], &block[..256]);
        // The second block restarts the pattern (content is block-periodic).
        assert_eq!(&a[CONTENT_BLOCK_SIZE..CONTENT_BLOCK_SIZE + 4], &block[..4]);
        assert_eq!(a.len() as u64, size);
    }

    // ------------------------------------------------------------------
    // P4a required-cookie mode
    // ------------------------------------------------------------------

    /// One raw HTTP/1.1 exchange with `Connection: close`.
    async fn raw_request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        cookie: Option<&str>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let mut stream = TcpStream::connect(addr).await.expect("connect fixture");
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        if let Some(cookie) = cookie {
            request.push_str("Cookie: ");
            request.push_str(cookie);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        stream.flush().await.expect("flush request");
        let mut response = Vec::new();
        let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test]
    async fn required_cookie_rejects_requests_without_it() {
        let server = TestServer::start(Config::new(16).with_required_cookie("session=abc"))
            .await
            .expect("start fixture");
        let response = raw_request(server.addr(), "GET", "/file", None).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        assert!(response.contains("Content-Type: text/plain"), "{response}");
        assert!(response.contains("cookie required"), "{response}");
        assert_eq!(server.stats().get_requests, 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn required_cookie_allows_matching_cookie_for_get_and_head() {
        let server = TestServer::start(Config::new(16).with_required_cookie("session=abc"))
            .await
            .expect("start fixture");

        let get = raw_request(server.addr(), "GET", "/file", Some("other=1; session=abc")).await;
        assert!(get.starts_with("HTTP/1.1 200"), "{get}");
        let head = raw_request(server.addr(), "HEAD", "/file", Some("session=abc")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Content-Length: 16"), "{head}");
        // HEAD must not carry a body after the header terminator.
        let header_end = head.find("\r\n\r\n").expect("header terminator");
        assert_eq!(header_end + 4, head.len(), "HEAD must not send a body");

        let stats = server.stats();
        assert_eq!(stats.get_requests, 1);
        assert_eq!(stats.head_requests, 1);
        assert_eq!(stats.responses_200, 2);
        server.shutdown().await;
    }
}
