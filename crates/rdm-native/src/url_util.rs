//! URL helpers shared by validation and logging.
//!
//! Downloads handed over by the browser must be plain `http(s)` URLs: `file:`,
//! `data:`, `blob:` and friends are rejected so an extension (or a compromised
//! page) cannot ask the download engine to read local files.

use url::Url;

/// Upper bound for an accepted download URL. Anything longer is almost
/// certainly an attack or a runaway `data:` URL.
pub const MAX_URL_BYTES: usize = 8 * 1024;

/// Parse and validate a browser-supplied download URL.
pub fn parse_http_url(raw: &str) -> Result<Url, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("url is empty".to_string());
    }
    if trimmed.len() > MAX_URL_BYTES {
        return Err(format!("url is longer than {MAX_URL_BYTES} bytes"));
    }
    let url = Url::parse(trimmed).map_err(|e| format!("invalid url: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        // `blob:` URLs have an inner http URL, but the browser already holds
        // the bytes; handing them to another process would not work anyway.
        other => {
            return Err(format!(
                "unsupported url scheme \"{other}\" (only http/https can be handed to RDM)"
            ))
        }
    }
    if url.host_str().is_none() {
        return Err("url has no host".to_string());
    }
    Ok(url)
}

/// Redact a URL for **log files**: drop credentials, query string and fragment.
///
/// Download URLs commonly carry expiring signatures (`?token=…`,
/// `?X-Amz-Signature=…`); those must never end up in a log file.
///
/// Named `*_for_log` (audit 2026-10-22 F-23) because `rdm-agent` has an
/// identically-named helper with the opposite contract: its `redact_url_userinfo`
/// keeps query/fragment so the agent can still download the URL. Never use this
/// function for URL handoff, and never use the agent one for log lines.
pub fn redact_url_for_log(raw: &str) -> String {
    match Url::parse(raw.trim()) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "<unparsable-url>".to_string(),
    }
}

/// Best-effort `host[:port]` extraction used in log lines about cookies.
pub fn host_of(raw: &str) -> Option<String> {
    Url::parse(raw.trim())
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_and_https() {
        assert!(parse_http_url("https://example.com/big.iso?token=1").is_ok());
        assert!(parse_http_url("http://127.0.0.1:8080/file").is_ok());
        assert!(parse_http_url("  https://example.com/a  ").is_ok());
    }

    #[test]
    fn rejects_dangerous_or_malformed_schemes() {
        for raw in [
            "",
            "file:///C:/Windows/win.ini",
            "data:text/plain,hi",
            "blob:https://example.com/1234",
            "javascript:alert(1)",
            "ftp://example.com/x",
            "not a url",
            "https://",
        ] {
            assert!(parse_http_url(raw).is_err(), "{raw:?} must be rejected");
        }
    }

    #[test]
    fn rejects_overlong_urls() {
        let raw = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(parse_http_url(&raw).is_err());
    }

    #[test]
    fn log_redaction_strips_query_fragment_and_credentials() {
        // F-23: this function is log-only; query/fragment must be gone.
        let redacted =
            redact_url_for_log("https://user:pass@example.com:8443/a/b.iso?sig=secret#frag");
        assert_eq!(redacted, "https://example.com:8443/a/b.iso");
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("pass"));
        assert_eq!(
            redact_url_for_log("nonsense"),
            "<unparsable-url>".to_string()
        );
    }

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("https://Download.Example.COM/x").as_deref(),
            Some("download.example.com")
        );
        assert_eq!(host_of("nope"), None);
    }
}
