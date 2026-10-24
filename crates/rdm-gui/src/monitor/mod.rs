//! 资源监控的纯数据层：快照类型、AI token 账本、格式化与 UI 状态。
//!
//! 设计文档：`docs/design/13-resource-monitor.md`。
//!
//! 分工：
//! * 本模块：跨层共享的类型、`AiUsageLedger` 的读写与聚合口径、格式化文本；
//! * [`scan`]：下载目录/状态目录的递归扫描（独立线程，不碰 UI）；
//! * [`aggregate`]：扫描结果 + 下载项 + AI 账本 → [`MonitorSnapshot`]。
//!
//! 纪律：本模块不引入 `gpui`，所有逻辑都可单测；不读取网络、不读取文件内容。

pub mod aggregate;
pub mod scan;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rdm_ai::TokenUsage;

use crate::views::app::FileCategory;

// ---------------------------------------------------------------------------
// 快照类型（UI 只读这些数据）
// ---------------------------------------------------------------------------

/// 资源监控页的一次完整数据快照。
#[derive(Debug, Clone)]
pub struct MonitorSnapshot {
    /// 统计根目录（`Settings::default_dir`）；未设置时为 `None`。
    pub root: Option<PathBuf>,
    /// 扫描完成时间（Unix ms）。
    pub scanned_at_ms: u64,
    /// 扫描耗时（ms）。
    pub scan_ms: u64,
    /// 下载库占用（真实文件字节）。
    pub library: LibraryUsage,
    /// 根目录所在卷的容量；未知为 `None`。
    pub disk: Option<DiskUsage>,
    /// AI token 消耗摘要。
    pub ai: AiUsageSummary,
    /// 传输/任务活动摘要。
    pub transfer: TransferSummary,
    /// 维护相关占用（`.part`、状态目录、AI 缓存、日志）。
    pub hygiene: HygieneUsage,
    /// 扫描是否因为条目/深度/错误上限被截断。
    pub truncated: bool,
    /// 被跳过的目录数（无权限/符号链接等）。
    pub skipped_dirs: u64,
}

impl MonitorSnapshot {
    /// 空快照（视图测试/首次渲染占位用）。
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            root: None,
            scanned_at_ms: 0,
            scan_ms: 0,
            library: LibraryUsage::default(),
            disk: None,
            ai: AiUsageSummary::default(),
            transfer: TransferSummary::default(),
            hygiene: HygieneUsage::default(),
            truncated: false,
            skipped_dirs: 0,
        }
    }
}

/// 卷容量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl DiskUsage {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    pub fn used_fraction(&self) -> Option<f32> {
        (self.total_bytes > 0)
            .then(|| (self.used_bytes() as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32)
    }
}

/// 下载目录占用。
#[derive(Debug, Clone, Default)]
pub struct LibraryUsage {
    /// 根目录是否存在。
    pub exists: bool,
    pub total_files: u64,
    pub total_bytes: u64,
    /// 直接子目录数量。
    pub top_dirs: u64,
    /// 按文件类型聚合（按字节降序，空分类会被省略）。
    pub categories: Vec<CategoryUsage>,
    /// 最大的文件（最多 8 个）。
    pub largest_files: Vec<FileUsage>,
    /// 未归属文件：磁盘上存在、但不在下载记录中的普通文件。
    pub untracked_files: u64,
    pub untracked_bytes: u64,
}

/// 单个文件类型的占用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategoryUsage {
    pub category: FileCategory,
    pub bytes: u64,
    pub files: u64,
}

impl CategoryUsage {
    /// 占下载库总量比例（下载库为空时 `None`）。
    pub fn fraction_of(&self, total_bytes: u64) -> Option<f32> {
        (total_bytes > 0).then(|| (self.bytes as f64 / total_bytes as f64).clamp(0.0, 1.0) as f32)
    }
}

/// 一个可展示的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUsage {
    pub name: String,
    /// 相对根目录的父目录（根目录为空串）。
    pub relative_dir: String,
    pub bytes: u64,
    /// 修改时间（Unix ms；未知 0）。
    pub modified_ms: u64,
    pub category: FileCategory,
    /// 是否能在下载记录里找到（决定显示「未归属」）。
    pub tracked: bool,
}

