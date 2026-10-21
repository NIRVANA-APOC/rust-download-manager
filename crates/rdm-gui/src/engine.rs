//! tokio 运行时与 GPUI 之间的桥。
//!
//! GPUI 有自己的 foreground/background executor，`Entity` 不是 `Send`；而 `rdm-core`
//! 的下载需要 tokio（reqwest/tokio::sync/tokio::time）。所以采用「双运行时 + 消息泵」：
//!
//! ```text
//! tokio Runtime                        GPUI 主线程
//! ┌───────────────────────────┐        ┌──────────────────────────────┐
//! │ download_with_handle      │        │ cx.spawn(async {             │
//! │   └─ DownloadEvent ──┐    │        │   while let Some(msg) =      │
//! │ ticker (250ms) ──────┼──▶ │ mpsc   │       rx.recv().await        │
//! │ AI parse/chat ───────┤    │ ─────▶ │   { weak.update(...) }       │
//! │ IPC server ──────────┘    │        └──────────────────────────────┘
//! └───────────────────────────┘
//! ```
//!
//! 注意：**不要**在 GPUI foreground future 里用 `tokio::time::sleep/tokio::spawn`
//! （GPUI executor 没有 tokio reactor 上下文，会 panic）。定时器统一在 tokio 侧
//! 生成 `EngineMsg::Tick`。
//!
//! 分类路由（`start_download_routed`）在 tokio 侧先 `probe` 再 `rdm_ai::classify`，
//! 分类失败/超时都不会阻塞下载：`probe` 失败退化为 URL 派生文件名，分类结果只用于
//! 选择保存目录；AI 请求都在独立 tokio 任务里并行，不占用下载 worker。

use std::path::PathBuf;
use std::time::Duration;

use rdm_core::{DownloadError, DownloadEvent, DownloadHandle, DownloadRequest, DownloadSummary};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

use crate::model::derive_file_name;

/// 事件泵消息。所有跨线程通信都走这一个通道。
pub enum EngineMsg {
    /// 某次运行（`run_id`）的引擎事件。
    DownloadEvent {
        id: String,
        run_id: u64,
        event: DownloadEvent,
    },
    /// 某次运行的最终结果；`Paused/Cancelled` 是控制流（见 `model::DownloadItem::apply_result`）。
    Finished {
        id: String,
        run_id: u64,
        result: Result<DownloadSummary, DownloadError>,
    },
    /// 分类完成：`dir` 已确定，下载即将开始（成功分类后、`download_with_handle` 前）。
    Routing {
        id: String,
        run_id: u64,
        dir: PathBuf,
        reason: String,
    },
    /// AI 解析完成（`rdm_ai::parse_url` 永不返回 Err；失败会降级为启发式报告）。
    AiParseFinished {
        request_id: u64,
        report: rdm_ai::ParseReport,
    },
    /// AI 解析在 GUI 侧无法继续（例如无效 URL / 内部错误）；供 app 层展示。
    ///
    /// 引擎的 `rdm_ai::parse_url` 永不返回 Err，所以本变体由 app 侧在本地校验失败时
    /// 构造（契约 §4.4 保留）；当前 app 直接调用 `AiPanelState::apply_parse_error`。
    #[allow(dead_code)]
    AiParseFailed { request_id: u64, error: String },
    /// 一次 AI 问答返回（流式结束时也发一次，`reply` 为完整文本）。
    AiChatFinished { request_id: u64, reply: String },
    /// AI 问答流式增量：GUI 追加到当前 assistant 消息（`reply` 最终仍会来）。
    AiChatDelta { request_id: u64, delta: String },
    /// AI 问答失败（未配置 / HTTP / JSON 错误）。
    AiChatFailed { request_id: u64, error: String },
    /// 250ms 心跳：采样全局速度曲线、刷新 ETA；没有活动下载时 GUI 不重绘。
    Tick,
    /// IPC 请求，交给 GUI 主线程处理并回包。
    Ipc {
        envelope: rdm_ipc::Envelope,
        reply: tokio::sync::oneshot::Sender<rdm_ipc::Response>,
    },
}

impl std::fmt::Debug for EngineMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DownloadEvent { id, run_id, event } => f
                .debug_struct("DownloadEvent")
                .field("id", id)
                .field("run_id", run_id)
                .field("event", &std::mem::discriminant(event))
                .finish(),
            Self::Finished { id, run_id, result } => f
                .debug_struct("Finished")
                .field("id", id)
                .field("run_id", run_id)
                .field("ok", &result.is_ok())
                .finish(),
            Self::Routing {
                id,
                run_id,
                dir,
                reason,
            } => f
                .debug_struct("Routing")
                .field("id", id)
                .field("run_id", run_id)
                .field("dir", dir)
                .field("reason", reason)
                .finish(),
            Self::AiParseFinished { request_id, report } => f
                .debug_struct("AiParseFinished")
                .field("request_id", request_id)
                .field("source_url", &report.source_url)
                .field("resources", &report.resources.len())
                .finish(),
            Self::AiParseFailed { request_id, error } => f
                .debug_struct("AiParseFailed")
                .field("request_id", request_id)
                .field("error", error)
                .finish(),
            Self::AiChatFinished { request_id, reply } => f
                .debug_struct("AiChatFinished")
                .field("request_id", request_id)
                .field("reply_len", &reply.len())
                .finish(),
            Self::AiChatDelta { request_id, delta } => f
                .debug_struct("AiChatDelta")
                .field("request_id", request_id)
                .field("delta_len", &delta.len())
                .finish(),
            Self::AiChatFailed { request_id, error } => f
                .debug_struct("AiChatFailed")
                .field("request_id", request_id)
                .field("error", error)
                .finish(),
            Self::Tick => f.write_str("Tick"),
            Self::Ipc { envelope, .. } => f.debug_struct("Ipc").field("id", &envelope.id).finish(),
        }
    }
}

/// 下载前的分类路由选项（§4.4）。
///
/// `state_dir` 是运行时状态目录（`settings.json`/`ai.json` 所在目录），由 app 侧传入，
/// 便于后续把分类/解析缓存接到同一处；当前分类本身不需要写盘。
#[derive(Clone)]
pub struct RoutingOptions {
    pub save_by_extension: bool,
    pub use_ai_classify: bool,
    pub ai_config: rdm_ai::AiConfig,
    pub state_dir: PathBuf,
}

