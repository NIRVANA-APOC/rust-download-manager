//! Small helpers shared by every MCP tool: argument extraction, policy
//! mapping, file paths, formatting and redaction.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rdm_agent::{redact_text, redact_value, AgentErrorInfo, ErrorCode, Policy, TaskView};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Error constructors (stable taxonomy, docs/agent-integration.md §2.3)
// ---------------------------------------------------------------------------

pub fn invalid_request(message: impl Into<String>) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::InvalidRequest, message).with_hint(
        "Fix the arguments; inspect the tool inputSchema or call rdm_capabilities for the contract.",
    )
}

pub fn invalid_arg(field: &str, expected: &str) -> AgentErrorInfo {
    invalid_request(format!("invalid `{field}`: expected {expected}"))
}

pub fn invalid_url(message: impl Into<String>) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::InvalidUrl, message)
        .with_hint("Only http:// and https:// URLs are accepted.")
}

pub fn policy_denied(message: impl Into<String>, details: Value) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::PolicyDenied, message)
        .with_hint("Edit the agent policy.json (allowed_hosts / allowed_dirs / size limits) to allow this; approval tokens cannot bypass policy.")
        .with_details(details)
}

pub fn needs_approval(message: impl Into<String>, details: Value) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::NeedsApproval, message)
        .with_hint("Call rdm_apply_plan with plan_id and approval_token to execute; tokens are single-use and expire after 10 minutes.")
        .with_details(details)
}

pub fn not_found(message: impl Into<String>) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::NotFound, message)
        .with_hint("Check the id with rdm_list_tasks; plans expire after 10 minutes.")
}

pub fn conflict(message: impl Into<String>) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::Conflict, message)
        .with_hint("Resolve the conflict explicitly (different file_name/dest_dir, resume, or discard the partial state) and retry.")
}

pub fn timeout(message: impl Into<String>, details: Value) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::Timeout, message)
        .with_hint(
            "The task keeps running; call rdm_wait_task again or inspect it with rdm_get_task.",
        )
        .with_details(details)
}

pub fn cancelled(message: impl Into<String>, details: Value) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::Cancelled, message)
        .with_hint("The wait was cancelled; the download task itself was not changed.")
        .with_details(details)
}

pub fn internal(message: impl Into<String>) -> AgentErrorInfo {
    AgentErrorInfo::new(ErrorCode::Internal, message).with_hint(
        "Unexpected server-side error; run rdm_doctor and check the MCP server stderr log.",
    )
}

/// Apply defence-in-depth redaction to an error before it becomes agent-visible.
pub fn redact_error(mut error: AgentErrorInfo) -> AgentErrorInfo {
    error.message = redact_text(&error.message);
    if let Some(hint) = &error.hint {
        error.hint = Some(redact_text(hint));
    }
    if let Some(details) = &error.details {
        error.details = Some(redact_value(details));
    }
    error
}

// ---------------------------------------------------------------------------
// Argument extraction
// ---------------------------------------------------------------------------

/// Tool arguments are a JSON object; `null`/missing means "no arguments".
pub fn args_object(arguments: &Value) -> Result<Map<String, Value>, AgentErrorInfo> {
    match arguments {
        Value::Object(map) => Ok(map.clone()),
        Value::Null => Ok(Map::new()),
        _ => Err(invalid_request(
            "tool arguments must be a JSON object (or omitted)",
        )),
    }
}

/// Reject typo'd/unknown parameters early with a clear message. The allowed
/// set comes from the tool's own `inputSchema`, so it cannot drift.
pub fn reject_unknown_args(tool: &str, args: &Map<String, Value>) -> Result<(), AgentErrorInfo> {
    let Some(definition) = rdm_agent::tool_by_name(tool) else {
        return Ok(());
    };
    let Some(properties) = definition
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    for key in args.keys() {
        if !properties.contains_key(key) {
            let allowed: Vec<&str> = properties.keys().map(String::as_str).collect();
            return Err(
                invalid_request(format!("unknown parameter `{key}` for {tool}"))
                    .with_hint(format!("Allowed parameters: {}", allowed.join(", "))),
            );
        }
    }
    Ok(())
}

fn value_not_null<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    match args.get(key) {
        None | Some(Value::Null) => None,
        Some(value) => Some(value),
    }
}

pub fn required_str(args: &Map<String, Value>, key: &str) -> Result<String, AgentErrorInfo> {
    match value_not_null(args, key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        Some(Value::String(_)) => Err(invalid_arg(key, "a non-empty string")),
        Some(_) => Err(invalid_arg(key, "a string")),
        None => Err(invalid_request(format!(
            "missing required parameter `{key}`"
        ))),
    }
}

