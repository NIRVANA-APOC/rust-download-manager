//! Bounded HTTP tool: the only network egress of the resolver.
//!
//! Redirects are followed manually so every hop is re-checked against the
//! policy and cross-origin hops never inherit credentials.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, COOKIE, LOCATION};
use reqwest::{Client, Method, StatusCode};
use url::Url;

use crate::contract::{new_id, Budget, ResolveError, ResolveErrorKind};
use crate::credentials::HostCredentials;
use crate::policy::{BudgetTracker, ResolvePolicy};

/// Default cap for one observed body.
pub const DEFAULT_MAX_OBSERVATION_BYTES: usize = 256 * 1024;
/// Maximum header value length accepted from callers.
const MAX_HEADER_VALUE: usize = 8 * 1024;

/// One request the resolver wants to perform.
#[derive(Debug, Clone)]
pub struct HttpRequestSpec {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub max_bytes: usize,
    pub follow_redirects: bool,
}

impl HttpRequestSpec {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".to_owned(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            max_bytes: DEFAULT_MAX_OBSERVATION_BYTES,
            follow_redirects: true,
        }
    }

    pub fn head(url: impl Into<String>) -> Self {
        Self {
            method: "HEAD".to_owned(),
            ..Self::get(url)
        }
    }

    pub fn max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    pub fn without_redirects(mut self) -> Self {
        self.follow_redirects = false;
        self
    }
}

/// One observed response.
#[derive(Debug, Clone)]
pub struct Observation {
    pub obs_id: String,
    pub request_url: String,
    pub final_url: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub truncated: bool,
}

impl Observation {
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn body_json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")
            .and_then(|value| value.trim().parse::<u64>().ok())
    }

    pub fn accepts_ranges(&self) -> Option<bool> {
        self.header("accept-ranges")
            .map(|value| value.eq_ignore_ascii_case("bytes"))
    }

    /// Whether the response looks like an HTML page rather than a file.
    pub fn looks_like_page(&self) -> bool {
        if let Some(content_type) = self.content_type() {
            let content_type = content_type.to_ascii_lowercase();
            if content_type.contains("text/html")
                || content_type.contains("application/xhtml")
                || content_type.contains("application/xml")
                || content_type.starts_with("text/")
            {
                return true;
            }
        }
        looks_like_html(&self.body)
    }
}

/// Content sniffing on the first bytes of a body.
pub fn looks_like_html(body: &[u8]) -> bool {
    let prefix = &body[..body.len().min(512)];
    let text = String::from_utf8_lossy(prefix)
        .trim_start()
        .to_ascii_lowercase();
    text.starts_with("<!doctype")
        || text.starts_with("<html")
        || text.starts_with("<?xml")
        || text.starts_with("<head")
        || text.starts_with("<body")
}

/// Bounded HTTP client bound to one policy + budget.
pub struct HttpTool {
    client: Client,
    policy: ResolvePolicy,
    budget: BudgetTracker,
    observations: u64,
    /// Optional per-host credentials (`docs/architecture.md` §11.12); injected
    /// into the initial request URL only, never into exploration.
    credentials: Option<HostCredentials>,
}