/// AI 消耗摘要。
#[derive(Debug, Clone, Default)]
pub struct AiUsageSummary {
    /// AI 是否已启用并配置了 key。
    pub configured: bool,
    /// 账本里的终身累计。
    pub lifetime: TokenUsage,
    pub parse: TokenUsage,
    pub chat: TokenUsage,
    /// 近 14 个 24h 桶（旧 → 新）。
    pub trend: Vec<TokenUsage>,
    /// 解析次数 / 缓存命中次数。
    pub parse_calls: u64,
    pub parse_cache_hits: u64,
    /// 含估算值的调用次数。
    pub estimated_calls: u64,
    /// 按模型的消耗（按 token 降序，最多 8 个）。
    pub models: Vec<ModelUsage>,
    /// 带 AI 归属的下载项数量。
    pub items_with_ai: u64,
    /// 全部下载项的 AI 归属合计（解析 + 旧版分类记录；会因共享重复，仅用于「最贵任务」排序）。
    pub item_tokens: TokenUsage,
    /// 最贵的下载项（按 token 降序，最多 8 个）。
    pub top_items: Vec<ItemTokenUsage>,
    /// 每 GB 已完成下载消耗的 token（解析 + 旧版分类记录）。
    pub tokens_per_gb: Option<f64>,
    /// 终身估算花费（人民币；只累计有价目的调用）。
    pub cost_cny: f64,
    /// 是否至少有一条调用能按内置价目估算费用。
    pub cost_known: bool,
    pub cost_parse: f64,
    pub cost_chat: f64,
    /// 最近一个 24h 桶 / 最近 7 个桶的估算花费。
    pub cost_today: f64,
    pub cost_last_7: f64,
    /// 无价目（Custom/未知模型）但确实花了 token 的调用数。
    pub unpriced_calls: u64,
    /// 当前下载项归属的估算花费合计。
    pub item_cost_cny: f64,
    /// 价目说明（快照日期/汇率），UI 页脚展示。
    pub pricing_note: Option<String>,
}

impl AiUsageSummary {
    /// 账本里有解析记录（用于区分「配了 AI 但没用过」与「用过」）。
    pub fn has_activity(&self) -> bool {
        self.parse_calls > 0 || !self.lifetime.is_zero() || !self.item_tokens.is_zero()
    }

    /// 是否应该展示估算金额（有价目且确实产生过费用记录）。
    pub fn show_cost(&self) -> bool {
        self.cost_known && (self.cost_cny > 0.0 || self.has_activity())
    }

    /// 缓存命中率；没有任何解析记录时为 `None`（UI 也可直接展示计数）。
    #[cfg(test)]
    pub fn cache_hit_rate(&self) -> Option<f32> {
        (self.parse_calls > 0).then(|| {
            (self.parse_cache_hits as f64 / self.parse_calls as f64).clamp(0.0, 1.0) as f32
        })
    }

    /// 最近一个 24h 桶（今天）的消耗。
    pub fn today(&self) -> TokenUsage {
        self.trend.last().copied().unwrap_or(TokenUsage::ZERO)
    }

