//! 新版 UI 的 action 与全局快捷键（契约 §10）。
//!
//! 命令面板、右键菜单与键盘共用同一批 Action：所有入口最终都走
//! `window.dispatch_action` → 根视图的 `on_action` 处理器，避免出现「第二套可用性判断」。
//!
//! 单字母/裸键只在 `TaskList` 上下文生效（输入框内不会误触发），带修饰键的才是全局。

use gpui::{actions, App, KeyBinding, SharedString};

actions!(
    rdm_ui,
    [
        // 全局
        OpenSettings,
        CloseSettings,
        OpenCommandPalette,
        CloseCommandPalette,
        FocusOmnibox,
        OpenAddDownload,
        ToggleSidebar,
        ToggleMonitor,
        ToggleDensity,
        CycleThemeMode,
        PauseAll,
        ResumeAll,
        ClearCompleted,
        ClearSearch,
        CloseLayer,
        // 列表（TaskList 上下文）
        SelectNext,
        SelectPrev,
        ExtendNext,
        ExtendPrev,
        ToggleSelect,
        SelectAllTasks,
        ClearSelection,
        ActivateFocused,
        PauseFocused,
        ResumeFocused,
        CancelFocused,
        RetryFocused,
        RemoveFocused,
        DeleteFileFocused,
        OpenFocusedFile,
        OpenFocusedFolder,
        CopyFocusedLink,
        // 命令面板
        PaletteUp,
        PaletteDown,
        PaletteConfirm,
    ]
);

/// 行级命令的种类（右键菜单 / 行内按钮 / 命令面板共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskCommandKind {
    Pause,
    Resume,
    Cancel,
    Retry,
    Remove,
    DeleteFile,
    OpenFile,
    OpenFolder,
    CopyLink,
    CopyPath,
    Details,
}

/// 带任务 id 的 action（用于右键菜单等无法直接捕获闭包的场景）。
///
/// 不参与 keymap JSON（`no_json`），只通过 `dispatch_action` 触发。
#[derive(Debug, Clone, PartialEq, gpui::Action)]
#[action(namespace = rdm_ui, no_json)]
pub struct TaskCommand {
    pub id: SharedString,
    pub kind: TaskCommandKind,
}

/// Toggle 某个智能视图/来源筛选（命令面板用）。
#[derive(Debug, Clone, PartialEq, gpui::Action)]
#[action(namespace = rdm_ui, no_json)]
pub struct SelectView {
    pub view: u8,
}

/// 安装全局快捷键。必须在 `Application::run` 内调用（`gpui_component::init` 之后）。
pub fn install(cx: &mut App) {
    cx.bind_keys([
        // ---- 全局（带修饰键；不在输入框裸键冲突） -----------------------
        KeyBinding::new("secondary-k", OpenCommandPalette, None),
        KeyBinding::new("secondary-shift-p", OpenCommandPalette, None),
        KeyBinding::new("secondary-n", OpenAddDownload, None),
        KeyBinding::new("secondary-l", FocusOmnibox, None),
        KeyBinding::new("secondary-f", FocusOmnibox, None),
        KeyBinding::new("secondary-comma", OpenSettings, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-shift-m", ToggleMonitor, None),
        KeyBinding::new("secondary-shift-d", ToggleDensity, None),
        KeyBinding::new("secondary-shift-v", OpenAddDownload, None),
        KeyBinding::new("secondary-shift-o", OpenFocusedFolder, None),
        KeyBinding::new("escape", CloseLayer, None),
        // ---- 命令面板 ---------------------------------------------------
        KeyBinding::new("up", PaletteUp, Some("CommandPalette")),
        KeyBinding::new("down", PaletteDown, Some("CommandPalette")),
        KeyBinding::new("enter", PaletteConfirm, Some("CommandPalette")),
        // ---- 任务列表（单键只在列表上下文生效） -------------------------
        KeyBinding::new("down", SelectNext, Some("TaskList")),
        KeyBinding::new("up", SelectPrev, Some("TaskList")),
        KeyBinding::new("shift-down", ExtendNext, Some("TaskList")),
        KeyBinding::new("shift-up", ExtendPrev, Some("TaskList")),
        KeyBinding::new("pageup", SelectPrev, Some("TaskList")),
        KeyBinding::new("pagedown", SelectNext, Some("TaskList")),
        KeyBinding::new("home", SelectPrev, Some("TaskList")),
        KeyBinding::new("end", SelectNext, Some("TaskList")),
        KeyBinding::new("space", ToggleSelect, Some("TaskList")),
        KeyBinding::new("enter", ActivateFocused, Some("TaskList")),
        KeyBinding::new("secondary-enter", OpenFocusedFolder, Some("TaskList")),
        KeyBinding::new("secondary-a", SelectAllTasks, Some("TaskList")),
        KeyBinding::new("secondary-c", CopyFocusedLink, Some("TaskList")),
        KeyBinding::new("delete", RemoveFocused, Some("TaskList")),
        KeyBinding::new("shift-delete", DeleteFileFocused, Some("TaskList")),
        KeyBinding::new("secondary-p", PauseFocused, Some("TaskList")),
        KeyBinding::new("secondary-r", ResumeFocused, Some("TaskList")),
        KeyBinding::new("secondary-shift-c", CancelFocused, Some("TaskList")),
    ]);
}

/// 新版 UI 的全局命令（命令面板展示顺序即此顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    AddDownload,
    FocusOmnibox,
    OpenSettings,
    OpenResolve,
    /// 解析经验库（design 17）。
    OpenExperience,
    ToggleSidebar,
    ToggleMonitor,
    ToggleDensity,
    CycleThemeMode,
    PauseAll,
    ResumeAll,
    ClearCompleted,
    OpenDownloadFolder,
}

