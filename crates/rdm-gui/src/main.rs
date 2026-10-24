//! RDM 桌面 GUI 入口。
//!
//! 启动顺序（与 `docs/extension-protocol.md` 和 Firefox 宿主的依赖约定一致）：
//! 1. 单实例检测：若 `ipc.json` 指向的 GUI 存活，则经 IPC 投递命令行 URL（或 Activate）
//!    后退出；`--silent` 启动不会抬起已运行实例的窗口；
//! 2. 建 tokio runtime → `bind 127.0.0.1:0` → `rdm_ipc::write_info`（尽早发布，宿主轮询）；
//! 3. 启动 IPC accept 循环与引擎 250ms ticker；
//! 4. 扫描 `%LOCALAPPDATA%\RDM\queue\*.json` 兜底队列（宿主在 GUI 未启动时的落盘任务）；
//! 5. 打开 GPUI 窗口（1180x760，暗色/浅色主题）；「静默启动」时窗口创建但不显示；
//! 6. 窗口关闭后清理托盘图标、删除 `ipc.json`、退出 tokio runtime。
//!
//! 系统集成（`desktop` 模块）：托盘图标 / 关闭到托盘（最小化仍到任务栏） / 开机自启动
//! （HKCU Run）。是否安装托盘由 `Settings::needs_tray()` 决定，注册表在启动和保存设置时同步。
//!
//! `rdm-gui.exe [--silent] [--autostart] [URL]` 支持启动即添加任务；GUI 已在运行时则走
//! 单实例投递。`--help` 打印用法。
//!
//! **子系统**：debug 默认是控制台子系统（便于开发看 tracing 输出，双击会弹命令行窗口）；
//! release 或启用 `windows-subsystem` feature 时是 GUI 子系统：双击不弹控制台，日志改写到
//! `%LOCALAPPDATA%\RDM\logs\gui.log`（安装版脚本用 `--features windows-subsystem` 构建）。

#![cfg_attr(
    any(not(debug_assertions), feature = "windows-subsystem"),
    windows_subsystem = "windows"
)]

mod agent;
mod ai_resolve;
mod app;
mod cli;
mod desktop;
mod engine;
mod experience_assistant;
mod ipc_server;
mod model;
mod monitor;
mod open_path;
mod resolve_session;
mod spool;
mod text_input;
mod theme;
mod ui;
mod views;

use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    px, size, App, Application, Bounds, Focusable, KeyBinding, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use rdm_ipc::{EnqueueItem, Request};
use tokio::sync::mpsc;

use crate::app::{Bootstrap, CloseDialog, GuiInputs, RdmApp, SettingsTab};
use crate::cli::UiModeArg;
use crate::engine::{EngineMsg, EngineRuntime};
use crate::text_input::{
    Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft,
    SelectRight, Submit, TextInput,
};
use crate::theme::Palette;

const WINDOW_WIDTH: f32 = 1180.0;
const WINDOW_HEIGHT: f32 = 760.0;
const MIN_WINDOW_WIDTH: f32 = 900.0;
const MIN_WINDOW_HEIGHT: f32 = 560.0;

