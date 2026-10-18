//! GUI 侧的纯数据模型：下载行状态、过滤器、设置、格式化、持久化。
//!
//! 这里刻意不引入 `gpui`，所有逻辑都可以用普通单元测试覆盖
//! （`cargo test -p rdm-gui`）。视图层（`app.rs`）只负责把这里的数据画出来。
//!
//! 设计要点：
//! - `DownloadItem` 是 GUI 的投影，真正的断点状态由 `rdm-core` 的 `.part.json` 持有；
//!   因此 GUI 持久化允许每 1s 落一次，丢一点展示数据不会影响正确性。
//! - `DownloadError::Paused / Cancelled` 是控制流，不是失败；见 `apply_result`。
//! - 过滤器「失败」包含 `Failed + Cancelled`（取消的任务也需要被看见/清理）。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rdm_core::{DownloadError, DownloadEvent, DownloadRequest, DownloadSummary};
use serde::{Deserialize, Serialize};

/// 速度曲线采样点数（最近 60 个 250ms 采样 ≈ 15 秒）。
pub const SPEED_HISTORY_LEN: usize = 60;
/// 持久化文件版本。
pub const STATE_FILE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// 时间
// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 状态 / 过滤器
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl ItemState {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Downloading => "下载中",
            Self::Paused => "已暂停",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Downloading)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// IPC `status` 使用的线上值（与 docs/extension-protocol.md 对齐）。
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Downloading,
    Completed,
    Paused,
    Failed,
}

impl Filter {
    pub const ALL: [Filter; 5] = [
        Filter::All,
        Filter::Downloading,
        Filter::Completed,
        Filter::Paused,
        Filter::Failed,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Downloading => "下载中",
            Self::Completed => "已完成",
            Self::Paused => "已暂停",
            Self::Failed => "失败",
        }
    }

    pub fn matches(self, state: ItemState) -> bool {
        match self {
            Self::All => true,
            Self::Downloading => state == ItemState::Downloading,
            Self::Completed => state == ItemState::Completed,
            Self::Paused => state == ItemState::Paused,
            Self::Failed => matches!(state, ItemState::Failed | ItemState::Cancelled),
        }
    }
}

// ---------------------------------------------------------------------------
// 主题 / 设置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

/// `settings.json` 的内容。字段都带 `#[serde(default)]`，旧文件缺字段时用默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub default_connections: usize,
    pub default_dir: Option<PathBuf>,
    pub speed_limit_bps: Option<u64>,
    /// 完成后是否记录/展示 SHA-256（核心契约没有独立的「计算」开关，
    /// 只有 `expected_sha256` 校验字段；这里控制是否展示 `DownloadSummary.sha256`）。
    pub verify_sha256: bool,
    pub theme: Theme,
    pub auto_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_connections: rdm_core::DEFAULT_CONNECTIONS,
            default_dir: default_download_dir(),
            speed_limit_bps: None,
            verify_sha256: false,
            theme: Theme::Dark,
            auto_start: true,
        }
    }
}

impl Settings {
    /// 收敛非法值（手工改过 JSON 也不至于 panic）。
    pub fn normalized(&self) -> Self {
        let mut out = self.clone();
        out.default_connections = out
            .default_connections
            .clamp(1, rdm_core::MAX_CONNECTIONS);
        out.speed_limit_bps = out.speed_limit_bps.filter(|v| *v > 0);
        out
    }

    /// 全局限速的「MB/s」展示文本（空 = 不限速）。
    pub fn speed_limit_mb_text(&self) -> String {
        match self.speed_limit_bps {
            Some(bps) => trim_float(bps as f64 / (1024.0 * 1024.0)),
            None => String::new(),
        }
    }

    /// 构造新下载的 `DownloadRequest`。`expected_sha256` 留空：见 `verify_sha256` 注释。
    pub fn build_request(&self, url: impl Into<String>) -> DownloadRequest {
        let mut req = DownloadRequest::new(url);
        req.connections = self
            .default_connections
            .clamp(1, rdm_core::MAX_CONNECTIONS);
        req.output_dir = self.default_dir.clone();
        req.speed_limit_bps = self.speed_limit_bps.filter(|v| *v > 0);
        req
    }
}

