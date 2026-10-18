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
    canvas, div, fill, point, px, rgb, size, uniform_list, AnyElement, App, Bounds, ClickEvent,
    Context, ElementId, Entity, Focusable, Font, FontFallbacks, Hsla, IntoElement, MouseButton,
    Render, UniformListScrollHandle, WeakEntity, Window,
};
use rdm_core::{DownloadHandle, DownloadRequest};
use rdm_ipc::{Envelope, Request, Response};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::engine::{EngineMsg, EngineRuntime};
use crate::ipc_server;
use crate::model::{
    self, active_count, filter_count, filtered_indices, format_limit, format_speed,
    global_speed_bps, next_download_id, validate_url, DownloadItem, Filter, ItemState, Settings,
    StopRequest, Theme,
};
use crate::open_path;
use crate::text_input::TextInput;
use crate::theme::Palette;

gpui::actions!(rdm_app, [CloseDialog]);

/// 主列表行高（uniform_list 要求等高）。
const ROW_HEIGHT: f32 = 88.0;
/// 全局速度曲线采样数。
const GLOBAL_HISTORY_LEN: usize = 60;

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
}

struct Toast {
    text: String,
    is_error: bool,
    created: Instant,
}

const TOAST_TTL: Duration = Duration::from_secs(5);

pub struct RdmApp {
    items: Vec<DownloadItem>,
    settings: Settings,
    filter: Filter,
    palette: Palette,
    font: Font,

    url_input: Entity<TextInput>,
    dir_input: Entity<TextInput>,
    limit_input: Entity<TextInput>,
    scroll_handle: UniformListScrollHandle,

    show_settings: bool,
    draft_connections: usize,
    draft_verify_sha256: bool,
    draft_theme: Theme,
    draft_auto_start: bool,

    engine: EngineRuntime,
    handles: HashMap<String, (u64, DownloadHandle)>,
    state_dir: PathBuf,
    next_seq: u64,
    speed_history: std::collections::VecDeque<f64>,

    dirty_persist: bool,
    last_save: Instant,
    toast: Option<Toast>,
}

impl RdmApp {
    pub fn new(
        cx: &mut Context<Self>,
        url_input: Entity<TextInput>,
        dir_input: Entity<TextInput>,
        limit_input: Entity<TextInput>,
        bootstrap: Bootstrap,
    ) -> Self {
        let Bootstrap {
            settings,
            items,
            next_seq,
            state_dir,
            engine,
            ui_rx,
            initial,
        } = bootstrap;
        let palette = Palette::for_theme(settings.theme);
        let font = ui_font();

        // 设置弹窗里的两个输入框先填上当前设置（弹窗打开时再刷新一次）。
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

        let mut app = Self {
            items,
            draft_connections: settings.default_connections,
            draft_verify_sha256: settings.verify_sha256,
            draft_theme: settings.theme,
            draft_auto_start: settings.auto_start,
            settings,
            filter: Filter::All,
            palette,
            font,
            url_input,
            dir_input,
            limit_input,
            scroll_handle: UniformListScrollHandle::new(),
            show_settings: false,
            engine,
            handles: HashMap::new(),
            state_dir,
            next_seq,
            speed_history: std::collections::VecDeque::with_capacity(GLOBAL_HISTORY_LEN),
            dirty_persist: false,
            last_save: Instant::now(),
            toast: None,
        };

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

        // GUI 主线程事件泵：tokio → GPUI。`rx` 是 tokio 的 UnboundedReceiver，
        // `recv().await` 的 waker 由 GPUI foreground dispatcher 派发，不碰 Entity。
        cx.spawn(async move |this, cx| {
            pump_loop(this, cx, ui_rx).await;
        })
        .detach();

        app
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
        let request = self.settings.build_request(url);
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
        self.draft_connections = self.settings.default_connections;
        self.draft_verify_sha256 = self.settings.verify_sha256;
        self.draft_theme = self.settings.theme;
        self.draft_auto_start = self.settings.auto_start;

        let dir_text = self
            .settings
            .default_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let limit_text = self.settings.speed_limit_mb_text();
        self.dir_input
            .update(cx, |input, cx| input.set_text(dir_text, cx));
        self.limit_input
            .update(cx, |input, cx| input.set_text(limit_text, cx));

        let handle = self.dir_input.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn cancel_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = false;
        let handle = self.url_input.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir_text = self.dir_input.read(cx).text().trim().to_string();
        let limit_text = self.limit_input.read(cx).text().to_owned();
        let speed_limit = match model::parse_speed_limit_mb(&limit_text) {
            Ok(value) => value,
            Err(err) => {
                self.set_toast(err, true);
                cx.notify();
                return;
            }
        };
        let mut settings = self.settings.clone();
        settings.default_connections = self.draft_connections.clamp(1, rdm_core::MAX_CONNECTIONS);
        settings.default_dir = if dir_text.is_empty() {
            None
        } else {
            Some(PathBuf::from(dir_text))
        };
        settings.speed_limit_bps = speed_limit;
        settings.verify_sha256 = self.draft_verify_sha256;
        settings.theme = self.draft_theme;
        settings.auto_start = self.draft_auto_start;
        self.settings = settings.normalized();

        if let Err(err) = model::save_settings(&self.state_dir, &self.settings) {
            tracing::warn!(error = %err, "保存 settings.json 失败");
            self.set_toast(format!("保存设置失败: {err}"), true);
        } else {
            self.set_toast("设置已保存", false);
        }
        self.apply_theme(cx);
        self.show_settings = false;
        let handle = self.url_input.read(cx).focus_handle(cx);
        window.focus(&handle);
        cx.notify();
    }

