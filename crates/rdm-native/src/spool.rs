//! On-disk spool for handoffs that could not reach the GUI.
//!
//! Contract (agreed with the Lead and reflected in
//! `docs/extension-protocol.md`): a spool file is *exactly* the serialized
//! `rdm_ipc::EnqueueItem` the host would have sent over IPC — no wrapper — so
//! the GUI can scan `%LOCALAPPDATA%\RDM\queue\*.json`, deserialize each file
//! as `EnqueueItem`, enqueue it, then delete the file (or move it to
//! `queue/rejected/` on failure).
//!
//! File names are `{unix_ms}-{pid}-{seq}.json`; writes are atomic
//! (`.tmp` + rename) so the GUI never sees a half-written handoff.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rdm_ipc::EnqueueItem;

/// Sub-directory of the RDM state directory used for spooled handoffs.
pub fn queue_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("queue")
}

/// Serialize `item` and atomically publish it in the spool directory.
///
/// Returns the path of the new spool file.
pub fn spool_item(state_dir: &Path, item: &EnqueueItem) -> std::io::Result<PathBuf> {
    let dir = queue_dir(state_dir);
    fs::create_dir_all(&dir)?;
    let payload = serde_json::to_vec_pretty(item).map_err(std::io::Error::other)?;
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
            Ok(()) => return Ok(path),
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
        let mut download = rdm_core::DownloadRequest::new("https://example.com/big.iso?token=secret");
        download.file_name = Some("big.iso".to_string());
        download.connections = 4;
        download.cookie = Some("a=1; b=2".to_string());
        download.referer = Some("https://example.com/page".to_string());
        download.user_agent = Some("Mozilla/5.0".to_string());
        let mut headers = BTreeMap::new();
        headers.insert("Accept".to_string(), "*/*".to_string());
        download.headers = headers;
        EnqueueItem {
            download,
            source: "firefox".to_string(),
            source_page: Some("https://example.com/page".to_string()),
        }
    }

    #[test]
    fn writes_enqueue_item_json_without_wrapper() {
        let state_dir = temp_state_dir("write");
        let item = sample_item();
        let path = spool_item(&state_dir, &item).unwrap();
        assert!(path.is_file());
        assert_eq!(path.parent(), Some(queue_dir(&state_dir).as_path()));
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with(".json"), "unexpected spool name: {name}");
        assert!(!name.starts_with('.'), "temp files must not be visible");

        // Must deserialize directly as EnqueueItem (GUI scan contract).
        let text = fs::read_to_string(&path).unwrap();
        let parsed: EnqueueItem = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.download.url, item.download.url);
        assert_eq!(parsed.download.file_name, item.download.file_name);
        assert_eq!(parsed.download.connections, 4);
        assert_eq!(parsed.download.cookie.as_deref(), Some("a=1; b=2"));
        assert_eq!(parsed.source, "firefox");
        assert_eq!(parsed.source_page, item.source_page);
        assert!(!text.contains("version"), "no wrapper fields expected");
        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn consecutive_spools_get_distinct_names() {
        let state_dir = temp_state_dir("unique");
        let item = sample_item();
        let first = spool_item(&state_dir, &item).unwrap();
        let second = spool_item(&state_dir, &item).unwrap();
        assert_ne!(first, second);
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
