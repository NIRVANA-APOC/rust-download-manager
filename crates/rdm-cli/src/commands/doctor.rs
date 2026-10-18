//! `rdm doctor` — readiness diagnostics for agents.
//!
//! Checks environment, disk/write permission, proxy env, GUI IPC, Firefox
//! native host registration, policy file, task-store writability and — when a
//! URL is given — DNS/TLS/HEAD/Range reachability. Every check reports
//! `{id, status: pass|warn|fail|skip, summary, details?, hint?}`.

use std::path::Path;

use rdm_agent::Policy;
use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::DoctorArgs;
use crate::output::{OutputMode, Reporter};
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
struct Check {
    id: &'static str,
    status: CheckStatus,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl Check {
    fn new(id: &'static str, status: CheckStatus, summary: impl Into<String>) -> Self {
        Self {
            id,
            status,
            summary: summary.into(),
            details: None,
            hint: None,
        }
    }

    fn pass(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, CheckStatus::Pass, summary)
    }

    fn warn(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, CheckStatus::Warn, summary)
    }

    fn fail(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, CheckStatus::Fail, summary)
    }

    fn skip(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, CheckStatus::Skip, summary)
    }

    fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

pub async fn run(mode: OutputMode, store_root: &Path, args: DoctorArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "doctor");

    let mut checks = Vec::new();
    checks.push(env_check());

    // Explicit policy parse (missing file is fine; corrupt file is a failure).
    let policy_parse = util::load_policy(store_root);
    let policy = Policy::load_or_default(store_root);
    checks.push(policy_check(store_root, &policy_parse));

    let default_dir = policy
        .allowed_dirs
        .first()
        .cloned()
        .unwrap_or_else(util::default_download_dir);
    checks.extend(disk_checks(&default_dir));
    checks.push(proxy_check());
    checks.push(firefox_check());
    checks.push(gui_ipc_check());
    checks.push(state_dir_check(store_root));

    match args.url {
        Some(url) => checks.extend(network_checks(&url).await),
        None => {
            for id in [
                "network.dns",
                "network.http",
                "network.tls",
                "network.range",
            ] {
                checks.push(
                    Check::skip(id, "no --url provided").hint(
                        "run `rdm doctor --url https://example.com/file --output json` to test connectivity",
                    ),
                );
            }
        }
    }

    let summary = summarize(&checks);
    let failed = summary.fail;
    let data = json!({
        "checks": checks,
        "summary": {
            "pass": summary.pass,
            "warn": summary.warn,
            "fail": summary.fail,
            "skip": summary.skip,
            "ok": summary.fail == 0,
        },
        "rdm_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "state_dir": store_root.display().to_string(),
        "default_dir": default_dir.display().to_string(),
        "policy": policy.summary(),
    });

    if reporter.is_text() {
        for check in &data["checks"].as_array().cloned().unwrap_or_default() {
            let id = check.get("id").and_then(Value::as_str).unwrap_or("?");
            let status = check.get("status").and_then(Value::as_str).unwrap_or("?");
            let summary = check.get("summary").and_then(Value::as_str).unwrap_or("");
            crate::outln!("[{status:<4}] {id:<22} {summary}");
            if let Some(hint) = check.get("hint").and_then(Value::as_str) {
                crate::outln!("         hint: {hint}");
            }
        }
        crate::outln!(
            "doctor: {} pass, {} warn, {} fail, {} skip",
            summary.pass,
            summary.warn,
            summary.fail,
            summary.skip
        );
    }

    if failed > 0 {
        let error = rdm_agent::AgentErrorInfo::new(
            rdm_agent::ErrorCode::Internal,
            format!("{failed} environment check(s) failed"),
        )
        .with_hint("inspect data.checks for the failing ids and hints")
        .with_details(json!({ "failed": failed }));
        reporter.finish_report(data, Some(error))
    } else {
        reporter.finish_report(data, None)
    }
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

fn env_check() -> Check {
    Check::pass(
        "env.os",
        format!(
            "{} {} / rdm {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            env!("CARGO_PKG_VERSION")
        ),
    )
    .details(json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "rdm_version": env!("CARGO_PKG_VERSION"),
        "exe": std::env::current_exe().ok().map(|path| path.display().to_string()),
        "current_dir": std::env::current_dir().ok().map(|path| path.display().to_string()),
    }))
}

fn policy_check(store_root: &Path, parsed: &Result<Option<Policy>, String>) -> Check {
    let path = Policy::path_for(store_root);
    match parsed {
        Ok(Some(policy)) => {
            Check::pass("policy", format!("policy file parsed: {}", path.display()))
                .details(policy.summary())
        }
        Ok(None) => Check::skip(
            "policy",
            format!(
                "no policy file at {}; built-in defaults apply",
                path.display()
            ),
        )
        .details(json!({ "policy": Policy::default().summary() })),
        Err(message) => Check::fail("policy", format!("policy file is invalid: {message}"))
            .hint("fix policy.json or delete it to fall back to safe defaults"),
    }
}

