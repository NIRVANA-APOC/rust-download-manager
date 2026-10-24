//! 资源监控页（`docs/design/13-resource-monitor.md`）。
//!
//! 纯渲染模块：数据全部来自 [`crate::monitor::MonitorSnapshot`]，动作通过
//! [`MonitorCallbacks`] 回传，不直接访问 `RdmApp` 的私有字段。
//!
//! 页面结构（设计 §3）：
//!
//! ```text
//! 页头（下载目录 / 上次扫描 / 扫描中 + 重新扫描、打开下载目录）
//! 4 张 KPI 卡（下载库占用 · 磁盘空间 · AI 消耗 · 待整理空间）
//! ┌ 主列 ────────────────────────┬ 侧列 ──────────────────────┐
//! │ 空间构成（堆叠条 + 分类图例） │ AI 消耗趋势（14 桶柱状）    │
//! │ 最大的文件（Top 8）           │ 传输活动（状态条 + 速度）    │
//! │ AI 使用明细（最贵下载项）     │ 维护与数据（状态目录/缓存）  │
//! └──────────────────────────────┴────────────────────────────┘
//! 页脚说明（数据来源 / 统计口径 / 扫描上限 / 隐私）
//! ```
//!
//! 宽度适配：只用 `flex_wrap` + `min_w`（[`render_monitor_page`] 不接触窗口宽度），
//! 内容区最宽 [`CONTENT_MAX_W`]、侧列固定 [`SIDE_COL_W`]；窄窗口自动单列、KPI 2×2，
//! 不出现横向滚动。所有颜色只来自 [`ThemeTokens`]，数字/路径/模型名一律等宽字体。

use std::path::Path;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    div, px, relative, AnyElement, App, ClickEvent, Div, FontWeight, Hsla, IntoElement,
    ScrollHandle, SharedString, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, Disableable, Sizable};
use rdm_ai::TokenUsage;

use crate::model::{format_bytes, format_speed};
use crate::monitor::scan::{MAX_SCAN_DEPTH, MAX_SCAN_ENTRIES};
use crate::monitor::{
    format_cny, format_cny_approx, format_percent, format_relative_ms, format_tokens,
    format_tokens_exact, format_tokens_maybe_estimated, trend_axis_labels, AiUsageSummary,
    CategoryUsage, FileUsage, ItemTokenUsage, LibraryUsage, MonitorSnapshot, TransferSummary,
    TREND_BUCKETS,
};
use crate::ui::icons::RdmIcon;
use crate::ui::theme::{
    self, ThemeTokens, FS_CAPTION, FS_DISPLAY, FS_H1, FS_MICRO, FS_MONO, FS_MONO_MICRO,
    FS_MONO_PATH, FS_ROW_TITLE, RADIUS_LG, RADIUS_MD, RADIUS_SM, RADIUS_XS,
};
use crate::ui::widgets::{
    divider, ellipsize_file_name, empty_state, icon_button, info_banner, meta_text, mono_text,
    progress_bar_colored, soft_tag, Severity,
};
use crate::views::app::FileCategory;

// ---------------------------------------------------------------------------
// 布局常量
// ---------------------------------------------------------------------------

/// 页面内容最大宽度（再宽也不拉伸卡片，避免图表被拉成细长条）。
const CONTENT_MAX_W: f32 = 1180.0;
/// 页面内边距。
const CONTENT_PAD: f32 = 24.0;
/// 卡片 / 区块之间的垂直间距。
const SECTION_GAP: f32 = 16.0;
/// 卡片内边距（设计 §4.1：16px）。
const CARD_PAD: f32 = 16.0;
/// 卡片内部元素间距。
const CARD_GAP: f32 = 12.0;
/// KPI 卡最小宽度（设计 §4.1）。
const KPI_MIN_W: f32 = 220.0;
/// 主列最小宽度：窗口变窄时侧列换行成单列（≈1000px 窗口断点）。
const MAIN_COL_MIN_W: f32 = 440.0;
/// 侧列最小 / 最大宽度：宽窗口时占右侧一列，窄窗口换行后仍尽量铺满。
const SIDE_COL_MIN_W: f32 = 300.0;
const SIDE_COL_MAX_W: f32 = 420.0;
/// 大趋势图高度。
const TREND_CHART_H: f32 = 64.0;
/// KPI 迷你趋势图高度。
const MINI_CHART_H: f32 = 20.0;
/// 柱状图最小高度（全 0 也画一条 2px 基线，避免「空白」）。
const BAR_MIN_H: f32 = 2.0;
/// 空间构成堆叠条高度（设计 §4.2：12px）。
const COMPOSITION_BAR_H: f32 = 12.0;
/// 传输状态分布条高度。
const TRANSFER_BAR_H: f32 = 8.0;
/// 数据超过该毫秒数视为过期（设计 §1.4：2 分钟标黄）。
const STALE_AFTER_MS: u64 = 2 * 60 * 1000;
/// 空态/骨架的最小内容高度。
const STATE_MIN_H: f32 = 380.0;

// ---------------------------------------------------------------------------
// 公开接口（`views/app.rs` 已按此集成；字段名与签名不可改）
// ---------------------------------------------------------------------------

/// 页面只读状态（由 `RdmApp` 组装）。
pub struct MonitorViewState<'a> {
    pub snapshot: Option<&'a MonitorSnapshot>,
    pub scanning: bool,
    pub error: Option<&'a str>,
    /// 已格式化的「上次扫描 12:00:12（42ms）」；`None` = 尚未扫描。
    pub last_scan_label: Option<String>,
    pub ai_configured: bool,
    pub download_dir: Option<&'a Path>,
    pub now_ms: u64,
}

/// 页面动作回调的公共类型（盒子化后由 `RdmApp` 构造）。
pub type MonitorSimpleCallback = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
pub type MonitorCategoryCallback =
    Box<dyn Fn(FileCategory, &ClickEvent, &mut Window, &mut App) + 'static>;
pub type MonitorItemCallback = Box<dyn Fn(&str, &ClickEvent, &mut Window, &mut App) + 'static>;

/// 页面动作（由 `RdmApp` 用 `cx.listener` 构造）。
pub struct MonitorCallbacks {
    pub on_rescan: MonitorSimpleCallback,
    pub on_open_download_dir: MonitorSimpleCallback,
    pub on_open_state_dir: MonitorSimpleCallback,
    pub on_open_ai_settings: MonitorSimpleCallback,
    pub on_jump_category: MonitorCategoryCallback,
    pub on_select_item: MonitorItemCallback,
    pub on_open_item_folder: MonitorItemCallback,
}

// ---------------------------------------------------------------------------
// 回调适配：同一闭包会在多张卡片里复用，统一包成 `Rc` 再逐处 clone
// ---------------------------------------------------------------------------

type SimpleCb = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type CategoryCb = Rc<dyn Fn(FileCategory, &ClickEvent, &mut Window, &mut App)>;
type IdCb = Rc<dyn Fn(&str, &ClickEvent, &mut Window, &mut App)>;

/// 页面内共享的回调集合。
struct Cbs {
    rescan: SimpleCb,
    open_download_dir: SimpleCb,
    open_state_dir: SimpleCb,
    open_ai_settings: SimpleCb,
    jump_category: CategoryCb,
    select_item: IdCb,
}

impl Cbs {
    fn new(callbacks: MonitorCallbacks) -> Self {
        // `on_open_item_folder` 需要下载项 id，而「最大的文件」条目（`FileUsage`）没有 id：
        // 行内不提供「打开所在文件夹」按钮（避免误导），改为卡片头的「打开下载目录」。
        // 字段保留以对齐 `app.rs` 集成。
        let _ = &callbacks.on_open_item_folder;
        Self {
            rescan: Rc::from(callbacks.on_rescan),
            open_download_dir: Rc::from(callbacks.on_open_download_dir),
            open_state_dir: Rc::from(callbacks.on_open_state_dir),
            open_ai_settings: Rc::from(callbacks.on_open_ai_settings),
            jump_category: Rc::from(callbacks.on_jump_category),
            select_item: Rc::from(callbacks.on_select_item),
        }
    }
}

/// 构造一个无参点击处理。
fn simple_click(cb: &SimpleCb) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(event, window, cx)
}

/// 构造一个「携带分类」的点击处理。
fn category_click(
    cb: &CategoryCb,
    category: FileCategory,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(category, event, window, cx)
}

/// 构造一个「携带下载项 id」的点击处理。
fn id_click(cb: &IdCb, id: String) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(id.as_str(), event, window, cx)
}

// ---------------------------------------------------------------------------
// 页面入口
// ---------------------------------------------------------------------------

