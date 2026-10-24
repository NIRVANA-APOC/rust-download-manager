//! Link parsing: source URL → downloadable resources
//! (`docs/design-next-features.md` §3.4).
//!
//! Flow (contract):
//! 1. normalize the URL and serve a fresh cache entry when available;
//! 2. fetch the URL (HEAD, then GET on failure) with caller-supplied
//!    credentials — non-page content types short-circuit to `direct_file`;
//! 3. for HTML: extract candidates, ask the LLM when configured;
//! 4. on any fetch/LLM failure fall back to local heuristics;
//! 5. write the result back into the in-memory [`ParseCache`].
//!
//! [`parse_url`] never returns `Err`. Credentials (cookie/referer/UA) are only
//! used for the HTTP request and are never stored in the report or the cache.

use std::collections::HashSet;
use std::time::Duration;

use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH,
    CONTENT_TYPE, COOKIE, REFERER, USER_AGENT,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::classify;
use crate::client::{AiClient, AiError};
use crate::config::AiConfig;
use crate::html;
use crate::store::ParseCache;
use crate::{extract_json_object, now_ms, truncate_chars};

/// Default cache freshness: 7 days.
pub const DEFAULT_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Maximum number of user-facing notes per report.
pub const MAX_NOTES: usize = 8;
/// Default maximum resources per report.
pub const DEFAULT_MAX_RESOURCES: usize = 40;
/// Maximum number of candidates embedded in an LLM prompt.
const MAX_PROMPT_CANDIDATES: usize = 80;
/// Hard cap on the fetched response body (pages are scanned, never stored).
const MAX_FETCH_BYTES: usize = 512 * 1024;
/// Maximum number of generic (`other`) heuristic resources.
const MAX_OTHER_RESOURCES: usize = 10;
/// Cache lifetime for reports that are degraded but may clear on their own.
/// Kept short so a transient failure cannot hide a site for the full success TTL.
pub const DEGRADED_MAX_AGE_MS: u64 = 10 * 60 * 1000;

/// Failure code for an unparsable / non-http(s) input URL.
pub const FAILURE_INVALID_URL: &str = "invalid_url";
/// Failure code for a 404/410 response.
pub const FAILURE_NOT_FOUND: &str = "not_found";
/// Failure code for a 403 response.
pub const FAILURE_FORBIDDEN: &str = "forbidden";
/// Failure code for a 401 response (or a redirect to a login page).
pub const FAILURE_LOGIN_REQUIRED: &str = "login_required";
/// Failure code for a 429 response.
pub const FAILURE_RATE_LIMITED: &str = "rate_limited";
/// Failure code for a 5xx response.
pub const FAILURE_SERVER_ERROR: &str = "server_error";
/// Failure code for a request timeout.
pub const FAILURE_TIMEOUT: &str = "timeout";
/// Failure code for a connection-level error (DNS, TLS, reset, …).
pub const FAILURE_NETWORK_ERROR: &str = "network_error";
/// Catch-all fetch failure.
pub const FAILURE_FETCH_FAILED: &str = "fetch_failed";
/// The page was fetched but the AI call failed / returned nothing usable, so the
/// resources (if any) are local heuristics rather than a validated pick.
pub const FAILURE_AI_UNAVAILABLE: &str = "ai_unavailable";
/// No downloadable resource was found.
pub const FAILURE_NO_RESOURCES: &str = "no_resources";

/// How long a report with `failure_kind` may stay cached.
///
/// `None` means "do not cache at all": transient failures must not occupy the
/// cache for [`DEFAULT_MAX_AGE_MS`], otherwise one bad minute hides the site for
/// a week and the next attempt never even hits the network.
fn cache_ttl_ms(failure_kind: Option<&str>) -> Option<u64> {
    match failure_kind {
        None => Some(DEFAULT_MAX_AGE_MS),
        Some(
            FAILURE_NOT_FOUND
            | FAILURE_FORBIDDEN
            | FAILURE_LOGIN_REQUIRED
            | FAILURE_RATE_LIMITED
            | FAILURE_AI_UNAVAILABLE,
        ) => Some(DEGRADED_MAX_AGE_MS),
        Some(_) => None,
    }
}

/// A link the caller wants to resolve.
///
/// `Debug` is hand-written so `cookie` is redacted: request structs can end up
/// in debug logs, and credentials must never leak that way.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ParseRequest {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Used only to fetch the page; **never** persisted or logged.
    #[serde(default)]
    pub cookie: Option<String>,
    /// Extra context from the extension/user.
    #[serde(default)]
    pub hint: Option<String>,
}

impl std::fmt::Debug for ParseRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParseRequest")
            .field("url", &self.url)
            .field("title", &self.title)
            .field("referer", &self.referer)
            .field("user_agent", &self.user_agent)
            .field("cookie", &self.cookie.as_ref().map(|_| "<redacted>"))
            .field("hint", &self.hint)
            .finish()
    }
}

