//! 下载目录 / 状态目录的递归扫描（独立线程，不碰 UI）。
//!
//! 契约（`docs/design/13-resource-monitor.md` §1.2）：
//! * 统计普通文件的 `metadata.len()`（逻辑大小，与资源管理器「大小」一致）；
//! * 跳过符号链接与无权限目录，记录 `skipped_dirs`；
//! * 最多 [`MAX_SCAN_ENTRIES`] 个目录项、[`MAX_SCAN_DEPTH`] 层深度，超出置 `truncated`；
//! * 不读取文件内容、不访问网络。

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use super::{DiskUsage, LibraryUsage, ScanOutcome};

/// 单次扫描最多读取的目录项数。
pub const MAX_SCAN_ENTRIES: u64 = 20_000;
/// 递归深度上限（根目录为第 0 层）。
pub const MAX_SCAN_DEPTH: u32 = 8;

/// 一次扫描的输入。
#[derive(Debug, Clone)]
pub struct ScanRequest {
    /// 下载目录（`Settings::default_dir`）；未设置时为 `None`。
    pub root: Option<PathBuf>,
    /// 状态目录（`%LOCALAPPDATA%\RDM`）。
    pub state_dir: PathBuf,
    /// 下载记录里已知的文件路径（用于区分「未归属文件」）。
    pub tracked_files: Vec<PathBuf>,
}

/// 状态目录 / `.part` / 未归属等维护口径的数据。
#[derive(Debug, Clone, Default)]
pub struct HygieneScan {
    pub part_bytes: u64,
    pub part_files: u64,
    pub untracked_bytes: u64,
    pub untracked_files: u64,
    pub state_dir_bytes: u64,
    pub ai_cache_bytes: u64,
    pub ai_cache_entries: u64,
    pub logs_bytes: u64,
}

/// 扫描线程的完整结果。
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub root: Option<PathBuf>,
    pub scanned_at_ms: u64,
    pub scan_ms: u64,
    pub disk: Option<DiskUsage>,
    pub library: LibraryUsage,
    pub hygiene: HygieneScan,
    pub truncated: bool,
    pub skipped_dirs: u64,
}

impl ScanResult {
    /// 无下载目录/根目录不存在时的空结果。
    pub fn empty(root: Option<PathBuf>, state_dir: &std::path::Path, now_ms: u64) -> Self {
        let state_dir_bytes = directory_size(state_dir, 4_000);
        Self {
            root,
            scanned_at_ms: now_ms,
            scan_ms: 0,
            disk: None,
            library: LibraryUsage::default(),
            hygiene: HygieneScan {
                state_dir_bytes,
                ..HygieneScan::default()
            },
            truncated: false,
            skipped_dirs: 0,
        }
    }
}

/// 启动扫描线程；结果通过通道回到 GUI 主线程（在 `EngineMsg::Tick` 里非阻塞轮询）。
pub fn spawn(request: ScanRequest) -> Receiver<ScanOutcome> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("rdm-monitor-scan".to_owned())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan(&request)))
                .map(|result| ScanOutcome::Ok(Box::new(result)))
                .unwrap_or_else(|_| ScanOutcome::Err("扫描线程异常终止".to_owned()));
            let _ = tx.send(outcome);
        })
        .map(|_| rx)
        .unwrap_or_else(|error| {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(ScanOutcome::Err(format!("无法启动扫描线程：{error}")));
            rx
        })
}

