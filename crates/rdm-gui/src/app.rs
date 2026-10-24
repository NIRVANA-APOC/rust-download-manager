//! RDM 主视图：顶栏（URL + 添加/设置）、左侧过滤栏、主下载列表（uniform_list）、
//! 设置弹窗、速度曲线、状态栏，以及 IPC 请求在 GUI 主线程上的处理。
//!
//! 数据流：引擎事件 / IPC 请求都经 `engine::EngineMsg` 进入 `pump_loop`（GPUI 主线程
//! 的 foreground task），合并成批后调用 [`RdmApp::apply_batch`] 更新状态并 `cx.notify()`。
//! 这样 tokio 侧永远不直接触碰 `Entity`，GPUI 侧也不会误用 tokio 定时器。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    canvas, div, fill, point, px, size, uniform_list, AnyElement, App, Bounds, ClickEvent, Context,
    ElementId, Entity, Focusable, Font, FontFallbacks, Hsla, IntoElement, MouseButton, Render,
    Rgba, ScrollDelta, ScrollHandle, ScrollWheelEvent, UniformListScrollHandle, WeakEntity, Window,
};
use rdm_ai::AiConfig;
use rdm_core::{DownloadHandle, DownloadRequest};
use rdm_ipc::{Envelope, Request, Response};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::desktop;
use crate::engine::{EngineMsg, EngineRuntime, RoutingOptions};
use crate::ipc_server;
use crate::model::{
    self, active_count, filter_count, filtered_indices, format_bytes, format_limit, format_speed,
    global_speed_bps, next_download_id, validate_url, AiPanelState, AiParseStatus, AiRole,
    DownloadItem, Filter, ItemState, Settings, StopRequest, Theme,
};
use crate::open_path;
use crate::text_input::TextInput;
use crate::theme::Palette;

gpui::actions!(rdm_app, [CloseDialog]);

/// 主列表行高（uniform_list 要求等高）。
const ROW_HEIGHT: f32 = 88.0;
/// 全局速度曲线采样数。
const GLOBAL_HISTORY_LEN: usize = 60;
/// 右侧 AI 栏固定宽度（窄窗口靠自动收起过滤栏给主列表让位）。
const AI_PANEL_WIDTH: f32 = 360.0;
/// 过滤栏宽度（`render_sidebar` 里使用）。
const SIDEBAR_WIDTH: f32 = 210.0;
/// 过滤栏 + 主列表仍能放下完整行布局所需的最小宽度。
const SIDEBAR_MIN_ROOM: f32 = 740.0;
/// 设置弹窗右侧内容区固定高度：5 个分类切换时弹窗高度不跳动（无滚动条）。
/// 实测最高的是 AI 分类（启用开关 + 3 个输入框 + 3 条提示，约 291 逻辑 px），
/// 取 320 留约 29px 余量：5 个分类都完整显示、不裁剪，切换分类弹窗高度不变。
const SETTINGS_CONTENT_HEIGHT: f32 = 320.0;

/// 一次性启动数据（由 `main` 组装；GUI 不直接做 IO 初始化）。
pub struct Bootstrap {
    pub settings: Settings,
    pub items: Vec<DownloadItem>,
    pub next_seq: u64,
    pub state_dir: PathBuf,
    pub engine: EngineRuntime,
    pub ui_rx: UnboundedReceiver<EngineMsg>,
    /// CLI URL / spool 队列里的初始任务。
    pub initial: Vec<rdm_ipc::EnqueueItem>,
    /// `state_dir/ai.json` 读入的 AI 配置（设置弹窗与 AI 解析共用）。
    pub ai_config: AiConfig,
    /// 浏览器 IPC（`%LOCALAPPDATA%\RDM\ipc.json`）写入失败时的警告文案。
    /// `main.rs` 在发布 endpoint 失败时传入；`None` 表示浏览器集成正常。
    pub ipc_warning: Option<String>,
}

/// `main.rs` 创建的输入框集合：一次注入，避免 `RdmApp::new` 堆一长串参数。
pub struct GuiInputs {
    pub url: Entity<TextInput>,
    pub dir: Entity<TextInput>,
    pub limit: Entity<TextInput>,
    /// 设置：默认连接数（1..=MAX_CONNECTIONS）。
    pub connections: Entity<TextInput>,
    /// 设置：AI Base URL。
    pub ai_base_url: Entity<TextInput>,
    /// 设置：AI 模型名。
    pub ai_model: Entity<TextInput>,
    /// 设置：AI API Key（掩码显示，仅写本机 ai.json）。
    pub ai_api_key: Entity<TextInput>,
    /// AI 栏底部聊天输入框。
    pub ai_chat: Entity<TextInput>,
}

/// 删除确认弹窗状态（§4.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteConfirm {
    /// 待删除任务在 `items` 里的索引。
    pub index: usize,
    /// 是否同时删除磁盘文件（已完成文件 / 未完成的 `.part`+`.part.json`）。
    pub delete_file: bool,
}

impl DeleteConfirm {
    /// 默认勾选「同时删除文件」的条件：已完成且知道落盘路径。
    pub fn default_delete_file(item: &DownloadItem) -> bool {
        item.state == ItemState::Completed && item.path.is_some()
    }
}

struct Toast {
    text: String,
    is_error: bool,
    created: Instant,
}

const TOAST_TTL: Duration = Duration::from_secs(5);

/// 设置弹窗左侧分类导航（顺序固定，默认选中「下载」）。
///
/// 每个分类只渲染自己的面板，但共用同一份 `draft_*` 草稿状态：切换分类不会丢弃
/// 未保存的修改，只有点「保存」才写 `settings.json`；点「取消」/关闭会回滚主题预览。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    /// 下载：默认目录 / 连接数 / 限速 / 动态线程 / 自动开始。
    #[default]
    Download,
    /// 分类与校验：按后缀分类 / AI 分类 / SHA-256 校验。
    Classification,
    /// AI：启用开关 + Base URL / 模型 / API Key 与使用提示。
    Ai,
    /// 外观：6 个主题（即时预览）。
    Appearance,
    /// 系统：开机自启动 / 最小化到托盘（关闭窗口时隐藏） / 静默启动。
    System,
}

impl SettingsTab {
    /// 左侧导航的展示顺序；截图/验收按这个顺序逐页检查。
    pub const ALL: [SettingsTab; 5] = [
        SettingsTab::Download,
        SettingsTab::Classification,
        SettingsTab::Ai,
        SettingsTab::Appearance,
        SettingsTab::System,
    ];

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Download => "下载",
            Self::Classification => "分类与校验",
            Self::Ai => "AI",
            Self::Appearance => "外观",
            Self::System => "系统",
        }
    }

    /// 左侧导航按钮的稳定元素 id（自动化/截图按它定位分类）。
    pub fn element_id(self) -> &'static str {
        match self {
            Self::Download => "settings-tab-download",
            Self::Classification => "settings-tab-classification",
            Self::Ai => "settings-tab-ai",
            Self::Appearance => "settings-tab-appearance",
            Self::System => "settings-tab-system",
        }
    }
}

pub struct RdmApp {
    items: Vec<DownloadItem>,
    settings: Settings,
    filter: Filter,
    palette: Palette,
    font: Font,

    url_input: Entity<TextInput>,
    dir_input: Entity<TextInput>,
    limit_input: Entity<TextInput>,
    connections_input: Entity<TextInput>,
    ai_config_inputs: (Entity<TextInput>, Entity<TextInput>, Entity<TextInput>),
    ai_chat_input: Entity<TextInput>,
    scroll_handle: UniformListScrollHandle,

    show_settings: bool,
    /// 设置弹窗当前分类（每次打开重置为「下载」；切换不影响 `draft_*` 草稿）。
    settings_tab: SettingsTab,
    /// 主窗口的 GPUI 句柄：托盘/IPC 恢复窗口时用它拿 `&mut Window`。
    window_handle: gpui::AnyWindowHandle,
    /// 主窗口的 Win32 `HWND`（Windows 之外为 `None`）：`ShowWindow` 显隐用。
    hwnd: Option<isize>,
    draft_verify_sha256: bool,
    draft_theme: Theme,
    draft_auto_start: bool,
    draft_dynamic: bool,
    draft_save_by_extension: bool,
    /// 系统集成：开机自启动（登录 Windows 时运行）。
    draft_launch_at_startup: bool,
    /// 系统集成：关闭窗口时隐藏到托盘（最小化仍留在任务栏）。
    draft_minimize_to_tray: bool,
    /// 系统集成：启动时不显示主窗口。
    draft_silent_start: bool,
    draft_ai: AiConfig,
    /// 当前生效的 AI 配置（保存设置后更新；解析/聊天直接使用）。
    ai_config: AiConfig,

    /// 右侧 AI 解析 / 聊天面板状态。
    ai: AiPanelState,
    /// 聊天历史滚动的状态句柄（滚轮/自动滚动共用）。
    ai_chat_scroll: ScrollHandle,
    /// 是否自动滚动到最新消息：用户向上滚动手动回看历史后置 false，
    /// 发送新问题 / 向下滚动时恢复 true；为 false 时新 delta 不再强拉底部。
    ai_autoscroll: bool,
    /// 当前聊天请求代号（用于丢弃迟到的回复）。
    ai_chat_request: Option<u64>,
    /// 解析/聊天请求代号自增源（解析与聊天共用一个序列）。
    next_ai_request: u64,
    /// 删除确认弹窗。
    delete_confirm: Option<DeleteConfirm>,
    /// 当前窗口宽度下主列表是否使用紧凑行布局（由 `render` 按视口计算）。
    compact_rows: bool,

    engine: EngineRuntime,
    handles: HashMap<String, (u64, DownloadHandle)>,
    state_dir: PathBuf,
    /// 浏览器集成不可用的警告文案（顶栏下方渲染成警告条；`None` 不显示）。
    ipc_warning: Option<String>,
    next_seq: u64,
    speed_history: std::collections::VecDeque<f64>,

    dirty_persist: bool,
    last_save: Instant,
    toast: Option<Toast>,
}

impl RdmApp {
    pub fn new(
        cx: &mut Context<Self>,
        inputs: GuiInputs,
        bootstrap: Bootstrap,
        window_handle: gpui::AnyWindowHandle,
        hwnd: Option<isize>,
    ) -> Self {
        let Bootstrap {
            settings,
            items,
            next_seq,
            state_dir,
            engine,
            ui_rx,
            initial,
            ai_config,
            ipc_warning,
        } = bootstrap;
        let palette = Palette::for_theme(settings.theme);
        let font = ui_font();
        let GuiInputs {
            url: url_input,
            dir: dir_input,
            limit: limit_input,
            connections: connections_input,
            ai_base_url,
            ai_model,
            ai_api_key,
            ai_chat: ai_chat_input,
        } = inputs;

        // 设置弹窗里的输入框先填上当前设置（弹窗打开时再刷新一次）。
        dir_input.update(cx, |input, cx| {
            input.set_text(
                settings
                    .default_dir
                    .as_ref()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default(),
                cx,
            );
        });
        limit_input.update(cx, |input, cx| {
            input.set_text(settings.speed_limit_mb_text(), cx)
        });
        connections_input.update(cx, |input, cx| {
            input.set_text(settings.default_connections.to_string(), cx)
        });
        ai_base_url.update(cx, |input, cx| {
            input.set_text(ai_config.base_url.clone(), cx)
        });
        ai_model.update(cx, |input, cx| input.set_text(ai_config.model.clone(), cx));
        ai_api_key.update(cx, |input, cx| {
            input.set_masked(true);
            input.set_text(ai_config.api_key.clone(), cx);
        });

        let mut app = Self {
            items,
            draft_verify_sha256: settings.verify_sha256,
            draft_theme: settings.theme,
            draft_auto_start: settings.auto_start,
            draft_dynamic: settings.dynamic_connections,
            draft_save_by_extension: settings.save_by_extension,
            draft_launch_at_startup: settings.launch_at_startup,
            draft_minimize_to_tray: settings.minimize_to_tray,
            draft_silent_start: settings.silent_start,
            draft_ai: ai_config.clone(),
            ai_config,
            settings,
            filter: Filter::All,
            palette,
            font,
            url_input,
            dir_input,
            limit_input,
            connections_input,
            ai_config_inputs: (ai_base_url, ai_model, ai_api_key),
            ai_chat_input,
            scroll_handle: UniformListScrollHandle::new(),
            show_settings: false,
            settings_tab: SettingsTab::default(),
            window_handle,
            hwnd,
            ai: AiPanelState::default(),
            ai_chat_scroll: ScrollHandle::new(),
            ai_autoscroll: true,
            ai_chat_request: None,
            next_ai_request: 0,
            delete_confirm: None,
            compact_rows: false,
            engine,
            handles: HashMap::new(),
            state_dir,
            ipc_warning,
            next_seq,
            speed_history: std::collections::VecDeque::with_capacity(GLOBAL_HISTORY_LEN),
            dirty_persist: false,
            last_save: Instant::now(),
            toast: None,
        };

        // AI 面板可能带着历史消息打开（解析新页面不清空聊天）：默认停在最新消息。
        // `scroll_to_bottom` 只是置标记，等面板第一次 prepaint 时才真正生效。
        app.ai_chat_scroll.scroll_to_bottom();

        // 启动时恢复：CLI URL / Firefox 兜底队列。先入队，泵启动后事件自然会被消费。
        for item in initial {
            match model::prepare_request(&app.settings, item.download) {
                Ok(request) => {
                    let source = if item.source.is_empty() {
                        "cli".to_string()
                    } else {
                        item.source
                    };
                    app.push_download(request, source);
                }
                Err(err) => tracing::warn!(error = %err, "启动任务 URL 非法，已跳过"),
            }
        }
        app.persist_now();

        // 系统集成：按 settings.json 同步注册表自启动项 / 托盘图标 / 关闭到托盘钩子。
        // 失败只弹 toast（不阻断启动）；静默启动时窗口由 main.rs 决定显隐。
        if let Some(warning) = app.sync_system_integration() {
            app.set_toast(warning, true);
        }

        // GUI 主线程事件泵：tokio → GPUI。`rx` 是 tokio 的 UnboundedReceiver，
        // `recv().await` 的 waker 由 GPUI foreground dispatcher 派发，不碰 Entity。
        cx.spawn(async move |this, cx| {
            pump_loop(this, cx, ui_rx).await;
        })
        .detach();

        app
    }

