//! 新版主窗口（M2）：三区单窗（侧栏 / 任务流 / 检查器）+ 顶栏 + 状态条。
//!
//! 设计依据：`docs/design/03-information-architecture.md`、`06-interaction-spec.md`；
//! 视觉 token：`ui/theme.rs`；组件：`ui/widgets` + `gpui-component`。
//!
//! 数据流沿用旧 GUI：引擎事件 / IPC 请求 → `EngineMsg` → 泵任务批处理 →
//! `apply_batch` 更新状态 → `cx.notify()`。渲染层不直接触碰 tokio。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    div, px, size, AnyElement, App, ClickEvent, Context, ElementId, Entity, Focusable, Hsla,
    IntoElement, MouseButton, ParentElement, Render, ScrollHandle, Styled, Subscription, Window,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::{h_flex, v_flex, Disableable, Sizable, VirtualListScrollHandle, WindowExt};
use rdm_ai::AiConfig;
use rdm_core::DownloadHandle;
use rdm_ipc::{Envelope, Request, Response};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::agent::{
    self, AgentIntent, AgentPlan, AgentPlanState, AgentTool, AgentTurn, DownloadSelection,
    StepStatus,
};
use crate::app::Bootstrap;
use crate::desktop;
use crate::engine::{EngineMsg, EngineRuntime, RoutingOptions};
use crate::ipc_server;
use crate::model::{
    self, active_count, format_limit, global_speed_bps, next_download_id, validate_url, Density,
    DownloadItem, ItemAiUsage, ItemState, ParseUsageSnapshot, Settings, StopRequest, UiThemeMode,
};
use crate::monitor::{self, AiUsageKind, AiUsageLedger, AiUsageRecord, MonitorSnapshot};
use crate::open_path;
use crate::resolve_session::{
    normalize_session_group, ResolveOrigin, ResolveStore, TitleSource, REUSE_SESSION_NOTICE,
};
use crate::ui::icons::RdmIcon;
use crate::ui::theme::{self, tokens, ThemeTokens};
use crate::ui::widgets::{
    divider, ellipsize_file_name, empty_state, icon_button, info_banner, meta_text, mono_text,
    nav_item, progress_bar, progress_bar_colored, section_label, soft_tag, sparkline, status_dot,
    status_pill, status_text, Severity,
};
use crate::views::actions::{
    ActivateFocused, ClearCompleted, ClearSearch, ClearSelection, CloseLayer, CloseSettings,
    Command, CopyFocusedLink, CycleThemeMode, DeleteFileFocused, ExtendNext, ExtendPrev,
    FocusOmnibox, OpenAddDownload, OpenCommandPalette, OpenFocusedFile, OpenFocusedFolder,
    OpenSettings, PaletteConfirm, PaletteDown, PaletteUp, PauseAll, PauseFocused, RemoveFocused,
    ResumeAll, ResumeFocused, RetryFocused, SelectAllTasks, SelectNext, SelectPrev, SelectView,
    TaskCommand, TaskCommandKind, ToggleDensity, ToggleMonitor, ToggleSelect, ToggleSidebar,
};
use crate::views::experience::{
    render_experience_page, ExperienceApplyCallback, ExperienceCallbacks, ExperienceIdCallback,
    ExperienceMergeCallback, ExperiencePageState, ExperienceSimpleCallback, ExperienceTextCallback,
    ExperienceVersionCallback, ExperienceViewProps,
};
use crate::views::monitor::{render_monitor_page, MonitorCallbacks, MonitorViewState};
use crate::views::settings::{SettingsPage, SettingsSection};

// ---------------------------------------------------------------------------
// 视图状态类型
// ---------------------------------------------------------------------------

/// 文件分类视图（03 §4.1「分类」组；按后缀归类，不依赖 AI）。
///
/// 后缀表**不在这里硬编码**：`for_file` / `matches` 读取 `model` 的进程内分类规则
/// 快照（源 = `rdm_ai::classify::default_category_rules` + 设置页自定义后缀），
/// 保证侧栏筛选、文件图标与磁盘分类目录三处一致
/// （`docs/design/16-reuse-and-simplification.md` §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCategory {
    Video,
    Audio,
    Document,
    Image,
    Archive,
    Software,
    Other,
}

impl FileCategory {
    pub const ALL: [FileCategory; 7] = [
        FileCategory::Video,
        FileCategory::Audio,
        FileCategory::Document,
        FileCategory::Image,
        FileCategory::Archive,
        FileCategory::Software,
        FileCategory::Other,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Video => "视频",
            Self::Audio => "音频",
            Self::Document => "文档",
            Self::Image => "图片",
            Self::Archive => "压缩包",
            Self::Software => "程序安装包",
            Self::Other => "其他",
        }
    }

    pub fn icon(self) -> RdmIcon {
        match self {
            Self::Video => RdmIcon::VideoFile,
            Self::Audio => RdmIcon::File,
            Self::Document => RdmIcon::TextFile,
            Self::Image => RdmIcon::ImageFile,
            Self::Archive => RdmIcon::ArchiveFile,
            Self::Software => RdmIcon::FileDown,
            Self::Other => RdmIcon::File,
        }
    }

    /// 视图分类对应的 `rdm_ai::classify::CategoryRule.kind`。
    ///
    /// 目前由 [`Self::from_kind`] 承担反向映射；保留 `kind` 供自定义分类扩展与
    /// 单测使用（编译期不产生 dead_code 噪声）。
    #[allow(dead_code)]
    pub fn kind(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Image => "image",
            Self::Archive => "archive",
            Self::Software => "program",
            Self::Other => "other",
        }
    }

    /// `kind` → 视图分类；未知 kind（手改 JSON 的自定义分类）归入「其他」。
    pub fn from_kind(kind: &str) -> Self {
        match kind {
            "video" => Self::Video,
            "audio" => Self::Audio,
            "document" => Self::Document,
            "image" => Self::Image,
            "archive" => Self::Archive,
            "program" => Self::Software,
            _ => Self::Other,
        }
    }

    /// 用当前生效的分类规则（设置页保存的自定义后缀）判断文件属于哪个分类。
    pub fn for_file(file_name: &str) -> Self {
        Self::from_kind(&model::category_kind_for_file(file_name))
    }

    pub fn matches(self, file_name: &str) -> bool {
        Self::for_file(file_name) == self
    }

    fn ordinal(self) -> u8 {
        match self {
            Self::Video => 0,
            Self::Audio => 1,
            Self::Document => 2,
            Self::Image => 3,
            Self::Archive => 4,
            Self::Software => 5,
            Self::Other => 6,
        }
    }

    fn from_ordinal(value: u8) -> Option<Self> {
        Self::ALL.get(value as usize).copied()
    }
}

/// 左侧智能视图 / 来源/分类筛选（03 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewFilter {
    #[default]
    All,
    Downloading,
    Paused,
    Completed,
    Failed,
    FromBrowser,
    FromAgent,
    Category(FileCategory),
}

impl ViewFilter {
    pub const PRIMARY: [ViewFilter; 5] = [
        ViewFilter::All,
        ViewFilter::Downloading,
        ViewFilter::Paused,
        ViewFilter::Completed,
        ViewFilter::Failed,
    ];
    pub const SOURCES: [ViewFilter; 2] = [ViewFilter::FromBrowser, ViewFilter::FromAgent];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Downloading => "下载中",
            Self::Paused => "已暂停",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::FromBrowser => "来自浏览器",
            Self::FromAgent => "来自 Agent",
            Self::Category(category) => category.label_zh(),
        }
    }

    pub fn icon(self) -> RdmIcon {
        match self {
            Self::All => RdmIcon::Density,
            Self::Downloading => RdmIcon::Download,
            Self::Paused => RdmIcon::CirclePause,
            Self::Completed => RdmIcon::CheckCircle,
            Self::Failed => RdmIcon::ErrorCircle,
            Self::FromBrowser => RdmIcon::Globe,
            Self::FromAgent => RdmIcon::Bot,
            Self::Category(category) => category.icon(),
        }
    }

    pub fn matches(self, item: &DownloadItem) -> bool {
        match self {
            Self::All => true,
            Self::Downloading => item.state == ItemState::Downloading,
            Self::Paused => item.state == ItemState::Paused,
            Self::Completed => item.state == ItemState::Completed,
            // 契约 §7 区分「失败」与「取消」：失败视图只含 Failed；已取消在「全部」中可见。
            Self::Failed => item.state == ItemState::Failed,
            Self::FromBrowser => matches!(
                item.source.as_str(),
                "browser" | "extension" | "firefox" | "chrome" | "ipc"
            ),
            Self::FromAgent => matches!(item.source.as_str(), "cli" | "agent" | "mcp"),
            Self::Category(category) => category.matches(&item.file_name),
        }
    }

    pub fn count(self, items: &[DownloadItem]) -> usize {
        items.iter().filter(|item| self.matches(item)).count()
    }

    pub fn index(self) -> u8 {
        match self {
            Self::All => 0,
            Self::Downloading => 1,
            Self::Paused => 2,
            Self::Completed => 3,
            Self::Failed => 4,
            Self::FromBrowser => 5,
            Self::FromAgent => 6,
            Self::Category(category) => 7 + category.ordinal(),
        }
    }

    pub fn from_index(index: u8) -> Option<Self> {
        Some(match index {
            0 => Self::All,
            1 => Self::Downloading,
            2 => Self::Paused,
            3 => Self::Completed,
            4 => Self::Failed,
            5 => Self::FromBrowser,
            6 => Self::FromAgent,
            other => Self::Category(FileCategory::from_ordinal(other.saturating_sub(7))?),
        })
    }
}

/// 列表排序键（03 §4.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Time,
    Name,
    Size,
    State,
    Source,
    Speed,
}

impl SortKey {
    pub const ALL: [SortKey; 6] = [
        SortKey::Time,
        SortKey::Name,
        SortKey::Size,
        SortKey::State,
        SortKey::Source,
        SortKey::Speed,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Time => "时间",
            Self::Name => "名称",
            Self::Size => "大小",
            Self::State => "状态",
            Self::Source => "来源",
            Self::Speed => "速度",
        }
    }

    /// 默认方向：时间/速度/大小降序，其余升序。
    pub fn default_ascending(self) -> bool {
        !matches!(self, SortKey::Time | SortKey::Size | SortKey::Speed)
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|key| *key == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

/// 主窗口的工作区页面（`docs/design/13-resource-monitor.md` §2、
/// `docs/design/15-resolve-sessions.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppPage {
    /// 下载库（默认）：侧栏 + 任务流 + 常驻详情面板。
    #[default]
    Downloads,
    /// 资源解析：独立的会话页面（解析历史 + 会话内对话），不再挤在详情面板里。
    Resolve,
    /// 解析经验库（design 17）：AI 成功后沉淀的可回放方法；可查看流程/使用情况/
    /// 编辑/删除/切换版本。
    Experience,
    /// 资源监控：侧栏 + 监控页。
    Monitor,
}

impl AppPage {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Downloads => "下载库",
            Self::Resolve => "资源解析",
            Self::Experience => "解析经验",
            Self::Monitor => "资源监控",
        }
    }
}

/// 监控页「点击分类行 → 回下载库筛选」回调（携带分类参数）。
type MonitorCallbacksJump = Box<dyn Fn(FileCategory, &ClickEvent, &mut Window, &mut App) + 'static>;
/// 监控页「点击任务/文件 → 回下载库/打开文件夹」回调（携带任务 id）。
type MonitorCallbacksSelect = Box<dyn Fn(&str, &ClickEvent, &mut Window, &mut App) + 'static>;

/// Toast（右下角，最多 3 条，≤5s；可撤销操作带「撤销」动作）。
struct Toast {
    text: String,
    severity: Severity,
    created: Instant,
    action: Option<ToastAction>,
}

const TOAST_TTL: Duration = Duration::from_secs(5);
const TOAST_MAX: usize = 3;

/// Toast 上可执行的撤销动作（06 §11）。
#[derive(Debug, Clone)]
enum ToastAction {
    UndoRemove {
        item: Box<DownloadItem>,
        index: usize,
    },
}

/// 删除文件确认（不可逆操作才用模态）。
#[derive(Debug, Clone)]
struct DeleteConfirm {
    id: String,
    file_name: String,
}

// ---------------------------------------------------------------------------
// 主视图
// ---------------------------------------------------------------------------

pub struct RdmApp {
    // ---- 数据与服务 ----
    items: Vec<DownloadItem>,
    settings: Settings,
    state_dir: PathBuf,
    engine: EngineRuntime,
    handles: HashMap<String, (u64, DownloadHandle)>,
    next_seq: u64,
    ai_config: AiConfig,
    ipc_warning: Option<String>,
    hwnd: Option<isize>,
    window_handle: gpui::AnyWindowHandle,

    // ---- 输入 ----
    omnibox: Entity<InputState>,
    palette_query: Entity<InputState>,
    add_url: Entity<InputState>,
    resolve_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,

    // ---- 视图状态 ----
    filter: ViewFilter,
    sort: SortKey,
    sort_ascending: bool,
    density: Density,
    compact_window: bool,
    selection: BTreeSet<String>,
    /// 多选模式：默认关闭（行左侧无候选框）；开启后行左侧显示复选框、顶部出现
    /// 「全选/取消全选」，点击行切换选中（再次点击已选行取消选中）。
    multi_select: bool,
    focused: Option<String>,
    anchor: Option<String>,
    list_focus: gpui::FocusHandle,
    list_scroll: VirtualListScrollHandle,
    sidebar_collapsed: bool,
    show_settings: bool,
    settings_page: Option<Entity<SettingsPage>>,
    palette_open: bool,
    palette_index: usize,
    dark: bool,
    /// 资源解析会话存储（独立「资源解析」页面；历史持久化到 state_dir）。
    resolve: ResolveStore,
    /// 下一次 AI 请求代号（解析 / 聊天 / 汇报 / 标题共用同一计数器，事件按会话归属）。
    next_ai_request: u64,
    /// 「使用此经验解析」的在途请求 id（design 18 §3.1）。
    apply_experience_request: Option<u64>,
    /// 「经验助手」在途请求 id（design 18 §3.2）。
    experience_assistant_request: Option<u64>,
    /// 资源解析页滚动句柄：新消息/工作流更新后自动贴底。
    resolve_scroll: ScrollHandle,
    /// 上次会话历史落盘时间（脏标记 + 1s 去抖，避免流式输出每个 delta 写盘）。
    last_resolve_save: Instant,
    /// 解析历史分组折叠状态（键 = 分组名；`None` = 「未分组」组）。
    resolve_collapsed_groups: BTreeSet<Option<String>>,
    /// 解析经验库页面状态（design 17）。
    experience: ExperiencePageState,

    // ---- 资源监控（docs/design/13-resource-monitor.md）----
    /// 当前工作区页面。
    page: AppPage,
    /// 资源监控快照 + 扫描线程状态。
    monitor: monitor::MonitorUiState,
    /// 监控页滚动句柄。
    monitor_scroll: ScrollHandle,
    /// AI token 账本（`<state_dir>/ai_usage.json`）。
    ai_ledger: AiUsageLedger,
    /// 打开设置页时要定位到的分类（监控页「去设置 AI」用）。
    pending_settings_section: Option<SettingsSection>,

    // ---- 反馈 ----
    toasts: Vec<Toast>,
    delete_confirm: Option<DeleteConfirm>,
    startup_hooks_done: bool,
    /// 全局热键触发的「快速添加」：等下一帧有 Window 时打开添加弹层。
    pending_quick_add: bool,
    /// 上一帧设置是否已打开：用于在「关闭 → 打开」的那一帧统一关闭 dialog/命令面板
    /// （覆盖所有入口，包括直接置位 `show_settings` 的调用方）。
    was_settings_open: bool,
    /// 命令面板「打开资源解析」触发的输入框聚焦：等下一帧有 Window 时执行。
    pending_resolve_focus: bool,
    last_tray_tooltip: Instant,
    /// 默认保存目录所在卷的（可用, 总量）字节数；用于侧栏存储卡。
    disk_free_total: Option<(u64, u64)>,
    last_disk_refresh: Instant,
    /// 待推送的系统通知（完成/失败），由 render 统一 push 到 Root 的通知层。
    pending_notifications: Vec<(Severity, String)>,

    // ---- 持久化 / 指标 ----
    speed_history: VecDeque<f64>,
    dirty_persist: bool,
    last_save: Instant,
}

