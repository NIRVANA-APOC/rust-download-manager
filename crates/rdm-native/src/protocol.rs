//! Native messaging protocol: inbound extension messages, validation, and
//! outbound responses.
//!
//! Wire shapes are frozen by `docs/extension-protocol.md` §1.3. Unknown fields
//! are ignored on purpose (forward compatibility); unknown message types and
//! malformed requests produce a structured `error` response rather than a
//! silent disconnect.
//!
//! Validation is defence in depth: the extension is not trusted to send sane
//! data. URLs must be http(s), file names are reduced to a safe basename,
//! headers pass an allow-list, and cookies are rejected if they could smuggle
//! CR/LF into the download engine's HTTP stack.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::ipc_client::StatusSnapshot;
use crate::logger::Logger;
use crate::url_util;

/// Version reported in `pong` responses (the host crate version).
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MAX_FILE_NAME_BYTES: usize = 255;
pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_COOKIE_BYTES: usize = 16 * 1024;
pub const MAX_HEADER_VALUE_BYTES: usize = 4096;
pub const MAX_HEADERS: usize = 32;
pub const MAX_REFERER_BYTES: usize = 4096;
pub const MAX_USER_AGENT_BYTES: usize = 1024;
pub const MAX_PROXY_BYTES: usize = 1024;
pub const MAX_SOURCE_BYTES: usize = 32;
pub const MAX_SOURCE_PAGE_BYTES: usize = 4096;

/// AI parser limits (`docs/design-next-features.md` §5.2): a page URL is much
/// shorter than a download URL, and a page title is capped at 1 KiB. Cookie /
/// referer / user-agent reuse the existing limits above.
pub const MAX_AI_URL_BYTES: usize = 4096;
pub const MAX_TITLE_BYTES: usize = 1024;

/// Page link snapshot limits (`docs/extension-protocol.md` v0.3, P4b-b2).
/// The snapshot is only a link list scraped from the page the extension sees;
/// HTML/body/form state is never transported.
pub const MAX_PAGE_URL_BYTES: usize = 4096;
pub const MAX_PAGE_TITLE_BYTES: usize = 1024;
pub const MAX_PAGE_LINKS: usize = 300;
pub const MAX_PAGE_LINK_BYTES: usize = 2048;

/// Error codes from `docs/extension-protocol.md` §5. Do not invent new ones:
/// the extension only understands these.
pub mod codes {
    pub const GUI_UNAVAILABLE: &str = "gui_unavailable";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const DUPLICATE: &str = "duplicate";
    pub const INTERNAL: &str = "internal";
}

/// Request headers worth forwarding to the download engine. `user-agent` and
/// `referer` travel in dedicated fields; everything else (Authorization,
/// Cookie, Host, Content-Length, …) is either handled separately or dropped.
const ALLOWED_HEADERS: &[&str] = &["accept", "accept-language", "cache-control", "pragma"];

fn default_true() -> bool {
    true
}

/// One message from the extension, internally tagged by `type`.
///
/// `AiParse` deliberately carries the page link snapshot inline (up to 300
/// links); the enum only exists while decoding one message, so the variant size
/// difference is irrelevant (same trade-off as `rdm_ipc::Request`).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Ping {
        #[serde(default)]
        id: Option<u64>,
    },
    /// Read-only progress poll (`docs/extension-protocol.md` §1.3). The reply
    /// is always a successful `status` frame, even when the GUI is down: the
    /// extension polls this repeatedly, so an `error` frame would spam it.
    Status {
        #[serde(default)]
        id: Option<u64>,
    },
    Enqueue {
        #[serde(default)]
        id: Option<u64>,
        request: Box<ExtensionRequest>,
        /// Extension option "自动启动 GUI"; default matches the documented
        /// host behaviour (try to launch RDM when it is not running).
        #[serde(default = "default_true")]
        launch_gui: bool,
    },
    /// Ask the GUI to run the AI link parser for a page
    /// (`docs/design-next-features.md` §5.2). The GUI shows the result in its
    /// AI panel; unlike `enqueue`, an AI parse request is **never** spooled to
    /// disk: parsing needs the live GUI and credentials must not be persisted.
    AiParse {
        #[serde(default)]
        id: Option<u64>,
        url: String,
        #[serde(default)]
        source_page: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        include_credentials: bool,
        #[serde(default)]
        cookie: Option<String>,
        #[serde(default)]
        referer: Option<String>,
        #[serde(default)]
        user_agent: Option<String>,
        /// P4b-b2: optional link snapshot of the page the extension sees
        /// (`docs/extension-protocol.md` v0.3). Validated here; invalid/oversized
        /// parts are dropped with a warning (never failing the parse itself).
        #[serde(default)]
        page_url: Option<String>,
        #[serde(default)]
        page_title: Option<String>,
        #[serde(default)]
        page_links: Vec<String>,
        /// Extension option "自动启动 GUI"; default matches `enqueue`.
        #[serde(default = "default_true")]
        launch_gui: bool,
    },
    /// Any future/unknown `type` value. Respond with a structured error instead
    /// of panicking or exiting.
    #[serde(other)]
    Unknown,
}

/// The `request` object of an `enqueue` message.
///
/// Mirrors `docs/extension-protocol.md` §1.3; every field is optional except
/// `url` so old/new extension versions keep working.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionRequest {
    pub url: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub output_dir: Option<String>,
    #[serde(default)]
    pub connections: Option<usize>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub speed_limit_bps: Option<u64>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_page: Option<String>,
}

/// One message from the host, internally tagged by `type`.
///
/// Field names match the protocol document exactly; the extension matches on
/// `type` + `id` and otherwise only reads the documented fields.
///
/// `Eq` is deliberately absent: [`Outgoing::Status`] carries `f64` speeds.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Pong {
        id: u64,
        version: String,
        gui_running: bool,
        protocol: u32,
    },
    /// Progress snapshot for a `status` poll.
    ///
    /// `ok` is always `true` (even when `gui_running` is `false`, in which case
    /// the snapshot is empty). The field set is the frozen wire contract from
    /// `docs/extension-protocol.md` §1.3; the extension already ships against
    /// these exact names.
    Status {
        id: u64,
        ok: bool,
        gui_running: bool,
        active: u64,
        global_speed_bps: f64,
        downloads: Vec<DownloadStatus>,
    },
    Enqueued {
        id: u64,
        download_id: String,
        gui_running: bool,
    },
    Queued {
        id: u64,
        reason: String,
        spool: String,
        /// True when cookie/authorization material was removed before the
        /// handoff was written to disk (privacy decision, see `spool` module).
        credentials_stripped: bool,
    },
    /// The GUI accepted an AI parse request and will show the result in its AI
    /// panel. `ok` is always `true`; failures use [`Outgoing::Error`] instead
    /// (the field is part of the wire contract in
    /// `docs/design-next-features.md` §5.2).
    AiParseAccepted {
        id: u64,
        ok: bool,
        /// GUI-side correlation id for the AI panel request (`0` when the GUI
        /// accepted the request without reporting one).
        request_id: u64,
        gui_running: bool,
    },
    Error {
        id: u64,
        code: String,
        message: String,
    },
}

