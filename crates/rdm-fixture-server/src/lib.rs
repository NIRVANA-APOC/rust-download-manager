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
//!
//! # Network shaping (benchmark scenarios)
//!
//! Optional, default-off shaping models a shared bottleneck unlike the legacy
//! per-granule `delay_ms` (which grows aggregate throughput linearly with
//! connections):
//! * [`Config::shared_rate_bps`] — one fair global pacer across every response
//!   body (virtual-time reservations, FIFO by arrival: no connection can starve
//!   the others).
//! * [`Config::per_connection_rate_bps`] — every response body has its own rate.
//! * [`Config::max_concurrent_requests`] — excess concurrent `GET`/`HEAD`
//!   requests get `429` + `Retry-After` instead of being served.
//! * [`Config::request_latency_ms`] / [`Config::request_jitter_ms`] — RTT before
//!   a response starts (jitter is derived deterministically from the request
//!   ordinal and the content seed).
//! * [`Config::slow_tail_bytes`] / [`Config::slow_tail_factor`] — the trailing N
//!   payload bytes are served at `rate / factor` (slow CDN tail block).
//! * [`Config::concurrency_penalty`] — each connection gets slower as the number
//!   of in-flight requests grows (step function; used by the `decline` bench).
//!
//! All pacing is deterministic (tokio timers, fixed computation) and the default
//! configuration behaves exactly as before.

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
use tokio::time::{sleep, sleep_until, timeout, Instant};

/// Size of the deterministic pseudo-random content block used to synthesise
/// payload bytes. Content repeats this block, which keeps generation cheap.
pub const CONTENT_BLOCK_SIZE: usize = 64 * 1024;

/// Default granule size for artificial latency.
pub const DEFAULT_DELAY_EVERY_BYTES: u64 = 256 * 1024;

/// Pacing quantum for rate shaping: only waits longer than this actually sleep.
///
/// Windows' default system timer granularity is ~15.6 ms, so sleeping for a
/// sub-millisecond remainder per 64 KiB piece rounds up to a whole tick and can
/// halve the observed rate. Instead the virtual schedule always advances by the
/// exact `bytes / rate`, and the response only sleeps when it is at least one
/// quantum ahead; pieces that are "already due" are written immediately. The
/// long-run rate therefore equals the configured value and bursts stay bounded
/// by one quantum (plus one timer tick).
const PACING_SLEEP_QUANTUM: Duration = Duration::from_millis(16);

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

