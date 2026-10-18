//! Small shared helpers for the CLI: sizes, hashing, directories, policy,
//! platform probes. Kept dependency-light on purpose.

use std::path::{Path, PathBuf};

use rdm_agent::{AgentErrorInfo, ErrorCode, Policy, PolicyDecision, TaskState};

// ---------------------------------------------------------------------------
// Output macros that never panic on a closed pipe
// ---------------------------------------------------------------------------

/// `println!` that ignores `EPIPE`/closed-pipe errors.
///
/// Agents routinely pipe `--output ndjson` into `head`/`Select-Object -First`,
/// which closes stdout early; a panicking `println!` would turn a healthy
/// command into exit code 101 instead of a clean truncation.
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, $($arg)*);
    }};
}

/// `print!` variant that ignores write errors (used for the progress line).
#[macro_export]
macro_rules! outprint {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = write!(lock, $($arg)*);
        let _ = lock.flush();
    }};
}

/// `eprintln!` variant that ignores write errors.
#[macro_export]
macro_rules! errln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = writeln!(lock, $($arg)*);
    }};
}

// ---------------------------------------------------------------------------
// Formatting / parsing
// ---------------------------------------------------------------------------

/// Parse human byte sizes: `1024`, `4K`, `64M`, `1.5G`, `10MiB`, `2GB`.
pub fn parse_bytes(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty byte size".to_string());
    }
    let split = value
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("invalid byte size `{value}`"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("invalid byte size `{value}`"));
    }
    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown byte size suffix `{other}` in `{value}`")),
    };
    Ok((number * multiplier).round() as u64)
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec.max(0.0) as u64))
}

pub fn format_eta(eta_secs: Option<f64>) -> String {
    match eta_secs {
        Some(seconds) if seconds.is_finite() => {
            let seconds = seconds.max(0.0).round() as u64;
            if seconds >= 3600 {
                format!(
                    "{:02}:{:02}:{:02}",
                    seconds / 3600,
                    (seconds % 3600) / 60,
                    seconds % 60
                )
            } else {
                format!("{:02}:{:02}", seconds / 60, seconds % 60)
            }
        }
        _ => "--:--".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Directories / temp files
// ---------------------------------------------------------------------------

pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Built-in default download directory (matches `rdm_agent::Policy::default`).
pub fn default_download_dir() -> PathBuf {
    home_dir().join("Downloads")
}

/// Unique temporary directory under the OS temp dir.
pub fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rdm-{tag}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// Create a temp directory (see [`unique_temp_dir`]).
pub fn create_temp_dir(tag: &str) -> std::io::Result<PathBuf> {
    let path = unique_temp_dir(tag);
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Policy / state store
// ---------------------------------------------------------------------------

pub fn policy_path(store_root: &Path) -> PathBuf {
    store_root.join("policy.json")
}

/// Load `policy.json` explicitly so callers can distinguish "missing" (None)
/// from "corrupt" (Err). The agent library's `load_or_default` hides both.
pub fn load_policy(store_root: &Path) -> Result<Option<Policy>, String> {
    let path = policy_path(store_root);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let policy: Policy = serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    Ok(Some(policy))
}

/// Parse a comma-separated / repeated list of task states.
pub fn parse_states(values: &[String]) -> Result<Option<Vec<TaskState>>, String> {
    if values.is_empty() {
        return Ok(None);
    }
    let mut states = Vec::new();
    for value in values {
        for item in value.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            states.push(parse_state(item)?);
        }
    }
    Ok(Some(states))
}

pub fn parse_state(value: &str) -> Result<TaskState, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "queued" => Ok(TaskState::Queued),
        "probing" => Ok(TaskState::Probing),
        "downloading" => Ok(TaskState::Downloading),
        "paused" => Ok(TaskState::Paused),
        "completed" => Ok(TaskState::Completed),
        "failed" => Ok(TaskState::Failed),
        "cancelled" | "canceled" => Ok(TaskState::Cancelled),
        other => Err(format!(
            "unknown task state `{other}` (expected queued|probing|downloading|paused|completed|failed|cancelled)"
        )),
    }
}

// ---------------------------------------------------------------------------
// Proxy environment / redaction
// ---------------------------------------------------------------------------

pub const PROXY_ENV_VARS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// Return `(NAME, value)` pairs for proxy-related environment variables.
pub fn proxy_environment() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for name in PROXY_ENV_VARS {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                found.push(((*name).to_string(), redact_proxy_url(&value)));
            }
        }
    }
    found
}

