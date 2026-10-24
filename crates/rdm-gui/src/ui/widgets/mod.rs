//! 基础组件（05-component-library 的薄实现）。
//!
//! 原则：能用 gpui-component 内建组件（Button/Input/Dialog/Menu/Tooltip…）的就用内建，
//! 本模块只补 **设计稿特有的小部件**（状态 Pill、细进度条、Kbd、空态、分组标题等），
//! 所有颜色来自 [`ThemeTokens`]，不出现色值字面量。

use gpui::prelude::*;
use gpui::{
    canvas, div, fill, point, px, relative, size, AnyElement, App, ClickEvent, Hsla, IntoElement,
    ParentElement, SharedString, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::v_flex;

use crate::ui::icons::RdmIcon;
use crate::ui::theme::{ThemeTokens, FS_CAPTION, FS_MICRO, FS_MONO_MICRO, RADIUS_SM, RADIUS_XS};

/// 纯图标按钮：强制 tooltip（契约 §8.5）。
pub fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: RdmIcon,
    tooltip: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    Button::new(id)
        .ghost()
        .compact()
        .icon(icon.icon())
        .tooltip(tooltip)
        .on_click(on_click)
}

/// 状态点（状态三通道之一；不单独承担语义）。
pub fn status_dot(color: Hsla, size: f32) -> impl IntoElement {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

/// 状态 Pill：图标 + 文案 + 颜色（契约 §5.9：三通道）。
pub fn status_pill(
    _t: &ThemeTokens,
    icon: RdmIcon,
    label: impl Into<SharedString>,
    foreground: Hsla,
    background: Hsla,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(20.0))
        .px(px(7.0))
        .rounded(px(999.0))
        .bg(background)
        .text_color(foreground)
        .text_size(px(FS_CAPTION))
        .child(icon.sized(13.0, foreground))
        .child(label.into())
        .into_any_element()
}

/// 细进度条（determinate）：4px 高、250ms 平滑由 GPUI 布局自然过渡。
pub fn progress_bar(t: &ThemeTokens, fraction: Option<f32>, height: f32) -> impl IntoElement {
    progress_bar_colored(t, fraction, height, t.accent)
}

/// 指定颜色的细进度条（暂停=warning、完成=success、失败=danger）。
pub fn progress_bar_colored(
    t: &ThemeTokens,
    fraction: Option<f32>,
    height: f32,
    color: Hsla,
) -> impl IntoElement {
    let value = fraction.unwrap_or(0.0).clamp(0.0, 1.0);
    div()
        .w_full()
        .h(px(height))
        .rounded(px(999.0))
        .bg(t.track)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(relative(value))
                .rounded(px(999.0))
                .bg(color),
        )
}

/// 快捷键提示（Kbd）：圆角小标签 + 等宽字体。
pub fn kbd(t: &ThemeTokens, label: impl Into<SharedString>) -> impl IntoElement {
    let t_kbd_bg = t.bg_sunken;
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(18.0))
        .min_w(px(18.0))
        .px(px(4.0))
        .rounded(px(RADIUS_XS))
        .bg(t_kbd_bg)
        .text_size(px(FS_MONO_MICRO))
        .child(label.into())
}

/// 软色标签（类型/来源等非状态信息）。
pub fn soft_tag(
    label: impl Into<SharedString>,
    foreground: Hsla,
    background: Hsla,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(18.0))
        .px(px(6.0))
        .rounded(px(RADIUS_XS))
        .bg(background)
        .text_color(foreground)
        .text_size(px(FS_MICRO))
        .child(label.into())
}

/// 分组标题（小号、弱化、可带计数）。
pub fn section_label(t: &ThemeTokens, label: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px(px(8.0))
        .pt(px(8.0))
        .pb(px(4.0))
        .text_size(px(FS_MICRO))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(t.text_tertiary)
        .child(label.into())
}

/// 等宽数字文本（速度/大小/ETA/百分比，防抖位）。
pub fn mono_text(text: impl Into<SharedString>, size: f32, color: Hsla) -> impl IntoElement {
    div()
        .text_size(px(size))
        .text_color(color)
        .font(crate::ui::theme::mono_font())
        .child(text.into())
}

/// 1px 分隔线。
pub fn divider(t: &ThemeTokens) -> impl IntoElement {
    div().h(px(1.0)).w_full().bg(t.border_subtle)
}