impl RdmApp {
    pub fn new(
        cx: &mut Context<Self>,
        window: &mut Window,
        bootstrap: Bootstrap,
        hwnd: Option<isize>,
    ) -> Self {
        let Bootstrap {
            settings,
            items,
            next_seq,
            state_dir,
            engine,
            ui_rx,
            initial,
            ai_config,
            ipc_warning,
        } = bootstrap;
        // 分类规则进程内快照：启动即用加载后的设置初始化，侧栏筛选/图标/监控
        // 扫描与磁盘分类共用同一份规则（设置保存时 apply_settings 会再次刷新）。
        model::set_classification_rules(&settings.classification);
        let initial_density = settings.density;
        let dark = match settings.resolved_ui_theme() {
            UiThemeMode::FollowSystem => !matches!(
                window.appearance(),
                gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
            ),
            UiThemeMode::Light => false,
            UiThemeMode::Dark => true,
        };
        theme::apply(cx, dark, settings.accent, settings.reduce_motion);

        let omnibox = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("粘贴下载链接，或搜索任务…")
                .clean_on_escape()
        });
        let palette_query = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入命令…")
                .clean_on_escape()
        });
        let add_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://example.com/file.zip")
                .clean_on_escape()
        });
        let resolve_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("粘贴网页链接，Enter 解析（资源解析只保留历史会话）")
                .clean_on_escape()
        });
        let mut _subscriptions = vec![
            cx.subscribe_in(&omnibox, window, |this, _, ev: &InputEvent, window, cx| {
                this.on_omnibox_event(ev, window, cx);
            }),
            cx.subscribe_in(
                &palette_query,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.confirm_palette(window, cx);
                    }
                },
            ),
            cx.subscribe_in(&add_url, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    let submitted = this.submit_add_from_dialog(cx);
                    if submitted {
                        window.close_dialog(cx);
                    }
                }
            }),
            // 资源解析页 URL 输入：Enter 解析；Shift+Enter 不触发（design 18 §3.3）。
            cx.subscribe_in(
                &resolve_input,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { secondary } = ev {
                        if !*secondary {
                            this.submit_resolve_url(window, cx);
                        }
                    }
                },
            ),
        ];

        // 资源监控的 AI 用量账本（读取失败自动退化为空账本，不影响启动）。
        let ai_ledger = AiUsageLedger::load(&state_dir);
        // 资源解析会话历史（读取失败退化为空历史，不影响启动）。
        let resolve = ResolveStore::load(&state_dir);
        // 解析经验库页面状态（design 17）：构造即读一次盘，随后用真实 state_dir 对齐。
        // design 18 §2.2：先把 v1 的精确 URL 经验幂等迁移成可复用模板（离线、不重放网络）。
        let experience_migration =
            rdm_resolve::migrate_experience_library(&state_dir.join("resolve"));
        if experience_migration.generalized > 0 {
            tracing::info!(
                generalized = experience_migration.generalized,
                scanned = experience_migration.scanned,
                "解析经验已迁移为可复用模板（旧版本保留可回滚）"
            );
        }
        let mut experience = ExperiencePageState::new(window, cx);
        experience.reload(&state_dir);
        // design 18：经验助手与「使用此经验解析」输入框的 Enter 订阅。
        _subscriptions.push(cx.subscribe_in(
            experience.assistant_input(),
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary } = ev {
                    if !*secondary {
                        let text = this.experience.take_assistant_input(window, cx);
                        if !text.trim().is_empty() {
                            this.send_experience_assistant(text, window, cx);
                        }
                    }
                }
            },
        ));
        _subscriptions.push(cx.subscribe_in(
            experience.use_experience_input(),
            window,
            |this, _, ev: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { secondary } = ev {
                    if !*secondary {
                        let url = this.experience.take_use_url();
                        let id = this
                            .experience
                            .selected_id()
                            .map(str::to_owned)
                            .unwrap_or_default();
                        if !id.is_empty() && !url.trim().is_empty() {
                            this.apply_experience_to_url(&id, &url, cx);
                            this.experience.cancel_use_experience();
                        }
                    }
                }
            },
        ));

        let mut app = Self {
            items,
            settings,
            state_dir,
            engine,
            handles: HashMap::new(),
            next_seq,
            ai_config,
            ipc_warning,
            hwnd,
            window_handle: window.window_handle(),
            omnibox,
            palette_query,
            add_url,
            resolve_input,
            _subscriptions,
            filter: ViewFilter::All,
            sort: SortKey::Time,
            sort_ascending: false,
            density: initial_density,
            compact_window: false,
            selection: BTreeSet::new(),
            multi_select: false,
            focused: None,
            anchor: None,
            list_focus: cx.focus_handle(),
            list_scroll: VirtualListScrollHandle::new(),
            sidebar_collapsed: false,
            show_settings: false,
            settings_page: None,
            palette_open: false,
            palette_index: 0,
            dark,
            resolve,
            next_ai_request: 0,
            apply_experience_request: None,
            experience_assistant_request: None,
            resolve_scroll: ScrollHandle::new(),
            last_resolve_save: Instant::now(),
            resolve_collapsed_groups: BTreeSet::new(),
            experience,
            page: AppPage::Downloads,
            monitor: monitor::MonitorUiState::default(),
            monitor_scroll: ScrollHandle::new(),
            ai_ledger,
            pending_settings_section: None,
            toasts: Vec::new(),
            delete_confirm: None,
            startup_hooks_done: false,
            pending_quick_add: false,
            was_settings_open: false,
            pending_resolve_focus: false,
            last_tray_tooltip: Instant::now(),
            disk_free_total: None,
            last_disk_refresh: Instant::now() - Duration::from_secs(60),
            pending_notifications: Vec::new(),
            speed_history: VecDeque::with_capacity(60),
            dirty_persist: false,
            last_save: Instant::now(),
        };

        // 启动恢复：CLI URL / Firefox 兜底队列。
        for item in initial {
            match model::prepare_request(&app.settings, item.download) {
                Ok(request) => {
                    let source = if item.source.is_empty() {
                        "cli".to_string()
                    } else {
                        item.source
                    };
                    app.push_download(request, source);
                }
                Err(err) => tracing::warn!(error = %err, "启动任务 URL 非法，已跳过"),
            }
        }
        app.persist_now();
        if experience_migration.generalized > 0 {
            app.toast(
                format!(
                    "已把 {} 条解析经验整理为可复用模板（旧版本保留，可在版本历史回滚）",
                    experience_migration.generalized
                ),
                Severity::Info,
            );
        }
        if let Some(warning) = app.ipc_warning.clone() {
            app.toast(warning, Severity::Warning);
        }
        if let Some(warning) = app.sync_system_integration() {
            app.toast(warning, Severity::Warning);
        }

        // 事件泵：tokio → GPUI 主线程。
        cx.spawn(async move |this, cx| {
            pump_loop(this, cx, ui_rx).await;
        })
        .detach();
        app
    }

    // -----------------------------------------------------------------------
    // 顶栏输入 / 添加下载
    // -----------------------------------------------------------------------

    fn on_omnibox_event(
        &mut self,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => cx.notify(),
            InputEvent::PressEnter { .. } => self.submit_omnibox(window, cx),
            _ => {}
        }
    }

    /// 万能输入栏提交：URL → 添加下载；其它文本 → 搜索（过滤已在渲染时实时生效）。
    fn submit_omnibox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.omnibox.read(cx).value().trim().to_string();
        if raw.is_empty() {
            return;
        }
        if raw.starts_with('>') {
            self.run_command_text(raw.trim_start_matches('>').trim(), cx);
            return;
        }
        match validate_url(&raw) {
            Ok(url) => {
                let request = self.settings.request_with_defaults(url);
                match model::prepare_request(&self.settings, request) {
                    Ok(request) => {
                        let id = self.push_download(request, "gui".to_string());
                        tracing::info!(download_id = %id, "GUI 添加下载（Omnibox）");
                        self.omnibox
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        self.toast(format!("已添加下载 {id}"), Severity::Success);
                    }
                    Err(err) => self.toast(err, Severity::Danger),
                }
            }
            Err(err) => {
                // 不是合法 URL：保持为搜索词；给出可执行提示。
                self.toast(
                    format!("{err}（当前作为搜索词过滤任务）"),
                    Severity::Warning,
                );
            }
        }
        cx.notify();
    }

    fn run_command_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.to_lowercase();
        let command = Command::ALL
            .into_iter()
            .find(|command| command.label_zh().to_lowercase().contains(&text))
            .or_else(|| {
                if "设置".contains(&text) || "settings".contains(&text) {
                    Some(Command::OpenSettings)
                } else {
                    None
                }
            });
        match command {
            Some(command) => self.run_command(command, cx),
            None => self.toast(format!("未知命令：{text}"), Severity::Warning),
        }
    }

    fn run_command(&mut self, command: Command, cx: &mut Context<Self>) {
        let window = self.window_handle;
        match command {
            Command::AddDownload | Command::FocusOmnibox => {
                let _ = window.update(cx, |_, window, cx| {
                    self.omnibox.update(cx, |input, cx| input.focus(window, cx));
                });
            }
            Command::OpenSettings => {
                self.open_settings(cx);
            }
            Command::ToggleSidebar => self.sidebar_collapsed = !self.sidebar_collapsed,
            Command::OpenResolve => {
                self.set_page(AppPage::Resolve, cx);
                // U-12：命令面板入口与侧栏入口保持一致——打开页面后聚焦会话输入框。
                self.pending_resolve_focus = true;
            }
            Command::OpenExperience => self.open_experience_page(cx),
            Command::ToggleMonitor => {
                let page = if self.page == AppPage::Monitor {
                    AppPage::Downloads
                } else {
                    AppPage::Monitor
                };
                self.set_page(page, cx);
            }
            Command::ToggleDensity => {
                self.density = match self.density {
                    Density::Comfortable => Density::Compact,
                    Density::Compact => Density::Comfortable,
                };
                self.settings.density = self.density;
                self.persist_settings();
            }
            Command::CycleThemeMode => {
                let mode = match self.settings.resolved_ui_theme() {
                    UiThemeMode::Dark => UiThemeMode::Light,
                    UiThemeMode::Light => UiThemeMode::FollowSystem,
                    UiThemeMode::FollowSystem => UiThemeMode::Dark,
                };
                self.set_theme_mode(mode, cx);
            }
            Command::PauseAll => self.pause_all(cx),
            Command::ResumeAll => self.resume_all(cx),
            Command::ClearCompleted => self.clear_completed(cx),
            Command::OpenDownloadFolder => {
                if let Some(dir) = self.settings.default_dir.clone() {
                    if let Err(err) = open_path::open_directory(&dir) {
                        self.toast(format!("打开保存目录失败：{err}"), Severity::Danger);
                    }
                }
            }
        }
        cx.notify();
    }

    fn set_theme_mode(&mut self, mode: UiThemeMode, cx: &mut Context<Self>) {
        self.settings.ui_theme = Some(mode);
        let resolved_dark = match mode {
            UiThemeMode::Light => false,
            UiThemeMode::Dark => true,
            UiThemeMode::FollowSystem => self.dark,
        };
        self.settings.sync_legacy_theme(resolved_dark);
        if let Err(err) = model::save_settings(&self.state_dir, &self.settings) {
            tracing::warn!(error = %err, "保存 settings.json 失败");
        }
        let window = self.window_handle;
        let _ = window.update(cx, |_, window, cx| {
            let dark = theme::resolve_dark(mode, window);
            theme::apply(cx, dark, self.settings.accent, self.settings.reduce_motion);
            window.refresh();
        });
        cx.notify();
    }

    fn set_dark_state(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark == dark {
            return;
        }
        self.dark = dark;
        theme::apply(cx, dark, self.settings.accent, self.settings.reduce_motion);
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 任务操作
    // -----------------------------------------------------------------------

    fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.id == id)
    }

    fn pause_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.pause_item(index);
            cx.notify();
        }
    }

    fn resume_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.resume_item(index);
            cx.notify();
        }
    }

    /// 取消下载：进入「正在取消」，引擎确认终态后由 `apply_result` 留下 Cancelled 行；
    /// 已暂停等非活动态则直接移除记录（可撤销）。这不是暂停。
    fn cancel_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let state = self.items[index].state;
        if state == ItemState::Downloading {
            if !self.items[index].begin_stop(StopRequest::Cancel) {
                return;
            }
            if let Some((_, handle)) = self.handles.get(id) {
                handle.cancel();
            }
            self.toast("正在取消…", Severity::Info);
            cx.notify();
            return;
        }
        if state.is_terminal() {
            return;
        }
        self.handles.remove(id);
        self.remove_item_undoable(index, cx);
    }

    fn retry_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            if matches!(
                self.items[index].state,
                ItemState::Paused | ItemState::Failed | ItemState::Cancelled
            ) {
                self.start_item(index);
                cx.notify();
            }
        }
    }

    /// 移除记录（可撤销）：仅删列表记录，不删文件（契约 §7.2）。
    fn remove_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.remove_item_undoable(index, cx);
        }
    }

    fn remove_item_undoable(&mut self, index: usize, cx: &mut Context<Self>) {
        // 审计 F-03：活动任务不能直接移除——撤销会把「已经停掉引擎的 downloading 行」
        // 原样插回列表，产生永远不动的假下载中。必须先暂停或取消。
        if self
            .items
            .get(index)
            .is_some_and(|item| item.state == ItemState::Downloading)
        {
            self.toast("下载中的任务请先暂停或取消，再移除记录", Severity::Warning);
            cx.notify();
            return;
        }
        let item = self.items.remove(index);
        if let Some((_, handle)) = self.handles.remove(&item.id) {
            handle.cancel();
        }
        self.selection.remove(&item.id);
        if self.focused.as_deref() == Some(item.id.as_str()) {
            self.focused = self
                .items
                .get(index.min(self.items.len().saturating_sub(1)))
                .map(|item| item.id.clone());
        }
        self.persist_now();
        self.toast_with_action(
            format!("已移除记录「{}」", ellipsize_file_name(&item.file_name, 42)),
            Severity::Info,
            ToastAction::UndoRemove {
                item: Box::new(item),
                index,
            },
        );
        cx.notify();
    }

    fn undo(&mut self, action: ToastAction, cx: &mut Context<Self>) {
        match action {
            ToastAction::UndoRemove { item, index } => {
                let index = index.min(self.items.len());
                self.items.insert(index, *item);
                self.persist_now();
                self.toast("已撤销移除", Severity::Success);
            }
        }
        cx.notify();
    }

    /// 删除文件（不可逆）：模态确认后清理磁盘文件 + 移除记录。
    fn open_delete_confirm(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        if self.items[index].state == ItemState::Downloading {
            // 审计 F-03：活动任务删除文件会与引擎写盘竞争；必须先暂停/取消。
            self.toast("下载中的任务请先暂停或取消，再删除文件", Severity::Warning);
            cx.notify();
            return;
        }
        let item = &self.items[index];
        let confirm = DeleteConfirm {
            id: item.id.clone(),
            file_name: item.file_name.clone(),
        };
        self.delete_confirm = Some(confirm.clone());
        let weak = cx.entity().downgrade();
        let weak_builder = weak.clone();
        let file_name = confirm.file_name.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let weak_ok = weak_builder.clone();
            dialog
                .title("删除文件？")
                .child(
                    v_flex()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .child(format!("将删除磁盘上的文件：{file_name}")),
                        )
                        .child(
                            div().text_size(px(12.0)).child(
                                "此操作不可撤销。若只想从列表移除记录，请选择「移除记录」。",
                            ),
                        ),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除文件")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = weak_ok.update(cx, |this, cx| this.confirm_delete_file(cx));
                    true
                })
        });
        cx.notify();
    }

    fn confirm_delete_file(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.delete_confirm.take() else {
            return;
        };
        let Some(index) = self.index_of(&confirm.id) else {
            return;
        };
        let item = &self.items[index];
        let targets = delete_targets(item, &self.state_dir);
        let (removed, errors) = delete_target_files(&targets);
        if !errors.is_empty() {
            self.toast(
                format!("删除文件失败（{} 个）：{}", errors.len(), errors[0]),
                Severity::Danger,
            );
        } else if removed > 0 {
            self.toast(format!("已删除任务和 {removed} 个文件"), Severity::Success);
        } else {
            self.toast("已删除任务（没有找到可删除的文件）", Severity::Info);
        }
        if let Some(index) = self.index_of(&confirm.id) {
            self.remove_item_undoable(index, cx);
        }
        cx.notify();
    }

    fn open_file_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            if let Some(path) = self.items[index].path.clone() {
                if let Err(err) = open_path::open_file(&path) {
                    self.toast(format!("打开文件失败：{err}"), Severity::Danger);
                }
            }
        }
        cx.notify();
    }

    fn open_folder_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            let item = &self.items[index];
            // 完成项返回文件路径（reveal_path 负责 /select 高亮），未完成项返回
            // output_dir/state_dir 目录；reveal_path 自身按「文件还是目录」分派。
            let target = open_path::reveal_target_for_item(
                item.path.as_deref(),
                item.request.output_dir.as_deref(),
                &self.state_dir,
            );
            if let Err(err) = open_path::reveal_path(&target) {
                self.toast(format!("打开文件夹失败：{err}"), Severity::Danger);
            }
        }
        cx.notify();
    }

    fn copy_link_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            let url = self.items[index].url.clone();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(url));
            self.toast("已复制链接", Severity::Success);
            cx.notify();
        }
    }

    fn copy_path_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            let text = self.items[index]
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| self.items[index].url.clone());
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            self.toast("已复制文件路径", Severity::Success);
            cx.notify();
        }
    }

    fn pause_item(&mut self, index: usize) {
        let id = self.items[index].id.clone();
        if !self.items[index].begin_stop(StopRequest::Pause) {
            return;
        }
        if let Some((_, handle)) = self.handles.get(&id) {
            handle.pause();
        }
        self.dirty_persist = true;
    }

    fn resume_item(&mut self, index: usize) {
        let state = self.items[index].state;
        if !matches!(
            state,
            ItemState::Paused | ItemState::Failed | ItemState::Cancelled
        ) || !self.items[index].stop_request.is_none()
        {
            return;
        }
        self.start_item(index);
    }

    fn start_item(&mut self, index: usize) {
        let settings = self.settings.clone();
        let item = &mut self.items[index];
        item.run_id += 1;
        let run_id = item.run_id;
        let id = item.id.clone();
        let mut request = item.request.clone();
        let routing = if let Some(dir) = item.save_dir.clone() {
            request.output_dir = Some(dir);
            None
        } else if settings.save_by_extension {
            Some(RoutingOptions {
                save_by_extension: true,
                classification: settings.classification.clone(),
            })
        } else {
            None
        };
        item.state = ItemState::Downloading;
        item.stop_request = StopRequest::None;
        item.error = None;
        item.note = Some("排队中…".to_string());
        item.speed_history.clear();
        item.clear_speed();
        let handle = DownloadHandle::new();
        self.handles.insert(id.clone(), (run_id, handle.clone()));
        self.engine
            .start_download_routed(id, run_id, request, handle, routing);
        self.persist_now();
    }

    fn allocate_id(&mut self) -> String {
        let id = next_download_id(self.next_seq);
        self.next_seq += 1;
        id
    }

    fn push_download(&mut self, request: rdm_core::DownloadRequest, source: String) -> String {
        let id = self.allocate_id();
        let mut item = DownloadItem::new(id.clone(), request, source);
        let auto_start = self.settings.auto_start;
        if !auto_start {
            item.state = ItemState::Paused;
            item.note = Some("已添加，等待开始（自动开始已关闭）".to_string());
        }
        self.items.push(item);
        let index = self.items.len() - 1;
        if auto_start {
            self.start_item(index);
        } else {
            self.persist_now();
        }
        id
    }

    pub(crate) fn pause_all(&mut self, cx: &mut Context<Self>) {
        let indexes: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.state == ItemState::Downloading)
            .map(|(index, _)| index)
            .collect();
        for index in indexes {
            self.pause_item(index);
        }
        self.toast("已暂停全部下载", Severity::Info);
        cx.notify();
    }

    pub(crate) fn resume_all(&mut self, cx: &mut Context<Self>) {
        let indexes: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.state == ItemState::Paused)
            .map(|(index, _)| index)
            .collect();
        for index in indexes {
            self.resume_item(index);
        }
        self.toast("已继续全部下载", Severity::Info);
        cx.notify();
    }

    /// 打开默认下载目录（托盘菜单用）。
    pub(crate) fn open_download_folder(&mut self, cx: &mut Context<Self>) {
        match self.settings.default_dir.clone() {
            Some(dir) => {
                if let Err(err) = open_path::open_directory(&dir) {
                    self.toast(format!("打开下载目录失败：{err}"), Severity::Danger);
                }
            }
            None => self.toast("尚未设置默认保存目录", Severity::Warning),
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 资源监控（docs/design/13-resource-monitor.md）
    // -----------------------------------------------------------------------

    /// 切换工作区页面（下载库 / 资源解析 / 资源监控）。
    pub(crate) fn set_page(&mut self, page: AppPage, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        if page == AppPage::Monitor {
            // 进入监控页：没有快照或快照超过 30s 立即扫描；否则保留旧数据。
            self.maybe_auto_scan_monitor(cx);
        }
        cx.notify();
    }

    /// 重新扫描下载目录（后台线程；正在扫描时忽略重复请求）。
    pub(crate) fn rescan_monitor(&mut self, cx: &mut Context<Self>) {
        if self.monitor.scanning {
            return;
        }
        let request = monitor::scan::ScanRequest {
            root: self.settings.default_dir.clone(),
            state_dir: self.state_dir.clone(),
            tracked_files: monitor::aggregate::tracked_paths(
                &self.items,
                self.settings.default_dir.as_deref(),
            ),
        };
        self.monitor.pending = Some(monitor::scan::spawn(request));
        self.monitor.scanning = true;
        self.monitor.error = None;
        self.monitor.last_request_ms = Some(monitor::now_ms());
        cx.notify();
    }

    /// 页面可见且快照过期时自动扫描（进入页面 + Tick 调用）。
    fn maybe_auto_scan_monitor(&mut self, cx: &mut Context<Self>) {
        if self.page != AppPage::Monitor || self.monitor.scanning {
            return;
        }
        let now = monitor::now_ms();
        let stale = self
            .monitor
            .last_success_ms
            .map(|last| now.saturating_sub(last) >= 30_000)
            .unwrap_or(true);
        if stale {
            self.rescan_monitor(cx);
        }
    }

    /// 非阻塞收取扫描结果（`EngineMsg::Tick` 调用）。
    fn poll_monitor_scan(&mut self, cx: &mut Context<Self>) {
        let Some(rx) = self.monitor.pending.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(monitor::ScanOutcome::Ok(scan)) => {
                let snapshot = monitor::aggregate::build_snapshot(
                    *scan,
                    &self.items,
                    &self.ai_ledger,
                    self.ai_config.is_configured(),
                );
                self.monitor.snapshot = Some(std::sync::Arc::new(snapshot));
                self.monitor.last_success_ms = Some(monitor::now_ms());
                self.monitor.error = None;
                self.monitor.pending = None;
                self.monitor.scanning = false;
                cx.notify();
            }
            Ok(monitor::ScanOutcome::Err(error)) => {
                self.monitor.error = Some(error);
                self.monitor.pending = None;
                self.monitor.scanning = false;
                cx.notify();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.monitor.error = Some("扫描线程异常结束".to_owned());
                self.monitor.pending = None;
                self.monitor.scanning = false;
                cx.notify();
            }
        }
    }

    pub(crate) fn monitor_snapshot(&self) -> Option<&MonitorSnapshot> {
        self.monitor.snapshot.as_deref()
    }

    pub(crate) fn monitor_scanning(&self) -> bool {
        self.monitor.scanning
    }

    pub(crate) fn monitor_error(&self) -> Option<&str> {
        self.monitor.error.as_deref()
    }

    pub(crate) fn monitor_ai_configured(&self) -> bool {
        self.ai_config.is_configured()
    }

    pub(crate) fn monitor_download_dir(&self) -> Option<&Path> {
        self.settings.default_dir.as_deref()
    }

    /// 页头「上次扫描 12:00:12（42ms）」文案。
    pub(crate) fn monitor_last_scan_label(&self) -> Option<String> {
        let scanned_at = self.monitor.last_success_ms?;
        let scan_ms = self
            .monitor
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.scan_ms)
            .unwrap_or(0);
        Some(format!(
            "上次扫描 {}（{scan_ms}ms）· 每 30s 自动刷新",
            monitor::format_clock_local(scanned_at)
        ))
    }

    pub(crate) fn open_monitor_download_dir(&mut self, cx: &mut Context<Self>) {
        self.open_download_folder(cx);
    }

    pub(crate) fn open_monitor_state_dir(&mut self, cx: &mut Context<Self>) {
        self.open_state_directory(cx);
    }

    /// 监控页 AI 空态 → 设置 · AI 助手。
    pub(crate) fn open_monitor_ai_settings(&mut self, cx: &mut Context<Self>) {
        self.open_settings_section(Some(SettingsSection::Ai), cx);
    }

    /// 监控页构成行 → 下载库 + 对应分类筛选。
    pub(crate) fn jump_to_category(&mut self, category: FileCategory, cx: &mut Context<Self>) {
        self.page = AppPage::Downloads;
        self.filter = ViewFilter::Category(category);
        cx.notify();
    }

    /// 监控页 AI 明细行 → 下载库并选中该任务。
    pub(crate) fn select_item_in_library(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.index_of(id).is_none() {
            self.toast("该任务已不在下载记录中", Severity::Warning);
            cx.notify();
            return;
        }
        self.page = AppPage::Downloads;
        self.filter = ViewFilter::All;
        self.focused = Some(id.to_owned());
        self.selection = [id.to_owned()].into_iter().collect();
        cx.notify();
    }

    /// 按当前 provider/model 的目录价估算一次调用的花费（人民币）。
    ///
    /// 只返回「有价目」的结果；Custom/未知模型返回 `None`，调用方不会把 0 元写进账本。
    /// token 为 0 但有价目（例如解析缓存命中）会返回 `Some(0.0)`，用于表达「有价目、本次免费」。
    fn estimate_ai_cost(&self, usage: &rdm_ai::TokenUsage) -> Option<f64> {
        let estimate = self.ai_config.cost_estimate(usage, monitor::now_ms());
        estimate.priced.then_some(estimate.cny)
    }

    /// 记录一次 AI 消耗到账本并落盘（资源监控的唯一写入口）。
    ///
    /// 参数较多是因为一条账本记录本身字段就多（种类/用量/模型/缓存/来源/会话/归属/费用）；
    /// 用命名清晰的单入口比拆成 builder 更不容易写错。
    #[allow(clippy::too_many_arguments)]
    fn record_ai_usage(
        &mut self,
        kind: AiUsageKind,
        usage: rdm_ai::TokenUsage,
        model: Option<String>,
        cache_hit: bool,
        host: Option<String>,
        session_id: Option<u64>,
        item_ids: Vec<String>,
        cost_cny: Option<f64>,
        cx: &mut Context<Self>,
    ) {
        if usage.is_zero() && !cache_hit {
            return;
        }
        let record = AiUsageRecord {
            at_ms: monitor::now_ms(),
            kind,
            host,
            model,
            cache_hit,
            item_ids,
            tokens: usage,
            session_id,
            cost_cny,
        };
        self.ai_ledger.record(record, monitor::now_ms());
        self.ai_ledger.save(&self.state_dir);
        cx.notify();
    }

    /// 解析完成后把 token/费用归属到随后创建的下载项（同一次解析的多个项共享）。
    ///
    /// `session_id` 是创建下载的会话：只消费**该会话**自己的解析 token 快照。
    /// 审计 F-02：归属状态不能放在 RdmApp 全局槽位，否则多会话/后台解析会互相覆盖。
    fn attach_parse_usage_to_items(&mut self, session_id: &str, item_ids: &[String]) {
        let expected_request = self
            .resolve
            .get(session_id)
            .map(|session| session.panel.request_id)
            .unwrap_or(0);
        // 从会话里取出快照；request id 不匹配说明快照属于该会话更早的一次解析，丢弃。
        let Some(snapshot) = self.resolve.get_mut(session_id).and_then(|session| {
            let matches_current = session
                .last_parse_usage
                .as_ref()
                .is_some_and(|snapshot| snapshot.request_id == expected_request);
            matches_current
                .then(|| session.last_parse_usage.take())
                .flatten()
        }) else {
            return;
        };
        if item_ids.is_empty() {
            if let Some(session) = self.resolve.get_mut(session_id) {
                session.last_parse_usage = Some(snapshot);
            }
            return;
        }
        let siblings = item_ids.len().min(u32::MAX as usize) as u32;
        let now = model::now_ms();
        for id in item_ids {
            if let Some(item) = self.items.iter_mut().find(|item| &item.id == id) {
                let ai = item.ai.get_or_insert_with(ItemAiUsage::default);
                ai.parse = snapshot.usage;
                ai.parse_cost_cny = snapshot.cost_cny.unwrap_or(0.0);
                if ai.model.is_none() {
                    ai.model = snapshot.model.clone();
                }
                ai.cache_hit = snapshot.cache_hit;
                ai.parse_siblings = siblings;
                ai.updated_ms = now;
            }
        }
        self.ai_ledger
            .attach_items_to_parse(snapshot.request_id, item_ids);
        self.ai_ledger.save(&self.state_dir);
        self.dirty_persist = true;
    }

    fn clear_completed(&mut self, cx: &mut Context<Self>) {
        let count = self
            .items
            .iter()
            .filter(|item| item.state == ItemState::Completed)
            .count();
        self.items.retain(|item| item.state != ItemState::Completed);
        self.persist_now();
        self.toast(
            format!("已清空 {count} 条已完成记录（文件保留）"),
            Severity::Info,
        );
        cx.notify();
    }

    fn run_task_command(
        &mut self,
        command: &TaskCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = command.id.to_string();
        match command.kind {
            TaskCommandKind::Pause => self.pause_by_id(&id, cx),
            TaskCommandKind::Resume => self.resume_by_id(&id, cx),
            TaskCommandKind::Cancel => self.cancel_by_id(&id, cx),
            TaskCommandKind::Retry => self.retry_by_id(&id, cx),
            TaskCommandKind::Remove => self.remove_by_id(&id, cx),
            TaskCommandKind::DeleteFile => self.open_delete_confirm(&id, window, cx),
            TaskCommandKind::OpenFile => self.open_file_by_id(&id, cx),
            TaskCommandKind::OpenFolder => self.open_folder_by_id(&id, cx),
            TaskCommandKind::CopyLink => self.copy_link_by_id(&id, cx),
            TaskCommandKind::CopyPath => self.copy_path_by_id(&id, cx),
            TaskCommandKind::Details => {
                // 详情面板已常驻右侧：这里只保证回到下载库并聚焦该行。
                self.page = AppPage::Downloads;
                self.focused = Some(id);
                self.selection = [command.id.to_string()].into_iter().collect();
                cx.notify();
            }
        }
    }

    // -----------------------------------------------------------------------
    // 选择 / 排序 / 过滤
    // -----------------------------------------------------------------------

    fn filtered_indices(&self, cx: &Context<Self>) -> Vec<usize> {
        let query = self.omnibox.read(cx).value().to_lowercase();
        filter_and_sort(
            &self.items,
            self.filter,
            &query,
            self.sort,
            self.sort_ascending,
        )
    }

    fn selected_or_focused(&self) -> Option<&DownloadItem> {
        self.selection
            .iter()
            .next()
            .or(self.focused.as_ref())
            .and_then(|id| self.items.iter().find(|item| &item.id == id))
    }

    fn focused_id(&self) -> Option<String> {
        self.focused
            .clone()
            .or_else(|| self.selection.iter().next().cloned())
    }

    fn move_focus(&mut self, delta: isize, extend: bool, cx: &Context<Self>) {
        let indexes = self.filtered_indices(cx);
        if indexes.is_empty() {
            return;
        }
        let current = self.focused.as_ref().and_then(|id| {
            indexes
                .iter()
                .position(|&index| &self.items[index].id == id)
        });
        let next = match current {
            Some(position) => {
                ((position as isize + delta).rem_euclid(indexes.len() as isize)) as usize
            }
            None => {
                if delta >= 0 {
                    0
                } else {
                    indexes.len() - 1
                }
            }
        };
        let id = self.items[indexes[next]].id.clone();
        if extend {
            let anchor_pos = self
                .anchor
                .as_ref()
                .and_then(|anchor| {
                    indexes
                        .iter()
                        .position(|&index| &self.items[index].id == anchor)
                })
                .unwrap_or(next);
            let (lo, hi) = if anchor_pos <= next {
                (anchor_pos, next)
            } else {
                (next, anchor_pos)
            };
            self.selection.clear();
            for index in &indexes[lo..=hi] {
                self.selection.insert(self.items[*index].id.clone());
            }
        } else {
            self.selection.clear();
            self.selection.insert(id.clone());
            self.anchor = Some(id.clone());
        }
        self.focused = Some(id);
        self.list_scroll
            .scroll_to_item(next, gpui::ScrollStrategy::Center);
    }

    fn toggle_selection(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.focused.clone() else {
            return;
        };
        if !self.selection.remove(&id) {
            self.selection.insert(id);
        }
        self.anchor = self.focused.clone();
        cx.notify();
    }

    /// 开启/关闭多选模式。
    ///
    /// 关闭时清空选择（行高亮/候选框一起消失）；开启时保留当前单选，方便从
    /// 「已选 1 项」继续多选。
    fn toggle_multi_select(&mut self, cx: &mut Context<Self>) {
        self.multi_select = !self.multi_select;
        if !self.multi_select {
            self.selection.clear();
            self.focused = None;
            self.anchor = None;
        }
        cx.notify();
    }

    /// 当前筛选结果是否已全部选中（用于「全选/取消全选」文案）。
    fn all_visible_selected(&self, cx: &Context<Self>) -> bool {
        let indexes = self.filtered_indices(cx);
        !indexes.is_empty()
            && indexes
                .iter()
                .all(|&index| self.selection.contains(&self.items[index].id))
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        // Ctrl+A/全选天然属于多选场景：自动进入多选模式，让行左侧复选框出现。
        self.multi_select = true;
        let indexes = self.filtered_indices(cx);
        self.selection = indexes
            .iter()
            .map(|&index| self.items[index].id.clone())
            .collect();
        self.anchor = self.focused.clone();
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        self.focused = None;
        self.anchor = None;
        cx.notify();
    }

    fn activate_focused(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.focused_id() else { return };
        let Some(index) = self.index_of(&id) else {
            return;
        };
        if self.items[index].state == ItemState::Completed {
            self.open_file_by_id(&id, cx);
        } else {
            // 详情面板已常驻右侧：回车只聚焦该行，无需切换面板。
            self.page = AppPage::Downloads;
            cx.notify();
        }
    }

    // -----------------------------------------------------------------------
    // 提示 / 持久化
    // -----------------------------------------------------------------------

    fn toast(&mut self, text: impl Into<String>, severity: Severity) {
        self.toasts.push(Toast {
            text: text.into(),
            severity,
            created: Instant::now(),
            action: None,
        });
        while self.toasts.len() > TOAST_MAX {
            self.toasts.remove(0);
        }
    }

    fn toast_with_action(
        &mut self,
        text: impl Into<String>,
        severity: Severity,
        action: ToastAction,
    ) {
        self.toasts.push(Toast {
            text: text.into(),
            severity,
            created: Instant::now(),
            action: Some(action),
        });
        while self.toasts.len() > TOAST_MAX {
            self.toasts.remove(0);
        }
    }

    fn prune_toasts(&mut self) -> bool {
        let before = self.toasts.len();
        self.toasts
            .retain(|toast| toast.created.elapsed() < TOAST_TTL);
        self.toasts.len() != before
    }

    fn persist_now(&mut self) {
        self.dirty_persist = false;
        self.last_save = Instant::now();
        if let Err(err) = model::save_downloads(&self.state_dir, &self.items, self.next_seq) {
            tracing::warn!(error = %err, "保存 downloads.json 失败");
            self.toast(format!("保存下载列表失败：{err}"), Severity::Danger);
        }
    }

    fn persist_settings(&mut self) {
        if let Err(err) = model::save_settings(&self.state_dir, &self.settings) {
            tracing::warn!(error = %err, "保存 settings.json 失败");
            self.toast(format!("保存设置失败：{err}"), Severity::Danger);
        }
    }

    fn flush_if_dirty(&mut self) {
        if self.dirty_persist && self.last_save.elapsed() >= Duration::from_secs(1) {
            self.persist_now();
        }
        self.save_resolve_history_if_due();
    }

    /// 资源解析历史落盘（脏标记 + 1s 去抖；退出时由 `Drop` 强制保存）。
    fn save_resolve_history_if_due(&mut self) {
        if !self.resolve.is_dirty() || self.last_resolve_save.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_resolve_save = Instant::now();
        if let Err(err) = self.resolve.save_if_dirty(&self.state_dir) {
            tracing::warn!(error = %err, "保存 resolve_sessions.json 失败");
        }
    }

    fn restore_window(&mut self, cx: &mut Context<Self>) {
        let hwnd = self.hwnd;
        let _ = self.window_handle.update(cx, move |_, window, app| {
            if let Some(raw) = hwnd {
                desktop::show_window(raw);
            }
            window.defer(app, move |window, _| {
                if let Some(raw) = hwnd {
                    desktop::show_window(raw);
                }
                window.activate_window();
            });
        });
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 引擎消息 / IPC
    // -----------------------------------------------------------------------

    fn apply_batch(&mut self, batch: Vec<EngineMsg>, cx: &mut Context<Self>) {
        let mut notify = false;
        for msg in batch {
            match msg {
                EngineMsg::DownloadEvent { id, run_id, event } => {
                    if let Some(item) = self
                        .items
                        .iter_mut()
                        .find(|item| item.id == id && item.run_id == run_id)
                    {
                        item.apply_event(event);
                        self.dirty_persist = true;
                        notify = true;
                    }
                }
                EngineMsg::Finished { id, run_id, result } => {
                    let mut completed_name: Option<String> = None;
                    let mut failed: Option<String> = None;
                    if let Some(item) = self
                        .items
                        .iter_mut()
                        .find(|item| item.id == id && item.run_id == run_id)
                    {
                        item.apply_result(result);
                        match item.state {
                            ItemState::Completed => completed_name = Some(item.file_name.clone()),
                            ItemState::Failed => {
                                failed = Some(format!(
                                    "{} 下载失败：{}",
                                    item.file_name,
                                    item.error.as_deref().unwrap_or("未知错误")
                                ))
                            }
                            _ => {}
                        }
                        self.dirty_persist = true;
                        notify = true;
                    }
                    if let Some(name) = completed_name {
                        self.pending_notifications
                            .push((Severity::Success, format!("{name} 已下载完成")));
                    }
                    if let Some(text) = failed {
                        self.pending_notifications.push((Severity::Danger, text));
                    }
                    if self
                        .handles
                        .get(&id)
                        .is_some_and(|(handle_run, _)| *handle_run == run_id)
                    {
                        self.handles.remove(&id);
                    }
                    self.persist_now();
                }
                EngineMsg::Routing {
                    id,
                    run_id,
                    dir,
                    reason,
                } => {
                    if let Some(item) = self
                        .items
                        .iter_mut()
                        .find(|item| item.id == id && item.run_id == run_id)
                    {
                        item.save_dir = Some(dir);
                        if !reason.is_empty() {
                            item.note = Some(format!("分类保存：{reason}"));
                        }
                        self.dirty_persist = true;
                        notify = true;
                    }
                }
                EngineMsg::AiParseFinished { request_id, report } => {
                    // 事件按「拥有该解析请求的会话」归属：用户切换会话/页面时，后台
                    // 解析结果仍写回原会话，不会被错误地塞进当前会话。
                    let Some(session_id) = self.session_id_by_parse_request(request_id) else {
                        continue;
                    };
                    // 资源监控：解析 token 归属到本次解析会话（成交后再回填 item id）。
                    // 审计 F-06：`cache_hit` 是独立事实（缓存命中即使 0 token 也要记一条账），
                    // 不能只看 `ai_used/usage/model` 推导。
                    let ai_assisted = report.ai_used
                        || !report.usage.is_zero()
                        || report.model.is_some()
                        || report.cache_hit;
                    let parse_snapshot = if ai_assisted {
                        let parse_cost = self.estimate_ai_cost(&report.usage);
                        self.record_ai_usage(
                            AiUsageKind::Parse,
                            report.usage,
                            report.model.clone(),
                            report.cache_hit,
                            model::url_host(&report.source_url),
                            Some(request_id),
                            Vec::new(),
                            parse_cost,
                            cx,
                        );
                        Some(ParseUsageSnapshot {
                            request_id,
                            usage: report.usage,
                            model: report.model.clone(),
                            cache_hit: report.cache_hit,
                            cost_cny: parse_cost,
                        })
                    } else {
                        // 纯启发式解析：不产生 AI 成本（也不覆盖本会话更早的归属数据——
                        // 新解析开始时已经清过该会话的快照）。
                        None
                    };
                    let is_agent = self
                        .resolve
                        .get(&session_id)
                        .is_some_and(|session| session.agent_request == Some(request_id));
                    if let Some(session) = self.resolve.get_mut(&session_id) {
                        // Design 17 §2.5：重新解析成功时与上次结果合并（保留更丰富的旧资源）。
                        // 旧资源在 `begin_resolve_parse` 开始前已快照（`begin_parse` 会清空面板）。
                        let previous = session.reparse_previous_resources.take();
                        session.panel.apply_report(report.clone());
                        if let Some(previous) = previous {
                            let kept = session.panel.merge_preserved_resources(previous);
                            if kept > 0 {
                                session.panel.push_note(format!(
                                    "已保留上次解析中更丰富的 {kept} 个资源（本次结果在前）"
                                ));
                            }
                        }
                        session.reparse_keep_best = false;
                        session.reparse_previous_resources = None;
                        session.last_parse_usage = parse_snapshot;
                        let now = model::now_ms();
                        session.touch(now);
                        if session.title_source == TitleSource::Fallback {
                            // 解析完成后资源清单才完整：先用确定性标题顶上，随后由 LLM 覆盖。
                            let fallback = session.fallback_title();
                            session.title = fallback;
                        }
                    }
                    self.resolve.mark_dirty();
                    // 解析可能发布了/升级了经验（design 17）：刷新经验库视图。
                    self.reload_experiences();
                    if is_agent {
                        self.finish_agent_resolve(&session_id, report, cx);
                    } else {
                        self.maybe_start_title_generation(&session_id, cx);
                    }
                    self.scroll_resolve_if_active(&session_id);
                    notify = true;
                }
                EngineMsg::AiParseFailed { request_id, error } => {
                    let Some(session_id) = self.session_id_by_parse_request(request_id) else {
                        continue;
                    };
                    let is_agent = self
                        .resolve
                        .get(&session_id)
                        .is_some_and(|session| session.agent_request == Some(request_id));
                    let mut preserved_toast: Option<String> = None;
                    if let Some(session) = self.resolve.get_mut(&session_id) {
                        // Design 17 §2.5：重新解析失败不清空已看到的结果。
                        // 旧资源在 `begin_resolve_parse` 前快照；失败时先还原再标错误。
                        let previous = session.reparse_previous_resources.take();
                        match previous.filter(|resources| !resources.is_empty()) {
                            Some(previous) => {
                                let preserved = previous.len();
                                session.panel.resources = previous;
                                session.panel.apply_reparse_error(error.clone(), preserved);
                                session
                                    .panel
                                    .push_message(model::AiRole::System, error.clone());
                                preserved_toast =
                                    Some(format!("重新解析失败；已保留上次 {preserved} 个资源"));
                            }
                            None => session.panel.apply_parse_error(error.clone()),
                        }
                        session.reparse_keep_best = false;
                        session.reparse_previous_resources = None;
                        let now = model::now_ms();
                        session.touch(now);
                        if session.title_source == TitleSource::Fallback {
                            let fallback = session.fallback_title();
                            session.title = fallback;
                        }
                    }
                    if let Some(message) = preserved_toast {
                        self.toast(message, Severity::Warning);
                    }
                    self.resolve.mark_dirty();
                    if is_agent {
                        self.finish_agent_parse_failed(&session_id, &error, cx);
                    } else {
                        self.maybe_start_title_generation(&session_id, cx);
                    }
                    self.scroll_resolve_if_active(&session_id);
                    notify = true;
                }
                EngineMsg::AiChatDelta { request_id, delta } => {
                    // design 18 §3.2：经验助手的流式回复优先按 request_id 路由到经验页。
                    if self.experience_assistant_request == Some(request_id) {
                        self.experience.assistant_delta(&delta);
                        notify = true;
                        continue;
                    }
                    let active_id = self.resolve.active_id().map(str::to_owned);
                    let mut active_scrolled = false;
                    if let Some(session) = self.resolve.find_mut_by_chat_request(request_id) {
                        session.panel.push_chat_delta(&delta);
                        if active_id.as_deref() == Some(session.id.as_str()) {
                            active_scrolled = true;
                        }
                        notify = true;
                    }
                    if active_scrolled {
                        self.resolve_scroll.scroll_to_bottom();
                    }
                }
                EngineMsg::AiChatFinished {
                    request_id,
                    reply,
                    usage,
                } => {
                    // design 18 §3.2：经验助手完成（不进入解析会话）。
                    if self.experience_assistant_request == Some(request_id) {
                        self.experience_assistant_request = None;
                        self.experience.assistant_finished(&reply);
                        self.record_chat_usage(request_id, usage, cx);
                        notify = true;
                        continue;
                    }
                    // 标题生成是「无消息」的模型调用：只更新会话标题，不产生聊天气泡。
                    if let Some(session_id) = self.session_id_by_title_request(request_id) {
                        if let Some(session) = self.resolve.get_mut(&session_id) {
                            session.title_request = None;
                            let now = model::now_ms();
                            if !session.apply_generated_title(&reply, now) {
                                // 模型输出不可用：保留确定性标题（不展示空标题/原文）。
                                let fallback = session.fallback_title();
                                session.title = fallback;
                                session.touch(now);
                            }
                        }
                        self.resolve.mark_dirty();
                        self.record_chat_usage(request_id, usage, cx);
                        self.scroll_resolve_if_active(&session_id);
                        notify = true;
                        continue;
                    }
                    let Some(session_id) = self.session_id_by_chat_request(request_id) else {
                        continue;
                    };
                    let is_agent_summary = self
                        .resolve
                        .get(&session_id)
                        .is_some_and(|session| session.agent_summary_request == Some(request_id));
                    if let Some(session) = self.resolve.get_mut(&session_id) {
                        session.chat_request = None;
                        if !is_agent_summary {
                            session.panel.finish_chat_stream(&reply);
                            session.touch(model::now_ms());
                        }
                    }
                    if is_agent_summary {
                        self.finish_agent_summary(&session_id, &reply, cx);
                    } else {
                        self.maybe_start_title_generation(&session_id, cx);
                    }
                    self.resolve.mark_dirty();
                    self.record_chat_usage(request_id, usage, cx);
                    self.scroll_resolve_if_active(&session_id);
                    notify = true;
                }
                EngineMsg::AiChatFailed {
                    request_id,
                    error,
                    usage,
                } => {
                    // design 18 §3.2：经验助手失败（保留已花用量，页面内联错误）。
                    if self.experience_assistant_request == Some(request_id) {
                        self.experience_assistant_request = None;
                        self.experience.assistant_failed(&error);
                        self.record_chat_usage(request_id, usage, cx);
                        notify = true;
                        continue;
                    }
                    if let Some(session_id) = self.session_id_by_title_request(request_id) {
                        // 标题生成失败：保留确定性标题，界面不显示模型错误（标题是增强项）。
                        if let Some(session) = self.resolve.get_mut(&session_id) {
                            session.title_request = None;
                            let now = model::now_ms();
                            if session.title_source == TitleSource::Fallback {
                                let fallback = session.fallback_title();
                                session.title = fallback;
                            }
                            session.touch(now);
                        }
                        self.resolve.mark_dirty();
                        self.record_chat_usage(request_id, usage, cx);
                        notify = true;
                        continue;
                    }
                    let Some(session_id) = self.session_id_by_chat_request(request_id) else {
                        continue;
                    };
                    let is_agent_summary = self
                        .resolve
                        .get(&session_id)
                        .is_some_and(|session| session.agent_summary_request == Some(request_id));
                    if let Some(session) = self.resolve.get_mut(&session_id) {
                        session.chat_request = None;
                        if !is_agent_summary {
                            session.panel.fail_chat_stream(&error);
                            session.touch(model::now_ms());
                        }
                    }
                    if is_agent_summary {
                        self.fail_agent_summary(&session_id, &error, cx);
                    }
                    self.resolve.mark_dirty();
                    self.record_chat_usage(request_id, usage, cx);
                    self.scroll_resolve_if_active(&session_id);
                    notify = true;
                }
                EngineMsg::ExperienceApplied {
                    request_id,
                    experience_id,
                    url,
                    outcome,
                } => {
                    // design 18 §3.1：只有当前在途的「使用此经验解析」请求才落 UI。
                    if self.apply_experience_request != Some(request_id) {
                        continue;
                    }
                    self.apply_experience_request = None;
                    self.finish_apply_experience(&experience_id, &url, outcome, cx);
                    notify = true;
                }
                EngineMsg::Tick => {
                    // 托盘提示每 2s 更新一次（活动数/总速度），静默驻留时也能看出进度。
                    if self.last_tray_tooltip.elapsed() >= Duration::from_secs(2) {
                        self.last_tray_tooltip = Instant::now();
                        if desktop::is_tray_alive() {
                            let active = active_count(&self.items);
                            let speed = global_speed_bps(&self.items);
                            desktop::update_tray_tooltip(&format!(
                                "RDM · {active} 个下载中 · {}",
                                model::format_speed(speed)
                            ));
                        }
                    }
                    if self.last_disk_refresh.elapsed() >= Duration::from_secs(30) {
                        self.last_disk_refresh = Instant::now();
                        if let Some(dir) = self.settings.default_dir.as_ref() {
                            self.disk_free_total = desktop::disk_space_for(dir);
                        }
                    }
                    if active_count(&self.items) > 0 {
                        let speed = global_speed_bps(&self.items);
                        self.speed_history.push_back(speed);
                        while self.speed_history.len() > 60 {
                            self.speed_history.pop_front();
                        }
                        notify = true;
                    }
                    if self.prune_toasts() {
                        notify = true;
                    }
                    // 资源监控：收取后台扫描结果；页面可见时按 30s 过期自动刷新。
                    self.poll_monitor_scan(cx);
                    if self.page == AppPage::Monitor {
                        self.maybe_auto_scan_monitor(cx);
                    }
                    self.flush_if_dirty();
                }
                EngineMsg::Ipc { envelope, reply } => {
                    let response = self.handle_ipc(envelope, cx);
                    let _ = reply.send(response);
                    notify = true;
                }
            }
        }
        if notify {
            cx.notify();
        }
    }

    fn handle_ipc(&mut self, envelope: Envelope, cx: &mut Context<Self>) -> Response {
        let request_id = envelope.id;
        match envelope.request {
            Request::Ping => Response::ok(request_id, ipc_server::ping_payload(&self.items)),
            Request::Status => Response::ok(request_id, ipc_server::status_payload(&self.items)),
            Request::Enqueue { item } => {
                let source = if item.source.is_empty() {
                    "ipc".to_string()
                } else {
                    item.source
                };
                match model::prepare_request(&self.settings, item.download) {
                    Ok(request) => {
                        let id = self.push_download(request, source);
                        self.restore_window(cx);
                        Response::ok(request_id, ipc_server::enqueue_payload(&id))
                    }
                    Err(err) => Response::error(request_id, err),
                }
            }
            Request::Pause { id } => {
                if self.index_of(&id).is_some() {
                    self.pause_by_id(&id, cx);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::Resume { id } => {
                if self.index_of(&id).is_some() {
                    self.resume_by_id(&id, cx);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::Cancel { id } => {
                if self.index_of(&id).is_some() {
                    self.cancel_by_id(&id, cx);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::OpenFolder { id } => {
                if self.index_of(&id).is_some() {
                    self.open_folder_by_id(&id, cx);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::AiParse {
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
                force_ai,
            } => match validate_url(&url) {
                Ok(url) => {
                    self.restore_window(cx);
                    let cookie = if include_credentials { cookie } else { None };
                    let page_snapshot = crate::ai_resolve::page_snapshot_from_ipc(
                        &url,
                        page_url.as_deref(),
                        page_title.as_deref(),
                        &page_links,
                    );
                    let request = rdm_ai::ParseRequest {
                        url,
                        title,
                        referer,
                        user_agent,
                        cookie,
                        hint: source_page,
                    };
                    // 浏览器扩展主动解析：同一规范化页面 URL 复用历史会话（设计契约 §2.3），
                    // 没命中才新建会话留档；响应形状不变（session_id 可能是复用后的 id）。
                    let now = model::now_ms();
                    let existing = self.reusable_resolve_session(&request.url);
                    self.page = AppPage::Resolve;
                    match existing {
                        Some((session_id, parsing, current_request)) => {
                            self.resolve.set_active(&session_id);
                            if parsing {
                                // 该会话已在解析：只切换过去，不重复发起。
                                self.resolve_scroll.scroll_to_bottom();
                                cx.notify();
                                Response::ok(
                                    request_id,
                                    serde_json::json!({
                                        "accepted": true,
                                        "request_id": current_request,
                                        "session_id": session_id,
                                    }),
                                )
                            } else {
                                if let Some(session) = self.resolve.get_mut(&session_id) {
                                    session.touch(now);
                                    session
                                        .panel
                                        .push_message(model::AiRole::System, REUSE_SESSION_NOTICE);
                                }
                                self.resolve.mark_dirty();
                                let ai_request_id = self.begin_resolve_parse(
                                    &session_id,
                                    request,
                                    page_snapshot,
                                    None,
                                    force_ai,
                                    cx,
                                );
                                Response::ok(
                                    request_id,
                                    serde_json::json!({
                                        "accepted": true,
                                        "request_id": ai_request_id,
                                        "session_id": session_id,
                                    }),
                                )
                            }
                        }
                        None => {
                            let session_id =
                                self.resolve.create_session(ResolveOrigin::Extension, now);
                            let ai_request_id = self.begin_resolve_parse(
                                &session_id,
                                request,
                                page_snapshot,
                                None,
                                force_ai,
                                cx,
                            );
                            Response::ok(
                                request_id,
                                serde_json::json!({
                                    "accepted": true,
                                    "request_id": ai_request_id,
                                    "session_id": session_id,
                                }),
                            )
                        }
                    }
                }
                Err(err) => Response::error(request_id, err),
            },
            Request::Activate => {
                self.restore_window(cx);
                Response::ok(request_id, serde_json::json!({ "activated": true }))
            }
        }
    }

    /// 在指定会话里启动一次资源解析（GUI 输入 / 浏览器扩展 / Agent 工作流共用）。
    ///
    /// 这是「开始一次解析」的**唯一入口**，统一负责：请求 id、清空旧工作流/标题/流式、
    /// 写入内存凭据、兜底标题、AI 未配置提示、调用引擎。`agent_plan = Some(..)` 表示本次
    /// 解析属于一个下载 Agent 工作流（用于完成后汇报/自动下载）；`None` 表示普通解析。
    ///
    /// 语义边界：一次会话同一时刻只允许一个「当前解析」；开始新解析会清掉该会话的
    /// 旧工作流与流式状态（旧解析的迟到结果会被 `session_id_by_parse_request` 丢弃），
    /// 但**不影响其它会话**的后台解析/聊天。
    ///
    /// `force_ai = true` 表示「重新解析」（design 17 §2.4）：跳过本地经验、强制 AI 探索
    /// （未配置 AI 时启发式兜底），并保留上次更丰富的资源结果（§2.5）。
    #[allow(clippy::too_many_arguments)]
    fn begin_resolve_parse(
        &mut self,
        session_id: &str,
        request: rdm_ai::ParseRequest,
        page_snapshot: Option<rdm_resolve::PageSnapshot>,
        agent_plan: Option<AgentPlan>,
        force_ai: bool,
        cx: &mut Context<Self>,
    ) -> u64 {
        let request_id = self.next_ai_request_id();
        let mut accepted = false;
        if let Some(session) = self.resolve.get_mut(session_id) {
            accepted = true;
            // 新解析接管会话：旧工作流/标题请求/流式输出全部作废，避免步骤挂起或串台。
            session.plan = None;
            session.agent_request = None;
            session.agent_summary_request = None;
            session.title_request = None;
            // 新解析开始：本会话上一次解析的 token 不再归属到后续下载项
            //（审计 F-02：只清本会话，不影响其它会话的归属）。
            session.last_parse_usage = None;
            // design 17 §2.5：重新解析要保留上次更丰富的资源（失败也不清空）。
            // 必须在 `begin_parse` 清空面板之前快照旧资源。
            session.reparse_keep_best = force_ai;
            session.reparse_previous_resources = force_ai.then(|| session.panel.resources.clone());
            session.panel.begin_parse(&request.url, request_id);
            session.panel.set_request_credential(&request);
            session.panel.abort_chat_stream();
            session.chat_request = None;
            let now = model::now_ms();
            session.touch(now);
            // 标题先给确定性兜底（host + 资源数），解析完成后由 LLM 覆盖。
            let fallback = session.fallback_title();
            session.title = fallback;
            session.title_source = TitleSource::Fallback;
            if let Some(mut plan) = agent_plan {
                plan.request_id = request_id;
                session.plan = Some(plan);
                session.agent_request = Some(request_id);
            }
            if force_ai {
                // 明确告知：这次不是经验复用，而是重新研究；AI 未配置时说明兜底路径。
                let message = if self.ai_config.is_configured() {
                    "重新解析：跳过本地解析经验，调用 AI 重新探索；本次结果会与上次结果合并（保留更丰富的资源）。"
                } else {
                    "重新解析：跳过本地解析经验；未配置 AI，本次使用启发式结果（可到设置 → AI 助手配置）。"
                };
                session.panel.push_message(model::AiRole::System, message);
            } else if !self.ai_config.is_configured() {
                let message = if session.agent_request.is_some() {
                    "AI 未配置：解析会使用本地启发式结果；工作流与下载不受影响。"
                } else {
                    "未启用/未配置 AI（设置 → AI 助手）；本次解析使用本地启发式结果。"
                };
                session.panel.push_message(model::AiRole::System, message);
            }
        }
        if !accepted {
            tracing::warn!(session_id, "解析启动失败：会话不存在");
            return request_id;
        }
        self.resolve.mark_dirty();
        self.resolve.set_active(session_id);
        self.page = AppPage::Resolve;
        self.engine.start_ai_parse_with_options(
            request_id,
            request,
            self.ai_config.clone(),
            self.state_dir.clone(),
            page_snapshot,
            force_ai,
        );
        cx.notify();
        request_id
    }

    /// 资源解析页的「下载所选」：只下载当前会话勾选的资源。
    fn download_selected_resources(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let selected = self
            .resolve
            .get(session_id)
            .map(|session| session.panel.selected_indices())
            .unwrap_or_default();
        if selected.is_empty() {
            self.toast("请先勾选要下载的资源", Severity::Warning);
            cx.notify();
            return;
        }
        let created = self.create_downloads_for(session_id, selected);
        if created.is_empty() {
            self.toast(
                "没有创建任何下载任务（检查保存目录/设置）",
                Severity::Warning,
            );
        } else {
            self.toast(
                format!("已添加 {} 个下载", created.len()),
                Severity::Success,
            );
        }
        cx.notify();
    }

    /// 按资源下标从指定会话创建下载任务（Agent 与「下载所选」共用）；返回文件名。
    ///
    /// 统一语义：下载参数来自设置；只有资源 host 与会话解析来源 host **完全一致**时
    /// 才附加该会话的内存凭据（Cookie/Referer/UA）；任务来源由**会话来源**派生
    /// （`ResolveOrigin::download_source`：gui / extension / agent），不再硬编码 `agent`。
    fn create_downloads_for(&mut self, session_id: &str, indices: Vec<usize>) -> Vec<String> {
        // 先从会话里取出「要下载什么 + 对应凭据 + 来源」，避免创建下载时长期借用会话。
        let (cards, source): (
            Vec<(rdm_ai::ParsedResource, Option<model::ParseCredential>)>,
            &'static str,
        ) = {
            let Some(session) = self.resolve.get(session_id) else {
                return Vec::new();
            };
            (
                indices
                    .iter()
                    .filter_map(|index| {
                        let card = session.panel.resources.get(*index)?;
                        let credential = session.panel.download_credentials_for(&card.resource.url);
                        Some((card.resource.clone(), credential))
                    })
                    .collect(),
                session.origin.download_source(),
            )
        };
        let mut created = Vec::new();
        let mut created_ids = Vec::new();
        for (resource, credential) in cards {
            let mut request = self.settings.request_with_defaults(resource.url.clone());
            // Final guard for task names: resolver/AI paths normalize already,
            // but a legacy/imported resource could still be a bare title. The
            // task, the on-disk file and the list icon must agree on a suffix.
            let file_name = rdm_ai::ensure_file_extension(
                &resource.name,
                resource.content_type.as_deref(),
                Some(&resource.url),
                Some(&resource.kind),
            );
            request.file_name = Some(file_name.clone());
            if let Some(credential) = credential {
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
            if let Ok(request) = model::prepare_request(&self.settings, request) {
                let id = self.push_download(request, source.to_string());
                created.push(file_name);
                created_ids.push(id);
            }
        }
        // 资源监控：本次解析的 token 归属到这批下载项（同一次解析共享）。
        self.attach_parse_usage_to_items(session_id, &created_ids);
        created
    }

    fn next_ai_request_id(&mut self) -> u64 {
        self.next_ai_request += 1;
        self.next_ai_request
    }

    // -----------------------------------------------------------------------
    // 资源解析页：会话消息 / Agent 工作流
    // -----------------------------------------------------------------------

    /// 资源解析页输入框提交：链接 → 复用同页面历史会话（没有才新建）并解析；
    /// 否则在当前会话里继续执行/问答。
    ///
    /// 语义边界（避免「一次输入到底进哪个会话」含糊）：
    /// - 消息里含 http(s) 链接 → 先按规范化 URL 找历史会话：命中则**复用**（不新建
    ///   重复历史），该会话正在解析时只切换过去；没命中才新建会话；
    /// - 不含链接（下载第 2 个/只要音频/普通问答）→ 作用在当前会话；
    /// - 当前没有会话且不含链接 → 新建一个会话承载这次对话。
    pub fn send_resolve_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.resolve_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let active = self.resolve.active();
        let now = model::now_ms();
        let route = match active {
            Some(session) => agent::route_turn(&text, &session.panel, now),
            None => agent::route_turn(&text, &model::AiPanelState::default(), now),
        };
        // 忙碌只阻止「在当前会话继续操作」；含链接的消息会切到/复用目标会话，
        // 因此后台解析/流式输出期间仍可发起新的解析。
        let starts_new_session = matches!(&route, AgentTurn::Plan { url: Some(_), .. });
        if let Some(session) = active {
            let busy = session.panel.chat_pending
                || session.plan.as_ref().is_some_and(AgentPlan::is_running);
            if busy && !starts_new_session {
                return;
            }
        }
        self.resolve_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.resolve_scroll.scroll_to_bottom();

        match route {
            AgentTurn::Plan {
                plan,
                url: Some(url),
                ..
            } => {
                // 链接入口：同一规范化页面 URL 复用历史会话（设计契约 §2.3）。
                // 来源语义：要求直接下载的 Agent 工作流记 `Agent`，只解析记 `Gui`。
                let origin = if plan.intent.wants_auto_download() {
                    ResolveOrigin::Agent
                } else {
                    ResolveOrigin::Gui
                };
                self.page = AppPage::Resolve;
                let request = rdm_ai::ParseRequest {
                    url,
                    title: None,
                    referer: None,
                    user_agent: None,
                    cookie: None,
                    hint: None,
                };
                let session_id = if let Some((session_id, parsing, _)) =
                    self.reusable_resolve_session(&request.url)
                {
                    if let Some(session) = self.resolve.get_mut(&session_id) {
                        session.panel.push_message(model::AiRole::User, &text);
                        if !parsing {
                            session
                                .panel
                                .push_message(model::AiRole::System, REUSE_SESSION_NOTICE);
                        }
                        session.touch(now);
                    }
                    self.resolve.mark_dirty();
                    if parsing {
                        // 该会话正在解析：只切换过去，不重复发起。
                        self.resolve.set_active(&session_id);
                        self.toast("该会话正在解析中，已切换过去", Severity::Info);
                        self.resolve_scroll.scroll_to_bottom();
                        cx.notify();
                        return;
                    }
                    let label =
                        model::url_host(&request.url).unwrap_or_else(|| request.url.clone());
                    self.toast(format!("已复用历史会话：{label}"), Severity::Info);
                    session_id
                } else {
                    // 新页面：新建会话留档，保留其它历史。
                    let session_id = self.resolve.create_session(origin, now);
                    if let Some(session) = self.resolve.get_mut(&session_id) {
                        session.panel.push_message(model::AiRole::User, &text);
                        session.touch(now);
                    }
                    self.resolve.mark_dirty();
                    session_id
                };
                self.begin_resolve_parse(&session_id, request, None, Some(plan), false, cx);
            }
            AgentTurn::Plan {
                plan,
                url: None,
                selection: Some(selection),
            } => {
                let session_id = self.ensure_active_session(ResolveOrigin::Gui, now);
                if let Some(session) = self.resolve.get_mut(&session_id) {
                    session.panel.push_message(model::AiRole::User, &text);
                }
                self.refresh_fallback_title(&session_id);
                self.resolve.mark_dirty();
                self.execute_agent_download_plan(&session_id, plan, selection, cx);
            }
            AgentTurn::Plan { mut plan, .. } => {
                // 防御：没有执行目标的计划不执行（不假装成功）。
                let session_id = self.ensure_active_session(ResolveOrigin::Gui, now);
                plan.fail("没有可执行目标", now);
                if let Some(session) = self.resolve.get_mut(&session_id) {
                    session.plan = Some(plan);
                    session.panel.push_message(model::AiRole::User, &text);
                    session.panel.push_message(
                        model::AiRole::Assistant,
                        "没有识别到可执行的链接或指令：请粘贴 http(s) 链接，或先解析一次再让我下载。",
                    );
                    session.touch(now);
                }
                self.refresh_fallback_title(&session_id);
                self.resolve.mark_dirty();
            }
            AgentTurn::Chat { prompt } => {
                let session_id = self.ensure_active_session(ResolveOrigin::Gui, now);
                if !self.ai_config.is_configured() {
                    if let Some(session) = self.resolve.get_mut(&session_id) {
                        session.panel.push_message(model::AiRole::User, &text);
                        session.panel.push_message(
                            model::AiRole::System,
                            "AI 未配置：普通问答不可用。但粘贴链接发送仍会直接解析并创建下载任务（设置 → AI 助手 只影响自然语言汇报）。",
                        );
                        session.touch(now);
                    }
                    self.refresh_fallback_title(&session_id);
                    self.resolve.mark_dirty();
                    cx.notify();
                    return;
                }
                let request_id = self.next_ai_request_id();
                if let Some(session) = self.resolve.get_mut(&session_id) {
                    session.chat_request = Some(request_id);
                    session.agent_summary_request = None;
                    session.panel.begin_chat_stream(&text);
                    session.touch(now);
                }
                self.refresh_fallback_title(&session_id);
                self.resolve.mark_dirty();
                self.engine.start_ai_chat_stream(
                    request_id,
                    self.ai_config.clone(),
                    agent::chat_system_prompt(),
                    prompt,
                );
            }
        }
        self.resolve_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 资源解析页的 URL 入口（design 18 §3.3）：只接受 http(s) 链接；不再解析自然语言。
    ///
    /// 复用同一 URL 的历史会话（与浏览器扩展/Omnibox 语义一致）；没有会话时新建。
    pub fn submit_resolve_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.resolve_input.read(cx).value().trim().to_string();
        if raw.is_empty() {
            return;
        }
        let url = match validate_url(&raw) {
            Ok(url) => url,
            Err(err) => {
                self.toast(
                    format!(
                        "{err}；资源解析页只接受链接，AI 调试/经验修补请在「解析经验」页使用助手"
                    ),
                    Severity::Warning,
                );
                return;
            }
        };
        self.resolve_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.page = AppPage::Resolve;
        let now = model::now_ms();
        let request = rdm_ai::ParseRequest {
            url,
            title: None,
            referer: None,
            user_agent: None,
            cookie: None,
            hint: None,
        };
        let session_id =
            if let Some((session_id, parsing, _)) = self.reusable_resolve_session(&request.url) {
                self.resolve.set_active(&session_id);
                if parsing {
                    self.toast("该会话正在解析中，已切换过去", Severity::Info);
                    self.resolve_scroll.scroll_to_bottom();
                    cx.notify();
                    return;
                }
                session_id
            } else {
                self.resolve.create_session(ResolveOrigin::Gui, now)
            };
        if let Some(session) = self.resolve.get_mut(&session_id) {
            // 历史会话线程保留一条可回看的输入；这不是聊天，只是本次解析的起点记录。
            session
                .panel
                .push_message(model::AiRole::User, request.url.clone());
            session.touch(now);
        }
        self.resolve.mark_dirty();
        self.begin_resolve_parse(&session_id, request, None, None, false, cx);
        self.resolve_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 确保有一个可写入的当前会话（没有就新建）；返回会话 id。
    fn ensure_active_session(&mut self, origin: ResolveOrigin, now_ms: u64) -> String {
        if let Some(id) = self.resolve.active_id().map(str::to_owned) {
            return id;
        }
        let id = self.resolve.create_session(origin, now_ms);
        self.page = AppPage::Resolve;
        id
    }

    /// 刷新确定性兜底标题（绝不覆盖 LLM 标题）：首条用户消息/来源 URL 变化后调用。
    fn refresh_fallback_title(&mut self, session_id: &str) {
        if let Some(session) = self.resolve.get_mut(session_id) {
            if session.title_source == TitleSource::Fallback {
                let fallback = session.fallback_title();
                session.title = fallback;
            }
        }
    }

    /// 从侧栏进入「资源解析」页：保留当前会话；没有会话时展示空态输入框。
    pub(crate) fn open_resolve_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page = AppPage::Resolve;
        self.resolve_input
            .update(cx, |input, cx| input.focus(window, cx));
        if self.resolve.active().is_some() {
            self.resolve_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// 「新建解析」：回到无会话空态（历史全部保留），聚焦输入框等待新地址。
    pub(crate) fn start_new_resolve_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resolve.clear_active();
        self.page = AppPage::Resolve;
        self.resolve_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// 点击历史会话行：切换到该会话（会话列表是唯一入口，不重复创建）。
    pub(crate) fn open_resolve_session(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.resolve.set_active(id) {
            return;
        }
        self.page = AppPage::Resolve;
        self.resolve_input
            .update(cx, |input, cx| input.focus(window, cx));
        self.resolve_scroll.scroll_to_bottom();
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 解析历史编辑：置顶 / 分组 / 删除（右键菜单 + 二次确认 dialog）
    // -----------------------------------------------------------------------

    /// 置顶 / 取消置顶会话（历史列表 pinned 优先排序）。
    fn toggle_resolve_pin(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(session) = self.resolve.get(id) else {
            return;
        };
        let pinned = !session.pinned;
        if !self.resolve.set_pinned(id, pinned) {
            return;
        }
        self.toast(
            if pinned {
                "已置顶解析会话"
            } else {
                "已取消置顶"
            },
            Severity::Info,
        );
        cx.notify();
    }

    /// 把会话移动到指定分组（`None` = 未分组）；编辑动作立即标记待落盘。
    fn set_resolve_session_group(&mut self, id: &str, group: Option<&str>, cx: &mut Context<Self>) {
        if !self.resolve.set_group(id, group) {
            return;
        }
        match normalize_session_group(group.unwrap_or_default()) {
            Some(group) => self.toast(format!("已移动到分组「{group}」"), Severity::Info),
            None => self.toast("已移动到「未分组」", Severity::Info),
        }
        cx.notify();
    }

    /// 折叠/展开一个历史分组（纯 UI 状态，不持久化；`None` = 「未分组」组）。
    fn toggle_resolve_group_collapsed(&mut self, group: Option<String>, cx: &mut Context<Self>) {
        if !self.resolve_collapsed_groups.remove(&group) {
            self.resolve_collapsed_groups.insert(group);
        }
        cx.notify();
    }

    /// 新建 / 重命名分组的单行 Input dialog；`rename_from = Some(old)` 表示重命名。
    ///
    /// 复用 `open_delete_confirm` 的 dialog 模式（不新建页面）；新建时输入为空不关闭，
    /// 重命名时输入为空 = 取消分组（`ResolveStore::rename_group` 语义）。
    fn open_resolve_group_dialog(
        &mut self,
        session_id: &str,
        rename_from: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.resolve.get(session_id).is_none() {
            return;
        }
        let renaming = rename_from.is_some();
        let title = if renaming {
            "重命名分组"
        } else {
            "新建分组"
        };
        let hint = if renaming {
            "重命名后，该分组下的全部会话一起移动；清空名称并确定 = 取消分组。"
        } else {
            "输入一个分组名称（例如：番剧 / 课程 / 素材）。同一页面的解析仍复用历史会话。"
        };
        let placeholder = if renaming {
            "分组名称（留空 = 取消分组）"
        } else {
            "分组名称"
        };
        let default_value = rename_from.clone().unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(default_value)
                .clean_on_escape()
        });
        let weak = cx.entity().downgrade();
        let session_id = session_id.to_string();
        let rename_from_ok = rename_from.clone();
        let input_ok = input.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let t = tokens(cx);
            let weak = weak.clone();
            let session_id = session_id.clone();
            let rename_from = rename_from_ok.clone();
            let input = input_ok.clone();
            dialog
                .title(title)
                .child(
                    v_flex()
                        .gap(px(8.0))
                        .child(Input::new(&input).w_full())
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(t.text_secondary)
                                .child(hint),
                        ),
                )
                .w(px(420.0))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("确定")
                        .cancel_text("取消"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let raw = input.read(cx).value();
                    let value = raw.trim().to_string();
                    // 新建分组必须有名字：保持 dialog 打开让用户补全。
                    if rename_from.is_none() && value.is_empty() {
                        return false;
                    }
                    let _ = weak.update(cx, |this, cx| {
                        this.apply_resolve_group_dialog(
                            &session_id,
                            rename_from.as_deref(),
                            &value,
                            cx,
                        );
                    });
                    true
                })
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// 应用分组 dialog 的输入：重命名整组（空 = 取消分组）或把会话移动到新分组。
    fn apply_resolve_group_dialog(
        &mut self,
        session_id: &str,
        rename_from: Option<&str>,
        value: &str,
        cx: &mut Context<Self>,
    ) {
        let new_group = normalize_session_group(value);
        match rename_from.and_then(normalize_session_group) {
            Some(old_group) => {
                let moved = self.resolve.rename_group(&old_group, value);
                if moved == 0 {
                    self.toast("分组没有变化", Severity::Info);
                } else {
                    // 折叠状态跟随分组重命名（旧组名 -> 新组名；取消分组时丢弃）。
                    if self
                        .resolve_collapsed_groups
                        .remove(&Some(old_group.clone()))
                    {
                        if let Some(new_group) = new_group.clone() {
                            self.resolve_collapsed_groups.insert(Some(new_group));
                        }
                    }
                    match new_group {
                        Some(new_group) => self.toast(
                            format!("分组已重命名为「{new_group}」（移动 {moved} 个会话）"),
                            Severity::Success,
                        ),
                        None => self.toast(
                            format!("已取消分组（{moved} 个会话回到「未分组」）"),
                            Severity::Info,
                        ),
                    }
                }
            }
            None => {
                let Some(group) = new_group else {
                    self.toast("分组名不能为空", Severity::Warning);
                    cx.notify();
                    return;
                };
                self.resolve.set_group(session_id, Some(&group));
                self.toast(format!("已移动到分组「{group}」"), Severity::Success);
            }
        }
        cx.notify();
    }

    /// 删除会话前的二次确认（不可撤销；下载任务不受影响）。
    fn open_resolve_delete_dialog(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.resolve.get(id) else {
            return;
        };
        let title = session.title.clone();
        let weak = cx.entity().downgrade();
        let id = id.to_string();
        window.open_dialog(cx, move |dialog, _, cx| {
            let t = tokens(cx);
            let weak = weak.clone();
            let id = id.clone();
            let title = title.clone();
            dialog
                .title("删除解析会话？")
                .child(
                    v_flex()
                        .gap(px(8.0))
                        .child(div().text_size(px(13.0)).child(format!(
                            "将删除会话「{title}」及其解析结果与聊天记录。"
                        )))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(t.text_secondary)
                                .child("此操作不可撤销；下载任务不受影响。删除后再次解析同一页面会新建会话。"),
                        ),
                )
                .w(px(460.0))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除会话")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| this.delete_resolve_session(&id, cx));
                    true
                })
        });
        cx.notify();
    }

    /// 执行删除：移除会话（当前会话被删时 store 自动回到空态）并提示。
    fn delete_resolve_session(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        if !self.resolve.remove(id) {
            return false;
        }
        self.toast("已删除解析会话", Severity::Info);
        cx.notify();
        true
    }

    // -----------------------------------------------------------------------
    // 会话事件归属辅助：AI 引擎事件只按 request id 找「拥有它的会话」，
    // 不假设事件一定属于当前可见会话（后台解析/聊天必须写回原会话）。
    // -----------------------------------------------------------------------

    fn session_id_by_parse_request(&self, request_id: u64) -> Option<String> {
        self.resolve
            .sessions()
            .iter()
            .find(|session| session.panel.request_id == request_id)
            .map(|session| session.id.clone())
    }

    fn session_id_by_chat_request(&self, request_id: u64) -> Option<String> {
        self.resolve
            .sessions()
            .iter()
            .find(|session| session.chat_request == Some(request_id))
            .map(|session| session.id.clone())
    }

    fn session_id_by_title_request(&self, request_id: u64) -> Option<String> {
        self.resolve
            .sessions()
            .iter()
            .find(|session| session.title_request == Some(request_id))
            .map(|session| session.id.clone())
    }

    /// 同页面复用查找（GUI 输入 + 浏览器扩展共用）：
    /// 返回 `(会话 id, 是否正在解析, 当前解析请求代号)`。
    ///
    /// 去重键 = `resolve_session::normalize_session_url`；命中最近更新的历史会话。
    fn reusable_resolve_session(&self, url: &str) -> Option<(String, bool, u64)> {
        let session = self.resolve.find_by_source_url(url)?;
        Some((
            session.id.clone(),
            session.panel.status == model::AiParseStatus::Parsing,
            session.panel.request_id,
        ))
    }

    /// 只有结果所属会话当前可见时才自动贴底（后台会话不抢滚动）。
    fn scroll_resolve_if_active(&mut self, session_id: &str) {
        if self.resolve.active_id() == Some(session_id) {
            self.resolve_scroll.scroll_to_bottom();
        }
    }

    /// 记录一次聊天/标题/汇报的模型消耗（只记账，不归属下载项）。
    fn record_chat_usage(
        &mut self,
        request_id: u64,
        usage: rdm_ai::TokenUsage,
        cx: &mut Context<Self>,
    ) {
        if usage.is_zero() {
            return;
        }
        let model = self.ai_config.model.clone();
        let cost = self.estimate_ai_cost(&usage);
        self.record_ai_usage(
            AiUsageKind::Chat,
            usage,
            Some(model),
            false,
            None,
            Some(request_id),
            Vec::new(),
            cost,
            cx,
        );
    }

    /// LLM 自动生成会话标题（失败/未配置时保留确定性 fallback，不阻塞解析）。
    fn maybe_start_title_generation(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if !self.ai_config.is_configured() {
            return;
        }
        let prompt = match self.resolve.get(session_id) {
            Some(session) if session.needs_title() && session.title_request.is_none() => {
                session.title_prompt()
            }
            _ => None,
        };
        let Some(prompt) = prompt else {
            return;
        };
        let request_id = self.next_ai_request_id();
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.title_request = Some(request_id);
        }
        self.resolve.mark_dirty();
        self.engine.start_ai_chat_stream(
            request_id,
            self.ai_config.clone(),
            TITLE_SYSTEM_PROMPT.to_string(),
            prompt,
        );
        cx.notify();
    }

    /// 解析完成：把结果写进工作流（成功 → 可选自动下载/汇报；失败 → 结构化原因）。
    fn finish_agent_resolve(
        &mut self,
        session_id: &str,
        report: rdm_ai::ParseReport,
        cx: &mut Context<Self>,
    ) {
        let Some(mut plan) = self
            .resolve
            .get_mut(session_id)
            .and_then(|session| session.plan.take())
        else {
            return;
        };
        let now = model::now_ms();
        let auto = plan.intent.wants_auto_download();
        let resources = self
            .resolve
            .get(session_id)
            .map(|session| session.panel.resources.clone())
            .unwrap_or_default();

        if report.resources.is_empty() {
            let detail = report
                .notes
                .iter()
                .map(|note| note.trim())
                .find(|note| !note.is_empty())
                .unwrap_or("未发现可下载资源")
                .to_string();
            plan.finish_step(
                AgentTool::Resolve,
                StepStatus::Failed,
                Some(detail.clone()),
                now,
            );
            plan.fail(detail, now);
            let summary = agent::resolve_summary(&plan, &report, &[]);
            plan.summary = Some(summary.clone());
            if let Some(session) = self.resolve.get_mut(session_id) {
                session.panel.last_plan_summary = Some(summary.clone());
                session
                    .panel
                    .push_message(model::AiRole::Assistant, summary);
                session.plan = Some(plan);
                session.agent_request = None;
                session.touch(now);
            }
            self.resolve.mark_dirty();
            self.maybe_start_title_generation(session_id, cx);
            self.scroll_resolve_if_active(session_id);
            cx.notify();
            return;
        }

        let detail = format!(
            "{} 个资源（{}）{}",
            report.resources.len(),
            kinds_label(&report),
            if report.ai_used { "，AI 参与" } else { "" }
        );
        plan.finish_step(AgentTool::Resolve, StepStatus::Done, Some(detail), now);

        let mut created: Vec<String> = Vec::new();
        if auto {
            plan.start_step(AgentTool::Download, now);
            let indices = agent::selected_resource_indices(&resources, DownloadSelection::All);
            created = self.create_downloads_for(session_id, indices);
            plan.created_downloads = created.clone();
            if created.is_empty() {
                plan.finish_step(
                    AgentTool::Download,
                    StepStatus::Failed,
                    Some("下载任务创建失败（检查保存目录/设置）".to_string()),
                    now,
                );
            } else {
                plan.finish_step(
                    AgentTool::Download,
                    StepStatus::Done,
                    Some(format!("已创建 {} 个下载任务", created.len())),
                    now,
                );
                self.toast(
                    format!("Agent 已创建 {} 个下载任务", created.len()),
                    Severity::Success,
                );
            }
        }

        let summary = agent::resolve_summary(&plan, &report, &created);
        plan.summary = Some(summary.clone());
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.last_plan_summary = Some(summary.clone());
            session.plan = Some(plan);
            session.touch(now);
        }
        if self.ai_config.is_configured() {
            self.start_agent_summary(session_id, &report, cx);
        } else {
            if let Some(session) = self.resolve.get_mut(session_id) {
                if let Some(plan) = session.plan.as_mut() {
                    plan.complete(Some("本地汇报".to_string()), now);
                }
                session
                    .panel
                    .push_message(model::AiRole::Assistant, summary);
                session.agent_request = None;
                session.touch(now);
            }
            self.resolve.mark_dirty();
            self.maybe_start_title_generation(session_id, cx);
            self.scroll_resolve_if_active(session_id);
            cx.notify();
        }
    }

    /// 解析失败 + Agent 工作流：结构化失败原因写回会话（不假装成功）。
    fn finish_agent_parse_failed(&mut self, session_id: &str, error: &str, cx: &mut Context<Self>) {
        let Some(mut plan) = self
            .resolve
            .get_mut(session_id)
            .and_then(|session| session.plan.take())
        else {
            return;
        };
        let now = model::now_ms();
        plan.fail(error.to_string(), now);
        let summary = plan
            .summary
            .clone()
            .unwrap_or_else(|| agent::failure_summary(error));
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.last_plan_summary = Some(summary.clone());
            session
                .panel
                .push_message(model::AiRole::Assistant, summary);
            session.plan = Some(plan);
            session.agent_request = None;
            session.touch(now);
        }
        self.resolve.mark_dirty();
        self.maybe_start_title_generation(session_id, cx);
        self.scroll_resolve_if_active(session_id);
        cx.notify();
    }

    /// 启动「执行汇报」流式模型调用（模型只汇报事实，不决定是否执行）。
    fn start_agent_summary(
        &mut self,
        session_id: &str,
        report: &rdm_ai::ParseReport,
        cx: &mut Context<Self>,
    ) {
        let prompt = match self.resolve.get(session_id) {
            Some(session) => session
                .plan
                .as_ref()
                .map(|plan| agent::summary_prompt(&plan.user_text, plan, report)),
            None => None,
        };
        let Some(prompt) = prompt else {
            return;
        };
        let system = agent::summary_system_prompt();
        let request_id = self.next_ai_request_id();
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.begin_assistant_stream();
            session.chat_request = Some(request_id);
            session.agent_summary_request = Some(request_id);
            if let Some(plan) = session.plan.as_mut() {
                plan.start_step(AgentTool::Summarize, model::now_ms());
            }
        }
        self.resolve.mark_dirty();
        self.engine
            .start_ai_chat_stream(request_id, self.ai_config.clone(), system, prompt);
        cx.notify();
    }

    /// 汇报流式结束：写消息 + 收尾计划。
    fn finish_agent_summary(&mut self, session_id: &str, reply: &str, cx: &mut Context<Self>) {
        let now = model::now_ms();
        let fallback = self
            .resolve
            .get(session_id)
            .and_then(|session| session.plan.as_ref().and_then(|plan| plan.summary.clone()))
            .unwrap_or_else(|| "工作流已完成。".to_string());
        let text = if reply.trim().is_empty() {
            fallback
        } else {
            reply.to_string()
        };
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.finish_chat_stream(&text);
            if let Some(plan) = session.plan.as_mut() {
                plan.summary = Some(text.clone());
                plan.finish_step(
                    AgentTool::Summarize,
                    StepStatus::Done,
                    Some("模型汇报".to_string()),
                    now,
                );
                plan.complete(None, now);
            }
            session.panel.last_plan_summary = Some(text);
            session.agent_request = None;
            session.agent_summary_request = None;
            session.touch(now);
        }
        self.resolve.mark_dirty();
        self.maybe_start_title_generation(session_id, cx);
        self.scroll_resolve_if_active(session_id);
        cx.notify();
    }

    /// 汇报失败：回退到确定性汇报（不让「模型挂了」掩盖已经执行的结果）。
    fn fail_agent_summary(&mut self, session_id: &str, error: &str, cx: &mut Context<Self>) {
        let now = model::now_ms();
        let fallback = self
            .resolve
            .get(session_id)
            .and_then(|session| session.plan.as_ref().and_then(|plan| plan.summary.clone()))
            .unwrap_or_else(|| "工作流已完成。".to_string());
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.fail_chat_stream(error);
            session
                .panel
                .push_message(model::AiRole::Assistant, fallback.clone());
            if let Some(plan) = session.plan.as_mut() {
                plan.summary = Some(fallback.clone());
                plan.finish_step(
                    AgentTool::Summarize,
                    StepStatus::Skipped,
                    Some("模型汇报失败，已展示本地汇报".to_string()),
                    now,
                );
                plan.complete(None, now);
            }
            session.panel.last_plan_summary = Some(fallback);
            session.agent_request = None;
            session.agent_summary_request = None;
            session.touch(now);
        }
        self.resolve.mark_dirty();
        self.maybe_start_title_generation(session_id, cx);
        self.scroll_resolve_if_active(session_id);
        cx.notify();
    }

    /// 对会话里已有的解析结果直接创建下载任务（「下载全部/所选」或后续指令）。
    fn execute_agent_download_plan(
        &mut self,
        session_id: &str,
        mut plan: AgentPlan,
        selection: DownloadSelection,
        cx: &mut Context<Self>,
    ) {
        let now = model::now_ms();
        let (resources, source_url) = match self.resolve.get(session_id) {
            Some(session) => (
                session.panel.resources.clone(),
                session.panel.source_url.clone(),
            ),
            None => (Vec::new(), String::new()),
        };
        let indices = agent::selected_resource_indices(&resources, selection);
        let created = self.create_downloads_for(session_id, indices);
        plan.created_downloads = created.clone();
        if created.is_empty() {
            plan.finish_step(
                AgentTool::Download,
                StepStatus::Failed,
                Some("没有创建任何下载任务".to_string()),
                now,
            );
        } else {
            plan.finish_step(
                AgentTool::Download,
                StepStatus::Done,
                Some(format!("已创建 {} 个下载任务", created.len())),
                now,
            );
        }
        let summary = agent::download_summary(selection, &source_url, &created);
        plan.summary = Some(summary.clone());
        plan.finish_step(
            AgentTool::Summarize,
            StepStatus::Done,
            Some("本地汇报".to_string()),
            now,
        );
        plan.complete(None, now);
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.last_plan_summary = Some(summary.clone());
            session
                .panel
                .push_message(model::AiRole::Assistant, summary);
            session.plan = Some(plan);
            session.agent_request = None;
            session.touch(now);
        }
        self.resolve.mark_dirty();
        if !created.is_empty() {
            self.toast(
                format!("已创建 {} 个下载任务", created.len()),
                Severity::Success,
            );
        }
        self.maybe_start_title_generation(session_id, cx);
        self.scroll_resolve_if_active(session_id);
        cx.notify();
    }

    fn agent_download_now(
        &mut self,
        session_id: &str,
        selection: DownloadSelection,
        cx: &mut Context<Self>,
    ) {
        let source_url = self
            .resolve
            .get(session_id)
            .map(|session| session.panel.source_url.clone())
            .unwrap_or_default();
        let plan = AgentPlan::new(
            AgentIntent::DownloadExisting(selection),
            &source_url,
            "下载",
            model::now_ms(),
        );
        self.execute_agent_download_plan(session_id, plan, selection, cx);
    }

    /// 「重新解析/重试解析」：优先复用会话里的工作流意图；纯解析会话（扩展/GUI 直接
    /// 解析没有 `AgentPlan`）直接用会话来源 URL 重新解析。
    ///
    /// Design 17 §2.4：重新解析 = **跳过本地解析经验、强制 AI 重新探索**（未配置 AI 时
    /// 启发式兜底），并保留上次更丰富的资源结果（§2.5）——否则「重新解析」没有意义。
    ///
    /// U-05：标题栏按钮的可用条件是「有来源 URL 且没有在跑」，因此这里必须对
    /// 无 plan 的会话也有响应，不能静默 return。
    fn retry_agent_plan(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let plan_info = self.resolve.get(session_id).and_then(|session| {
            session
                .plan
                .as_ref()
                .map(|plan| (plan.intent, plan.user_text.clone(), plan.source_url.clone()))
        });
        let Some((intent, user_text, source_url)) = plan_info else {
            let source_url = self
                .resolve
                .get(session_id)
                .map(|session| session.panel.source_url.clone())
                .unwrap_or_default();
            if source_url.is_empty() {
                self.toast("该会话没有可重新解析的来源地址", Severity::Warning);
                cx.notify();
                return;
            }
            if let Some(session) = self.resolve.get_mut(session_id) {
                session
                    .panel
                    .push_message(model::AiRole::User, "重新解析（跳过经验，调用 AI）");
            }
            let request = rdm_ai::ParseRequest {
                url: source_url,
                title: None,
                referer: None,
                user_agent: None,
                cookie: None,
                hint: None,
            };
            self.begin_resolve_parse(session_id, request, None, None, true, cx);
            return;
        };
        if source_url.is_empty() {
            if let AgentIntent::DownloadExisting(selection) = intent {
                if let Some(session) = self.resolve.get_mut(session_id) {
                    session.panel.push_message(model::AiRole::User, "重试下载");
                }
                let plan = AgentPlan::new(intent, "", &user_text, model::now_ms());
                self.execute_agent_download_plan(session_id, plan, selection, cx);
            }
            return;
        }
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.push_message(
                model::AiRole::User,
                format!("重新解析（跳过经验，调用 AI）：{user_text}"),
            );
        }
        let plan = AgentPlan::new(intent, &source_url, &user_text, model::now_ms());
        let request = rdm_ai::ParseRequest {
            url: source_url,
            title: None,
            referer: None,
            user_agent: None,
            cookie: None,
            hint: None,
        };
        self.begin_resolve_parse(session_id, request, None, Some(plan), true, cx);
    }

    /// 停止生成：中止当前会话的流式输出；工作流仍在进行时计划标记为「已取消」
    /// （`AgentPlanState::Cancelled` 必须有生产代码路径，见审计 F-20）。
    /// 解析请求本身无法取消——迟到结果会被 request id 丢弃。
    /// 旧聊天流的中止（design 18 起解析页不再有 chat 输入框；保留给测试钩子/未来复用）。
    #[allow(dead_code)]
    fn stop_resolve_stream(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let now = model::now_ms();
        if let Some(session) = self.resolve.get_mut(session_id) {
            session.panel.abort_chat_stream();
            session.chat_request = None;
            session.agent_summary_request = None;
            session.agent_request = None;
            if let Some(plan) = session.plan.as_mut() {
                if plan.is_running() {
                    plan.cancel("已停止生成", now);
                }
            }
            session.touch(now);
        }
        self.resolve.mark_dirty();
        self.scroll_resolve_if_active(session_id);
        cx.notify();
    }
}

