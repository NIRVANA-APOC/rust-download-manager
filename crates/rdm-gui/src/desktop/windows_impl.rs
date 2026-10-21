//! Win32 实现：托盘图标、主窗口显隐、关闭到托盘钩子、开机自启动注册表项。
//!
//! 所有 `unsafe` Win32 调用都集中在这个文件里，上层（`app.rs` / `main.rs`）只看到
//! 安全的封装。托盘线程有独立的隐藏消息窗口，因此不会和 GPUI 的窗口过程/消息循环
//! 互相干扰；主窗口的**关闭**拦截通过链式替换 `GWLP_WNDPROC` 实现，原窗口过程仍是
//! GPUI 自己的 `window_procedure`，所以 GPUI 的绘制/输入/最小化逻辑不受影响
//! （最小化不拦截，交给原窗口过程正常最小化到任务栏）。

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallWindowProcW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, IsIconic,
    LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TrackPopupMenu, TranslateMessage,
    GWLP_WNDPROC, HICON, IDI_APPLICATION, MF_SEPARATOR, MF_STRING, MSG, SW_HIDE, SW_RESTORE,
    SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_STYLE, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
    WNDCLASS_STYLES, WNDPROC, WS_EX_TOOLWINDOW,
};

use super::TrayEvent;

/// 托盘图标在任务栏里的唯一 ID（同进程内唯一即可）。
const TRAY_UID: u32 = 1;
/// 托盘图标回调消息（自定义消息固定从 `WM_APP` 起）。
const WM_RDM_TRAY: u32 = WM_APP + 1;
const MENU_SHOW: usize = 1;
const MENU_QUIT: usize = 2;
/// `ERROR_FILE_NOT_FOUND` 的 HRESULT 形式（删除不存在的注册表值时视为成功）。
const HRESULT_FILE_NOT_FOUND: i32 = 0x8007_0002u32 as i32;
/// 托盘图标首次添加的重试次数/间隔：登录时 explorer 可能还没就绪，
/// `Shell_NotifyIcon` 会短暂失败，稍后重试即可恢复。
const ICON_ATTEMPTS: usize = 8;
const ICON_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// 测试专用（不写用户文档）：低完整性/无 shell 会话里 `Shell_NotifyIcon` 会被 UIPI
/// 拒绝（ERROR_ACCESS_DENIED），无法自动化验证托盘图标本身。设置该环境变量后，
/// 图标添加失败时保留托盘消息窗口并按「已安装」继续，供沙箱冒烟测试验证
/// 窗口显隐/托盘消息/退出链路；真实运行永远不会设置它。
const TEST_ICONLESS_ENV: &str = "RDM_TEST_TRAY_WITHOUT_ICON";

/// 托盘事件发送端（由 `init_event_channel` 设置一次）。
static EVENTS: OnceLock<UnboundedSender<TrayEvent>> = OnceLock::new();
/// 托盘线程是否已成功添加图标。
static TRAY_ALIVE: AtomicBool = AtomicBool::new(false);
/// 托盘隐藏消息窗口的 `HWND`（0 = 未创建）。
static TRAY_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// `Settings::minimize_to_tray`（开启后关闭窗口时隐藏到托盘）的运行期镜像；
/// 窗口过程里拿不到 Entity，只能读这个原子量。
static MINIMIZE_TO_TRAY: AtomicBool = AtomicBool::new(false);
/// 被替换下来的原窗口过程（GPUI 的 `window_procedure`）。
static OLD_WNDPROC: AtomicIsize = AtomicIsize::new(0);
/// 托盘提示文本（explorer 重启后重新添加图标时复用）。
static TOOLTIP: Mutex<Option<String>> = Mutex::new(None);
/// `TaskbarCreated` 广播消息 id（explorer 重启后需要重新 `Shell_NotifyIcon`）。
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
/// 托盘线程句柄（`shutdown_tray` 时 join）。
static TRAY_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// 事件通道 / 托盘生命周期
// ---------------------------------------------------------------------------