/// 同步执行一次扫描（线程内调用；也便于单测）。
pub fn scan(request: &ScanRequest) -> ScanResult {
    let started = std::time::Instant::now();
    let now = super::now_ms();
    let Some(root) = request.root.as_ref() else {
        return ScanResult::empty(None, &request.state_dir, now);
    };
    if !root.is_dir() {
        return ScanResult::empty(Some(root.clone()), &request.state_dir, now);
    }

    let disk = crate::desktop::disk_space_for(root).map(|(free, total)| DiskUsage {
        free_bytes: free,
        total_bytes: total,
    });

    let tracked = normalize_tracked(&request.tracked_files);
    let mut walker = Walker::new(root, &tracked);
    walker.walk();
    let mut result = ScanResult {
        root: Some(root.clone()),
        scanned_at_ms: now,
        scan_ms: started.elapsed().as_millis() as u64,
        disk,
        library: walker.library,
        hygiene: HygieneScan {
            part_bytes: walker.part_bytes,
            part_files: walker.part_files,
            untracked_bytes: walker.untracked_bytes,
            untracked_files: walker.untracked_files,
            state_dir_bytes: directory_size(&request.state_dir, 4_000),
            ai_cache_bytes: file_size(&request.state_dir.join("ai").join("parse-cache.json")),
            ai_cache_entries: 0,
            logs_bytes: directory_size(&request.state_dir.join("logs"), 2_000),
        },
        truncated: walker.truncated,
        skipped_dirs: walker.skipped_dirs,
    };
    result.hygiene.ai_cache_entries = parse_cache_entries(&request.state_dir);
    result.scan_ms = started.elapsed().as_millis() as u64;
    result
}

// ---------------------------------------------------------------------------
// 目录遍历
// ---------------------------------------------------------------------------

struct Walker<'a> {
    root: &'a std::path::Path,
    tracked: &'a [String],
    library: LibraryUsage,
    part_bytes: u64,
    part_files: u64,
    untracked_bytes: u64,
    untracked_files: u64,
    truncated: bool,
    skipped_dirs: u64,
    entries: u64,
}

impl<'a> Walker<'a> {
    fn new(root: &'a std::path::Path, tracked: &'a [String]) -> Self {
        Self {
            root,
            tracked,
            library: LibraryUsage::default(),
            part_bytes: 0,
            part_files: 0,
            untracked_bytes: 0,
            untracked_files: 0,
            truncated: false,
            skipped_dirs: 0,
            entries: 0,
        }
    }

    fn walk(&mut self) {
        self.library.exists = true;
        self.library.top_dirs = count_top_dirs(self.root);
        let mut stack = vec![(self.root.to_path_buf(), 0u32)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(read_dir) = std::fs::read_dir(&dir) else {
                self.skipped_dirs = self.skipped_dirs.saturating_add(1);
                continue;
            };
            for entry in read_dir.flatten() {
                self.entries = self.entries.saturating_add(1);
                if self.entries > MAX_SCAN_ENTRIES {
                    self.truncated = true;
                    return;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if file_type.is_symlink() {
                    self.skipped_dirs = self.skipped_dirs.saturating_add(1);
                    continue;
                }
                if file_type.is_dir() {
                    if depth < MAX_SCAN_DEPTH {
                        stack.push((path, depth + 1));
                    } else {
                        self.truncated = true;
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let bytes = metadata.len();
                self.add_file(&path, &name, bytes, &metadata);
            }
        }
    }

    fn add_file(
        &mut self,
        path: &std::path::Path,
        name: &str,
        bytes: u64,
        metadata: &std::fs::Metadata,
    ) {
        self.library.total_files = self.library.total_files.saturating_add(1);
        self.library.total_bytes = self.library.total_bytes.saturating_add(bytes);
        let tracked = self.is_tracked(path);
        let is_part = name.ends_with(".part") || name.ends_with(".part.json");
        // `.part` 单独统计为「断点数据」，不计入「未归属文件」，
        // 否则进行中的任务（GUI 此时还没有最终 path）会让待整理空间虚高。
        if !tracked && !is_part {
            self.untracked_bytes = self.untracked_bytes.saturating_add(bytes);
            self.untracked_files = self.untracked_files.saturating_add(1);
        }
        if is_part {
            self.part_bytes = self.part_bytes.saturating_add(bytes);
            self.part_files = self.part_files.saturating_add(1);
        }
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        let category = crate::views::app::FileCategory::for_file(name);
        if let Some(existing) = self
            .library
            .categories
            .iter_mut()
            .find(|usage| usage.category == category)
        {
            existing.bytes = existing.bytes.saturating_add(bytes);
            existing.files = existing.files.saturating_add(1);
        } else {
            self.library.categories.push(super::CategoryUsage {
                category,
                bytes,
                files: 1,
            });
        }
        self.library.largest_files.push(super::FileUsage {
            name: name.to_owned(),
            relative_dir: path
                .parent()
                .and_then(|parent| parent.strip_prefix(self.root).ok())
                .map(|relative| relative.display().to_string())
                .unwrap_or_default(),
            bytes,
            modified_ms,
            category,
            tracked,
        });
        self.library
            .largest_files
            .sort_by_key(|file| std::cmp::Reverse(file.bytes));
        self.library.largest_files.truncate(8);
        self.library
            .categories
            .sort_by_key(|category| std::cmp::Reverse(category.bytes));
    }

    fn is_tracked(&self, path: &std::path::Path) -> bool {
        let normalized = normalize_path(path);
        self.tracked.iter().any(|known| {
            known == &normalized
                || known == &format!("{normalized}.part")
                || known == &format!("{normalized}.part.json")
        })
    }
}

fn count_top_dirs(root: &std::path::Path) -> u64 {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
                .count() as u64
        })
        .unwrap_or(0)
}

/// 路径归一化（Windows 大小写不敏感；统一小写 + 正斜杠）。
fn normalize_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

fn normalize_tracked(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|path| normalize_path(path)).collect()
}