/// 会话标题生成的 system prompt：只产出标题，不参与解析/工作流判断。
const TITLE_SYSTEM_PROMPT: &str =
    "你是下载管理器的会话标题生成器。根据用户提供的解析上下文，输出一个简短中文标题。\
只输出标题本身：不超过 20 个字，不要引号、不要编号、不要标点结尾、不要解释或换行。";

/// 解析结果里出现过的资源类型（中文、去重），用于工作流步骤摘要。
fn kinds_label(report: &rdm_ai::ParseReport) -> String {
    let mut labels: Vec<&'static str> = Vec::new();
    for resource in &report.resources {
        let label = agent::resource_kind_label(&resource.kind);
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        "无".to_string()
    } else {
        labels.join("、")
    }
}

/// 引擎消息泵：合并批处理，每批最多 1024 条。
async fn pump_loop(
    this: gpui::WeakEntity<RdmApp>,
    cx: &mut gpui::AsyncApp,
    mut rx: UnboundedReceiver<EngineMsg>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::with_capacity(16);
        batch.push(first);
        while batch.len() < 1024 {
            match rx.try_recv() {
                Ok(message) => batch.push(message),
                Err(_) => break,
            }
        }
        if this
            .update(cx, |app, cx| app.apply_batch(batch, cx))
            .is_err()
        {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// 文件删除辅助（与旧 UI 保持同一语义）
// ---------------------------------------------------------------------------

fn append_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

fn delete_targets(item: &DownloadItem, state_dir: &std::path::Path) -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = Vec::new();
    if let Some(path) = &item.path {
        targets.push(path.clone());
        targets.push(append_suffix(path, ".part"));
        targets.push(append_suffix(path, ".part.json"));
    }
    if let Some(dir) = item.save_dir.as_ref().or(item.request.output_dir.as_ref()) {
        let base = dir.join(&item.file_name);
        targets.push(base.clone());
        targets.push(append_suffix(&base, ".part"));
        targets.push(append_suffix(&base, ".part.json"));
    }
    // 旧版 GUI 的临时目录兜底（重启恢复时可能只剩 .part 文件）。
    if targets.is_empty() {
        let base = state_dir.join(&item.file_name);
        targets.push(append_suffix(&base, ".part"));
        targets.push(append_suffix(&base, ".part.json"));
    }
    let mut deduped: Vec<PathBuf> = Vec::new();
    for path in targets {
        if !deduped.contains(&path) {
            deduped.push(path);
        }
    }
    deduped
}

fn delete_target_files(targets: &[PathBuf]) -> (usize, Vec<String>) {
    let mut removed = 0usize;
    let mut errors = Vec::new();
    for path in targets {
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(err) => errors.push(format!("{}: {err}", path.display())),
        }
    }
    (removed, errors)
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

const ROW_ACTIONS_WIDTH: f32 = 84.0;

impl RdmApp {
    fn on_row_click(
        &mut self,
        id: &str,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 双击：默认模式打开文件/详情；多选模式下忽略第二次点击（避免刚选中的行
        // 因双击的第二次 click 立即被取消选中）。
        if event.click_count() >= 2 {
            self.focused = Some(id.to_string());
            if !self.multi_select {
                if !self.selection.contains(id) {
                    self.selection.clear();
                    self.selection.insert(id.to_string());
                }
                self.anchor = Some(id.to_string());
                self.activate_focused(cx);
            }
            self.list_focus.focus(window);
            cx.notify();
            return;
        }
        let modifiers = event.modifiers();
        if modifiers.control {
            if !self.selection.remove(id) {
                self.selection.insert(id.to_string());
            }
            self.focused = Some(id.to_string());
            self.anchor = Some(id.to_string());
        } else if modifiers.shift {
            if let Some(anchor) = self.anchor.clone() {
                if let Some(anchor_index) = self.index_of(&anchor) {
                    if let Some(target_index) = self.index_of(id) {
                        let (lo, hi) = if anchor_index <= target_index {
                            (anchor_index, target_index)
                        } else {
                            (target_index, anchor_index)
                        };
                        self.selection.clear();
                        for index in lo..=hi {
                            self.selection.insert(self.items[index].id.clone());
                        }
                    }
                }
            } else {
                self.selection.clear();
                self.selection.insert(id.to_string());
                self.anchor = Some(id.to_string());
            }
            self.focused = Some(id.to_string());
        } else if self.multi_select {
            // 多选模式：普通点击切换该行；再次点击已选行 = 取消选中。
            if !self.selection.remove(id) {
                self.selection.insert(id.to_string());
            }
            self.focused = Some(id.to_string());
            self.anchor = Some(id.to_string());
        } else if self.selection.contains(id) {
            // 默认模式：再次点击已选中的记录取消选中（行高亮一并消失）。
            self.selection.clear();
            self.anchor = None;
        } else {
            self.selection.clear();
            self.selection.insert(id.to_string());
            self.focused = Some(id.to_string());
            self.anchor = Some(id.to_string());
        }
        self.list_focus.focus(window);
        // 取消选中后不能留下「焦点高亮」让用户误以为仍选中（2026-10-22 用户反馈）。
        if self.selection.is_empty() {
            self.focused = None;
            self.anchor = None;
        }
        cx.notify();
    }

    fn render_titlebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let ipc_ok = self.ipc_warning.is_none();
        h_flex()
            .h(px(theme::TITLEBAR_H))
            .flex_shrink_0()
            .items_center()
            .gap(px(12.0))
            .px(px(16.0))
            .bg(t.bg_surface)
            .border_b_1()
            .border_color(t.border_subtle)
            // 品牌
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .flex_none()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(24.0))
                            .rounded(px(theme::RADIUS_SM))
                            .bg(t.accent_soft)
                            .child(RdmIcon::Download.sized(15.0, t.accent)),
                    )
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_primary)
                            .child("RDM"),
                    ),
            )
            // 万能输入栏
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .max_w(px(720.0))
                    .mx(px(12.0))
                    .child(
                        Input::new(&self.omnibox)
                            .bordered(false)
                            .appearance(true)
                            .cleanable(false)
                            .prefix(RdmIcon::Search.sized(14.0, t.text_tertiary))
                            .suffix(
                                h_flex()
                                    .gap(px(2.0))
                                    .child(crate::ui::widgets::kbd(&t, "Ctrl"))
                                    .child(crate::ui::widgets::kbd(&t, "K")),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .flex_none()
                    // 浏览器接管状态（正常时弱化，异常才醒目——契约 §6.4）
                    .when(ipc_ok, |this| {
                        this.child(
                            h_flex()
                                .gap(px(4.0))
                                .items_center()
                                .child(status_dot(t.success, 6.0))
                                .child(status_text(&t, "浏览器接管已连接")),
                        )
                    })
                    .child(
                        Button::new("pause-all")
                            .ghost()
                            .compact()
                            .icon(RdmIcon::Pause.icon())
                            .tooltip("全部暂停")
                            .on_click(cx.listener(|this, _, _, cx| this.pause_all(cx))),
                    )
                    .child(icon_button(
                        "open-monitor",
                        RdmIcon::Gauge,
                        "资源监控（Ctrl+Shift+M）",
                        cx.listener(|this, _, _, cx| {
                            let page = if this.page == AppPage::Monitor {
                                AppPage::Downloads
                            } else {
                                AppPage::Monitor
                            };
                            this.set_page(page, cx);
                        }),
                    ))
                    .child(icon_button(
                        "open-settings",
                        RdmIcon::Settings,
                        "设置（Ctrl+,）",
                        cx.listener(|this, _, _, cx| this.open_settings(cx)),
                    )),
            )
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let collapsed = self.sidebar_collapsed || self.compact_window;
        let width = if collapsed {
            theme::SIDEBAR_COLLAPSED_W
        } else {
            theme::SIDEBAR_W
        };
        // 工作区（应用级页面）：资源解析 / 解析经验 / 资源监控。下载库是默认页，不占侧栏
        // nav 行（2026-10-22 冗余移除；任一智能视图/分类行点击即回到下载库，Esc 亦返回）。
        let mut workspace = v_flex().gap(px(2.0)).px(px(0.0));
        for page in [AppPage::Resolve, AppPage::Experience, AppPage::Monitor] {
            let selected = self.page == page;
            let icon = match page {
                AppPage::Resolve => RdmIcon::Sparkle,
                AppPage::Experience => RdmIcon::BookOpen,
                AppPage::Monitor => RdmIcon::Gauge,
                AppPage::Downloads => RdmIcon::Inbox,
            };
            let count = match page {
                AppPage::Resolve => Some(self.resolve.len()),
                AppPage::Experience => Some(self.experience_count()),
                AppPage::Monitor => None,
                AppPage::Downloads => None,
            };
            workspace = workspace.child(nav_item(
                &t,
                ElementId::Name(format!("page-{}", page.label_zh()).into()),
                icon,
                page.label_zh(),
                count,
                selected,
                collapsed,
                cx.listener(move |this, _, window, cx| match page {
                    AppPage::Resolve => this.open_resolve_page(window, cx),
                    AppPage::Experience => this.open_experience_page(cx),
                    _ => this.set_page(page, cx),
                }),
            ));
        }

        let mut nav = v_flex().gap(px(2.0)).px(px(0.0));
        for view in ViewFilter::PRIMARY {
            let count = view.count(&self.items);
            let selected = self.page == AppPage::Downloads && self.filter == view;
            let index = view.index();
            nav = nav.child(nav_item(
                &t,
                ElementId::Name(format!("nav-{index}").into()),
                view.icon(),
                view.label_zh(),
                Some(count),
                selected,
                collapsed,
                cx.listener(move |this, _, _, cx| {
                    this.page = AppPage::Downloads;
                    this.filter = ViewFilter::from_index(index).unwrap_or(ViewFilter::All);
                    cx.notify();
                }),
            ));
        }
        nav = nav.child(div().h(px(8.0)));
        for view in ViewFilter::SOURCES {
            let count = view.count(&self.items);
            let selected = self.page == AppPage::Downloads && self.filter == view;
            let index = view.index();
            nav = nav.child(nav_item(
                &t,
                ElementId::Name(format!("nav-{index}").into()),
                view.icon(),
                view.label_zh(),
                Some(count),
                selected,
                collapsed,
                cx.listener(move |this, _, _, cx| {
                    this.page = AppPage::Downloads;
                    this.filter = ViewFilter::from_index(index).unwrap_or(ViewFilter::All);
                    cx.notify();
                }),
            ));
        }
        if !collapsed {
            nav = nav.child(div().h(px(8.0)));
            nav = nav.child(section_label(&t, "分类"));
        }
        for category in FileCategory::ALL {
            let view = ViewFilter::Category(category);
            let count = view.count(&self.items);
            let selected = self.page == AppPage::Downloads && self.filter == view;
            let index = view.index();
            nav = nav.child(nav_item(
                &t,
                ElementId::Name(format!("nav-{index}").into()),
                category.icon(),
                category.label_zh(),
                Some(count),
                selected,
                collapsed,
                cx.listener(move |this, _, _, cx| {
                    this.page = AppPage::Downloads;
                    this.filter = ViewFilter::from_index(index).unwrap_or(ViewFilter::All);
                    cx.notify();
                }),
            ));
        }

        v_flex()
            .flex_none()
            .w(px(width))
            .h_full()
            .bg(t.bg_surface)
            .border_r_1()
            .border_color(t.border_subtle)
            .py(px(8.0))
            .child(if collapsed {
                div().into_any_element()
            } else {
                section_label(&t, "工作区").into_any_element()
            })
            .child(workspace)
            .child(div().h(px(4.0)))
            .child(nav)
            .child(div().flex_1())
            .child(divider(&t))
            .child(
                v_flex()
                    .px(px(12.0))
                    .py(px(8.0))
                    .gap(px(6.0))
                    .when(!collapsed, |this| {
                        let dir_text = self
                            .settings
                            .default_dir
                            .as_ref()
                            .map(|dir| dir.display().to_string())
                            .unwrap_or_else(|| "未设置保存目录".to_string());
                        this.child(
                            h_flex()
                                .gap(px(6.0))
                                .items_center()
                                .child(RdmIcon::Disk.sized(13.0, t.text_tertiary))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .text_size(px(11.0))
                                        .text_color(t.text_secondary)
                                        .child(dir_text),
                                ),
                        )
                        .when_some(self.disk_free_total, |this, (free, total)| {
                            let used = total.saturating_sub(free);
                            let fraction = if total > 0 {
                                used as f32 / total as f32
                            } else {
                                0.0
                            };
                            this.child(progress_bar(&t, Some(fraction), 4.0)).child(
                                div().text_size(px(10.5)).text_color(t.text_tertiary).child(
                                    format!(
                                        "剩余 {} / 共 {}",
                                        model::format_bytes(free),
                                        model::format_bytes(total)
                                    ),
                                ),
                            )
                        })
                        .child(
                            h_flex()
                                .id("sidebar-open-monitor")
                                .items_center()
                                .gap(px(4.0))
                                .px(px(4.0))
                                .py(px(3.0))
                                .rounded(px(theme::RADIUS_SM))
                                .cursor_pointer()
                                .hover(|style| style.bg(t.bg_hover))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_page(AppPage::Monitor, cx);
                                }))
                                .child(RdmIcon::Gauge.sized(13.0, t.accent))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(t.accent)
                                        .child("查看资源监控"),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(4.0))
                            .when(!collapsed, |this| {
                                this.child(div().flex_1()).child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(t.text_tertiary)
                                        .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
                                )
                            })
                            .child(icon_button(
                                "sidebar-settings",
                                RdmIcon::Settings,
                                "设置",
                                cx.listener(|this, _, _, cx| this.open_settings(cx)),
                            )),
                    ),
            )
    }

    /// 资源监控页（数据来自 `monitor` 模块，渲染在 `views/monitor.rs`）。
    fn render_monitor(&self, cx: &Context<Self>) -> AnyElement {
        let t = tokens(cx);
        let entity = cx.entity();
        let state = MonitorViewState {
            snapshot: self.monitor_snapshot(),
            scanning: self.monitor_scanning(),
            error: self.monitor_error(),
            last_scan_label: self.monitor_last_scan_label(),
            ai_configured: self.monitor_ai_configured(),
            download_dir: self.monitor_download_dir(),
            now_ms: model::now_ms(),
        };
        let jump_category = {
            let entity = entity.clone();
            Box::new(
                move |category: FileCategory,
                      _event: &ClickEvent,
                      _: &mut Window,
                      app: &mut App| {
                    entity.update(app, |this, cx| this.jump_to_category(category, cx));
                },
            ) as MonitorCallbacksJump
        };
        let select_item = {
            let entity = entity.clone();
            Box::new(
                move |id: &str, _event: &ClickEvent, _: &mut Window, app: &mut App| {
                    let id = id.to_owned();
                    entity.update(app, |this, cx| this.select_item_in_library(&id, cx));
                },
            ) as MonitorCallbacksSelect
        };
        let open_item_folder = {
            let entity = entity.clone();
            Box::new(
                move |id: &str, _event: &ClickEvent, _: &mut Window, app: &mut App| {
                    let id = id.to_owned();
                    entity.update(app, |this, cx| this.open_folder_by_id(&id, cx));
                },
            ) as MonitorCallbacksSelect
        };
        let callbacks = MonitorCallbacks {
            on_rescan: Box::new(cx.listener(|this, _, _, cx| this.rescan_monitor(cx))),
            on_open_download_dir: Box::new(
                cx.listener(|this, _, _, cx| this.open_monitor_download_dir(cx)),
            ),
            on_open_state_dir: Box::new(
                cx.listener(|this, _, _, cx| this.open_monitor_state_dir(cx)),
            ),
            on_open_ai_settings: Box::new(
                cx.listener(|this, _, _, cx| this.open_monitor_ai_settings(cx)),
            ),
            on_jump_category: jump_category,
            on_select_item: select_item,
            on_open_item_folder: open_item_folder,
        };
        render_monitor_page(&t, &self.monitor_scroll, state, callbacks)
    }

    fn render_list_header(&self, cx: &Context<Self>, visible_count: usize) -> AnyElement {
        let t = tokens(cx);
        let selected = self.selection.len();
        // 批量操作条只在「多选模式 + 已选」出现；默认模式的选择只服务详情/检查器，
        // 不再用批量条打断列表标题，也不显示左侧候选框。
        if self.multi_select && selected > 0 {
            return self.render_bulk_header(cx, selected);
        }
        let all_selected = self.all_visible_selected(cx);
        h_flex()
            .h(px(theme::LIST_HEADER_H))
            .flex_shrink_0()
            .px(px(16.0))
            .gap(px(8.0))
            .items_center()
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(t.text_primary)
                    .child(self.filter.label_zh()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(if self.multi_select {
                        t.accent
                    } else {
                        t.text_tertiary
                    })
                    .child(if self.multi_select {
                        format!("已选 {selected} / {visible_count}")
                    } else {
                        format!("{visible_count}")
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("sort")
                    .ghost()
                    .small()
                    .icon(RdmIcon::Sort.icon())
                    .label(self.sort.label_zh())
                    .tooltip(format!(
                        "排序：{}（{}）— 点击切换字段，Shift+点击切换方向",
                        self.sort.label_zh(),
                        if self.sort_ascending {
                            "升序"
                        } else {
                            "降序"
                        }
                    ))
                    .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                        if ev.modifiers().shift {
                            this.sort_ascending = !this.sort_ascending;
                        } else {
                            this.sort = this.sort.next();
                            this.sort_ascending = this.sort.default_ascending();
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("density")
                    .ghost()
                    .small()
                    .icon(RdmIcon::Density.icon())
                    .tooltip(if self.density == Density::Comfortable {
                        "切换为紧凑列表"
                    } else {
                        "切换为舒适列表"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.density = match this.density {
                            Density::Comfortable => Density::Compact,
                            _ => Density::Comfortable,
                        };
                        this.settings.density = this.density;
                        this.persist_settings();
                        cx.notify();
                    })),
            )
            // 右上角「多选」开关（原「全选」位置）：默认无候选框；打开后才显示
            // 行左侧复选框与「全选/取消全选」。
            .child({
                let mut button = Button::new("multi-select")
                    .small()
                    .icon(RdmIcon::CheckCircle.icon())
                    .label("多选")
                    .tooltip(if self.multi_select {
                        "退出多选（Esc）"
                    } else {
                        "多选：显示候选框并批量操作"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_multi_select(cx)));
                if self.multi_select {
                    button = button.primary();
                } else {
                    button = button.ghost();
                }
                button
            })
            .when(self.multi_select, |this| {
                this.child(
                    Button::new("select-all-filtered")
                        .ghost()
                        .small()
                        .label(if all_selected {
                            "取消全选"
                        } else {
                            "全选"
                        })
                        .tooltip("全选/取消全选当前筛选结果（Ctrl+A）")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.all_visible_selected(cx) {
                                this.clear_selection(cx);
                            } else {
                                this.select_all(cx);
                            }
                        })),
                )
            })
            .into_any_element()
    }

    fn render_bulk_header(&self, cx: &Context<Self>, selected: usize) -> AnyElement {
        let t = tokens(cx);
        let all_selected = self.all_visible_selected(cx);
        h_flex()
            .h(px(theme::BULK_BAR_H))
            .flex_shrink_0()
            .px(px(16.0))
            .gap(px(8.0))
            .items_center()
            .bg(t.accent_soft)
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(t.accent)
                    .child(format!("已选 {selected} 项")),
            )
            .child(
                Button::new("bulk-select-toggle")
                    .ghost()
                    .small()
                    .label(if all_selected {
                        "取消全选"
                    } else {
                        "全选"
                    })
                    .tooltip("全选/取消全选当前筛选结果（Ctrl+A）")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.all_visible_selected(cx) {
                            this.clear_selection(cx);
                        } else {
                            this.select_all(cx);
                        }
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("bulk-pause")
                    .ghost()
                    .small()
                    .icon(RdmIcon::Pause.icon())
                    .label("暂停")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let ids: Vec<String> = this.selection.iter().cloned().collect();
                        for id in ids {
                            this.pause_by_id(&id, cx);
                        }
                    })),
            )
            .child(
                Button::new("bulk-resume")
                    .ghost()
                    .small()
                    .icon(RdmIcon::Play.icon())
                    .label("继续")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let ids: Vec<String> = this.selection.iter().cloned().collect();
                        for id in ids {
                            this.resume_by_id(&id, cx);
                        }
                    })),
            )
            .child(
                Button::new("bulk-remove")
                    .ghost()
                    .small()
                    .icon(RdmIcon::Trash.icon())
                    .label("移除记录")
                    .on_click(cx.listener(|this, _, _, cx| this.remove_selected(cx))),
            )
            .child(
                Button::new("bulk-exit-multi")
                    .ghost()
                    .small()
                    .label("退出多选")
                    .tooltip("退出多选（Esc）")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_multi_select(cx))),
            )
            .into_any_element()
    }

    fn remove_selected(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.selection.iter().cloned().collect();
        for id in ids {
            if let Some(index) = self.index_of(&id) {
                self.remove_item_undoable(index, cx);
            }
        }
        self.selection.clear();
        cx.notify();
    }

    fn compact_rows(&self) -> bool {
        self.compact_window || self.density == Density::Compact
    }

    fn row_height(&self) -> f32 {
        if self.compact_rows() {
            theme::ROW_COMPACT
        } else {
            theme::ROW_COMFORTABLE
        }
    }

    fn render_row(&self, index: usize, compact: bool, cx: &Context<Self>) -> AnyElement {
        let t = tokens(cx);
        let item = &self.items[index];
        let id = item.id.clone();
        let selected = self.selection.contains(&id);
        let focused = self.focused.as_deref() == Some(id.as_str());
        let state = item.state;
        let stopping = !item.stop_request.is_none();
        let state_label = item
            .stop_request
            .label_zh()
            .map(str::to_string)
            .unwrap_or_else(|| state.label_zh().to_string());
        let (state_fg, state_bg) = match state {
            ItemState::Downloading => (t.info, t.info_soft),
            ItemState::Paused => (t.warning, t.warning_soft),
            ItemState::Completed => (t.success, t.success_soft),
            ItemState::Failed => (t.danger_text, t.danger_soft),
            ItemState::Cancelled => (t.text_tertiary, t.bg_hover),
        };
        let row_h = if compact {
            theme::ROW_COMPACT
        } else {
            theme::row_height(self.density)
        };
        let bg = if selected {
            t.bg_selected
        } else if focused {
            t.bg_hover
        } else {
            t.bg_app
        };
        let fraction = item.progress_fraction().map(|value| value as f32);
        let row_id = id.clone();
        let row_id2 = id.clone();
        let row_id3 = id.clone();
        let row_id4 = id.clone();

        let header_line = h_flex()
            .w_full()
            .min_w(px(0.0))
            .gap(px(8.0))
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(t.text_primary)
                    .child(item.file_name.clone()),
            )
            .when(state == ItemState::Downloading, |this| {
                this.child(mono_text(item.speed_text(), 12.5, t.text_secondary))
                    .child(mono_text(item.percent_text(), 12.5, t.text_primary))
                    .child(mono_text(item.eta_text(), 12.5, t.text_tertiary))
            })
            .when(state == ItemState::Paused, |this| {
                this.child(mono_text(item.percent_text(), 12.5, t.text_primary))
            })
            .when(
                state != ItemState::Downloading && state != ItemState::Paused,
                |this| this.child(mono_text(item.size_text(), 12.5, t.text_tertiary)),
            )
            .when(state == ItemState::Downloading && !compact, |this| {
                this.child(render_sparkline(item, t.accent, 16.0))
            })
            .child(status_pill(
                &t,
                RdmIcon::for_state(state),
                state_label.clone(),
                state_fg,
                state_bg,
            ));

        let meta_line = h_flex()
            .w_full()
            .min_w(px(0.0))
            .gap(px(8.0))
            .items_center()
            .text_size(px(12.0))
            .text_color(t.text_secondary)
            .child(
                h_flex()
                    .gap(px(4.0))
                    .items_center()
                    .child(source_icon(&item.source).sized(12.0, t.text_tertiary))
                    .child(meta_text(&t, source_label(&item.source))),
            )
            .child(meta_text(&t, format!("· {}", item.connections_text())))
            .when(state == ItemState::Paused, |this| {
                this.child(meta_text(&t, format!("· {}", item.downloaded_text())))
            })
            .when(state == ItemState::Downloading, |this| {
                this.child(meta_text(
                    &t,
                    format!("· {}", item.note.clone().unwrap_or_default()),
                ))
            })
            .when(state == ItemState::Completed, |this| {
                if let Some(path) = &item.path {
                    this.child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(t.text_tertiary)
                            .child(path.display().to_string()),
                    )
                } else {
                    this
                }
            })
            .when(
                state == ItemState::Failed || state == ItemState::Cancelled,
                |this| {
                    this.child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(t.danger_text)
                            .child(item.error.clone().unwrap_or_else(|| "未知错误".to_string())),
                    )
                },
            );

        let body = if compact {
            v_flex()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(2.0))
                .child(header_line)
                .when(
                    state == ItemState::Downloading || state == ItemState::Paused,
                    |this| {
                        let color = if state == ItemState::Paused {
                            t.warning
                        } else {
                            t.accent
                        };
                        this.child(progress_bar_colored(&t, fraction, 2.0, color))
                    },
                )
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(4.0))
                .child(header_line)
                .when(
                    state == ItemState::Downloading || state == ItemState::Paused,
                    |this| {
                        let color = match state {
                            ItemState::Paused => t.warning,
                            _ => t.accent,
                        };
                        this.child(progress_bar_colored(&t, fraction, 4.0, color))
                    },
                )
                .child(meta_line)
                .into_any_element()
        };

        let actions = h_flex()
            .flex_none()
            .w(px(ROW_ACTIONS_WIDTH))
            .justify_end()
            .gap(px(2.0))
            .items_center()
            .child(match state {
                ItemState::Downloading if !stopping => {
                    let target = row_id.clone();
                    icon_button(
                        ElementId::Name(format!("pause-{row_id}").into()),
                        RdmIcon::Pause,
                        "暂停下载",
                        cx.listener(move |this, _, _, cx| this.pause_by_id(&target, cx)),
                    )
                    .into_any_element()
                }
                ItemState::Paused => {
                    let target = row_id2.clone();
                    icon_button(
                        ElementId::Name(format!("resume-{row_id}").into()),
                        RdmIcon::Play,
                        "继续下载",
                        cx.listener(move |this, _, _, cx| this.resume_by_id(&target, cx)),
                    )
                    .into_any_element()
                }
                ItemState::Failed | ItemState::Cancelled => {
                    let target = row_id3.clone();
                    icon_button(
                        ElementId::Name(format!("retry-{row_id}").into()),
                        RdmIcon::Retry,
                        "重试",
                        cx.listener(move |this, _, _, cx| this.retry_by_id(&target, cx)),
                    )
                    .into_any_element()
                }
                ItemState::Completed => {
                    let target = row_id4.clone();
                    icon_button(
                        ElementId::Name(format!("open-{row_id}").into()),
                        RdmIcon::External,
                        "打开文件",
                        cx.listener(move |this, _, _, cx| this.open_file_by_id(&target, cx)),
                    )
                    .into_any_element()
                }
                _ => div().into_any_element(),
            })
            .child({
                let target = id.clone();
                icon_button(
                    ElementId::Name(format!("folder-{id}").into()),
                    RdmIcon::Folder,
                    "打开所在文件夹",
                    cx.listener(move |this, _, _, cx| this.open_folder_by_id(&target, cx)),
                )
            });

        let menu_focus = self.list_focus.clone();
        let click_id = id.clone();
        let right_id = id.clone();
        let row = h_flex()
            .id(ElementId::Name(format!("row-{id}").into()))
            .h(px(row_h))
            .w_full()
            .px(px(16.0))
            .gap(px(10.0))
            .items_center()
            .bg(bg)
            .border_b_1()
            .border_color(t.border_subtle)
            .cursor_pointer()
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                this.on_row_click(&click_id, ev, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    if this.modal_layer_blocks_actions() {
                        return;
                    }
                    // 右键未选中的行：默认模式清空后单选；多选模式保留其它已选项。
                    if !this.selection.contains(&right_id) {
                        if !this.multi_select {
                            this.selection.clear();
                        }
                        this.selection.insert(right_id.clone());
                    }
                    this.focused = Some(right_id.clone());
                    cx.notify();
                }),
            );

        let row = row
            // 候选框只在多选模式出现；默认模式列表左侧没有空框（2026-10-22 用户反馈）。
            .when(self.multi_select, |this| {
                this.child(if selected {
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(16.0))
                        .rounded(px(4.0))
                        .bg(t.accent)
                        .child(RdmIcon::Check.sized(11.0, t.on_accent))
                        .into_any_element()
                } else {
                    div()
                        .flex_none()
                        .size(px(16.0))
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(t.border_control)
                        .into_any_element()
                })
            })
            .child(RdmIcon::for_file(&item.file_name).sized(20.0, state_fg))
            .child(body)
            .child(actions);

        // 16 §6：设置/命令面板打开时不挂载行右键菜单。gpui-component 的 ContextMenu
        // 打开逻辑是 `window.on_mouse_event` 的 bubble 监听（只判断鼠标是否落在行 bounds 内），
        // 不受覆盖层 `.occlude()` 影响；必须在渲染期不注册，否则右键仍会穿透模态层
        //（2026-10-22 真实鼠标验收实测：occlude 只挡住行点击，挡不住 ContextMenu）。
        if self.modal_layer_blocks_actions() {
            return row.into_any_element();
        }

        // 右键菜单：用 action 派发到根视图统一处理。
        let task_id = item.id.clone();
        let menu_id = task_id.clone();
        row.context_menu(move |menu, _, _| {
            let menu = menu.action_context(menu_focus.clone());
            let mut menu = menu;
            if state == ItemState::Downloading {
                menu = menu.menu(
                    "暂停下载",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::Pause,
                    }),
                );
            }
            if state == ItemState::Paused {
                menu = menu.menu(
                    "继续下载",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::Resume,
                    }),
                );
            }
            if state == ItemState::Failed {
                menu = menu.menu(
                    "重试",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::Retry,
                    }),
                );
            }
            if matches!(
                state,
                ItemState::Downloading | ItemState::Paused | ItemState::Failed
            ) {
                menu = menu.menu(
                    "取消下载",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::Cancel,
                    }),
                );
            }
            if state == ItemState::Completed {
                menu = menu
                    .menu(
                        "打开文件",
                        Box::new(TaskCommand {
                            id: menu_id.clone().into(),
                            kind: TaskCommandKind::OpenFile,
                        }),
                    )
                    .menu(
                        "打开所在文件夹",
                        Box::new(TaskCommand {
                            id: menu_id.clone().into(),
                            kind: TaskCommandKind::OpenFolder,
                        }),
                    );
            } else {
                menu = menu.menu(
                    "打开所在文件夹",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::OpenFolder,
                    }),
                );
            }
            menu.separator()
                .menu(
                    "复制链接",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::CopyLink,
                    }),
                )
                .menu(
                    "复制文件路径",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::CopyPath,
                    }),
                )
                .menu(
                    "详情",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::Details,
                    }),
                )
                .separator()
                .menu(
                    "移除记录",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::Remove,
                    }),
                )
                .menu(
                    "删除文件…",
                    Box::new(TaskCommand {
                        id: menu_id.clone().into(),
                        kind: TaskCommandKind::DeleteFile,
                    }),
                )
        })
        .into_any_element()
    }

    fn render_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let indexes = self.filtered_indices(cx);
        let visible = indexes.len();
        let row_h = self.row_height();
        let sizes = Rc::new(vec![size(px(880.0), px(row_h)); visible]);
        let empty = visible == 0;
        let list = if empty {
            empty_state(
                &t,
                RdmIcon::Inbox,
                if self.items.is_empty() {
                    "还没有下载任务"
                } else {
                    "当前视图没有任务"
                },
                "把下载链接粘贴到顶部输入栏，按 Enter 开始；浏览器里的下载会自动交给 RDM。",
                vec![],
            )
            .into_any_element()
        } else {
            div()
                .id("task-list-surface")
                .flex_1()
                .min_h(px(0.0))
                .key_context("TaskList")
                .track_focus(&self.list_focus)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.list_focus.focus(window);
                    cx.notify();
                }))
                .child(
                    gpui_component::v_virtual_list(
                        cx.entity(),
                        "task-list",
                        sizes,
                        move |this, range, _window, cx| {
                            range
                                .map(|index| {
                                    let source = indexes.get(index).copied();
                                    match source {
                                        Some(source_index) => {
                                            let compact = this.compact_rows();
                                            this.render_row(source_index, compact, cx)
                                        }
                                        None => div().into_any_element(),
                                    }
                                })
                                .collect()
                        },
                    )
                    .track_scroll(&self.list_scroll)
                    .into_any_element(),
                )
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .bg(t.bg_app)
            .child(self.render_list_header(cx, visible))
            .child(for_test_hint(&t, self.ipc_warning.clone()))
            .child(list)
    }

    /// 常驻详情面板（右侧，不可关闭、无页签）：03 §4.5 修订版。
    ///
    /// 「资源 / 助手」两块内容已迁到独立「资源解析」页面，这里只保留任务详情；
    /// 窗口变窄时也不隐藏（详情是主界面的一部分，不再有开关/快捷键）。
    fn render_inspector(&self, cx: &Context<Self>) -> AnyElement {
        let t = tokens(cx);
        v_flex()
            .flex_none()
            .w(px(theme::INSPECTOR_W))
            .h_full()
            .bg(t.bg_surface)
            .border_l_1()
            .border_color(t.border_subtle)
            .child(
                h_flex()
                    .h(px(40.0))
                    .px(px(12.0))
                    .gap(px(6.0))
                    .items_center()
                    .border_b_1()
                    .border_color(t.border_subtle)
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_primary)
                            .child("详情"),
                    )
                    .child(div().flex_1()),
            )
            .child(
                div()
                    .id("inspector-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .p(px(12.0))
                    .child(self.render_inspector_details(cx)),
            )
            .into_any_element()
    }

    fn render_inspector_details(&self, cx: &Context<Self>) -> AnyElement {
        let t = tokens(cx);
        let Some(item) = self.selected_or_focused() else {
            return empty_state(
                &t,
                RdmIcon::Info,
                "未选择任务",
                "在列表中选择一个任务查看详情。",
                vec![],
            )
            .into_any_element();
        };
        let id = item.id.clone();
        let state = item.state;
        let mut column = v_flex().gap(px(12.0)).w_full();
        column = column
            .child(
                v_flex()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(item.file_name.clone()),
                    )
                    .child(status_pill(
                        &t,
                        RdmIcon::for_state(state),
                        state.label_zh(),
                        match state {
                            ItemState::Downloading => t.info,
                            ItemState::Paused => t.warning,
                            ItemState::Completed => t.success,
                            ItemState::Failed => t.danger_text,
                            ItemState::Cancelled => t.text_tertiary,
                        },
                        t.bg_hover,
                    )),
            )
            .child(progress_bar(
                &t,
                item.progress_fraction().map(|f| f as f32),
                6.0,
            ))
            .when(state == ItemState::Downloading, |this| {
                this.child(
                    div()
                        .w_full()
                        .h(px(48.0))
                        .child(render_sparkline(item, t.accent, 48.0)),
                )
            })
            .child(
                v_flex()
                    .gap(px(4.0))
                    .child(detail_row("大小", item.size_text(), true, &t))
                    .child(detail_row("速度", item.speed_text(), true, &t))
                    .child(detail_row("预计剩余", item.eta_text(), true, &t))
                    .child(detail_row("连接", item.connections_text(), false, &t))
                    .child(detail_row(
                        "来源",
                        source_label(&item.source).to_string(),
                        false,
                        &t,
                    ))
                    // 资源监控：单任务 AI 成本（解析 + 分类；共享解析会有说明）。
                    .when_some(item.ai.clone(), |this, ai_usage| {
                        let mut parts: Vec<String> = Vec::new();
                        if ai_usage.parse.total_tokens > 0 {
                            parts.push(format!(
                                "解析 {}",
                                monitor::format_tokens_maybe_estimated(&ai_usage.parse)
                            ));
                        }
                        if ai_usage.classify.total_tokens > 0 {
                            parts.push(format!(
                                "分类 {}",
                                monitor::format_tokens_maybe_estimated(&ai_usage.classify)
                            ));
                        }
                        if parts.is_empty() {
                            parts.push(if ai_usage.cache_hit {
                                "解析缓存命中（0 token）".to_owned()
                            } else {
                                "无 token 记录".to_owned()
                            });
                        }
                        let mut value = parts.join(" · ");
                        if ai_usage.cost_cny() > 0.0 {
                            value.push_str(&format!(
                                " · {}",
                                monitor::format_cny_approx(ai_usage.cost_cny())
                            ));
                        } else if ai_usage.has_tokens() {
                            value.push_str(" · 无价目");
                        }
                        if let Some(model) = ai_usage
                            .model
                            .as_deref()
                            .map(str::trim)
                            .filter(|model| !model.is_empty())
                        {
                            value.push_str(&format!("（{model}）"));
                        }
                        if ai_usage.parse_siblings > 1 {
                            value.push_str(&format!(
                                "，与 {} 项共享该次解析",
                                ai_usage.parse_siblings - 1
                            ));
                        }
                        if ai_usage.estimated() {
                            value.push_str(" · ≈ 表示估算");
                        }
                        this.child(detail_row("AI 消耗", value, false, &t)).when(
                            ai_usage.cache_hit,
                            |this| {
                                this.child(h_flex().gap(px(4.0)).items_center().child(status_pill(
                                    &t,
                                    RdmIcon::CheckCircle,
                                    "解析缓存命中",
                                    t.success,
                                    t.success_soft,
                                )))
                            },
                        )
                    })
                    .child(detail_row(
                        "添加时间",
                        format_relative_time(item.created_ms),
                        false,
                        &t,
                    ))
                    .when_some(item.path.clone(), |this, path| {
                        this.child(detail_row(
                            "保存路径",
                            path.display().to_string(),
                            false,
                            &t,
                        ))
                    })
                    .child(detail_row("链接", item.url.clone(), false, &t))
                    .when_some(item.sha256.clone(), |this, hash| {
                        this.child(detail_row("SHA-256", hash, true, &t))
                    }),
            )
            .child(divider(&t))
            .child(
                h_flex()
                    .gap(px(6.0))
                    .flex_wrap()
                    .child({
                        let target = id.clone();
                        Button::new("detail-open")
                            .small()
                            .label("打开文件")
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.open_file_by_id(&target, cx)
                                }),
                            )
                    })
                    .child({
                        let target = id.clone();
                        Button::new("detail-folder")
                            .small()
                            .ghost()
                            .label("打开所在文件夹")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_folder_by_id(&target, cx)
                            }))
                    })
                    .child({
                        let target = id.clone();
                        Button::new("detail-copy")
                            .small()
                            .ghost()
                            .label("复制链接")
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.copy_link_by_id(&target, cx)
                                }),
                            )
                    }),
            )
            .child(
                h_flex()
                    .gap(px(6.0))
                    .child(match state {
                        ItemState::Downloading => Button::new("detail-pause")
                            .small()
                            .ghost()
                            .label("暂停")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.focused_id() {
                                    this.pause_by_id(&id, cx)
                                }
                            }))
                            .into_any_element(),
                        ItemState::Paused => Button::new("detail-resume")
                            .small()
                            .ghost()
                            .label("继续")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.focused_id() {
                                    this.resume_by_id(&id, cx)
                                }
                            }))
                            .into_any_element(),
                        ItemState::Failed | ItemState::Cancelled => Button::new("detail-retry")
                            .small()
                            .ghost()
                            .label("重试")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.focused_id() {
                                    this.retry_by_id(&id, cx)
                                }
                            }))
                            .into_any_element(),
                        _ => div().into_any_element(),
                    })
                    .child({
                        let target = id.clone();
                        Button::new("detail-remove")
                            .small()
                            .ghost()
                            .label("移除记录")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_by_id(&target, cx)),
                            )
                    }),
            )
            .when_some(item.error.clone(), |this, error| {
                this.child(info_banner(
                    &t,
                    RdmIcon::Alert,
                    Severity::Danger,
                    error,
                    vec![],
                ))
            });
        column.into_any_element()
    }

    // -----------------------------------------------------------------------
    // 解析经验库（design 17；view 主体在 views/experience.rs）
    // -----------------------------------------------------------------------

    /// 打开「解析经验」页：切换页面并刷新经验列表。
    pub(crate) fn open_experience_page(&mut self, cx: &mut Context<Self>) {
        self.page = AppPage::Experience;
        self.reload_experiences();
        cx.notify();
    }

    /// 经验库页主体：委托 `views/experience.rs` 渲染，回调在此接到 `RdmApp` 状态与
    /// 持久化动作（选中/版本预览/回滚/编辑/删除/刷新/用经验解析）。
    fn render_experience(&self, viewport_width: f32, cx: &Context<Self>) -> AnyElement {
        let t = tokens(cx);
        let entity = cx.entity();
        let callbacks = ExperienceCallbacks {
            on_select: experience_id_callback(entity.clone(), |this, id, _window, cx| {
                // 切换经验会清空助手上下文：丢弃在途请求，避免迟到回复落到新经验上。
                if this.experience.selected_id() != Some(id) {
                    this.experience_assistant_request = None;
                }
                this.experience.select(id);
                cx.notify();
            }),
            on_select_version: experience_version_callback(
                entity.clone(),
                |this, id, version, _window, cx| {
                    this.experience.preview_version(id, version);
                    cx.notify();
                },
            ),
            on_set_active: experience_version_callback(
                entity.clone(),
                |this, id, version, _window, cx| {
                    match this.experience.store().set_active_version(id, version) {
                        Ok(_) => {
                            this.experience.reload(&this.state_dir);
                            // 回滚后回到 active 版本视图（避免详情停留在被替换的「历史版本」预览）。
                            this.experience.select(id);
                            this.toast(format!("已将 v{version} 设为当前版本"), Severity::Success);
                        }
                        Err(error) => {
                            this.toast(format!("切换版本失败：{error}"), Severity::Warning)
                        }
                    }
                    cx.notify();
                },
            ),
            on_open_edit: experience_id_callback(entity.clone(), |this, id, window, cx| {
                this.experience.begin_edit(id, window, cx);
                cx.notify();
            }),
            on_save_edit: experience_simple_callback(entity.clone(), |this, _window, cx| {
                let Some(id) = this.experience.selected_id().map(str::to_owned) else {
                    return;
                };
                match this.experience.edit_form_to_recipe() {
                    Ok(recipe) => match this.experience.store().update(&id, recipe) {
                        Ok(updated) => {
                            let version = updated.active_version;
                            this.experience.close_edit();
                            this.experience.reload(&this.state_dir);
                            this.toast(
                                format!("已保存为 v{version}（旧版本可在版本历史中回滚）"),
                                Severity::Success,
                            );
                        }
                        Err(error) => this.experience.set_edit_error(Some(error)),
                    },
                    Err(_inline_error) => {
                        // 校验错误已由 `edit_form_to_recipe` 写进行内错误，无需重复提示。
                    }
                }
                cx.notify();
            }),
            on_cancel_edit: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.experience.close_edit();
                cx.notify();
            }),
            on_confirm_delete: experience_id_callback(entity.clone(), |this, id, window, cx| {
                this.experience.begin_delete(id, window, cx);
                cx.notify();
            }),
            on_confirm_delete_yes: experience_simple_callback(
                entity.clone(),
                |this, _window, cx| {
                    let Some(id) = this.experience.delete_target().map(str::to_owned) else {
                        return;
                    };
                    match this.experience.store().delete(&id) {
                        Ok(removed) => {
                            this.experience.cancel_delete();
                            this.experience.reload(&this.state_dir);
                            if removed {
                                this.toast("已删除解析经验", Severity::Success);
                            }
                        }
                        Err(error) => {
                            this.toast(format!("删除失败：{error}"), Severity::Warning);
                        }
                    }
                    cx.notify();
                },
            ),
            on_cancel_delete: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.experience.cancel_delete();
                cx.notify();
            }),
            on_refresh: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.experience.reload(&this.state_dir);
                cx.notify();
            }),
            on_open_resolve_page: experience_simple_callback(
                entity.clone(),
                |this, _window, cx| {
                    this.page = AppPage::Resolve;
                    this.pending_resolve_focus = true;
                    cx.notify();
                },
            ),
            on_use_experience: experience_id_callback(entity.clone(), |this, id, window, cx| {
                // design 18 §3.1：按钮展开/收起「应用到新地址」行，不再用来源地址重解析。
                if this.experience.use_experience_target() == Some(id) {
                    this.experience.cancel_use_experience();
                } else {
                    this.experience.begin_use_experience(id, window, cx);
                }
                cx.notify();
            }),
            // ---- design 18：经验助手 / 使用经验 / 同类合并 ----
            on_send_assistant: experience_text_callback(
                entity.clone(),
                |this, text, window, cx| {
                    this.send_experience_assistant(text.to_owned(), window, cx);
                },
            ),
            on_stop_assistant: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.stop_experience_assistant(cx);
            }),
            on_apply_patch: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.apply_experience_patch(cx);
            }),
            on_discard_patch: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.experience.discard_patch();
                cx.notify();
            }),
            on_open_settings_ai: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.open_settings_section(Some(SettingsSection::Ai), cx);
            }),
            on_apply_experience: experience_apply_callback(
                entity.clone(),
                |this, id, url, _window, cx| {
                    this.apply_experience_to_url(id, url, cx);
                    this.experience.cancel_use_experience();
                },
            ),
            on_merge_group: experience_merge_callback(
                entity.clone(),
                |this, target, sources, _window, cx| {
                    this.merge_experiences(target, sources, cx);
                },
            ),
            on_dismiss_merge: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.experience.dismiss_merge_hint();
                cx.notify();
            }),
            on_toggle_assistant: experience_simple_callback(entity.clone(), |this, _window, cx| {
                this.experience.toggle_assistant();
                cx.notify();
            }),
        };
        render_experience_page(
            &t,
            &self.experience,
            ExperienceViewProps {
                viewport_width,
                ai_configured: self.ai_config.is_configured(),
            },
            callbacks,
        )
    }

    /// 侧栏计数：经验条数。
    fn experience_count(&self) -> usize {
        self.experience.len()
    }

    /// 经验助手：发送一条用户指令（design 18 §3.2）。
    ///
    /// 上下文 = 当前选中经验的完整 recipe JSON（`experience_assistant::system_prompt`）；
    /// 回复流式写回页面，包含 recipe JSON 时页面生成“应用为新版本”预览。
    pub(crate) fn send_experience_assistant(
        &mut self,
        text: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        // 流式生成期间不允许再次发送（Enter 也走同一路径；渲染层已禁用按钮）。
        if self.experience.assistant_busy() {
            return;
        }
        let Some(experience) = self.experience.selected().cloned() else {
            self.experience.assistant_failed("请先选择一条解析经验");
            cx.notify();
            return;
        };
        if !self.ai_config.is_configured() {
            self.experience.assistant_failed(
                "AI 未配置：请到「设置 → AI 助手」配置 Provider 后再使用经验助手（不会用假回复代替）",
            );
            cx.notify();
            return;
        }
        let request_id = self.next_ai_request_id();
        self.experience_assistant_request = Some(request_id);
        // 多轮上下文：把页面里已有的助手对话带给模型（不含刚追加的空占位）。
        let history: Vec<crate::experience_assistant::TranscriptTurn> = self
            .experience
            .assistant_messages()
            .iter()
            .filter(|message| !message.text.trim().is_empty())
            .map(|message| {
                if message.role == crate::views::experience::AssistantRole::User {
                    crate::experience_assistant::TranscriptTurn::user(message.text.clone())
                } else {
                    crate::experience_assistant::TranscriptTurn::assistant(message.text.clone())
                }
            })
            .collect();
        self.experience.assistant_begin(&text);
        let system = crate::experience_assistant::system_prompt(&experience);
        let prompt = crate::experience_assistant::build_prompt(&history, &text);
        self.engine
            .start_ai_chat_stream(request_id, self.ai_config.clone(), system, prompt);
        cx.notify();
    }

    /// 停止经验助手流式生成（迟到的 AI 事件按 request_id 丢弃）。
    pub(crate) fn stop_experience_assistant(&mut self, cx: &mut Context<Self>) {
        if self.experience_assistant_request.take().is_some() {
            self.experience.assistant_failed("已停止生成");
        }
        cx.notify();
    }

    /// 应用助手建议的 recipe 为新版本（校验失败内联，不写盘）。
    pub(crate) fn apply_experience_patch(&mut self, cx: &mut Context<Self>) {
        let Some(recipe) = self.experience.take_patch_recipe() else {
            return;
        };
        let Some(id) = self.experience.selected_id().map(str::to_owned) else {
            return;
        };
        match self.experience.store().update(&id, recipe) {
            Ok(updated) => {
                let version = updated.active_version;
                self.experience.reset_assistant();
                self.experience.reload(&self.state_dir);
                self.toast(
                    format!("经验助手建议已应用为 v{version}（旧版本可在版本历史回滚）"),
                    Severity::Success,
                );
            }
            Err(error) => self.experience.set_patch_error(Some(error)),
        }
        cx.notify();
    }

    /// 合并同类经验（design 18 §3.1）：把 sources 的流程并入 target，一次调用产出全部资源。
    pub(crate) fn merge_experiences(
        &mut self,
        target: &str,
        sources: &[String],
        cx: &mut Context<Self>,
    ) {
        let refs: Vec<&str> = sources.iter().map(String::as_str).collect();
        match self.experience.store().merge(target, &refs) {
            Ok(merged) => {
                let flow_count = merged.flows.len();
                let resources = merged.best_resources;
                self.experience.dismiss_merge_hint();
                self.experience.reload(&self.state_dir);
                self.experience.select(target);
                self.toast(
                    format!("已合并为 {flow_count} 个流程、{resources} 个资源（使用经验会一次回放全部流程）"),
                    Severity::Success,
                );
            }
            Err(error) => self.toast(format!("合并失败：{error}"), Severity::Warning),
        }
        cx.notify();
    }

    /// 从磁盘重新加载经验列表（AI 解析完成/编辑删除后调用）。
    fn reload_experiences(&mut self) {
        self.experience.reload(&self.state_dir);
    }

    /// 「使用此经验解析」（design 18 §3.1）：把经验主流程+补充流程应用到用户给的**新地址**。
    ///
    /// 确定性回放、不调用 AI；结果落到一条新的解析会话，失败也保留错误横幅可继续重试。
    pub(crate) fn apply_experience_to_url(
        &mut self,
        experience_id: &str,
        url: &str,
        cx: &mut Context<Self>,
    ) {
        let url = match validate_url(url.trim()) {
            Ok(url) => url,
            Err(err) => {
                self.toast(format!("地址无效：{err}"), Severity::Warning);
                return;
            }
        };
        let request_id = self.next_ai_request_id();
        self.apply_experience_request = Some(request_id);
        self.toast("正在用解析经验回放（不调用 AI）…", Severity::Info);
        self.engine.start_apply_experience(
            request_id,
            experience_id.to_owned(),
            url,
            self.state_dir.clone(),
        );
        cx.notify();
    }

    /// 应用经验结果 → 解析会话（复用同 URL 历史；无则新建）。
    fn finish_apply_experience(
        &mut self,
        experience_id: &str,
        url: &str,
        outcome: rdm_resolve::ResolveOutcome,
        cx: &mut Context<Self>,
    ) {
        let now = model::now_ms();
        let session_id = self
            .reusable_resolve_session(url)
            .map(|(id, _, _)| id)
            .unwrap_or_else(|| self.resolve.create_session(ResolveOrigin::Gui, now));
        let toast: (String, Severity);
        let mut to_resolve_page = true;
        if let Some(report) = crate::ai_resolve::report_from_outcome(url, None, None, &outcome) {
            let count = report.resources.len();
            let note = format!("使用解析经验 {experience_id}（不调用 AI，{count} 个资源）");
            if let Some(session) = self.resolve.get_mut(&session_id) {
                session.panel.apply_report(report);
                // resolver 的 outcome notes 已包含「使用解析经验 <id>@v<n>（跳过 AI，N 个资源）」；
                // 只在缺失时补一条，避免状态卡重复两行。
                if !session
                    .panel
                    .notes
                    .iter()
                    .any(|existing| existing.contains("使用解析经验"))
                {
                    session.panel.push_note(note);
                }
                session.touch(now);
                if session.title_source == TitleSource::Fallback {
                    let fallback = session.fallback_title();
                    session.title = fallback;
                }
            }
            toast = if count > 0 {
                (
                    format!("使用经验解析完成：{count} 个资源（未调用 AI）"),
                    Severity::Success,
                )
            } else {
                (
                    "经验回放完成，但没有拿到可下载资源".to_owned(),
                    Severity::Warning,
                )
            };
            self.experience.cancel_use_experience();
        } else {
            let detail = outcome
                .diagnostics
                .error
                .as_ref()
                .map(|error| error.detail.clone())
                .filter(|detail| !detail.trim().is_empty())
                .unwrap_or_else(|| outcome.diagnostics.summary.clone());
            if let Some(session) = self.resolve.get_mut(&session_id) {
                session.panel.apply_parse_error(detail.clone());
                session
                    .panel
                    .push_note(format!("使用解析经验 {experience_id} 失败：{detail}"));
                session.touch(now);
            }
            self.experience.set_use_error(Some(detail.clone()));
            // 失败时留在经验页：错误在 URL 行内联显示，用户可直接改地址重试。
            to_resolve_page = false;
            toast = (format!("经验回放失败：{detail}"), Severity::Warning);
        }
        self.resolve.set_active(&session_id);
        if to_resolve_page {
            self.page = AppPage::Resolve;
            self.pending_resolve_focus = false;
        }
        self.resolve.mark_dirty();
        self.reload_experiences();
        self.toast(toast.0, toast.1);
        cx.notify();
    }

    /// 选中第一条经验（E2E/截图钩子与首次打开页面用）。
    fn experience_select_first(&mut self) {
        if self.experience.selected_id().is_some() {
            return;
        }
        let first = self
            .experience
            .visible()
            .first()
            .map(|experience| experience.id.clone());
        if let Some(id) = first {
            self.experience.select(&id);
        }
    }

    // -----------------------------------------------------------------------
    // 资源解析页（独立页面：会话历史 + 会话内解析/对话/下载）
    // -----------------------------------------------------------------------

    /// 资源解析页面主体：左侧会话历史列表 + 右侧当前会话线程（含底部输入框）。
    fn render_resolve_page(&self, cx: &Context<Self>) -> AnyElement {
        let t = tokens(cx);
        let list = self.render_resolve_session_list(&t, cx);
        let thread: AnyElement = match self.resolve.active() {
            Some(session) => self.render_resolve_thread(session, &t, cx),
            None => self.render_resolve_welcome(&t, cx),
        };
        h_flex()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .child(list)
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .h_full()
                    .bg(t.bg_app)
                    // design 18 §3.3：资源解析页不再承载聊天，只在顶部保留 URL 入口；
                    // 会话线程只读展示历史解析结果。
                    .child(self.render_resolve_url_bar(&t, cx))
                    .child(thread),
            )
            .into_any_element()
    }

    /// 资源解析页顶部 URL 入口（design 18 §3.3）：只接受 http(s) 链接，不再有自然语言聊天。
    fn render_resolve_url_bar(&self, t: &ThemeTokens, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .flex_shrink_0()
            .w_full()
            .px(px(16.0))
            .py(px(10.0))
            .gap(px(6.0))
            .border_b_1()
            .border_color(t.border_subtle)
            .bg(t.bg_surface)
            .child(
                h_flex()
                    .w_full()
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(
                                Input::new(&self.resolve_input)
                                    .w_full(),
                            ),
                    )
                    .child(
                        Button::new("resolve-url-submit")
                            .small()
                            .primary()
                            .label("解析")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_resolve_url(window, cx);
                            })),
                    )
                    .child(
                        Button::new("resolve-new")
                            .small()
                            .ghost()
                            .label("新解析")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_new_resolve_session(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(t.text_tertiary)
                    .child("资源解析只保留历史会话；AI 调试 / 修补 / 整理经验请到「解析经验」页使用右侧经验助手。"),
            )
            .into_any_element()
    }

    /// 左侧「解析历史」列表：每次解析一个会话，新→旧；标题由 LLM 自动生成。
    fn render_resolve_session_list(&self, t: &ThemeTokens, cx: &Context<Self>) -> AnyElement {
        let mut column = v_flex()
            .flex_none()
            .w(px(264.0))
            .h_full()
            .bg(t.bg_surface)
            .border_r_1()
            .border_color(t.border_subtle)
            .child(
                h_flex()
                    .h(px(44.0))
                    .px(px(12.0))
                    .gap(px(8.0))
                    .items_center()
                    .border_b_1()
                    .border_color(t.border_subtle)
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_primary)
                            .child("资源解析"),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(t.text_tertiary)
                            .child(format!("{} 条历史", self.resolve.len())),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("resolve-open-experience")
                            .ghost()
                            .compact()
                            .icon(RdmIcon::BookOpen.icon())
                            .tooltip("解析经验库（可查看流程/使用情况/编辑/删除）")
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.open_experience_page(cx);
                            })),
                    )
                    .child(
                        Button::new("resolve-new-session")
                            .ghost()
                            .compact()
                            .icon(RdmIcon::Plus.icon())
                            .tooltip("新建解析（保留全部历史）")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_new_resolve_session(window, cx);
                            })),
                    ),
            );
        let mut list = v_flex()
            .id("resolve-session-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .py(px(4.0));
        if self.resolve.is_empty() {
            list = list.child(
                div()
                    .px(px(12.0))
                    .py(px(16.0))
                    .text_size(px(11.0))
                    .text_color(t.text_tertiary)
                    .child("还没有解析历史。粘贴一个网页地址开始第一次解析。"),
            );
        }
        // 历史列表排序 = 置顶优先，其后最近活动时间降序（store 的唯一排序规则）；
        // 分组以可折叠小标题呈现（组名 + 数量 + 箭头），「未分组」固定最后一组。
        // 只有存在自定义分组时才显示分组标题（全部未分组时保持扁平列表，不造噪声）。
        let sessions = self.resolve.sorted_sessions();
        let mut group_order: Vec<Option<String>> = Vec::new();
        for session in &sessions {
            if !group_order.iter().any(|group| group == &session.group) {
                group_order.push(session.group.clone());
            }
        }
        let has_custom_groups = group_order.iter().any(Option::is_some);
        if !has_custom_groups {
            for session in sessions {
                list = list.child(self.render_resolve_session_row(session, t, cx));
            }
        } else {
            // 「未分组」固定最后（其余组按全局排序中的首次出现顺序）。
            if let Some(index) = group_order.iter().position(Option::is_none) {
                let ungrouped = group_order.remove(index);
                group_order.push(ungrouped);
            }
            for group in group_order {
                let members: Vec<&crate::resolve_session::ResolveSession> = sessions
                    .iter()
                    .copied()
                    .filter(|session| session.group == group)
                    .collect();
                // 全部会话都有分组时不会有「未分组」成员：不渲染空标题。
                if members.is_empty() {
                    continue;
                }
                let collapsed = self.resolve_collapsed_groups.contains(&group);
                list = list.child(self.render_resolve_group_header(
                    group.as_deref(),
                    members.len(),
                    collapsed,
                    t,
                    cx,
                ));
                if collapsed {
                    continue;
                }
                for session in members {
                    list = list.child(self.render_resolve_session_row(session, t, cx));
                }
            }
        }
        column = column.child(list);
        column.into_any_element()
    }

    /// 历史分组标题行：折叠箭头 + 组名 + 数量（点击折叠/展开）。
    fn render_resolve_group_header(
        &self,
        group: Option<&str>,
        count: usize,
        collapsed: bool,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let key = group.map(str::to_owned);
        let label = group.unwrap_or("未分组").to_string();
        let icon = if collapsed {
            RdmIcon::ChevronRight
        } else {
            RdmIcon::ChevronDown
        };
        let element_id = match group {
            Some(name) => format!("resolve-group-{name}"),
            None => "resolve-group-ungrouped".to_string(),
        };
        h_flex()
            .id(ElementId::Name(element_id.into()))
            .mx(px(6.0))
            .mt(px(4.0))
            .px(px(6.0))
            .py(px(3.0))
            .gap(px(5.0))
            .items_center()
            .rounded(px(theme::RADIUS_SM))
            .cursor_pointer()
            .hover(move |style| style.bg(t.bg_hover))
            .child(icon.sized(11.0, t.text_tertiary))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(t.text_tertiary)
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(t.text_tertiary)
                    .child(count.to_string()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_resolve_group_collapsed(key.clone(), cx);
            }))
            .into_any_element()
    }

    /// 会话历史行：状态图标 + LLM 标题 + 更新时间/资源数 + 解析中指示。
    fn render_resolve_session_row(
        &self,
        session: &crate::resolve_session::ResolveSession,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = self.resolve.active_id() == Some(session.id.as_str());
        let id = session.id.clone();
        let (icon, color, _) = resolve_status_style(&session.panel.status, t);
        let parsing = session.panel.status == model::AiParseStatus::Parsing;
        let pinned = session.pinned;
        let title_color = if selected {
            t.text_primary
        } else {
            t.text_secondary
        };
        let meta = format!(
            "{} · {} 个资源",
            format_relative_time(session.updated_ms),
            session.resource_count()
        );
        // 右键菜单所需数据在渲染时快照（菜单打开时读不到 &self）。
        let entity = cx.entity();
        let menu_id = session.id.clone();
        let menu_pinned = session.pinned;
        let menu_group = session.group.clone();
        let groups = self.resolve.groups();
        let row = h_flex()
            .id(ElementId::Name(
                format!("resolve-session-{}", session.id).into(),
            ))
            .mx(px(6.0))
            .px(px(8.0))
            .py(px(7.0))
            .gap(px(8.0))
            .items_center()
            .rounded(px(theme::RADIUS_SM))
            .cursor_pointer()
            .bg(if selected {
                t.bg_selected
            } else {
                t.bg_surface
            })
            .hover(move |style| style.bg(t.bg_hover))
            .child(icon.sized(14.0, color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap(px(2.0))
                    .child(
                        h_flex()
                            .gap(px(4.0))
                            .items_center()
                            // 置顶指示：弱化的「置顶」图标（不引入新图标库）。
                            .when(pinned, |this| {
                                this.child(
                                    RdmIcon::ArrowUp
                                        .sized(9.5, t.text_tertiary)
                                        .into_any_element(),
                                )
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(12.0))
                                    .text_color(title_color)
                                    .child(session.title.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(px(5.0))
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(t.text_tertiary)
                                    .child(meta),
                            )
                            .when(parsing, |this| {
                                this.child(mono_text("解析中…", 10.0, t.info))
                            }),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_resolve_session(&id, window, cx);
            }));
        // 16 §6：模态层打开时不挂载右键菜单。gpui-component 的 ContextMenu 用
        // `window.on_mouse_event` bubble 监听打开，`.occlude()` 挡不住它；只能在渲染期不注册
        //（与下载行右键菜单同一修复，2026-10-22 真实鼠标验收实测）。
        if self.modal_layer_blocks_actions() {
            return row.into_any_element();
        }
        // 右键菜单：置顶 / 移动到分组 / 重命名分组 / 删除会话。
        // 用 PopupMenuItem 的点击回调（`window.listener_for`）而不是 Action 派发，
        // 避免依赖 Root 上的全局 on_action（本页菜单是会话级动态项）。
        let build_menu =
            move |menu: PopupMenu, window: &mut Window, _cx: &mut Context<PopupMenu>| {
                let mut menu = menu;
                // 置顶 / 取消置顶
                let entity_pin = entity.clone();
                let id_pin = menu_id.clone();
                let pin_label = if menu_pinned {
                    "取消置顶"
                } else {
                    "置顶"
                };
                menu = menu.item(
                    PopupMenuItem::new(pin_label)
                        .icon(RdmIcon::ArrowUp.icon())
                        .on_click(
                            window.listener_for(&entity_pin, move |this, _: &ClickEvent, _, cx| {
                                this.toggle_resolve_pin(&id_pin, cx)
                            }),
                        ),
                );
                // 移动到分组… = 现有分组 + 未分组 + 新建分组…
                let entity_move = entity.clone();
                let id_move = menu_id.clone();
                let current_group = menu_group.clone();
                let groups_for_menu = groups.clone();
                menu = menu.submenu(
                    "移动到分组…",
                    window,
                    _cx,
                    move |sub, window, _cx| {
                        let mut sub = sub;
                        for group in groups_for_menu.iter() {
                            let entity_item = entity_move.clone();
                            let id_item = id_move.clone();
                            let group_item = group.clone();
                            sub = sub.item(
                                PopupMenuItem::new(group.clone())
                                    .checked(current_group.as_deref() == Some(group.as_str()))
                                    .on_click(window.listener_for(
                                        &entity_item,
                                        move |this, _: &ClickEvent, _, cx| {
                                            this.set_resolve_session_group(
                                                &id_item,
                                                Some(&group_item),
                                                cx,
                                            )
                                        },
                                    )),
                            );
                        }
                        let entity_ungrouped = entity_move.clone();
                        let id_ungrouped = id_move.clone();
                        sub = sub.item(
                            PopupMenuItem::new("未分组")
                                .checked(current_group.is_none())
                                .on_click(window.listener_for(
                                    &entity_ungrouped,
                                    move |this, _: &ClickEvent, _, cx| {
                                        this.set_resolve_session_group(&id_ungrouped, None, cx)
                                    },
                                )),
                        );
                        let entity_new = entity_move.clone();
                        let id_new = id_move.clone();
                        sub.separator()
                            .item(
                                PopupMenuItem::new("新建分组…").on_click(window.listener_for(
                                    &entity_new,
                                    move |this, _: &ClickEvent, window, cx| {
                                        this.open_resolve_group_dialog(&id_new, None, window, cx)
                                    },
                                )),
                            )
                    },
                );
                // 重命名分组…（仅当会话已处于某个分组）
                if let Some(current) = menu_group.clone() {
                    let entity_rename = entity.clone();
                    let id_rename = menu_id.clone();
                    let rename_from = current.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("重命名分组「{current}」…")).on_click(
                            window.listener_for(
                                &entity_rename,
                                move |this, _: &ClickEvent, window, cx| {
                                    this.open_resolve_group_dialog(
                                        &id_rename,
                                        Some(rename_from.clone()),
                                        window,
                                        cx,
                                    )
                                },
                            ),
                        ),
                    );
                }
                // 删除会话（二次确认；danger 色用自绘 item）
                let entity_delete = entity.clone();
                let id_delete = menu_id.clone();
                menu.separator().item(
                    PopupMenuItem::element(|_, cx| {
                        let t = tokens(cx);
                        div().text_color(t.danger_text).child("删除会话")
                    })
                    .on_click(window.listener_for(
                        &entity_delete,
                        move |this, _: &ClickEvent, window, cx| {
                            this.open_resolve_delete_dialog(&id_delete, window, cx)
                        },
                    )),
                )
            };
        row.context_menu(build_menu).into_any_element()
    }

    /// 右侧线程：标题栏（标题/状态/来源/重试）+ 滚动正文 + 固定输入框。
    fn render_resolve_thread(
        &self,
        session: &crate::resolve_session::ResolveSession,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (icon, color, background) = resolve_status_style(&session.panel.status, t);
        let running = session.plan.as_ref().is_some_and(AgentPlan::is_running);
        let can_retry = !session.panel.source_url.is_empty() && !running;
        let retry_id = session.id.clone();
        let header = h_flex()
            .h(px(48.0))
            .flex_shrink_0()
            .px(px(16.0))
            .gap(px(8.0))
            .items_center()
            .border_b_1()
            .border_color(t.border_subtle)
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap(px(2.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_primary)
                            .child(session.title.clone()),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .child(status_pill(
                                t,
                                icon,
                                session.panel.status.clone().label_zh(),
                                color,
                                background,
                            ))
                            .child(soft_tag(
                                session.origin.label_zh(),
                                t.text_secondary,
                                t.bg_hover,
                            ))
                            .child(meta_text(
                                t,
                                format!(
                                    "{} · {} 个资源",
                                    format_relative_time(session.created_ms),
                                    session.resource_count()
                                ),
                            )),
                    ),
            )
            .child(
                Button::new("resolve-retry")
                    .ghost()
                    .small()
                    .icon(RdmIcon::Refresh.icon())
                    .label("重新解析")
                    .tooltip("跳过本地解析经验，调用 AI 重新探索（保留上次更丰富的资源）")
                    .disabled(!can_retry)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.retry_agent_plan(&retry_id, cx);
                    })),
            );
        let body = self.render_resolve_body(session, t, cx);
        v_flex()
            .flex_1()
            .min_h(px(0.0))
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// 滚动正文：解析状态卡 → 消息（工作流卡片插在最后一条用户消息之后）→ 资源清单。
    fn render_resolve_body(
        &self,
        session: &crate::resolve_session::ResolveSession,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut body = v_flex()
            .id("resolve-thread-scroll")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.resolve_scroll)
            .p(px(16.0))
            .gap(px(12.0));

        body = body.child(self.render_resolve_status_card(session, t, cx));

        if session.panel.messages.is_empty() {
            body = body.child(
                v_flex()
                    .gap(px(6.0))
                    .p(px(12.0))
                    .rounded(px(theme::RADIUS_SM))
                    .bg(t.bg_surface)
                    .border_1()
                    .border_color(t.border_subtle)
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .child(RdmIcon::Bot.sized(16.0, t.accent))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("解析会话"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(t.text_secondary)
                            .child("粘贴网页链接直接发送：RDM 会解析页面里的可下载资源；也可以说「下载这个链接」「下载第 2 个」「只要音频」。"),
                    ),
            );
        } else {
            let messages = &session.panel.messages;
            let split = session
                .plan
                .as_ref()
                .and_then(|_| messages.iter().rposition(|m| m.role == model::AiRole::User))
                .map(|index| index + 1)
                .unwrap_or(messages.len());
            for message in &messages[..split] {
                body = body.child(render_resolve_message(message, t));
            }
            if let Some(plan) = session.plan.as_ref() {
                body = body.child(self.render_agent_plan(session, plan, t, cx));
            }
            for message in &messages[split..] {
                body = body.child(render_resolve_message(message, t));
            }
        }
        if session.panel.streaming {
            body = body.child(
                h_flex()
                    .gap(px(4.0))
                    .items_center()
                    .child(RdmIcon::Loader.sized(12.0, t.info))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(t.text_tertiary)
                            .child("正在生成…"),
                    ),
            );
        }
        if !session.panel.resources.is_empty() {
            body = body.child(self.render_resolve_resources(session, t, cx));
        }
        body.into_any_element()
    }

    /// 解析状态卡：来源 URL、状态、AI/缓存/模型信息、notes 与错误块。
    fn render_resolve_status_card(
        &self,
        session: &crate::resolve_session::ResolveSession,
        t: &ThemeTokens,
        _cx: &Context<Self>,
    ) -> AnyElement {
        let panel = &session.panel;
        if panel.status == model::AiParseStatus::Idle {
            return div().into_any_element();
        }
        let mut column = v_flex()
            .gap(px(6.0))
            .w_full()
            .p(px(12.0))
            .rounded(px(theme::RADIUS_SM))
            .bg(t.bg_surface)
            .border_1()
            .border_color(t.border_subtle);
        if !panel.source_url.is_empty() {
            column = column.child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(RdmIcon::Link.sized(13.0, t.text_tertiary))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(t.text_secondary)
                            .child(panel.source_url.clone()),
                    ),
            );
        }
        // 解析方式标签（设计 16 §1.2.3 / 设计 17 §2.4-2.5）：脚本命中优先于「AI 参与」，
        // 让「这次没花 AI」在界面上直接可见；AI 成功后沉淀了经验的，再补一个「已沉淀经验」。
        let recipe_hit = panel
            .notes
            .iter()
            .any(|note| note.contains("本地解析经验命中"));
        let forced_reparse = panel
            .notes
            .iter()
            .any(|note| note.contains("重新解析：已跳过本地解析经验"));
        let experience_saved = panel.notes.iter().any(|note| {
            note.contains("已保存解析经验")
                || note.contains("解析经验已更新")
                || note.contains("解析经验已升级")
        });
        // 「保留最佳经验」：本次 AI 流程不如已有经验丰富，服务端明确保留了旧流程。
        let best_kept = panel
            .notes
            .iter()
            .any(|note| note.contains("已保留更丰富的解析经验"));
        // 「保留最佳结果」：GUI 合并了上次更丰富的资源清单（design 17 §2.5）。
        let result_kept = panel
            .notes
            .iter()
            .any(|note| note.contains("已保留上次解析"));
        let (method_label, method_fg, method_bg) = if forced_reparse {
            ("AI 重新解析", t.accent, t.accent_soft)
        } else if recipe_hit {
            ("经验复用", t.success, t.success_soft)
        } else if panel.ai_used {
            ("AI 参与", t.accent, t.accent_soft)
        } else {
            ("启发式", t.text_secondary, t.bg_hover)
        };
        column = column.child(
            h_flex()
                .gap(px(6.0))
                .items_center()
                .child(soft_tag(method_label, method_fg, method_bg))
                .when(experience_saved, |this| {
                    this.child(soft_tag("已沉淀经验", t.info, t.info_soft))
                })
                .when(best_kept, |this| {
                    this.child(soft_tag("保留最佳经验", t.success, t.success_soft))
                })
                .when(result_kept, |this| {
                    this.child(soft_tag("保留最佳结果", t.info, t.info_soft))
                })
                .when(panel.cache_hit, |this| {
                    this.child(soft_tag("缓存命中", t.info, t.info_soft))
                })
                .when_some(panel.model.clone(), |this, model| {
                    this.child(soft_tag(model, t.text_secondary, t.bg_hover))
                })
                .when(!panel.usage.is_zero(), |this| {
                    this.child(mono_text(
                        monitor::format_tokens_maybe_estimated(&panel.usage),
                        10.5,
                        t.text_tertiary,
                    ))
                }),
        );
        if let Some(error) = &panel.error {
            column = column.child(info_banner(
                t,
                RdmIcon::Alert,
                Severity::Danger,
                error.clone(),
                vec![],
            ));
        }
        for note in &panel.notes {
            // design 17：内部诊断 notes（草稿落盘路径等）属于实现细节，不当作产品信息展示。
            if is_internal_resolve_note(note) {
                continue;
            }
            column = column.child(
                div()
                    .text_size(px(11.0))
                    .text_color(t.text_tertiary)
                    .child(format!("· {note}")),
            );
        }
        column.into_any_element()
    }

    /// 资源清单：勾选 + 全选 +「下载所选」（下载链路与 Agent 共用）。
    fn render_resolve_resources(
        &self,
        session: &crate::resolve_session::ResolveSession,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let total = session.panel.resources.len();
        let selected = session.panel.selected_count();
        let select_all_id = session.id.clone();
        let download_id = session.id.clone();
        let mut column = v_flex()
            .gap(px(6.0))
            .w_full()
            .p(px(12.0))
            .rounded(px(theme::RADIUS_SM))
            .bg(t.bg_surface)
            .border_1()
            .border_color(t.border_subtle)
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_primary)
                            .child(format!("{total} 个资源 · 已选 {selected}")),
                    )
                    .child(
                        Button::new("resolve-select-all")
                            .ghost()
                            .small()
                            .label(if selected == total {
                                "取消全选"
                            } else {
                                "全选"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let all = this
                                    .resolve
                                    .get(&select_all_id)
                                    .map(|session| {
                                        session.panel.selected_count()
                                            == session.panel.resources.len()
                                    })
                                    .unwrap_or(false);
                                if let Some(session) = this.resolve.get_mut(&select_all_id) {
                                    session.panel.select_all(!all);
                                    session.touch(model::now_ms());
                                }
                                this.resolve.mark_dirty();
                                cx.notify();
                            })),
                    ),
            );
        for (index, card) in session.panel.resources.iter().enumerate() {
            let checked = card.selected;
            let row_id = session.id.clone();
            let icon = RdmIcon::for_file(&card.resource.name);
            let size = match card.resource.size_bytes {
                Some(size) if size > 0 => model::format_bytes(size),
                _ => "大小未知".to_string(),
            };
            let kind = agent::resource_kind_label(&card.resource.kind);
            column = column.child(
                h_flex()
                    .id(ElementId::Name(
                        format!("resolve-resource-{}", card.resource.url).into(),
                    ))
                    .gap(px(8.0))
                    .p(px(6.0))
                    .rounded(px(theme::RADIUS_SM))
                    .items_center()
                    .bg(if checked { t.accent_soft } else { t.bg_app })
                    .cursor_pointer()
                    .child(if checked {
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.0))
                            .rounded(px(4.0))
                            .bg(t.accent)
                            .child(RdmIcon::Check.sized(11.0, t.on_accent))
                            .into_any_element()
                    } else {
                        div()
                            .flex_none()
                            .size(px(16.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(t.border_control)
                            .into_any_element()
                    })
                    .child(icon.sized(16.0, t.text_tertiary))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(0.0))
                            .gap(px(1.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .text_color(t.text_primary)
                                    .child(if card.resource.name.trim().is_empty() {
                                        "未命名资源".to_string()
                                    } else {
                                        card.resource.name.clone()
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap(px(6.0))
                                    .items_center()
                                    .child(soft_tag(kind, t.text_secondary, t.bg_hover))
                                    .child(
                                        div()
                                            .text_size(px(10.5))
                                            .text_color(t.text_tertiary)
                                            .child(size),
                                    ),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(session) = this.resolve.get_mut(&row_id) {
                            session.panel.toggle_resource(index);
                            session.touch(model::now_ms());
                        }
                        this.resolve.mark_dirty();
                        cx.notify();
                    })),
            );
        }
        column = column.child(
            Button::new("resolve-download-selected")
                .primary()
                .small()
                .label(format!("下载所选（{selected}）"))
                .disabled(selected == 0)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.download_selected_resources(&download_id, cx);
                })),
        );
        column.into_any_element()
    }

    /// 空态：还没有解析会话时展示入口说明（输入框固定在底部）。
    fn render_resolve_welcome(&self, t: &ThemeTokens, _cx: &Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h(px(0.0))
            .child(
                v_flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .p(px(24.0))
                    .child(RdmIcon::Sparkle.sized(34.0, t.accent))
                    .child(
                        div()
                            .text_size(px(18.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_primary)
                            .child("资源解析"),
                    )
                    .child(
                        div()
                            .max_w(px(520.0))
                            .text_size(px(12.0))
                            .text_color(t.text_secondary)
                            .child("粘贴一个网页链接开始解析；解析历史会自动保存在左侧，可回看/置顶/分组/删除。需要 AI 调试或修补解析经验？到「解析经验」页使用右侧经验助手。"),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .flex_wrap()
                            .justify_center()
                            .child(soft_tag("粘贴链接 → 解析", t.accent, t.accent_soft))
                            .child(soft_tag("历史只读保留", t.text_secondary, t.bg_hover))
                            .child(soft_tag("经验助手在解析经验页", t.info, t.info_soft)),
                    ),
            )
            .into_any_element()
    }

    /// 工作流卡片：步骤时间线 + 资源勾选 + 下一步动作（按会话 id 操作，不依赖「当前」）。
    fn render_agent_plan(
        &self,
        session: &crate::resolve_session::ResolveSession,
        plan: &AgentPlan,
        t: &ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let now = model::now_ms();
        let session_id = session.id.clone();
        let (status_icon, status_color, status_bg) = match plan.state {
            AgentPlanState::Running => (RdmIcon::Loader, t.info, t.info_soft),
            AgentPlanState::Completed => (RdmIcon::CheckCircle, t.success, t.success_soft),
            AgentPlanState::Failed => (RdmIcon::ErrorCircle, t.danger_text, t.danger_soft),
            AgentPlanState::Cancelled => (RdmIcon::Close, t.text_tertiary, t.bg_hover),
        };
        let mut card = v_flex()
            .gap(px(7.0))
            .p(px(10.0))
            .w_full()
            .rounded(px(theme::RADIUS_SM))
            .bg(t.bg_surface)
            .border_1()
            .border_color(t.border_subtle);
        card = card.child(
            h_flex()
                .gap(px(6.0))
                .items_center()
                .child(RdmIcon::Bot.sized(15.0, t.accent))
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("下载工作流"),
                )
                .child(mono_text(plan.progress_label(), 10.5, t.text_tertiary))
                .child(status_pill(
                    t,
                    status_icon,
                    plan.state.label_zh(),
                    status_color,
                    status_bg,
                ))
                .child(div().flex_1())
                .child(mono_text(
                    format!("{:.1}s", plan.elapsed_ms(now) as f64 / 1000.0),
                    10.5,
                    t.text_tertiary,
                )),
        );
        for step in &plan.steps {
            let (icon, color) = match step.status {
                StepStatus::Pending => (RdmIcon::Circle, t.text_tertiary),
                StepStatus::Running => (RdmIcon::Loader, t.info),
                StepStatus::Done => (RdmIcon::CheckCircle, t.success),
                StepStatus::Failed => (RdmIcon::ErrorCircle, t.danger_text),
                StepStatus::Skipped => (RdmIcon::Minus, t.text_tertiary),
            };
            let mut row = h_flex()
                .gap(px(6.0))
                .items_start()
                .w_full()
                .child(icon.sized(13.0, color))
                .child(
                    v_flex()
                        .gap(px(1.0))
                        .flex_1()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(t.text_primary)
                                .child(step.tool.title()),
                        )
                        .when_some(step.detail.clone(), |this, detail| {
                            this.child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(t.text_tertiary)
                                    .child(detail),
                            )
                        }),
                )
                .child(soft_tag(step.status.label_zh(), color, t.bg_hover));
            if let Some(duration) = step.duration_ms(now) {
                row = row.child(mono_text(format!("{duration}ms"), 10.0, t.text_tertiary));
            } else if step.status == StepStatus::Running {
                row = row.child(mono_text("进行中…", 10.0, t.info));
            }
            card = card.child(row);
        }
        let total = session.panel.resources.len();
        let selected = session.panel.selected_count();
        if total > 0 && plan.state != AgentPlanState::Cancelled {
            card = card.child(divider(t));
            card = card.child(
                h_flex().gap(px(6.0)).items_center().child(
                    div()
                        .flex_1()
                        .text_size(px(11.0))
                        .text_color(t.text_secondary)
                        .child(format!(
                            "{total} 个可下载资源 · 已选 {selected}（清单在下方）"
                        )),
                ),
            );
            if plan.state == AgentPlanState::Completed || plan.state == AgentPlanState::Failed {
                let all_id = session_id.clone();
                let selected_id = session_id.clone();
                card = card.child(
                    h_flex()
                        .gap(px(6.0))
                        .flex_wrap()
                        .child(
                            Button::new("agent-download-all")
                                .primary()
                                .small()
                                .label(format!("下载全部（{total}）"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.agent_download_now(&all_id, DownloadSelection::All, cx);
                                })),
                        )
                        .child(
                            Button::new("agent-download-selected")
                                .ghost()
                                .small()
                                .label(format!("下载所选（{selected}）"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.agent_download_now(
                                        &selected_id,
                                        DownloadSelection::Selected,
                                        cx,
                                    );
                                })),
                        ),
                );
            }
        }
        if plan.state == AgentPlanState::Failed || plan.state == AgentPlanState::Completed {
            let retry_id = session_id.clone();
            card = card.child(
                h_flex()
                    .gap(px(6.0))
                    .flex_wrap()
                    .child(
                        Button::new("agent-retry")
                            .small()
                            .ghost()
                            .tooltip("跳过本地经验，调用 AI 重新解析（保留上次更丰富的资源）")
                            .label(if plan.state == AgentPlanState::Failed {
                                "重试解析（AI）"
                            } else {
                                "重新解析（AI）"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.retry_agent_plan(&retry_id, cx);
                            })),
                    )
                    .child(
                        Button::new("agent-open-settings")
                            .small()
                            .ghost()
                            .label(if self.ai_config.is_configured() {
                                "AI 设置"
                            } else {
                                "配置 AI（可选）"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_settings = true;
                                this.pending_settings_section = Some(SettingsSection::Ai);
                                cx.notify();
                            })),
                    ),
            );
        }
        card.into_any_element()
    }

    fn render_status_strip(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let active = active_count(&self.items);
        let speed = global_speed_bps(&self.items);
        h_flex()
            .h(px(theme::STATUSBAR_H))
            .flex_shrink_0()
            .px(px(12.0))
            .gap(px(12.0))
            .items_center()
            .bg(t.bg_surface)
            .border_t_1()
            .border_color(t.border_subtle)
            .child(
                h_flex()
                    .gap(px(4.0))
                    .items_center()
                    .child(status_dot(
                        if active > 0 {
                            t.accent
                        } else {
                            t.text_tertiary
                        },
                        6.0,
                    ))
                    .child(mono_text(model::format_speed(speed), 11.0, t.text_primary)),
            )
            .child(status_text(&t, format!("{active} 个下载中")))
            .child(status_text(
                &t,
                format!("限速 {}", format_limit(&self.settings)),
            ))
            .child(div().flex_1())
            // AI 消耗快捷入口（有记录时才出现，点一下进资源监控）。
            .when(self.ai_ledger.totals.total_tokens > 0, |this| {
                this.child(
                    h_flex()
                        .id("status-ai-tokens")
                        .items_center()
                        .gap(px(4.0))
                        .px(px(4.0))
                        .rounded(px(theme::RADIUS_XS))
                        .cursor_pointer()
                        .hover(|style| style.bg(t.bg_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_page(AppPage::Monitor, cx);
                        }))
                        .child(RdmIcon::Sparkle.sized(12.0, t.info))
                        .child(mono_text(
                            format!(
                                "{} tokens",
                                monitor::format_tokens(self.ai_ledger.totals.total_tokens)
                            ),
                            10.5,
                            t.text_secondary,
                        )),
                )
                .when(self.ai_ledger.cost_cny > 0.0, |this| {
                    this.child(mono_text(
                        format!("· {}", monitor::format_cny_approx(self.ai_ledger.cost_cny)),
                        10.5,
                        t.info,
                    ))
                })
            })
            .child(status_text(&t, format!("v{}", env!("CARGO_PKG_VERSION"))))
    }

    fn render_toasts(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.toasts.is_empty() {
            return None;
        }
        let t = tokens(cx);
        let mut stack = v_flex()
            .absolute()
            .right(px(16.0))
            .bottom(px(theme::STATUSBAR_H + 12.0))
            .gap(px(8.0))
            .w(px(340.0));
        for (index, toast) in self.toasts.iter().enumerate() {
            let (fg, bg) = match toast.severity {
                Severity::Info => (t.info, t.bg_elevated),
                Severity::Success => (t.success, t.bg_elevated),
                Severity::Warning => (t.warning, t.bg_elevated),
                Severity::Danger => (t.danger, t.bg_elevated),
            };
            let icon = match toast.severity {
                Severity::Info => RdmIcon::Info,
                Severity::Success => RdmIcon::CheckCircle,
                Severity::Warning => RdmIcon::Alert,
                Severity::Danger => RdmIcon::ErrorCircle,
            };
            let mut card = h_flex()
                .gap(px(8.0))
                .items_center()
                .p(px(10.0))
                .rounded(px(theme::RADIUS_MD))
                .bg(bg)
                .border_1()
                .border_color(t.border_default)
                .shadow_lg()
                .child(icon.sized(16.0, fg))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(t.text_primary)
                        .child(toast.text.clone()),
                );
            if toast.action.is_some() {
                card = card.child(
                    Button::new(ElementId::Name(format!("toast-undo-{index}").into()))
                        .small()
                        .ghost()
                        .label("撤销")
                        .on_click(cx.listener(move |this, _, _, cx| this.toast_undo(index, cx))),
                );
            }
            stack = stack.child(card);
        }
        Some(stack.into_any_element())
    }

    /// Toast 上的「撤销」：移除该 toast 并执行对应撤销动作。
    fn toast_undo(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.toasts.len() {
            return;
        }
        let action = self.toasts.remove(index).action;
        if let Some(action) = action {
            self.undo(action, cx);
        }
        cx.notify();
    }
    // -----------------------------------------------------------------------
    // 命令面板 / 添加下载弹层
    // -----------------------------------------------------------------------

    fn filtered_commands(&self, cx: &Context<Self>) -> Vec<Command> {
        let query = self.palette_query.read(cx).value().to_lowercase();
        Command::ALL
            .into_iter()
            .filter(|command| {
                query.is_empty()
                    || command.label_zh().to_lowercase().contains(&query)
                    || command.group().to_lowercase().contains(&query)
            })
            .collect()
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_index = 0;
        self.palette_query.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        cx.notify();
    }

    fn move_palette(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.filtered_commands(cx).len();
        if count == 0 {
            return;
        }
        let next = (self.palette_index as isize + delta).rem_euclid(count as isize);
        self.palette_index = next as usize;
        cx.notify();
    }

    fn confirm_palette(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette_open {
            return;
        }
        let commands = self.filtered_commands(cx);
        let Some(command) = commands
            .get(self.palette_index.min(commands.len().saturating_sub(1)))
            .copied()
        else {
            return;
        };
        self.palette_open = false;
        self.run_command(command, cx);
        cx.notify();
    }

    /// 打开「添加下载」弹层（Ctrl+N / 顶栏「添加」/ 命令面板）。
    fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_url
            .update(cx, |input, cx| input.set_value("", window, cx));
        let save_dir = self
            .settings
            .default_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "未设置（使用系统默认下载目录）".to_string());
        let weak = cx.entity().downgrade();
        let add_url = self.add_url.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let hint_color = gpui_component::ActiveTheme::theme(cx).muted_foreground;
            let weak_ok = weak.clone();
            dialog
                .title("添加下载")
                .child(
                    v_flex()
                        .gap(px(8.0))
                        .w_full()
                        .child(Input::new(&add_url).cleanable(true))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(hint_color)
                                .child(format!("保存到：{save_dir}")),
                        ),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("开始下载")
                        .cancel_text("取消"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let mut submitted = false;
                    let _ = weak_ok.update(cx, |app, cx| {
                        submitted = app.submit_add_from_dialog(cx);
                    });
                    submitted
                })
        });
        cx.notify();
    }

    /// 读取添加弹层的 URL 并入队；返回是否成功（成功时对话框关闭）。
    fn submit_add_from_dialog(&mut self, cx: &mut Context<Self>) -> bool {
        let raw = self.add_url.read(cx).value().trim().to_string();
        if raw.is_empty() {
            self.toast("请输入下载链接", Severity::Warning);
            return false;
        }
        let request = match validate_url(&raw) {
            Ok(url) => self.settings.request_with_defaults(url),
            Err(err) => {
                self.toast(err, Severity::Danger);
                return false;
            }
        };
        match model::prepare_request(&self.settings, request) {
            Ok(request) => {
                let id = self.push_download(request, "gui".to_string());
                tracing::info!(download_id = %id, "GUI 添加下载（弹层）");
                self.toast(format!("已添加下载 {id}"), Severity::Success);
                cx.notify();
                true
            }
            Err(err) => {
                self.toast(err, Severity::Danger);
                false
            }
        }
    }

    fn render_palette(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.palette_open {
            return None;
        }
        let t = tokens(cx);
        let commands = self.filtered_commands(cx);
        let selected = self.palette_index.min(commands.len().saturating_sub(1));
        let mut rows = v_flex().gap(px(2.0));
        for (index, command) in commands.iter().enumerate() {
            let command = *command;
            let is_selected = index == selected;
            rows = rows.child(
                h_flex()
                    .id(ElementId::Name(format!("palette-{index}").into()))
                    .px(px(10.0))
                    .py(px(7.0))
                    .gap(px(8.0))
                    .rounded(px(theme::RADIUS_SM))
                    .bg(if is_selected {
                        t.accent_soft
                    } else {
                        t.bg_elevated
                    })
                    .text_color(if is_selected {
                        t.accent
                    } else {
                        t.text_primary
                    })
                    .cursor_pointer()
                    .child(command.icon().sized(14.0, t.text_tertiary))
                    .child(div().flex_1().text_size(px(13.0)).child(command.label_zh()))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(t.text_tertiary)
                            .child(command.group()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.palette_open = false;
                        this.run_command(command, cx);
                        cx.notify();
                    })),
            );
        }
        if commands.is_empty() {
            rows = rows.child(
                div()
                    .px(px(10.0))
                    .py(px(12.0))
                    .text_size(px(12.0))
                    .text_color(t.text_tertiary)
                    .child("没有匹配的命令"),
            );
        }
        Some(
            v_flex()
                .id("palette-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .items_center()
                .pt(px(96.0))
                .bg(t.bg_overlay)
                .on_click(cx.listener(|this, _, _, cx| this.close_palette(cx)))
                .child(
                    v_flex()
                        .id("palette-panel")
                        .w(px(560.0))
                        .gap(px(8.0))
                        .p(px(10.0))
                        .rounded(px(theme::RADIUS_LG))
                        .bg(t.bg_elevated)
                        .border_1()
                        .border_color(t.border_default)
                        .shadow_lg()
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .child(Input::new(&self.palette_query).cleanable(false))
                        .child(
                            h_flex()
                                .items_center()
                                .child(section_label(&t, "命令"))
                                .child(div().flex_1())
                                .child(icon_button(
                                    "palette-close",
                                    RdmIcon::Close,
                                    "关闭命令面板",
                                    cx.listener(|this, _, _, cx| this.close_palette(cx)),
                                )),
                        )
                        .child(rows)
                        .child(divider(&t))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(t.text_tertiary)
                                .child("输入筛选 · Enter 执行 · 点击选择 · Esc 关闭"),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// 过滤 + 排序（纯函数，便于单测）：返回 `items` 的下标序列。
pub(crate) fn filter_and_sort(
    items: &[DownloadItem],
    filter: ViewFilter,
    query: &str,
    sort: SortKey,
    ascending: bool,
) -> Vec<usize> {
    let query = query.to_lowercase();
    let mut indexes: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| filter.matches(item))
        .filter(|(_, item)| {
            // 审计 F-21：搜索要匹配「用户看得见的文案」——来源在行/详情里显示为
            // 中文（浏览器/命令行/Agent/桌面端），只搜原始 token 会让「浏览器」搜不到。
            query.is_empty()
                || item.file_name.to_lowercase().contains(&query)
                || item.url.to_lowercase().contains(&query)
                || item.source.to_lowercase().contains(&query)
                || source_label(&item.source).contains(&query)
        })
        .map(|(index, _)| index)
        .collect();
    indexes.sort_by(|&a, &b| {
        let left = &items[a];
        let right = &items[b];
        let ordering = match sort {
            SortKey::Time => left.created_ms.cmp(&right.created_ms),
            SortKey::Name => left
                .file_name
                .to_lowercase()
                .cmp(&right.file_name.to_lowercase()),
            SortKey::Size => left.total.unwrap_or(0).cmp(&right.total.unwrap_or(0)),
            // 生命周期 ordinal，而不是中文文案的字典序（审计 F-21）。
            SortKey::State => left.state.sort_rank().cmp(&right.state.sort_rank()),
            SortKey::Source => source_label(&left.source).cmp(source_label(&right.source)),
            SortKey::Speed => left
                .speed_bps
                .partial_cmp(&right.speed_bps)
                .unwrap_or(std::cmp::Ordering::Equal),
        };
        if ascending {
            ordering
        } else {
            ordering.reverse()
        }
    });
    indexes
}

/// 速度迷你曲线（canvas 柱状，最近 60 个采样；绘制限制在 72×height 画布内，
/// 采样满历史时不会溢出覆盖右侧状态徽章，详见 [`sparkline`]）。
fn render_sparkline(item: &DownloadItem, color: Hsla, height: f32) -> AnyElement {
    let history: Vec<f64> = item.speed_history.iter().copied().collect();
    sparkline(history, color, 72.0, height)
}

/// 相对时间（任务创建时间 → 「x 分钟前」）；避免引入 chrono 依赖。
fn format_relative_time(created_ms: u64) -> String {
    let now = model::now_ms();
    if created_ms >= now {
        return "刚刚".to_string();
    }
    let secs = (now - created_ms) / 1000;
    if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86_400)
    }
}

