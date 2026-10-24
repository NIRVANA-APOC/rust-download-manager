//! 新版 UI 视图层（下载中枢）。
//!
//! 模块拆分见 `docs/design/08-gpui-implementation.md` §4.2；M2 首先落地主窗口壳
//! （app.rs），其余视图在该文件内以方法实现，随 M2/M3 拆成独立模块。

pub mod actions;
pub mod app;
pub mod experience;
pub mod monitor;
pub mod settings;