/// 目录大小（有界递归；用于状态目录/日志）。
pub fn directory_size(root: &std::path::Path, max_entries: u64) -> u64 {
    let mut total = 0u64;
    let mut entries = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            entries += 1;
            if entries > max_entries {
                return total;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                total = total.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
            }
        }
    }
    total
}

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn parse_cache_entries(state_dir: &std::path::Path) -> u64 {
    let path = state_dir.join("ai").join("parse-cache.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("entries")?
                .as_object()
                .map(|map| map.len() as u64)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rdm-scan-{tag}-{}-{}",
            std::process::id(),
            super::super::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scans_nested_directories_and_splits_part_files() {
        let dir = temp_dir("nested");
        std::fs::create_dir_all(dir.join("视频")).unwrap();
        std::fs::write(dir.join("视频").join("movie.mp4"), vec![0u8; 1024]).unwrap();
        std::fs::write(dir.join("notes.pdf"), vec![0u8; 128]).unwrap();
        std::fs::write(dir.join("pending.iso.part"), vec![0u8; 64]).unwrap();

        let request = ScanRequest {
            root: Some(dir.clone()),
            state_dir: dir.join("state"),
            tracked_files: vec![dir.join("notes.pdf")],
        };
        let result = scan(&request);
        assert_eq!(result.library.total_files, 3);
        assert_eq!(result.library.total_bytes, 1024 + 128 + 64);
        assert_eq!(result.library.categories.len(), 3);
        assert_eq!(result.hygiene.part_files, 1);
        assert_eq!(result.hygiene.part_bytes, 64);
        assert_eq!(
            result.hygiene.untracked_files, 1,
            "movie only: .part is reported separately"
        );
        assert_eq!(result.hygiene.untracked_bytes, 1024);
        assert!(result.disk.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_root_yields_an_empty_result() {
        let state = temp_dir("missing");
        let request = ScanRequest {
            root: Some(state.join("does-not-exist")),
            state_dir: state.clone(),
            tracked_files: Vec::new(),
        };
        let result = scan(&request);
        assert!(!result.library.exists);
        assert_eq!(result.library.total_files, 0);
        assert!(result.disk.is_none());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn normalize_path_is_case_insensitive() {
        let tracked = normalize_tracked(&[PathBuf::from("C:\\Users\\A\\File.MP4")]);
        assert_eq!(tracked[0], "c:/users/a/file.mp4");
    }
}
