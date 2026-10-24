//! 把扫描结果 + 下载记录 + AI 账本聚合成 [`MonitorSnapshot`]。
//!
//! 纯函数（无 IO），全部口径在一处定义，方便单测和「数字对得上」的验收。

use std::path::PathBuf;

use super::scan::ScanResult;
use super::*;
use crate::model::DownloadItem;

/// 下载记录里已知的文件路径（含 `.part` 变体的基准路径）。
///
/// 生成规则与 `views/app.rs::delete_targets` 一致：优先 `item.path`，其次
/// `save_dir/file_name`，最后 `default_dir/file_name`；`.part`/`.part.json`
/// 由扫描侧按后缀匹配，不需要在这里展开。
pub fn tracked_paths(
    items: &[DownloadItem],
    default_dir: Option<&std::path::Path>,
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for item in items {
        if let Some(path) = &item.path {
            paths.push(path.clone());
        }
        if let Some(dir) = item.save_dir.as_ref().or(item.request.output_dir.as_ref()) {
            paths.push(dir.join(&item.file_name));
        }
        if let Some(dir) = default_dir {
            paths.push(dir.join(&item.file_name));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// 聚合一次完整快照。
pub fn build_snapshot(
    scan: ScanResult,
    items: &[DownloadItem],
    ledger: &AiUsageLedger,
    ai_configured: bool,
) -> MonitorSnapshot {
    MonitorSnapshot {
        root: scan.root.clone(),
        scanned_at_ms: scan.scanned_at_ms,
        scan_ms: scan.scan_ms,
        library: scan.library.clone(),
        disk: scan.disk,
        ai: build_ai_summary(&scan.library, items, ledger, ai_configured),
        transfer: build_transfer_summary(items),
        hygiene: HygieneUsage {
            part_bytes: scan.hygiene.part_bytes,
            part_files: scan.hygiene.part_files,
            untracked_bytes: scan.hygiene.untracked_bytes,
            untracked_files: scan.hygiene.untracked_files,
            state_dir_bytes: scan.hygiene.state_dir_bytes,
            ai_cache_bytes: scan.hygiene.ai_cache_bytes,
            ai_cache_entries: scan.hygiene.ai_cache_entries,
            logs_bytes: scan.hygiene.logs_bytes,
        },
        truncated: scan.truncated,
        skipped_dirs: scan.skipped_dirs,
    }
}

fn build_transfer_summary(items: &[DownloadItem]) -> TransferSummary {
    let mut summary = TransferSummary {
        total: items.len() as u64,
        ..TransferSummary::default()
    };
    for item in items {
        match item.state {
            crate::model::ItemState::Downloading => {
                summary.downloading = summary.downloading.saturating_add(1);
                summary.active_connections = summary
                    .active_connections
                    .saturating_add(item.active_workers as u64);
                if item.speed_bps.is_finite() && item.speed_bps > 0.0 {
                    summary.current_speed_bps += item.speed_bps;
                }
                if let Some(total) = item.total {
                    summary.active_remaining_bytes = summary
                        .active_remaining_bytes
                        .saturating_add(total.saturating_sub(item.downloaded));
                }
            }
            crate::model::ItemState::Paused => {
                summary.paused = summary.paused.saturating_add(1);
            }
            crate::model::ItemState::Completed => {
                summary.completed = summary.completed.saturating_add(1);
                summary.completed_bytes = summary
                    .completed_bytes
                    .saturating_add(item.total.unwrap_or(item.downloaded));
            }
            crate::model::ItemState::Failed => {
                summary.failed = summary.failed.saturating_add(1);
            }
            crate::model::ItemState::Cancelled => {
                summary.cancelled = summary.cancelled.saturating_add(1);
            }
        }
        summary.downloaded_bytes = summary.downloaded_bytes.saturating_add(item.downloaded);
    }
    if !summary.current_speed_bps.is_finite() || summary.current_speed_bps < 0.0 {
        summary.current_speed_bps = 0.0;
    }
    summary
}

fn build_ai_summary(
    library: &LibraryUsage,
    items: &[DownloadItem],
    ledger: &AiUsageLedger,
    ai_configured: bool,
) -> AiUsageSummary {
    let priced = ledger.priced_records > 0;
    let mut summary = AiUsageSummary {
        configured: ai_configured,
        lifetime: ledger.totals,
        parse: ledger.kind_usage(AiUsageKind::Parse),
        chat: ledger.kind_usage(AiUsageKind::Chat),
        trend: ledger.trend.as_slice().to_vec(),
        parse_calls: ledger.parse_calls,
        parse_cache_hits: ledger.parse_cache_hits,
        estimated_calls: ledger.estimated_calls,
        models: ledger.models_sorted().into_iter().take(8).collect(),
        cost_cny: ledger.cost_cny,
        cost_known: priced,
        cost_parse: ledger.kind_cost(AiUsageKind::Parse),
        cost_chat: ledger.kind_cost(AiUsageKind::Chat),
        cost_today: ledger.trend.cost_today(),
        cost_last_7: ledger.trend.cost_last(7),
        unpriced_calls: ledger.unpriced_calls,
        pricing_note: priced.then(|| {
            format!(
                "≈ 估算 · 价目快照 {} · USD→CNY {}",
                rdm_ai::providers::PRICE_CATALOG_DATE,
                rdm_ai::providers::USD_TO_CNY
            )
        }),
        ..AiUsageSummary::default()
    };
    // 趋势桶缺失（旧账本/首次运行）时补足 14 个空桶，UI 不处理变长。
    while summary.trend.len() < TREND_BUCKETS {
        summary.trend.insert(0, TokenUsage::ZERO);
    }

    let mut item_tokens = TokenUsage::ZERO;
    let mut item_cost = 0.0;
    let mut top: Vec<ItemTokenUsage> = Vec::new();
    for item in items {
        let Some(ai) = item.ai.as_ref() else {
            continue;
        };
        let total = ai.total();
        if ai.has_tokens() {
            summary.items_with_ai = summary.items_with_ai.saturating_add(1);
        }
        if total.total_tokens == 0 {
            continue;
        }
        item_tokens.merge(total);
        item_cost += ai.cost_cny();
        top.push(ItemTokenUsage {
            id: item.id.clone(),
            file_name: item.file_name.clone(),
            tokens: total,
            parse: ai.parse,
            model: ai.model.clone(),
            cache_hit: ai.cache_hit,
            parse_siblings: ai.parse_siblings.max(1),
            size_bytes: item.total,
            cost_cny: ai.cost_cny(),
        });
    }
    summary.item_tokens = item_tokens;
    summary.item_cost_cny = item_cost;
    top.sort_by(|a, b| {
        b.tokens
            .total_tokens
            .cmp(&a.tokens.total_tokens)
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    top.truncate(8);
    summary.top_items = top;

    // 「每 GB 下载消耗多少 token」：只用当前记录里的任务做样本，口径一致。
    if library.total_bytes > 0 && !item_tokens.is_zero() {
        let gib = library.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        if gib > 0.0 {
            summary.tokens_per_gb = Some(item_tokens.total_tokens as f64 / gib);
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DownloadItem, ItemState};
    use rdm_ai::TokenUsage;

    fn item(name: &str, state: ItemState, downloaded: u64, total: Option<u64>) -> DownloadItem {
        let mut request = rdm_core::DownloadRequest::new(format!("https://example.com/{name}"));
        request.file_name = Some(name.to_owned());
        let mut item = DownloadItem::new(name, request, "gui");
        item.file_name = name.to_owned();
        item.state = state;
        item.downloaded = downloaded;
        item.total = total;
        item
    }

    fn usage(total: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: total / 2,
            completion_tokens: total - total / 2,
            total_tokens: total,
            calls: 1,
            estimated: false,
        }
    }

    fn scan_with(bytes: u64, files: u64, untracked: u64) -> ScanResult {
        ScanResult {
            root: Some(PathBuf::from("C:/Downloads")),
            scanned_at_ms: 1,
            scan_ms: 12,
            disk: Some(DiskUsage {
                free_bytes: 100,
                total_bytes: 400,
            }),
            library: LibraryUsage {
                exists: true,
                total_files: files,
                total_bytes: bytes,
                untracked_files: untracked,
                untracked_bytes: untracked * 10,
                ..LibraryUsage::default()
            },
            hygiene: super::scan::HygieneScan {
                part_files: 2,
                part_bytes: 20,
                untracked_files: untracked,
                untracked_bytes: untracked * 10,
                state_dir_bytes: 50,
                ..super::scan::HygieneScan::default()
            },
            truncated: false,
            skipped_dirs: 0,
        }
    }

    #[test]
    fn transfer_summary_counts_states_and_bytes() {
        let mut downloading = item("a.mp4", ItemState::Downloading, 50, Some(200));
        downloading.active_workers = 4;
        downloading.speed_bps = 1024.0;
        let paused = item("b.zip", ItemState::Paused, 10, Some(100));
        let completed = item("c.pdf", ItemState::Completed, 100, Some(100));
        let failed = item("d.iso", ItemState::Failed, 0, Some(300));
        let cancelled = item("e.bin", ItemState::Cancelled, 0, None);
        let summary = build_transfer_summary(&[downloading, paused, completed, failed, cancelled]);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.downloading, 1);
        assert_eq!(summary.paused, 1);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.cancelled, 1);
        assert_eq!(summary.active_connections, 4);
        assert_eq!(summary.current_speed_bps, 1024.0);
        assert_eq!(summary.active_remaining_bytes, 150);
        assert_eq!(summary.completed_bytes, 100);
        assert_eq!(summary.downloaded_bytes, 160);
    }

    #[test]
    fn ai_summary_aggregates_item_usage_and_ledger() {
        let mut scanned = item("a.mp4", ItemState::Completed, 100, Some(100));
        scanned.ai = Some(crate::model::ItemAiUsage {
            parse: usage(1_000),
            classify: usage(100),
            parse_cost_cny: 0.02,
            classify_cost_cny: 0.004,
            model: Some("deepseek-flash".to_owned()),
            cache_hit: false,
            parse_siblings: 2,
            updated_ms: 1,
        });
        let mut empty = item("b.zip", ItemState::Paused, 0, Some(10));
        empty.ai = Some(crate::model::ItemAiUsage::default());

        let mut ledger = AiUsageLedger::default();
        ledger.normalize(1_700_000_000_000);
        ledger.record(
            AiUsageRecord {
                at_ms: 1_700_000_000_000,
                kind: AiUsageKind::Parse,
                host: Some("example.com".to_owned()),
                model: Some("deepseek-flash".to_owned()),
                cache_hit: false,
                item_ids: Vec::new(),
                session_id: None,
                cost_cny: Some(0.02),
                tokens: usage(1_000),
            },
            1_700_000_000_000,
        );
        ledger.record(
            AiUsageRecord {
                at_ms: 1_700_000_000_000,
                kind: AiUsageKind::Classify,
                host: None,
                model: Some("deepseek-flash".to_owned()),
                cache_hit: false,
                item_ids: vec!["dl_1".to_owned()],
                session_id: None,
                cost_cny: Some(0.004),
                tokens: usage(100),
            },
            1_700_000_000_000,
        );

        let summary = build_ai_summary(
            &scan_with(100, 1, 0).library,
            &[scanned, empty],
            &ledger,
            true,
        );
        assert!(summary.configured);
        assert_eq!(summary.lifetime.total_tokens, 1_100);
        assert_eq!(summary.parse.total_tokens, 1_000);
        assert_eq!(summary.parse_calls, 1);
        assert_eq!(summary.items_with_ai, 1);
        assert_eq!(summary.item_tokens.total_tokens, 1_100);
        assert_eq!(summary.top_items.len(), 1);
        assert_eq!(summary.top_items[0].parse_siblings, 2);
        assert!((summary.top_items[0].cost_cny - 0.024).abs() < 1e-9);
        assert_eq!(summary.trend.len(), TREND_BUCKETS);
        assert!(summary.cost_known);
        assert!((summary.cost_cny - 0.024).abs() < 1e-9);
        assert!((summary.cost_parse - 0.02).abs() < 1e-9);
        assert!((summary.item_cost_cny - 0.024).abs() < 1e-9);
        assert!(summary.pricing_note.is_some());
        assert_eq!(summary.unpriced_calls, 0);
        assert_eq!(
            summary.tokens_per_gb,
            Some(1_100.0 * (1024.0 * 1024.0 * 1024.0) / 100.0)
        );
    }

    #[test]
    fn snapshot_carries_hygiene_and_disk() {
        let scan = scan_with(500, 4, 3);
        let snapshot = build_snapshot(scan, &[], &AiUsageLedger::default(), false);
        assert_eq!(snapshot.library.total_bytes, 500);
        assert_eq!(snapshot.hygiene.part_bytes, 20);
        assert_eq!(snapshot.hygiene.untracked_files, 3);
        assert_eq!(snapshot.hygiene.state_dir_bytes, 50);
        assert_eq!(snapshot.disk.unwrap().free_bytes, 100);
        assert!(!snapshot.ai.configured);
        assert!(snapshot.ai.trend.iter().all(|bucket| bucket.is_zero()));
    }

    #[test]
    fn tracked_paths_cover_path_save_dir_and_default_dir() {
        let mut item = item("a.mp4", ItemState::Completed, 1, Some(1));
        item.path = Some(PathBuf::from("C:/Downloads/Video/a.mp4"));
        item.save_dir = Some(PathBuf::from("C:/Downloads/Video"));
        let paths = tracked_paths(&[item], Some(std::path::Path::new("C:/Downloads")));
        assert!(paths.contains(&PathBuf::from("C:/Downloads/Video/a.mp4")));
        assert!(paths.contains(&PathBuf::from("C:/Downloads").join("a.mp4")));
    }
}