/// One downloadable resource.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedResource {
    pub url: String,
    pub name: String,
    /// `video`/`audio`/`image`/`archive`/`document`/`program`/`other`.
    pub kind: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Result of resolving one link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseReport {
    pub source_url: String,
    /// `direct_file` / `html_page` / `unknown`.
    pub source_kind: String,
    #[serde(default)]
    pub title: Option<String>,
    pub resources: Vec<ParsedResource>,
    /// Short user-facing notes (≤ 8).
    pub notes: Vec<String>,
    pub ai_used: bool,
    #[serde(default)]
    pub model: Option<String>,
    /// Token usage of the model call(s) behind this report. Zero when no model
    /// was called; `estimated = true` when the provider did not report usage.
    #[serde(default)]
    pub usage: crate::usage::TokenUsage,
    pub created_ms: u64,
    #[serde(default)]
    pub cache_hit: bool,
    /// Why this report is degraded, when it is.
    ///
    /// `None` when the parse produced usable resources without a known failure.
    /// Values are the exported [`FAILURE_*`] codes. A degraded report can still
    /// carry heuristic resources, so clients must treat this as diagnostics
    /// rather than as a hard failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
}

/// Knobs for [`parse_url`].
#[derive(Debug, Clone)]
pub struct ParseOptions {
    pub use_cache: bool,
    pub max_age_ms: u64,
    pub max_resources: usize,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            use_cache: true,
            max_age_ms: DEFAULT_MAX_AGE_MS,
            max_resources: DEFAULT_MAX_RESOURCES,
        }
    }
}

/// Normalize to an absolute `http(s)` URL without fragment; `None` for
/// anything else (relative URLs, `javascript:`, `data:`, …).
pub fn normalize_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = Url::parse(trimmed).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let mut parsed = parsed;
    parsed.set_fragment(None);
    Some(parsed.to_string())
}