    /// 把 `Settings` 里的桌面集成开关落到位（启动与保存设置后调用）：
    /// - 托盘图标：`needs_tray()` 时安装，否则移除；
    /// - 关闭到托盘窗口钩子：开启且托盘可用时安装（幂等；只拦截关闭，最小化正常到任务栏）；
    /// - 开机自启动：写/删 `HKCU\...\Run`。
    ///
    /// 返回需要展示给用户的错误（不 panic、不阻断其它设置）。
    fn sync_system_integration(&mut self) -> Option<String> {
        let mut warning: Option<String> = None;

        desktop::set_minimize_to_tray(self.settings.minimize_to_tray);

        if self.settings.needs_tray() {
            if !desktop::is_tray_alive() {
                match desktop::install_tray(&desktop::tray_tooltip()) {
                    Ok(()) => {
                        if self.settings.minimize_to_tray {
                            if let Some(hwnd) = self.hwnd {
                                if let Err(err) = desktop::install_window_hook(hwnd) {
                                    push_warning(
                                        &mut warning,
                                        format!("安装托盘窗口钩子失败: {err}"),
                                    );
                                }
                            }
                        }
                    }
                    Err(err) => {
                        push_warning(
                            &mut warning,
                            format!("托盘图标不可用（{err}），窗口仍可正常使用"),
                        );
                    }
                }
            } else if self.settings.minimize_to_tray {
                if let Some(hwnd) = self.hwnd {
                    if let Err(err) = desktop::install_window_hook(hwnd) {
                        push_warning(&mut warning, format!("安装托盘窗口钩子失败: {err}"));
                    }
                }
            }
        } else if desktop::is_tray_alive() {
            desktop::shutdown_tray();
        }

        if let Err(err) = desktop::autostart::sync(self.settings.launch_at_startup) {
            tracing::warn!(error = %err, "开机自启动设置未生效");
            push_warning(&mut warning, format!("开机自启动设置未生效: {err}"));
        } else {
            tracing::info!(
                enabled = self.settings.launch_at_startup,
                registry_value_present = desktop::autostart::is_enabled(),
                "开机自启动状态已同步"
            );
        }

        warning
    }

    /// 托盘「全部暂停」：与主界面一致，等引擎确认 Paused。
    pub fn pause_all_from_tray(&mut self, cx: &mut Context<Self>) {
        let mut count = 0usize;
        for index in 0..self.items.len() {
            if self.items[index].state == ItemState::Downloading {
                self.pause_item(index);
                count += 1;
            }
        }
        if count > 0 {
            self.set_toast(format!("已暂停 {count} 个下载"), false);
        }
        cx.notify();
    }

    /// 托盘「全部继续」。
    pub fn resume_all_from_tray(&mut self, cx: &mut Context<Self>) {
        let mut count = 0usize;
        for index in 0..self.items.len() {
            if self.items[index].state == ItemState::Paused {
                self.resume_item(index);
                count += 1;
            }
        }
        if count > 0 {
            self.set_toast(format!("已继续 {count} 个下载"), false);
        }
        cx.notify();
    }

    /// 全局热键 `Ctrl+Shift+D`：恢复窗口并提示粘贴链接（旧版没有添加弹层）。
    pub fn quick_add_from_tray(&mut self, cx: &mut Context<Self>) {
        self.show_from_tray(cx);
        self.set_toast("已通过全局热键唤醒：粘贴链接后按 Enter 开始下载", false);
        cx.notify();
    }

    /// 托盘「打开下载目录」。
    pub fn open_download_folder_from_tray(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.settings.default_dir.clone() else {
            self.set_toast("尚未设置默认保存目录", true);
            return;
        };
        if let Err(err) = open_path::open_directory(&dir) {
            self.set_toast(format!("打开下载目录失败: {err}"), true);
        }
        cx.notify();
    }

    /// 从托盘/第二次启动/IPC `Activate` 恢复主窗口（静默启动或关闭到托盘后）。
    pub fn show_from_tray(&mut self, cx: &mut Context<Self>) {
        self.restore_window(cx);
    }

