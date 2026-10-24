//! 图标体系（04 §6）：统一入口，禁止在视图里手写 SVG 路径。
//!
//! - [`RdmIcon`]：打包在 `assets/icons/` 的 Lucide 线性图标（路径 `rdm-icons/<name>.svg`）；
//! - [`IconName`]：gpui-component 内建图标（路径 `icons/<name>.svg`），少量复用。

use gpui::prelude::*;
use gpui::{px, Hsla, Pixels};
use gpui_component::{Icon, IconName, IconNamed};

use crate::model::ItemState;

/// RDM 首批图标（04 §6.2 + 派生补充）。值即 `assets/icons/<file>.svg` 文件名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdmIcon {
    Download,
    Pause,
    Play,
    Close,
    Retry,
    File,
    Folder,
    FolderOpen,
    Copy,
    Trash,
    Search,
    Settings,
    Plus,
    Sparkle,
    Filter,
    Sort,
    Density,
    ChevronDown,
    ChevronRight,
    Check,
    CheckCircle,
    Alert,
    ErrorCircle,
    Clock,
    Disk,
    Link,
    Globe,
    Command,
    External,
    More,
    PanelLeftClose,
    PanelLeftOpen,
    PanelRightClose,
    PanelRightOpen,
    Clipboard,
    Info,
    VideoFile,
    TextFile,
    ArchiveFile,
    ImageFile,
    Monitor,
    ArrowRight,
    Sliders,
    Gauge,
    Plug,
    Sun,
    Moon,
    RotateCcw,
    CirclePause,
    CirclePlay,
    ListFilter,
    Languages,
    Keyboard,
    Palette,
    Archive,
    FileDown,
    DownloadCloud,
    Shield,
    Lock,
    Wifi,
    WifiOff,
    PlugZap,
    Refresh,
    Send,
    Bot,
    Message,
    CornerDownLeft,
    ChevronsUpDown,
    ArrowUp,
    ArrowDown,
    Minus,
    Square,
    Circle,
    Loader,
    Inbox,
    /// 解析经验库（design 17）。
    BookOpen,
}

impl RdmIcon {
    /// SVG 文件名（不含扩展名）。
    pub fn name(self) -> &'static str {
        use RdmIcon::*;
        match self {
            Download => "download",
            Pause => "pause",
            Play => "play",
            Close => "x",
            Retry => "rotate-cw",
            File => "file",
            Folder => "folder",
            FolderOpen => "folder-open",
            Copy => "copy",
            Trash => "trash-2",
            Search => "search",
            Settings => "settings",
            Plus => "plus",
            Sparkle => "sparkles",
            Filter => "filter",
            Sort => "arrow-up-down",
            Density => "rows-3",
            ChevronDown => "chevron-down",
            ChevronRight => "chevron-right",
            Check => "check",
            CheckCircle => "circle-check",
            Alert => "triangle-alert",
            ErrorCircle => "circle-x",
            Clock => "clock",
            Disk => "hard-drive",
            Link => "link",
            Globe => "globe",
            Command => "command",
            External => "external-link",
            More => "ellipsis",
            PanelLeftClose => "panel-left-close",
            PanelLeftOpen => "panel-left-open",
            PanelRightClose => "panel-right-close",
            PanelRightOpen => "panel-right-open",
            Clipboard => "clipboard",
            Info => "info",
            VideoFile => "file-video",
            TextFile => "file-text",
            ArchiveFile => "file-archive",
            ImageFile => "file-image",
            Monitor => "monitor",
            ArrowRight => "arrow-right",
            Sliders => "sliders-horizontal",
            Gauge => "gauge",
            Plug => "plug",
            Sun => "sun",
            Moon => "moon",
            RotateCcw => "rotate-ccw",
            CirclePause => "circle-pause",
            CirclePlay => "circle-play",
            ListFilter => "list-filter",
            Languages => "languages",
            Keyboard => "keyboard",
            Palette => "palette",
            Archive => "archive",
            FileDown => "file-down",
            DownloadCloud => "download-cloud",
            Shield => "shield",
            Lock => "lock",
            Wifi => "wifi",
            WifiOff => "wifi-off",
            PlugZap => "plug-zap",
            Refresh => "refresh-cw",
            Send => "send",
            Bot => "bot",
            Message => "message-square",
            CornerDownLeft => "corner-down-left",
            ChevronsUpDown => "chevrons-up-down",
            ArrowUp => "arrow-up",
            ArrowDown => "arrow-down",
            Minus => "minus",
            Square => "square",
            Circle => "circle",
            Loader => "loader-circle",
            Inbox => "inbox",
            BookOpen => "book-open",
        }
    }

    pub fn path(self) -> gpui::SharedString {
        format!("rdm-icons/{}.svg", self.name()).into()
    }

    /// 默认尺寸（16px）图标元素。
    pub fn icon(self) -> Icon {
        Icon::default().path(self.path())
    }

    /// 指定尺寸与颜色的图标元素。
    pub fn sized(self, size: f32, color: Hsla) -> Icon {
        Icon::default()
            .path(self.path())
            .text_color(color)
            .w(px(size))
            .h(px(size))
    }

    /// 文件类型图标（按扩展名，04 §6.2 派生）。
    pub fn for_file(name: &str) -> RdmIcon {
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "m4v" | "ts" => RdmIcon::VideoFile,
            "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "bz2" | "zst" => RdmIcon::ArchiveFile,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "avif" => {
                RdmIcon::ImageFile
            }
            "txt" | "md" | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "json"
            | "toml" | "yaml" | "yml" | "log" | "csv" | "xml" | "html" => RdmIcon::TextFile,
            _ => RdmIcon::File,
        }
    }

    /// 状态图标（契约 §7.1：状态 = 图标 + 文案 + 颜色三通道）。
    pub fn for_state(state: ItemState) -> RdmIcon {
        match state {
            ItemState::Downloading => RdmIcon::Download,
            ItemState::Paused => RdmIcon::CirclePause,
            ItemState::Completed => RdmIcon::CheckCircle,
            ItemState::Failed => RdmIcon::ErrorCircle,
            ItemState::Cancelled => RdmIcon::Close,
        }
    }
}

/// gpui-component 内建图标元素（16px）。
#[allow(dead_code)] // Kit API：视图可按需使用
pub fn builtin(name: IconName) -> Icon {
    Icon::default().path(name.path())
}

/// gpui-component 内建图标元素 + 尺寸。
#[allow(dead_code)] // Kit API：视图可按需使用
pub fn builtin_sized(name: IconName, size: f32) -> Icon {
    Icon::default().path(name.path()).w(px(size)).h(px(size))
}

/// 状态色（用于图标着色；与 `ThemeTokens` 的语义色对应）。
#[allow(dead_code)] // Kit API：视图按需着色
pub fn state_color(state: ItemState, dark: bool) -> Hsla {
    let t = if dark {
        crate::ui::theme::dark(crate::model::AccentPreset::Blue)
    } else {
        crate::ui::theme::light(crate::model::AccentPreset::Blue)
    };
    match state {
        ItemState::Downloading => t.accent,
        ItemState::Paused => t.warning,
        ItemState::Completed => t.success,
        ItemState::Failed => t.danger,
        ItemState::Cancelled => t.text_tertiary,
    }
}

/// 图标像素尺寸（布局用，非渲染尺寸）。
#[allow(dead_code)] // Kit API
pub fn icon_px(size: f32) -> Pixels {
    px(size)
}
