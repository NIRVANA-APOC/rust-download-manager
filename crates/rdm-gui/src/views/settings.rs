//! 设置页（M3，`docs/design/03-information-architecture.md` §7 / `06` §3.5）。
//!
//! 形态：主窗口内**全屏路由**（设计 Q2 允许的兜底方案，见 `08` D5）：不阻塞下载、
//! 无「保存/取消」仪式、改动立即生效并落盘；顶部搜索定位设置项。
//!
//! 本文件是设置页的唯一实现（9 页：通用 / 下载引擎 / 存储与分类 / 浏览器集成 /
//! AI 助手 / 外观 / 快捷键 / 高级 / 关于）。所有颜色只从 [`crate::ui::theme`] 的
//! 语义 token 读取；所有写操作都通过 `weak_app` 回调 [`crate::views::app::RdmApp`]。
//!
//! 关键实现约束（踩坑记录）：
//! - `SettingsPage` 是在 `RdmApp::render` 里创建的，**创建/渲染期间不能读 `RdmApp`**
//!   （GPUI 的 `Entity::read` 在同 entity 更新中会 panic）。因此首帧先用默认值占位，
//!   由 `cx.spawn_in` 在下一帧 `initialize()` 里读取快照并回填输入框。
//! - 文本/数字输入使用 500ms 防抖（`Timer` + generation 校验），开关/单选/强调色
//!   立即应用；行内显示「已保存」1.2s，不弹全局 toast。
//! - `Esc` 关闭整页由根视图（`RdmApp` 的 `CloseLayer`）处理，本页不重复拦截。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, point, px, AnyElement, ClipboardItem, Context, Entity, FocusHandle, FontWeight,
    IntoElement, KeyDownEvent, PathPromptOptions, Render, ScrollHandle, SharedString, Styled,
    Subscription, Timer, WeakEntity, Window,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::{h_flex, v_flex, ActiveTheme, Disableable, Sizable, WindowExt};
use rdm_ai::providers::{
    deepseek_is_off_peak, deepseek_peak_note_zh, find_model, price_label_zh, price_short_zh,
    ProviderKind, PRICE_CATALOG_DATE,
};
use rdm_ai::{
    default_category_rules, AiClient, AiConfig, CategoryRule, MAX_EXTENSIONS_PER_CATEGORY,
    MAX_EXTENSION_CHARS,
};

use crate::model::{
    now_ms, parse_connections, parse_speed_limit_mb, AccentPreset, Density, Settings, UiThemeMode,
};
use crate::ui::icons::RdmIcon;
use crate::ui::theme;
use crate::ui::theme::{
    tokens, ThemeTokens, FS_BODY, FS_CAPTION, FS_H1, FS_H2, FS_MICRO, FS_MONO, FS_MONO_MICRO,
    FS_MONO_PATH, RADIUS_MD, RADIUS_SM,
};
use crate::ui::widgets::{icon_button, info_banner, mono_text, status_pill, Severity};

/// 左侧设置导航宽度（03 §7.1：200px）。
const NAV_W: f32 = 200.0;
/// 内容区最大宽度（03 §7.1：约 720，左对齐）。
const CONTENT_MAX_W: f32 = 720.0;
/// 内容区内边距。
const CONTENT_PAD: f32 = 32.0;
/// 文本/数字输入保存防抖（03 §7.1：500ms）。
const SAVE_DEBOUNCE_MS: u64 = 500;
/// 行内「已保存」可见时长（1.2s）。
const SAVED_VISIBLE_MS: u64 = 1200;
/// 搜索跳转后的行高亮时长。
const HIGHLIGHT_MS: u64 = 1600;
/// 页脚提示使用同一套 saved 指示器时的哨兵行 id。
const FOOTER_ROW: &str = "settings-footer";
/// Provider 档案 chip 的键盘焦点句柄上限（3 个内置 + 可增删的自定义档案）。
const AI_PROFILE_FOCUS_MAX: usize = 12;
/// 模型列表最大显示高度（design 16 §5.3：约 240px，超出滚动）。
const AI_MODEL_LIST_MAX_H: f32 = 240.0;
/// 后台模型拉取结果的轮询间隔。
const AI_MODEL_FETCH_POLL_MS: u64 = 120;
/// 后台模型拉取的最长等待（请求本身另有 `timeout_secs` 总超时）。
const AI_MODEL_FETCH_TIMEOUT_MS: u64 = 45_000;

/// 自绘 Switch 的 element id 与焦点句柄（A8：36×20 胶囊 + 16 滑块，开 accent / 关 track）。
const TOGGLE_IDS: [&str; 9] = [
    "general-auto-start-switch",
    "general-silent-start-switch",
    "general-minimize-to-tray-switch",
    "general-launch-at-startup-switch",
    "engine-dynamic-switch",
    "storage-save-by-extension-switch",
    "storage-verify-sha256-switch",
    "appearance-reduce-motion-switch",
    "ai-enabled-switch",
];

// ---------------------------------------------------------------------------
// 分类
// ---------------------------------------------------------------------------

/// 设置页分类（顺序固定，对应 03 §7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSection {
    #[default]
    General,
    Engine,
    Storage,
    Browser,
    Ai,
    Appearance,
    Shortcuts,
    Advanced,
    About,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 9] = [
        SettingsSection::General,
        SettingsSection::Engine,
        SettingsSection::Storage,
        SettingsSection::Browser,
        SettingsSection::Ai,
        SettingsSection::Appearance,
        SettingsSection::Shortcuts,
        SettingsSection::Advanced,
        SettingsSection::About,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::General => "通用",
            Self::Engine => "下载引擎",
            Self::Storage => "存储与分类",
            Self::Browser => "浏览器集成",
            Self::Ai => "AI 助手",
            Self::Appearance => "外观",
            Self::Shortcuts => "快捷键",
            Self::Advanced => "高级",
            Self::About => "关于",
        }
    }

    pub fn description_zh(self) -> &'static str {
        match self {
            Self::General => "启动、静默与关闭行为；系统集成项保存后立即尝试生效。",
            Self::Engine => "新任务的连接数与带宽上限；修改对已开始的任务不生效。",
            Self::Storage => "默认保存目录、按后缀分类规则与校验展示。",
            Self::Browser => {
                "本机 IPC 通道与浏览器接管状态；原生宿主与扩展的安装状态需在浏览器侧确认。"
            }
            Self::Ai => "Provider 档案（Base URL / API Key / 模型独立保存）与实时拉取的模型列表；仅保存在本机，不写入日志。",
            Self::Appearance => "主题、强调色、密度与动效；改动立即预览并保存。",
            Self::Shortcuts => "当前版本的固定键位（只读）；自定义快捷键尚未实现。",
            Self::Advanced => "状态目录、诊断信息与危险操作。",
            Self::About => "版本、许可与文档位置；仅展示已验证的平台信息。",
        }
    }

    pub fn icon(self) -> RdmIcon {
        match self {
            Self::General => RdmIcon::Settings,
            Self::Engine => RdmIcon::Gauge,
            Self::Storage => RdmIcon::Disk,
            Self::Browser => RdmIcon::Globe,
            Self::Ai => RdmIcon::Sparkle,
            Self::Appearance => RdmIcon::Palette,
            Self::Shortcuts => RdmIcon::Keyboard,
            Self::Advanced => RdmIcon::Sliders,
            Self::About => RdmIcon::Info,
        }
    }

    /// 本页是否有可恢复的设置项（快捷键/关于等只读页没有）。
    pub fn has_settings(self) -> bool {
        matches!(
            self,
            Self::General | Self::Engine | Self::Storage | Self::Ai | Self::Appearance
        )
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// 搜索目录（设置项的静态索引；标题/说明与页面内行一致）
// ---------------------------------------------------------------------------

struct CatalogEntry {
    section: SettingsSection,
    /// 设置行 id（与渲染行的 id 一致，用于跳转后高亮）。
    row: &'static str,
    title: &'static str,
    description: &'static str,
}

const CATALOG: &[CatalogEntry] = &[
    // 通用
    CatalogEntry {
        section: SettingsSection::General,
        row: "general-auto-start",
        title: "自动开始",
        description: "添加下载后立即开始；关闭后新任务保持暂停。",
    },
    CatalogEntry {
        section: SettingsSection::General,
        row: "general-silent-start",
        title: "静默启动",
        description: "启动时不显示主窗口，仅驻留托盘；需托盘可用。",
    },
    CatalogEntry {
        section: SettingsSection::General,
        row: "general-minimize-to-tray",
        title: "关闭窗口时隐藏到托盘",
        description: "关闭窗口时隐藏到托盘；最小化仍到任务栏。",
    },
    CatalogEntry {
        section: SettingsSection::General,
        row: "general-launch-at-startup",
        title: "开机自启动",
        description: "登录 Windows 后自动启动；写入当前用户启动项。",
    },
    // 下载引擎
    CatalogEntry {
        section: SettingsSection::Engine,
        row: "engine-connections",
        title: "默认连接数",
        description: "每个任务的并行连接上限；动态规划开启时自动调节。",
    },
    CatalogEntry {
        section: SettingsSection::Engine,
        row: "engine-speed-limit",
        title: "全局限速",
        description: "全局下载速度上限，单位 MB/s；留空表示不限速。",
    },
    CatalogEntry {
        section: SettingsSection::Engine,
        row: "engine-dynamic",
        title: "动态线程规划",
        description: "按速度自动调整连接数；关闭时使用固定连接数。",
    },
    // 存储与分类
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-default-dir",
        title: "默认保存目录",
        description: "新任务的默认保存目录；下载中可单独修改。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-save-by-extension",
        title: "按后缀分类",
        description: "按文件后缀归入视频 / 音频 / 图片 / 文档等子目录。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-class-video",
        title: "视频后缀名",
        description: "逗号分隔的后缀名（如 mp4, mkv）；保存到「视频」目录。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-class-audio",
        title: "音频后缀名",
        description: "逗号分隔的后缀名（如 mp3, flac）；保存到「音频」目录。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-class-document",
        title: "文档后缀名",
        description: "逗号分隔的后缀名（如 pdf, docx）；保存到「文档」目录。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-class-image",
        title: "图片后缀名",
        description: "逗号分隔的后缀名（如 jpg, png）；保存到「图片」目录。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-class-archive",
        title: "压缩包后缀名",
        description: "逗号分隔的后缀名（如 zip, rar, 7z）；保存到「压缩包」目录。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-class-program",
        title: "程序安装包后缀名",
        description: "逗号分隔的后缀名（如 exe, msi）；保存到「程序安装包」目录。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-class-other",
        title: "其他（兜底）",
        description: "未识别类型保存到根目录；兜底分类不参与后缀匹配。",
    },
    CatalogEntry {
        section: SettingsSection::Storage,
        row: "storage-verify-sha256",
        title: "SHA-256 校验",
        description: "展示引擎返回的 SHA-256 校验值；不额外计算哈希。",
    },
    // 浏览器集成
    CatalogEntry {
        section: SettingsSection::Browser,
        row: "browser-status",
        title: "浏览器接管状态",
        description: "本机 IPC 通道是否就绪；原生宿主与扩展的安装状态需在浏览器侧确认。",
    },
    CatalogEntry {
        section: SettingsSection::Browser,
        row: "browser-state-dir",
        title: "状态目录",
        description: "ipc.json、日志与任务状态所在目录。",
    },
    CatalogEntry {
        section: SettingsSection::Browser,
        row: "browser-diagnostics",
        title: "诊断信息",
        description: "复制 IPC 端口、集成状态与设置摘要（已脱敏，不含 token / API Key）。",
    },
    // AI 助手
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-enabled",
        title: "启用 AI 助手",
        description: "启用后用于解析页面资源与 AI 助手对话。",
    },
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-provider",
        title: "Provider 档案",
        description: "内置 DeepSeek / OpenAI / SiliconFlow 与自定义 Provider；切换时 API 地址、API Key、模型一起切换。",
    },
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-profile-name",
        title: "Provider 名称",
        description: "自定义 Provider 可改名；内置档案名称固定。",
    },
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-base-url",
        title: "API 地址",
        description: "每个 Provider 独立保存；可指向任意 OpenAI 兼容网关。",
    },
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-api-key",
        title: "API Key",
        description: "每个 Provider 独立保存；仅存本机 ai.json，不会写入日志。",
    },
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-model",
        title: "模型（实时拉取）",
        description: "从 GET /models 拉取当前 Provider 的真实模型；可搜索选择，也可手填任意 model id。",
    },
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-pricing",
        title: "价格与估算",
        description: "价目快照日期、官方价格页、峰谷价与估算口径；无价目时明确显示「无价目估算」。",
    },
    CatalogEntry {
        section: SettingsSection::Ai,
        row: "ai-privacy",
        title: "隐私与日志",
        description: "请求只发送到你配置的服务；不内置代理、不收遥测。",
    },
    // 外观
    CatalogEntry {
        section: SettingsSection::Appearance,
        row: "appearance-theme",
        title: "主题模式",
        description: "跟随系统，或强制使用浅色 / 深色。",
    },
    CatalogEntry {
        section: SettingsSection::Appearance,
        row: "appearance-accent",
        title: "强调色",
        description: "只影响按钮、进度条、选中态等强调元素。",
    },
    CatalogEntry {
        section: SettingsSection::Appearance,
        row: "appearance-density",
        title: "密度",
        description: "列表行高：舒适 64px / 紧凑 40px。",
    },
    CatalogEntry {
        section: SettingsSection::Appearance,
        row: "appearance-reduce-motion",
        title: "减少动效",
        description: "关闭过渡动画；进度条更新保留。",
    },
    // 快捷键
    CatalogEntry {
        section: SettingsSection::Shortcuts,
        row: "shortcuts-global",
        title: "全局快捷键",
        description: "命令面板、添加下载、设置、侧栏、资源解析与常驻详情面板等。",
    },
    CatalogEntry {
        section: SettingsSection::Shortcuts,
        row: "shortcuts-list",
        title: "列表快捷键",
        description: "移动、暂停/继续、打开、删除与复制等。",
    },
    // 高级
    CatalogEntry {
        section: SettingsSection::Advanced,
        row: "advanced-state-dir",
        title: "打开状态目录",
        description: "打开包含 ipc.json、日志与任务状态的目录。",
    },
    CatalogEntry {
        section: SettingsSection::Advanced,
        row: "advanced-diagnostics",
        title: "复制诊断信息",
        description: "复制版本、状态目录、IPC 与设置摘要（已脱敏）。",
    },
    CatalogEntry {
        section: SettingsSection::Advanced,
        row: "advanced-reset-all",
        title: "恢复所有设置默认",
        description: "危险操作：恢复出厂设置（含 AI 配置），需要确认。",
    },
    // 关于
    CatalogEntry {
        section: SettingsSection::About,
        row: "about-version",
        title: "版本",
        description: "当前构建的版本号。",
    },
    CatalogEntry {
        section: SettingsSection::About,
        row: "about-product",
        title: "产品",
        description: "RDM（下载中枢）：本机下载任务、浏览器接管与页面资源解析。",
    },
    CatalogEntry {
        section: SettingsSection::About,
        row: "about-license",
        title: "开源许可",
        description: "RDM 以 MIT OR Apache-2.0 双许可发布。",
    },
    CatalogEntry {
        section: SettingsSection::About,
        row: "about-design",
        title: "设计文档",
        description: "设计包位于仓库 docs/design/。",
    },
    CatalogEntry {
        section: SettingsSection::About,
        row: "about-build",
        title: "构建",
        description: "本机构建配置；当前仅在 Windows 上验证。",
    },
];

/// 设置搜索：按页面名 / 标题 / 说明做大小写不敏感的字面匹配。
fn search_hits(query: &str) -> Vec<&'static CatalogEntry> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    CATALOG
        .iter()
        .filter(|entry| {
            entry.title.to_lowercase().contains(&query)
                || entry.description.to_lowercase().contains(&query)
                || entry.section.label_zh().contains(&query)
        })
        .take(40)
        .collect()
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct SavedNote {
    row: &'static str,
    text: &'static str,
}

/// 设置行内容区可用宽度（720 内容宽 - 左右 padding 32×2 = 656，减去行 padding 12×2 = 632）。
const ROW_INNER_W: f32 = 632.0;
/// 设置行右侧控件列固定宽度：保证说明文字永远不与其重叠（B8；320 可容纳输入框 + 已保存）。
const ROW_CONTROL_W: f32 = 320.0;
/// 行内左右两列间距。
const ROW_GAP: f32 = 16.0;
/// 内容区内部可用宽度（720 - 32×2）；非设置行的整宽元素（如浏览器状态卡）显式使用。
const CONTENT_INNER_W: f32 = CONTENT_MAX_W - CONTENT_PAD * 2.0;