    /// 显示 + 激活主窗口。静默启动时窗口的 placement 由 GPUI 延迟应用，
    /// 因此先显示再在下一帧 defer 中重放一次显示/激活，避免停在默认位置。
    fn restore_window(&mut self, cx: &mut Context<Self>) {
        let hwnd = self.hwnd;
        let _ = self.window_handle.update(cx, move |_, window, app| {
            if let Some(raw) = hwnd {
                desktop::show_window(raw);
            }
            window.defer(app, move |window, _| {
                if let Some(raw) = hwnd {
                    desktop::show_window(raw);
                }
                window.activate_window();
            });
        });
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 交互
    // -----------------------------------------------------------------------

    pub fn add_from_input(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.url_input.read(cx).text().to_owned();
        let url = match validate_url(&raw) {
            Ok(url) => url,
            Err(err) => {
                self.set_toast(err, true);
                cx.notify();
                return;
            }
        };
        let request = self.settings.request_with_defaults(url);
        self.url_input.update(cx, |input, cx| input.clear(cx));
        match model::prepare_request(&self.settings, request) {
            Ok(request) => {
                let id = self.push_download(request, "gui".to_string());
                tracing::info!(download_id = %id, "GUI 添加下载");
            }
            Err(err) => self.set_toast(err, true),
        }
        cx.notify();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = true;
        // 每次打开都从「下载」分类开始（用户最常改的就是下载参数）。
        self.settings_tab = SettingsTab::Download;
        self.draft_verify_sha256 = self.settings.verify_sha256;
        self.draft_theme = self.settings.theme;
        self.draft_auto_start = self.settings.auto_start;
        self.draft_dynamic = self.settings.dynamic_connections;
        self.draft_save_by_extension = self.settings.save_by_extension;
        self.draft_launch_at_startup = self.settings.launch_at_startup;
        self.draft_minimize_to_tray = self.settings.minimize_to_tray;
        self.draft_silent_start = self.settings.silent_start;
        self.draft_ai = self.ai_config.clone();

        let dir_text = self
            .settings
            .default_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let limit_text = self.settings.speed_limit_mb_text();
        let connections_text = self.settings.default_connections.to_string();
        let (base_url, model, api_key) = (
            self.ai_config.base_url.clone(),
            self.ai_config.model.clone(),
            self.ai_config.api_key.clone(),
        );
        self.dir_input
            .update(cx, |input, cx| input.set_text(dir_text, cx));
        self.limit_input
            .update(cx, |input, cx| input.set_text(limit_text, cx));
        self.connections_input
            .update(cx, |input, cx| input.set_text(connections_text, cx));
        self.ai_config_inputs.0.update(cx, |input, cx| {
            input.set_text(base_url, cx);
        });
        self.ai_config_inputs.1.update(cx, |input, cx| {
            input.set_text(model, cx);
        });
        self.ai_config_inputs.2.update(cx, |input, cx| {
            input.set_text(api_key, cx);
        });

        let handle = self.dir_input.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    /// 测试/截图专用入口：打开设置弹窗，可再定位到指定分类（`None` = 默认「下载」）。
    ///
    /// 正常用户路径是顶栏「设置」按钮 → [`RdmApp::open_settings`]；这里只由 `main.rs`
    /// 的 `RDM_TEST_OPEN_SETTINGS` 环境变量在隔离状态目录下调用，用于逐分类截图/验收，
    /// 不改变 CLI 参数契约，也不会写用户设置。
    pub(crate) fn open_settings_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        tab: Option<SettingsTab>,
    ) {
        self.open_settings(window, cx);
        if let Some(tab) = tab {
            self.settings_tab = tab;
            cx.notify();
        }
    }

    fn cancel_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = false;
        // 主题是即时预览的：取消时回退到已保存主题。
        self.apply_theme(cx);
        let handle = self.url_input.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir_text = self.dir_input.read(cx).text().trim().to_string();
        let limit_text = self.limit_input.read(cx).text().to_owned();
        let connections_text = self.connections_input.read(cx).text().to_owned();
        let connections = match model::parse_connections(&connections_text) {
            Ok(value) => value,
            Err(err) => {
                self.set_toast(err, true);
                cx.notify();
                return;
            }
        };
        let speed_limit = match model::parse_speed_limit_mb(&limit_text) {
            Ok(value) => value,
            Err(err) => {
                self.set_toast(err, true);
                cx.notify();
                return;
            }
        };

        let mut settings = self.settings.clone();
        settings.default_connections = connections;
        settings.default_dir = if dir_text.is_empty() {
            None
        } else {
            Some(PathBuf::from(dir_text))
        };
        settings.speed_limit_bps = speed_limit;
        settings.verify_sha256 = self.draft_verify_sha256;
        settings.theme = self.draft_theme;
        settings.auto_start = self.draft_auto_start;
        settings.dynamic_connections = self.draft_dynamic;
        settings.save_by_extension = self.draft_save_by_extension;
        settings.launch_at_startup = self.draft_launch_at_startup;
        settings.minimize_to_tray = self.draft_minimize_to_tray;
        settings.silent_start = self.draft_silent_start;
        self.settings = settings.normalized();

        // AI 配置：文本输入框 → draft，再原子写 <state>/ai.json。
        let mut ai = self.draft_ai.clone();
        ai.base_url = self.ai_config_inputs.0.read(cx).text().trim().to_string();
        ai.model = self.ai_config_inputs.1.read(cx).text().trim().to_string();
        ai.api_key = self.ai_config_inputs.2.read(cx).text().trim().to_string();
        if ai.base_url.is_empty() {
            ai.base_url = AiConfig::default().base_url;
        }
        if ai.model.is_empty() {
            ai.model = AiConfig::default().model;
        }
        // 没有可用的 key（含环境变量）时不可能配置成功，保持关闭避免误导。
        if ai.resolved_api_key().is_empty() {
            ai.enabled = false;
        }
        self.draft_ai = ai.clone();

        let mut save_error: Option<String> = None;
        if let Err(err) = model::save_settings(&self.state_dir, &self.settings) {
            tracing::warn!(error = %err, "保存 settings.json 失败");
            save_error = Some(format!("保存设置失败: {err}"));
        }
        if let Err(err) = ai.save(&self.state_dir) {
            tracing::warn!(error = %err, "保存 ai.json 失败");
            save_error = Some(format!("保存 AI 配置失败: {err}"));
        }
        self.ai_config = ai;
        // 系统集成开关（托盘 / 自启动）立即生效；静默启动在下次启动生效。
        if let Some(warning) = self.sync_system_integration() {
            save_error = Some(match save_error {
                Some(err) => format!("{err}；{warning}"),
                None => warning,
            });
        }
        self.apply_theme(cx);
        self.show_settings = false;
        match save_error {
            Some(err) => self.set_toast(err, true),
            None => self.set_toast("设置已保存", false),
        }
        let handle = self.url_input.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn apply_theme(&mut self, cx: &mut Context<Self>) {
        let theme = self.settings.theme;
        self.apply_palette(theme, cx);
    }

    /// 设置弹窗内点主题按钮时的即时预览：只切换当前界面配色，不写 settings.json。
    /// 取消/关闭弹窗会回退到已保存主题；保存时把 `draft_theme` 落盘并保持。
    fn preview_theme(&mut self, cx: &mut Context<Self>) {
        let theme = self.draft_theme;
        self.apply_palette(theme, cx);
    }

    fn apply_palette(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.palette = Palette::for_theme(theme);
        let palette = self.palette;
        self.url_input
            .update(cx, |input, _| input.set_palette(palette));
        self.dir_input
            .update(cx, |input, _| input.set_palette(palette));
        self.limit_input
            .update(cx, |input, _| input.set_palette(palette));
        self.connections_input
            .update(cx, |input, _| input.set_palette(palette));
        self.ai_config_inputs.0.update(cx, |input, _| {
            input.set_palette(palette);
        });
        self.ai_config_inputs.1.update(cx, |input, _| {
            input.set_palette(palette);
        });
        self.ai_config_inputs.2.update(cx, |input, _| {
            input.set_palette(palette);
        });
        self.ai_chat_input
            .update(cx, |input, _| input.set_palette(palette));
    }

    fn close_dialog(&mut self, _: &CloseDialog, window: &mut Window, cx: &mut Context<Self>) {
        if self.delete_confirm.take().is_some() {
            cx.notify();
            return;
        }
        if self.show_settings {
            self.cancel_settings(window, cx);
        }
    }

    // -----------------------------------------------------------------------
    // 下载操作（按 id，供行内按钮 / IPC 共用）
    // -----------------------------------------------------------------------

    fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.id == id)
    }

    fn pause_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.pause_item(index);
            cx.notify();
        }
    }

    fn resume_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.resume_item(index);
            cx.notify();
        }
    }

    fn cancel_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.cancel_item(index);
            cx.notify();
        }
    }

    /// 顶栏「AI 解析」：取 URL 输入框内容 → 打开/刷新右侧 AI 栏并发起解析。
    pub fn ai_parse_from_input(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.url_input.read(cx).text().to_owned();
        let url = match validate_url(&raw) {
            Ok(url) => url,
            Err(err) => {
                self.show_ai_panel();
                self.set_toast(err, true);
                cx.notify();
                return;
            }
        };
        let request = rdm_ai::ParseRequest {
            url,
            title: None,
            referer: None,
            user_agent: None,
            cookie: None,
            hint: None,
        };
        // 手动输入 URL 没有页面快照；扩展/native 的解析才可能带快照。
        self.begin_ai_parse(request, None, cx);
    }

    /// 打开 AI 栏（不清空已有结果）。
    fn show_ai_panel(&mut self) {
        self.ai.visible = true;
        self.show_settings = false;
        self.delete_confirm = None;
    }

    /// 发起一次解析：面板立即进入「解析中」，结果经 `EngineMsg::AiParseFinished/Failed` 回来。
    ///
    /// `request.cookie` 只随本次内存请求传递，绝不持久化或写日志（见 §5.1）。
    /// P4b-a：解析开始时把本次请求的 Cookie/Referer/UA 记到面板（仅内存），
    /// 供「下载选中」按 host 交接给 `DownloadRequest`；下次解析替换、失败清除。
    /// P4b-b2：`page_snapshot` 是扩展/native 传来的页面链接快照（仅 resolver-first
    /// 路径使用，legacy 忽略；不落盘/不进日志值）。
    /// 返回本次请求代号（IPC 回包使用）。
    fn begin_ai_parse(
        &mut self,
        request: rdm_ai::ParseRequest,
        page_snapshot: Option<rdm_resolve::PageSnapshot>,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.next_ai_request += 1;
        let request_id = self.next_ai_request;
        self.show_ai_panel();
        self.ai.begin_parse(&request.url, request_id);
        self.ai.set_request_credential(&request);
        // 解析新页面时旧的聊天回复已无法匹配 request_id（下面会清空），
        // 同步收尾流式状态，避免「正在输出」占位/发送按钮一直卡住。
        self.ai.abort_chat_stream();
        self.ai_chat_request = None;
        if !self.ai_config.is_configured() {
            self.ai.push_message(
                AiRole::System,
                "未启用/未配置 AI（设置 → AI 配置）；本次解析使用本地启发式结果。",
            );
        }
        tracing::info!(
            request_id,
            page_snapshot_links = page_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.links.len()),
            "发起 AI 解析"
        );
        self.engine.start_ai_parse(
            request_id,
            request,
            self.ai_config.clone(),
            self.state_dir.clone(),
            page_snapshot,
        );
        cx.notify();
        request_id
    }

    /// AI 栏「下载选中」：逐个构造请求并入队，带资源文件名。
    fn download_ai_selected(&mut self, cx: &mut Context<Self>) {
        let selected = self.ai.selected_indices();
        if selected.is_empty() {
            self.set_toast("请先勾选要下载的资源", true);
            cx.notify();
            return;
        }
        let resources: Vec<rdm_ai::ParsedResource> = selected
            .into_iter()
            .filter_map(|index| {
                self.ai
                    .resources
                    .get(index)
                    .map(|card| card.resource.clone())
            })
            .collect();
        let mut added = 0usize;
        let mut first_error: Option<String> = None;
        let mut failed = 0usize;
        for resource in resources {
            let mut request = self.settings.request_with_defaults(resource.url.clone());
            if !resource.name.trim().is_empty() {
                request.file_name = Some(resource.name.clone());
            }
            // P4b-a：只有资源 host 与本次解析来源 host 完全一致时才交接内存凭据
            // （Cookie 不能跨 host 发送）。凭据不进日志/任务 store。
            self.ai.apply_credentials(&resource.url, &mut request);
            match model::prepare_request(&self.settings, request) {
                Ok(request) => {
                    self.push_download(request, "ai".to_string());
                    added += 1;
                }
                Err(err) => {
                    failed += 1;
                    first_error.get_or_insert(err);
                }
            }
        }
        if added > 0 {
            self.set_toast(format!("已添加 {added} 个下载"), false);
        }
        if let Some(err) = first_error {
            self.set_toast(format!("{failed} 个资源添加失败：{err}"), true);
        }
        cx.notify();
    }

    /// AI 栏底部输入框：Enter/发送 → `start_ai_chat_stream`（上下文带解析结果摘要）。
    pub fn send_ai_chat(&mut self, cx: &mut Context<Self>) {
        let text = self.ai_chat_input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.ai.chat_pending {
            self.set_toast("AI 正在回复，请稍候", true);
            cx.notify();
            return;
        }
        if !self.ai_config.is_configured() {
            self.ai.visible = true;
            self.ai.push_message(
                AiRole::System,
                "AI 未配置：请在设置 → AI 配置填写 Base URL / 模型 / API Key 并启用。",
            );
            self.ai_chat_input.update(cx, |input, cx| input.clear(cx));
            cx.notify();
            return;
        }
        self.next_ai_request += 1;
        let request_id = self.next_ai_request;
        self.ai_chat_request = Some(request_id);
        self.ai.visible = true;
        // 发送新问题 = 用户重新关注最新回复：恢复自动滚动，并立即滚到底部。
        self.ai_autoscroll = true;
        self.ai.begin_chat_stream(&text);
        self.ai_chat_input.update(cx, |input, cx| input.clear(cx));
        let system = AI_CHAT_SYSTEM_PROMPT.to_string();
        let prompt = build_ai_chat_prompt(&text, &self.ai);
        tracing::info!(request_id, "发起 AI 聊天（流式）");
        self.engine
            .start_ai_chat_stream(request_id, self.ai_config.clone(), system, prompt);
        self.ai_chat_scroll.scroll_to_bottom();
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 删除确认（行内「删除」→ 弹窗 → 按选择清理文件后移出列表）
    // -----------------------------------------------------------------------

    fn open_delete_confirm(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        self.delete_confirm = Some(DeleteConfirm {
            index,
            delete_file: DeleteConfirm::default_delete_file(item),
        });
        cx.notify();
    }

    /// 行内「删除」（渲染层拿到的是过滤后的行号，这里换算回 `items` 索引）。
    fn open_delete_confirm_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.open_delete_confirm(index, cx);
        }
    }

    fn cancel_delete_confirm(&mut self, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        cx.notify();
    }

    fn toggle_delete_file(&mut self, cx: &mut Context<Self>) {
        if let Some(confirm) = &mut self.delete_confirm {
            confirm.delete_file = !confirm.delete_file;
        }
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.delete_confirm.take() else {
            return;
        };
        let Some(item) = self.items.get(confirm.index) else {
            cx.notify();
            return;
        };
        let id = item.id.clone();
        if confirm.delete_file {
            let targets = delete_targets(item, &self.state_dir);
            let (removed, errors) = delete_target_files(&targets);
            if !errors.is_empty() {
                self.set_toast(
                    format!("删除文件失败（{} 个）：{}", errors.len(), errors[0]),
                    true,
                );
            } else if removed > 0 {
                self.set_toast(format!("已删除任务和 {removed} 个文件"), false);
            } else {
                self.set_toast("已删除任务（没有找到可删除的文件）", false);
            }
        } else {
            self.set_toast("已删除任务", false);
        }
        // 文件清理失败也把任务移出列表：错误已经通过 toast 提示，避免任务卡在列表里。
        if let Some(index) = self.items.iter().position(|item| item.id == id) {
            self.remove_item(index);
        }
        cx.notify();
    }

    fn open_folder_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let item = &self.items[index];
        let target = open_path::reveal_target_for_item(
            item.path.as_deref(),
            item.request.output_dir.as_deref(),
            &self.state_dir,
        );
        if let Err(err) = open_path::reveal_path(&target) {
            self.set_toast(format!("打开文件夹失败: {err}"), true);
        }
        cx.notify();
    }

    fn open_file_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        if let Some(path) = self.items[index].path.clone() {
            if let Err(err) = open_path::open_file(&path) {
                self.set_toast(format!("打开文件失败: {err}"), true);
            }
        }
        cx.notify();
    }

    fn pause_item(&mut self, index: usize) {
        let id = self.items[index].id.clone();
        // 状态保持「下载中」，等引擎确认 Paused 再迁移（避免 resume 与旧任务并发写盘）。
        if !self.items[index].begin_stop(StopRequest::Pause) {
            return;
        }
        if let Some((_, handle)) = self.handles.get(&id) {
            handle.pause();
        }
    }

    fn resume_item(&mut self, index: usize) {
        let state = self.items[index].state;
        if !matches!(
            state,
            ItemState::Paused | ItemState::Failed | ItemState::Cancelled
        ) || !self.items[index].stop_request.is_none()
        {
            return;
        }
        self.start_item(index);
    }

    fn cancel_item(&mut self, index: usize) {
        let id = self.items[index].id.clone();
        if self.items[index].state == ItemState::Downloading {
            // 取消同样等引擎回 Cancelled；期间展示「取消中…」。
            if !self.items[index].begin_stop(StopRequest::Cancel) {
                return;
            }
            if let Some((_, handle)) = self.handles.get(&id) {
                handle.cancel();
            }
            return;
        }
        if self.items[index].state.is_terminal() {
            return;
        }
        // 已暂停的任务没有运行中的引擎 task，直接落为已取消。
        self.handles.remove(&id);
        let item = &mut self.items[index];
        item.state = ItemState::Cancelled;
        item.stop_request = StopRequest::None;
        item.error = Some("已取消".to_string());
        item.note = None;
        item.clear_speed();
        self.persist_now();
    }

    fn remove_item(&mut self, index: usize) {
        let id = self.items[index].id.clone();
        if let Some((_, handle)) = self.handles.remove(&id) {
            handle.cancel();
        }
        self.items.remove(index);
        self.persist_now();
    }

    fn clear_completed(&mut self, cx: &mut Context<Self>) {
        self.items.retain(|item| item.state != ItemState::Completed);
        self.persist_now();
        cx.notify();
    }

    fn start_item(&mut self, index: usize) {
        // 先复制路由参数，避免与 `items[index]` 的可变借用冲突。
        let settings = self.settings.clone();
        let item = &mut self.items[index];
        item.run_id += 1;
        let run_id = item.run_id;
        let id = item.id.clone();
        let mut request = item.request.clone();
        // 续传复用上次分类保存目录（§4.2 save_dir），不再重新 probe/分类。
        let routing = if let Some(dir) = item.save_dir.clone() {
            request.output_dir = Some(dir);
            None
        } else if settings.save_by_extension {
            Some(RoutingOptions {
                save_by_extension: true,
                classification: settings.classification.clone(),
            })
        } else {
            None
        };
        item.state = ItemState::Downloading;
        item.stop_request = StopRequest::None;
        item.error = None;
        item.note = Some("排队中…".to_string());
        item.speed_history.clear();
        item.clear_speed();
        let handle = DownloadHandle::new();
        self.handles.insert(id.clone(), (run_id, handle.clone()));
        self.engine
            .start_download_routed(id, run_id, request, handle, routing);
        self.persist_now();
    }

    fn allocate_id(&mut self) -> String {
        let id = next_download_id(self.next_seq);
        self.next_seq += 1;
        id
    }

    /// 入队并（按设置）自动开始；返回 download_id。
    fn push_download(&mut self, request: DownloadRequest, source: String) -> String {
        let id = self.allocate_id();
        let mut item = DownloadItem::new(id.clone(), request, source);
        let auto_start = self.settings.auto_start;
        if !auto_start {
            item.state = ItemState::Paused;
            item.note = Some("已添加，等待开始（自动开始已关闭）".to_string());
        }
        self.items.push(item);
        let index = self.items.len() - 1;
        if auto_start {
            self.start_item(index);
        } else {
            self.persist_now();
        }
        id
    }

    /// 展示一次性提示（状态栏左侧，5 秒后自动消失；`main.rs` 初始化告警也用）。
    pub(crate) fn set_toast(&mut self, text: impl Into<String>, is_error: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            is_error,
            created: Instant::now(),
        });
    }

    // -----------------------------------------------------------------------
    // 持久化
    // -----------------------------------------------------------------------

    fn persist_now(&mut self) {
        self.dirty_persist = false;
        self.last_save = Instant::now();
        if let Err(err) = model::save_downloads(&self.state_dir, &self.items, self.next_seq) {
            tracing::warn!(error = %err, "保存 downloads.json 失败");
            self.set_toast(format!("保存下载列表失败: {err}"), true);
        }
    }

    fn flush_if_dirty(&mut self) {
        if self.dirty_persist && self.last_save.elapsed() >= Duration::from_secs(1) {
            self.persist_now();
        }
    }

    // -----------------------------------------------------------------------
    // 引擎消息批处理
    // -----------------------------------------------------------------------

    fn apply_batch(&mut self, batch: Vec<EngineMsg>, cx: &mut Context<Self>) {
        let mut notify = false;
        for msg in batch {
            match msg {
                EngineMsg::DownloadEvent { id, run_id, event } => {
                    if let Some(item) = self
                        .items
                        .iter_mut()
                        .find(|item| item.id == id && item.run_id == run_id)
                    {
                        item.apply_event(event);
                        self.dirty_persist = true;
                        notify = true;
                    }
                }
                EngineMsg::Finished { id, run_id, result } => {
                    if let Some(item) = self
                        .items
                        .iter_mut()
                        .find(|item| item.id == id && item.run_id == run_id)
                    {
                        item.apply_result(result);
                        self.dirty_persist = true;
                        notify = true;
                    }
                    if self
                        .handles
                        .get(&id)
                        .is_some_and(|(handle_run, _)| *handle_run == run_id)
                    {
                        self.handles.remove(&id);
                    }
                    self.persist_now();
                }
                EngineMsg::Routing {
                    id,
                    run_id,
                    dir,
                    reason,
                } => {
                    if let Some(item) = self
                        .items
                        .iter_mut()
                        .find(|item| item.id == id && item.run_id == run_id)
                    {
                        item.save_dir = Some(dir);
                        if !reason.is_empty() {
                            item.note = Some(format!("分类保存：{reason}"));
                        }
                        self.dirty_persist = true;
                        notify = true;
                    }
                }
                EngineMsg::AiParseFinished { request_id, report } => {
                    // 丢弃旧请求/已关闭面板后的迟到结果。
                    if self.ai.request_id == request_id {
                        self.ai.apply_report(report);
                        notify = true;
                    }
                }
                EngineMsg::AiParseFailed { request_id, error } => {
                    if self.ai.request_id == request_id {
                        self.ai.apply_parse_error(error);
                        notify = true;
                    }
                }
                EngineMsg::AiChatDelta { request_id, delta } => {
                    // 只接受当前请求的增量；旧请求（或被解析刷新丢掉的）直接忽略。
                    if self.ai_chat_request == Some(request_id) {
                        self.ai.push_chat_delta(&delta);
                        // 用户向上滚看历史后不抢滚动位置；否则跟随最新内容。
                        if self.ai_autoscroll {
                            self.ai_chat_scroll.scroll_to_bottom();
                        }
                        notify = true;
                    }
                }
                EngineMsg::AiChatFinished {
                    request_id,
                    reply,
                    usage: _,
                } => {
                    if self.ai_chat_request == Some(request_id) {
                        self.ai_chat_request = None;
                        // 用完整回复覆盖流式占位（网络丢包时 delta 可能缺字/重复）。
                        self.ai.finish_chat_stream(&reply);
                        if self.ai_autoscroll {
                            self.ai_chat_scroll.scroll_to_bottom();
                        }
                        notify = true;
                    }
                }
                EngineMsg::AiChatFailed {
                    request_id,
                    error,
                    usage: _,
                } => {
                    if self.ai_chat_request == Some(request_id) {
                        self.ai_chat_request = None;
                        self.ai.fail_chat_stream(&error);
                        if self.ai_autoscroll {
                            self.ai_chat_scroll.scroll_to_bottom();
                        }
                        notify = true;
                    }
                }
                EngineMsg::Tick => {
                    if active_count(&self.items) > 0 {
                        let speed = global_speed_bps(&self.items);
                        self.speed_history.push_back(speed);
                        while self.speed_history.len() > GLOBAL_HISTORY_LEN {
                            self.speed_history.pop_front();
                        }
                        notify = true;
                    }
                    if let Some(toast) = &self.toast {
                        if toast.created.elapsed() > TOAST_TTL {
                            self.toast = None;
                            notify = true;
                        }
                    }
                    self.flush_if_dirty();
                }
                EngineMsg::ExperienceApplied { .. } => {
                    // 旧版 UI（--ui=legacy）不承载解析经验；没有对应界面可更新。
                }
                EngineMsg::Ipc { envelope, reply } => {
                    let response = self.handle_ipc(envelope, cx);
                    let _ = reply.send(response);
                    notify = true;
                }
            }
        }
        if notify {
            cx.notify();
        }
    }

    /// GUI 主线程处理 IPC 请求（已鉴权、已解析）。
    fn handle_ipc(&mut self, envelope: Envelope, cx: &mut Context<Self>) -> Response {
        let request_id = envelope.id;
        match envelope.request {
            Request::Ping => Response::ok(request_id, ipc_server::ping_payload(&self.items)),
            Request::Status => Response::ok(request_id, ipc_server::status_payload(&self.items)),
            Request::Enqueue { item } => {
                let source = if item.source.is_empty() {
                    "ipc".to_string()
                } else {
                    item.source
                };
                match model::prepare_request(&self.settings, item.download) {
                    Ok(request) => {
                        let id = self.push_download(request, source);
                        tracing::info!(download_id = %id, "IPC 入队");
                        Response::ok(request_id, ipc_server::enqueue_payload(&id))
                    }
                    Err(err) => Response::error(request_id, err),
                }
            }
            Request::Pause { id } => {
                if let Some(index) = self.index_of(&id) {
                    self.pause_item(index);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::Resume { id } => {
                if let Some(index) = self.index_of(&id) {
                    self.resume_item(index);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::Cancel { id } => {
                if let Some(index) = self.index_of(&id) {
                    self.cancel_item(index);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::OpenFolder { id } => {
                if self.index_of(&id).is_some() {
                    self.open_folder_by_id(&id, cx);
                    Response::ok(request_id, serde_json::json!({ "id": id }))
                } else {
                    Response::error(request_id, "unknown download id")
                }
            }
            Request::AiParse {
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
                // 旧版 UI 不提供「强制 AI 重新解析」入口；字段仍接受（IPC 兼容）。
                force_ai: _force_ai,
            } => {
                match validate_url(&url) {
                    Ok(url) => {
                        self.activate_window(cx);
                        // Cookie 只在本次内存请求里转交抓取逻辑，不落盘、不写日志。
                        let cookie = if include_credentials { cookie } else { None };
                        // P4b-b2：host 不符/无效的页面快照在转换时被忽略（None），
                        // 只在本机内存链路传给 resolver-first；不写日志值。
                        let page_snapshot = crate::ai_resolve::page_snapshot_from_ipc(
                            &url,
                            page_url.as_deref(),
                            page_title.as_deref(),
                            &page_links,
                        );
                        let request = rdm_ai::ParseRequest {
                            url,
                            title,
                            referer,
                            user_agent,
                            cookie,
                            hint: source_page,
                        };
                        let ai_request_id = self.begin_ai_parse(request, page_snapshot, cx);
                        Response::ok(
                            request_id,
                            serde_json::json!({
                                "accepted": true,
                                "request_id": ai_request_id,
                            }),
                        )
                    }
                    Err(err) => Response::error(request_id, err),
                }
            }
            Request::Activate => {
                self.activate_window(cx);
                Response::ok(request_id, serde_json::json!({ "activated": true }))
            }
        }
    }

    /// 激活/恢复主窗口：浏览器扩展的 `Activate`、第二次启动、AI 解析都会用到。
    ///
    /// 不能只用 `cx.active_window()`：静默启动或「关闭到托盘」后窗口是隐藏的，
    /// 此时 GPUI 没有 active window，必须按保存的句柄显式 `ShowWindow` 恢复。
    fn activate_window(&mut self, cx: &mut Context<Self>) {
        self.restore_window(cx);
    }

    // -----------------------------------------------------------------------
    // 渲染
    // -----------------------------------------------------------------------

    fn render_topbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .h(px(56.))
            .px(px(16.))
            .bg(palette.panel)
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(palette.accent)
                            .child("RDM"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(palette.text_faint)
                            .child("Rust Download Manager"),
                    ),
            )
            .child(div().flex_1().child(self.url_input.clone()))
            .child(primary_button(
                "add-url",
                "添加",
                palette,
                cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.add_from_input(window, cx);
                }),
            ))
            .child(ghost_button(
                "ai-parse",
                "AI 解析",
                palette,
                cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.ai_parse_from_input(window, cx);
                }),
            ))
            .child(ghost_button(
                "open-settings",
                "设置",
                palette,
                cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.open_settings(window, cx);
                }),
            ))
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let mut filters = Vec::new();
        for (index, filter) in Filter::ALL.iter().enumerate() {
            let active = self.filter == *filter;
            let count = filter_count(&self.items, *filter);
            let filter = *filter;
            filters.push(
                div()
                    .id(ElementId::named_usize("filter", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(32.))
                    .px(px(10.))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if active {
                        palette.accent_soft
                    } else {
                        palette.panel
                    })
                    .text_size(px(13.))
                    .text_color(if active {
                        palette.accent
                    } else {
                        palette.text_dim
                    })
                    .hover(move |style| style.bg(palette.panel_alt))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.filter = filter;
                        cx.notify();
                    }))
                    .child(filter.label_zh())
                    .child(
                        div()
                            .text_xs()
                            .text_color(if active {
                                palette.accent
                            } else {
                                palette.text_faint
                            })
                            .child(format!("{count}")),
                    ),
            );
        }

        let global_speed = global_speed_bps(&self.items);
        let history: Vec<f64> = self.speed_history.iter().copied().collect();
        let history_color = palette.accent;

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .px(px(12.))
            .py(px(16.))
            .gap(px(6.))
            .bg(palette.panel)
            .border_r_1()
            .border_color(palette.border)
            .child(
                div()
                    .px(px(10.))
                    .pb(px(4.))
                    .text_xs()
                    .text_color(palette.text_faint)
                    .child("过滤器"),
            )
            .children(filters)
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded_md()
                    .bg(palette.panel_alt)
                    .child(
                        div()
                            .text_xs()
                            .text_color(palette.text_faint)
                            .child("全局限速"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.text_dim)
                            .child(format_limit(&self.settings)),
                    )
                    .child(
                        div()
                            .pt(px(6.))
                            .text_xs()
                            .text_color(palette.text_faint)
                            .child("全局速度"),
                    )
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(palette.accent)
                            .child(format_speed(global_speed)),
                    )
                    .child(
                        div().pt(px(6.)).h(px(48.)).w_full().child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, _| {
                                    if history.is_empty() {
                                        return;
                                    }
                                    let max =
                                        history.iter().copied().fold(0.0_f64, f64::max).max(1.0);
                                    let count = history.len() as f32;
                                    let gap = 1.0_f32;
                                    let width: f32 = bounds.size.width.into();
                                    let height: f32 = bounds.size.height.into();
                                    let origin_x: f32 = bounds.origin.x.into();
                                    let origin_y: f32 = bounds.origin.y.into();
                                    let bar_width =
                                        ((width - gap * (count - 1.0)) / count).max(1.0);
                                    for (index, value) in history.iter().enumerate() {
                                        let bar_height =
                                            (height * (*value as f32 / max as f32)).max(1.0);
                                        let x = origin_x + index as f32 * (bar_width + gap);
                                        let y = origin_y + height - bar_height;
                                        window.paint_quad(fill(
                                            Bounds::new(
                                                point(px(x), px(y)),
                                                size(px(bar_width), px(bar_height)),
                                            ),
                                            history_color,
                                        ));
                                    }
                                },
                            )
                            .w_full()
                            .h_full(),
                        ),
                    ),
            )
    }

    fn render_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let indices = filtered_indices(&self.items, self.filter);
        let count = indices.len();

        let content: AnyElement = if count == 0 {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(15.))
                        .text_color(palette.text_dim)
                        .child("暂无下载任务"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.text_faint)
                        .child("在上方输入 URL，点击「添加」或按 Enter 开始下载"),
                )
                .into_any_element()
        } else {
            let processor =
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let indices = filtered_indices(&this.items, this.filter);
                    // AI 栏/窄窗口下主列表变窄，行进入紧凑布局（仍显示动态连接信息）。
                    let compact = this.compact_rows;
                    let mut rows = Vec::with_capacity(range.len());
                    for index in range {
                        if let Some(item_index) = indices.get(index).copied() {
                            if let Some(item) = this.items.get(item_index) {
                                rows.push(
                                    this.render_row(index, item, compact, cx).into_any_element(),
                                );
                            }
                        }
                    }
                    rows
                });
            uniform_list("downloads", count, processor)
                .size_full()
                .track_scroll(self.scroll_handle.clone())
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(40.))
                    .px(px(16.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(palette.border)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(self.filter.label_zh()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.text_faint)
                                    .child(format!("{count} 项")),
                            ),
                    )
                    .child(ghost_button(
                        "clear-completed",
                        "清空已完成",
                        palette,
                        cx.listener(|this, _: &ClickEvent, _window, cx| {
                            if this
                                .items
                                .iter()
                                .any(|item| item.state == ItemState::Completed)
                            {
                                this.clear_completed(cx);
                            }
                        }),
                    )),
            )
            .child(content)
    }

    fn render_row(
        &self,
        index: usize,
        item: &DownloadItem,
        compact: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let palette = self.palette;
        let id = item.id.clone();
        let state = item.state;
        let stopping = !item.stop_request.is_none();
        let badge_color = if stopping {
            palette.warning
        } else {
            match state {
                ItemState::Downloading => palette.info,
                ItemState::Paused => palette.warning,
                ItemState::Completed => palette.success,
                ItemState::Failed => palette.danger,
                ItemState::Cancelled => palette.text_faint,
            }
        };
        let badge_label = if let Some(label) = item.stop_request.label_zh() {
            label
        } else {
            state.label_zh()
        };
        let progress_color = match state {
            ItemState::Downloading => palette.accent,
            ItemState::Paused => palette.warning,
            ItemState::Completed => palette.success,
            ItemState::Failed | ItemState::Cancelled => palette.danger,
        };
        let fraction = item.progress_fraction().unwrap_or(0.0) as f32;
        let alternate = index % 2 == 1;
        let background = if alternate {
            palette.row_alt
        } else {
            palette.bg
        };

        // 文件名（完成后可点击打开文件）。
        let file_label = div()
            .flex_1()
            .truncate()
            .text_size(px(14.))
            .text_color(palette.text)
            .child(item.file_name.clone());
        let file_label: AnyElement = if state == ItemState::Completed && item.path.is_some() {
            let open_id = id.clone();
            div()
                .id(ElementId::named_usize("file-open", index))
                .flex_1()
                .truncate()
                .text_size(px(14.))
                .text_color(palette.text)
                .cursor_pointer()
                .hover(move |style| style.text_color(palette.accent))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.open_file_by_id(&open_id, cx);
                }))
                .child(item.file_name.clone())
                .into_any_element()
        } else {
            file_label.into_any_element()
        };

        // 第二行说明：错误 / 重试 / 分类保存目录 / 路径（开启 SHA-256 后展示引擎返回的校验值）。
        let detail_text = if let Some(error) = &item.error {
            error.clone()
        } else if let Some(note) = &item.note {
            note.clone()
        } else if state != ItemState::Completed {
            match &item.save_dir {
                Some(dir) => format!("分类保存到：{}", dir.display()),
                None => item.url.clone(),
            }
        } else if self.settings.verify_sha256 {
            if let Some(hash) = &item.sha256 {
                format!("SHA-256: {hash}")
            } else {
                item.path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| item.url.clone())
            }
        } else {
            item.path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| item.url.clone())
        };
        let detail_color = if stopping {
            palette.warning
        } else if item.error.is_some() {
            palette.danger
        } else if item.note.is_some() {
            palette.warning
        } else {
            palette.text_faint
        };

        // 行内操作按钮。停止动作等待引擎确认期间只保留目录/删除。
        let mut actions: Vec<AnyElement> = Vec::new();
        match state {
            ItemState::Downloading if !stopping => {
                let target = id.clone();
                actions.push(row_action_button(
                    ElementId::named_usize("pause", index),
                    "暂停",
                    palette,
                    true,
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.pause_by_id(&target, cx);
                    }),
                ));
                let target = id.clone();
                actions.push(row_action_button(
                    ElementId::named_usize("cancel", index),
                    "取消",
                    palette,
                    true,
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.cancel_by_id(&target, cx);
                    }),
                ));
            }
            ItemState::Paused | ItemState::Failed | ItemState::Cancelled if !stopping => {
                let target = id.clone();
                actions.push(row_action_button(
                    ElementId::named_usize("resume", index),
                    "继续",
                    palette,
                    true,
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.resume_by_id(&target, cx);
                    }),
                ));
            }
            ItemState::Completed => {
                let target = id.clone();
                actions.push(row_action_button(
                    ElementId::named_usize("open-file", index),
                    "打开",
                    palette,
                    item.path.is_some(),
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.open_file_by_id(&target, cx);
                    }),
                ));
            }
            // Downloading 且 stopping：只显示目录/删除（在 match 外追加）。
            _ => {}
        }
        {
            let target = id.clone();
            actions.push(row_action_button(
                ElementId::named_usize("open-folder", index),
                "目录",
                palette,
                true,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.open_folder_by_id(&target, cx);
                }),
            ));
        }
        {
            let target = id.clone();
            actions.push(row_action_button(
                ElementId::named_usize("remove", index),
                "删除",
                palette,
                true,
                cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.open_delete_confirm_by_id(&target, cx);
                }),
            ));
        }

        // 行内速度迷你曲线：交给共享 widget，绘制区域限制在 72×18 画布内
        // （历史 bug：60 采样按 1px 柱宽 + 1px 间隙画成 119px，溢出覆盖右侧徽章）。
        let sparkline = crate::ui::widgets::sparkline(
            item.speed_history.iter().copied().collect(),
            progress_color.into(),
            72.0,
            18.0,
        );

        div()
            .flex()
            .flex_col()
            .h(px(ROW_HEIGHT))
            .px(px(16.))
            .py(px(10.))
            .gap(px(4.))
            .bg(background)
            .border_b_1()
            .border_color(palette.border)
            // 第一行：状态徽章 + 文件名 + 行内操作
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .h(px(22.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(58.))
                            .h(px(20.))
                            .rounded_sm()
                            .bg(Hsla::from(badge_color).opacity(0.16))
                            .text_color(badge_color)
                            .text_xs()
                            .child(badge_label),
                    )
                    .child(file_label)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .children(actions),
                    ),
            )
            // 第二行：错误/重试/路径（单行截断）
            .child(
                div()
                    .h(px(16.))
                    .truncate()
                    .text_xs()
                    .text_color(detail_color)
                    .child(detail_text),
            )
            // 第三行：进度条 + 数值 + 速度曲线。
            // AI 栏占宽时进入紧凑布局：隐藏大小/ETA/曲线，优先保证进度与动态连接数可读。
            .child({
                let mut metrics: Vec<AnyElement> = Vec::new();
                metrics.push(
                    div()
                        .flex_1()
                        .h(px(8.))
                        .rounded_sm()
                        .bg(palette.track)
                        .child(
                            div()
                                .h_full()
                                .rounded_sm()
                                .bg(progress_color)
                                .w(gpui::relative(fraction.clamp(0.0, 1.0))),
                        )
                        .into_any_element(),
                );
                metrics.push(
                    div()
                        .w(px(46.))
                        .text_xs()
                        .text_color(palette.text_dim)
                        .child(item.percent_text())
                        .into_any_element(),
                );
                if !compact {
                    metrics.push(
                        div()
                            .w(px(80.))
                            .text_xs()
                            .text_color(palette.text_dim)
                            .child(item.size_text())
                            .into_any_element(),
                    );
                }
                metrics.push(
                    div()
                        .w(px(88.))
                        .text_xs()
                        .text_color(if state.is_active() {
                            palette.text
                        } else {
                            palette.text_dim
                        })
                        .child(item.speed_text())
                        .into_any_element(),
                );
                if !compact {
                    metrics.push(
                        div()
                            .w(px(52.))
                            .text_xs()
                            .text_color(palette.text_dim)
                            .child(format!("ETA {}", item.eta_text()))
                            .into_any_element(),
                    );
                }
                metrics.push(
                    div()
                        .w(px(100.))
                        .text_xs()
                        .text_color(palette.text_faint)
                        .child(item.connections_text())
                        .into_any_element(),
                );
                if !compact {
                    metrics.push(sparkline);
                }
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .h(px(20.))
                    .children(metrics)
            })
            .into_any_element()
    }

    fn render_statusbar(&self) -> impl IntoElement {
        let palette = self.palette;
        let active = active_count(&self.items);
        let workers: usize = self
            .items
            .iter()
            .filter(|item| item.state.is_active())
            .map(|item| item.active_workers)
            .sum();
        let completed = filter_count(&self.items, Filter::Completed);
        let (left, color) = match &self.toast {
            Some(toast) if toast.is_error => (toast.text.clone(), palette.danger),
            Some(toast) => (toast.text.clone(), palette.success),
            None => (
                format!("{active} 个活动任务 · {workers} 个连接 · {completed} 个已完成"),
                palette.text_dim,
            ),
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .h(px(26.))
            .px(px(16.))
            .bg(palette.panel)
            .border_t_1()
            .border_color(palette.border)
            .text_xs()
            .text_color(color)
            .child(left)
            .child(
                div()
                    .text_color(palette.text_faint)
                    .child(format!("RDM v{}", env!("CARGO_PKG_VERSION"))),
            )
    }

    /// 设置弹窗：顶部标题栏 + 左侧分类导航 + 右侧当前分类内容 + 底部「取消 / 保存」。
    ///
    /// 分类只决定「渲染哪一页」，所有 `draft_*` 草稿状态始终保留在 `RdmApp` 上：
    /// 切换分类不会丢未保存的修改，点「保存」才落盘；主题的即时预览与回滚逻辑不变。
    fn render_settings(&self, cx: &Context<Self>) -> impl IntoElement {
        let palette = self.palette;

        // 左侧分类导航：样式对齐 `render_sidebar` 的过滤器行（选中 accent_soft）。
        let mut tabs: Vec<AnyElement> = Vec::new();
        for tab in SettingsTab::ALL {
            let active = self.settings_tab == tab;
            tabs.push(
                div()
                    .id(tab.element_id())
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(32.))
                    .px(px(10.))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if active {
                        palette.accent_soft
                    } else {
                        palette.panel
                    })
                    .text_size(px(13.))
                    .text_color(if active {
                        palette.accent
                    } else {
                        palette.text_dim
                    })
                    .hover(move |style| style.bg(palette.panel_alt))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        // 只切当前分类；draft_* 保持不变（草稿不因切换分类丢失）。
                        this.settings_tab = tab;
                        cx.notify();
                    }))
                    .child(tab.label_zh())
                    .into_any_element(),
            );
        }

        let panel = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .w(px(760.))
            .p(px(16.))
            .bg(palette.panel)
            .border_1()
            .border_color(palette.border)
            .rounded_lg()
            .shadow_lg()
            .child(
                // 标题栏固定在顶部，不随分类切换移动。
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("设置"),
                    )
                    .child(ghost_button(
                        "settings-close",
                        "关闭",
                        palette,
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.cancel_settings(window, cx);
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(18.))
                    .w_full()
                    .min_w(px(0.))
                    .child(
                        // 分类导航：固定宽度、允许内部收缩，长分类名不会撑破弹窗。
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .w(px(160.))
                            .min_w(px(160.))
                            .flex_shrink_0()
                            .child(
                                div()
                                    .px(px(10.))
                                    .pb(px(4.))
                                    .text_xs()
                                    .text_color(palette.text_faint)
                                    .child("分类"),
                            )
                            .children(tabs),
                    )
                    .child(
                        // 内容区固定高度：5 个分类切换时弹窗高度不变（无滚动条）。
                        // 右列 min_w(0) 允许长说明换行，不会把弹窗撑破（旧截图 bug 教训）。
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(SETTINGS_CONTENT_HEIGHT))
                            .children(self.render_settings_content(cx)),
                    ),
            )
            .child(
                // 底部操作栏固定：取消回滚主题预览，保存才写 settings.json / ai.json。
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(8.))
                    .child(ghost_button(
                        "settings-cancel",
                        "取消",
                        palette,
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.cancel_settings(window, cx);
                        }),
                    ))
                    .child(primary_button(
                        "settings-save",
                        "保存",
                        palette,
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.save_settings(window, cx);
                        }),
                    )),
            );

        div()
            .id("settings-backdrop")
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(palette.overlay)
            // 吞掉点击，避免穿透到背后的列表行；弹窗内部控件仍先收到事件。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(panel)
    }

    /// 当前选中分类的设置内容（只渲染一页；切换不影响 `draft_*` 草稿）。
    fn render_settings_content(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        match self.settings_tab {
            SettingsTab::Download => self.render_settings_download(cx),
            SettingsTab::Classification => self.render_settings_classification(cx),
            SettingsTab::Ai => self.render_settings_ai(cx),
            SettingsTab::Appearance => self.render_settings_appearance(cx),
            SettingsTab::System => self.render_settings_system(cx),
        }
    }

    /// 下载分类：默认保存目录 / 默认连接数 / 全局限速 / 动态线程规划 / 自动开始。
    fn render_settings_download(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let mut fields: Vec<AnyElement> = Vec::new();
        fields.push(section_title(
            "下载",
            "新建任务的默认参数；保存后对之后添加的任务生效",
            palette,
        ));
        fields.push(labeled_input(
            "默认保存目录",
            self.dir_input.clone(),
            palette,
        ));
        fields.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .w_full()
                .min_w(px(0.))
                .child(settings_label("默认连接数", palette))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(self.connections_input.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(palette.text_faint)
                        .child(format!("1..={}", rdm_core::MAX_CONNECTIONS)),
                )
                .into_any_element(),
        );
        fields.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .w_full()
                .min_w(px(0.))
                .child(settings_label("全局限速", palette))
                .child(div().flex_1().min_w(px(0.)).child(self.limit_input.clone()))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(palette.text_faint)
                        .child("MB/s，空=不限速"),
                )
                .into_any_element(),
        );
        fields.push(
            settings_toggle_row(
                "toggle-dynamic",
                "动态线程规划",
                "按实时吞吐自动增减连接数",
                self.draft_dynamic,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_dynamic = !this.draft_dynamic;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields.push(
            settings_toggle_row(
                "toggle-auto",
                "自动开始",
                "添加后立即开始下载",
                self.draft_auto_start,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_auto_start = !this.draft_auto_start;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields
    }

    /// 分类与校验：按后缀分类保存 / SHA-256 校验（含原底部 SHA 提示）。
    fn render_settings_classification(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let mut fields: Vec<AnyElement> = Vec::new();
        fields.push(section_title(
            "分类与校验",
            "按文件类型自动归档，并决定完成后是否记录校验值",
            palette,
        ));
        fields.push(
            settings_toggle_row(
                "toggle-save-by-ext",
                "按后缀分类保存",
                "视频/音频/图片… 自动分子目录",
                self.draft_save_by_extension,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_save_by_extension = !this.draft_save_by_extension;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields.push(
            settings_toggle_row(
                "toggle-sha",
                "SHA-256 校验",
                "完成后记录引擎返回的校验值",
                self.draft_verify_sha256,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_verify_sha256 = !this.draft_verify_sha256;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        // 原来固定在最底部的 SHA 说明，跟着对应开关放进本分类。
        fields.push(
            div()
                .pt(px(2.))
                .w_full()
                .min_w(px(0.))
                .text_xs()
                .text_color(palette.text_faint)
                .child(
                    "提示：SHA-256 由下载引擎在结果中返回；核心请求契约的 expected_sha256 \
                     仅用于传入期望值校验。",
                )
                .into_any_element(),
        );
        fields
    }

    /// AI 分类：启用 AI + Base URL / 模型 / API Key + 三条使用提示（只写本机 ai.json）。
    fn render_settings_ai(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let mut fields: Vec<AnyElement> = Vec::new();
        fields.push(section_title(
            "AI 配置",
            "只写入本机 ai.json；API Key 不进下载记录",
            palette,
        ));
        fields.push(
            settings_toggle_row(
                "toggle-ai-enabled",
                "启用 AI",
                "关闭时解析只用本地启发式",
                self.draft_ai.enabled,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_ai.enabled = !this.draft_ai.enabled;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields.push(labeled_input(
            "Base URL",
            self.ai_config_inputs.0.clone(),
            palette,
        ));
        fields.push(labeled_input(
            "模型",
            self.ai_config_inputs.1.clone(),
            palette,
        ));
        fields.push(labeled_input(
            "API Key",
            self.ai_config_inputs.2.clone(),
            palette,
        ));
        fields.push(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .w_full()
                .min_w(px(0.))
                .text_xs()
                .text_color(palette.text_faint)
                .child(div().w_full().child(
                    "① 默认 DeepSeek：Base URL https://api.deepseek.com/v1，模型 deepseek-flash（自动补 /chat/completions）。",
                ))
                .child(
                    div()
                        .w_full()
                        .child("② API Key 掩码显示；环境变量 RDM_AI_API_KEY 优先。"),
                )
                .child(
                    div()
                        .w_full()
                        .child("③ 启用 AI 后「AI 解析」会调用模型，失败自动降级本地启发式。"),
                )
                .into_any_element(),
        );
        fields
    }

    /// 外观分类：当前主题名 + 6 个主题按钮（两行三列，即时预览）。
    fn render_settings_appearance(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let theme = self.draft_theme;
        let mut fields: Vec<AnyElement> = Vec::new();
        fields.push(section_title(
            "外观",
            "主题即时预览；取消或关闭弹窗会回退到已保存主题",
            palette,
        ));
        fields.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(settings_label("主题", palette))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.accent)
                        .child(theme.label_zh()),
                )
                .into_any_element(),
        );
        // 6 个变体，两行三列（短标签，完整名见 `Theme::label_zh`）。
        for row in Theme::ALL.chunks(3) {
            let mut buttons: Vec<AnyElement> = Vec::new();
            for value in row {
                let value = *value;
                buttons.push(
                    theme_button(
                        theme_button_id(value),
                        value,
                        theme,
                        palette,
                        cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.draft_theme = value;
                            this.preview_theme(cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                );
            }
            fields.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .children(buttons)
                    .into_any_element(),
            );
        }
        fields
    }

    /// 系统分类：开机自启动 / 最小化到托盘（仅关闭窗口生效） / 静默启动（Windows 系统集成）。
    fn render_settings_system(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let palette = self.palette;
        let mut fields: Vec<AnyElement> = Vec::new();
        // 三个开关默认都关（不改变旧用户行为）；托盘图标只在需要时出现。
        fields.push(section_title(
            "系统集成（Windows）",
            "保存后托盘/自启动立即生效；静默启动在下次启动生效",
            palette,
        ));
        fields.push(
            settings_toggle_row(
                "toggle-launch-at-startup",
                "开机自启动",
                "登录 Windows 后自动运行 RDM",
                self.draft_launch_at_startup,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_launch_at_startup = !this.draft_launch_at_startup;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields.push(
            settings_toggle_row(
                "toggle-minimize-to-tray",
                "最小化到托盘",
                "关闭窗口时隐藏到托盘；最小化仍留在任务栏",
                self.draft_minimize_to_tray,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_minimize_to_tray = !this.draft_minimize_to_tray;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields.push(
            settings_toggle_row(
                "toggle-silent-start",
                "静默启动",
                "启动时不显示主窗口，直接驻留托盘",
                self.draft_silent_start,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_silent_start = !this.draft_silent_start;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields
    }

    /// 删除确认弹窗（§4 功能 4）。`clear_completed` 不经过这里，保持「只移出列表」。
    fn render_delete_confirm(
        &self,
        confirm: DeleteConfirm,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let palette = self.palette;
        let item = self.items.get(confirm.index);
        let name = item
            .map(|item| item.file_name.clone())
            .unwrap_or_else(|| "（任务已不存在）".to_string());
        let state = item.map(|item| item.state);
        let hint = match state {
            Some(ItemState::Downloading) => "下载中的任务会先取消，再移出列表。",
            Some(ItemState::Completed) => "勾选后会删除已下载的文件。",
            Some(_) => "勾选后会清理未完成的 .part / .part.json 断点文件。",
            None => "",
        };

        let panel = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .w(px(440.))
            .p(px(18.))
            .bg(palette.panel)
            .border_1()
            .border_color(palette.border)
            .rounded_lg()
            .shadow_lg()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("删除任务"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .w_full()
                    .min_w(px(0.))
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .text_size(px(14.))
                            .text_color(palette.text)
                            .truncate()
                            .child(name),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_xs()
                            .text_color(palette.text_faint)
                            .child(hint),
                    ),
            )
            .child(delete_file_checkbox(
                "delete-checkbox",
                confirm.delete_file,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.toggle_delete_file(cx);
                }),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(8.))
                    .child(ghost_button(
                        "delete-cancel",
                        "取消",
                        palette,
                        cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.cancel_delete_confirm(cx);
                        }),
                    ))
                    .child(danger_button(
                        "delete-confirm",
                        "删除",
                        palette,
                        cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.confirm_delete(cx);
                        }),
                    )),
            );

        div()
            .id("delete-backdrop")
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(palette.overlay)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(panel)
    }

    /// 右侧 AI 解析 / 聊天栏：固定 360px，主列表保持 `flex_1`。
    fn render_ai_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let ai = &self.ai;
        let configured = self.ai_config.is_configured();
        let status_text = ai_status_text(ai, configured);
        let status_color = ai_status_color(ai, palette);

        let mut body: Vec<AnyElement> = Vec::new();
        body.push(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(12.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_shrink_0()
                                .px(px(6.))
                                .py(px(1.))
                                .rounded_sm()
                                .bg(Hsla::from(status_color).opacity(0.16))
                                .text_xs()
                                .text_color(status_color)
                                .child(status_text),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_xs()
                                .text_color(palette.text_faint)
                                .truncate()
                                .child(truncate_chars(&ai.source_url, 46)),
                        ),
                )
                .into_any_element(),
        );
        if !configured {
            body.push(
                div()
                    .px(px(12.))
                    .text_xs()
                    .text_color(palette.warning)
                    .child("未启用 AI：设置 → AI 配置填写 Base URL / 模型 / API Key。当前展示本地启发式结果。")
                    .into_any_element(),
            );
        }
        if let Some(error) = &ai.error {
            body.push(
                div()
                    .px(px(12.))
                    .text_xs()
                    .text_color(palette.danger)
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        for note in ai.notes.iter().take(4) {
            body.push(
                div()
                    .px(px(12.))
                    .text_xs()
                    .text_color(palette.text_faint)
                    .truncate()
                    .child(format!("· {note}"))
                    .into_any_element(),
            );
        }

        // 资源区：勾选 + 全选/全不选 + 下载选中（超过 6 个折叠提示）。
        let mut resources_section: Vec<AnyElement> = Vec::new();
        if ai.status == AiParseStatus::Parsing {
            resources_section.push(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .text_sm()
                    .text_color(palette.info)
                    .child("正在解析页面资源…")
                    .into_any_element(),
            );
        } else if !ai.resources.is_empty() {
            let selected = ai.selected_count();
            resources_section.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(palette.text_dim)
                            .child(format!("资源 {selected}/{}", ai.resources.len())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .flex_shrink_0()
                            .gap(px(4.))
                            .child(row_action_button(
                                "ai-select-all",
                                "全选",
                                palette,
                                true,
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.ai.select_all(true);
                                    cx.notify();
                                }),
                            ))
                            .child(row_action_button(
                                "ai-select-none",
                                "全不选",
                                palette,
                                true,
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.ai.select_all(false);
                                    cx.notify();
                                }),
                            ))
                            .child(row_action_button(
                                "ai-download-selected",
                                "下载选中",
                                palette,
                                selected > 0,
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.download_ai_selected(cx);
                                }),
                            )),
                    )
                    .into_any_element(),
            );
            for (index, card) in ai.resources.iter().take(6).enumerate() {
                resources_section.push(
                    self.render_ai_resource_card(index, card, palette, cx)
                        .into_any_element(),
                );
            }
            if ai.resources.len() > 6 {
                resources_section.push(
                    div()
                        .px(px(12.))
                        .text_xs()
                        .text_color(palette.text_faint)
                        .child(format!("还有 {} 个资源未显示", ai.resources.len() - 6))
                        .into_any_element(),
                );
            }
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .children(resources_section)
                    .into_any_element(),
            );
        } else if ai.status == AiParseStatus::Idle {
            body.push(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .text_xs()
                    .text_color(palette.text_faint)
                    .child("在顶栏输入 URL 后点「AI 解析」，或从浏览器扩展发送当前页面。")
                    .into_any_element(),
            );
        }

        // 聊天消息：完整历史（model 侧上限 200 条）放进可滚动容器，鼠标滚轮可回看。
        let streaming_tail = if ai.streaming
            && ai
                .messages
                .last()
                .is_some_and(|message| message.role == AiRole::Assistant)
        {
            Some(ai.messages.len() - 1)
        } else {
            None
        };
        let mut messages: Vec<AnyElement> = Vec::with_capacity(ai.messages.len() + 1);
        for (index, message) in ai.messages.iter().enumerate() {
            let is_streaming_tail = streaming_tail == Some(index);
            let color = if is_streaming_tail && message.text.is_empty() {
                palette.info
            } else {
                match message.role {
                    AiRole::System => palette.text_faint,
                    AiRole::User => palette.accent,
                    AiRole::Assistant => palette.text,
                }
            };
            let text = if is_streaming_tail {
                if message.text.is_empty() {
                    // 已经发出请求但还没收到第一段 delta。
                    "AI 正在输出…".to_string()
                } else {
                    // 轻量光标：标记这条消息还会继续追加（不做动画，避免引入定时器）。
                    format!("{}▍", message.text)
                }
            } else {
                message.text.clone()
            };
            messages.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .w_full()
                    .min_w(px(0.))
                    .px(px(12.))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(palette.text_faint)
                            .child(message.role.label_zh()),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .text_xs()
                            .text_color(color)
                            .child(text),
                    )
                    .into_any_element(),
            );
        }
        if ai.chat_pending && !ai.streaming {
            messages.push(
                div()
                    .px(px(12.))
                    .text_xs()
                    .text_color(palette.info)
                    .child("AI 正在思考…")
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .w(px(AI_PANEL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .bg(palette.panel)
            .border_l_1()
            .border_color(palette.border)
            // 面板宽度固定，长 URL/聊天内容一律在内部裁剪/换行，不横向撑破布局。
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(40.))
                    .px(px(12.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(palette.border)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("AI 解析"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.text_faint)
                                    .child(ai.status.clone().label_zh()),
                            ),
                    )
                    .child(ghost_button(
                        "ai-close",
                        "关闭",
                        palette,
                        cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.ai.visible = false;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .pt(px(8.))
                    .flex_shrink_0()
                    .children(body),
            )
            .child(
                // 可滚动历史：`flex_1 + min_h(0)` 让它吃掉剩余高度，内容超出时滚轮可回看。
                // 不强行 `justify_end`：内容高于视口时 flex-end 会让顶部历史不可达。
                div()
                    .id("ai-chat-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.ai_chat_scroll)
                    .gap(px(6.))
                    .pt(px(6.))
                    .pb(px(8.))
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, _cx| {
                        // GPUI 0.2.2 约定：`ScrollDelta` 的 y > 0 = 向上滚
                        // （Windows 后端直接把 WM_MOUSEWHEEL 的正 hiword 放进来，
                        // 见 gpui-0.2.2/src/platform/windows/events.rs）；
                        // `ScrollHandle` 的 offset.y 向下滚时才变得更负。
                        let dy = scroll_wheel_dy(event);
                        if dy > 0.0 && this.ai_chat_scroll.max_offset().height > px(0.) {
                            // 向上滚 = 用户想回看历史：内容确实可滚时停止自动跟随。
                            this.ai_autoscroll = false;
                        } else if dy < 0.0 {
                            // 向下滚 = 恢复自动跟随（简化策略：不精确判断是否到底）。
                            this.ai_autoscroll = true;
                        }
                    }))
                    .children(messages),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(10.))
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(palette.border)
                    .child(self.ai_chat_input.clone())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.text_faint)
                                    .child("Enter 发送 · 上下文含解析摘要"),
                            )
                            .child(row_action_button(
                                "ai-chat-send",
                                "发送",
                                palette,
                                !ai.chat_pending,
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.send_ai_chat(cx);
                                }),
                            )),
                    ),
            )
    }

    fn render_ai_resource_card(
        &self,
        index: usize,
        card: &crate::model::AiResourceCard,
        palette: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let resource = &card.resource;
        let kind = resource_kind_label(&resource.kind);
        let size = resource
            .size_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "大小未知".to_string());
        let card_id = ElementId::named_usize("ai-card", index);
        let mut row = div()
            .id(card_id)
            .flex()
            .flex_col()
            .gap(px(1.))
            .mx(px(8.))
            .px(px(8.))
            .py(px(5.))
            .rounded_sm()
            .bg(palette.panel_alt)
            .border_1()
            .border_color(if card.selected {
                palette.accent
            } else {
                palette.border
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.ai.toggle_resource(index);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(checkbox_mark(card.selected, palette))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .text_color(palette.text)
                            .child(if resource.name.is_empty() {
                                truncate_chars(&resource.url, 40)
                            } else {
                                resource.name.clone()
                            }),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .text_color(palette.text_faint)
                            .child(format!("{kind} · {size}")),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .pl(px(20.))
                    .truncate()
                    .text_size(px(10.))
                    .text_color(palette.text_faint)
                    .child(truncate_chars(&resource.url, 52)),
            );
        if !card.selected {
            row = row.opacity(0.78);
        }
        row
    }
}