/// Resolve `request.url` into resources.
///
/// Never returns an error: fetch and LLM failures degrade to heuristic results
/// with explanatory `notes` and a machine-readable [`ParseReport::failure_kind`].
/// Degraded reports are only cached briefly (see [`cache_ttl_ms`]); transient
/// failures are not cached at all.
pub async fn parse_url(
    request: &ParseRequest,
    config: &AiConfig,
    cache: &mut ParseCache,
    options: &ParseOptions,
) -> ParseReport {
    let created_ms = now_ms();
    let Some(source_url) = normalize_url(&request.url) else {
        return ParseReport {
            source_url: request.url.trim().to_owned(),
            source_kind: "unknown".to_owned(),
            title: request.title.clone(),
            resources: Vec::new(),
            notes: vec!["链接不是有效的 http(s) 地址".to_owned()],
            ai_used: false,
            model: None,
            usage: crate::usage::TokenUsage::default(),
            created_ms,
            cache_hit: false,
            failure_kind: Some(FAILURE_INVALID_URL.to_owned()),
        };
    };

    if options.use_cache {
        if let Some(cached) = cache.get(&source_url) {
            // A degraded entry (fetch/AI failure) must not be replayed for the
            // full week-long success TTL.
            let fresh = match cache_ttl_ms(cached.failure_kind.as_deref()) {
                Some(ttl) => {
                    created_ms.saturating_sub(cached.created_ms) <= ttl.min(options.max_age_ms)
                }
                None => false,
            };
            if fresh {
                let mut resources = cached.resources.clone();
                resources.truncate(options.max_resources);
                tracing::debug!(url = %source_url, "rdm-ai parse: serving a fresh cache entry");
                return ParseReport {
                    source_url,
                    source_kind: cached.source_kind.clone(),
                    title: cached.title.clone(),
                    resources,
                    notes: cached.notes.clone(),
                    ai_used: cached.model.is_some(),
                    model: cached.model.clone(),
                    usage: crate::usage::TokenUsage::default(),
                    created_ms,
                    cache_hit: true,
                    failure_kind: cached.failure_kind.clone(),
                };
            }
        }
    }

    let max_resources = options.max_resources;
    let mut notes = Vec::new();
    let mut title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let mut resources: Vec<ParsedResource> = Vec::new();
    let mut candidates: Vec<String> = Vec::new();
    let mut source_kind = "unknown".to_owned();
    let mut ai_used = false;
    let mut model = None;
    let mut failure_kind: Option<&'static str> = None;
    // Token accounting for every model call made by this parse (including calls
    // that later failed to produce a usable reply: those were still billed).
    let mut usage = crate::usage::TokenUsage::default();

    match fetch(&source_url, request, config, &mut notes).await {
        FetchOutcome::DirectFile {
            url,
            name,
            size_bytes,
            content_type,
        } => {
            source_kind = "direct_file".to_owned();
            let name = name
                .and_then(|name| clean_file_name(&name))
                .or_else(|| file_name_from_url(&url))
                .unwrap_or_else(|| "download.bin".to_owned());
            let kind = classify::resource_kind(&name, content_type.as_deref()).to_owned();
            resources.push(ParsedResource {
                url: normalize_url(&url).unwrap_or(url),
                name,
                kind,
                size_bytes,
                content_type,
                note: None,
            });
            notes.push("直链文件，无需 AI 解析".to_owned());
        }
        FetchOutcome::Page { url, html: page } => {
            source_kind = "html_page".to_owned();
            let page_base = Url::parse(&url).or_else(|_| Url::parse(&source_url)).ok();
            let scan = match &page_base {
                Some(base) => html::scan_page(&page, base),
                None => html::ScannedPage::default(),
            };
            if title.is_none() {
                title = scan.title.clone();
            }
            let page_text = scan.text;
            let mut links = prioritized_candidates(&source_url, scan.links);
            links = with_source_candidate(&source_url, links);
            candidates = links;
            if candidates.is_empty() {
                notes.push("页面中未发现候选下载链接".to_owned());
            }

            if config.is_configured() {
                let hints = cache.host_hints(&source_url);
                let (outcome, call_usage) = ask_llm(
                    config,
                    request,
                    &source_url,
                    &candidates,
                    &page_text,
                    title.as_deref(),
                    &hints,
                    max_resources,
                )
                .await;
                usage.merge(call_usage);
                match outcome {
                    Ok(outcome) if !(outcome.resources.is_empty() && outcome.notes.is_empty()) => {
                        resources = outcome.resources;
                        notes.extend(outcome.notes);
                        ai_used = true;
                        model = Some(config.model.clone());
                    }
                    Ok(_) => {
                        // The model answered but produced nothing usable: the
                        // resources below are local heuristics, not a validated pick.
                        failure_kind = Some(FAILURE_AI_UNAVAILABLE);
                        notes.push("AI 未返回有效结果，以下为本地启发式结果".to_owned());
                        resources = heuristic_resources(&candidates, max_resources);
                    }
                    Err(error) => {
                        failure_kind = Some(FAILURE_AI_UNAVAILABLE);
                        tracing::debug!(
                            error = %error,
                            "rdm-ai parse: AI call failed, using local heuristics"
                        );
                        notes.push(format!(
                            "AI 调用失败（{}），以下为本地启发式结果",
                            summarize_ai_error(&error)
                        ));
                        resources = heuristic_resources(&candidates, max_resources);
                    }
                }
            } else {
                notes.push("未配置 AI，以下为本地启发式结果".to_owned());
                resources = heuristic_resources(&candidates, max_resources);
            }
            if resources.is_empty() {
                notes.push("未发现可下载资源".to_owned());
            }
        }
        FetchOutcome::Failed { reason, kind } => {
            failure_kind = Some(kind);
            notes.push(format!("抓取页面失败（{reason}），以下为本地启发式结果"));
            let mut guessed = Vec::new();
            let source_name = file_name_from_url(&source_url).unwrap_or_default();
            if classify::resource_kind(&source_name, None) != "other" {
                guessed.push(source_url.clone());
            }
            candidates = guessed.clone();
            resources = heuristic_resources(&guessed, max_resources);
            if resources.is_empty() {
                notes.push("未发现可下载资源".to_owned());
            }
        }
    }

    // Normalize display names once all paths (AI, heuristic, direct file) have
    // produced resources: a resolved stream without a suffix must not become a
    // document-looking task name. Uses verified Content-Type first, then the
    // final URL path, then the resource kind (see `ensure_file_extension`).
    for resource in &mut resources {
        resource.name = classify::ensure_file_extension(
            &resource.name,
            resource.content_type.as_deref(),
            Some(&resource.url),
            Some(&resource.kind),
        );
    }
    resources.truncate(max_resources);
    normalize_notes(&mut notes);
    if failure_kind.is_none() && resources.is_empty() {
        failure_kind = Some(FAILURE_NO_RESOURCES);
    }

    let report = ParseReport {
        source_url,
        source_kind,
        title,
        resources,
        notes,
        ai_used,
        model,
        usage,
        created_ms,
        cache_hit: false,
        failure_kind: failure_kind.map(str::to_owned),
    };
    // Transient failures (`cache_ttl_ms` = None) must not be cached: a single
    // bad minute would otherwise hide the site for the full success TTL.
    if cache_ttl_ms(failure_kind).is_some() {
        cache.put(&report, &instructions_for(request, &candidates, ai_used));
    }
    report
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

enum FetchOutcome {
    DirectFile {
        url: String,
        name: Option<String>,
        size_bytes: Option<u64>,
        content_type: Option<String>,
    },
    Page {
        url: String,
        html: String,
    },
    Failed {
        reason: String,
        kind: &'static str,
    },
}

async fn fetch(
    source_url: &str,
    request: &ParseRequest,
    config: &AiConfig,
    notes: &mut Vec<String>,
) -> FetchOutcome {
    let timeout_secs = config.timeout_secs.clamp(1, 30);
    let client = match build_fetch_client(request, timeout_secs, notes) {
        Ok(client) => client,
        Err(reason) => {
            return FetchOutcome::Failed {
                reason,
                kind: FAILURE_FETCH_FAILED,
            }
        }
    };
    let url = match Url::parse(source_url) {
        Ok(url) => url,
        Err(error) => {
            return FetchOutcome::Failed {
                reason: format!("URL 无效: {error}"),
                kind: FAILURE_INVALID_URL,
            }
        }
    };

    // HEAD is cheap and often enough to classify a direct file. Any failure
    // (405, 403, missing content type, network error) falls through to GET.
    if let Ok(response) = client.head(url.clone()).send().await {
        let status = response.status();
        let final_url = response.url().to_string();
        if status.is_success() {
            if let Some((name, size_bytes, content_type)) =
                direct_file_meta(&final_url, response.headers())
            {
                return FetchOutcome::DirectFile {
                    url: final_url,
                    name,
                    size_bytes,
                    content_type,
                };
            }
        }
    }

    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(error) => {
            return FetchOutcome::Failed {
                reason: describe_fetch_error(&error),
                kind: transport_failure_kind(&error),
            }
        }
    };
    let status = response.status();
    let final_url = response.url().to_string();
    let headers = response.headers().clone();
    if !status.is_success() {
        return FetchOutcome::Failed {
            reason: format!("HTTP {}", status.as_u16()),
            kind: http_failure_kind(status.as_u16()),
        };
    }
    if let Some((name, size_bytes, content_type)) = direct_file_meta(&final_url, &headers) {
        return FetchOutcome::DirectFile {
            url: final_url,
            name,
            size_bytes,
            content_type,
        };
    }

    let content_type = header_value(&headers, CONTENT_TYPE);
    let body = match read_body_capped(response, MAX_FETCH_BYTES).await {
        Ok(body) => body,
        Err((reason, kind)) => return FetchOutcome::Failed { reason, kind },
    };
    let text = String::from_utf8_lossy(&body).into_owned();
    if is_page_content_type(content_type.as_deref()) || looks_like_html(&text) {
        FetchOutcome::Page {
            url: final_url,
            html: text,
        }
    } else {
        FetchOutcome::DirectFile {
            url: final_url,
            name: None,
            size_bytes: None,
            content_type,
        }
    }
}