/// 渲染整页（可滚动）。
pub fn render_monitor_page(
    t: &ThemeTokens,
    scroll: &ScrollHandle,
    state: MonitorViewState<'_>,
    callbacks: MonitorCallbacks,
) -> AnyElement {
    let t = *t;
    let cb = Cbs::new(callbacks);
    let snapshot = state.snapshot;

    let body: AnyElement = if state.download_dir.is_none() {
        // 未设置保存目录：任何统计都没有意义（设计验收 1）。
        no_directory_state(&t, &cb)
    } else if let Some(snapshot) = snapshot {
        snapshot_content(&t, &state, snapshot, &cb)
    } else if let Some(error) = state.error {
        scan_failed_state(&t, error, &cb)
    } else if state.scanning {
        scanning_state(&t)
    } else {
        idle_state(&t, &cb)
    };

    let content = v_flex()
        .w_full()
        .max_w(px(CONTENT_MAX_W))
        .gap(px(SECTION_GAP))
        .p(px(CONTENT_PAD))
        .child(page_header(&t, &state, &cb))
        .child(body);

    v_flex()
        .size_full()
        .bg(t.bg_app)
        .text_color(t.text_primary)
        .font(theme::ui_font())
        .child(
            div()
                .id("monitor-scroll")
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .child(h_flex().w_full().justify_center().child(content)),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 页头
// ---------------------------------------------------------------------------

fn page_header(t: &ThemeTokens, state: &MonitorViewState<'_>, cb: &Cbs) -> AnyElement {
    let scanning = state.scanning;

    let title_row = h_flex()
        .gap(px(10.0))
        .items_center()
        .flex_wrap()
        .child(
            div()
                .text_size(px(FS_H1))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text_primary)
                .child("资源监控"),
        )
        .when(scanning, |this| this.child(scanning_pill(t)));

    // 副标题：下载目录 · 上次扫描 HH:MM:SS（Nms）· 每 30s 自动刷新。
    let mut subtitle = h_flex().w_full().flex_wrap().items_center().gap(px(6.0));
    match state.download_dir {
        Some(dir) => {
            let full = dir.display().to_string();
            let tip = full.clone();
            subtitle = subtitle.child(
                div()
                    .id("monitor-download-dir")
                    .max_w(px(460.0))
                    .truncate()
                    .font(theme::mono_font())
                    .text_size(px(FS_MONO_PATH))
                    .text_color(t.text_secondary)
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .child(full),
            );
        }
        None => {
            subtitle = subtitle.child(meta_text(t, "尚未设置下载目录"));
        }
    }
    if let Some(label) = state.last_scan_label.as_ref() {
        subtitle = subtitle.child(dot(t)).child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child(label.clone()),
        );
    } else if !scanning {
        subtitle = subtitle.child(dot(t)).child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("尚未扫描"),
        );
    }

    // 右侧动作：重新扫描（扫描中禁用 + 「扫描中…」）、打开下载目录。
    let rescan = Button::new("monitor-rescan")
        .small()
        .primary()
        .icon(RdmIcon::Refresh.icon())
        .label(if scanning {
            "扫描中…"
        } else {
            "重新扫描"
        })
        .loading(scanning)
        .disabled(scanning)
        .tooltip(if scanning {
            "正在扫描下载目录，请稍候"
        } else {
            "重新统计下载目录占用、AI 消耗与可回收空间"
        })
        .on_click(simple_click(&cb.rescan));

    let dir_set = state.download_dir.is_some();
    let open_dir = Button::new("monitor-header-open-dir")
        .small()
        .icon(RdmIcon::FolderOpen.icon())
        .label("打开下载目录")
        .disabled(!dir_set)
        .tooltip(if dir_set {
            "在资源管理器中打开下载目录"
        } else {
            "还没有设置下载目录"
        })
        .on_click(simple_click(&cb.open_download_dir));

    h_flex()
        .w_full()
        .flex_wrap()
        .items_end()
        .justify_between()
        .gap(px(12.0))
        .child(
            v_flex()
                .flex_1()
                .min_w(px(260.0))
                .gap(px(6.0))
                .child(title_row)
                .child(subtitle),
        )
        .child(
            h_flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .child(rescan)
                .child(open_dir),
        )
        .into_any_element()
}

/// 「扫描中…」状态胶囊（带旋转指示器）。
fn scanning_pill(t: &ThemeTokens) -> AnyElement {
    h_flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h(px(22.0))
        .px(px(8.0))
        .rounded(px(999.0))
        .bg(t.accent_soft)
        .child(Spinner::new().small().color(t.accent))
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.accent)
                .child("扫描中…"),
        )
        .into_any_element()
}

/// 分隔点 `·`。
fn dot(t: &ThemeTokens) -> AnyElement {
    div()
        .flex_none()
        .text_size(px(FS_MICRO))
        .text_color(t.text_tertiary)
        .child("·")
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 状态页：未设置目录 / 空闲 / 首次扫描 / 首次失败
// ---------------------------------------------------------------------------

fn no_directory_state(t: &ThemeTokens, cb: &Cbs) -> AnyElement {
    let actions = vec![Button::new("monitor-empty-rescan")
        .small()
        .icon(RdmIcon::Refresh.icon())
        .label("重新扫描")
        .tooltip("如果刚刚在设置里选择了保存目录，点这里重新统计")
        .on_click(simple_click(&cb.rescan))
        .into_any_element()];
    v_flex()
        .w_full()
        .min_h(px(STATE_MIN_H))
        .items_center()
        .justify_center()
        .child(empty_state(
            t,
            RdmIcon::FolderOpen,
            "还没有设置下载目录",
            "先设置一个保存目录，RDM 才能统计真实占用、AI 消耗与可回收空间。",
            actions,
        ))
        .into_any_element()
}

fn idle_state(t: &ThemeTokens, cb: &Cbs) -> AnyElement {
    let actions = vec![Button::new("monitor-idle-rescan")
        .small()
        .primary()
        .icon(RdmIcon::Refresh.icon())
        .label("重新扫描")
        .tooltip("统计下载目录占用、AI 消耗与可回收空间")
        .on_click(simple_click(&cb.rescan))
        .into_any_element()];
    v_flex()
        .w_full()
        .min_h(px(STATE_MIN_H))
        .items_center()
        .justify_center()
        .child(empty_state(
            t,
            RdmIcon::Gauge,
            "还没有扫描数据",
            "点「重新扫描」统计下载目录占用、AI 消耗与可回收空间。",
            actions,
        ))
        .into_any_element()
}

/// 首次扫描骨架：保留页面结构，避免「空白 + 转圈」的廉价感。
fn scanning_state(t: &ThemeTokens) -> AnyElement {
    let mut kpis = h_flex().w_full().flex_wrap().gap(px(12.0));
    for index in 0..4 {
        kpis = kpis.child(skeleton_kpi(t, index));
    }
    let columns = h_flex()
        .w_full()
        .flex_wrap()
        .items_start()
        .gap(px(SECTION_GAP))
        .child(skeleton_card(t, 280.0).flex_1().min_w(px(MAIN_COL_MIN_W)))
        .child(
            skeleton_card(t, 280.0)
                .flex_1()
                .min_w(px(SIDE_COL_MIN_W))
                .max_w(px(SIDE_COL_MAX_W)),
        );

    v_flex()
        .w_full()
        .gap(px(SECTION_GAP))
        .child(scanning_banner(t))
        .child(kpis)
        .child(columns)
        .into_any_element()
}

fn scanning_banner(t: &ThemeTokens) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap(px(8.0))
        .min_h(px(36.0))
        .px(px(12.0))
        .rounded(px(RADIUS_SM))
        .bg(t.info_soft)
        .child(Spinner::new().small().color(t.info))
        .child(
            div()
                .text_size(px(FS_CAPTION))
                .text_color(t.info)
                .child("正在扫描下载目录…"),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("扫描在后台线程执行，不阻塞下载"),
        )
        .into_any_element()
}

fn skeleton_kpi(t: &ThemeTokens, index: usize) -> AnyElement {
    v_flex()
        .flex_1()
        .min_w(px(KPI_MIN_W))
        .min_h(px(112.0))
        .gap(px(10.0))
        .p(px(CARD_PAD))
        .rounded(px(RADIUS_LG))
        .bg(t.bg_surface)
        .border_1()
        .border_color(t.border_subtle)
        .child(
            h_flex()
                .gap(px(8.0))
                .items_center()
                .child(skeleton_block(t, 24.0, 24.0, RADIUS_SM))
                .child(skeleton_block(t, 72.0, 12.0, RADIUS_XS)),
        )
        .child(skeleton_block(t, 118.0, 24.0, RADIUS_XS))
        .child(skeleton_block(t, 150.0, 11.0, RADIUS_XS))
        .child(
            div()
                .flex_none()
                .w(relative(0.55 + (index % 3) as f32 * 0.12))
                .h(px(4.0))
                .rounded(px(999.0))
                .bg(t.bg_hover),
        )
        .into_any_element()
}

fn skeleton_card(t: &ThemeTokens, min_h: f32) -> Div {
    v_flex()
        .gap(px(CARD_GAP))
        .min_h(px(min_h))
        .p(px(CARD_PAD))
        .rounded(px(RADIUS_LG))
        .bg(t.bg_surface)
        .border_1()
        .border_color(t.border_subtle)
        .child(
            h_flex()
                .gap(px(8.0))
                .items_center()
                .child(skeleton_block(t, 14.0, 14.0, RADIUS_XS))
                .child(skeleton_block(t, 88.0, 14.0, RADIUS_XS)),
        )
        .child(divider(t))
        .child(skeleton_block(t, 260.0, 12.0, RADIUS_XS))
        .child(skeleton_block(t, 210.0, 12.0, RADIUS_XS))
        .child(skeleton_block(t, 240.0, 12.0, RADIUS_XS))
        .child(skeleton_block(t, 180.0, 12.0, RADIUS_XS))
}

fn skeleton_block(t: &ThemeTokens, width: f32, height: f32, radius: f32) -> AnyElement {
    div()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .rounded(px(radius))
        .bg(t.bg_hover)
        .into_any_element()
}

fn scan_failed_state(t: &ThemeTokens, error: &str, cb: &Cbs) -> AnyElement {
    let retry = Button::new("monitor-failed-retry")
        .small()
        .primary()
        .icon(RdmIcon::Retry.icon())
        .label("重试")
        .tooltip("重新扫描下载目录")
        .on_click(simple_click(&cb.rescan));
    v_flex()
        .w_full()
        .gap(px(SECTION_GAP))
        .child(error_banner(t, error, cb))
        .child(
            v_flex()
                .w_full()
                .min_h(px(STATE_MIN_H))
                .items_center()
                .justify_center()
                .gap(px(10.0))
                .p(px(24.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(56.0))
                        .rounded(px(theme::RADIUS_XL))
                        .bg(t.danger_soft)
                        .child(RdmIcon::Alert.sized(24.0, t.danger)),
                )
                .child(
                    div()
                        .text_size(px(FS_H1))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_primary)
                        .child("扫描下载目录失败"),
                )
                .child(
                    div()
                        .max_w(px(420.0))
                        .text_center()
                        .text_size(px(FS_CAPTION))
                        .text_color(t.text_secondary)
                        .child("暂时没有可展示的数据。确认目录可访问后再试一次。"),
                )
                .child(retry),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 快照内容：横幅 + KPI + 两列 + 页脚
// ---------------------------------------------------------------------------

fn snapshot_content(
    t: &ThemeTokens,
    state: &MonitorViewState<'_>,
    snapshot: &MonitorSnapshot,
    cb: &Cbs,
) -> AnyElement {
    let mut root = v_flex().w_full().gap(px(SECTION_GAP));

    // 横幅：错误（保留旧数据）/ 目录不可用 / 扫描截断 / 数据过期。
    if let Some(error) = state.error {
        root = root.child(error_banner(t, error, cb));
    }
    if !snapshot.library.exists {
        let path = snapshot
            .root
            .as_deref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "--".to_owned());
        root = root.child(info_banner(
            t,
            RdmIcon::Alert,
            Severity::Warning,
            format!("下载目录不存在或无法访问：{path}"),
            vec![Button::new("monitor-missing-dir-retry")
                .small()
                .ghost()
                .label("重新扫描")
                .on_click(simple_click(&cb.rescan))
                .into_any_element()],
        ));
    }
    if snapshot.truncated {
        root = root.child(info_banner(
            t,
            RdmIcon::Alert,
            Severity::Warning,
            format!(
                "扫描已截断（达到 {MAX_SCAN_ENTRIES} 项 / 深度 {MAX_SCAN_DEPTH} 上限），以下数字是下限。"
            ),
            vec![Button::new("monitor-truncated-refresh")
                .small()
                .ghost()
                .label("重新扫描")
                .on_click(simple_click(&cb.rescan))
                .into_any_element()],
        ));
    }
    if is_stale(state, snapshot) {
        root = root.child(info_banner(
            t,
            RdmIcon::Clock,
            Severity::Warning,
            "数据可能已过期，点击刷新。",
            vec![Button::new("monitor-stale-refresh")
                .small()
                .ghost()
                .label("刷新")
                .on_click(simple_click(&cb.rescan))
                .into_any_element()],
        ));
    }

    root.child(kpi_row(t, state, snapshot, cb))
        .child(main_columns(t, state, snapshot, cb))
        .child(footer_notes(t, snapshot))
        .into_any_element()
}

