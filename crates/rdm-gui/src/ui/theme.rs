//! 新版 UI 的语义设计 token（docs/design/04-visual-system.md §2）。
//!
//! 纪律：视图层只能引用本模块的语义 token（`tokens(cx).text_primary` 等），
//! **不得**直接写 `rgb(0x…)` / `rgba(0x…)` 字面量（契约 §8.2）。
//!
//! 两套主题共享同一组字段，只替换色值；强调色预设只替换 accent 组。

use gpui::{px, rgb, rgba, Font, FontFallbacks, Hsla, Rgba};
use gpui_component::{Theme, ThemeMode};

use crate::model::{AccentPreset, Density, UiThemeMode};

// ---------------------------------------------------------------------------
// 尺寸 / 排版常量（04 §5、契约 §6.1）
// ---------------------------------------------------------------------------

/// 标题/工具栏高度。
pub const TITLEBAR_H: f32 = 48.0;
/// 底部状态条高度。
pub const STATUSBAR_H: f32 = 28.0;
/// 左侧栏宽度（展开）。
pub const SIDEBAR_W: f32 = 216.0;
/// 左侧栏折叠宽度（仅图标）。
pub const SIDEBAR_COLLAPSED_W: f32 = 56.0;
/// 右侧检查器宽度。
pub const INSPECTOR_W: f32 = 320.0;
/// 列表头高度（含搜索/排序工具栏）。
pub const LIST_HEADER_H: f32 = 44.0;
/// 批量操作条高度。
pub const BULK_BAR_H: f32 = 44.0;
/// 列表行高：舒适 / 紧凑。
pub const ROW_COMFORTABLE: f32 = 64.0;
pub const ROW_COMPACT: f32 = 40.0;
/// 窗口宽度低于该值时自动紧凑（侧栏折叠 + 紧凑行）。
pub const COMPACT_BREAKPOINT: f32 = 1100.0;
/// 最小窗口尺寸（契约 §6.1）。
pub const MIN_WINDOW_W: f32 = 960.0;
pub const MIN_WINDOW_H: f32 = 600.0;

pub fn row_height(density: Density) -> f32 {
    density.row_height()
}

// 字号（04 §4.2）：只允许这 11 档。
pub const FS_DISPLAY: f32 = 24.0;
pub const FS_H1: f32 = 20.0;
pub const FS_H2: f32 = 16.0;
pub const FS_ROW_TITLE: f32 = 14.0;
pub const FS_BODY: f32 = 13.0;
pub const FS_CAPTION: f32 = 12.0;
pub const FS_MICRO: f32 = 11.0;
pub const FS_MONO: f32 = 12.5;
pub const FS_MONO_MICRO: f32 = 10.5;
pub const FS_MONO_PATH: f32 = 11.0;

// 圆角（04 §5.2）。
pub const RADIUS_XS: f32 = 4.0;
pub const RADIUS_SM: f32 = 6.0;
pub const RADIUS_MD: f32 = 8.0;
pub const RADIUS_LG: f32 = 10.0;
pub const RADIUS_XL: f32 = 16.0;

// ---------------------------------------------------------------------------
// 字体
// ---------------------------------------------------------------------------

/// UI 正文字体栈：Segoe UI Variable Text + 显式 CJK fallback（gpui.md §4.2）。
pub fn ui_font() -> Font {
    let mut font = gpui::font("Segoe UI Variable Text");
    font.fallbacks = Some(FontFallbacks::from_fonts(vec![
        "Segoe UI".to_string(),
        "Microsoft YaHei UI".to_string(),
        "Segoe UI Symbol".to_string(),
        "Segoe UI Emoji".to_string(),
    ]));
    font
}

/// 数字/等宽字体：速度、大小、ETA、哈希、路径、快捷键。
pub fn mono_font() -> Font {
    let mut font = gpui::font("Cascadia Mono");
    font.fallbacks = Some(FontFallbacks::from_fonts(vec![
        "Consolas".to_string(),
        "Microsoft YaHei UI".to_string(),
    ]));
    font
}

// ---------------------------------------------------------------------------
// 语义 token
// ---------------------------------------------------------------------------