fn disk_checks(dir: &Path) -> Vec<Check> {
    let mut checks = Vec::new();

    let write_check = match util::write_probe(dir) {
        Ok(()) => Check::pass(
            "disk.write",
            format!("{} exists and is writable", dir.display()),
        )
        .details(json!({ "dir": dir.display().to_string(), "writable": true })),
        Err(message) => Check::warn(
            "disk.write",
            format!("cannot write to {}: {message}", dir.display()),
        )
        .hint(
            "pass `-d <DIR>` to download elsewhere, or create the directory and grant write access",
        ),
    };
    checks.push(write_check);

    match util::free_disk_bytes(dir) {
        Ok(bytes) if bytes < 100 * 1024 * 1024 => {
            checks.push(
                Check::warn(
                    "disk.space",
                    format!(
                        "low disk space on {}: {} free",
                        dir.display(),
                        util::format_bytes(bytes)
                    ),
                )
                .details(json!({ "dir": dir.display().to_string(), "free_bytes": bytes }))
                .hint("free up space before downloading large files"),
            );
        }
        Ok(bytes) => {
            checks.push(
                Check::pass(
                    "disk.space",
                    format!("{} free on {}", util::format_bytes(bytes), dir.display()),
                )
                .details(json!({ "dir": dir.display().to_string(), "free_bytes": bytes })),
            );
        }
        Err(message) => {
            checks.push(Check::warn(
                "disk.space",
                format!("could not determine free space: {message}"),
            ));
        }
    }

    checks
}

fn proxy_check() -> Check {
    let proxies = util::proxy_environment();
    if proxies.is_empty() {
        Check::pass("proxy.env", "no proxy environment variables set")
    } else {
        Check::pass(
            "proxy.env",
            format!("{} proxy environment variable(s) set", proxies.len()),
        )
        .details(json!({ "variables": proxies }))
    }
}

fn firefox_check() -> Check {
    match util::firefox_native_host_registered() {
        Some(true) => Check::pass(
            "firefox.native_host",
            "HKCU native messaging host `com.rdm.download` is registered",
        ),
        Some(false) => Check::skip(
            "firefox.native_host",
            "Firefox native messaging host is not registered",
        )
        .hint("run `scripts/install-firefox-host.ps1` to enable browser handoff"),
        None => Check::skip(
            "firefox.native_host",
            "native host registration check is Windows-only",
        ),
    }
}

fn gui_ipc_check() -> Check {
    match util::gui_ipc_status() {
        util::GuiIpcStatus::Online { port, pid } => Check::pass(
            "gui.ipc",
            format!("RDM GUI IPC is online on 127.0.0.1:{port} (pid {pid})"),
        )
        .details(json!({ "online": true, "port": port, "pid": pid })),
        util::GuiIpcStatus::NotRunning => Check::skip(
            "gui.ipc",
            "RDM GUI is not running (CLI/agent downloads do not need it)",
        )
        .hint("start `rdm-gui` for desktop features; `rdm download` works without it"),
        util::GuiIpcStatus::Stale { port } => Check::skip(
            "gui.ipc",
            format!("stale ipc.json (port {port} not answering)"),
        )
        .hint("start `rdm-gui`; it rewrites ipc.json, or ignore for CLI downloads"),
        util::GuiIpcStatus::Invalid(message) => {
            Check::warn("gui.ipc", format!("ipc.json is invalid: {message}"))
                .hint("start `rdm-gui` to regenerate ipc.json")
        }
    }
}

fn state_dir_check(store_root: &Path) -> Check {
    let store = rdm_agent::TaskStore::new(store_root);
    match store.ensure_layout() {
        Ok(()) => match util::write_probe(store_root) {
            Ok(()) => Check::pass(
                "state.dir",
                format!("task store is writable: {}", store_root.display()),
            )
            .details(json!({ "path": store_root.display().to_string() })),
            Err(message) => Check::fail(
                "state.dir",
                format!("task store is not writable: {message}"),
            )
            .hint("pass `--state-dir <DIR>` with a writable path"),
        },
        Err(error) => Check::fail(
            "state.dir",
            format!("cannot create task store {}: {error}", store_root.display()),
        )
        .hint("pass `--state-dir <DIR>` with a writable path"),
    }
}

