//! GUI 侧的纯数据模型：下载行状态、过滤器、设置、格式化、持久化。
//!
//! 这里刻意不引入 `gpui`，所有逻辑都可以用普通单元测试覆盖
//! （`cargo test -p rdm-gui`）。视图层（`app.rs`）只负责把这里的数据画出来。
//!
//! 设计要点：
//! - `DownloadItem` 是 GUI 的投影，真正的断点状态由 `rdm-core` 的 `.part.json` 持有；
//!   因此 GUI 持久化允许每 1s 落一次，丢一点展示数据不会影响正确性。
//! - `DownloadError::Paused / Cancelled` 是控制流，不是失败；见 `apply_result`。
//! - 过滤器有两种历史语义，互不混淆：legacy [`Filter::Failed`] 把 `Failed + Cancelled`
//!   都算失败；新版 `views::app::ViewFilter::Failed` **只含 `Failed`**（取消只出现在「全部」）。
//!   新代码请使用 `ViewFilter`；`Filter` 仅为旧版 UI 保留（审计 F-07）。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rdm_ai::{CategoryRule, TokenUsage};
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

    /// 生命周期展示排序（下载中 → 已暂停 → 已完成 → 失败 → 已取消）。
    ///
    /// 审计 F-21：排序必须用固定 ordinal，不能按中文文案的字典序（码点序会得到
    /// 「下载中/已暂停/已完成/失败/已取消」这种看似随机的顺序）。
    pub fn sort_rank(self) -> u8 {
        match self {
            Self::Downloading => 0,
            Self::Paused => 1,
            Self::Completed => 2,
            Self::Failed => 3,
            Self::Cancelled => 4,
        }
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

/// 正在等待引擎确认的停止动作。
///
/// 暂停/取消不是「立刻生效」：引擎 worker 还要走完当前 64 KiB 读循环、flush 状态。
/// 在引擎回 `Paused/Cancelled` 之前，GUI 保持 `Downloading` 状态并展示「暂停中/取消中」，
/// 避免用户点「继续」时新旧两个任务同时写同一个 `.part`（会损坏文件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopRequest {
    #[default]
    None,
    Pause,
    Cancel,
}

impl StopRequest {
    pub fn label_zh(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Pause => Some("暂停中"),
            Self::Cancel => Some("取消中"),
        }
    }

    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
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
    /// Catppuccin Latte（浅色）。
    CatppuccinLatte,
    /// Catppuccin Frappe（深色）。
    CatppuccinFrappe,
    /// Catppuccin Macchiato（深色）。
    CatppuccinMacchiato,
    /// Catppuccin Mocha（深色）。
    CatppuccinMocha,
}

impl Theme {
    /// 设置页展示顺序；`theme.rs` 的 `Palette` 必须覆盖这 6 个变体。
    pub const ALL: [Theme; 6] = [
        Theme::Dark,
        Theme::Light,
        Theme::CatppuccinLatte,
        Theme::CatppuccinFrappe,
        Theme::CatppuccinMacchiato,
        Theme::CatppuccinMocha,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::CatppuccinFrappe => "Catppuccin Frappe",
            Self::CatppuccinMacchiato => "Catppuccin Macchiato",
            Self::CatppuccinMocha => "Catppuccin Mocha",
        }
    }
}

/// 新版 UI 的主题模式（`ui_theme` 字段）。`None` 表示跟随系统；
/// 旧版 `theme` 字段在首次升级时映射为 `Some(Light/Dark)`（见 `resolved_ui_theme`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiThemeMode {
    /// 跟随 Windows 应用明暗模式。
    #[default]
    FollowSystem,
    Light,
    Dark,
}

impl UiThemeMode {
    pub const ALL: [UiThemeMode; 3] = [
        UiThemeMode::FollowSystem,
        UiThemeMode::Light,
        UiThemeMode::Dark,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::FollowSystem => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        }
    }
}

/// 强调色预设（04 §3，默认蓝）。只影响可交互强调元素，不影响状态色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccentPreset {
    #[default]
    Blue,
    Cyan,
    Violet,
    Green,
    Orange,
}

impl AccentPreset {
    pub const ALL: [AccentPreset; 5] = [
        AccentPreset::Blue,
        AccentPreset::Cyan,
        AccentPreset::Violet,
        AccentPreset::Green,
        AccentPreset::Orange,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Blue => "蓝",
            Self::Cyan => "青",
            Self::Violet => "紫",
            Self::Green => "绿",
            Self::Orange => "橙",
        }
    }
}

/// 列表密度：舒适 64px / 紧凑 40px（04 §7）。窗口 <1100px 时运行时自动紧凑。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

impl Density {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Comfortable => "舒适",
            Self::Compact => "紧凑",
        }
    }

    pub fn row_height(self) -> f32 {
        match self {
            Self::Comfortable => 64.0,
            Self::Compact => 40.0,
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
    /// 动态线程规划：写入 `DownloadRequest.dynamic_connections`（默认开）。
    pub dynamic_connections: bool,
    /// 按后缀/内容类型分目录保存（默认开）。
    pub save_by_extension: bool,
    /// 分类规则（后缀 → 目录名，唯一事实来源，见 `rdm_ai::classify`）。
    ///
    /// 字段级 `serde(default)` + 结构体级 `default` 保证旧 `settings.json`
    /// （没有该字段）自动迁移到内置默认规则；空/非法列表由 `normalized()` 修复。
    #[serde(default = "default_classification")]
    pub classification: Vec<CategoryRule>,

    // ---------------------------------------------------------------
    // 系统集成（桌面端）
    // ---------------------------------------------------------------
    /// 开机自启动：写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（默认关）。
    pub launch_at_startup: bool,
    /// 最小化到托盘：开启后**关闭窗口**时隐藏到托盘而不是退出；最小化按钮仍正常
    /// 最小化到任务栏（默认关，需托盘）。
    pub minimize_to_tray: bool,
    /// 静默启动：启动时不显示主窗口，直接驻留托盘（默认关，需托盘）。
    pub silent_start: bool,

    // ---------------------------------------------------------------
    // 新版 UI（docs/design）；旧版 UI 继续读 `theme` 字段，互不破坏。
    // ---------------------------------------------------------------
    /// 新版 UI 主题模式：`None` 时按旧 `theme` 字段迁移（浅色系 → Light，其余 → Dark）。
    pub ui_theme: Option<UiThemeMode>,
    /// 强调色预设（默认蓝）。
    pub accent: AccentPreset,
    /// 列表密度（默认舒适；窗口过窄时运行时自动紧凑）。
    pub density: Density,
    /// 减少动效：关闭过渡动画与循环 spinner（仍保留进度条更新）。
    pub reduce_motion: bool,
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
            dynamic_connections: true,
            save_by_extension: true,
            classification: default_classification(),
            launch_at_startup: false,
            minimize_to_tray: false,
            silent_start: false,
            ui_theme: None,
            accent: AccentPreset::default(),
            density: Density::default(),
            reduce_motion: false,
        }
    }
}

/// `serde(default)` 入口：旧 settings.json 没有 `classification` 时用内置规则。
fn default_classification() -> Vec<CategoryRule> {
    rdm_ai::default_category_rules()
}

impl Settings {
    /// 新版 UI 实际使用的主题模式：显式 `ui_theme` 优先；否则从旧 `theme` 迁移。
    ///
    /// 迁移规则：浅色系（Light / Catppuccin Latte）→ Light，其余 → Dark。
    /// 这样从旧版本升级的用户不会在启动时突然从深色/浅色切换。
    pub fn resolved_ui_theme(&self) -> UiThemeMode {
        if let Some(mode) = self.ui_theme {
            return mode;
        }
        match self.theme {
            Theme::Light | Theme::CatppuccinLatte => UiThemeMode::Light,
            _ => UiThemeMode::Dark,
        }
    }

    /// 把新版外观设置同步回旧版 `theme` 字段，保证 `RDM_UI=legacy` 回退时观感一致。
    pub fn sync_legacy_theme(&mut self, dark: bool) {
        match self.ui_theme {
            Some(UiThemeMode::Light) | Some(UiThemeMode::Dark) => {
                self.theme = if dark { Theme::Dark } else { Theme::Light };
            }
            Some(UiThemeMode::FollowSystem) | None => {
                if self.ui_theme.is_none() {
                    self.theme = if dark { Theme::Dark } else { Theme::Light };
                }
            }
        }
    }

    /// 是否需要常驻托盘图标：关闭到托盘与静默启动都依赖托盘。
    ///
    /// 单独开启「开机自启动」不强制托盘（正常显示主窗口），只有这两个开关会
    /// 让窗口可能处于隐藏状态，因此必须有托盘入口可以恢复。
    pub fn needs_tray(&self) -> bool {
        self.minimize_to_tray || self.silent_start
    }

    /// 启动时实际是否静默运行（设置开或命令行 `--silent` 强制）。
    pub fn effective_silent_start(&self, cli_silent: bool) -> bool {
        self.silent_start || cli_silent
    }

    /// 收敛非法值（手工改过 JSON 也不至于 panic）。
    pub fn normalized(&self) -> Self {
        let mut out = self.clone();
        out.default_connections = out.default_connections.clamp(1, rdm_core::MAX_CONNECTIONS);
        out.speed_limit_bps = out.speed_limit_bps.filter(|v| *v > 0);
        // 分类规则唯一事实来源：小写、去点、去重（全局唯一后缀）、限长；
        // 缺失分类补默认、`other` 兜底永远存在（见 `rdm_ai::sanitize_category_rules`）。
        out.classification = rdm_ai::sanitize_category_rules(&out.classification);
        out
    }

    /// 全局限速的「MB/s」展示文本（空 = 不限速）。
    pub fn speed_limit_mb_text(&self) -> String {
        match self.speed_limit_bps {
            Some(bps) => trim_float(bps as f64 / (1024.0 * 1024.0)),
            None => String::new(),
        }
    }

    /// 用设置填好下载默认参数（连接数/保存目录/限速/动态规划）的请求构造器。
    ///
    /// 语义边界（审计 F-24）：**不校验 URL、也不保证请求可下载**；调用方必须再走
    /// [`prepare_request`]（或自行校验 URL）。逐任务字段（如 `file_name`）可在两步之间设置。
    pub fn request_with_defaults(&self, url: impl Into<String>) -> DownloadRequest {
        let mut req = DownloadRequest::new(url);
        req.connections = self.default_connections.clamp(1, rdm_core::MAX_CONNECTIONS);
        req.output_dir = self.default_dir.clone();
        req.speed_limit_bps = self.speed_limit_bps.filter(|v| *v > 0);
        req.dynamic_connections = self.dynamic_connections;
        req
    }
}

// ---------------------------------------------------------------------------
// 分类规则进程内快照（GUI 筛选 / 磁盘分类共用同一份规则）
// ---------------------------------------------------------------------------

/// 进程内分类规则快照。
///
/// `Settings::classification` 保存后通过 [`set_classification_rules`] 写入这里；
/// 侧栏 `FileCategory` 筛选/计数、图标、资源监控扫描都从快照读取，保证
/// 「磁盘目录 / 侧栏分类 / 文件图标」三处永远一致（`docs/design/16` §4.1）。
/// 初始值 = 内置默认规则，因此未加载设置前也有一份可用规则。
static CLASSIFICATION_RULES: OnceLock<RwLock<Vec<CategoryRule>>> = OnceLock::new();

