//! 组件展示窗（`--ui=kit`，M1 验收用）。
//!
//! 展示 `docs/design/05-component-library.md` 的基础组件 × 六态 × 亮/暗 ×
//! 舒适/紧凑 × 中文/数字矩阵，并复用主窗口（`views/app.rs`）的真实行样式：
//!
//! ```text
//! 01 按钮 Button      A1/A2/A3/A28：变体 / 尺寸 / primary+secondary 六态
//! 02 输入 Input       A4/A5/A6：单行 cleanable+前后缀 / 掩码 / 搜索 / 多行 auto_grow / 校验错误 / 禁用
//! 03 选择控件         A7–A11：Checkbox / Switch / RadioGroup / Select / Slider
//! 04 状态与反馈       A14–A16/A21/A22：状态 Pill / 进度条 / 不确定态 / Tag / Badge / Toast / Banner
//! 05 列表行           B3：舒适 64 / 紧凑 40 + default/hover/selected/focused/多选
//! 06 排版             04 §4.2 的 11 档字号 + 中文/数字混排 + 等宽数字 + 文件名省略
//! 07 图标             04 §6：RdmIcon 全量网格 + 16/20/24 三档尺寸
//! 08 空态/导航        A23/A24/A25：empty_state / nav_item（展开+折叠）/ Kbd 组合
//! ```
//!
//! 纪律：
//! - 只引用 [`crate::ui::theme::tokens`] 的语义 token，不写色值字面量；
//! - 所有切换走 `theme::apply(...)` + `window.refresh()`，即时预览（仅本窗口内存态，
//!   不写入用户设置）；
//! - 中文文案 + 显式 CJK fallback（`theme::ui_font()`），无豆腐块；
//! - 内容区可滚动、全部 flex + wrap，窗口缩到 960×600 不产生横向滚动。

use gpui::prelude::*;
use gpui::{
    div, px, relative, AnyElement, App, Context, ElementId, Entity, FontWeight, Hsla, IntoElement,
    Keystroke, ParentElement, Render, SharedString, Styled, Subscription, Window,
};
use gpui_component::badge::Badge;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::kbd::Kbd;
use gpui_component::progress::Progress;
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::tag::Tag;
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, Disableable, Root, Selectable, Sizable, Size};

use crate::model::{AccentPreset, Density, ItemState, Settings, UiThemeMode};
use crate::ui::icons::RdmIcon;
use crate::ui::theme;
use crate::ui::theme::tokens;
use crate::ui::widgets::{
    divider, ellipsize_file_name, empty_state, info_banner, kbd, mono_text, nav_item, progress_bar,
    section_label, soft_tag, status_pill, Severity,
};

// ---------------------------------------------------------------------------
// 尺寸常量（对齐 04 §5 / 05 §0.2）
// ---------------------------------------------------------------------------

/// 卡片之间的大间距（space.5 附近）。
const CARD_GAP: f32 = 20.0;
/// 卡片内部元素间距。
const INNER_GAP: f32 = 12.0;
/// demo 字段固定宽度（960px 窗口下两列可容纳）。
const FIELD_W: f32 = 340.0;

// ---------------------------------------------------------------------------
// 视图状态
// ---------------------------------------------------------------------------

/// 输入框集合（`InputState` 必须在 window 下创建，见 08 D6 / API 速查 §3.1）。
struct ShowcaseInputs {
    single: Entity<InputState>,
    masked: Entity<InputState>,
    search: Entity<InputState>,
    multiline: Entity<InputState>,
    invalid: Entity<InputState>,
    disabled: Entity<InputState>,
}

/// 受控选择控件状态（组件不自动维护，见 API 速查 §11-11）。
struct ShowcaseControls {
    select: Entity<SelectState<Vec<String>>>,
    select_value: Option<String>,
    slider: Entity<SliderState>,
    slider_value: f32,
    checkbox_checked: bool,
    checkbox_plain: bool,
    switch_on: bool,
    switch_off: bool,
}

/// 组件展示视图。
pub struct Showcase {
    /// 工具栏选择的主题模式（含「跟随系统」）。
    mode: UiThemeMode,
    /// 解析后的实际明暗（`FollowSystem` 时取窗口外观）。
    dark: bool,
    accent: AccentPreset,
    density: Density,
    reduce_motion: bool,
    /// 真实按钮点击后的反馈文案（证明 hover/pressed 之外还有可交互反馈）。
    last_action: String,
    inputs: ShowcaseInputs,
    controls: ShowcaseControls,
    /// 保留订阅句柄；drop 即退订。
    _subscriptions: Vec<Subscription>,
}

impl Showcase {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, settings: &Settings) -> Self {
        let mode = settings.resolved_ui_theme();
        let dark = theme::resolve_dark(mode, window);

        let single = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("粘贴下载链接，或搜索任务…")
                .clean_on_escape()
        });
        let masked = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-…（API Key，掩码显示）")
                .masked(true)
                .default_value("sk-rdm-demo-9f3a17c2")
        });
        // 搜索型：单行只加 cleanable；`searchable(true)` 是 debug_assert（仅多行），会崩溃。
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索任务…")
                .clean_on_escape()
        });
        let multiline = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(2, 4)
                .soft_wrap(true)
                .default_value("https://example.com/ubuntu-24.04.1-desktop-amd64.iso\nhttps://example.com/models/模型权重.bin")
                .placeholder("批量添加：每行一个链接")
        });
        let invalid = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("ftp://mirror.example.com/file.zip")
                .validate(|value: &str, _cx| {
                    value.is_empty()
                        || value.starts_with("http://")
                        || value.starts_with("https://")
                })
        });
        let disabled =
            cx.new(|cx| InputState::new(window, cx).placeholder("引擎暂停时不可编辑（S5 禁用态）"));

        let select = cx.new(|cx| {
            SelectState::new(
                vec![
                    "全部来源".to_string(),
                    "浏览器扩展".to_string(),
                    "命令行".to_string(),
                    "剪贴板".to_string(),
                    "批量导入".to_string(),
                ],
                None,
                window,
                cx,
            )
        });
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(10.0)
                .step(0.1)
                .default_value(2.0)
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &select,
            window,
            |this, _, event: &SelectEvent<Vec<String>>, _, cx| {
                let SelectEvent::Confirm(value) = event;
                this.controls.select_value = value.clone();
                cx.notify();
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &slider,
            window,
            |this, _, event: &SliderEvent, _, cx| {
                let SliderEvent::Change(value) = event;
                this.controls.slider_value = value.end();
                cx.notify();
            },
        ));

        Self {
            mode,
            dark,
            accent: settings.accent,
            density: settings.density,
            reduce_motion: settings.reduce_motion,
            last_action: "—".to_string(),
            inputs: ShowcaseInputs {
                single,
                masked,
                search,
                multiline,
                invalid,
                disabled,
            },
            controls: ShowcaseControls {
                select,
                select_value: None,
                slider,
                slider_value: 2.0,
                checkbox_checked: true,
                checkbox_plain: false,
                switch_on: true,
                switch_off: false,
            },
            _subscriptions: subscriptions,
        }
    }

    /// 当前状态摘要（工具栏右上角展示，证明切换即时生效）。
    fn state_summary(&self) -> String {
        let mode = match self.mode {
            UiThemeMode::FollowSystem => {
                format!(
                    "跟随系统（实际{}）",
                    if self.dark { "深色" } else { "浅色" }
                )
            }
            UiThemeMode::Light => "浅色".to_string(),
            UiThemeMode::Dark => "深色".to_string(),
        };
        format!(
            "模式：{mode} · 强调色：{} · 密度：{}（{}px）· 动效：{}",
            self.accent.label_zh(),
            self.density.label_zh(),
            self.density.row_height() as i32,
            if self.reduce_motion {
                "减少"
            } else {
                "完整"
            }
        )
    }

    /// 应用外观变更：写全局 token → `gpui-component` Theme → 立刻重绘。
    fn apply_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        theme::apply(cx, self.dark, self.accent, self.reduce_motion);
        window.refresh();
        cx.notify();
    }
}

impl Render for Showcase {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tokens(cx);
        let content = self.render_content(&t, cx);
        // 给滚动内容一个确定宽度：滚动容器在 GPUI/taffy 下宽度不确定，
        // 百分比宽度（w_full）会退化为收缩包裹，卡片会按内容缩窄。
        // 用窗口逻辑宽度减左右 24px 内边距显式给定，窗口缩放时随重绘更新。
        let content_width = (window.viewport_size().width - px(48.0)).max(px(560.0));