/// Hide `user:password@` inside a proxy URL before it reaches logs/output.
pub fn redact_proxy_url(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_string();
    };
    let after_scheme = &value[scheme_end + 3..];
    let Some(at) = after_scheme.find('@') else {
        return value.to_string();
    };
    let authority = &after_scheme[..at];
    if !authority.contains(':') {
        return value.to_string();
    }
    format!(
        "{}***@{}",
        &value[..scheme_end + 3],
        &after_scheme[at + 1..]
    )
}

// ---------------------------------------------------------------------------
// Platform probes
// ---------------------------------------------------------------------------

/// Best-effort free disk space for the volume containing `path`.
pub fn free_disk_bytes(path: &Path) -> Result<u64, String> {
    #[cfg(windows)]
    {
        let root = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let drive = root
            .components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .filter(|name| name.len() == 2 && name.ends_with(':'))
            .ok_or_else(|| format!("cannot determine drive for {}", path.display()))?;
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("(Get-PSDrive -Name {}).Free", &drive[..1]),
            ])
            .output()
            .map_err(|error| format!("powershell failed: {error}"))?;
        if !output.status.success() {
            return Err("powershell Get-PSDrive failed".to_string());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        text.trim()
            .parse::<u64>()
            .map_err(|error| format!("unexpected powershell output `{}`: {error}", text.trim()))
    }
    #[cfg(unix)]
    {
        let output = std::process::Command::new("df")
            .args(["-P", "-k", &path.to_string_lossy()])
            .output()
            .map_err(|error| format!("df failed: {error}"))?;
        if !output.status.success() {
            return Err("df failed".to_string());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text
            .lines()
            .nth(1)
            .ok_or_else(|| "df produced no data line".to_string())?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        let available = fields
            .get(3)
            .ok_or_else(|| "df output missing available space".to_string())?;
        available
            .parse::<u64>()
            .map(|kib| kib.saturating_mul(1024))
            .map_err(|error| format!("df parse error: {error}"))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        Err("free-space probe unsupported on this platform".to_string())
    }
}

/// Verify a directory exists and is writable by creating + deleting a probe.
pub fn write_probe(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let probe = dir.join(format!(
        ".rdm-doctor-{}-{}",
        std::process::id(),
        rdm_agent::now_iso8601().replace(':', "-")
    ));
    std::fs::write(&probe, b"rdm doctor write probe")
        .map_err(|error| format!("cannot write in {}: {error}", dir.display()))?;
    std::fs::remove_file(&probe)
        .map_err(|error| format!("cannot remove probe {}: {error}", probe.display()))?;
    Ok(())
}

/// Firefox native-messaging host registration check (Windows registry).
pub fn firefox_native_host_registered() -> Option<bool> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Mozilla\NativeMessagingHosts\com.rdm.download",
                "/ve",
            ])
            .output()
            .ok()?;
        Some(output.status.success())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// GUI IPC liveness (`%LOCALAPPDATA%\RDM\ipc.json` + TCP probe).
pub fn gui_ipc_status() -> GuiIpcStatus {
    let info_path = rdm_ipc::info_path();
    if !info_path.exists() {
        return GuiIpcStatus::NotRunning;
    }
    match rdm_ipc::read_info() {
        Ok(info) => {
            if rdm_ipc::is_gui_alive() {
                GuiIpcStatus::Online {
                    port: info.port,
                    pid: info.pid,
                }
            } else {
                GuiIpcStatus::Stale { port: info.port }
            }
        }
        Err(error) => GuiIpcStatus::Invalid(error.to_string()),
    }
}

#[derive(Debug, Clone)]
pub enum GuiIpcStatus {
    NotRunning,
    Online { port: u16, pid: u32 },
    Stale { port: u16 },
    Invalid(String),
}

// ---------------------------------------------------------------------------
// Agent policy enforcement helpers (shared by download / probe / run)
// ---------------------------------------------------------------------------

/// Warning recorded when no `policy.json` exists: the safe defaults are NOT
/// enforced by direct CLI commands (accepted by the agent contract).
pub const POLICY_ABSENT_WARNING: &str =
    "policy-file-absent: default policy limits are not enforced by this command";

/// Load the effective policy of a task-store root.
///
/// `Ok(None)` means "no policy file" (CLI proceeds with a warning);
/// `Err` means the file exists but cannot be read/parsed. Corrupt policy must
/// never be silently ignored: agents rely on it as a security boundary.
pub fn load_effective_policy(store_root: &Path) -> Result<Option<Policy>, AgentErrorInfo> {
    match load_policy(store_root) {
        Ok(policy) => Ok(policy.map(Policy::normalized)),
        Err(message) => Err(crate::output::invalid_request(format!(
            "invalid policy.json: {message}"
        ))
        .with_hint("fix `<state-dir>/policy.json`, or delete it to use built-in defaults")),
    }
}