/// One download entry inside a `status` response (`docs/extension-protocol.md`
/// §1.3).
///
/// This mirrors the GUI's `status_payload` item shape one-for-one. Only
/// `Serialize` is derived: the native host parses the GUI payload tolerantly in
/// [`crate::ipc_client`] instead of relying on strict deserialization, so one
/// malformed entry can never fail the whole poll.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DownloadStatus {
    pub id: String,
    pub file_name: String,
    /// `downloading` | `paused` | `completed` | `failed` | `cancelled`
    /// (forwarded verbatim from the GUI).
    pub state: String,
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Total size in bytes; `null` while unknown (e.g. no `Content-Length`).
    pub total: Option<u64>,
    /// Current speed in bytes per second (floating point by contract).
    pub speed_bps: f64,
    /// Planned connection count for this download (the configured maximum).
    pub connections: u64,
    /// Threads currently downloading (`DownloadItem::active_workers`).
    ///
    /// Additive v0.2 field: a GUI that predates it yields `0`, which the
    /// extension must treat as "unknown" and fall back to [`Self::connections`].
    pub active_connections: u64,
    /// Dynamic-planner target; `0` = the engine has not reported one yet.
    pub target_connections: u64,
    /// Peak concurrent workers reached during this download.
    pub peak_connections: u64,
}

impl Outgoing {
    pub fn pong(id: u64, gui_running: bool) -> Self {
        Outgoing::Pong {
            id,
            version: HOST_VERSION.to_string(),
            gui_running,
            protocol: rdm_ipc::PROTOCOL_VERSION,
        }
    }

    /// Build the `status` frame for one poll. `ok` is always `true`; when the
    /// GUI is unavailable the caller passes an empty snapshot plus
    /// `gui_running: false` (see `host::status_frame`).
    pub fn status(id: u64, gui_running: bool, snapshot: StatusSnapshot) -> Self {
        Outgoing::Status {
            id,
            ok: true,
            gui_running,
            active: snapshot.active,
            global_speed_bps: snapshot.global_speed_bps,
            downloads: snapshot.downloads,
        }
    }

    pub fn enqueued(id: u64, download_id: String) -> Self {
        Outgoing::Enqueued {
            id,
            download_id,
            gui_running: true,
        }
    }

    pub fn queued(
        id: u64,
        reason: &str,
        spool_path: &std::path::Path,
        credentials_stripped: bool,
    ) -> Self {
        Outgoing::Queued {
            id,
            reason: reason.to_string(),
            spool: spool_path.display().to_string(),
            credentials_stripped,
        }
    }

    pub fn error(id: u64, code: &str, message: impl Into<String>) -> Self {
        Outgoing::Error {
            id,
            code: code.to_string(),
            message: message.into(),
        }
    }

    pub fn ai_parse_accepted(id: u64, request_id: u64, gui_running: bool) -> Self {
        Outgoing::AiParseAccepted {
            id,
            ok: true,
            request_id,
            gui_running,
        }
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| {
            // Serialization of this enum cannot realistically fail; keep the
            // protocol usable even if it somehow does.
            br#"{"type":"error","id":0,"code":"internal","message":"failed to encode response"}"#
                .to_vec()
        })
    }
}

/// Validated `ai_parse` request, ready to be forwarded to the GUI over IPC.
///
/// Mirrors the field set of `rdm_ipc::Request::AiParse`. Cookies are only kept
/// when the user explicitly opted in (`include_credentials`), are capped at
/// [`MAX_COOKIE_BYTES`], and are never logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiParseRequest {
    pub url: String,
    pub source_page: Option<String>,
    pub title: Option<String>,
    pub include_credentials: bool,
    pub cookie: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    /// P4b-b2 page link snapshot (validated; empty/`None` = no snapshot).
    pub page_url: Option<String>,
    pub page_title: Option<String>,
    pub page_links: Vec<String>,
}

impl AiParseRequest {
    /// Validate the browser-supplied AI parse fields.
    ///
    /// Only a bad `url` is fatal (matching [`ExtensionRequest::validate`]);
    /// suspicious optional fields are dropped with a warning so the parse still
    /// happens. The `cookie` is dropped unless `include_credentials` is `true`:
    /// that flag is the user's explicit consent, and the extension must not be
    /// able to bypass it.
    pub fn validate(self, logger: &Logger) -> Result<Self, String> {
        let url = self.url.trim().to_string();
        if url.is_empty() {
            return Err("url is empty".to_string());
        }
        if url.len() > MAX_AI_URL_BYTES {
            return Err(format!("url is longer than {MAX_AI_URL_BYTES} bytes"));
        }
        let parsed = url_util::parse_http_url(&url)?;
        let host = parsed.host_str().unwrap_or("unknown").to_ascii_lowercase();

        let source_page = sanitize_optional_url(
            self.source_page.as_deref(),
            MAX_SOURCE_PAGE_BYTES,
            logger,
            "source_page",
        );
        let title = sanitize_optional_text(self.title.as_deref(), MAX_TITLE_BYTES, logger, "title");

        let cookie = match self.cookie.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                if !self.include_credentials {
                    logger.info("ai_parse cookie dropped: include_credentials=false");
                    None
                } else if raw.len() > MAX_COOKIE_BYTES {
                    logger.warn(&format!(
                        "ai_parse cookie dropped: {} bytes exceeds the {MAX_COOKIE_BYTES} byte limit (host={host})",
                        raw.len()
                    ));
                    None
                } else if has_control_chars(raw) {
                    logger.warn("ai_parse cookie dropped: contains control characters");
                    None
                } else {
                    // Privacy: log shape only, never values (same rule as enqueue).
                    Some(raw.to_string())
                }
            }
            _ => None,
        };

        let referer = sanitize_optional_url(
            self.referer.as_deref(),
            MAX_REFERER_BYTES,
            logger,
            "referer",
        );
        let user_agent = sanitize_optional_text(
            self.user_agent.as_deref(),
            MAX_USER_AGENT_BYTES,
            logger,
            "user_agent",
        );

        // P4b-b2: the page link snapshot never fails the parse; invalid parts
        // are dropped with a warning that carries counts/lengths only.
        let (page_url, page_title, page_links) = sanitize_page_snapshot(
            self.page_url.as_deref(),
            self.page_title.as_deref(),
            self.page_links,
            &parsed,
            logger,
        );

        logger.info(&format!(
            "ai_parse request accepted: host={host} url={} source_page={} title_bytes={} include_credentials={} cookie_bytes={} referer={} user_agent={} page_links={} page_title_bytes={}",
            url_util::redact_url(&url),
            source_page.is_some(),
            title.as_deref().map_or(0, str::len),
            self.include_credentials,
            cookie.as_deref().map_or(0, str::len),
            referer.is_some(),
            user_agent.is_some(),
            page_links.len(),
            page_title.as_deref().map_or(0, str::len),
        ));

        Ok(Self {
            url,
            source_page,
            title,
            include_credentials: self.include_credentials,
            cookie,
            referer,
            user_agent,
            page_url,
            page_title,
            page_links,
        })
    }
}