impl Command {
    pub const ALL: [Command; 13] = [
        Command::AddDownload,
        Command::FocusOmnibox,
        Command::OpenSettings,
        Command::OpenResolve,
        Command::OpenExperience,
        Command::ToggleSidebar,
        Command::ToggleMonitor,
        Command::ToggleDensity,
        Command::CycleThemeMode,
        Command::PauseAll,
        Command::ResumeAll,
        Command::ClearCompleted,
        Command::OpenDownloadFolder,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::AddDownload => "添加下载…",
            Self::FocusOmnibox => "聚焦万能输入栏",
            Self::OpenSettings => "打开设置",
            Self::OpenResolve => "打开资源解析",
            Self::OpenExperience => "打开解析经验",
            Self::ToggleSidebar => "折叠/展开侧栏",
            Self::ToggleMonitor => "打开/关闭资源监控",
            Self::ToggleDensity => "切换列表密度",
            Self::CycleThemeMode => "切换明暗主题",
            Self::PauseAll => "全部暂停",
            Self::ResumeAll => "全部继续",
            Self::ClearCompleted => "清空已完成",
            Self::OpenDownloadFolder => "打开默认保存目录",
        }
    }

    /// 命令面板行首图标。
    pub fn icon(self) -> crate::ui::icons::RdmIcon {
        use crate::ui::icons::RdmIcon;
        match self {
            Self::AddDownload => RdmIcon::Plus,
            Self::FocusOmnibox => RdmIcon::Search,
            Self::OpenSettings => RdmIcon::Settings,
            Self::OpenResolve => RdmIcon::Sparkle,
            Self::OpenExperience => RdmIcon::BookOpen,
            Self::ToggleSidebar => RdmIcon::PanelLeftClose,
            Self::ToggleMonitor => RdmIcon::Gauge,
            Self::ToggleDensity => RdmIcon::Density,
            Self::CycleThemeMode => RdmIcon::Sun,
            Self::PauseAll => RdmIcon::Pause,
            Self::ResumeAll => RdmIcon::Play,
            Self::ClearCompleted => RdmIcon::Trash,
            Self::OpenDownloadFolder => RdmIcon::Folder,
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            Self::AddDownload
            | Self::FocusOmnibox
            | Self::OpenSettings
            | Self::OpenResolve
            | Self::OpenExperience
            | Self::ToggleMonitor => "导航",
            Self::ToggleSidebar | Self::ToggleDensity | Self::CycleThemeMode => "外观",
            Self::PauseAll | Self::ResumeAll | Self::ClearCompleted => "任务",
            Self::OpenDownloadFolder => "文件",
        }
    }
}