fn classification_rules_lock() -> &'static RwLock<Vec<CategoryRule>> {
    CLASSIFICATION_RULES.get_or_init(|| RwLock::new(rdm_ai::default_category_rules()))
}

/// 用设置里的分类规则刷新全局快照（内部会再清洗一次，永远保留全部 kind）。
///
/// 调用点：`RdmApp::new`（启动加载设置后）与 `RdmApp::apply_settings`
/// （设置页保存后）——见 `docs/design/16` §4.1。
pub fn set_classification_rules(rules: &[CategoryRule]) {
    let sanitized = rdm_ai::sanitize_category_rules(rules);
    let mut snapshot = classification_rules_lock()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *snapshot = sanitized;
}

/// 当前生效的分类规则（克隆；热路径请用 [`with_classification_rules`]）。
///
/// 设置页编辑器与后续自定义分类入口使用；非测试构建目前没有直接调用方。
#[allow(dead_code)]
pub fn classification_rules() -> Vec<CategoryRule> {
    with_classification_rules(|rules| rules.to_vec())
}

/// 在分类规则快照的读锁下执行 `f`（侧栏计数等热路径，避免每次克隆整表）。
pub fn with_classification_rules<R>(f: impl FnOnce(&[CategoryRule]) -> R) -> R {
    let snapshot = classification_rules_lock()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(snapshot.as_slice())
}

/// 文件名 → 分类 kind（`video`/`audio`/…/`other`），使用当前生效规则。
///
/// 未知 kind（例如手工 JSON 里的自定义分类）返回 `""`，由调用方归入「其他」。
pub fn category_kind_for_file(file_name: &str) -> String {
    with_classification_rules(|rules| {
        rdm_ai::category_for_with_rules(file_name, None, rules)
            .kind
            .clone()
    })
}

/// 文件名 → 分类目录名（`视频`/`文档`/…/`其他`），使用当前生效规则。
///
/// 用于「分类编辑器/自定义分类」等展示路径；当前 UI 直连 `CategoryRule.label`。
#[allow(dead_code)]
pub fn category_for_file(file_name: &str) -> String {
    with_classification_rules(|rules| {
        let rule = rdm_ai::category_for_with_rules(file_name, None, rules);
        if rule.label.trim().is_empty() {
            rdm_ai::CATEGORY_OTHER.to_owned()
        } else {
            rule.label.clone()
        }
    })
}

/// 校验 URL、补齐设置里的默认目录/限速，并用 GUI 设置收敛下载参数。
///
/// GUI 添加、IPC `enqueue`（浏览器扩展 → 原生宿主）、CLI URL、spool 恢复都走这一条路径。
/// **下载参数以 GUI 设置为唯一权威**：连接数强制为 `Settings::default_connections`、
/// 动态线程规划跟随 `Settings::dynamic_connections`——扩展/宿主随请求传来的 `connections`
/// 只作为向后兼容字段被接收，不再决定实际并发上限（2026-10-20 修复「GUI 设 16、
/// 扩展 popup 却显示上限 8」）。重试/续传不走这里，保留任务原始参数。
pub fn prepare_request(
    settings: &Settings,
    mut request: DownloadRequest,
) -> Result<DownloadRequest, String> {
    request.url = validate_url(&request.url)?;
    request = request.normalized();
    // 设置是唯一权威：覆盖扩展/宿主/旧 spool 文件里携带的连接数。
    request.connections = settings
        .default_connections
        .clamp(1, rdm_core::MAX_CONNECTIONS);
    request.dynamic_connections = settings.dynamic_connections;
    if request.output_dir.is_none() {
        request.output_dir = settings.default_dir.clone();
    }
    if request.speed_limit_bps.is_none() {
        request.speed_limit_bps = settings.speed_limit_bps.filter(|value| *value > 0);
    }
    Ok(request)
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

/// 解析设置页的「最大线程数」输入框：只接受 `1..=MAX_CONNECTIONS` 的整数。
pub fn parse_connections(text: &str) -> Result<usize, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("请输入连接数".to_string());
    }
    let value: usize = text.parse().map_err(|_| "连接数必须是整数".to_string())?;
    if !(1..=rdm_core::MAX_CONNECTIONS).contains(&value) {
        return Err(format!(
            "连接数必须在 1..={} 之间",
            rdm_core::MAX_CONNECTIONS
        ));
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
// AI 面板纯状态（供 app.rs 渲染；不依赖 gpui）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRole {
    System,
    User,
    Assistant,
}

impl AiRole {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::System => "系统",
            Self::User => "我",
            Self::Assistant => "AI",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiChatMessage {
    pub role: AiRole,
    pub text: String,
    /// 消息时间戳（`model.rs` 契约字段；渲染层暂未展示时间）。
    #[allow(dead_code)]
    pub created_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AiResourceCard {
    pub resource: rdm_ai::ParsedResource,
    pub selected: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AiParseStatus {
    #[default]
    Idle,
    Parsing,
    Ready,
    Failed,
}

impl AiParseStatus {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Idle => "未解析",
            Self::Parsing => "解析中…",
            Self::Ready => "已解析",
            Self::Failed => "解析失败",
        }
    }
}

/// 聊天历史保留上限：超出后丢弃最旧的消息。
///
/// 面板改成可滚动历史后消息会一直攒着，必须设上限，否则长时间对话会拖慢每帧渲染；
/// 流式输出中的 assistant 占位永远在末尾，不会被这条规则丢掉。
pub const AI_CHAT_HISTORY_LIMIT: usize = 200;

/// 一次 AI 解析来源的**内存**凭据（P4b-a，`docs/architecture.md` §11.12）。
///
/// GUI 从浏览器上下文拿到的一次性 Cookie/Referer/UA 只用于：
/// 1. resolver-first 的验证/方法回放（engine → `HostCredentials`）；
/// 2. 本次解析结果的「下载选中」交接（host 与解析来源完全一致时填入
///    `DownloadRequest{cookie,referer,user_agent}`）。
///
/// 生命周期：下一次解析开始时被替换、解析失败时清除；**不实现
/// `Serialize`/`Deserialize`**（永远不会进 settings/任务 store/解析缓存/日志），
/// 手写 `Debug` 只显示 host 与「是否设置」，不显示任何值。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ParseCredential {
    /// 归一化后的来源 host：小写、去尾点、不含端口。
    pub host: String,
    pub cookie: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
}

impl ParseCredential {
    /// 没有 host 或三个字段全空 → 等于没有凭据。
    pub fn is_empty(&self) -> bool {
        self.host.is_empty()
            || (self.cookie.is_none() && self.referer.is_none() && self.user_agent.is_none())
    }
}

impl std::fmt::Debug for ParseCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只暴露 host 与「是否设置」；任何字段值都不进日志/调试输出。
        f.debug_struct("ParseCredential")
            .field("host", &self.host)
            .field("cookie", &self.cookie.is_some())
            .field("referer", &self.referer.is_some())
            .field("user_agent", &self.user_agent.is_some())
            .finish()
    }
}

/// AI 成本归属到单个下载项（资源监控「哪个文件最贵」的数据来源）。
///
/// 解析产生的 token 属于**整次解析**：同一次解析创建了 N 个下载项时，每个项都记录
/// 同一份 `parse`（`parse_siblings = N`），资源监控的总量以账本（`AiUsageLedger`）为准，
/// 不会重复累计；UI 用「共享 N 项」徽标解释单个任务的归属。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemAiUsage {
    /// 创建该下载项的 AI 解析所消耗的 token（缓存命中时为 0）。
    #[serde(default)]
    pub parse: TokenUsage,
    /// **历史字段**：旧版本「AI 分类」消耗的 token。
    ///
    /// `docs/design/16-reuse-and-simplification.md` §4 起分类完全按后缀确定性完成，
    /// 不再调用 AI，因此新任务永远为 0；保留字段只为读取旧 `downloads.json`。
    #[serde(default)]
    pub classify: TokenUsage,
    /// 解析部分的估算花费（人民币；无价目为 0）。
    #[serde(default)]
    pub parse_cost_cny: f64,
    /// **历史字段**：旧版本 AI 分类的估算花费（新任务永远为 0）。
    #[serde(default)]
    pub classify_cost_cny: f64,
    /// 实际使用的模型名（解析；历史记录可能来自分类）；未知为 `None`。
    #[serde(default)]
    pub model: Option<String>,
    /// 解析结果是否来自本地解析缓存（命中不代表本次有 token 消耗）。
    #[serde(default)]
    pub cache_hit: bool,
    /// 与该项共享同一次解析的下载项数量（1 = 独占）。
    #[serde(default = "one_sibling")]
    pub parse_siblings: u32,
    /// 最近一次记录时间。
    #[serde(default)]
    pub updated_ms: u64,
}

fn one_sibling() -> u32 {
    1
}

impl ItemAiUsage {
    /// 该项可归属的 token 合计（解析 + 历史分类记录；不因共享而折算）。
    pub fn total(&self) -> TokenUsage {
        let mut total = self.parse;
        total.merge(self.classify);
        total
    }

    /// 该项可归属的估算花费合计（人民币；含历史分类记录）。
    pub fn cost_cny(&self) -> f64 {
        self.parse_cost_cny + self.classify_cost_cny
    }

    /// 该项是否有可展示的 AI 消耗：token 总量 **或** 调用次数非零都算。
    ///
    /// 审计 F-06：失败/被拒绝的模型调用可能 `total_tokens == 0` 但 `calls > 0`，
    /// 只看 total_tokens 会把这类记录误判为「无 AI 消耗」。
    pub fn has_tokens(&self) -> bool {
        let total = self.total();
        total.calls > 0 || total.total_tokens > 0
    }

    pub fn estimated(&self) -> bool {
        self.parse.estimated || self.classify.estimated
    }
}

/// 一次 AI 解析的完成结果（用于把 token/费用归属到随后创建的下载项）。
#[derive(Debug, Clone, Default)]
pub struct ParseUsageSnapshot {
    pub request_id: u64,
    pub usage: TokenUsage,
    pub model: Option<String>,
    pub cache_hit: bool,
    /// 本次解析的估算花费（有价目时 `Some`）。
    pub cost_cny: Option<f64>,
}

/// AI 解析/聊天面板的纯状态机。`app.rs` 只负责把字段画出来。
#[derive(Debug, Clone, Default)]
pub struct AiPanelState {
    pub visible: bool,
    pub status: AiParseStatus,
    pub source_url: String,
    pub messages: Vec<AiChatMessage>,
    pub resources: Vec<AiResourceCard>,
    pub notes: Vec<String>,
    pub ai_used: bool,
    pub cache_hit: bool,
    pub model: Option<String>,
    /// 本次解析消耗的 token（资源监控记账 + 检查器展示）。
    pub usage: TokenUsage,
    pub error: Option<String>,
    /// P4b-a：本次解析来源的内存凭据（仅下载交接用；Debug 已脱敏）。
    pub credential: Option<ParseCredential>,
    /// 解析请求代号；迟到的结果按此丢弃。
    pub request_id: u64,
    pub chat_pending: bool,
    /// 是否有 assistant 消息正在流式输出（`app.rs` 据此渲染「正在输出」占位/光标）。
    pub streaming: bool,
    /// 最近一次 Agent 工作流的确定性汇报（供后续普通问答的 prompt 引用；不持久化）。
    pub last_plan_summary: Option<String>,
}

