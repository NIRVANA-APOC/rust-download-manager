//! Resource verification: decide whether a candidate URL really is a file the
//! download engine can consume.
//!
//! The engine itself does not inspect Content-Type or the body
//! (`docs/research/resolve-to-download-contract.md` §Q5), so this module is the
//! only place that can reject a login page dressed up as a download.

use sha2::{Digest, Sha256};

use crate::contract::{now_ms, Verification, VerificationMethod, VerificationStatus};
use crate::http::{looks_like_html, HttpRequestSpec, HttpTool, Observation};

/// Hints that change how expensive probing may be.
#[derive(Debug, Clone)]
pub struct VerifyHints {
    /// The URL may be consumed by its first request: never issue a body-range
    /// probe, settle for a HEAD-only verdict.
    pub one_time: bool,
    /// Bytes requested for the bounded body probe.
    pub max_prefix_bytes: usize,
    /// Reject files larger than this when a length is advertised.
    pub max_size_bytes: Option<u64>,
}

impl Default for VerifyHints {
    fn default() -> Self {
        Self {
            one_time: false,
            max_prefix_bytes: 1024,
            max_size_bytes: None,
        }
    }
}

/// Verifier bound to one [`HttpTool`] (and therefore one budget).
pub struct Verifier<'a> {
    http: &'a mut HttpTool,
}

impl<'a> Verifier<'a> {
    pub fn new(http: &'a mut HttpTool) -> Self {
        Self { http }
    }

    /// Probe `url` and return the evidence-backed verdict.
    pub async fn verify(&mut self, url: &str, hints: &VerifyHints) -> Verification {
        let mut verification = Verification {
            checked_ms: now_ms(),
            ..Verification::default()
        };

        let head = self
            .http
            .request(HttpRequestSpec::head(url).max_bytes(0))
            .await;
        match head {
            Ok(observation) => {
                verification.evidence.push(head_evidence(&observation));
                apply_common(&mut verification, &observation);
                if head_requires_get_fallback(observation.status) {
                    // Some servers reject HEAD (405/501) but serve ranged GETs
                    // just fine, so try one bounded GET before giving up.
                    verification.evidence.push(format!(
                        "head rejected (status {}); falling back to a bounded get",
                        observation.status
                    ));
                    if hints.one_time {
                        // Never spend a one-time URL on a ranged probe.
                        verification.status = VerificationStatus::UnknownLength;
                        return verification;
                    }
                    return self.bounded_get(url, hints, verification).await;
                }
                if let Some(status) = status_verdict(observation.status, &observation) {
                    verification.status = status;
                    return verification;
                }
                if observation.looks_like_page() || is_page_content_type(observation.content_type())
                {
                    // `text/html` on the *final* URL after a redirect is the
                    // classic "login wall" shape.
                    verification.status = if observation.final_url != observation.request_url {
                        VerificationStatus::LoginRequired
                    } else {
                        VerificationStatus::HtmlPage
                    };
                    return verification;
                }
                let file_like = observation.content_type().is_some()
                    || observation.content_length().is_some()
                    || observation.header("content-disposition").is_some();
                if hints.one_time {
                    verification.method = VerificationMethod::HeadOnly;
                    verification.status = if file_like {
                        VerificationStatus::HeadOnlyVerified
                    } else {
                        VerificationStatus::UnknownLength
                    };
                    return verification;
                }
                if !file_like {
                    // No usable headers: spend one bounded GET to learn more.
                    return self.bounded_get(url, hints, verification).await;
                }
                self.bounded_get(url, hints, verification).await
            }
            Err(error) => {
                // Some servers reject HEAD; a bounded GET is still worth one try.
                if matches!(error.kind, crate::contract::ResolveErrorKind::PolicyDenied) {
                    verification.status = VerificationStatus::PolicyDenied;
                    verification
                        .evidence
                        .push(format!("policy: {}", error.detail));
                    return verification;
                }
                verification
                    .evidence
                    .push(format!("head failed: {:?} ({})", error.kind, error.detail));
                self.bounded_get(url, hints, verification).await
            }
        }
    }