        v_flex()
            .size_full()
            .bg(t.bg_app)
            .text_color(t.text_primary)
            .font(theme::ui_font())
            .child(self.render_toolbar(&t, cx))
            .child(
                div()
                    .id("kit-scroll")
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .p(px(24.0))
                    .child(div().w(content_width).child(content)),
            )
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

// ---------------------------------------------------------------------------
// 顶部工具栏：主题 / 强调色 / 密度 / 减少动效（全部即时预览）
// ---------------------------------------------------------------------------

impl Showcase {
    fn render_toolbar(&self, t: &theme::ThemeTokens, cx: &Context<Self>) -> AnyElement {
        let mut mode_buttons = h_flex().gap(px(4.0));
        for mode in UiThemeMode::ALL {
            mode_buttons = mode_buttons.child(
                Button::new(ElementId::Name(format!("kit-mode-{mode:?}").into()))
                    .small()
                    .selected(self.mode == mode)
                    .label(mode.label_zh())
                    .tooltip(match mode {
                        UiThemeMode::FollowSystem => "跟随 Windows 应用明暗模式（按窗口外观解析）",
                        UiThemeMode::Light => "浅色主题",
                        UiThemeMode::Dark => "深色主题",
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.mode = mode;
                        this.dark = theme::resolve_dark(mode, window);
                        this.apply_appearance(window, cx);
                    })),
            );
        }

        let mut accent_swatches = h_flex().gap(px(8.0)).items_center();
        for (index, preset) in AccentPreset::ALL.into_iter().enumerate() {
            let selected = self.accent == preset;
            // 色板预览取该预设在当前明暗下的 accent / on-accent（仍是 theme 模块的 token）。
            let preview = if self.dark {
                theme::dark(preset)
            } else {
                theme::light(preset)
            };
            accent_swatches = accent_swatches.child(
                div()
                    .id(ElementId::Name(format!("kit-accent-{index}").into()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(26.0))
                    .rounded_full()
                    .bg(preview.accent)
                    .border_1()
                    .border_color(if selected { t.focus } else { t.border_default })
                    .when(selected, |this| this.border_2())
                    .cursor_pointer()
                    .tooltip(move |window, cx| {
                        Tooltip::new(format!("强调色：{}", preset.label_zh())).build(window, cx)
                    })
                    .when(selected, |this| {
                        this.child(RdmIcon::Check.sized(13.0, preview.on_accent))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.accent = preset;
                        this.apply_appearance(window, cx);
                    })),
            );
        }

        let mut density_buttons = h_flex().gap(px(4.0));
        for density in [Density::Comfortable, Density::Compact] {
            density_buttons = density_buttons.child(
                Button::new(ElementId::Name(format!("kit-density-{density:?}").into()))
                    .small()
                    .selected(self.density == density)
                    .icon(RdmIcon::Density.icon())
                    .label(density.label_zh())
                    .tooltip(format!(
                        "列表密度：{}（行高 {}px）",
                        density.label_zh(),
                        density.row_height() as i32
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.density = density;
                        window.refresh();
                        cx.notify();
                    })),
            );
        }

        v_flex()
            .flex_shrink_0()
            .w_full()
            .bg(t.bg_surface)
            .border_b_1()
            .border_color(t.border_subtle)
            .child(
                h_flex()
                    .h(px(44.0))
                    .px(px(16.0))
                    .gap(px(10.0))
                    .items_center()
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
                                    .child(RdmIcon::Palette.sized(15.0, t.accent)),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::FS_H2))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("RDM UI Kit · 组件展示"),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(theme::FS_CAPTION))
                            .text_color(t.text_secondary)
                            .child(self.state_summary()),
                    ),
            )
            .child(
                h_flex()
                    .px(px(16.0))
                    .pb(px(10.0))
                    .gap(px(12.0))
                    .items_center()
                    .flex_wrap()
                    .child(toolbar_group(t, "主题", mode_buttons.into_any_element()))
                    .child(vertical_rule(t))
                    .child(toolbar_group(
                        t,
                        "强调色",
                        accent_swatches.into_any_element(),
                    ))
                    .child(vertical_rule(t))
                    .child(toolbar_group(t, "密度", density_buttons.into_any_element()))
                    .child(vertical_rule(t))
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .child(
                                Switch::new("kit-reduce-motion")
                                    .checked(self.reduce_motion)
                                    .label("减少动效")
                                    .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                        this.reduce_motion = *checked;
                                        this.apply_appearance(window, cx);
                                    })),
                            )
                            .child(micro(
                                t,
                                if self.reduce_motion {
                                    "时长 → 0"
                                } else {
                                    "800ms / 250ms"
                                },
                                t.text_tertiary,
                            )),
                    )
                    .child(div().flex_1())
                    .child(micro(
                        t,
                        "切换即时生效 · 仅本窗口预览，不写设置",
                        t.text_tertiary,
                    )),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// 内容分区
// ---------------------------------------------------------------------------

impl Showcase {
    fn render_content(&self, t: &theme::ThemeTokens, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .gap(px(CARD_GAP))
            .child(card(
                t,
                "01",
                "按钮 Button",
                "A1 / A2 / A3 / A28 · 变体 × 尺寸 × 六态（hover/pressed 用真实组件）",
                self.render_buttons(t, cx),
            ))
            .child(card(
                t,
                "02",
                "输入 Input",
                "A4 / A5 / A6 · 单行（cleanable/prefix/suffix）· 掩码 · 搜索 · 多行 auto_grow · 校验错误 · 禁用",
                self.render_inputs(t),
            ))
            .child(card(
                t,
                "03",
                "选择控件 Selection",
                "A7–A11 · Checkbox / Switch / RadioGroup / Select / Slider（受控，状态实时显示）",
                self.render_selection(t, cx),
            ))
            .child(card(
                t,
                "04",
                "状态与反馈 Status",
                "A14–A16 / A21 / A22 · 5 种 ItemState · 进度条 · Spinner · Tag/Badge · Toast · Banner",
                self.render_status(t),
            ))
            .child(card(
                t,
                "05",
                "列表行 List Row",
                "B3 · 舒适 64px / 紧凑 40px · default / hover / selected / focused / 多选（静态）",
                self.render_rows(t),
            ))
            .child(card(
                t,
                "06",
                "排版 Typography",
                "04 §4.2 · 11 档字号 / 字重 / 行高 · 中文+数字混排 · 等宽数字 · 长文件名省略",
                render_typography(t),
            ))
            .child(card(
                t,
                "07",
                "图标 Icons",
                "04 §6 · RdmIcon 全量网格 + 16 / 20 / 24 三档尺寸",
                render_icons(t),
            ))
            .child(card(
                t,
                "08",
                "空态 / 导航 Navigation",
                "A23 / A24 / A25 · empty_state · nav_item 展开与折叠 · Kbd 快捷键组合",
                self.render_empty_and_nav(t, cx),
            ))
            .into_any_element()
    }

    // -- 01 按钮 -----------------------------------------------------------

