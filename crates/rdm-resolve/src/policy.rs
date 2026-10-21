//! Network policy and budget enforcement for the resolver.
//!
//! Mirrors `rdm_agent::Policy` semantics (host allow-list, private hosts denied
//! by default) but stays independent so the resolver can be tested without the
//! agent contract crate.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use url::Url;

use crate::contract::{Budget, ResolveError, ResolveErrorKind};

/// Network scope allowed for one resolver instance.
#[derive(Debug, Clone)]
pub struct ResolvePolicy {
    /// `*` allows everything; `*.example.com` matches subdomains *and* the apex.
    pub allowed_hosts: Vec<String>,
    /// Loopback / private / link-local targets are denied unless this is `true`.
    pub allow_private_hosts: bool,
    /// Upper bound for redirect hops (also capped by [`Budget::redirects`]).
    pub max_redirects: u32,
}

impl Default for ResolvePolicy {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["*".to_owned()],
            allow_private_hosts: false,
            max_redirects: 5,
        }
    }
}

impl ResolvePolicy {
    /// Permissive policy for local fixture servers (`127.0.0.1` allowed).
    pub fn loopback() -> Self {
        Self {
            allowed_hosts: vec!["*".to_owned()],
            allow_private_hosts: true,
            max_redirects: 5,
        }
    }

    /// Check scheme, host allow-list and private-address rules.
    pub fn check_url(&self, url: &Url) -> Result<(), ResolveError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ResolveError::new(
                ResolveErrorKind::UnsupportedScheme,
                format!("unsupported scheme: {}", url.scheme()),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| ResolveError::new(ResolveErrorKind::InvalidUrl, "URL has no host"))?;
        if !self.host_allowed(host) {
            return Err(ResolveError::new(
                ResolveErrorKind::PolicyDenied,
                format!("host not allowed by policy: {host}"),
            ));
        }
        if !self.allow_private_hosts && is_private_host(host) {
            return Err(ResolveError::new(
                ResolveErrorKind::PolicyDenied,
                format!("private/loopback host denied: {host}"),
            ));
        }
        Ok(())
    }

    /// Exact host, `*.suffix`, or `*`.
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.allowed_hosts.iter().any(|pattern| {
            let pattern = pattern.trim().to_ascii_lowercase();
            if pattern == "*" {
                return true;
            }
            if let Some(suffix) = pattern.strip_prefix("*.") {
                return host == suffix || host.ends_with(&format!(".{suffix}"));
            }
            host == pattern
        })
    }
}

/// Loopback / private / link-local detection.
///
/// Only literal addresses and well-known local names are checked here; a public
/// hostname that resolves to a private address (DNS rebinding) is *not* caught.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    // `Url::host_str()` keeps the brackets around IPv6 literals (`[::1]`).
    let literal = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(&host);
    let Ok(ip) = literal.parse::<IpAddr>() else {
        return false;
    };
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local fe80::/10
        }
    }
}

/// Counters + wall-clock deadline for one run.
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    budget: Budget,
    started: Instant,
    requests: u32,
    bytes: u64,
    redirects: u32,
}

impl BudgetTracker {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            started: Instant::now(),
            requests: 0,
            bytes: 0,
            redirects: 0,
        }
    }

    pub fn budget(&self) -> Budget {
        self.budget
    }

    pub fn requests(&self) -> u32 {
        self.requests
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Reserve one HTTP request (including redirect hops).
    pub fn take_request(&mut self) -> Result<(), ResolveError> {
        self.check_deadline()?;
        if self.requests >= self.budget.http_requests {
            return Err(ResolveError::new(
                ResolveErrorKind::BudgetExhausted,
                format!("http request budget exhausted ({} requests)", self.requests),
            ));
        }
        self.requests += 1;
        Ok(())
    }

    /// Reserve one redirect hop.
    pub fn take_redirect(&mut self, max_redirects: u32) -> Result<(), ResolveError> {
        self.check_deadline()?;
        let limit = self.budget.redirects.min(max_redirects);
        if self.redirects >= limit {
            return Err(ResolveError::new(
                ResolveErrorKind::BudgetExhausted,
                format!("redirect budget exhausted ({limit} hops)"),
            ));
        }
        self.redirects += 1;
        Ok(())
    }

    /// Account bytes read; fails (and stops the run) when the cap is exceeded.
    pub fn add_bytes(&mut self, bytes: u64) -> Result<(), ResolveError> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.budget.bytes_read {
            return Err(ResolveError::new(
                ResolveErrorKind::BudgetExhausted,
                format!(
                    "byte budget exhausted ({}/{} bytes)",
                    self.bytes, self.budget.bytes_read
                ),
            ));
        }
        Ok(())
    }

    pub fn check_deadline(&self) -> Result<(), ResolveError> {
        if self.started.elapsed().as_millis() as u64 > self.budget.wall_ms {
            return Err(ResolveError::new(
                ResolveErrorKind::BudgetExhausted,
                format!("wall-clock budget exhausted ({} ms)", self.budget.wall_ms),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    #[test]
    fn wildcard_and_suffix_hosts() {
        let policy = ResolvePolicy {
            allowed_hosts: vec!["example.com".to_owned(), "*.cdn.example.com".to_owned()],
            ..ResolvePolicy::default()
        };
        assert!(policy.host_allowed("example.com"));
        assert!(policy.host_allowed("cdn.example.com"));
        assert!(policy.host_allowed("a.cdn.example.com"));
        assert!(!policy.host_allowed("evil.com"));
        assert!(!policy.host_allowed("example.com.evil.com"));
    }

    #[test]
    fn private_hosts_are_denied_by_default() {
        let policy = ResolvePolicy::default();
        assert!(policy.check_url(&url("http://127.0.0.1:8080/x")).is_err());
        assert!(policy.check_url(&url("http://localhost/x")).is_err());
        assert!(policy.check_url(&url("http://10.0.0.5/x")).is_err());
        assert!(policy.check_url(&url("http://[::1]/x")).is_err());
        assert!(policy.check_url(&url("https://example.com/x")).is_ok());
        let loopback = ResolvePolicy::loopback();
        assert!(loopback.check_url(&url("http://127.0.0.1:8080/x")).is_ok());
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        let policy = ResolvePolicy::loopback();
        assert!(policy.check_url(&url("ftp://example.com/x")).is_err());
        assert!(policy.check_url(&url("file:///etc/passwd")).is_err());
    }

    #[test]
    fn budget_stops_requests_bytes_and_time() {
        let mut tracker = BudgetTracker::new(Budget {
            wall_ms: 60_000,
            http_requests: 2,
            bytes_read: 10,
            redirects: 1,
        });
        assert!(tracker.take_request().is_ok());
        assert!(tracker.take_request().is_ok());
        assert!(tracker.take_request().is_err());
        assert!(tracker.add_bytes(11).is_err());
    }
}