    async fn bounded_get(
        &mut self,
        url: &str,
        hints: &VerifyHints,
        mut verification: Verification,
    ) -> Verification {
        let prefix = hints.max_prefix_bytes.max(1);
        let spec = HttpRequestSpec::get(url)
            .max_bytes(prefix)
            .header("range", format!("bytes=0-{}", prefix - 1));
        let observation = match self.http.request(spec).await {
            Ok(observation) => observation,
            Err(error) => {
                verification
                    .evidence
                    .push(format!("get failed: {:?} ({})", error.kind, error.detail));
                verification.status = match error.kind {
                    crate::contract::ResolveErrorKind::PolicyDenied => {
                        VerificationStatus::PolicyDenied
                    }
                    crate::contract::ResolveErrorKind::Timeout => VerificationStatus::NetworkError,
                    _ => VerificationStatus::NetworkError,
                };
                return verification;
            }
        };
        verification.evidence.push(get_evidence(&observation));
        apply_common(&mut verification, &observation);
        verification.method = if observation.status == 206 {
            VerificationMethod::RangedGet
        } else {
            VerificationMethod::BoundedGet
        };

        if let Some(status) = status_verdict(observation.status, &observation) {
            verification.status = status;
            return verification;
        }
        if observation.looks_like_page() || is_page_content_type(observation.content_type()) {
            verification.status = if observation.final_url != observation.request_url {
                VerificationStatus::LoginRequired
            } else {
                VerificationStatus::HtmlPage
            };
            return verification;
        }
        if observation.body.is_empty() {
            verification.status = match observation.content_length() {
                Some(0) => VerificationStatus::EmptyBody,
                _ => VerificationStatus::UnknownLength,
            };
            return verification;
        }
        if let Some(cap) = hints.max_size_bytes {
            if verification.size_bytes.is_some_and(|size| size > cap) {
                verification.evidence.push(format!(
                    "size {} exceeds cap {cap}",
                    verification.size_bytes.unwrap_or(0)
                ));
                verification.status = VerificationStatus::UnknownLength;
                return verification;
            }
        }
        verification.status = VerificationStatus::Verified;
        verification
    }
}

/// HEAD statuses that mean "method not supported", not "resource missing".
fn head_requires_get_fallback(status: u16) -> bool {
    matches!(status, 405 | 501)
}

fn head_evidence(observation: &Observation) -> String {
    format!(
        "head status={} content-type={} content-length={} accept-ranges={}",
        observation.status,
        observation.content_type().unwrap_or("-"),
        observation
            .content_length()
            .map_or_else(|| "-".to_owned(), |v| v.to_string()),
        observation
            .accepts_ranges()
            .map_or_else(|| "-".to_owned(), |v| v.to_string()),
    )
}

fn get_evidence(observation: &Observation) -> String {
    let digest = Sha256::digest(&observation.body);
    format!(
        "{} status={} bytes={} truncated={} prefix-sha256={}",
        if observation.status == 206 {
            "ranged-get"
        } else {
            "bounded-get"
        },
        observation.status,
        observation.body.len(),
        observation.truncated,
        hex::encode(digest),
    )
}

fn is_page_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("text/html")
            || value.contains("application/xhtml")
            || value.starts_with("text/")
    })
}

fn status_verdict(status: u16, observation: &Observation) -> Option<VerificationStatus> {
    match status {
        200 | 206 => None,
        401 => Some(VerificationStatus::LoginRequired),
        403 | 410 => Some(VerificationStatus::ForbiddenOrExpired),
        404 => Some(VerificationStatus::NotFound),
        429 => Some(VerificationStatus::RateLimited),
        status if (500..600).contains(&status) => Some(VerificationStatus::ServerError),
        status if (300..400).contains(&status) => Some(VerificationStatus::NotAFile),
        _ => {
            let _ = observation;
            Some(VerificationStatus::NotAFile)
        }
    }
}

fn apply_common(verification: &mut Verification, observation: &Observation) {
    verification.http_status = Some(observation.status);
    verification.final_url = Some(observation.final_url.clone());
    verification.content_type = observation.content_type().map(str::to_owned);
    verification.size_bytes = content_range_total(observation)
        .or_else(|| observation.content_length())
        .filter(|size| *size > 0 || observation.status == 200);
    verification.supports_ranges = observation.accepts_ranges().or({
        if observation.status == 206 {
            Some(true)
        } else {
            None
        }
    });
    verification.etag = observation.header("etag").map(str::to_owned);
    verification.last_modified = observation.header("last-modified").map(str::to_owned);
}