/// 详情面板的「标签 + 值」行（值可等宽）。
fn detail_row(label: &'static str, value: String, mono: bool, t: &ThemeTokens) -> AnyElement {
    h_flex()
        .w_full()
        .gap(px(8.0))
        .items_start()
        .child(
            div()
                .w(px(64.0))
                .flex_none()
                .text_size(px(11.0))
                .text_color(t.text_tertiary)
                .child(label),
        )
        .child(if mono {
            mono_text(value, 11.0, t.text_secondary).into_any_element()
        } else {
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(12.0))
                .text_color(t.text_secondary)
                .child(value)
                .into_any_element()
        })
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 解析经验页回调适配（design 17）：views/experience.rs 是纯视图 + Box<dyn Fn> 回调，
// 这里把回调桥接到 `RdmApp`（monitor 页使用同一模式）。
// ---------------------------------------------------------------------------

/// 无参经验页动作 → `cx.listener` 风格的 `Box<dyn Fn>`。
fn experience_simple_callback(
    entity: Entity<RdmApp>,
    handler: impl Fn(&mut RdmApp, &mut Window, &mut Context<RdmApp>) + 'static,
) -> ExperienceSimpleCallback {
    Box::new(
        move |_event: &ClickEvent, window: &mut Window, app: &mut App| {
            entity.update(app, |this, cx| handler(this, window, cx));
        },
    )
}