fn main() {
    init_logging();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "RDM GUI 启动"
    );
    let state_dir = rdm_ipc::state_dir();

    // ---- 0. 命令行参数 ----------------------------------------------------
    let args = match cli::parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            tracing::error!(error = %err, "命令行参数错误");
            eprintln!("rdm-gui: {err}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if args.help {
        println!("{}", cli::USAGE);
        return;
    }
    let ui_mode = args.resolved_ui();
    tracing::info!(ui = ?ui_mode, "UI 模式");
    if args.autostart {
        tracing::info!("由开机自启动项启动");
    }
    let cli_url = args.url.clone();

    // ---- 1. 单实例 --------------------------------------------------------
    if rdm_ipc::is_gui_alive() {
        if let Some(url) = cli_url.as_deref() {
            let request = match model::validate_url(url) {
                Ok(url) => Request::Enqueue {
                    item: EnqueueItem {
                        download: rdm_core::DownloadRequest::new(url),
                        source: "cli".to_string(),
                        source_page: None,
                    },
                },
                Err(err) => {
                    eprintln!("rdm-gui: 非法 URL: {err}");
                    std::process::exit(2);
                }
            };
            match rdm_ipc::send(request, Duration::from_secs(3)) {
                Ok(response) => {
                    let download_id = response
                        .data
                        .as_ref()
                        .and_then(|data| data.get("download_id"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<unknown>");
                    println!("RDM 已在运行，URL 已投递（download_id={download_id}）");
                    return;
                }
                Err(err) => {
                    eprintln!("rdm-gui: 投递到运行中的 RDM 失败: {err}");
                    std::process::exit(1);
                }
            }
        } else if args.silent {
            // 静默启动的第二次调用不应把已隐藏的窗口拉到前台。
            println!("RDM 已在运行（静默模式不激活窗口）");
            return;
        } else {
            match rdm_ipc::send(Request::Activate, Duration::from_secs(3)) {
                Ok(_) => {
                    println!("RDM 已在运行，已激活窗口");
                    return;
                }
                Err(err) => {
                    eprintln!("rdm-gui: 激活运行中的 RDM 失败: {err}");
                    std::process::exit(1);
                }
            }
        }
    }

    // ---- 2. 读取本地状态 --------------------------------------------------
    let settings = model::load_settings(&state_dir);
    // 静默启动：设置开启或命令行 --silent 强制；托盘不可用时会在窗口创建后回退显示。
    let start_silent = settings.effective_silent_start(args.silent);
    // AI 配置从 <state>/ai.json 读入（缺失/损坏 → 默认值，默认未启用）。
    let ai_config = rdm_ai::AiConfig::load(&state_dir);
    let loaded = model::load_downloads(&state_dir);
    if let Some(error) = &loaded.error {
        tracing::warn!(error = %error, "加载 downloads.json 失败，从空列表开始");
    }
    if loaded.recovered > 0 {
        tracing::info!(
            recovered = loaded.recovered,
            "重启恢复：{} 个进行中任务转为已暂停",
            loaded.recovered
        );
    }
    if loaded.skipped > 0 {
        tracing::warn!(
            skipped = loaded.skipped,
            "downloads.json 中 {} 条记录 URL 非法已跳过",
            loaded.skipped
        );
    }

    let mut initial: Vec<EnqueueItem> = Vec::new();
    if let Some(url) = cli_url.as_deref() {
        match model::validate_url(url) {
            Ok(url) => initial.push(EnqueueItem {
                download: settings.request_with_defaults(url),
                source: "cli".to_string(),
                source_page: None,
            }),
            Err(err) => {
                eprintln!("rdm-gui: 非法 URL: {err}");
                std::process::exit(2);
            }
        }
    }

    // ---- 3. tokio runtime + IPC server（尽早发布 ipc.json）----------------
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("rdm-gui: 创建 tokio runtime 失败: {err}");
            std::process::exit(1);
        }
    };
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<EngineMsg>();
    let engine = EngineRuntime::new(runtime.handle().clone(), ui_tx.clone());
    let token = ipc_server::generate_token();
    let (listener, info) = match runtime.block_on(ipc_server::bind(token.clone())) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("rdm-gui: IPC 绑定 127.0.0.1:0 失败: {err}");
            std::process::exit(1);
        }
    };
    let ipc_warning = match ipc_server::publish(&info) {
        Ok(path) => {
            tracing::info!(
                path = %path.display(),
                port = info.port,
                "IPC endpoint 已发布"
            );
            None
        }
        Err(err) => {
            // 发布失败不阻断 UI（浏览器集成不可用），但要在界面上明确提示用户，
            // 否则扩展只会显示"GUI 未运行"而用户无从排查。
            tracing::error!(error = %err, "写入 ipc.json 失败，浏览器集成将不可用");
            eprintln!("rdm-gui: 写入 ipc.json 失败: {err}");
            Some(format!(
                "浏览器集成不可用：无法写入 {}（{err}）。请改用安装目录下的 RDM（README §7），或检查该目录写权限。",
                rdm_ipc::info_path().display()
            ))
        }
    };
    runtime.spawn(ipc_server::serve(listener, token, ui_tx));
    engine.start_ticker();

    // ---- 4. spool 兜底队列（必须在 write_info 之后扫描）-------------------
    let drain = spool::drain_spool(&state_dir);
    if !drain.items.is_empty() || drain.rejected > 0 || drain.skipped > 0 {
        tracing::info!(
            restored = drain.items.len(),
            rejected = drain.rejected,
            skipped = drain.skipped,
            "spool 队列扫描完成"
        );
    }
    initial.extend(drain.items);

    let bootstrap = Bootstrap {
        settings,
        items: loaded.items,
        next_seq: loaded.next_seq,
        state_dir: state_dir.clone(),
        engine,
        ui_rx,
        initial,
        ai_config,
        ipc_warning,
    };
    let palette = Palette::for_theme(bootstrap.settings.theme);

    // ---- 5. GPUI ----------------------------------------------------------
    // 托盘事件通道必须先于窗口创建：RdmApp::new 会按设置安装托盘图标，
    // 截获的点击/退出事件经这个通道回到 GPUI 主线程（见 run 闭包里的 async loop）。
    let tray_rx = desktop::init_event_channel();

    Application::new()
        .with_assets(ui::assets::RdmAssets)
        .run(move |cx: &mut App| {
            if ui_mode == UiModeArg::Kit {
                gpui_component::init(cx);
                let settings = bootstrap.settings.clone();
                ui::showcase::open(cx, &settings);
                cx.on_window_closed(|cx| cx.quit()).detach();
                return;
            }
            if ui_mode == UiModeArg::New {
                // gpui-component 必须先 init：Root/主题/Input/keymap 都依赖其全局状态。
                gpui_component::init(cx);
                views::actions::install(cx);
                open_hub_window(cx, start_silent, bootstrap, tray_rx);
                return;
            }
            bind_keys(cx);
            let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("RDM — Rust Download Manager".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
                // 静默启动：窗口照常创建（下载/IPC 全部可用），但不显示、不抢焦点；
                // 用户点托盘图标后由 RdmApp::show_from_tray 显示。
                focus: !start_silent,
                show: !start_silent,
                ..Default::default()
            };
            let hotkey_enabled = bootstrap.settings.needs_tray();

            let window = match cx.open_window(options, move |window, cx| {
                let url_input =
                    cx.new(|cx| TextInput::new(cx, "输入或粘贴下载 URL（http/https）", palette));
                let dir_input =
                    cx.new(|cx| TextInput::new(cx, "例如 C:\\Users\\you\\Downloads", palette));
                let limit_input = cx.new(|cx| TextInput::new(cx, "例如 2 或 1.5", palette));
                let connections_input = cx.new(|cx| {
                    TextInput::new(cx, format!("1..={}", rdm_core::MAX_CONNECTIONS), palette)
                });
                let ai_base_url =
                    cx.new(|cx| TextInput::new(cx, "例如 https://api.deepseek.com/v1", palette));
                let ai_model = cx.new(|cx| TextInput::new(cx, "例如 deepseek-flash", palette));
                let ai_api_key = cx.new(|cx| TextInput::new(cx, "sk-…（仅本机）", palette));
                let ai_chat_input = cx.new(|cx| TextInput::new(cx, "追问 AI…", palette));

                let inputs = GuiInputs {
                    url: url_input.clone(),
                    dir: dir_input,
                    limit: limit_input,
                    connections: connections_input,
                    ai_base_url,
                    ai_model,
                    ai_api_key,
                    ai_chat: ai_chat_input.clone(),
                };
                let window_handle = window.window_handle();
                let hwnd = desktop::raw_hwnd(window);
                // GPUI 无 per-window icon API：把 exe 内嵌的 RDM 图标显式设到窗口，
                // 否则标题栏左上角/Alt-Tab 会显示系统占位图标或空白（2026-10-22 反馈）。
                if let Some(raw) = hwnd {
                    desktop::set_window_icon(raw);
                }
                let app = cx.new(|cx| RdmApp::new(cx, inputs, bootstrap, window_handle, hwnd));

                // 验收/截图钩子（不写用户文档）：`RDM_TEST_OPEN_SETTINGS=1` 时启动后直接
                // 打开设置弹窗；也可写成 download/classification/ai/appearance/system 定位到
                // 某个分类，便于逐页截图。与 `RDM_TEST_TRAY_WITHOUT_ICON` 同一约定：只认
                // 环境变量、不改变 CLI 参数契约，也不写用户设置（隔离 LOCALAPPDATA 下使用）。
                if let Some(tab) = test_open_settings_tab() {
                    app.update(cx, |app, cx| {
                        app.open_settings_for_test(window, cx, tab);
                    });
                }

                // 关闭到托盘由 `desktop` 安装在主窗口上的消息钩子拦截
                // （见 `desktop::install_window_hook`）：托盘可用且开关打开时，WM_CLOSE
                // 直接隐藏窗口、不进入 GPUI 的关闭流程；最小化不拦截，正常到任务栏。

                // 托盘事件 → GPUI 主线程：显示主窗口 / 真正退出。
                {
                    let mut rx = tray_rx;
                    let weak_app = app.downgrade();
                    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                        while let Some(event) = rx.recv().await {
                            match event {
                                desktop::TrayEvent::ShowMain => {
                                    let _ = weak_app.update(cx, |app, cx| app.show_from_tray(cx));
                                }
                                desktop::TrayEvent::PauseAll => {
                                    let _ =
                                        weak_app.update(cx, |app, cx| app.pause_all_from_tray(cx));
                                }
                                desktop::TrayEvent::ResumeAll => {
                                    let _ =
                                        weak_app.update(cx, |app, cx| app.resume_all_from_tray(cx));
                                }
                                desktop::TrayEvent::OpenDownloadFolder => {
                                    let _ = weak_app.update(cx, |app, cx| {
                                        app.open_download_folder_from_tray(cx)
                                    });
                                }
                                desktop::TrayEvent::QuickAdd => {
                                    let _ =
                                        weak_app.update(cx, |app, cx| app.quick_add_from_tray(cx));
                                }
                                desktop::TrayEvent::Quit => {
                                    // 先移除托盘图标再退，避免残留「幽灵图标」。
                                    desktop::shutdown_hotkey();
                                    desktop::shutdown_tray();
                                    let _ = cx.update(|cx| cx.quit());
                                    break;
                                }
                            }
                        }
                    })
                    .detach();
                }

                // 静默启动回退：没有托盘就没有恢复入口，直接显示窗口并提示。
                if start_silent && !desktop::is_tray_alive() {
                    if let Some(raw) = hwnd {
                        desktop::show_window(raw);
                    }
                    window.activate_window();
                    cx.activate(true);
                    app.update(cx, |app, cx| {
                        app.set_toast("托盘不可用，已回退为显示主窗口", true);
                        cx.notify();
                    });
                }

                // URL 输入框 Enter = 添加。Entity 引用通过弱句柄回传，避免循环强引用。
                let weak_app = app.downgrade();
                url_input.update(cx, |input, _cx| {
                    input.on_submit = Some(Rc::new(move |window, cx| {
                        weak_app
                            .update(cx, |app, cx| app.add_from_input(window, cx))
                            .ok();
                    }));
                });

                // AI 栏聊天输入框 Enter = 发送（上下文含解析摘要）。
                let weak_app_chat = app.downgrade();
                ai_chat_input.update(cx, |input, _cx| {
                    input.on_submit = Some(Rc::new(move |_window, cx| {
                        weak_app_chat
                            .update(cx, |app, cx| app.send_ai_chat(cx))
                            .ok();
                    }));
                });

                let focus = url_input.read(cx).focus_handle(cx);
                window.focus(&focus);
                app
            }) {
                Ok(window) => window,
                Err(err) => {
                    eprintln!("rdm-gui: 打开窗口失败: {err}");
                    desktop::shutdown_tray();
                    ipc_server::cleanup_info_file();
                    std::process::exit(1);
                }
            };
            let _ = window;
            // 全局热键（Ctrl+Shift+D 快速添加）：仅在后台上运行（关闭到托盘/静默启动）时注册。
            if hotkey_enabled {
                if let Err(err) = desktop::install_hotkey() {
                    tracing::warn!(error = %err, "全局热键不可用");
                }
            }
            cx.on_window_closed(|cx| {
                // 正常退出路径（未开启「关闭到托盘」）：先把托盘图标摘掉。
                desktop::shutdown_hotkey();
                desktop::shutdown_tray();
                cx.quit();
            })
            .detach();
            if !start_silent {
                cx.activate(true);
            }
        });

    // ---- 6. 清理 ----------------------------------------------------------
    desktop::shutdown_tray();
    ipc_server::cleanup_info_file();
    drop(runtime);
    tracing::info!("RDM GUI 退出");
}

