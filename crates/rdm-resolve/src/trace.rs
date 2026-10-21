//! Sanitized per-run traces (`docs/architecture.md` §11.10).
//!
//! One run may persist `<root>/runs/<run_id>.jsonl`: a header line, one JSON
//! line per [`RunStep`] and — when a limit was hit — a terminating
//! `{"truncated":true}` marker line. Files are written atomically (unique
//! temporary name + rename) and old runs are pruned best-effort.
//!
//! Privacy is enforced at this boundary:
//!
//! * URLs keep only `scheme://host[:port]/path` — query and fragment are
//!   dropped, and unparsable input becomes `<invalid-url>`;
//! * `cookie` / `authorization` / `proxy-authorization` values become
//!   `[redacted]`;
//! * response bodies are never stored (`args_digest` only), and the writing
//!   side re-sanitizes [`RunTrace::source_url`], [`RunStep::args_excerpt`] and
//!   [`RunStep::note`] so a careless caller cannot leak a raw URL.
//!
//! Every failure is an `io::Result` the caller is expected to downgrade to a
//! `tracing::warn!` — persisting a trace must never change a resolution result.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::contract::RecipeRef;

/// JSONL trace schema identifier.
pub const TRACE_SCHEMA: &str = "rdm.resolve.trace/v1";
/// How many `runs/*.jsonl` files are retained (newest win).
pub const RETENTION_RUNS: usize = 200;
/// Hard cap on persisted steps per run.
pub const MAX_STEPS: usize = 500;
/// Hard cap on the persisted JSONL payload size.
pub const MAX_BYTES: usize = 512 * 1024;
/// Excerpt cap in characters (`RunStep::args_excerpt`).
pub const MAX_EXCERPT_CHARS: usize = 256;

/// One persisted step of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStep {
    /// Zero-based position in the run.
    pub index: u32,
    /// Action/step kind, e.g. `recipe_step`, `http_request`, `builtin_verify`.
    pub op: String,
    /// `sha256:<hex>` of the (redacted) arguments.
    pub args_digest: String,
    /// Redacted, truncated human-readable arguments (never a body).
    #[serde(default)]
    pub args_excerpt: Option<String>,
    /// `ok` or `error:<kind_snake>`.
    pub outcome: String,
    #[serde(default)]
    pub duration_ms: u64,
    /// Observation handle (`obs_…`) when the step produced one.
    #[serde(default)]
    pub obs_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Header + counters of one persisted run (step bodies live on their own
/// lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTrace {
    pub run_id: String,
    pub created_ms: u64,
    /// `recipe` | `explore` | `auto`.
    pub mode: String,
    /// `succeeded` | `partial` | `failed` | `budget_exhausted` | …
    pub status: String,
    /// Redacted input URL.
    pub source_url: String,
    pub recipe: Option<RecipeRef>,
    pub llm_calls: u32,
    pub http_requests: u32,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
    pub error_kind: Option<String>,
    pub steps: Vec<RunStep>,
    pub truncated: bool,
}

/// First JSONL line of a run file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TraceHeader {
    schema: String,
    run_id: String,
    created_ms: u64,
    mode: String,
    status: String,
    source_url: String,
    #[serde(default)]
    recipe: Option<RecipeRef>,
    #[serde(default)]
    llm_calls: u32,
    #[serde(default)]
    http_requests: u32,
    #[serde(default)]
    bytes_read: u64,
    #[serde(default)]
    elapsed_ms: u64,
    #[serde(default)]
    error_kind: Option<String>,
    #[serde(default)]
    truncated: bool,
}

impl TraceHeader {
    fn new(trace: &RunTrace, truncated: bool) -> Self {
        Self {
            schema: TRACE_SCHEMA.to_owned(),
            run_id: trace.run_id.clone(),
            created_ms: trace.created_ms,
            mode: trace.mode.clone(),
            status: trace.status.clone(),
            source_url: redact_url(&trace.source_url),
            recipe: trace.recipe.clone(),
            llm_calls: trace.llm_calls,
            http_requests: trace.http_requests,
            bytes_read: trace.bytes_read,
            elapsed_ms: trace.elapsed_ms,
            error_kind: trace.error_kind.clone(),
            truncated,
        }
    }

