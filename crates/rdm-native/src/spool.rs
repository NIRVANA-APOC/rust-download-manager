//! On-disk spool for handoffs that could not reach the GUI.
//!
//! Contract (agreed with the Lead and reflected in
//! `docs/extension-protocol.md`): a spool file is the serialized
//! `rdm_ipc::EnqueueItem` the host would have sent over IPC — no wrapper — so
//! the GUI can scan `%LOCALAPPDATA%\RDM\queue\*.json`, deserialize each file
//! as `EnqueueItem`, enqueue it, then delete the file (or move it to
//! `queue/rejected/` on failure).
//!
//! **Privacy decision (Lead, 2026-10-19): credentials are stripped before
//! spooling.** Cookies and `Authorization`/`Proxy-Authorization`/`Cookie`
//! headers are removed by [`strip_credentials`], and the `queued` response
//! reports `credentials_stripped`. The GUI may therefore later fail on
//! downloads that require a login; the extension tells the user to start RDM
//! and retry. This is preferable to writing session credentials to disk.
//!
//! File names are `{unix_ms}-{pid}-{seq}.json`; writes are atomic
//! (`.tmp` + rename) so the GUI never sees a half-written handoff.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rdm_ipc::EnqueueItem;

/// Header names whose values are credentials and must never be persisted.
pub const SENSITIVE_HEADER_NAMES: [&str; 3] = ["authorization", "proxy-authorization", "cookie"];

/// Sub-directory of the RDM state directory used for spooled handoffs.
pub fn queue_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("queue")
}

/// Result of publishing one handoff to the spool directory.
#[derive(Debug, Clone)]
pub struct SpooledHandoff {
    pub path: PathBuf,
    /// Whether any credential material (cookie / sensitive headers) was removed.
    pub credentials_stripped: bool,
}

/// Remove credentials from a handoff so it can safely be written to disk.
///
/// * clears `download.cookie`;
/// * removes headers whose name is `authorization`, `proxy-authorization` or
///   `cookie` (case-insensitive);
/// * keeps non-credential headers (Accept, Referer, UA, …).
///
/// Returns `true` when something was actually removed. This is defence in
/// depth: the extension protocol allow-list already drops such headers, but
/// the spool is a trust boundary and must not depend on that.
pub fn strip_credentials(item: &mut EnqueueItem) -> bool {
    let mut stripped = item.download.cookie.take().is_some();
    let headers_before = item.download.headers.len();
    item.download.headers.retain(|name, _value| {
        !SENSITIVE_HEADER_NAMES.contains(&name.to_ascii_lowercase().as_str())
    });
    if item.download.headers.len() != headers_before {
        stripped = true;
    }
    stripped
}

