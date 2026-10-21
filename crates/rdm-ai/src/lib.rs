//! RDM AI helpers (F2).
//!
//! Modules:
//! * [`config`] — OpenAI-compatible provider settings (`<state>/ai.json`).
//! * [`client`] — minimal `chat/completions` client that never leaks the API key.
//! * [`html`] — dependency-free HTML link/text scanner.
//! * [`parse`] — URL → downloadable resources (fetch + LLM + heuristic fallback).
//! * [`classify`] — save-directory classification (extension rules + optional AI).
//! * [`store`] — parse cache (`<state>/ai/parse-cache.json`).
//!
//! The authoritative interface contract is `docs/design-next-features.md` §3.

pub mod classify;
pub mod client;
pub mod config;
pub mod html;
pub mod parse;
pub mod store;

pub use classify::{
    category_for, classify, classify_local, classify_with_ai, list_subdirs, sanitize_dir_name,
    ClassifyInput, ClassifyOutcome, CATEGORY_OTHER,
};
pub use client::{AiClient, AiError, ChatMessage, ChatRole};
pub use config::AiConfig;
pub use parse::{
    normalize_url, parse_url, ParseOptions, ParseReport, ParseRequest, ParsedResource,
    DEGRADED_MAX_AGE_MS, FAILURE_AI_UNAVAILABLE, FAILURE_FETCH_FAILED, FAILURE_FORBIDDEN,
    FAILURE_INVALID_URL, FAILURE_LOGIN_REQUIRED, FAILURE_NETWORK_ERROR, FAILURE_NOT_FOUND,
    FAILURE_NO_RESOURCES, FAILURE_RATE_LIMITED, FAILURE_SERVER_ERROR, FAILURE_TIMEOUT,
};
pub use store::{CachedParse, ParseCache, ParseSession};

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (0 if the system clock is before 1970).
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Truncate to at most `max_chars` characters, appending `…` when shortened.
pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Atomically replace `path` with `contents`.
///
/// The temporary file lives next to the destination so the final `rename` stays
/// on one volume. Its name is unique per call (`.<name>.<pid>.<seq>.tmp`):
/// a fixed `.tmp` name lets two concurrent writers truncate each other's file
/// and publish a half-written cache. On Unix the file is made owner-only
/// (`0600`) before it is published; Windows ACLs are left as-is (matching
/// `rdm-ipc`).
pub(crate) fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("atomic");
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    std::fs::write(&tmp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
}

/// Extract the first balanced JSON object out of an LLM reply, tolerating
/// markdown ```json fences``` and surrounding prose.
///
/// Public since v0.4: `rdm-resolve` parses JsonAction replies with the same
/// tolerant extractor the classify/parse paths use.
pub fn extract_json_object(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let body = if let Some(rest) = trimmed.strip_prefix("```") {
        // Drop an optional language tag, e.g. `json`.
        let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or(rest);
        rest.trim_end_matches('`').trim()
    } else {
        trimmed
    };
    let start = body.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in body[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + character.len_utf8();
                    return Some(&body[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Shared test helpers: serializes tests that mutate process-wide environment
/// variables (`RDM_AI_API_KEY`).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    pub static ENV_LOCK: Mutex<()> = Mutex::new(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_keeps_short_values() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn truncate_chars_marks_cut_values() {
        assert_eq!(truncate_chars("hello", 2), "he…");
        assert_eq!(truncate_chars("hello", 0), "");
    }

    #[test]
    fn extract_json_object_handles_fences_and_prose() {
        assert_eq!(
            extract_json_object("```json\n{\"dir\":\"视频\"}\n```"),
            Some("{\"dir\":\"视频\"}")
        );
        assert_eq!(
            extract_json_object("好的：{\"a\":1} 完成"),
            Some("{\"a\":1}")
        );
        assert_eq!(extract_json_object("no json here"), None);
        assert_eq!(extract_json_object("{}{}"), Some("{}"));
    }

    #[test]
    fn write_atomic_replaces_file() {
        let dir =
            std::env::temp_dir().join(format!("rdm-ai-atomic-{}-{}", std::process::id(), now_ms()));
        let path = dir.join("nested").join("data.json");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_atomic_writes_publish_one_complete_value() {
        // A fixed `.tmp` name let two writers truncate each other's file; the
        // published file must always be exactly one writer's complete payload.
        let dir = std::env::temp_dir().join(format!(
            "rdm-ai-atomic-race-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.json");
        let values: Vec<String> = (0..8)
            .map(|index| format!("{{\"n\":{index},\"pad\":\"{}\"}}", "x".repeat(2048)))
            .collect();

        std::thread::scope(|scope| {
            for value in &values {
                let path = path.clone();
                scope.spawn(move || {
                    for _ in 0..10 {
                        write_atomic(&path, value).unwrap();
                    }
                });
            }
        });

        let published = std::fs::read_to_string(&path).unwrap();
        assert!(
            values.contains(&published),
            "published content must be one writer's complete value, got {} bytes",
            published.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
