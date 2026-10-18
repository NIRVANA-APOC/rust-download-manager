//! Remote metadata probing (architecture §3.2).
//!
//! 1. `HEAD` with the caller's headers.
//! 2. If that does not yield a length / range capability, fall back to a
//!    `GET Range: bytes=0-0` and inspect the status:
//!    * `206` + `Content-Range: bytes 0-0/N` → ranges supported, total = N
//!    * `200` + `Content-Length` → ranges not supported, single stream
//!    * `416` + `Content-Range: bytes */N` → empty/exhausted range; N is total
//! 3. Filename priority: `Content-Disposition` `filename*` → `filename` →
//!    final URL's last path segment → `download.bin`.

use reqwest::header::{
    HeaderMap, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    ETAG, LAST_MODIFIED, RANGE,
};
use reqwest::{Client, StatusCode};

use crate::client::validate_url;
use crate::filename;
use crate::{DownloadError, DownloadRequest, RemoteMeta};

/// Parsed `Content-Range` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentRange {
    pub start: u64,
    pub end: u64,
    /// `None` for `bytes 0-0/*`.
    pub total: Option<u64>,
}

/// Parse `Content-Range: bytes 0-1023/4096` (or `bytes */4096`).
pub(crate) fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    if let Some(total) = rest.strip_prefix("*/") {
        return Some(ContentRange {
            start: 0,
            end: 0,
            total: Some(total.trim().parse().ok()?),
        });
    }
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some(ContentRange {
        start: start.trim().parse().ok()?,
        end: end.trim().parse().ok()?,
        total: if total.trim() == "*" {
            None
        } else {
            Some(total.trim().parse().ok()?)
        },
    })
}

#[derive(Debug, Clone, Default)]
struct HeaderSnapshot {
    content_length: Option<u64>,
    accept_ranges: Option<bool>,
    content_type: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    content_disposition: Option<String>,
    content_range: Option<ContentRange>,
}

impl HeaderSnapshot {
    fn capture(headers: &HeaderMap) -> Self {
        let content_length = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());
        let accept_ranges = headers
            .get(ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().eq_ignore_ascii_case("bytes"));
        let content_range = headers
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
        Self {
            content_length,
            accept_ranges,
            content_type: header_string(headers, CONTENT_TYPE),
            etag: header_string(headers, ETAG),
            last_modified: header_string(headers, LAST_MODIFIED),
            content_disposition: header_string(headers, CONTENT_DISPOSITION),
            content_range,
        }
    }

    /// Prefer fields present in `self`, fill gaps from `fallback`.
    fn merge(fallback: &Self, primary: &Self) -> Self {
        Self {
            content_length: primary.content_length.or(fallback.content_length),
            accept_ranges: primary.accept_ranges.or(fallback.accept_ranges),
            content_type: primary
                .content_type
                .clone()
                .or_else(|| fallback.content_type.clone()),
            etag: primary.etag.clone().or_else(|| fallback.etag.clone()),
            last_modified: primary
                .last_modified
                .clone()
                .or_else(|| fallback.last_modified.clone()),
            content_disposition: primary
                .content_disposition
                .clone()
                .or_else(|| fallback.content_disposition.clone()),
            content_range: primary.content_range.or(fallback.content_range),
        }
    }
}

fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Probe the remote resource using a caller-supplied client.
pub(crate) async fn probe_with(
    client: &Client,
    request: &DownloadRequest,
) -> Result<RemoteMeta, DownloadError> {
    let url = validate_url(&request.url)?;

    let head = client
        .head(url.clone())
        .send()
        .await
        .map_err(|error| DownloadError::Http(error.to_string()))?;
    let head_status = head.status();
    let head_final_url = head.url().to_string();
    let head_version = version_string(head.version());
    // NOTE: `Response::content_length()` is a *body* size hint (Some(0) for a
    // HEAD response); only the actual header is meaningful here.
    let snapshot = HeaderSnapshot::capture(head.headers());
    drop(head);

    if head_status.is_success() {
        if snapshot.accept_ranges == Some(false) {
            // Header says ranges are not supported; single stream it is.
            let final_url = head_final_url;
            return Ok(build_meta(
                final_url,
                snapshot.content_length,
                false,
                snapshot,
                head_version,
            ));
        }
        if snapshot.accept_ranges == Some(true) && snapshot.content_length.is_some() {
            let final_url = head_final_url;
            return Ok(build_meta(
                final_url,
                snapshot.content_length,
                true,
                snapshot,
                head_version,
            ));
        }
        // Missing length or missing/ambiguous Accept-Ranges: verify with a
        // one-byte ranged GET.
        return ranged_fallback(client, &url, &snapshot).await;
    }

    if matches!(
        head_status,
        StatusCode::FORBIDDEN | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
    ) {
        return ranged_fallback(client, &url, &snapshot).await;
    }

    Err(DownloadError::Status {
        status: head_status.as_u16(),
        url: head_final_url,
    })
}

async fn ranged_fallback(
    client: &Client,
    url: &url::Url,
    head: &HeaderSnapshot,
) -> Result<RemoteMeta, DownloadError> {
    let response = client
        .get(url.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|error| DownloadError::Http(error.to_string()))?;

    let status = response.status();
    let final_url = response.url().to_string();
    let fallback_version = version_string(response.version());
    // Only the header matters: for a 206 the body size hint is the range
    // length, not the remote total.
    let snapshot = HeaderSnapshot::capture(response.headers());
    // The body is irrelevant (at most one byte); drop the response so the
    // connection can be reset/closed.
    drop(response);

    let merged = HeaderSnapshot::merge(head, &snapshot);
    match status {
        StatusCode::PARTIAL_CONTENT => {
            let total = merged
                .content_range
                .and_then(|range| range.total)
                .or(merged.content_length);
            Ok(build_meta(final_url, total, true, merged, fallback_version))
        }
        StatusCode::OK => Ok(build_meta(
            final_url,
            merged.content_length,
            false,
            merged,
            fallback_version,
        )),
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let total = merged.content_range.and_then(|range| range.total);
            Ok(build_meta(
                final_url,
                total,
                false,
                merged,
                fallback_version,
            ))
        }
        other => Err(DownloadError::Status {
            status: other.as_u16(),
            url: final_url,
        }),
    }
}

fn build_meta(
    final_url: String,
    total_bytes: Option<u64>,
    supports_ranges: bool,
    headers: HeaderSnapshot,
    http_version: Option<String>,
) -> RemoteMeta {
    let file_name = filename::derive(headers.content_disposition.as_deref(), &final_url);
    RemoteMeta {
        final_url,
        file_name,
        total_bytes,
        supports_ranges,
        content_type: headers.content_type,
        etag: headers.etag,
        last_modified: headers.last_modified,
        http_version,
    }
}

/// Render a HTTP version as `HTTP/1.1` / `HTTP/2.0`.
pub(crate) fn version_string(version: reqwest::Version) -> Option<String> {
    Some(format!("{version:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 0-0/1234"),
            Some(ContentRange {
                start: 0,
                end: 0,
                total: Some(1234)
            })
        );
        assert_eq!(
            parse_content_range("bytes 100-199/200"),
            Some(ContentRange {
                start: 100,
                end: 199,
                total: Some(200)
            })
        );
        assert_eq!(
            parse_content_range("bytes 5-9/*"),
            Some(ContentRange {
                start: 5,
                end: 9,
                total: None
            })
        );
        assert_eq!(
            parse_content_range("bytes */0"),
            Some(ContentRange {
                start: 0,
                end: 0,
                total: Some(0)
            })
        );
        assert_eq!(parse_content_range("nibbles 0-0/1"), None);
        assert_eq!(parse_content_range("bytes nope"), None);
    }
}