/// 一套主题的全部语义色值。字段名与 04 §2 的 token 一一对应（点号换下划线）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeTokens {
    // 背景与表面
    pub bg_app: Hsla,
    pub bg_surface: Hsla,
    pub bg_elevated: Hsla,
    pub bg_sunken: Hsla,
    pub bg_hover: Hsla,
    pub bg_selected: Hsla,
    pub bg_overlay: Hsla,
    // 边框
    pub border_subtle: Hsla,
    pub border_default: Hsla,
    pub border_strong: Hsla,
    pub border_control: Hsla,
    // 文本
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub text_disabled: Hsla,
    pub text_inverse: Hsla,
    // 强调
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_active: Hsla,
    pub accent_soft: Hsla,
    pub accent_soft_hover: Hsla,
    pub on_accent: Hsla,
    // 状态
    pub success: Hsla,
    pub success_soft: Hsla,
    pub warning: Hsla,
    pub warning_soft: Hsla,
    pub danger: Hsla,
    pub danger_soft: Hsla,
    pub danger_text: Hsla,
    pub info: Hsla,
    pub info_soft: Hsla,
    // 其他
    pub track: Hsla,
    pub focus: Hsla,
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn hexa(value: u32) -> Hsla {
    rgba(value).into()
}

/// 颜色混合（用于 accent.soft-hover 这类派生值）：`t=0` 返回 `a`，`t=1` 返回 `b`。
fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let a: Rgba = a.into();
    let b: Rgba = b.into();
    let t = t.clamp(0.0, 1.0);
    rgba_with_alpha(
        a.r + (b.r - a.r) * t,
        a.g + (b.g - a.g) * t,
        a.b + (b.b - a.b) * t,
        a.a + (b.a - a.a) * t,
    )
}

fn rgba_with_alpha(r: f32, g: f32, b: f32, a: f32) -> Hsla {
    Rgba { r, g, b, a }.into()
}

/// 强调色组（04 §3）：`(accent, accent_hover, accent_active, accent_soft, on_accent)`。
fn accent_group(preset: AccentPreset, dark: bool) -> (Hsla, Hsla, Hsla, Hsla, Hsla) {
    match (preset, dark) {
        (AccentPreset::Blue, false) => (
            hex(0x2563EB),
            hex(0x1D4ED8),
            hex(0x1A46B8),
            hex(0xE7F0FF),
            hex(0xFFFFFF),
        ),
        (AccentPreset::Blue, true) => (
            hex(0x5B9DFF),
            hex(0x7DB2FF),
            hex(0x4A8DF0),
            hex(0x16283F),
            hex(0x0B1220),
        ),
        (AccentPreset::Cyan, false) => (
            hex(0x0E7490),
            hex(0x0B5E76),
            hex(0x0A5266),
            hex(0xE7F1F4),
            hex(0xFFFFFF),
        ),
        (AccentPreset::Cyan, true) => (
            hex(0x3DD6E8),
            hex(0x66E2F0),
            hex(0x2FC2D4),
            hex(0x1A3139),
            hex(0x06232A),
        ),
        (AccentPreset::Violet, false) => (
            hex(0x7C3AED),
            hex(0x6D2BD8),
            hex(0x6224C4),
            hex(0xF2EBFD),
            hex(0xFFFFFF),
        ),
        (AccentPreset::Violet, true) => (
            hex(0xA78BFA),
            hex(0xBCA5FF),
            hex(0x9575F0),
            hex(0x27283B),
            hex(0x140B2E),
        ),
        (AccentPreset::Green, false) => (
            hex(0x047857),
            hex(0x036847),
            hex(0x025C3F),
            hex(0xE6F2EE),
            hex(0xFFFFFF),
        ),
        (AccentPreset::Green, true) => (
            hex(0x4ADE80),
            hex(0x6FE89B),
            hex(0x3ACB70),
            hex(0x1B322C),
            hex(0x052E16),
        ),
        (AccentPreset::Orange, false) => (
            hex(0xB45309),
            hex(0x9A4607),
            hex(0x8A3E06),
            hex(0xF8EEE6),
            hex(0xFFFFFF),
        ),
        (AccentPreset::Orange, true) => (
            hex(0xFBBF24),
            hex(0xFCD165),
            hex(0xF0AF10),
            hex(0x312E21),
            hex(0x2A1A02),
        ),
    }
}

