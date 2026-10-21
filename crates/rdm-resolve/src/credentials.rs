//! Per-host credentials bridge (P4a, `docs/architecture.md` §11.12).
//!
//! Local entry points (GUI / native bridge) may hold one-shot cookies and
//! related headers captured in a browser context. [`HostCredentials`] carries
//! them **in memory only** and scopes them to exact hosts; [`crate::http::HttpTool`]
//! injects them into the initial request URL only (never into redirects to a
//! different origin, never into exploration).
//!
//! Deliberate properties:
//!
//! * **not serializable** — there is no `Serialize`/`Deserialize` impl, so a
//!   credential can never be written to `ai.json`, the task store, a trace or a
//!   draft by accident;
//! * **redacted Debug** — only the host count is rendered, never a value;
//! * bounded: [`MAX_CREDENTIAL_HOSTS`] hosts, [`MAX_CREDENTIAL_HEADERS_PER_HOST`]
//!   headers per host and [`MAX_CREDENTIAL_VALUE_BYTES`] per value; oversized
//!   inserts are ignored with a warning;
//! * hop-by-hop headers (`host`, `content-length`, `connection`, …) are filtered
//!   at insert time.

use url::Url;

/// Maximum number of hosts carried by one [`HostCredentials`].
pub const MAX_CREDENTIAL_HOSTS: usize = 32;
/// Maximum number of headers accepted per host.
pub const MAX_CREDENTIAL_HEADERS_PER_HOST: usize = 16;
/// Maximum accepted header value length in bytes.
pub const MAX_CREDENTIAL_VALUE_BYTES: usize = 8 * 1024;

/// Headers that must never be injected from the credential bridge.
const HOP_BY_HOP_HEADERS: [&str; 7] = [
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
];

/// In-memory, host-scoped credential headers (never serialized).
#[derive(Clone, Default)]
pub struct HostCredentials {
    /// `(normalized_host, [(header_name, value)])`; names keep the caller's
    /// casing after the first insert, values are never rendered by `Debug`.
    entries: Vec<(String, Vec<(String, String)>)>,
}

impl HostCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `headers` for `host` (other hosts keep their own entries).
    ///
    /// Invalid hosts/headers and limits are ignored with a `tracing::warn!`
    /// instead of failing: the bridge is best-effort and must never break a
    /// resolution by itself. A later header with the same case-insensitive name
    /// replaces the earlier value.
    pub fn insert(&mut self, host: &str, headers: Vec<(String, String)>) {
        let Some(host) = normalize_host(host) else {
            tracing::warn!("rdm-resolve: ignoring credentials for an empty host");
            return;
        };
        let filtered = filter_headers(headers);
        if filtered.is_empty() {
            return;
        }
        let Some(index) = self.entries.iter().position(|(entry, _)| entry == &host) else {
            if self.entries.len() >= MAX_CREDENTIAL_HOSTS {
                tracing::warn!(
                    host = %host,
                    limit = MAX_CREDENTIAL_HOSTS,
                    "rdm-resolve: ignoring credentials beyond the host limit"
                );
                return;
            }
            let merged = merge_headers(Vec::new(), filtered);
            if merged.is_empty() {
                return;
            }
            self.entries.push((host, merged));
            return;
        };
        let merged = merge_headers(self.entries[index].1.clone(), filtered);
        if merged.is_empty() {
            return;
        }
        self.entries[index].1 = merged;
    }

    /// Chainable convenience: `HostCredentials::new().with_cookie(host, cookie)`.
    pub fn with_cookie(mut self, host: &str, cookie: impl Into<String>) -> Self {
        self.insert(host, vec![("Cookie".to_owned(), cookie.into())]);
        self
    }

    /// Chainable convenience for one extra header (e.g. `Referer`, `User-Agent`).
    pub fn with_header(
        mut self,
        host: &str,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.insert(host, vec![(name.into(), value.into())]);
        self
    }

    /// Chainable convenience for a header list.
    pub fn with_host(mut self, host: &str, headers: Vec<(String, String)>) -> Self {
        self.insert(host, headers);
        self
    }

    /// Headers to inject for `url`'s host (empty for other hosts / non-HTTP(S)).
    ///
    /// Host matching is exact, case-insensitive and ignores a trailing dot;
    /// ports are part of the URL authority but not of the host key.
    pub fn for_url(&self, url: &Url) -> Vec<(String, String)> {
        if url.scheme() != "http" && url.scheme() != "https" {
            return Vec::new();
        }
        let Some(host) = url.host_str().and_then(normalize_host) else {
            return Vec::new();
        };
        self.entries
            .iter()
            .find(|(entry, _)| entry == &host)
            .map(|(_, headers)| headers.clone())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of hosts with at least one header.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl std::fmt::Debug for HostCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCredentials")
            .field("hosts", &self.entries.len())
            .field("is_empty", &self.is_empty())
            .finish()
    }
}