fn error_banner(t: &ThemeTokens, error: &str, cb: &Cbs) -> AnyElement {
    info_banner(
        t,
        RdmIcon::ErrorCircle,
        Severity::Danger,
        format!("扫描下载目录失败：{error}"),
        vec![Button::new("monitor-error-retry")
            .small()
            .ghost()
            .label("重试")
            .on_click(simple_click(&cb.rescan))
            .into_any_element()],
    )
    .into_any_element()
}

/// 快照是否过期（设计 §1.4：超过 2 分钟标黄）。
fn is_stale(state: &MonitorViewState<'_>, snapshot: &MonitorSnapshot) -> bool {
    snapshot.scanned_at_ms > 0
        && state.now_ms.saturating_sub(snapshot.scanned_at_ms) > STALE_AFTER_MS
}

// ---------------------------------------------------------------------------
// KPI 卡
// ---------------------------------------------------------------------------

fn kpi_row(
    t: &ThemeTokens,
    state: &MonitorViewState<'_>,
    snapshot: &MonitorSnapshot,
    cb: &Cbs,
) -> AnyElement {
    h_flex()
        .w_full()
        .flex_wrap()
        .gap(px(12.0))
        .child(kpi_library(t, snapshot))
        .child(kpi_disk(t, snapshot))
        .child(kpi_ai(t, state, snapshot, cb))
        .child(kpi_reclaim(t, snapshot))
        .into_any_element()
}

fn kpi_library(t: &ThemeTokens, snapshot: &MonitorSnapshot) -> AnyElement {
    let library = &snapshot.library;
    let exists = library.exists;
    let value = if exists {
        format_bytes(library.total_bytes)
    } else {
        "--".to_owned()
    };
    let explain = div()
        .font(theme::mono_font())
        .text_size(px(FS_MICRO))
        .text_color(t.text_secondary)
        .child(if exists {
            format!(
                "{} 个文件 · {} 个临时文件",
                library.total_files, snapshot.hygiene.part_files
            )
        } else {
            "下载目录不可用".to_owned()
        })
        .into_any_element();
    let visual = snapshot.disk.and_then(|disk| {
        (exists && disk.total_bytes > 0 && library.total_bytes > 0).then(|| {
            let fraction =
                (library.total_bytes as f64 / disk.total_bytes as f64).clamp(0.0, 1.0) as f32;
            h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_1().min_w(px(0.0)).child(progress_bar_colored(
                    t,
                    Some(fraction),
                    4.0,
                    t.accent,
                )))
                .child(
                    div()
                        .flex_none()
                        .font(theme::mono_font())
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(format!("占磁盘 {}", format_percent(Some(fraction)))),
                )
                .into_any_element()
        })
    });
    kpi_card(
        t,
        "下载库占用",
        RdmIcon::Disk,
        t.accent,
        t.accent_soft,
        value,
        None,
        explain,
        visual,
    )
}

fn kpi_disk(t: &ThemeTokens, snapshot: &MonitorSnapshot) -> AnyElement {
    match snapshot.disk {
        Some(disk) => {
            let used = disk.used_fraction();
            let color = disk_bar_color(t, used);
            let explain = div()
                .font(theme::mono_font())
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child(format!(
                    "共 {} · 已用 {}",
                    format_bytes(disk.total_bytes),
                    format_percent(used)
                ))
                .into_any_element();
            let visual = Some(progress_bar_colored(t, used, 4.0, color).into_any_element());
            kpi_card(
                t,
                "磁盘空间",
                RdmIcon::Gauge,
                t.accent,
                t.accent_soft,
                format_bytes(disk.free_bytes),
                Some("可用"),
                explain,
                visual,
            )
        }
        None => {
            let explain = div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child("无法读取卷容量")
                .into_any_element();
            kpi_card(
                t,
                "磁盘空间",
                RdmIcon::Gauge,
                t.accent,
                t.accent_soft,
                "--".to_owned(),
                None,
                explain,
                None,
            )
        }
    }
}

/// AI 卡片的渲染模式（设计 §4.4）：**只要有历史用量就永远渲染数据**，
/// 只有「当前未配置且从未用过」才显示「AI 助手未配置」空态，两者之间给一行弱提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiViewMode {
    /// 未配置且没有任何历史用量：空态 + 「去设置 AI」。
    Unconfigured,
    /// 当前未启用 / 未配置，但账本或下载项里有历史用量：照常渲染数据 + 弱提示。
    HistoryOnly,
    /// 已配置：正常渲染。
    Live,
}

/// 是否有任何 AI 历史用量（账本累计 / 解析次数 / 下载项归属三者任一）。
fn ai_has_history(ai: &AiUsageSummary) -> bool {
    ai.has_activity() || ai.lifetime.total_tokens > 0 || !ai.item_tokens.is_zero()
}

/// 三处 AI 卡片共用同一判断：数据优先于「未配置」空态。
fn ai_view_mode(ai: &AiUsageSummary, configured: bool) -> AiViewMode {
    if configured {
        AiViewMode::Live
    } else if ai_has_history(ai) {
        AiViewMode::HistoryOnly
    } else {
        AiViewMode::Unconfigured
    }
}

/// `HistoryOnly` 的一行弱提示（避免用空态盖掉已有的历史数据）。
fn ai_history_hint(t: &ThemeTokens) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap(px(6.0))
        .child(RdmIcon::Info.sized(12.0, t.text_tertiary))
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("AI 当前未启用，以下为历史记录"),
        )
        .into_any_element()
}

/// 账本累计 token；账本为空时回退到下载项归属合计（旧账本 / 演示数据）。
fn ai_lifetime_usage(ai: &AiUsageSummary) -> TokenUsage {
    if ai.lifetime.total_tokens > 0 {
        ai.lifetime
    } else {
        ai.item_tokens
    }
}

/// KPI「AI 消耗」的主数字与单位（设计 14 §5.2）：
/// 有价目时费用是主视觉（`≈ ¥x`），无价目时仍以 token 为主——
/// 没有价目就**绝不**显示 `¥0.00` 假装有数据。
fn ai_kpi_value(ai: &AiUsageSummary) -> (String, &'static str) {
    if ai.show_cost() && ai_has_history(ai) {
        (format_cny_approx(ai.cost_cny), "估算")
    } else {
        (
            format_tokens_maybe_estimated(&ai_lifetime_usage(ai)),
            "token",
        )
    }
}

/// 明细卡「费用小结」：`≈ ¥0.42 · 解析 ¥0.30 · 对话 ¥0.02`；
/// 没有价目返回 `None`（UI 不渲染费用行）。
fn format_cost_breakdown(ai: &AiUsageSummary) -> Option<String> {
    if !ai.show_cost() {
        return None;
    }
    let mut text = format_cny_approx(ai.cost_cny);
    text.push_str(&format!(" · 解析 {}", format_cny(ai.cost_parse)));
    if ai.cost_chat > 0.0 {
        text.push_str(&format!(" · 对话 {}", format_cny(ai.cost_chat)));
    }
    Some(text)
}

/// 无价目调用提示（仅存在未计价调用时返回）。
fn format_unpriced_hint(ai: &AiUsageSummary) -> Option<String> {
    (ai.unpriced_calls > 0).then(|| format!("{} 次调用无价目，未计入费用", ai.unpriced_calls))
}

fn kpi_ai(
    t: &ThemeTokens,
    state: &MonitorViewState<'_>,
    snapshot: &MonitorSnapshot,
    cb: &Cbs,
) -> AnyElement {
    let ai = &snapshot.ai;
    let configured = state.ai_configured || snapshot.ai.configured;
    let mode = ai_view_mode(ai, configured);

    if mode == AiViewMode::Unconfigured {
        let explain = div()
            .text_size(px(FS_MICRO))
            .text_color(t.text_secondary)
            .child("AI 助手未配置 · 启用后统计每次解析消耗")
            .into_any_element();
        let visual = Some(
            h_flex()
                .w_full()
                .child(
                    Button::new("monitor-kpi-ai-settings")
                        .small()
                        .ghost()
                        .icon(RdmIcon::Sparkle.icon())
                        .label("去设置 AI")
                        .tooltip("打开 设置 · AI 助手")
                        .on_click(simple_click(&cb.open_ai_settings)),
                )
                .into_any_element(),
        );
        return kpi_card(
            t,
            "AI 消耗",
            RdmIcon::Sparkle,
            t.info,
            t.info_soft,
            "--".to_owned(),
            None,
            explain,
            visual,
        );
    }

    // 有历史数据就展示真实数字（当前是否启用不影响历史账本）。
    let has_history = ai_has_history(ai);
    let token_text = format_tokens_maybe_estimated(&ai_lifetime_usage(ai));
    // 有价目 → 主数字是估算花费；无价目 → token 主数字 + 「不估算费用」提示。
    let (value, unit) = if has_history {
        ai_kpi_value(ai)
    } else {
        (token_text.clone(), "token")
    };
    let explain = if has_history {
        let mut line = v_flex().w_full().gap(px(3.0)).child(
            div()
                .font(theme::mono_font())
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child(format!(
                    "{token_text} tokens · {} 次解析 · 缓存命中 {} 次",
                    ai.parse_calls, ai.parse_cache_hits
                )),
        );
        if !ai.cost_known {
            line = line.child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_tertiary)
                    .child("无价目（自定义/未知模型），不估算费用"),
            );
        }
        if mode == AiViewMode::HistoryOnly {
            line = line.child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_tertiary)
                    .child("AI 当前未启用，以下为历史记录"),
            );
        }
        line.into_any_element()
    } else {
        div()
            .text_size(px(FS_MICRO))
            .text_color(t.text_secondary)
            .child("还没有 AI 解析记录；下一次解析后会出现在这里")
            .into_any_element()
    };
    let visual = has_history.then(|| mini_trend(t, ai, MINI_CHART_H));
    kpi_card(
        t,
        "AI 消耗",
        RdmIcon::Sparkle,
        t.info,
        t.info_soft,
        value,
        Some(unit),
        explain,
        visual,
    )
}

