//! RDM 桌面 GUI 入口。
//!
//! 启动顺序（与 `docs/extension-protocol.md` 和 Firefox 宿主的依赖约定一致）：
//! 1. 单实例检测：若 `ipc.json` 指向的 GUI 存活，则经 IPC 投递命令行 URL（或 Activate）后退出；
//! 2. 建 tokio runtime → `bind 127.0.0.1:0` → `rdm_ipc::write_info`（尽早发布，宿主轮询）；
//! 3. 启动 IPC accept 循环与引擎 250ms ticker；
//! 4. 扫描 `%LOCALAPPDATA%\RDM\queue\*.json` 兜底队列（宿主在 GUI 未启动时的落盘任务）；
//! 5. 打开 GPUI 窗口（1180x760，暗色/浅色主题）；
//! 6. 窗口关闭后删除 `ipc.json`、退出 tokio runtime。
//!
//! `rdm-gui.exe <URL>` 支持启动即添加任务；GUI 已在运行时则走单实例投递。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod engine;
mod ipc_server;
mod model;
mod open_path;
mod spool;
mod text_input;
mod theme;

use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Application, Bounds, Focusable, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};
use rdm_ipc::{EnqueueItem, Request};
use tokio::sync::mpsc;

use crate::app::{Bootstrap, CloseDialog, RdmApp};
use crate::engine::{EngineMsg, EngineRuntime};
use crate::theme::Palette;
use crate::text_input::{
    Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft, SelectRight,
    Submit, TextInput,
};

const WINDOW_WIDTH: f32 = 1180.0;
const WINDOW_HEIGHT: f32 = 760.0;
const MIN_WINDOW_WIDTH: f32 = 900.0;
const MIN_WINDOW_HEIGHT: f32 = 560.0;

fn main() {
    init_logging();
    let state_dir = rdm_ipc::state_dir();
    let cli_url = std::env::args().nth(1).filter(|arg| !arg.starts_with('-'));

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
        tracing::warn!(skipped = loaded.skipped, "downloads.json 中 {} 条记录 URL 非法已跳过", loaded.skipped);
    }

    let mut initial: Vec<EnqueueItem> = Vec::new();
    if let Some(url) = cli_url.as_deref() {
        match model::validate_url(url) {
            Ok(url) => initial.push(EnqueueItem {
                download: settings.build_request(url),
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
    match ipc_server::publish(&info) {
        Ok(path) => tracing::info!(
            path = %path.display(),
            port = info.port,
            "IPC endpoint 已发布"
        ),
        Err(err) => {
            // 发布失败不阻断 UI（浏览器集成不可用），但要把错误记下来。
            tracing::error!(error = %err, "写入 ipc.json 失败，Firefox 集成将不可用");
            eprintln!("rdm-gui: 写入 ipc.json 失败: {err}");
        }
    }
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
    };
    let palette = Palette::for_theme(bootstrap.settings.theme);

    // ---- 5. GPUI ----------------------------------------------------------
    Application::new().run(move |cx: &mut App| {
        bind_keys(cx);
        let bounds = Bounds::centered(
            None,
            size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
            cx,
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("RDM — Rust Download Manager".into()),
                ..Default::default()
            }),
            window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
            focus: true,
            ..Default::default()
        };

        let window = match cx.open_window(options, move |window, cx| {
            let url_input =
                cx.new(|cx| TextInput::new(cx, "输入或粘贴下载 URL（http/https）", palette));
            let dir_input = cx.new(|cx| {
                TextInput::new(cx, "例如 C:\\Users\\you\\Downloads（留空使用默认）", palette)
            });
            let limit_input =
                cx.new(|cx| TextInput::new(cx, "例如 2 或 1.5，留空不限速", palette));

            let app = cx.new(|cx| {
                RdmApp::new(
                    cx,
                    url_input.clone(),
                    dir_input,
                    limit_input,
                    bootstrap,
                )
            });

            // URL 输入框 Enter = 添加。Entity 引用通过弱句柄回传，避免循环强引用。
            let weak_app = app.downgrade();
            url_input.update(cx, |input, _cx| {
                input.on_submit = Some(Rc::new(move |window, cx| {
                    weak_app
                        .update(cx, |app, cx| app.add_from_input(window, cx))
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
                ipc_server::cleanup_info_file();
                std::process::exit(1);
            }
        };
        let _ = window;

        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });

    // ---- 6. 清理 ----------------------------------------------------------
    ipc_server::cleanup_info_file();
    drop(runtime);
    tracing::info!("RDM GUI 退出");
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
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("rdm_gui=info,rdm_core=info,info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