    fn render_buttons(&self, t: &theme::ThemeTokens, cx: &Context<Self>) -> AnyElement {
        let variants = h_flex()
            .flex_wrap()
            .gap(px(8.0))
            .items_center()
            .child(demo_action_button(cx, "kit-btn-primary", "开始下载", true))
            .child(demo_action_button(
                cx,
                "kit-btn-secondary",
                "打开文件夹",
                false,
            ))
            .child(
                Button::new("kit-btn-ghost")
                    .ghost()
                    .label("取消")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.last_action = "取消".to_string();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("kit-btn-danger")
                    .danger()
                    .label("删除文件…")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.last_action = "删除文件…".to_string();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("kit-btn-icon")
                    .ghost()
                    .compact()
                    .icon(RdmIcon::Settings.icon())
                    .tooltip("设置（图标按钮必须带 tooltip，A3）"),
            )
            .child(
                // SplitButton（A2）：主操作 + 下拉段，两枚真实 primary 按钮紧密组合。
                h_flex()
                    .gap(px(1.0))
                    .child(
                        Button::new("kit-btn-split-main")
                            .primary()
                            .icon(RdmIcon::Plus.icon())
                            .label("添加下载"),
                    )
                    .child(
                        Button::new("kit-btn-split-more")
                            .primary()
                            .icon(RdmIcon::ChevronDown.icon())
                            .tooltip("更多添加方式（拆分段）"),
                    ),
            )
            .into_any_element();

        let sizes = h_flex()
            .flex_wrap()
            .gap(px(8.0))
            .items_center()
            .child(
                Button::new("kit-btn-sm")
                    .primary()
                    .small()
                    .label("小按钮 sm"),
            )
            .child(Button::new("kit-btn-md").primary().label("默认按钮 md"))
            .child(
                Button::new("kit-btn-lg")
                    .primary()
                    .large()
                    .label("大按钮 lg"),
            )
            .child(
                Button::new("kit-btn-loading-real")
                    .primary()
                    .loading(true)
                    .label("正在添加"),
            )
            .child(
                Button::new("kit-btn-disabled-real")
                    .label("禁用按钮")
                    .disabled(true),
            )
            .into_any_element();

        v_flex()
            .gap(px(14.0))
            .child(demo_row(
                t,
                "变体 Variants（md）：primary / secondary(default) / ghost / danger / icon / split",
                variants,
            ))
            .child(demo_row(
                t,
                "尺寸 Sizes：sm 28 / md 32 / lg 36（loading 保宽、disabled 走 S5 配方）",
                sizes,
            ))
            .child(demo_row(
                t,
                "Primary 六态：S1 default / S2 hover / S3 pressed / S4 focus / S5 disabled / S6 loading",
                state_matrix(t, BtnSkin::Primary),
            ))
            .child(demo_row(
                t,
                "Secondary 六态：同一套 R1 配方（focus = 外 2px focus 环 + 内 1px surface 隔离）",
                state_matrix(t, BtnSkin::Secondary),
            ))
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .child(micro(
                        t,
                        format!("最近动作：{}（点击真实按钮验证 hover / pressed 内建状态）", self.last_action),
                        t.text_tertiary,
                    )),
            )
            .into_any_element()
    }

    // -- 02 输入 -----------------------------------------------------------

    fn render_inputs(&self, t: &theme::ThemeTokens) -> AnyElement {
        let single = Input::new(&self.inputs.single)
            .w_full()
            .cleanable(true)
            .prefix(RdmIcon::Link.sized(14.0, t.text_tertiary))
            .suffix(
                h_flex()
                    .gap(px(2.0))
                    .child(kbd(t, "Ctrl"))
                    .child(kbd(t, "V")),
            )
            .into_any_element();

        let masked = Input::new(&self.inputs.masked)
            .w_full()
            .mask_toggle()
            .into_any_element();

        let search = Input::new(&self.inputs.search)
            .w_full()
            .cleanable(true)
            .prefix(RdmIcon::Search.sized(14.0, t.text_tertiary))
            .suffix(micro(t, "单行", t.text_tertiary))
            .into_any_element();

        let multiline = Input::new(&self.inputs.multiline)
            .w_full()
            .into_any_element();

        let invalid = Input::new(&self.inputs.invalid).w_full().into_any_element();

        let disabled = Input::new(&self.inputs.disabled)
            .w_full()
            .disabled(true)
            .into_any_element();

        v_flex()
            .gap(px(INNER_GAP))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(16.0))
                    .items_start()
                    .child(demo_field(
                        t,
                        "单行 · cleanable + prefix/suffix",
                        single,
                        "placeholder 只给示例值；Esc 清空（clean_on_escape）",
                    ))
                    .child(demo_field(
                        t,
                        "掩码 · API Key（masked + mask_toggle）",
                        masked,
                        "右侧眼睛切换明文；值不落日志",
                    ))
                    .child(demo_field(
                        t,
                        "搜索型 · 单行（SearchField）",
                        search,
                        "单行不要用 InputState::searchable(true)：debug_assert 仅多行",
                    )),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(16.0))
                    .items_start()
                    .child(
                        v_flex()
                            .w(px(480.0))
                            .flex_none()
                            .gap(px(6.0))
                            .child(field_label(t, "多行 · auto_grow(2,4) + soft_wrap"))
                            .child(multiline)
                            .child(micro(
                                t,
                                "每行一个链接；行数 ≥5 时内部滚动（A6 的 P0 降级形态）",
                                t.text_tertiary,
                            )),
                    )
                    .child(
                        v_flex()
                            .w(px(FIELD_W))
                            .flex_none()
                            .gap(px(6.0))
                            .child(field_label(t, "校验错误态 · validate + 下方说明"))
                            .child(invalid)
                            .child(
                                h_flex()
                                    .gap(px(4.0))
                                    .items_center()
                                    .child(RdmIcon::ErrorCircle.sized(12.0, t.danger_text))
                                    .child(micro(
                                        t,
                                        "请输入 http/https 链接（R2：边框 danger + 11px 说明）",
                                        t.danger_text,
                                    )),
                            ),
                    )
                    .child(demo_field(
                        t,
                        "禁用态 · S5",
                        disabled,
                        "文字 text.disabled、底 bg.sunken、描边 border.subtle",
                    )),
            )
            .into_any_element()
    }

    // -- 03 选择控件 -------------------------------------------------------

    fn render_selection(&self, t: &theme::ThemeTokens, cx: &Context<Self>) -> AnyElement {
        let checkboxes = v_flex()
            .gap(px(8.0))
            .child(
                Checkbox::new("kit-cb-1")
                    .checked(self.controls.checkbox_checked)
                    .label("完成后自动打开文件")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.controls.checkbox_checked = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("kit-cb-2")
                    .checked(self.controls.checkbox_plain)
                    .label("记住本次选择")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.controls.checkbox_plain = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("kit-cb-3")
                    .label("禁用态（S5，仍可 Tab 到达）")
                    .disabled(true),
            )
            .into_any_element();

        let switches = v_flex()
            .gap(px(8.0))
            .child(
                Switch::new("kit-sw-1")
                    .checked(self.controls.switch_on)
                    .label("开机自启动")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.controls.switch_on = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("kit-sw-2")
                    .checked(self.controls.switch_off)
                    .label("关闭后最小化到托盘")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.controls.switch_off = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("kit-sw-3")
                    .label("托盘不可用（禁用）")
                    .disabled(true),
            )
            .into_any_element();

        // RadioGroup 直接切换主题模式：与工具栏按钮同一条 theme::apply 路径。
        // 选项顺序必须与 UiThemeMode::ALL 一致（selected_index 用的是 children 下标）。
        let mut radio_group = RadioGroup::vertical("kit-radio-theme")
            .selected_index(Some(mode_index(self.mode)))
            .gap(px(6.0));
        for (index, mode) in UiThemeMode::ALL.iter().enumerate() {
            radio_group = radio_group.child(
                Radio::new(ElementId::Name(format!("kit-radio-{index}").into()))
                    .label(mode.label_zh()),
            );
        }
        let radios = radio_group
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                let Some(mode) = mode_from_index(*index) else {
                    return;
                };
                this.mode = mode;
                this.dark = theme::resolve_dark(mode, window);
                this.apply_appearance(window, cx);
            }))
            .into_any_element();

        let select = v_flex()
            .w_full()
            .gap(px(6.0))
            .child(
                div().w_full().child(
                    Select::new(&self.controls.select)
                        .cleanable(true)
                        .placeholder("来源筛选"),
                ),
            )
            .child(micro(
                t,
                format!(
                    "当前值：{}（SelectEvent::Confirm）",
                    self.controls.select_value.as_deref().unwrap_or("未选择")
                ),
                t.text_tertiary,
            ))
            .into_any_element();

        let slider = v_flex()
            .w_full()
            .gap(px(8.0))
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(Slider::new(&self.controls.slider)),
                    )
                    .child(mono_text(
                        format!("{:.1} MB/s", self.controls.slider_value),
                        theme::FS_MONO,
                        t.text_primary,
                    )),
            )
            .child(micro(
                t,
                "全局限速：拖动/键盘步进 ±1（Shift ±10），值实时可见（A11）",
                t.text_tertiary,
            ))
            .into_any_element();

        h_flex()
            .flex_wrap()
            .gap(px(24.0))
            .items_start()
            .child(selection_column(
                t,
                "Checkbox · 多选语义",
                checkboxes,
                300.0,
            ))
            .child(selection_column(
                t,
                "Switch · 立即生效开关",
                switches,
                300.0,
            ))
            .child(selection_column(
                t,
                "RadioGroup · 主题模式（可交互）",
                radios,
                260.0,
            ))
            .child(selection_column(t, "Select · 下拉筛选", select, 320.0))
            .child(selection_column(t, "Slider · 全局限速", slider, 340.0))
            .into_any_element()
    }

    // -- 04 状态与反馈 -----------------------------------------------------

    fn render_status(&self, t: &theme::ThemeTokens) -> AnyElement {
        let mut pills = h_flex().flex_wrap().gap(px(8.0)).items_center();
        for state in [
            ItemState::Downloading,
            ItemState::Paused,
            ItemState::Completed,
            ItemState::Failed,
            ItemState::Cancelled,
        ] {
            let (fg, bg) = state_colors(t, state);
            pills = pills.child(status_pill(
                t,
                RdmIcon::for_state(state),
                state.label_zh(),
                fg,
                bg,
            ));
        }

        let mut bars = h_flex().flex_wrap().gap(px(20.0)).items_center();
        for (label, fraction) in [("0%", 0.0_f32), ("35%", 0.35), ("100%", 1.0)] {
            // A15：进行中 accent、完成 success；0% 只剩轨道。
            let fill = if fraction >= 1.0 { t.success } else { t.accent };
            let caption = if fraction >= 1.0 {
                "完成（A15：完成态 success）"
            } else if fraction <= 0.0 {
                "等待开始（0%）"
            } else {
                "已下载 1.4 GB / 2.3 GB"
            };
            bars = bars.child(
                v_flex()
                    .w(px(200.0))
                    .gap(px(6.0))
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(micro(t, label, t.text_secondary))
                            .child(mono_text(
                                format!("{:.0}%", fraction * 100.0),
                                theme::FS_MONO_MICRO,
                                t.text_tertiary,
                            )),
                    )
                    .child(colored_progress_bar(t, Some(fraction), 6.0, fill))
                    .child(micro(t, caption, t.text_tertiary)),
            );
        }
        bars = bars.child(
            h_flex()
                .w(px(200.0))
                .gap(px(8.0))
                .items_center()
                .child(spinner_sized(t, Size::Small))
                .child(spinner_sized(t, Size::Medium))
                .child(spinner_sized(t, Size::Large))
                .child(micro(t, "不确定态 Spinner 14/16/20", t.text_tertiary)),
        );
        bars = bars.child(
            v_flex()
                .w(px(200.0))
                .gap(px(6.0))
                .child(micro(t, "gpui-component Progress", t.text_secondary))
                .child(Progress::new().value(35.0).w(px(200.0)))
                .child(micro(t, "内建 8px 高 / 4px 圆角", t.text_tertiary)),
        );

        let tags = h_flex()
            .flex_wrap()
            .gap(px(8.0))
            .items_center()
            .child(soft_tag("中性标签", t.text_secondary, t.bg_hover))
            .child(soft_tag("主色标签", t.accent, t.accent_soft))
            .child(soft_tag("成功", t.success, t.success_soft))
            .child(soft_tag("警告", t.warning, t.warning_soft))
            .child(soft_tag("危险", t.danger_text, t.danger_soft))
            .child(div().w(px(12.0)))
            .child(Tag::primary().child("主色").small())
            .child(Tag::success().child("已完成").small())
            .child(Tag::warning().child("已暂停").small())
            .child(Tag::danger().outline().child("失败").small())
            .child(Tag::info().child("来自浏览器").small())
            .child(Tag::secondary().rounded_full().child("Pill 标签").small())
            .into_any_element();

        let badges = h_flex()
            .flex_wrap()
            .gap(px(24.0))
            .items_center()
            .child(
                Badge::new()
                    .count(3)
                    .child(RdmIcon::Inbox.sized(22.0, t.text_secondary)),
            )
            .child(
                Badge::new()
                    .dot()
                    .child(RdmIcon::Info.sized(22.0, t.text_secondary)),
            )
            .child(
                Badge::new()
                    .count(120)
                    .max(99)
                    .child(RdmIcon::Message.sized(22.0, t.text_secondary)),
            )
            .child(micro(
                t,
                "Badge::count(3) / dot() / count(120).max(99)",
                t.text_tertiary,
            ))
            .into_any_element();

        let toasts = h_flex()
            .flex_wrap()
            .gap(px(16.0))
            .items_start()
            .child(toast_card(
                t,
                "kit-toast-undo",
                RdmIcon::CheckCircle,
                t.success,
                "已移除记录",
                Some("撤销"),
            ))
            .child(toast_card(
                t,
                "kit-toast-open",
                RdmIcon::ErrorCircle,
                t.danger,
                "下载失败：服务器不支持断点续传",
                Some("打开 RDM"),
            ))
            .into_any_element();

        let banners = v_flex()
            .gap(px(8.0))
            .child(info_banner(
                t,
                RdmIcon::Globe,
                Severity::Info,
                "浏览器接管已开启。Chrome 已连接。",
                vec![banner_action("kit-banner-info", "管理浏览器集成")],
            ))
            .child(info_banner(
                t,
                RdmIcon::CheckCircle,
                Severity::Success,
                "宿主已连接（127.0.0.1:47612）。",
                vec![],
            ))
            .child(info_banner(
                t,
                RdmIcon::Alert,
                Severity::Warning,
                "未检测到浏览器扩展，接管暂不可用。",
                vec![banner_action("kit-banner-warn", "安装扩展")],
            ))
            .child(info_banner(
                t,
                RdmIcon::ErrorCircle,
                Severity::Danger,
                "IPC 连接异常：无法写入 ipc.json。",
                vec![banner_action("kit-banner-danger", "重试")],
            ))
            .into_any_element();

        v_flex()
            .gap(px(INNER_GAP))
            .child(demo_row(
                t,
                "ItemState · 5 种状态 Pill（图标 + 文案 + 颜色三通道，A14）",
                pills.into_any_element(),
            ))
            .child(demo_row(
                t,
                "进度 ProgressBar · determinate 0 / 35 / 100% + indeterminate Spinner（A15/A16）",
                bars.into_any_element(),
            ))
            .child(demo_row(
                t,
                "Tag / Badge · 设计 SoftTag 与 gpui-component 内建 Tag/Badge（A14）",
                v_flex()
                    .gap(px(10.0))
                    .child(tags)
                    .child(badges)
                    .into_any_element(),
            ))
            .child(demo_row(
                t,
                "Toast · 静态样式（A21；不真正 push_notification，避免演示窗弹通知）",
                toasts,
            ))
            .child(demo_row(
                t,
                "Banner · info / success / warning / danger 四种 severity（A22）",
                banners,
            ))
            .into_any_element()
    }

    // -- 05 列表行 ---------------------------------------------------------

    fn render_rows(&self, t: &theme::ThemeTokens) -> AnyElement {
        let live_height = self.density.row_height();
        v_flex()
            .gap(px(INNER_GAP))
            .child(demo_row(
                t,
                format!(
                    "当前密度预览 · {}（工具栏切换「舒适 / 紧凑」，下面这一行行高即时变化）",
                    self.density.label_zh()
                ),
                v_flex()
                    .gap(px(6.0))
                    .child(labeled_row(
                        t,
                        format!("当前 · {}px", live_height as i32),
                        sample_task_row(t, live_height, RowSkin::Selected),
                    ))
                    .into_any_element(),
            ))
            .child(demo_row(
                t,
                "行高 · 舒适 64px / 紧凑 40px（与主窗口 render_row 同结构：勾选 + 类型图标 + 双行信息 + 状态 + 动作）",
                v_flex()
                    .gap(px(6.0))
                    .child(labeled_row(
                        t,
                        "舒适 · 64px",
                        sample_task_row(t, theme::ROW_COMFORTABLE, RowSkin::Default),
                    ))
                    .child(labeled_row(
                        t,
                        "紧凑 · 40px",
                        sample_task_row(t, theme::ROW_COMPACT, RowSkin::Default),
                    ))
                    .into_any_element(),
            ))
            .child(demo_row(
                t,
                "行状态 · default / hover / selected / focused / multi-selected（R3：S3 = bg.selected + 左 2px accent 条）",
                v_flex()
                    .gap(px(6.0))
                    .child(labeled_row(
                        t,
                        "default 默认",
                        sample_task_row(t, theme::ROW_COMFORTABLE, RowSkin::Default),
                    ))
                    .child(labeled_row(
                        t,
                        "hover 悬停",
                        sample_task_row(t, theme::ROW_COMFORTABLE, RowSkin::Hover),
                    ))
                    .child(labeled_row(
                        t,
                        "selected 选中",
                        sample_task_row(t, theme::ROW_COMFORTABLE, RowSkin::Selected),
                    ))
                    .child(labeled_row(
                        t,
                        "focused 焦点",
                        sample_task_row(t, theme::ROW_COMFORTABLE, RowSkin::Focused),
                    ))
                    .child(labeled_row(
                        t,
                        "多选 multi",
                        sample_task_row(t, theme::ROW_COMFORTABLE, RowSkin::Multi),
                    ))
                    .into_any_element(),
            ))
            .child(micro(
                t,
                "截断：文件名超长时中间省略并保留扩展名；右侧数值用 mono 等宽字体防抖位。",
                t.text_tertiary,
            ))
            .into_any_element()
    }

    // -- 08 空态 / 导航 ----------------------------------------------------

    fn render_empty_and_nav(&self, t: &theme::ThemeTokens, cx: &Context<Self>) -> AnyElement {
        let empty = divider_box(
            t,
            div()
                .w_full()
                .h(px(240.0))
                .child(empty_state(
                    t,
                    RdmIcon::Inbox,
                    "还没有下载任务",
                    "粘贴下载链接到顶部输入栏，或在 Firefox / Chrome 中直接下载，RDM 会自动接管。",
                    vec![h_flex()
                        .gap(px(8.0))
                        .items_center()
                        .child(
                            Button::new("kit-empty-add")
                                .primary()
                                .small()
                                .label("粘贴链接"),
                        )
                        .child(
                            Button::new("kit-empty-import")
                                .ghost()
                                .small()
                                .label("批量导入…"),
                        )
                        .into_any_element()],
                ))
                .into_any_element(),
        );

        let expanded_nav = v_flex()
            .w(px(theme::SIDEBAR_W))
            .flex_none()
            .h(px(196.0))
            .py(px(6.0))
            .rounded(px(theme::RADIUS_MD))
            .bg(t.bg_surface)
            .border_1()
            .border_color(t.border_subtle)
            .child(section_label(t, "智能视图"))
            .child(nav_item(
                t,
                ElementId::Name("kit-nav-downloading".into()),
                RdmIcon::Download,
                "下载中",
                Some(3),
                true,
                false,
                |_, _, _| {},
            ))
            .child(nav_item(
                t,
                ElementId::Name("kit-nav-completed".into()),
                RdmIcon::CheckCircle,
                "已完成",
                Some(12),
                false,
                false,
                |_, _, _| {},
            ))
            .child(nav_item(
                t,
                ElementId::Name("kit-nav-failed".into()),
                RdmIcon::ErrorCircle,
                "失败",
                Some(1),
                false,
                false,
                |_, _, _| {},
            ))
            .into_any_element();

        let collapsed_nav = v_flex()
            .w(px(theme::SIDEBAR_COLLAPSED_W))
            .flex_none()
            .h(px(196.0))
            .py(px(6.0))
            .rounded(px(theme::RADIUS_MD))
            .bg(t.bg_surface)
            .border_1()
            .border_color(t.border_subtle)
            .child(nav_item(
                t,
                ElementId::Name("kit-nav-c1".into()),
                RdmIcon::Download,
                "下载中",
                Some(3),
                true,
                true,
                |_, _, _| {},
            ))
            .child(nav_item(
                t,
                ElementId::Name("kit-nav-c2".into()),
                RdmIcon::CheckCircle,
                "已完成",
                Some(12),
                false,
                true,
                |_, _, _| {},
            ))
            .child(nav_item(
                t,
                ElementId::Name("kit-nav-c3".into()),
                RdmIcon::Inbox,
                "全部",
                Some(16),
                false,
                true,
                |_, _, _| {},
            ))
            .into_any_element();

        let shortcuts = h_flex()
            .flex_wrap()
            .gap(px(18.0))
            .items_center()
            .child(kbd_combo(t, &["Ctrl", "K"], "命令面板"))
            .child(kbd_combo(t, &["Ctrl", "N"], "添加下载"))
            .child(kbd_combo(t, &["Ctrl", "F"], "搜索任务"))
            .child(kbd_combo(t, &["Enter"], "确认 / 提交"))
            .child(kbd_combo(t, &["Esc"], "关闭当前层"))
            .child(kbd_combo(t, &["↑", "↓"], "列表移动"))
            .into_any_element();

        // gpui-component 内建 Kbd：`Kbd::new(Keystroke)` 自己会渲染键帽文本，
        // 所以旁边只放「用途」说明，避免重复显示按键。
        let builtin_kbds = h_flex()
            .flex_wrap()
            .gap(px(16.0))
            .items_center()
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(Kbd::new(Keystroke::parse("ctrl-k").unwrap()))
                    .child(micro(t, "命令面板", t.text_tertiary)),
            )
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(Kbd::new(Keystroke::parse("enter").unwrap()))
                    .child(micro(t, "确认 / 提交", t.text_tertiary)),
            )
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(Kbd::new(Keystroke::parse("escape").unwrap()))
                    .child(micro(t, "关闭当前层", t.text_tertiary)),
            )
            .into_any_element();

        let _ = cx;
        v_flex()
            .gap(px(INNER_GAP))
            .child(demo_row(
                t,
                "EmptyState · 首次启动（A23：图标 + 事实标题 + 下一步说明 + 主/次动作）",
                empty,
            ))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(16.0))
                    .items_start()
                    .child(selection_column(
                        t,
                        "展开 216px（选中高亮 + 左条）",
                        expanded_nav,
                        220.0,
                    ))
                    .child(selection_column(
                        t,
                        "折叠 56px（仅图标 + tooltip）",
                        collapsed_nav,
                        60.0,
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(280.0))
                            .gap(px(10.0))
                            .child(field_label(t, "Kbd · 快捷键组合（设计 Token 版）"))
                            .child(shortcuts)
                            .child(field_label(t, "Kbd · gpui-component 内建版"))
                            .child(builtin_kbds)
                            .child(micro(
                                t,
                                "所有快捷键展示必须用 Kbd；文案与契约 §10 一致。",
                                t.text_tertiary,
                            )),
                    ),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// 通用小部件（全部只吃 ThemeTokens）