fn kpi_reclaim(t: &ThemeTokens, snapshot: &MonitorSnapshot) -> AnyElement {
    let hygiene = &snapshot.hygiene;
    let untracked = hygiene
        .untracked_bytes
        .max(snapshot.library.untracked_bytes);
    let untracked_files = hygiene
        .untracked_files
        .max(snapshot.library.untracked_files);
    let total = untracked.saturating_add(hygiene.part_bytes);
    let clean = total == 0;
    let mut explain = h_flex().flex_wrap().items_center().gap(px(6.0)).child(
        div()
            .font(theme::mono_font())
            .text_size(px(FS_MICRO))
            .text_color(t.text_secondary)
            .child(format!(
                "未归属 {untracked_files} 个 · 临时 {} 个",
                hygiene.part_files
            )),
    );
    if clean && snapshot.library.total_bytes == 0 {
        // 没有进度条可承载「干净」语义时，用 success 小字补上。
        explain = explain.child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.success)
                .child("很干净"),
        );
    }
    let explain = explain.into_any_element();
    // 细条比例 = 待整理 / 下载库总量（不再是「待整理内部占比」，
    // 否则永远 100% 全满，视觉上像磁盘告急）；干净时用 success 语义。
    let visual = (snapshot.library.total_bytes > 0).then(|| {
        let fraction = (total as f64 / snapshot.library.total_bytes as f64).clamp(0.0, 1.0) as f32;
        let color = if clean { t.success } else { t.warning };
        h_flex()
            .w_full()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w(px(0.0)).child(progress_bar_colored(
                t,
                Some(fraction),
                4.0,
                color,
            )))
            .child(
                div()
                    .flex_none()
                    .font(theme::mono_font())
                    .text_size(px(FS_MICRO))
                    .text_color(if clean { t.success } else { t.text_tertiary })
                    .child(if clean {
                        "很干净".to_owned()
                    } else {
                        format!("占下载库 {}", format_percent(Some(fraction)))
                    }),
            )
            .into_any_element()
    });
    kpi_card(
        t,
        "待整理空间",
        RdmIcon::Inbox,
        if clean { t.success } else { t.warning },
        if clean {
            t.success_soft
        } else {
            t.warning_soft
        },
        format_bytes(total),
        None,
        explain,
        visual,
    )
}

/// KPI 卡外壳（设计 §4.1）：图标 chip + 标题 → 大字 + 单位 → 一句解释 → 可选视觉。
#[allow(clippy::too_many_arguments)]
fn kpi_card(
    t: &ThemeTokens,
    title: &str,
    icon: RdmIcon,
    accent: Hsla,
    tint: Hsla,
    value: String,
    unit: Option<&str>,
    explain: AnyElement,
    visual: Option<AnyElement>,
) -> AnyElement {
    let mut card = v_flex()
        .flex_1()
        .min_w(px(KPI_MIN_W))
        .min_h(px(112.0))
        .gap(px(10.0))
        .p(px(CARD_PAD))
        .rounded(px(RADIUS_LG))
        .bg(t.bg_surface)
        .border_1()
        .border_color(t.border_subtle)
        .child(
            h_flex()
                .gap(px(8.0))
                .items_center()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(24.0))
                        .rounded(px(RADIUS_SM))
                        .bg(tint)
                        .child(icon.sized(14.0, accent)),
                )
                .child(
                    div()
                        .text_size(px(FS_CAPTION))
                        .text_color(t.text_secondary)
                        .child(title.to_owned()),
                ),
        )
        .child(
            h_flex()
                .gap(px(4.0))
                .items_baseline()
                .child(
                    div()
                        .font(theme::mono_font())
                        .text_size(px(FS_DISPLAY))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_primary)
                        .child(value),
                )
                .when_some(unit, |this, unit| {
                    this.child(
                        div()
                            .flex_none()
                            .text_size(px(FS_CAPTION))
                            .text_color(t.text_tertiary)
                            .child(unit.to_owned()),
                    )
                }),
        )
        // 解释行撑满剩余高度，把「视觉」压到卡片底部，同排卡片底部对齐。
        .child(div().flex_1().min_h(px(0.0)).child(explain));
    if let Some(visual) = visual {
        card = card.child(visual);
    }
    card.into_any_element()
}

// ---------------------------------------------------------------------------
// 主区两列
// ---------------------------------------------------------------------------

fn main_columns(
    t: &ThemeTokens,
    state: &MonitorViewState<'_>,
    snapshot: &MonitorSnapshot,
    cb: &Cbs,
) -> AnyElement {
    let configured = state.ai_configured || snapshot.ai.configured;
    let ai_mode = ai_view_mode(&snapshot.ai, configured);
    let main = v_flex()
        .flex_1()
        .min_w(px(MAIN_COL_MIN_W))
        .gap(px(SECTION_GAP))
        .child(composition_card(t, &snapshot.library, cb))
        .child(largest_files_card(t, &snapshot.library, state.now_ms, cb))
        .child(ai_detail_card(t, &snapshot.ai, ai_mode, cb));
    let side = v_flex()
        .flex_1()
        .min_w(px(SIDE_COL_MIN_W))
        .max_w(px(SIDE_COL_MAX_W))
        .gap(px(SECTION_GAP))
        .child(ai_trend_card(t, &snapshot.ai, ai_mode, cb))
        .child(transfer_card(t, &snapshot.transfer))
        .child(hygiene_card(t, snapshot, cb));
    h_flex()
        .w_full()
        .flex_wrap()
        .items_start()
        .gap(px(SECTION_GAP))
        .child(main)
        .child(side)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 卡片外壳 / 卡片头 / 通用小部件
// ---------------------------------------------------------------------------

/// 区块卡片：`bg_surface` + 1px 描边 + RADIUS_LG（与 KPI 卡同一层级）。
fn card(t: &ThemeTokens) -> Div {
    v_flex()
        .w_full()
        .flex_shrink_0()
        .gap(px(CARD_GAP))
        .p(px(CARD_PAD))
        .rounded(px(RADIUS_LG))
        .bg(t.bg_surface)
        .border_1()
        .border_color(t.border_subtle)
}

/// 卡片头：图标 + 标题 + 可选副标题 + 右侧动作。
fn card_header(
    t: &ThemeTokens,
    icon: RdmIcon,
    title: &str,
    subtitle: Option<String>,
    trailing: Option<AnyElement>,
) -> AnyElement {
    let mut header = h_flex()
        .w_full()
        .items_center()
        .gap(px(8.0))
        .child(icon.sized(14.0, t.text_tertiary))
        .child(
            div()
                .flex_none()
                .text_size(px(FS_ROW_TITLE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text_primary)
                .child(title.to_owned()),
        );
    header = match subtitle {
        Some(subtitle) => header.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(subtitle),
        ),
        None => header.child(div().flex_1()),
    };
    header.children(trailing).into_any_element()
}

/// 卡片内空态（图标 chip + 标题 + 说明 + 可选动作）。
fn inline_empty(
    t: &ThemeTokens,
    icon: RdmIcon,
    title: &str,
    description: &str,
    action: Option<AnyElement>,
) -> AnyElement {
    v_flex()
        .w_full()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .py(px(22.0))
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(36.0))
                .rounded(px(RADIUS_MD))
                .bg(t.bg_sunken)
                .child(icon.sized(18.0, t.text_tertiary)),
        )
        .child(
            div()
                .text_size(px(FS_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_secondary)
                .child(title.to_owned()),
        )
        .child(
            div()
                .max_w(px(280.0))
                .text_center()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(description.to_owned()),
        )
        .children(action)
        .into_any_element()
}

/// AI 未配置统一空态（趋势 / 明细卡共用）。
fn ai_not_configured(t: &ThemeTokens, cb: &Cbs) -> AnyElement {
    inline_empty(
        t,
        RdmIcon::Sparkle,
        "AI 助手未配置",
        "启用后可在这里看到每次解析消耗的 token",
        Some(
            Button::new("monitor-ai-settings-card")
                .small()
                .icon(RdmIcon::Sparkle.icon())
                .label("去设置 AI")
                .tooltip("打开 设置 · AI 助手")
                .on_click(simple_click(&cb.open_ai_settings))
                .into_any_element(),
        ),
    )
}

/// 「标签 + 等宽值」信息行（维护 / 传输卡片用）。
fn info_row(t: &ThemeTokens, label: &str, value: String) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap(px(8.0))
        .h(px(26.0))
        .child(
            div()
                .flex_none()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_secondary)
                .child(label.to_owned()),
        )
        .child(div().flex_1())
        .child(mono_text(value, FS_MONO_MICRO, t.text_primary))
        .into_any_element()
}

/// 横向堆叠条：`segments` 的比例之和应为 1；段间用 2px `border_r` 制造间隙。
fn stacked_bar(t: &ThemeTokens, height: f32, segments: &[(f32, Hsla)]) -> AnyElement {
    let mut row = h_flex()
        .w_full()
        .h(px(height))
        .rounded(px(999.0))
        .bg(t.track)
        .overflow_hidden();
    let count = segments.len();
    for (index, (fraction, color)) in segments.iter().enumerate() {
        if *fraction <= 0.0 {
            continue;
        }
        let mut segment = div()
            .flex_none()
            .h_full()
            .w(relative(fraction.clamp(0.0, 1.0)))
            .bg(*color);
        if index + 1 < count {
            segment = segment.border_r_2().border_color(t.bg_surface);
        }
        row = row.child(segment);
    }
    row.into_any_element()
}

/// 迷你 14 桶趋势（KPI 卡）：今天 accent，其余降低 alpha。
fn mini_trend(t: &ThemeTokens, ai: &AiUsageSummary, height: f32) -> AnyElement {
    let values = trend_values(&ai.trend);
    let max = values.iter().copied().max().unwrap_or(0);
    let count = values.len();
    let mut row = h_flex().w_full().h(px(height)).items_end().gap(px(3.0));
    for (index, value) in values.iter().enumerate() {
        let bar_h = bar_height(*value, max, height);
        let today = index + 1 == count;
        row = row.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .h(px(bar_h))
                .rounded(px(1.0))
                .bg(if today {
                    t.accent
                } else {
                    t.accent.opacity(0.35)
                }),
        );
    }
    row.into_any_element()
}

