//! Shared HTTP client construction.
//!
//! One `reqwest::Client` per download (connection pool + HTTP/2 multiplexing +
//! timeouts). `Accept-Encoding: identity` is forced last so that ranged reads
//! can never be transformed by a content coding (which would invalidate every
//! `Content-Range`).

use std::time::Duration;

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, COOKIE, REFERER, USER_AGENT,
};

use crate::{DownloadError, DownloadRequest, DEFAULT_USER_AGENT};

/// Connection timeout (per architecture §3.5).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Read timeout for a single response read.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn build(request: &DownloadRequest) -> Result<reqwest::Client, DownloadError> {
    build_inner(request, false)
}

/// HTTP/1.1-only client used for *chunked* transfers.
///
/// This is deliberate: over HTTP/2 all range requests would share one TCP
/// connection and therefore one congestion window, so "8 connections" would
/// deliver ~1 connection of throughput. Forcing h1 turns each worker into a
/// real TCP connection and makes multi-connection speedups honest.
pub(crate) fn build_http1(request: &DownloadRequest) -> Result<reqwest::Client, DownloadError> {
    build_inner(request, true)
}

fn build_inner(request: &DownloadRequest, http1_only: bool) -> Result<reqwest::Client, DownloadError> {
    let mut headers = HeaderMap::new();

    // Caller-provided headers first so explicit fields below win.
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            DownloadError::Other(format!("invalid request header name: {name}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            DownloadError::Other(format!("invalid value for request header {name}"))
        })?;
        headers.insert(name, value);
    }

    let user_agent = request.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT);
    headers.insert(USER_AGENT, header(user_agent, "user-agent")?);
    if let Some(referer) = &request.referer {
        headers.insert(REFERER, header(referer, "referer")?);
    }
    if let Some(cookie) = &request.cookie {
        headers.insert(COOKIE, header(cookie, "cookie")?);
    }
    // Never negotiate compression: ranged offsets address the stored bytes.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .pool_max_idle_per_host(crate::MAX_CONNECTIONS)
        .redirect(reqwest::redirect::Policy::limited(10));

    if http1_only {
        builder = builder.http1_only();
    }

    if let Some(proxy) = &request.proxy {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|error| DownloadError::Other(format!("invalid proxy `{proxy}`: {error}")))?;
        builder = builder.proxy(proxy);
    }
    if request.insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }

    builder
        .build()
        .map_err(|error| DownloadError::Http(format!("failed to build HTTP client: {error}")))
}

fn header(value: &str, what: &str) -> Result<HeaderValue, DownloadError> {
    HeaderValue::from_str(value)
        .map_err(|_| DownloadError::Other(format!("invalid {what} header value")))
}

/// Validate and normalise the request URL (http/https only).
pub(crate) fn validate_url(url: &str) -> Result<url::Url, DownloadError> {
    let parsed =
        url::Url::parse(url).map_err(|error| DownloadError::InvalidUrl(format!("{url}: {error}")))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(DownloadError::InvalidUrl(format!(
            "{url}: unsupported scheme `{other}`"
        ))),
    }
}