/// 携带经验 id 的经验页动作。
fn experience_id_callback(
    entity: Entity<RdmApp>,
    handler: impl Fn(&mut RdmApp, &str, &mut Window, &mut Context<RdmApp>) + 'static,
) -> ExperienceIdCallback {
    Box::new(
        move |id: &str, _event: &ClickEvent, window: &mut Window, app: &mut App| {
            let id = id.to_owned();
            entity.update(app, |this, cx| handler(this, &id, window, cx));
        },
    )
}

/// 携带自由文本（经验助手输入 / 快捷指令）的经验页动作。
fn experience_text_callback(
    entity: Entity<RdmApp>,
    handler: impl Fn(&mut RdmApp, &str, &mut Window, &mut Context<RdmApp>) + 'static,
) -> ExperienceTextCallback {
    Box::new(
        move |text: &str, _event: &ClickEvent, window: &mut Window, app: &mut App| {
            let text = text.to_owned();
            entity.update(app, |this, cx| handler(this, &text, window, cx));
        },
    )
}

/// 携带 (experience_id, url) 的经验页动作（「使用此经验解析」）。
fn experience_apply_callback(
    entity: Entity<RdmApp>,
    handler: impl Fn(&mut RdmApp, &str, &str, &mut Window, &mut Context<RdmApp>) + 'static,
) -> ExperienceApplyCallback {
    Box::new(
        move |id: &str, url: &str, _event: &ClickEvent, window: &mut Window, app: &mut App| {
            let id = id.to_owned();
            let url = url.to_owned();
            entity.update(app, |this, cx| handler(this, &id, &url, window, cx));
        },
    )
}

