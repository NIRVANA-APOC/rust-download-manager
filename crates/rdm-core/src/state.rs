//! Crash-safe resume state (`<target>.part.json`) and path helpers.
//!
//! Layout matches architecture §3.4: the data file is `<target>.part` and the
//! state file is `<target>.part.json`. Writes are atomic (temp file + rename),
//! so a crash either keeps the previous state or the new one — never a torn
//! JSON document.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{DownloadRequest, RemoteMeta};

pub(crate) const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeState {
    pub version: u32,
    pub url: String,
    pub final_url: String,
    pub file_name: String,
    pub total_bytes: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub completed_chunks: Vec<u64>,
    pub downloaded_bytes: u64,
}

/// `<target>.part`
pub(crate) fn part_path(target: &Path) -> PathBuf {
    append_suffix(target, ".part")
}

/// `<target>.part.json`
pub(crate) fn state_path(target: &Path) -> PathBuf {
    append_suffix(target, ".part.json")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Load and parse resume state. Corrupt/unsupported state is a warning, never
/// a hard failure: the caller simply restarts from zero.
pub(crate) async fn load(path: &Path) -> Option<ResumeState> {
    let data = match tokio::fs::read(path).await {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not read resume state");
            return None;
        }
    };
    match serde_json::from_slice::<ResumeState>(&data) {
        Ok(state) if state.version == STATE_VERSION => Some(state),
        Ok(state) => {
            tracing::warn!(
                path = %path.display(),
                version = state.version,
                "unsupported resume state version; restarting"
            );
            None
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "corrupt resume state; restarting");
            None
        }
    }
}

/// Atomic save: temp file in the same directory, fsync, rename over the state.
pub(crate) async fn save_atomic(path: &Path, state: &ResumeState) -> std::io::Result<()> {
    let data = serde_json::to_vec(state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let tmp = append_suffix(path, ".tmp");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(&data).await?;
        file.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await
}

/// Validators must match before we trust an existing `.part` file.
///
/// Checks total length, original/final URL, `ETag`, `Last-Modified` and that
/// the stored chunk geometry is self-consistent. A mismatch means "server file
/// changed" (or our own state is stale) and the transfer restarts from zero.
pub(crate) fn is_compatible(
    state: &ResumeState,
    request: &DownloadRequest,
    meta: &RemoteMeta,
    total: u64,
) -> bool {
    state.version == STATE_VERSION
        && state.url == request.url
        && state.final_url == meta.final_url
        && state.total_bytes == total
        && state.etag == meta.etag
        && state.last_modified == meta.last_modified
        && state.chunk_size > 0
        && state.total_chunks == total.div_ceil(state.chunk_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(final_url: &str, total: Option<u64>, etag: Option<&str>) -> RemoteMeta {
        RemoteMeta {
            final_url: final_url.to_string(),
            file_name: "file.bin".to_string(),
            total_bytes: total,
            supports_ranges: true,
            content_type: None,
            etag: etag.map(str::to_owned),
            last_modified: None,
            http_version: None,
        }
    }

    fn request(url: &str) -> DownloadRequest {
        DownloadRequest::new(url)
    }

    fn state_for(total: u64) -> ResumeState {
        ResumeState {
            version: STATE_VERSION,
            url: "https://example.com/file.bin".to_string(),
            final_url: "https://cdn.example.com/file.bin".to_string(),
            file_name: "file.bin".to_string(),
            total_bytes: total,
            etag: Some("\"abc\"".to_string()),
            last_modified: None,
            chunk_size: 4 * 1024 * 1024,
            total_chunks: total.div_ceil(4 * 1024 * 1024),
            completed_chunks: vec![0, 1],
            downloaded_bytes: 8 * 1024 * 1024,
        }
    }

    #[test]
    fn path_helpers_match_contract() {
        let target = Path::new("dir").join("file.bin");
        assert_eq!(part_path(&target), Path::new("dir").join("file.bin.part"));
        assert_eq!(
            state_path(&target),
            Path::new("dir").join("file.bin.part.json")
        );
    }

    #[test]
    fn compatible_state_is_accepted() {
        let total = 32 * 1024 * 1024;
        let state = state_for(total);
        assert!(is_compatible(
            &state,
            &request("https://example.com/file.bin"),
            &meta("https://cdn.example.com/file.bin", Some(total), Some("\"abc\"")),
            total
        ));
    }

    #[test]
    fn changed_validators_are_rejected() {
        let total = 32 * 1024 * 1024;
        let state = state_for(total);
        let req = request("https://example.com/file.bin");

        assert!(!is_compatible(
            &state,
            &req,
            &meta("https://cdn.example.com/file.bin", Some(total), Some("\"changed\"")),
            total
        ));
        assert!(!is_compatible(
            &state,
            &req,
            &meta("https://cdn.example.com/file.bin", Some(total), Some("\"abc\"")),
            total + 1
        ));
        assert!(!is_compatible(
            &state,
            &req,
            &meta("https://other.example.com/file.bin", Some(total), Some("\"abc\"")),
            total
        ));
        assert!(!is_compatible(
            &state,
            &request("https://example.com/other.bin"),
            &meta("https://cdn.example.com/file.bin", Some(total), Some("\"abc\"")),
            total
        ));
    }

    #[tokio::test]
    async fn atomic_roundtrip_and_corruption_tolerance() {
        let dir = std::env::temp_dir().join(format!("rdm-state-test-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("file.bin.part.json");

        let state = state_for(32 * 1024 * 1024);
        save_atomic(&path, &state).await.unwrap();
        assert_eq!(load(&path).await, Some(state.clone()));
        // No temp file is left behind.
        assert!(!path.with_file_name("file.bin.part.json.tmp").exists());

        // Corrupt JSON and unsupported versions are ignored, not fatal.
        tokio::fs::write(&path, b"not json").await.unwrap();
        assert_eq!(load(&path).await, None);
        let mut newer = state;
        newer.version = 99;
        tokio::fs::write(&path, serde_json::to_vec(&newer).unwrap())
            .await
            .unwrap();
        assert_eq!(load(&path).await, None);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