/// 每个下载任务的事件转发 + 结果收敛。
///
/// `rdm-core` 的 `download_with_handle` 消费 `DownloadEvent` 的 sender，因此另起一个
/// forward 任务把事件转到 GUI 通道；`download_with_handle` 返回后等 forward 排空，
/// 再发 `Finished`，保证 GUI 先看到事件、后看到终态。
#[derive(Clone)]
pub struct EngineRuntime {
    runtime: Handle,
    ui_tx: UnboundedSender<EngineMsg>,
}

impl EngineRuntime {
    pub fn new(runtime: Handle, ui_tx: UnboundedSender<EngineMsg>) -> Self {
        Self { runtime, ui_tx }
    }

    /// 启动一个下载任务。`handle` 由 `DownloadItem` 保存，用于暂停/取消。
    ///
    /// 契约 §4.4 的兼容入口（GUI 目前统一走 [`start_download_routed`](Self::start_download_routed)，
    /// 本入口保留给测试/不分类场景）。
    #[allow(dead_code)]
    pub fn start_download(
        &self,
        id: String,
        run_id: u64,
        request: DownloadRequest,
        handle: DownloadHandle,
    ) {
        self.spawn_download(id, run_id, request, handle, None);
    }

    /// 与 [`start_download`](Self::start_download) 相同，但在 tokio 侧先做
    /// 「probe → 分类目录 → 下载」。
    ///
    /// `routing = None` 时行为与 `start_download` 完全一致；resume 时由调用方把
    /// `item.save_dir` 写进 `request.output_dir`。只有「按后缀分类」或「AI 分类且
    /// AI 已配置」时才真正 probe/分类，避免开关可开但实际无分类能力时白跑探测。
    pub fn start_download_routed(
        &self,
        id: String,
        run_id: u64,
        request: DownloadRequest,
        handle: DownloadHandle,
        routing: Option<RoutingOptions>,
    ) {
        let routing = routing.filter(|options| {
            options.save_by_extension
                || (options.use_ai_classify && options.ai_config.is_configured())
        });
        self.spawn_download(id, run_id, request, handle, routing);
    }

    /// 解析：**resolver-first**（`docs/architecture.md` §8.5 / §11.12）。
    ///
    /// 先跑 `rdm-resolve`（`mode=auto`：方法回放 → 内置直链验证 → 已配置 AI 时页面
    /// 探索）；至少 1 个 `verified`/`head_only_verified` 资源时映射成既有
    /// [`rdm_ai::ParseReport`] 并直接回 UI，**不写** GUI 解析缓存（resolver 有自己的
    /// 轨迹）。以下情况降级 legacy [`rdm_ai::parse_url`]（启发式 + 可选模型 + 解析缓存）：
    ///
    /// 1. 无 verified 资源 / 解析失败（含 `model_unavailable` 等临时失败）；
    /// 2. （P4a）`ParseRequest.cookie` 非空时不再直接 legacy：先把它与 referer/UA 构造成
    ///    **仅内存、仅输入 host** 的 [`rdm_resolve::HostCredentials`] 交给 resolver 做
    ///    验证/方法回放；仍无 verified 资源时才降级 legacy。
    /// 3. （P4b-b2）`page_snapshot`（来自扩展/native/IPC 的页面链接快照）只挂到
    ///    resolver-first 路径；legacy `rdm_ai::parse_url` 不使用它。
    ///
    /// 凭据/快照都只存在于本次任务的闭包里，不写盘、不进日志值/轨迹/report。
    pub fn start_ai_parse(
        &self,
        request_id: u64,
        request: rdm_ai::ParseRequest,
        config: rdm_ai::AiConfig,
        state_dir: PathBuf,
        page_snapshot: Option<rdm_resolve::PageSnapshot>,
    ) {
        let tx = self.ui_tx.clone();
        self.runtime.spawn(async move {
            let started = std::time::Instant::now();
            // P4a：凭据只在这里构造一次，随本任务作用域结束而丢弃。
            let credentials = host_credentials(&request);
            let authenticated = request.cookie.is_some();
            // P4b-b2：日志只记快照链接数（不记 URL/链接值）。
            let snapshot_links = page_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.links.len());

            let (outcome, health_note) =
                resolve_with_resolver(&request, &config, &state_dir, credentials, page_snapshot)
                    .await;
            let method_health_warned = health_note.is_some();
            let report = crate::ai_resolve::report_from_outcome(
                &request.url,
                request.title.as_deref(),
                Some(config.model.as_str()),
                &outcome,
            );
            if let Some(mut report) = report {
                // P3b：degraded/broken 方法提示只追加到 notes，不改结果/资源/其它字段。
                append_health_note(&mut report, health_note);
                tracing::debug!(
                    request_id,
                    source = "resolver",
                    authenticated,
                    page_snapshot_links = snapshot_links,
                    method_health_warned,
                    resources = report.resources.len(),
                    llm_calls = outcome.diagnostics.llm_calls,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "AI 解析完成（resolver-first）"
                );
                if tx
                    .send(EngineMsg::AiParseFinished { request_id, report })
                    .is_err()
                {
                    tracing::debug!(request_id, "AI 解析结果丢弃：GUI 通道已关闭");
                }
                return;
            }
            tracing::debug!(
                request_id,
                source = "legacy",
                authenticated,
                page_snapshot_links = snapshot_links,
                method_health_warned,
                reason = ?outcome.status,
                resolver_kind = ?outcome.diagnostics.error.as_ref().map(|error| error.kind),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "resolver 未产出可下载资源，降级 legacy"
            );

            // ---- legacy 降级：保持原行为（启发式 + 可选模型 + 解析缓存） ----
            let options = rdm_ai::ParseOptions::default();
            let mut session = rdm_ai::ParseSession::open(&state_dir).await;
            let mut report =
                rdm_ai::parse_url(&request, &config, session.cache_mut(), &options).await;
            // P3b：legacy 降级路径同样附加方法健康度提示（仅当方法确实匹配 URL）。
            append_health_note(&mut report, health_note);
            if let Err(err) = session.save() {
                tracing::warn!(error = %err, "写入 AI 解析缓存失败");
            }
            tracing::debug!(
                request_id,
                source = "legacy",
                authenticated,
                resources = report.resources.len(),
                ai_used = report.ai_used,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "AI 解析完成（legacy）"
            );
            if tx
                .send(EngineMsg::AiParseFinished { request_id, report })
                .is_err()
            {
                tracing::debug!(request_id, "AI 解析结果丢弃：GUI 通道已关闭");
            }
        });
    }

    /// Chat（流式）：每段 delta 通过 `AiChatDelta` 增量回 UI，结束时再回 `AiChatFinished`。
    pub fn start_ai_chat_stream(
        &self,
        request_id: u64,
        config: rdm_ai::AiConfig,
        system: String,
        prompt: String,
    ) {
        let tx = self.ui_tx.clone();
        self.runtime.spawn(async move {
            let client = match rdm_ai::AiClient::new(config) {
                Ok(client) => client,
                Err(err) => {
                    let _ = tx.send(EngineMsg::AiChatFailed {
                        request_id,
                        error: err.to_string(),
                    });
                    return;
                }
            };
            let delta_tx = tx.clone();
            let mut on_delta = move |delta: &str| {
                if delta.is_empty() {
                    return;
                }
                let _ = delta_tx.send(EngineMsg::AiChatDelta {
                    request_id,
                    delta: delta.to_owned(),
                });
            };
            let message = match client.chat_stream(&system, &prompt, &mut on_delta).await {
                Ok(reply) => EngineMsg::AiChatFinished { request_id, reply },
                Err(err) => EngineMsg::AiChatFailed {
                    request_id,
                    error: err.to_string(),
                },
            };
            let _ = tx.send(message);
        });
    }

    /// 启动 250ms 心跳。通道关闭（GUI 退出）时自动结束。
    pub fn start_ticker(&self) {
        let tx = self.ui_tx.clone();
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if tx.send(EngineMsg::Tick).is_err() {
                    break;
                }
            }
        });
    }

    fn spawn_download(
        &self,
        id: String,
        run_id: u64,
        mut request: DownloadRequest,
        handle: DownloadHandle,
        routing: Option<RoutingOptions>,
    ) {
        let ui_tx = self.ui_tx.clone();
        self.runtime.spawn(async move {
            if let Some(options) = routing {
                classify_and_route(&id, run_id, &mut request, &options, &ui_tx).await;
            }

            // probe / LLM 期间用户可能已经暂停或取消：不要再启动下载。
            // 下载本身的控制流错误由 `download_with_handle` 收敛，这里只做前置检查。
            if handle.is_cancelled() {
                let _ = ui_tx.send(EngineMsg::Finished {
                    id,
                    run_id,
                    result: Err(DownloadError::Cancelled { downloaded: 0 }),
                });
                return;
            }
            if handle.is_paused() {
                let _ = ui_tx.send(EngineMsg::Finished {
                    id,
                    run_id,
                    result: Err(DownloadError::Paused { downloaded: 0 }),
                });
                return;
            }

            let events_tx = ui_tx.clone();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<DownloadEvent>();
            let forward_id = id.clone();
            let forward = tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    if events_tx
                        .send(EngineMsg::DownloadEvent {
                            id: forward_id.clone(),
                            run_id,
                            event,
                        })
                        .is_err()
                    {
                        // GUI 已退出：排空后结束。
                        break;
                    }
                }
            });

            let result = rdm_core::download_with_handle(request, Some(event_tx), handle).await;
            let _ = forward.await;
            let _ = ui_tx.send(EngineMsg::Finished { id, run_id, result });
        });
    }
}