/// Serialize `item` (with credentials stripped) and atomically publish it in
/// the spool directory.
pub fn spool_item(state_dir: &Path, item: &EnqueueItem) -> std::io::Result<SpooledHandoff> {
    let dir = queue_dir(state_dir);
    fs::create_dir_all(&dir)?;

    let mut safe = item.clone();
    let credentials_stripped = strip_credentials(&mut safe);
    let payload = serde_json::to_vec_pretty(&safe).map_err(std::io::Error::other)?;

    let created_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();

    for sequence in 0u32..1_000 {
        let file_name = format!("{created_ms}-{pid}-{sequence}.json");
        let path = dir.join(&file_name);
        if path.exists() {
            continue;
        }
        let tmp = dir.join(format!(".{file_name}.tmp"));
        fs::write(&tmp, &payload)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => {
                return Ok(SpooledHandoff {
                    path,
                    credentials_stripped,
                })
            }
            Err(err) if path.exists() => {
                // Lost a race for this name; remove the temp file and retry.
                let _ = fs::remove_file(&tmp);
                if sequence == 999 {
                    return Err(err);
                }
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
        }
    }
    Err(std::io::Error::other(
        "could not allocate a unique spool file name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn temp_state_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rdm-native-spool-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample_item() -> EnqueueItem {
        let mut download =
            rdm_core::DownloadRequest::new("https://example.com/big.iso?token=secret");
        download.file_name = Some("big.iso".to_string());
        download.connections = 4;
        download.cookie = Some("sid=topsecret; theme=dark".to_string());
        download.referer = Some("https://example.com/page".to_string());
        download.user_agent = Some("Mozilla/5.0".to_string());
        let mut headers = BTreeMap::new();
        headers.insert("Accept".to_string(), "*/*".to_string());
        headers.insert(
            "Authorization".to_string(),
            "Bearer must-not-persist".to_string(),
        );
        headers.insert("Cookie".to_string(), "sid=topsecret".to_string());
        headers.insert("Proxy-Authorization".to_string(), "Basic leak".to_string());
        download.headers = headers;
        EnqueueItem {
            download,
            source: "firefox".to_string(),
            source_page: Some("https://example.com/page".to_string()),
        }
    }

    #[test]
    fn strip_credentials_removes_cookie_and_sensitive_headers() {
        let mut item = sample_item();
        assert!(strip_credentials(&mut item));
        assert_eq!(item.download.cookie, None);
        assert_eq!(
            item.download.headers.get("Accept").map(String::as_str),
            Some("*/*")
        );
        for name in ["Authorization", "Cookie", "Proxy-Authorization"] {
            assert!(
                !item.download.headers.contains_key(name),
                "{name} must be stripped"
            );
        }
        // Non-sensitive context is preserved.
        assert_eq!(
            item.download.referer.as_deref(),
            Some("https://example.com/page")
        );
        assert_eq!(item.download.user_agent.as_deref(), Some("Mozilla/5.0"));
    }

    #[test]
    fn strip_credentials_is_case_insensitive_and_idempotent() {
        let mut download = rdm_core::DownloadRequest::new("https://example.com/a");
        let mut headers = BTreeMap::new();
        headers.insert("AUTHORIZATION".to_string(), "secret".to_string());
        headers.insert("cookie".to_string(), "a=1".to_string());
        download.headers = headers;
        let mut item = EnqueueItem {
            download,
            source: "firefox".to_string(),
            source_page: None,
        };
        assert!(strip_credentials(&mut item));
        assert!(item.download.headers.is_empty());
        assert!(!strip_credentials(&mut item), "already clean");
        assert_eq!(item.download.cookie, None);
    }

    #[test]
    fn writes_enqueue_item_json_with_credentials_stripped() {
        let state_dir = temp_state_dir("write");
        let item = sample_item();
        let spooled = spool_item(&state_dir, &item).expect("spool");
        assert!(spooled.credentials_stripped);
        assert!(spooled.path.is_file());
        assert_eq!(spooled.path.parent(), Some(queue_dir(&state_dir).as_path()));
        let name = spooled
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(name.ends_with(".json"), "unexpected spool name: {name}");
        assert!(!name.starts_with('.'), "temp files must not be visible");

        // Must deserialize directly as EnqueueItem (GUI scan contract).
        let text = fs::read_to_string(&spooled.path).unwrap();
        let parsed: EnqueueItem = serde_json::from_str(&text).expect("bare EnqueueItem JSON");
        assert_eq!(parsed.download.url, item.download.url);
        assert_eq!(parsed.download.file_name, item.download.file_name);
        assert_eq!(parsed.download.connections, 4);
        assert_eq!(
            parsed.download.referer.as_deref(),
            Some("https://example.com/page")
        );
        assert_eq!(parsed.source, "firefox");
        assert_eq!(parsed.source_page, item.source_page);
        assert!(!text.contains("version"), "no wrapper fields expected");

        // Privacy assertions: no credential value may remain in the file.
        assert_eq!(parsed.download.cookie, None, "cookie must not be persisted");
        assert!(!parsed.download.headers.contains_key("Authorization"));
        for secret in ["topsecret", "must-not-persist", "Basic leak", "sid="] {
            assert!(
                !text.contains(secret),
                "spool file leaked credential material: {secret}"
            );
        }

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn reports_no_stripping_when_nothing_sensitive() {
        let state_dir = temp_state_dir("clean");
        let mut item = sample_item();
        item.download.cookie = None;
        item.download.headers.clear();
        let spooled = spool_item(&state_dir, &item).expect("spool");
        assert!(!spooled.credentials_stripped);
        let text = fs::read_to_string(&spooled.path).unwrap();
        assert!(!text.contains("topsecret"));
        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn consecutive_spools_get_distinct_names() {
        let state_dir = temp_state_dir("unique");
        let item = sample_item();
        let first = spool_item(&state_dir, &item).unwrap();
        let second = spool_item(&state_dir, &item).unwrap();
        assert_ne!(first.path, second.path);
        let files: Vec<_> = fs::read_dir(queue_dir(&state_dir))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 2);
        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn no_temp_files_left_behind() {
        let state_dir = temp_state_dir("tmp");
        spool_item(&state_dir, &sample_item()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(queue_dir(&state_dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        let _ = fs::remove_dir_all(&state_dir);
    }
}