fn build_fetch_client(
    request: &ParseRequest,
    timeout_secs: u64,
    notes: &mut Vec<String>,
) -> Result<reqwest::Client, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    if let Some(value) = sane_header(request.user_agent.as_deref()) {
        match HeaderValue::from_str(value) {
            Ok(value) => {
                headers.insert(USER_AGENT, value);
            }
            Err(_) => notes.push("User-Agent 头非法，已忽略".to_owned()),
        }
    }
    if let Some(value) = sane_header(request.referer.as_deref()) {
        match HeaderValue::from_str(value) {
            Ok(value) => {
                headers.insert(REFERER, value);
            }
            Err(_) => notes.push("Referer 头非法，已忽略".to_owned()),
        }
    }
    if let Some(value) = sane_header(request.cookie.as_deref()) {
        match HeaderValue::from_str(value) {
            Ok(value) => {
                headers.insert(COOKIE, value);
            }
            Err(_) => notes.push("Cookie 头非法，已忽略".to_owned()),
        }
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(timeout_secs.min(10)))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|error| format!("HTTP 客户端初始化失败: {error}"))
}

fn sane_header(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

async fn read_body_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, (String, &'static str)> {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = cap.saturating_sub(body.len());
                if room == 0 {
                    break;
                }
                let take = chunk.len().min(room);
                body.extend_from_slice(&chunk[..take]);
                if body.len() >= cap {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                return Err((describe_fetch_error(&error), transport_failure_kind(&error)))
            }
        }
    }
    Ok(body)
}

fn describe_fetch_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "请求超时".to_owned()
    } else if error.is_connect() {
        "连接失败".to_owned()
    } else {
        truncate_chars(&error.to_string(), 200)
    }
}

/// Map an HTTP status to a stable [`ParseReport::failure_kind`] code.
fn http_failure_kind(status: u16) -> &'static str {
    match status {
        401 => FAILURE_LOGIN_REQUIRED,
        403 => FAILURE_FORBIDDEN,
        404 | 410 => FAILURE_NOT_FOUND,
        429 => FAILURE_RATE_LIMITED,
        status if status >= 500 => FAILURE_SERVER_ERROR,
        _ => FAILURE_FETCH_FAILED,
    }
}

/// Map a transport-level failure to a stable [`ParseReport::failure_kind`] code.
fn transport_failure_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        FAILURE_TIMEOUT
    } else if error.is_connect() {
        FAILURE_NETWORK_ERROR
    } else {
        FAILURE_FETCH_FAILED
    }
}

/// `(name, size, content_type)` when the response headers describe a file
/// rather than a page.
fn direct_file_meta(
    final_url: &str,
    headers: &HeaderMap,
) -> Option<(Option<String>, Option<u64>, Option<String>)> {
    let content_type = header_value(headers, CONTENT_TYPE);
    let disposition = header_value(headers, CONTENT_DISPOSITION);
    let filename = disposition
        .as_deref()
        .and_then(parse_content_disposition_filename);
    let page_like = content_type
        .as_deref()
        .map(|content_type| is_page_content_type(Some(content_type)))
        .unwrap_or(false);
    if filename.is_none() && !(content_type.is_some() && !page_like) {
        return None;
    }
    let size_bytes = header_value(headers, CONTENT_LENGTH).and_then(|value| value.parse().ok());
    let name = filename.or_else(|| file_name_from_url(final_url));
    Some((name, size_bytes, content_type))
}

