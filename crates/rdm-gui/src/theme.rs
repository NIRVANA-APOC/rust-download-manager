//! 深色 / 浅色 / Catppuccin 四套调色板。视图层只从 `Palette` 取颜色，
//! 避免散落的魔法色值。
//!
//! Catppuccin 取值来自官方 palette（Latte/Frappe/Macchiato/Mocha）：
//! - 窗口底色 `base`、面板 `mantle`、输入框/悬浮 `surface0`、交替行 `crust`、
//!   边框/进度轨道 `surface1`；
//! - 文字 `text` / `subtext0` / `overlay0`；
//! - 强调 `blue`（hover 用 `sky`），状态色 `green` / `yellow` / `red`。

use gpui::{rgb, rgba, Rgba};

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
    /// 强调色/危险色按钮上的前景色（保证对比度）。
    pub on_accent: Rgba,
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
            Theme::Dark => dark(),
            Theme::Light => light(),
            Theme::CatppuccinLatte => catppuccin_latte(),
            Theme::CatppuccinFrappe => catppuccin_frappe(),
            Theme::CatppuccinMacchiato => catppuccin_macchiato(),
            Theme::CatppuccinMocha => catppuccin_mocha(),
        }
    }
}

fn dark() -> Palette {
    Palette {
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
        on_accent: rgb(0xffffff),
        success: rgb(0x22c55e),
        warning: rgb(0xf59e0b),
        danger: rgb(0xef4444),
        info: rgb(0x38bdf8),
        track: rgb(0x233044),
        overlay: rgba(0x000000a8),
    }
}

fn light() -> Palette {
    Palette {
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
        on_accent: rgb(0xffffff),
        success: rgb(0x15803d),
        warning: rgb(0xb45309),
        danger: rgb(0xb91c1c),
        info: rgb(0x0369a1),
        track: rgb(0xdde6f0),
        overlay: rgba(0x0f131980),
    }
}

/// Catppuccin Latte（浅色）：base `#eff1f5`，blue `#1e66f5`。
fn catppuccin_latte() -> Palette {
    Palette {
        bg: rgb(0xeff1f5),
        panel: rgb(0xe6e9ef),
        panel_alt: rgb(0xccd0da),
        row_alt: rgb(0xdce0e8),
        border: rgb(0xbcc0cc),
        text: rgb(0x4c4f69),
        text_dim: rgb(0x6c6f85),
        text_faint: rgb(0x9ca0b0),
        accent: rgb(0x1e66f5),
        accent_hover: rgb(0x04a5e5),
        accent_soft: rgba(0x1e66f533),
        on_accent: rgb(0xffffff),
        success: rgb(0x40a02b),
        warning: rgb(0xdf8e1d),
        danger: rgb(0xd20f39),
        info: rgb(0x04a5e5),
        track: rgb(0xccd0da),
        overlay: rgba(0x4c4f69a8),
    }
}

/// Catppuccin Frappe（深色）：base `#303446`，blue `#8caaee`。
fn catppuccin_frappe() -> Palette {
    Palette {
        bg: rgb(0x303446),
        panel: rgb(0x292c3c),
        panel_alt: rgb(0x414559),
        row_alt: rgb(0x232634),
        border: rgb(0x51576d),
        text: rgb(0xc6d0f5),
        text_dim: rgb(0xa5adce),
        text_faint: rgb(0x737994),
        accent: rgb(0x8caaee),
        accent_hover: rgb(0x99d1db),
        accent_soft: rgba(0x8caaee33),
        on_accent: rgb(0x303446),
        success: rgb(0xa6d189),
        warning: rgb(0xe5c890),
        danger: rgb(0xe78284),
        info: rgb(0x99d1db),
        track: rgb(0x51576d),
        overlay: rgba(0x232634a8),
    }
}

/// Catppuccin Macchiato（深色）：base `#24273a`，blue `#8aadf4`。
fn catppuccin_macchiato() -> Palette {
    Palette {
        bg: rgb(0x24273a),
        panel: rgb(0x1e2030),
        panel_alt: rgb(0x363a4f),
        row_alt: rgb(0x181926),
        border: rgb(0x494d64),
        text: rgb(0xcad3f5),
        text_dim: rgb(0xa5adcb),
        text_faint: rgb(0x6e738d),
        accent: rgb(0x8aadf4),
        accent_hover: rgb(0x91d7e3),
        accent_soft: rgba(0x8aadf433),
        on_accent: rgb(0x24273a),
        success: rgb(0xa6da95),
        warning: rgb(0xeed49f),
        danger: rgb(0xed8796),
        info: rgb(0x91d7e3),
        track: rgb(0x494d64),
        overlay: rgba(0x181926a8),
    }
}

/// Catppuccin Mocha（深色）：base `#1e1e2e`，blue `#89b4fa`。
fn catppuccin_mocha() -> Palette {
    Palette {
        bg: rgb(0x1e1e2e),
        panel: rgb(0x181825),
        panel_alt: rgb(0x313244),
        row_alt: rgb(0x11111b),
        border: rgb(0x45475a),
        text: rgb(0xcdd6f4),
        text_dim: rgb(0xa6adc8),
        text_faint: rgb(0x6c7086),
        accent: rgb(0x89b4fa),
        accent_hover: rgb(0x89dceb),
        accent_soft: rgba(0x89b4fa33),
        on_accent: rgb(0x1e1e2e),
        success: rgb(0xa6e3a1),
        warning: rgb(0xf9e2af),
        danger: rgb(0xf38ba8),
        info: rgb(0x89dceb),
        track: rgb(0x45475a),
        overlay: rgba(0x11111ba8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_has_a_distinct_palette() {
        let palettes: Vec<Palette> = Theme::ALL.iter().copied().map(Palette::for_theme).collect();
        assert_eq!(palettes.len(), 6);
        for (i, left) in palettes.iter().enumerate() {
            for right in palettes.iter().skip(i + 1) {
                assert_ne!(
                    (left.bg, left.accent),
                    (right.bg, right.accent),
                    "两个主题不应共用底色+强调色"
                );
            }
        }
    }

    #[test]
    fn palettes_keep_text_contrasting_with_background() {
        for theme in Theme::ALL {
            let palette = Palette::for_theme(theme);
            assert_ne!(palette.text, palette.bg, "{theme:?} 的文字色不能等于底色");
            assert_ne!(palette.border, palette.bg, "{theme:?} 的边框色不能等于底色");
        }
    }
}
