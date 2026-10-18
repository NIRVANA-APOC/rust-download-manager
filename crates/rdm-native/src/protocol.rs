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
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Ping {
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
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Pong {
        id: u64,
        version: String,
        gui_running: bool,
        protocol: u32,
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
    Error {
        id: u64,
        code: String,
        message: String,
    },
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

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| {
            // Serialization of this enum cannot realistically fail; keep the
            // protocol usable even if it somehow does.
            br#"{"type":"error","id":0,"code":"internal","message":"failed to encode response"}"#
                .to_vec()
        })
    }
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
    }
}