/// Parse/validate the URL *before* any policy decision: only `http`/`https`
/// are supported. Invalid syntax and unsupported schemes are `invalid_url`
/// (exit 2) — never `policy_denied` (exit 4), so agents can distinguish a bad
/// request from a policy block.
pub fn validate_http_url(raw: &str) -> Result<(), AgentErrorInfo> {
    let parsed = url::Url::parse(raw).map_err(|error| {
        AgentErrorInfo::new(
            ErrorCode::InvalidUrl,
            format!("invalid URL `{raw}`: {error}"),
        )
        .with_hint("pass an absolute http(s) URL, e.g. https://example.com/file.iso")
        .with_details(serde_json::json!({ "url": raw }))
    })?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(AgentErrorInfo::new(
            ErrorCode::InvalidUrl,
            format!("unsupported URL scheme `{scheme}`: only http/https are supported"),
        )
        .with_hint("pass an absolute http(s) URL, e.g. https://example.com/file.iso")
        .with_details(serde_json::json!({ "url": raw, "scheme": scheme })));
    }
    Ok(())
}

/// URL/host + destination-directory policy pre-check.
pub fn policy_precheck(
    policy: &Policy,
    url: &str,
    dest_dir: Option<&Path>,
) -> Result<(), AgentErrorInfo> {
    match policy.check_url(url) {
        PolicyDecision::Allow => {}
        PolicyDecision::Deny { reason } => {
            return Err(AgentErrorInfo::new(
                ErrorCode::PolicyDenied,
                format!("URL denied by policy: {reason}"),
            )
            .with_hint(
                "add the host to `policy.json` allowed_hosts, or set allow_private_hosts=true",
            )
            .with_details(serde_json::json!({
                "allowed_hosts": policy.allowed_hosts,
                "allow_private_hosts": policy.allow_private_hosts,
            })));
        }
        PolicyDecision::NeedsApproval {
            reason,
            total_bytes,
        } => {
            return Err(needs_approval_error(reason, total_bytes, policy));
        }
    }

    if let Some(dir) = dest_dir {
        if !policy.check_dest_dir(dir) {
            return Err(AgentErrorInfo::new(
                ErrorCode::PolicyDenied,
                format!(
                    "destination `{}` is outside policy.allowed_dirs",
                    dir.display()
                ),
            )
            .with_hint("pick a destination inside an allowed dir, or update `policy.json`")
            .with_details(serde_json::json!({ "allowed_dirs": policy.allowed_dirs })));
        }
    }
    Ok(())
}

/// Size gate applied after `probe` learned the remote length.
pub fn policy_gate_size(policy: &Policy, total_bytes: Option<u64>) -> Result<(), AgentErrorInfo> {
    match policy.gate(total_bytes) {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Deny { reason } => Err(AgentErrorInfo::new(
            ErrorCode::PolicyDenied,
            format!("size denied by policy: {reason}"),
        )
        .with_hint("raise `max_bytes_per_task` in policy.json")
        .with_details(serde_json::json!({
            "total_bytes": total_bytes,
            "max_bytes_per_task": policy.max_bytes_per_task,
        }))),
        PolicyDecision::NeedsApproval {
            reason,
            total_bytes,
        } => Err(needs_approval_error(reason, total_bytes, policy)),
    }
}

/// `allow_overwrite=false`: refuse *before* downloading when the target exists.
pub fn policy_overwrite_gate(policy: &Policy, target: &Path) -> Result<(), AgentErrorInfo> {
    if policy.allow_overwrite || !target.exists() {
        return Ok(());
    }
    Err(AgentErrorInfo::new(
        ErrorCode::Conflict,
        format!("target file already exists: {}", target.display()),
    )
    .with_hint(
        "remove/rename the existing file, choose another `--name`/dir, or set allow_overwrite=true in policy.json",
    )
    .with_details(serde_json::json!({
        "path": target.display().to_string(),
        "allow_overwrite": false,
    })))
}

/// Error returned when streaming exceeds `max_bytes_per_task` (unknown length).
pub fn policy_size_exceeded_error(
    policy: &Policy,
    downloaded: u64,
    target: &Path,
) -> AgentErrorInfo {
    AgentErrorInfo::new(
        ErrorCode::PolicyDenied,
        format!(
            "download exceeded policy.max_bytes_per_task ({downloaded} bytes downloaded) while streaming unknown length"
        ),
    )
    .with_hint("raise `max_bytes_per_task` in policy.json; the partial `.part` file was kept")
    .with_details(serde_json::json!({
        "downloaded_bytes": downloaded,
        "max_bytes_per_task": policy.max_bytes_per_task,
        "part_path": target.display().to_string(),
    }))
}