    fn apply_theme(&mut self, cx: &mut Context<Self>) {
        self.palette = Palette::for_theme(self.settings.theme);
        let palette = self.palette;
        self.url_input
            .update(cx, |input, _| input.set_palette(palette));
        self.dir_input
            .update(cx, |input, _| input.set_palette(palette));
        self.limit_input
            .update(cx, |input, _| input.set_palette(palette));
    }

    fn close_dialog(&mut self, _: &CloseDialog, window: &mut Window, cx: &mut Context<Self>) {
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

    fn remove_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.index_of(id) {
            self.remove_item(index);
            cx.notify();
        }
    }

    fn open_folder_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let item = &self.items[index];
        let target = open_path::resolve_folder_target(
            item.path.as_deref(),
            item.request.output_dir.as_deref(),
            &self.state_dir,
        );
        if let Err(err) = open_path::open_folder(&target) {
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
        let item = &mut self.items[index];
        item.run_id += 1;
        let run_id = item.run_id;
        let id = item.id.clone();
        let request = item.request.clone();
        item.state = ItemState::Downloading;
        item.stop_request = StopRequest::None;
        item.error = None;
        item.note = Some("排队中…".to_string());
        item.speed_history.clear();
        item.clear_speed();
        let handle = DownloadHandle::new();
        self.handles.insert(id.clone(), (run_id, handle.clone()));
        self.engine.start_download(id, run_id, request, handle);
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

    fn set_toast(&mut self, text: impl Into<String>, is_error: bool) {
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
            Request::Activate => {
                self.activate_window(cx);
                Response::ok(request_id, serde_json::json!({ "activated": true }))
            }
        }
    }