impl AiPanelState {
    /// 开始一次解析：清空上一次的解析结果（保留聊天消息），并展示面板。
    ///
    /// 上一次的内存凭据也在此丢弃：新解析必须由 `app.rs` 紧接着调用
    /// [`Self::set_request_credential`] 才会带上自己的（可能有）凭据。
    pub fn begin_parse(&mut self, url: &str, request_id: u64) {
        self.visible = true;
        self.status = AiParseStatus::Parsing;
        self.source_url = url.to_string();
        self.resources.clear();
        self.notes.clear();
        self.ai_used = false;
        self.cache_hit = false;
        self.model = None;
        self.usage = TokenUsage::default();
        self.error = None;
        self.credential = None;
        self.request_id = request_id;
    }

    /// 应用成功返回的解析报告。默认全部资源勾选。
    pub fn apply_report(&mut self, report: rdm_ai::ParseReport) {
        self.visible = true;
        self.status = AiParseStatus::Ready;
        self.source_url = report.source_url;
        self.notes = report.notes;
        self.ai_used = report.ai_used;
        self.cache_hit = report.cache_hit;
        self.model = report.model;
        self.usage = report.usage;
        self.resources = report
            .resources
            .into_iter()
            .map(|resource| AiResourceCard {
                resource,
                selected: true,
            })
            .collect();
        self.error = None;
    }

    /// 应用解析失败（本地校验失败 / 无法访问页面等；AI 失败会由启发式结果兜底）。
    ///
    /// 失败不保留解析凭据：下载交接只对「本次成功解析」的来源 host 生效。
    pub fn apply_parse_error(&mut self, error: String) {
        self.visible = true;
        self.status = AiParseStatus::Failed;
        self.resources.clear();
        self.notes.clear();
        self.usage = TokenUsage::default();
        self.error = Some(error);
        self.clear_credential();
    }

    /// 重新解析成功后的「保留最佳结果」合并（design 17 §2.5）：
    /// 新结果在前，旧结果中 URL 未出现的按原顺序追加（保留原勾选状态）。
    /// 返回被保留的旧资源数量；去重键 = 资源 URL。
    pub fn merge_preserved_resources(&mut self, previous: Vec<AiResourceCard>) -> usize {
        let mut seen: std::collections::BTreeSet<String> = self
            .resources
            .iter()
            .map(|card| card.resource.url.clone())
            .collect();
        let mut kept = 0usize;
        for card in previous {
            if seen.insert(card.resource.url.clone()) {
                self.resources.push(card);
                kept += 1;
            }
        }
        kept
    }

    /// 追加一条面板 note（保持 `rdm_ai::parse::MAX_NOTES` 上限，新的在最前）。
    pub fn push_note(&mut self, note: impl Into<String>) {
        let note = note.into();
        self.notes.retain(|existing| existing != &note);
        self.notes.insert(0, note);
        self.notes.truncate(rdm_ai::parse::MAX_NOTES);
    }

    /// 重新解析失败但上次已有结果时：保留资源清单，只记录错误 + 说明（design 17 §2.5）。
    ///
    /// 返回 `true` 表示保留了上次结果（状态保持 `Ready`）；没有可保留结果时返回
    /// `false`，调用方应改用 [`Self::apply_parse_error`]。
    pub fn apply_reparse_error(&mut self, error: String, preserved: usize) -> bool {
        if self.resources.is_empty() {
            return false;
        }
        self.visible = true;
        // 状态保持 Ready：资源清单仍然可用，错误以横幅 + note 的形式呈现
        //（`begin_parse` 把状态置为 Parsing，这里必须显式恢复）。
        self.status = AiParseStatus::Ready;
        self.error = Some(error);
        self.usage = TokenUsage::default();
        self.clear_credential();
        self.push_note(format!(
            "重新解析失败；已保留上次解析的 {preserved} 个资源（可继续下载或稍后重试）"
        ));
        true
    }

    /// P4b-a：记录本次解析请求携带的**内存**凭据（`app.rs` 在 `begin_parse` 后调用）。
    ///
    /// host 从 `request.url` 归一化（小写/去尾点/不含端口）；cookie/referer/UA 去空串。
    /// 无效 URL 或三个字段全空 → 清空凭据（新解析总是替换上一次的凭据）。
    pub fn set_request_credential(&mut self, request: &rdm_ai::ParseRequest) {
        self.credential = credential_from_request(request);
    }

    /// 清除内存凭据（解析失败 / 主动切换上下文时调用）。
    pub fn clear_credential(&mut self) {
        self.credential = None;
    }

    /// P4b-a 下载交接：资源 host 与解析来源 host **完全一致**（大小写/尾点归一化）
    /// 时返回本次解析的凭据副本，否则 `None`（Cookie 不应跨 host 发送）。
    pub fn download_credentials_for(&self, resource_url: &str) -> Option<ParseCredential> {
        let credential = self.credential.as_ref()?;
        if credential.is_empty() {
            return None;
        }
        let host = normalized_url_host(resource_url)?;
        (host == credential.host).then(|| credential.clone())
    }

    /// P4b-a：把匹配 host 的解析凭据填入 `DownloadRequest`（`app.rs`「下载选中」调用）。
    ///
    /// 只在 host 完全一致时生效；只填充本面板持有的非空字段，不会把请求里已有的
    /// 值清空（GUI 自己构造的下载请求这些字段本来就是 `None`）。
    pub fn apply_credentials(&self, resource_url: &str, request: &mut rdm_core::DownloadRequest) {
        let Some(credential) = self.download_credentials_for(resource_url) else {
            return;
        };
        if credential.cookie.is_some() {
            request.cookie = credential.cookie;
        }
        if credential.referer.is_some() {
            request.referer = credential.referer;
        }
        if credential.user_agent.is_some() {
            request.user_agent = credential.user_agent;
        }
    }

    pub fn toggle_resource(&mut self, index: usize) {
        if let Some(card) = self.resources.get_mut(index) {
            card.selected = !card.selected;
        }
    }

    pub fn select_all(&mut self, selected: bool) {
        for card in &mut self.resources {
            card.selected = selected;
        }
    }

    pub fn selected_indices(&self) -> Vec<usize> {
        self.resources
            .iter()
            .enumerate()
            .filter(|(_, card)| card.selected)
            .map(|(index, _)| index)
            .collect()
    }

    pub fn selected_count(&self) -> usize {
        self.resources.iter().filter(|card| card.selected).count()
    }

    pub fn push_message(&mut self, role: AiRole, text: impl Into<String>) {
        self.push_message_capped(role, text.into());
    }

    /// 追加一条消息并保证历史不超过 [`AI_CHAT_HISTORY_LIMIT`]（丢最旧）。
    fn push_message_capped(&mut self, role: AiRole, text: String) {
        self.messages.push(AiChatMessage {
            role,
            text,
            created_ms: now_ms(),
        });
        if self.messages.len() > AI_CHAT_HISTORY_LIMIT {
            let excess = self.messages.len() - AI_CHAT_HISTORY_LIMIT;
            self.messages.drain(..excess);
        }
    }

    /// 开始一次流式问答：追加用户消息 + 一条空的 assistant 占位消息，进入流式状态。
    ///
    /// 调用方负责把面板设为可见并清空输入框；`chat_pending` 与 `streaming` 一起置位，
    /// 保证「发送按钮禁用」与「正在输出」两个 UI 状态不会互相矛盾。
    pub fn begin_chat_stream(&mut self, user_text: &str) {
        self.push_message(AiRole::User, user_text);
        self.push_message(AiRole::Assistant, String::new());
        self.streaming = true;
        self.chat_pending = true;
    }

    /// 开始一次**只产生 assistant 消息**的流式输出（Agent 执行汇报）。
    ///
    /// 与 [`Self::begin_chat_stream`] 的区别：不追加用户消息——用户请求已经作为
    /// 工作流计划的一部分上屏，汇报是这次工具执行的收尾，不再重复一条「用户问」。
    pub fn begin_assistant_stream(&mut self) {
        self.push_message(AiRole::Assistant, String::new());
        self.streaming = true;
        self.chat_pending = true;
    }

    /// 追加一段流式增量：空增量直接丢弃；末尾不是 assistant 时新建一条。
    ///
    /// 只追加不重置文本，最终以 [`Self::finish_chat_stream`] 的完整回复为准（防丢字/重复）。
    pub fn push_chat_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        match self.messages.last_mut() {
            Some(message) if message.role == AiRole::Assistant => message.text.push_str(delta),
            _ => self.push_message(AiRole::Assistant, delta),
        }
    }

    /// 结束流式输出：用引擎给的完整回复覆盖末尾 assistant（空回复填占位文案）。
    ///
    /// 末尾不是 assistant（例如历史被重置）时新建一条，避免把回复丢掉。
    pub fn finish_chat_stream(&mut self, full_reply: &str) {
        self.streaming = false;
        self.chat_pending = false;
        let text = if full_reply.trim().is_empty() {
            "（无内容）".to_string()
        } else {
            full_reply.to_string()
        };
        match self.messages.last_mut() {
            Some(message) if message.role == AiRole::Assistant => message.text = text,
            _ => self.push_message(AiRole::Assistant, text),
        }
    }

    /// 流式失败：清掉流式状态；末尾为空 assistant 占位时删除它（避免留下空白气泡），
    /// 再追加一条 system 错误消息。已经收到的部分文本会保留（引擎只会在没有文本时
    /// 回 `AiChatFailed`，这里仍按「保留已有内容」处理，防御未来改协议）。
    pub fn fail_chat_stream(&mut self, error: &str) {
        self.streaming = false;
        self.chat_pending = false;
        self.drop_empty_assistant_tail();
        self.push_message(AiRole::System, format!("AI 回复失败：{error}"));
    }

    /// 中止流式输出但不展示错误（解析新页面等切换上下文的场景）。
    ///
    /// 迟到的引擎事件会被 `request_id` 过滤掉；这里同步收尾状态，避免 `chat_pending`
    /// / `streaming` 与 UI 长期不一致。
    pub fn abort_chat_stream(&mut self) {
        self.streaming = false;
        self.chat_pending = false;
        self.drop_empty_assistant_tail();
    }

    /// 删除末尾「没有内容」的 assistant 占位消息（流式开始后、任何 delta 之前的状态）。
    fn drop_empty_assistant_tail(&mut self) {
        if self
            .messages
            .last()
            .is_some_and(|message| message.role == AiRole::Assistant && message.text.is_empty())
        {
            self.messages.pop();
        }
    }
}

// ---------------------------------------------------------------------------
// P4b-a：解析凭据的 host 归一化（`docs/architecture.md` §11.12）
// ---------------------------------------------------------------------------

/// URL → 归一化 host（小写、去尾点、不含端口）；非 http(s)/无 host → `None`。
///
/// 这是「从 URL 取 host」的**唯一权威实现**（解析凭据归属、AI 账本来源 host 共用），
/// 避免各视图各写一份大小写/尾点规则导致同一 URL 在不同地方得到不同 host。
pub fn url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?.trim();
    (!host.is_empty()).then(|| normalize_host(host))
}