impl Render for RdmApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let font = self.font.clone();
        // AI 栏固定 360px；窄窗口（最小 900px）自动收起过滤栏，保证主列表仍放得下完整行布局。
        let viewport_width: f32 = window.viewport_size().width.into();
        let ai_visible = self.ai.visible;
        let show_sidebar = sidebar_visible(viewport_width, ai_visible);
        self.compact_rows = rows_compact(viewport_width, ai_visible);
        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(palette.bg)
            .text_color(palette.text)
            .font(font)
            .on_action(cx.listener(Self::close_dialog))
            .child(self.render_topbar(cx))
            // 浏览器集成不可用时在顶栏下方显示一条细警告条（仅 Some 时渲染）。
            .children(
                self.ipc_warning
                    .as_ref()
                    .map(|warning| render_ipc_warning(warning, palette)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .children(show_sidebar.then(|| self.render_sidebar(cx)))
                    .child(self.render_list(cx))
                    .children(ai_visible.then(|| self.render_ai_panel(cx))),
            )
            .child(self.render_statusbar());
        if self.show_settings {
            root = root.child(self.render_settings(cx));
        }
        if let Some(confirm) = self.delete_confirm {
            root = root.child(self.render_delete_confirm(confirm, cx));
        }
        root
    }
}

// ---------------------------------------------------------------------------
// 事件泵
// ---------------------------------------------------------------------------