// ---------------------------------------------------------------------------

/// 卡片：编号 + 标题 + 说明 + 分隔线 + 内容。
fn card(
    t: &theme::ThemeTokens,
    index: &'static str,
    title: &'static str,
    subtitle: &'static str,
    body: AnyElement,
) -> AnyElement {
    v_flex()
        .w_full()
        .flex_shrink_0()
        .gap(px(INNER_GAP))
        .p(px(16.0))
        .rounded(px(theme::RADIUS_MD))
        .bg(t.bg_surface)
        .border_1()
        .border_color(t.border_subtle)
        .child(
            h_flex()
                .gap(px(8.0))
                .items_center()
                .flex_wrap()
                .child(
                    div()
                        .text_size(px(theme::FS_H2))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .px(px(6.0))
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .rounded(px(theme::RADIUS_XS))
                        .bg(t.bg_sunken)
                        .text_size(px(theme::FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(index),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(theme::FS_CAPTION))
                        .text_color(t.text_secondary)
                        .child(subtitle),
                ),
        )
        .child(divider(t))
        .child(body)
        .into_any_element()
}

/// demo 行：小标题 + 内容。
fn demo_row(
    t: &theme::ThemeTokens,
    label: impl Into<SharedString>,
    content: AnyElement,
) -> AnyElement {
    v_flex()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(theme::FS_MICRO))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_tertiary)
                .child(label.into()),
        )
        .child(content)
        .into_any_element()
}