/// 行内提示：左侧标题 + 一句说明（可带错误），右侧控件 +「已保存」。
///
/// 两列固定布局（左 `ROW_INNER_W - ROW_CONTROL_W - ROW_GAP`，右 `ROW_CONTROL_W`），
/// 说明文字在左侧列内被动换行；窗口最小宽度 960 时内容区恒为 720，因此不会挤压重叠。
fn settings_row(
    t: &ThemeTokens,
    id: &'static str,
    title: &'static str,
    description: impl Into<SharedString>,
    control: AnyElement,
    error: Option<&str>,
    saved: Option<&'static str>,
    highlighted: bool,
) -> AnyElement {
    let left_width = ROW_INNER_W - ROW_GAP - ROW_CONTROL_W;
    let mut left = v_flex()
        .w(px(left_width))
        .flex_none()
        .gap(px(2.0))
        .child(
            div()
                .w_full()
                .text_size(px(FS_BODY))
                .font_weight(FontWeight::MEDIUM)
                .child(title),
        )
        .child(
            div()
                .w_full()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_secondary)
                .child(description.into()),
        );
    if let Some(error) = error {
        left = left.child(
            div()
                .w_full()
                .text_size(px(FS_MICRO))
                .text_color(t.danger)
                .child(error.to_string()),
        );
    }

    let mut right = h_flex()
        .w(px(ROW_CONTROL_W))
        .flex_none()
        .justify_end()
        .items_center()
        .gap(px(8.0))
        .child(control);
    if let Some(saved) = saved {
        right = right.child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.success)
                .child(saved),
        );
    }

    div()
        .id(id)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(ROW_GAP))
        .min_h(px(56.0))
        .px(px(12.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(t.border_subtle)
        .when(highlighted, |this| this.bg(t.accent_soft))
        .when(highlighted, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(12.0))
                    .h(px(24.0))
                    .w(px(2.0))
                    .rounded(px(1.0))
                    .bg(t.accent),
            )
        })
        .child(left)
        .child(right)
        .into_any_element()
}

/// 分区行（标题/说明在上，整行宽度给选择器类控件）：
/// 提供商 chip、模型 chip 与价格说明这类横向内容超过控件列（320）时使用，
/// 视觉上仍是「标题 + 说明 + 内容」的设置行，并保留行内「已保存」与搜索高亮。
fn settings_stack_row(
    t: &ThemeTokens,
    id: &'static str,
    title: &'static str,
    description: impl Into<SharedString>,
    content: AnyElement,
    error: Option<&str>,
    saved: Option<&'static str>,
    highlighted: bool,
) -> AnyElement {
    let mut head_left = v_flex()
        .flex_1()
        .min_w(px(0.0))
        .gap(px(2.0))
        .child(
            div()
                .w_full()
                .text_size(px(FS_BODY))
                .font_weight(FontWeight::MEDIUM)
                .child(title),
        )
        .child(
            div()
                .w_full()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_secondary)
                .child(description.into()),
        );
    if let Some(error) = error {
        head_left = head_left.child(
            div()
                .w_full()
                .text_size(px(FS_MICRO))
                .text_color(t.danger)
                .child(error.to_string()),
        );
    }

    let mut head = h_flex()
        .w_full()
        .items_start()
        .gap(px(8.0))
        .child(head_left);
    if let Some(saved) = saved {
        head = head.child(
            div()
                .flex_none()
                .text_size(px(FS_MICRO))
                .text_color(t.success)
                .child(saved),
        );
    }

    div()
        .id(id)
        .relative()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .min_h(px(56.0))
        .px(px(12.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(t.border_subtle)
        .when(highlighted, |this| this.bg(t.accent_soft))
        .when(highlighted, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(12.0))
                    .h(px(24.0))
                    .w(px(2.0))
                    .rounded(px(1.0))
                    .bg(t.accent),
            )
        })
        .child(head)
        .child(content)
        .into_any_element()
}

/// 「分类与后缀名」行：图标 + 分类名 + 说明（可带行内错误），下方是**整行宽**的后缀输入控件 +
/// 「恢复默认」。
///
/// 2026-10-22 用户反馈：后缀列表较长，原先右侧 320px 列里的 180px 输入框只能看到部分后缀名。
/// 改为两行布局（标题/说明在上、输入框独占一行），输入框可用宽度 ≈ `ROW_INNER_W - 140px`。
#[allow(clippy::too_many_arguments)]
fn classification_row(
    t: &ThemeTokens,
    id: &'static str,
    icon: RdmIcon,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: AnyElement,
    error: Option<&str>,
    saved: Option<&'static str>,
    highlighted: bool,
) -> AnyElement {
    let mut body = v_flex().w_full().min_w(px(0.0)).gap(px(4.0));
    let mut head = h_flex()
        .w_full()
        .items_center()
        .gap(px(8.0))
        .child(div().flex_none().child(icon.sized(16.0, t.text_secondary)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(FS_BODY))
                .font_weight(FontWeight::MEDIUM)
                .child(title.into()),
        );
    if let Some(saved) = saved {
        head = head.child(
            div()
                .flex_none()
                .text_size(px(FS_MICRO))
                .text_color(t.success)
                .child(saved),
        );
    }
    body = body.child(head).child(
        div()
            .w_full()
            .text_size(px(FS_CAPTION))
            .text_color(t.text_secondary)
            .child(description.into()),
    );
    if let Some(error) = error {
        body = body.child(
            div()
                .w_full()
                .text_size(px(FS_MICRO))
                .text_color(t.danger)
                .child(error.to_string()),
        );
    }
    body = body.child(control);

    div()
        .id(id)
        .relative()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .min_h(px(56.0))
        .px(px(12.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(t.border_subtle)
        .when(highlighted, |this| this.bg(t.accent_soft))
        .when(highlighted, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(12.0))
                    .h(px(24.0))
                    .w(px(2.0))
                    .rounded(px(1.0))
                    .bg(t.accent),
            )
        })
        .child(body)
        .into_any_element()
}

fn value_text(t: &ThemeTokens, value: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(FS_BODY))
        .text_color(t.text_secondary)
        .child(value.into())
        .into_any_element()
}

fn kv_row(t: &ThemeTokens, label: &'static str, value: impl Into<SharedString>) -> AnyElement {
    h_flex()
        .gap(px(8.0))
        .items_start()
        .child(
            div()
                .w(px(96.0))
                .flex_none()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_tertiary)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_secondary)
                .child(value.into()),
        )
        .into_any_element()
}

/// 快捷键键帽（05 A24 / 04 §4.2：mono 10.5、`bg.sunken` 底 + `border.subtle` 描边）。
fn key_chip(t: &ThemeTokens, label: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(18.0))
        .min_w(px(18.0))
        .px(px(5.0))
        .rounded(px(4.0))
        .bg(t.bg_sunken)
        .border_1()
        .border_color(t.border_subtle)
        .font(theme::mono_font())
        .text_size(px(FS_MONO_MICRO))
        .text_color(t.text_secondary)
        .child(label)
        .into_any_element()
}

fn shortcut_row(t: &ThemeTokens, keys: &[&'static str], action: &'static str) -> AnyElement {
    h_flex()
        .min_h(px(34.0))
        .items_center()
        .gap(px(12.0))
        .px(px(12.0))
        .py(px(4.0))
        .border_b_1()
        .border_color(t.border_subtle)
        .child(
            h_flex()
                .w(px(230.0))
                .flex_none()
                .flex_wrap()
                .gap(px(4.0))
                .children(keys.iter().map(|key| key_chip(t, key))),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(FS_BODY))
                .text_color(t.text_secondary)
                .child(action),
        )
        .into_any_element()
}

/// Base URL 校验：留空 = 使用预设默认服务；否则必须是带主机名的 http(s) URL。
///
/// 用 `url::Url::parse` 取代字符串前缀判断（审计 2026-10-22 F-19）：
/// `HTTP://api.example.com/v1` 这类大写 scheme 合法；`"http:// bad"`、
/// `ftp://x`、`http://`（无 host）等必须在保存前拒绝，而不是等请求阶段才报错。
fn validate_ai_base_url(raw: &str) -> Result<(), String> {
    let base = raw.trim();
    if base.is_empty() {
        return Ok(());
    }
    let parsed = url::Url::parse(base).map_err(|err| {
        format!("Base URL 不是合法 URL（{err}）；示例：https://api.example.com/v1")
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "Base URL 只支持 http(s)（当前 scheme={}）；示例：https://api.example.com/v1",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("Base URL 缺少主机名；示例：https://api.example.com/v1".to_string());
    }
    Ok(())
}

/// AI 配置校验：Base URL 合法；启用时必须填写模型。
fn validate_ai_config(config: &AiConfig) -> Result<(), String> {
    validate_ai_base_url(&config.base_url)?;
    if config.enabled && config.model.trim().is_empty() {
        return Err("启用 AI 助手时必须填写模型名称".to_string());
    }
    Ok(())
}

fn base_url_error(config: &AiConfig) -> Option<String> {
    validate_ai_base_url(&config.base_url).err()
}

fn model_error(config: &AiConfig) -> Option<String> {
    if config.enabled && config.model.trim().is_empty() {
        Some("启用 AI 助手时必须填写模型名称".to_string())
    } else {
        None
    }
}

/// 模型搜索过滤：大小写不敏感子串，保持 Provider 返回的排序。
fn filter_model_ids<'a>(models: &'a [String], query: &str) -> Vec<&'a String> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return models.iter().collect();
    }
    models
        .iter()
        .filter(|model| model.to_ascii_lowercase().contains(&query))
        .collect()
}

/// 北京时间 `HH:MM`（模型列表状态行的「来源 · 时间」）。
fn clock_hhmm_zh(at_ms: u64) -> String {
    const BEIJING_OFFSET_MINUTES: u64 = 8 * 60;
    let minutes = (at_ms / 60_000 + BEIJING_OFFSET_MINUTES) % (24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// 一次后台模型拉取：标准线程 + `std::sync::mpsc`，GPUI 侧用 `Timer` 轮询。
///
/// 不触碰 `EngineRuntime`（design 16 §5.2）：设置页自建一次性线程，内部放一个
/// current-thread tokio runtime 跑 [`AiClient::list_models`]，结果经 channel 回传。
struct AiModelsFetch {
    receiver: std::sync::mpsc::Receiver<Result<Vec<String>, String>>,
    started_ms: u64,
    endpoint: String,
}

/// 模型列表状态行（拉取中 / 成功来源与时间 / 失败原因）。
#[derive(Default)]
struct AiModelsStatus {
    text: String,
    error: bool,
}

impl AiModelsStatus {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
        }
    }
}

/// 读取 `<state_dir>/ipc.json` 的脱敏摘要（**永不读取/输出 token**）。
fn ipc_summary(state_dir: &Path) -> String {
    let path = state_dir.join("ipc.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => return format!("ipc.json 不可读（{error}）"),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => return format!("ipc.json 格式异常（{error}）"),
    };
    let field = |key: &str| {
        value
            .get(key)
            .map(|value| match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "?".to_string())
    };
    format!(
        "protocol={} port={} pid={} app_version={}（token 已省略）",
        field("protocol"),
        field("port"),
        field("pid"),
        field("app_version"),
    )
}