pub fn needs_approval_error(
    reason: String,
    total_bytes: Option<u64>,
    policy: &Policy,
) -> AgentErrorInfo {
    AgentErrorInfo::new(
        ErrorCode::NeedsApproval,
        format!("approval required: {reason}"),
    )
    .with_hint(
        "raise `require_approval_over_bytes` in policy.json; approval tokens are only available through the MCP plan/apply flow",
    )
    .with_details(serde_json::json!({
        "total_bytes": total_bytes,
        "require_approval_over_bytes": policy.require_approval_over_bytes,
    }))
}

/// Approximate the engine's target filename for pre-download policy checks
/// (the engine still performs its own Windows-safe sanitisation + de-dupe).
pub fn candidate_target_path(dir: &Path, requested_name: Option<&str>, meta_name: &str) -> PathBuf {
    let desired = requested_name.unwrap_or(meta_name);
    dir.join(sanitize_file_name(desired))
}

/// Windows-safe filename sanitisation mirroring the engine's rules (used only
/// for policy pre-checks; the engine remains authoritative).
pub fn sanitize_file_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().chars() {
        if ch.is_control() {
            out.push('_');
            continue;
        }
        match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            _ => out.push(ch),
        }
    }
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }
    let out = out.trim_start().to_string();
    if out.is_empty() || out == "." || out == ".." {
        return "download.bin".to_string();
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_byte_sizes() {
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_bytes("4K").unwrap(), 4096);
        assert_eq!(parse_bytes("64M").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_bytes("1.5G").unwrap(), 1610612736);
        assert_eq!(parse_bytes("10MiB").unwrap(), 10 * 1024 * 1024);
        assert!(parse_bytes("nope").is_err());
        assert!(parse_bytes("4X").is_err());
    }

    #[test]
    fn formats_byte_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn redacts_proxy_credentials() {
        assert_eq!(
            redact_proxy_url("http://user:secret@127.0.0.1:7890"),
            "http://***@127.0.0.1:7890"
        );
        assert_eq!(
            redact_proxy_url("http://127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert_eq!(redact_proxy_url("127.0.0.1:7890"), "127.0.0.1:7890");
    }

    #[test]
    fn validates_http_urls_before_policy() {
        assert!(validate_http_url("https://example.com/a.bin").is_ok());
        assert!(validate_http_url("http://127.0.0.1:9/a").is_ok());
        for bad in [
            "ftp://example.com/x",
            "file:///C:/x",
            "example.com/x",
            "",
            "http://[broken",
        ] {
            let error = validate_http_url(bad).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidUrl, "url: {bad}");
        }
    }

    #[test]
    fn sanitizes_policy_target_names() {
        assert_eq!(sanitize_file_name("a/b:c*?.bin"), "a_b_c__.bin");
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name("  ..  "), "download.bin");
        assert_eq!(sanitize_file_name("report.bin"), "report.bin");
    }

    #[test]
    fn policy_helpers_enforce_hosts_size_and_overwrite() {
        let default = Policy::default();
        let denied = policy_precheck(&default, "http://127.0.0.1:9/file", None).unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);

        let allow_private = Policy {
            allow_private_hosts: true,
            ..Policy::default()
        };
        assert!(policy_precheck(&allow_private, "http://127.0.0.1:9/file", None).is_ok());

        let target = std::env::temp_dir().join(format!(
            "rdm-policy-target-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&target, b"x").unwrap();
        let conflict = policy_overwrite_gate(&allow_private, &target).unwrap_err();
        assert_eq!(conflict.code, ErrorCode::Conflict);
        let overwrite = Policy {
            allow_overwrite: true,
            ..allow_private
        };
        assert!(policy_overwrite_gate(&overwrite, &target).is_ok());
        let _ = std::fs::remove_file(&target);

        let size_denied = policy_gate_size(
            &Policy {
                max_bytes_per_task: Some(10),
                ..Policy::default()
            },
            Some(11),
        )
        .unwrap_err();
        assert_eq!(size_denied.code, ErrorCode::PolicyDenied);

        let approval = policy_gate_size(
            &Policy {
                max_bytes_per_task: None,
                require_approval_over_bytes: Some(5),
                ..Policy::default()
            },
            Some(10),
        )
        .unwrap_err();
        assert_eq!(approval.code, ErrorCode::NeedsApproval);

        assert!(policy_gate_size(
            &Policy {
                max_bytes_per_task: None,
                require_approval_over_bytes: None,
                ..Policy::default()
            },
            None,
        )
        .is_ok());
    }
}