    fn into_trace(self, steps: Vec<RunStep>, truncated: bool) -> RunTrace {
        RunTrace {
            run_id: self.run_id,
            created_ms: self.created_ms,
            mode: self.mode,
            status: self.status,
            source_url: self.source_url,
            recipe: self.recipe,
            llm_calls: self.llm_calls,
            http_requests: self.http_requests,
            bytes_read: self.bytes_read,
            elapsed_ms: self.elapsed_ms,
            error_kind: self.error_kind,
            steps,
            truncated: truncated || self.truncated,
        }
    }
}

/// Reader/writer for the `<root>/runs/` trace store.
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceStore;

impl TraceStore {
    /// Directory holding run files for a resolver state root.
    pub fn runs_dir(root: &Path) -> PathBuf {
        root.join("runs")
    }

    /// Atomically persist one run. Returns the final path.
    ///
    /// Best-effort retention runs after a successful rename. I/O errors are
    /// returned (never panicked) so the resolver can merely log them.
    pub fn write(root: &Path, trace: &RunTrace) -> io::Result<PathBuf> {
        let runs = Self::runs_dir(root);
        std::fs::create_dir_all(&runs)?;

        let marker = "{\"truncated\":true}";
        let mut truncated = trace.truncated;
        let mut step_lines: Vec<String> = Vec::new();
        let mut bytes: usize = 0;
        for step in trace.steps.iter().take(MAX_STEPS) {
            if step_lines.len() >= MAX_STEPS {
                truncated = true;
                break;
            }
            let mut step = step.clone();
            step.args_excerpt = step.args_excerpt.as_deref().map(redact_excerpt);
            step.note = step.note.as_deref().map(redact_excerpt);
            let line = serde_json::to_string(&step).map_err(|error| {
                io::Error::other(format!("cannot serialize trace step: {error}"))
            })?;
            let line_bytes = line.len() + 1;
            if bytes + line_bytes + marker.len() + 1 > MAX_BYTES {
                truncated = true;
                break;
            }
            step_lines.push(line);
            bytes += line_bytes;
        }
        if trace.steps.len() > MAX_STEPS {
            truncated = true;
        }

        let header = TraceHeader::new(trace, truncated);
        let header_line = serde_json::to_string(&header)
            .map_err(|error| io::Error::other(format!("cannot serialize trace header: {error}")))?;
        let mut payload = String::with_capacity(
            header_line.len() + bytes + if truncated { marker.len() + 2 } else { 0 } + 1,
        );
        payload.push_str(&header_line);
        payload.push('\n');
        for line in &step_lines {
            payload.push_str(line);
            payload.push('\n');
        }
        if truncated {
            payload.push_str(marker);
            payload.push('\n');
        }

        let path = runs.join(run_file_name(&trace.run_id));
        write_atomic(&path, payload.as_bytes())?;
        prune_runs(&runs);
        Ok(path)
    }

    /// Load one run back (diagnostics/tests). Malformed step lines are skipped;
    /// only a missing file or an unreadable header is an error.
    pub fn load(root: &Path, run_id: &str) -> io::Result<RunTrace> {
        let path = Self::runs_dir(root).join(run_file_name(run_id));
        let text = std::fs::read_to_string(&path)?;
        let mut lines = text.lines();
        let header_line = lines.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("trace {} has no header line", path.display()),
            )
        })?;
        let header: TraceHeader = serde_json::from_str(header_line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid trace header in {}: {error}", path.display()),
            )
        })?;
        let mut steps = Vec::new();
        let mut truncated = header.truncated;
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<RunStep>(line) {
                Ok(step) => {
                    if step.index < MAX_STEPS as u32 {
                        steps.push(step);
                    }
                }
                Err(_) => {
                    // A truncation marker is expected; any other bad line is
                    // skipped so a partially written file stays readable.
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                        if value.get("truncated").and_then(|value| value.as_bool()) == Some(true) {
                            truncated = true;
                        }
                    }
                }
            }
        }
        Ok(header.into_trace(steps, truncated))
    }
}

