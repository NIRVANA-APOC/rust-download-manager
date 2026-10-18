//! `%LOCALAPPDATA%\RDM\queue\*.json` 兜底队列的启动扫描。
//!
//! Firefox 原生宿主在 GUI 未运行时把 `rdm_ipc::EnqueueItem` 裸 JSON 落盘到
//! `queue/`（见 `docs/extension-protocol.md` §1.3）。GUI 启动、发布 `ipc.json`
//! 之后扫描该目录：成功解析的入队并删除文件，解析失败的移动到 `queue/rejected/`
//! 保留现场，绝不阻塞启动。

use std::path::{Path, PathBuf};

use rdm_ipc::EnqueueItem;

#[derive(Debug, Default)]
pub struct SpoolDrain {
    /// 成功解析、待入队的项目（按文件名排序，保证确定性）。
    pub items: Vec<EnqueueItem>,
    /// 移动到 `rejected/` 的坏文件数。
    pub rejected: usize,
    /// 无法移动/读取而被跳过的文件数。
    pub skipped: usize,
}

pub fn queue_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("queue")
}

pub fn rejected_dir(state_dir: &Path) -> PathBuf {
    queue_dir(state_dir).join("rejected")
}

/// 扫描并消费兜底队列。幂等、容错：任何单个文件的失败都不会中断整体扫描。
pub fn drain_spool(state_dir: &Path) -> SpoolDrain {
    let mut out = SpoolDrain::default();
    let dir = queue_dir(state_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return out,
        Err(err) => {
            tracing::warn!(path = %dir.display(), error = %err, "无法读取 spool 队列目录");
            return out;
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("json")
        })
        .collect();
    paths.sort();

    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|err| format!("读取失败: {err}"))
            .and_then(|text| {
                serde_json::from_str::<EnqueueItem>(&text)
                    .map_err(|err| format!("JSON 解析失败: {err}"))
            });

        match parsed {
            Ok(item) => match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::info!(path = %path.display(), url = %item.download.url, "从 spool 队列恢复下载");
                    out.items.push(item);
                }
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "spool 文件删除失败，跳过");
                    out.skipped += 1;
                }
            },
            Err(reason) => {
                tracing::warn!(path = %path.display(), reason = %reason, "spool 文件无效，移动到 rejected/");
                if reject_file(state_dir, &path) {
                    out.rejected += 1;
                } else {
                    out.skipped += 1;
                }
            }
        }
    }
    out
}

fn reject_file(state_dir: &Path, path: &Path) -> bool {
    let rejected = rejected_dir(state_dir);
    if let Err(err) = std::fs::create_dir_all(&rejected) {
        tracing::warn!(path = %rejected.display(), error = %err, "创建 rejected 目录失败");
        return false;
    }
    let target = rejected.join(
        path.file_name()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| std::ffi::OsString::from("invalid.json")),
    );
    match std::fs::rename(path, &target) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "移动 spool 文件到 rejected 失败");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdm_core::DownloadRequest;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "rdm-gui-spool-{tag}-{}-{}",
            std::process::id(),
            crate::model::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_item(url: &str) -> EnqueueItem {
        EnqueueItem {
            download: DownloadRequest::new(url),
            source: "firefox".to_string(),
            source_page: Some("https://example.com/".to_string()),
        }
    }

    #[test]
    fn missing_queue_dir_is_noop() {
        let dir = temp_dir("missing");
        let drain = drain_spool(&dir);
        assert!(drain.items.is_empty());
        assert_eq!(drain.rejected, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn drains_valid_and_rejects_invalid() {
        let dir = temp_dir("drain");
        let queue = queue_dir(&dir);
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::write(
            queue.join("b.json"),
            serde_json::to_vec(&sample_item("https://example.com/b.bin")).unwrap(),
        )
        .unwrap();
        std::fs::write(
            queue.join("a.json"),
            serde_json::to_vec(&sample_item("https://example.com/a.bin")).unwrap(),
        )
        .unwrap();
        std::fs::write(queue.join("bad.json"), b"{oops").unwrap();
        std::fs::write(queue.join("ignored.txt"), b"not json").unwrap();

        let drain = drain_spool(&dir);
        assert_eq!(drain.items.len(), 2);
        assert_eq!(drain.rejected, 1);
        assert_eq!(drain.skipped, 0);
        // 按文件名排序：a.json 先于 b.json
        assert_eq!(drain.items[0].download.url, "https://example.com/a.bin");
        assert_eq!(drain.items[1].download.url, "https://example.com/b.bin");
        assert!(!queue.join("a.json").exists());
        assert!(!queue.join("b.json").exists());
        assert!(rejected_dir(&dir).join("bad.json").exists());
        assert!(queue.join("ignored.txt").exists(), "非 json 文件不处理");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spool_item_json_is_bare_enqueue_item() {
        // 与 firefox-dev / 宿主约定：文件内容就是 EnqueueItem 裸 JSON。
        let item = sample_item("https://example.com/raw.bin");
        let json = serde_json::to_string(&item).unwrap();
        let parsed: EnqueueItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.download.url, "https://example.com/raw.bin");
        assert_eq!(parsed.source, "firefox");
    }
}