/// 用 `rdm-resolve` 解析一次（GUI 本机用户策略：允许所有 host、允许私网/回环，
/// `docs/architecture.md` §8.5）。
///
/// 未配置 AI 时 `with_ai_config` 内部按未配置处理：只做方法回放 + 内置直链验证；
/// 已配置时 HTML 页面还会进入预算内的 JsonAction 探索（§11.9）。`credentials`
/// （P4a）只在打开本次解析时挂到 service 上，resolver 内部会把它限定到输入 host
/// 且进入探索前清空（§11.12）。`page_snapshot`（P4b-b2）只在 resolver-first 路径
/// 传给 resolver；legacy 分支（调用方）永远看不到它。
///
/// 返回 `(outcome, health_note)`：`health_note` 是 P3b 的 degraded/broken 方法提示
/// （解析前从同一个 registry 读取；resolver-first 与 legacy 降级两条路径都会把它
/// 追加到报告 notes）。
async fn resolve_with_resolver(
    request: &rdm_ai::ParseRequest,
    config: &rdm_ai::AiConfig,
    state_dir: &std::path::Path,
    credentials: Option<rdm_resolve::HostCredentials>,
    page_snapshot: Option<rdm_resolve::PageSnapshot>,
) -> (rdm_resolve::ResolveOutcome, Option<String>) {
    let mut service = rdm_resolve::ResolverService::with_state_dir(
        state_dir,
        rdm_resolve::ResolvePolicy::loopback(),
    )
    .with_ai_config(config.clone());
    if let Some(credentials) = credentials {
        service = service.with_credentials(credentials);
    }
    if let Some(page_snapshot) = page_snapshot {
        service = service.with_page_snapshot(page_snapshot);
    }
    // P3b：解析前读本地方法库 active 版本健康度（提示只进 notes，不影响结果）。
    let health_note = pre_run_health_note(service.registry(), request.url.trim());
    let outcome = service
        .resolve(rdm_resolve::ResolveRequest {
            url: request.url.clone(),
            goal: request.hint.clone(),
            mode: rdm_resolve::ResolveMode::Auto,
            recipe_id: None,
            budget: Default::default(),
            max_resources: Some(8),
            max_llm_calls: None,
        })
        .await;
    (outcome, health_note)
}

/// P3b（`docs/architecture.md` §8.5 v0.16）：解析前判断 URL 是否匹配到本地方法库
/// 中 `degraded`/`broken` 的 active 方法，并生成给 GUI 面板的提示。
///
/// `None`：URL 无效 / 没有匹配方法 / 方法健康（ok/untested）。
fn pre_run_health_note(
    registry: &rdm_resolve::RecipeRegistry,
    request_url: &str,
) -> Option<String> {
    let url = url::Url::parse(request_url.trim()).ok()?;
    let hit = registry.find_for_url(&url)?;
    let recipe = hit.recipe;
    let health = registry.health_for(&recipe.id, recipe.version);
    crate::ai_resolve::method_health_note(&recipe.id, recipe.version, &health)
}

