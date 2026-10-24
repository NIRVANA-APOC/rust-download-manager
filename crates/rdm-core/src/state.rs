//! Crash-safe resume state (`<target>.part.json`) and path helpers.
//!
//! The data file is `<target>.part` and the state file is
//! `<target>.part.json`. Writes are atomic (temp file + rename), so a crash
//! either keeps the previous state or the new one — never a torn JSON document.
//!
//! ## Two state formats
//!
//! * **v1** recorded `completed_chunks` (a bitmap of fixed-size chunks). It is
//!   still readable: chunks are converted into byte ranges on load, so a user
//!   upgrading mid-download keeps their progress.
//! * **v2** records `completed_ranges`, a sorted list of half-open byte ranges.
//!   This lets the kernel split a chunk in flight (tail stealing) and still
//!   resume exactly: any completed sub-range is remembered, and only the
//!   complement is downloaded again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{DownloadRequest, RemoteMeta};

/// Current writer format.
pub(crate) const STATE_VERSION: u32 = 2;
/// Old chunk-bitmap format, still accepted on load.
pub(crate) const STATE_VERSION_LEGACY: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeState {
    pub version: u32,
    pub url: String,
    pub final_url: String,
    pub file_name: String,
    pub total_bytes: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Base chunk geometry of the run that wrote this state (informational for
    /// v2; used to convert v1 chunk ids into ranges).
    pub chunk_size: u64,
    pub total_chunks: u64,
    /// v1 payload; empty for v2 writers.
    #[serde(default)]
    pub completed_chunks: Vec<u64>,
    /// v2 payload: sorted, non-overlapping half-open byte ranges.
    #[serde(default)]
    pub completed_ranges: Vec<[u64; 2]>,
    pub downloaded_bytes: u64,
}

impl ResumeState {
    /// Normalized, sorted, merged completed byte ranges.
    ///
    /// v1 states are converted from chunk ids; v2 states are validated and
    /// merged. Returns `None` if the stored ranges are inconsistent (out of
    /// bounds, overlapping, zero-length), in which case the caller restarts.
    pub fn completed_byte_ranges(&self) -> Option<Vec<(u64, u64)>> {
        let raw: Vec<(u64, u64)> = match self.version {
            STATE_VERSION_LEGACY => {
                if self.chunk_size == 0 || self.total_chunks == 0 {
                    // Empty file: no ranges, but valid.
                    return if self.total_bytes == 0 {
                        Some(Vec::new())
                    } else {
                        None
                    };
                }
                let expected = self.total_bytes.div_ceil(self.chunk_size);
                if expected != self.total_chunks {
                    return None;
                }
                let mut ranges = Vec::with_capacity(self.completed_chunks.len());
                for &index in &self.completed_chunks {
                    if index >= self.total_chunks {
                        return None;
                    }
                    let start = index * self.chunk_size;
                    let end = (start + self.chunk_size).min(self.total_bytes);
                    if start >= end {
                        return None;
                    }
                    ranges.push((start, end));
                }
                ranges
            }
            STATE_VERSION => {
                let mut ranges = Vec::with_capacity(self.completed_ranges.len());
                for pair in &self.completed_ranges {
                    let (start, end) = (pair[0], pair[1]);
                    if start >= end || end > self.total_bytes {
                        return None;
                    }
                    ranges.push((start, end));
                }
                ranges
            }
            _ => return None,
        };
        Some(merge_ranges(raw))
    }
}

/// Sort and merge touching/overlapping ranges.
pub(crate) fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|(start, end)| start < end);
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, last_end)) if start <= *last_end => {
                if end > *last_end {
                    *last_end = end;
                }
            }
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Complement of `ranges` inside `[0, total)` — the work that still has to be
/// downloaded.
pub(crate) fn missing_ranges(ranges: &[(u64, u64)], total: u64) -> Vec<(u64, u64)> {
    let mut missing = Vec::new();
    let mut cursor = 0u64;
    for &(start, end) in ranges {
        if start > cursor {
            missing.push((cursor, start));
        }
        cursor = cursor.max(end);
    }
    if cursor < total {
        missing.push((cursor, total));
    }
    missing
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
        Ok(state) if state.version == STATE_VERSION || state.version == STATE_VERSION_LEGACY => {
            Some(state)
        }
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
/// Checks total length, original/final URL, `ETag`, `Last-Modified`, that the
/// stored base geometry is self-consistent, and (v2) that the byte ranges are
/// well-formed. A mismatch means "server file changed" (or our own state is
/// stale) and the transfer restarts from zero.
pub(crate) fn is_compatible(
    state: &ResumeState,
    request: &DownloadRequest,
    meta: &RemoteMeta,
    total: u64,
) -> bool {
    if state.version != STATE_VERSION && state.version != STATE_VERSION_LEGACY {
        return false;
    }
    if state.url != request.url
        || state.final_url != meta.final_url
        || state.total_bytes != total
        || state.etag != meta.etag
        || state.last_modified != meta.last_modified
        || state.chunk_size == 0
        || state.total_chunks != total.div_ceil(state.chunk_size)
    {
        return false;
    }
    state.completed_byte_ranges().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RemoteMeta;

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

    fn state_v2(total: u64) -> ResumeState {
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
            completed_chunks: Vec::new(),
            completed_ranges: vec![[0, 8 * 1024 * 1024]],
            downloaded_bytes: 8 * 1024 * 1024,
        }
    }

    fn state_v1(total: u64) -> ResumeState {
        ResumeState {
            version: STATE_VERSION_LEGACY,
            url: "https://example.com/file.bin".to_string(),
            final_url: "https://cdn.example.com/file.bin".to_string(),
            file_name: "file.bin".to_string(),
            total_bytes: total,
            etag: Some("\"abc\"".to_string()),
            last_modified: None,
            chunk_size: 4 * 1024 * 1024,
            total_chunks: total.div_ceil(4 * 1024 * 1024),
            completed_chunks: vec![0, 1],
            completed_ranges: Vec::new(),
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
    fn compatible_state_is_accepted_for_both_formats() {
        let total = 32 * 1024 * 1024;
        for state in [state_v2(total), state_v1(total)] {
            assert!(is_compatible(
                &state,
                &request("https://example.com/file.bin"),
                &meta(
                    "https://cdn.example.com/file.bin",
                    Some(total),
                    Some("\"abc\"")
                ),
                total
            ));
        }
    }

    #[test]
    fn changed_validators_are_rejected() {
        let total = 32 * 1024 * 1024;
        let state = state_v2(total);
        let req = request("https://example.com/file.bin");

        assert!(!is_compatible(
            &state,
            &req,
            &meta(
                "https://cdn.example.com/file.bin",
                Some(total),
                Some("\"changed\"")
            ),
            total
        ));
        assert!(!is_compatible(
            &state,
            &req,
            &meta(
                "https://cdn.example.com/file.bin",
                Some(total),
                Some("\"abc\"")
            ),
            total + 1
        ));
        assert!(!is_compatible(
            &state,
            &req,
            &meta(
                "https://other.example.com/file.bin",
                Some(total),
                Some("\"abc\"")
            ),
            total
        ));
        assert!(!is_compatible(
            &state,
            &request("https://example.com/other.bin"),
            &meta(
                "https://cdn.example.com/file.bin",
                Some(total),
                Some("\"abc\"")
            ),
            total
        ));
    }

    #[test]
    fn v1_chunks_are_converted_to_ranges() {
        let state = state_v1(32 * 1024 * 1024);
        assert_eq!(
            state.completed_byte_ranges(),
            Some(vec![(0, 8 * 1024 * 1024)])
        );
    }

    #[test]
    fn v2_ranges_are_merged_and_validated() {
        let mut state = state_v2(32 * 1024 * 1024);
        state.completed_ranges = vec![[4 * 1024 * 1024, 8 * 1024 * 1024], [0, 4 * 1024 * 1024]];
        assert_eq!(
            state.completed_byte_ranges(),
            Some(vec![(0, 8 * 1024 * 1024)])
        );
        state.completed_ranges = vec![[0, 4], [2, 8]];
        assert_eq!(state.completed_byte_ranges(), Some(vec![(0, 8)]));
        // Out of bounds / empty / reversed ranges are invalid.
        state.completed_ranges = vec![[0, 33 * 1024 * 1024]];
        assert_eq!(state.completed_byte_ranges(), None);
        state.completed_ranges = vec![[5, 5]];
        assert_eq!(state.completed_byte_ranges(), None);
        state.completed_ranges = vec![[9, 3]];
        assert_eq!(state.completed_byte_ranges(), None);
    }

    #[test]
    fn missing_ranges_is_the_complement() {
        let total = 32;
        let done = merge_ranges(vec![(0, 4), (8, 12), (12, 16), (30, 32)]);
        assert_eq!(missing_ranges(&done, total), vec![(4, 8), (16, 30)]);
        assert_eq!(missing_ranges(&[], total), vec![(0, 32)]);
        assert_eq!(missing_ranges(&[(0, 32)], total), Vec::<(u64, u64)>::new());
    }

    #[tokio::test]
    async fn atomic_roundtrip_and_corruption_tolerance() {
        let dir = std::env::temp_dir().join(format!("rdm-state-test-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("file.bin.part.json");

        let state = state_v2(32 * 1024 * 1024);
        save_atomic(&path, &state).await.unwrap();
        assert_eq!(load(&path).await, Some(state.clone()));
        // No temp file is left behind.
        assert!(!path.with_file_name("file.bin.part.json.tmp").exists());

        // A legacy v1 document is still readable.
        let legacy = state_v1(32 * 1024 * 1024);
        tokio::fs::write(&path, serde_json::to_vec(&legacy).unwrap())
            .await
            .unwrap();
        assert_eq!(load(&path).await, Some(legacy));

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