fn header_value(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn is_page_content_type(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let normalized = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    normalized.contains("html") || normalized.contains("xml") || normalized.starts_with("text/")
}

fn looks_like_html(body: &str) -> bool {
    let trimmed = body.trim_start();
    if trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<!DOCTYPE")
        || trimmed.starts_with("<html")
        || trimmed.starts_with("<?xml")
    {
        return true;
    }
    let sample: String = body.chars().take(1024).collect();
    sample.contains("<a ")
        || sample.contains("<a>")
        || sample.contains("<head")
        || sample.contains("<body")
        || sample.contains("<div")
}

// ---------------------------------------------------------------------------
// Content-Disposition / file names
// ---------------------------------------------------------------------------

fn parse_content_disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';') {
        let Some((key, raw_value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let raw_value = raw_value.trim();
        if key == "filename*" {
            if let Some(name) = decode_extended_filename(raw_value) {
                return Some(name);
            }
        } else if key == "filename" && plain.is_none() {
            plain = clean_file_name(&decode_disposition_value(raw_value));
        }
    }
    plain
}

fn decode_extended_filename(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"');
    let (charset, encoded) = match value.split_once("''") {
        Some((charset, encoded)) => (charset.trim().to_ascii_lowercase(), encoded),
        None => ("utf-8".to_owned(), value),
    };
    let bytes = percent_decode_bytes(encoded);
    let decoded = if charset.contains("8859-1")
        || charset.contains("latin1")
        || charset.contains("windows-1252")
    {
        bytes.iter().map(|&byte| byte as char).collect::<String>()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    clean_file_name(&decoded)
}

fn decode_disposition_value(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value);
    value.replace("\\\"", "\"").replace("\\\\", "\\")
}

fn clean_file_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let name: String = name.chars().filter(|ch| !ch.is_control()).collect();
    let name = name.trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
    if name.is_empty() {
        return None;
    }
    Some(truncate_chars(name, 120))
}

fn file_name_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let segment = parsed
        .path_segments()?
        .rfind(|segment| !segment.is_empty())?;
    clean_file_name(&percent_decode(segment))
}

fn percent_decode(input: &str) -> String {
    String::from_utf8_lossy(&percent_decode_bytes(input)).into_owned()
}

fn percent_decode_bytes(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Candidates + heuristics
// ---------------------------------------------------------------------------

fn prioritized_candidates(source_url: &str, links: Vec<String>) -> Vec<String> {
    let source_host = host_of(source_url);
    let mut same_site = Vec::new();
    let mut rest = Vec::new();
    for link in links {
        match (&source_host, host_of(&link)) {
            (Some(source), Some(host)) if *source == host => same_site.push(link),
            _ => rest.push(link),
        }
    }
    same_site.extend(rest);
    same_site.truncate(html::MAX_CANDIDATES);
    same_site
}

fn with_source_candidate(source_url: &str, mut links: Vec<String>) -> Vec<String> {
    let source_name = file_name_from_url(source_url).unwrap_or_default();
    if classify::resource_kind(&source_name, None) != "other"
        && !links.iter().any(|link| link == source_url)
    {
        links.insert(0, source_url.to_owned());
        links.truncate(html::MAX_CANDIDATES);
    }
    links
}

fn heuristic_resources(candidates: &[String], max_resources: usize) -> Vec<ParsedResource> {
    let mut known = Vec::new();
    let mut other = Vec::new();
    for url in candidates {
        if known.len() >= max_resources {
            break;
        }
        let name = file_name_from_url(url).unwrap_or_else(|| "download.bin".to_owned());
        let kind = classify::resource_kind(&name, None);
        let resource = ParsedResource {
            url: url.clone(),
            name,
            kind: kind.to_owned(),
            size_bytes: None,
            content_type: None,
            note: None,
        };
        if kind == "other" {
            if other.len() < MAX_OTHER_RESOURCES {
                other.push(resource);
            }
        } else {
            known.push(resource);
        }
    }
    let remaining = max_resources.saturating_sub(known.len());
    known.extend(other.into_iter().take(remaining));
    known.truncate(max_resources);
    known
}

fn normalize_notes(notes: &mut Vec<String>) {
    let mut seen = HashSet::new();
    let mut cleaned = Vec::new();
    for note in notes.drain(..) {
        let note = truncate_chars(note.trim(), 240);
        if note.is_empty() || !seen.insert(note.clone()) {
            continue;
        }
        cleaned.push(note);
        if cleaned.len() >= MAX_NOTES {
            break;
        }
    }
    *notes = cleaned;
}

fn host_of(url: &str) -> Option<String> {
    Url::parse(url.trim())
        .ok()
        .and_then(|parsed| parsed.host_str().map(|host| host.to_ascii_lowercase()))
}

fn instructions_for(request: &ParseRequest, candidates: &[String], ai_used: bool) -> String {
    let mut parts = vec![
        format!("ai={ai_used}"),
        format!("candidates={}", candidates.len()),
    ];
    if let Some(title) = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        parts.push(format!("title={}", truncate_chars(title, 80)));
    }
    if let Some(hint) = request
        .hint
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        parts.push(format!("hint={}", truncate_chars(hint, 120)));
    }
    // Deliberately never includes cookie / referer / user-agent.
    truncate_chars(&parts.join("; "), 400)
}

// ---------------------------------------------------------------------------
// LLM
// ---------------------------------------------------------------------------