/// Concurrency-linked per-connection slowdown ("the more parallel requests, the
/// slower each one gets").
///
/// While at most `free_inflight` requests are in flight every response streams
/// at its configured rate; every extra in-flight request divides that rate by
/// another `1 + extra_slowdown`:
///
/// `effective = per_connection_rate / (1 + (inflight - free_inflight) * extra_slowdown)`
///
/// With `free_inflight = 4, extra_slowdown = 1, per_connection = 16 MiB/s` the
/// aggregate throughput peaks around 4 connections (~64 MiB/s) and *declines*
/// afterwards (~25 MiB/s at 8, ~20 MiB/s at 16) — the `decline` benchmark
/// scenario. Default off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyPenalty {
    /// In-flight requests that are not penalised.
    pub free_inflight: usize,
    /// Additional slowdown per extra in-flight request; `0` disables the effect.
    pub extra_slowdown: u64,
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
    /// Global shared bandwidth cap in bytes/second, summed over every
    /// concurrent response body (models the total link capacity). `None`
    /// (default) = unlimited. Need not exceed `u64::MAX / 1_000_000_000`.
    pub shared_rate_bps: Option<u64>,
    /// Per-response body rate cap in bytes/second (models a per-connection
    /// server limit). `None` (default) = unlimited.
    pub per_connection_rate_bps: Option<u64>,
    /// Maximum number of `GET`/`HEAD` requests served concurrently. Extra
    /// requests are rejected immediately with `429` + `Retry-After:
    /// reject_retry_after_secs`. `None` (default) = unlimited.
    pub max_concurrent_requests: Option<usize>,
    /// `Retry-After` value (seconds) used by the concurrency rejection; default 1.
    pub reject_retry_after_secs: u64,
    /// Fixed delay before a `GET`/`HEAD` response starts (models RTT).
    /// `0` (default) = no delay.
    pub request_latency_ms: u64,
    /// Upper bound of the deterministic per-request jitter added to
    /// [`Config::request_latency_ms`], in milliseconds. `0` (default) = none.
    pub request_jitter_ms: u64,
    /// When set, the trailing N bytes of the payload are served at
    /// `rate / slow_tail_factor` (models a slow tail block / slow CDN edge).
    /// `None` (default) = no tail.
    pub slow_tail_bytes: Option<u64>,
    /// Divisor applied to the rate inside the slow tail; default 8. `0` is
    /// treated as `1`.
    pub slow_tail_factor: u64,
    /// Every Nth request (1-based, counted across the server) is served at
    /// `rate / slow_request_factor` for its whole body. Models a straggler
    /// connection/bad CDN edge that a byte-range scheduler can work around by
    /// splitting the slow range onto another request. `None` (default) = off.
    pub slow_request_every: Option<u64>,
    /// Divisor for [`Config::slow_request_every`]; default 16.
    pub slow_request_factor: u64,
    /// Exact 1-based request ordinals whose whole body is slow. More
    /// deterministic than [`Config::slow_request_every`] for A/B experiments:
    /// after the HEAD probe, ordinal 2 is the first payload request.
    pub slow_request_ordinals: Vec<u64>,
    /// Close the connection after this many body bytes for requests listed in
    /// [`Config::truncate_on_ordinals`] (simulates a dropped/reset connection).
    pub truncate_after_bytes: Option<u64>,
    /// Request ordinals whose body is truncated once (see
    /// [`Config::truncate_after_bytes`]).
    pub truncate_on_ordinals: Vec<u64>,
    /// Request ordinals (1-based) that receive valid `206` headers but an empty
    /// body. Models a broken CDN edge / a server that advertises range support
    /// but returns zero bytes for some ranges. Default empty.
    pub empty_range_ordinals: Vec<u64>,
    /// After this many body bytes, listed request ordinals go silent (the
    /// handler sleeps). Models a half-open connection / stalled CDN edge.
    pub stall_after_bytes: Option<u64>,
    /// Request ordinals whose body stalls once (see
    /// [`Config::stall_after_bytes`]).
    pub stall_on_ordinals: Vec<u64>,
    /// Optional "more connections = slower per connection" penalty. Default off.
    pub concurrency_penalty: Option<ConcurrencyPenalty>,
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
            shared_rate_bps: None,
            per_connection_rate_bps: None,
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            request_jitter_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            truncate_after_bytes: None,
            truncate_on_ordinals: Vec::new(),
            empty_range_ordinals: Vec::new(),
            stall_after_bytes: None,
            stall_on_ordinals: Vec::new(),
            concurrency_penalty: None,
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

    /// Cap the aggregate body throughput shared by every concurrent response.
    pub fn with_shared_rate(mut self, bps: u64) -> Self {
        self.shared_rate_bps = Some(bps.max(1));
        self
    }

    /// Cap each individual response body rate.
    pub fn with_per_connection_rate(mut self, bps: u64) -> Self {
        self.per_connection_rate_bps = Some(bps.max(1));
        self
    }

    /// Serve at most `n` concurrent `GET`/`HEAD` requests; extra ones get
    /// `429` + `Retry-After: 1`.
    pub fn with_concurrency_limit(mut self, n: usize) -> Self {
        self.max_concurrent_requests = Some(n.max(1));
        self
    }

    /// `Retry-After` (seconds) sent by concurrency rejections.
    pub fn with_reject_retry_after(mut self, secs: u64) -> Self {
        self.reject_retry_after_secs = secs;
        self
    }

    /// Fixed RTT (milliseconds) before every `GET`/`HEAD` response starts.
    pub fn with_request_latency(mut self, ms: u64) -> Self {
        self.request_latency_ms = ms;
        self
    }

    /// Deterministic jitter (0..=ms) added to the request latency.
    pub fn with_request_jitter(mut self, ms: u64) -> Self {
        self.request_jitter_ms = ms;
        self
    }

    /// Serve the trailing `bytes` payload bytes at `rate / 8`.
    pub fn with_slow_tail(mut self, bytes: u64) -> Self {
        self.slow_tail_bytes = Some(bytes);
        self
    }

    /// Divisor applied inside the slow tail (default 8).
    pub fn with_slow_tail_factor(mut self, factor: u64) -> Self {
        self.slow_tail_factor = factor.max(1);
        self
    }

    /// Serve every Nth request (1-based) at `1/factor` of its normal rate.
    pub fn with_slow_request_every(mut self, every: u64) -> Self {
        self.slow_request_every = Some(every.max(1));
        self
    }

    /// Divisor applied to bodies of [`Config::slow_request_every`] requests.
    pub fn with_slow_request_factor(mut self, factor: u64) -> Self {
        self.slow_request_factor = factor.max(1);
        self
    }

    /// Mark exact request ordinals (1-based) as slow stragglers.
    pub fn with_slow_request_ordinals(mut self, ordinals: impl IntoIterator<Item = u64>) -> Self {
        self.slow_request_ordinals = ordinals.into_iter().collect();
        self
    }

    /// Truncate the body of the listed request ordinals after `bytes` bytes.
    pub fn with_truncated_body(
        mut self,
        bytes: u64,
        ordinals: impl IntoIterator<Item = u64>,
    ) -> Self {
        self.truncate_after_bytes = Some(bytes);
        self.truncate_on_ordinals = ordinals.into_iter().collect();
        self
    }

    /// Serve a valid `206` head with **zero** body bytes for the listed request
    /// ordinals (1-based). Models servers that advertise range support but
    /// return empty range bodies for some requests/edges.
    pub fn with_empty_range_bodies(mut self, ordinals: impl IntoIterator<Item = u64>) -> Self {
        self.empty_range_ordinals = ordinals.into_iter().collect();
        self
    }

    /// Go silent after `bytes` body bytes for the listed request ordinals
    /// (1-based). Models a half-open connection that never delivers the rest.
    pub fn with_stalled_body(
        mut self,
        bytes: u64,
        ordinals: impl IntoIterator<Item = u64>,
    ) -> Self {
        self.stall_after_bytes = Some(bytes);
        self.stall_on_ordinals = ordinals.into_iter().collect();
        self
    }

    /// Slow every response down as concurrency grows (see [`ConcurrencyPenalty`]).
    pub fn with_concurrency_penalty(mut self, free_inflight: usize, extra_slowdown: u64) -> Self {
        self.concurrency_penalty = Some(ConcurrencyPenalty {
            free_inflight,
            extra_slowdown,
        });
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
    /// Peak number of simultaneously in-flight `GET`/`HEAD` requests. Updated
    /// on every entry, so it also reports the realised concurrency of the
    /// client.
    pub max_inflight_requests: u64,
    /// Requests rejected because [`Config::max_concurrent_requests`] was
    /// exceeded (each also counted in [`Stats::responses_429`]).
    pub rejected_concurrency: u64,
    /// Number of body pieces that had to wait on the global shared pacer
    /// (a proxy for "the shared link was saturated"). Always 0 when
    /// [`Config::shared_rate_bps`] is `None`.
    pub shared_throttle_waits: u64,
    /// Responses whose body was deliberately truncated (connection closed).
    pub truncated_responses: u64,
    /// Valid `206` heads served with a deliberately empty body.
    pub empty_range_bodies: u64,
    /// Responses that went silent mid-body after [`Config::stall_after_bytes`].
    pub stalled_responses: u64,
}