/// 携带 (target_id, source_ids) 的经验页动作（同类经验合并）。
fn experience_merge_callback(
    entity: Entity<RdmApp>,
    handler: impl Fn(&mut RdmApp, &str, &[String], &mut Window, &mut Context<RdmApp>) + 'static,
) -> ExperienceMergeCallback {
    Box::new(
        move |target: &str,
              sources: &[String],
              _event: &ClickEvent,
              window: &mut Window,
              app: &mut App| {
            let target = target.to_owned();
            let sources = sources.to_vec();
            entity.update(app, |this, cx| handler(this, &target, &sources, window, cx));
        },
    )
}

/// 携带经验 id + 版本号的经验页动作。
fn experience_version_callback(
    entity: Entity<RdmApp>,
    handler: impl Fn(&mut RdmApp, &str, u32, &mut Window, &mut Context<RdmApp>) + 'static,
) -> ExperienceVersionCallback {
    Box::new(
        move |id: &str, version: u32, _event: &ClickEvent, window: &mut Window, app: &mut App| {
            let id = id.to_owned();
            entity.update(app, |this, cx| handler(this, &id, version, window, cx));
        },
    )
}

/// 解析状态卡不展示的内部诊断 note（草稿落盘路径/模型辅助草稿等）：
/// 这些是给 CLI/日志排查用的实现细节，展示给用户只会造成噪声。
fn is_internal_resolve_note(note: &str) -> bool {
    let note = note.trim();
    note.starts_with("draft saved:")
        || note.starts_with("model-assisted draft saved:")
        || note.contains("\\resolve\\drafts\\")
        || note.contains("/resolve/drafts/")
}

