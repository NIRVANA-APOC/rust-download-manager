//! 桌面系统集成：托盘图标、关闭到托盘、静默启动的窗口显隐、开机自启动。
//!
//! Windows 实现（Win32 + 注册表）在 `desktop/windows_impl.rs`；其它平台编译成明确的
//! 「不支持」桩，方便非 Windows 宿主 `cargo check`。
//!
//! 线程模型：
//! - 托盘图标运行在独立线程（自己的隐藏 Win32 消息窗口 + `GetMessage` 循环），
//!   用户操作经 `tokio::sync::mpsc::unbounded_channel` 送回 GPUI 主线程；explorer 重启
//!   收到 `TaskbarCreated` 广播后会重新添加图标；
//! - 主窗口显隐由 `ShowWindow` 完成，但调用放在短生命周期线程里：GPUI 在帧回调/App 更新
//!   期间持有 `App` 借用，若在同一调用栈里同步派发 `WM_SHOWWINDOW`/`WM_ACTIVATE`，GPUI
//!   的回调会二次借用 `App` 并打出 "RefCell already borrowed" 错误；
//! - 注册表只在启动与保存设置时读写。
//!
//! 三种开关的关系见 `model::Settings`：托盘图标只在 `needs_tray()`（关闭到托盘 /
//! 静默启动）时存在，否则窗口一旦隐藏就没有恢复入口。

/// 托盘图标触发的 UI 事件（托盘线程 → GPUI 主线程）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// 左键单击/双击托盘图标，或右键菜单「显示主界面」。
    ShowMain,
    /// 右键菜单「全部暂停」。
    PauseAll,
    /// 右键菜单「全部继续」。
    ResumeAll,
    /// 右键菜单「打开下载目录」。
    OpenDownloadFolder,
    /// 右键菜单「退出 RDM」。
    Quit,
    /// 全局热键 `Ctrl+Shift+D`（快速添加）。
    QuickAdd,
}

/// 开机自启动（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）。
///
/// `command_line` 是跨平台纯函数（便于单测）；`sync` / `is_enabled` 走平台实现。
pub mod autostart {
    /// Run 键路径（相对 HKEY_CURRENT_USER）。
    pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    /// Run 键下的值名（RDM 独占；卸载/关闭开关时删除）。
    pub const VALUE_NAME: &str = "RDM";

    /// 开机自启动命令行：`"<exe 绝对路径>" --autostart`。
    ///
    /// 路径始终加引号：`%LOCALAPPDATA%`、`C:\Program Files` 等都可能含空格。
    pub fn command_line(exe: &std::path::Path) -> String {
        format!("\"{}\" --autostart", exe.display())
    }

    #[cfg(not(windows))]
    pub(crate) use super::unsupported::{
        autostart_is_enabled as is_enabled, autostart_sync as sync,
    };
    #[cfg(windows)]
    pub(crate) use super::windows_impl::{
        autostart_is_enabled as is_enabled, autostart_sync as sync,
    };
}

/// 初始化托盘事件通道，返回接收端（交给 GPUI 主线程的 async loop）。
///
/// 必须在 `install_tray` 之前调用；只允许调用一次（重复调用会返回一个永不给事件的
/// 空通道）。
pub fn init_event_channel() -> tokio::sync::mpsc::UnboundedReceiver<TrayEvent> {
    imp::init_event_channel()
}

/// 安装托盘图标（幂等：已安装时直接返回 `Ok(())`）。
///
/// 失败时不 panic：调用方应把错误展示给用户并回退「正常显示主窗口」。
pub fn install_tray(tooltip: &str) -> Result<(), String> {
    imp::install_tray(tooltip)
}

/// 移除托盘图标并结束托盘线程（幂等）。
pub fn shutdown_tray() {
    imp::shutdown_tray()
}

/// 托盘图标当前是否已安装且线程存活。
pub fn is_tray_alive() -> bool {
    imp::is_tray_alive()
}

/// 安装「关闭 → 隐藏到托盘」的窗口消息钩子（幂等）。
///
/// 只替换 `rdm-gui` 自己主窗口的 `GWLP_WNDPROC`（链式调用原 GPUI 窗口过程），
/// 不影响同进程其它窗口。最小化**不**拦截（正常最小化到任务栏）；关闭按钮是否真的
/// 隐藏由 [`set_minimize_to_tray`] 的运行期开关决定。
pub fn install_window_hook(hwnd: isize) -> Result<(), String> {
    imp::install_window_hook(hwnd)
}

/// 运行期开关：关闭窗口时隐藏到托盘（来自 `Settings::minimize_to_tray`）。
///
/// 最小化按钮不受影响，仍正常最小化到任务栏。关闭按钮的实际拦截在
/// `install_window_hook` 安装的窗口过程里；这里只维护开关镜像，供窗口过程
/// （拿不到 Entity）读取。
pub fn set_minimize_to_tray(enabled: bool) {
    imp::set_minimize_to_tray(enabled)
}

/// 显示并恢复主窗口（静默启动/托盘恢复/`Request::Activate` 共用）。
pub fn show_window(hwnd: isize) {
    imp::show_window(hwnd)
}

/// 从 GPUI 窗口取 Win32 `HWND` 原始值（非 Windows 返回 `None`）。
/// 安装全局热键 `Ctrl+Shift+D`（快速添加）；非 Windows 返回 Unsupported。
pub fn install_hotkey() -> Result<(), String> {
    imp::install_hotkey()
}

/// 全局热键是否已注册。
pub fn is_hotkey_installed() -> bool {
    imp::is_hotkey_installed()
}

/// 结束全局热键线程（幂等）。
pub fn shutdown_hotkey() {
    imp::shutdown_hotkey()
}

/// 查询目录所在卷的（可用, 总量）字节数；非 Windows 返回 None。
pub fn disk_space_for(path: &std::path::Path) -> Option<(u64, u64)> {
    imp::disk_space_for(path)
}

/// 更新托盘图标提示文本（活动数/速度变化时调用；含 explorer 重启后的重加图标路径）。
pub fn update_tray_tooltip(tooltip: &str) {
    imp::update_tray_tooltip(tooltip);
}

pub fn raw_hwnd(window: &gpui::Window) -> Option<isize> {
    imp::raw_hwnd(window)
}

/// 给主窗口设置 exe 内嵌的 RDM 图标（标题栏左上角 / Alt-Tab；非 Windows 无操作）。
pub fn set_window_icon(hwnd: isize) {
    imp::set_window_icon(hwnd)
}

/// 托盘图标的提示文本。
pub fn tray_tooltip() -> String {
    format!("RDM — Rust Download Manager v{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(windows)]
pub(crate) mod windows_impl;
#[cfg(windows)]
use windows_impl as imp;

#[cfg(not(windows))]
pub(crate) mod unsupported;
#[cfg(not(windows))]
use unsupported as imp;

#[cfg(test)]
mod tests {
    use super::autostart;

    #[test]
    fn autostart_command_line_quotes_exe_path() {
        let exe = std::path::Path::new(r"C:\Program Files\RDM\rdm-gui.exe");
        assert_eq!(
            autostart::command_line(exe),
            r#""C:\Program Files\RDM\rdm-gui.exe" --autostart"#
        );
    }

    #[test]
    fn autostart_value_name_is_stable() {
        // 值名会被升级/自检脚本依赖，改动需要同步文档与测试。
        assert_eq!(autostart::VALUE_NAME, "RDM");
        assert!(autostart::RUN_KEY.ends_with(r"CurrentVersion\Run"));
    }
}