/// Fair global pacer: callers reserve a virtual finish time and sleep until it.
///
/// Bookings are served in mutex-lock (arrival) order and each reservation moves
/// the virtual clock forward by `bytes / rate`, so the long-run aggregate rate
/// equals `rate` and no connection can monopolise the bandwidth (unlike a
/// shared token bucket with a plain retry loop). Rate changes apply to later
/// reservations, which makes mid-transfer updates possible.
struct SharedPacer {
    next_free: Mutex<Instant>,
}

impl SharedPacer {
    fn new() -> Self {
        Self {
            next_free: Mutex::new(Instant::now()),
        }
    }

    /// Reserve `bytes` at `rate_bps` and return the virtual finish deadline.
    fn reserve(&self, bytes: u64, rate_bps: u64) -> Instant {
        let duration = duration_for(bytes, rate_bps);
        let mut next_free = self.next_free.lock().expect("pacer mutex poisoned");
        let now = Instant::now();
        let start = (*next_free).max(now);
        let finish = start + duration;
        *next_free = finish;
        finish
    }
}

/// Guard keeping the in-flight `GET`/`HEAD` counter accurate on every return
/// path (early `429`, aborted transfer, io error, normal completion).
struct InflightGuard {
    state: Arc<ServerState>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.state.inflight_requests.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Snapshot of the shaping knobs taken once per 64 KiB body piece. Read under
/// the config `RwLock` so a running transfer can observe
/// [`TestServer::set_shared_rate`] changes (the `drift` benchmark).
#[derive(Debug, Clone)]
struct ShapingSnapshot {
    size: u64,
    shared_rate_bps: Option<u64>,
    per_connection_rate_bps: Option<u64>,
    slow_tail_bytes: Option<u64>,
    slow_tail_factor: u64,
    slow_request_every: Option<u64>,
    slow_request_factor: u64,
    slow_request_ordinals: Vec<u64>,
    concurrency_penalty: Option<ConcurrencyPenalty>,
}

impl ShapingSnapshot {
    fn from_config(config: &Config) -> Self {
        Self {
            size: config.size,
            shared_rate_bps: config.shared_rate_bps,
            per_connection_rate_bps: config.per_connection_rate_bps,
            slow_tail_bytes: config.slow_tail_bytes,
            slow_tail_factor: config.slow_tail_factor.max(1),
            slow_request_every: config.slow_request_every,
            slow_request_factor: config.slow_request_factor.max(1),
            slow_request_ordinals: config.slow_request_ordinals.clone(),
            concurrency_penalty: config.concurrency_penalty,
        }
    }

    /// Rate divisor for the absolute payload offset (slow tail or not).
    fn tail_factor_at(&self, absolute: u64) -> u64 {
        let Some(tail) = self.slow_tail_bytes else {
            return 1;
        };
        if tail == 0 || self.size == 0 {
            return 1;
        }
        let tail_start = self.size.saturating_sub(tail);
        if absolute >= tail_start {
            self.slow_tail_factor.max(1)
        } else {
            1
        }
    }