// ---------------------------------------------------------------------------
// 空间构成
// ---------------------------------------------------------------------------

fn composition_card(t: &ThemeTokens, library: &LibraryUsage, cb: &Cbs) -> AnyElement {
    let total = library.total_bytes;
    let header = card_header(
        t,
        RdmIcon::Gauge,
        "空间构成",
        Some("按文件类型统计".to_owned()),
        Some(
            icon_button(
                "monitor-composition-open-dir",
                RdmIcon::FolderOpen,
                "在资源管理器中打开下载目录",
                simple_click(&cb.open_download_dir),
            )
            .into_any_element(),
        ),
    );

    let mut root = card(t).child(header).child(divider(t));

    if total == 0 || library.categories.is_empty() {
        // 空态：灰色空条 + 一句解释（设计 §4.2）。
        return root
            .child(
                div()
                    .w_full()
                    .h(px(COMPOSITION_BAR_H))
                    .rounded(px(999.0))
                    .bg(t.track),
            )
            .child(inline_empty(
                t,
                RdmIcon::Inbox,
                "下载目录还是空的",
                "下载完成后会按文件类型统计占用",
                None,
            ))
            .into_any_element();
    }

    let bytes: Vec<u64> = library.categories.iter().map(|usage| usage.bytes).collect();
    let fractions = stack_fractions(&bytes);
    let segments: Vec<(f32, Hsla)> = library
        .categories
        .iter()
        .enumerate()
        .filter(|(_, usage)| usage.bytes > 0)
        .map(|(index, usage)| (fractions[index], category_color(t, usage.category, index)))
        .collect();
    root = root.child(stacked_bar(t, COMPOSITION_BAR_H, &segments));

    let mut legend = v_flex().w_full().gap(px(2.0));
    for (index, usage) in library.categories.iter().enumerate() {
        legend = legend.child(composition_row(t, usage, index, total, cb));
    }
    root.child(legend)
        .child(divider(t))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("合计"),
                )
                .child(div().flex_1())
                .child(mono_text(
                    format_bytes(total),
                    FS_MONO_MICRO,
                    t.text_primary,
                ))
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(format!("{} 个文件", library.total_files)),
                ),
        )
        .into_any_element()
}

/// 分类图例行：色块 + 分类 + 字节 + 数量 + 百分比；整行可点击回下载库筛选。
fn composition_row(
    t: &ThemeTokens,
    usage: &CategoryUsage,
    index: usize,
    total_bytes: u64,
    cb: &Cbs,
) -> AnyElement {
    let color = category_color(t, usage.category, index);
    let group: SharedString = format!("monitor-category-{index}").into();
    let hover_bg = t.bg_hover;
    let percent = format_percent(usage.fraction_of(total_bytes));
    let tooltip_text = format!(
        "回下载库查看「{}」（{} · {} 个文件）",
        usage.category.label_zh(),
        format_bytes(usage.bytes),
        usage.files
    );
    div()
        .id(("monitor-category-row", index))
        .group(group.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .h(px(32.0))
        .px(px(8.0))
        .rounded(px(RADIUS_SM))
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg))
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .on_click(category_click(&cb.jump_category, usage.category))
        .child(div().flex_none().size(px(8.0)).rounded(px(2.0)).bg(color))
        .child(
            div()
                .flex_none()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_primary)
                .child(usage.category.label_zh()),
        )
        .child(div().flex_1())
        .child(mono_text(
            format_bytes(usage.bytes),
            FS_MONO_MICRO,
            t.text_secondary,
        ))
        .child(
            div()
                .flex_none()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(format!("{} 个", usage.files)),
        )
        .child(
            div()
                .flex_none()
                .w(px(44.0))
                .text_right()
                .font(theme::mono_font())
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child(percent),
        )
        .child(
            div()
                .flex_none()
                .opacity(0.35)
                .group_hover(group, move |style| style.opacity(1.0))
                .child(RdmIcon::ChevronRight.sized(12.0, t.text_tertiary)),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 最大的文件
// ---------------------------------------------------------------------------

fn largest_files_card(
    t: &ThemeTokens,
    library: &LibraryUsage,
    now_ms: u64,
    cb: &Cbs,
) -> AnyElement {
    let mut root = card(t)
        .child(card_header(
            t,
            RdmIcon::FileDown,
            "最大的文件",
            Some("按文件大小排序 · 最多 8 个".to_owned()),
            Some(
                icon_button(
                    "monitor-largest-open-dir",
                    RdmIcon::FolderOpen,
                    "在资源管理器中打开下载目录",
                    simple_click(&cb.open_download_dir),
                )
                .into_any_element(),
            ),
        ))
        .child(divider(t));

    if library.largest_files.is_empty() {
        return root
            .child(inline_empty(
                t,
                RdmIcon::File,
                "暂无可显示的文件",
                "下载目录里还没有文件，或当前目录不可访问",
                None,
            ))
            .into_any_element();
    }

    for (index, file) in library.largest_files.iter().take(8).enumerate() {
        root = root.child(largest_file_row(t, file, index, now_ms));
    }
    root.into_any_element()
}

fn largest_file_row(t: &ThemeTokens, file: &FileUsage, index: usize, now_ms: u64) -> AnyElement {
    let path_label = if file.relative_dir.is_empty() {
        file.name.clone()
    } else {
        format!("{}\\{}", file.relative_dir, file.name)
    };
    let tooltip_text = if file.tracked {
        format!("{path_label}（在下载记录中）")
    } else {
        format!("{path_label}（不在下载记录中，未归属）")
    };
    h_flex()
        .id(("monitor-largest-file", index))
        .w_full()
        .items_center()
        .gap(px(8.0))
        .h(px(36.0))
        .px(px(4.0))
        .rounded(px(RADIUS_XS))
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .child(
            div()
                .flex_none()
                .w(px(16.0))
                .child(file.category.icon().sized(14.0, t.text_tertiary)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_primary)
                .child(ellipsize_file_name(&file.name, 32)),
        )
        .when(!file.tracked, |this| {
            this.child(soft_tag("未归属", t.warning, t.warning_soft))
        })
        .child(soft_tag(
            file.category.label_zh(),
            t.text_secondary,
            t.bg_sunken,
        ))
        .child(div().flex_none().child(mono_text(
            format_bytes(file.bytes),
            FS_MONO_MICRO,
            t.text_primary,
        )))
        .child(
            div()
                .flex_none()
                .w(px(58.0))
                .text_right()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(format_relative_ms(now_ms, file.modified_ms)),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// AI 使用明细
// ---------------------------------------------------------------------------

/// 明细卡头摘要：`共 4 项 · 400 tokens · ≈ ¥0.0006`。
/// token 是解析/探索/工作流等 AI 调用合计（AI 分类已于 2026-10-22 移除）；金额只在有价目时
/// 追加（无价目不硬塞 ¥0）。
fn format_item_summary(ai: &AiUsageSummary) -> String {
    let mut summary = format!(
        "共 {} 项 · {} tokens",
        ai.items_with_ai,
        format_tokens(ai.item_tokens.total_tokens)
    );
    if ai.show_cost() {
        summary.push_str(&format!(" · {}", format_cny_approx(ai.item_cost_cny)));
    }
    summary
}

/// 明细卡副标题：费用成为主视觉后，排序说明与之一致（数据仍按 token 排序，
/// 金额与 token 同向，因此不需要改排序逻辑）。
fn ai_detail_subtitle(ai: &AiUsageSummary) -> &'static str {
    if ai.show_cost() {
        "最贵的下载项 · 按 token / 估算费用排序"
    } else {
        "最贵的下载项 · 按 token 排序"
    }
}

fn ai_detail_card(t: &ThemeTokens, ai: &AiUsageSummary, mode: AiViewMode, cb: &Cbs) -> AnyElement {
    let has_history = ai_has_history(ai);
    let header = card_header(
        t,
        RdmIcon::Bot,
        "AI 使用明细",
        Some(ai_detail_subtitle(ai).to_owned()),
        (ai.items_with_ai > 0 && !ai.top_items.is_empty()).then(|| {
            mono_text(format_item_summary(ai), FS_MONO_MICRO, t.text_secondary).into_any_element()
        }),
    );
    let mut root = card(t).child(header).child(divider(t));

    if mode == AiViewMode::Unconfigured {
        return root.child(ai_not_configured(t, cb)).into_any_element();
    }
    if mode == AiViewMode::HistoryOnly {
        root = root.child(ai_history_hint(t));
    }
    if has_history {
        // 设计 §1.2「按种类 / 按模型 token + 费用」的落点。
        root = root.child(ai_breakdown_strip(t, ai));
        if !ai.models.is_empty() {
            root = root.child(ai_models_strip(t, ai));
        }
        root = root.child(divider(t));
    }

    if ai.top_items.is_empty() {
        let (title, description) = if has_history {
            (
                "AI 已产生消耗，但还没有归属到下载项",
                "解析后创建下载，会在这里按项汇总",
            )
        } else {
            ("还没有 AI 解析记录", "下一次解析后会出现在这里")
        };
        root = root.child(inline_empty(t, RdmIcon::Sparkle, title, description, None));
    } else {
        for (index, item) in ai.top_items.iter().take(8).enumerate() {
            root = root.child(ai_detail_row(t, item, index, ai.show_cost(), cb));
        }
    }

    // 无价目调用：如实注明未计入费用，不把缺失价目算成 ¥0。
    if let Some(hint) = format_unpriced_hint(ai) {
        root = root.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(6.0))
                .child(RdmIcon::Info.sized(12.0, t.text_tertiary))
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(hint),
                ),
        );
    }
    root.into_any_element()
}

/// 「解析 / 对话」token 小结 + 费用小结（有价目时才加费用行）。
fn ai_breakdown_strip(t: &ThemeTokens, ai: &AiUsageSummary) -> AnyElement {
    let mut lines = v_flex()
        .w_full()
        .gap(px(3.0))
        .child(ai_kind_tokens_row(t, ai));
    if let Some(cost_text) = format_cost_breakdown(ai) {
        lines = lines.child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("费用"),
                )
                .child(detail_chip(cost_text, t.info)),
        );
    }
    lines.into_any_element()
}

