//! Browser page snapshot bridge (P4b-b1, `docs/architecture.md` §11.11 v0.13).
//!
//! A local GUI / native bridge may hand the resolver a **link snapshot** of the
//! page it could see in the browser (title + links only — never HTML/body).
//! Seeded exploration uses that snapshot as the synthetic page observation, so
//! links behind a login page can participate in evidence binding without the
//! resolver fetching the page itself.
//!
//! The snapshot is untrusted input from the page: it is validated here, but its
//! content is only ever treated as data (wrapped in `<untrusted_content>` in the
//! exploration prompt) and every emitted URL is still re-verified locally.
//!
//! This type deliberately does **not** implement `Serialize`/`Deserialize` and
//! is never part of `ResolveRequest`; CLI/MCP cannot set it.

use url::Url;

/// Maximum number of links kept from one snapshot.
pub const MAX_SNAPSHOT_LINKS: usize = 300;
/// Maximum title length in characters.
pub const MAX_SNAPSHOT_TITLE: usize = 300;
/// Maximum page URL length in characters.
pub const MAX_SNAPSHOT_URL: usize = 2048;
/// Maximum single link length in characters.
pub const MAX_SNAPSHOT_LINK: usize = 2048;
/// How many links are shown in the initial exploration prompt.
pub const MAX_SNAPSHOT_PROMPT_LINKS: usize = 50;

/// Untrusted page snapshot supplied by the local browser bridge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageSnapshot {
    /// Absolute http(s) page URL (the request URL whose host it belongs to).
    pub url: String,
    pub title: Option<String>,
    /// Absolute http(s) links scraped by the bridge, in page order.
    pub links: Vec<String>,
}

impl PageSnapshot {
    /// Normalize untrusted input: filter non-http(s)/relative links, trim,
    /// deduplicate (keeping first position), enforce the size limits and
    /// truncate the title. An invalid page URL becomes an empty string (then
    /// [`Self::is_valid_for`] is `false`).
    pub fn normalized(self) -> Self {
        let url = match Url::parse(self.url.trim()) {
            Ok(parsed)
                if matches!(parsed.scheme(), "http" | "https")
                    && !parsed.host_str().unwrap_or_default().is_empty()
                    && self.url.trim().chars().count() <= MAX_SNAPSHOT_URL =>
            {
                parsed.to_string()
            }
            _ => String::new(),
        };
        let title = self
            .title
            .map(|title| title.trim().to_owned())
            .filter(|title| !title.is_empty())
            .map(|title| truncate_chars(&title, MAX_SNAPSHOT_TITLE));
        let mut links: Vec<String> = Vec::new();
        for link in self.links {
            let trimmed = link.trim();
            if trimmed.is_empty() || trimmed.chars().count() > MAX_SNAPSHOT_LINK {
                continue;
            }
            let Ok(parsed) = Url::parse(trimmed) else {
                continue;
            };
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none_or(str::is_empty)
            {
                continue;
            }
            let normalized = parsed.to_string();
            if !links.contains(&normalized) {
                links.push(normalized);
            }
            if links.len() >= MAX_SNAPSHOT_LINKS {
                break;
            }
        }
        Self { url, title, links }
    }

    /// Whether the snapshot belongs to `request_url`: both must be http(s) and
    /// the hosts must match (lowercase, trailing dot ignored).
    pub fn is_valid_for(&self, request_url: &Url) -> bool {
        if !matches!(request_url.scheme(), "http" | "https") {
            return false;
        }
        let Ok(snapshot_url) = Url::parse(&self.url) else {
            return false;
        };
        if !matches!(snapshot_url.scheme(), "http" | "https") {
            return false;
        }
        let (Some(snapshot_host), Some(request_host)) =
            (snapshot_url.host_str(), request_url.host_str())
        else {
            return false;
        };
        normalize_host(snapshot_host) == normalize_host(request_host)
    }