/// 输入/选择控件的标签。
fn field_label(t: &theme::ThemeTokens, label: &'static str) -> AnyElement {
    div()
        .text_size(px(theme::FS_MICRO))
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text_secondary)
        .child(label)
        .into_any_element()
}

/// 带说明的固定宽度 demo 字段。
fn demo_field(
    t: &theme::ThemeTokens,
    label: &'static str,
    control: AnyElement,
    hint: &'static str,
) -> AnyElement {
    v_flex()
        .w(px(FIELD_W))
        .flex_none()
        .gap(px(6.0))
        .child(field_label(t, label))
        .child(control)
        .when(!hint.is_empty(), |this| {
            this.child(micro(t, hint, t.text_tertiary))
        })
        .into_any_element()
}

/// 选择控件列（标题 + 内容）。
fn selection_column(
    t: &theme::ThemeTokens,
    label: &'static str,
    control: AnyElement,
    width: f32,
) -> AnyElement {
    v_flex()
        .w(px(width))
        .flex_none()
        .gap(px(8.0))
        .child(field_label(t, label))
        .child(control)
        .into_any_element()
}

/// 工具栏分组（标题 + 控件）。
fn toolbar_group(t: &theme::ThemeTokens, label: &'static str, control: AnyElement) -> AnyElement {
    h_flex()
        .gap(px(6.0))
        .items_center()
        .child(micro(t, label, t.text_tertiary))
        .child(control)
        .into_any_element()
}

/// 工具栏竖向分隔线。
fn vertical_rule(t: &theme::ThemeTokens) -> AnyElement {
    div()
        .w(px(1.0))
        .h(px(18.0))
        .flex_none()
        .bg(t.border_subtle)
        .into_any_element()
}

/// 小号弱化文本。
fn micro(t: &theme::ThemeTokens, text: impl Into<SharedString>, color: Hsla) -> AnyElement {
    let _ = t;
    div()
        .text_size(px(theme::FS_MICRO))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// 斑马线/容器用的演示盒（bg.app + 1px 描边）。
fn divider_box(t: &theme::ThemeTokens, content: AnyElement) -> AnyElement {
    div()
        .w_full()
        .rounded(px(theme::RADIUS_MD))
        .bg(t.bg_app)
        .border_1()
        .border_color(t.border_subtle)
        .overflow_hidden()
        .child(content)
        .into_any_element()
}

/// 行标签 + 行内容（列表行 demo 用）。
fn labeled_row(
    t: &theme::ThemeTokens,
    label: impl Into<SharedString>,
    row: AnyElement,
) -> AnyElement {
    let label: SharedString = label.into();
    h_flex()
        .gap(px(12.0))
        .items_center()
        .child(
            div()
                .w(px(110.0))
                .flex_none()
                .text_size(px(theme::FS_MICRO))
                .text_color(t.text_tertiary)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .rounded(px(theme::RADIUS_SM))
                .overflow_hidden()
                .child(row),
        )
        .into_any_element()
}

/// 指定颜色的细进度条（A15：进行中 accent / 完成 success）。
fn colored_progress_bar(
    t: &theme::ThemeTokens,
    fraction: Option<f32>,
    height: f32,
    color: Hsla,
) -> AnyElement {
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
        .into_any_element()
}

/// 列表行状态（静态演示）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowSkin {
    Default,
    Hover,
    Selected,
    Focused,
    Multi,
}