/// 空状态：图标 + 标题 + 一句解释 + 1–3 个动作入口（契约 §7.4）。
pub fn empty_state(
    t: &ThemeTokens,
    icon: RdmIcon,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    actions: Vec<AnyElement>,
) -> impl IntoElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(56.0))
                .rounded(px(crate::ui::theme::RADIUS_XL))
                .bg(t.bg_sunken)
                .child(icon.sized(24.0, t.text_tertiary)),
        )
        .child(
            div()
                .text_size(px(crate::ui::theme::FS_DISPLAY))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(t.text_primary)
                .child(title.into()),
        )
        .child(
            div()
                .max_w(px(420.0))
                .text_size(px(crate::ui::theme::FS_BODY))
                .text_color(t.text_secondary)
                .child(description.into()),
        )
        .children(actions)
}

/// 列表行内的次要信息（域名 · 连接数 等）。
pub fn meta_text(t: &ThemeTokens, text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(px(FS_CAPTION))
        .text_color(t.text_secondary)
        .child(text.into())
}

/// 可折叠侧栏的导航项：图标 + 文案 + 计数 + 选中态（选中 = accent-soft + 左侧 2px 条）。
pub fn nav_item(
    t: &ThemeTokens,
    id: impl Into<gpui::ElementId>,
    icon: RdmIcon,
    label: impl Into<SharedString>,
    count: Option<usize>,
    selected: bool,
    collapsed: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let fg = if selected { t.accent } else { t.text_secondary };
    let bg = if selected {
        t.accent_soft
    } else {
        t.bg_surface
    };
    let label = label.into();
    div()
        .id(id)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .h(px(32.0))
        .mx(px(8.0))
        .px(px(8.0))
        .rounded(px(RADIUS_SM))
        .bg(bg)
        .text_color(fg)
        .text_size(px(crate::ui::theme::FS_BODY))
        .cursor_pointer()
        .when(selected, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(-8.0))
                    .top(px(7.0))
                    .w(px(2.0))
                    .h(px(18.0))
                    .rounded(px(1.0))
                    .bg(t.accent),
            )
        })
        .when(!selected, |this| this.hover(move |s| s.bg(t.bg_hover)))
        .child(icon.sized(16.0, fg))
        .when(!collapsed, |this| {
            this.child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(label.clone()),
            )
            .when_some(count, |this, count| {
                this.child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(if selected { t.accent } else { t.text_tertiary })
                        .child(count.to_string()),
                )
            })
        })
        .when(collapsed, |this| this.justify_center())
        .on_click(on_click)
}

/// 底部状态栏里的小字。
pub fn status_text(t: &ThemeTokens, text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(px(FS_MICRO))
        .text_color(t.text_secondary)
        .child(text.into())
}

/// 横幅/提示条（浏览器集成异常等）：图标 + 文案 + 可选动作。
pub fn info_banner(
    t: &ThemeTokens,
    icon: RdmIcon,
    severity: Severity,
    text: impl Into<SharedString>,
    actions: Vec<AnyElement>,
) -> impl IntoElement {
    let (fg, bg) = match severity {
        Severity::Info => (t.info, t.info_soft),
        Severity::Warning => (t.warning, t.warning_soft),
        Severity::Danger => (t.danger_text, t.danger_soft),
        Severity::Success => (t.success, t.success_soft),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .min_h(px(32.0))
        .px(px(12.0))
        .py(px(4.0))
        .bg(bg)
        .text_color(fg)
        .text_size(px(FS_CAPTION))
        .child(icon.sized(14.0, fg))
        .child(div().flex_1().child(text.into()))
        .children(actions)
}

/// 提示条严重度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Danger,
}

/// 速度迷你曲线（canvas 柱状，自适应画布宽度）。
///
/// 新版下载中枢行、旧版界面行与详情面板共用。绘制区域**严格限制**在
/// `width × height` 内：历史采样多时同步收缩柱宽与间隙，而不是把每根柱子
/// 都撑到至少 1px 再按 1px 间隙排布——历史 bug 里 60 个采样需要
/// `60 * 1 + 59 * 1 = 119px`，72px 的画布会把曲线画到右侧「下载中」状态徽章上
/// （见 2026-10-24 用户截图：曲线溢出覆盖「下载中」Pill）。
pub fn sparkline(history: Vec<f64>, color: Hsla, width: f32, height: f32) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_sparkline(&history, bounds, window, color),
    )
    .w(px(width))
    .h(px(height))
    .into_any_element()
}