/// 截图/验收钩子：`RDM_TEST_SETTINGS_SECTION=<section>` 打开指定分类。
///
/// 与 `RDM_TEST_OPEN_SETTINGS` 同一约定：只认环境变量，不写用户文档。
fn test_section_from_env() -> Option<SettingsSection> {
    let raw = std::env::var("RDM_TEST_SETTINGS_SECTION")
        .ok()
        .or_else(|| {
            // 也接受 `RDM_TEST_OPEN_SETTINGS=appearance|ai|system|classification` 的旧写法。
            std::env::var("RDM_TEST_OPEN_SETTINGS")
                .ok()
                .filter(|value| {
                    let value = value.trim().to_ascii_lowercase();
                    !(value.is_empty()
                        || value == "0"
                        || value == "1"
                        || value == "true"
                        || value == "false")
                })
        })?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "general" | "1" | "true" => Some(SettingsSection::General),
        "engine" | "download" => Some(SettingsSection::Engine),
        "storage" | "classification" => Some(SettingsSection::Storage),
        "browser" | "system" => Some(SettingsSection::Browser),
        "ai" => Some(SettingsSection::Ai),
        "appearance" => Some(SettingsSection::Appearance),
        "shortcuts" | "keys" => Some(SettingsSection::Shortcuts),
        "advanced" => Some(SettingsSection::Advanced),
        "about" => Some(SettingsSection::About),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 分类与后缀名（design 16 §4.4：后缀列表是分类的唯一事实来源）
// ---------------------------------------------------------------------------

/// 「分类与后缀名」可编辑分类的稳定 id 表：（kind、设置行 id、恢复默认按钮 id）。
///
/// 顺序与 `rdm_ai::CATEGORY_KINDS` 一致；`other` 是兜底行，没有输入框，故不在表内。
const CLASSIFICATION_EDITORS: [(&str, &str, &str); 6] = [
    ("video", "storage-class-video", "storage-class-video-reset"),
    ("audio", "storage-class-audio", "storage-class-audio-reset"),
    (
        "document",
        "storage-class-document",
        "storage-class-document-reset",
    ),
    ("image", "storage-class-image", "storage-class-image-reset"),
    (
        "archive",
        "storage-class-archive",
        "storage-class-archive-reset",
    ),
    (
        "program",
        "storage-class-program",
        "storage-class-program-reset",
    ),
];

/// 某个 kind 是否是可编辑分类；返回（设置行 id、恢复默认按钮 id）。
fn classification_editor_ids(kind: &str) -> Option<(&'static str, &'static str)> {
    CLASSIFICATION_EDITORS
        .iter()
        .find(|(editor_kind, _, _)| *editor_kind == kind)
        .map(|(_, row, reset)| (*row, *reset))
}

/// 后缀列表 → 输入框展示文本（`zip, rar, 7z`）。
fn extension_list_text(extensions: &[String]) -> String {
    extensions.join(", ")
}

/// 解析「逗号分隔的后缀名」输入（design 16 §4.4）。
///
/// 分隔符：`,`、中文逗号 `，` 或任意空白（可混用）。每一项去空白、去掉前导 `.`、
/// 统一小写后去重；非法字符 / 过长 / 整体为空 / 超过每类上限都返回行内错误文案。
fn parse_extension_input(raw: &str) -> Result<Vec<String>, String> {
    let mut extensions: Vec<String> = Vec::new();
    for token in raw.split(|ch: char| ch == ',' || ch == '，' || ch.is_whitespace()) {
        let trimmed = token.trim();
        let extension = trimmed.trim_start_matches('.').to_ascii_lowercase();
        if extension.is_empty() {
            continue;
        }
        if extension.chars().count() > MAX_EXTENSION_CHARS {
            return Err(format!(
                "后缀「{trimmed}」过长：最多 {MAX_EXTENSION_CHARS} 个字符"
            ));
        }
        if !extension.chars().all(|ch| ch.is_ascii_alphanumeric()) {
            return Err(format!("后缀「{trimmed}」含非法字符：只能包含字母和数字"));
        }
        if !extensions.iter().any(|existing| existing == &extension) {
            extensions.push(extension);
        }
    }
    if extensions.is_empty() {
        return Err("至少填写一个后缀名（逗号或空格分隔），例如 zip, rar, 7z".to_string());
    }
    if extensions.len() > MAX_EXTENSIONS_PER_CATEGORY {
        return Err(format!(
            "每个分类最多 {MAX_EXTENSIONS_PER_CATEGORY} 个后缀（当前 {} 个）",
            extensions.len()
        ));
    }
    Ok(extensions)
}

/// 后缀全局唯一性校验：返回第一个已被其他分类占用的（后缀、分类名）。
fn extension_conflict(
    kind: &str,
    extensions: &[String],
    rules: &[CategoryRule],
) -> Option<(String, String)> {
    for rule in rules {
        if rule.kind == kind || rule.kind == "other" {
            continue;
        }
        for extension in extensions {
            if rule
                .extensions
                .iter()
                .any(|owned| owned.eq_ignore_ascii_case(extension))
            {
                return Some((extension.clone(), rule.label.clone()));
            }
        }
    }
    None
}

/// 把 `kind` 恢复为内置默认后缀，并从其他分类移回被占用的后缀（保证全局唯一）。
///
/// 返回 `None` 表示不是内置分类；`Some(n)` 的 `n` 是从其他分类移回的后缀数。
fn restore_default_extensions(rules: &mut [CategoryRule], kind: &str) -> Option<usize> {
    let defaults = default_category_rules();
    let default_rule = defaults.iter().find(|rule| rule.kind == kind)?;
    let desired = default_rule.extensions.clone();
    let mut moved = 0usize;
    for rule in rules.iter_mut() {
        if rule.kind == kind || rule.kind == "other" {
            continue;
        }
        let before = rule.extensions.len();
        rule.extensions.retain(|extension| {
            !desired
                .iter()
                .any(|wanted| wanted.eq_ignore_ascii_case(extension))
        });
        moved += before - rule.extensions.len();
    }
    let rule = rules.iter_mut().find(|rule| rule.kind == kind)?;
    rule.extensions = desired;
    Some(moved)
}

// ---------------------------------------------------------------------------
// 设置页视图
// ---------------------------------------------------------------------------

/// 设置页视图（独立 Entity：避免输入框/搜索状态牵动主窗口重绘）。
pub struct SettingsPage {
    weak_app: WeakEntity<crate::views::app::RdmApp>,
    pub(crate) section: SettingsSection,
    pub(crate) draft: Settings,
    /// 首帧占位：初始化任务完成前不渲染具体设置值。
    initialized: bool,

    // 顶部搜索
    search_input: Entity<InputState>,
    search_query: String,

    // 下载引擎输入
    connections_input: Entity<InputState>,
    speed_input: Entity<InputState>,
    connections_error: Option<String>,
    speed_error: Option<String>,

    // 存储与分类
    dir_error: Option<String>,
    /// 「分类与后缀名」每个可编辑分类一个后缀输入框（key = `CategoryRule.kind`）。
    classification_inputs: BTreeMap<&'static str, Entity<InputState>>,
    /// 每个分类的行内错误（非法后缀 / 与其他分类冲突）；保存成功后清除。
    classification_errors: BTreeMap<String, String>,

    // AI 助手（每个 Provider 一份独立档案；字段跟随当前档案一起切换）
    ai_base_url_input: Entity<InputState>,
    ai_model_input: Entity<InputState>,
    ai_api_key_input: Entity<InputState>,
    /// 自定义 Provider 的显示名称。
    ai_profile_name_input: Entity<InputState>,
    /// 模型列表搜索框（大小写不敏感子串）。
    ai_model_search_input: Entity<InputState>,
    ai_draft: AiConfig,
    ai_applied: AiConfig,
    ai_last_row: &'static str,
    /// Provider 档案 chip 的键盘焦点（按 `profiles()` 顺序复用）。
    ai_profile_focus: Vec<FocusHandle>,
    /// 模型搜索关键字。
    ai_model_filter: String,
    /// 模型列表状态行（拉取中 / 成功来源与时间 / 失败原因）。
    ai_models_status: AiModelsStatus,
    /// 在途的后台模型拉取；`None` 表示空闲。
    ai_models_fetch: Option<AiModelsFetch>,
    /// 拉取代次：切换 Provider/重置时递增，旧轮询任务自动作废。
    ai_models_serial: u64,
    /// 本次设置会话内已发起过模型拉取的档案 id（避免每次渲染重复请求）。
    ai_models_fetched: BTreeMap<String, u64>,
    ai_models_scroll: ScrollHandle,

    // 只读快照（避免在 render 期间读 RdmApp）
    browser_ok: bool,
    browser_warning: Option<String>,
    state_dir: PathBuf,
    ipc_summary: String,

    // 保存/高亮状态
    serial: u64,
    ai_serial: u64,
    saved: Option<SavedNote>,
    saved_token: u64,
    highlight_row: Option<&'static str>,
    section_offsets: [f32; 9],

    // 键盘可达
    nav_focus: Vec<FocusHandle>,
    accent_focus: Vec<FocusHandle>,
    toggle_focus: Vec<(&'static str, FocusHandle)>,
    content_scroll: ScrollHandle,

    /// 输入框事件订阅必须随视图持有（drop 即取消）。
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    /// 定位到指定分类（资源监控页「去设置 AI」等跨页入口用）。
    pub fn select_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.section = section;
        self.search_query.clear();
        self.content_scroll.set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        weak_app: WeakEntity<crate::views::app::RdmApp>,
    ) -> Self {
        let defaults = Settings::default();
        let ai_defaults = AiConfig::default();

        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索设置…")
                .clean_on_escape()
        });
        let connections_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(defaults.default_connections.to_string())
        });
        let speed_input = cx.new(|cx| InputState::new(window, cx).placeholder("不限速"));

        // 分类与后缀名：每个可编辑分类一个输入框（`other` 兜底行没有输入框）。
        // 初值用内置默认规则填好（未聚焦时从第一个后缀开始显示，不会像 `set_value`
        // 那样因光标在末尾而把长列表滚到尾部）；真正设置值在 `initialize` 回填。
        let default_rules = defaults.classification.clone();
        let classification_inputs: BTreeMap<&'static str, Entity<InputState>> =
            CLASSIFICATION_EDITORS
                .iter()
                .map(|(kind, _, _)| {
                    let initial = default_rules
                        .iter()
                        .find(|rule| rule.kind == *kind)
                        .map(|rule| extension_list_text(&rule.extensions))
                        .unwrap_or_default();
                    let example = default_rules
                        .iter()
                        .find(|rule| rule.kind == *kind)
                        .map(|rule| {
                            let preview = &rule.extensions[..rule.extensions.len().min(3)];
                            extension_list_text(preview)
                        })
                        .unwrap_or_default();
                    let placeholder = if example.is_empty() {
                        "逗号分隔的后缀名".to_string()
                    } else {
                        format!("例如 {example}")
                    };
                    (
                        *kind,
                        cx.new(|cx| {
                            InputState::new(window, cx)
                                .default_value(initial.clone())
                                .placeholder(placeholder.clone())
                        }),
                    )
                })
                .collect();
        let ai_base_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(ai_defaults.base_url.clone())
                .placeholder("https://api.deepseek.com/v1")
        });
        let ai_model_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(ai_defaults.model.clone())
                .placeholder("deepseek-flash")
        });
        let ai_api_key_input =
            cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("sk-…"));
        let ai_profile_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Provider 名称"));
        // 截图/验收钩子：预设模型搜索文本，用于验证「输入文本可见 + 列表被过滤」
        //（真实键盘输入走同一 `InputState`；无头截图脚本用它注入确定性内容）。
        let test_model_filter = std::env::var("RDM_TEST_AI_MODEL_FILTER").unwrap_or_default();
        let ai_model_search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(test_model_filter.clone())
                .placeholder("搜索模型…（输入即过滤）")
                .clean_on_escape()
        });

        let mut subscriptions: Vec<Subscription> = Vec::new();

        // 搜索：输入即过滤，Enter 跳到第一个结果。
        {
            let input = search_input.clone();
            subscriptions.push(cx.subscribe_in(
                &search_input,
                window,
                move |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        this.search_query = input.read(cx).value().to_string();
                        cx.notify();
                    }
                    InputEvent::PressEnter { secondary: false } => {
                        this.jump_to_first_search_hit(window, cx);
                    }
                    _ => {}
                },
            ));
        }

        // 默认连接数：合法才写入草稿并防抖保存，非法行内红字。
        {
            let input = connections_input.clone();
            subscriptions.push(cx.subscribe_in(
                &connections_input,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::Change) || !this.initialized {
                        return;
                    }
                    let text = input.read(cx).value().to_string();
                    match parse_connections(&text) {
                        Ok(value) => {
                            this.connections_error = None;
                            if value != this.draft.default_connections {
                                this.draft.default_connections = value;
                                this.schedule_save("engine-connections", window, cx);
                            } else {
                                cx.notify();
                            }
                        }
                        Err(error) => {
                            this.connections_error = Some(error);
                            cx.notify();
                        }
                    }
                },
            ));
        }

        // 全局限速：空 = 不限速。
        {
            let input = speed_input.clone();
            subscriptions.push(cx.subscribe_in(
                &speed_input,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::Change) || !this.initialized {
                        return;
                    }
                    let text = input.read(cx).value().to_string();
                    match parse_speed_limit_mb(&text) {
                        Ok(value) => {
                            this.speed_error = None;
                            if value != this.draft.speed_limit_bps {
                                this.draft.speed_limit_bps = value;
                                this.schedule_save("engine-speed-limit", window, cx);
                            } else {
                                cx.notify();
                            }
                        }
                        Err(error) => {
                            this.speed_error = Some(error);
                            cx.notify();
                        }
                    }
                },
            ));
        }

        // 分类后缀名：解析 → 唯一性校验 → 写入草稿并走 500ms 防抖保存（design 16 §4.4）。
        for (kind, input) in classification_inputs.iter() {
            let input = input.clone();
            let event_input = input.clone();
            let kind = *kind;
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::Change) || !this.initialized {
                        return;
                    }
                    let text = event_input.read(cx).value().to_string();
                    this.classification_input_changed(kind, &text, window, cx);
                },
            ));
        }

        // AI 输入：三项都走 500ms 防抖，只有配置变化时才写 ai.json。
        // Base URL / API Key 变化后旧模型列表不再可信：清掉本会话的拉取记录，
        // 保存成功后自动重新拉取一次。
        for (input, row) in [
            (ai_base_url_input.clone(), "ai-base-url"),
            (ai_model_input.clone(), "ai-model"),
            (ai_api_key_input.clone(), "ai-api-key"),
        ] {
            let input_probe = input.clone();
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::Change) || !this.initialized {
                        return;
                    }
                    let value = input_probe.read(cx).value().to_string();
                    match row {
                        "ai-base-url" => this.ai_draft.base_url = value,
                        "ai-model" => this.ai_draft.model = value,
                        _ => this.ai_draft.api_key = value,
                    }
                    if matches!(row, "ai-base-url" | "ai-api-key") {
                        let id = this.ai_draft.active_profile_id().to_owned();
                        this.ai_models_fetched.remove(&id);
                        this.ai_models_status =
                            AiModelsStatus::info("地址或 Key 已变化，保存后自动重新拉取模型");
                    }
                    this.ai_last_row = row;
                    if validate_ai_config(&this.ai_draft).is_ok()
                        && this.ai_draft != this.ai_applied
                    {
                        this.schedule_ai_save(window, cx);
                    } else {
                        cx.notify();
                    }
                },
            ));
        }

        // Provider 名称（仅自定义档案可编辑）：500ms 防抖写回当前档案。
        {
            let input = ai_profile_name_input.clone();
            subscriptions.push(cx.subscribe_in(
                &ai_profile_name_input,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::Change) || !this.initialized {
                        return;
                    }
                    let value = input.read(cx).value().to_string();
                    if this.ai_draft.rename_active_profile(&value) {
                        this.ai_last_row = "ai-profile-name";
                        this.schedule_ai_save(window, cx);
                    } else {
                        cx.notify();
                    }
                },
            ));
        }

        // 模型搜索：输入即过滤（大小写不敏感子串），Enter 选中首个匹配项。
        {
            let input = ai_model_search_input.clone();
            subscriptions.push(cx.subscribe_in(
                &ai_model_search_input,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if !this.initialized {
                        return;
                    }
                    match event {
                        InputEvent::Change => {
                            this.ai_model_filter = input.read(cx).value().to_string();
                            cx.notify();
                        }
                        InputEvent::PressEnter { secondary } => {
                            if *secondary {
                                return;
                            }
                            let models = this.ai_draft.active_models();
                            let filter = this.ai_model_filter.clone();
                            if let Some(first) = filter_model_ids(&models, &filter)
                                .first()
                                .map(|model| (*model).clone())
                            {
                                this.select_ai_model(&first, window, cx);
                            }
                        }
                        _ => {}
                    }
                },
            ));
        }

        let nav_focus = (0..SettingsSection::ALL.len())
            .map(|_| cx.focus_handle().tab_stop(true))
            .collect();
        let accent_focus = (0..AccentPreset::ALL.len())
            .map(|_| cx.focus_handle().tab_stop(true))
            .collect();
        let ai_profile_focus = (0..AI_PROFILE_FOCUS_MAX)
            .map(|_| cx.focus_handle().tab_stop(true))
            .collect();
        let toggle_focus = TOGGLE_IDS
            .iter()
            .map(|id| (*id, cx.focus_handle().tab_stop(true)))
            .collect();

        let this = Self {
            weak_app,
            section: SettingsSection::default(),
            draft: defaults,
            initialized: false,
            search_input,
            search_query: String::new(),
            connections_input,
            speed_input,
            connections_error: None,
            speed_error: None,
            dir_error: None,
            classification_inputs,
            classification_errors: BTreeMap::new(),
            ai_base_url_input,
            ai_model_input,
            ai_api_key_input,
            ai_profile_name_input,
            ai_model_search_input,
            ai_draft: ai_defaults.clone(),
            ai_applied: ai_defaults,
            ai_last_row: "ai-enabled",
            ai_profile_focus,
            ai_model_filter: test_model_filter,
            ai_models_status: AiModelsStatus::default(),
            ai_models_fetch: None,
            ai_models_serial: 0,
            ai_models_fetched: BTreeMap::new(),
            ai_models_scroll: ScrollHandle::new(),
            browser_ok: true,
            browser_warning: None,
            state_dir: PathBuf::new(),
            ipc_summary: String::new(),
            serial: 0,
            ai_serial: 0,
            saved: None,
            saved_token: 0,
            highlight_row: None,
            section_offsets: [0.0; 9],
            nav_focus,
            accent_focus,
            toggle_focus,
            content_scroll: ScrollHandle::new(),
            subscriptions,
        };

        // 延迟初始化：`SettingsPage::new` 在 `RdmApp::render` 内执行，此时读取
        // RdmApp 会触发 GPUI「entity already being updated」panic；放到下一帧再做。
        let init_task = cx.spawn_in(window, async move |weak, cx| {
            let _ = weak.update_in(cx, |this, window, cx| this.initialize(window, cx));
        });
        init_task.detach();

        this
    }

    /// 读取 RdmApp 快照并回填所有输入框（此时 RdmApp 不在更新栈上）。
    fn initialize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.initialized {
            return;
        }
        let Some(app) = self.weak_app.upgrade() else {
            return;
        };
        let (draft, ai, browser_ok, browser_warning, state_dir) = app.read_with(cx, |app, _| {
            (
                app.settings_snapshot(),
                app.ai_config_snapshot(),
                app.browser_integration_ok(),
                app.browser_integration_warning(),
                app.state_dir_path(),
            )
        });

        self.draft = draft.normalized();
        self.ai_draft = ai.clone();
        self.ai_applied = ai;
        self.browser_ok = browser_ok;
        self.browser_warning = browser_warning;
        self.state_dir = state_dir;
        self.ipc_summary = ipc_summary(&self.state_dir.clone());
        if let Some(section) = test_section_from_env() {
            self.section = section;
        }

        self.sync_engine_inputs(window, cx);
        self.sync_ai_inputs(window, cx);
        self.sync_classification_inputs(window, cx);
        self.connections_error = None;
        self.speed_error = None;
        self.dir_error = None;
        self.initialized = true;
        cx.notify();

        // 打开设置后焦点进入搜索框（03 §7 / 06 §3.5）。
        let search = self.search_input.clone();
        window.defer(cx, move |window, cx| {
            search.update(cx, |state, cx| state.focus(window, cx));
        });

        // 直接打开 AI 页时自动拉取一次当前 Provider 的模型（design 16 §5.2）。
        if self.section == SettingsSection::Ai {
            self.maybe_auto_fetch_models(window, cx);
        }

        // 截图/验收钩子：把焦点放到模型搜索框，用于验证「真实键盘输入是否回显」。
        if self.section == SettingsSection::Ai && std::env::var("RDM_TEST_FOCUS_AI_SEARCH").is_ok()
        {
            let input = self.ai_model_search_input.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |state, cx| state.focus(window, cx));
            });
        }

        // 截图/验收钩子：把焦点放到「视频」后缀输入框，验证分类编辑器真实输入回显。
        if self.section == SettingsSection::Storage
            && std::env::var("RDM_TEST_FOCUS_CLASSIFICATION").is_ok()
        {
            if let Some(input) = self.classification_inputs.get("video").cloned() {
                window.defer(cx, move |window, cx| {
                    input.update(cx, |state, cx| state.focus(window, cx));
                });
            }
        }
    }

    fn sync_engine_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connections = self.draft.default_connections.to_string();
        self.connections_input
            .update(cx, |state, cx| state.set_value(connections, window, cx));
        let speed = self.draft.speed_limit_mb_text();
        self.speed_input
            .update(cx, |state, cx| state.set_value(speed, window, cx));
    }

    fn sync_ai_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_ai_value_inputs(window, cx);
        let api_key = self.ai_draft.api_key.clone();
        self.ai_api_key_input
            .update(cx, |state, cx| state.set_value(api_key, window, cx));
        if let Some(profile) = self.ai_draft.active_profile() {
            let name = profile.display_name().to_owned();
            self.ai_profile_name_input
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }
    }

    /// 只回填 Base URL / 模型两个输入框（切换 Provider 或选择模型后调用）。
    ///
    /// API Key 由 [`sync_ai_inputs`](Self::sync_ai_inputs) 单独回填，避免在用户
    /// 正在输入时重置遮罩输入框。
    fn sync_ai_value_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let base_url = self.ai_draft.base_url.clone();
        self.ai_base_url_input
            .update(cx, |state, cx| state.set_value(base_url, window, cx));
        let model = self.ai_draft.model.clone();
        self.ai_model_input
            .update(cx, |state, cx| state.set_value(model, window, cx));
    }

    // -----------------------------------------------------------------------
    // 分类与后缀名（design 16 §4.4：按后缀分类，唯一事实来源 = `rdm_ai::classify`）
    // -----------------------------------------------------------------------

    /// 把草稿里的后缀规则回填到每个输入框（初始化 / 恢复默认 / 恢复所有设置后调用）。
    fn sync_classification_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let texts: Vec<(&'static str, String)> = self
            .classification_inputs
            .keys()
            .map(|kind| {
                let text = self
                    .draft
                    .classification
                    .iter()
                    .find(|rule| rule.kind == *kind)
                    .map(|rule| extension_list_text(&rule.extensions))
                    .unwrap_or_default();
                (*kind, text)
            })
            .collect();
        for (kind, text) in texts {
            if let Some(input) = self.classification_inputs.get(kind) {
                // 值没变就不 `set_value`：避免把未聚焦输入框的滚动位置/光标推到末尾。
                if input.read(cx).value().as_ref() == text {
                    continue;
                }
                input.update(cx, |state, cx| state.set_value(text, window, cx));
            }
        }
        self.classification_errors.clear();
    }

    /// 分类后缀输入变化：解析并校验唯一性，合法才写入草稿并走 500ms 防抖保存。
    ///
    /// 非法输入（空、非法字符、与其他分类冲突）只显示行内错误，**保留草稿里的旧值**。
    fn classification_input_changed(
        &mut self,
        kind: &'static str,
        raw: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parsed = match parse_extension_input(raw) {
            Ok(extensions) => extensions,
            Err(error) => {
                self.classification_errors.insert(kind.to_owned(), error);
                cx.notify();
                return;
            }
        };
        if let Some((extension, owner)) =
            extension_conflict(kind, &parsed, &self.draft.classification)
        {
            self.classification_errors.insert(
                kind.to_owned(),
                format!("后缀「{extension}」已属于「{owner}」；同一后缀只能出现在一个分类中"),
            );
            cx.notify();
            return;
        }
        self.classification_errors.remove(kind);
        let Some(rule) = self
            .draft
            .classification
            .iter_mut()
            .find(|rule| rule.kind == kind)
        else {
            return;
        };
        if rule.extensions == parsed {
            cx.notify();
            return;
        }
        rule.extensions = parsed;
        let Some((row, _)) = classification_editor_ids(kind) else {
            return;
        };
        self.schedule_save(row, window, cx);
    }

    /// 「恢复默认」：把该分类的后缀恢复为内置默认并立即保存。
    ///
    /// 默认后缀若当前被其他分类占用（用户手动移动过），会从其他分类移回，保证
    /// 「同一后缀只归属一个分类」；随后统一回填所有行，其他分类的变化可见。
    fn reset_category_extensions(
        &mut self,
        kind: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((row, _)) = classification_editor_ids(kind) else {
            return;
        };
        if restore_default_extensions(&mut self.draft.classification, kind).is_none() {
            return;
        }
        // 行内「已保存」列宽有限，固定用短文案；被移回的后缀在其他分类行上直接可见。
        self.apply_now(row, "已恢复默认", window, cx);
        self.sync_classification_inputs(window, cx);
        cx.notify();
    }

    /// 清空模型视图状态（切换 Provider / 恢复默认时）。
    ///
    /// 在途的后台拉取通过 `ai_models_serial` 作废，不会被旧结果覆盖。
    fn reset_ai_models_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ai_models_serial = self.ai_models_serial.wrapping_add(1);
        self.ai_models_fetch = None;
        self.ai_models_status = AiModelsStatus::default();
        self.ai_model_filter.clear();
        self.ai_model_search_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.ai_models_scroll.set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    /// 切换 Provider 档案：三个字段（Base URL / API Key / 模型）一起切换。
    ///
    /// `AiConfig::activate_profile` 先把当前平面值写回旧档案，再载入目标档案，
    /// 因此不会出现「改了 Provider 但 API Key 仍属于上一个 Provider」的状态。
    fn select_ai_profile(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_draft.active_profile_id() == id {
            return;
        }
        if !self.ai_draft.activate_profile(id) {
            return;
        }
        self.reset_ai_models_view(window, cx);
        self.sync_ai_inputs(window, cx);
        self.ai_last_row = "ai-provider";
        self.apply_ai_now("ai-provider", "已保存", window, cx);
        cx.notify();
    }

    /// 新增一个空白自定义 Provider 并切换到它。
    ///
    /// 初始 Base URL / 模型沿用当前 Provider，避免「已启用 AI 但新档案没有模型」
    /// 导致立即保存失败；API Key 留空由用户填写。
    fn add_custom_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fallback_base_url = self.ai_draft.base_url.clone();
        let fallback_model = self.ai_draft.model.clone();
        let id = self.ai_draft.add_custom_profile();
        if let Some(profile) = self.ai_draft.profile_mut(&id) {
            profile.base_url = fallback_base_url;
            profile.model = fallback_model;
        }
        self.ai_draft.activate_profile(&id);
        self.reset_ai_models_view(window, cx);
        self.sync_ai_inputs(window, cx);
        self.ai_last_row = "ai-provider";
        self.apply_ai_now("ai-provider", "已添加", window, cx);
        let input = self.ai_profile_name_input.clone();
        window.defer(cx, move |window, cx| {
            input.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.notify();
    }

    /// 删除当前自定义 Provider（内置 Provider 受保护，不会走到这里）。
    fn delete_active_custom_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.ai_draft.active_profile_id().to_owned();
        if !self.ai_draft.remove_profile(&id) {
            return;
        }
        self.ai_models_fetched.remove(&id);
        self.reset_ai_models_view(window, cx);
        self.sync_ai_inputs(window, cx);
        self.ai_last_row = "ai-provider";
        self.apply_ai_now("ai-provider", "已删除该 Provider", window, cx);
        cx.notify();
    }

    /// 从模型列表里选一个模型并立即保存。
    fn select_ai_model(&mut self, model: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_draft.model.trim() == model.trim() {
            return;
        }
        self.ai_draft.model = model.trim().to_owned();
        self.sync_ai_value_inputs(window, cx);
        self.ai_last_row = "ai-model";
        self.apply_ai_now("ai-model", "已保存", window, cx);
    }

    fn focus_ai_profile_chip(&self, index: usize, window: &mut Window) {
        if let Some(handle) = self.ai_profile_focus.get(index) {
            handle.focus(window);
        }
    }

    /// Provider 档案 chip 键盘：←/→ 移动焦点，Enter/Space 切换。
    fn ai_profile_key(
        &mut self,
        index: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.ai_draft.profiles().len().min(AI_PROFILE_FOCUS_MAX);
        if count == 0 {
            return;
        }
        match event.keystroke.key.as_str() {
            "left" => {
                let target = (index + count - 1) % count;
                self.focus_ai_profile_chip(target, window);
                cx.notify();
                cx.stop_propagation();
            }
            "right" => {
                let target = (index + 1) % count;
                self.focus_ai_profile_chip(target, window);
                cx.notify();
                cx.stop_propagation();
            }
            "enter" | "space" => {
                let id = self
                    .ai_draft
                    .profiles()
                    .get(index)
                    .map(|profile| profile.id.clone());
                if let Some(id) = id {
                    self.focus_ai_profile_chip(index, window);
                    self.select_ai_profile(&id, window, cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // AI 助手：Provider 档案 / 实时模型拉取 / 价格说明
    // -----------------------------------------------------------------------

    /// Provider 档案 chip 行：内置 3 个 + 自定义档案，末尾「+ 添加自定义 Provider」。
    ///
    /// 列表来自 `AiConfig::profiles()`（数据驱动），不再硬编码 `ProviderKind::ALL`。
    fn ai_profile_chips(
        &mut self,
        t: &ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.ai_draft.active_profile_id().to_owned();
        let profiles: Vec<(String, String)> = self
            .ai_draft
            .profiles()
            .iter()
            .map(|profile| (profile.id.clone(), profile.display_name().to_owned()))
            .collect();
        let mut row = h_flex().flex_wrap().gap(px(6.0));
        for (index, (id, name)) in profiles.into_iter().enumerate() {
            let selected = active == id;
            let fg = if selected {
                t.text_primary
            } else {
                t.text_secondary
            };
            let border = if selected { t.accent } else { t.border_default };
            let bg = if selected {
                t.accent_soft
            } else {
                t.bg_surface
            };
            let handle = self
                .ai_profile_focus
                .get(index)
                .cloned()
                .unwrap_or_else(|| self.nav_focus[0].clone());
            let click_id = id.clone();
            row = row.child(
                div()
                    .id(("ai-profile-chip", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(28.0))
                    .px(px(12.0))
                    .rounded(px(RADIUS_SM))
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .text_size(px(FS_CAPTION))
                    .font_weight(if selected {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(fg)
                    .cursor_pointer()
                    .track_focus(&handle)
                    .tab_stop(true)
                    .focus(|style| style.border_1().border_color(t.focus))
                    .hover(move |style| {
                        style.bg(if selected {
                            t.accent_soft_hover
                        } else {
                            t.bg_hover
                        })
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus_ai_profile_chip(index, window);
                        this.select_ai_profile(&click_id, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        this.ai_profile_key(index, event, window, cx);
                    }))
                    .child(name),
            );
        }
        row = row.child(
            div()
                .id("ai-profile-add")
                .flex()
                .items_center()
                .justify_center()
                .h(px(28.0))
                .px(px(12.0))
                .rounded(px(RADIUS_SM))
                .border_1()
                .border_color(t.border_default)
                .bg(t.bg_surface)
                .text_size(px(FS_CAPTION))
                .text_color(t.text_secondary)
                .cursor_pointer()
                .hover(move |style| style.bg(t.bg_hover))
                .on_click(cx.listener(|this, _, window, cx| this.add_custom_provider(window, cx)))
                .child("+ 添加自定义 Provider"),
        );
        row.into_any_element()
    }

    /// 模型编辑区：手填 model id + 搜索框 + 「拉取模型」按钮 + 状态行 + 可滚动列表。
    fn ai_model_panel(
        &mut self,
        t: &ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let models = self.ai_draft.active_models();
        let current = self.ai_draft.model.clone();
        let preset = self.ai_draft.provider_preset();
        let endpoint = self.ai_draft.models_endpoint();
        let filter = self.ai_model_filter.clone();
        let filtered: Vec<String> = filter_model_ids(&models, &filter)
            .into_iter()
            .cloned()
            .collect();
        let total = models.len();
        let shown = filtered.len();
        let fetching = self.ai_models_fetch.is_some();

        let mut panel = v_flex().w_full().gap(px(8.0)).child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(Input::new(&self.ai_model_input).w(px(360.0)))
                .child(
                    Button::new("ai-fetch-models")
                        .outline()
                        .small()
                        .label(if fetching {
                            "拉取中…"
                        } else {
                            "拉取模型"
                        })
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_model_fetch(window, cx)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("可直接手填任意 model id（修改后自动保存）"),
                ),
        );

        // 搜索框独占一行；输入即过滤模型列表。
        // 2026-10-22 用户反馈两轮：
        //  1) 原先塞在 flex 行里的输入框宽度塌缩到只显示图标 → 改为独占一行；
        //  2) 改成 `w_full()` + 默认尺寸后宽度正常，但**真实键盘输入不绘制字符**（值在更新、
        //     列表在过滤，框内只有光标）。根因是 gpui-component `Input` 在设置页 flex 布局里
        //     使用 `w_full()` 时文本元素布局异常；必须用**固定像素宽度 + `.small()`**（与顶栏
        //     搜索框一致）。回归验证：`scripts/verify-ai-search-echo.ps1`（真实 SendInput）。
        panel = panel.child(
            v_flex()
                .w_full()
                .gap(px(4.0))
                .child(
                    Input::new(&self.ai_model_search_input)
                        .small()
                        .w(px(560.0))
                        .cleanable(true)
                        .prefix(RdmIcon::Search.sized(14.0, t.text_tertiary)),
                )
                .child(
                    h_flex().w_full().justify_end().child(
                        div()
                            .text_size(px(FS_MICRO))
                            .text_color(t.text_tertiary)
                            .child(format!("显示 {shown} / {total} 个模型")),
                    ),
                ),
        );

        let status = &self.ai_models_status;
        let icon = if status.error {
            RdmIcon::Alert
        } else if fetching {
            RdmIcon::Loader
        } else {
            RdmIcon::Info
        };
        let status_color = if status.error {
            t.danger
        } else {
            t.text_tertiary
        };
        let status_text = if status.text.is_empty() {
            format!("尚未拉取模型；来源：{endpoint}")
        } else {
            status.text.clone()
        };
        panel = panel.child(
            h_flex()
                .w_full()
                .items_start()
                .gap(px(6.0))
                .child(icon.sized(14.0, status_color))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(FS_MICRO))
                        .text_color(status_color)
                        .child(status_text),
                ),
        );

        let mut list = v_flex()
            .id("ai-models-scroll")
            .w_full()
            .max_h(px(AI_MODEL_LIST_MAX_H))
            .overflow_y_scroll()
            .track_scroll(&self.ai_models_scroll)
            .rounded(px(RADIUS_SM))
            .border_1()
            .border_color(t.border_subtle)
            .bg(t.bg_surface);
        if total == 0 {
            list = list.child(
                div()
                    .p(px(10.0))
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_tertiary)
                    .child(format!(
                        "当前 Provider 还没有模型列表：点击「拉取模型」从 {endpoint} 获取（需要 API Key）；也可以直接在上方手填 model id。"
                    )),
            );
        } else if shown == 0 {
            list = list.child(
                div()
                    .p(px(10.0))
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_tertiary)
                    .child(format!(
                        "没有匹配「{}」的模型；可直接在上方手填 model id。",
                        filter.trim()
                    )),
            );
        } else {
            for (index, model) in filtered.iter().enumerate().take(500) {
                let selected = model.eq_ignore_ascii_case(&current);
                let click_model = model.clone();
                let fg = if selected {
                    t.text_primary
                } else {
                    t.text_secondary
                };
                let price_hint = preset
                    .and_then(|preset| {
                        preset
                            .models
                            .iter()
                            .find(|price| price.model.eq_ignore_ascii_case(model))
                    })
                    .map(price_short_zh);
                let mut row = div()
                    .id(("ai-model-row", index))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(RADIUS_SM))
                    .cursor_pointer()
                    .when(selected, |this| this.bg(t.accent_soft))
                    .hover(move |style| {
                        style.bg(if selected {
                            t.accent_soft_hover
                        } else {
                            t.bg_hover
                        })
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_ai_model(&click_model, window, cx);
                    }))
                    .child(div().flex_1().min_w(px(0.0)).truncate().child(mono_text(
                        model.clone(),
                        FS_MONO_MICRO,
                        fg,
                    )));
                if selected {
                    row = row.child(RdmIcon::Check.sized(12.0, t.accent));
                }
                if let Some(hint) = price_hint {
                    row = row.child(
                        div()
                            .flex_none()
                            .text_size(px(FS_MICRO))
                            .text_color(t.text_tertiary)
                            .child(hint),
                    );
                }
                list = list.child(row);
            }
        }
        panel = panel.child(list);
        panel.into_any_element()
    }

    /// 若当前 Provider 本次设置会话内还没拉取过模型，就自动拉取一次。
    ///
    /// 触发点：打开 AI 页、切到 AI 页、切换 Provider、保存 API 地址/Key 之后。
    fn maybe_auto_fetch_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_models_fetch.is_some() {
            return;
        }
        let id = self.ai_draft.active_profile_id().to_owned();
        if self.ai_models_fetched.contains_key(&id) {
            return;
        }
        if self.ai_draft.resolved_api_key().is_empty() {
            if self.ai_draft.active_models().is_empty() {
                self.ai_models_status =
                    AiModelsStatus::info("填写 API Key 后会自动拉取模型列表（也可点「拉取模型」）");
                cx.notify();
            }
            return;
        }
        self.start_model_fetch(window, cx);
    }

    /// 启动一次后台模型拉取（标准线程 + 内部 current-thread tokio runtime）。
    ///
    /// 设置页允许「先拉模型、后启用」：临时把 `enabled` 置 true 通过客户端校验。
    fn start_model_fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_models_fetch.is_some() {
            return;
        }
        let mut config = self.ai_draft.clone();
        config.enabled = true;
        let endpoint = config.models_endpoint();
        if config.resolved_api_key().is_empty() {
            self.ai_models_status = AiModelsStatus::error("先填写 API Key 再拉取模型");
            cx.notify();
            return;
        }
        let profile_id = self.ai_draft.active_profile_id().to_owned();
        self.ai_models_serial = self.ai_models_serial.wrapping_add(1);
        let serial = self.ai_models_serial;
        self.ai_models_fetched.insert(profile_id, now_ms());
        self.ai_models_status = AiModelsStatus::info(format!("正在从 {endpoint} 拉取模型…"));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<Vec<String>, String> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("无法创建后台运行时：{error}"))?;
                runtime.block_on(async move {
                    let client = AiClient::new(config)
                        .map_err(|error| format!("创建 AI 客户端失败：{error}"))?;
                    client
                        .list_models()
                        .await
                        .map_err(|error| format!("拉取模型失败：{error}"))
                })
            })();
            let _ = sender.send(result);
        });
        self.ai_models_fetch = Some(AiModelsFetch {
            receiver,
            started_ms: now_ms(),
            endpoint,
        });
        cx.notify();
        let task = cx.spawn_in(window, async move |weak, cx| loop {
            Timer::after(Duration::from_millis(AI_MODEL_FETCH_POLL_MS)).await;
            let outcome = weak.update_in(cx, |this, window, cx| {
                this.poll_model_fetch(serial, window, cx)
            });
            match outcome {
                Ok(false) => continue,
                _ => break,
            }
        });
        task.detach();
    }

    /// 轮询一次后台拉取结果；返回 `true` 表示本次拉取已结束（成功/失败/超时）。
    fn poll_model_fetch(
        &mut self,
        serial: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.ai_models_serial != serial {
            return true;
        }
        let Some((result, endpoint, started_ms)) = self.ai_models_fetch.as_ref().map(|fetch| {
            (
                fetch.receiver.try_recv(),
                fetch.endpoint.clone(),
                fetch.started_ms,
            )
        }) else {
            return true;
        };
        match result {
            Ok(result) => {
                self.ai_models_fetch = None;
                self.apply_fetched_models(result, &endpoint, window, cx);
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if now_ms().saturating_sub(started_ms) > AI_MODEL_FETCH_TIMEOUT_MS {
                    self.ai_models_fetch = None;
                    self.ai_models_status = AiModelsStatus::error(format!(
                        "拉取模型超时（{} 秒）；请检查 API 地址与网络后重试",
                        AI_MODEL_FETCH_TIMEOUT_MS / 1000
                    ));
                    cx.notify();
                    true
                } else {
                    false
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.ai_models_fetch = None;
                self.ai_models_status = AiModelsStatus::error("后台拉取线程意外结束，请重试");
                cx.notify();
                true
            }
        }
    }

    /// 把拉取结果写入当前档案并保存；失败只更新状态行（保留上次列表/手填值）。
    fn apply_fetched_models(
        &mut self,
        result: Result<Vec<String>, String>,
        endpoint: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let models = match result {
            Ok(models) => models,
            Err(error) => {
                self.ai_models_status = AiModelsStatus::error(error);
                cx.notify();
                return;
            }
        };
        let count = models.len();
        self.ai_models_status = if count == 0 {
            AiModelsStatus::info(format!("服务返回 0 个模型（{endpoint}）；可手填 model id"))
        } else {
            AiModelsStatus::info(format!(
                "已获取 {count} 个模型 · 来源 {endpoint} · {}（北京时间）",
                clock_hhmm_zh(now_ms())
            ))
        };
        // 新 Provider 尚未选模型时默认选第一个，避免「启用 AI 但无模型」而无法保存。
        if self.ai_draft.model.trim().is_empty() {
            if let Some(first) = models.first().cloned() {
                self.ai_draft.model = first;
                self.sync_ai_value_inputs(window, cx);
            }
        }
        self.ai_draft.set_active_models(models);
        self.ai_last_row = "ai-model";
        if validate_ai_config(&self.ai_draft).is_err() {
            self.ai_models_status =
                AiModelsStatus::error("模型列表已更新，但启用 AI 时必须先选择/填写一个模型");
            cx.notify();
            return;
        }
        self.apply_ai_now("ai-model", "已更新模型列表", window, cx);
    }

    /// 「价格与估算」行内容：随当前模型动态展示；无价目时明确写「无价目估算」。
    fn ai_pricing_content(&self, t: &ThemeTokens) -> AnyElement {
        let ai = &self.ai_draft;
        let kind = ai.resolved_provider();
        let model = ai.model.trim().to_owned();
        let price = if model.is_empty() {
            None
        } else {
            find_model(kind, &model)
        };
        let preset = ai.provider_preset();
        let mut content = v_flex().gap(px(4.0));
        match price {
            Some(price) => {
                content = content
                    .child(
                        div()
                            .text_size(px(FS_CAPTION))
                            .text_color(t.text_secondary)
                            .child(format!(
                                "当前模型 {}（{}）· {}",
                                model,
                                price.label,
                                price_label_zh(price)
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(FS_MICRO))
                            .text_color(t.text_tertiary)
                            .child(format!(
                                "价目快照 {PRICE_CATALOG_DATE} · 价格来自官方公开页 · 估算口径，不代表账单"
                            )),
                    );
                if let Some(preset) = preset {
                    content = content.child(mono_text(
                        format!("查看价格页：{}", preset.docs_url),
                        FS_MONO_MICRO,
                        t.text_tertiary,
                    ));
                }
            }
            None => {
                let reason = if model.is_empty() {
                    "无价目估算：尚未选择模型；从上方列表选择或手填 model id 后再显示价目。"
                        .to_string()
                } else if preset.is_none() || kind == ProviderKind::Custom {
                    format!(
                        "无价目估算：自定义 Provider 不内置价格；模型「{model}」的 token 用量仍正常统计。"
                    )
                } else {
                    format!(
                        "无价目估算：模型「{model}」不在内置价目表中；资源监控只统计 token 用量。"
                    )
                };
                content = content.child(
                    h_flex()
                        .items_start()
                        .gap(px(6.0))
                        .child(RdmIcon::Info.sized(14.0, t.info))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(FS_CAPTION))
                                .text_color(t.text_secondary)
                                .child(reason),
                        ),
                );
            }
        }
        if kind == ProviderKind::DeepSeek {
            let off_peak = deepseek_is_off_peak(now_ms());
            content = content.child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_tertiary)
                    .child(format!(
                        "{}。当前（北京时间）{}。",
                        deepseek_peak_note_zh(),
                        if off_peak {
                            "空闲时段，价格为高峰五折"
                        } else {
                            "高峰时段，按高峰价估算"
                        }
                    )),
            );
        }
        content.into_any_element()
    }

    // -----------------------------------------------------------------------
    // 保存 / 提示
    // -----------------------------------------------------------------------

    /// 500ms 防抖后应用 `draft`；generation 不匹配的旧 timer 直接丢弃。
    fn schedule_save(&mut self, row: &'static str, window: &Window, cx: &mut Context<Self>) {
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let task = cx.spawn_in(window, async move |weak, cx| {
            Timer::after(Duration::from_millis(SAVE_DEBOUNCE_MS)).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                if !this.initialized || this.serial != serial {
                    return;
                }
                this.apply_now(row, "已保存", window, cx);
            });
        });
        task.detach();
        cx.notify();
    }

    fn schedule_ai_save(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.ai_serial = self.ai_serial.wrapping_add(1);
        let serial = self.ai_serial;
        let task = cx.spawn_in(window, async move |weak, cx| {
            Timer::after(Duration::from_millis(SAVE_DEBOUNCE_MS)).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                if !this.initialized || this.ai_serial != serial {
                    return;
                }
                let row = this.ai_last_row;
                this.apply_ai_now(row, "已保存", window, cx);
            });
        });
        task.detach();
        cx.notify();
    }

    /// 立即提交设置草稿到 RdmApp（`apply_settings`：落盘 + 主题/系统集成即时生效）。
    fn apply_now(
        &mut self,
        row: &'static str,
        saved_text: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.serial = self.serial.wrapping_add(1);
        let draft = self.draft.clone().normalized();
        self.draft = draft.clone();
        if let Some(app) = self.weak_app.upgrade() {
            app.update(cx, |app, app_cx| app.apply_settings(draft, window, app_cx));
        }
        self.mark_saved(row, saved_text, window, cx);
        cx.notify();
    }

    /// 立即保存 AI 配置（写 `<state_dir>/ai.json`）。
    ///
    /// 保存前把当前平面值（Base URL / API Key / 模型）回写进行中的 Provider
    /// 档案，保证「任一字段变更 → 校验 → 写回档案 → 保存 → 立即生效」。
    fn apply_ai_now(
        &mut self,
        row: &'static str,
        saved_text: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ai_serial = self.ai_serial.wrapping_add(1);
        if validate_ai_config(&self.ai_draft).is_err() {
            cx.notify();
            return;
        }
        self.ai_draft.sync_active_profile();
        self.ai_applied = self.ai_draft.clone();
        let config = self.ai_draft.clone();
        if let Some(app) = self.weak_app.upgrade() {
            app.update(cx, |app, app_cx| app.apply_ai_config(config, app_cx));
        }
        self.mark_saved(row, saved_text, window, cx);
        cx.notify();
        // 地址/Key/Provider 变化后旧模型列表不再可信：保存成功后自动重新拉取。
        if matches!(row, "ai-provider" | "ai-base-url" | "ai-api-key") {
            self.maybe_auto_fetch_models(window, cx);
        }
    }

    fn mark_saved(
        &mut self,
        row: &'static str,
        text: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.saved = Some(SavedNote { row, text });
        self.saved_token = self.saved_token.wrapping_add(1);
        let token = self.saved_token;
        let task = cx.spawn_in(window, async move |weak, cx| {
            Timer::after(Duration::from_millis(SAVED_VISIBLE_MS)).await;
            let _ = weak.update_in(cx, |this, _window, cx| {
                if this.saved_token == token {
                    this.saved = None;
                    cx.notify();
                }
            });
        });
        task.detach();
    }

    fn saved_for(&self, row: &'static str) -> Option<&'static str> {
        self.saved
            .filter(|note| note.row == row)
            .map(|note| note.text)
    }

    fn is_highlighted(&self, row: &'static str) -> bool {
        self.highlight_row == Some(row)
    }

    fn flash_highlight(&mut self, row: &'static str, window: &Window, cx: &mut Context<Self>) {
        self.highlight_row = Some(row);
        let task = cx.spawn_in(window, async move |weak, cx| {
            Timer::after(Duration::from_millis(HIGHLIGHT_MS)).await;
            let _ = weak.update_in(cx, |this, _window, cx| {
                if this.highlight_row == Some(row) {
                    this.highlight_row = None;
                    cx.notify();
                }
            });
        });
        task.detach();
    }

    fn toggle_focus_for(&self, id: &'static str) -> FocusHandle {
        self.toggle_focus
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, handle)| handle.clone())
            .unwrap_or_else(|| self.nav_focus[0].clone())
    }

    /// 自绘 Switch（05 A8：36×20 轨道 + 16 滑块；开 `accent` / 关 `track`）。
    ///
    /// 说明：gpui-component 0.5.1 的 `Switch` 使用 `Theme.switch` / `switch_thumb`，
    /// 而 `ui/theme.rs` 尚未映射这两个字段（暗色主题下仍是默认浅色轨道），因此这里
    /// 按设计稿自绘；`Tab` 可达，`Space` / `Enter` 切换，焦点环用 `focus` token。
    fn switch_control(
        &self,
        t: &ThemeTokens,
        id: &'static str,
        checked: bool,
        window: &Window,
        cx: &mut Context<Self>,
        on_change: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let handle = self.toggle_focus_for(id);
        let focused = handle.is_focused(window);
        let on_change = std::rc::Rc::new(on_change);
        let click_change = on_change.clone();
        let key_change = on_change;
        let track = if checked { t.accent } else { t.track };
        let thumb = if checked {
            t.on_accent
        } else {
            t.text_tertiary
        };
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .p(px(2.0))
            .w(px(36.0))
            .h(px(20.0))
            .rounded_full()
            .bg(track)
            .cursor_pointer()
            .track_focus(&handle)
            .tab_stop(true)
            .border_1()
            .border_color(if focused { t.focus } else { track })
            .hover(move |style| {
                style.bg(if checked {
                    t.accent_hover
                } else {
                    t.border_strong
                })
            })
            .when(checked, |this| this.justify_end())
            .child(
                div()
                    .flex_none()
                    .size(px(16.0))
                    .rounded_full()
                    .bg(thumb)
                    .shadow_md(),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                click_change(this, window, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "space" | "enter") {
                    key_change(this, window, cx);
                    cx.stop_propagation();
                }
            }))
            .into_any_element()
    }

    // -----------------------------------------------------------------------
    // 导航 / 搜索跳转 / 恢复默认
    // -----------------------------------------------------------------------

    fn switch_section(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if section == self.section {
            return;
        }
        // 记住每页滚动位置（03 §7 / 06 §3.5：切换不丢滚动位置）。
        let y: f32 = self.content_scroll.offset().y.into();
        self.section_offsets[self.section.index()] = y;
        self.section = section;
        let y = self.section_offsets[section.index()];
        self.content_scroll.set_offset(point(px(0.0), px(y)));
        self.highlight_row = None;
        cx.notify();
        // 进入 AI 页时自动拉取一次当前 Provider 的模型（design 16 §5.2）。
        if section == SettingsSection::Ai {
            self.maybe_auto_fetch_models(window, cx);
        }
    }

    fn nav_key(
        &mut self,
        index: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "up" => {
                let target = (index + SettingsSection::ALL.len() - 1) % SettingsSection::ALL.len();
                self.nav_focus[target].focus(window);
                cx.notify();
                cx.stop_propagation();
            }
            "down" => {
                let target = (index + 1) % SettingsSection::ALL.len();
                self.nav_focus[target].focus(window);
                cx.notify();
                cx.stop_propagation();
            }
            "enter" | "space" => {
                let section = SettingsSection::ALL[index];
                self.nav_focus[index].focus(window);
                self.switch_section(section, window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn accent_key(
        &mut self,
        index: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "left" => {
                let target = (index + AccentPreset::ALL.len() - 1) % AccentPreset::ALL.len();
                self.accent_focus[target].focus(window);
                cx.notify();
                cx.stop_propagation();
            }
            "right" => {
                let target = (index + 1) % AccentPreset::ALL.len();
                self.accent_focus[target].focus(window);
                cx.notify();
                cx.stop_propagation();
            }
            "enter" | "space" => {
                let preset = AccentPreset::ALL[index];
                self.accent_focus[index].focus(window);
                self.draft.accent = preset;
                self.apply_now("appearance-accent", "已保存", window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn search_query_is_empty(&self) -> bool {
        self.search_query.trim().is_empty()
    }

    fn jump_to_first_search_hit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = search_hits(&self.search_query).into_iter().next() else {
            return;
        };
        self.jump_to(entry.section, entry.row, window, cx);
    }

    fn jump_to(
        &mut self,
        section: SettingsSection,
        row: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.section = section;
        self.section_offsets[section.index()] = 0.0;
        self.content_scroll.set_offset(point(px(0.0), px(0.0)));
        if !self.search_query_is_empty() {
            self.search_input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.search_query.clear();
        self.flash_highlight(row, window, cx);
        cx.notify();
    }

    fn reset_current_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let defaults = Settings::default();
        match self.section {
            SettingsSection::General => {
                self.draft.auto_start = defaults.auto_start;
                self.draft.silent_start = defaults.silent_start;
                self.draft.minimize_to_tray = defaults.minimize_to_tray;
                self.draft.launch_at_startup = defaults.launch_at_startup;
            }
            SettingsSection::Engine => {
                self.draft.default_connections = defaults.default_connections;
                self.draft.speed_limit_bps = defaults.speed_limit_bps;
                self.draft.dynamic_connections = defaults.dynamic_connections;
                self.connections_error = None;
                self.speed_error = None;
                self.sync_engine_inputs(window, cx);
            }
            SettingsSection::Storage => {
                self.draft.default_dir = defaults.default_dir.clone();
                self.draft.save_by_extension = defaults.save_by_extension;
                self.draft.verify_sha256 = defaults.verify_sha256;
                self.draft.classification = defaults.classification.clone();
                self.dir_error = None;
                self.sync_classification_inputs(window, cx);
            }
            SettingsSection::Ai => {
                self.ai_draft = AiConfig::default();
                self.ai_applied = self.ai_draft.clone();
                self.ai_models_fetched.clear();
                self.reset_ai_models_view(window, cx);
                self.sync_ai_inputs(window, cx);
                self.apply_ai_now(FOOTER_ROW, "已恢复本页默认", window, cx);
                return;
            }
            SettingsSection::Appearance => {
                self.draft.ui_theme = defaults.ui_theme;
                self.draft.accent = defaults.accent;
                self.draft.density = defaults.density;
                self.draft.reduce_motion = defaults.reduce_motion;
            }
            _ => return,
        }
        self.apply_now(FOOTER_ROW, "已恢复本页默认", window, cx);
    }

    // -----------------------------------------------------------------------
    // 诊断
    // -----------------------------------------------------------------------

    fn diagnostics_text(&self) -> String {
        let mut out = String::new();
        out.push_str("RDM 诊断信息\n");
        out.push_str(&format!("版本: {}\n", env!("CARGO_PKG_VERSION")));
        out.push_str(&format!(
            "构建: {}\n",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        ));
        out.push_str(&format!(
            "平台: {}-{}（当前仅验证 Windows）\n",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        out.push_str(&format!("状态目录: {}\n", self.state_dir.display()));
        out.push_str(&format!(
            "浏览器集成: {}\n",
            if self.browser_ok {
                "已连接"
            } else {
                "异常"
            }
        ));
        if let Some(warning) = &self.browser_warning {
            out.push_str(&format!("集成警告: {warning}\n"));
        }
        out.push_str(&format!("本机通道: {}\n", self.ipc_summary));
        let profile_label = self
            .ai_draft
            .active_profile()
            .map(|profile| profile.display_name().to_owned())
            .unwrap_or_else(|| self.ai_draft.resolved_provider().label_zh().to_owned());
        out.push_str(&format!(
            "AI 助手: {}（Provider={}；共 {} 个 Provider 档案；Base URL={} / 模型={}；API Key {}）\n",
            if self.ai_draft.enabled {
                "已启用"
            } else {
                "未启用"
            },
            profile_label,
            self.ai_draft.profiles.len(),
            if self.ai_draft.base_url.trim().is_empty() {
                "默认"
            } else {
                self.ai_draft.base_url.trim()
            },
            if self.ai_draft.model.trim().is_empty() {
                "默认"
            } else {
                self.ai_draft.model.trim()
            },
            if self.ai_draft.resolved_api_key().is_empty() {
                "未配置"
            } else {
                "已配置（值已隐藏）"
            },
        ));
        out.push_str(&format!(
            "默认连接数: {} / 全局限速: {} / 动态线程规划: {}\n",
            self.draft.default_connections,
            match self.draft.speed_limit_bps {
                Some(bps) => format!("{:.1} MB/s", bps as f64 / (1024.0 * 1024.0)),
                None => "不限速".to_string(),
            },
            if self.draft.dynamic_connections {
                "开"
            } else {
                "关"
            },
        ));
        out.push_str(&format!(
            "默认保存目录: {}\n",
            self.draft
                .default_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "系统默认".to_string())
        ));
        out.push_str(&format!(
            "主题: {} / 强调色: {} / 密度: {}\n",
            self.draft.resolved_ui_theme().label_zh(),
            self.draft.accent.label_zh(),
            self.draft.density.label_zh(),
        ));
        out.push_str("已脱敏：不含 IPC token 与 AI API Key。\n");
        out
    }

    fn copy_diagnostics(&mut self, cx: &mut Context<Self>) {
        let text = self.diagnostics_text();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    // -----------------------------------------------------------------------
    // 渲染：外框
    // -----------------------------------------------------------------------

    fn render_header(
        &mut self,
        t: &ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .h(px(theme::TITLEBAR_H))
            .flex_shrink_0()
            .px(px(16.0))
            .gap(px(12.0))
            .items_center()
            .bg(t.bg_surface)
            .border_b_1()
            .border_color(t.border_subtle)
            .child(
                div()
                    .text_size(px(FS_H2))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("RDM 设置"),
            )
            .child(div().flex_1())
            .child(
                Input::new(&self.search_input)
                    .small()
                    .w(px(260.0))
                    .cleanable(true)
                    .prefix(RdmIcon::Search.sized(14.0, t.text_tertiary)),
            )
            .child(icon_button(
                "settings-close",
                RdmIcon::Close,
                "关闭设置（Esc）",
                cx.listener(|this, _, _, cx| {
                    let _ = this.weak_app.update(cx, |app, cx| {
                        app.close_settings_page(cx);
                    });
                }),
            ))
            .into_any_element()
    }

    fn render_nav(&mut self, t: &ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let mut nav = v_flex()
            .w(px(NAV_W))
            .flex_none()
            .h_full()
            .py(px(8.0))
            .bg(t.bg_surface)
            .border_r_1()
            .border_color(t.border_subtle);

        for (index, section) in SettingsSection::ALL.iter().copied().enumerate() {
            let selected = self.section == section;
            let handle = self.nav_focus[index].clone();
            let foreground = if selected { t.accent } else { t.text_secondary };
            let show_warning = matches!(section, SettingsSection::Browser) && !self.browser_ok;
            nav = nav.child(
                div()
                    .id(("settings-nav", index))
                    .relative()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(34.0))
                    .mx(px(8.0))
                    .px(px(10.0))
                    .rounded(px(RADIUS_SM))
                    .bg(if selected {
                        t.accent_soft
                    } else {
                        t.bg_surface
                    })
                    .text_color(foreground)
                    .cursor_pointer()
                    .track_focus(&handle)
                    .tab_stop(true)
                    .focus(|style| style.border_1().border_color(t.focus))
                    .when(!selected, |this| {
                        this.hover(move |style| style.bg(t.bg_hover))
                    })
                    .when(selected, |this| {
                        this.child(
                            div()
                                .absolute()
                                .left(px(-8.0))
                                .top(px(8.0))
                                .h(px(18.0))
                                .w(px(2.0))
                                .rounded(px(1.0))
                                .bg(t.accent),
                        )
                    })
                    .child(section.icon().sized(16.0, foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(FS_BODY))
                            .child(section.label_zh()),
                    )
                    .when(show_warning, |this| {
                        this.child(div().size(px(6.0)).rounded_full().bg(t.warning))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.nav_focus[index].focus(window);
                        this.switch_section(section, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        this.nav_key(index, event, window, cx);
                    })),
            );
        }
        nav.into_any_element()
    }

    fn render_loading(&self, t: &ThemeTokens) -> AnyElement {
        v_flex()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .py(px(96.0))
            .child(
                div()
                    .text_size(px(FS_BODY))
                    .text_color(t.text_secondary)
                    .child("正在读取设置…"),
            )
            .into_any_element()
    }

    fn render_content(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.initialized {
            self.render_loading(t)
        } else if !self.search_query_is_empty() {
            self.render_search(t, window, cx)
        } else {
            self.render_section(t, window, cx)
        }
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let header = self.render_header(&t, window, cx);
        let nav = self.render_nav(&t, cx);
        let content = self.render_content(&t, window, cx);

        v_flex()
            .size_full()
            .bg(t.bg_app)
            .text_color(t.text_primary)
            .font(theme::ui_font())
            .key_context("SettingsPage")
            // 06 §1.5 层级 5：搜索框有内容时 Esc 先清空搜索，不关闭整页。
            .capture_action(cx.listener(
                |this, _: &crate::views::actions::CloseLayer, window, cx| {
                    if !this.search_query_is_empty() {
                        this.search_input
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        this.search_query.clear();
                        cx.stop_propagation();
                        cx.notify();
                    }
                },
            ))
            .child(header)
            .child(
                h_flex().flex_1().min_h(px(0.0)).child(nav).child(
                    div()
                        .id("settings-scroll")
                        .flex_1()
                        .min_w(px(0.0))
                        .h_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.content_scroll)
                        .child(
                            div().w_full().flex().child(
                                v_flex()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .max_w(px(CONTENT_MAX_W))
                                    .p(px(CONTENT_PAD))
                                    .child(content),
                            ),
                        ),
                ),
            )
    }
}

// ---------------------------------------------------------------------------
// 渲染：页面公共结构
// ---------------------------------------------------------------------------

impl SettingsPage {
    fn render_section(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.section {
            SettingsSection::General => self.page_general(t, window, cx),
            SettingsSection::Engine => self.page_engine(t, window, cx),
            SettingsSection::Storage => self.page_storage(t, window, cx),
            SettingsSection::Browser => self.page_browser(t, window, cx),
            SettingsSection::Ai => self.page_ai(t, window, cx),
            SettingsSection::Appearance => self.page_appearance(t, window, cx),
            SettingsSection::Shortcuts => self.page_shortcuts(t, window, cx),
            SettingsSection::Advanced => self.page_advanced(t, window, cx),
            SettingsSection::About => self.page_about(t, window, cx),
        }
    }

    fn page_body(
        &mut self,
        t: &ThemeTokens,
        title: &'static str,
        description: &'static str,
        rows: Vec<AnyElement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let footer = self.render_footer(t, window, cx);
        v_flex()
            .gap(px(2.0))
            .child(
                v_flex()
                    .gap(px(6.0))
                    .pb(px(12.0))
                    .child(
                        div()
                            .text_size(px(FS_H1))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(FS_CAPTION))
                            .text_color(t.text_secondary)
                            .child(description),
                    ),
            )
            .child(v_flex().children(rows))
            .child(footer)
            .into_any_element()
    }

    fn render_footer(
        &mut self,
        t: &ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut footer = h_flex()
            .pt(px(16.0))
            .pb(px(24.0))
            .items_center()
            .gap(px(12.0));
        if self.section.has_settings() {
            footer = footer.child(
                Button::new("settings-reset-page")
                    .text()
                    .small()
                    .label("恢复本页默认")
                    .text_color(t.text_secondary)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.reset_current_page(window, cx);
                    })),
            );
        } else {
            footer = footer.child(
                Button::new("settings-reset-page")
                    .text()
                    .small()
                    .label("恢复本页默认")
                    .disabled(true)
                    .text_color(t.text_disabled)
                    .tooltip("本页没有可恢复的设置"),
            );
        }
        if let Some(saved) = self.saved_for(FOOTER_ROW) {
            footer = footer.child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.success)
                    .child(saved),
            );
        }
        footer.into_any_element()
    }

    fn page_general(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();

        rows.push(settings_row(
            t,
            "general-auto-start",
            "自动开始",
            "添加下载后立即开始；关闭后新任务保持暂停。",
            self.switch_control(
                t,
                "general-auto-start-switch",
                self.draft.auto_start,
                window,
                cx,
                |this, window, cx| {
                    this.draft.auto_start = !this.draft.auto_start;
                    this.apply_now("general-auto-start", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("general-auto-start"),
            self.is_highlighted("general-auto-start"),
        ));

        rows.push(settings_row(
            t,
            "general-silent-start",
            "静默启动",
            "启动时不显示主窗口，仅驻留托盘；需托盘可用。",
            self.switch_control(
                t,
                "general-silent-start-switch",
                self.draft.silent_start,
                window,
                cx,
                |this, window, cx| {
                    this.draft.silent_start = !this.draft.silent_start;
                    this.apply_now("general-silent-start", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("general-silent-start"),
            self.is_highlighted("general-silent-start"),
        ));

        rows.push(settings_row(
            t,
            "general-minimize-to-tray",
            "关闭窗口时隐藏到托盘",
            "关闭窗口时隐藏到托盘；最小化仍到任务栏。",
            self.switch_control(
                t,
                "general-minimize-to-tray-switch",
                self.draft.minimize_to_tray,
                window,
                cx,
                |this, window, cx| {
                    this.draft.minimize_to_tray = !this.draft.minimize_to_tray;
                    this.apply_now("general-minimize-to-tray", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("general-minimize-to-tray"),
            self.is_highlighted("general-minimize-to-tray"),
        ));

        rows.push(settings_row(
            t,
            "general-launch-at-startup",
            "开机自启动",
            "登录 Windows 后自动启动；写入当前用户启动项。",
            self.switch_control(
                t,
                "general-launch-at-startup-switch",
                self.draft.launch_at_startup,
                window,
                cx,
                |this, window, cx| {
                    this.draft.launch_at_startup = !this.draft.launch_at_startup;
                    this.apply_now("general-launch-at-startup", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("general-launch-at-startup"),
            self.is_highlighted("general-launch-at-startup"),
        ));

        self.page_body(
            t,
            "通用",
            SettingsSection::General.description_zh(),
            rows,
            window,
            cx,
        )
    }

    fn page_engine(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();

        // 默认连接数：[−] 输入 [+] 个连接（B8 规格）。
        let connections = self.draft.default_connections;
        let connections_control = h_flex()
            .items_center()
            .gap(px(4.0))
            .child(
                Button::new("engine-connections-minus")
                    .ghost()
                    .compact()
                    .icon(RdmIcon::Minus.icon())
                    .tooltip("减少连接数")
                    .disabled(connections <= 1)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let next = this.draft.default_connections.saturating_sub(1).max(1);
                        if next != this.draft.default_connections {
                            this.draft.default_connections = next;
                            let text = next.to_string();
                            this.connections_input.update(cx, |state, cx| {
                                state.set_value(text, window, cx);
                            });
                            this.apply_now("engine-connections", "已保存", window, cx);
                        }
                    })),
            )
            .child(Input::new(&self.connections_input).small().w(px(64.0)))
            .child(
                Button::new("engine-connections-plus")
                    .ghost()
                    .compact()
                    .icon(RdmIcon::Plus.icon())
                    .tooltip("增加连接数")
                    .disabled(connections >= rdm_core::MAX_CONNECTIONS)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let next =
                            (this.draft.default_connections + 1).min(rdm_core::MAX_CONNECTIONS);
                        if next != this.draft.default_connections {
                            this.draft.default_connections = next;
                            let text = next.to_string();
                            this.connections_input.update(cx, |state, cx| {
                                state.set_value(text, window, cx);
                            });
                            this.apply_now("engine-connections", "已保存", window, cx);
                        }
                    })),
            )
            .child(
                div()
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_secondary)
                    .child("个连接"),
            )
            .into_any_element();

        rows.push(settings_row(
            t,
            "engine-connections",
            "默认连接数",
            "每个任务的并行连接上限；动态规划开启时自动调节。",
            connections_control,
            self.connections_error.as_deref(),
            self.saved_for("engine-connections"),
            self.is_highlighted("engine-connections"),
        ));

        let speed_control = h_flex()
            .items_center()
            .gap(px(6.0))
            .child(Input::new(&self.speed_input).small().w(px(110.0)))
            .child(
                div()
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_secondary)
                    .child("MB/s"),
            )
            .into_any_element();
        rows.push(settings_row(
            t,
            "engine-speed-limit",
            "全局限速",
            "全部任务共用的速度上限（MB/s）；留空表示不限速。",
            speed_control,
            self.speed_error.as_deref(),
            self.saved_for("engine-speed-limit"),
            self.is_highlighted("engine-speed-limit"),
        ));

        rows.push(settings_row(
            t,
            "engine-dynamic",
            "动态线程规划",
            "按速度自动调整连接数；关闭时使用固定连接数。",
            self.switch_control(
                t,
                "engine-dynamic-switch",
                self.draft.dynamic_connections,
                window,
                cx,
                |this, window, cx| {
                    this.draft.dynamic_connections = !this.draft.dynamic_connections;
                    this.apply_now("engine-dynamic", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("engine-dynamic"),
            self.is_highlighted("engine-dynamic"),
        ));

        self.page_body(
            t,
            "下载引擎",
            SettingsSection::Engine.description_zh(),
            rows,
            window,
            cx,
        )
    }

    fn page_storage(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();

        let dir_text = self
            .draft
            .default_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "系统默认下载目录".to_string());
        let dir_control = h_flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .max_w(px(168.0))
                    .truncate()
                    .font(theme::mono_font())
                    .text_size(px(FS_MONO_PATH))
                    .text_color(t.text_secondary)
                    .child(dir_text),
            )
            .child(
                Button::new("storage-browse")
                    .outline()
                    .small()
                    .label("浏览…")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.pick_default_dir(window, cx);
                    })),
            )
            .into_any_element();
        rows.push(settings_row(
            t,
            "storage-default-dir",
            "默认保存目录",
            "新任务的默认保存目录；下载中可单独修改。",
            dir_control,
            self.dir_error.as_deref(),
            self.saved_for("storage-default-dir"),
            self.is_highlighted("storage-default-dir"),
        ));

        rows.push(settings_row(
            t,
            "storage-save-by-extension",
            "按后缀分类",
            "按文件后缀归入视频 / 音频 / 图片 / 文档等子目录。",
            self.switch_control(
                t,
                "storage-save-by-extension-switch",
                self.draft.save_by_extension,
                window,
                cx,
                |this, window, cx| {
                    this.draft.save_by_extension = !this.draft.save_by_extension;
                    this.apply_now("storage-save-by-extension", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("storage-save-by-extension"),
            self.is_highlighted("storage-save-by-extension"),
        ));

        // 分类与后缀名：每个分类一行（图标 + 分类名 + 后缀输入 + 恢复默认），
        // 「其他」是兜底行，没有输入框（design 16 §4.4）。
        rows.push(crate::ui::widgets::section_label(t, "分类与后缀名").into_any_element());
        rows.push(
            div()
                .px(px(12.0))
                .pb(px(2.0))
                .text_size(px(FS_CAPTION))
                .text_color(t.text_secondary)
                .child("按后缀名分类保存；同一后缀只归属一个分类，未识别类型保存到根目录")
                .into_any_element(),
        );
        rows.push(
            div()
                .px(px(12.0))
                .pb(px(8.0))
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(
                    "多个后缀用逗号或空格分隔；修改后自动保存，侧栏筛选与下载目录立即使用新规则。",
                )
                .into_any_element(),
        );
        let rules = self.draft.classification.clone();
        for rule in rules {
            let Some((row_id, reset_id)) = classification_editor_ids(&rule.kind) else {
                // 「其他」兜底行：没有输入框，仅说明兜底行为。
                rows.push(settings_row(
                    t,
                    "storage-class-other",
                    "其他",
                    "未识别类型保存到根目录（兜底分类，不参与后缀匹配）。",
                    value_text(t, "根目录"),
                    None,
                    None,
                    self.is_highlighted("storage-class-other"),
                ));
                continue;
            };
            let Some(input) = self.classification_inputs.get(rule.kind.as_str()).cloned() else {
                continue;
            };
            let error = self.classification_errors.get(&rule.kind).cloned();
            let reset_kind = rule.kind.clone();
            let description = format!("保存到「{}」目录", rule.label);
            // 输入框给足宽度（500px，约占设置页内容区 80%），一次可见完整后缀列表；
            // 右侧「恢复默认」靠右对齐（2026-10-22 用户反馈：原先 180px 只看得到部分后缀）。
            let control = h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(Input::new(&input).small().w(px(500.0)).cleanable(true))
                .child(div().flex_1())
                .child(
                    Button::new(reset_id)
                        .ghost()
                        .small()
                        .label("恢复默认")
                        .tooltip("把该分类恢复为内置后缀；被其他分类占用的后缀会一并移回")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.reset_category_extensions(&reset_kind, window, cx);
                        })),
                )
                .into_any_element();
            rows.push(classification_row(
                t,
                row_id,
                crate::views::app::FileCategory::from_kind(&rule.kind).icon(),
                rule.label,
                description,
                control,
                error.as_deref(),
                self.saved_for(row_id),
                self.is_highlighted(row_id),
            ));
        }

        rows.push(settings_row(
            t,
            "storage-verify-sha256",
            "SHA-256 校验",
            "展示引擎返回的 SHA-256 校验值；不额外计算哈希。",
            self.switch_control(
                t,
                "storage-verify-sha256-switch",
                self.draft.verify_sha256,
                window,
                cx,
                |this, window, cx| {
                    this.draft.verify_sha256 = !this.draft.verify_sha256;
                    this.apply_now("storage-verify-sha256", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("storage-verify-sha256"),
            self.is_highlighted("storage-verify-sha256"),
        ));

        self.page_body(
            t,
            "存储与分类",
            SettingsSection::Storage.description_zh(),
            rows,
            window,
            cx,
        )
    }

    fn pick_default_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择默认保存目录".into()),
        });
        let task = cx.spawn_in(window, async move |weak, cx| {
            let result = receiver.await;
            let _ = weak.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        this.draft.default_dir = Some(path);
                        this.dir_error = None;
                        this.apply_now("storage-default-dir", "已保存", window, cx);
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    this.dir_error = Some(format!("无法打开目录选择器：{error}"));
                    cx.notify();
                }
                Err(_) => {
                    this.dir_error = Some("目录选择已取消或中断".to_string());
                    cx.notify();
                }
            });
        });
        task.detach();
    }

    fn page_browser(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ok = self.browser_ok;
        let (pill_icon, pill_label, pill_fg, pill_bg) = if ok {
            (RdmIcon::CheckCircle, "已连接", t.success, t.success_soft)
        } else {
            (RdmIcon::Alert, "异常", t.warning, t.warning_soft)
        };
        let mut card = v_flex()
            .w(px(CONTENT_INNER_W))
            .gap(px(8.0))
            .p(px(16.0))
            .rounded(px(RADIUS_MD))
            .bg(t.bg_surface)
            .border_1()
            .border_color(t.border_default)
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(RdmIcon::Globe.sized(16.0, t.info))
                    .child(
                        div()
                            .text_size(px(FS_H2))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("浏览器接管"),
                    )
                    .child(div().flex_1())
                    .child(status_pill(t, pill_icon, pill_label, pill_fg, pill_bg)),
            )
            .child(kv_row(
                t,
                "本机通道",
                if ok { "IPC 已就绪" } else { "IPC 未就绪" },
            ))
            .child(kv_row(
                t,
                "原生宿主",
                "标识 com.rdm.download（安装状态需在浏览器侧确认）",
            ))
            .child(kv_row(
                t,
                "Chrome / Firefox",
                "桌面端不探测浏览器状态；请在扩展管理页确认已安装并启用",
            ))
            .child(kv_row(t, "IPC 端点", self.ipc_summary.clone()))
            .child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_tertiary)
                    .child("Cookie 仅在解析请求中于本机进程间转发，不写入日志或磁盘。"),
            );

        if !ok {
            if let Some(warning) = &self.browser_warning {
                card = card.child(info_banner(
                    t,
                    RdmIcon::Alert,
                    Severity::Warning,
                    warning.clone(),
                    vec![Button::new("browser-open-state-dir-warning")
                        .outline()
                        .small()
                        .label("打开状态目录")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let _ = this.weak_app.update(cx, |app, cx| {
                                app.open_state_directory(cx);
                            });
                        }))
                        .into_any_element()],
                ));
            }
            card = card.child(
                div()
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_secondary)
                    .child(
                        "若状态异常：确认 RDM 正在运行、扩展已启用，然后重试；\
                         也可先打开状态目录检查 ipc.json 是否存在且未过期。",
                    ),
            );
        }

        let mut rows: Vec<AnyElement> = vec![
            settings_row(
                t,
                "browser-status",
                "浏览器接管状态",
                "本机 IPC 通道是否就绪；原生宿主与扩展的安装状态需在浏览器侧确认。",
                div()
                    .text_size(px(FS_CAPTION))
                    .text_color(if ok { t.success } else { t.warning })
                    .child(if ok {
                        "本机通道可用"
                    } else {
                        "本机通道异常"
                    })
                    .into_any_element(),
                None,
                None,
                self.is_highlighted("browser-status"),
            ),
            settings_row(
                t,
                "browser-state-dir",
                "状态目录",
                "ipc.json、日志与任务状态所在目录。",
                Button::new("browser-open-state-dir")
                    .outline()
                    .small()
                    .label("打开状态目录")
                    .tooltip("打开状态目录")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _ = this.weak_app.update(cx, |app, cx| {
                            app.open_state_directory(cx);
                        });
                    }))
                    .into_any_element(),
                None,
                None,
                self.is_highlighted("browser-state-dir"),
            ),
            settings_row(
                t,
                "browser-diagnostics",
                "诊断信息",
                "复制 IPC 端口、集成状态与设置摘要（已脱敏，不含 token / API Key）。",
                Button::new("browser-copy-diagnostics")
                    .outline()
                    .small()
                    .label("复制诊断信息")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.copy_diagnostics(cx);
                        this.mark_saved("browser-diagnostics", "已复制", window, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
                None,
                self.saved_for("browser-diagnostics"),
                self.is_highlighted("browser-diagnostics"),
            ),
        ];

        let mut body = v_flex()
            .gap(px(2.0))
            .child(
                v_flex()
                    .gap(px(6.0))
                    .pb(px(12.0))
                    .child(
                        div()
                            .text_size(px(FS_H1))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("浏览器集成"),
                    )
                    .child(
                        div()
                            .text_size(px(FS_CAPTION))
                            .text_color(t.text_secondary)
                            .child(SettingsSection::Browser.description_zh()),
                    ),
            )
            .child(div().pb(px(16.0)).child(card))
            .child(
                div()
                    .pb(px(4.0))
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_tertiary)
                    .child(
                        "桌面端无法直接探测原生宿主或浏览器扩展的安装状态；\
                         若浏览器没有把下载交给 RDM，请先在扩展管理页确认扩展已启用。",
                    ),
            )
            .child(v_flex().children(std::mem::take(&mut rows)));

        let footer = self.render_footer(t, window, cx);
        body = body.child(footer);
        body.into_any_element()
    }

    fn page_ai(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        let ai = self.ai_draft.clone();
        let custom = ai
            .active_profile()
            .map(|profile| !profile.is_builtin())
            .unwrap_or(false);

        let enabled_control = self.switch_control(
            t,
            "ai-enabled-switch",
            ai.enabled,
            window,
            cx,
            |this, window, cx| {
                let previous = this.ai_draft.enabled;
                this.ai_draft.enabled = !previous;
                if validate_ai_config(&this.ai_draft).is_err() {
                    this.ai_draft.enabled = previous;
                    cx.notify();
                    return;
                }
                this.ai_last_row = "ai-enabled";
                this.apply_ai_now("ai-enabled", "已保存", window, cx);
            },
        );
        rows.push(settings_row(
            t,
            "ai-enabled",
            "启用 AI 助手",
            "启用后用于解析页面资源与 AI 助手对话。",
            enabled_control,
            None,
            self.saved_for("ai-enabled"),
            self.is_highlighted("ai-enabled"),
        ));

        // Provider 档案：每份档案独立保存 Base URL / API Key / 模型。
        let profile_chips = self.ai_profile_chips(t, window, cx);
        rows.push(settings_stack_row(
            t,
            "ai-provider",
            "Provider 档案",
            "内置 DeepSeek / OpenAI / SiliconFlow 与自定义 Provider；切换时 API 地址、API Key 与模型一起切换，不再串用。",
            profile_chips,
            None,
            self.saved_for("ai-provider"),
            self.is_highlighted("ai-provider"),
        ));

        if custom {
            rows.push(settings_row(
                t,
                "ai-profile-name",
                "Provider 名称",
                "自定义 Provider 的显示名称；仅保存在本机。",
                Input::new(&self.ai_profile_name_input)
                    .small()
                    .w(px(240.0))
                    .into_any_element(),
                None,
                self.saved_for("ai-profile-name"),
                self.is_highlighted("ai-profile-name"),
            ));
        }

        let endpoint = ai.models_endpoint();
        rows.push(settings_row(
            t,
            "ai-base-url",
            "API 地址",
            format!("仅属于当前 Provider；模型列表来自 GET {endpoint}。"),
            Input::new(&self.ai_base_url_input)
                .small()
                .w(px(260.0))
                .into_any_element(),
            base_url_error(&ai).as_deref(),
            self.saved_for("ai-base-url"),
            self.is_highlighted("ai-base-url"),
        ));

        rows.push(settings_row(
            t,
            "ai-api-key",
            "API Key",
            "仅属于当前 Provider；保存在本机 ai.json，不写入日志。",
            Input::new(&self.ai_api_key_input)
                .small()
                .w(px(260.0))
                .mask_toggle()
                .into_any_element(),
            None,
            self.saved_for("ai-api-key"),
            self.is_highlighted("ai-api-key"),
        ));

        // 模型：实时拉取的可搜索滚动列表 + 手填 model id + 拉取按钮 + 状态行。
        let model_panel = self.ai_model_panel(t, window, cx);
        rows.push(settings_stack_row(
            t,
            "ai-model",
            "模型（实时拉取）",
            "列表来自当前 Provider 的 GET /models；可搜索选择，也可手填任意 model id（修改后自动保存）。",
            model_panel,
            model_error(&ai).as_deref(),
            self.saved_for("ai-model"),
            self.is_highlighted("ai-model"),
        ));

        // 价格与估算：随当前模型动态展示，无价目时明确写「无价目估算」。
        let pricing_content = self.ai_pricing_content(t);
        rows.push(settings_stack_row(
            t,
            "ai-pricing",
            "价格与估算",
            "内置价目快照只用于估算展示，不代表真实账单；自定义 Provider/未知模型显示「无价目估算」。",
            pricing_content,
            None,
            None,
            self.is_highlighted("ai-pricing"),
        ));

        if custom {
            rows.push(settings_row(
                t,
                "ai-provider-delete",
                "删除该 Provider",
                "仅删除当前自定义 Provider 的档案（含其 API Key 与模型列表）；内置 Provider 不可删除。",
                Button::new("ai-delete-profile")
                    .danger()
                    .small()
                    .label("删除该 Provider")
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.delete_active_custom_provider(window, cx)
                        }),
                    )
                    .into_any_element(),
                None,
                None,
                self.is_highlighted("ai-provider-delete"),
            ));
        }

        rows.push(settings_row(
            t,
            "ai-privacy",
            "隐私与日志",
            "请求只发送到你配置的服务；不内置代理、不收遥测。",
            value_text(t, "仅本机保存"),
            None,
            None,
            self.is_highlighted("ai-privacy"),
        ));

        let mut body = v_flex().gap(px(2.0)).child(
            v_flex()
                .gap(px(6.0))
                .pb(px(12.0))
                .child(
                    div()
                        .text_size(px(FS_H1))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("AI 助手"),
                )
                .child(
                    div()
                        .text_size(px(FS_CAPTION))
                        .text_color(t.text_secondary)
                        .child(SettingsSection::Ai.description_zh()),
                ),
        );
        if !ai.is_configured() {
            body = body.child(div().pb(px(12.0)).child(info_banner(
                t,
                RdmIcon::Info,
                Severity::Info,
                "AI 助手未配置：解析页面资源将使用启发式规则；选择 Provider、填写 API Key 与模型后立即生效。",
                vec![],
            )));
        }
        body = body.child(v_flex().children(rows));
        let footer = self.render_footer(t, window, cx);
        body.child(footer).into_any_element()
    }

    fn page_appearance(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();

        let theme_index = UiThemeMode::ALL
            .iter()
            .position(|mode| *mode == self.draft.resolved_ui_theme())
            .unwrap_or(0);
        let theme_group = RadioGroup::horizontal("appearance-theme")
            .selected_index(Some(theme_index))
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                this.draft.ui_theme = Some(UiThemeMode::ALL[*index]);
                this.apply_now("appearance-theme", "已保存", window, cx);
            }))
            .child(Radio::new("appearance-theme-follow").label("跟随系统"))
            .child(Radio::new("appearance-theme-light").label("浅色"))
            .child(Radio::new("appearance-theme-dark").label("深色"));
        rows.push(settings_row(
            t,
            "appearance-theme",
            "主题模式",
            "跟随系统，或强制使用浅色 / 深色。",
            theme_group.into_any_element(),
            None,
            self.saved_for("appearance-theme"),
            self.is_highlighted("appearance-theme"),
        ));

        // 强调色 5 预设：色块 + 名称，键盘 ←/→ 切换、Enter/Space 应用。
        let dark = cx.theme().mode.is_dark();
        let mut swatches = h_flex().gap(px(4.0)).items_center();
        for (index, preset) in AccentPreset::ALL.iter().copied().enumerate() {
            let selected = self.draft.accent == preset;
            let handle = self.accent_focus[index].clone();
            let color = if dark {
                theme::dark(preset).accent
            } else {
                theme::light(preset).accent
            };
            let label_color = if selected {
                t.text_primary
            } else {
                t.text_secondary
            };
            swatches = swatches.child(
                div()
                    .id(("settings-accent", index))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .p(px(6.0))
                    .rounded(px(RADIUS_SM))
                    .cursor_pointer()
                    .track_focus(&handle)
                    .tab_stop(true)
                    .focus(|style| style.border_1().border_color(t.focus))
                    .when(selected, |this| this.bg(t.accent_soft))
                    .hover(move |style| style.bg(t.bg_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.accent_focus[index].focus(window);
                        this.draft.accent = preset;
                        this.apply_now("appearance-accent", "已保存", window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        this.accent_key(index, event, window, cx);
                    }))
                    .child(
                        div()
                            .size(px(20.0))
                            .rounded_full()
                            .bg(color)
                            .border_2()
                            .border_color(if selected {
                                t.text_primary
                            } else {
                                t.border_default
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(FS_CAPTION))
                            .text_color(label_color)
                            .child(preset.label_zh()),
                    ),
            );
        }
        rows.push(settings_row(
            t,
            "appearance-accent",
            "强调色",
            "只影响按钮、进度条、选中态等强调元素。",
            swatches.into_any_element(),
            None,
            self.saved_for("appearance-accent"),
            self.is_highlighted("appearance-accent"),
        ));

        let density = self.draft.density;
        let density_index = if density == Density::Comfortable {
            0
        } else {
            1
        };
        let density_group = RadioGroup::horizontal("appearance-density")
            .selected_index(Some(density_index))
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                this.draft.density = if *index == 0 {
                    Density::Comfortable
                } else {
                    Density::Compact
                };
                this.apply_now("appearance-density", "已保存", window, cx);
            }))
            .child(Radio::new("appearance-density-comfortable").label("舒适"))
            .child(Radio::new("appearance-density-compact").label("紧凑"));
        rows.push(settings_row(
            t,
            "appearance-density",
            "密度",
            "列表行高：舒适 64px / 紧凑 40px。",
            density_group.into_any_element(),
            None,
            self.saved_for("appearance-density"),
            self.is_highlighted("appearance-density"),
        ));

        rows.push(settings_row(
            t,
            "appearance-reduce-motion",
            "减少动效",
            "关闭过渡动画；进度条更新保留。",
            self.switch_control(
                t,
                "appearance-reduce-motion-switch",
                self.draft.reduce_motion,
                window,
                cx,
                |this, window, cx| {
                    this.draft.reduce_motion = !this.draft.reduce_motion;
                    this.apply_now("appearance-reduce-motion", "已保存", window, cx);
                },
            ),
            None,
            self.saved_for("appearance-reduce-motion"),
            self.is_highlighted("appearance-reduce-motion"),
        ));

        self.page_body(
            t,
            "外观",
            SettingsSection::Appearance.description_zh(),
            rows,
            window,
            cx,
        )
    }

    fn page_shortcuts(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let global_rows: Vec<AnyElement> = vec![
            shortcut_row(t, &["Ctrl+K", "Ctrl+Shift+P"], "命令面板"),
            shortcut_row(t, &["Ctrl+N"], "添加下载"),
            shortcut_row(
                t,
                &["Ctrl+Shift+D"],
                "快速添加（全局热键；仅在启用托盘驻留时注册）",
            ),
            shortcut_row(t, &["Ctrl+L", "/"], "聚焦万能输入栏"),
            shortcut_row(t, &["Ctrl+,"], "打开设置"),
            shortcut_row(t, &["Ctrl+B"], "折叠/展开侧栏"),
            shortcut_row(t, &["Ctrl+Shift+V"], "从剪贴板添加（先预览确认）"),
            shortcut_row(t, &["Ctrl+Shift+O"], "打开默认保存目录"),
            shortcut_row(
                t,
                &["Ctrl+1..5"],
                "切换智能视图（全部/下载中/已完成/失败/已暂停）",
            ),
            shortcut_row(t, &["Esc"], "分层退出（关闭浮层 / 清除选择 / 停止搜索）"),
            shortcut_row(t, &["F1"], "快捷键帮助（命令面板内）"),
        ];
        let list_rows: Vec<AnyElement> = vec![
            shortcut_row(t, &["Ctrl+F"], "搜索任务"),
            shortcut_row(t, &["↑", "↓"], "移动选中行"),
            shortcut_row(t, &["Shift+↑", "Shift+↓"], "扩展选择"),
            shortcut_row(t, &["PageUp", "PageDown"], "翻页"),
            shortcut_row(t, &["Home", "End"], "首/末行"),
            shortcut_row(t, &["Ctrl+A"], "全选当前视图"),
            shortcut_row(t, &["Space"], "暂停 / 继续选中项"),
            shortcut_row(t, &["Enter"], "打开文件（已完成；详情面板常驻右侧）"),
            shortcut_row(t, &["Ctrl+Enter"], "打开所在文件夹"),
            shortcut_row(t, &["Delete"], "移除记录（可撤销）"),
            shortcut_row(t, &["Shift+Delete"], "删除文件…（确认）"),
            shortcut_row(t, &["Ctrl+C"], "复制链接"),
            shortcut_row(t, &["Ctrl+Shift+C"], "复制文件路径"),
            shortcut_row(t, &["Shift+F10"], "打开当前行右键菜单"),
        ];

        let mut rows: Vec<AnyElement> = Vec::new();
        rows.push(crate::ui::widgets::section_label(t, "全局").into_any_element());
        rows.extend(global_rows);
        rows.push(crate::ui::widgets::section_label(t, "列表").into_any_element());
        rows.extend(list_rows);
        rows.push(
            div()
                .pt(px(12.0))
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("只读：当前版本不支持自定义快捷键；Ctrl+Shift+D 系统级全局热键尚未实现，因此不在此列出。")
                .into_any_element(),
        );

        self.page_body(
            t,
            "快捷键",
            SettingsSection::Shortcuts.description_zh(),
            rows,
            window,
            cx,
        )
    }

    fn page_advanced(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();

        let state_dir = self.state_dir.clone();
        let state_dir_text = state_dir.display().to_string();
        rows.push(settings_row(
            t,
            "advanced-state-dir",
            "状态目录",
            state_dir_text.clone(),
            Button::new("advanced-open-state-dir")
                .outline()
                .small()
                .label("打开状态目录")
                .on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.weak_app.update(cx, |app, cx| {
                        app.open_state_directory(cx);
                    });
                }))
                .into_any_element(),
            None,
            None,
            self.is_highlighted("advanced-state-dir"),
        ));

        rows.push(settings_row(
            t,
            "advanced-diagnostics",
            "诊断信息",
            "复制版本、状态目录、IPC 端口、浏览器集成与设置摘要（已脱敏，不含 token / API Key）。",
            Button::new("advanced-copy-diagnostics")
                .outline()
                .small()
                .label("复制诊断信息")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.copy_diagnostics(cx);
                    this.mark_saved("advanced-diagnostics", "已复制", window, cx);
                    cx.notify();
                }))
                .into_any_element(),
            None,
            self.saved_for("advanced-diagnostics"),
            self.is_highlighted("advanced-diagnostics"),
        ));

        let danger = v_flex()
            .gap(px(8.0))
            .p(px(16.0))
            .mt(px(16.0))
            .rounded(px(RADIUS_MD))
            .bg(t.danger_soft)
            .border_1()
            .border_color(t.danger)
            .child(
                h_flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(RdmIcon::Alert.sized(16.0, t.danger))
                    .child(
                        div()
                            .text_size(px(FS_BODY))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.danger_text)
                            .child("危险区"),
                    ),
            )
            .child(
                div()
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_secondary)
                    .child(
                        "恢复所有设置默认会清除外观、下载引擎、存储与 AI 配置（含 API Key）；\
                         已保存的下载任务不受影响，此操作不可撤销。",
                    ),
            )
            .child(
                h_flex().child(
                    Button::new("advanced-reset-all")
                        .danger()
                        .small()
                        .label("恢复所有设置默认")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_reset_all_dialog(window, cx);
                        })),
                ),
            );

        let body = v_flex()
            .gap(px(2.0))
            .child(
                v_flex()
                    .gap(px(6.0))
                    .pb(px(12.0))
                    .child(
                        div()
                            .text_size(px(FS_H1))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("高级"),
                    )
                    .child(
                        div()
                            .text_size(px(FS_CAPTION))
                            .text_color(t.text_secondary)
                            .child(SettingsSection::Advanced.description_zh()),
                    ),
            )
            .child(v_flex().children(rows))
            .child(danger.into_any_element());
        let footer = self.render_footer(t, window, cx);
        body.child(footer).into_any_element()
    }

    fn open_reset_all_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak_app = self.weak_app.clone();
        let weak_page = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let weak_app = weak_app.clone();
            let weak_page = weak_page.clone();
            dialog
                .title("恢复所有设置默认？")
                .width(px(460.0))
                .child(
                    "将把外观、下载引擎、存储与 AI 配置（含 API Key）恢复为出厂默认。\
                     已保存的下载任务不受影响，此操作不可撤销。",
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("恢复默认")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let defaults = Settings::default();
                    if let Some(app) = weak_app.upgrade() {
                        let draft = defaults.clone();
                        app.update(cx, |app, app_cx| app.apply_settings(draft, window, app_cx));
                        app.update(cx, |app, app_cx| {
                            app.apply_ai_config(AiConfig::default(), app_cx)
                        });
                    }
                    if let Some(page) = weak_page.upgrade() {
                        page.update(cx, |this, page_cx| {
                            this.draft = defaults;
                            this.ai_draft = AiConfig::default();
                            this.ai_applied = this.ai_draft.clone();
                            this.ai_models_fetched.clear();
                            this.ai_models_fetch = None;
                            this.ai_models_status = AiModelsStatus::default();
                            this.ai_model_filter.clear();
                            this.connections_error = None;
                            this.speed_error = None;
                            this.dir_error = None;
                            this.sync_engine_inputs(window, page_cx);
                            this.sync_ai_inputs(window, page_cx);
                            this.sync_classification_inputs(window, page_cx);
                            this.saved = Some(SavedNote {
                                row: FOOTER_ROW,
                                text: "已恢复所有设置默认",
                            });
                            page_cx.notify();
                        });
                    }
                    true
                })
        });
    }

    fn page_about(
        &mut self,
        t: &ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let build = format!(
            "{} · {}-{}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let rows: Vec<AnyElement> = vec![
            settings_row(
                t,
                "about-version",
                "版本",
                "当前构建的版本号。",
                mono_text(env!("CARGO_PKG_VERSION"), FS_MONO, t.text_secondary).into_any_element(),
                None,
                None,
                self.is_highlighted("about-version"),
            ),
            settings_row(
                t,
                "about-product",
                "产品",
                "本机下载任务、浏览器接管与页面资源解析。",
                value_text(t, "RDM（下载中枢）"),
                None,
                None,
                self.is_highlighted("about-product"),
            ),
            settings_row(
                t,
                "about-license",
                "开源许可",
                "以 MIT OR Apache-2.0 双许可发布。",
                value_text(t, "MIT OR Apache-2.0"),
                None,
                None,
                self.is_highlighted("about-license"),
            ),
            settings_row(
                t,
                "about-design",
                "设计文档",
                "设计包位于仓库 docs/design/。",
                value_text(t, "docs/design/"),
                None,
                None,
                self.is_highlighted("about-design"),
            ),
            settings_row(
                t,
                "about-build",
                "构建",
                "仅展示本机构建信息；当前只在 Windows 验证。",
                mono_text(build, FS_MONO, t.text_secondary).into_any_element(),
                None,
                None,
                self.is_highlighted("about-build"),
            ),
        ];

        self.page_body(
            t,
            "关于",
            SettingsSection::About.description_zh(),
            rows,
            window,
            cx,
        )
    }

    fn render_search(
        &mut self,
        t: &ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hits = search_hits(&self.search_query);
        if hits.is_empty() {
            return crate::ui::widgets::empty_state(
                t,
                RdmIcon::Search,
                "没有找到设置",
                "试试「连接数」「主题」「快捷键」等关键词。",
                vec![],
            )
            .into_any_element();
        }

        let mut rows = v_flex().gap(px(2.0)).child(
            div()
                .pb(px(8.0))
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(format!("找到 {} 项设置", hits.len())),
        );
        for (index, entry) in hits.iter().enumerate() {
            let section = entry.section;
            let row = entry.row;
            rows = rows.child(
                div()
                    .id(("settings-search-hit", index))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(RADIUS_SM))
                    .cursor_pointer()
                    .hover(move |style| style.bg(t.bg_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.jump_to(section, row, window, cx);
                    }))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .text_size(px(FS_MICRO))
                                    .text_color(t.text_tertiary)
                                    .child(format!("{} ›", section.label_zh())),
                            )
                            .child(
                                div()
                                    .text_size(px(FS_BODY))
                                    .text_color(t.text_primary)
                                    .child(entry.title),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(FS_CAPTION))
                            .text_color(t.text_secondary)
                            .child(entry.description),
                    ),
            );
        }

        v_flex()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(px(FS_H1))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("搜索结果"),
            )
            .child(rows)
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// 单元测试（纯逻辑：搜索索引与 AI 校验）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rdm_ai::CATEGORY_KINDS;

    // -- AI Base URL 校验（审计 2026-10-22 F-19）---------------------------

    #[test]
    fn ai_base_url_accepts_http_https_and_uppercase_scheme() {
        for raw in [
            "",
            "   ",
            "https://api.example.com/v1",
            "HTTP://api.example.com/v1",
            "http://127.0.0.1:8080/v1",
        ] {
            assert!(
                validate_ai_base_url(raw).is_ok(),
                "{raw:?} should be accepted"
            );
        }
    }

    #[test]
    fn ai_base_url_rejects_malformed_or_unsupported_urls() {
        for raw in [
            "http:// bad",
            "ftp://x",
            "http://",
            "not a url",
            "api.example.com/v1",
        ] {
            assert!(
                validate_ai_base_url(raw).is_err(),
                "{raw:?} should be rejected"
            );
        }
        // 错误文案必须可执行（包含示例），而不是只说“必须以…开头”。
        let err = validate_ai_base_url("ftp://x").unwrap_err();
        assert!(err.contains("http(s)"), "err={err}");
        assert!(err.contains("示例"), "err={err}");
    }

    #[test]
    fn ai_config_validation_requires_model_only_when_enabled() {
        let mut config = AiConfig {
            base_url: "HTTP://api.example.com/v1".to_string(),
            model: String::new(),
            enabled: false,
            ..Default::default()
        };
        assert!(validate_ai_config(&config).is_ok());
        config.enabled = true;
        assert!(validate_ai_config(&config).is_err());
        config.model = "deepseek-chat".to_string();
        assert!(validate_ai_config(&config).is_ok());
        config.base_url = "ftp://x".to_string();
        assert!(validate_ai_config(&config).is_err());
    }

    #[test]
    fn base_url_error_uses_the_same_validation() {
        let mut config = AiConfig::default();
        assert!(base_url_error(&config).is_none());
        config.base_url = "HTTP://api.example.com/v1".to_string();
        assert!(base_url_error(&config).is_none());
        config.base_url = "http:// bad".to_string();
        assert!(base_url_error(&config).is_some());
    }

    #[test]
    fn catalog_row_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for entry in CATALOG {
            assert!(
                seen.insert(entry.row),
                "duplicated settings row id: {}",
                entry.row
            );
        }
    }

    #[test]
    fn catalog_covers_every_section() {
        for section in SettingsSection::ALL {
            assert!(
                CATALOG.iter().any(|entry| entry.section == section),
                "section without catalog rows: {}",
                section.label_zh()
            );
        }
    }

    // -- 分类与后缀名（design 16 §4.4）--------------------------------------

    #[test]
    fn classification_editors_cover_every_non_other_kind_in_order() {
        let expected: Vec<&str> = CATEGORY_KINDS
            .iter()
            .copied()
            .filter(|kind| *kind != "other")
            .collect();
        let actual: Vec<&str> = CLASSIFICATION_EDITORS
            .iter()
            .map(|(kind, _, _)| *kind)
            .collect();
        assert_eq!(actual, expected);
        assert!(classification_editor_ids("other").is_none());
        assert!(classification_editor_ids("program").is_some());
        assert!(classification_editor_ids("unknown").is_none());
    }

    #[test]
    fn extension_input_normalizes_separators_case_and_duplicates() {
        assert_eq!(
            parse_extension_input("ZIP, .Rar，7z").unwrap(),
            vec!["zip", "rar", "7z"]
        );
        // 空格 / 制表符 / 换行同样作为分隔符。
        assert_eq!(
            parse_extension_input("mp4  mkv\tts\nwebm").unwrap(),
            vec!["mp4", "mkv", "ts", "webm"]
        );
        // 去重保序（大小写不敏感）。
        assert_eq!(
            parse_extension_input(".MP4, mp4, Mp4, mkv").unwrap(),
            vec!["mp4", "mkv"]
        );
        assert_eq!(parse_extension_input("zip,,").unwrap(), vec!["zip"]);
        assert_eq!(
            extension_list_text(&parse_extension_input("zip, rar").unwrap()),
            "zip, rar"
        );
    }

    #[test]
    fn extension_input_rejects_empty_illegal_or_too_many() {
        for raw in [
            "",
            "   ",
            " , ， ",
            "mp4, bad-char",
            "mp4, 中文",
            "mp4, bad/slash",
        ] {
            assert!(parse_extension_input(raw).is_err(), "{raw:?} 应当被拒绝");
        }
        let long = "a".repeat(MAX_EXTENSION_CHARS + 1);
        assert!(parse_extension_input(&long).is_err());
        assert!(
            parse_extension_input(&format!("{long}, mp4")).is_err(),
            "任一非法后缀都整体拒绝"
        );
        let too_many = (0..=MAX_EXTENSIONS_PER_CATEGORY)
            .map(|index| format!("x{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(parse_extension_input(&too_many).is_err());
        let max_allowed = (0..MAX_EXTENSIONS_PER_CATEGORY)
            .map(|index| format!("x{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            parse_extension_input(&max_allowed).unwrap().len(),
            MAX_EXTENSIONS_PER_CATEGORY
        );
    }

    #[test]
    fn extension_conflict_reports_the_owner_category() {
        let rules = default_category_rules();
        assert_eq!(
            extension_conflict("video", &["zip".to_string()], &rules),
            Some(("zip".to_string(), "压缩包".to_string()))
        );
        // 自己已拥有的后缀不算冲突；「其他」没有后缀，永不冲突。
        assert_eq!(
            extension_conflict("archive", &["zip".to_string()], &rules),
            None
        );
        assert_eq!(
            extension_conflict("other", &["zzz".to_string()], &rules),
            None
        );
        // 新后缀不与任何分类冲突。
        assert_eq!(
            extension_conflict("video", &["zzz".to_string()], &rules),
            None
        );
    }

    #[test]
    fn restoring_a_category_moves_suffixes_back_from_other_categories() {
        let mut rules = default_category_rules();
        // 模拟用户把 `iso` 从压缩包移到了程序安装包。
        rules
            .iter_mut()
            .find(|rule| rule.kind == "archive")
            .expect("archive")
            .extensions
            .retain(|extension| extension != "iso");
        rules
            .iter_mut()
            .find(|rule| rule.kind == "program")
            .expect("program")
            .extensions
            .push("iso".to_string());

        let moved = restore_default_extensions(&mut rules, "archive").expect("archive 是内置分类");
        assert_eq!(moved, 1, "从程序安装包移回 1 个后缀");
        assert!(rules
            .iter()
            .find(|rule| rule.kind == "archive")
            .expect("archive")
            .extensions
            .contains(&"iso".to_string()));
        assert!(!rules
            .iter()
            .find(|rule| rule.kind == "program")
            .expect("program")
            .extensions
            .contains(&"iso".to_string()));
        assert!(restore_default_extensions(&mut rules, "nope").is_none());
        // 幂等：再恢复一次没有可移动的后缀。
        assert_eq!(restore_default_extensions(&mut rules, "archive"), Some(0));
    }

    #[test]
    fn catalog_lists_classification_suffix_rows() {
        for (_, row, _) in CLASSIFICATION_EDITORS {
            assert!(
                CATALOG.iter().any(|entry| entry.row == row),
                "catalog 缺少分类行 {row}"
            );
        }
        assert!(CATALOG
            .iter()
            .any(|entry| entry.row == "storage-class-other"));
        assert!(search_hits("后缀")
            .iter()
            .any(|entry| entry.row == "storage-class-video"));
        assert!(search_hits("压缩包")
            .iter()
            .any(|entry| entry.row == "storage-class-archive"));
        // 「AI 分类」已完全移除：没有开关条目，也没有相关文案。
        assert!(CATALOG.iter().all(|entry| !entry.title.contains("AI 分类")));
    }

    #[test]
    fn search_filters_by_title_section_and_description() {
        assert!(search_hits("").is_empty());
        assert!(search_hits("   ").is_empty());
        assert!(search_hits("连接数")
            .iter()
            .any(|entry| entry.row == "engine-connections"));
        assert!(search_hits("外观")
            .iter()
            .all(|entry| entry.section == SettingsSection::Appearance));
        assert!(search_hits("不存在的关键词").is_empty());
    }

    #[test]
    fn section_has_settings_matches_expectations() {
        assert!(SettingsSection::General.has_settings());
        assert!(SettingsSection::Ai.has_settings());
        assert!(!SettingsSection::Shortcuts.has_settings());
        assert!(!SettingsSection::About.has_settings());
    }

    #[test]
    fn ai_validation_accepts_defaults_and_rejects_bad_url() {
        let mut config = AiConfig::default();
        assert!(validate_ai_config(&config).is_ok());

        config.base_url = "api.example.com/v1".to_string();
        assert!(base_url_error(&config).is_some());
        assert!(validate_ai_config(&config).is_err());

        config.base_url = "https://api.example.com/v1".to_string();
        config.enabled = true;
        config.model = "  ".to_string();
        assert!(model_error(&config).is_some());
        assert!(validate_ai_config(&config).is_err());

        config.model = "deepseek-flash".to_string();
        assert!(validate_ai_config(&config).is_ok());
    }

    #[test]
    fn catalog_includes_provider_and_pricing_rows() {
        assert!(search_hits("Provider")
            .iter()
            .any(|entry| entry.row == "ai-provider"));
        assert!(search_hits("DeepSeek")
            .iter()
            .any(|entry| entry.row == "ai-provider"));
        assert!(search_hits("模型")
            .iter()
            .any(|entry| entry.row == "ai-model"));
        assert!(search_hits("价格")
            .iter()
            .any(|entry| entry.row == "ai-pricing"));
        assert!(search_hits("峰谷")
            .iter()
            .any(|entry| entry.row == "ai-pricing"));
    }

    #[test]
    fn model_filter_is_case_insensitive_and_keeps_provider_order() {
        let models: Vec<String> = ["z-model", "DeepSeek-Flash", "qwen-72b"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(filter_model_ids(&models, "").len(), 3);
        assert_eq!(filter_model_ids(&models, "   ").len(), 3);
        let deepseek = filter_model_ids(&models, "deepseek");
        assert_eq!(deepseek, [&models[1]]);
        assert_eq!(filter_model_ids(&models, "QWEN"), [&models[2]]);
        assert!(filter_model_ids(&models, "no-such-model").is_empty());
    }

    #[test]
    fn clock_label_uses_beijing_time() {
        assert_eq!(clock_hhmm_zh(0), "08:00");
        assert_eq!(clock_hhmm_zh(16 * 60 * 60 * 1000), "00:00");
        assert_eq!(clock_hhmm_zh(10 * 60 * 1000 + 30_000), "08:10");
    }

    #[test]
    fn all_builtin_profiles_are_seeded_from_presets() {
        let config = AiConfig::default();
        let profiles = config.profiles();
        assert_eq!(profiles.len(), 3);
        for profile in profiles {
            assert!(profile.is_builtin(), "{}", profile.id);
            assert!(profile.base_url.starts_with("http"));
            assert!(!profile.model.is_empty());
            assert!(!profile.models.is_empty());
            assert!(profile.kind.is_some());
        }
        assert!(!deepseek_peak_note_zh().is_empty());
        assert!(deepseek_peak_note_zh().contains("高峰"));
        assert!(!price_label_zh(
            find_model(ProviderKind::DeepSeek, "deepseek-flash").expect("catalog price")
        )
        .is_empty());
        assert!(!PRICE_CATALOG_DATE.is_empty());
    }

    #[test]
    fn deepseek_peak_note_matches_off_peak_helper() {
        // 不依赖本机时区：只校验两个 API 都可用且文案完整。
        let _ = deepseek_is_off_peak(now_ms());
        let note = deepseek_peak_note_zh();
        assert!(note.contains("09:00"), "{note}");
        assert!(note.contains("18:00"), "{note}");
    }

    #[test]
    fn ipc_summary_never_exposes_token() {
        let temp = std::env::temp_dir().join(format!(
            "rdm-settings-ipc-summary-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&temp);
        let path = temp.join("ipc.json");
        std::fs::write(
            &path,
            r#"{"protocol":1,"port":4321,"token":"super-secret-token","pid":42,"app_version":"0.1.0"}"#,
        )
        .unwrap();
        let summary = ipc_summary(&temp);
        assert!(summary.contains("port=4321"));
        assert!(summary.contains("pid=42"));
        assert!(!summary.contains("super-secret-token"));
        let _ = std::fs::remove_dir_all(&temp);
    }
}