pub(crate) fn init_event_channel() -> tokio::sync::mpsc::UnboundedReceiver<TrayEvent> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    // 只允许初始化一次；重复调用返回的通道永远不会收到事件（调用方是单实例 GUI）。
    let _ = EVENTS.set(sender);
    receiver
}

fn send_event(event: TrayEvent) {
    if let Some(sender) = EVENTS.get() {
        let _ = sender.send(event);
    }
}

pub(crate) fn install_tray(tooltip: &str) -> Result<(), String> {
    if TRAY_ALIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    if EVENTS.get().is_none() {
        return Err("托盘事件通道未初始化（内部错误）".to_string());
    }
    if let Ok(mut slot) = TOOLTIP.lock() {
        *slot = Some(tooltip.to_string());
    }
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let tooltip = tooltip.to_string();
    let thread = std::thread::Builder::new()
        .name("rdm-tray".to_string())
        .spawn(move || tray_thread(&tooltip, ready_tx))
        .map_err(|err| format!("创建托盘线程失败: {err}"))?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {
            if let Ok(mut slot) = TRAY_THREAD.lock() {
                *slot = Some(thread);
            }
            Ok(())
        }
        Ok(Err(err)) => {
            let _ = thread.join();
            Err(err)
        }
        Err(_) => Err("托盘图标初始化超时（5s）".to_string()),
    }
}

pub(crate) fn shutdown_tray() {
    let raw = TRAY_WINDOW.swap(0, Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    let handle = TRAY_THREAD.lock().ok().and_then(|mut slot| slot.take());
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    TRAY_ALIVE.store(false, Ordering::SeqCst);
}

pub(crate) fn is_tray_alive() -> bool {
    TRAY_ALIVE.load(Ordering::SeqCst)
}

fn tray_thread(tooltip: &str, ready: mpsc::Sender<Result<(), String>>) {
    // explorer 重启后会向所有顶层窗口广播 TaskbarCreated，先取到消息 id。
    let _ = TASKBAR_CREATED.set(unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) });
    match unsafe { create_tray_window(tooltip) } {
        Ok(hwnd) => {
            TRAY_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
            TRAY_ALIVE.store(true, Ordering::SeqCst);
            let _ = ready.send(Ok(()));
            tracing::info!(hwnd = hwnd.0 as isize, "托盘图标已安装");
            unsafe { message_loop() };
            TRAY_ALIVE.store(false, Ordering::SeqCst);
            TRAY_WINDOW.store(0, Ordering::SeqCst);
            tracing::info!("托盘线程结束");
        }
        Err(err) => {
            tracing::warn!(error = %err, "托盘图标安装失败");
            let _ = ready.send(Err(err));
        }
    }
}

unsafe fn create_tray_window(tooltip: &str) -> Result<HWND, String> {
    let hinstance = GetModuleHandleW(None)
        .map(|module| HINSTANCE(module.0))
        .map_err(|err| format!("GetModuleHandleW 失败: {err}"))?;
    let class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(tray_wndproc),
        hInstance: hinstance,
        lpszClassName: w!("RDM.TrayIconWindow"),
        ..Default::default()
    };
    // 重复安装时窗口类可能已注册（ERROR_CLASS_ALREADY_EXISTS），忽略注册结果，
    // 真正的失败由 CreateWindowExW 报告。
    let _atom = RegisterClassW(&class);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        w!("RDM.TrayIconWindow"),
        w!("RDM Tray"),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinstance),
        None,
    )
    .map_err(|err| format!("创建托盘消息窗口失败: {err}"))?;
    if !add_icon_with_retries(hwnd, tooltip) {
        let code = windows::core::Error::from_win32().code().0;
        if std::env::var_os(TEST_ICONLESS_ENV).is_some() {
            tracing::warn!(
                win32_error = code,
                "Shell_NotifyIcon 失败；已设置 {TEST_ICONLESS_ENV}，测试模式继续（无可见图标）"
            );
        } else {
            let _ = DestroyWindow(hwnd);
            return Err(format!(
                "Shell_NotifyIcon 添加托盘图标失败（Win32 错误 {code}）"
            ));
        }
    }
    Ok(hwnd)
}