/// Validate the optional P4b-b2 page link snapshot.
///
/// Returns `(page_url, page_title, page_links)`. The whole group is dropped
/// (parse still proceeds) when the page URL is missing/invalid/oversized or its
/// host does not match the parse URL's host; a link list without a page URL is
/// dropped too (it has no attribution). Logs carry only counts/lengths/hosts,
/// never URL contents.
fn sanitize_page_snapshot(
    page_url: Option<&str>,
    page_title: Option<&str>,
    page_links: Vec<String>,
    request_url: &Url,
    logger: &Logger,
) -> (Option<String>, Option<String>, Vec<String>) {
    let Some(raw_page_url) = page_url.map(str::trim).filter(|value| !value.is_empty()) else {
        if !page_links.is_empty() {
            logger.warn(&format!(
                "ai_parse page snapshot dropped: {} link(s) without a page_url",
                page_links.len()
            ));
        }
        return (None, None, Vec::new());
    };
    if raw_page_url.len() > MAX_PAGE_URL_BYTES {
        logger.warn(&format!(
            "ai_parse page snapshot dropped: page_url is {} bytes (limit {MAX_PAGE_URL_BYTES})",
            raw_page_url.len()
        ));
        return (None, None, Vec::new());
    }
    let parsed_page = match url_util::parse_http_url(raw_page_url) {
        Ok(parsed) => parsed,
        Err(_) => {
            logger.warn(&format!(
                "ai_parse page snapshot dropped: page_url is invalid ({} bytes)",
                raw_page_url.len()
            ));
            return (None, None, Vec::new());
        }
    };
    let request_host = normalized_url_host(request_url);
    let page_host = normalized_url_host(&parsed_page);
    if page_host != request_host {
        logger.warn(&format!(
            "ai_parse page snapshot dropped: page_url host does not match url host={request_host}"
        ));
        return (None, None, Vec::new());
    }

    let page_url = Some(raw_page_url.to_string());
    let page_title = sanitize_optional_text(page_title, MAX_PAGE_TITLE_BYTES, logger, "page_title");
    let page_links = sanitize_page_links(page_links, logger);
    (page_url, page_title, page_links)
}

/// Lowercase host with a single trailing dot ignored (`a.Example.COM.` →
/// `a.example.com`); `unknown` for invalid URLs (never matches a real host).
fn normalized_url_host(url: &Url) -> String {
    url.host_str()
        .unwrap_or("unknown")
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Filter an untrusted link list: absolute http(s) only, trimmed, deduplicated
/// (first occurrence wins) and capped at [`MAX_PAGE_LINKS`] / [`MAX_PAGE_LINK_BYTES`].
fn sanitize_page_links(raw_links: Vec<String>, logger: &Logger) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for raw in raw_links {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_PAGE_LINK_BYTES {
            dropped += 1;
            continue;
        }
        let Ok(parsed) = url_util::parse_http_url(trimmed) else {
            dropped += 1;
            continue;
        };
        let normalized = parsed.to_string();
        if links.contains(&normalized) {
            continue;
        }
        if links.len() >= MAX_PAGE_LINKS {
            dropped += 1;
            continue;
        }
        links.push(normalized);
    }
    if dropped > 0 {
        logger.warn(&format!(
            "ai_parse page_links dropped {dropped} invalid/over-limit item(s)"
        ));
    }
    links
}

impl ExtensionRequest {
    /// Validate the browser-supplied request and turn it into the canonical
    /// `rdm_ipc::EnqueueItem` forwarded to the GUI.
    ///
    /// Hard errors (only really "bad URL") come back as `Err`; suspicious but
    /// recoverable material (a bad referer, a disallowed header, an oversized
    /// cookie) is dropped with a warning so the download still happens.
    pub fn validate(self, logger: &Logger) -> Result<rdm_ipc::EnqueueItem, String> {
        let url = url_util::parse_http_url(&self.url)?;
        let host = url.host_str().unwrap_or("unknown").to_ascii_lowercase();

        let file_name = self.file_name.as_deref().and_then(sanitize_file_name);

        let output_dir = match self.output_dir.as_deref() {
            Some(raw) => match sanitize_output_dir(raw) {
                Some(path) => Some(path),
                None => {
                    logger.warn("output_dir dropped: not an absolute, control-free path");
                    None
                }
            },
            None => None,
        };

        let connections = self
            .connections
            .unwrap_or(rdm_core::DEFAULT_CONNECTIONS)
            .clamp(1, rdm_core::MAX_CONNECTIONS);

        let headers = sanitize_headers(&self.headers, logger);

        let cookie = match self.cookie.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                if raw.len() > MAX_COOKIE_BYTES {
                    logger.warn(&format!(
                        "cookie dropped: {} bytes exceeds the {MAX_COOKIE_BYTES} byte limit (host={host})",
                        raw.len()
                    ));
                    None
                } else if has_control_chars(raw) {
                    logger.warn(&format!(
                        "cookie dropped: contains control characters (host={host})"
                    ));
                    None
                } else {
                    // Privacy: log shape only, never values (see docs §4).
                    logger.info(&format!(
                        "cookie accepted: host={host} bytes={} entries={}",
                        raw.len(),
                        raw.split(';').filter(|p| !p.trim().is_empty()).count()
                    ));
                    Some(raw.to_string())
                }
            }
            _ => None,
        };

        let referer = match self.referer.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                if raw.len() > MAX_REFERER_BYTES
                    || has_control_chars(raw)
                    || url_util::parse_http_url(raw).is_err()
                {
                    logger.warn("referer dropped: invalid or oversized value");
                    None
                } else {
                    Some(raw.to_string())
                }
            }
            _ => None,
        };

        let user_agent = match self.user_agent.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                if raw.len() > MAX_USER_AGENT_BYTES || has_control_chars(raw) {
                    logger.warn("user_agent dropped: invalid or oversized value");
                    None
                } else {
                    Some(raw.to_string())
                }
            }
            _ => None,
        };

        let proxy = match self.proxy.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                let valid = raw.len() <= MAX_PROXY_BYTES
                    && !has_control_chars(raw)
                    && url::Url::parse(raw)
                        .ok()
                        .map(|u| matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h"))
                        .unwrap_or(false);
                if valid {
                    Some(raw.to_string())
                } else {
                    logger.warn("proxy dropped: invalid or unsupported value");
                    None
                }
            }
            _ => None,
        };

        let speed_limit_bps = self.speed_limit_bps.filter(|bps| *bps > 0);

        let expected_sha256 = match self.expected_sha256.as_deref().map(str::trim) {
            Some(raw) if is_sha256_hex(raw) => Some(raw.to_ascii_lowercase()),
            Some(raw) if !raw.is_empty() => {
                logger.warn(&format!(
                    "expected_sha256 dropped: expected 64 hex chars, got {} chars",
                    raw.len()
                ));
                None
            }
            _ => None,
        };

        let source = self
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| is_simple_token(s))
            .unwrap_or("firefox")
            .to_string();

        let source_page = match self.source_page.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                if raw.len() > MAX_SOURCE_PAGE_BYTES || url_util::parse_http_url(raw).is_err() {
                    None
                } else {
                    Some(raw.to_string())
                }
            }
            _ => None,
        };

        let mut download = rdm_core::DownloadRequest::new(url.as_str());
        download.file_name = file_name;
        download.output_dir = output_dir;
        download.connections = connections;
        download.headers = headers;
        download.cookie = cookie;
        download.referer = referer;
        download.user_agent = user_agent;
        download.proxy = proxy;
        download.speed_limit_bps = speed_limit_bps;
        download.expected_sha256 = expected_sha256;
        download.resume = true;
        // Never let a browser handoff disable certificate checks.
        download.insecure = false;

        logger.info(&format!(
            "request accepted: url={} file={} connections={} headers={} cookie={} referer={}",
            url_util::redact_url(url.as_str()),
            download.file_name.as_deref().unwrap_or("<server-derived>"),
            connections,
            download.headers.len(),
            download.cookie.is_some(),
            download.referer.is_some(),
        ));

        Ok(rdm_ipc::EnqueueItem {
            download,
            source,
            source_page,
        })
    }
}