/// GPUI foreground task：把 tokio 侧消息合并成批，交给主线程更新 Entity。
async fn pump_loop(
    this: WeakEntity<RdmApp>,
    cx: &mut gpui::AsyncApp,
    mut rx: UnboundedReceiver<EngineMsg>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::with_capacity(16);
        batch.push(first);
        // 合并突发事件（一次引擎 reporter tick 可能带来多条），每批最多 1024 条。
        while batch.len() < 1024 {
            match rx.try_recv() {
                Ok(message) => batch.push(message),
                Err(_) => break,
            }
        }
        if this
            .update(cx, |app, cx| app.apply_batch(batch, cx))
            .is_err()
        {
            break; // 窗口/App 已销毁
        }
    }
}

// ---------------------------------------------------------------------------
// 响应式布局判定（纯函数，可单测）
// ---------------------------------------------------------------------------

/// 主列表低于该宽度时切换紧凑行（隐藏大小/ETA/速度曲线，保留连接信息）。
const LIST_MIN_WITH_SIDEBAR: f32 = 560.0;

/// AI 栏打开且窗口较窄时收起左侧过滤栏，把宽度让给主列表。
fn sidebar_visible(viewport_width: f32, ai_visible: bool) -> bool {
    !ai_visible || viewport_width - AI_PANEL_WIDTH >= SIDEBAR_MIN_ROOM
}

