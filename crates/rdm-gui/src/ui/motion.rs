//! 动效（04 §5.4）：只用于层级变化，≤160ms；提供「减少动效」开关。
//!
//! GPUI 0.2.2 用 `Animation`/`with_animation` 做过渡；本期先统一时长常量与
//! 减少动效策略，视图层按需使用。循环动画只允许 spinner。

#![allow(dead_code)] // 设计 token 基线：视图层按需使用

use std::time::Duration;

use gpui::App;

/// 立即生效（状态色/图标变化）。
pub const INSTANT: Duration = Duration::from_millis(0);
/// 按钮按下、勾选、开关。
pub const FAST: Duration = Duration::from_millis(90);
/// 悬停/选中背景、检查器展开。
pub const BASE: Duration = Duration::from_millis(120);
/// 命令面板/弹层淡入 + 4px 上移。
pub const OVERLAY: Duration = Duration::from_millis(160);
/// 进度条宽度（与引擎 250ms 泵对齐）。
pub const PROGRESS: Duration = Duration::from_millis(250);
/// 唯一允许的循环动画：spinner。
pub const SPIN: Duration = Duration::from_millis(800);

/// 按「减少动效」设置收敛时长。
pub fn duration(cx: &App, duration: Duration) -> Duration {
    if crate::ui::theme::reduce_motion(cx) {
        INSTANT
    } else {
        duration
    }
}

/// 是否允许动效。
pub fn enabled(cx: &App) -> bool {
    !crate::ui::theme::reduce_motion(cx)
}