/// 解析「MB/s」文本框；空或 0 表示不限速。
pub fn parse_speed_limit_mb(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let mb: f64 = text
        .parse()
        .map_err(|_| "全局限速必须是数字（MB/s）".to_string())?;
    if !mb.is_finite() || mb < 0.0 {
        return Err("全局限速不能为负数".to_string());
    }
    if mb == 0.0 {
        return Ok(None);
    }
    let bps = (mb * 1024.0 * 1024.0).round();
    if bps >= u64::MAX as f64 {
        return Err("全局限速过大".to_string());
    }
    Ok(Some(bps as u64))
}

/// 校验并收敛连接数输入。
pub fn parse_connections(text: &str) -> Result<usize, String> {
    let text = text.trim();
    let value: usize = text
        .parse()
        .map_err(|_| "连接数必须是整数".to_string())?;
    if !(1..=rdm_core::MAX_CONNECTIONS).contains(&value) {
        return Err(format!("连接数需在 1..={} 之间", rdm_core::MAX_CONNECTIONS));
    }
    Ok(value)
}

fn trim_float(value: f64) -> String {
    let mut s = format!("{value:.2}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

// ---------------------------------------------------------------------------
// URL / 文件名
// ---------------------------------------------------------------------------

/// 只接受 http(s)，拒绝 `file://` 与空白字符。
pub fn validate_url(input: &str) -> Result<String, String> {
    let url = input.trim();
    if url.is_empty() {
        return Err("请输入下载 URL".to_string());
    }
    if url.chars().any(char::is_whitespace) {
        return Err("URL 不能包含空白字符".to_string());
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("仅支持 http(s) URL".to_string());
    }
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let rest = &url[scheme_end..];
    let host_end = rest
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() || host.starts_with(':') {
        return Err("URL 缺少主机名".to_string());
    }
    Ok(url.to_string())
}

/// 从 URL 推导初始文件名（引擎 `Metadata` 事件会覆盖为服务器给的名字）。
pub fn derive_file_name(url: &str) -> String {
    let without_fragment = url.split(['?', '#']).next().unwrap_or(url);
    let after_scheme = match without_fragment.find("://") {
        Some(index) => &without_fragment[index + 3..],
        None => without_fragment,
    };
    // 去掉主机名部分，只保留 path（没有 path 时视为根路径）。
    let path = match after_scheme.find('/') {
        Some(index) => &after_scheme[index + 1..],
        None => "",
    };
    let last = path.rsplit('/').find(|seg| !seg.is_empty()).unwrap_or("");
    if last.is_empty() {
        "download.bin".to_string()
    } else {
        last.to_string()
    }
}

// ---------------------------------------------------------------------------
// 格式化
// ---------------------------------------------------------------------------

const BYTE_UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// `4.2 MB` / `512 KB` / `0 B`。
pub fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", BYTE_UNITS[unit])
    } else {
        format!("{value:.0} {}", BYTE_UNITS[unit])
    }
}

/// `4.2 MB/s`；0 显示 `0 B/s`。
pub fn format_speed(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0 B/s".to_string();
    }
    format!("{}/s", format_bytes(bps.round() as u64))
}

/// `00:32` / `01:02:03`；未知返回 `--`。
pub fn format_eta(secs: Option<f64>) -> String {
    let Some(secs) = secs else {
        return "--".to_string();
    };
    if !secs.is_finite() || secs < 0.0 {
        return "--".to_string();
    }
    let total = secs.ceil() as u64;
    if total >= 3600 {
        format!(
            "{:02}:{:02}:{:02}",
            total / 3600,
            (total % 3600) / 60,
            total % 60
        )
    } else {
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}

/// 0.68 -> `68%`；未知返回 `--`。
pub fn format_percent(fraction: Option<f64>) -> String {
    match fraction {
        Some(f) if f.is_finite() => format!("{:.0}%", f.clamp(0.0, 1.0) * 100.0),
        _ => "--".to_string(),
    }
}

pub fn format_limit(settings: &Settings) -> String {
    match settings.speed_limit_bps {
        Some(bps) => format_speed(bps as f64),
        None => "不限速".to_string(),
    }
}

// ---------------------------------------------------------------------------
// 下载行
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub id: String,
    pub request: DownloadRequest,
    pub file_name: String,
    pub url: String,
    pub state: ItemState,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub speed_bps: f64,
    pub eta_secs: Option<f64>,
    pub active_workers: usize,
    pub path: Option<PathBuf>,
    pub error: Option<String>,
    /// 运行中的瞬时状态（探测/重试/降级），完成后清空。
    pub note: Option<String>,
    pub source: String,
    pub created_ms: u64,
    pub speed_history: VecDeque<f64>,
    pub range_supported: Option<bool>,
    pub sha256: Option<String>,
    /// 当前引擎任务的代号；旧任务迟到的事件按此丢弃。
    pub run_id: u64,
}