struct LlmOutcome {
    resources: Vec<ParsedResource>,
    notes: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
async fn ask_llm(
    config: &AiConfig,
    request: &ParseRequest,
    source_url: &str,
    candidates: &[String],
    page_text: &str,
    title: Option<&str>,
    host_hints: &[String],
    max_resources: usize,
) -> (Result<LlmOutcome, AiError>, crate::usage::TokenUsage) {
    let client = match AiClient::new(crate::client::bounded_config(config)) {
        Ok(client) => client,
        Err(error) => return (Err(error), crate::usage::TokenUsage::default()),
    };
    let system = llm_system_prompt(max_resources);
    let user = llm_user_prompt(
        request, source_url, candidates, page_text, title, host_hints,
    );
    let reply = client.chat(&system, &user).await;
    // Collect usage even when the reply could not be parsed: the provider was
    // already billed for the call.
    let usage = client.take_usage();
    let outcome = reply.and_then(|reply| parse_llm_reply(&reply, max_resources));
    (outcome, usage)
}

fn llm_system_prompt(max_resources: usize) -> String {
    format!(
        "你是 RDM 下载管理器的链接解析助手。根据来源页面、候选链接和页面摘要，找出用户最可能想下载的资源。\
只输出一个 JSON 对象（不要 Markdown、不要解释）：\
{{\"resources\":[{{\"url\":\"https://...\",\"name\":\"文件名.mp4\",\"kind\":\"video|audio|image|archive|document|program|other\",\"size_bytes\":123,\"content_type\":\"video/mp4\",\"note\":\"简短说明\"}}],\"notes\":[\"面向用户的简短解析事项\"]}}。\
规则：链接必须是 http(s)；name 必须包含真实文件扩展名（如 .mp4/.m4a/.zip；链接无后缀时按 content_type 或 kind 推断，不要只写标题）；最多 {max_resources} 个资源；notes 最多 8 条且每条不超过 80 字；不确定的可选字段直接省略；不要编造不存在的链接。"
    )
}

fn llm_user_prompt(
    request: &ParseRequest,
    source_url: &str,
    candidates: &[String],
    page_text: &str,
    title: Option<&str>,
    host_hints: &[String],
) -> String {
    let mut prompt = String::new();
    prompt.push_str("来源页面: ");
    prompt.push_str(source_url);
    prompt.push('\n');
    if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
        prompt.push_str("页面标题: ");
        prompt.push_str(&truncate_chars(title, 300));
        prompt.push('\n');
    }
    if let Some(hint) = request
        .hint
        .as_deref()
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
    {
        prompt.push_str("用户提示: ");
        prompt.push_str(&truncate_chars(hint, 500));
        prompt.push('\n');
    }
    if let Some(referer) = request
        .referer
        .as_deref()
        .map(str::trim)
        .filter(|referer| !referer.is_empty())
    {
        prompt.push_str("来源页(Referer): ");
        prompt.push_str(&truncate_chars(referer, 500));
        prompt.push('\n');
    }

    if candidates.is_empty() {
        prompt.push_str("候选链接: （无）\n");
    } else {
        prompt.push_str(&format!(
            "候选链接（按同站优先排序，共 {} 条，最多列出 {} 条）:\n",
            candidates.len(),
            candidates.len().min(MAX_PROMPT_CANDIDATES)
        ));
        for (index, candidate) in candidates.iter().take(MAX_PROMPT_CANDIDATES).enumerate() {
            prompt.push_str(&format!(
                "{}. {}\n",
                index + 1,
                truncate_chars(candidate, 500)
            ));
        }
    }
    if page_text.trim().is_empty() {
        prompt.push_str("页面摘要: （无）\n");
    } else {
        prompt.push_str("页面摘要:\n");
        prompt.push_str(page_text.trim());
        prompt.push('\n');
    }
    if !host_hints.is_empty() {
        prompt.push_str("同站历史解析事项:\n");
        for hint in host_hints {
            prompt.push_str("- ");
            prompt.push_str(&truncate_chars(hint, 200));
            prompt.push('\n');
        }
    }
    prompt.push_str("请只输出 JSON。");
    prompt
}

fn parse_llm_reply(reply: &str, max_resources: usize) -> Result<LlmOutcome, AiError> {
    let json = extract_json_object(reply)
        .ok_or_else(|| AiError::Json("no JSON object found in the AI reply".to_owned()))?;
    let value: Value = serde_json::from_str(json)
        .map_err(|error| AiError::Json(format!("invalid AI JSON: {error}")))?;

    let mut resources = Vec::new();
    let mut seen = HashSet::new();
    if let Some(items) = value.get("resources").and_then(Value::as_array) {
        for item in items {
            if resources.len() >= max_resources {
                break;
            }
            if let Some(resource) = parse_llm_resource(item) {
                if seen.insert(resource.url.clone()) {
                    resources.push(resource);
                }
            }
        }
    }

    let mut notes = Vec::new();
    if let Some(items) = value.get("notes").and_then(Value::as_array) {
        for item in items {
            if notes.len() >= MAX_NOTES {
                break;
            }
            if let Some(note) = item.as_str() {
                let note = truncate_chars(note.trim(), 240);
                if !note.is_empty() {
                    notes.push(note);
                }
            }
        }
    }

    Ok(LlmOutcome { resources, notes })
}