/// 「解析 / 对话」token 行（各类型自带 ≈ 估算标记）。
fn ai_kind_tokens_row(t: &ThemeTokens, ai: &AiUsageSummary) -> AnyElement {
    let mut row = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(px(6.0))
        .child(detail_chip(
            format!("解析 {}", format_tokens_maybe_estimated(&ai.parse)),
            t.text_secondary,
        ));
    if ai.chat.total_tokens > 0 {
        row = row.child(dot(t)).child(detail_chip(
            format!("对话 {}", format_tokens_maybe_estimated(&ai.chat)),
            t.text_secondary,
        ));
    }
    // 数字共用末尾单位，避免「解析 391 · 对话 400」看不出是 token。
    row = row.child(
        div()
            .flex_none()
            .text_size(px(FS_MICRO))
            .text_color(t.text_tertiary)
            .child("tokens"),
    );
    row.into_any_element()
}

/// 「按模型」token + 费用行（最多 3 个，超出显示 +N）。
fn ai_models_strip(t: &ThemeTokens, ai: &AiUsageSummary) -> AnyElement {
    let mut row = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .flex_none()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(if ai.show_cost() {
                    "按模型 · 估算价"
                } else {
                    "按模型"
                }),
        );
    for model in ai.models.iter().take(3) {
        let mut label = format!(
            "{} {} tokens",
            model.model,
            format_tokens_maybe_estimated(&model.tokens)
        );
        if ai.show_cost() && model.cost_cny > 0.0 {
            label.push_str(&format!(" · {}", format_cny(model.cost_cny)));
        }
        row = row.child(soft_tag(label, t.info, t.info_soft));
    }
    if ai.models.len() > 3 {
        row = row.child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(format!("+{} 个模型", ai.models.len() - 3)),
        );
    }
    row.into_any_element()
}

fn ai_detail_row(
    t: &ThemeTokens,
    item: &ItemTokenUsage,
    index: usize,
    cost_known: bool,
    cb: &Cbs,
) -> AnyElement {
    let hover_bg = t.bg_hover;
    let mut tooltip_text = format!(
        "在下载库中查看「{}」\ntoken 为 AI 解析合计（{}）",
        item.file_name,
        format_tokens_exact(item.tokens.total_tokens)
    );
    if cost_known && item.cost_cny > 0.0 {
        tooltip_text.push_str(&format!("\n估算花费 {}", format_cny_approx(item.cost_cny)));
    }
    let mut detail = h_flex().gap(px(8.0)).flex_wrap().items_center();
    detail = detail.child(detail_chip(
        format!("解析 {}", format_tokens(item.parse.total_tokens)),
        t.text_secondary,
    ));
    if item.cache_hit {
        detail = detail.child(badge(
            ("monitor-cache-badge", index),
            RdmIcon::CheckCircle,
            "缓存命中".to_owned(),
            t.success,
            t.success_soft,
            Some("该次解析命中缓存，未产生 token 消耗".to_owned()),
        ));
    }
    if item.parse_siblings > 1 {
        detail = detail.child(badge(
            ("monitor-share-badge", index),
            RdmIcon::Info,
            format!("共享 {} 项", item.parse_siblings),
            t.info,
            t.info_soft,
            Some("解析 token 为该次解析的整次消耗，由这些下载项共享".to_owned()),
        ));
    }
    if let Some(size) = item.size_bytes {
        detail = detail.child(detail_chip(format_bytes(size), t.text_tertiary));
    }

    div()
        .id(("monitor-ai-item", index))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .px(px(8.0))
        .py(px(7.0))
        .rounded(px(RADIUS_SM))
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg))
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .on_click(id_click(&cb.select_item, item.id.clone()))
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(3.0))
                .child(
                    h_flex()
                        .gap(px(6.0))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(FS_CAPTION))
                                .text_color(t.text_primary)
                                .child(item.file_name.clone()),
                        )
                        .when_some(item.model.clone(), |this, model| {
                            this.child(soft_tag(model, t.info, t.info_soft))
                        }),
                )
                .child(detail),
        )
        .child(
            v_flex()
                .flex_none()
                .items_end()
                .gap(px(1.0))
                .child(
                    h_flex()
                        .items_baseline()
                        .gap(px(3.0))
                        .child(mono_text(
                            format_tokens_maybe_estimated(&item.tokens),
                            FS_MONO,
                            t.text_primary,
                        ))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(FS_MICRO))
                                .text_color(t.text_tertiary)
                                .child("tokens"),
                        ),
                )
                .when(cost_known && item.cost_cny > 0.0, |this| {
                    this.child(detail_chip(
                        format_cny_approx(item.cost_cny),
                        t.text_secondary,
                    ))
                }),
        )
        .child(
            div()
                .flex_none()
                .opacity(0.35)
                .child(RdmIcon::ChevronRight.sized(12.0, t.text_tertiary)),
        )
        .into_any_element()
}

/// 明细行里的小字信息（比 badge 更弱）。
fn detail_chip(text: String, color: Hsla) -> AnyElement {
    div()
        .flex_none()
        .font(theme::mono_font())
        .text_size(px(FS_MICRO))
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// 小徽标（图标 + 文案），可带一条解释性 tooltip。
fn badge(
    id: (&'static str, usize),
    icon: RdmIcon,
    label: String,
    foreground: Hsla,
    background: Hsla,
    tooltip: Option<String>,
) -> AnyElement {
    let mut root = h_flex()
        .id(id)
        .flex_none()
        .items_center()
        .gap(px(3.0))
        .h(px(18.0))
        .px(px(5.0))
        .rounded(px(RADIUS_XS))
        .bg(background)
        .text_color(foreground)
        .child(icon.sized(11.0, foreground))
        .child(div().text_size(px(FS_MICRO)).child(label));
    if let Some(tooltip) = tooltip {
        root = root.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));
    }
    root.into_any_element()
}

// ---------------------------------------------------------------------------
// AI 消耗趋势
// ---------------------------------------------------------------------------

fn ai_trend_card(t: &ThemeTokens, ai: &AiUsageSummary, mode: AiViewMode, cb: &Cbs) -> AnyElement {
    let has_history = ai_has_history(ai);
    let header = card_header(
        t,
        RdmIcon::Sparkle,
        "AI 消耗趋势",
        Some("近 14 天 · 滚动 24 小时".to_owned()),
        (mode != AiViewMode::Unconfigured).then(|| {
            mono_text(
                format!(
                    "近 7 天 {} token",
                    format_tokens_maybe_estimated(&ai.last_7())
                ),
                FS_MONO_MICRO,
                t.text_secondary,
            )
            .into_any_element()
        }),
    );
    let mut root = card(t).child(header).child(divider(t));

    if mode == AiViewMode::Unconfigured {
        return root.child(ai_not_configured(t, cb)).into_any_element();
    }
    if mode == AiViewMode::HistoryOnly {
        root = root.child(ai_history_hint(t));
    }
    // 柱状图仍是 token；费用单独一行（有价目才显示），避免改图表的统计口径。
    if ai.show_cost() {
        root = root.child(ai_cost_window_strip(t, ai));
    }

    // 有数据（哪怕全 0）时始终保留柱状图 + 轴标签，不用整卡空态。
    let values = trend_values(&ai.trend);
    let labels = trend_axis_labels();
    root = root
        .child(trend_chart(t, &values, TREND_CHART_H))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(labels[0]),
                )
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(labels[1]),
                )
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(labels[2]),
                ),
        )
        .child(divider(t))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(12.0))
                .child(stat_block(
                    t,
                    "今日",
                    format_tokens_maybe_estimated(&ai.today()),
                ))
                .child(stat_block(
                    t,
                    "累计",
                    format_tokens(ai_lifetime_usage(ai).total_tokens),
                ))
                .when(ai.estimated_calls > 0, |this| {
                    this.child(stat_block(t, "估算", format!("{} 次", ai.estimated_calls)))
                }),
        );
    if !has_history {
        root = root.child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("还没有 AI 解析记录；下一次解析后会出现在这里"),
        );
    }
    root.into_any_element()
}

/// 趋势卡费用行：`费用 近 7 天 ≈ ¥x · 今日 ≈ ¥y`（仅在有价目时渲染）。
fn ai_cost_window_strip(t: &ThemeTokens, ai: &AiUsageSummary) -> AnyElement {
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .flex_none()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("费用"),
        )
        .child(detail_chip(
            format!("近 7 天 {}", format_cny_approx(ai.cost_last_7)),
            t.text_primary,
        ))
        .child(dot(t))
        .child(detail_chip(
            format!("今日 {}", format_cny_approx(ai.cost_today)),
            t.text_secondary,
        ))
        .into_any_element()
}

/// 趋势柱状图：今天 accent、其余降低 alpha；柱可 hover 看当日数值。
fn trend_chart(t: &ThemeTokens, values: &[u64], height: f32) -> AnyElement {
    let max = values.iter().copied().max().unwrap_or(0);
    let count = values.len();
    let mut row = h_flex().w_full().h(px(height)).items_end().gap(px(4.0));
    for (index, value) in values.iter().enumerate() {
        let bar_h = bar_height(*value, max, height);
        let today = index + 1 == count;
        let color = if today {
            t.accent
        } else {
            t.accent.opacity(0.35)
        };
        let tooltip_text = format!(
            "{} · {}",
            trend_bucket_label(index, count),
            format_tokens(*value)
        );
        row = row.child(
            div()
                .id(("monitor-trend-bar", index))
                .flex_1()
                .min_w(px(0.0))
                .h(px(bar_h))
                .rounded(px(2.0))
                .bg(color)
                .hover(move |style| style.bg(t.accent))
                .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx)),
        );
    }
    row.into_any_element()
}

/// 趋势卡底部的小统计块（标签 + 等宽数字）。
fn stat_block(t: &ThemeTokens, label: &str, value: String) -> AnyElement {
    v_flex()
        .flex_none()
        .gap(px(1.0))
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(label.to_owned()),
        )
        .child(mono_text(value, FS_MONO_MICRO, t.text_primary))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 传输活动
// ---------------------------------------------------------------------------

/// 传输状态分类（分布条 + 图例顺序固定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl TransferKind {
    const ALL: [TransferKind; 5] = [
        TransferKind::Downloading,
        TransferKind::Paused,
        TransferKind::Completed,
        TransferKind::Failed,
        TransferKind::Cancelled,
    ];

    fn label_zh(self) -> &'static str {
        match self {
            Self::Downloading => "下载中",
            Self::Paused => "已暂停",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
        }
    }

    fn count(self, transfer: &TransferSummary) -> u64 {
        match self {
            Self::Downloading => transfer.downloading,
            Self::Paused => transfer.paused,
            Self::Completed => transfer.completed,
            Self::Failed => transfer.failed,
            Self::Cancelled => transfer.cancelled,
        }
    }

    fn color(self, t: &ThemeTokens) -> Hsla {
        match self {
            Self::Downloading => t.accent,
            Self::Paused => t.warning,
            Self::Completed => t.success,
            Self::Failed => t.danger,
            Self::Cancelled => t.text_tertiary,
        }
    }
}