impl HttpTool {
    pub fn new(policy: ResolvePolicy, budget: Budget) -> Result<Self, ResolveError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(budget.wall_ms.max(1)))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("rdm-resolve/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                ResolveError::new(
                    ResolveErrorKind::Internal,
                    format!("cannot build HTTP client: {error}"),
                )
            })?;
        Ok(Self {
            client,
            policy,
            budget: BudgetTracker::new(budget),
            observations: 0,
            credentials: None,
        })
    }

    /// Attach host-scoped credentials (builder form). Empty credentials keep the
    /// tool credential-free.
    pub fn with_credentials(mut self, credentials: HostCredentials) -> Self {
        self.set_credentials(credentials);
        self
    }

    /// Attach (or replace) host-scoped credentials. Empty input is treated as
    /// [`Self::clear_credentials`].
    pub fn set_credentials(&mut self, credentials: HostCredentials) {
        self.credentials = if credentials.is_empty() {
            None
        } else {
            Some(credentials)
        };
    }

    /// Drop the credentials (mandatory before any JSON-action exploration).
    pub fn clear_credentials(&mut self) {
        self.credentials = None;
    }

    /// Whether the tool currently carries credentials (never the values).
    pub fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }

    pub fn budget(&self) -> &BudgetTracker {
        &self.budget
    }

    pub fn policy(&self) -> &ResolvePolicy {
        &self.policy
    }

    /// Perform one request, following (and re-checking) redirects.
    pub async fn request(&mut self, spec: HttpRequestSpec) -> Result<Observation, ResolveError> {
        let mut url = Url::parse(&spec.url).map_err(|error| {
            ResolveError::new(
                ResolveErrorKind::InvalidUrl,
                format!("invalid URL {}: {error}", spec.url),
            )
        })?;
        let mut method = normalize_method(&spec.method)?;
        let mut headers = spec.headers.clone();
        // Credentials apply to the *initial* URL host only (§11.12). Header
        // names that came from the bridge are tracked so a cross-origin redirect
        // can drop every injected header (not just Cookie/Authorization).
        let mut injected_headers = apply_credentials(&mut headers, &url, self.credentials.as_ref());
        let mut body = spec.body.clone();

        loop {
            self.policy.check_url(&url)?;
            self.budget.take_request()?;

            let mut builder =
                self.client
                    .request(method.clone(), url.clone())
                    .timeout(Duration::from_millis(
                        self.budget.budget().wall_ms.saturating_sub(1).max(1),
                    ));
            for (name, value) in &headers {
                if value.len() > MAX_HEADER_VALUE {
                    return Err(ResolveError::new(
                        ResolveErrorKind::InvalidInput,
                        format!("header {name} exceeds {MAX_HEADER_VALUE} bytes"),
                    ));
                }
                builder = builder.header(name.as_str(), value.as_str());
            }
            if let Some(body) = &body {
                builder = builder.body(body.clone());
            }

            let response = builder
                .send()
                .await
                .map_err(|error| transport_error(&error))?;
            let status = response.status();

            if spec.follow_redirects && status.is_redirection() {
                if let Some(location) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                {
                    let next = url.join(location).map_err(|error| {
                        ResolveError::new(
                            ResolveErrorKind::InvalidUrl,
                            format!("invalid redirect target {location}: {error}"),
                        )
                    })?;
                    self.budget.take_redirect(self.policy.max_redirects)?;
                    // New origin: never forward credentials or explicit headers
                    // that were meant for the previous host.
                    if next.origin() != url.origin() {
                        strip_sensitive_headers(&mut headers);
                        strip_credential_headers(&mut headers, &injected_headers);
                        injected_headers.clear();
                    }
                    // 303 always switches to GET; 301/302 do so for non-GET/HEAD.
                    if status == StatusCode::SEE_OTHER
                        || (method != Method::GET
                            && method != Method::HEAD
                            && matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND))
                    {
                        method = Method::GET;
                        body = None;
                    }
                    tracing::debug!(from = %url, to = %next, "rdm-resolve: following redirect");
                    url = next;
                    continue;
                }
            }

            let final_url = response.url().to_string();
            let response_headers = collect_headers(response.headers());
            let cap = spec.max_bytes;
            let (body, truncated) = read_body_capped(response, cap, &mut self.budget).await?;
            self.observations += 1;
            return Ok(Observation {
                obs_id: new_id("obs"),
                request_url: spec.url.clone(),
                final_url,
                status: status.as_u16(),
                headers: response_headers,
                body,
                truncated,
            });
        }
    }
}

fn normalize_method(method: &str) -> Result<Method, ResolveError> {
    match method.trim().to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "HEAD" => Ok(Method::HEAD),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "DELETE" => Ok(Method::DELETE),
        other => Err(ResolveError::new(
            ResolveErrorKind::InvalidInput,
            format!("unsupported HTTP method: {other}"),
        )),
    }
}

fn collect_headers(headers: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    let mut collected = BTreeMap::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            collected.insert(name.as_str().to_ascii_lowercase(), value.to_owned());
        }
    }
    collected
}

/// Drop credentials when a redirect leaves the original origin.
fn strip_sensitive_headers(headers: &mut BTreeMap<String, String>) {
    headers.retain(|name, _| {
        !name.eq_ignore_ascii_case(COOKIE.as_str())
            && !name.eq_ignore_ascii_case(AUTHORIZATION.as_str())
    });
}