    /// 最近 7 个桶的消耗。
    pub fn last_7(&self) -> TokenUsage {
        let mut total = TokenUsage::ZERO;
        for bucket in self.trend.iter().rev().take(7) {
            total.merge(*bucket);
        }
        total
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: TokenUsage,
    /// 该模型的估算花费（无价目 = 0）。
    pub cost_cny: f64,
}

/// 下载项维度的 AI 消耗。
#[derive(Debug, Clone, PartialEq)]
pub struct ItemTokenUsage {
    pub id: String,
    pub file_name: String,
    pub tokens: TokenUsage,
    pub parse: TokenUsage,
    pub model: Option<String>,
    pub cache_hit: bool,
    /// 与同一次解析共享的下载项数量（1 = 独占）。
    pub parse_siblings: u32,
    pub size_bytes: Option<u64>,
    /// 该项可归属的估算花费（解析 + 旧版分类；无价目 = 0）。
    pub cost_cny: f64,
}

/// 传输/任务活动。
#[derive(Debug, Clone, Default)]
pub struct TransferSummary {
    pub total: u64,
    pub downloading: u64,
    pub paused: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub active_connections: u64,
    pub current_speed_bps: f64,
    /// 当前活动的剩余字节。
    pub active_remaining_bytes: u64,
    /// 所有记录已下载字节之和。
    pub downloaded_bytes: u64,
    /// 已完成记录的字节（无 total 时用 downloaded 兜底）。
    pub completed_bytes: u64,
}

/// 维护相关占用。
#[derive(Debug, Clone, Default)]
pub struct HygieneUsage {
    /// `.part` 断点数据（含 `.part.json`）。
    pub part_bytes: u64,
    pub part_files: u64,
    /// 未归属文件（与 [`LibraryUsage`] 同口径，这里用于 KPI 汇总）。
    pub untracked_bytes: u64,
    pub untracked_files: u64,
    /// 状态目录（`%LOCALAPPDATA%\RDM`）总占用。
    pub state_dir_bytes: u64,
    /// AI 解析缓存大小 / 条目数。
    pub ai_cache_bytes: u64,
    pub ai_cache_entries: u64,
    /// 日志目录大小。
    pub logs_bytes: u64,
}

// ---------------------------------------------------------------------------
// UI 状态（app.rs 持有；扫描结果通过 channel 回到主线程）
// ---------------------------------------------------------------------------

/// 扫描线程返回的结果。
pub enum ScanOutcome {
    Ok(Box<scan::ScanResult>),
    Err(String),
}

/// 资源监控页的运行时状态（GUI 主线程）。
#[derive(Default)]
pub struct MonitorUiState {
    pub snapshot: Option<Arc<MonitorSnapshot>>,
    pub scanning: bool,
    pub error: Option<String>,
    /// 最近一次成功扫描完成时间（Unix ms）。
    pub last_success_ms: Option<u64>,
    /// 正在运行的扫描线程通道。
    pub pending: Option<std::sync::mpsc::Receiver<ScanOutcome>>,
    /// 上次触发扫描的时间（`Instant` 不好跨模块序列化，用 ms + 单调时钟由 app 维护）。
    pub last_request_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// AI 用量账本（`<state_dir>/ai_usage.json`）
// ---------------------------------------------------------------------------

/// 账本文件版本。
pub const AI_USAGE_VERSION: u32 = 1;
/// 账本文件相对 `state_dir` 的路径。
pub const AI_USAGE_FILE: &str = "ai_usage.json";
/// 14 个滚动 24h 桶。
pub const TREND_BUCKETS: usize = 14;
/// 24h 毫秒数。
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// 保留的原始记录条数（用于「最近 AI 活动」；总量在 totals 里）。
pub const MAX_AI_RECORDS: usize = 200;
/// 按模型聚合的保留上限。
pub const MAX_AI_MODELS: usize = 16;

/// 一次 AI 调用的记账记录。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AiUsageRecord {
    /// Unix ms。
    pub at_ms: u64,
    pub kind: AiUsageKind,
    /// 脱敏 host（只有 host，不含 path/query）；聊天/分类可能没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub cache_hit: bool,
    /// 归属的下载项 id（解析在下单后回填；聊天/分类可能为空）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub item_ids: Vec<String>,
    /// 解析会话 id（`AiParseFinished.request_id`），用于把下单回填到正确记录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// 按内置价目估算的人民币花费；`None` = 无价目（Custom/未知模型），
    /// `Some(0.0)` = 有价目但 token 为 0（例如缓存命中）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cny: Option<f64>,
    pub tokens: TokenUsage,
}

/// AI 调用种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiUsageKind {
    /// 链接/页面解析。
    Parse,
    /// 保存目录分类。
    ///
    /// **历史兼容**：`docs/design/16-reuse-and-simplification.md` §4 起分类不再调用
    /// AI，因此不会产生新的 `classify` 记录；保留该变体只为反序列化旧
    /// `ai_usage.json`（否则整本账会因未知枚举值加载失败）。UI 不再展示分类消耗。
    Classify,
    /// AI 助手聊天。
    Chat,
}

impl AiUsageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Classify => "classify",
            Self::Chat => "chat",
        }
    }
}

/// 滚动 24h 趋势桶。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageTrend {
    /// 桶锚点（最近一次 ensure 的 ms）。
    #[serde(default)]
    pub updated_ms: u64,
    /// 旧 → 新，长度固定 [`TREND_BUCKETS`]。
    #[serde(default)]
    pub buckets: Vec<TokenUsage>,
    /// 与 `buckets` 对齐的估算花费（人民币；旧账本缺省为空，由 ensure 补 0）。
    #[serde(default)]
    pub cost_buckets: Vec<f64>,
}

impl UsageTrend {
    #[cfg(test)]
    pub fn new(now_ms: u64) -> Self {
        Self {
            updated_ms: now_ms,
            buckets: vec![TokenUsage::ZERO; TREND_BUCKETS],
            cost_buckets: vec![0.0; TREND_BUCKETS],
        }
    }