/// 非零状态及其计数（分布条/图例共用；纯函数便于单测）。
fn transfer_breakdown(transfer: &TransferSummary) -> Vec<(TransferKind, u64)> {
    TransferKind::ALL
        .iter()
        .copied()
        .map(|kind| (kind, kind.count(transfer)))
        .filter(|(_, count)| *count > 0)
        .collect()
}

fn transfer_card(t: &ThemeTokens, transfer: &TransferSummary) -> AnyElement {
    let header = card_header(
        t,
        RdmIcon::DownloadCloud,
        "传输活动",
        Some(format!("共 {} 条下载记录", transfer.total)),
        None,
    );
    let mut root = card(t).child(header).child(divider(t));

    if transfer.total == 0 {
        return root
            .child(inline_empty(
                t,
                RdmIcon::Inbox,
                "还没有下载记录",
                "开始下载后，这里会显示当前速度与各状态的数量",
                None,
            ))
            .into_any_element();
    }

    let breakdown = transfer_breakdown(transfer);
    let counts: Vec<u64> = breakdown.iter().map(|(_, count)| *count).collect();
    let fractions = stack_fractions(&counts);
    let segments: Vec<(f32, Hsla)> = fractions
        .iter()
        .zip(breakdown.iter())
        .map(|(fraction, (kind, _))| (*fraction, kind.color(t)))
        .collect();
    let mut legend = h_flex().w_full().flex_wrap().items_center().gap(px(10.0));
    for (kind, count) in breakdown.iter() {
        legend = legend.child(legend_item(t, kind.label_zh(), *count, kind.color(t)));
    }

    root = root
        .child(stacked_bar(t, TRANSFER_BAR_H, &segments))
        .child(legend)
        .child(divider(t));

    if transfer.downloading > 0 {
        root = root.child(
            v_flex()
                .w_full()
                .gap(px(2.0))
                .child(
                    h_flex()
                        .items_baseline()
                        .gap(px(6.0))
                        .child(mono_text(
                            format_speed(transfer.current_speed_bps),
                            FS_H1,
                            t.text_primary,
                        ))
                        .child(
                            div()
                                .text_size(px(FS_MICRO))
                                .text_color(t.text_tertiary)
                                .child("当前速度"),
                        ),
                )
                .child(
                    div()
                        .font(theme::mono_font())
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_secondary)
                        .child(format!(
                            "{} 个连接 · 进行中剩余 {}",
                            transfer.active_connections,
                            format_bytes(transfer.active_remaining_bytes)
                        )),
                ),
        );
    } else {
        root = root
            .child(
                div()
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_secondary)
                    .child("当前没有进行中的下载"),
            )
            .child(
                div()
                    .font(theme::mono_font())
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_tertiary)
                    .child(format!("{} 个已完成", transfer.completed)),
            );
    }

    root.child(divider(t))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(12.0))
                .child(stat_block(
                    t,
                    "累计已下载",
                    format_bytes(transfer.downloaded_bytes),
                ))
                .child(stat_block(
                    t,
                    "已完成字节",
                    format_bytes(transfer.completed_bytes),
                )),
        )
        .into_any_element()
}

fn legend_item(t: &ThemeTokens, label: &str, count: u64, color: Hsla) -> AnyElement {
    h_flex()
        .flex_none()
        .items_center()
        .gap(px(5.0))
        .child(div().flex_none().size(px(8.0)).rounded_full().bg(color))
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child(label.to_owned()),
        )
        .child(mono_text(count.to_string(), FS_MONO_MICRO, t.text_primary))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 维护与数据
// ---------------------------------------------------------------------------

fn hygiene_card(t: &ThemeTokens, snapshot: &MonitorSnapshot, cb: &Cbs) -> AnyElement {
    let hygiene = &snapshot.hygiene;
    let mut root = card(t)
        .child(card_header(
            t,
            RdmIcon::Archive,
            "维护与数据",
            Some("RDM 本机占用".to_owned()),
            None,
        ))
        .child(divider(t))
        .child(info_row(
            t,
            "状态目录",
            format_bytes(hygiene.state_dir_bytes),
        ))
        .child(info_row(
            t,
            "AI 解析缓存",
            format!(
                "{} · {} 条",
                format_bytes(hygiene.ai_cache_bytes),
                hygiene.ai_cache_entries
            ),
        ))
        .child(info_row(t, "日志", format_bytes(hygiene.logs_bytes)))
        .child(info_row(
            t,
            "下载记录",
            format!("{} 条", snapshot.transfer.total),
        ))
        .child(info_row(
            t,
            "临时文件",
            format!(
                "{} 个 · {}",
                hygiene.part_files,
                format_bytes(hygiene.part_bytes)
            ),
        ));
    if snapshot.skipped_dirs > 0 {
        root = root.child(info_row(
            t,
            "跳过的目录",
            format!("{} 个", snapshot.skipped_dirs),
        ));
    }
    root.child(divider(t))
        .child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(8.0))
                .child(
                    Button::new("monitor-open-state-dir")
                        .small()
                        .icon(RdmIcon::FolderOpen.icon())
                        .label("打开状态目录")
                        .tooltip("在资源管理器中打开 RDM 状态目录（日志、缓存、任务记录）")
                        .on_click(simple_click(&cb.open_state_dir)),
                )
                .child(
                    Button::new("monitor-open-download-dir")
                        .small()
                        .ghost()
                        .icon(RdmIcon::Folder.icon())
                        .label("打开下载目录")
                        .tooltip("在资源管理器中打开下载目录")
                        .on_click(simple_click(&cb.open_download_dir)),
                ),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 页脚说明
// ---------------------------------------------------------------------------

fn footer_notes(t: &ThemeTokens, snapshot: &MonitorSnapshot) -> AnyElement {
    let mut root = v_flex()
        .w_full()
        .gap(px(4.0))
        .pt(px(12.0))
        .border_t_1()
        .border_color(t.border_subtle);
    if snapshot.truncated {
        root = root.child(footer_line(
            format!(
                "扫描已截断（达到 {MAX_SCAN_ENTRIES} 项 / 深度 {MAX_SCAN_DEPTH} 上限），以上数字是下限。"
            ),
            t.warning,
        ));
    }
    root = root
        .child(footer_line(
            "数据来自本机扫描与下载记录；token 来自模型返回的用量（≈ 表示估算）。不会上传。"
                .to_owned(),
            t.text_tertiary,
        ))
        .child(footer_line(
            format!(
                "按文件大小统计（非簇占用）；最多扫描 {MAX_SCAN_ENTRIES} 个目录项、深度 {MAX_SCAN_DEPTH}，跳过符号链接与无权限目录。"
            ),
            t.text_tertiary,
        ))
        .child(footer_line(
            format!(
                "未归属 = 磁盘上存在但不在下载记录中的文件，只报告不删除。本次扫描耗时 {}ms。",
                snapshot.scan_ms
            ),
            t.text_tertiary,
        ));
    // 费用口径：价目快照说明 + 「估算不是账单」（设计 14 §5.2）。
    if let Some(note) = snapshot.ai.pricing_note.as_deref() {
        root = root.child(footer_line(note.to_owned(), t.text_tertiary));
    }
    if snapshot.ai.show_cost() || snapshot.ai.pricing_note.is_some() {
        root = root.child(footer_line(
            "费用为按目录价估算，不代表账单；DeepSeek 工作高峰/空闲时段价格不同。".to_owned(),
            t.text_tertiary,
        ));
    }
    if snapshot.skipped_dirs > 0 {
        root = root.child(footer_line(
            format!("已跳过 {} 个无法读取的目录。", snapshot.skipped_dirs),
            t.text_tertiary,
        ));
    }
    if snapshot.ai.estimated_calls > 0 {
        root = root.child(footer_line(
            format!(
                "AI 用量中有 {} 次按字符估算（provider 未返回 usage）。",
                snapshot.ai.estimated_calls
            ),
            t.text_tertiary,
        ));
    }
    root.into_any_element()
}