/// 主列表实际可用宽度不足时使用紧凑行布局。
fn rows_compact(viewport_width: f32, ai_visible: bool) -> bool {
    let list_width = viewport_width
        - if ai_visible { AI_PANEL_WIDTH } else { 0.0 }
        - if sidebar_visible(viewport_width, ai_visible) {
            SIDEBAR_WIDTH
        } else {
            0.0
        };
    list_width < LIST_MIN_WITH_SIDEBAR
}

// ---------------------------------------------------------------------------
// AI / 删除 / 主题的纯逻辑（可单测，不依赖 gpui）
// ---------------------------------------------------------------------------

/// AI 聊天 system prompt：限定在「根据解析结果回答下载相关问题」。
const AI_CHAT_SYSTEM_PROMPT: &str = "你是 RDM（Rust Download Manager）桌面端的下载助手。\
用户刚解析了一个网页，右侧面板里有候选下载资源。请用简洁的中文回答：哪些资源值得下载、\
如何按文件名/类型/大小选择、可能的坑（登录、Referer、HLS 分片等）。\
如果用户想下载，提示勾选资源后点「下载选中」。不要编造解析结果中不存在的资源，也不要输出 JSON。";

/// 主题按钮的短标签（`Theme::label_zh` 的 Catppuccin 名字在 6 个按钮里放不下）。
fn theme_short_label(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "深色",
        Theme::Light => "浅色",
        Theme::CatppuccinLatte => "Latte",
        Theme::CatppuccinFrappe => "Frappe",
        Theme::CatppuccinMacchiato => "Macchiato",
        Theme::CatppuccinMocha => "Mocha",
    }
}