/// Merge host-scoped credentials into request headers.
///
/// Returns the lowercased names that were injected so a cross-origin redirect
/// can drop exactly those headers (explicit caller headers are never touched).
pub(crate) fn apply_credentials(
    headers: &mut BTreeMap<String, String>,
    url: &Url,
    credentials: Option<&HostCredentials>,
) -> Vec<String> {
    let Some(credentials) = credentials else {
        return Vec::new();
    };
    let mut injected = Vec::new();
    for (name, value) in credentials.for_url(url) {
        // An explicit header from the recipe/explorer always wins.
        if headers
            .keys()
            .any(|existing| existing.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        injected.push(name.to_ascii_lowercase());
        headers.insert(name, value);
    }
    injected
}

/// Remove every header injected by the credential bridge (cross-origin hop).
fn strip_credential_headers(headers: &mut BTreeMap<String, String>, injected: &[String]) {
    if injected.is_empty() {
        return;
    }
    headers.retain(|name, _| {
        !injected
            .iter()
            .any(|injected| name.eq_ignore_ascii_case(injected))
    });
}

async fn read_body_capped(
    mut response: reqwest::Response,
    cap: usize,
    budget: &mut BudgetTracker,
) -> Result<(Vec<u8>, bool), ResolveError> {
    let mut body = Vec::new();
    let mut truncated = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                budget.add_bytes(chunk.len() as u64)?;
                let room = cap.saturating_sub(body.len());
                if room == 0 {
                    truncated = true;
                    break;
                }
                let take = chunk.len().min(room);
                body.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    truncated = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => return Err(transport_error(&error)),
        }
    }
    Ok((body, truncated))
}