pub fn optional_str(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, AgentErrorInfo> {
    match value_not_null(args, key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid_arg(key, "a string")),
    }
}

pub fn optional_bool(
    args: &Map<String, Value>,
    key: &str,
    default: bool,
) -> Result<bool, AgentErrorInfo> {
    match value_not_null(args, key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(invalid_arg(key, "a boolean")),
    }
}

pub fn optional_u64(args: &Map<String, Value>, key: &str) -> Result<Option<u64>, AgentErrorInfo> {
    match value_not_null(args, key) {
        None => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid_arg(key, "a non-negative integer")),
        Some(_) => Err(invalid_arg(key, "a non-negative integer")),
    }
}

pub fn optional_usize(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<usize>, AgentErrorInfo> {
    match optional_u64(args, key)? {
        None => Ok(None),
        Some(value) => usize::try_from(value)
            .map(Some)
            .map_err(|_| invalid_arg(key, "an integer that fits in memory")),
    }
}

pub fn optional_u64_bounded(
    args: &Map<String, Value>,
    key: &str,
    min: u64,
    max: u64,
    default: u64,
) -> Result<u64, AgentErrorInfo> {
    let value = optional_u64(args, key)?.unwrap_or(default);
    if value < min || value > max {
        return Err(invalid_arg(key, &format!("an integer in {min}..={max}")));
    }
    Ok(value)
}

pub fn optional_str_list(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, AgentErrorInfo> {
    match value_not_null(args, key) {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(text) => out.push(text.clone()),
                    _ => return Err(invalid_arg(key, "an array of strings")),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(invalid_arg(key, "an array of strings")),
    }
}

pub fn optional_u64_list(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<u64>>, AgentErrorInfo> {
    match value_not_null(args, key) {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Number(number) => {
                        let value = number
                            .as_u64()
                            .ok_or_else(|| invalid_arg(key, "an array of positive integers"))?;
                        out.push(value);
                    }
                    _ => return Err(invalid_arg(key, "an array of positive integers")),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(invalid_arg(key, "an array of positive integers")),
    }
}

pub fn optional_headers(
    args: &Map<String, Value>,
) -> Result<BTreeMap<String, String>, AgentErrorInfo> {
    match value_not_null(args, "headers") {
        None => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => {
            let mut out = BTreeMap::new();
            for (name, value) in map {
                let header_name = name.trim();
                if header_name.is_empty()
                    || !header_name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(ch))
                {
                    return Err(invalid_arg("headers", "valid HTTP header names"));
                }
                let Value::String(value) = value else {
                    return Err(invalid_arg("headers", "string header values"));
                };
                out.insert(header_name.to_string(), value.clone());
            }
            Ok(out)
        }
        Some(_) => Err(invalid_arg("headers", "an object of string values")),
    }
}

pub fn validate_expected_sha256(value: &str) -> Result<String, AgentErrorInfo> {
    let value = value.trim();
    if value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(invalid_arg(
            "expected_sha256",
            "64 hexadecimal characters (SHA-256)",
        ))
    }
}

/// Validate `url` before it ever reaches the network.
pub fn validate_url(url: &str) -> Result<(), AgentErrorInfo> {
    let parsed = url::Url::parse(url).map_err(|error| invalid_url(format!("{url}: {error}")))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(invalid_url(format!("{url}: unsupported scheme `{other}`"))),
    }
}

pub fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
}

/// Build a [`rdm_core::DownloadRequest`] from tool arguments. Does not apply
/// policy; callers do that after (and again for the redirected final URL).
pub fn build_request(
    args: &Map<String, Value>,
    policy: &Policy,
    include_download_fields: bool,
) -> Result<rdm_core::DownloadRequest, AgentErrorInfo> {
    let url = required_str(args, "url")?;
    validate_url(&url)?;
    let mut request = rdm_core::DownloadRequest::new(url);
    if include_download_fields {
        request.output_dir = optional_str(args, "dest_dir")?.map(PathBuf::from);
        request.file_name = optional_str(args, "file_name")?;
        let connections = optional_u64_bounded(
            args,
            "connections",
            1,
            rdm_core::MAX_CONNECTIONS as u64,
            policy.default_connections as u64,
        )?;
        request.connections = connections as usize;
        if let Some(chunk_size) = optional_u64(args, "chunk_size")? {
            if chunk_size < rdm_core::MIN_CHUNK_SIZE {
                return Err(invalid_arg(
                    "chunk_size",
                    &format!("an integer >= {} bytes", rdm_core::MIN_CHUNK_SIZE),
                ));
            }
            request.chunk_size = Some(chunk_size);
        }
        if let Some(expected) = optional_str(args, "expected_sha256")? {
            request.expected_sha256 = Some(validate_expected_sha256(&expected)?);
        }
        request.speed_limit_bps = optional_u64(args, "speed_limit_bps")?;
        request.resume = optional_bool(args, "resume", true)?;
    }
    request.headers = optional_headers(args)?;
    request.cookie = optional_str(args, "cookie")?;
    request.referer = optional_str(args, "referer")?;
    request.user_agent = optional_str(args, "user_agent")?;
    request.proxy = optional_str(args, "proxy")?;
    request.insecure = optional_bool(args, "insecure", false)?;
    Ok(request)
}