async fn network_checks(url: &str) -> Vec<Check> {
    let mut checks = Vec::new();
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => {
            checks.push(
                Check::fail("network.dns", format!("invalid URL: {error}"))
                    .hint("pass an absolute http(s) URL"),
            );
            checks.push(Check::skip("network.http", "invalid URL"));
            checks.push(Check::skip("network.tls", "invalid URL"));
            checks.push(Check::skip("network.range", "invalid URL"));
            return checks;
        }
    };
    let scheme = parsed.scheme().to_string();
    let host = parsed.host_str().unwrap_or("").to_string();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });

    // DNS
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addresses) => {
            let addresses: Vec<String> = addresses.map(|addr| addr.ip().to_string()).collect();
            if addresses.is_empty() {
                checks.push(Check::fail(
                    "network.dns",
                    format!("no address found for {host}"),
                ));
            } else {
                checks.push(
                    Check::pass(
                        "network.dns",
                        format!("{host} resolves ({} address(es))", addresses.len()),
                    )
                    .details(json!({ "host": host, "port": port, "addresses": addresses })),
                );
            }
        }
        Err(error) => {
            checks.push(
                Check::fail("network.dns", format!("cannot resolve {host}: {error}"))
                    .hint("check DNS/VPN/proxy configuration"),
            );
        }
    }

    // HEAD / Range probe (also exercises TLS when https).
    let request = rdm_core::DownloadRequest::new(url.to_string());
    match rdm_core::probe(&request).await {
        Ok(meta) => {
            checks.push(
                Check::pass(
                    "network.http",
                    format!(
                        "HEAD ok: {} bytes, {:?}",
                        meta.total_bytes
                            .map(util::format_bytes)
                            .unwrap_or_else(|| "unknown".into()),
                        meta.http_version
                    ),
                )
                .details(json!({
                    "final_url": meta.final_url,
                    "total_bytes": meta.total_bytes,
                    "http_version": meta.http_version,
                    "content_type": meta.content_type,
                })),
            );
            if scheme == "https" {
                checks.push(Check::pass("network.tls", "TLS handshake succeeded"));
            } else {
                checks.push(Check::skip("network.tls", "URL is plain http"));
            }
            if meta.supports_ranges {
                checks.push(
                    Check::pass(
                        "network.range",
                        "server supports byte ranges (segmented download)",
                    )
                    .details(json!({ "total_bytes": meta.total_bytes })),
                );
            } else {
                checks.push(
                    Check::warn(
                        "network.range",
                        "server ignores Range; downloads fall back to a single stream",
                    )
                    .hint("multi-connection speedup is impossible against this server"),
                );
            }
        }
        Err(error) => {
            let info = rdm_agent::AgentErrorInfo::from_download_error(&error);
            checks.push(
                Check::fail("network.http", info.message.clone())
                    .details(json!({ "code": info.code.as_str(), "retryable": info.retryable }))
                    .hint(info.hint.clone().unwrap_or_else(|| {
                        "check connectivity/proxy; `rdm probe <URL>` shows the raw error".into()
                    })),
            );
            checks.push(Check::skip(
                "network.tls",
                "probe failed before TLS could be verified",
            ));
            checks.push(Check::skip("network.range", "probe failed"));
        }
    }
    checks
}

// ---------------------------------------------------------------------------
// Summary / output helpers
// ---------------------------------------------------------------------------

struct Summary {
    pass: usize,
    warn: usize,
    fail: usize,
    skip: usize,
}

fn summarize(checks: &[Check]) -> Summary {
    let mut summary = Summary {
        pass: 0,
        warn: 0,
        fail: 0,
        skip: 0,
    };
    for check in checks {
        match check.status {
            CheckStatus::Pass => summary.pass += 1,
            CheckStatus::Warn => summary.warn += 1,
            CheckStatus::Fail => summary.fail += 1,
            CheckStatus::Skip => summary.skip += 1,
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_snake_case() {
        let value = serde_json::to_value(CheckStatus::Pass).unwrap();
        assert_eq!(value, json!("pass"));
        assert_eq!(
            serde_json::to_value(CheckStatus::Skip).unwrap(),
            json!("skip")
        );
    }

    #[test]
    fn summarize_counts_all_statuses() {
        let checks = vec![
            Check::pass("a", "x"),
            Check::warn("b", "x"),
            Check::fail("c", "x"),
            Check::skip("d", "x"),
        ];
        let summary = summarize(&checks);
        assert_eq!(summary.pass, 1);
        assert_eq!(summary.warn, 1);
        assert_eq!(summary.fail, 1);
        assert_eq!(summary.skip, 1);
    }

    #[test]
    fn check_serializes_optional_fields() {
        let value = serde_json::to_value(Check::pass("x", "ok")).unwrap();
        assert!(value.get("details").is_none());
        assert!(value.get("hint").is_none());
        let value = serde_json::to_value(Check::fail("x", "no").hint("do it")).unwrap();
        assert_eq!(value["hint"], json!("do it"));
    }
}