impl DownloadItem {
    pub fn new(id: impl Into<String>, request: DownloadRequest, source: impl Into<String>) -> Self {
        let file_name = request
            .file_name
            .clone()
            .unwrap_or_else(|| derive_file_name(&request.url));
        Self {
            id: id.into(),
            url: request.url.clone(),
            request,
            file_name,
            state: ItemState::Paused,
            total: None,
            downloaded: 0,
            speed_bps: 0.0,
            eta_secs: None,
            active_workers: 0,
            path: None,
            error: None,
            note: None,
            source: source.into(),
            created_ms: now_ms(),
            speed_history: VecDeque::with_capacity(SPEED_HISTORY_LEN),
            range_supported: None,
            sha256: None,
            run_id: 0,
        }
    }

    pub fn progress_fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.downloaded as f64 / total as f64).clamp(0.0, 1.0)),
            _ => None,
        }
    }

    pub fn size_text(&self) -> String {
        match self.total {
            Some(total) => format_bytes(total),
            None if self.downloaded > 0 => format!("{} / ?", format_bytes(self.downloaded)),
            None => "未知大小".to_string(),
        }
    }

    pub fn percent_text(&self) -> String {
        format_percent(self.progress_fraction())
    }

    pub fn speed_text(&self) -> String {
        format_speed(self.speed_bps)
    }

    pub fn eta_text(&self) -> String {
        format_eta(self.eta_secs)
    }

    /// 连接数展示：已完成/暂停时显示请求配置；下载中显示引擎报告的活动 worker。
    pub fn connections_text(&self) -> String {
        if self.range_supported == Some(false) {
            return "单连接".to_string();
        }
        let n = if self.state.is_active() && self.active_workers > 0 {
            self.active_workers
        } else {
            self.request.connections
        };
        format!("{n} 连接")
    }

    pub fn push_speed_sample(&mut self, value: f64) {
        self.speed_history
            .push_back(if value.is_finite() { value.max(0.0) } else { 0.0 });
        while self.speed_history.len() > SPEED_HISTORY_LEN {
            self.speed_history.pop_front();
        }
    }

    pub fn clear_speed(&mut self) {
        self.speed_bps = 0.0;
        self.eta_secs = None;
        self.active_workers = 0;
    }

    /// 应用一条引擎事件（仅当 `run_id` 匹配时由调用方调用）。
    pub fn apply_event(&mut self, event: DownloadEvent) {
        match event {
            DownloadEvent::Probing => {
                self.note = Some("正在探测服务器…".to_string());
            }
            DownloadEvent::Metadata(meta) => {
                self.file_name = meta.file_name;
                self.total = meta.total_bytes;
                self.range_supported = Some(meta.supports_ranges);
                if !meta.supports_ranges {
                    self.note = Some("服务器不支持 Range，将单连接下载".to_string());
                }
            }
            DownloadEvent::Started {
                connections,
                chunk_size: _,
                total_chunks,
                resumed_chunks,
            } => {
                self.active_workers = connections;
                if resumed_chunks > 0 {
                    self.note = Some(format!(
                        "断点续传：已完成 {resumed_chunks}/{total_chunks} 块"
                    ));
                } else {
                    self.note = None;
                }
            }
            DownloadEvent::Progress {
                downloaded,
                total,
                speed_bps,
                eta_secs,
                active_workers,
            } => {
                if self.state == ItemState::Downloading {
                    self.downloaded = downloaded;
                    if total.is_some() {
                        self.total = total;
                    }
                    self.speed_bps = speed_bps;
                    self.eta_secs = eta_secs;
                    self.active_workers = active_workers;
                    self.push_speed_sample(speed_bps);
                    self.note = None;
                } else {
                    // 暂停/取消过程中仍可能收到最后一帧进度：只更新字节数。
                    self.downloaded = self.downloaded.max(downloaded);
                }
            }
            DownloadEvent::Retrying {
                chunk: _,
                attempt,
                delay_ms,
                reason,
            } => {
                self.note = Some(format!(
                    "第 {attempt} 次重试（{}s）：{reason}",
                    (delay_ms / 1000).max(1)
                ));
            }
            DownloadEvent::Completed { .. } => {
                // 最终结果由 `apply_result` 收敛，这里只清状态提示。
                self.note = None;
            }
            DownloadEvent::Paused { downloaded } => {
                if self.state != ItemState::Cancelled {
                    self.state = ItemState::Paused;
                    self.downloaded = self.downloaded.max(downloaded);
                    self.clear_speed();
                    self.note = None;
                }
            }
            DownloadEvent::Failed { error } => {
                if self.state != ItemState::Cancelled {
                    self.state = ItemState::Failed;
                    self.error = Some(error);
                    self.clear_speed();
                    self.note = None;
                }
            }
        }
    }

    /// 收敛引擎任务的最终结果。`Paused / Cancelled` 是控制流，分别映射到
    /// `Paused / Cancelled` 状态；其余错误进入 `Failed`。
    pub fn apply_result(&mut self, result: Result<DownloadSummary, DownloadError>) {
        match result {
            Ok(summary) => {
                self.state = ItemState::Completed;
                self.total = Some(summary.total_bytes);
                self.downloaded = summary.total_bytes;
                self.path = Some(summary.path);
                self.error = None;
                self.note = None;
                self.range_supported = Some(summary.range_supported);
                self.sha256 = summary.sha256;
                self.clear_speed();
            }
            Err(DownloadError::Paused { downloaded }) => {
                if self.state != ItemState::Cancelled {
                    self.state = ItemState::Paused;
                    self.downloaded = self.downloaded.max(downloaded);
                    self.note = None;
                    self.clear_speed();
                }
            }
            Err(DownloadError::Cancelled { downloaded }) => {
                self.state = ItemState::Cancelled;
                self.downloaded = self.downloaded.max(downloaded);
                self.error = Some("已取消".to_string());
                self.note = None;
                self.clear_speed();
            }
            Err(error) => {
                self.state = ItemState::Failed;
                self.error = Some(error.to_string());
                self.note = None;
                self.clear_speed();
            }
        }
    }

    /// 把当前状态投影成持久化记录。
    pub fn to_persisted(&self) -> PersistedItem {
        PersistedItem {
            id: self.id.clone(),
            file_name: self.file_name.clone(),
            url: self.url.clone(),
            request: self.request.clone(),
            state: self.state,
            total: self.total,
            downloaded: self.downloaded,
            path: self.path.clone(),
            error: self.error.clone(),
            source: self.source.clone(),
            created_ms: self.created_ms,
            sha256: self.sha256.clone(),
            range_supported: self.range_supported,
        }
    }

    pub fn from_persisted(saved: PersistedItem) -> Self {
        let file_name = if saved.file_name.is_empty() {
            saved
                .request
                .file_name
                .clone()
                .unwrap_or_else(|| derive_file_name(&saved.request.url))
        } else {
            saved.file_name
        };
        let url = if saved.url.is_empty() {
            saved.request.url.clone()
        } else {
            saved.url
        };
        // 架构约定：重启后进行中 → 暂停。
        let state = if saved.state == ItemState::Downloading {
            ItemState::Paused
        } else {
            saved.state
        };
        let mut item = Self {
            id: saved.id,
            request: saved.request,
            file_name,
            url,
            state,
            total: saved.total,
            downloaded: saved.downloaded,
            speed_bps: 0.0,
            eta_secs: None,
            active_workers: 0,
            path: saved.path,
            error: saved.error,
            note: None,
            source: saved.source,
            created_ms: saved.created_ms,
            speed_history: VecDeque::with_capacity(SPEED_HISTORY_LEN),
            range_supported: saved.range_supported,
            sha256: saved.sha256,
            run_id: 0,
        };
        if item.state == ItemState::Paused && item.error.is_none() {
            item.note = Some("启动恢复：已暂停，可继续".to_string());
        }
        item
    }
}