    /// Rate divisor for a whole response (straggler request).
    fn request_factor(&self, ordinal: u64) -> u64 {
        if ordinal == 0 {
            return 1;
        }
        if self.slow_request_ordinals.contains(&ordinal) {
            return self.slow_request_factor.max(1);
        }
        match self.slow_request_every {
            Some(every) if every > 0 && ordinal.is_multiple_of(every) => {
                self.slow_request_factor.max(1)
            }
            _ => 1,
        }
    }

    /// Per-connection rate divisor from the concurrency penalty.
    fn penalty_divisor(&self, inflight: u64) -> u64 {
        let Some(penalty) = self.concurrency_penalty else {
            return 1;
        };
        let extra = inflight.saturating_sub(penalty.free_inflight as u64);
        1u64.saturating_add(extra.saturating_mul(penalty.extra_slowdown))
    }
}

/// Convert a byte budget at `rate_bps` into a duration (at least 1 ns).
fn duration_for(bytes: u64, rate_bps: u64) -> Duration {
    let rate = rate_bps.max(1) as u128;
    let nanos = (bytes as u128 * 1_000_000_000u128) / rate;
    Duration::from_nanos(nanos.max(1).min(u64::MAX as u128) as u64)
}

/// Deterministic 64-bit mix used for request jitter (no RNG dependency).
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Jitter in `0..=jitter_ms` derived from `(seed, request ordinal)`.
fn deterministic_jitter_ms(seed: u64, ordinal: u64, jitter_ms: u64) -> u64 {
    if jitter_ms == 0 {
        return 0;
    }
    splitmix64(seed ^ ordinal.wrapping_mul(0x9E37_79B9_7F4A_7C15)) % (jitter_ms + 1)
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
    /// Truncate ordinals already used.
    truncate_used: Mutex<HashSet<u64>>,
    /// Empty-range-body ordinals already served.
    empty_range_used: Mutex<HashSet<u64>>,
    /// Stall ordinals already served.
    stall_used: Mutex<HashSet<u64>>,
    /// Rate-limit rejections already served.
    rate_limited_served: AtomicU64,
    /// Global shared-bandwidth pacer (used only when `shared_rate_bps` is set).
    shared_pacer: SharedPacer,
    /// In-flight `GET`/`HEAD` handlers, including ones sleeping in the
    /// request-latency simulation.
    inflight_requests: AtomicU64,
    /// Observed peak of `inflight_requests` (reported through `Stats`).
    max_inflight_requests: AtomicU64,
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
            truncate_used: Mutex::new(HashSet::new()),
            empty_range_used: Mutex::new(HashSet::new()),
            stall_used: Mutex::new(HashSet::new()),
            rate_limited_served: AtomicU64::new(0),
            shared_pacer: SharedPacer::new(),
            inflight_requests: AtomicU64::new(0),
            max_inflight_requests: AtomicU64::new(0),
        }
    }

    fn stats(&self) -> Stats {
        let mut stats = self.stats.lock().expect("stats mutex poisoned").clone();
        stats.max_inflight_requests = self.max_inflight_requests.load(Ordering::SeqCst);
        stats
    }

    fn reset_stats(&self) {
        *self.stats.lock().expect("stats mutex poisoned") = Stats::default();
        // Keep the invariant "peak >= current" so a reset mid-flight cannot
        // report a peak below the live concurrency.
        let current = self.inflight_requests.load(Ordering::SeqCst);
        self.max_inflight_requests.store(current, Ordering::SeqCst);
    }

    fn with_stats<R>(&self, f: impl FnOnce(&mut Stats) -> R) -> R {
        let mut guard = self.stats.lock().expect("stats mutex poisoned");
        f(&mut guard)
    }

    fn current_inflight(&self) -> u64 {
        self.inflight_requests.load(Ordering::SeqCst)
    }

    /// Enter one `GET`/`HEAD` request. Returns `None` when the configured
    /// concurrency limit is exceeded (the caller must then send `429`); the
    /// returned guard decrements the counter on drop (RAII, every path).
    fn enter_inflight(self: &Arc<Self>, limit: Option<usize>) -> Option<InflightGuard> {
        let now = self.inflight_requests.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(limit) = limit {
            // Rejected requests never count towards the observed peak: they are
            // not "being processed", they only receive a 429.
            if now > limit as u64 {
                self.inflight_requests.fetch_sub(1, Ordering::SeqCst);
                return None;
            }
        }
        self.max_inflight_requests.fetch_max(now, Ordering::SeqCst);
        Some(InflightGuard {
            state: Arc::clone(self),
        })
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

    /// Change the global shared bandwidth cap while the server is running
    /// (e.g. the `drift` benchmark ramps it up halfway through a transfer).
    /// `None` removes the cap. Takes effect for body pieces that start after
    /// the update; already reserved pieces keep their schedule.
    pub async fn set_shared_rate(&self, bps: Option<u64>) {
        self.update_config(|c| c.shared_rate_bps = bps.map(|rate| rate.max(1)))
            .await;
    }

    /// Change the per-response rate cap while the server is running.
    pub async fn set_per_connection_rate(&self, bps: Option<u64>) {
        self.update_config(|c| {
            c.per_connection_rate_bps = bps.map(|rate| rate.max(1));
        })
        .await;
    }

    /// Change (or remove with `None`) the concurrency limit while running.
    pub async fn set_concurrency_limit(&self, limit: Option<usize>) {
        self.update_config(|c| c.max_concurrent_requests = limit.map(|n| n.max(1)))
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

    // Concurrency limit (default off): count every in-flight GET/HEAD, and
    // reject the request that would exceed the configured limit with
    // `429` + `Retry-After`. The guard keeps the counter correct on every
    // return path (including io errors and the shared-pacer waits below).
    let _inflight = match state.enter_inflight(config.max_concurrent_requests) {
        Some(guard) => guard,
        None => {
            state.with_stats(|s| {
                s.rejected_concurrency += 1;
                s.responses_429 += 1;
            });
            write_head(
                writer,
                429,
                "Too Many Requests",
                &[
                    ("Retry-After", config.reject_retry_after_secs.to_string()),
                    ("Content-Length", "0".to_string()),
                    ("Connection", "keep-alive".to_string()),
                ],
            )
            .await?;
            return Ok(true);
        }
    };

    // Request latency / RTT simulation (default off). Jitter is deterministic:
    // it only depends on the content seed and the request ordinal (which is
    // assigned in arrival order on this server).
    let jitter_ms = deterministic_jitter_ms(config.seed, request.ordinal, config.request_jitter_ms);
    let latency_ms = config.request_latency_ms.saturating_add(jitter_ms);
    if latency_ms > 0 {
        sleep(Duration::from_millis(latency_ms)).await;
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
    // RFC 9110 §13.1.5: a request with `If-Range` whose validator does not
    // match the current representation must be answered with the whole
    // representation (200), i.e. the Range header is ignored. This is how a
    // downloader detects that the object changed between range requests.
    let range_valid = match (request.header("if-range"), config.etag.as_deref()) {
        (Some(validator), Some(etag)) => validator == etag,
        // No current validator or no If-Range: keep range semantics.
        _ => true,
    };
    let range = if range_valid { range } else { None };
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

    // Broken-range-edge injection: valid 206 headers, zero body bytes.
    if status == 206 && config.empty_range_ordinals.contains(&request.ordinal) {
        let first_time = state
            .empty_range_used
            .lock()
            .expect("fixture mutex poisoned")
            .insert(request.ordinal);
        if first_time {
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-length"));
            if !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
            {
                headers.push(("Transfer-Encoding", "chunked".to_string()));
            }
            state.with_stats(|s| {
                s.responses_206 += 1;
                s.empty_range_bodies += 1;
            });
            write_head(writer, status, "Partial Content", &headers).await?;
            writer.write_all(b"0\r\n\r\n").await?;
            return Ok(true);
        }
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
    send_body(
        writer,
        state,
        config,
        start,
        length,
        chunked,
        request.ordinal,
    )
    .await?;
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
///
/// Shaping (all default-off, see the module docs):
/// * `per_connection_rate_bps` — local virtual-time pacer (one per response).
/// * `shared_rate_bps` — reservation on the server-wide [`SharedPacer`]; the
///   reservation is taken per 64 KiB piece and the piece is written when its
///   slot arrives, so the aggregate stays at the cap and arrivals are served
///   in FIFO order (no single connection can monopolise the link).
/// * `slow_tail_bytes` — pieces inside the trailing window use
///   `rate / slow_tail_factor`.
/// * `concurrency_penalty` — divides the per-connection rate by
///   `1 + (inflight - free) * extra_slowdown`.
///
/// The shaping config is re-read for every piece, which lets
/// [`TestServer::set_shared_rate`] change a running transfer's rate.
async fn send_body(
    writer: &mut OwnedWriteHalf,
    state: &Arc<ServerState>,
    config: &Config,
    start: u64,
    length: u64,
    chunked: bool,
    ordinal: u64,
) -> std::io::Result<()> {
    let granule = if config.delay_ms > 0 && config.delay_every_bytes > 0 {
        config.delay_every_bytes
    } else {
        length.max(1)
    };
    let mut sent = 0u64;
    let mut buf = vec![0u8; CONTENT_BLOCK_SIZE];
    // 本响应的本地节拍（仅 per_connection_rate 生效时推进），跨 granule 保留。
    let mut next_local: Option<Instant> = None;
    // Straggler requests are slow for their whole body.
    let request_factor = ShapingSnapshot::from_config(&*state.config.read().await)
        .request_factor(ordinal)
        .max(1);
    // Simulated mid-body connection drop (one truncation per listed ordinal).
    let truncate_after = match config.truncate_after_bytes {
        Some(bytes) if config.truncate_on_ordinals.contains(&ordinal) => {
            let mut used = state.truncate_used.lock().expect("truncate mutex poisoned");
            used.insert(ordinal).then_some(bytes)
        }
        _ => None,
    };
    // Simulated half-open connection: go silent after N bytes (one per ordinal).
    let stall_after = match config.stall_after_bytes {
        Some(bytes) if config.stall_on_ordinals.contains(&ordinal) => {
            let mut used = state.stall_used.lock().expect("stall mutex poisoned");
            used.insert(ordinal).then_some(bytes)
        }
        _ => None,
    };
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
            let absolute = start + sent;

            // 每片读取一次塑形配置（drift 场景需要在传输中修改 shared_rate）。
            let shaping = ShapingSnapshot::from_config(&*state.config.read().await);
            let tail_factor = shaping.tail_factor_at(absolute);
            let penalty_divisor = shaping.penalty_divisor(state.current_inflight());

            // 本地（每连接）虚拟时间轴：按字节精确推进，不因唤醒迟到而重置。
            let mut local_deadline: Option<Instant> = None;
            if let Some(rate) = shaping.per_connection_rate_bps {
                let effective = (rate / tail_factor / penalty_divisor / request_factor).max(1);
                let duration = duration_for(want as u64, effective);
                let now = Instant::now();
                let finish = next_local.unwrap_or(now) + duration;
                next_local = Some(finish);
                local_deadline = Some(finish);
            }

            // 全局共享令牌：按到达顺序预约虚拟完成时刻，保证公平。
            let mut shared_deadline: Option<Instant> = None;
            if let Some(rate) = shaping.shared_rate_bps {
                let effective = (rate / tail_factor / request_factor).max(1);
                shared_deadline = Some(state.shared_pacer.reserve(want as u64, effective));
            }

            let deadline = match (local_deadline, shared_deadline) {
                (Some(local), Some(shared)) => Some(local.max(shared)),
                (local, shared) => local.or(shared),
            };

            // 只有“还差至少一个量子”的等待才 sleep：小于量子的余额立即补发。
            // 见 [`PACING_SLEEP_QUANTUM`]：这样系统定时器粒度不会把速率拖慢。
            if let Some(deadline) = deadline {
                if deadline.saturating_duration_since(Instant::now()) >= PACING_SLEEP_QUANTUM {
                    if shared_deadline == Some(deadline) {
                        state.with_stats(|s| s.shared_throttle_waits += 1);
                    }
                    sleep_until(deadline).await;
                }
            }

            let piece = &mut buf[..want];
            fill_from_block(&state.block, absolute, piece);

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
            if let Some(limit) = stall_after {
                if sent >= limit {
                    writer.flush().await?;
                    state.with_stats(|s| s.stalled_responses += 1);
                    // Half-open connection: no FIN, no more bytes. The client's
                    // adaptive stall detector must abandon this attempt.
                    sleep(Duration::from_secs(60)).await;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "fixture stalled response body",
                    ));
                }
            }
            if let Some(limit) = truncate_after {
                if sent >= limit {
                    // Flush what we already wrote, give the client a moment to
                    // read it, then drop the connection without finishing the
                    // body. Without the pause the client can lose the last few
                    // socket bytes, which makes resume-offset assertions flaky.
                    writer.flush().await?;
                    sleep(Duration::from_millis(100)).await;
                    state.with_stats(|s| s.truncated_responses += 1);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "fixture truncated response body",
                    ));
                }
            }
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

    // ------------------------------------------------------------------
    // Network shaping (benchmark scenarios)
    // ------------------------------------------------------------------

    /// Parsed response of one `Connection: close` exchange, with timings.
    struct RawResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        elapsed: Duration,
    }

    impl RawResponse {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    async fn fetch(
        addr: SocketAddr,
        method: &str,
        path: &str,
        extra_headers: &[(&str, String)],
    ) -> RawResponse {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let started = Instant::now();
        let mut stream = TcpStream::connect(addr).await.expect("connect fixture");
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        for (name, value) in extra_headers {
            request.push_str(name);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        stream.flush().await.expect("flush request");
        let mut raw = Vec::new();
        timeout(Duration::from_secs(30), stream.read_to_end(&mut raw))
            .await
            .expect("response within deadline")
            .expect("read response");
        let elapsed = started.elapsed();
        let header_end = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header terminator");
        let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
        let body = raw[header_end + 4..].to_vec();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .expect("status line");
        let headers = lines
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        RawResponse {
            status,
            headers,
            body,
            elapsed,
        }
    }

    async fn fetch_range(addr: SocketAddr, start: u64, end: u64) -> RawResponse {
        fetch(
            addr,
            "GET",
            "/file",
            &[("Range", format!("bytes={start}-{end}"))],
        )
        .await
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        for ordinal in 0..64 {
            let a = deterministic_jitter_ms(7, ordinal, 50);
            let b = deterministic_jitter_ms(7, ordinal, 50);
            assert_eq!(a, b, "jitter must be deterministic");
            assert!(a <= 50, "jitter {a} exceeds bound");
        }
        assert_eq!(deterministic_jitter_ms(7, 1, 0), 0);
        // Different seeds must produce different jitter for at least one ordinal
        // (guards against accidentally ignoring the seed).
        let differs = (0..64).any(|ordinal| {
            deterministic_jitter_ms(7, ordinal, 1000) != deterministic_jitter_ms(8, ordinal, 1000)
        });
        assert!(differs, "jitter must depend on the seed");
    }

    #[test]
    fn shaping_defaults_match_legacy_behaviour() {
        let config = Config::new(1234);
        assert_eq!(config.shared_rate_bps, None);
        assert_eq!(config.per_connection_rate_bps, None);
        assert_eq!(config.max_concurrent_requests, None);
        assert_eq!(config.reject_retry_after_secs, 1);
        assert_eq!(config.request_latency_ms, 0);
        assert_eq!(config.request_jitter_ms, 0);
        assert_eq!(config.slow_tail_bytes, None);
        assert_eq!(config.slow_tail_factor, 8);
        assert_eq!(config.slow_request_every, None);
        assert_eq!(config.slow_request_factor, 16);
        assert!(config.slow_request_ordinals.is_empty());
        assert!(config.truncate_after_bytes.is_none());
        assert!(config.truncate_on_ordinals.is_empty());
        assert_eq!(config.concurrency_penalty, None);
        assert_eq!(config.delay_ms, 0);
        assert_eq!(config.delay_every_bytes, DEFAULT_DELAY_EVERY_BYTES);
    }

    #[tokio::test]
    async fn slow_request_every_slows_one_of_n_bodies() {
        let total = 512 * 1024u64;
        let rate = 2 * 1024 * 1024u64; // normal body ~0.25 s
        let server = TestServer::start(
            Config::new(total)
                .with_per_connection_rate(rate)
                .with_slow_request_every(2)
                .with_slow_request_factor(8),
        )
        .await
        .expect("start fixture");

        // Request #1 is normal, request #2 is a straggler (8x slower).
        let first = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(first.status, 200);
        let first_elapsed = first.elapsed;
        assert!(
            first_elapsed < Duration::from_secs(2),
            "first body too slow: {first_elapsed:?}"
        );
        let second = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(second.status, 200);
        assert!(
            second.elapsed >= Duration::from_secs(1),
            "second body should be a straggler: {:?}",
            second.elapsed
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn truncated_body_closes_connection_early_once() {
        let total = 512 * 1024u64;
        let server = TestServer::start(Config::new(total).with_truncated_body(128 * 1024, [1]))
            .await
            .expect("start fixture");

        // First request is cut off after 128 KiB.
        let first = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(first.status, 200);
        assert_eq!(first.body.len(), 128 * 1024);
        assert_eq!(server.stats().truncated_responses, 1);

        // The truncation is one-shot: the next request gets the full body.
        let second = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(second.status, 200);
        assert_eq!(second.body.len() as u64, total);
        assert_eq!(server.stats().truncated_responses, 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn shared_rate_caps_aggregate_throughput() {
        let total = 1024 * 1024u64;
        let rate = 2 * 1024 * 1024u64; // 1 MiB / 2 MiB/s → ~0.5 s
        let server = TestServer::start(Config::new(total).with_shared_rate(rate))
            .await
            .expect("start fixture");

        let started = Instant::now();
        let mut tasks = Vec::new();
        for index in 0..4u64 {
            let addr = server.addr();
            let start = index * 256 * 1024;
            tasks.push(tokio::spawn(fetch_range(
                addr,
                start,
                start + 256 * 1024 - 1,
            )));
        }
        for task in tasks {
            let response = task.await.expect("join");
            assert_eq!(response.status, 206, "range response");
            assert_eq!(response.body.len(), 256 * 1024);
        }
        let elapsed = started.elapsed();

        // 1 MiB at 2 MiB/s ≈ 0.5 s. Very loose bounds: the point is "not
        // 4× faster than the cap" on a loaded machine, not wall-clock accuracy.
        assert!(
            elapsed >= Duration::from_millis(350),
            "aggregate faster than shared cap: {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_secs(10),
            "shared pacing too slow: {elapsed:?}"
        );
        let stats = server.stats();
        assert_eq!(stats.body_bytes_sent, total);
        assert!(
            stats.shared_throttle_waits > 0,
            "expected shared pacer waits, stats={stats:?}"
        );
        assert!(
            stats.max_inflight_requests >= 2,
            "expected overlapping requests, stats={stats:?}"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn per_connection_rate_limits_single_stream() {
        let total = 512 * 1024u64;
        let rate = 2 * 1024 * 1024u64; // ~0.25 s
        let server = TestServer::start(Config::new(total).with_per_connection_rate(rate))
            .await
            .expect("start fixture");

        let response = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, server.content());
        assert!(
            response.elapsed >= Duration::from_millis(150),
            "per-connection cap not applied: {:?}",
            response.elapsed
        );
        assert_eq!(server.stats().body_bytes_sent, total);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn concurrency_limit_rejects_excess_with_retry_after() {
        let server = TestServer::start(
            Config::new(1024 * 1024)
                .with_concurrency_limit(2)
                .with_reject_retry_after(1)
                .with_request_latency(200),
        )
        .await
        .expect("start fixture");

        let mut tasks = Vec::new();
        for _ in 0..6 {
            let addr = server.addr();
            // Tiny ranges: the requests only exist to hold an in-flight slot.
            tasks.push(tokio::spawn(fetch_range(addr, 0, 15)));
        }
        let mut accepted = 0;
        let mut rejected = 0;
        for task in tasks {
            let response = task.await.expect("join");
            if response.status == 429 {
                rejected += 1;
                assert_eq!(
                    response.header("retry-after"),
                    Some("1"),
                    "concurrency rejection must carry Retry-After"
                );
            } else {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 2, "exactly the limit is served");
        assert_eq!(rejected, 4, "excess requests are rejected");
        let stats = server.stats();
        assert_eq!(stats.rejected_concurrency, 4);
        assert_eq!(stats.responses_429, 4);
        assert_eq!(
            stats.max_inflight_requests, 2,
            "rejected requests must not inflate the observed peak"
        );

        // The RAII guard must not leak slots: a follow-up request succeeds.
        let after = fetch_range(server.addr(), 0, 15).await;
        assert_eq!(after.status, 206, "in-flight slots leaked");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn slow_tail_slows_trailing_bytes() {
        let total = 1024 * 1024u64;
        let shared = 8 * 1024 * 1024u64;
        let tail = total / 2;
        // 非尾部 512 KiB @ 8 MiB/s ≈ 64 ms，尾部 512 KiB @ 1 MiB/s ≈ 512 ms.
        let server = TestServer::start(
            Config::new(total)
                .with_shared_rate(shared)
                .with_slow_tail(tail)
                .with_slow_tail_factor(8),
        )
        .await
        .expect("start fixture");

        let response = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, server.content());
        assert!(
            response.elapsed >= Duration::from_millis(400),
            "slow tail not applied: {:?}",
            response.elapsed
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn request_latency_delays_response_start() {
        let server = TestServer::start(
            Config::new(1024)
                .with_request_latency(120)
                .with_request_jitter(0),
        )
        .await
        .expect("start fixture");

        let response = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body.len(), 1024);
        assert!(
            response.elapsed >= Duration::from_millis(100),
            "request latency not applied: {:?}",
            response.elapsed
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn concurrency_penalty_slows_many_connections() {
        let total = 8 * 1024 * 1024u64;
        let server = TestServer::start(
            Config::new(total)
                .with_per_connection_rate(16 * 1024 * 1024)
                .with_concurrency_penalty(4, 1),
        )
        .await
        .expect("start fixture");

        // 8 × 1 MiB 并发：inflight=8 时每连接只有 16/(1+4)=3.2 MiB/s，
        // 合计约 25.6 MiB/s → 8 MiB 至少约 300 ms（无惩罚时约 62 ms）。
        let started = Instant::now();
        let mut tasks = Vec::new();
        for index in 0..8u64 {
            let addr = server.addr();
            let start = index * 1024 * 1024;
            tasks.push(tokio::spawn(fetch_range(
                addr,
                start,
                start + 1024 * 1024 - 1,
            )));
        }
        for task in tasks {
            let response = task.await.expect("join");
            assert_eq!(response.status, 206);
            assert_eq!(response.body.len(), 1024 * 1024);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "concurrency penalty not applied: {elapsed:?}"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn shared_rate_can_change_mid_run() {
        let total = 256 * 1024u64;
        let server = TestServer::start(Config::new(total).with_shared_rate(1024 * 1024))
            .await
            .expect("start fixture");

        // 慢：256 KiB @ 1 MiB/s ≈ 256 ms。
        let slow = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(slow.status, 200);
        assert!(
            slow.elapsed >= Duration::from_millis(180),
            "initial shared rate not applied: {:?}",
            slow.elapsed
        );

        // 运行中提速到 8 MiB/s：同样大小约 32 ms。
        server.set_shared_rate(Some(8 * 1024 * 1024)).await;
        let fast = fetch(server.addr(), "GET", "/file", &[]).await;
        assert_eq!(fast.status, 200);
        assert!(
            fast.elapsed <= Duration::from_millis(150),
            "updated shared rate not applied: {:?}",
            fast.elapsed
        );
        assert_eq!(server.stats().body_bytes_sent, 2 * total);
        server.shutdown().await;
    }
}