/// 浅色主题（04 §2；含 P-01…P-04 无障碍修正）。
pub fn light(preset: AccentPreset) -> ThemeTokens {
    let (accent, accent_hover, accent_active, accent_soft, on_accent) = accent_group(preset, false);
    ThemeTokens {
        bg_app: hex(0xF5F7FA),
        bg_surface: hex(0xFFFFFF),
        bg_elevated: hex(0xFFFFFF),
        bg_sunken: hex(0xEEF1F5),
        bg_hover: hex(0xEEF2F7),
        bg_selected: hex(0xE7F0FF),
        bg_overlay: hexa(0x12182161),
        border_subtle: hex(0xE7EBF1),
        border_default: hex(0xD8DEE8),
        border_strong: hex(0xC3CCD9),
        border_control: hex(0x7E8B9C),
        text_primary: hex(0x121821),
        text_secondary: hex(0x5A6575),
        text_tertiary: hex(0x616C7E),
        text_disabled: hex(0xA3AFBF),
        text_inverse: hex(0xFFFFFF),
        accent,
        accent_hover,
        accent_active,
        accent_soft,
        accent_soft_hover: mix(accent_soft, accent, 0.08),
        on_accent,
        success: hex(0x15803D),
        success_soft: hexa(0x15803D1A),
        warning: hex(0xB45309),
        warning_soft: hexa(0xB453091A),
        danger: hex(0xDC2626),
        danger_soft: hexa(0xDC262617),
        danger_text: hex(0xC81E1E),
        info: hex(0x0369A1),
        info_soft: hexa(0x0369A117),
        track: hex(0xDDE6F0),
        focus: hex(0x2563EB),
    }
}

/// 深色主题（04 §2；含 P-01…P-04 无障碍修正）。
pub fn dark(preset: AccentPreset) -> ThemeTokens {
    let (accent, accent_hover, accent_active, accent_soft, on_accent) = accent_group(preset, true);
    ThemeTokens {
        bg_app: hex(0x0E1116),
        bg_surface: hex(0x151A21),
        bg_elevated: hex(0x1C232D),
        bg_sunken: hex(0x0A0D12),
        bg_hover: hex(0x1B222C),
        bg_selected: hex(0x16283F),
        bg_overlay: hexa(0x05070BA8),
        border_subtle: hex(0x232B36),
        border_default: hex(0x2E3847),
        border_strong: hex(0x3D4A5C),
        border_control: hex(0x5C6E88),
        text_primary: hex(0xE8EDF4),
        text_secondary: hex(0x9AA6B6),
        text_tertiary: hex(0x8491A3),
        text_disabled: hex(0x4E5A6B),
        text_inverse: hex(0x0B1220),
        accent,
        accent_hover,
        accent_active,
        accent_soft,
        accent_soft_hover: mix(accent_soft, accent, 0.14),
        on_accent,
        success: hex(0x3DDC97),
        success_soft: hexa(0x3DDC9724),
        warning: hex(0xF5A524),
        warning_soft: hexa(0xF5A52424),
        danger: hex(0xFF6B6B),
        danger_soft: hexa(0xFF6B6B24),
        danger_text: hex(0xFF6B6B),
        info: hex(0x38BDF8),
        info_soft: hexa(0x38BDF824),
        track: hex(0x233044),
        focus: hex(0x7DB2FF),
    }
}

// ---------------------------------------------------------------------------
// 全局主题状态
// ---------------------------------------------------------------------------

/// 新版 UI 当前生效的视觉状态（Global）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiVisualState {
    pub dark: bool,
    pub accent: AccentPreset,
    pub reduce_motion: bool,
}

impl gpui::Global for UiVisualState {}

/// 读取当前 token（浅/深由全局状态决定）。
pub fn tokens(cx: &gpui::App) -> ThemeTokens {
    let state = cx.global::<UiVisualState>();
    if state.dark {
        dark(state.accent)
    } else {
        light(state.accent)
    }
}

/// 是否减少动效。
#[allow(dead_code)] // Kit API：视图按需查询
pub fn reduce_motion(cx: &gpui::App) -> bool {
    cx.global::<UiVisualState>().reduce_motion
}

/// 安装/更新新版主题：写入全局 token 状态，并把色值映射进 `gpui-component` 的
/// `Theme`（保证其内建 Button/Input/Dialog/Notification 等与新 token 一致）。
pub fn apply(cx: &mut gpui::App, dark: bool, accent: AccentPreset, reduce_motion: bool) {
    cx.set_global(UiVisualState {
        dark,
        accent,
        reduce_motion,
    });
    apply_component_theme(cx);
}

