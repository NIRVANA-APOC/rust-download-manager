//! 深色 / 浅色两套调色板。视图层只从 `Palette` 取颜色，避免散落的魔法色值。

use gpui::{Rgba, rgb, rgba};

use crate::model::Theme;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// 窗口底色
    pub bg: Rgba,
    /// 顶栏 / 侧栏 / 卡片
    pub panel: Rgba,
    /// 次级面板（列表行悬浮、输入框）
    pub panel_alt: Rgba,
    /// 列表行交替色
    pub row_alt: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub text_dim: Rgba,
    pub text_faint: Rgba,
    pub accent: Rgba,
    pub accent_hover: Rgba,
    pub accent_soft: Rgba,
    pub success: Rgba,
    pub warning: Rgba,
    pub danger: Rgba,
    pub info: Rgba,
    /// 进度条轨道
    pub track: Rgba,
    /// 弹窗遮罩
    pub overlay: Rgba,
}

impl Palette {
    pub fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self {
                bg: rgb(0x0f1319),
                panel: rgb(0x151b23),
                panel_alt: rgb(0x1b232e),
                row_alt: rgb(0x121821),
                border: rgb(0x263141),
                text: rgb(0xe6edf3),
                text_dim: rgb(0x93a1b3),
                text_faint: rgb(0x5f6c7d),
                accent: rgb(0x3b82f6),
                accent_hover: rgb(0x60a5fa),
                accent_soft: rgba(0x3b82f633),
                success: rgb(0x22c55e),
                warning: rgb(0xf59e0b),
                danger: rgb(0xef4444),
                info: rgb(0x38bdf8),
                track: rgb(0x233044),
                overlay: rgba(0x000000a8),
            },
            Theme::Light => Self {
                bg: rgb(0xf2f5f9),
                panel: rgb(0xffffff),
                panel_alt: rgb(0xf5f8fc),
                row_alt: rgb(0xf7f9fc),
                border: rgb(0xd6dfe9),
                text: rgb(0x1b2430),
                text_dim: rgb(0x5a6878),
                text_faint: rgb(0x8b98a6),
                accent: rgb(0x2563eb),
                accent_hover: rgb(0x1d4ed8),
                accent_soft: rgba(0x2563eb26),
                success: rgb(0x15803d),
                warning: rgb(0xb45309),
                danger: rgb(0xb91c1c),
                info: rgb(0x0369a1),
                track: rgb(0xdde6f0),
                overlay: rgba(0x0f131980),
            },
        }
    }
}
