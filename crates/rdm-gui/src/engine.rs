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
//! │ IPC server ──────────┘    │ ─────▶ │   { weak.update(...) }       │
//! └───────────────────────────┘        └──────────────────────────────┘
//! ```
//!
//! 注意：**不要**在 GPUI foreground future 里用 `tokio::time::sleep/tokio::spawn`
//! （GPUI executor 没有 tokio reactor 上下文，会 panic）。定时器统一在 tokio 侧
//! 生成 `EngineMsg::Tick`。

use std::time::Duration;

use rdm_core::{DownloadError, DownloadEvent, DownloadHandle, DownloadRequest, DownloadSummary};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

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
            Self::Tick => f.write_str("Tick"),
            Self::Ipc { envelope, .. } => f.debug_struct("Ipc").field("id", &envelope.id).finish(),
        }
    }
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
    pub fn start_download(
        &self,
        id: String,
        run_id: u64,
        request: DownloadRequest,
        handle: DownloadHandle,
    ) {
        let events_tx = self.ui_tx.clone();
        let finished_tx = self.ui_tx.clone();
        self.runtime.spawn(async move {
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
            let _ = finished_tx.send(EngineMsg::Finished { id, run_id, result });
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 核心目前是 stub（返回 Unsupported），但未来实现后这个测试仍然成立：
    /// 用非法 scheme 保证引擎快速返回错误，不发网络请求 / 不依赖测试环境。
    #[test]
    fn engine_finishes_with_error_for_invalid_request() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = EngineRuntime::new(runtime.handle().clone(), tx);

        engine.start_download(
            "dl_000001".to_string(),
            7,
            DownloadRequest::new("not-a-valid-url"),
            DownloadHandle::new(),
        );

        let finished = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match rx.recv().await {
                        Some(EngineMsg::Finished { id, run_id, result }) => {
                            return Some((id, run_id, result.is_err()));
                        }
                        Some(_) => continue,
                        None => return None,
                    }
                }
            })
            .await
        });

        let (id, run_id, is_err) = finished.expect("超时未收到 Finished").expect("通道关闭");
        assert_eq!(id, "dl_000001");
        assert_eq!(run_id, 7);
        assert!(is_err, "非法 URL 应返回错误");
    }

    #[test]
    fn ticker_emits_ticks_without_panicking() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
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
}