    /// Whether `url` addresses the snapshot page itself (scheme, host, port,
    /// path and query all equal). Used by the seeded `page_read` shortcut.
    pub fn matches_url(&self, url: &Url) -> bool {
        let Ok(snapshot_url) = Url::parse(&self.url) else {
            return false;
        };
        if snapshot_url.scheme() != url.scheme() {
            return false;
        }
        let (Some(snapshot_host), Some(url_host)) = (snapshot_url.host_str(), url.host_str())
        else {
            return false;
        };
        if normalize_host(snapshot_host) != normalize_host(url_host) {
            return false;
        }
        if snapshot_url.port() != url.port() {
            return false;
        }
        snapshot_url.path() == url.path() && snapshot_url.query() == url.query()
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    // Exactly `max_chars` characters, no ellipsis: the contract caps the title
    // at 300 chars and callers assert that prefix is sent verbatim.
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Url {
        Url::parse(raw).expect("test URL")
    }

    #[test]
    fn normalization_filters_and_deduplicates_links() {
        let snapshot = PageSnapshot {
            url: " HTTPS://Example.com/page?x=1 ".to_owned(),
            title: Some("  Demo  ".to_owned()),
            links: vec![
                "https://cdn.example.com/a.bin".to_owned(),
                "https://cdn.example.com/a.bin".to_owned(),
                "/relative.bin".to_owned(),
                "javascript:alert(1)".to_owned(),
                "mailto:x@example.com".to_owned(),
                "file:///etc/passwd".to_owned(),
                "  https://cdn.example.com/b.bin?y=2  ".to_owned(),
                "".to_owned(),
            ],
        }
        .normalized();
        assert_eq!(snapshot.url, "https://example.com/page?x=1");
        assert_eq!(snapshot.title.as_deref(), Some("Demo"));
        assert_eq!(
            snapshot.links,
            vec![
                "https://cdn.example.com/a.bin".to_owned(),
                "https://cdn.example.com/b.bin?y=2".to_owned(),
            ]
        );
    }

    #[test]
    fn invalid_page_url_is_dropped_and_links_still_normalized() {
        for invalid in ["", "not a url", "/relative", "ftp://example.com/x"] {
            let snapshot = PageSnapshot {
                url: invalid.to_owned(),
                title: None,
                links: vec!["https://example.com/a".to_owned()],
            }
            .normalized();
            assert!(snapshot.url.is_empty(), "{invalid:?}");
            assert!(!snapshot.is_valid_for(&parse("https://example.com/")));
            assert_eq!(snapshot.links.len(), 1);
        }
    }

    #[test]
    fn limits_truncate_title_links_and_lengths() {
        let long_title = "t".repeat(MAX_SNAPSHOT_TITLE + 50);
        let mut links = Vec::new();
        for index in 0..(MAX_SNAPSHOT_LINKS + 25) {
            links.push(format!("https://cdn.example.com/{index}.bin"));
        }
        links.push(format!(
            "https://cdn.example.com/{}",
            "x".repeat(MAX_SNAPSHOT_LINK)
        ));
        let snapshot = PageSnapshot {
            url: "https://example.com/".to_owned(),
            title: Some(long_title),
            links,
        }
        .normalized();
        assert_eq!(
            snapshot.title.as_deref().unwrap().chars().count(),
            MAX_SNAPSHOT_TITLE
        );
        assert_eq!(snapshot.links.len(), MAX_SNAPSHOT_LINKS);
        // The oversized link is dropped.
        assert!(snapshot
            .links
            .iter()
            .all(|link| link.chars().count() <= MAX_SNAPSHOT_LINK));
    }

    #[test]
    fn host_validation_is_exact_but_case_and_dot_insensitive() {
        let snapshot = PageSnapshot {
            url: "https://Example.COM./page".to_owned(),
            title: None,
            links: Vec::new(),
        }
        .normalized();
        assert!(snapshot.is_valid_for(&parse("https://example.com/file")));
        assert!(snapshot.is_valid_for(&parse("https://EXAMPLE.COM:8443/file")));
        assert!(!snapshot.is_valid_for(&parse("https://sub.example.com/file")));
        assert!(!snapshot.is_valid_for(&parse("https://other.com/file")));
        assert!(!snapshot.is_valid_for(&parse("ftp://example.com/file")));

        // Empty links snapshots are still valid.
        assert!(snapshot.links.is_empty());
        assert!(snapshot.is_valid_for(&parse("https://example.com/")));
    }

    #[test]
    fn matches_url_requires_scheme_host_port_path_and_query() {
        let snapshot = PageSnapshot {
            url: "https://example.com/page?a=1".to_owned(),
            title: None,
            links: Vec::new(),
        };
        assert!(snapshot.matches_url(&parse("https://example.com/page?a=1")));
        assert!(snapshot.matches_url(&parse("HTTPS://EXAMPLE.COM./page?a=1")));
        assert!(!snapshot.matches_url(&parse("https://example.com/page?a=2")));
        assert!(!snapshot.matches_url(&parse("https://example.com/other?a=1")));
        assert!(!snapshot.matches_url(&parse("http://example.com/page?a=1")));
        // The contract compares scheme/host/port/path/query only; a fragment on
        // the same page still identifies the snapshot page.
        assert!(snapshot.matches_url(&parse("https://example.com/page?a=1#frag")));
        assert!(!snapshot.matches_url(&parse("https://example.com:8443/page?a=1")));
    }
}