/// 添加托盘图标，带短暂重试（覆盖开机登录时 explorer 尚未就绪的竞态）。
unsafe fn add_icon_with_retries(hwnd: HWND, tooltip: &str) -> bool {
    // 测试模式下（环境变量存在）不做重试：直接暴露一次失败并保留托盘消息窗口。
    let attempts = if std::env::var_os(TEST_ICONLESS_ENV).is_some() {
        1
    } else {
        ICON_ATTEMPTS
    };
    for attempt in 1..=attempts {
        if add_icon(hwnd, tooltip) {
            return true;
        }
        if attempt < attempts {
            std::thread::sleep(ICON_RETRY_INTERVAL);
        }
    }
    false
}

unsafe fn add_icon(hwnd: HWND, tooltip: &str) -> bool {
    let hinstance = GetModuleHandleW(None)
        .map(|module| HINSTANCE(module.0))
        .unwrap_or_default();
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_RDM_TRAY,
        hIcon: load_icon(hinstance).unwrap_or_default(),
        ..Default::default()
    };
    copy_wide(&mut data.szTip, tooltip);
    Shell_NotifyIconW(NIM_ADD, &data).as_bool()
}

/// 优先用 exe 里嵌入的 RDM 图标（`build.rs` 的 `IDI_RDM_APP`），失败回落系统默认图标。
unsafe fn load_icon(hinstance: HINSTANCE) -> Option<HICON> {
    if let Ok(icon) = LoadIconW(Some(hinstance), w!("IDI_RDM_APP")) {
        return Some(icon);
    }
    LoadIconW(None, IDI_APPLICATION).ok()
}

unsafe fn message_loop() {
    let mut message = MSG::default();
    loop {
        let result = GetMessageW(&mut message, None, 0, 0);
        // 0 = WM_QUIT，-1 = 错误：两种情况都退出托盘线程。
        if result.0 <= 0 {
            break;
        }
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // explorer 重启（任务栏重建）：重新添加图标。
    if let Some(&taskbar_created) = TASKBAR_CREATED.get() {
        if taskbar_created != 0 && message == taskbar_created {
            let tooltip = TOOLTIP
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .unwrap_or_default();
            let _ = add_icon_with_retries(hwnd, &tooltip);
            return LRESULT(0);
        }
    }
    if message == WM_RDM_TRAY {
        // 兼容 NOTIFYICON_VERSION 0：lParam 的低 16 位是鼠标消息。
        let event = (lparam.0 as u32) & 0xFFFF;
        if event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK {
            send_event(TrayEvent::ShowMain);
        } else if event == WM_RBUTTONUP {
            show_menu(hwnd);
        }
        return LRESULT(0);
    }
    if message == WM_COMMAND {
        let id = wparam.0 & 0xFFFF;
        match id {
            MENU_SHOW => send_event(TrayEvent::ShowMain),
            MENU_QUIT => send_event(TrayEvent::Quit),
            _ => {}
        }
        return LRESULT(0);
    }
    if message == WM_CLOSE {
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }
    if message == WM_DESTROY {
        let data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_UID,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        TRAY_ALIVE.store(false, Ordering::SeqCst);
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("显示主界面"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("退出 RDM"));
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    // 右键菜单需要前台窗口才能正常关闭（点击别处消失）。
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
}

fn copy_wide(dst: &mut [u16], text: &str) {
    let mut len = 0usize;
    for unit in text.encode_utf16() {
        if len + 1 >= dst.len() {
            break;
        }
        dst[len] = unit;
        len += 1;
    }
    dst[len] = 0;
}

// ---------------------------------------------------------------------------
// 主窗口显隐 / 关闭到托盘钩子
// ---------------------------------------------------------------------------

pub(crate) fn show_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    std::thread::spawn(move || unsafe {
        let window = HWND(hwnd as *mut _);
        // 窗口可能只是「最小化到任务栏」（2026-10-20 起最小化不再进托盘），此时 SW_SHOW
        // 不会取消最小化、窗口仍缩在任务栏；先用 SW_RESTORE 恢复，再按需显示。
        let command = if IsIconic(window).as_bool() {
            SW_RESTORE
        } else {
            SW_SHOW
        };
        let _ = ShowWindow(window, command);
        // 托盘点击时本进程刚收到输入，通常允许抢前台；失败也不致命（窗口已可见）。
        let _ = SetForegroundWindow(window);
    });
}

pub(crate) fn hide_window(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    std::thread::spawn(move || unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
    });
}