// ---------------------------------------------------------------------------
// 聚合统计
// ---------------------------------------------------------------------------

pub fn filter_count(items: &[DownloadItem], filter: Filter) -> usize {
    items
        .iter()
        .filter(|item| filter.matches(item.state))
        .count()
}

pub fn active_count(items: &[DownloadItem]) -> usize {
    items.iter().filter(|item| item.state.is_active()).count()
}

pub fn global_speed_bps(items: &[DownloadItem]) -> f64 {
    items
        .iter()
        .filter(|item| item.state.is_active())
        .map(|item| item.speed_bps)
        .filter(|v| v.is_finite() && *v > 0.0)
        .sum()
}

/// 过滤后的索引列表（给 `uniform_list` 用）。
pub fn filtered_indices(items: &[DownloadItem], filter: Filter) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| filter.matches(item.state))
        .map(|(ix, _)| ix)
        .collect()
}

pub fn next_download_id(seq: u64) -> String {
    format!("dl_{seq:06}")
}

// ---------------------------------------------------------------------------
// 持久化
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedItem {
    pub id: String,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub url: String,
    pub request: DownloadRequest,
    pub state: ItemState,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub range_supported: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default = "state_version")]
    pub version: u32,
    #[serde(default)]
    pub items: Vec<PersistedItem>,
    #[serde(default)]
    pub next_seq: u64,
}