/// 解析状态 → (图标, 前景色, 背景色)：会话行 / 线程标题 / 状态卡共用同一映射，
/// 避免同一状态在不同位置出现不同颜色或不同图标（语义一致）。
fn resolve_status_style(status: &model::AiParseStatus, t: &ThemeTokens) -> (RdmIcon, Hsla, Hsla) {
    match status {
        model::AiParseStatus::Idle => (RdmIcon::Circle, t.text_tertiary, t.bg_hover),
        model::AiParseStatus::Parsing => (RdmIcon::Loader, t.info, t.info_soft),
        model::AiParseStatus::Ready => (RdmIcon::CheckCircle, t.success, t.success_soft),
        model::AiParseStatus::Failed => (RdmIcon::ErrorCircle, t.danger_text, t.danger_soft),
    }
}

/// 会话消息气泡（用户/助手/系统统一渲染；角色 → 颜色的唯一映射）。
fn render_resolve_message(message: &model::AiChatMessage, t: &ThemeTokens) -> AnyElement {
    let (role_color, text_color) = match message.role {
        model::AiRole::User => (t.text_tertiary, t.text_primary),
        model::AiRole::Assistant => (t.accent, t.text_primary),
        model::AiRole::System => (t.warning, t.text_secondary),
    };
    v_flex()
        .gap(px(2.0))
        .w_full()
        .child(
            div()
                .text_size(px(10.5))
                .text_color(role_color)
                .child(message.role.label_zh()),
        )
        .child(
            div()
                .text_size(px(12.5))
                .text_color(text_color)
                .child(message.text.clone()),
        )
        .into_any_element()
}

fn source_label(source: &str) -> &'static str {
    match source {
        "browser" | "extension" | "firefox" | "chrome" | "ipc" => "浏览器",
        "cli" => "命令行",
        "agent" | "mcp" => "Agent",
        "ai" => "AI 解析",
        "gui" => "桌面端",
        _ => "其他",
    }
}

fn source_icon(source: &str) -> RdmIcon {
    match source {
        "browser" | "extension" | "firefox" | "chrome" | "ipc" => RdmIcon::Globe,
        "cli" | "agent" | "mcp" => RdmIcon::Command,
        "ai" => RdmIcon::Sparkle,
        _ => RdmIcon::Monitor,
    }
}

/// 从 URL 提取脱敏 host（AI 账本只保存 host，绝不保存 path/query/cookie）。
///
/// 已删除本文件内的私有实现：统一复用 [`model::url_host`]（同一归一化规则，
/// 与解析凭据 host 比较一致）。
#[cfg(test)]
mod monitor_helpers_tests {
    use crate::model::url_host;

    #[test]
    fn source_host_is_lowercase_and_query_free() {
        assert_eq!(
            url_host("https://Example.COM./path?token=secret#frag"),
            Some("example.com".to_owned())
        );
        assert_eq!(
            url_host("http://127.0.0.1:8080/file"),
            Some("127.0.0.1".to_owned())
        );
        assert_eq!(url_host("not a url"), None);
        assert_eq!(url_host("file:///C:/x"), None);
    }
}