/// 列表行示例：结构与 `views/app.rs::render_row` 一致（舒适 64 / 紧凑 40）。
fn sample_task_row(t: &theme::ThemeTokens, height: f32, skin: RowSkin) -> AnyElement {
    let selected = matches!(skin, RowSkin::Selected | RowSkin::Multi);
    let bg = match skin {
        RowSkin::Default => t.bg_app,
        RowSkin::Hover | RowSkin::Focused => t.bg_hover,
        RowSkin::Selected | RowSkin::Multi => t.bg_selected,
    };
    let comfortable = height > 48.0;

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
                .text_size(px(theme::FS_ROW_TITLE))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_primary)
                .child("ubuntu-24.04.1-desktop-amd64.iso"),
        )
        .child(mono_text("2.3 MB/s", theme::FS_MONO, t.text_secondary))
        .child(mono_text("62%", theme::FS_MONO, t.text_primary))
        .child(mono_text(
            "剩余 4 分 12 秒",
            theme::FS_MONO,
            t.text_tertiary,
        ))
        .child(status_pill(
            t,
            RdmIcon::Download,
            "下载中",
            t.info,
            t.info_soft,
        ));

    let body = if comfortable {
        v_flex()
            .flex_1()
            .min_w(px(0.0))
            .gap(px(4.0))
            .child(header_line)
            .child(progress_bar(t, Some(0.62), 4.0))
            .child(
                h_flex()
                    .w_full()
                    .min_w(px(0.0))
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        h_flex()
                            .gap(px(4.0))
                            .items_center()
                            .child(RdmIcon::Globe.sized(12.0, t.text_tertiary))
                            .child(field_caption(t, "来自浏览器")),
                    )
                    .child(field_caption(t, "· 8 个连接"))
                    .child(field_caption(t, "· 1.4 GB / 2.3 GB"))
                    .child(div().flex_1())
                    .child(mono_text(
                        "sha256: 9f3a…c2",
                        theme::FS_MONO_MICRO,
                        t.text_tertiary,
                    )),
            )
            .into_any_element()
    } else {
        v_flex()
            .flex_1()
            .min_w(px(0.0))
            .gap(px(2.0))
            .child(header_line)
            .child(progress_bar(t, Some(0.62), 2.0))
            .into_any_element()
    };

    let row = h_flex()
        .relative()
        .h(px(height))
        .w_full()
        .px(px(12.0))
        .gap(px(10.0))
        .items_center()
        .bg(bg)
        .border_b_1()
        .border_color(t.border_subtle)
        .when(skin == RowSkin::Focused, |this| {
            this.border_1().border_color(t.focus)
        })
        .when(selected, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .bottom(px(0.0))
                    .w(px(2.0))
                    .bg(t.accent),
            )
        })
        .child(if selected {
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(16.0))
                .rounded(px(theme::RADIUS_XS))
                .bg(t.accent)
                .child(RdmIcon::Check.sized(11.0, t.on_accent))
                .into_any_element()
        } else {
            div()
                .flex_none()
                .size(px(16.0))
                .rounded(px(theme::RADIUS_XS))
                .border_1()
                .border_color(t.border_control)
                .into_any_element()
        })
        .child(
            RdmIcon::for_file("ubuntu-24.04.1-desktop-amd64.iso")
                .sized(if comfortable { 20.0 } else { 16.0 }, t.info),
        )
        .child(body)
        .child(
            h_flex()
                .flex_none()
                .gap(px(8.0))
                .items_center()
                .child(RdmIcon::Folder.sized(16.0, t.text_tertiary))
                .child(RdmIcon::More.sized(16.0, t.text_tertiary)),
        );
    row.into_any_element()
}

/// 行内次要信息的说明文字。
fn field_caption(t: &theme::ThemeTokens, text: &'static str) -> AnyElement {
    div()
        .text_size(px(theme::FS_CAPTION))
        .text_color(t.text_secondary)
        .child(text)
        .into_any_element()
}

/// 状态色（与主窗口/05 §0.4 一致）。
fn state_colors(t: &theme::ThemeTokens, state: ItemState) -> (Hsla, Hsla) {
    match state {
        ItemState::Downloading => (t.info, t.info_soft),
        ItemState::Paused => (t.warning, t.warning_soft),
        ItemState::Completed => (t.success, t.success_soft),
        ItemState::Failed => (t.danger_text, t.danger_soft),
        ItemState::Cancelled => (t.text_tertiary, t.bg_hover),
    }
}

/// Spinner（A16）：尺寸档位。
fn spinner_sized(t: &theme::ThemeTokens, size: Size) -> AnyElement {
    Spinner::new()
        .color(t.accent)
        .with_size(size)
        .into_any_element()
}

/// Banner 右侧动作（静态按钮，无副作用）。
fn banner_action(id: &'static str, label: &'static str) -> AnyElement {
    Button::new(id)
        .ghost()
        .small()
        .label(label)
        .into_any_element()
}

/// Toast 静态样式（A21）：bg.elevated + border.default + shadow.e2 + 图标 + 动作 + 关闭。
fn toast_card(
    t: &theme::ThemeTokens,
    id: &'static str,
    icon: RdmIcon,
    fg: Hsla,
    text: &'static str,
    action: Option<&'static str>,
) -> AnyElement {
    let mut card = h_flex()
        .w(px(360.0))
        .min_h(px(40.0))
        .px(px(12.0))
        .py(px(6.0))
        .gap(px(8.0))
        .items_center()
        .rounded(px(theme::RADIUS_MD))
        .bg(t.bg_elevated)
        .border_1()
        .border_color(t.border_default)
        .shadow_lg()
        .child(icon.sized(16.0, fg))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(theme::FS_CAPTION))
                .text_color(t.text_primary)
                .child(text),
        );
    if let Some(label) = action {
        card = card.child(Button::new(id).ghost().small().label(label));
    }
    card.child(RdmIcon::Close.sized(14.0, t.text_tertiary))
        .into_any_element()
}

/// Kbd 组合：`Ctrl` `+` `K` + 说明。
fn kbd_combo(
    t: &theme::ThemeTokens,
    keys: &'static [&'static str],
    label: &'static str,
) -> AnyElement {
    let mut row = h_flex().gap(px(4.0)).items_center();
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            row = row.child(micro(t, "+", t.text_tertiary));
        }
        row = row.child(kbd(t, *key));
    }
    row.child(
        div()
            .pl(px(2.0))
            .text_size(px(theme::FS_MICRO))
            .text_color(t.text_tertiary)
            .child(label),
    )
    .into_any_element()
}

// ---------------------------------------------------------------------------
// 按钮六态静态矩阵（S1–S6：token 配方直出，文字标注）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BtnSkin {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BtnState {
    Default,
    Hover,
    Pressed,
    Focus,
    Disabled,
    Loading,
}

impl BtnState {
    fn label_zh(self) -> &'static str {
        match self {
            Self::Default => "default 常态",
            Self::Hover => "hover 悬停",
            Self::Pressed => "pressed 按下",
            Self::Focus => "focus 焦点",
            Self::Disabled => "disabled 禁用",
            Self::Loading => "loading 加载",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Default => 1,
            Self::Hover => 2,
            Self::Pressed => 3,
            Self::Focus => 4,
            Self::Disabled => 5,
            Self::Loading => 6,
        }
    }
}

/// 静态按钮（六态矩阵用；不走真实组件，确保截图能同时看到全部状态）。
fn static_button(t: &theme::ThemeTokens, skin: BtnSkin, state: BtnState) -> AnyElement {
    let (bg, fg, border): (Hsla, Hsla, Hsla) = match (skin, state) {
        (BtnSkin::Primary, BtnState::Hover) => (t.accent_hover, t.on_accent, t.accent_hover),
        (BtnSkin::Primary, BtnState::Pressed) => (t.accent_active, t.on_accent, t.accent_active),
        (BtnSkin::Primary, BtnState::Disabled) => (t.bg_sunken, t.text_disabled, t.border_subtle),
        (BtnSkin::Primary, _) => (t.accent, t.on_accent, t.accent),
        (BtnSkin::Secondary, BtnState::Hover) => (t.bg_hover, t.text_primary, t.border_control),
        (BtnSkin::Secondary, BtnState::Pressed) => {
            (t.bg_selected, t.text_primary, t.border_control)
        }
        (BtnSkin::Secondary, BtnState::Disabled) => (t.bg_sunken, t.text_disabled, t.border_subtle),
        (BtnSkin::Secondary, _) => (t.bg_surface, t.text_primary, t.border_default),
    };
    let label = match skin {
        BtnSkin::Primary => "开始下载",
        BtnSkin::Secondary => "打开文件夹",
    };

    let mut inner = h_flex()
        .h(px(32.0))
        .min_w(px(112.0))
        .justify_center()
        .items_center()
        .gap(px(6.0))
        .px(px(12.0))
        .rounded(px(theme::RADIUS_SM))
        .bg(bg)
        .text_color(fg)
        .text_size(px(theme::FS_BODY))
        .font_weight(FontWeight::MEDIUM)
        .border_1()
        .border_color(border);
    if state == BtnState::Loading {
        inner = inner.child(Spinner::new().color(fg).with_size(Size::Small));
    }
    inner = inner.child(label);

    if state == BtnState::Focus {
        div()
            .rounded(px(theme::RADIUS_SM + 2.0))
            .border_2()
            .border_color(t.focus)
            .p(px(2.0))
            .child(inner)
            .into_any_element()
    } else {
        inner.into_any_element()
    }
}