fn state_version() -> u32 {
    STATE_FILE_VERSION
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_FILE_VERSION,
            items: Vec::new(),
            next_seq: 1,
        }
    }
}

#[derive(Debug, Default)]
pub struct LoadOutcome {
    pub items: Vec<DownloadItem>,
    pub skipped: usize,
    /// 进行中 → 暂停 的条数。
    pub recovered: usize,
    pub next_seq: u64,
    pub error: Option<String>,
}

pub fn downloads_path(state_dir: &Path) -> PathBuf {
    state_dir.join("downloads.json")
}

pub fn settings_path(state_dir: &Path) -> PathBuf {
    state_dir.join("settings.json")
}

pub fn load_downloads(state_dir: &Path) -> LoadOutcome {
    let path = downloads_path(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return LoadOutcome {
                next_seq: 1,
                ..Default::default()
            };
        }
        Err(err) => {
            return LoadOutcome {
                next_seq: 1,
                error: Some(format!("读取 {} 失败: {err}", path.display())),
                ..Default::default()
            };
        }
    };
    let state: PersistedState = match serde_json::from_str(&text) {
        Ok(state) => state,
        Err(err) => {
            return LoadOutcome {
                next_seq: 1,
                error: Some(format!("解析 {} 失败: {err}", path.display())),
                ..Default::default()
            };
        }
    };

    let mut items = Vec::new();
    let mut skipped = 0usize;
    let mut recovered = 0usize;
    for saved in state.items {
        if validate_url(&saved.request.url).is_err() {
            skipped += 1;
            continue;
        }
        if saved.state == ItemState::Downloading {
            recovered += 1;
        }
        items.push(DownloadItem::from_persisted(saved));
    }
    let max_seq = items
        .iter()
        .filter_map(|item| item.id.strip_prefix("dl_"))
        .filter_map(|digits| digits.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    let next_seq = state.next_seq.max(max_seq + 1).max(1);
    LoadOutcome {
        items,
        skipped,
        recovered,
        next_seq,
        error: None,
    }
}

pub fn save_downloads(state_dir: &Path, items: &[DownloadItem], next_seq: u64) -> std::io::Result<()> {
    let state = PersistedState {
        version: STATE_FILE_VERSION,
        items: items.iter().map(DownloadItem::to_persisted).collect(),
        next_seq,
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    write_atomic(&downloads_path(state_dir), &bytes)
}

pub fn load_settings(state_dir: &Path) -> Settings {
    let path = settings_path(state_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Settings>(&text) {
            Ok(settings) => settings.normalized(),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "settings.json 解析失败，使用默认设置");
                Settings::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "settings.json 读取失败，使用默认设置");
            Settings::default()
        }
    }
}

pub fn save_settings(state_dir: &Path, settings: &Settings) -> std::io::Result<()> {
    let settings = settings.normalized();
    let bytes = serde_json::to_vec_pretty(&settings)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    write_atomic(&settings_path(state_dir), &bytes)
}