/// 新版 UI（下载中枢）窗口：`gpui_component::Root` 作为窗口根视图，
/// 提供统一的 dialog/sheet/notification 浮层与焦点恢复。
fn open_hub_window(
    cx: &mut App,
    start_silent: bool,
    bootstrap: Bootstrap,
    tray_rx: tokio::sync::mpsc::UnboundedReceiver<desktop::TrayEvent>,
) {
    let bounds = Bounds::centered(None, size(px(1280.0), px(760.0)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("RDM".into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(960.0), px(600.0))),
        focus: !start_silent,
        show: !start_silent,
        ..Default::default()
    };
    let hotkey_enabled = bootstrap.settings.needs_tray();
    let window = match cx.open_window(options, move |window, cx| {
        let hwnd = desktop::raw_hwnd(window);
        // GPUI 无 per-window icon API：把 exe 内嵌的 RDM 图标显式设到窗口，
        // 否则标题栏左上角/Alt-Tab 会显示系统占位图标或空白（2026-10-22 反馈）。
        if let Some(raw) = hwnd {
            desktop::set_window_icon(raw);
        }
        let app = cx.new(|cx| views::app::RdmApp::new(cx, window, bootstrap, hwnd));

        // 验收钩子：RDM_TEST_OPEN_SETTINGS 打开设置页（新 UI 全屏设置）。
        if let Some(tab) = test_open_settings_tab() {
            app.update(cx, |app, cx| {
                app.open_settings_for_test(settings_section_for_tab(tab), cx)
            });
        }
        // 开发/截图钩子：RDM_TEST_UI_STATE=demo 注入示例任务（仅隔离状态目录使用）。
        if std::env::var("RDM_TEST_UI_STATE")
            .map(|value| value.eq_ignore_ascii_case("demo"))
            .unwrap_or(false)
        {
            app.update(cx, |app, cx| app.seed_demo_state(cx));
        }

        // 托盘事件 → 新 UI 主窗口（显示 / 退出）。
        {
            let mut rx = tray_rx;
            let weak = app.downgrade();
            cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                while let Some(event) = rx.recv().await {
                    match event {
                        desktop::TrayEvent::ShowMain => {
                            let _ = weak.update(cx, |app, cx| app.show_from_tray(cx));
                        }
                        desktop::TrayEvent::PauseAll => {
                            let _ = weak.update(cx, |app, cx| app.pause_all(cx));
                        }
                        desktop::TrayEvent::ResumeAll => {
                            let _ = weak.update(cx, |app, cx| app.resume_all(cx));
                        }
                        desktop::TrayEvent::OpenDownloadFolder => {
                            let _ = weak.update(cx, |app, cx| app.open_download_folder(cx));
                        }
                        desktop::TrayEvent::QuickAdd => {
                            let _ = weak.update(cx, |app, cx| app.quick_add_from_hotkey(cx));
                        }
                        desktop::TrayEvent::Quit => {
                            desktop::shutdown_hotkey();
                            desktop::shutdown_tray();
                            let _ = cx.update(|cx| cx.quit());
                            break;
                        }
                    }
                }
            })
            .detach();
        }

        // 静默启动回退：没有托盘就没有恢复入口，直接显示窗口并提示。
        if start_silent && !desktop::is_tray_alive() {
            if let Some(raw) = hwnd {
                desktop::show_window(raw);
            }
            window.activate_window();
            cx.activate(true);
            app.update(cx, |app, cx| {
                app.startup_notice("托盘不可用，已回退为显示主窗口", true, cx);
            });
        } else if !start_silent {
            app.update(cx, |app, cx| app.focus_omnibox(window, cx));
        }

        cx.new(|cx| gpui_component::Root::new(app, window, cx))
    }) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("rdm-gui: 打开窗口失败: {err}");
            desktop::shutdown_tray();
            ipc_server::cleanup_info_file();
            std::process::exit(1);
        }
    };
    let _ = window;
    // 全局热键（Ctrl+Shift+D 快速添加）：仅在后台上运行（关闭到托盘/静默启动）时注册，
    // 避免在没有托盘驻留需求时抢走浏览器等应用的 Ctrl+Shift+D。
    if hotkey_enabled {
        if let Err(err) = desktop::install_hotkey() {
            tracing::warn!(error = %err, "全局热键不可用");
        }
    }
    cx.on_window_closed(|cx| {
        desktop::shutdown_tray();
        cx.quit();
    })
    .detach();
    if !start_silent {
        cx.activate(true);
    }
}
fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("secondary-a", SelectAll, None),
        KeyBinding::new("secondary-v", Paste, None),
        KeyBinding::new("secondary-c", Copy, None),
        KeyBinding::new("secondary-x", Cut, None),
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
        KeyBinding::new("enter", Submit, None),
        KeyBinding::new("escape", CloseDialog, None),
    ]);
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("rdm_gui=info,rdm_core=info,info"))
    };
    // 无控制台构建（release / windows-subsystem feature）没有 stderr 可看：日志写文件，
    // 否则双击启动后出问题完全无迹可查。打不开日志文件时退回默认 stdout。
    if cfg!(any(feature = "windows-subsystem", not(debug_assertions))) {
        if let Some(file) = gui_log_file() {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_target(false)
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
            return;
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_target(false)
        .try_init();
}

/// GUI 日志文件：`%LOCALAPPDATA%\RDM\logs\gui.log`（追加写，>4 MiB 轮转到 `gui.log.1`）。
///
/// 只有无控制台构建使用；目录不可写时返回 `None` 回退 stdout。
fn gui_log_file() -> Option<std::fs::File> {
    let path = rdm_ipc::state_dir().join("logs").join("gui.log");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    if let Ok(metadata) = std::fs::metadata(&path) {
        if metadata.len() > 4 * 1024 * 1024 {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// 自动化验收/截图专用环境变量（不写用户文档、不改变 CLI 参数契约）：
/// `RDM_TEST_OPEN_SETTINGS=1` 启动后直接打开设置弹窗；值也可写成
/// `download|classification|ai|appearance|system` 直接定位到某个分类，便于逐页截图。
/// 与 `desktop` 的 `RDM_TEST_TRAY_WITHOUT_ICON` 保持一致：只认环境变量。
const TEST_OPEN_SETTINGS_ENV: &str = "RDM_TEST_OPEN_SETTINGS";

/// 解析 `RDM_TEST_OPEN_SETTINGS`：外层 `None` = 不打开弹窗；`Some(None)` = 打开并停在
/// 默认「下载」分类；`Some(Some(tab))` = 打开并定位到指定分类。
fn test_open_settings_tab() -> Option<Option<SettingsTab>> {
    let raw = std::env::var(TEST_OPEN_SETTINGS_ENV).ok()?;
    let key = raw.trim().to_ascii_lowercase();
    if key.is_empty() || key == "0" || key == "false" {
        return None;
    }
    let tab = match key.as_str() {
        "classification" => Some(SettingsTab::Classification),
        "ai" => Some(SettingsTab::Ai),
        "appearance" => Some(SettingsTab::Appearance),
        "system" => Some(SettingsTab::System),
        // "1"/"true"/其它真值：默认「下载」分类。
        _ => None,
    };
    Some(tab)
}

/// 把旧版设置弹窗的 tab 映射到新设置页分类（仅用于 `RDM_TEST_OPEN_SETTINGS` 截图钩子）。
fn settings_section_for_tab(
    tab: Option<SettingsTab>,
) -> Option<crate::views::settings::SettingsSection> {
    use crate::views::settings::SettingsSection;
    match tab {
        Some(SettingsTab::Download) => Some(SettingsSection::General),
        Some(SettingsTab::Classification) => Some(SettingsSection::Storage),
        Some(SettingsTab::Ai) => Some(SettingsSection::Ai),
        Some(SettingsTab::Appearance) => Some(SettingsSection::Appearance),
        // 「系统」类（自启动/托盘/静默）在新 IA 里并入了「通用」。
        Some(SettingsTab::System) => Some(SettingsSection::General),
        None => None,
    }
}