/// 单个状态单元：按钮 + 状态文字。
fn state_cell(t: &theme::ThemeTokens, skin: BtnSkin, state: BtnState) -> AnyElement {
    v_flex()
        .items_center()
        .gap(px(6.0))
        .child(static_button(t, skin, state))
        .child(
            div()
                .text_size(px(theme::FS_MICRO))
                .text_color(t.text_tertiary)
                .child(format!("S{} · {}", state.index(), state.label_zh())),
        )
        .into_any_element()
}

/// 六态矩阵（一行 6 个）。.
fn state_matrix(t: &theme::ThemeTokens, skin: BtnSkin) -> AnyElement {
    let mut row = h_flex().flex_wrap().gap(px(10.0)).items_center();
    for state in [
        BtnState::Default,
        BtnState::Hover,
        BtnState::Pressed,
        BtnState::Focus,
        BtnState::Disabled,
        BtnState::Loading,
    ] {
        row = row.child(state_cell(t, skin, state));
    }
    row.into_any_element()
}

/// 真实按钮（点击后更新「最近动作」）。
fn demo_action_button(
    cx: &Context<Showcase>,
    id: &'static str,
    label: &'static str,
    primary: bool,
) -> Button {
    let button = Button::new(id)
        .label(label)
        .tooltip(label)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.last_action = label.to_string();
            cx.notify();
        }));
    if primary {
        button.primary()
    } else {
        button
    }
}

// ---------------------------------------------------------------------------
// 06 排版
// ---------------------------------------------------------------------------

struct TypeSpec {
    token: &'static str,
    size: f32,
    line: f32,
    weight: FontWeight,
    sample: &'static str,
    mono: bool,
}

/// 04 §4.2 的 11 档字号（display → mono-path）。
const TYPE_SCALE: &[TypeSpec] = &[
    TypeSpec {
        token: "type.display",
        size: theme::FS_DISPLAY,
        line: 32.0,
        weight: FontWeight::SEMIBOLD,
        sample: "还没有下载任务",
        mono: false,
    },
    TypeSpec {
        token: "type.h1",
        size: theme::FS_H1,
        line: 28.0,
        weight: FontWeight::SEMIBOLD,
        sample: "下载设置",
        mono: false,
    },
    TypeSpec {
        token: "type.h2",
        size: theme::FS_H2,
        line: 24.0,
        weight: FontWeight::SEMIBOLD,
        sample: "智能视图 / 列表视图名",
        mono: false,
    },
    TypeSpec {
        token: "type.row-title",
        size: theme::FS_ROW_TITLE,
        line: 20.0,
        weight: FontWeight::MEDIUM,
        sample: "ubuntu-24.04.1-desktop-amd64.iso",
        mono: false,
    },
    TypeSpec {
        token: "type.body",
        size: theme::FS_BODY,
        line: 20.0,
        weight: FontWeight::NORMAL,
        sample: "粘贴下载链接，或搜索任务…",
        mono: false,
    },
    TypeSpec {
        token: "type.body-strong",
        size: theme::FS_BODY,
        line: 20.0,
        weight: FontWeight::MEDIUM,
        sample: "已下载 1.4 GB / 2.3 GB",
        mono: false,
    },
    TypeSpec {
        token: "type.caption",
        size: theme::FS_CAPTION,
        line: 18.0,
        weight: FontWeight::NORMAL,
        sample: "2.3 GB · 8 个连接 · 来自浏览器",
        mono: false,
    },
    TypeSpec {
        token: "type.caption-strong",
        size: theme::FS_CAPTION,
        line: 18.0,
        weight: FontWeight::MEDIUM,
        sample: "剩余 4 分 12 秒",
        mono: false,
    },
    TypeSpec {
        token: "type.micro",
        size: theme::FS_MICRO,
        line: 16.0,
        weight: FontWeight::NORMAL,
        sample: "上次检查：2026-10-21 18:30",
        mono: false,
    },
    TypeSpec {
        token: "type.mono",
        size: theme::FS_MONO,
        line: 17.0,
        weight: FontWeight::NORMAL,
        sample: "2.3 MB/s · 62% · 1.4 GB",
        mono: true,
    },
    TypeSpec {
        token: "type.mono-micro",
        size: theme::FS_MONO_MICRO,
        line: 15.0,
        weight: FontWeight::NORMAL,
        sample: "v0.1.0 · build 20261021",
        mono: true,
    },
    TypeSpec {
        token: "type.mono-path",
        size: theme::FS_MONO_PATH,
        line: 15.0,
        weight: FontWeight::NORMAL,
        sample: r"D:\Downloads\其他\file.iso",
        mono: true,
    },
];

/// 超长文件名（验证中间省略 + 保留扩展名）。
const LONG_FILE_NAME: &str = "最终幻想7重制版-完整原声带-无损音质-2026重制合集-Original-Soundtrack-Final-Fantasy-VII-Remake-Collector-Edition.flac";