/// Lowercase + trim + drop one trailing dot; `None` when nothing is left.
fn normalize_host(host: &str) -> Option<String> {
    let normalized = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn filter_headers(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut filtered = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let name = name.trim().to_owned();
        if name.is_empty() {
            continue;
        }
        if HOP_BY_HOP_HEADERS
            .iter()
            .any(|hop_by_hop| name.eq_ignore_ascii_case(hop_by_hop))
        {
            tracing::warn!(
                header = %name,
                "rdm-resolve: ignoring hop-by-hop credential header"
            );
            continue;
        }
        if value.len() > MAX_CREDENTIAL_VALUE_BYTES {
            tracing::warn!(
                header = %name,
                bytes = value.len(),
                limit = MAX_CREDENTIAL_VALUE_BYTES,
                "rdm-resolve: ignoring oversized credential header value"
            );
            continue;
        }
        if value.trim().is_empty() {
            continue;
        }
        filtered.push((name, value));
    }
    filtered
}

/// Merge new headers into an existing list (case-insensitive, later wins),
/// enforcing [`MAX_CREDENTIAL_HEADERS_PER_HOST`]. Returns the unchanged input
/// when the merged result would exceed the limit.
fn merge_headers(
    mut existing: Vec<(String, String)>,
    incoming: Vec<(String, String)>,
) -> Vec<(String, String)> {
    for (name, value) in incoming {
        existing.retain(|(current, _)| !current.eq_ignore_ascii_case(&name));
        if existing.len() >= MAX_CREDENTIAL_HEADERS_PER_HOST {
            tracing::warn!(
                limit = MAX_CREDENTIAL_HEADERS_PER_HOST,
                "rdm-resolve: ignoring credentials beyond the per-host header limit"
            );
            return Vec::new();
        }
        existing.push((name, value));
    }
    existing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test URL")
    }

    #[test]
    fn empty_by_default_and_empty_credentials_are_truthful() {
        let credentials = HostCredentials::new();
        assert!(credentials.is_empty());
        assert_eq!(credentials.len(), 0);
        assert!(credentials
            .for_url(&url("https://example.com/a"))
            .is_empty());
        assert!(HostCredentials::default().is_empty());
    }

    #[test]
    fn host_normalization_and_exact_matching() {
        let credentials = HostCredentials::new()
            .with_cookie(" Example.COM. ", "session=abc")
            .with_header("api.example.com", "Referer", "https://example.com/page");
        assert_eq!(credentials.len(), 2);

        let headers = credentials.for_url(&url("https://example.com/file"));
        assert_eq!(
            headers,
            vec![("Cookie".to_owned(), "session=abc".to_owned())]
        );
        // Case-insensitive + trailing-dot host normalization.
        assert_eq!(credentials.for_url(&url("HTTPS://EXAMPLE.COM./x")), headers);
        // Ports are not part of the host key; other hosts get nothing.
        assert_eq!(
            credentials.for_url(&url("https://example.com:8443/x")),
            headers
        );
        assert!(credentials
            .for_url(&url("https://other.example.com/x"))
            .is_empty());
        assert!(credentials
            .for_url(&url("https://sub.example.com/x"))
            .is_empty());
        // Non-HTTP(S) schemes are never injected.
        assert!(credentials.for_url(&url("file:///tmp/x")).is_empty());
        assert!(credentials.for_url(&url("ftp://example.com/x")).is_empty());
    }

    #[test]
    fn later_headers_win_case_insensitively() {
        let mut credentials = HostCredentials::new();
        credentials.insert(
            "example.com",
            vec![
                ("Cookie".to_owned(), "a=1".to_owned()),
                ("user-agent".to_owned(), "one".to_owned()),
            ],
        );
        credentials.insert(
            "example.com",
            vec![
                ("cookie".to_owned(), "b=2".to_owned()),
                ("User-Agent".to_owned(), "two".to_owned()),
            ],
        );
        let headers = credentials.for_url(&url("https://example.com/"));
        assert_eq!(headers.len(), 2, "{headers:?}");
        assert_eq!(headers[0].1, "b=2");
        assert_eq!(headers[1].1, "two");
    }

    #[test]
    fn hop_by_hop_and_invalid_headers_are_filtered() {
        let credentials = HostCredentials::new().with_host(
            "example.com",
            vec![
                ("Host".to_owned(), "evil".to_owned()),
                ("Content-Length".to_owned(), "999".to_owned()),
                ("connection".to_owned(), "close".to_owned()),
                ("transfer-encoding".to_owned(), "chunked".to_owned()),
                ("upgrade".to_owned(), "h2c".to_owned()),
                ("te".to_owned(), "trailers".to_owned()),
                ("trailer".to_owned(), "x".to_owned()),
                ("".to_owned(), "nameless".to_owned()),
                (" Cookie ".to_owned(), "session=ok".to_owned()),
            ],
        );
        let headers = credentials.for_url(&url("https://example.com/"));
        assert_eq!(
            headers,
            vec![("Cookie".to_owned(), "session=ok".to_owned())]
        );
    }

    #[test]
    fn limits_are_enforced_and_ignored() {
        // Per-host header limit: 16 accepted, the 17th insert is ignored.
        let mut credentials = HostCredentials::new();
        let mut accepted = Vec::new();
        for index in 0..MAX_CREDENTIAL_HEADERS_PER_HOST {
            accepted.push((format!("x-header-{index}"), format!("value-{index}")));
        }
        credentials.insert("example.com", accepted);
        assert_eq!(
            credentials.for_url(&url("https://example.com/")).len(),
            MAX_CREDENTIAL_HEADERS_PER_HOST
        );
        credentials.insert(
            "example.com",
            vec![("x-one-too-many".to_owned(), "v".to_owned())],
        );
        assert_eq!(
            credentials.for_url(&url("https://example.com/")).len(),
            MAX_CREDENTIAL_HEADERS_PER_HOST,
            "an over-limit insert is ignored"
        );

        // Oversized value is dropped, not stored.
        credentials.insert(
            "example.com",
            vec![(
                "x-huge".to_owned(),
                "x".repeat(MAX_CREDENTIAL_VALUE_BYTES + 1),
            )],
        );
        assert!(credentials
            .for_url(&url("https://example.com/"))
            .iter()
            .all(|(name, _)| name != "x-huge"));
    }

    #[test]
    fn host_limit_is_enforced() {
        let mut credentials = HostCredentials::new();
        for index in 0..MAX_CREDENTIAL_HOSTS {
            credentials.insert(
                &format!("host{index}.example.com"),
                vec![("cookie".to_owned(), format!("session={index}"))],
            );
        }
        assert_eq!(credentials.len(), MAX_CREDENTIAL_HOSTS);
        credentials.insert(
            "overflow.example.com",
            vec![("cookie".to_owned(), "session=overflow".to_owned())],
        );
        assert_eq!(credentials.len(), MAX_CREDENTIAL_HOSTS);
        assert!(credentials
            .for_url(&url("https://overflow.example.com/"))
            .is_empty());
    }

    #[test]
    fn debug_never_renders_values() {
        let credentials = HostCredentials::new()
            .with_cookie("example.com", "session=super-secret")
            .with_header("example.com", "Authorization", "Bearer top-secret");
        let rendered = format!("{credentials:?}");
        assert!(rendered.contains("HostCredentials"), "{rendered}");
        assert!(rendered.contains("hosts"), "{rendered}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("top-secret"), "{rendered}");
        assert!(!rendered.contains("session="), "{rendered}");
    }
}