/// Sanitize a URL for persistence: keep `scheme://host[:port]/path`.
///
/// Query and fragment are dropped (signed URLs commonly carry tokens there);
/// callers that pass non-URL text get `<invalid-url>`.
pub fn redact_url(raw: &str) -> String {
    let Ok(url) = Url::parse(raw.trim()) else {
        return "<invalid-url>".to_owned();
    };
    let mut out = String::with_capacity(url.as_str().len());
    out.push_str(url.scheme());
    out.push(':');
    if let Some(host) = url.host_str() {
        out.push_str("//");
        out.push_str(host);
        if let Some(port) = url.port() {
            out.push(':');
            out.push_str(&port.to_string());
        }
    }
    out.push_str(url.path());
    out
}

/// Sanitize free-form text for persistence.
///
/// Embedded `http(s)` URLs go through [`redact_url`], credential header values
/// (`cookie`/`authorization`/`proxy-authorization`) become `[redacted]`, and
/// the result is capped at [`MAX_EXCERPT_CHARS`] characters.
pub fn redact_excerpt(raw: &str) -> String {
    let mut text = credential_re()
        .replace_all(raw, "$1: [redacted]")
        .into_owned();
    text = url_re()
        .replace_all(&text, |captures: &regex::Captures<'_>| {
            let matched = captures
                .get(0)
                .map(|matched| matched.as_str())
                .unwrap_or_default();
            let trimmed = matched.trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '!', '?']);
            let suffix = &matched[trimmed.len()..];
            format!("{}{}", redact_url(trimmed), suffix)
        })
        .into_owned();
    truncate_chars(&text, MAX_EXCERPT_CHARS)
}

/// `sha256:<hex>` digest used for `RunStep::args_digest`.
pub fn digest_args(args: &str) -> String {
    let digest = Sha256::digest(args.as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

/// `snake_case` JSON name of a failure kind (`recipe_stale`, …).
pub fn error_kind_name(kind: &crate::contract::ResolveErrorKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "internal".to_owned())
}

fn credential_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(proxy-authorization|authorization|cookie)\s*[:=]\s*[^\r\n,;]+")
            .expect("credential header regex is valid")
    })
}

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"https?://[^\s<>"']+"#).expect("URL regex is valid"))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// File-system safe `<run_id>.jsonl` name (no separators, no traversal).
fn run_file_name(run_id: &str) -> String {
    let mut safe: String = run_id
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        safe.push_str("run_unknown");
    }
    safe.push_str(".jsonl");
    safe
}

fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("run.jsonl");
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    let result = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Delete the oldest run files until [`RETENTION_RUNS`] remain.
fn prune_runs(runs: &Path) {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= RETENTION_RUNS {
        return;
    }
    files.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let excess = files.len() - RETENTION_RUNS;
    for (_, path) in files.into_iter().take(excess) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "rdm-resolve: cannot prune old run trace"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn trace_with_steps(run_id: &str, steps: Vec<RunStep>) -> RunTrace {
        RunTrace {
            run_id: run_id.to_owned(),
            created_ms: 1_700_000_000_000,
            mode: "recipe".to_owned(),
            status: "succeeded".to_owned(),
            source_url: "https://example.com/file.bin?token=secret#frag".to_owned(),
            recipe: Some(RecipeRef {
                id: "example.com-file".to_owned(),
                version: 3,
                state: "matched".to_owned(),
            }),
            llm_calls: 0,
            http_requests: 2,
            bytes_read: 4096,
            elapsed_ms: 120,
            error_kind: None,
            steps,
            truncated: false,
        }
    }

    fn step(index: u32) -> RunStep {
        RunStep {
            index,
            op: "http_request".to_owned(),
            args_digest: digest_args(&format!("GET /d/{index}")),
            args_excerpt: Some(format!(
                "GET https://api.example.com/d/{index}?token=secret"
            )),
            outcome: "ok".to_owned(),
            duration_ms: 5,
            obs_id: Some(format!("obs_{index}")),
            note: None,
        }
    }

    #[test]
    fn redact_url_keeps_host_and_path_but_drops_query_and_fragment() {
        assert_eq!(
            redact_url("https://API.Example.com:8443/d/123?token=abc&x=1#frag"),
            "https://api.example.com:8443/d/123"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:8000/files/a.bin?Signature=x"),
            "http://127.0.0.1:8000/files/a.bin"
        );
        assert_eq!(redact_url("https://example.com"), "https://example.com/");
        assert_eq!(redact_url("not a url"), "<invalid-url>");
        assert_eq!(redact_url(""), "<invalid-url>");
    }

    #[test]
    fn redact_excerpt_drops_tokens_and_credential_values() {
        let excerpt = redact_excerpt(
            "GET https://api.example.com/d/123?token=supersecret \
             Cookie: session=abc123 Authorization: Bearer topsecret-key",
        );
        assert!(!excerpt.contains("supersecret"), "{excerpt}");
        assert!(!excerpt.contains("abc123"), "{excerpt}");
        assert!(!excerpt.contains("topsecret-key"), "{excerpt}");
        assert!(
            excerpt.contains("https://api.example.com/d/123"),
            "{excerpt}"
        );
        assert!(excerpt.contains("[redacted]"), "{excerpt}");
    }

    #[test]
    fn redact_excerpt_caps_length_and_keeps_punctuation() {
        let long = format!("see (https://example.com/a?x=1) {}", "y".repeat(600));
        let excerpt = redact_excerpt(&long);
        assert_eq!(excerpt.chars().count(), MAX_EXCERPT_CHARS);
        assert!(
            excerpt.starts_with("see (https://example.com/a)"),
            "{excerpt}"
        );
    }

    #[test]
    fn write_and_load_round_trip_redacts_urls() {
        let dir = TempDir::new("trace-roundtrip");
        let trace = trace_with_steps("run_1_0", vec![step(0), step(1)]);
        let path = TraceStore::write(dir.path(), &trace).expect("write trace");
        assert!(path.ends_with("runs/run_1_0.jsonl"));

        let raw = std::fs::read_to_string(&path).expect("read trace");
        assert!(!raw.contains("token=secret"), "query leaked: {raw}");
        assert!(!raw.contains("token=abc"), "query leaked: {raw}");
        assert!(!raw.contains("frag"), "fragment leaked: {raw}");
        let first = raw.lines().next().expect("header");
        let header: serde_json::Value = serde_json::from_str(first).expect("header json");
        assert_eq!(header["schema"], TRACE_SCHEMA);
        assert_eq!(header["source_url"], "https://example.com/file.bin");
        assert_eq!(header["recipe"]["id"], "example.com-file");
        assert_eq!(
            raw.lines().count(),
            3,
            "header + 2 steps, no truncation marker"
        );

        let loaded = TraceStore::load(dir.path(), "run_1_0").expect("load trace");
        assert_eq!(loaded.run_id, "run_1_0");
        assert_eq!(loaded.status, "succeeded");
        assert_eq!(loaded.recipe.expect("recipe").version, 3);
        assert_eq!(loaded.steps.len(), 2);
        assert_eq!(loaded.steps[1].index, 1);
        assert!(!loaded.truncated);
        assert_eq!(
            loaded.steps[0].args_excerpt.as_deref(),
            Some("GET https://api.example.com/d/0")
        );
    }

    #[test]
    fn write_truncates_at_max_steps_and_marks_the_file() {
        let dir = TempDir::new("trace-truncate-steps");
        let steps: Vec<RunStep> = (0..MAX_STEPS as u32 + 25).map(step).collect();
        let trace = trace_with_steps("run_1_1", steps);
        let path = TraceStore::write(dir.path(), &trace).expect("write trace");

        let raw = std::fs::read_to_string(&path).expect("read trace");
        assert!(raw.trim_end().ends_with("{\"truncated\":true}"), "{raw}");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 1 + MAX_STEPS + 1);
        let loaded = TraceStore::load(dir.path(), "run_1_1").expect("load");
        assert!(loaded.truncated);
        assert_eq!(loaded.steps.len(), MAX_STEPS);
    }

    #[test]
    fn load_tolerates_bad_lines_and_missing_files() {
        let dir = TempDir::new("trace-bad-lines");
        let trace = trace_with_steps("run_1_2", vec![step(0), step(1)]);
        let path = TraceStore::write(dir.path(), &trace).expect("write trace");
        let raw = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.insert(1, "this is not json");
        lines.insert(3, "{\"truncated\":true}");
        std::fs::write(&path, lines.join("\n")).expect("rewrite");

        let loaded = TraceStore::load(dir.path(), "run_1_2").expect("load tolerant");
        assert_eq!(loaded.steps.len(), 2);
        assert!(
            loaded.truncated,
            "a marker line still marks the run truncated"
        );

        let missing = TraceStore::load(dir.path(), "run_missing").expect_err("missing run");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn write_enforces_the_byte_cap() {
        let dir = TempDir::new("trace-bytes");
        let mut steps = Vec::new();
        for index in 0..MAX_STEPS as u32 {
            let mut item = step(index);
            // Excerpts are capped, but a caller-controlled digest/op is not: a
            // pathological trace must still respect MAX_BYTES.
            item.args_digest = "d".repeat(4 * 1024);
            steps.push(item);
        }
        let path =
            TraceStore::write(dir.path(), &trace_with_steps("run_1_3", steps)).expect("write");
        let bytes = std::fs::metadata(&path).expect("metadata").len() as usize;
        assert!(bytes <= MAX_BYTES + 64, "trace is {bytes} bytes");
        let raw = std::fs::read_to_string(&path).expect("read trace");
        assert!(raw.trim_end().ends_with("{\"truncated\":true}"), "{raw}");
        let loaded = TraceStore::load(dir.path(), "run_1_3").expect("load");
        assert!(loaded.truncated);
        assert!(loaded.steps.len() < MAX_STEPS);
    }

    #[test]
    fn write_prunes_old_runs_beyond_retention() {
        let dir = TempDir::new("trace-retention");
        let runs = TraceStore::runs_dir(dir.path());
        std::fs::create_dir_all(&runs).expect("runs dir");
        for index in 0..(RETENTION_RUNS + 5) {
            std::fs::write(
                runs.join(format!("run_old_{index}.jsonl")),
                "{\"schema\":\"rdm.resolve.trace/v1\"}\n",
            )
            .expect("seed old run");
        }
        let path = TraceStore::write(dir.path(), &trace_with_steps("run_new", vec![step(0)]))
            .expect("write new trace");
        assert!(path.exists());
        let count = std::fs::read_dir(&runs)
            .expect("list runs")
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("jsonl")
            })
            .count();
        assert_eq!(count, RETENTION_RUNS);
    }

    #[test]
    fn run_file_names_cannot_escape_the_runs_directory() {
        assert_eq!(run_file_name("../../etc/passwd"), ".._.._etc_passwd.jsonl");
        assert_eq!(run_file_name(""), "run_unknown.jsonl");
        assert!(run_file_name("run_1").ends_with(".jsonl"));
    }

    #[test]
    fn digest_is_stable_and_prefixed() {
        let first = digest_args("GET https://example.com/a");
        assert_eq!(first, digest_args("GET https://example.com/a"));
        assert!(first.starts_with("sha256:"));
        assert_eq!(first.len(), "sha256:".len() + 64);
        assert_ne!(first, digest_args("GET https://example.com/b"));
    }
}