/// 在 canvas 边界内绘制速度柱状曲线（右边界逐柱裁剪，容忍浮点累计误差）。
fn paint_sparkline(
    history: &[f64],
    bounds: gpui::Bounds<gpui::Pixels>,
    window: &mut Window,
    color: Hsla,
) {
    if history.is_empty() {
        return;
    }
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    let origin_x: f32 = bounds.origin.x.into();
    let origin_y: f32 = bounds.origin.y.into();
    let end_x = origin_x + width;
    let max = history.iter().copied().fold(0.0_f64, f64::max).max(1.0);
    let (gap, bar_width) = sparkline_bar_layout(width, history.len());
    for (index, value) in history.iter().enumerate() {
        let x = origin_x + index as f32 * (bar_width + gap);
        if x >= end_x {
            continue;
        }
        let clamped_width = (end_x - x).min(bar_width);
        if clamped_width <= 0.0 {
            continue;
        }
        let bar_height = (height * (*value as f32 / max as f32)).clamp(1.0, height);
        window.paint_quad(fill(
            gpui::Bounds::new(
                point(px(x), px(origin_y + height - bar_height)),
                size(px(clamped_width), px(bar_height)),
            ),
            color,
        ));
    }
}

/// 柱状曲线布局：返回 `(间隙, 柱宽)`，并保证
/// `count * 柱宽 + (count - 1) * 间隙 <= width`。
///
/// 间隙最多 1px 且最多占画布宽度的 25%，样本数越多间隙越小；柱宽由剩余
/// 宽度均分，从而让 60 个采样也能排进 72px 的迷你曲线画布。
pub fn sparkline_bar_layout(width: f32, count: usize) -> (f32, f32) {
    let width = width.max(0.0);
    if count == 0 || width <= 0.0 {
        return (0.0, 0.0);
    }
    let count_f = count as f32;
    let gap = if count > 1 {
        (width * 0.25 / (count_f - 1.0)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let bar_width = ((width - gap * (count_f - 1.0)) / count_f).max(0.0);
    (gap, bar_width)
}

#[cfg(test)]
mod sparkline_layout_tests {
    use super::sparkline_bar_layout;

    /// 回归：60 个采样（每分钟累计满历史）必须完整排进 72px 行内迷你曲线，
    /// 不允许超出画布（历史 bug：1px 柱宽 + 1px 间隙画成 119px，溢出覆盖状态徽章）。
    #[test]
    fn sixty_samples_fit_the_row_sparkline_width() {
        let (gap, bar_width) = sparkline_bar_layout(72.0, 60);
        let painted = 60.0 * bar_width + 59.0 * gap;
        assert!(
            painted <= 72.0 + 0.01,
            "60 个采样绘制宽度 {painted}px 超出 72px 画布（柱宽 {bar_width}px，间隙 {gap}px）"
        );
    }

    /// 任意样本数/画布宽度下，布局总宽都不超过画布宽度。
    #[test]
    fn layout_never_exceeds_canvas_width() {
        for width in [1.0_f32, 10.0, 48.0, 72.0, 120.0] {
            for count in 1..=200_usize {
                let (gap, bar_width) = sparkline_bar_layout(width, count);
                assert!(gap >= 0.0 && bar_width >= 0.0, "负尺寸：count={count}");
                let painted = count as f32 * bar_width + (count - 1) as f32 * gap;
                assert!(
                    painted <= width + 0.01,
                    "count={count} width={width}: painted={painted}"
                );
            }
        }
    }

    /// 罕见样本数下柱状图仍可绘制，不会全部塌缩为 0。
    #[test]
    fn single_sample_fills_the_whole_width() {
        let (gap, bar_width) = sparkline_bar_layout(72.0, 1);
        assert_eq!(gap, 0.0);
        assert!((bar_width - 72.0).abs() < f32::EPSILON);
    }
}

/// 把长文件名截断成「前 N…后 M」并保留扩展名（04 §4.2）。
pub fn ellipsize_file_name(name: &str, max_chars: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max_chars || max_chars < 8 {
        return name.to_string();
    }
    let ext_len = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.chars().count() + 1)
        .unwrap_or(0)
        .min(max_chars / 2);
    let head = max_chars.saturating_sub(ext_len + 1);
    let tail = ext_len;
    let head_str: String = chars[..head].iter().collect();
    let tail_str: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_str}…{tail_str}")
}