/// Apply the URL + destination policy gates for a download request.
pub fn check_policy_url(policy: &Policy, url: &str) -> Result<(), AgentErrorInfo> {
    match policy.check_url(url) {
        rdm_agent::PolicyDecision::Deny { reason } => Err(policy_denied(
            format!("URL rejected by policy: {reason}"),
            serde_json::json!({
                "stage": "url",
                "url": rdm_agent::redact_url(url),
                "allowed_hosts": policy.allowed_hosts,
                "allow_private_hosts": policy.allow_private_hosts,
            }),
        )),
        _ => Ok(()),
    }
}

/// Apply the URL + destination policy gates for a download request.
pub fn check_download_policy(
    policy: &Policy,
    request: &rdm_core::DownloadRequest,
    dest_dir: &Path,
) -> Result<(), AgentErrorInfo> {
    check_policy_url(policy, &request.url)?;
    if !policy.check_dest_dir(dest_dir) {
        return Err(policy_denied(
            format!(
                "destination directory is outside policy.allowed_dirs: {}",
                dest_dir.display()
            ),
            serde_json::json!({
                "stage": "dest_dir",
                "dest_dir": dest_dir.display().to_string(),
                "allowed_dirs": policy
                    .summary()
                    .get("allowed_dirs")
                    .cloned()
                    .unwrap_or(Value::Null),
            }),
        ));
    }
    Ok(())
}

/// Resolve an optional agent-supplied `dest_dir` against the policy default.
pub fn resolve_dest_dir(
    args: &Map<String, Value>,
    policy: &Policy,
) -> Result<PathBuf, AgentErrorInfo> {
    match optional_str(args, "dest_dir")? {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => policy.allowed_dirs.first().cloned().ok_or_else(|| {
            policy_denied(
                "policy.allowed_dirs is empty; no destination is permitted",
                serde_json::json!({ "allowed_dirs": [] }),
            )
        }),
    }
}

// ---------------------------------------------------------------------------
// Paths / files
// ---------------------------------------------------------------------------

const MAX_NAME_CHARS: usize = 180;