fn content_range_total(observation: &Observation) -> Option<u64> {
    let value = observation.header("content-range")?.trim();
    let total = value.rsplit_once('/')?.1.trim();
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

/// Convenience: does this body look like an HTML document?
pub fn body_looks_like_page(body: &[u8]) -> bool {
    looks_like_html(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Budget;
    use crate::http::HttpTool;
    use crate::policy::ResolvePolicy;

    #[test]
    fn content_range_total_is_parsed() {
        let mut observation = Observation {
            obs_id: "obs".to_owned(),
            request_url: "https://x/y".to_owned(),
            final_url: "https://x/y".to_owned(),
            status: 206,
            headers: Default::default(),
            body: vec![0u8; 4],
            truncated: false,
        };
        observation
            .headers
            .insert("content-range".to_owned(), "bytes 0-3/4096".to_owned());
        assert_eq!(content_range_total(&observation), Some(4096));
        observation
            .headers
            .insert("content-range".to_owned(), "bytes 0-3/*".to_owned());
        assert_eq!(content_range_total(&observation), None);
    }

    #[test]
    fn status_verdict_maps_common_failures() {
        let observation = Observation {
            obs_id: "obs".to_owned(),
            request_url: "https://x/y".to_owned(),
            final_url: "https://x/y".to_owned(),
            status: 200,
            headers: Default::default(),
            body: Vec::new(),
            truncated: false,
        };
        assert_eq!(
            status_verdict(401, &observation),
            Some(VerificationStatus::LoginRequired)
        );
        assert_eq!(
            status_verdict(404, &observation),
            Some(VerificationStatus::NotFound)
        );
        assert_eq!(
            status_verdict(429, &observation),
            Some(VerificationStatus::RateLimited)
        );
        assert_eq!(
            status_verdict(503, &observation),
            Some(VerificationStatus::ServerError)
        );
        assert_eq!(status_verdict(200, &observation), None);
    }

    #[test]
    fn verifier_can_be_constructed_with_a_budget() {
        let mut tool = HttpTool::new(
            ResolvePolicy::loopback(),
            Budget {
                wall_ms: 1000,
                ..Budget::default()
            },
        )
        .unwrap();
        let hints = VerifyHints::default();
        assert_eq!(hints.max_prefix_bytes, 1024);
        let _verifier = Verifier::new(&mut tool);
    }

    #[test]
    fn policy_denied_is_not_downloadable() {
        assert!(!VerificationStatus::PolicyDenied.is_downloadable());
    }

    #[test]
    fn head_status_405_and_501_require_a_get_fallback() {
        assert!(head_requires_get_fallback(405));
        assert!(head_requires_get_fallback(501));
        for status in [200, 401, 403, 404, 410, 429, 500] {
            assert!(!head_requires_get_fallback(status), "status {status}");
        }
    }

    /// Loopback fixture: `HEAD` answers `head_status`, `GET` answers a 4-byte
    /// `application/octet-stream`, and every method is recorded.
    async fn spawn_method_aware_server(
        head_status: u16,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let methods: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = methods.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let observed = observed.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 2048];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request);
                    let method = text.split_whitespace().next().unwrap_or("").to_owned();
                    observed
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(method.clone());
                    let response = if method == "HEAD" {
                        format!(
                            "HTTP/1.1 {head_status} X\r\nAllow: GET, HEAD\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\ndata"
                            .to_owned()
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        (base_url, methods)
    }

    #[tokio::test]
    async fn head_405_falls_back_to_a_bounded_get() {
        let (base_url, methods) = spawn_method_aware_server(405).await;
        let mut tool = HttpTool::new(
            ResolvePolicy::loopback(),
            Budget {
                wall_ms: 5_000,
                ..Budget::default()
            },
        )
        .unwrap();
        let url = format!("{base_url}/file.bin");
        let verification = Verifier::new(&mut tool)
            .verify(&url, &VerifyHints::default())
            .await;

        assert_eq!(verification.status, VerificationStatus::Verified);
        let methods = methods
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(methods, vec!["HEAD".to_owned(), "GET".to_owned()]);
    }

    #[tokio::test]
    async fn one_time_head_405_never_issues_a_get() {
        let (base_url, methods) = spawn_method_aware_server(405).await;
        let mut tool = HttpTool::new(
            ResolvePolicy::loopback(),
            Budget {
                wall_ms: 5_000,
                ..Budget::default()
            },
        )
        .unwrap();
        let url = format!("{base_url}/signed.bin");
        let hints = VerifyHints {
            one_time: true,
            ..VerifyHints::default()
        };
        let verification = Verifier::new(&mut tool).verify(&url, &hints).await;

        assert_eq!(verification.status, VerificationStatus::UnknownLength);
        assert_eq!(
            methods
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            &["HEAD".to_owned()]
        );
    }
}