/// 解析凭据归属用的 host 比较键：与 [`url_host`] 同一归一化规则。
fn normalized_url_host(url: &str) -> Option<String> {
    url_host(url)
}

/// host 归一化：trim + 小写 + 去一个尾点（与 `rdm-resolve::HostCredentials` 一致）。
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn non_empty_credential(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// 从解析请求提取内存凭据；无 host / 三个字段全空 → `None`。
fn credential_from_request(request: &rdm_ai::ParseRequest) -> Option<ParseCredential> {
    let host = normalized_url_host(&request.url)?;
    let cookie = non_empty_credential(request.cookie.as_deref());
    let referer = non_empty_credential(request.referer.as_deref());
    let user_agent = non_empty_credential(request.user_agent.as_deref());
    if cookie.is_none() && referer.is_none() && user_agent.is_none() {
        return None;
    }
    Some(ParseCredential {
        host,
        cookie,
        referer,
        user_agent,
    })
}

// ---------------------------------------------------------------------------
// URL / 文件名
// ---------------------------------------------------------------------------

/// 只接受 http(s)，拒绝 `file://`、空白字符与非法 URL。
///
/// 审计 F-04：与 core/native/CLI/MCP 一致，使用 `url::Url::parse` 做真实解析校验
/// （端口越界、非法 percent-encoding、空 host 都在输入阶段拒绝），而不是只看前缀；
/// 返回值保持「trim 后的原始文本」（不重写用户 URL 的大小写/路径）。
pub fn validate_url(input: &str) -> Result<String, String> {
    let url = input.trim();
    if url.is_empty() {
        return Err("请输入下载 URL".to_string());
    }
    if url.chars().any(char::is_whitespace) {
        return Err("URL 不能包含空白字符".to_string());
    }
    let parsed = url::Url::parse(url).map_err(|err| format!("URL 格式无效：{err}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("仅支持 http(s) URL".to_string()),
    }
    if parsed.host_str().is_none_or(str::is_empty) {
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
    /// 本次下载实际达到的峰值并发 worker 数（来自 `Concurrency` 事件 / 最终 summary）。
    pub peak_connections: usize,
    /// 动态线程规划当前目标（`Concurrency.target`；0 = 引擎还没报告）。
    pub target_connections: usize,
    pub path: Option<PathBuf>,
    /// 分类保存后的实际目录；持久化并在「继续下载/重试」时复用。
    pub save_dir: Option<PathBuf>,
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
    /// 已请求、等待引擎确认的停止动作。
    pub stop_request: StopRequest,
    /// AI 成本归属（解析/分类 token）；从未使用 AI 的任务为 `None`。
    pub ai: Option<ItemAiUsage>,
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
            peak_connections: 0,
            target_connections: 0,
            path: None,
            save_dir: None,
            error: None,
            note: None,
            source: source.into(),
            created_ms: now_ms(),
            speed_history: VecDeque::with_capacity(SPEED_HISTORY_LEN),
            range_supported: None,
            sha256: None,
            run_id: 0,
            stop_request: StopRequest::None,
            ai: None,
        }
    }

    pub fn progress_fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => {
                Some((self.downloaded as f64 / total as f64).clamp(0.0, 1.0))
            }
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

    /// 已下载/总量（暂停行用；总数未知时只显示已下载）。
    pub fn downloaded_text(&self) -> String {
        match self.total {
            Some(total) if total > 0 => {
                format!(
                    "{} / {}",
                    format_bytes(self.downloaded),
                    format_bytes(total)
                )
            }
            _ => format_bytes(self.downloaded),
        }
    }

    pub fn speed_text(&self) -> String {
        format_speed(self.speed_bps)
    }

    pub fn eta_text(&self) -> String {
        format_eta(self.eta_secs)
    }

    /// 连接数展示。
    ///
    /// - 不支持 Range 的服务器固定「单连接」；
    /// - 动态规划运行中：`"{active} / {target} · 上限 {max}"`；
    /// - 其余情况保持旧文案（`"{n} 连接"`，下载中优先展示活动 worker 数）。
    pub fn connections_text(&self) -> String {
        if self.range_supported == Some(false) {
            return "单连接".to_string();
        }
        if self.state.is_active() && self.target_connections > 0 {
            return format!(
                "{} / {} · 上限 {}",
                self.active_workers, self.target_connections, self.request.connections
            );
        }
        let n = if self.state.is_active() && self.active_workers > 0 {
            self.active_workers
        } else {
            self.request.connections
        };
        format!("{n} 连接")
    }

    pub fn push_speed_sample(&mut self, value: f64) {
        self.speed_history.push_back(if value.is_finite() {
            value.max(0.0)
        } else {
            0.0
        });
        while self.speed_history.len() > SPEED_HISTORY_LEN {
            self.speed_history.pop_front();
        }
    }

    pub fn clear_speed(&mut self) {
        self.speed_bps = 0.0;
        self.eta_secs = None;
        self.active_workers = 0;
    }

    /// 请求停止（暂停/取消）。
    ///
    /// 只有「下载中且没有待确认停止动作」时生效；返回 false 表示忽略重复请求。
    /// 状态保持 `Downloading`，等引擎回 `Paused/Cancelled` 后再迁移（见 `StopRequest`）。
    pub fn begin_stop(&mut self, request: StopRequest) -> bool {
        if self.state != ItemState::Downloading || !self.stop_request.is_none() {
            return false;
        }
        self.stop_request = request;
        self.note = request.label_zh().map(|label| format!("{label}…"));
        true
    }

    /// 引擎确认停止后清理停止标记。
    fn finish_stop(&mut self) {
        self.stop_request = StopRequest::None;
    }

    /// 应用一条引擎事件（仅当 `run_id` 匹配时由调用方调用）。
    pub fn apply_event(&mut self, event: DownloadEvent) {
        let stopping = !self.stop_request.is_none();
        match event {
            DownloadEvent::Probing => {
                if !stopping {
                    self.note = Some("正在探测服务器…".to_string());
                }
            }
            DownloadEvent::Metadata(meta) => {
                // `request.file_name` 是用户显式指定，不能被服务器推导名覆盖。
                if self.request.file_name.is_none() {
                    self.file_name = meta.file_name;
                }
                self.total = meta.total_bytes;
                self.range_supported = Some(meta.supports_ranges);
                if !meta.supports_ranges && !stopping {
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
                if !stopping {
                    self.note = if resumed_chunks > 0 {
                        Some(format!(
                            "断点续传：已完成 {resumed_chunks}/{total_chunks} 块"
                        ))
                    } else {
                        None
                    };
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
                    if !stopping {
                        self.note = None;
                    }
                } else {
                    // 暂停/取消确认后仍可能收到最后一帧进度：只回填字节数。
                    self.downloaded = self.downloaded.max(downloaded);
                }
            }
            DownloadEvent::Retrying {
                chunk: _,
                attempt,
                delay_ms,
                reason,
            } => {
                if !stopping {
                    self.note = Some(format!(
                        "第 {attempt} 次重试（{}s）：{reason}",
                        (delay_ms / 1000).max(1)
                    ));
                }
            }
            DownloadEvent::Concurrency { target, active } => {
                // 动态线程规划的目标/活跃数变化。
                self.target_connections = target;
                self.active_workers = active;
                self.peak_connections = self.peak_connections.max(active);
            }
            DownloadEvent::Completed { .. } => {
                // 最终结果由 `apply_result` 收敛，这里只清状态提示。
                if !stopping {
                    self.note = None;
                }
            }
            DownloadEvent::Paused { downloaded } => {
                if self.state != ItemState::Cancelled {
                    self.state = ItemState::Paused;
                    self.downloaded = self.downloaded.max(downloaded);
                    self.clear_speed();
                    self.error = None;
                    self.note = None;
                    self.finish_stop();
                }
            }
            DownloadEvent::Failed { error } => {
                if self.state != ItemState::Cancelled {
                    if self.stop_request == StopRequest::Cancel {
                        // 等 `Finished` 收敛成 Cancelled，先保留错误信息。
                        self.error = Some(error);
                    } else {
                        self.state = ItemState::Failed;
                        self.error = Some(error);
                        self.clear_speed();
                        self.note = None;
                        self.finish_stop();
                    }
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
                if let Some(name) = summary.path.file_name() {
                    self.file_name = name.to_string_lossy().into_owned();
                }
                self.path = Some(summary.path);
                self.error = None;
                self.note = None;
                self.range_supported = Some(summary.range_supported);
                self.peak_connections =
                    self.peak_connections.max(summary.peak_connections as usize);
                self.sha256 = summary.sha256;
                self.clear_speed();
                self.finish_stop();
            }
            Err(DownloadError::Paused { downloaded }) => {
                if self.state != ItemState::Cancelled {
                    self.state = ItemState::Paused;
                    self.downloaded = self.downloaded.max(downloaded);
                    self.note = None;
                    self.clear_speed();
                }
                self.finish_stop();
            }
            Err(DownloadError::Cancelled { downloaded }) => {
                self.state = ItemState::Cancelled;
                self.downloaded = self.downloaded.max(downloaded);
                self.error = Some("已取消".to_string());
                self.note = None;
                self.clear_speed();
                self.finish_stop();
            }
            Err(error) => {
                if self.state == ItemState::Cancelled || self.stop_request == StopRequest::Cancel {
                    // 用户已取消：引擎在探测阶段可能返回普通错误而不是 Cancelled，
                    // 保持"已取消"语义，避免 UI 把用户操作显示成失败。
                    self.state = ItemState::Cancelled;
                    self.error = Some("已取消".to_string());
                    self.note = None;
                    self.clear_speed();
                } else {
                    self.state = ItemState::Failed;
                    self.error = Some(error.to_string());
                    self.note = None;
                    self.clear_speed();
                }
                self.finish_stop();
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
            peak_connections: self.peak_connections,
            target_connections: self.target_connections,
            save_dir: self.save_dir.clone(),
            ai: self.ai.clone(),
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
            peak_connections: saved.peak_connections,
            target_connections: saved.target_connections,
            path: saved.path,
            save_dir: saved.save_dir,
            error: saved.error,
            note: None,
            source: saved.source,
            created_ms: saved.created_ms,
            speed_history: VecDeque::with_capacity(SPEED_HISTORY_LEN),
            range_supported: saved.range_supported,
            sha256: saved.sha256,
            run_id: 0,
            stop_request: StopRequest::None,
            ai: saved.ai,
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
    let total: f64 = items
        .iter()
        .filter(|item| item.state.is_active())
        .map(|item| item.speed_bps)
        .filter(|value| value.is_finite() && *value > 0.0)
        .sum();
    // 归一化 -0.0 / NaN，避免 IPC JSON 里出现 "-0.0"。
    if total.is_finite() && total > 0.0 {
        total
    } else {
        0.0
    }
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
    /// 峰值并发（旧文件缺省 = 0）。
    #[serde(default)]
    pub peak_connections: usize,
    /// 动态规划目标（旧文件缺省 = 0）。
    #[serde(default)]
    pub target_connections: usize,
    /// 分类保存目录（旧文件缺省 = None）。
    #[serde(default)]
    pub save_dir: Option<PathBuf>,
    /// AI 成本归属（旧文件缺省 = None，表示从未使用 AI）。
    #[serde(default)]
    pub ai: Option<ItemAiUsage>,
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

pub fn save_downloads(
    state_dir: &Path,
    items: &[DownloadItem],
    next_seq: u64,
) -> std::io::Result<()> {
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
            peak_connections: 8,
            controller: None,
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
    fn pause_waits_for_engine_confirmation() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_event(DownloadEvent::Progress {
            downloaded: 100,
            total: Some(1_000),
            speed_bps: 10.0,
            eta_secs: Some(3.0),
            active_workers: 4,
        });
        assert!(item.begin_stop(StopRequest::Pause));
        assert_eq!(item.state, ItemState::Downloading, "暂停请求不能立刻改状态");
        assert_eq!(item.stop_request, StopRequest::Pause);
        assert!(
            !item.begin_stop(StopRequest::Cancel),
            "重复停止请求应被忽略"
        );

        // 引擎确认后 > 迁移到 Paused，并清停止标记。
        item.apply_event(DownloadEvent::Paused { downloaded: 700 });
        assert_eq!(item.state, ItemState::Paused);
        assert_eq!(item.downloaded, 700);
        assert!(item.stop_request.is_none());
        assert!(
            !item.begin_stop(StopRequest::Pause),
            "已暂停任务不再接受停止请求"
        );
    }

    #[test]
    fn cancel_pending_maps_late_engine_error_to_cancelled() {
        let mut item = item_in_state(1, ItemState::Downloading);
        assert!(item.begin_stop(StopRequest::Cancel));
        item.apply_result(Err(DownloadError::Http("probe aborted".to_string())));
        assert_eq!(item.state, ItemState::Cancelled);
        assert!(item.stop_request.is_none());
        assert_eq!(item.error.as_deref(), Some("已取消"));
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
    fn cancel_wins_over_late_non_control_flow_error() {
        // 用户在探测阶段取消时，引擎可能先返回普通错误；UI 应保持「已取消」。
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_result(Err(DownloadError::Cancelled { downloaded: 0 }));
        item.apply_result(Err(DownloadError::Http(
            "probe aborted by cancellation".to_string(),
        )));
        assert_eq!(item.state, ItemState::Cancelled);
        assert_eq!(item.error.as_deref(), Some("已取消"));
    }

    #[test]
    fn prepare_request_applies_defaults_and_clamps() {
        let settings = Settings {
            default_connections: 4,
            default_dir: Some(PathBuf::from("D:/downloads")),
            speed_limit_bps: Some(1_048_576),
            verify_sha256: false,
            theme: Theme::Dark,
            auto_start: true,
            dynamic_connections: false,
            save_by_extension: true,
            classification: rdm_ai::default_category_rules(),
            launch_at_startup: false,
            minimize_to_tray: false,
            silent_start: false,
            ui_theme: None,
            accent: AccentPreset::default(),
            density: Density::default(),
            reduce_motion: false,
        };

        // URL 校验 + 连接数/动态规划跟随 GUI 设置 + 默认目录/限速补齐。
        let mut request = DownloadRequest::new("  https://example.com/a.bin  ");
        request.connections = 999; // 扩展/宿主的旧值会被 GUI 设置覆盖
        request.dynamic_connections = true;
        let prepared = prepare_request(&settings, request).unwrap();
        assert_eq!(prepared.url, "https://example.com/a.bin");
        assert_eq!(prepared.connections, 4, "连接数应为 GUI 设置值");
        assert!(!prepared.dynamic_connections, "动态规划应跟随 GUI 设置");
        assert_eq!(prepared.output_dir, Some(PathBuf::from("D:/downloads")));
        assert_eq!(prepared.speed_limit_bps, Some(1_048_576));

        // 显式传的目录/限速不被覆盖。
        let mut request = DownloadRequest::new("https://example.com/b.bin");
        request.output_dir = Some(PathBuf::from("C:/custom"));
        request.speed_limit_bps = Some(2_097_152);
        let prepared = prepare_request(&settings, request).unwrap();
        assert_eq!(prepared.output_dir, Some(PathBuf::from("C:/custom")));
        assert_eq!(prepared.speed_limit_bps, Some(2_097_152));

        // 非法 URL 拒绝。
        assert!(prepare_request(&settings, DownloadRequest::new("ftp://x/a")).is_err());
    }

    /// 回归：浏览器扩展/宿主传来的 connections 不得覆盖 GUI 的「默认连接数」
    /// （用户报障：GUI 设 16、扩展 popup 显示上限 8）。
    #[test]
    fn prepare_request_uses_gui_connections_over_handoff_value() {
        let mut settings = Settings {
            default_connections: 16,
            dynamic_connections: false,
            ..Default::default()
        };

        let mut request = DownloadRequest::new("https://example.com/a.bin");
        request.connections = 8; // 旧扩展默认值 / 宿主缺省值
        request.dynamic_connections = true;
        let prepared = prepare_request(&settings, request).unwrap();
        assert_eq!(prepared.connections, 16);
        assert!(!prepared.dynamic_connections);

        // 设置值越界时仍收敛到 1..=MAX_CONNECTIONS。
        settings.default_connections = 0;
        let prepared =
            prepare_request(&settings, DownloadRequest::new("https://example.com/b.bin")).unwrap();
        assert_eq!(prepared.connections, 1);
        settings.default_connections = 999;
        let prepared =
            prepare_request(&settings, DownloadRequest::new("https://example.com/c.bin")).unwrap();
        assert_eq!(prepared.connections, rdm_core::MAX_CONNECTIONS);
    }

    #[test]
    fn completed_result_sets_path_and_hash() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_result(Ok(sample_summary()));
        assert_eq!(item.state, ItemState::Completed);
        assert_eq!(item.total, Some(1_000));
        assert_eq!(item.downloaded, 1_000);
        assert_eq!(
            item.path.as_deref(),
            Some(Path::new("C:/downloads/archive.zip"))
        );
        assert_eq!(item.file_name, "archive.zip");
        assert_eq!(item.sha256.as_deref(), Some("deadbeef"));
        assert_eq!(item.progress_fraction(), Some(1.0));
    }

    #[test]
    fn explicit_file_name_wins_over_server_metadata() {
        let mut request = sample_request();
        request.file_name = Some("custom-name.bin".to_string());
        let mut item = DownloadItem::new(next_download_id(2), request, "test");
        item.apply_event(DownloadEvent::Metadata(rdm_core::RemoteMeta {
            final_url: "https://example.com/original".to_string(),
            file_name: "server-name.bin".to_string(),
            total_bytes: Some(10),
            supports_ranges: true,
            content_type: None,
            etag: None,
            last_modified: None,
            http_version: None,
        }));
        assert_eq!(item.file_name, "custom-name.bin");
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

    #[test]
    fn concurrency_events_track_target_and_peak() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_event(DownloadEvent::Concurrency {
            target: 4,
            active: 2,
        });
        assert_eq!(item.target_connections, 4);
        assert_eq!(item.active_workers, 2);
        assert_eq!(item.peak_connections, 2);
        assert_eq!(item.connections_text(), "2 / 4 · 上限 8");

        item.apply_event(DownloadEvent::Concurrency {
            target: 6,
            active: 6,
        });
        assert_eq!(item.peak_connections, 6);

        // 回退目标不降低历史峰值。
        item.apply_event(DownloadEvent::Concurrency {
            target: 3,
            active: 3,
        });
        assert_eq!(item.peak_connections, 6);
        assert_eq!(item.connections_text(), "3 / 3 · 上限 8");

        // 暂停后回到旧文案（展示请求配置）。
        item.apply_result(Err(DownloadError::Paused { downloaded: 10 }));
        assert_eq!(item.connections_text(), "8 连接");
    }

    #[test]
    fn summary_peak_connections_is_merged() {
        let mut item = item_in_state(1, ItemState::Downloading);
        item.apply_event(DownloadEvent::Concurrency {
            target: 3,
            active: 3,
        });
        let mut summary = sample_summary();
        summary.peak_connections = 6;
        item.apply_result(Ok(summary));
        assert_eq!(item.peak_connections, 6);
        assert_eq!(item.state, ItemState::Completed);
        // 完成后不再展示动态文案。
        assert_eq!(item.connections_text(), "8 连接");
    }

    // --- 设置 ---

    #[test]
    fn speed_limit_roundtrip() {
        assert_eq!(parse_speed_limit_mb("").unwrap(), None);
        assert_eq!(parse_speed_limit_mb("0").unwrap(), None);
        assert_eq!(parse_speed_limit_mb("2").unwrap(), Some(2 * 1024 * 1024));
        assert_eq!(parse_speed_limit_mb("1.5").unwrap(), Some(1_572_864));
        assert!(parse_speed_limit_mb("abc").is_err());
        assert!(parse_speed_limit_mb("-1").is_err());
        let mut settings = Settings {
            speed_limit_bps: Some(2 * 1024 * 1024),
            ..Default::default()
        };
        assert_eq!(settings.speed_limit_mb_text(), "2");
        settings.speed_limit_bps = Some(1_572_864);
        assert_eq!(settings.speed_limit_mb_text(), "1.5");
        assert_eq!(format_limit(&settings), "1.5 MB/s");
        settings.speed_limit_bps = None;
        assert_eq!(format_limit(&settings), "不限速");
    }

    #[test]
    fn connections_normalization() {
        let mut settings = Settings {
            default_connections: 999,
            ..Default::default()
        };
        let normalized = settings.normalized();
        assert_eq!(normalized.default_connections, rdm_core::MAX_CONNECTIONS);
        let request = normalized.request_with_defaults("https://example.com/a.bin");
        assert_eq!(request.connections, rdm_core::MAX_CONNECTIONS);
        settings.default_connections = 0;
        assert_eq!(settings.normalized().default_connections, 1);
    }

    #[test]
    fn new_setting_flags_default_true_and_feed_request() {
        let settings = Settings::default();
        assert!(settings.dynamic_connections);
        assert!(settings.save_by_extension);
        assert_eq!(
            settings.classification,
            rdm_ai::default_category_rules(),
            "默认分类规则来自 rdm-ai 唯一事实来源"
        );
        assert!(
            settings
                .request_with_defaults("https://example.com/a.bin")
                .dynamic_connections
        );

        let mut settings = settings;
        settings.dynamic_connections = false;
        assert!(
            !settings
                .request_with_defaults("https://example.com/a.bin")
                .dynamic_connections
        );
        // normalized 只收敛数值，不重置开关。
        assert!(!settings.normalized().dynamic_connections);
    }

    #[test]
    fn old_settings_json_uses_new_flag_defaults() {
        // 旧 JSON 里的 `ai_classify` 字段（已删除）必须被 serde 忽略而不是报错。
        let old = r#"{"default_connections":4,"speed_limit_bps":null,"verify_sha256":true,
            "theme":"light","auto_start":false,"ai_classify":false}"#;
        let settings: Settings = serde_json::from_str(old).unwrap();
        assert!(settings.dynamic_connections);
        assert!(settings.save_by_extension);
        assert_eq!(
            settings.classification,
            rdm_ai::default_category_rules(),
            "旧 JSON 没有 classification 时必须回落到内置规则"
        );
        assert_eq!(settings.theme, Theme::Light);
        assert!(!settings.auto_start);
    }

    #[test]
    fn parses_connection_input() {
        assert_eq!(parse_connections("8").unwrap(), 8);
        assert_eq!(parse_connections(" 1 ").unwrap(), 1);
        assert_eq!(
            parse_connections(&rdm_core::MAX_CONNECTIONS.to_string()).unwrap(),
            rdm_core::MAX_CONNECTIONS
        );
        assert!(parse_connections("0").is_err());
        assert!(parse_connections("999").is_err());
        assert!(parse_connections("").is_err());
        assert!(parse_connections("abc").is_err());
        assert!(parse_connections("-1").is_err());
    }

    #[test]
    fn theme_labels() {
        assert_eq!(Theme::Dark.label_zh(), "深色");
        assert_eq!(Theme::Light.label_zh(), "浅色");
        assert_eq!(Theme::CatppuccinLatte.label_zh(), "Catppuccin Latte");
        assert_eq!(Theme::CatppuccinFrappe.label_zh(), "Catppuccin Frappe");
        assert_eq!(
            Theme::CatppuccinMacchiato.label_zh(),
            "Catppuccin Macchiato"
        );
        assert_eq!(Theme::CatppuccinMocha.label_zh(), "Catppuccin Mocha");
        assert_eq!(Theme::ALL.len(), 6);
        assert_eq!(
            Theme::ALL.map(Theme::label_zh),
            [
                "深色",
                "浅色",
                "Catppuccin Latte",
                "Catppuccin Frappe",
                "Catppuccin Macchiato",
                "Catppuccin Mocha",
            ]
        );
    }

    #[test]
    fn theme_serde_names_are_stable() {
        assert_eq!(
            serde_json::to_string(&Theme::CatppuccinLatte).unwrap(),
            "\"catppuccin_latte\""
        );
        assert_eq!(
            serde_json::to_string(&Theme::CatppuccinFrappe).unwrap(),
            "\"catppuccin_frappe\""
        );
        assert_eq!(
            serde_json::to_string(&Theme::CatppuccinMacchiato).unwrap(),
            "\"catppuccin_macchiato\""
        );
        assert_eq!(
            serde_json::to_string(&Theme::CatppuccinMocha).unwrap(),
            "\"catppuccin_mocha\""
        );
        // 旧值不变。
        assert_eq!(serde_json::to_string(&Theme::Dark).unwrap(), "\"dark\"");
        assert_eq!(serde_json::to_string(&Theme::Light).unwrap(), "\"light\"");
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
        state
            .items
            .push(item_in_state(2, ItemState::Paused).to_persisted());
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
        let mut classification = rdm_ai::default_category_rules();
        classification[0].extensions = vec!["zzz".to_owned()];
        let settings = Settings {
            default_connections: 4,
            default_dir: Some(dir.join("dl")),
            speed_limit_bps: Some(1_048_576),
            verify_sha256: true,
            theme: Theme::Light,
            auto_start: false,
            dynamic_connections: false,
            save_by_extension: false,
            classification,
            launch_at_startup: true,
            minimize_to_tray: true,
            silent_start: false,
            ui_theme: None,
            accent: AccentPreset::default(),
            density: Density::default(),
            reduce_motion: false,
        };
        save_settings(&dir, &settings).unwrap();
        let loaded = load_settings(&dir);
        assert_eq!(loaded.default_connections, 4);
        assert_eq!(loaded.default_dir, settings.default_dir);
        assert_eq!(loaded.speed_limit_bps, Some(1_048_576));
        assert!(loaded.verify_sha256);
        assert_eq!(loaded.theme, Theme::Light);
        assert!(!loaded.auto_start);
        assert!(!loaded.dynamic_connections);
        assert!(!loaded.save_by_extension);
        // 自定义分类规则随 settings.json 持久化（保持全部 kind，other 兜底）。
        assert_eq!(loaded.classification.len(), rdm_ai::CATEGORY_KINDS.len());
        assert_eq!(loaded.classification[0].kind, "video");
        assert_eq!(loaded.classification[0].extensions, vec!["zzz".to_owned()]);
        assert_eq!(
            loaded.classification.last().map(|rule| rule.kind.as_str()),
            Some("other")
        );
        assert!(loaded.launch_at_startup);
        assert!(loaded.minimize_to_tray);
        assert!(!loaded.silent_start);
        assert!(loaded.needs_tray());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn classification_normalization_sanitizes_and_keeps_every_kind() {
        let settings = Settings {
            classification: vec![rdm_ai::CategoryRule {
                kind: "video".to_owned(),
                label: "视频".to_owned(),
                extensions: vec![
                    ".MP4".to_owned(),
                    " mp4 ".to_owned(),
                    "bad-char".to_owned(),
                    String::new(),
                ],
            }],
            ..Settings::default()
        };
        let normalized = settings.normalized();
        assert_eq!(
            normalized.classification.len(),
            rdm_ai::CATEGORY_KINDS.len()
        );
        assert_eq!(normalized.classification[0].extensions, vec!["mp4"]);
        // 空/损坏的规则列表整体回落默认，而不是让所有下载都进根目录。
        let broken = Settings {
            classification: Vec::new(),
            ..Settings::default()
        };
        assert_eq!(
            broken.normalized().classification,
            rdm_ai::default_category_rules()
        );
    }

    #[test]
    fn classification_snapshot_reads_the_active_rules() {
        // 读取默认快照（测试不修改全局快照，避免与并行测试互相影响）。
        assert_eq!(category_kind_for_file("movie.mp4"), "video");
        assert_eq!(category_kind_for_file("song.mp3"), "audio");
        assert_eq!(category_kind_for_file("data.json"), "document");
        assert_eq!(category_kind_for_file("unknown.zzz"), "other");
        assert_eq!(category_for_file("movie.mp4"), "视频");
        assert_eq!(category_for_file("unknown.zzz"), rdm_ai::CATEGORY_OTHER);
        assert!(!classification_rules().is_empty());
    }

    #[test]
    fn old_settings_json_defaults_new_system_integration_fields() {
        // 旧版 settings.json 没有三个新字段：反序列化必须回落到默认值（全关）。
        let dir = temp_dir("settings-old");
        let path = settings_path(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"theme":"light","auto_start":false,"default_connections":6}"#,
        )
        .unwrap();
        let loaded = load_settings(&dir);
        assert_eq!(loaded.default_connections, 6);
        assert!(!loaded.auto_start);
        assert!(!loaded.launch_at_startup);
        assert!(!loaded.minimize_to_tray);
        assert!(!loaded.silent_start);
        assert!(!loaded.needs_tray());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn needs_tray_and_effective_silent_start() {
        let mut settings = Settings::default();
        assert!(!settings.needs_tray());
        assert!(!settings.effective_silent_start(false));

        settings.minimize_to_tray = true;
        assert!(settings.needs_tray());
        assert!(!settings.effective_silent_start(false));

        settings.minimize_to_tray = false;
        settings.silent_start = true;
        assert!(settings.needs_tray());
        assert!(settings.effective_silent_start(false));
        assert!(settings.effective_silent_start(true));

        // 仅开机自启动不需要托盘。
        settings.silent_start = false;
        settings.launch_at_startup = true;
        assert!(!settings.needs_tray());
        assert!(settings.effective_silent_start(true));
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

    #[test]
    fn persisted_new_fields_roundtrip_and_backward_compatible() {
        let mut item = item_in_state(1, ItemState::Paused);
        item.peak_connections = 5;
        item.target_connections = 4;
        item.save_dir = Some(PathBuf::from("C:/downloads/视频"));
        let saved = item.to_persisted();
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains("\"peak_connections\":5"));
        let restored = DownloadItem::from_persisted(serde_json::from_str(&json).unwrap());
        assert_eq!(restored.peak_connections, 5);
        assert_eq!(restored.target_connections, 4);
        assert_eq!(restored.save_dir, Some(PathBuf::from("C:/downloads/视频")));

        // 旧 downloads.json 没有新字段 → 默认 0/None，不报错。
        let old = r#"{"id":"dl_000009","file_name":"a.bin",
            "url":"https://example.com/a.bin",
            "request":{"url":"https://example.com/a.bin"},"state":"paused"}"#;
        let saved: PersistedItem = serde_json::from_str(old).unwrap();
        assert_eq!(saved.peak_connections, 0);
        assert_eq!(saved.target_connections, 0);
        assert!(saved.save_dir.is_none());
        let item = DownloadItem::from_persisted(saved);
        assert_eq!(item.state, ItemState::Paused);
    }

    // --- AI 面板 ---

    fn ai_resource(name: &str) -> rdm_ai::ParsedResource {
        rdm_ai::ParsedResource {
            url: format!("https://example.com/{name}"),
            name: name.to_string(),
            kind: "video".to_string(),
            size_bytes: Some(1024),
            content_type: Some("video/mp4".to_string()),
            note: None,
        }
    }

    fn ai_report() -> rdm_ai::ParseReport {
        rdm_ai::ParseReport {
            source_url: "https://example.com/page".to_string(),
            source_kind: "html_page".to_string(),
            title: Some("示例页面".to_string()),
            resources: vec![ai_resource("a.mp4"), ai_resource("b.mp4")],
            notes: vec!["整理出 2 个视频资源".to_string()],
            ai_used: true,
            model: Some("gpt-4o-mini".to_string()),
            usage: rdm_ai::TokenUsage {
                prompt_tokens: 1200,
                completion_tokens: 300,
                total_tokens: 1500,
                calls: 1,
                estimated: false,
            },
            created_ms: 1,
            cache_hit: false,
            failure_kind: None,
        }
    }

    #[test]
    fn ai_panel_parse_lifecycle() {
        let mut panel = AiPanelState::default();
        assert_eq!(panel.status, AiParseStatus::Idle);
        assert!(!panel.visible);

        panel.begin_parse("https://example.com/page", 7);
        assert!(panel.visible);
        assert_eq!(panel.status, AiParseStatus::Parsing);
        assert_eq!(panel.request_id, 7);
        assert_eq!(panel.source_url, "https://example.com/page");
        assert!(panel.resources.is_empty());
        assert!(panel.error.is_none());
        assert!(panel.usage.is_zero());

        panel.apply_report(ai_report());
        assert_eq!(panel.status, AiParseStatus::Ready);
        assert_eq!(panel.resources.len(), 2);
        assert_eq!(panel.selected_count(), 2);
        assert_eq!(panel.notes, vec!["整理出 2 个视频资源".to_string()]);
        assert!(panel.ai_used);
        assert!(!panel.cache_hit);
        assert_eq!(panel.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(panel.usage.total_tokens, 1_500);
        assert!(panel.error.is_none());
    }

    #[test]
    fn item_ai_usage_totals_parse_and_classify() {
        let usage = ItemAiUsage {
            parse: rdm_ai::TokenUsage {
                prompt_tokens: 900,
                completion_tokens: 300,
                total_tokens: 1_200,
                calls: 1,
                estimated: false,
            },
            classify: rdm_ai::TokenUsage {
                prompt_tokens: 80,
                completion_tokens: 20,
                total_tokens: 100,
                calls: 1,
                estimated: true,
            },
            parse_cost_cny: 0.012,
            classify_cost_cny: 0.003,
            model: Some("mock".to_owned()),
            cache_hit: false,
            parse_siblings: 2,
            updated_ms: 1,
        };
        assert_eq!(usage.total().total_tokens, 1_300);
        assert_eq!(usage.total().calls, 2);
        assert!(usage.has_tokens());
        assert!(usage.estimated());
        assert!((usage.cost_cny() - 0.015).abs() < f64::EPSILON);
        assert!(!ItemAiUsage::default().has_tokens());
        assert!(ItemAiUsage::default().total().is_zero());
        assert_eq!(ItemAiUsage::default().cost_cny(), 0.0);
    }

    #[test]
    fn old_persisted_items_default_to_no_ai_usage() {
        // 旧 downloads.json（没有 ai 字段）必须能反序列化。
        let json = r#"{
            "id": "dl_000001",
            "request": {"url": "https://example.com/a.mp4"},
            "state": "completed"
        }"#;
        let saved: PersistedItem = serde_json::from_str(json).expect("legacy item");
        assert!(saved.ai.is_none());
        let item = DownloadItem::from_persisted(saved);
        assert!(item.ai.is_none());
    }

    #[test]
    fn persisted_item_round_trips_ai_usage() {
        let mut item = item_in_state(1, ItemState::Completed);
        item.ai = Some(ItemAiUsage {
            parse: rdm_ai::TokenUsage::single(90, 10),
            classify: rdm_ai::TokenUsage::single(9, 1),
            parse_cost_cny: 0.002,
            classify_cost_cny: 0.0004,
            model: Some("mock".to_owned()),
            cache_hit: true,
            parse_siblings: 3,
            updated_ms: 42,
        });
        let saved = item.to_persisted();
        let restored = DownloadItem::from_persisted(saved);
        let ai = restored.ai.expect("ai usage survives persistence");
        assert_eq!(ai.parse.total_tokens, 100);
        assert_eq!(ai.classify.total_tokens, 10);
        assert_eq!(ai.parse_siblings, 3);
        assert!(ai.cache_hit);
    }

    #[test]
    fn ai_panel_selection_helpers() {
        let mut panel = AiPanelState::default();
        panel.apply_report(ai_report());
        panel.toggle_resource(0);
        assert_eq!(panel.selected_indices(), vec![1]);
        assert_eq!(panel.selected_count(), 1);
        // 越界忽略。
        panel.toggle_resource(99);
        panel.select_all(false);
        assert_eq!(panel.selected_count(), 0);
        assert!(panel.selected_indices().is_empty());
        panel.select_all(true);
        assert_eq!(panel.selected_indices(), vec![0, 1]);
    }

    #[test]
    fn ai_panel_error_and_messages() {
        let mut panel = AiPanelState::default();
        panel.begin_parse("https://example.com/x", 1);
        panel.apply_parse_error("无法访问页面".to_string());
        assert_eq!(panel.status, AiParseStatus::Failed);
        assert_eq!(panel.error.as_deref(), Some("无法访问页面"));
        assert!(panel.resources.is_empty());

        assert_eq!(AiRole::User.label_zh(), "我");
        panel.push_message(AiRole::User, "这个页面有视频吗？");
        panel.push_message(AiRole::Assistant, String::from("有 2 个 mp4。"));
        assert_eq!(panel.messages.len(), 2);
        assert_eq!(panel.messages[0].role, AiRole::User);
        assert!(panel.messages[0].created_ms > 0);
        assert_eq!(panel.messages[1].text, "有 2 个 mp4。");
        assert_eq!(panel.messages[1].role, AiRole::Assistant);
    }

    #[test]
    fn ai_panel_reparse_keeps_the_richer_previous_resources() {
        // Design 17 §2.5：重新解析成功后按 URL 去重合并，旧的更丰富结果不丢。
        let mut panel = AiPanelState::default();
        panel.begin_parse("https://example.com/page", 1);
        panel.apply_report(ai_report());
        panel.toggle_resource(0); // 旧结果：b.mp4 未选
        let previous = panel.resources.clone();

        // 新一次 AI 只返回 1 个（与旧结果不同的）资源。
        let mut next = ai_report();
        next.resources = vec![ai_resource("c.mp4")];
        next.notes = vec!["重新解析：已跳过本地解析经验（强制 AI）".to_string()];
        panel.begin_parse("https://example.com/page", 2);
        panel.apply_report(next);
        let kept = panel.merge_preserved_resources(previous.clone());

        assert_eq!(kept, 2, "两个旧资源都未出现在新结果里，应全部保留");
        assert_eq!(panel.resources.len(), 3);
        assert_eq!(panel.resources[0].resource.name, "c.mp4");
        assert_eq!(panel.resources[1].resource.name, "a.mp4");
        assert_eq!(panel.resources[2].resource.name, "b.mp4");
        // 旧勾选状态保留：a.mp4 曾取消勾选。
        assert!(!panel.resources[1].selected);

        // 幂等：再次合并同一批旧资源不会重复追加。
        assert_eq!(panel.merge_preserved_resources(previous), 0);
        assert_eq!(panel.resources.len(), 3);
    }

    #[test]
    fn ai_panel_reparse_failure_preserves_the_previous_result() {
        // Design 17 §2.5：重新解析失败不清空已看到的结果。
        let mut panel = AiPanelState::default();
        panel.begin_parse("https://example.com/page", 1);
        panel.apply_report(ai_report());
        let preserved = panel.resources.len();

        assert!(panel.apply_reparse_error("AI 返回空结果".to_string(), preserved));
        assert_eq!(
            panel.status,
            AiParseStatus::Ready,
            "资源仍可用，状态不退回 Failed"
        );
        assert_eq!(panel.resources.len(), preserved);
        assert_eq!(panel.error.as_deref(), Some("AI 返回空结果"));
        assert!(panel
            .notes
            .first()
            .is_some_and(|note| note.contains("已保留上次解析")));

        // 没有可保留结果时返回 false，让调用方走普通失败路径。
        let mut empty = AiPanelState::default();
        assert!(!empty.apply_reparse_error("x".to_string(), 0));
    }

    #[test]
    fn ai_panel_push_note_is_bounded_and_front_loaded() {
        let mut panel = AiPanelState::default();
        for index in 0..20 {
            panel.push_note(format!("note-{index}"));
        }
        assert_eq!(panel.notes.len(), rdm_ai::parse::MAX_NOTES);
        assert_eq!(panel.notes[0], "note-19");
        // 重复 note 只保留一份，并移动到最前。
        panel.push_note("note-15");
        assert_eq!(
            panel.notes.iter().filter(|note| *note == "note-15").count(),
            1
        );
        assert_eq!(panel.notes[0], "note-15");
    }

    // ------------------------------------------------------------------
    // P4b-a: download credential hand-off (`docs/architecture.md` §11.12)
    // ------------------------------------------------------------------

    fn credential_request(
        url: &str,
        cookie: Option<&str>,
        referer: Option<&str>,
        user_agent: Option<&str>,
    ) -> rdm_ai::ParseRequest {
        rdm_ai::ParseRequest {
            url: url.to_string(),
            title: None,
            referer: referer.map(str::to_owned),
            user_agent: user_agent.map(str::to_owned),
            cookie: cookie.map(str::to_owned),
            hint: None,
        }
    }

    #[test]
    fn set_request_credential_normalizes_the_source_host() {
        let mut panel = AiPanelState::default();
        panel.set_request_credential(&credential_request(
            "https://App.Example.COM.:8443/page?q=1#frag",
            Some("  session=secret  "),
            Some("https://app.example.com/ref"),
            Some("rdm-agent/1.0"),
        ));

        let credential = panel.credential.as_ref().expect("credential");
        // 小写、去尾点、端口不进 host key。
        assert_eq!(credential.host, "app.example.com");
        assert_eq!(credential.cookie.as_deref(), Some("session=secret"));
        assert_eq!(
            credential.referer.as_deref(),
            Some("https://app.example.com/ref")
        );
        assert_eq!(credential.user_agent.as_deref(), Some("rdm-agent/1.0"));
        assert!(!credential.is_empty());
    }

    #[test]
    fn set_request_credential_ignores_invalid_or_empty_requests() {
        let mut panel = AiPanelState::default();
        // 无效 URL → 清空（新解析替换旧凭据）。
        panel.set_request_credential(&credential_request("not a url", Some("a=b"), None, None));
        assert!(panel.credential.is_none());
        // cookie/referer/UA 全为空/空白 → None。
        panel.set_request_credential(&credential_request(
            "https://example.com/x",
            Some("   "),
            Some(""),
            Some("  "),
        ));
        assert!(panel.credential.is_none());
        // 非 http(s) scheme 不接受。
        panel.set_request_credential(&credential_request(
            "file:///tmp/x",
            Some("a=b"),
            None,
            None,
        ));
        assert!(panel.credential.is_none());
        // 只有 Referer/UA（无 cookie）也是凭据。
        panel.set_request_credential(&credential_request(
            "https://example.com/x",
            None,
            Some("https://example.com/ref"),
            None,
        ));
        let credential = panel.credential.as_ref().expect("referer-only credential");
        assert!(credential.cookie.is_none());
        assert_eq!(
            credential.referer.as_deref(),
            Some("https://example.com/ref")
        );
    }

    #[test]
    fn download_credentials_match_only_the_exact_host() {
        let mut panel = AiPanelState::default();
        panel.set_request_credential(&credential_request(
            "https://App.Example.com./page",
            Some("session=1"),
            None,
            None,
        ));

        // 大小写/尾点归一化 + 端口不同仍算同一 host。
        assert!(panel
            .download_credentials_for("https://APP.example.COM:8443/files/a.bin")
            .is_some());
        // 同 host 的 http 资源也匹配（host 相同即可）。
        assert!(panel
            .download_credentials_for("http://app.example.com/a.bin")
            .is_some());
        // 子域 / 不同 host / 非法 URL / 非 http(s) → None。
        assert!(panel
            .download_credentials_for("https://cdn.example.com/files/a.bin")
            .is_none());
        assert!(panel
            .download_credentials_for("https://other.test/a.bin")
            .is_none());
        assert!(panel.download_credentials_for("not a url").is_none());
        assert!(panel
            .download_credentials_for("file:///tmp/a.bin")
            .is_none());
    }

    #[test]
    fn download_credentials_absent_without_a_credential() {
        let panel = AiPanelState::default();
        assert!(panel.credential.is_none());
        assert!(panel
            .download_credentials_for("https://example.com/a.bin")
            .is_none());

        // 直接塞一个空 ParseCredential 也视为没有凭据。
        let panel = AiPanelState {
            credential: Some(ParseCredential::default()),
            ..AiPanelState::default()
        };
        assert!(panel
            .download_credentials_for("https://example.com/a.bin")
            .is_none());
    }

    #[test]
    fn apply_credentials_fills_only_matching_hosts() {
        let mut panel = AiPanelState::default();
        panel.set_request_credential(&credential_request(
            "https://example.com/page",
            Some("session=cookie-value"),
            Some("https://example.com/ref"),
            Some("rdm-test/1.0"),
        ));

        let mut request = rdm_core::DownloadRequest::new("https://Example.com/files/a.bin");
        panel.apply_credentials("https://example.com/files/a.bin", &mut request);
        assert_eq!(request.cookie.as_deref(), Some("session=cookie-value"));
        assert_eq!(request.referer.as_deref(), Some("https://example.com/ref"));
        assert_eq!(request.user_agent.as_deref(), Some("rdm-test/1.0"));

        // 不同 host → 一个字段都不填。
        let mut other = rdm_core::DownloadRequest::new("https://cdn.example.com/a.bin");
        panel.apply_credentials("https://cdn.example.com/a.bin", &mut other);
        assert!(other.cookie.is_none());
        assert!(other.referer.is_none());
        assert!(other.user_agent.is_none());
    }

    #[test]
    fn apply_credentials_leaves_requests_untouched_without_credentials() {
        let panel = AiPanelState::default();
        let mut request = rdm_core::DownloadRequest::new("https://example.com/a.bin");
        request.user_agent = Some("existing-agent".to_string());
        panel.apply_credentials("https://example.com/a.bin", &mut request);
        assert!(request.cookie.is_none());
        assert_eq!(request.user_agent.as_deref(), Some("existing-agent"));
    }

    #[test]
    fn download_handoff_survives_prepare_request_normalization() {
        // app.rs 的「下载选中」顺序：build_request → apply_credentials → prepare_request。
        let settings = Settings::default();
        let mut panel = AiPanelState::default();
        panel.set_request_credential(&credential_request(
            "https://example.com/page",
            Some("session=flow"),
            Some("https://example.com/ref"),
            Some("rdm-flow/1.0"),
        ));

        let mut request = settings.request_with_defaults("https://example.com/files/a.bin");
        panel.apply_credentials("https://example.com/files/a.bin", &mut request);
        let request = prepare_request(&settings, request).expect("valid request");

        assert_eq!(request.cookie.as_deref(), Some("session=flow"));
        assert_eq!(request.referer.as_deref(), Some("https://example.com/ref"));
        assert_eq!(request.user_agent.as_deref(), Some("rdm-flow/1.0"));
    }

    #[test]
    fn parse_credential_debug_never_leaks_plaintext_secrets() {
        let credential = ParseCredential {
            host: "example.com".to_string(),
            cookie: Some("session=super-secret".to_string()),
            referer: Some("https://example.com/secret-ref".to_string()),
            user_agent: Some("secret-agent".to_string()),
        };
        let rendered = format!("{credential:?}");
        assert!(rendered.contains("ParseCredential"));
        assert!(rendered.contains("example.com"));
        // 只有「是否设置」可见，不出现任何明文值。
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("secret-ref"), "{rendered}");
        assert!(!rendered.contains("secret-agent"), "{rendered}");
        assert!(rendered.contains("cookie: true"), "{rendered}");
        assert!(rendered.contains("referer: true"), "{rendered}");
        assert!(rendered.contains("user_agent: true"), "{rendered}");

        // 面板 Debug（Option<ParseCredential>）同样不泄漏。
        let panel = AiPanelState {
            credential: Some(credential),
            ..AiPanelState::default()
        };
        let panel_debug = format!("{panel:?}");
        assert!(!panel_debug.contains("super-secret"), "{panel_debug}");
        assert!(!panel_debug.contains("secret-agent"), "{panel_debug}");

        // 空凭据的 Debug 只显示布尔值。
        let empty = format!("{:?}", ParseCredential::default());
        assert!(empty.contains("cookie: false"), "{empty}");
    }

    #[test]
    fn new_parse_replaces_and_failure_clears_the_credential() {
        let mut panel = AiPanelState::default();
        panel.begin_parse("https://a.example/page", 1);
        panel.set_request_credential(&credential_request(
            "https://a.example/page",
            Some("session=a"),
            None,
            None,
        ));
        assert_eq!(panel.credential.as_ref().unwrap().host, "a.example");

        // 新解析（不同 host）替换旧凭据。
        panel.begin_parse("https://b.example/page", 2);
        panel.set_request_credential(&credential_request(
            "https://b.example/page",
            None,
            Some("https://b.example/ref"),
            Some("ua"),
        ));
        let credential = panel.credential.as_ref().expect("replaced credential");
        assert_eq!(credential.host, "b.example");
        assert!(credential.cookie.is_none());

        // 解析失败 → 凭据清除，不再参与下载交接。
        panel.apply_parse_error("boom".to_string());
        assert!(panel.credential.is_none());
        assert!(panel
            .download_credentials_for("https://b.example/files/a.bin")
            .is_none());

        // 显式 clear 同样生效。
        panel.set_request_credential(&credential_request(
            "https://c.example/page",
            Some("session=c"),
            None,
            None,
        ));
        assert!(panel.credential.is_some());
        panel.clear_credential();
        assert!(panel.credential.is_none());
    }

    #[test]
    fn begin_assistant_stream_adds_only_an_assistant_placeholder() {
        let mut panel = AiPanelState::default();
        panel.begin_assistant_stream();
        assert_eq!(panel.messages.len(), 1);
        assert_eq!(panel.messages[0].role, AiRole::Assistant);
        assert!(panel.messages[0].text.is_empty());
        assert!(panel.streaming);
        assert!(panel.chat_pending);
        panel.push_chat_delta("已创建 2 个下载任务。");
        panel.finish_chat_stream("已创建 2 个下载任务。");
        assert_eq!(panel.messages.len(), 1);
        assert_eq!(panel.messages[0].text, "已创建 2 个下载任务。");
        assert!(!panel.streaming);
        assert!(!panel.chat_pending);
    }

    #[test]
    fn ai_chat_stream_assembles_deltas_in_order() {
        let mut panel = AiPanelState::default();
        assert!(!panel.streaming);
        panel.begin_chat_stream("下载哪个？");
        // 用户消息 + 空的 assistant 占位；两个「进行中」标志一起置位。
        assert_eq!(panel.messages.len(), 2);
        assert_eq!(panel.messages[0].role, AiRole::User);
        assert_eq!(panel.messages[0].text, "下载哪个？");
        assert_eq!(panel.messages[1].role, AiRole::Assistant);
        assert!(panel.messages[1].text.is_empty());
        assert!(panel.streaming);
        assert!(panel.chat_pending);

        panel.push_chat_delta("推荐");
        panel.push_chat_delta("下载");
        panel.push_chat_delta(" mp4。");
        assert_eq!(panel.messages[1].text, "推荐下载 mp4。");

        // 空 delta 忽略：不新增消息、不改文本。
        let before = panel.messages.len();
        panel.push_chat_delta("");
        assert_eq!(panel.messages.len(), before);
        assert_eq!(panel.messages[1].text, "推荐下载 mp4。");

        panel.finish_chat_stream("推荐下载 mp4。");
        assert!(!panel.streaming);
        assert!(!panel.chat_pending);
        // finish 覆盖而不是追加，消息数不变（防止丢字/重复）。
        assert_eq!(panel.messages.len(), 2);
        assert_eq!(panel.messages[1].text, "推荐下载 mp4。");
    }

    #[test]
    fn ai_chat_stream_finish_overwrites_partial_reply() {
        let mut panel = AiPanelState::default();
        panel.begin_chat_stream("q");
        panel.push_chat_delta("半截");
        panel.finish_chat_stream("完整回复");
        assert_eq!(panel.messages.len(), 2);
        assert_eq!(panel.messages[1].role, AiRole::Assistant);
        assert_eq!(panel.messages[1].text, "完整回复");
        assert!(!panel.streaming);

        // 空回复（模型没给内容）用占位文案，避免渲染出空白气泡。
        panel.begin_chat_stream("q2");
        panel.finish_chat_stream("");
        assert_eq!(panel.messages.last().unwrap().text, "（无内容）");
        assert!(!panel.streaming);
    }

    #[test]
    fn ai_chat_stream_fail_appends_system_error() {
        let mut panel = AiPanelState::default();
        panel.begin_chat_stream("q");
        panel.fail_chat_stream("HTTP 401");
        assert!(!panel.streaming);
        assert!(!panel.chat_pending);
        // 空占位被移除：只留下用户消息 + system 错误。
        assert_eq!(panel.messages.len(), 2);
        assert_eq!(panel.messages[0].role, AiRole::User);
        assert_eq!(panel.messages[1].role, AiRole::System);
        assert!(panel.messages[1].text.contains("HTTP 401"));

        // 已经有部分文本时保留 assistant 内容，错误信息单独追加。
        panel.begin_chat_stream("q2");
        panel.push_chat_delta("部分");
        panel.fail_chat_stream("超时");
        assert_eq!(panel.messages.len(), 5);
        assert_eq!(panel.messages[3].role, AiRole::Assistant);
        assert_eq!(panel.messages[3].text, "部分");
        assert_eq!(panel.messages[4].role, AiRole::System);
        assert!(panel.messages[4].text.contains("超时"));
    }

    #[test]
    fn ai_chat_abort_clears_streaming_without_error_message() {
        let mut panel = AiPanelState::default();
        panel.begin_chat_stream("q");
        panel.abort_chat_stream();
        assert!(!panel.streaming);
        assert!(!panel.chat_pending);
        // 不留下空的 assistant 占位，也不追加错误消息。
        assert_eq!(panel.messages.len(), 1);
        assert_eq!(panel.messages[0].role, AiRole::User);
    }

    #[test]
    fn ai_chat_history_keeps_at_most_limit_messages() {
        let mut panel = AiPanelState::default();
        // 150 轮问答 = 300 条，超过上限后丢最旧的 100 条。
        for index in 0..150 {
            panel.push_message(AiRole::User, format!("q{index}"));
            panel.push_message(AiRole::Assistant, format!("a{index}"));
        }
        assert_eq!(panel.messages.len(), AI_CHAT_HISTORY_LIMIT);
        assert_eq!(panel.messages[0].text, "q50");
        assert_eq!(panel.messages.last().unwrap().text, "a149");

        // 流式占位在末尾，永远不会被上限规则丢掉。
        panel.begin_chat_stream("最新问题");
        assert_eq!(panel.messages.len(), AI_CHAT_HISTORY_LIMIT);
        assert_eq!(panel.messages.last().unwrap().role, AiRole::Assistant);
        assert_eq!(panel.messages[panel.messages.len() - 2].text, "最新问题");
    }

    #[test]
    fn ai_chat_delta_without_trailing_assistant_creates_one() {
        let mut panel = AiPanelState::default();
        panel.push_message(AiRole::User, "q");
        panel.push_chat_delta("hi");
        assert_eq!(panel.messages.len(), 2);
        assert_eq!(panel.messages[1].role, AiRole::Assistant);
        assert_eq!(panel.messages[1].text, "hi");
        // `streaming` 只由 begin/finish/fail 管理，单个 delta 不臆造流式状态。
        assert!(!panel.streaming);
    }

    #[test]
    fn ai_chat_finish_without_placeholder_creates_assistant() {
        let mut panel = AiPanelState::default();
        panel.push_message(AiRole::User, "q");
        panel.finish_chat_stream("完整回复");
        assert_eq!(panel.messages.len(), 2);
        assert_eq!(panel.messages[1].role, AiRole::Assistant);
        assert_eq!(panel.messages[1].text, "完整回复");
        assert!(!panel.streaming);
    }
}