/// 把语义 token 映射到 `gpui_component::Theme`（供其内建组件使用）。
pub fn apply_component_theme(cx: &mut gpui::App) {
    let t = tokens(cx);
    let dark = cx.global::<UiVisualState>().dark;
    let theme = Theme::global_mut(cx);
    theme.mode = if dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    theme.font_family = "Segoe UI Variable Text".into();
    theme.mono_font_family = "Cascadia Mono".into();
    theme.font_size = px(FS_BODY);
    theme.mono_font_size = px(FS_MONO);
    theme.radius = px(RADIUS_SM);
    theme.radius_lg = px(RADIUS_LG);
    theme.shadow = true;
    theme.tile_grid_size = px(4.0);

    let c = &mut theme.colors;
    // 基础
    c.background = t.bg_app;
    c.foreground = t.text_primary;
    c.border = t.border_default;
    c.input = t.border_control;
    c.caret = t.accent;
    c.ring = t.focus;
    c.selection = t.accent_soft;
    // 主按钮 / 链接
    c.primary = t.accent;
    c.primary_foreground = t.on_accent;
    c.primary_hover = t.accent_hover;
    c.primary_active = t.accent_active;
    c.link = t.accent;
    c.link_hover = t.accent_hover;
    c.link_active = t.accent_active;
    // 次级按钮 / 中性
    c.secondary = t.bg_hover;
    c.secondary_foreground = t.text_primary;
    c.secondary_hover = t.bg_hover;
    c.secondary_active = t.bg_selected;
    c.muted = t.bg_sunken;
    c.muted_foreground = t.text_tertiary;
    c.accent = t.accent_soft;
    c.accent_foreground = t.text_primary;
    // 浮层
    c.popover = t.bg_elevated;
    c.popover_foreground = t.text_primary;
    // 列表
    c.list = t.bg_app;
    c.list_hover = t.bg_hover;
    c.list_active = t.bg_selected;
    c.list_active_border = t.accent;
    c.list_head = t.bg_surface;
    c.list_even = t.bg_app;
    // 侧栏
    c.sidebar = t.bg_surface;
    c.sidebar_foreground = t.text_primary;
    c.sidebar_border = t.border_subtle;
    c.sidebar_accent = t.bg_hover;
    c.sidebar_accent_foreground = t.text_primary;
    c.sidebar_primary = t.accent;
    c.sidebar_primary_foreground = t.on_accent;
    // 状态色（gpui-component 的 danger/success/... 是「实底背景」语义）
    c.danger = t.danger;
    c.danger_foreground = t.text_inverse;
    c.danger_hover = t.danger;
    c.danger_active = t.danger;
    c.success = t.success;
    c.success_foreground = t.text_inverse;
    c.success_hover = t.success;
    c.success_active = t.success;
    c.warning = t.warning;
    c.warning_foreground = t.text_inverse;
    c.warning_hover = t.warning;
    c.warning_active = t.warning;
    c.info = t.info;
    c.info_foreground = t.text_inverse;
    c.info_hover = t.info;
    c.info_active = t.info;
    // 开关（Switch）：开=accent 轨道（组件用 primary），关=track；滑块统一用 on_accent
    c.switch = t.track;
    c.switch_thumb = t.on_accent;
    // 进度 / 滚动条 / 骨架
    c.progress_bar = t.accent;
    c.slider_bar = t.track;
    c.slider_thumb = t.accent;
    c.scrollbar = Hsla::transparent_black();
    c.scrollbar_thumb = t.border_strong;
    c.scrollbar_thumb_hover = t.text_tertiary;
    c.skeleton = t.bg_hover;
    c.accordion = t.bg_surface;
    c.accordion_hover = t.bg_hover;
    c.group_box = t.bg_surface;
    c.group_box_foreground = t.text_primary;
    c.description_list_label = t.bg_sunken;
    c.description_list_label_foreground = t.text_secondary;
    c.drag_border = t.accent;
    c.drop_target = t.accent_soft;
    // 图表
    c.chart_1 = t.accent;
    c.chart_2 = t.info;
    c.chart_3 = t.success;
    c.chart_4 = t.warning;
    c.chart_5 = t.danger;
}

/// 由设置解析「实际是否深色」：`FollowSystem` 时读取窗口外观。
pub fn resolve_dark(mode: UiThemeMode, window: &gpui::Window) -> bool {
    match mode {
        UiThemeMode::Light => false,
        UiThemeMode::Dark => true,
        UiThemeMode::FollowSystem => !matches!(window.appearance(), gpui::WindowAppearance::Light),
    }
}