    /// 时间前进后滚动桶；最多滚掉整段历史。
    pub fn ensure(&mut self, now_ms: u64) {
        if self.buckets.len() != TREND_BUCKETS {
            self.buckets = vec![TokenUsage::ZERO; TREND_BUCKETS];
        }
        if self.cost_buckets.len() != TREND_BUCKETS {
            self.cost_buckets = vec![0.0; TREND_BUCKETS];
        }
        if self.updated_ms == 0 {
            self.updated_ms = now_ms;
            return;
        }
        if now_ms <= self.updated_ms {
            return;
        }
        let steps = ((now_ms - self.updated_ms) / DAY_MS) as usize;
        if steps == 0 {
            return;
        }
        let steps = steps.min(TREND_BUCKETS);
        for _ in 0..steps {
            self.buckets.remove(0);
            self.buckets.push(TokenUsage::ZERO);
            self.cost_buckets.remove(0);
            self.cost_buckets.push(0.0);
        }
        self.updated_ms = self.updated_ms.saturating_add(steps as u64 * DAY_MS);
    }

    /// 把一次消耗记入「当前」桶（调用前请先 [`Self::ensure`]）。
    pub fn add(&mut self, tokens: TokenUsage) {
        if let Some(last) = self.buckets.last_mut() {
            last.merge(tokens);
        }
    }

    /// 把一次估算花费记入「当前」桶（调用前请先 [`Self::ensure`]）。
    pub fn add_cost(&mut self, cost_cny: f64) {
        if let Some(last) = self.cost_buckets.last_mut() {
            *last += cost_cny;
        }
    }

    pub fn as_slice(&self) -> &[TokenUsage] {
        &self.buckets
    }

    /// 最近 n 个桶的估算花费。
    pub fn cost_last(&self, buckets: usize) -> f64 {
        self.cost_buckets.iter().rev().take(buckets).sum()
    }

    /// 当前桶（今天）的估算花费。
    pub fn cost_today(&self) -> f64 {
        self.cost_buckets.last().copied().unwrap_or(0.0)
    }
}

/// AI 用量账本：终身累计 + 按种类/模型 + 14 桶趋势 + 最近记录。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AiUsageLedger {
    #[serde(default = "ledger_version")]
    pub version: u32,
    #[serde(default)]
    pub totals: TokenUsage,
    /// 终身估算花费（人民币；无价目调用不计入）。
    #[serde(default)]
    pub cost_cny: f64,
    /// `parse` / `classify` / `chat`。
    #[serde(default)]
    pub by_kind: BTreeMap<String, TokenUsage>,
    #[serde(default)]
    pub by_kind_cost: BTreeMap<String, f64>,
    #[serde(default)]
    pub by_model: BTreeMap<String, TokenUsage>,
    #[serde(default)]
    pub by_model_cost: BTreeMap<String, f64>,
    #[serde(default)]
    pub trend: UsageTrend,
    #[serde(default)]
    pub parse_calls: u64,
    #[serde(default)]
    pub parse_cache_hits: u64,
    #[serde(default)]
    pub estimated_calls: u64,
    /// 有价目且已计费的记录数（含 0 元缓存命中）。
    #[serde(default)]
    pub priced_records: u64,
    /// 无价目但确实消耗了 token 的调用数。
    #[serde(default)]
    pub unpriced_calls: u64,
    /// 最近记录（旧 → 新，最多 [`MAX_AI_RECORDS`]）。
    #[serde(default)]
    pub records: Vec<AiUsageRecord>,
}

fn ledger_version() -> u32 {
    AI_USAGE_VERSION
}

impl Default for AiUsageLedger {
    fn default() -> Self {
        Self {
            version: AI_USAGE_VERSION,
            totals: TokenUsage::ZERO,
            cost_cny: 0.0,
            by_kind: BTreeMap::new(),
            by_kind_cost: BTreeMap::new(),
            by_model: BTreeMap::new(),
            by_model_cost: BTreeMap::new(),
            trend: UsageTrend::default(),
            parse_calls: 0,
            parse_cache_hits: 0,
            estimated_calls: 0,
            priced_records: 0,
            unpriced_calls: 0,
            records: Vec::new(),
        }
    }
}