/// 稳定的 ElementId（`theme_button` 要求 `&'static str`）。
fn theme_button_id(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "theme-dark",
        Theme::Light => "theme-light",
        Theme::CatppuccinLatte => "theme-latte",
        Theme::CatppuccinFrappe => "theme-frappe",
        Theme::CatppuccinMacchiato => "theme-macchiato",
        Theme::CatppuccinMocha => "theme-mocha",
    }
}

/// 资源类型 → 中文短标签（与 `rdm-ai` 的 kind 取值对齐）。
fn resource_kind_label(kind: &str) -> &'static str {
    match kind {
        "video" => "视频",
        "audio" => "音频",
        "image" => "图片",
        "archive" => "压缩包",
        "document" => "文档",
        "program" => "程序",
        _ => "其他",
    }
}

/// 截断到最多 `max_chars` 个字符（按字符而不是字节，避免切断中文/emoji）。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// AI 栏状态文案：解析中 / 命中缓存 / AI 已解析 / 启发式 / 失败。
fn ai_status_text(ai: &AiPanelState, configured: bool) -> String {
    match ai.status {
        AiParseStatus::Idle => "未解析".to_string(),
        AiParseStatus::Parsing => "解析中…".to_string(),
        AiParseStatus::Failed => "解析失败".to_string(),
        AiParseStatus::Ready => {
            if ai.cache_hit {
                "命中缓存".to_string()
            } else if ai.ai_used {
                match &ai.model {
                    Some(model) => format!("AI 已解析 · {model}"),
                    None => "AI 已解析".to_string(),
                }
            } else if configured {
                "启发式解析".to_string()
            } else {
                "本地启发式（AI 未配置）".to_string()
            }
        }
    }
}

fn ai_status_color(ai: &AiPanelState, palette: Palette) -> Rgba {
    match ai.status {
        AiParseStatus::Idle => palette.text_faint,
        AiParseStatus::Parsing => palette.info,
        AiParseStatus::Failed => palette.danger,
        AiParseStatus::Ready if ai.cache_hit => palette.info,
        AiParseStatus::Ready => palette.success,
    }
}

/// 把解析结果压成聊天上下文（资源清单 + notes），供 `start_ai_chat_stream` 的 prompt 使用。
fn build_ai_parse_summary(ai: &AiPanelState) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "来源 URL: {}", ai.source_url);
    let _ = writeln!(out, "解析状态: {}", ai.status.clone().label_zh());
    if ai.cache_hit {
        out.push_str("缓存: 命中解析缓存\n");
    }
    if let Some(model) = &ai.model {
        let _ = writeln!(out, "模型: {model}");
    }
    let _ = writeln!(
        out,
        "AI 参与: {}",
        if ai.ai_used {
            "是"
        } else {
            "否（本地启发式）"
        }
    );
    if let Some(error) = &ai.error {
        let _ = writeln!(out, "错误: {error}");
    }
    if !ai.notes.is_empty() {
        out.push_str("解析事项:\n");
        for note in ai.notes.iter().take(8) {
            let _ = writeln!(out, "- {note}");
        }
    }
    let _ = writeln!(
        out,
        "资源（共 {} 个，已勾选 {} 个）:",
        ai.resources.len(),
        ai.selected_count()
    );
    for (index, card) in ai.resources.iter().take(20).enumerate() {
        let resource = &card.resource;
        let size = resource
            .size_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "大小未知".to_string());
        let _ = writeln!(
            out,
            "{}. [{}] {} | {} | {} | {}",
            index + 1,
            if card.selected {
                "已勾选"
            } else {
                "未勾选"
            },
            resource.name,
            resource_kind_label(&resource.kind),
            size,
            resource.url
        );
    }
    out
}

fn build_ai_chat_prompt(user_text: &str, ai: &AiPanelState) -> String {
    format!(
        "【解析结果摘要】\n{}\n【用户问题】\n{}",
        build_ai_parse_summary(ai),
        user_text
    )
}

/// 把滚轮事件统一成 y 方向标量：正值 = 向上滚，负值 = 向下滚。
///
/// GPUI 0.2.2 的 `ScrollWheelEvent.delta` 有「行」和「像素」两种编码：Windows
/// 鼠标滚轮走 `Lines`（`gpui/src/platform/windows/events.rs::handle_mouse_wheel_msg`
/// 里 `WM_MOUSEWHEEL` 的 `signed_hiword()/WHEEL_DELTA * lines`），触控板走
/// `Pixels`，两者符号约定一致（向前/向上滚为正）。
fn scroll_wheel_dy(event: &ScrollWheelEvent) -> f32 {
    match event.delta {
        ScrollDelta::Pixels(delta) => delta.y.into(),
        ScrollDelta::Lines(delta) => delta.y,
    }
}

fn append_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// 删除任务时需要清理的磁盘路径（去重）：
/// - 已完成任务的落盘文件；
/// - 目标文件 + 引擎断点文件 `*.part` / `*.part.json` / `*.part.json.tmp`。
fn delete_targets(item: &DownloadItem, state_dir: &std::path::Path) -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = Vec::new();
    if let Some(path) = &item.path {
        targets.push(path.clone());
    }
    // 分类保存目录优先；其次请求里的输出目录；再其次设置默认目录；最后退化为 state_dir
    // （引擎实际会落到进程当前目录，但 GUI 侧拿不到更精确的位置）。
    let dir = item
        .save_dir
        .clone()
        .or_else(|| item.request.output_dir.clone())
        .or_else(model::default_download_dir)
        .unwrap_or_else(|| state_dir.to_path_buf());
    targets.push(dir.join(&item.file_name));

    let mut expanded: Vec<PathBuf> = Vec::new();
    for target in targets {
        if expanded.contains(&target) {
            continue;
        }
        expanded.push(target.clone());
        for suffix in [".part", ".part.json", ".part.json.tmp"] {
            let part = append_suffix(&target, suffix);
            if !expanded.contains(&part) {
                expanded.push(part);
            }
        }
    }
    expanded
}

/// 删除已存在的文件；返回（删除数量, 错误列表）。不存在的文件不算错误。
fn delete_target_files(targets: &[PathBuf]) -> (usize, Vec<String>) {
    let mut removed = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for path in targets {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => errors.push(format!("{}: {err}", path.display())),
        }
    }
    (removed, errors)
}

// ---------------------------------------------------------------------------
// 小部件
// ---------------------------------------------------------------------------

/// 顶栏下方的浏览器集成警告条：`main.rs` 写 `ipc.json` 失败时才有内容。
///
/// 单行截断：完整错误（路径 + 原因）已经写进日志，界面只保留一行可读提示，
/// 避免把顶栏高度撑开；鼠标悬停不是必须，`README §7` 里有完整排查步骤。
fn render_ipc_warning(warning: &str, palette: Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .flex_shrink_0()
        .h(px(26.))
        .px(px(16.))
        .bg(Hsla::from(palette.warning).opacity(0.16))
        .border_b_1()
        .border_color(palette.warning)
        .text_xs()
        .text_color(palette.warning)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .child(warning.to_string()),
        )
}

fn ui_font() -> Font {
    let mut font = gpui::font("Segoe UI");
    // DirectWrite 的系统 fallback 需要显式传给 TextLayout（见 docs/research/gpui.md §4.2）。
    font.fallbacks = Some(FontFallbacks::from_fonts(vec![
        "Microsoft YaHei UI".to_string(),
        "Segoe UI Symbol".to_string(),
        "Segoe UI Emoji".to_string(),
    ]));
    font
}

/// 把一条系统集成警告追加到汇总文案（多条用「；」连接）。
fn push_warning(target: &mut Option<String>, text: String) {
    *target = Some(match target.take() {
        Some(old) => format!("{old}；{text}"),
        None => text,
    });
}

fn primary_button(
    id: &'static str,
    label: &'static str,
    palette: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(36.))
        .px(px(18.))
        .flex_shrink_0()
        .rounded_md()
        .bg(palette.accent)
        .text_color(palette.on_accent)
        .text_size(px(14.))
        .cursor_pointer()
        .hover(move |style| style.bg(palette.accent_hover))
        .on_click(on_click)
        .child(label)
}

fn danger_button(
    id: &'static str,
    label: &'static str,
    palette: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(36.))
        .px(px(18.))
        .flex_shrink_0()
        .rounded_md()
        .bg(palette.danger)
        .text_color(palette.on_accent)
        .text_size(px(14.))
        .cursor_pointer()
        .hover(move |style| style.bg(palette.accent))
        .on_click(on_click)
        .child(label)
}