    fn activate_window(&self, cx: &mut Context<Self>) {
        if let Some(handle) = cx.active_window() {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
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
            .w(px(210.))
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
                    let mut rows = Vec::with_capacity(range.len());
                    for index in range {
                        if let Some(item_index) = indices.get(index).copied() {
                            if let Some(item) = this.items.get(item_index) {
                                rows.push(this.render_row(index, item, cx).into_any_element());
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

    fn render_row(&self, index: usize, item: &DownloadItem, cx: &Context<Self>) -> AnyElement {
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

        // 第二行说明：错误 / 重试 / 路径（开启 SHA-256 后展示引擎返回的校验值）。
        let detail_text = if let Some(error) = &item.error {
            error.clone()
        } else if let Some(note) = &item.note {
            note.clone()
        } else if state == ItemState::Completed {
            if self.settings.verify_sha256 {
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
            }
        } else {
            item.url.clone()
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
                    this.remove_by_id(&target, cx);
                }),
            ));
        }

        let history: Vec<f64> = item.speed_history.iter().copied().collect();
        let sparkline_color = progress_color;
        let sparkline = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                if history.is_empty() {
                    return;
                }
                let max = history.iter().copied().fold(0.0_f64, f64::max).max(1.0);
                let count = history.len() as f32;
                let gap = 1.0_f32;
                let width: f32 = bounds.size.width.into();
                let height: f32 = bounds.size.height.into();
                let origin_x: f32 = bounds.origin.x.into();
                let origin_y: f32 = bounds.origin.y.into();
                let bar_width = ((width - gap * (count - 1.0)) / count).max(1.0);
                for (i, value) in history.iter().enumerate() {
                    let bar_height = (height * (*value as f32 / max as f32)).max(1.0);
                    let x = origin_x + i as f32 * (bar_width + gap);
                    let y = origin_y + height - bar_height;
                    window.paint_quad(fill(
                        Bounds::new(point(px(x), px(y)), size(px(bar_width), px(bar_height))),
                        sparkline_color,
                    ));
                }
            },
        )
        .w(px(88.))
        .h(px(18.))
        .into_any_element();

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
            // 第三行：进度条 + 数值 + 速度曲线
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .h(px(20.))
                    .child(
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
                            ),
                    )
                    .child(
                        div()
                            .w(px(46.))
                            .text_xs()
                            .text_color(palette.text_dim)
                            .child(item.percent_text()),
                    )
                    .child(
                        div()
                            .w(px(88.))
                            .text_xs()
                            .text_color(palette.text_dim)
                            .child(item.size_text()),
                    )
                    .child(
                        div()
                            .w(px(88.))
                            .text_xs()
                            .text_color(if state.is_active() {
                                palette.text
                            } else {
                                palette.text_dim
                            })
                            .child(item.speed_text()),
                    )
                    .child(
                        div()
                            .w(px(58.))
                            .text_xs()
                            .text_color(palette.text_dim)
                            .child(format!("ETA {}", item.eta_text())),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .text_xs()
                            .text_color(palette.text_faint)
                            .child(item.connections_text()),
                    )
                    .child(sparkline),
            )
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

    fn render_settings(&self, cx: &Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let draft = self.draft_connections;
        let theme = self.draft_theme;
        let sha = self.draft_verify_sha256;
        let auto = self.draft_auto_start;

        let mut fields: Vec<AnyElement> = Vec::new();
        fields.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.))
                .child(settings_label("默认连接数", palette))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(row_action_button(
                            "conn-dec",
                            "−",
                            palette,
                            draft > 1,
                            cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.draft_connections =
                                    this.draft_connections.saturating_sub(1).max(1);
                                cx.notify();
                            }),
                        ))
                        .child(
                            div()
                                .w(px(44.))
                                .text_center()
                                .text_sm()
                                .child(format!("{draft}")),
                        )
                        .child(row_action_button(
                            "conn-inc",
                            "+",
                            palette,
                            draft < rdm_core::MAX_CONNECTIONS,
                            cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.draft_connections =
                                    (this.draft_connections + 1).min(rdm_core::MAX_CONNECTIONS);
                                cx.notify();
                            }),
                        )),
                )
                .into_any_element(),
        );
        fields.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.))
                .child(settings_label("默认保存目录", palette))
                .child(div().flex_1().child(self.dir_input.clone()))
                .into_any_element(),
        );
        fields.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.))
                .child(settings_label("全局限速", palette))
                .child(div().flex_1().child(self.limit_input.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.text_faint)
                        .child("MB/s，留空不限速"),
                )
                .into_any_element(),
        );
        fields.push(
            settings_toggle_row(
                "toggle-sha",
                "SHA-256 校验",
                "完成后记录引擎返回的校验值",
                sha,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_verify_sha256 = !this.draft_verify_sha256;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        fields.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.))
                .child(settings_label("主题", palette))
                .child(div().flex_1())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(theme_button(
                            "theme-dark",
                            Theme::Dark,
                            theme,
                            palette,
                            cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.draft_theme = Theme::Dark;
                                cx.notify();
                            }),
                        ))
                        .child(theme_button(
                            "theme-light",
                            Theme::Light,
                            theme,
                            palette,
                            cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.draft_theme = Theme::Light;
                                cx.notify();
                            }),
                        )),
                )
                .into_any_element(),
        );
        fields.push(
            settings_toggle_row(
                "toggle-auto",
                "自动开始",
                "添加后立即开始下载",
                auto,
                palette,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.draft_auto_start = !this.draft_auto_start;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );

        let panel = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .w(px(560.))
            .p(px(18.))
            .bg(palette.panel)
            .border_1()
            .border_color(palette.border)
            .rounded_lg()
            .shadow_lg()
            .child(
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
            .children(fields)
            .child(
                div()
                    .pt(px(4.))
                    .text_xs()
                    .text_color(palette.text_faint)
                    .child("提示：SHA-256 由下载引擎在结果中返回；核心请求契约的 expected_sha256 仅用于传入期望值校验。"),
            )
            .child(
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
}

impl Render for RdmApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let font = self.font.clone();
        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(palette.bg)
            .text_color(palette.text)
            .font(font)
            .on_action(cx.listener(Self::close_dialog))
            .child(self.render_topbar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(self.render_list(cx)),
            )
            .child(self.render_statusbar());
        if self.show_settings {
            root = root.child(self.render_settings(cx));
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
// 小部件
// ---------------------------------------------------------------------------

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
        .text_color(rgb(0xffffff))
        .text_size(px(14.))
        .cursor_pointer()
        .hover(move |style| style.bg(palette.accent_hover))
        .on_click(on_click)
        .child(label)
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
        .w(px(64.))
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
    button.child(value.label_zh())
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
        .gap(px(12.))
        .cursor_pointer()
        .on_click(on_click)
        .child(settings_label(title, palette))
        .child(div().flex_1())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(div().text_xs().text_color(palette.text_faint).child(hint))
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