/// Extract the numeric message id, tolerating absent/non-numeric values (a
/// missing id becomes `0`, per the protocol agreement with the extension).
pub fn extract_id(message: &serde_json::Value) -> u64 {
    message.get("id").and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Reduce an extension-provided "file name override" to a single safe path
/// component. Returns `None` for names that only consist of separators/dots.
pub fn sanitize_file_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip directories on both Windows and POSIX separators, so
    // `..\..\evil.exe` becomes `evil.exe`.
    let base = trimmed
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed);
    let mut cleaned = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_control() {
            continue;
        }
        match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => cleaned.push('_'),
            _ => cleaned.push(ch),
        }
    }
    let cleaned = cleaned.trim().trim_end_matches('.').trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(truncate_utf8(&cleaned, MAX_FILE_NAME_BYTES))
}

fn sanitize_output_dir(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_PATH_BYTES || has_control_chars(trimmed) {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return None;
    }
    Some(path)
}

fn sanitize_headers(
    headers: &BTreeMap<String, String>,
    logger: &Logger,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut dropped = 0usize;
    for (name, value) in headers {
        if out.len() >= MAX_HEADERS {
            dropped += 1;
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if !ALLOWED_HEADERS.contains(&lower.as_str()) {
            dropped += 1;
            continue;
        }
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_HEADER_VALUE_BYTES || has_control_chars(value) {
            dropped += 1;
            continue;
        }
        out.insert(name.clone(), value.to_string());
    }
    if dropped > 0 {
        // Log names never appear (they can be arbitrary page input); the count
        // is enough for diagnostics.
        logger.warn(&format!(
            "dropped {dropped} header(s) not on the allow-list"
        ));
    }
    out
}

/// Sanitize an optional URL field of an AI parse request. Invalid, oversized
/// or non-http(s) values are dropped with a warning instead of failing the
/// whole request (the URL itself is the only fatal field).
fn sanitize_optional_url(
    raw: Option<&str>,
    max_bytes: usize,
    logger: &Logger,
    field: &str,
) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.len() > max_bytes || url_util::parse_http_url(raw).is_err() {
        logger.warn(&format!(
            "ai_parse {field} dropped: invalid or oversized value"
        ));
        return None;
    }
    Some(raw.to_string())
}

/// Sanitize an optional free-text field (`title`, `user_agent`): strip control
/// characters, collapse whitespace and truncate to `max_bytes` (UTF-8 safe).
/// Returns `None` when nothing printable is left. Values are never logged.
fn sanitize_optional_text(
    raw: Option<&str>,
    max_bytes: usize,
    logger: &Logger,
    field: &str,
) -> Option<String> {
    let raw = raw?;
    let mut cleaned = String::with_capacity(raw.len().min(max_bytes));
    let mut last_was_space = false;
    for ch in raw.chars() {
        if ch.is_control() {
            // Treat control characters as separators so words never get glued
            // together ("Mozilla/5.0\nInjected: x" -> "Mozilla/5.0 Injected: x").
            if !cleaned.is_empty() && !last_was_space {
                cleaned.push(' ');
                last_was_space = true;
            }
            continue;
        }
        if ch.is_whitespace() {
            if cleaned.is_empty() || last_was_space {
                continue;
            }
            cleaned.push(' ');
            last_was_space = true;
        } else {
            cleaned.push(ch);
            last_was_space = false;
        }
    }
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > max_bytes {
        logger.warn(&format!("ai_parse {field} truncated to {max_bytes} bytes"));
    }
    Some(truncate_utf8(trimmed, max_bytes))
}