/// 原子写：同目录 `.tmp` + rename。Windows 的 `rename` 会替换已存在的目标文件。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// `%USERPROFILE%\Downloads`（存在于本机时）。
pub fn default_download_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            let dir = PathBuf::from(profile).join("Downloads");
            if dir.is_dir() {
                return Some(dir);
            }
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let dir = PathBuf::from(home).join("Downloads");
            if dir.is_dir() {
                return Some(dir);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "rdm-gui-test-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_request() -> DownloadRequest {
        DownloadRequest::new("https://example.com/files/archive.zip")
    }

    fn sample_summary() -> DownloadSummary {
        DownloadSummary {
            path: PathBuf::from("C:/downloads/archive.zip"),
            total_bytes: 1_000,
            elapsed_ms: 500,
            avg_speed_bps: 2_000.0,
            connections: 8,
            sha256: Some("deadbeef".to_string()),
            resumed_chunks: 0,
            range_supported: true,
            http_version: None,
        }
    }

    // --- 格式化 ---

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(11 * 1024), "11 KB");
        assert_eq!(format_bytes(4_500_000), "4.3 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn formats_speed_and_eta() {
        assert_eq!(format_speed(0.0), "0 B/s");
        assert_eq!(format_speed(-5.0), "0 B/s");
        assert_eq!(format_speed(4.2 * 1024.0 * 1024.0), "4.2 MB/s");
        assert_eq!(format_eta(None), "--");
        assert_eq!(format_eta(Some(0.0)), "00:00");
        assert_eq!(format_eta(Some(32.2)), "00:33");
        assert_eq!(format_eta(Some(3725.0)), "01:02:05");
        assert_eq!(format_percent(None), "--");
        assert_eq!(format_percent(Some(0.684)), "68%");
        assert_eq!(format_percent(Some(2.0)), "100%");
    }

    // --- URL / 文件名 ---

    #[test]
    fn validates_urls() {
        assert!(validate_url("https://example.com/a.bin").is_ok());
        assert!(validate_url("  http://127.0.0.1:8080/x  ").is_ok());
        assert!(validate_url("").is_err());
        assert!(validate_url("ftp://example.com/a").is_err());
        assert!(validate_url("file:///C:/secret").is_err());
        assert!(validate_url("http://").is_err());
        assert!(validate_url("https://exa mple.com").is_err());
        // 返回值 trim 过
        assert_eq!(
            validate_url(" https://example.com/a ").unwrap(),
            "https://example.com/a"
        );
    }

    #[test]
    fn derives_file_names() {
        assert_eq!(derive_file_name("https://example.com/a/b.zip"), "b.zip");
        assert_eq!(
            derive_file_name("https://example.com/a/b.zip?token=1#frag"),
            "b.zip"
        );
        assert_eq!(derive_file_name("https://example.com/"), "download.bin");
        assert_eq!(derive_file_name("https://example.com"), "download.bin");
    }

    // --- 过滤器 / 聚合 ---

    fn item_in_state(id: u64, state: ItemState) -> DownloadItem {
        let mut item = DownloadItem::new(next_download_id(id), sample_request(), "test");
        item.state = state;
        item
    }

    #[test]
    fn filters_and_counts() {
        let items = vec![
            item_in_state(1, ItemState::Downloading),
            item_in_state(2, ItemState::Downloading),
            item_in_state(3, ItemState::Completed),
            item_in_state(4, ItemState::Paused),
            item_in_state(5, ItemState::Failed),
            item_in_state(6, ItemState::Cancelled),
        ];
        assert_eq!(filter_count(&items, Filter::All), 6);
        assert_eq!(filter_count(&items, Filter::Downloading), 2);
        assert_eq!(filter_count(&items, Filter::Completed), 1);
        assert_eq!(filter_count(&items, Filter::Paused), 1);
        // 「失败」包含取消
        assert_eq!(filter_count(&items, Filter::Failed), 2);
        assert_eq!(filtered_indices(&items, Filter::Failed), vec![4, 5]);
        assert_eq!(active_count(&items), 2);
    }

    #[test]
    fn global_speed_sums_only_active() {
        let mut a = item_in_state(1, ItemState::Downloading);
        a.speed_bps = 1_000.0;
        let mut b = item_in_state(2, ItemState::Downloading);
        b.speed_bps = 2_500.0;
        let mut c = item_in_state(3, ItemState::Paused);
        c.speed_bps = 9_999.0;
        assert_eq!(global_speed_bps(&[a, b, c]), 3_500.0);
    }

    #[test]
    fn download_id_is_padded() {
        assert_eq!(next_download_id(1), "dl_000001");
        assert_eq!(next_download_id(42), "dl_000042");
    }

    // --- 状态机 ---

    #[test]
    fn progress_updates_and_history_is_capped() {
        let mut item = item_in_state(1, ItemState::Downloading);
        for i in 0..(SPEED_HISTORY_LEN + 10) {
            item.apply_event(DownloadEvent::Progress {
                downloaded: i as u64 * 100,
                total: Some(100_000),
                speed_bps: i as f64,
                eta_secs: Some(10.0),
                active_workers: 8,
            });
        }
        assert_eq!(item.speed_history.len(), SPEED_HISTORY_LEN);
        assert_eq!(item.downloaded, (SPEED_HISTORY_LEN as u64 + 9) * 100);
        assert_eq!(item.speed_bps, (SPEED_HISTORY_LEN + 9) as f64);
        assert_eq!(item.active_workers, 8);
    }

    #[test]
    fn paused_is_control_flow_not_failure() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.downloaded = 10;
        item.apply_result(Err(DownloadError::Paused { downloaded: 500 }));
        assert_eq!(item.state, ItemState::Paused);
        assert_eq!(item.downloaded, 500);
        assert!(item.error.is_none());
    }

    #[test]
    fn cancelled_and_failed_are_distinct() {
        let mut cancelled = item_in_state(1, ItemState::Downloading);
        cancelled.apply_result(Err(DownloadError::Cancelled { downloaded: 7 }));
        assert_eq!(cancelled.state, ItemState::Cancelled);
        assert_eq!(cancelled.error.as_deref(), Some("已取消"));

        let mut failed = item_in_state(2, ItemState::Downloading);
        failed.apply_result(Err(DownloadError::Http("boom".to_string())));
        assert_eq!(failed.state, ItemState::Failed);
        assert!(failed.error.unwrap().contains("boom"));
    }

    #[test]
    fn completed_result_sets_path_and_hash() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_result(Ok(sample_summary()));
        assert_eq!(item.state, ItemState::Completed);
        assert_eq!(item.total, Some(1_000));
        assert_eq!(item.downloaded, 1_000);
        assert_eq!(item.path.as_deref(), Some(Path::new("C:/downloads/archive.zip")));
        assert_eq!(item.sha256.as_deref(), Some("deadbeef"));
        assert_eq!(item.progress_fraction(), Some(1.0));
    }

    #[test]
    fn cancel_ignores_late_progress_events() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_result(Err(DownloadError::Cancelled { downloaded: 100 }));
        item.apply_event(DownloadEvent::Progress {
            downloaded: 900,
            total: Some(1_000),
            speed_bps: 10.0,
            eta_secs: Some(1.0),
            active_workers: 8,
        });
        assert_eq!(item.state, ItemState::Cancelled);
        assert_eq!(item.speed_bps, 0.0);
        assert_eq!(item.downloaded, 900, "字节数仍允许回填");
    }

    #[test]
    fn metadata_and_retry_notes() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_event(DownloadEvent::Metadata(rdm_core::RemoteMeta {
            final_url: "https://cdn.example.com/x.bin".to_string(),
            file_name: "server-name.bin".to_string(),
            total_bytes: Some(2_048),
            supports_ranges: false,
            content_type: Some("application/octet-stream".to_string()),
            etag: None,
            last_modified: None,
            http_version: None,
        }));
        assert_eq!(item.file_name, "server-name.bin");
        assert_eq!(item.total, Some(2_048));
        assert_eq!(item.range_supported, Some(false));
        item.apply_event(DownloadEvent::Retrying {
            chunk: 3,
            attempt: 2,
            delay_ms: 1_500,
            reason: "503".to_string(),
        });
        assert!(item.note.as_ref().unwrap().contains("第 2 次重试"));
        assert_eq!(item.connections_text(), "单连接");
    }

    // --- 设置 ---

    #[test]
    fn speed_limit_roundtrip() {
        assert_eq!(parse_speed_limit_mb("").unwrap(), None);
        assert_eq!(parse_speed_limit_mb("0").unwrap(), None);
        assert_eq!(parse_speed_limit_mb("2").unwrap(), Some(2 * 1024 * 1024));
        assert_eq!(
            parse_speed_limit_mb("1.5").unwrap(),
            Some(1_572_864)
        );
        assert!(parse_speed_limit_mb("abc").is_err());
        assert!(parse_speed_limit_mb("-1").is_err());
        let mut settings = Settings::default();
        settings.speed_limit_bps = Some(2 * 1024 * 1024);
        assert_eq!(settings.speed_limit_mb_text(), "2");
        settings.speed_limit_bps = Some(1_572_864);
        assert_eq!(settings.speed_limit_mb_text(), "1.5");
        assert_eq!(format_limit(&settings), "1.5 MB/s");
        settings.speed_limit_bps = None;
        assert_eq!(format_limit(&settings), "不限速");
    }

    #[test]
    fn connections_parsing_and_normalization() {
        assert_eq!(parse_connections("8").unwrap(), 8);
        assert!(parse_connections("0").is_err());
        assert!(parse_connections("33").is_err());
        let mut settings = Settings::default();
        settings.default_connections = 999;
        let normalized = settings.normalized();
        assert_eq!(normalized.default_connections, rdm_core::MAX_CONNECTIONS);
        let request = normalized.build_request("https://example.com/a.bin");
        assert_eq!(request.connections, rdm_core::MAX_CONNECTIONS);
    }

    #[test]
    fn theme_toggle() {
        assert_eq!(Theme::Dark.toggled(), Theme::Light);
        assert_eq!(Theme::Light.label_zh(), "浅色");
    }

    // --- 持久化 ---

    #[test]
    fn downloads_roundtrip_recovers_active_as_paused() {
        let dir = temp_dir("downloads");
        let mut a = item_in_state(1, ItemState::Downloading);
        a.downloaded = 123;
        a.total = Some(1_000);
        a.apply_event(DownloadEvent::Progress {
            downloaded: 123,
            total: Some(1_000),
            speed_bps: 50.0,
            eta_secs: Some(3.0),
            active_workers: 4,
        });
        let b = item_in_state(2, ItemState::Completed);
        save_downloads(&dir, &[a, b], 3).unwrap();

        let loaded = load_downloads(&dir);
        assert!(loaded.error.is_none());
        assert_eq!(loaded.items.len(), 2);
        assert_eq!(loaded.recovered, 1);
        assert_eq!(loaded.skipped, 0);
        assert_eq!(loaded.next_seq, 3);
        assert_eq!(loaded.items[0].state, ItemState::Paused);
        assert_eq!(loaded.items[0].downloaded, 123);
        assert_eq!(loaded.items[1].state, ItemState::Completed);
        assert_eq!(loaded.items[0].speed_bps, 0.0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_and_corrupt_downloads_are_tolerated() {
        let dir = temp_dir("corrupt");
        assert!(load_downloads(&dir).items.is_empty());
        std::fs::write(downloads_path(&dir), b"{not json").unwrap();
        let loaded = load_downloads(&dir);
        assert!(loaded.items.is_empty());
        assert!(loaded.error.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_urls_are_skipped_on_load() {
        let dir = temp_dir("skip");
        let mut state = PersistedState::default();
        let mut saved = item_in_state(1, ItemState::Paused).to_persisted();
        saved.request.url = "file:///etc/passwd".to_string();
        state.items.push(saved);
        state.items.push(item_in_state(2, ItemState::Paused).to_persisted());
        std::fs::write(
            downloads_path(&dir),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
        let loaded = load_downloads(&dir);
        assert_eq!(loaded.skipped, 1);
        assert_eq!(loaded.items.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_roundtrip() {
        let dir = temp_dir("settings");
        let settings = Settings {
            default_connections: 4,
            default_dir: Some(dir.join("dl")),
            speed_limit_bps: Some(1_048_576),
            verify_sha256: true,
            theme: Theme::Light,
            auto_start: false,
        };
        save_settings(&dir, &settings).unwrap();
        let loaded = load_settings(&dir);
        assert_eq!(loaded.default_connections, 4);
        assert_eq!(loaded.default_dir, settings.default_dir);
        assert_eq!(loaded.speed_limit_bps, Some(1_048_576));
        assert!(loaded.verify_sha256);
        assert_eq!(loaded.theme, Theme::Light);
        assert!(!loaded.auto_start);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let dir = temp_dir("atomic");
        let path = dir.join("x.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn persisted_state_serializes_next_seq() {
        let state = PersistedState::default();
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"next_seq\":1"));
        assert!(json.contains("\"version\":1"));
    }
}