fn footer_line(text: String, color: Hsla) -> AnyElement {
    div()
        .text_size(px(FS_MICRO))
        .text_color(color)
        .child(text)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 纯计算 helper（不依赖 GPUI 运行时，便于单测）
// ---------------------------------------------------------------------------

/// 柱高归一化：0 → 最小高度（2px），等于最大值 → 满高，其余按比例。
fn bar_height(value: u64, max: u64, chart_height: f32) -> f32 {
    if !chart_height.is_finite() || chart_height <= 0.0 {
        return 0.0;
    }
    let min_height = BAR_MIN_H.min(chart_height);
    if max == 0 || value == 0 {
        return min_height;
    }
    let ratio = value.min(max) as f32 / max as f32;
    (chart_height * ratio).clamp(min_height, chart_height)
}

/// 堆叠段比例：和为 1（最后一段吃掉浮点误差）；计数为 0 的段返回 0。
fn stack_fractions(parts: &[u64]) -> Vec<f32> {
    let total: u64 = parts.iter().copied().sum();
    if total == 0 {
        return vec![0.0; parts.len()];
    }
    let last_non_zero = parts.iter().rposition(|part| *part > 0);
    let mut fractions = Vec::with_capacity(parts.len());
    let mut accumulated = 0.0_f32;
    for (index, part) in parts.iter().enumerate() {
        if *part == 0 {
            fractions.push(0.0);
            continue;
        }
        let fraction = if Some(index) == last_non_zero {
            (1.0 - accumulated).max(0.0)
        } else {
            let value = (*part as f64 / total as f64) as f32;
            accumulated += value;
            value
        };
        fractions.push(fraction);
    }
    fractions
}

/// 趋势桶数值（不足 14 桶时左补 0，保证柱状图结构稳定）。
fn trend_values(buckets: &[TokenUsage]) -> Vec<u64> {
    let mut values: Vec<u64> = buckets
        .iter()
        .rev()
        .take(TREND_BUCKETS)
        .rev()
        .map(|bucket| bucket.total_tokens)
        .collect();
    while values.len() < TREND_BUCKETS {
        values.insert(0, 0);
    }
    values
}

/// 趋势柱的日期标签（`index` 从旧到新）。
fn trend_bucket_label(index: usize, count: usize) -> String {
    if count == 0 {
        return "今天".to_owned();
    }
    let from_end = count.saturating_sub(1).saturating_sub(index.min(count - 1));
    match from_end {
        0 => "今天".to_owned(),
        1 => "昨天".to_owned(),
        days => format!("{days} 天前"),
    }
}

/// 分类色：同一强调色的明度梯度（「其他」固定弱化色，设计 §4.2）。
fn category_color(t: &ThemeTokens, category: FileCategory, index: usize) -> Hsla {
    if category == FileCategory::Other {
        return t.text_tertiary;
    }
    const ALPHAS: [f32; 7] = [1.0, 0.82, 0.68, 0.55, 0.44, 0.34, 0.26];
    let alpha = ALPHAS[index.min(ALPHAS.len() - 1)];
    if alpha >= 1.0 {
        t.accent
    } else {
        t.accent.opacity(alpha)
    }
}

/// 磁盘占用条颜色：≥92% danger、≥78% warning，其余 accent（设计 §5 语义色）。
fn disk_bar_color(t: &ThemeTokens, used: Option<f32>) -> Hsla {
    match used {
        Some(value) if value >= 0.92 => t.danger,
        Some(value) if value >= 0.78 => t.warning,
        _ => t.accent,
    }
}

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

    #[test]
    fn bar_height_keeps_baseline_and_full_scale() {
        assert_eq!(bar_height(0, 0, 64.0), 2.0);
        assert_eq!(bar_height(0, 100, 64.0), 2.0);
        assert_eq!(bar_height(100, 100, 64.0), 64.0);
        assert_eq!(bar_height(50, 100, 64.0), 32.0);
        assert!((bar_height(10, 100, 64.0) - 6.4).abs() < 1e-5);
        assert_eq!(bar_height(10, 100, 0.0), 0.0);
    }

    #[test]
    fn stack_fractions_sum_to_one_and_skip_zero_parts() {
        let fractions = stack_fractions(&[1, 3, 0, 4]);
        assert_eq!(fractions.len(), 4);
        assert_eq!(fractions[2], 0.0);
        let sum: f32 = fractions.iter().sum();
        assert!((sum - 1.0).abs() < f32::EPSILON, "sum = {sum}");
        assert!((fractions[0] - 0.125).abs() < 1e-6);
        assert!((fractions[3] - 0.5).abs() < 1e-6);
        assert_eq!(stack_fractions(&[0, 0]), vec![0.0, 0.0]);
    }

    #[test]
    fn trend_values_pads_to_fourteen_buckets() {
        let values = trend_values(&[]);
        assert_eq!(values.len(), TREND_BUCKETS);
        assert!(values.iter().all(|value| *value == 0));

        let mut buckets = vec![TokenUsage::ZERO; 3];
        buckets[0] = usage(10);
        buckets[1] = usage(20);
        buckets[2] = usage(30);
        let values = trend_values(&buckets);
        assert_eq!(values.len(), TREND_BUCKETS);
        assert_eq!(&values[TREND_BUCKETS - 3..], &[10, 20, 30]);
    }

    #[test]
    fn trend_bucket_labels_use_relative_days() {
        assert_eq!(trend_bucket_label(13, 14), "今天");
        assert_eq!(trend_bucket_label(12, 14), "昨天");
        assert_eq!(trend_bucket_label(7, 14), "6 天前");
        assert_eq!(trend_bucket_label(0, 14), "13 天前");
        assert_eq!(trend_bucket_label(0, 0), "今天");
    }

    #[test]
    fn transfer_breakdown_keeps_order_and_skips_empty_states() {
        let transfer = TransferSummary {
            total: 5,
            downloading: 1,
            paused: 0,
            completed: 3,
            failed: 1,
            cancelled: 0,
            active_connections: 4,
            current_speed_bps: 1024.0,
            active_remaining_bytes: 150,
            downloaded_bytes: 160,
            completed_bytes: 100,
        };
        let breakdown = transfer_breakdown(&transfer);
        assert_eq!(
            breakdown.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            vec![
                TransferKind::Downloading,
                TransferKind::Completed,
                TransferKind::Failed
            ]
        );
        let sum: u64 = breakdown.iter().map(|(_, count)| *count).sum();
        assert_eq!(sum, transfer.total);
    }

    #[test]
    fn disk_bar_color_uses_thresholds() {
        let t = crate::ui::theme::light(crate::model::AccentPreset::Blue);
        assert_eq!(disk_bar_color(&t, None), t.accent);
        assert_eq!(disk_bar_color(&t, Some(0.5)), t.accent);
        assert_eq!(disk_bar_color(&t, Some(0.8)), t.warning);
        assert_eq!(disk_bar_color(&t, Some(0.95)), t.danger);
    }

    #[test]
    fn category_gradient_keeps_other_neutral() {
        let t = crate::ui::theme::light(crate::model::AccentPreset::Blue);
        assert_eq!(category_color(&t, FileCategory::Other, 0), t.text_tertiary);
        assert_eq!(category_color(&t, FileCategory::Video, 0), t.accent);
        let second = category_color(&t, FileCategory::Audio, 1);
        assert_ne!(second, t.accent);
    }

    #[test]
    fn stale_detection_uses_two_minutes() {
        let state = MonitorViewState {
            snapshot: None,
            scanning: false,
            error: None,
            last_scan_label: None,
            ai_configured: false,
            download_dir: None,
            now_ms: STALE_AFTER_MS + 2_000,
        };
        assert!(!is_stale(&state, &MonitorSnapshot::empty()));
        let mut snapshot = MonitorSnapshot::empty();
        snapshot.scanned_at_ms = 1_000;
        assert!(is_stale(&state, &snapshot));
        let mut fresh = MonitorSnapshot::empty();
        fresh.scanned_at_ms = state.now_ms - 1_000;
        assert!(!is_stale(&state, &fresh));
    }

    #[test]
    fn ai_mode_prefers_history_over_unconfigured_state() {
        // 未配置 + 无历史 → 空态。
        let mut ai = AiUsageSummary::default();
        assert!(!ai_has_history(&ai));
        assert_eq!(ai_view_mode(&ai, false), AiViewMode::Unconfigured);

        // 账本里有历史（即使当前配置已关闭）→ 必须渲染数据，不能显示「未配置」。
        ai.lifetime = usage(3_630);
        assert!(ai_has_history(&ai));
        assert_eq!(ai_view_mode(&ai, false), AiViewMode::HistoryOnly);
        assert_eq!(ai_view_mode(&ai, true), AiViewMode::Live);

        // 已配置但从未用过：正常渲染（0 + 提示），不是「未配置」空态。
        let fresh = AiUsageSummary::default();
        assert_eq!(ai_view_mode(&fresh, true), AiViewMode::Live);

        // 账本丢失但下载项有归属用量，也算历史。
        let item_only = AiUsageSummary {
            item_tokens: usage(10),
            ..AiUsageSummary::default()
        };
        assert!(ai_has_history(&item_only));
        assert_eq!(ai_view_mode(&item_only, false), AiViewMode::HistoryOnly);
    }

    #[test]
    fn kpi_ai_uses_estimated_cost_only_when_price_is_known() {
        let mut ai = AiUsageSummary {
            lifetime: usage(3_630),
            ..AiUsageSummary::default()
        };
        // 无价目：主数字保持 token，且单位不是「估算」。
        let (value, unit) = ai_kpi_value(&ai);
        assert_eq!(unit, "token");
        assert_eq!(value, "3.63K");

        // 有价目：主数字切到 `≈ ¥x`，单位「估算」。
        ai.cost_known = true;
        ai.cost_cny = 0.42;
        let (value, unit) = ai_kpi_value(&ai);
        assert_eq!(unit, "估算");
        assert!(value.starts_with("≈ ¥"), "value = {value}");
        assert_eq!(value, format_cny_approx(0.42));

        // 无历史时即使 cost_known 也不显示费用（不伪造 0 元）。
        let cost_only = AiUsageSummary {
            cost_known: true,
            cost_cny: 0.42,
            ..AiUsageSummary::default()
        };
        assert_eq!(ai_kpi_value(&cost_only).1, "token");
    }

    #[test]
    fn cost_breakdown_and_unpriced_hint_follow_price_availability() {
        let mut ai = AiUsageSummary {
            lifetime: usage(100),
            ..AiUsageSummary::default()
        };
        assert!(format_cost_breakdown(&ai).is_none());
        assert!(format_unpriced_hint(&ai).is_none());

        ai.cost_known = true;
        ai.cost_cny = 0.42;
        ai.cost_parse = 0.30;
        ai.cost_chat = 0.02;
        let text = format_cost_breakdown(&ai).expect("priced breakdown");
        assert!(text.starts_with("≈ ¥0.42"), "text = {text}");
        assert!(text.contains("解析"));
        assert!(!text.contains("分类"), "AI 分类已移除，不再展示: {text}");
        assert!(text.contains("对话"));

        // 没有对话消耗时不硬塞「对话 ¥0.00」。
        ai.cost_chat = 0.0;
        let text = format_cost_breakdown(&ai).expect("priced breakdown");
        assert!(!text.contains("对话"), "text = {text}");

        ai.unpriced_calls = 3;
        assert_eq!(
            format_unpriced_hint(&ai).as_deref(),
            Some("3 次调用无价目，未计入费用")
        );
    }

    #[test]
    fn item_summary_keeps_token_unit_and_hides_unpriced_cost() {
        // 无价目：`共 4 项 · 400 tokens`（不能出现裸数字 400，也不硬塞 ¥）。
        let mut ai = AiUsageSummary {
            items_with_ai: 4,
            item_tokens: usage(400),
            ..AiUsageSummary::default()
        };
        let text = format_item_summary(&ai);
        assert_eq!(text, "共 4 项 · 400 tokens");
        assert!(!text.contains('¥'), "text = {text}");

        // 有价目：追加 `≈ ¥x`，单位仍在。
        ai.lifetime = usage(400);
        ai.cost_known = true;
        ai.item_cost_cny = 0.0006;
        let text = format_item_summary(&ai);
        assert!(text.contains("400 tokens"), "text = {text}");
        assert!(text.ends_with(&format_cny_approx(0.0006)), "text = {text}");
    }

    #[test]
    fn detail_subtitle_mentions_cost_ordering_when_priced() {
        let plain = AiUsageSummary::default();
        assert_eq!(ai_detail_subtitle(&plain), "最贵的下载项 · 按 token 排序");

        let priced = AiUsageSummary {
            cost_known: true,
            cost_cny: 0.01,
            lifetime: usage(100),
            ..AiUsageSummary::default()
        };
        assert_eq!(
            ai_detail_subtitle(&priced),
            "最贵的下载项 · 按 token / 估算费用排序"
        );
    }
}