fn parse_llm_resource(item: &Value) -> Option<ParsedResource> {
    let url = normalize_url(item.get("url")?.as_str()?)?;
    let content_type = item
        .get("content_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_chars(value, 120));
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .and_then(clean_file_name)
        .or_else(|| file_name_from_url(&url))
        .unwrap_or_else(|| "download.bin".to_owned());
    let kind = item
        .get("kind")
        .and_then(Value::as_str)
        .map(|kind| kind.trim().to_ascii_lowercase())
        .filter(|kind| is_known_kind(kind))
        .unwrap_or_else(|| classify::resource_kind(&name, content_type.as_deref()).to_owned());
    let size_bytes = item.get("size_bytes").and_then(Value::as_u64);
    let note = item
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(|note| truncate_chars(note, 200));
    // Model-provided titles often omit the suffix (e.g. a video page title).
    // Normalize here too so direct `parse_llm_reply` callers and the legacy
    // parse path share the same contract.
    let name =
        classify::ensure_file_extension(&name, content_type.as_deref(), Some(&url), Some(&kind));
    Some(ParsedResource {
        url,
        name,
        kind,
        size_bytes,
        content_type,
        note,
    })
}

fn is_known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "video" | "audio" | "image" | "archive" | "document" | "program" | "other"
    )
}