impl AiUsageLedger {
    /// `<state_dir>/ai_usage.json`。
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(AI_USAGE_FILE)
    }

    /// 读取账本；缺文件/损坏/版本不符都退化为空账本（监控不是关键路径，不能因此启动失败）。
    pub fn load(state_dir: &Path) -> Self {
        let path = Self::path(state_dir);
        let Ok(text) = fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(mut ledger) => {
                if ledger.version != AI_USAGE_VERSION {
                    return Self::default();
                }
                ledger.normalize(now_ms());
                ledger
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ai_usage.json 解析失败，使用空账本");
                Self::default()
            }
        }
    }

    /// 原子写回（失败只记日志：监控数据不是正确性数据）。
    pub fn save(&self, state_dir: &Path) {
        if let Err(error) = self.try_save(state_dir) {
            tracing::warn!(%error, "ai_usage.json 写入失败");
        }
    }

    fn try_save(&self, state_dir: &Path) -> io::Result<()> {
        let path = Self::path(state_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::other(format!("cannot serialize ai usage: {error}")))?;
        let tmp = path.with_file_name(format!(".{AI_USAGE_FILE}.{}.tmp", std::process::id()));
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)
    }

    /// 时间前进后滚动趋势桶（读取/记录前调用）。
    pub fn normalize(&mut self, now_ms: u64) {
        self.trend.ensure(now_ms);
    }

    /// 记录一次 AI 消耗并更新全部聚合。
    pub fn record(&mut self, record: AiUsageRecord, now_ms: u64) {
        self.normalize(now_ms);
        let tokens = record.tokens;
        self.totals.merge(tokens);
        self.by_kind
            .entry(record.kind.as_str().to_owned())
            .or_insert(TokenUsage::ZERO)
            .merge(tokens);
        if let Some(model) = record
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            self.by_model
                .entry(model.to_owned())
                .or_insert(TokenUsage::ZERO)
                .merge(tokens);
            self.trim_models();
        }
        self.trend.add(tokens);
        // 费用：有价目记录写入终身/按种类/按模型/趋势；无价目只计数，不假装 0 元。
        match record.cost_cny {
            Some(cost) => {
                let cost = cost.max(0.0);
                self.cost_cny += cost;
                self.priced_records = self.priced_records.saturating_add(1);
                *self
                    .by_kind_cost
                    .entry(record.kind.as_str().to_owned())
                    .or_insert(0.0) += cost;
                if let Some(model) = record
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                {
                    *self.by_model_cost.entry(model.to_owned()).or_insert(0.0) += cost;
                }
                self.trend.add_cost(cost);
            }
            None if !tokens.is_zero() => {
                self.unpriced_calls = self.unpriced_calls.saturating_add(1);
            }
            None => {}
        }
        if record.kind == AiUsageKind::Parse {
            self.parse_calls = self.parse_calls.saturating_add(1);
            if record.cache_hit {
                self.parse_cache_hits = self.parse_cache_hits.saturating_add(1);
            }
        }
        if tokens.estimated {
            self.estimated_calls = self.estimated_calls.saturating_add(tokens.calls as u64);
        }
        self.records.push(record);
        while self.records.len() > MAX_AI_RECORDS {
            self.records.remove(0);
        }
    }

    /// 把下载项 id 回填到最近一条指定 session 的解析记录（用户在解析后点了下载）。
    pub fn attach_items_to_parse(&mut self, session_id: u64, item_ids: &[String]) {
        if item_ids.is_empty() {
            return;
        }
        if let Some(record) = self.records.iter_mut().rev().find(|record| {
            record.kind == AiUsageKind::Parse && record.session_id == Some(session_id)
        }) {
            for id in item_ids {
                if !record.item_ids.contains(id) {
                    record.item_ids.push(id.clone());
                }
            }
        }
    }

    pub fn kind_usage(&self, kind: AiUsageKind) -> TokenUsage {
        self.by_kind
            .get(kind.as_str())
            .copied()
            .unwrap_or(TokenUsage::ZERO)
    }

    /// 某类调用的估算花费（人民币；无价目为 0）。
    pub fn kind_cost(&self, kind: AiUsageKind) -> f64 {
        self.by_kind_cost.get(kind.as_str()).copied().unwrap_or(0.0)
    }

    /// 按 token 降序的模型消耗（含估算花费）。
    pub fn models_sorted(&self) -> Vec<ModelUsage> {
        let mut models: Vec<ModelUsage> = self
            .by_model
            .iter()
            .map(|(model, tokens)| ModelUsage {
                model: model.clone(),
                tokens: *tokens,
                cost_cny: self.by_model_cost.get(model).copied().unwrap_or(0.0),
            })
            .collect();
        models.sort_by(|a, b| {
            b.tokens
                .total_tokens
                .cmp(&a.tokens.total_tokens)
                .then_with(|| a.model.cmp(&b.model))
        });
        models
    }

    fn trim_models(&mut self) {
        if self.by_model.len() <= MAX_AI_MODELS {
            return;
        }
        let mut models: Vec<(String, TokenUsage)> = self
            .by_model
            .iter()
            .map(|(model, tokens)| (model.clone(), *tokens))
            .collect();
        models.sort_by(|a, b| {
            b.1.total_tokens
                .cmp(&a.1.total_tokens)
                .then_with(|| a.0.cmp(&b.0))
        });
        models.truncate(MAX_AI_MODELS);
        self.by_model = models.into_iter().collect();
        // 模型价格聚合同步裁剪（避免 map 无限增长）。
        self.by_model_cost
            .retain(|model, _| self.by_model.contains_key(model));
    }
}