pub(crate) fn set_minimize_to_tray(enabled: bool) {
    MINIMIZE_TO_TRAY.store(enabled, Ordering::SeqCst);
}

/// 是否允许把**关闭按钮**改为「隐藏到托盘」：开关打开且托盘图标存活。
///
/// 最小化按钮不经过这里：`hooked_wndproc` 只拦截 WM_CLOSE，SC_MINIMIZE 会转发给
/// GPUI 的原窗口过程，正常最小化到任务栏。
pub(crate) fn can_hide_to_tray() -> bool {
    MINIMIZE_TO_TRAY.load(Ordering::SeqCst) && TRAY_ALIVE.load(Ordering::SeqCst)
}

pub(crate) fn install_window_hook(hwnd: isize) -> Result<(), String> {
    if hwnd == 0 {
        return Err("主窗口句柄为空，无法安装关闭到托盘钩子".to_string());
    }
    if OLD_WNDPROC.load(Ordering::SeqCst) != 0 {
        return Ok(());
    }
    unsafe {
        let window = HWND(hwnd as *mut _);
        let previous = GetWindowLongPtrW(window, GWLP_WNDPROC);
        if previous == 0 {
            let code = windows::core::Error::from_win32().code().0;
            return Err(format!("读取主窗口过程失败（Win32 错误 {code}）"));
        }
        SetWindowLongPtrW(
            window,
            GWLP_WNDPROC,
            hooked_wndproc as *const () as usize as isize,
        );
        OLD_WNDPROC.store(previous, Ordering::SeqCst);
    }
    tracing::info!("已安装关闭到托盘窗口钩子");
    Ok(())
}

/// 链式窗口过程：只拦截「关闭」（隐藏到托盘），其余消息（含最小化）全部转发给
/// GPUI 的原窗口过程。
///
/// 关闭到托盘必须在这里拦截（而不是 `Window::on_window_should_close`）：WM_CLOSE
/// 直接返回 0 即可，不进入 GPUI 的关闭流程；窗口过程运行在窗口自己的线程上，行为最直接。
///
/// **不拦截 WM_SYSCOMMAND/SC_MINIMIZE**：最小化必须走原窗口过程（正常最小化到任务栏），
/// 否则用户点最小化会误进托盘、任务栏也找不到窗口。是否隐藏只由 `can_hide_to_tray`
///（开关开 + 托盘存活）决定，且只作用于关闭按钮。
unsafe extern "system" fn hooked_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLOSE && can_hide_to_tray() {
        hide_window(hwnd.0 as isize);
        return LRESULT(0);
    }
    let previous_raw = OLD_WNDPROC.load(Ordering::SeqCst);
    if previous_raw == 0 {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let previous: WNDPROC = std::mem::transmute(previous_raw);
    CallWindowProcW(previous, hwnd, message, wparam, lparam)
}

pub(crate) fn raw_hwnd(window: &gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // 注意：gpui::Window 自身有一个同名固有方法 `window_handle()`（返回 GPUI 句柄），
    // 这里必须用完全限定语法调用 `HasWindowHandle` 的 trait 方法。
    let handle: raw_window_handle::WindowHandle<'_> =
        HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 开机自启动（HKCU\...\Run）
// ---------------------------------------------------------------------------

pub(crate) fn autostart_sync(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("获取可执行文件路径失败: {err}"))?;
    let command = super::autostart::command_line(&exe);
    write_run_value(if enabled {
        Some(command.as_str())
    } else {
        None
    })
}