fn transport_error(error: &reqwest::Error) -> ResolveError {
    if error.is_timeout() {
        ResolveError::new(
            ResolveErrorKind::Timeout,
            format!("request timed out: {error}"),
        )
    } else if error.is_connect() {
        ResolveError::new(
            ResolveErrorKind::NetworkError,
            format!("connection failed: {error}"),
        )
    } else {
        ResolveError::new(
            ResolveErrorKind::NetworkError,
            format!("transport error: {error}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn html_sniffing_handles_doctype_and_whitespace() {
        assert!(looks_like_html(b"  <!DOCTYPE html><html>"));
        assert!(looks_like_html(b"<html lang=\"en\">"));
        assert!(!looks_like_html(b"\x00\x01PNG"));
        assert!(!looks_like_html(b"{\"a\":1}"));
    }

    #[test]
    fn sensitive_headers_are_stripped_on_cross_origin() {
        let mut headers = BTreeMap::from([
            ("cookie".to_owned(), "a=b".to_owned()),
            ("Authorization".to_owned(), "Bearer x".to_owned()),
            ("accept".to_owned(), "*/*".to_owned()),
        ]);
        strip_sensitive_headers(&mut headers);
        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key("accept"));
    }

    #[test]
    fn unsupported_methods_are_rejected() {
        assert!(normalize_method("TRACE").is_err());
        assert!(normalize_method("get").is_ok());
    }

    // ------------------------------------------------------------------
    // P4a credentials bridge (docs/architecture.md §11.12)
    // ------------------------------------------------------------------

    #[test]
    fn apply_credentials_scopes_by_host_and_respects_explicit_headers() {
        let credentials = HostCredentials::new()
            .with_cookie("example.com", "session=cred")
            .with_header("example.com", "X-Api-Key", "cred-key");
        let url = Url::parse("https://example.com/file").unwrap();
        let mut headers = BTreeMap::new();
        let injected = apply_credentials(&mut headers, &url, Some(&credentials));
        assert_eq!(injected, vec!["cookie".to_owned(), "x-api-key".to_owned()]);
        assert_eq!(
            headers.get("Cookie").map(String::as_str),
            Some("session=cred")
        );
        assert_eq!(
            headers.get("X-Api-Key").map(String::as_str),
            Some("cred-key")
        );

        // Another host is untouched.
        let mut other = BTreeMap::new();
        let other_url = Url::parse("https://other.example.com/file").unwrap();
        assert!(apply_credentials(&mut other, &other_url, Some(&credentials)).is_empty());
        assert!(other.is_empty());

        // Non-HTTP(S) never receives credentials.
        let mut file_headers = BTreeMap::new();
        let file_url = Url::parse("file:///tmp/file.bin").unwrap();
        assert!(apply_credentials(&mut file_headers, &file_url, Some(&credentials)).is_empty());

        // An explicit header wins over the bridge (case-insensitive).
        let mut explicit = BTreeMap::from([("Cookie".to_owned(), "explicit=1".to_owned())]);
        let injected = apply_credentials(&mut explicit, &url, Some(&credentials));
        assert_eq!(injected, vec!["x-api-key".to_owned()]);
        assert_eq!(
            explicit.get("Cookie").map(String::as_str),
            Some("explicit=1")
        );
    }

    #[derive(Clone)]
    enum ServerReply {
        Ok,
        Redirect(String),
    }

    type Captured = Arc<Mutex<Vec<BTreeMap<String, String>>>>;

    /// Minimal loopback server: records lowercase request headers and answers
    /// `200 ok` or a `302` redirect.
    async fn spawn_server(reply: ServerReply) -> (String, Captured, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind capture server");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let sink = Arc::clone(&sink);
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 2048];
                    while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => raw.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    let mut headers = BTreeMap::new();
                    for line in text.lines().skip(1) {
                        if let Some((name, value)) = line.split_once(':') {
                            headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
                        }
                    }
                    sink.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(headers);
                    let response = match reply {
                        ServerReply::Ok => {
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                                .to_owned()
                        }
                        ServerReply::Redirect(location) => format!(
                            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        ),
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (base, captured, task)
    }

    fn captured_headers(captured: &Captured) -> Vec<BTreeMap<String, String>> {
        captured
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn tool_with_credentials(credentials: HostCredentials) -> HttpTool {
        HttpTool::new(ResolvePolicy::loopback(), Budget::default())
            .expect("http tool")
            .with_credentials(credentials)
    }

    #[tokio::test]
    async fn http_tool_injects_credentials_and_clear_stops_them() {
        let (base, captured, task) = spawn_server(ServerReply::Ok).await;
        let credentials = HostCredentials::new().with_cookie("127.0.0.1", "session=secret");
        let mut tool = tool_with_credentials(credentials);

        tool.request(HttpRequestSpec::get(format!("{base}/one")))
            .await
            .expect("first request");
        assert_eq!(
            captured_headers(&captured)[0]
                .get("cookie")
                .map(String::as_str),
            Some("session=secret")
        );

        tool.clear_credentials();
        assert!(!tool.has_credentials());
        tool.request(HttpRequestSpec::get(format!("{base}/two")))
            .await
            .expect("second request");
        assert!(
            !captured_headers(&captured)[1].contains_key("cookie"),
            "cleared credentials must not be injected"
        );
        task.abort();
    }

    #[tokio::test]
    async fn cross_origin_redirect_drops_injected_credentials() {
        let (target_base, target_captured, target_task) = spawn_server(ServerReply::Ok).await;
        let (redirect_base, redirect_captured, redirect_task) =
            spawn_server(ServerReply::Redirect(format!("{target_base}/file"))).await;
        let credentials = HostCredentials::new()
            .with_cookie("127.0.0.1", "session=secret")
            .with_header("127.0.0.1", "X-Api-Key", "secret-key");
        let mut tool = tool_with_credentials(credentials);

        let observation = tool
            .request(HttpRequestSpec::get(format!("{redirect_base}/start")))
            .await
            .expect("redirected request");
        assert_eq!(observation.status, 200);
        assert_eq!(observation.final_url, format!("{target_base}/file"));

        // The initial request carried the bridge headers…
        let first = &captured_headers(&redirect_captured)[0];
        assert_eq!(
            first.get("cookie").map(String::as_str),
            Some("session=secret")
        );
        assert_eq!(
            first.get("x-api-key").map(String::as_str),
            Some("secret-key")
        );
        // …but the cross-origin hop (same host, different port) carried none.
        let second = &captured_headers(&target_captured)[0];
        assert!(!second.contains_key("cookie"), "{second:?}");
        assert!(!second.contains_key("x-api-key"), "{second:?}");
        target_task.abort();
        redirect_task.abort();
    }
}