/// 当前 Unix 毫秒（与 `model::now_ms` 同口径；这里避免循环依赖）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 格式化（UI 唯一入口；避免各视图各写一套）
// ---------------------------------------------------------------------------

/// token 数量的紧凑显示：`1.24M` / `12.4K` / `932`。
pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else if tokens >= 1_000 {
        format!("{:.2}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// 带估算标记的 token 文本：估算值前缀 `≈`。
pub fn format_tokens_maybe_estimated(tokens: &TokenUsage) -> String {
    let text = format_tokens(tokens.total_tokens);
    if tokens.estimated {
        format!("≈{text}")
    } else {
        text
    }
}

/// 人民币金额：`¥1.23` / `¥0.0123` / `<¥0.0001` / `¥0.00`。
pub fn format_cny(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "¥0.00".to_owned();
    }
    if value >= 1.0 {
        format!("¥{value:.2}")
    } else if value >= 0.0001 {
        format!("¥{value:.4}")
    } else {
        "<¥0.0001".to_owned()
    }
}

/// 带「估算」语义的人民币金额：`≈ ¥1.23`。
pub fn format_cny_approx(value: f64) -> String {
    format!("≈ {}", format_cny(value))
}

/// 精确千分位（明细/精确值展示）: `1,234,567`。
pub fn format_tokens_exact(tokens: u64) -> String {
    let raw = tokens.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().enumerate() {
        if index > 0 && (raw.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 百分比：`82%`；小于 1% 显示 `<1%`；未知 `--`。
pub fn format_percent(fraction: Option<f32>) -> String {
    match fraction {
        Some(value) if value.is_finite() => {
            let percent = (value.clamp(0.0, 1.0) * 100.0) as u32;
            if percent == 0 && value > 0.0 {
                "<1%".to_owned()
            } else {
                format!("{percent}%")
            }
        }
        _ => "--".to_owned(),
    }
}

/// 相对时间：刚刚 / N 分钟前 / N 小时前 / N 天前。
pub fn format_relative_ms(now_ms: u64, then_ms: u64) -> String {
    if then_ms == 0 {
        return "--".to_owned();
    }
    let elapsed = now_ms.saturating_sub(then_ms);
    if elapsed < 60_000 {
        return "刚刚".to_owned();
    }
    let minutes = elapsed / 60_000;
    if minutes < 60 {
        return format!("{minutes} 分钟前");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} 小时前");
    }
    let days = hours / 24;
    if days < 365 {
        return format!("{days} 天前");
    }
    format!("{} 年前", days / 365)
}

/// 「HH:MM:SS」（本地时区）。
pub fn format_clock_local(ms: u64) -> String {
    let seconds = (ms / 1000) as i64;
    let seconds_of_day = seconds.rem_euclid(86_400);
    let local = (seconds_of_day + local_utc_offset_seconds()).rem_euclid(86_400);
    let hour = local / 3_600;
    let minute = (local % 3_600) / 60;
    let second = local % 60;
    format!("{hour:02}:{minute:02}:{second:02}")
}

/// 本机 UTC 偏移秒数（Windows 用 `GetTimeZoneInformation`，其它平台退回 0）。
fn local_utc_offset_seconds() -> i64 {
    #[cfg(windows)]
    {
        use windows::Win32::System::Time::GetTimeZoneInformation;
        use windows::Win32::System::Time::TIME_ZONE_INFORMATION;
        // SAFETY: 结构体由系统填充，调用无副作用。
        let mut info = unsafe { std::mem::zeroed::<TIME_ZONE_INFORMATION>() };
        let rc = unsafe { GetTimeZoneInformation(&mut info) };
        let bias_minutes = if rc == 2 {
            // 夏令时生效：Bias + DaylightBias（单位分钟）。
            info.Bias + info.DaylightBias
        } else {
            info.Bias + info.StandardBias
        };
        -(bias_minutes as i64) * 60
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// 扫描时间轴标签（旧 → 新）：`13 天前` … `今天`。
pub fn trend_axis_labels() -> [&'static str; 3] {
    ["13 天前", "7 天前", "今天"]
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(total: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: total / 2,
            completion_tokens: total - total / 2,
            total_tokens: total,
            calls: 1,
            estimated: false,
        }
    }

    fn temp_state_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rdm-monitor-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn formats_tokens_compactly() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.00K");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(1_240_000), "1.24M");
        assert_eq!(format_tokens_exact(1_234_567), "1,234,567");
        assert_eq!(format_tokens_exact(999), "999");
    }

    #[test]
    fn formats_percent_boundaries() {
        assert_eq!(format_percent(None), "--");
        assert_eq!(format_percent(Some(0.0)), "0%");
        assert_eq!(format_percent(Some(0.004)), "<1%");
        assert_eq!(format_percent(Some(0.82)), "82%");
        assert_eq!(format_percent(Some(1.5)), "100%");
    }

    #[test]
    fn formats_cny_for_all_magnitudes() {
        assert_eq!(format_cny(0.0), "¥0.00");
        assert_eq!(format_cny(1.234), "¥1.23");
        assert_eq!(format_cny(0.012_345), "¥0.0123");
        assert_eq!(format_cny(0.000_123), "¥0.0001");
        assert_eq!(format_cny(0.000_001), "<¥0.0001");
        assert_eq!(format_cny_approx(0.05), "≈ ¥0.0500");
    }

    #[test]
    fn records_usage_and_updates_trend_and_counters() {
        let now = 1_700_000_000_000;
        let mut ledger = AiUsageLedger::default();
        ledger.normalize(now);
        ledger.record(
            AiUsageRecord {
                at_ms: now,
                kind: AiUsageKind::Parse,
                host: Some("example.com".to_owned()),
                model: Some("mock".to_owned()),
                cache_hit: false,
                item_ids: Vec::new(),
                session_id: Some(7),
                cost_cny: Some(0.03),
                tokens: usage(1_500),
            },
            now,
        );
        ledger.record(
            AiUsageRecord {
                at_ms: now,
                kind: AiUsageKind::Parse,
                host: Some("example.com".to_owned()),
                model: Some("mock".to_owned()),
                cache_hit: true,
                item_ids: Vec::new(),
                session_id: None,
                cost_cny: Some(0.0),
                tokens: TokenUsage::ZERO,
            },
            now,
        );
        ledger.record(
            AiUsageRecord {
                at_ms: now,
                kind: AiUsageKind::Classify,
                host: None,
                model: Some("mock".to_owned()),
                cache_hit: false,
                item_ids: vec!["dl_1".to_owned()],
                session_id: None,
                cost_cny: Some(0.005),
                tokens: usage(120),
            },
            now,
        );

        assert_eq!(ledger.totals.total_tokens, 1_620);
        assert_eq!(ledger.kind_usage(AiUsageKind::Parse).total_tokens, 1_500);
        assert_eq!(ledger.kind_usage(AiUsageKind::Classify).total_tokens, 120);
        assert_eq!(ledger.parse_calls, 2);
        assert_eq!(ledger.parse_cache_hits, 1);
        assert_eq!(ledger.records.len(), 3);
        assert!((ledger.cost_cny - 0.035).abs() < 1e-9);
        assert_eq!(ledger.priced_records, 3);
        assert!((ledger.kind_cost(AiUsageKind::Parse) - 0.03).abs() < 1e-9);
        assert!((ledger.trend.cost_today() - 0.035).abs() < 1e-9);
        assert_eq!(ledger.trend.buckets.len(), TREND_BUCKETS);
        assert_eq!(ledger.trend.buckets[TREND_BUCKETS - 1].total_tokens, 1_620);

        ledger.attach_items_to_parse(7, &["dl_9".to_owned()]);
        assert_eq!(ledger.records[0].item_ids, vec!["dl_9".to_owned()]);
    }

    #[test]
    fn trend_buckets_roll_by_whole_days() {
        let now = 1_700_000_000_000;
        let mut trend = UsageTrend::new(now);
        trend.add(usage(100));
        assert_eq!(trend.buckets[TREND_BUCKETS - 1].total_tokens, 100);

        // 前进两天：旧值滚到倒数第三格。
        trend.ensure(now + 2 * DAY_MS);
        assert_eq!(trend.buckets[TREND_BUCKETS - 1].total_tokens, 0);
        assert_eq!(trend.buckets[TREND_BUCKETS - 3].total_tokens, 100);

        // 前进一整年：全部滚掉，不 panic。
        trend.ensure(now + 365 * DAY_MS);
        assert!(trend.buckets.iter().all(|bucket| bucket.is_zero()));
        assert_eq!(trend.buckets.len(), TREND_BUCKETS);
    }

    #[test]
    fn estimated_calls_are_counted() {
        let now = 1_700_000_000_000;
        let mut ledger = AiUsageLedger::default();
        ledger.normalize(now);
        let mut tokens = usage(10);
        tokens.estimated = true;
        ledger.record(
            AiUsageRecord {
                at_ms: now,
                kind: AiUsageKind::Chat,
                host: None,
                model: None,
                cache_hit: false,
                item_ids: Vec::new(),
                session_id: None,
                cost_cny: None,
                tokens,
            },
            now,
        );
        assert_eq!(ledger.estimated_calls, 1);
        assert_eq!(ledger.kind_usage(AiUsageKind::Chat).total_tokens, 10);
        assert_eq!(ledger.unpriced_calls, 1, "无价目调用要单独计数");
        assert_eq!(ledger.cost_cny, 0.0);
    }

    #[test]
    fn ledger_round_trips_and_ignores_corrupt_files() {
        let dir = temp_state_dir("ledger");
        let mut ledger = AiUsageLedger::default();
        ledger.record(
            AiUsageRecord {
                at_ms: 42,
                kind: AiUsageKind::Parse,
                host: Some("example.com".to_owned()),
                model: Some("m".to_owned()),
                cache_hit: false,
                item_ids: vec!["dl_1".to_owned()],
                session_id: None,
                cost_cny: Some(0.01),
                tokens: usage(50),
            },
            now_ms(),
        );
        ledger.save(&dir);
        let loaded = AiUsageLedger::load(&dir);
        assert_eq!(loaded.totals.total_tokens, 50);
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.cost_cny, 0.01);

        std::fs::write(AiUsageLedger::path(&dir), "not json").unwrap();
        assert!(AiUsageLedger::load(&dir).totals.is_zero());

        std::fs::write(
            AiUsageLedger::path(&dir),
            r#"{"version":999,"totals":{"total_tokens":7}}"#,
        )
        .unwrap();
        assert!(AiUsageLedger::load(&dir).totals.is_zero());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_aggregation_is_sorted_and_capped() {
        let now = 1_700_000_000_000;
        let mut ledger = AiUsageLedger::default();
        ledger.normalize(now);
        for index in 0..(MAX_AI_MODELS + 4) {
            ledger.record(
                AiUsageRecord {
                    at_ms: now,
                    kind: AiUsageKind::Parse,
                    host: None,
                    model: Some(format!("m{index}")),
                    cache_hit: false,
                    item_ids: Vec::new(),
                    session_id: None,
                    cost_cny: Some(0.001),
                    tokens: usage((index as u64 + 1) * 10),
                },
                now,
            );
        }
        assert!(ledger.by_model.len() <= MAX_AI_MODELS);
        assert!(ledger.by_model_cost.len() <= MAX_AI_MODELS);
        let models = ledger.models_sorted();
        assert!(models[0].tokens.total_tokens >= models[1].tokens.total_tokens);
        assert!(models[0].cost_cny > 0.0);
    }

    #[test]
    fn summary_helpers_report_today_and_last_week() {
        let mut summary = AiUsageSummary {
            trend: vec![TokenUsage::ZERO; TREND_BUCKETS],
            ..AiUsageSummary::default()
        };
        for (index, bucket) in summary.trend.iter_mut().enumerate() {
            *bucket = usage((index as u64 + 1) * 10);
        }
        assert_eq!(summary.today().total_tokens, 140);
        assert_eq!(summary.last_7().total_tokens, 770);
        summary.parse_calls = 4;
        summary.parse_cache_hits = 2;
        assert_eq!(summary.cache_hit_rate(), Some(0.5));
        assert!(summary.has_activity());
    }

    #[test]
    fn clock_formatting_matches_local_day() {
        // 2026-10-22T00:00:00Z 的毫秒值；时区偏移由宿主决定，只校验格式。
        let text = format_clock_local(1_792_627_200_000);
        assert_eq!(text.len(), 8, "{text}");
        assert_eq!(text.chars().filter(|ch| *ch == ':').count(), 2);
        assert_eq!(format_relative_ms(10_000, 0), "--");
        assert_eq!(format_relative_ms(10_000, 9_500), "刚刚");
        assert_eq!(format_relative_ms(3_600_000, 10_000), "59 分钟前");
    }
}