fn for_test_hint(t: &ThemeTokens, warning: Option<String>) -> AnyElement {
    match warning {
        Some(warning) => {
            info_banner(t, RdmIcon::Alert, Severity::Warning, warning, vec![]).into_any_element()
        }
        None => div().into_any_element(),
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

impl RdmApp {
    // -----------------------------------------------------------------------
    // 设置窗口接线 / 演示数据（开发与截图钩子）
    // -----------------------------------------------------------------------

    /// 供设置页读取 AI 配置。
    pub(crate) fn ai_config_snapshot(&self) -> AiConfig {
        self.ai_config.clone()
    }

    /// 设置页保存 AI 配置（写 `<state>/ai.json`，解析/聊天立即使用新配置）。
    pub(crate) fn apply_ai_config(&mut self, config: AiConfig, cx: &mut Context<Self>) {
        self.ai_config = config.clone();
        if let Err(err) = config.save(&self.state_dir) {
            self.toast(format!("保存 AI 配置失败：{err}"), Severity::Danger);
        }
        // 成功不弹全局 toast：设置页已做行内「已保存」，避免双重提示（05 §B8）。
        cx.notify();
    }

    /// 浏览器集成是否正常（IPC endpoint 已发布）。
    pub(crate) fn browser_integration_ok(&self) -> bool {
        self.ipc_warning.is_none()
    }

    /// 浏览器集成警告文案（正常时为 None）。
    pub(crate) fn browser_integration_warning(&self) -> Option<String> {
        self.ipc_warning.clone()
    }

    /// 打开状态目录（浏览器集成诊断页用）。
    pub(crate) fn open_state_directory(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = open_path::open_directory(&self.state_dir) {
            self.toast(format!("打开状态目录失败：{err}"), Severity::Danger);
        }
        cx.notify();
    }

    /// 供设置页读取当前设置（设置页 Entity 持有自己的草稿，提交时回调这里）。
    pub(crate) fn settings_snapshot(&self) -> Settings {
        self.settings.clone()
    }

    pub(crate) fn state_dir_path(&self) -> PathBuf {
        self.state_dir.clone()
    }

    /// 设置页提交：立即生效 + 落盘 + 同步系统集成/主题。
    pub(crate) fn apply_settings(
        &mut self,
        new_settings: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old = std::mem::replace(&mut self.settings, new_settings.normalized());
        // 分类规则保存后立即生效：侧栏筛选/图标/监控扫描读同一份进程内快照
        // （`docs/design/16` §4.1；磁盘分类在下次 start_item 时用 `settings` 规则）。
        model::set_classification_rules(&self.settings.classification);
        // 主题/强调色/减少动效即时生效。
        let resolved = self.settings.resolved_ui_theme();
        let dark = theme::resolve_dark(resolved, window);
        self.dark = dark;
        self.density = self.settings.density;
        theme::apply(cx, dark, self.settings.accent, self.settings.reduce_motion);
        self.settings.sync_legacy_theme(dark);
        if let Err(err) = model::save_settings(&self.state_dir, &self.settings) {
            self.toast(format!("保存设置失败：{err}"), Severity::Danger);
        }
        // 系统集成（托盘/自启动）只在相关开关变化时重同步。
        if old.minimize_to_tray != self.settings.minimize_to_tray
            || old.silent_start != self.settings.silent_start
            || old.launch_at_startup != self.settings.launch_at_startup
        {
            if let Some(warning) = self.sync_system_integration() {
                self.toast(warning, Severity::Warning);
            }
        }
        // 成功不弹全局 toast：设置页行内提示「已保存」即可（05 §B8）。
        cx.notify();
    }

    /// 设置页面关闭。
    pub(crate) fn close_settings_page(&mut self, cx: &mut Context<Self>) {
        self.show_settings = false;
        cx.notify();
    }

    /// 开发/截图钩子（`RDM_TEST_UI_STATE=demo`）：注入一组覆盖各状态与来源的示例任务。
    ///
    /// 只在隔离 `LOCALAPPDATA` 的验收环境使用，不写用户文档；不会访问网络，
    /// 任务只是内存状态（会被下载列表持久化到隔离状态目录）。
    pub fn seed_demo_state(&mut self, cx: &mut Context<Self>) {
        use crate::model::{DownloadItem, ItemState};
        let demo = [
            (
                "演示视频-4K.mp4",
                "browser",
                ItemState::Downloading,
                2_400_000_000u64,
                1_488_000_000u64,
                2_360_000.0,
                Some("D:\\Downloads\\影视\\演示视频-4K.mp4"),
            ),
            (
                "数据集-2026-10.tar.gz",
                "cli",
                ItemState::Downloading,
                860_000_000,
                55_000_000,
                6_100_000.0,
                Some("D:\\Downloads\\归档\\数据集-2026-10.tar.gz"),
            ),
            (
                "项目素材包.zip",
                "extension",
                ItemState::Paused,
                512_000_000,
                214_000_000,
                0.0,
                Some("D:\\Downloads\\归档\\项目素材包.zip"),
            ),
            (
                "产品手册.pdf",
                "browser",
                ItemState::Completed,
                18_400_000,
                18_400_000,
                0.0,
                Some("D:\\Downloads\\文档\\产品手册.pdf"),
            ),
            (
                "离线安装包.msi",
                "agent",
                ItemState::Failed,
                1_200_000_000,
                340_000_000,
                0.0,
                Some("D:\\Downloads\\软件\\离线安装包.msi"),
            ),
            (
                "壁纸合集-4K.zip",
                "browser",
                ItemState::Completed,
                96_000_000,
                96_000_000,
                0.0,
                Some("D:\\Downloads\\图片\\壁纸合集-4K.zip"),
            ),
            (
                "会议录像-2026-10-20.mp4",
                "browser",
                ItemState::Completed,
                3_200_000_000,
                3_200_000_000,
                0.0,
                Some("D:\\Downloads\\视频\\会议录像-2026-10-20.mp4"),
            ),
            (
                "日志-诊断.txt",
                "cli",
                ItemState::Failed,
                120_000,
                0,
                0.0,
                None,
            ),
        ];
        for (file_name, source, state, total, downloaded, speed, path) in demo {
            let url = format!("https://example.com/files/{file_name}");
            let mut request = self.settings.request_with_defaults(url);
            request.file_name = Some(file_name.to_string());
            let id = self.allocate_id();
            let mut item = DownloadItem::new(id, request, source.to_string());
            item.file_name = file_name.to_string();
            item.state = state;
            item.total = Some(total);
            item.downloaded = downloaded;
            item.speed_bps = speed;
            item.eta_secs = if speed > 0.0 && total > downloaded {
                Some((total - downloaded) as f64 / speed)
            } else {
                None
            };
            item.active_workers = if state == ItemState::Downloading {
                8
            } else {
                0
            };
            item.target_connections = 8;
            item.peak_connections = 8;
            item.range_supported = Some(true);
            item.path = path.map(PathBuf::from);
            if state == ItemState::Downloading {
                item.note = Some("动态线程规划中".to_string());
                for i in 0..40 {
                    item.push_speed_sample(speed * (0.85 + (i as f64 * 0.37).sin() * 0.15));
                }
            } else if state == ItemState::Failed {
                item.error = Some("服务器返回 403（需要登录），已自动重试 3 次".to_string());
            }
            self.items.push(item);
        }
        // 资源监控演示数据：给前 4 个任务注入可展示的 AI 成本（只写隔离状态目录）。
        // 真实截图证据走安装版 + mock AI（scripts/dev-monitor-e2e.ps1），这里是开发自检用。
        let now = model::now_ms();
        let demo_ai: [(&str, u64, u64, bool); 4] = [
            ("deepseek-flash", 1_840, 210, false),
            ("deepseek-v4-pro", 960, 180, false),
            ("deepseek-flash", 0, 120, true),
            ("deepseek-flash", 320, 0, false),
        ];
        let demo_ids: Vec<String> = self
            .items
            .iter()
            .take(4)
            .map(|item| item.id.clone())
            .collect();
        for (index, id) in demo_ids.iter().enumerate() {
            let Some((model_name, parse_tokens, classify_tokens, cache_hit)) =
                demo_ai.get(index).copied()
            else {
                break;
            };
            let Some(item) = self.items.iter_mut().find(|item| &item.id == id) else {
                continue;
            };
            let parse_usage = rdm_ai::TokenUsage {
                prompt_tokens: parse_tokens.saturating_mul(3) / 4,
                completion_tokens: parse_tokens / 4,
                total_tokens: parse_tokens,
                calls: u32::from(parse_tokens > 0),
                estimated: false,
            };
            let classify_usage = rdm_ai::TokenUsage {
                prompt_tokens: classify_tokens.saturating_mul(3) / 4,
                completion_tokens: classify_tokens / 4,
                total_tokens: classify_tokens,
                calls: u32::from(classify_tokens > 0),
                estimated: false,
            };
            let parse_at = now.saturating_sub((3 - index as u64) * 6 * 60 * 60 * 1000);
            let parse_estimate = rdm_ai::providers::estimate_cost(
                rdm_ai::providers::ProviderKind::DeepSeek,
                model_name,
                &parse_usage,
                parse_at,
            );
            let classify_estimate = rdm_ai::providers::estimate_cost(
                rdm_ai::providers::ProviderKind::DeepSeek,
                model_name,
                &classify_usage,
                now,
            );
            item.ai = Some(ItemAiUsage {
                parse: parse_usage,
                classify: classify_usage,
                parse_cost_cny: if parse_estimate.priced {
                    parse_estimate.cny
                } else {
                    0.0
                },
                classify_cost_cny: if classify_estimate.priced {
                    classify_estimate.cny
                } else {
                    0.0
                },
                model: Some(model_name.to_owned()),
                cache_hit,
                parse_siblings: if index == 1 { 2 } else { 1 },
                updated_ms: now,
            });
        }
        for (index, id) in demo_ids.iter().enumerate() {
            let Some((model_name, parse_tokens, classify_tokens, cache_hit)) =
                demo_ai.get(index).copied()
            else {
                break;
            };
            if parse_tokens > 0 || cache_hit {
                let parse_at = now.saturating_sub((3 - index as u64) * 6 * 60 * 60 * 1000);
                let tokens = rdm_ai::TokenUsage {
                    prompt_tokens: parse_tokens.saturating_mul(3) / 4,
                    completion_tokens: parse_tokens / 4,
                    total_tokens: parse_tokens,
                    calls: u32::from(parse_tokens > 0),
                    estimated: false,
                };
                let estimate = rdm_ai::providers::estimate_cost(
                    rdm_ai::providers::ProviderKind::DeepSeek,
                    model_name,
                    &tokens,
                    parse_at,
                );
                self.ai_ledger.record(
                    monitor::AiUsageRecord {
                        at_ms: parse_at,
                        kind: AiUsageKind::Parse,
                        host: Some("example.com".to_owned()),
                        model: Some(model_name.to_owned()),
                        cache_hit,
                        item_ids: vec![id.clone()],
                        session_id: None,
                        cost_cny: estimate.priced.then_some(estimate.cny),
                        tokens,
                    },
                    now,
                );
            }
            if classify_tokens > 0 {
                let tokens = rdm_ai::TokenUsage {
                    prompt_tokens: classify_tokens.saturating_mul(3) / 4,
                    completion_tokens: classify_tokens / 4,
                    total_tokens: classify_tokens,
                    calls: 1,
                    estimated: false,
                };
                let estimate = rdm_ai::providers::estimate_cost(
                    rdm_ai::providers::ProviderKind::DeepSeek,
                    model_name,
                    &tokens,
                    now,
                );
                self.ai_ledger.record(
                    monitor::AiUsageRecord {
                        at_ms: now,
                        kind: AiUsageKind::Classify,
                        host: None,
                        model: Some(model_name.to_owned()),
                        cache_hit: false,
                        item_ids: vec![id.clone()],
                        session_id: None,
                        cost_cny: estimate.priced.then_some(estimate.cny),
                        tokens,
                    },
                    now,
                );
            }
        }
        self.ai_ledger.save(&self.state_dir);
        self.dirty_persist = true;
        cx.notify();
        if let Ok(raw) = std::env::var("RDM_TEST_UI_STATE_COUNT") {
            if let Ok(count) = raw.parse::<usize>() {
                let states = [
                    ItemState::Completed,
                    ItemState::Paused,
                    ItemState::Downloading,
                    ItemState::Failed,
                    ItemState::Cancelled,
                ];
                let sources = ["browser", "cli", "agent", "extension"];
                for index in 0..count {
                    let file_name = format!(
                        "perf-{index:05}.{}",
                        match index % 5 {
                            0 => "iso",
                            1 => "mp4",
                            2 => "zip",
                            3 => "pdf",
                            _ => "bin",
                        }
                    );
                    let state = states[index % states.len()];
                    let total = 1_000_000u64 + (index as u64 % 500) * 4_000_000;
                    let mut request = self
                        .settings
                        .request_with_defaults(format!("https://example.com/perf/{file_name}"));
                    request.file_name = Some(file_name.clone());
                    let id = format!("perf-{index:05}");
                    let mut item =
                        DownloadItem::new(id, request, sources[index % sources.len()].to_string());
                    item.file_name = file_name;
                    item.state = state;
                    item.total = Some(total);
                    item.downloaded = match state {
                        ItemState::Completed => total,
                        ItemState::Paused => total / 2,
                        ItemState::Downloading => total / 3,
                        _ => 0,
                    };
                    item.speed_bps = if state == ItemState::Downloading {
                        1_200_000.0 + (index as f64 % 9.0) * 100_000.0
                    } else {
                        0.0
                    };
                    item.active_workers = if state == ItemState::Downloading {
                        8
                    } else {
                        0
                    };
                    item.target_connections = 8;
                    item.peak_connections = 8;
                    item.range_supported = Some(true);
                    item.created_ms = item.created_ms.saturating_sub(index as u64 * 1_000);
                    if state == ItemState::Failed {
                        item.error =
                            Some("服务器返回 403（需要登录），已自动重试 3 次".to_string());
                    }
                    self.items.push(item);
                }
            }
        }
        self.persist_now();
        cx.notify();
    }

    /// 全局热键 `Ctrl+Shift+D`：显示窗口并打开「添加下载」弹层。
    pub(crate) fn quick_add_from_hotkey(&mut self, cx: &mut Context<Self>) {
        // 模态层打开时全局热键短路，不叠加添加弹层（16 §6）。
        if self.modal_layer_blocks_actions() {
            return;
        }
        self.pending_quick_add = true;
        self.show_from_tray(cx);
        cx.notify();
    }

    /// 聚焦万能输入栏（Ctrl+N / Ctrl+L / 启动时）。
    pub fn focus_omnibox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.omnibox.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// 打开设置（全屏模态覆盖层；16 §6 层级隔离）：
    /// - 关闭命令面板，避免浮层越过设置层；
    /// - render 检测到「关闭 → 打开」时关闭所有 gpui-component dialog（添加/删除确认等）；
    /// - 覆盖层容器 `.occlude()` 阻断下层列表/行的鼠标命中（右键菜单、行点击、悬停按钮）。
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.open_settings_section(None, cx);
    }

    /// 打开设置并可定位到指定分类；设置页已创建时立即切换分类。
    pub(crate) fn open_settings_section(
        &mut self,
        section: Option<SettingsSection>,
        cx: &mut Context<Self>,
    ) {
        self.palette_open = false;
        self.show_settings = true;
        if let Some(section) = section {
            if let Some(page) = self.settings_page.clone() {
                page.update(cx, |page, cx| page.select_section(section, cx));
            } else {
                self.pending_settings_section = Some(section);
            }
        }
        cx.notify();
    }

    /// 验收钩子：`RDM_TEST_OPEN_SETTINGS` 在启动后打开设置。
    pub fn open_settings_for_test(
        &mut self,
        section: Option<SettingsSection>,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_section(section, cx);
    }

    /// 模态层（设置覆盖层 / 命令面板）打开时，任务、选择、导航与搜索类动作一律短路
    /// （16 §6「全局快捷键与任务操作在模态层打开时全部短路」）。
    fn modal_layer_blocks_actions(&self) -> bool {
        self.show_settings || self.palette_open
    }

    /// 启动一次性提示（静默启动回退等）。
    pub fn startup_notice(
        &mut self,
        text: impl Into<String>,
        is_error: bool,
        cx: &mut Context<Self>,
    ) {
        self.toast(
            text,
            if is_error {
                Severity::Warning
            } else {
                Severity::Info
            },
        );
        cx.notify();
    }

    /// 把设置里的桌面集成开关落到系统（托盘 / 关闭到托盘 / 开机自启动）。
    fn sync_system_integration(&mut self) -> Option<String> {
        let mut warning: Option<String> = None;
        desktop::set_minimize_to_tray(self.settings.minimize_to_tray);
        // 全局热键跟随「托盘驻留」开关：开启时注册，关闭时释放（避免抢系统快捷键）。
        if self.settings.needs_tray() {
            if !desktop::is_hotkey_installed() {
                if let Err(err) = desktop::install_hotkey() {
                    warning = Some(format!("全局热键不可用（{err}）"));
                }
            }
        } else if desktop::is_hotkey_installed() {
            desktop::shutdown_hotkey();
        }
        if self.settings.needs_tray() {
            if !desktop::is_tray_alive() {
                match desktop::install_tray(&desktop::tray_tooltip()) {
                    Ok(()) => {
                        if self.settings.minimize_to_tray {
                            if let Some(hwnd) = self.hwnd {
                                if let Err(err) = desktop::install_window_hook(hwnd) {
                                    warning = Some(format!("安装托盘窗口钩子失败：{err}"));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        warning = Some(format!("托盘图标不可用（{err}），窗口仍可正常使用"));
                    }
                }
            } else if self.settings.minimize_to_tray {
                if let Some(hwnd) = self.hwnd {
                    if let Err(err) = desktop::install_window_hook(hwnd) {
                        warning = Some(format!("安装托盘窗口钩子失败：{err}"));
                    }
                }
            }
        } else if desktop::is_tray_alive() {
            desktop::shutdown_tray();
        }
        if let Err(err) = desktop::autostart::sync(self.settings.launch_at_startup) {
            warning = Some(format!("开机自启动设置未生效：{err}"));
        }
        warning
    }
}

/// 测试/截图环境变量开关：**只有非空值才算启用**。
///
/// PowerShell 7.6 的 `[Environment]::SetEnvironmentVariable(name, $null, 'Process')`
/// 会把变量留成空字符串；旧的 `std::env::var(..).is_ok()` 会让多个截图钩子同时触
/// 发（曾导致 E2E 截图被命令面板/添加弹层遮住）。空串/`0`/`false` 一律视为关闭。
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// 读取测试/截图文本型环境变量：空白值视为未设置。
fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl Render for RdmApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport_width: f32 = window.viewport_size().width.into();
        self.compact_window = viewport_width < theme::COMPACT_BREAKPOINT;
        // FollowSystem：窗口外观变化时同步（GPUI 每次 render 读一次即可）。
        if self.settings.resolved_ui_theme() == UiThemeMode::FollowSystem {
            let dark = !matches!(
                window.appearance(),
                gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
            );
            self.set_dark_state(dark, cx);
        }
        let t = tokens(cx);

        // 完成/失败通知：在 render 里统一 push 到 Root 通知层（apply_batch 没有 Window）。
        if !self.pending_notifications.is_empty() {
            for (severity, text) in std::mem::take(&mut self.pending_notifications) {
                let notification = match severity {
                    Severity::Success => Notification::success(text),
                    Severity::Danger => Notification::error(text),
                    Severity::Warning => Notification::warning(text),
                    Severity::Info => Notification::info(text),
                };
                window.push_notification(notification, cx);
            }
        }

        // 全局热键（Ctrl+Shift+D）→ 快速添加：在 render 里打开弹层（此时有 Window）。
        // 模态层（设置/命令面板）打开时不再叠加添加弹层（16 §6）。
        if self.pending_quick_add {
            self.pending_quick_add = false;
            if !self.show_settings && !self.palette_open {
                self.open_add_dialog(window, cx);
            }
        }

        // 模态层级隔离（16 §6）：设置从关闭→打开的这一帧统一清理下层弹层。
        // 放在 render 里做而不是事件处理器：避免窗口重入更新，且覆盖所有入口
        // （标题栏/侧栏/命令面板/监控页/Agent 卡片直接置位 show_settings 的情况）。
        if self.show_settings && !self.was_settings_open {
            self.palette_open = false;
            window.close_all_dialogs(cx);
        }
        self.was_settings_open = self.show_settings;

        // 命令面板「打开资源解析」→ 下一帧聚焦会话输入框（U-12：与侧栏入口一致）。
        if self.pending_resolve_focus {
            self.pending_resolve_focus = false;
            if self.page == AppPage::Resolve {
                self.resolve_input
                    .update(cx, |input, cx| input.focus(window, cx));
            }
        }

        // 截图/验收钩子：启动后直接打开命令面板/添加弹层（与 RDM_TEST_OPEN_SETTINGS 同一约定）。
        // 设置已打开时不再叠加浮层（模态层级隔离）。
        if !self.startup_hooks_done {
            self.startup_hooks_done = true;
            if env_flag("RDM_TEST_OPEN_PALETTE") && !self.show_settings {
                self.palette_open = true;
                self.palette_query
                    .update(cx, |input, cx| input.set_value("", window, cx));
                let handle = self.palette_query.read(cx).focus_handle(cx);
                window.focus(&handle);
            }
            if env_flag("RDM_TEST_OPEN_ADD") && !self.show_settings {
                self.open_add_dialog(window, cx);
            }
            if env_flag("RDM_TEST_OPEN_ASSISTANT") || env_flag("RDM_TEST_OPEN_RESOLVE") {
                self.page = AppPage::Resolve;
                self.resolve_input
                    .update(cx, |input, cx| input.focus(window, cx));
            }
            // Agent 工作流验收钩子：启动后直接以给定文本发送一条助手消息。
            if let Some(prompt) = env_text("RDM_TEST_AGENT_PROMPT") {
                self.page = AppPage::Resolve;
                self.resolve_input.update(cx, |input, cx| {
                    input.set_value(prompt.clone(), window, cx);
                });
                self.send_resolve_message(window, cx);
            }
            // 资源监控验收/截图钩子：启动后直接切到监控页并触发扫描。
            if env_flag("RDM_TEST_OPEN_MONITOR") {
                self.page = AppPage::Monitor;
                self.rescan_monitor(cx);
            }
            // 解析经验库验收/截图钩子（design 17）：启动后切到经验页、读盘并选中第一条，
            // 让「列表 + 详情 + 流程时间线」在同一张截图里可见。
            if env_flag("RDM_TEST_OPEN_EXPERIENCE") {
                self.page = AppPage::Experience;
                self.reload_experiences();
                self.experience_select_first();
            }
            // 经验页「编辑 / 删除」模态层截图钩子（只用于真实运行证据，不写用户文档）。
            if env_flag("RDM_TEST_EXPERIENCE_EDIT") || env_flag("RDM_TEST_EXPERIENCE_DELETE") {
                self.page = AppPage::Experience;
                self.reload_experiences();
                self.experience_select_first();
                if let Some(id) = self.experience.selected_id().map(str::to_owned) {
                    if env_flag("RDM_TEST_EXPERIENCE_EDIT") {
                        self.experience.begin_edit(&id, window, cx);
                    } else {
                        self.experience.begin_delete(&id, window, cx);
                    }
                }
            }
            // design 18 验收钩子：直接对一个新 URL 应用经验（不经过鼠标）。
            if let Some(spec) = env_text("RDM_TEST_APPLY_EXPERIENCE") {
                if let Some((id, url)) = spec.split_once('|') {
                    self.page = AppPage::Experience;
                    self.reload_experiences();
                    self.experience.select(id);
                    self.apply_experience_to_url(id, url, cx);
                }
            }
            // design 18 验收钩子：合并同类经验（target|source1,source2）。
            if let Some(spec) = env_text("RDM_TEST_MERGE_EXPERIENCE") {
                if let Some((target, sources)) = spec.split_once('|') {
                    let sources: Vec<String> = sources
                        .split(',')
                        .map(str::trim)
                        .filter(|source| !source.is_empty())
                        .map(str::to_owned)
                        .collect();
                    self.merge_experiences(target, &sources, cx);
                    self.page = AppPage::Experience;
                }
            }
            // design 18 验收钩子：打开经验页并直接给经验助手发一条消息。
            if let Some(prompt) = env_text("RDM_TEST_EXPERIENCE_ASSISTANT_PROMPT") {
                self.page = AppPage::Experience;
                self.reload_experiences();
                self.experience_select_first();
                self.send_experience_assistant(prompt, window, cx);
            }
            // design 18 验收钩子：打开经验页并展开「使用此经验解析」URL 行（截图）。
            if env_flag("RDM_TEST_USE_EXPERIENCE") {
                self.page = AppPage::Experience;
                self.reload_experiences();
                self.experience_select_first();
                if let Some(id) = self.experience.selected_id().map(str::to_owned) {
                    self.experience.begin_use_experience(&id, window, cx);
                }
            }
            // design 18 验收钩子：用本地文件模拟一条「经验助手」回复（含 recipe JSON），
            // 可选直接应用为下一个版本——不依赖真实 LLM，专门验证预览卡/应用/版本递增链路。
            if let Some(path) = env_text("RDM_TEST_EXPERIENCE_ASSISTANT_REPLY_FILE") {
                self.page = AppPage::Experience;
                self.reload_experiences();
                self.experience_select_first();
                if let Ok(reply) = std::fs::read_to_string(&path) {
                    self.experience.assistant_begin("（测试）整理为可复用经验");
                    self.experience.assistant_finished(&reply);
                    if env_flag("RDM_TEST_EXPERIENCE_APPLY_PATCH") {
                        self.apply_experience_patch(cx);
                    }
                } else {
                    tracing::warn!(path = %path, "RDM_TEST_EXPERIENCE_ASSISTANT_REPLY_FILE 读取失败");
                }
            }
            // 常驻详情面板验收/截图钩子：选中第一条任务，让右侧详情有真实内容。
            if env_flag("RDM_TEST_SELECT_FIRST") {
                if let Some(item) = self.items.first() {
                    let id = item.id.clone();
                    self.focused = Some(id.clone());
                    self.selection = [id].into_iter().collect();
                }
            }
        }

        // 设置页按需创建（Entity 持有输入框状态；避免每次打开重建）。
        if self.show_settings && self.settings_page.is_none() {
            let weak = cx.entity().downgrade();
            let page = cx.new(|cx| SettingsPage::new(window, cx, weak));
            if let Some(section) = self.pending_settings_section.take() {
                page.update(cx, |page, cx| page.select_section(section, cx));
            }
            self.settings_page = Some(page);
        }

        let sidebar = self.render_sidebar(cx).into_any_element();
        let body: AnyElement = match self.page {
            AppPage::Downloads => {
                let list = self.render_list(cx).into_any_element();
                // 详情面板常驻右侧：不再有开关/快捷键，也没有「收起」分支。
                let inspector = self.render_inspector(cx);
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .min_w(px(0.0))
                    .child(list)
                    .child(inspector)
                    .into_any_element()
            }
            AppPage::Resolve => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .min_w(px(0.0))
                .child(self.render_resolve_page(cx))
                .into_any_element(),
            AppPage::Experience => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .min_w(px(0.0))
                .child(self.render_experience(viewport_width, cx))
                .into_any_element(),
            AppPage::Monitor => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .min_w(px(0.0))
                .child(self.render_monitor(cx))
                .into_any_element(),
        };
        let toasts = self.render_toasts(cx);
        let settings = if self.show_settings {
            self.settings_page.clone().map(|page| {
                div()
                    .id("settings-overlay")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    // 16 §6：模态层必须阻断下层鼠标命中——下层任务行的右键菜单、行点击、
                    // 悬停按钮全部失效；设置页自身子元素仍是最上层 hitbox，保持可交互。
                    .occlude()
                    .child(page)
                    .into_any_element()
            })
        } else {
            None
        };
        // 设置打开时不再渲染命令面板（两者不应叠加；打开设置入口已主动关闭面板）。
        let palette = if self.show_settings {
            None
        } else {
            self.render_palette(cx)
        };

        let root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.bg_app)
            .text_color(t.text_primary)
            .font(theme::ui_font())
            // 关闭层：Esc 的层级处理（设置 > 命令面板 > 页面 > 选择；16 §6）。
            .on_action(cx.listener(|this, _: &CloseLayer, window, cx| {
                if this.show_settings {
                    // 全屏设置是最高模态层；其内部搜索框的 Esc 由 SettingsPage 先消费。
                    this.show_settings = false;
                } else if this.palette_open {
                    this.palette_open = false;
                } else if this.page == AppPage::Experience && this.experience.modal_open() {
                    // 解析经验页的编辑/删除是页内模态层：Esc 先关闭它，不离开页面。
                    this.experience.close_modal();
                } else if this.page != AppPage::Downloads {
                    // 资源解析 / 资源监控 → 返回下载库（浮层优先，见 06 §1.5 层级）。
                    this.set_page(AppPage::Downloads, cx);
                } else if this.multi_select && this.selection.is_empty() {
                    // 多选模式且没有选中项：Esc 退出多选（行左侧候选框消失）。
                    this.toggle_multi_select(cx);
                } else if !this.selection.is_empty() {
                    this.selection.clear();
                } else {
                    this.omnibox.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                    });
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                // 入口本身永远可用；打开时关闭命令面板/弹层（见 open_settings）。
                this.open_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseSettings, _, cx| {
                if this.show_settings {
                    this.show_settings = false;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                if this.show_settings {
                    return;
                }
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusOmnibox, window, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.omnibox.update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenAddDownload, window, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.open_add_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.sidebar_collapsed = !this.sidebar_collapsed;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleMonitor, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                let page = if this.page == AppPage::Monitor {
                    AppPage::Downloads
                } else {
                    AppPage::Monitor
                };
                this.set_page(page, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleDensity, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.density = match this.density {
                    Density::Comfortable => Density::Compact,
                    Density::Compact => Density::Comfortable,
                };
                this.settings.density = this.density;
                this.persist_settings();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CycleThemeMode, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.run_command(Command::CycleThemeMode, cx);
            }))
            .on_action(cx.listener(|this, _: &PauseAll, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.pause_all(cx);
            }))
            .on_action(cx.listener(|this, _: &ResumeAll, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.resume_all(cx);
            }))
            .on_action(cx.listener(|this, _: &ClearCompleted, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.clear_completed(cx);
            }))
            .on_action(cx.listener(|this, _: &ClearSearch, window, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.omnibox
                    .update(cx, |input, cx| input.set_value("", window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectView, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                // 命令面板选择视图（不常用；按 index 映射）。
                let _ = this;
                cx.notify();
            }))
            // 列表键盘（模态层打开时全部短路：任务/选择类动作不得越过设置层）。
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.move_focus(1, false, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.move_focus(-1, false, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.move_focus(1, true, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ExtendPrev, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.move_focus(-1, true, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSelect, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.toggle_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllTasks, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.select_all(cx);
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.clear_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateFocused, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.activate_focused(cx);
            }))
            .on_action(cx.listener(|this, _: &PauseFocused, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.pause_by_id(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ResumeFocused, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.resume_by_id(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RetryFocused, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.retry_by_id(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RemoveFocused, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.remove_by_id(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DeleteFileFocused, window, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.open_delete_confirm(&id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenFocusedFile, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.open_file_by_id(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenFocusedFolder, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.open_folder_by_id(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CopyFocusedLink, _, cx| {
                if this.modal_layer_blocks_actions() {
                    return;
                }
                if let Some(id) = this.focused_id() {
                    this.copy_link_by_id(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, command: &TaskCommand, window, cx| {
                // 右键菜单动作也必须短路：菜单可能在下层已打开，设置打开后不得生效。
                if this.modal_layer_blocks_actions() {
                    return;
                }
                this.run_task_command(command, window, cx);
            }))
            // 命令面板键盘
            .on_action(cx.listener(|this, _: &PaletteUp, _, cx| {
                if this.show_settings {
                    return;
                }
                this.move_palette(-1, cx)
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _, cx| {
                if this.show_settings {
                    return;
                }
                this.move_palette(1, cx)
            }))
            .on_action(cx.listener(|this, _: &PaletteConfirm, window, cx| {
                if this.show_settings {
                    return;
                }
                this.confirm_palette(window, cx)
            }))
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(sidebar)
                    .child(body),
            )
            .child(self.render_status_strip(cx))
            .children(toasts)
            .children(settings)
            .children(palette)
            // gpui-component 0.5.1 的 Root 不会自动渲染浮层层：必须由内容视图显式挂载
            // （官方 example/story 同款写法；缺了会导致 open_dialog/push_notification 静默失效）。
            .children(gpui_component::Root::render_dialog_layer(window, cx))
            .children(gpui_component::Root::render_sheet_layer(window, cx))
            .children(gpui_component::Root::render_notification_layer(window, cx));
        root
    }
}

impl RdmApp {
    /// 显示/激活主窗口（托盘/IPC 用）。
    pub fn show_from_tray(&mut self, cx: &mut Context<Self>) {
        self.restore_window(cx);
    }
}

impl Drop for RdmApp {
    fn drop(&mut self) {
        // 解析历史是用户资产：窗口关闭/进程退出时强制落盘（流式输出未完成的会话也保存）。
        if self.resolve.is_dirty() {
            if let Err(err) = self.resolve.save_if_dirty(&self.state_dir) {
                tracing::warn!(error = %err, "退出时保存 resolve_sessions.json 失败");
            }
        }
    }
}

/// 旧版 UI 的窗口钩子需要 `RdmApp::skip_taskbar` 之类的调用点很少，这里不额外暴露。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemState;

    fn item(name: &str, state: ItemState, source: &str, size: u64, speed: f64) -> DownloadItem {
        let mut request = rdm_core::DownloadRequest::new(format!("https://example.com/{name}"));
        request.file_name = Some(name.to_string());
        let mut item = DownloadItem::new(name, request, source);
        item.file_name = name.to_string();
        item.state = state;
        item.total = Some(size);
        item.speed_bps = speed;
        item
    }

    #[test]
    fn filters_match_states_and_sources() {
        let items = vec![
            item("a.iso", ItemState::Downloading, "browser", 10, 1.0),
            item("b.iso", ItemState::Paused, "cli", 20, 0.0),
            item("c.iso", ItemState::Completed, "ipc", 30, 0.0),
            item("d.iso", ItemState::Failed, "agent", 40, 0.0),
            item("e.iso", ItemState::Cancelled, "gui", 50, 0.0),
        ];
        assert_eq!(ViewFilter::All.count(&items), 5);
        assert_eq!(ViewFilter::Downloading.count(&items), 1);
        assert_eq!(ViewFilter::Paused.count(&items), 1);
        assert_eq!(ViewFilter::Completed.count(&items), 1);
        // 审计 F-07：新 UI 的「失败」只含 Failed，取消只在「全部」可见。
        assert_eq!(ViewFilter::Failed.count(&items), 1);
        assert_eq!(ViewFilter::FromBrowser.count(&items), 2);
        assert_eq!(ViewFilter::FromAgent.count(&items), 2);
        assert_eq!(ViewFilter::from_index(5), Some(ViewFilter::FromBrowser));
        assert_eq!(ViewFilter::from_index(99), None);
    }

    #[test]
    fn filter_and_sort_honours_query_and_direction() {
        let items = vec![
            item("alpha.iso", ItemState::Completed, "browser", 10, 0.0),
            item("beta.zip", ItemState::Downloading, "cli", 30, 2.0),
            item("gamma.mp4", ItemState::Downloading, "browser", 20, 1.0),
        ];
        let sorted = filter_and_sort(&items, ViewFilter::Downloading, "", SortKey::Size, true);
        assert_eq!(sorted, vec![2, 1]);
        let sorted_desc = filter_and_sort(&items, ViewFilter::All, "", SortKey::Size, false);
        assert_eq!(sorted_desc, vec![1, 2, 0]);
        let query = filter_and_sort(&items, ViewFilter::All, "BETA", SortKey::Time, false);
        assert_eq!(query, vec![1]);
        assert!(
            filter_and_sort(&items, ViewFilter::Completed, "beta", SortKey::Time, false).is_empty()
        );
        // 审计 F-21：搜索匹配来源的**展示文案**（中文），而不只是原始 token。
        let chinese_source =
            filter_and_sort(&items, ViewFilter::All, "浏览器", SortKey::Time, false);
        assert_eq!(chinese_source.len(), 2, "搜「浏览器」应命中 browser 来源");
    }

    #[test]
    fn state_sort_uses_lifecycle_rank_not_label_codepoints() {
        let items = vec![
            item("cancelled.iso", ItemState::Cancelled, "gui", 10, 0.0),
            item("failed.iso", ItemState::Failed, "gui", 10, 0.0),
            item("completed.iso", ItemState::Completed, "gui", 10, 0.0),
            item("paused.iso", ItemState::Paused, "gui", 10, 0.0),
            item("downloading.iso", ItemState::Downloading, "gui", 10, 0.0),
        ];
        let sorted = filter_and_sort(&items, ViewFilter::All, "", SortKey::State, true);
        let order: Vec<&str> = sorted
            .iter()
            .map(|&i| items[i].file_name.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "downloading.iso",
                "paused.iso",
                "completed.iso",
                "failed.iso",
                "cancelled.iso"
            ]
        );
    }

    #[test]
    fn sort_keys_have_expected_default_directions() {
        assert!(!SortKey::Time.default_ascending());
        assert!(SortKey::Name.default_ascending());
        assert_eq!(SortKey::Time.next(), SortKey::Name);
        assert_eq!(SortKey::ALL.len(), 6);
    }

    #[test]
    fn delete_targets_include_part_files() {
        let mut item = item("x.iso", ItemState::Completed, "browser", 100, 0.0);
        item.path = Some(PathBuf::from(r"D:\Downloads\x.iso"));
        let targets = delete_targets(&item, std::path::Path::new(r"C:\state"));
        assert!(targets.iter().any(|path| path.ends_with("x.iso")));
        assert!(targets.iter().any(|path| path.ends_with("x.iso.part")));
        assert!(targets.iter().any(|path| path.ends_with("x.iso.part.json")));
        assert_eq!(
            targets
                .iter()
                .filter(|path| path.ends_with("x.iso.part"))
                .count(),
            1
        );
    }

    #[test]
    fn file_name_ellipsis_keeps_extension() {
        let long = "这是一个非常非常非常长的文件名用于测试截断逻辑-2026-10-21.zip";
        let short = ellipsize_file_name(long, 24);
        assert!(short.ends_with(".zip"));
        assert!(short.contains('…'));
        assert!(short.chars().count() <= 24);
    }
}