pub(crate) fn autostart_is_enabled() -> bool {
    read_run_value().is_some()
}

fn run_key_read() -> Result<windows_registry::Key, String> {
    windows_registry::CURRENT_USER
        .open(super::autostart::RUN_KEY)
        .map_err(|err| format!("打开注册表自启动项失败: {err}"))
}

fn run_key_write() -> Result<windows_registry::Key, String> {
    windows_registry::CURRENT_USER
        .create(super::autostart::RUN_KEY)
        .map_err(|err| format!("打开注册表自启动项失败: {err}"))
}

fn write_run_value(command: Option<&str>) -> Result<(), String> {
    write_named_value(super::autostart::VALUE_NAME, command)
}

fn write_named_value(name: &str, command: Option<&str>) -> Result<(), String> {
    match command {
        Some(command) => {
            let key = run_key_write()?;
            key.set_string(name, command)
                .map_err(|err| format!("写入开机自启动失败: {err}"))
        }
        None => {
            // 关闭路径尽量只读：Run 键/值本来就不存在时直接成功（也避免在
            // 低权限会话里因为「无需写入」而误报权限错误）。
            let Ok(key) = run_key_read() else {
                return Ok(());
            };
            if key.get_string(name).is_err() {
                return Ok(());
            }
            match key.remove_value(name) {
                Ok(()) => Ok(()),
                Err(err) if err.code().0 == HRESULT_FILE_NOT_FOUND => Ok(()),
                Err(err) => Err(format!("删除开机自启动失败: {err}")),
            }
        }
    }
}

fn read_run_value() -> Option<String> {
    read_named_value(super::autostart::VALUE_NAME)
}

fn read_named_value(name: &str) -> Option<String> {
    let key = run_key_read().ok()?;
    key.get_string(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_registry_roundtrip_uses_run_key() {
        // 用进程号隔离值名，验证真实 HKCU\...\Run 的写入/读取/删除路径，
        // 不碰生产值名 "RDM"（避免影响本机已启用的自启动项）。
        let name = format!("RDM-test-{}", std::process::id());
        let command = format!(
            "\"{}\" --autostart",
            std::env::current_exe().unwrap().display()
        );
        // 低完整性沙箱（如 DSH workspace-write）禁止写 HKCU：这种环境下跳过真实
        // 注册表往返，避免误报失败；普通开发机/CI 会真正执行下面的断言。
        if let Err(err) = write_named_value(&name, Some(&command)) {
            eprintln!("SKIP autostart_registry_roundtrip_uses_run_key: {err}");
            return;
        }
        assert_eq!(read_named_value(&name).as_deref(), Some(command.as_str()));
        // 删除是幂等的：值不存在时也算成功（启动时清理陈旧项走同一条路径）。
        write_named_value(&name, None).expect("删除测试值");
        write_named_value(&name, None).expect("重复删除也成功");
        assert!(read_named_value(&name).is_none());
    }

    #[test]
    fn close_to_tray_flag_only_hides_when_tray_alive() {
        set_minimize_to_tray(true);
        // 单测里没有托盘线程，因此即便开关打开也不允许把窗口藏起来（否则失联）。
        // 注意：该开关只影响关闭按钮（WM_CLOSE）；最小化不拦截、正常到任务栏。
        assert!(!can_hide_to_tray());
        set_minimize_to_tray(false);
        assert!(!can_hide_to_tray());
    }

    #[test]
    fn wide_copy_truncates_and_terminates() {
        let mut buffer = [0xFFFFu16; 8];
        copy_wide(&mut buffer, "RDM 下载");
        assert_eq!(buffer[0], 'R' as u16);
        assert_eq!(buffer[3], ' ' as u16);
        assert_eq!(buffer[4], '下' as u16);
        assert_eq!(buffer[5], '载' as u16);
        assert_eq!(buffer[6], 0);
        // 最后一个位置没有被写坏。
        assert_eq!(buffer[7], 0xFFFF);
    }
}
