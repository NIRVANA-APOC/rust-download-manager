//! RDM 新版 UI Kit（docs/design/04/05/08）。
//!
//! - [`theme`]：语义 token、字体、尺寸常量、gpui-component 主题映射；
//! - [`assets`]：自定义 SVG 图标 + gpui-component 内建图标；
//! - [`icons`]：图标统一入口（`RdmIcon`）；
//! - [`widgets`]：基础组件（按钮/徽章/进度/状态 Pill/空态/Kbd…）；
//! - [`motion`]：动效时长与「减少动效」；
//! - 全局 action 与快捷键在 `views/actions.rs`；
//! - [`showcase`]：组件展示窗（`--ui=kit`，M1 验收用）。

pub mod assets;
pub mod icons;
pub mod motion;
pub mod showcase;
pub mod theme;
pub mod widgets;

/// 常用类型/工具的一次性导入（视图模块内部使用）。
#[allow(unused_imports)]
pub mod prelude {
    pub use gpui::{px, AnyElement, Hsla, IntoElement};
    pub use gpui_component::{h_flex, v_flex, ActiveTheme, Icon, Sizable};

    pub use super::icons::{builtin, RdmIcon};
    pub use super::theme::{tokens, ThemeTokens};
    pub use super::widgets::*;
}