/// Windows-safe filename sanitizer (mirrors the engine's resolver so conflict
/// checks and the engine agree on the target name).
pub fn sanitize_filename(raw: &str) -> String {
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
    let out = if reserved { format!("_{out}") } else { out };
    if out.chars().count() <= MAX_NAME_CHARS {
        return out;
    }
    let cutoff = out
        .char_indices()
        .nth(MAX_NAME_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(out.len());
    out[..cutoff].to_string()
}

/// `<dir>/<sanitized file name>` — the base target the engine will use when no
/// partial state is present.
pub fn expected_target(dir: &Path, file_name: &str) -> PathBuf {
    dir.join(sanitize_filename(file_name))
}

pub fn part_paths(target: &Path) -> (PathBuf, PathBuf) {
    (
        append_suffix(target, ".part"),
        append_suffix(target, ".part.json"),
    )
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Remove a completed target plus any partial state (used for allow_overwrite
/// and discard_partial).
pub fn remove_target_and_partials(target: &Path) -> std::io::Result<()> {
    let (part, state) = part_paths(target);
    for path in [target.to_path_buf(), part, state] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub struct Sha256Outcome {
    pub sha256: Option<String>,
    pub bytes_hashed: u64,
    pub truncated: bool,
    pub size: u64,
}

/// Stream a SHA-256 over at most `max_bytes` (default: the whole file).
pub fn sha256_file(path: &Path, max_bytes: Option<u64>) -> std::io::Result<Sha256Outcome> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let limit = max_bytes.unwrap_or(size).min(size);
    let mut hasher = Sha256::new();
    let mut remaining = limit;
    let mut buffer = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let want = (buffer.len() as u64).min(remaining) as usize;
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let bytes_hashed = limit - remaining;
    let truncated = bytes_hashed < size;
    Ok(Sha256Outcome {
        sha256: Some(hex::encode(hasher.finalize())),
        bytes_hashed,
        truncated,
        size,
    })
}

/// Recursively collect files under `root` up to `max_files` entries and
/// `max_depth` directory levels (never follows symlinks/reparse points).
pub fn walk_files(
    root: &Path,
    max_files: usize,
    max_depth: usize,
    out: &mut Vec<PathBuf>,
) -> usize {
    fn visit(
        dir: &Path,
        depth: usize,
        max_depth: usize,
        max_files: usize,
        out: &mut Vec<PathBuf>,
        scanned: &mut usize,
    ) {
        if depth > max_depth || *scanned >= max_files {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if *scanned >= max_files {
                return;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            *scanned += 1;
            if file_type.is_dir() {
                visit(&path, depth + 1, max_depth, max_files, out, scanned);
            } else if file_type.is_file() {
                out.push(path);
            }
        }
    }
    let mut scanned = 0usize;
    visit(root, 0, max_depth, max_files, out, &mut scanned);
    scanned
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

pub fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub fn format_size(bytes: u64) -> String {
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
    format!("{}/s", format_size(bytes_per_sec.max(0.0) as u64))
}

pub fn format_eta(eta_secs: Option<f64>) -> String {
    match eta_secs {
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
            let seconds = seconds.round() as u64;
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

pub fn format_percent(downloaded: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!("{:.1}%", downloaded as f64 * 100.0 / total as f64),
        _ => "?%".to_string(),
    }
}

/// One-line progress message used by `notifications/progress` and tool text.
pub fn progress_message(task: &TaskView) -> String {
    format!(
        "{} {} {} / {} ({}) ETA {}",
        task.state.as_str(),
        format_speed(task.speed_bps),
        format_size(task.downloaded_bytes),
        task.total_bytes
            .map(format_size)
            .unwrap_or_else(|| "?".to_string()),
        format_percent(task.downloaded_bytes, task.total_bytes),
        format_eta(task.eta_secs),
    )
}

/// Human-readable summary for `content[0].text`.
pub fn task_summary_text(task: &TaskView) -> String {
    match task.state {
        rdm_agent::TaskState::Completed => {
            let path = task
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "Task {} completed: {} saved to {} ({}){}",
                task.id,
                format_size(
                    task.result
                        .as_ref()
                        .map(|r| r.size)
                        .unwrap_or(task.downloaded_bytes)
                ),
                path,
                task.result
                    .as_ref()
                    .map(|r| format!("{} ms", r.elapsed_ms))
                    .unwrap_or_else(|| "elapsed unknown".to_string()),
                task.result
                    .as_ref()
                    .and_then(|r| r.sha256.as_ref())
                    .map(|sha| format!("\nsha256: {sha}"))
                    .unwrap_or_default(),
            )
        }
        rdm_agent::TaskState::Failed => format!(
            "Task {} failed: {}",
            task.id,
            task.error
                .as_ref()
                .map(|error| format!("[{}] {}", error.code.as_str(), error.message))
                .unwrap_or_else(|| "unknown error".to_string())
        ),
        rdm_agent::TaskState::Paused => format!(
            "Task {} paused at {} / {} (use rdm_resume_task to continue)",
            task.id,
            format_size(task.downloaded_bytes),
            task.total_bytes
                .map(format_size)
                .unwrap_or_else(|| "?".to_string())
        ),
        rdm_agent::TaskState::Cancelled => format!(
            "Task {} cancelled at {} (partial data retained unless discard_partial=true)",
            task.id,
            format_size(task.downloaded_bytes)
        ),
        state => format!(
            "Task {} is {}: {} / {}",
            task.id,
            state.as_str(),
            format_size(task.downloaded_bytes),
            task.total_bytes
                .map(format_size)
                .unwrap_or_else(|| "?".to_string())
        ),
    }
}

/// Error-level helper: millisecond duration cap for waits.
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 60_000;
pub const MAX_WAIT_TIMEOUT_MS: u64 = 3_600_000;

pub fn wait_timeout_from_args(args: &Map<String, Value>) -> Result<u64, AgentErrorInfo> {
    optional_u64_bounded(
        args,
        "wait_timeout_ms",
        1,
        MAX_WAIT_TIMEOUT_MS,
        DEFAULT_WAIT_TIMEOUT_MS,
    )
}

pub fn wait_timeout_ms_from_args(
    args: &Map<String, Value>,
    field: &str,
) -> Result<u64, AgentErrorInfo> {
    optional_u64_bounded(args, field, 1, MAX_WAIT_TIMEOUT_MS, DEFAULT_WAIT_TIMEOUT_MS)
}

pub fn duration_from_ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_matches_engine_rules() {
        assert_eq!(
            sanitize_filename(r#"a<b>c:d"e/f\g|h?i*j"#),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_filename("name.  "), "name");
        assert_eq!(sanitize_filename("   "), "download.bin");
        assert_eq!(sanitize_filename(".."), "download.bin");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("lpt9.txt"), "_lpt9.txt");
        let long = sanitize_filename(&"汉".repeat(500));
        assert!(long.chars().count() <= MAX_NAME_CHARS);
    }

    #[test]
    fn target_paths_append_suffixes() {
        let target = Path::new("C:\\dl").join("file.bin");
        let (part, state) = part_paths(&target);
        assert_eq!(part, Path::new("C:\\dl").join("file.bin.part"));
        assert_eq!(state, Path::new("C:\\dl").join("file.bin.part.json"));
        assert_eq!(
            expected_target(Path::new("C:\\dl"), "file.bin"),
            Path::new("C:\\dl").join("file.bin")
        );
    }

    #[test]
    fn formats_are_stable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_speed(2048.0), "2.0 KiB/s");
        assert_eq!(format_eta(Some(62.0)), "01:02");
        assert_eq!(format_eta(None), "--:--");
        assert_eq!(format_percent(50, Some(100)), "50.0%");
        assert_eq!(format_percent(1, None), "?%");
    }

    #[test]
    fn policy_errors_are_redacted() {
        let error = redact_error(policy_denied(
            "request to https://user:pass@example.com/x denied",
            serde_json::json!({ "url": "https://user:pass@example.com/x" }),
        ));
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(!serialized.contains("user:pass"));
        assert_eq!(error.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn sha256_matches_hash_and_bounds() {
        let dir = std::env::temp_dir().join(format!("rdm-mcp-util-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        std::fs::write(&path, b"hello world").unwrap();
        let outcome = sha256_file(&path, None).unwrap();
        // SHA-256("hello world")
        assert_eq!(
            outcome.sha256.as_deref(),
            Some("b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9")
        );
        assert_eq!(outcome.bytes_hashed, 11);
        assert!(!outcome.truncated);
        let bounded = sha256_file(&path, Some(5)).unwrap();
        assert!(bounded.truncated);
        assert_eq!(bounded.bytes_hashed, 5);
        // SHA-256("hello")
        assert_eq!(
            bounded.sha256.as_deref(),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn argument_helpers_reject_bad_shapes() {
        let args = serde_json::json!({
            "url": "https://example.com/a",
            "connections": 4,
            "wait": true,
            "headers": { "Accept": "text/plain" }
        });
        let map = args_object(&args).unwrap();
        assert_eq!(map.len(), 4);
        assert!(args_object(&serde_json::json!([])).is_err());
        assert_eq!(required_str(&map, "url").unwrap(), "https://example.com/a");
        assert!(required_str(&map, "missing").is_err());
        assert_eq!(optional_u64(&map, "connections").unwrap(), Some(4));
        assert!(optional_bool(&map, "wait", false).unwrap());
        assert!(optional_u64(
            &serde_json::json!({"n": -1}).as_object().unwrap().clone(),
            "n"
        )
        .is_err());
        assert_eq!(
            optional_headers(&map)
                .unwrap()
                .get("Accept")
                .map(String::as_str),
            Some("text/plain")
        );
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("https://example.com").is_ok());
        assert_eq!(
            url_host("https://example.com/a").as_deref(),
            Some("example.com")
        );
        assert!(validate_expected_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_expected_sha256("abc").is_err());
    }

    #[test]
    fn walk_files_is_bounded_and_skips_missing() {
        let dir = std::env::temp_dir().join(format!("rdm-mcp-walk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a").join("one.part"), b"x").unwrap();
        std::fs::write(dir.join("a/b").join("two.part.json"), b"{}").unwrap();
        let mut files = Vec::new();
        walk_files(&dir, 100, 8, &mut files);
        assert_eq!(files.len(), 2);
        let mut capped = Vec::new();
        walk_files(&dir, 1, 8, &mut capped);
        assert!(capped.len() <= 1);
        let mut missing = Vec::new();
        walk_files(&dir.join("nope"), 10, 2, &mut missing);
        assert!(missing.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