/// 把健康度提示追加到报告 notes（不改资源/结构）。
///
/// `ParseReport.notes` 契约是 ≤8 条、GUI 面板只显示前 8 条：追加后若超限，
/// 丢弃最早的条目以保证新提示可见。
fn append_health_note(report: &mut rdm_ai::ParseReport, health_note: Option<String>) {
    let Some(note) = health_note else {
        return;
    };
    if report.notes.iter().any(|existing| existing == &note) {
        return;
    }
    report.notes.push(note);
    if report.notes.len() > rdm_ai::parse::MAX_NOTES {
        let excess = report.notes.len() - rdm_ai::parse::MAX_NOTES;
        report.notes.drain(..excess);
    }
}

/// P4a 凭据桥：把 GUI/native 在浏览器上下文里拿到的一次性 Cookie/Referer/UA
/// 包成 **仅内存** 的 [`rdm_resolve::HostCredentials`]（仅输入 URL 的 host）。
///
/// 只传这三个头；调用方保证凭据不落盘、不入日志/report/轨迹。URL 无效或没有
/// Cookie 时返回 `None`（无效 URL 由 legacy 启发式路径处理）。
fn host_credentials(request: &rdm_ai::ParseRequest) -> Option<rdm_resolve::HostCredentials> {
    let cookie = non_empty(request.cookie.as_deref())?;
    let url = url::Url::parse(request.url.trim()).ok()?;
    let host = url.host_str()?.to_owned();
    let mut credentials = rdm_resolve::HostCredentials::default().with_cookie(&host, cookie);
    if let Some(referer) = non_empty(request.referer.as_deref()) {
        credentials = credentials.with_header(&host, "referer", referer);
    }
    if let Some(user_agent) = non_empty(request.user_agent.as_deref()) {
        credentials = credentials.with_header(&host, "user-agent", user_agent);
    }
    Some(credentials)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// 分类目录：`probe` 失败不阻塞下载，只退化为 URL 派生文件名 + 无 Content-Type。
async fn classify_and_route(
    id: &str,
    run_id: u64,
    request: &mut DownloadRequest,
    options: &RoutingOptions,
    ui_tx: &UnboundedSender<EngineMsg>,
) {
    tracing::debug!(
        state_dir = %options.state_dir.display(),
        url = %request.url,
        "开始分类路由"
    );

    let base = request
        .output_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let meta = rdm_core::probe(request).await.ok();
    let file_name = request
        .file_name
        .clone()
        .or_else(|| meta.as_ref().map(|meta| meta.file_name.clone()))
        .unwrap_or_else(|| derive_file_name(&request.url));
    let content_type = meta.as_ref().and_then(|meta| meta.content_type.clone());
    let input = rdm_ai::ClassifyInput {
        url: request.url.clone(),
        file_name,
        content_type,
        root_dir: base.clone(),
        existing_dirs: rdm_ai::list_subdirs(&base),
    };
    let outcome = rdm_ai::classify(&input, &options.ai_config, options.use_ai_classify).await;

    // 用户关掉了「按后缀分类」且 AI 没有实际生效：保持原目录，不发 Routing。
    // （`rdm_ai::classify` 在 AI 未配置/失败时会降级到本地后缀规则，这里把它挡掉。）
    if !options.save_by_extension && !outcome.ai_used {
        return;
    }

    let dir = base.join(&outcome.dir_name);
    let _ = ui_tx.send(EngineMsg::Routing {
        id: id.to_string(),
        run_id,
        dir: dir.clone(),
        reason: outcome.reason,
    });
    request.output_dir = Some(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "rdm-gui-engine-{tag}-{}-{}",
            std::process::id(),
            crate::model::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// 等到 `Finished`/`Ai*` 结果，忽略中间事件；返回所有收到的消息（供断言 Routing 等）。
    fn drain_until_finished(
        runtime: &tokio::runtime::Runtime,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<EngineMsg>,
        timeout: Duration,
    ) -> Vec<EngineMsg> {
        runtime.block_on(async {
            let mut seen = Vec::new();
            let _ = tokio::time::timeout(timeout, async {
                while let Some(msg) = rx.recv().await {
                    let terminal = matches!(
                        msg,
                        EngineMsg::Finished { .. }
                            | EngineMsg::AiParseFinished { .. }
                            | EngineMsg::AiParseFailed { .. }
                            | EngineMsg::AiChatFinished { .. }
                            | EngineMsg::AiChatFailed { .. }
                    );
                    seen.push(msg);
                    if terminal {
                        return;
                    }
                }
            })
            .await;
            seen
        })
    }

    fn invalid_request() -> DownloadRequest {
        DownloadRequest::new("not-a-valid-url")
    }

    fn parse_request(url: &str) -> rdm_ai::ParseRequest {
        rdm_ai::ParseRequest {
            url: url.to_string(),
            title: None,
            referer: None,
            user_agent: None,
            cookie: None,
            hint: None,
        }
    }

    fn parse_report() -> rdm_ai::ParseReport {
        rdm_ai::ParseReport {
            source_url: "https://example.com/page".to_string(),
            source_kind: "html_page".to_string(),
            title: None,
            resources: vec![rdm_ai::ParsedResource {
                url: "https://example.com/a.mp4".to_string(),
                name: "a.mp4".to_string(),
                kind: "video".to_string(),
                size_bytes: None,
                content_type: Some("video/mp4".to_string()),
                note: None,
            }],
            notes: Vec::new(),
            ai_used: false,
            model: None,
            created_ms: 1,
            cache_hit: false,
            failure_kind: None,
        }
    }

    /// 核心目前是 stub（返回 Unsupported），但未来实现后这个测试仍然成立：
    /// 用非法 scheme 保证引擎快速返回错误，不发网络请求 / 不依赖测试环境。
    #[test]
    fn engine_finishes_with_error_for_invalid_request() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);

        engine.start_download(
            "dl_000001".to_string(),
            7,
            invalid_request(),
            DownloadHandle::new(),
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(10));
        let finished = messages
            .iter()
            .find_map(|msg| match msg {
                EngineMsg::Finished { id, run_id, result } => {
                    Some((id.clone(), *run_id, result.is_err()))
                }
                _ => None,
            })
            .expect("超时未收到 Finished");
        assert_eq!(finished.0, "dl_000001");
        assert_eq!(finished.1, 7);
        assert!(finished.2, "非法 URL 应返回错误");
    }

    #[test]
    fn ticker_emits_ticks_without_panicking() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        engine.start_ticker();
        let tick = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(EngineMsg::Tick) = rx.recv().await {
                        return true;
                    }
                }
            })
            .await
        });
        assert!(tick.is_ok());
        // 丢弃 runtime 时任务被取消，不应 panic。
        drop(engine);
    }

    #[test]
    fn routed_download_without_routing_matches_plain_start() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        engine.start_download_routed(
            "dl_000002".to_string(),
            3,
            invalid_request(),
            DownloadHandle::new(),
            None,
        );
        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(10));
        assert!(
            !messages
                .iter()
                .any(|msg| matches!(msg, EngineMsg::Routing { .. })),
            "routing=None 时不应有 Routing 消息"
        );
        let finished = messages
            .iter()
            .find_map(|msg| match msg {
                EngineMsg::Finished { id, run_id, result } => {
                    Some((id.clone(), *run_id, result.is_err()))
                }
                _ => None,
            })
            .expect("超时未收到 Finished");
        assert_eq!(finished.0, "dl_000002");
        assert_eq!(finished.1, 3);
        assert!(finished.2);
    }

    #[test]
    fn routed_download_with_both_switches_off_skips_classification() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let routing = Some(RoutingOptions {
            save_by_extension: false,
            use_ai_classify: false,
            ai_config: rdm_ai::AiConfig::default(),
            state_dir: PathBuf::from("."),
        });
        engine.start_download_routed(
            "dl_000003".to_string(),
            4,
            invalid_request(),
            DownloadHandle::new(),
            routing,
        );
        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(10));
        assert!(!messages
            .iter()
            .any(|msg| matches!(msg, EngineMsg::Routing { .. })));
        assert!(messages
            .iter()
            .any(|msg| matches!(msg, EngineMsg::Finished { result: Err(_), .. })));
    }

    #[test]
    fn routed_download_classifies_and_reports_routing() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let root = temp_dir("route");
        // 用 `.mp4` 让本地规则产生「视频」子目录（probe 仍会因非法 URL 失败）。
        let mut request = DownloadRequest::new("not-a-valid-url/video.mp4");
        request.output_dir = Some(root.clone());
        let routing = Some(RoutingOptions {
            save_by_extension: true,
            use_ai_classify: true,
            ai_config: rdm_ai::AiConfig::default(), // 未配置：走本地后缀规则
            state_dir: root.clone(),
        });
        engine.start_download_routed(
            "dl_000004".to_string(),
            5,
            request,
            DownloadHandle::new(),
            routing,
        );
        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(15));
        let routing = messages
            .iter()
            .find_map(|msg| match msg {
                EngineMsg::Routing { dir, reason, .. } => Some((dir.clone(), reason.clone())),
                _ => None,
            })
            .expect("分类成功应发送 Routing 消息");
        assert_eq!(routing.0.parent(), Some(root.as_path()));
        assert_eq!(
            routing
                .0
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            Some("视频".to_string())
        );
        assert!(!routing.1.is_empty(), "Routing 应带简短理由");
        assert!(
            messages
                .iter()
                .any(|msg| matches!(msg, EngineMsg::Finished { result: Err(_), .. })),
            "路由后非法 URL 仍应快速失败"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn routed_download_without_extension_saving_keeps_root_dir() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let root = temp_dir("route-noext");
        let mut request = invalid_request();
        request.output_dir = Some(root.clone());
        let routing = Some(RoutingOptions {
            save_by_extension: false,
            use_ai_classify: true,
            ai_config: rdm_ai::AiConfig::default(), // AI 未配置 → 本地规则不启用
            state_dir: root.clone(),
        });
        engine.start_download_routed(
            "dl_000005".to_string(),
            6,
            request,
            DownloadHandle::new(),
            routing,
        );
        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(15));
        assert!(
            !messages
                .iter()
                .any(|msg| matches!(msg, EngineMsg::Routing { .. })),
            "关闭后缀分类且 AI 未生效时不应改变目录"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ai_parse_without_configured_ai_finishes_with_report() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("parse");
        engine.start_ai_parse(
            9,
            parse_request("not-a-valid-url"),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            None,
        );
        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(15));
        let report = messages
            .iter()
            .find_map(|msg| match msg {
                EngineMsg::AiParseFinished { request_id, report } => Some((*request_id, report)),
                _ => None,
            })
            .expect("超时未收到 AiParseFinished");
        assert_eq!(report.0, 9);
        assert!(!report.1.ai_used, "未配置 AI 时应为启发式结果");
        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    // ------------------------------------------------------------------
    // P5: resolver-first + legacy fallback (`docs/architecture.md` §8.5)
    // ------------------------------------------------------------------

    fn ai_parse_report(messages: &[EngineMsg]) -> (u64, &rdm_ai::ParseReport) {
        messages
            .iter()
            .find_map(|msg| match msg {
                EngineMsg::AiParseFinished { request_id, report } => Some((*request_id, report)),
                _ => None,
            })
            .expect("超时未收到 AiParseFinished")
    }

    fn legacy_cache_exists(state_dir: &std::path::Path) -> bool {
        rdm_ai::store::ParseCache::cache_path(state_dir).exists()
    }

    /// 递归检查目录下任何文件是否包含 `needle`（用于断言凭据不落盘）。
    fn dir_contains_text(root: &std::path::Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if dir_contains_text(&path, needle) {
                    return true;
                }
            } else if std::fs::read(&path)
                .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }

    #[test]
    fn ai_parse_resolver_first_maps_a_direct_file_without_the_legacy_cache() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("resolver-first");
        let server = runtime
            .block_on(rdm_fixture_server::TestServer::start(
                rdm_fixture_server::Config::new(96 * 1024),
            ))
            .expect("start fixture server");
        let url = server.file_url();

        engine.start_ai_parse(
            21,
            parse_request(&url),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            None,
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 21);
        assert_eq!(report.source_url, url);
        assert_eq!(report.source_kind, "direct_file");
        assert_eq!(report.resources.len(), 1);
        assert_eq!(report.resources[0].url, url);
        assert_eq!(report.resources[0].size_bytes, Some(96 * 1024));
        assert!(!report.ai_used, "无模型调用时 ai_used=false");
        assert!(report.model.is_none());
        assert!(!report.cache_hit);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("builtin direct-file verification")),
            "resolver 摘要应进入 notes: {:?}",
            report.notes
        );
        assert!(
            !legacy_cache_exists(&state_dir),
            "resolver-first 成功时不得写 GUI 旧解析缓存"
        );

        runtime.block_on(server.shutdown());
        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    // ------------------------------------------------------------------
    // P4a: credential bridge (`docs/architecture.md` §11.12)
    // ------------------------------------------------------------------

    fn authenticated_request(url: &str, cookie: &str) -> rdm_ai::ParseRequest {
        let mut request = parse_request(url);
        request.cookie = Some(cookie.to_string());
        request.referer = Some("https://app.example.test/page".to_string());
        request.user_agent = Some("rdm-gui-test/1.0".to_string());
        request
    }

    #[test]
    fn ai_parse_with_cookie_uses_resolver_credentials_first() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("cookie-resolver");
        // 服务端要求正确 Cookie 才返回文件：只有把 GUI 的 cookie 交给 resolver
        // （HostCredentials → HttpTool 注入）才能 verified。
        let server = runtime
            .block_on(rdm_fixture_server::TestServer::start(
                rdm_fixture_server::Config::new(96 * 1024).with_required_cookie("session=ok"),
            ))
            .expect("start fixture server");
        let url = server.file_url();

        engine.start_ai_parse(
            24,
            authenticated_request(&url, "session=ok"),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            None,
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 24);
        assert_eq!(report.source_url, url);
        assert_eq!(report.source_kind, "direct_file");
        assert_eq!(report.resources.len(), 1);
        assert_eq!(report.resources[0].url, url);
        assert_eq!(report.resources[0].size_bytes, Some(96 * 1024));
        assert!(!report.ai_used);
        assert!(
            !legacy_cache_exists(&state_dir),
            "Cookie 命中 resolver credentials 时不得写 GUI 旧解析缓存"
        );
        // P4a 安全：Cookie/Referer/UA 只在内存，resolver 轨迹/任何落盘文件都不含凭据值。
        assert!(
            !dir_contains_text(&state_dir, "session=ok"),
            "Cookie 值不得写入 state_dir（轨迹/缓存）"
        );
        assert!(!format!("{report:?}").contains("session=ok"));

        runtime.block_on(server.shutdown());
        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    #[test]
    fn ai_parse_with_wrong_cookie_falls_back_to_legacy() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("cookie-wrong");
        let server = runtime
            .block_on(rdm_fixture_server::TestServer::start(
                rdm_fixture_server::Config::new(96 * 1024).with_required_cookie("session=ok"),
            ))
            .expect("start fixture server");
        let url = server.file_url();

        engine.start_ai_parse(
            25,
            authenticated_request(&url, "session=wrong"),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            None,
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 25);
        assert_eq!(report.source_url, url);
        assert!(
            legacy_cache_exists(&state_dir),
            "错误 Cookie 时 resolver 无 verified 资源 → 必须降级 legacy 并维护旧缓存"
        );
        // legacy 抓取也被 401 拒绝：报告降级但结构不变（不断言具体资源数）。
        assert!(
            report.failure_kind.is_some(),
            "legacy 降级报告应带 failure_kind: {report:?}"
        );

        runtime.block_on(server.shutdown());
        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    #[test]
    fn ai_parse_with_cookie_on_an_html_page_falls_back_to_legacy() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("cookie-html-legacy");
        let url = spawn_html_server("<html><body><p>login wall</p></body></html>");

        engine.start_ai_parse(
            26,
            authenticated_request(&url, "session=fixture"),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            None,
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 26);
        assert_eq!(report.source_url, url);
        assert!(!report.ai_used);
        assert!(
            legacy_cache_exists(&state_dir),
            "登录页 HTML 的链接提取仍需 legacy（页面桥属 P4b）"
        );

        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    // ------------------------------------------------------------------
    // P4b-b2: page link snapshot wiring (`docs/architecture.md` §11.12)
    // ------------------------------------------------------------------

    #[test]
    fn ai_parse_passes_a_page_snapshot_into_the_resolver() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("page-snapshot");
        let server = runtime
            .block_on(rdm_fixture_server::TestServer::start(
                rdm_fixture_server::Config::new(96 * 1024),
            ))
            .expect("start fixture server");
        let url = server.file_url();
        let snapshot = rdm_resolve::PageSnapshot {
            url: url.clone(),
            title: Some("Fixture page".to_string()),
            links: vec![url.clone()],
        };

        engine.start_ai_parse(
            27,
            parse_request(&url),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            Some(snapshot),
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 27);
        assert_eq!(report.source_url, url);
        assert_eq!(report.source_kind, "direct_file");
        assert_eq!(report.resources.len(), 1);
        assert!(!report.ai_used);
        // 快照只走 resolver-first：不写 GUI 旧缓存，也不出现在报告里。
        assert!(!legacy_cache_exists(&state_dir));
        assert!(!format!("{report:?}").contains("Fixture page"));

        runtime.block_on(server.shutdown());
        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    // ------------------------------------------------------------------
    // P3b: method health note wiring (`docs/architecture.md` §8.5 v0.16)
    // ------------------------------------------------------------------

    /// Fixture method matching `http://127.0.0.1:<port>/page`, emitting
    /// `{{base}}/file` (the fixture server's payload path).
    fn health_recipe(id: &str, version: u32) -> rdm_resolve::Recipe {
        rdm_resolve::Recipe::from_json(
            &serde_json::json!({
                "schema": "rdm.recipe/v1",
                "id": id,
                "version": version,
                "match": {
                    "hosts": ["127.0.0.1"],
                    "url_pattern": "^(?P<base>http://127\\.0\\.0\\.1:\\d+)/page$"
                },
                "steps": [{
                    "id": "noop",
                    "op": "assert",
                    "args": { "kind": "equals", "value": "x", "expected": "x" }
                }],
                "emit": { "url": "{{base}}/file" }
            })
            .to_string(),
        )
        .expect("valid health fixture recipe")
    }

    /// Persist `health-method@2` and record two structural failures (degraded).
    fn seed_degraded_method(state_dir: &std::path::Path) -> std::path::PathBuf {
        let recipes_dir = state_dir.join("resolve").join("recipes");
        let mut registry = rdm_resolve::RecipeRegistry::with_root(&recipes_dir);
        let recipe = health_recipe("health-method", 2);
        registry.save(&recipe).expect("write recipe");
        registry.insert(recipe).expect("insert recipe");
        for (run_id, step_id) in [("run_1", Some("noop")), ("run_2", None)] {
            registry
                .record_failure(
                    "health-method",
                    2,
                    run_id,
                    rdm_resolve::ResolveErrorKind::RecipeStale,
                    step_id,
                    10,
                )
                .expect("record failure");
        }
        assert_eq!(
            registry.health_for("health-method", 2).status,
            rdm_resolve::MethodStatus::Degraded
        );
        recipes_dir
    }

    #[test]
    fn pre_run_health_note_reports_a_degraded_matching_method() {
        let state_dir = temp_dir("health-note-check");
        let recipes_dir = seed_degraded_method(&state_dir);
        let registry = rdm_resolve::RecipeRegistry::with_root(&recipes_dir);

        let note = pre_run_health_note(&registry, "http://127.0.0.1:45678/page")
            .expect("degraded method must produce a note");
        assert!(note.contains("health-method@v2"), "{note}");
        assert!(note.contains("已降级"), "{note}");
        assert!(note.contains("2 次"), "{note}");
        assert!(note.contains("recipe_stale"), "{note}");
        assert!(note.contains("rdm recipe rollback"), "{note}");
        assert!(note.chars().count() <= 240, "{note}");

        // Unmatched URL / invalid input never warn.
        assert!(pre_run_health_note(&registry, "https://example.com/page").is_none());
        assert!(pre_run_health_note(&registry, "not a url").is_none());

        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    #[test]
    fn pre_run_health_note_is_silent_for_healthy_or_empty_registries() {
        let empty = rdm_resolve::RecipeRegistry::empty();
        assert!(pre_run_health_note(&empty, "http://127.0.0.1:1234/page").is_none());

        // A matching but never-run method is `untested` → no note.
        let state_dir = temp_dir("health-note-untested");
        let recipes_dir = state_dir.join("resolve").join("recipes");
        let mut registry = rdm_resolve::RecipeRegistry::with_root(&recipes_dir);
        let recipe = health_recipe("fresh-method", 1);
        registry.save(&recipe).unwrap();
        registry.insert(recipe).unwrap();
        let reloaded = rdm_resolve::RecipeRegistry::with_root(&recipes_dir);
        assert!(pre_run_health_note(&reloaded, "http://127.0.0.1:1234/page").is_none());

        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    #[test]
    fn append_health_note_keeps_the_warning_visible_when_notes_are_full() {
        let mut report = parse_report();
        report.notes = (0..rdm_ai::parse::MAX_NOTES)
            .map(|index| format!("note-{index}"))
            .collect();
        let warning = "本地方法 health-method@v2 已降级".to_string();
        append_health_note(&mut report, Some(warning.clone()));
        assert_eq!(report.notes.len(), rdm_ai::parse::MAX_NOTES);
        assert_eq!(report.notes.last(), Some(&warning));
        assert!(!report.notes.iter().any(|note| note == "note-0"));

        // None / duplicate notes are ignored.
        let before = report.notes.clone();
        append_health_note(&mut report, None);
        append_health_note(&mut report, Some(warning));
        assert_eq!(report.notes, before);
    }

    #[test]
    fn ai_parse_appends_a_degraded_method_health_note_to_the_report() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("health-note-e2e");
        seed_degraded_method(&state_dir);
        let server = runtime
            .block_on(rdm_fixture_server::TestServer::start(
                rdm_fixture_server::Config::new(96 * 1024),
            ))
            .expect("start fixture server");
        // `health-method@2` matches this page and emits the fixture payload URL.
        let url = server.url("/page");

        engine.start_ai_parse(
            28,
            parse_request(&url),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            None,
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 28);
        assert_eq!(report.resources.len(), 1);
        assert!(!report.ai_used);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("health-method@v2") && note.contains("已降级")),
            "degraded method note missing: {:?}",
            report.notes
        );
        // Health note does not change which path produced the report (recipe
        // replay → resolver-first, no GUI cache write).
        assert!(!legacy_cache_exists(&state_dir));

        runtime.block_on(server.shutdown());
        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    #[test]
    fn legacy_fallback_also_appends_the_degraded_method_health_note() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("health-note-legacy");
        seed_degraded_method(&state_dir);

        // `health-method@2` matches, but its emitted URL (port 1) is unreachable,
        // so the resolver cannot verify anything → legacy heuristic fallback.
        engine.start_ai_parse(
            29,
            parse_request("http://127.0.0.1:1/page"),
            rdm_ai::AiConfig::default(),
            state_dir.clone(),
            None,
        );

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 29);
        assert!(
            legacy_cache_exists(&state_dir),
            "unverifiable recipe must fall back to legacy"
        );
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("health-method@v2") && note.contains("已降级")),
            "legacy fallback must keep the degraded-method note: {:?}",
            report.notes
        );
        assert!(!report.ai_used);

        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    /// 极简 HTML mock：任意请求都回同一页；用于「页面 + AI 不可用 → 降级 legacy」。
    ///
    /// 线程是 detach 的（测试结束由进程退出回收）：解析要发几次请求取决于被降级的
    /// 链路，join 固定次数容易把测试挂死。
    fn spawn_html_server(html: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind html mock");
        let url = format!("http://{}/page", listener.local_addr().expect("local addr"));
        std::thread::spawn(move || {
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        url
    }

    #[test]
    fn ai_parse_falls_back_to_legacy_when_the_model_is_unavailable() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let state_dir = temp_dir("model-unavailable");
        let url = spawn_html_server("<html><body><p>nothing here</p></body></html>");
        // 已配置 AI 但端点不可达：resolver 探索 → model_unavailable → legacy 降级。
        let config = rdm_ai::AiConfig {
            enabled: true,
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "mock".to_string(),
            api_key: "sk-gui-test".to_string(),
            timeout_secs: 2,
        };

        engine.start_ai_parse(23, parse_request(&url), config, state_dir.clone(), None);

        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(20));
        let (request_id, report) = ai_parse_report(&messages);
        assert_eq!(request_id, 23);
        assert_eq!(report.source_url, url);
        assert!(!report.ai_used, "模型不可用时 legacy 应有启发式结果");
        assert!(
            legacy_cache_exists(&state_dir),
            "resolver 失败后必须降级 legacy 并维护旧缓存"
        );
        assert!(
            !format!("{report:?}").contains("sk-gui-test"),
            "报告不得泄露 API key"
        );

        std::fs::remove_dir_all(&state_dir).unwrap();
    }

    #[test]
    fn ai_chat_without_config_fails_fast() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        engine.start_ai_chat_stream(
            11,
            rdm_ai::AiConfig::default(),
            "you are a helper".to_string(),
            "hello".to_string(),
        );
        let messages = drain_until_finished(&runtime, &mut rx, Duration::from_secs(10));
        let error = messages
            .iter()
            .find_map(|msg| match msg {
                EngineMsg::AiChatFailed { request_id, error } => Some((*request_id, error.clone())),
                _ => None,
            })
            .expect("超时未收到 AiChatFailed");
        assert_eq!(error.0, 11);
        assert!(!error.1.is_empty());
    }

    #[test]
    fn engine_msg_debug_covers_new_variants() {
        let routing = EngineMsg::Routing {
            id: "dl_1".to_string(),
            run_id: 1,
            dir: PathBuf::from("C:/dl/视频"),
            reason: "后缀".to_string(),
        };
        assert!(format!("{routing:?}").contains("Routing"));

        let finished = EngineMsg::AiParseFinished {
            request_id: 2,
            report: parse_report(),
        };
        assert!(format!("{finished:?}").contains("AiParseFinished"));

        let failed = EngineMsg::AiParseFailed {
            request_id: 3,
            error: "bad url".to_string(),
        };
        assert!(format!("{failed:?}").contains("bad url"));

        let chat = EngineMsg::AiChatFinished {
            request_id: 4,
            reply: "ok".to_string(),
        };
        assert!(format!("{chat:?}").contains("AiChatFinished"));

        let delta = EngineMsg::AiChatDelta {
            request_id: 4,
            delta: "流式".to_string(),
        };
        let rendered = format!("{delta:?}");
        assert!(rendered.contains("AiChatDelta"));
        assert!(rendered.contains("delta_len"), "{rendered}");

        let chat_failed = EngineMsg::AiChatFailed {
            request_id: 5,
            error: "not configured".to_string(),
        };
        assert!(format!("{chat_failed:?}").contains("not configured"));
    }

    /// 读一次 HTTP 请求（head + 按 Content-Length 的 body），返回 body 文本。
    fn read_http_body(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let (head_end, content_len) = loop {
            let read = stream.read(&mut tmp).unwrap_or(0);
            if read == 0 {
                return String::new();
            }
            buf.extend_from_slice(&tmp[..read]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + len {
                    break (pos + 4, len);
                }
            }
        };
        String::from_utf8_lossy(&buf[head_end..head_end + content_len]).to_string()
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn spawn_sse_server(chunks: Vec<&'static str>) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let body = read_http_body(&mut stream);
            assert!(
                body.contains("\"stream\":true"),
                "请求应带 stream:true，实际 body={body}"
            );
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            stream.write_all(head.as_bytes()).expect("write head");
            stream.flush().expect("flush head");
            for delta in &chunks {
                let data = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{delta}\"}}}}]}}\n\n"
                );
                let frame = format!("{:x}\r\n{data}\r\n", data.len());
                stream.write_all(frame.as_bytes()).expect("write chunk");
                stream.flush().expect("flush chunk");
                std::thread::sleep(Duration::from_millis(25));
            }
            let done = "data: [DONE]\n\n";
            let frame = format!("{:x}\r\n{done}\r\n0\r\n\r\n", done.len());
            stream.write_all(frame.as_bytes()).expect("write done");
            stream.flush().expect("flush done");
        });
        (port, handle)
    }

    fn mock_stream_config(port: u16) -> rdm_ai::AiConfig {
        rdm_ai::AiConfig {
            enabled: true,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            model: "mock-stream".to_string(),
            api_key: "sk-stream-test".to_string(),
            timeout_secs: 5,
        }
    }

    fn collect_stream_messages(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<EngineMsg>,
        timeout: Duration,
    ) -> (String, Option<String>, Option<String>) {
        let deadline = std::time::Instant::now() + timeout;
        let mut deltas = String::new();
        let mut reply = None;
        let mut failure = None;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(EngineMsg::AiChatDelta { request_id, delta }) => {
                    assert_eq!(request_id, 42);
                    deltas.push_str(&delta);
                }
                Ok(EngineMsg::AiChatFinished {
                    request_id,
                    reply: full,
                }) => {
                    assert_eq!(request_id, 42);
                    reply = Some(full);
                    break;
                }
                Ok(EngineMsg::AiChatFailed { request_id, error }) => {
                    assert_eq!(request_id, 42);
                    failure = Some(error);
                    break;
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        (deltas, reply, failure)
    }

    #[test]
    fn ai_chat_stream_forwards_sse_deltas_then_finished() {
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let (port, server) = spawn_sse_server(vec!["答", "案", "流式"]);
        engine.start_ai_chat_stream(
            42,
            mock_stream_config(port),
            "系统提示".to_string(),
            "问题".to_string(),
        );
        let (deltas, reply, failure) = collect_stream_messages(&mut rx, Duration::from_secs(10));
        server.join().expect("mock server thread");
        assert_eq!(failure, None);
        assert_eq!(deltas, "答案流式", "delta 应按到达顺序拼接");
        assert_eq!(reply.as_deref(), Some("答案流式"));
    }

    #[test]
    fn ai_chat_stream_reports_http_error() {
        use std::io::Write;
        let runtime = test_runtime();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = read_http_body(&mut stream);
            let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno";
            stream.write_all(response.as_bytes()).expect("write error");
            stream.flush().expect("flush error");
        });
        engine.start_ai_chat_stream(
            42,
            mock_stream_config(port),
            "系统提示".to_string(),
            "问题".to_string(),
        );
        let (deltas, reply, failure) = collect_stream_messages(&mut rx, Duration::from_secs(10));
        server.join().expect("mock server thread");
        assert!(deltas.is_empty());
        assert_eq!(reply, None);
        let error = failure.expect("应收到 AiChatFailed");
        assert!(error.contains("500"), "错误信息应包含状态码: {error}");
    }
}