pub fn has_control_chars(value: &str) -> bool {
    value.chars().any(|ch| ch.is_control())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn is_simple_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SOURCE_BYTES
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_logger() -> (Logger, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "rdm-native-protocol-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Logger::new(&dir), dir)
    }

    fn parse(json: &str) -> serde_json::Result<Incoming> {
        serde_json::from_str(json)
    }

    #[test]
    fn parses_ping_with_id_and_without() {
        match parse(r#"{"type":"ping","id":7}"#).unwrap() {
            Incoming::Ping { id } => assert_eq!(id, Some(7)),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(r#"{"type":"ping"}"#).unwrap() {
            Incoming::Ping { id } => assert_eq!(id, None),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_minimal_enqueue_with_launch_gui_default() {
        let msg =
            parse(r#"{"type":"enqueue","id":2,"request":{"url":"https://example.com/a.bin"}}"#)
                .unwrap();
        match msg {
            Incoming::Enqueue {
                id,
                request,
                launch_gui,
            } => {
                assert_eq!(id, Some(2));
                assert!(launch_gui, "launch_gui must default to true");
                assert_eq!(request.url, "https://example.com/a.bin");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_launch_gui_false() {
        let msg = parse(
            r#"{"type":"enqueue","id":1,"launch_gui":false,"request":{"url":"https://example.com/a"}}"#,
        )
        .unwrap();
        match msg {
            Incoming::Enqueue { launch_gui, .. } => assert!(!launch_gui),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_captured_not_fatal() {
        match parse(r#"{"type":"activate","id":3}"#).unwrap() {
            Incoming::Unknown => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn enqueue_without_request_is_a_serde_error() {
        assert!(parse(r#"{"type":"enqueue","id":2}"#).is_err());
    }

    #[test]
    fn extract_id_tolerates_missing_and_weird_values() {
        let value: serde_json::Value = serde_json::from_str(r#"{"type":"ping","id":"x"}"#).unwrap();
        assert_eq!(extract_id(&value), 0);
        let value: serde_json::Value = serde_json::from_str(r#"{"type":"ping","id":9}"#).unwrap();
        assert_eq!(extract_id(&value), 9);
        let value: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert_eq!(extract_id(&value), 0);
    }

    #[test]
    fn validate_full_request_maps_every_field() {
        let (logger, dir) = test_logger();
        let request: ExtensionRequest = serde_json::from_str(
            r#"{
                "url": "https://example.com/big.iso?token=secret",
                "file_name": "renamed.iso",
                "output_dir": "C:\\Users\\me\\Downloads",
                "connections": 99,
                "headers": {
                    "Accept": "application/octet-stream",
                    "Authorization": "Bearer leak-me",
                    "X-Weird": "nope"
                },
                "cookie": "a=1; b=2",
                "referer": "https://example.com/page",
                "user_agent": "Mozilla/5.0 test",
                "speed_limit_bps": 1048576,
                "source": "firefox",
                "source_page": "https://example.com/page"
            }"#,
        )
        .unwrap();
        let item = request.validate(&logger).unwrap();
        assert_eq!(
            item.download.url,
            "https://example.com/big.iso?token=secret"
        );
        assert_eq!(item.download.file_name.as_deref(), Some("renamed.iso"));
        assert_eq!(
            item.download.output_dir.as_deref(),
            Some(Path::new(r"C:\Users\me\Downloads"))
        );
        assert_eq!(item.download.connections, rdm_core::MAX_CONNECTIONS);
        assert_eq!(
            item.download.headers.get("Accept").map(String::as_str),
            Some("application/octet-stream")
        );
        assert!(
            !item.download.headers.contains_key("Authorization"),
            "Authorization must never be forwarded"
        );
        assert!(!item.download.headers.contains_key("X-Weird"));
        assert_eq!(item.download.cookie.as_deref(), Some("a=1; b=2"));
        assert_eq!(
            item.download.referer.as_deref(),
            Some("https://example.com/page")
        );
        assert_eq!(
            item.download.user_agent.as_deref(),
            Some("Mozilla/5.0 test")
        );
        assert_eq!(item.download.speed_limit_bps, Some(1_048_576));
        assert_eq!(item.source, "firefox");
        assert_eq!(
            item.source_page.as_deref(),
            Some("https://example.com/page")
        );
        assert!(item.download.resume, "resume defaults to true");
        assert!(
            !item.download.insecure,
            "browser handoffs cannot disable TLS checks"
        );
        // Privacy check: the log must not contain the cookie or the URL query.
        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(!log.contains("a=1; b=2"));
        assert!(!log.contains("secret"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_rejects_non_http_urls() {
        let (logger, dir) = test_logger();
        for url in [
            "file:///C:/Windows/win.ini",
            "data:text/plain,x",
            "nonsense",
        ] {
            let request: ExtensionRequest =
                serde_json::from_str(&format!(r#"{{"url":"{url}"}}"#)).unwrap();
            assert!(request.validate(&logger).is_err(), "{url} must be rejected");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_drops_dangerous_cookie() {
        let (logger, dir) = test_logger();
        let request: ExtensionRequest = serde_json::from_str(
            r#"{"url":"https://example.com/a","cookie":"a=1\r\nX-Injected: yes"}"#,
        )
        .unwrap();
        let item = request.validate(&logger).unwrap();
        assert_eq!(item.download.cookie, None);
        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("cookie dropped"));
        assert!(!log.contains("X-Injected"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_drops_relative_output_dir_and_bad_referer() {
        let (logger, dir) = test_logger();
        let request: ExtensionRequest = serde_json::from_str(
            r#"{"url":"https://example.com/a","output_dir":"relative/dir","referer":"javascript:alert(1)"}"#,
        )
        .unwrap();
        let item = request.validate(&logger).unwrap();
        assert_eq!(item.download.output_dir, None);
        assert_eq!(item.download.referer, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_sha256_and_speed_limit() {
        let (logger, dir) = test_logger();
        let request: ExtensionRequest = serde_json::from_str(&format!(
            r#"{{"url":"https://example.com/a","expected_sha256":"{}","speed_limit_bps":0}}"#,
            "A".repeat(64)
        ))
        .unwrap();
        let item = request.validate(&logger).unwrap();
        assert_eq!(item.download.expected_sha256, Some("a".repeat(64)));
        assert_eq!(item.download.speed_limit_bps, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn filename_sanitizer_strips_paths_and_invalid_chars() {
        assert_eq!(
            sanitize_file_name(r"..\..\evil.exe").as_deref(),
            Some("evil.exe")
        );
        assert_eq!(
            sanitize_file_name("/tmp/../x/a<b>c.iso").as_deref(),
            Some("a_b_c.iso")
        );
        assert_eq!(sanitize_file_name("   "), None);
        assert_eq!(sanitize_file_name(".."), None);
        // 目录路径退化成最后一段（防御性清洗，不视为错误）。
        assert_eq!(sanitize_file_name(r"C:\dir\").as_deref(), Some("dir"));
        let long = "汉".repeat(100);
        let sanitized = sanitize_file_name(&long).unwrap();
        assert!(sanitized.len() <= MAX_FILE_NAME_BYTES);
        assert!(sanitized.chars().count() < 100);
    }

    #[test]
    fn response_shapes_match_protocol_document() {
        let pong = Outgoing::pong(1, true);
        let json: serde_json::Value = serde_json::from_slice(&pong.to_json()).unwrap();
        assert_eq!(json["type"], "pong");
        assert_eq!(json["id"], 1);
        assert_eq!(json["gui_running"], true);
        assert_eq!(
            json["protocol"].as_u64(),
            Some(rdm_ipc::PROTOCOL_VERSION as u64)
        );
        assert!(json["version"].is_string());

        let enqueued = Outgoing::enqueued(2, "dl_01".into());
        let json: serde_json::Value = serde_json::from_slice(&enqueued.to_json()).unwrap();
        assert_eq!(json["type"], "enqueued");
        assert_eq!(json["download_id"], "dl_01");
        assert_eq!(json["gui_running"], true);

        let queued = Outgoing::queued(3, codes::GUI_UNAVAILABLE, Path::new(r"C:\tmp\1.json"), true);
        let json: serde_json::Value = serde_json::from_slice(&queued.to_json()).unwrap();
        assert_eq!(json["type"], "queued");
        assert_eq!(json["reason"], "gui_unavailable");
        assert_eq!(json["spool"], r"C:\tmp\1.json");
        assert_eq!(
            json["credentials_stripped"], true,
            "queued responses must tell the extension that credentials were not persisted"
        );
        let clean = Outgoing::queued(
            4,
            codes::GUI_UNAVAILABLE,
            Path::new(r"C:\tmp\2.json"),
            false,
        );
        let clean_json: serde_json::Value = serde_json::from_slice(&clean.to_json()).unwrap();
        assert_eq!(clean_json["credentials_stripped"], false);

        let error = Outgoing::error(5, codes::INVALID_REQUEST, "bad");
        let json: serde_json::Value = serde_json::from_slice(&error.to_json()).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["code"], "invalid_request");
    }

    #[test]
    fn incoming_ignores_unknown_fields() {
        assert!(
            parse(r#"{"type":"ping","id":1,"future_field":{"a":1}}"#).is_ok(),
            "unknown fields must be ignored for forward compatibility"
        );
        assert!(
            parse(
                r#"{"type":"ai_parse","id":2,"url":"https://example.com/","future_field":{"a":1}}"#
            )
            .is_ok(),
            "ai_parse must ignore unknown fields too"
        );
        assert!(
            parse(r#"{"type":"status","id":3,"future_field":[1,2,3]}"#).is_ok(),
            "status must ignore unknown fields too"
        );
    }

    // -- Status polling (`status`) -----------------------------------------

    fn sample_download() -> DownloadStatus {
        DownloadStatus {
            id: "dl_000001".to_string(),
            file_name: "a.bin".to_string(),
            state: "downloading".to_string(),
            downloaded: 1_048_576,
            total: Some(10_485_760),
            speed_bps: 524_288.0,
            connections: 8,
            active_connections: 6,
            target_connections: 12,
            peak_connections: 16,
        }
    }

    fn sample_snapshot() -> StatusSnapshot {
        StatusSnapshot {
            active: 2,
            global_speed_bps: 1_048_576.0,
            downloads: vec![sample_download()],
        }
    }

    #[test]
    fn parses_status_minimal_message_with_and_without_id() {
        match parse(r#"{"type":"status","id":51}"#).unwrap() {
            Incoming::Status { id } => assert_eq!(id, Some(51)),
            other => panic!("unexpected: {other:?}"),
        }
        match parse(r#"{"type":"status"}"#).unwrap() {
            Incoming::Status { id } => assert_eq!(id, None),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn status_response_shape_matches_protocol_document() {
        let frame = Outgoing::status(51, true, sample_snapshot());
        let json: serde_json::Value = serde_json::from_slice(&frame.to_json()).unwrap();

        // Exact frozen top-level field set (`docs/extension-protocol.md` §1.3):
        // the extension already ships against these names, so a rename here is
        // a breaking change and must fail loudly.
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("status frame is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "active",
                "downloads",
                "global_speed_bps",
                "gui_running",
                "id",
                "ok",
                "type",
            ]
        );
        assert_eq!(json["type"], "status");
        assert_eq!(json["id"], 51);
        assert_eq!(json["ok"], true);
        assert_eq!(json["gui_running"], true);
        assert_eq!(json["active"], 2);
        assert_eq!(
            json["global_speed_bps"].as_f64(),
            Some(1_048_576.0),
            "global_speed_bps is a floating point field"
        );

        let download = &json["downloads"][0];
        let mut keys: Vec<&str> = download
            .as_object()
            .expect("download entry is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "active_connections",
                "connections",
                "downloaded",
                "file_name",
                "id",
                "peak_connections",
                "speed_bps",
                "state",
                "target_connections",
                "total",
            ]
        );
        assert_eq!(download["id"], "dl_000001");
        assert_eq!(download["file_name"], "a.bin");
        assert_eq!(download["state"], "downloading");
        assert_eq!(download["downloaded"], 1_048_576);
        assert_eq!(download["total"], 10_485_760);
        assert_eq!(download["speed_bps"].as_f64(), Some(524_288.0));
        assert_eq!(download["connections"], 8);
        assert_eq!(
            download["active_connections"], 6,
            "active worker count must be forwarded"
        );
        assert_eq!(download["target_connections"], 12);
        assert_eq!(download["peak_connections"], 16);
    }

    #[test]
    fn status_thread_fields_serialize_even_when_zero() {
        // 旧 GUI 缺字段时宿主用 0 填充，但必须总是输出这三个键（扩展按字段存在
        // 与否无法区分"未知"），0 由扩展解释为未知。
        let mut download = sample_download();
        download.active_connections = 0;
        download.target_connections = 0;
        download.peak_connections = 0;
        let frame = Outgoing::status(
            53,
            true,
            StatusSnapshot {
                active: 1,
                global_speed_bps: 0.0,
                downloads: vec![download],
            },
        );
        let json: serde_json::Value = serde_json::from_slice(&frame.to_json()).unwrap();
        for field in [
            "active_connections",
            "target_connections",
            "peak_connections",
        ] {
            assert!(
                json["downloads"][0].get(field).is_some(),
                "{field} must always be serialized (got {json})"
            );
            assert_eq!(json["downloads"][0][field], 0, "{field} must be 0");
        }
    }

    #[test]
    fn status_unknown_total_serializes_as_null() {
        let mut download = sample_download();
        download.total = None;
        download.speed_bps = 0.0;
        let snapshot = StatusSnapshot {
            active: 1,
            global_speed_bps: 0.0,
            downloads: vec![download],
        };
        let frame = Outgoing::status(7, true, snapshot);
        let json: serde_json::Value = serde_json::from_slice(&frame.to_json()).unwrap();
        assert!(
            json["downloads"][0]["total"].is_null(),
            "unknown total must be null (never 0), got {}",
            json["downloads"][0]["total"]
        );
    }

    #[test]
    fn status_unavailable_frame_is_ok_with_empty_snapshot() {
        let frame = Outgoing::status(52, false, StatusSnapshot::default());
        let json: serde_json::Value = serde_json::from_slice(&frame.to_json()).unwrap();
        assert_eq!(json["type"], "status");
        assert_eq!(json["id"], 52);
        assert_eq!(
            json["ok"], true,
            "a missing GUI must still be an ok frame, otherwise a 1 Hz poll spams errors"
        );
        assert_eq!(json["gui_running"], false);
        assert_eq!(json["active"], 0);
        assert_eq!(json["global_speed_bps"].as_f64(), Some(0.0));
        assert_eq!(
            json["downloads"]
                .as_array()
                .map(|downloads| downloads.len()),
            Some(0)
        );
    }

    // -- AI parse (`ai_parse`) ---------------------------------------------

    fn ai_request_from_json(json: &str) -> AiParseRequest {
        match parse(json).unwrap() {
            Incoming::AiParse {
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
                ..
            } => AiParseRequest {
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_ai_parse_minimal_message_with_defaults() {
        match parse(r#"{"type":"ai_parse","id":9,"url":"https://example.com/page"}"#).unwrap() {
            Incoming::AiParse {
                id,
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
                launch_gui,
            } => {
                assert_eq!(id, Some(9));
                assert_eq!(url, "https://example.com/page");
                assert_eq!(source_page, None);
                assert_eq!(title, None);
                assert!(!include_credentials, "credentials are opt-in");
                assert_eq!(cookie, None);
                assert_eq!(referer, None);
                assert_eq!(user_agent, None);
                // P4b-b2: old extensions send no page snapshot at all.
                assert_eq!(page_url, None);
                assert_eq!(page_title, None);
                assert!(page_links.is_empty());
                assert!(launch_gui, "launch_gui must default to true");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_ai_parse_full_message() {
        let json = r#"{
            "type": "ai_parse",
            "id": 7,
            "url": "https://example.com/page",
            "title": "Example Page",
            "source_page": "https://example.com/list",
            "include_credentials": true,
            "cookie": "sid=topsecret; theme=dark",
            "referer": "https://example.com/list",
            "user_agent": "Mozilla/5.0 (test)",
            "page_url": "https://example.com/page",
            "page_title": "Example Page",
            "page_links": ["https://cdn.example.com/a.mp4"],
            "launch_gui": false
        }"#;
        match parse(json).unwrap() {
            Incoming::AiParse {
                id,
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
                launch_gui,
            } => {
                assert_eq!(id, Some(7));
                assert_eq!(url, "https://example.com/page");
                assert_eq!(source_page.as_deref(), Some("https://example.com/list"));
                assert_eq!(title.as_deref(), Some("Example Page"));
                assert!(include_credentials);
                assert_eq!(cookie.as_deref(), Some("sid=topsecret; theme=dark"));
                assert_eq!(referer.as_deref(), Some("https://example.com/list"));
                assert_eq!(user_agent.as_deref(), Some("Mozilla/5.0 (test)"));
                assert_eq!(page_url.as_deref(), Some("https://example.com/page"));
                assert_eq!(page_title.as_deref(), Some("Example Page"));
                assert_eq!(page_links, vec!["https://cdn.example.com/a.mp4".to_owned()]);
                assert!(!launch_gui);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_ai_parse_maps_every_field_and_never_logs_cookie() {
        let (logger, dir) = test_logger();
        let request = ai_request_from_json(
            r#"{
                "type": "ai_parse",
                "url": "https://example.com/page?token=secret-signature",
                "title": "  Example   Page  ",
                "source_page": "https://example.com/list?q=secret",
                "include_credentials": true,
                "cookie": "sid=topsecret; theme=dark",
                "referer": "https://example.com/list",
                "user_agent": "Mozilla/5.0 (test)"
            }"#,
        );
        let validated = request.validate(&logger).expect("valid ai_parse");
        assert_eq!(
            validated.url,
            "https://example.com/page?token=secret-signature"
        );
        assert_eq!(validated.title.as_deref(), Some("Example Page"));
        assert_eq!(
            validated.source_page.as_deref(),
            Some("https://example.com/list?q=secret")
        );
        assert!(validated.include_credentials);
        assert_eq!(
            validated.cookie.as_deref(),
            Some("sid=topsecret; theme=dark")
        );
        assert_eq!(
            validated.referer.as_deref(),
            Some("https://example.com/list")
        );
        assert_eq!(validated.user_agent.as_deref(), Some("Mozilla/5.0 (test)"));

        // Privacy: cookie/query values must never reach the log.
        let log = std::fs::read_to_string(logger.path()).unwrap();
        for secret in ["topsecret", "sid=", "secret-signature", "q=secret"] {
            assert!(!log.contains(secret), "log leaked {secret}: {log}");
        }
        assert!(log.contains("ai_parse request accepted"), "log: {log}");
        assert!(log.contains("cookie_bytes="), "log: {log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_ai_parse_rejects_invalid_and_oversized_urls() {
        let (logger, dir) = test_logger();
        for url in [
            "file:///C:/Windows/win.ini",
            "data:text/plain,x",
            "javascript:alert(1)",
            "ftp://example.com/page",
            "not a url",
            "",
        ] {
            let request = ai_request_from_json(&format!(r#"{{"type":"ai_parse","url":"{url}"}}"#));
            let error = request.validate(&logger).expect_err("invalid url");
            assert!(
                error.contains("url") || error.contains("scheme") || error.contains("host"),
                "unexpected error for {url:?}: {error}"
            );
        }

        let oversized = format!(
            r#"{{"type":"ai_parse","url":"https://example.com/{}"}}"#,
            "a".repeat(MAX_AI_URL_BYTES)
        );
        let error = ai_request_from_json(&oversized)
            .validate(&logger)
            .expect_err("oversized url");
        assert!(
            error.contains(&MAX_AI_URL_BYTES.to_string()),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_ai_parse_drops_cookie_without_explicit_consent() {
        let (logger, dir) = test_logger();
        let validated = ai_request_from_json(
            r#"{"type":"ai_parse","url":"https://example.com/page",
                "include_credentials":false,"cookie":"sid=topsecret"}"#,
        )
        .validate(&logger)
        .expect("valid ai_parse");
        assert!(!validated.include_credentials);
        assert_eq!(
            validated.cookie, None,
            "cookie must never be forwarded without explicit consent"
        );
        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("include_credentials=false"), "log: {log}");
        assert!(!log.contains("topsecret"), "cookie value leaked: {log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_ai_parse_drops_unsafe_or_oversized_cookie() {
        let (logger, dir) = test_logger();

        let with_newline = ai_request_from_json(
            r#"{"type":"ai_parse","url":"https://example.com/","include_credentials":true,
                "cookie":"a=1\r\nX-Injected: yes"}"#,
        )
        .validate(&logger)
        .expect("valid ai_parse");
        assert_eq!(with_newline.cookie, None);
        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("cookie dropped"), "log: {log}");
        assert!(!log.contains("X-Injected"), "log: {log}");

        let oversized_cookie = format!("a={}", "v".repeat(MAX_COOKIE_BYTES));
        let oversized = ai_request_from_json(&format!(
            r#"{{"type":"ai_parse","url":"https://example.com/","include_credentials":true,
                "cookie":"{oversized_cookie}"}}"#
        ))
        .validate(&logger)
        .expect("valid ai_parse");
        assert_eq!(oversized.cookie, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_ai_parse_sanitizes_optional_fields() {
        let (logger, dir) = test_logger();
        let long_title = "汉".repeat(1000);
        let validated = ai_request_from_json(
            r#"{"type":"ai_parse","url":"https://example.com/",
                "title":"  hello\u0007   world  ",
                "source_page":"file:///C:/secret.txt",
                "referer":"javascript:alert(1)",
                "user_agent":"Mozilla/5.0\nInjected: x"}"#,
        )
        .validate(&logger)
        .expect("valid ai_parse");
        assert_eq!(validated.title.as_deref(), Some("hello world"));
        assert_eq!(
            validated.source_page, None,
            "non-http source_page is dropped"
        );
        assert_eq!(validated.referer, None, "non-http referer is dropped");
        assert_eq!(
            validated.user_agent.as_deref(),
            Some("Mozilla/5.0 Injected: x"),
            "control characters must not glue/smuggle header values"
        );

        let truncated = ai_request_from_json(&format!(
            r#"{{"type":"ai_parse","url":"https://example.com/","title":"{long_title}"}}"#
        ))
        .validate(&logger)
        .expect("valid ai_parse");
        let title = truncated.title.expect("title truncated, not dropped");
        assert!(title.len() <= MAX_TITLE_BYTES, "title too long: {title:?}");
        assert!(title.chars().count() < 1000);
        let _ = std::fs::remove_dir_all(dir);
    }

    // -- P4b-b2: page link snapshot ----------------------------------------

    #[test]
    fn validate_ai_parse_keeps_a_valid_page_snapshot() {
        let (logger, dir) = test_logger();
        let validated = ai_request_from_json(
            r#"{
                "type": "ai_parse",
                "url": "https://example.com/page?token=secret-signature",
                "page_url": "  https://Example.COM./page?token=secret-signature  ",
                "page_title": "  Demo\tPage  ",
                "page_links": [
                    " https://cdn.example.com/a.bin ",
                    "https://cdn.example.com/a.bin",
                    "https://cdn.example.com/b.zip?x=1"
                ]
            }"#,
        )
        .validate(&logger)
        .expect("valid ai_parse");

        assert_eq!(
            validated.page_url.as_deref(),
            Some("https://Example.COM./page?token=secret-signature")
        );
        assert_eq!(validated.page_title.as_deref(), Some("Demo Page"));
        assert_eq!(
            validated.page_links,
            vec![
                "https://cdn.example.com/a.bin".to_string(),
                "https://cdn.example.com/b.zip?x=1".to_string()
            ],
            "links trimmed, deduplicated in order"
        );

        // Privacy: logs carry counts/lengths only, never URL/link values.
        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("page_links=2"), "log: {log}");
        assert!(log.contains("page_title_bytes=9"), "log: {log}");
        for secret in ["secret-signature", "cdn.example.com/a.bin", "b.zip"] {
            assert!(!log.contains(secret), "log leaked {secret}: {log}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_ai_parse_drops_page_snapshot_when_invalid_or_foreign() {
        let (logger, dir) = test_logger();
        for page_url in [
            "https://evil.example/page",    // different host
            "https://sub.example.com/page", // subdomain is a different host
            "file:///C:/Windows/win.ini",   // non-http(s)
            "javascript:alert(1)",
            "not a url",
        ] {
            let validated = ai_request_from_json(&format!(
                r#"{{"type":"ai_parse","url":"https://example.com/page",
                    "page_url":"{page_url}","page_title":"t",
                    "page_links":["https://cdn.example.com/a.bin"]}}"#
            ))
            .validate(&logger)
            .expect("a bad snapshot never fails the parse");
            assert_eq!(validated.page_url, None, "{page_url}");
            assert_eq!(validated.page_title, None, "{page_url}");
            assert!(validated.page_links.is_empty(), "{page_url}");
            assert_eq!(
                validated.url, "https://example.com/page",
                "the parse URL itself is unchanged"
            );
        }

        // Oversized page URL (> MAX_PAGE_URL_BYTES) drops the whole group too.
        let oversized = format!(
            r#"{{"type":"ai_parse","url":"https://example.com/page",
                "page_url":"https://example.com/{}",
                "page_links":["https://cdn.example.com/a.bin"]}}"#,
            "a".repeat(MAX_PAGE_URL_BYTES)
        );
        let validated = ai_request_from_json(&oversized)
            .validate(&logger)
            .expect("valid ai_parse");
        assert!(validated.page_url.is_none());
        assert!(validated.page_links.is_empty());

        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("page snapshot dropped"), "log: {log}");
        // Hosts may appear (diagnostics); paths/query values must not.
        assert!(!log.contains("win.ini"), "log: {log}");
        assert!(!log.contains("cdn.example.com/a.bin"), "log: {log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_ai_parse_filters_page_links() {
        let (logger, dir) = test_logger();
        let mut links: Vec<String> = (0..(MAX_PAGE_LINKS + 5))
            .map(|index| format!("https://cdn.example.com/{index}.bin"))
            .collect();
        links.push("https://cdn.example.com/0.bin".to_owned()); // duplicate
        links.push("/relative.bin".to_owned());
        links.push("javascript:alert(1)".to_owned());
        links.push(format!(
            "https://cdn.example.com/{}",
            "x".repeat(MAX_PAGE_LINK_BYTES)
        ));
        let json = serde_json::json!({
            "type": "ai_parse",
            "url": "https://example.com/page",
            "page_url": "https://example.com/page",
            "page_links": links,
        })
        .to_string();

        let validated = ai_request_from_json(&json)
            .validate(&logger)
            .expect("valid ai_parse");
        assert_eq!(validated.page_links.len(), MAX_PAGE_LINKS);
        assert_eq!(
            validated.page_links[0],
            "https://cdn.example.com/0.bin".to_owned(),
            "dedupe keeps the first occurrence"
        );
        assert!(validated
            .page_links
            .iter()
            .all(|link| link.starts_with("https://") && link.len() <= MAX_PAGE_LINK_BYTES));
        assert!(!validated
            .page_links
            .iter()
            .any(|link| link.contains("relative")));

        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("page_links dropped"), "log: {log}");
        assert!(!log.contains("cdn.example.com/0.bin"), "log: {log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_ai_parse_drops_page_links_without_page_url() {
        let (logger, dir) = test_logger();
        let validated = ai_request_from_json(
            r#"{"type":"ai_parse","url":"https://example.com/page",
                "page_links":["https://cdn.example.com/a.bin"]}"#,
        )
        .validate(&logger)
        .expect("valid ai_parse");
        assert!(validated.page_url.is_none());
        assert!(validated.page_title.is_none());
        assert!(
            validated.page_links.is_empty(),
            "links without a snapshot page have no attribution"
        );
        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("without a page_url"), "log: {log}");
        assert!(!log.contains("cdn.example.com"), "log: {log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ai_request_from_json_defaults_page_snapshot_fields() {
        // Old extensions/hosts omit the v0.3 fields entirely.
        let request = ai_request_from_json(r#"{"type":"ai_parse","url":"https://example.com/x"}"#);
        assert!(request.page_url.is_none());
        assert!(request.page_title.is_none());
        assert!(request.page_links.is_empty());

        let (logger, dir) = test_logger();
        let validated = request.validate(&logger).expect("valid ai_parse");
        assert!(validated.page_url.is_none());
        assert!(validated.page_title.is_none());
        assert!(validated.page_links.is_empty());
        assert!(validated.url == "https://example.com/x");
        let log = std::fs::read_to_string(logger.path()).unwrap();
        assert!(log.contains("page_links=0"), "log: {log}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ai_parse_accepted_response_shape_matches_protocol_document() {
        let accepted = Outgoing::ai_parse_accepted(7, 12, true);
        let json: serde_json::Value = serde_json::from_slice(&accepted.to_json()).unwrap();
        assert_eq!(json["type"], "ai_parse_accepted");
        assert_eq!(json["id"], 7);
        assert_eq!(json["ok"], true);
        assert_eq!(json["request_id"], 12);
        assert_eq!(json["gui_running"], true);

        let without_id = Outgoing::ai_parse_accepted(8, 0, false);
        let json: serde_json::Value = serde_json::from_slice(&without_id.to_json()).unwrap();
        assert_eq!(json["request_id"], 0);
        assert_eq!(json["gui_running"], false);
    }
}