/// 复选框方框（勾选时用强调色 + `on_accent` 对勾）。
fn checkbox_mark(checked: bool, palette: Palette) -> impl IntoElement {
    let mut mark = div()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(16.))
        .rounded_sm()
        .border_1()
        .border_color(if checked {
            palette.accent
        } else {
            palette.border
        })
        .bg(if checked {
            palette.accent
        } else {
            palette.panel
        })
        .text_size(px(11.))
        .text_color(palette.on_accent);
    if checked {
        mark = mark.child("✓");
    }
    mark
}

fn delete_file_checkbox(
    id: &'static str,
    checked: bool,
    palette: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .on_click(on_click)
        .child(checkbox_mark(checked, palette))
        .child(
            div()
                .text_size(px(13.))
                .text_color(palette.text)
                .child("同时删除已下载的文件"),
        )
}

fn labeled_input(label: &'static str, input: Entity<TextInput>, palette: Palette) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .w_full()
        .min_w(px(0.))
        .child(settings_label(label, palette))
        // min_w(0) 让输入框容器可以收缩，长路径/URL 由 TextInput 内部横向滚动裁剪。
        .child(div().flex_1().min_w(px(0.)).child(input))
        .into_any_element()
}

fn section_title(title: &'static str, hint: &'static str, palette: Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .w_full()
        .min_w(px(0.))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(palette.accent)
                .child(title),
        )
        .child(
            div()
                .w_full()
                .text_xs()
                .text_color(palette.text_faint)
                .child(hint),
        )
        .into_any_element()
}

fn ghost_button(
    id: &'static str,
    label: &'static str,
    palette: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(36.))
        .px(px(14.))
        .flex_shrink_0()
        .rounded_md()
        .border_1()
        .border_color(palette.border)
        .bg(palette.panel_alt)
        .text_color(palette.text_dim)
        .text_size(px(13.))
        .cursor_pointer()
        .hover(move |style| {
            style
                .bg(palette.panel)
                .border_color(palette.accent)
                .text_color(palette.accent)
        })
        .on_click(on_click)
        .child(label)
}

fn row_action_button(
    id: impl Into<ElementId>,
    label: &'static str,
    palette: Palette,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut button = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(22.))
        .px(px(9.))
        .rounded_sm()
        .border_1()
        .border_color(palette.border)
        .bg(palette.panel_alt)
        .text_xs()
        .text_color(if enabled {
            palette.text_dim
        } else {
            palette.text_faint
        });
    if enabled {
        button = button
            .cursor_pointer()
            .hover(move |style| {
                style
                    .bg(palette.panel)
                    .border_color(palette.accent)
                    .text_color(palette.accent)
            })
            .on_click(on_click);
    }
    button.child(label).into_any_element()
}

fn theme_button(
    id: &'static str,
    value: Theme,
    current: Theme,
    palette: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let active = value == current;
    let mut button = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(28.))
        .w(px(92.))
        .rounded_sm()
        .border_1()
        .border_color(if active {
            palette.accent
        } else {
            palette.border
        })
        .bg(if active {
            palette.accent_soft
        } else {
            palette.panel_alt
        })
        .text_color(if active {
            palette.accent
        } else {
            palette.text_dim
        })
        .text_xs()
        .cursor_pointer()
        .on_click(on_click);
    if !active {
        button = button.hover(move |style| style.border_color(palette.accent));
    }
    button.child(theme_short_label(value))
}

fn settings_label(text: &'static str, palette: Palette) -> impl IntoElement {
    div()
        .w(px(110.))
        .flex_shrink_0()
        .text_size(px(13.))
        .text_color(palette.text_dim)
        .child(text)
}

fn settings_toggle_row(
    id: &'static str,
    title: &'static str,
    hint: &'static str,
    value: bool,
    palette: Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let knob_bg = if value {
        palette.success
    } else {
        palette.text_faint
    };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .w_full()
        .min_w(px(0.))
        .cursor_pointer()
        .on_click(on_click)
        .child(settings_label(title, palette))
        // 说明文字可收缩并省略，避免长文案把列宽撑破（截图 bug 根因之一）。
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_xs()
                .text_color(palette.text_faint)
                .child(hint),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .child(
                    div()
                        .w(px(38.))
                        .h(px(20.))
                        .rounded_lg()
                        .bg(if value {
                            Hsla::from(palette.success).opacity(0.28)
                        } else {
                            Hsla::from(palette.track)
                        })
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .ml(if value { px(20.) } else { px(2.) })
                                .size(px(16.))
                                .rounded_lg()
                                .bg(knob_bg),
                        ),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AiResourceCard, DownloadItem};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rdm-gui-app-{tag}-{}-{}",
            std::process::id(),
            crate::model::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_tabs_cover_all_categories_in_order() {
        // 用户诉求「设置界面分类，不要堆一页」：固定 5 个分类、默认「下载」。
        assert_eq!(SettingsTab::ALL.len(), 5, "设置弹窗必须恰好有 5 个分类");
        assert_eq!(
            SettingsTab::default(),
            SettingsTab::Download,
            "默认分类必须是「下载」"
        );
        assert_eq!(
            SettingsTab::ALL[0],
            SettingsTab::Download,
            "导航第一项是「下载」"
        );
        let labels: Vec<&str> = SettingsTab::ALL.iter().map(|tab| tab.label_zh()).collect();
        assert_eq!(labels, vec!["下载", "分类与校验", "AI", "外观", "系统"]);
        for tab in SettingsTab::ALL {
            assert!(!tab.label_zh().is_empty(), "{tab:?} 的分类名不能为空");
        }
        let ids: std::collections::HashSet<&str> = SettingsTab::ALL
            .iter()
            .map(|tab| tab.element_id())
            .collect();
        assert_eq!(ids.len(), 5, "分类导航的元素 id 必须唯一");
        for id in ids {
            assert!(id.starts_with("settings-tab-"), "分类 id 约定前缀: {id}");
        }
    }

    #[test]
    fn theme_short_labels_and_ids_cover_all_variants() {
        let labels: Vec<&str> = Theme::ALL
            .iter()
            .map(|theme| theme_short_label(*theme))
            .collect();
        assert_eq!(
            labels,
            vec!["深色", "浅色", "Latte", "Frappe", "Macchiato", "Mocha"]
        );
        let ids: std::collections::HashSet<&str> = Theme::ALL
            .iter()
            .map(|theme| theme_button_id(*theme))
            .collect();
        assert_eq!(ids.len(), Theme::ALL.len(), "每个主题按钮 id 必须唯一");
    }

    #[test]
    fn responsive_layout_decisions() {
        // 最小窗口 900px + AI 栏：收起过滤栏（主列表 540），行进入紧凑布局。
        assert!(!sidebar_visible(900.0, true));
        assert!(rows_compact(900.0, true));
        // 900px 且 AI 栏关闭：过滤栏保留，行布局完整（列表 690）。
        assert!(sidebar_visible(900.0, false));
        assert!(!rows_compact(900.0, false));
        // 默认窗口 1180px + AI 栏：过滤栏保留（列表 610），行不必紧凑。
        assert!(sidebar_visible(1180.0, true));
        assert!(!rows_compact(1180.0, true));
        // 1000px 这类中间宽度 + AI 栏：收起过滤栏后列表 640，行不必紧凑。
        assert!(!sidebar_visible(1000.0, true));
        assert!(!rows_compact(1000.0, true));
    }

    #[test]
    fn resource_kind_labels_are_chinese() {
        assert_eq!(resource_kind_label("video"), "视频");
        assert_eq!(resource_kind_label("archive"), "压缩包");
        assert_eq!(resource_kind_label("unknown-kind"), "其他");
    }

    #[test]
    fn truncate_chars_is_char_aware() {
        assert_eq!(truncate_chars("abcdef", 10), "abcdef");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("中文资源名称", 3), "中文…");
        assert_eq!(truncate_chars("", 3), "");
    }

    fn item_with_dir(tag: &str, file_name: &str) -> (DownloadItem, PathBuf) {
        let dir = temp_dir(tag);
        let mut request = DownloadRequest::new("https://example.com/a.iso");
        request.output_dir = Some(dir.clone());
        request.file_name = Some(file_name.to_string());
        let item = DownloadItem::new("dl_000001", request, "test");
        (item, dir)
    }

    #[test]
    fn delete_targets_include_part_files_and_dedup() {
        let (item, dir) = item_with_dir("targets", "a.iso");
        let targets = delete_targets(&item, std::path::Path::new(r"C:\state"));
        assert!(targets.contains(&dir.join("a.iso")));
        assert!(targets.contains(&dir.join("a.iso.part")));
        assert!(targets.contains(&dir.join("a.iso.part.json")));
        assert!(targets.contains(&dir.join("a.iso.part.json.tmp")));
        let mut dedup = targets.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), targets.len(), "删除目标必须去重");
    }

    #[test]
    fn delete_targets_prefer_completed_path_and_save_dir() {
        let (mut item, dir) = item_with_dir("targets-save-dir", "b.mkv");
        let save_dir = dir.join("视频");
        let completed = save_dir.join("b.mkv");
        item.save_dir = Some(save_dir.clone());
        item.path = Some(completed.clone());
        item.state = ItemState::Completed;
        let targets = delete_targets(&item, std::path::Path::new(r"C:\state"));
        assert_eq!(targets[0], completed);
        assert!(targets.contains(&save_dir.join("b.mkv.part")));
    }

    #[test]
    fn delete_target_files_removes_existing_and_ignores_missing() {
        let dir = temp_dir("delete-files");
        let target = dir.join("file.bin");
        let part = dir.join("file.bin.part");
        std::fs::write(&target, b"x").unwrap();
        std::fs::write(&part, b"y").unwrap();
        let missing = dir.join("missing.bin");
        let targets = vec![target.clone(), part.clone(), missing];
        let (removed, errors) = delete_target_files(&targets);
        assert_eq!(removed, 2);
        assert!(errors.is_empty(), "不存在文件不应报错: {errors:?}");
        assert!(!target.exists());
        assert!(!part.exists());
        // 再删一次：全部 NotFound，不报错也不计数。
        let (again, errors) = delete_target_files(&targets);
        assert_eq!(again, 0);
        assert!(errors.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_confirm_defaults_only_for_completed_with_path() {
        let request = DownloadRequest::new("https://example.com/a.bin");
        let mut item = DownloadItem::new("dl_1", request, "test");
        assert!(!DeleteConfirm::default_delete_file(&item));
        item.state = ItemState::Completed;
        assert!(
            !DeleteConfirm::default_delete_file(&item),
            "没有 path 不默认勾选"
        );
        item.path = Some(PathBuf::from("C:/downloads/a.bin"));
        assert!(DeleteConfirm::default_delete_file(&item));
    }

    #[test]
    fn scroll_wheel_dy_reads_up_as_positive() {
        // Windows 鼠标滚轮向上 = 正的行 delta；触控板向上的像素 delta 同样为正。
        let line_up = ScrollWheelEvent {
            delta: ScrollDelta::Lines(point(0.0, 1.0)),
            ..Default::default()
        };
        let line_down = ScrollWheelEvent {
            delta: ScrollDelta::Lines(point(0.0, -3.0)),
            ..Default::default()
        };
        let pixel_up = ScrollWheelEvent {
            delta: ScrollDelta::Pixels(point(px(0.), px(12.))),
            ..Default::default()
        };
        assert!(scroll_wheel_dy(&line_up) > 0.0);
        assert!(scroll_wheel_dy(&line_down) < 0.0);
        assert!(scroll_wheel_dy(&pixel_up) > 0.0);
        assert_eq!(scroll_wheel_dy(&ScrollWheelEvent::default()), 0.0);
    }

    fn ready_ai_panel() -> AiPanelState {
        let mut ai = AiPanelState {
            visible: true,
            status: AiParseStatus::Ready,
            source_url: "https://example.com/page".to_string(),
            ai_used: true,
            model: Some("gpt-test".to_string()),
            ..Default::default()
        };
        ai.notes.push("该站需要 Referer".to_string());
        ai.resources.push(AiResourceCard {
            resource: rdm_ai::ParsedResource {
                url: "https://example.com/v.mp4".to_string(),
                name: "v.mp4".to_string(),
                kind: "video".to_string(),
                size_bytes: Some(2048),
                content_type: Some("video/mp4".to_string()),
                note: None,
            },
            selected: true,
        });
        ai
    }

    #[test]
    fn chat_prompt_contains_parse_summary_and_question() {
        let ai = ready_ai_panel();
        let prompt = build_ai_chat_prompt("下载哪个？", &ai);
        assert!(prompt.contains("下载哪个？"));
        assert!(prompt.contains("https://example.com/page"));
        assert!(prompt.contains("v.mp4"));
        assert!(prompt.contains("视频"));
        assert!(prompt.contains("gpt-test"));
        assert!(prompt.contains("该站需要 Referer"));
    }

    #[test]
    fn ai_status_text_covers_all_states() {
        let mut ai = AiPanelState::default();
        assert_eq!(ai_status_text(&ai, false), "未解析");
        ai.status = AiParseStatus::Parsing;
        assert_eq!(ai_status_text(&ai, false), "解析中…");
        ai.status = AiParseStatus::Failed;
        assert_eq!(ai_status_text(&ai, false), "解析失败");
        ai.status = AiParseStatus::Ready;
        ai.cache_hit = true;
        assert_eq!(ai_status_text(&ai, true), "命中缓存");
        ai.cache_hit = false;
        ai.ai_used = true;
        ai.model = Some("m".to_string());
        assert_eq!(ai_status_text(&ai, true), "AI 已解析 · m");
        ai.ai_used = false;
        ai.model = None;
        assert_eq!(ai_status_text(&ai, true), "启发式解析");
        assert_eq!(ai_status_text(&ai, false), "本地启发式（AI 未配置）");
    }
}