fn render_typography(t: &theme::ThemeTokens) -> AnyElement {
    let mut scale = v_flex().gap(px(2.0));
    for spec in TYPE_SCALE {
        scale = scale.child(
            h_flex()
                .items_center()
                .gap(px(12.0))
                .py(px(5.0))
                .border_b_1()
                .border_color(t.border_subtle)
                .child(
                    div()
                        .w(px(140.0))
                        .flex_none()
                        .font(theme::mono_font())
                        .text_size(px(theme::FS_MONO_MICRO))
                        .text_color(t.text_tertiary)
                        .child(spec.token),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .line_height(px(spec.line))
                        .text_size(px(spec.size))
                        .font_weight(spec.weight)
                        .text_color(t.text_primary)
                        .when(spec.mono, |this| this.font(theme::mono_font()))
                        .child(spec.sample),
                )
                .child(
                    div()
                        .flex_none()
                        .font(theme::mono_font())
                        .text_size(px(theme::FS_MONO_MICRO))
                        .text_color(t.text_tertiary)
                        .child(format!(
                            "{}px / {}px · {}",
                            spec.size as i32, spec.line as i32, spec.weight.0 as i32
                        )),
                ),
        );
    }

    let mixed = h_flex()
        .flex_wrap()
        .gap(px(20.0))
        .items_center()
        .child(
            div()
                .text_size(px(theme::FS_BODY))
                .line_height(px(20.0))
                .text_color(t.text_primary)
                .child("中文与数字混排留 1/8 em 间距：已下载 1.4 GB / 2.3 GB，共 8 个连接，剩余 4 分 12 秒。"),
        )
        .child(
            div()
                .text_size(px(theme::FS_BODY))
                .line_height(px(20.0))
                .text_color(t.text_secondary)
                .child("同样一行不加空格的对照：已下载1.4GB/2.3GB，共8个连接。"),
        )
        .into_any_element();

    let mono_numbers = h_flex()
        .flex_wrap()
        .gap(px(16.0))
        .items_center()
        .child(mono_text("0123456789", theme::FS_MONO, t.text_primary))
        .child(mono_text("2.3 MB/s", theme::FS_MONO, t.text_primary))
        .child(mono_text("62%", theme::FS_MONO, t.text_primary))
        .child(mono_text("1.4 GB / 2.3 GB", theme::FS_MONO, t.text_primary))
        .child(mono_text("4 分 12 秒", theme::FS_MONO, t.text_primary))
        .child(micro(
            t,
            "Cascadia Mono（等宽数字，防抖位）",
            t.text_tertiary,
        ))
        .into_any_element();

    let ellipsized = ellipsize_file_name(LONG_FILE_NAME, 36);
    let truncation = v_flex()
        .gap(px(8.0))
        .child(
            h_flex()
                .gap(px(10.0))
                .items_center()
                .child(
                    div()
                        .w(px(120.0))
                        .flex_none()
                        .text_size(px(theme::FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("原文"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .font(theme::mono_font())
                        .text_size(px(theme::FS_MONO_PATH))
                        .text_color(t.text_secondary)
                        .child(LONG_FILE_NAME),
                ),
        )
        .child(
            h_flex()
                .gap(px(10.0))
                .items_center()
                .child(
                    div()
                        .w(px(120.0))
                        .flex_none()
                        .text_size(px(theme::FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("中间省略"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .font(theme::mono_font())
                        .text_size(px(theme::FS_MONO_PATH))
                        .text_color(t.text_primary)
                        .child(ellipsized.clone()),
                ),
        )
        .child(micro(
            t,
            "规则：文件名截断保留扩展名（不允许把 .flac 截掉）；单行截断用 truncate / text_ellipsis。",
            t.text_tertiary,
        ))
        .into_any_element();

    v_flex()
        .gap(px(INNER_GAP))
        .child(demo_row(
            t,
            "字号 / 行高 / 字重 · 11 档（04 §4.2，禁止中间字号）",
            scale.into_any_element(),
        ))
        .child(demo_row(
            t,
            "中文 + 数字混排（手动空格，不用全局 letter-spacing）",
            mixed,
        ))
        .child(demo_row(t, "等宽数字 · mono 字体防抖位", mono_numbers))
        .child(demo_row(t, "长文件名 ellipsize（保留扩展名）", truncation))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 07 图标
// ---------------------------------------------------------------------------

/// `RdmIcon` 全量（与 `ui/icons.rs` 枚举一一对应；新增图标时同步本表）。
const ALL_ICONS: &[RdmIcon] = &[
    RdmIcon::Download,
    RdmIcon::Pause,
    RdmIcon::Play,
    RdmIcon::Close,
    RdmIcon::Retry,
    RdmIcon::File,
    RdmIcon::Folder,
    RdmIcon::FolderOpen,
    RdmIcon::Copy,
    RdmIcon::Trash,
    RdmIcon::Search,
    RdmIcon::Settings,
    RdmIcon::Plus,
    RdmIcon::Sparkle,
    RdmIcon::Filter,
    RdmIcon::Sort,
    RdmIcon::Density,
    RdmIcon::ChevronDown,
    RdmIcon::ChevronRight,
    RdmIcon::Check,
    RdmIcon::CheckCircle,
    RdmIcon::Alert,
    RdmIcon::ErrorCircle,
    RdmIcon::Clock,
    RdmIcon::Disk,
    RdmIcon::Link,
    RdmIcon::Globe,
    RdmIcon::Command,
    RdmIcon::External,
    RdmIcon::More,
    RdmIcon::PanelLeftClose,
    RdmIcon::PanelLeftOpen,
    RdmIcon::PanelRightClose,
    RdmIcon::PanelRightOpen,
    RdmIcon::Clipboard,
    RdmIcon::Info,
    RdmIcon::VideoFile,
    RdmIcon::TextFile,
    RdmIcon::ArchiveFile,
    RdmIcon::ImageFile,
    RdmIcon::Monitor,
    RdmIcon::ArrowRight,
    RdmIcon::Sliders,
    RdmIcon::Gauge,
    RdmIcon::Plug,
    RdmIcon::Sun,
    RdmIcon::Moon,
    RdmIcon::RotateCcw,
    RdmIcon::CirclePause,
    RdmIcon::CirclePlay,
    RdmIcon::ListFilter,
    RdmIcon::Languages,
    RdmIcon::Keyboard,
    RdmIcon::Palette,
    RdmIcon::Archive,
    RdmIcon::FileDown,
    RdmIcon::DownloadCloud,
    RdmIcon::Shield,
    RdmIcon::Lock,
    RdmIcon::Wifi,
    RdmIcon::WifiOff,
    RdmIcon::PlugZap,
    RdmIcon::Refresh,
    RdmIcon::Send,
    RdmIcon::Bot,
    RdmIcon::Message,
    RdmIcon::CornerDownLeft,
    RdmIcon::ChevronsUpDown,
    RdmIcon::ArrowUp,
    RdmIcon::ArrowDown,
    RdmIcon::Minus,
    RdmIcon::Square,
    RdmIcon::Circle,
    RdmIcon::Loader,
    RdmIcon::Inbox,
];

fn render_icons(t: &theme::ThemeTokens) -> AnyElement {
    let sizes = h_flex()
        .flex_wrap()
        .gap(px(28.0))
        .items_center()
        .child(icon_size_sample(t, 16.0))
        .child(icon_size_sample(t, 20.0))
        .child(icon_size_sample(t, 24.0))
        .child(
            h_flex()
                .gap(px(12.0))
                .items_center()
                .child(RdmIcon::CheckCircle.sized(20.0, t.success))
                .child(RdmIcon::Alert.sized(20.0, t.warning))
                .child(RdmIcon::ErrorCircle.sized(20.0, t.danger))
                .child(RdmIcon::Info.sized(20.0, t.info))
                .child(RdmIcon::Sparkle.sized(20.0, t.accent))
                .child(micro(t, "语义色着色（currentColor）", t.text_tertiary)),
        )
        .into_any_element();

    let mut grid = h_flex().flex_wrap().gap(px(0.0));
    for icon in ALL_ICONS {
        grid = grid.child(
            v_flex()
                .w(px(112.0))
                .h(px(64.0))
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .child(icon.sized(20.0, t.text_primary))
                .child(
                    div()
                        .font(theme::mono_font())
                        .text_size(px(theme::FS_MONO_MICRO))
                        .text_color(t.text_tertiary)
                        .child(icon.name()),
                ),
        );
    }

    v_flex()
        .gap(px(INNER_GAP))
        .child(demo_row(
            t,
            "尺寸 · 16（行内）/ 20（侧栏）/ 24（空状态）",
            sizes,
        ))
        .child(demo_row(
            t,
            format!(
                "全量网格 · {} 枚 RdmIcon（资产：assets/icons/<name>.svg，禁止手写 SVG 路径）",
                ALL_ICONS.len()
            ),
            grid.into_any_element(),
        ))
        .into_any_element()
}

/// 图标尺寸示例（图标 + 名称 + 像素）。
fn icon_size_sample(t: &theme::ThemeTokens, size: f32) -> AnyElement {
    v_flex()
        .items_center()
        .gap(px(4.0))
        .child(
            div()
                .h(px(28.0))
                .flex()
                .items_center()
                .child(RdmIcon::Download.sized(size, t.accent)),
        )
        .child(micro(t, format!("{size}px"), t.text_tertiary))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 主题模式索引（RadioGroup 与工具栏共用）
// ---------------------------------------------------------------------------

fn mode_index(mode: UiThemeMode) -> usize {
    UiThemeMode::ALL
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0)
}

fn mode_from_index(index: usize) -> Option<UiThemeMode> {
    UiThemeMode::ALL.get(index).copied()
}

// ---------------------------------------------------------------------------
// 打开窗口（签名固定：`--ui=kit` 由 main.rs 接线）
// ---------------------------------------------------------------------------

/// 打开组件展示窗（`--ui=kit`）。
pub fn open(cx: &mut App, settings: &Settings) {
    let bounds = gpui::Bounds::centered(None, gpui::size(px(1280.0), px(800.0)), cx);
    let options = gpui::WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("RDM UI Kit".into()),
            ..Default::default()
        }),
        window_min_size: Some(gpui::size(px(theme::MIN_WINDOW_W), px(theme::MIN_WINDOW_H))),
        ..Default::default()
    };
    let settings = settings.clone();
    let result = cx.open_window(options, move |window, cx| {
        // 先按设置应用主题，再创建视图（视图首帧即为正确颜色）。
        let mode = settings.resolved_ui_theme();
        let dark = theme::resolve_dark(mode, window);
        theme::apply(cx, dark, settings.accent, settings.reduce_motion);
        let view = cx.new(|cx| Showcase::new(window, cx, &settings));
        cx.new(|cx| Root::new(view, window, cx))
    });
    if let Ok(handle) = result {
        let _ = handle;
    }
    let _ = cx.activate(true);
}

// ---------------------------------------------------------------------------
// 纯逻辑单测（不依赖 GPU）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::widgets::ellipsize_file_name;

    #[test]
    fn icon_index_covers_rdm_icon_library() {
        // 枚举当前 75 枚；允许后续新增，但不能漏到个位数。
        assert!(ALL_ICONS.len() >= 70, "RdmIcon 全量网格至少 70 枚");
        // 无重复（重复渲染会让网格出现重复项）。
        for (index, icon) in ALL_ICONS.iter().enumerate() {
            assert!(
                !ALL_ICONS[..index].contains(icon),
                "图标重复：{}",
                icon.name()
            );
        }
    }

    #[test]
    fn mode_index_roundtrip() {
        for (index, mode) in UiThemeMode::ALL.iter().enumerate() {
            assert_eq!(mode_index(*mode), index);
            assert_eq!(mode_from_index(index), Some(*mode));
        }
        assert_eq!(mode_from_index(UiThemeMode::ALL.len()), None);
        assert_eq!(mode_index(UiThemeMode::FollowSystem), 0);
    }

    #[test]
    fn button_state_labels_cover_six_states() {
        let states = [
            BtnState::Default,
            BtnState::Hover,
            BtnState::Pressed,
            BtnState::Focus,
            BtnState::Disabled,
            BtnState::Loading,
        ];
        let labels: Vec<&str> = states.iter().map(|state| state.label_zh()).collect();
        assert_eq!(labels.len(), 6);
        for (index, state) in states.iter().enumerate() {
            assert_eq!(state.index(), index + 1, "S 编号必须从 1 连续到 6");
        }
    }

    #[test]
    fn long_file_name_keeps_extension() {
        let short = ellipsize_file_name(LONG_FILE_NAME, 36);
        assert!(short.contains('…'), "超长文件名应做中间省略：{short}");
        assert!(short.ends_with(".flac"), "必须保留扩展名：{short}");
        assert!(short.chars().count() <= 36 + 1, "省略后长度受限：{short}");
        // 很短的名字原样返回。
        assert_eq!(ellipsize_file_name("file.iso", 36), "file.iso");
    }
}