fn summarize_ai_error(error: &AiError) -> String {
    truncate_chars(&error.to_string(), 160)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_debug_redacts_cookie() {
        let request = ParseRequest {
            url: "https://example.com/page".to_owned(),
            cookie: Some("session=debug-secret".to_owned()),
            ..ParseRequest::default()
        };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("debug-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(rendered.contains("https://example.com/page"));
    }

    #[test]
    fn normalize_url_rules() {
        assert_eq!(
            normalize_url(" https://example.com/a/b?c=1#frag "),
            Some("https://example.com/a/b?c=1".to_owned())
        );
        assert_eq!(
            normalize_url("http://example.com"),
            Some("http://example.com/".to_owned())
        );
        assert_eq!(normalize_url("/relative"), None);
        assert_eq!(normalize_url("javascript:void(0)"), None);
        assert_eq!(normalize_url("ftp://example.com/x"), None);
        assert_eq!(normalize_url("   "), None);
        assert_eq!(normalize_url("not a url"), None);
    }

    #[test]
    fn llm_reply_parses_fenced_json() {
        let reply = r#"```json
{"resources":[{"url":"https://cdn.example.com/v.mp4","name":"v.mp4","kind":"video","size_bytes":9},
              {"url":"javascript:void(0)","name":"bad"},
              {"url":"https://cdn.example.com/v.mp4#t=1"}],
 "notes":["需要 Referer"]}
```"#;
        let outcome = parse_llm_reply(reply, 40).unwrap();
        assert_eq!(outcome.resources.len(), 1);
        assert_eq!(outcome.resources[0].url, "https://cdn.example.com/v.mp4");
        assert_eq!(outcome.resources[0].kind, "video");
        assert_eq!(outcome.resources[0].size_bytes, Some(9));
        assert_eq!(outcome.notes, vec!["需要 Referer".to_owned()]);
    }

    #[test]
    fn llm_reply_rejects_non_json() {
        assert!(matches!(
            parse_llm_reply("I cannot help with that", 10),
            Err(AiError::Json(_))
        ));
    }

    #[test]
    fn llm_reply_sanitizes_names_and_kinds() {
        let reply = r#"{"resources":[
            {"url":"https://example.com/x","name":"../../evil.exe","kind":"NOT-A-KIND"},
            {"url":"https://example.com/y.zip","name":"","kind":"archive"}
        ]}"#;
        let outcome = parse_llm_reply(reply, 10).unwrap();
        assert_eq!(outcome.resources[0].name, "evil.exe");
        assert_eq!(outcome.resources[0].kind, "program");
        assert_eq!(outcome.resources[1].name, "y.zip");
        assert_eq!(outcome.resources[1].kind, "archive");
    }

    #[test]
    fn llm_reply_adds_missing_extension_from_content_type_or_url() {
        let reply = r#"{"resources":[
            {"url":"https://cdn.example.com/stream?sign=1","name":"标题 (720P)","content_type":"video/mp4","kind":"video"},
            {"url":"https://cdn.example.com/42924311105-1-192.mp4?e=sig","name":"另一个标题 (1080P)","kind":"video"}
        ]}"#;
        let outcome = parse_llm_reply(reply, 10).unwrap();
        assert_eq!(outcome.resources[0].name, "标题 (720P).mp4");
        assert_eq!(outcome.resources[1].name, "另一个标题 (1080P).mp4");
    }

    #[test]
    fn llm_reply_respects_max_resources() {
        let mut items = Vec::new();
        for index in 0..20 {
            items.push(format!(
                r#"{{"url":"https://example.com/{index}.mp4","name":"{index}.mp4"}}"#
            ));
        }
        let reply = format!(r#"{{"resources":[{}]}}"#, items.join(","));
        let outcome = parse_llm_reply(&reply, 5).unwrap();
        assert_eq!(outcome.resources.len(), 5);
    }

    #[test]
    fn heuristic_prefers_known_types_and_caps_other() {
        let candidates = vec![
            "https://example.com/a.mp4".to_owned(),
            "https://example.com/b.mp3".to_owned(),
            "https://example.com/c.bin".to_owned(),
            "https://example.com/d.html".to_owned(),
        ];
        let resources = heuristic_resources(&candidates, 40);
        assert_eq!(resources[0].kind, "video");
        assert_eq!(resources[1].kind, "audio");
        // Generic candidates are kept but after real files, capped to 10 total.
        assert!(resources.len() <= 40);
        assert_eq!(resources.len(), 4);
        assert!(resources.iter().any(|resource| resource.kind == "other"));
    }

    #[test]
    fn heuristic_honors_max_resources() {
        let candidates: Vec<String> = (0..30)
            .map(|index| format!("https://example.com/{index}.mp4"))
            .collect();
        assert_eq!(heuristic_resources(&candidates, 7).len(), 7);
        assert_eq!(heuristic_resources(&candidates, 0).len(), 0);
    }

    #[test]
    fn heuristics_from_html_scan_use_relative_resolution() {
        let base = Url::parse("https://example.com/pages/index.html").unwrap();
        let page = html::scan_page(
            r#"<a href="movie.mp4">m</a><a href="/audio/song.mp3">s</a>"#,
            &base,
        );
        let candidates = with_source_candidate(
            "https://example.com/pages/index.html",
            prioritized_candidates("https://example.com/pages/index.html", page.links),
        );
        let resources = heuristic_resources(&candidates, 40);
        assert_eq!(resources.len(), 2);
        assert!(resources
            .iter()
            .any(|resource| resource.url == "https://example.com/pages/movie.mp4"));
    }

    #[test]
    fn same_site_candidates_come_first() {
        let links = vec![
            "https://cdn.other.com/a.mp4".to_owned(),
            "https://example.com/b.mp4".to_owned(),
        ];
        let sorted = prioritized_candidates("https://example.com/page", links);
        assert_eq!(sorted[0], "https://example.com/b.mp4");
    }

    #[test]
    fn content_disposition_filenames() {
        assert_eq!(
            parse_content_disposition_filename("attachment; filename=\"movie.mp4\"").as_deref(),
            Some("movie.mp4")
        );
        assert_eq!(
            parse_content_disposition_filename("inline; filename=file.zip").as_deref(),
            Some("file.zip")
        );
        assert_eq!(
            parse_content_disposition_filename(
                "attachment; filename*=UTF-8''na%C3%AFve%20file.mp4"
            )
            .as_deref(),
            Some("naïve file.mp4")
        );
        assert_eq!(
            parse_content_disposition_filename("attachment; filename*=iso-8859-1''caf%E9.bin")
                .as_deref(),
            Some("café.bin")
        );
        assert_eq!(parse_content_disposition_filename("attachment"), None);
    }

    #[test]
    fn file_names_from_urls() {
        assert_eq!(
            file_name_from_url("https://example.com/path/na%C3%AFve.mp4?token=1").as_deref(),
            Some("naïve.mp4")
        );
        assert_eq!(file_name_from_url("https://example.com/").as_deref(), None);
        assert_eq!(
            clean_file_name("../../etc/passwd"),
            Some("passwd".to_owned())
        );
        assert_eq!(clean_file_name(".."), None);
    }

    #[test]
    fn page_and_direct_content_type_detection() {
        assert!(is_page_content_type(Some("text/html; charset=utf-8")));
        assert!(is_page_content_type(Some("application/xhtml+xml")));
        assert!(is_page_content_type(Some("text/plain")));
        assert!(!is_page_content_type(Some("application/octet-stream")));
        assert!(!is_page_content_type(Some("video/mp4")));
        assert!(!is_page_content_type(None));

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("1234"));
        let meta = direct_file_meta("https://example.com/v.mp4", &headers).unwrap();
        assert_eq!(meta.0.as_deref(), Some("v.mp4"));
        assert_eq!(meta.1, Some(1234));

        let mut page_headers = HeaderMap::new();
        page_headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        assert!(direct_file_meta("https://example.com/page", &page_headers).is_none());

        // A filename in Content-Disposition is a direct file even with a text type.
        page_headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"report.pdf\""),
        );
        let meta = direct_file_meta("https://example.com/page", &page_headers).unwrap();
        assert_eq!(meta.0.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn notes_are_deduped_and_capped() {
        let mut notes: Vec<String> = (0..20).map(|index| format!("note-{index}")).collect();
        notes.push("note-0".to_owned());
        normalize_notes(&mut notes);
        assert_eq!(notes.len(), MAX_NOTES);
        assert_eq!(notes[0], "note-0");
    }

    #[test]
    fn instructions_never_include_credentials() {
        let request = ParseRequest {
            url: "https://example.com/page".to_owned(),
            cookie: Some("session=super-secret-cookie".to_owned()),
            referer: Some("https://example.com/ref".to_owned()),
            user_agent: Some("Agent/1.0".to_owned()),
            hint: Some("want the 4k version".to_owned()),
            title: Some("Example".to_owned()),
        };
        let instructions =
            instructions_for(&request, &["https://example.com/a.mp4".to_owned()], false);
        assert!(!instructions.contains("super-secret-cookie"));
        assert!(!instructions.contains("Agent/1.0"));
        assert!(instructions.contains("candidates=1"));
        assert!(instructions.contains("title=Example"));
    }
}
