//! 非 Windows 宿主的「不支持」桩。
//!
//! RDM 桌面端目前只在 Windows 上验证过（README「已知限制」）；这里保证非 Windows
//! 目标仍能 `cargo check` 通过，并让调用方拿到明确错误而不是静默失效。

use tokio::sync::mpsc::UnboundedReceiver;

use super::TrayEvent;

const UNSUPPORTED: &str = "托盘/开机自启动仅支持 Windows";

pub(crate) fn init_event_channel() -> UnboundedReceiver<TrayEvent> {
    // 发送端立即丢弃：接收端会立刻结束。
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    rx
}

pub(crate) fn install_tray(_tooltip: &str) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

pub(crate) fn shutdown_tray() {}

pub(crate) fn is_tray_alive() -> bool {
    false
}

pub(crate) fn update_tray_tooltip(_tooltip: &str) {}

pub(crate) fn install_hotkey() -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

pub(crate) fn shutdown_hotkey() {}

pub(crate) fn is_hotkey_installed() -> bool {
    false
}

pub(crate) fn disk_space_for(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

pub(crate) fn install_window_hook(_hwnd: isize) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

pub(crate) fn set_minimize_to_tray(_enabled: bool) {}

pub(crate) fn show_window(_hwnd: isize) {}

pub(crate) fn raw_hwnd(_window: &gpui::Window) -> Option<isize> {
    None
}

pub(crate) fn set_window_icon(_hwnd: isize) {}

pub(crate) fn autostart_sync(_enabled: bool) -> Result<(), String> {
    Err(UNSUPPORTED.to_string())
}

pub(crate) fn autostart_is_enabled() -> bool {
    false
}
