//! 可复现的内核基准：在进程内起 `rdm-fixture-server`，用公开 API `rdm_core::download`
//! 下载确定性内容，输出人类可读表格 + 稳定 JSON。
//!
//! 设计目标：
//! * **确定性**：fixture 内容由 seed 生成，塑形全部由 tokio 定时器驱动；不访问公网。
//! * **诚实**：每个样本都逐字节比对下载文件与 `server.content()`，不一致即报错；
//!   未跑成的场景在 JSON 里显式记录错误，而不是静默跳过。
//! * **可复现**：场景参数固化在本文件（见 [`all_scenarios`]），命令与指标定义见
//!   `docs/bench/kernel-benchmark-method.md`。
//!
//! 用法（Windows PowerShell，cargo 在 PATH）：
//!
//! ```powershell
//! cargo run -p rdm-core --example bench_kernel -- --scenario small --repeats 1
//! cargo run -p rdm-core --example bench_kernel -- --scenario all --repeats 3 `
//!   --out bench-out/kernel-baseline-2026-10-22.json
//! ```
//!
//! CLI：`--scenario <all|plateau|linear|decline|decline6|pushback|drift|latency|tail|straggler|small>`、
//! `--connections <N>`（默认 8，`small` 场景固定测 1/4/8）、
//! `--dynamic <true|false|both>`（默认 true；`both` = 同场景/连接数先动态后静态成对比较）、
//! `--size-multiplier <N>`（默认 1，上限 16：size/slow_tail 按比例放大，用于把场景拉长到
//! 动态控制器 down-probe 周期之上）、`--chunk-size <bytes>`（可选，覆盖场景默认值）、
//! `--repeats <K>`（默认 3）、`--out <path>`（可选，写 JSON）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rdm_core::{download, DownloadEvent, DownloadRequest, DownloadSummary};
use rdm_fixture_server::{ConcurrencyPenalty, Config, Stats, TestServer};
use serde_json::{json, Value};

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// 场景定义（数值即报告的一部分；改动请同步文档）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScenarioName {
    Plateau,
    Linear,
    Decline,
    Decline6,
    Pushback,
    Drift,
    Latency,
    Tail,
    Straggler,
    Small,
}

impl ScenarioName {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "plateau" => Self::Plateau,
            "linear" => Self::Linear,
            "decline" => Self::Decline,
            "decline6" => Self::Decline6,
            "pushback" => Self::Pushback,
            "drift" => Self::Drift,
            "latency" => Self::Latency,
            "tail" => Self::Tail,
            "straggler" => Self::Straggler,
            "small" => Self::Small,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Plateau => "plateau",
            Self::Linear => "linear",
            Self::Decline => "decline",
            Self::Decline6 => "decline6",
            Self::Pushback => "pushback",
            Self::Drift => "drift",
            Self::Latency => "latency",
            Self::Tail => "tail",
            Self::Straggler => "straggler",
            Self::Small => "small",
        }
    }
}

const ALL_SCENARIO_NAMES: &str =
    "all|plateau|linear|decline|decline6|pushback|drift|latency|tail|straggler|small";

/// 一个小型连接数网格：`small` 场景固定比较 1/4/8 连接。
const SMALL_CONNECTION_GRID: &[usize] = &[1, 4, 8];

/// 一个基准场景的完整服务器塑形参数 + 客户端参数。
#[derive(Debug, Clone)]
struct ScenarioSpec {
    name: ScenarioName,
    /// 写进报告的人类可读意图。
    description: &'static str,
    size: u64,
    shared_rate_bps: Option<u64>,
    per_connection_rate_bps: Option<u64>,
    max_concurrent_requests: Option<usize>,
    reject_retry_after_secs: u64,
    request_latency_ms: u64,
    slow_tail_bytes: Option<u64>,
    slow_tail_factor: u64,
    /// 每隔 N 个请求把整条响应体降速 1/slow_request_factor 倍（慢连接/坏边缘）。
    slow_request_every: Option<u64>,
    /// 慢请求的降速倍数（默认 16）。
    slow_request_factor: u64,
    /// 精确慢请求序号（确定性 straggler 场景）。
    slow_request_ordinals: Vec<u64>,
    concurrency_penalty: Option<ConcurrencyPenalty>,
    /// 场景默认 chunk size（`--chunk-size` 可覆盖）。
    default_chunk_size: Option<u64>,
    /// 非空时忽略 `--connections`，按此网格逐个测量。
    connection_grid: &'static [usize],
    /// 传输到 50% 时把 shared rate 提到该值（drift 场景）。
    drift_shared_rate_bps: Option<u64>,
}

impl ScenarioSpec {
    fn drift(&self) -> bool {
        self.drift_shared_rate_bps.is_some()
    }

    /// 按 `--size-multiplier` 放大场景规模。
    ///
    /// * `size` 直接翻倍；
    /// * `slow_tail_bytes` 保持原来的**比例**（`tail / size`），这样 tail 场景
    ///   仍是“末尾 10%”；
    /// * drift 的 50% 触发点按进度计算，自动随之缩放；
    /// * 限速/延迟/并发上限等“网络条件”不缩放，否则就不再是同一场景。
    fn scaled(&self, multiplier: u64) -> ScenarioSpec {
        if multiplier <= 1 {
            return self.clone();
        }
        let mut scaled = self.clone();
        scaled.size = self.size.saturating_mul(multiplier);
        if let Some(tail) = self.slow_tail_bytes {
            let ratio_preserved =
                (scaled.size as u128 * tail as u128 / self.size.max(1) as u128) as u64;
            scaled.slow_tail_bytes = Some(ratio_preserved.max(1));
        }
        scaled
    }
}

fn scenario_spec(name: ScenarioName) -> ScenarioSpec {
    match name {
        ScenarioName::Plateau => ScenarioSpec {
            name,
            description: "共享 16 MiB/s + 单连接 8 MiB/s：2 连接即可跑满共享链路，\
                          更多连接不应带来收益（动态规划应回落到 ~2）",
            size: 32 * MIB,
            shared_rate_bps: Some(16 * MIB),
            per_connection_rate_bps: Some(8 * MIB),
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: None,
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Linear => ScenarioSpec {
            name,
            description: "共享 64 MiB/s + 单连接 8 MiB/s：8 连接时线性扩展到共享上限\
                          （最优连接数 = 8）",
            size: 64 * MIB,
            shared_rate_bps: Some(64 * MIB),
            per_connection_rate_bps: Some(8 * MIB),
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: None,
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Decline => ScenarioSpec {
            name,
            description: "单连接 16 MiB/s，但在途请求 > 4 后每多一个连接再降速：\
                          聚合吞吐在 4 连接附近见顶后下降（动态应主动回退）",
            size: 32 * MIB,
            shared_rate_bps: None,
            per_connection_rate_bps: Some(16 * MIB),
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: Some(ConcurrencyPenalty {
                free_inflight: 4,
                extra_slowdown: 1,
            }),
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Decline6 => ScenarioSpec {
            name,
            description:
                "单连接 8 MiB/s，在途 >6 后每多一个连接降速：最优并发是 6（8 被惩罚、4 欠载），\
                          检验减半下探后的中点回查能否找到 6",
            size: 32 * MIB,
            shared_rate_bps: None,
            per_connection_rate_bps: Some(8 * MIB),
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: Some(ConcurrencyPenalty {
                free_inflight: 6,
                extra_slowdown: 1,
            }),
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Pushback => ScenarioSpec {
            name,
            description: "服务器并发上限 4：超出的请求立刻 429 + Retry-After 1s，\
                          检验内核退避 / 动态连接回收后仍能完成",
            size: 32 * MIB,
            shared_rate_bps: None,
            per_connection_rate_bps: None,
            max_concurrent_requests: Some(4),
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: None,
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Drift => ScenarioSpec {
            name,
            description: "共享链路前 50% 为 16 MiB/s，传输到 50% 后升到 64 MiB/s：\
                          检验运行中速率变化下的自适应",
            size: 32 * MIB,
            shared_rate_bps: Some(16 * MIB),
            per_connection_rate_bps: None,
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: None,
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: Some(64 * MIB),
        },
        ScenarioName::Latency => ScenarioSpec {
            name,
            description: "线性吞吐（shared 64 / per-conn 8 MiB/s）叠加 80 ms RTT：\
                          衡量高延迟下多连接的收益与探测开销",
            size: 32 * MIB,
            shared_rate_bps: Some(64 * MIB),
            per_connection_rate_bps: Some(8 * MIB),
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 80,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: None,
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Tail => ScenarioSpec {
            name,
            description: "共享 64 MiB/s，末尾 10% 按 1/16 限速（4 MiB/s，模拟 CDN 慢尾块）：\
                          检验工作窃取能否掩盖慢尾",
            size: 32 * MIB,
            shared_rate_bps: Some(64 * MIB),
            per_connection_rate_bps: None,
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: Some(32 * MIB / 10),
            slow_tail_factor: 16,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: None,
            default_chunk_size: None,
            connection_grid: &[],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Straggler => ScenarioSpec {
            name,
            description: "共享 64 MiB/s，第一条 payload GET 整段降速 1/16（确定性慢连接）：\
                          检验范围窃取能否让空闲 worker 接管慢段（RDM_DISABLE_STEAL=1 可对照）",
            size: 16 * MIB,
            shared_rate_bps: Some(64 * MIB),
            per_connection_rate_bps: Some(8 * MIB),
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 0,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: vec![2],
            concurrency_penalty: None,
            // First payload GET is slow: 4 MiB at 1/16 rate ≈ 8 s if unsplit.
            default_chunk_size: Some(4 * MIB),
            connection_grid: &[4],
            drift_shared_rate_bps: None,
        },
        ScenarioName::Small => ScenarioSpec {
            name,
            description: "1 MiB 小文件 + 40 ms RTT、256 KiB chunk：比较 1/4/8 连接的\
                          RTT 摊薄效果（`--connections` 在 small 场景被忽略）",
            size: MIB,
            shared_rate_bps: None,
            per_connection_rate_bps: None,
            max_concurrent_requests: None,
            reject_retry_after_secs: 1,
            request_latency_ms: 40,
            slow_tail_bytes: None,
            slow_tail_factor: 8,
            slow_request_every: None,
            slow_request_factor: 16,
            slow_request_ordinals: Vec::new(),
            concurrency_penalty: None,
            default_chunk_size: Some(256 * 1024),
            connection_grid: SMALL_CONNECTION_GRID,
            drift_shared_rate_bps: None,
        },
    }
}

fn all_scenarios() -> Vec<ScenarioSpec> {
    [
        ScenarioName::Plateau,
        ScenarioName::Linear,
        ScenarioName::Decline,
        ScenarioName::Decline6,
        ScenarioName::Pushback,
        ScenarioName::Drift,
        ScenarioName::Latency,
        ScenarioName::Tail,
        ScenarioName::Straggler,
        ScenarioName::Small,
    ]
    .into_iter()
    .map(scenario_spec)
    .collect()
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// `--dynamic` 的三种模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicMode {
    /// 只跑 `dynamic_connections = false`（静态连接数基线）。
    Static,
    /// 只跑 `dynamic_connections = true`（默认）。
    Dynamic,
    /// 同一场景/连接数先跑 dynamic=true 再跑 dynamic=false，成对比较。
    Both,
}

impl DynamicMode {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "true" | "1" | "yes" | "on" => Self::Dynamic,
            "false" | "0" | "no" | "off" => Self::Static,
            "both" | "compare" => Self::Both,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Dynamic => "dynamic",
            Self::Both => "both",
        }
    }

    /// 依次要跑的 `dynamic_connections` 取值；`Both` 先动态后静态。
    fn sequence(self) -> &'static [bool] {
        match self {
            Self::Static => &[false],
            Self::Dynamic => &[true],
            Self::Both => &[true, false],
        }
    }
}

/// 场景规模缩放上限：`size * multiplier <= 16 * 64 MiB = 1 GiB`。
const MAX_SIZE_MULTIPLIER: u64 = 16;

struct Args {
    /// `None` = 全部场景。
    scenario: Option<ScenarioName>,
    connections: usize,
    dynamic: DynamicMode,
    chunk_size: Option<u64>,
    repeats: usize,
    out: Option<PathBuf>,
    /// 场景规模倍数：size / slow_tail 按比例放大（drift 触发点随进度自动缩放）。
    size_multiplier: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            scenario: None,
            connections: 8,
            dynamic: DynamicMode::Dynamic,
            chunk_size: None,
            repeats: 3,
            out: None,
            size_multiplier: 1,
        }
    }
}

fn usage() -> String {
    format!(
        "usage: bench_kernel [--scenario <{}>] [--connections N]\n\
         \x20                  [--dynamic true|false|both] [--size-multiplier N]\n\
         \x20                  [--chunk-size BYTES] [--repeats K] [--out PATH]\n\n\
         --dynamic both: 同一场景/连接数先跑 dynamic=true 再跑 dynamic=false（各 repeats 次）\n\
         --size-multiplier N: 场景规模（size、slow_tail 等按比例）放大 N 倍（1..={}），\n\
         用于把 plateau/decline 拉长到控制器 down-probe 周期（~5s）以上\n\n\
         场景参数（size / shared_rate / per_connection_rate / concurrency / latency / tail）\n\
         固化在 crates/rdm-core/examples/bench_kernel.rs，指标定义见\n\
         docs/bench/kernel-benchmark-method.md。",
        ALL_SCENARIO_NAMES, MAX_SIZE_MULTIPLIER
    )
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut next_value = |name: &str| -> Result<String> {
            iter.next()
                .ok_or_else(|| anyhow::anyhow!("缺少 {name} 的取值\n\n{}", usage()))
        };
        match flag.as_str() {
            "--scenario" => {
                let value = next_value("--scenario")?;
                args.scenario = if value == "all" {
                    None
                } else {
                    Some(ScenarioName::parse(&value).ok_or_else(|| {
                        anyhow::anyhow!("未知场景 `{value}`；可选：{ALL_SCENARIO_NAMES}")
                    })?)
                };
            }
            "--connections" => {
                args.connections = next_value("--connections")?
                    .parse()
                    .context("--connections 需要正整数")?;
                if args.connections == 0 {
                    bail!("--connections 必须 >= 1");
                }
            }
            "--dynamic" => {
                let value = next_value("--dynamic")?;
                args.dynamic = DynamicMode::parse(&value).ok_or_else(|| {
                    anyhow::anyhow!("--dynamic 只接受 true/false/both，收到 `{value}`")
                })?;
            }
            "--size-multiplier" => {
                args.size_multiplier = next_value("--size-multiplier")?
                    .parse()
                    .context("--size-multiplier 需要正整数")?;
                if args.size_multiplier == 0 || args.size_multiplier > MAX_SIZE_MULTIPLIER {
                    bail!("--size-multiplier 必须在 1..={MAX_SIZE_MULTIPLIER} 之间");
                }
            }
            "--chunk-size" => {
                args.chunk_size = Some(
                    next_value("--chunk-size")?
                        .parse()
                        .context("--chunk-size 需要字节数")?,
                );
            }
            "--repeats" => {
                args.repeats = next_value("--repeats")?
                    .parse()
                    .context("--repeats 需要整数")?;
                if args.repeats == 0 {
                    bail!("--repeats 必须 >= 1");
                }
            }
            "--out" => args.out = Some(PathBuf::from(next_value("--out")?)),
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => bail!("未知参数 `{other}`\n\n{}", usage()),
        }
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// 单次运行
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct EventStats {
    total: u64,
    progress: u64,
    concurrency: u64,
    retrying: u64,
    /// `DownloadEvent::Concurrency` 里出现过的最大 target。
    max_target: usize,
    /// `DownloadEvent::Concurrency` 的完整轨迹 `(target, active)`：动态控制器
    /// 下调/上调连接的证据（按事件顺序）。
    concurrency_trace: Vec<(usize, usize)>,
    /// `DownloadEvent::Progress.active_workers` 的峰值（实测并发）。
    max_active_workers: u64,
}

/// 单次下载的全部观测值（JSON 里的一个 `samples[]` 元素）。
#[derive(Debug)]
struct Sample {
    connections_requested: usize,
    dynamic: bool,
    chunk_size: Option<u64>,
    /// 本工具实测的完整任务耗时（从发起 download 到返回 summary）。
    elapsed_ms: u64,
    /// 内核 summary 自报耗时，用于交叉校验。
    summary_elapsed_ms: u64,
    /// `total_bytes / elapsed`，有效吞吐（MiB/s）。
    effective_mib_s: f64,
    /// 内核 summary 的 `avg_speed_bps`（MiB/s）。
    avg_speed_mib_s: f64,
    plan_connections: usize,
    peak_connections: u32,
    total_bytes: u64,
    server_body_bytes_sent: u64,
    server_requests: u64,
    server_get_requests: u64,
    server_head_requests: u64,
    server_responses_429: u64,
    server_rejected_concurrency: u64,
    server_max_inflight: u64,
    server_shared_throttle_waits: u64,
    event_total: u64,
    event_progress: u64,
    event_concurrency: u64,
    event_retrying: u64,
    max_concurrency_target: usize,
    concurrency_trace: Vec<(usize, usize)>,
    max_active_workers: u64,
    drift_switch_applied: bool,
    verified: bool,
}

#[derive(Debug, Default)]
struct ServerDelta {
    body_bytes_sent: u64,
    requests: u64,
    get_requests: u64,
    head_requests: u64,
    responses_429: u64,
    rejected_concurrency: u64,
    max_inflight_requests: u64,
    shared_throttle_waits: u64,
}

impl ServerDelta {
    fn between(before: &Stats, after: &Stats) -> Self {
        Self {
            body_bytes_sent: after.body_bytes_sent.saturating_sub(before.body_bytes_sent),
            requests: after.requests.saturating_sub(before.requests),
            get_requests: after.get_requests.saturating_sub(before.get_requests),
            head_requests: after.head_requests.saturating_sub(before.head_requests),
            responses_429: after.responses_429.saturating_sub(before.responses_429),
            rejected_concurrency: after
                .rejected_concurrency
                .saturating_sub(before.rejected_concurrency),
            max_inflight_requests: after
                .max_inflight_requests
                .max(before.max_inflight_requests),
            shared_throttle_waits: after
                .shared_throttle_waits
                .saturating_sub(before.shared_throttle_waits),
        }
    }
}

fn server_config(spec: &ScenarioSpec) -> Config {
    let mut config = Config::new(spec.size);
    if let Some(rate) = spec.shared_rate_bps {
        config = config.with_shared_rate(rate);
    }
    if let Some(rate) = spec.per_connection_rate_bps {
        config = config.with_per_connection_rate(rate);
    }
    if let Some(limit) = spec.max_concurrent_requests {
        config = config.with_concurrency_limit(limit);
    }
    if spec.reject_retry_after_secs != 1 {
        config = config.with_reject_retry_after(spec.reject_retry_after_secs);
    }
    if spec.request_latency_ms > 0 {
        config = config.with_request_latency(spec.request_latency_ms);
    }
    if let Some(tail) = spec.slow_tail_bytes {
        config = config
            .with_slow_tail(tail)
            .with_slow_tail_factor(spec.slow_tail_factor);
    }
    if let Some(every) = spec.slow_request_every {
        config = config
            .with_slow_request_every(every)
            .with_slow_request_factor(spec.slow_request_factor);
    }
    if !spec.slow_request_ordinals.is_empty() {
        config = config
            .with_slow_request_ordinals(spec.slow_request_ordinals.iter().copied())
            .with_slow_request_factor(spec.slow_request_factor);
    }
    if let Some(penalty) = spec.concurrency_penalty {
        config = config.with_concurrency_penalty(penalty.free_inflight, penalty.extra_slowdown);
    }
    config
}

/// 等待服务器计数稳定（客户端结束后可能还有最后几字节在 socket 缓冲里）。
async fn settle_stats(server: &TestServer) -> Stats {
    let mut last = server.stats();
    let mut stable = 0;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let now = server.stats();
        if now.body_bytes_sent == last.body_bytes_sent && now.requests == last.requests {
            stable += 1;
            if stable >= 2 {
                return now;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    server.stats()
}

fn count_event(stats: &mut EventStats, event: &DownloadEvent) {
    stats.total += 1;
    match event {
        DownloadEvent::Progress { active_workers, .. } => {
            stats.progress += 1;
            stats.max_active_workers = stats.max_active_workers.max(*active_workers as u64);
        }
        DownloadEvent::Concurrency { target, active } => {
            stats.concurrency += 1;
            stats.max_target = stats.max_target.max(*target);
            stats.concurrency_trace.push((*target, *active));
        }
        DownloadEvent::Retrying { .. } => stats.retrying += 1,
        _ => {}
    }
}

/// 在同一个任务里跑 download 并消费事件（drift 场景在 50% 时修改服务器速率）。
///
/// 用 `tokio::select!` 而不是 `tokio::spawn`，避免对 future 的 `Send` 额外假设，
/// 也保证事件计数不会在 summary 返回后丢失。
async fn download_with_events(
    request: DownloadRequest,
    drift_server: Option<&TestServer>,
    drift_rate_bps: Option<u64>,
) -> Result<(DownloadSummary, EventStats, bool)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut stats = EventStats::default();
    let mut drift_switch_applied = false;

    let download_future = download(request, Some(tx));
    tokio::pin!(download_future);

    let mut outcome = None;
    while outcome.is_none() {
        tokio::select! {
            maybe_event = rx.recv() => {
                if let Some(event) = maybe_event {
                    count_event(&mut stats, &event);
                    if let (
                        DownloadEvent::Progress { downloaded, total, .. },
                        Some(server),
                        Some(rate),
                    ) = (&event, drift_server, drift_rate_bps)
                    {
                        let reached_half =
                            total.is_none_or(|total| downloaded.saturating_mul(2) >= total);
                        if !drift_switch_applied && reached_half && *downloaded > 0 {
                            server.set_shared_rate(Some(rate)).await;
                            drift_switch_applied = true;
                        }
                    }
                }
                // `None` 只会在 download future 结束后出现；下一轮 select 会
                // 立刻从 future 分支退出循环。
            }
            result = &mut download_future => {
                outcome = Some(result);
            }
        }
    }

    // 排空 download 返回前已经入队的事件（例如 Completed）。
    while let Ok(event) = rx.try_recv() {
        count_event(&mut stats, &event);
    }

    let summary = outcome
        .expect("download future completed")
        .context("download 失败")?;
    Ok((summary, stats, drift_switch_applied))
}

fn verify_payload(actual: &[u8], expected: &[u8]) -> Result<()> {
    if actual == expected {
        return Ok(());
    }
    let limit = actual.len().min(expected.len());
    let at = (0..limit)
        .find(|index| actual[*index] != expected[*index])
        .unwrap_or(limit);
    bail!(
        "下载内容与 fixture 不一致：首个差异字节 {at}（实际 {} 字节，期望 {} 字节）",
        actual.len(),
        expected.len()
    );
}

async fn run_sample(
    spec: &ScenarioSpec,
    connections: usize,
    dynamic: bool,
    args: &Args,
) -> Result<Sample> {
    // Samples start independent servers with ephemeral ports that can be
    // reused; do not let a learned origin cap leak across samples.
    rdm_core::testing::reset_origin_budgets();
    let server = TestServer::start(server_config(spec))
        .await
        .context("启动 fixture server 失败")?;
    let expected = server.content();
    let temp = TempDir::new(spec.name.as_str()).context("创建临时目录失败")?;
    let chunk_size = args.chunk_size.or(spec.default_chunk_size);

    let mut request = DownloadRequest::new(server.file_url());
    request.output_dir = Some(temp.path().to_path_buf());
    request.connections = connections;
    request.dynamic_connections = dynamic;
    request.chunk_size = chunk_size;
    request.resume = false;

    let before = server.stats();
    let started = Instant::now();
    let (summary, event_stats, drift_switch_applied) = download_with_events(
        request,
        if spec.drift() { Some(&server) } else { None },
        spec.drift_shared_rate_bps,
    )
    .await?;
    let elapsed = started.elapsed();
    let after = settle_stats(&server).await;
    let delta = ServerDelta::between(&before, &after);

    let actual = std::fs::read(&summary.path).with_context(|| {
        format!(
            "读取下载文件失败（summary.path = {}）",
            summary.path.display()
        )
    })?;
    verify_payload(&actual, &expected)?;

    let elapsed_ms = elapsed.as_millis() as u64;
    let effective_mib_s = if elapsed.as_secs_f64() > 0.0 {
        summary.total_bytes as f64 / elapsed.as_secs_f64() / MIB as f64
    } else {
        0.0
    };

    server.shutdown().await;

    Ok(Sample {
        connections_requested: connections,
        dynamic,
        chunk_size,
        elapsed_ms,
        summary_elapsed_ms: summary.elapsed_ms,
        effective_mib_s,
        avg_speed_mib_s: summary.avg_speed_bps / MIB as f64,
        plan_connections: summary.connections,
        peak_connections: summary.peak_connections,
        total_bytes: summary.total_bytes,
        server_body_bytes_sent: delta.body_bytes_sent,
        server_requests: delta.requests,
        server_get_requests: delta.get_requests,
        server_head_requests: delta.head_requests,
        server_responses_429: delta.responses_429,
        server_rejected_concurrency: delta.rejected_concurrency,
        server_max_inflight: delta.max_inflight_requests,
        server_shared_throttle_waits: delta.shared_throttle_waits,
        event_total: event_stats.total,
        event_progress: event_stats.progress,
        event_concurrency: event_stats.concurrency,
        event_retrying: event_stats.retrying,
        max_concurrency_target: event_stats.max_target,
        concurrency_trace: event_stats.concurrency_trace,
        max_active_workers: event_stats.max_active_workers,
        drift_switch_applied,
        verified: true,
    })
}

// ---------------------------------------------------------------------------
// 聚合 / 报告
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct GroupReport {
    connections: usize,
    dynamic: bool,
    samples: Vec<Sample>,
    errors: Vec<String>,
}

impl GroupReport {
    fn median_elapsed_ms(&self) -> Option<u64> {
        median_u64(self.samples.iter().map(|sample| sample.elapsed_ms))
    }

    fn min_elapsed_ms(&self) -> Option<u64> {
        self.samples.iter().map(|sample| sample.elapsed_ms).min()
    }

    fn max_elapsed_ms(&self) -> Option<u64> {
        self.samples.iter().map(|sample| sample.elapsed_ms).max()
    }

    fn median_effective_mib_s(&self) -> Option<f64> {
        median_f64(self.samples.iter().map(|sample| sample.effective_mib_s))
    }

    fn median_avg_speed_mib_s(&self) -> Option<f64> {
        median_f64(self.samples.iter().map(|sample| sample.avg_speed_mib_s))
    }

    /// 所有样本的 `Concurrency.target` 轨迹（按样本顺序拼接）。
    fn target_trace(&self) -> Vec<usize> {
        self.samples
            .iter()
            .flat_map(|sample| sample.concurrency_trace.iter().map(|(target, _)| *target))
            .collect()
    }

    /// 轨迹中出现过的最小 target（动态控制器下探到的最低计划连接数）。
    fn min_concurrency_target(&self) -> Option<usize> {
        self.samples
            .iter()
            .flat_map(|sample| sample.concurrency_trace.iter().map(|(target, _)| *target))
            .min()
    }

    fn max_429(&self) -> u64 {
        self.samples
            .iter()
            .map(|sample| sample.server_responses_429)
            .max()
            .unwrap_or(0)
    }

    fn max_inflight(&self) -> u64 {
        self.samples
            .iter()
            .map(|sample| sample.server_max_inflight)
            .max()
            .unwrap_or(0)
    }

    fn median_peak_connections(&self) -> Option<u32> {
        median_u64(
            self.samples
                .iter()
                .map(|sample| sample.peak_connections as u64),
        )
        .map(|value| value as u32)
    }
}

#[derive(Debug)]
struct ScenarioRun {
    spec: ScenarioSpec,
    groups: Vec<GroupReport>,
}

fn median_u64(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut values: Vec<u64> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    // 偶数个样本时取上中位数，保证 median 是真实出现过的观测值。
    Some(values[values.len() / 2])
}

fn median_f64(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut values: Vec<f64> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

fn size_label(bytes: u64) -> String {
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn rate_label(bps: u64) -> String {
    if bps >= MIB && bps.is_multiple_of(MIB) {
        format!("{} MiB/s", bps / MIB)
    } else if bps >= 1024 && bps.is_multiple_of(1024) {
        format!("{} KiB/s", bps / 1024)
    } else {
        format!("{bps} B/s")
    }
}

fn describe_server(spec: &ScenarioSpec) -> String {
    let mut parts = vec![format!("size={}", size_label(spec.size))];
    if let Some(value) = spec.shared_rate_bps {
        parts.push(format!("shared={}", rate_label(value)));
    }
    if let Some(value) = spec.per_connection_rate_bps {
        parts.push(format!("per-conn={}", rate_label(value)));
    }
    if let Some(value) = spec.max_concurrent_requests {
        parts.push(format!(
            "max-concurrent={value}(retry-after={}s)",
            spec.reject_retry_after_secs
        ));
    }
    if spec.request_latency_ms > 0 {
        parts.push(format!("rtt={}ms", spec.request_latency_ms));
    }
    if let Some(value) = spec.slow_tail_bytes {
        parts.push(format!(
            "slow-tail={}@1/{}",
            size_label(value),
            spec.slow_tail_factor
        ));
    }
    if let Some(penalty) = spec.concurrency_penalty {
        parts.push(format!(
            "penalty=free{}:+1/{}",
            penalty.free_inflight, penalty.extra_slowdown
        ));
    }
    if let Some(value) = spec.drift_shared_rate_bps {
        parts.push(format!("drift→{} at 50%", rate_label(value)));
    }
    parts.join(" ")
}

fn print_group_tables(spec: &ScenarioSpec, groups: &[GroupReport], mode: DynamicMode) {
    println!("\n  per-run samples:");
    println!(
        "  {:<8} {:>7} {:>4} {:>10} {:>9} {:>10} {:>10} {:>5} {:>5} {:>8} {:>7} {:>8} {:>6} {:>8} {:>6} {:>7}  verify",
        "conns", "dynamic", "run", "elapsed", "task_ms", "eff MiB/s", "avg MiB/s", "peak", "plan",
        "maxTarget", "events", "retrying", "429", "rejected", "inflight", "waits"
    );
    for group in groups {
        for (index, sample) in group.samples.iter().enumerate() {
            println!(
                "  {:<8} {:>7} {:>4} {:>10} {:>9} {:>10.2} {:>10.2} {:>5} {:>5} {:>8} {:>7} {:>8} {:>6} {:>8} {:>6} {:>7}  {}",
                sample.connections_requested,
                if sample.dynamic { "on" } else { "off" },
                index + 1,
                format!("{}ms", sample.elapsed_ms),
                sample.summary_elapsed_ms,
                sample.effective_mib_s,
                sample.avg_speed_mib_s,
                sample.peak_connections,
                sample.plan_connections,
                sample.max_concurrency_target,
                sample.event_total,
                sample.event_retrying,
                sample.server_responses_429,
                sample.server_rejected_concurrency,
                sample.server_max_inflight,
                sample.server_shared_throttle_waits,
                if sample.verified { "ok" } else { "MISMATCH" }
            );
            // 动态控制器的 target 轨迹是“是否主动下探/回收连接”的直接证据。
            if sample.dynamic && !sample.concurrency_trace.is_empty() {
                let trace = sample
                    .concurrency_trace
                    .iter()
                    .map(|(target, active)| format!("{target}({active})"))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                println!("  {:<8} {:>7} {:>4} target 轨迹: {trace}", "", "", "");
            }
        }
        for error in &group.errors {
            println!(
                "  !! {} connections (dynamic={}): 运行失败：{error}",
                group.connections,
                if group.dynamic { "on" } else { "off" }
            );
        }
    }

    println!("\n  summary (中位数 + 全样本范围):");
    println!(
        "  {:<8} {:>7} {:>5} {:>12} {:>10} {:>10} {:>14} {:>12} {:>5} {:>9} {:>8} {:>7}",
        "conns",
        "dynamic",
        "runs",
        "median",
        "min",
        "max",
        "median eff",
        "median avg",
        "peak",
        "minTarget",
        "max 429",
        "inflight"
    );
    for group in groups {
        let median = group
            .median_elapsed_ms()
            .map(|value| format!("{value}ms"))
            .unwrap_or_else(|| "n/a".to_string());
        let min = group
            .min_elapsed_ms()
            .map(|value| format!("{value}ms"))
            .unwrap_or_else(|| "n/a".to_string());
        let max = group
            .max_elapsed_ms()
            .map(|value| format!("{value}ms"))
            .unwrap_or_else(|| "n/a".to_string());
        let eff = group
            .median_effective_mib_s()
            .map(|value| format!("{value:.2} MiB/s"))
            .unwrap_or_else(|| "n/a".to_string());
        let avg = group
            .median_avg_speed_mib_s()
            .map(|value| format!("{value:.2} MiB/s"))
            .unwrap_or_else(|| "n/a".to_string());
        let peak = group
            .median_peak_connections()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string());
        let min_target = group
            .min_concurrency_target()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string());
        println!(
            "  {:<8} {:>7} {:>5} {:>12} {:>10} {:>10} {:>14} {:>12} {:>5} {:>9} {:>8} {:>7}",
            group.connections,
            if group.dynamic { "on" } else { "off" },
            format!(
                "{}/{}",
                group.samples.len(),
                group.samples.len() + group.errors.len()
            ),
            median,
            min,
            max,
            eff,
            avg,
            peak,
            min_target,
            group.max_429(),
            group.max_inflight()
        );
    }

    // `--dynamic both`：并排打印动态 vs 静态的 median / 峰值，便于直接读数。
    if mode == DynamicMode::Both {
        println!("\n  dynamic vs static（同一场景/连接数，median）:");
        println!(
            "  {:<8} {:>12} {:>13} {:>9} {:>12} {:>13} {:>9} {:>9}",
            "conns",
            "dyn median",
            "static median",
            "delta",
            "dyn eff",
            "static eff",
            "dyn peak",
            "static peak"
        );
        let mut connections: Vec<usize> = groups.iter().map(|group| group.connections).collect();
        connections.sort_unstable();
        connections.dedup();
        for connections in connections {
            let dynamic = groups
                .iter()
                .find(|group| group.connections == connections && group.dynamic);
            let statik = groups
                .iter()
                .find(|group| group.connections == connections && !group.dynamic);
            let (Some(dynamic), Some(statik)) = (dynamic, statik) else {
                continue;
            };
            let dyn_median = dynamic.median_elapsed_ms();
            let static_median = statik.median_elapsed_ms();
            let delta = match (dyn_median, static_median) {
                (Some(dyn_ms), Some(static_ms)) if static_ms > 0 => {
                    format!(
                        "{:+.1}%",
                        (dyn_ms as f64 - static_ms as f64) / static_ms as f64 * 100.0
                    )
                }
                _ => "n/a".to_string(),
            };
            println!(
                "  {:<8} {:>12} {:>13} {:>9} {:>12} {:>13} {:>9} {:>9}",
                connections,
                dyn_median
                    .map(|value| format!("{value}ms"))
                    .unwrap_or_else(|| "n/a".to_string()),
                static_median
                    .map(|value| format!("{value}ms"))
                    .unwrap_or_else(|| "n/a".to_string()),
                delta,
                dynamic
                    .median_effective_mib_s()
                    .map(|value| format!("{value:.2} MiB/s"))
                    .unwrap_or_else(|| "n/a".to_string()),
                statik
                    .median_effective_mib_s()
                    .map(|value| format!("{value:.2} MiB/s"))
                    .unwrap_or_else(|| "n/a".to_string()),
                dynamic
                    .median_peak_connections()
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
                statik
                    .median_peak_connections()
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
            );
        }
    }
    let _ = spec;
}

fn sample_json(sample: &Sample, repeat: usize) -> Value {
    let concurrency_trace: Vec<Value> = sample
        .concurrency_trace
        .iter()
        .map(|(target, active)| json!({ "target": target, "active": active }))
        .collect();
    json!({
        "repeat": repeat,
        "connections_requested": sample.connections_requested,
        "dynamic": sample.dynamic,
        "chunk_size": sample.chunk_size,
        "elapsed_ms": sample.elapsed_ms,
        "summary_elapsed_ms": sample.summary_elapsed_ms,
        "effective_mib_s": round3(sample.effective_mib_s),
        "avg_speed_mib_s": round3(sample.avg_speed_mib_s),
        "plan_connections": sample.plan_connections,
        "peak_connections": sample.peak_connections,
        "total_bytes": sample.total_bytes,
        "verified": sample.verified,
        "server": {
            "body_bytes_sent": sample.server_body_bytes_sent,
            "requests": sample.server_requests,
            "get_requests": sample.server_get_requests,
            "head_requests": sample.server_head_requests,
            "responses_429": sample.server_responses_429,
            "rejected_concurrency": sample.server_rejected_concurrency,
            "max_inflight_requests": sample.server_max_inflight,
            "shared_throttle_waits": sample.server_shared_throttle_waits,
        },
        "events": {
            "total": sample.event_total,
            "progress": sample.event_progress,
            "concurrency": sample.event_concurrency,
            "retrying": sample.event_retrying,
            "max_concurrency_target": sample.max_concurrency_target,
            "max_active_workers": sample.max_active_workers,
            "concurrency_trace": concurrency_trace,
        },
        "drift_switch_applied": sample.drift_switch_applied,
    })
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn scenario_json(run: &ScenarioRun) -> Value {
    let spec = &run.spec;
    let groups: Vec<Value> = run
        .groups
        .iter()
        .map(|group| {
            let samples: Vec<Value> = group
                .samples
                .iter()
                .enumerate()
                .map(|(repeat, sample)| sample_json(sample, repeat + 1))
                .collect();
            json!({
                "connections": group.connections,
                "dynamic": group.dynamic,
                "runs": group.samples.len(),
                "failures": group.errors.len(),
                "median_elapsed_ms": group.median_elapsed_ms(),
                "min_elapsed_ms": group.min_elapsed_ms(),
                "max_elapsed_ms": group.max_elapsed_ms(),
                "median_effective_mib_s": group.median_effective_mib_s().map(round3),
                "median_avg_speed_mib_s": group.median_avg_speed_mib_s().map(round3),
                "median_peak_connections": group.median_peak_connections(),
                "min_concurrency_target": group.min_concurrency_target(),
                "concurrency_target_trace": group.target_trace(),
                "max_responses_429": group.max_429(),
                "max_inflight_requests": group.max_inflight(),
                "samples": samples,
                "errors": group.errors,
            })
        })
        .collect();
    json!({
        "name": spec.name.as_str(),
        "description": spec.description,
        "server": {
            "size_bytes": spec.size,
            "shared_rate_bps": spec.shared_rate_bps,
            "per_connection_rate_bps": spec.per_connection_rate_bps,
            "max_concurrent_requests": spec.max_concurrent_requests,
            "reject_retry_after_secs": spec.reject_retry_after_secs,
            "request_latency_ms": spec.request_latency_ms,
            "slow_tail_bytes": spec.slow_tail_bytes,
            "slow_tail_factor": spec.slow_tail_factor,
            "concurrency_penalty": spec.concurrency_penalty.map(|penalty| json!({
                "free_inflight": penalty.free_inflight,
                "extra_slowdown": penalty.extra_slowdown,
            })),
            "drift_shared_rate_bps": spec.drift_shared_rate_bps,
            "default_chunk_size": spec.default_chunk_size,
        },
        "groups": groups,
    })
}

fn report_json(args: &Args, started_unix: u64, runs: &[ScenarioRun]) -> Value {
    let scenarios: Vec<Value> = runs.iter().map(scenario_json).collect();
    let failures: usize = runs
        .iter()
        .flat_map(|run| run.groups.iter())
        .map(|group| group.errors.len())
        .sum();
    json!({
        "schema": "rdm-kernel-bench/v1",
        "generated_unix": started_unix,
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "rdm_core_version": env!("CARGO_PKG_VERSION"),
        },
        "args": {
            "scenario": args.scenario.map(ScenarioName::as_str).unwrap_or("all"),
            "connections": args.connections,
            "dynamic": args.dynamic.as_str(),
            "chunk_size": args.chunk_size,
            "repeats": args.repeats,
            "size_multiplier": args.size_multiplier,
        },
        "metric_definitions": {
            "elapsed_ms": "本工具实测的完整任务耗时：从调用 download() 到返回 summary（含探测/规划/传输/校验状态写入）",
            "effective_mib_s": "total_bytes / elapsed_ms（端到端有效吞吐）",
            "avg_speed_mib_s": "内核 summary.avg_speed_bps / 1 MiB（内核自报平均速度）",
            "peak_connections": "summary.peak_connections：实际同时活跃 worker 的峰值",
            "min_concurrency_target": "Concurrency 事件轨迹中出现过的最小 target（动态控制器下探到的最低计划连接数）",
            "concurrency_target_trace": "Concurrency 事件的 target 序列（每个样本内部按事件顺序，多个 repeats 依次拼接）",
            "max_active_workers": "Progress 事件 active_workers 的峰值",
            "max_inflight_requests": "fixture 服务器观测到的同时处理中 GET/HEAD 峰值",
            "responses_429": "fixture 返回的 429 总数（含并发上限拒绝与注入式限流）",
            "rejected_concurrency": "其中因 max_concurrent_requests 被拒的数量",
            "shared_throttle_waits": "服务器共享限速器发生等待的字节片数（shared_rate 关闭时恒为 0）",
        },
        "failures": failures,
        "scenarios": scenarios,
    })
}

// ---------------------------------------------------------------------------
// 临时目录
// ---------------------------------------------------------------------------

/// 自清理临时目录（Drop 时删除，失败忽略）。
///
/// 默认建在系统临时目录下；若不可写（例如受限的 CI / agent 沙箱只允许写工作区），
/// 自动回退到仓库内 `bench-out/tmp/`（`bench-out/` 已 gitignore）。也可用环境变量
/// `RDM_BENCH_TMP` 显式指定可写的父目录。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("rdm-bench-{tag}-{}-{nanos}", std::process::id());
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Ok(root) = std::env::var("RDM_BENCH_TMP") {
            if !root.is_empty() {
                roots.push(PathBuf::from(root));
            }
        }
        roots.push(std::env::temp_dir());
        roots.push(PathBuf::from("bench-out").join("tmp"));

        let mut failures = Vec::new();
        for root in roots {
            let path = root.join(&name);
            let _ = std::fs::remove_dir_all(&path);
            match std::fs::create_dir_all(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        bail!(
            "无法创建临时目录（已尝试：{}）；可用 RDM_BENCH_TMP 指定可写目录",
            failures.join("; ")
        )
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("bench_kernel: {error:#}");
        std::process::exit(2);
    }
}

async fn run() -> Result<()> {
    let args = parse_args()?;
    let specs: Vec<ScenarioSpec> = match args.scenario {
        Some(name) => vec![scenario_spec(name)],
        None => all_scenarios(),
    }
    .into_iter()
    .map(|spec| spec.scaled(args.size_multiplier))
    .collect();
    let started_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    println!("RDM kernel benchmark (schema rdm-kernel-bench/v1)");
    println!(
        "args: scenario={} connections={} dynamic={} chunk_size={:?} repeats={} size_multiplier={}",
        args.scenario.map(ScenarioName::as_str).unwrap_or("all"),
        args.connections,
        args.dynamic.as_str(),
        args.chunk_size,
        args.repeats,
        args.size_multiplier
    );
    if args.dynamic == DynamicMode::Both {
        println!(
            "     （同一场景/连接数交替跑 dynamic=true/false，每轮交换先后顺序，各 {} 次）",
            args.repeats
        );
    }

    let mut runs = Vec::new();
    for spec in &specs {
        println!("\n== {} ==", spec.name.as_str());
        println!("  {}", spec.description);
        println!("  server: {}", describe_server(spec));
        if args.size_multiplier > 1 {
            println!("  size multiplier: {}x", args.size_multiplier);
        }

        let connection_list: Vec<usize> = if spec.connection_grid.is_empty() {
            vec![args.connections]
        } else {
            spec.connection_grid.to_vec()
        };
        let mut groups = Vec::new();
        for connections in connection_list {
            // In `both` mode alternate the order on every repeat: the first run
            // in a process is measurably colder, so always running dynamic
            // first would bias the comparison (~10% on linear in round 1).
            let dynamics: Vec<bool> = args.dynamic.sequence().to_vec();
            let mut per_dynamic: Vec<(bool, Vec<Sample>, Vec<String>)> = dynamics
                .iter()
                .map(|&dynamic| (dynamic, Vec::new(), Vec::new()))
                .collect();
            for repeat in 0..args.repeats {
                let order: Vec<usize> = if per_dynamic.len() > 1 && repeat % 2 == 1 {
                    (0..per_dynamic.len()).rev().collect()
                } else {
                    (0..per_dynamic.len()).collect()
                };
                for index in order {
                    let dynamic = per_dynamic[index].0;
                    match run_sample(spec, connections, dynamic, &args).await {
                        Ok(sample) => {
                            println!(
                                "  [{connections} conns, dynamic={}, run {}/{}] {} ms, {:.2} MiB/s (eff), peak={}, target-trace={:?}",
                                if dynamic { "on" } else { "off" },
                                repeat + 1,
                                args.repeats,
                                sample.elapsed_ms,
                                sample.effective_mib_s,
                                sample.peak_connections,
                                sample.concurrency_trace
                                    .iter()
                                    .map(|(target, _)| *target)
                                    .collect::<Vec<_>>()
                            );
                            per_dynamic[index].1.push(sample);
                        }
                        Err(error) => {
                            eprintln!(
                                "  !! [{connections} conns, dynamic={}, run {}/{}] failed: {error:#}",
                                if dynamic { "on" } else { "off" },
                                repeat + 1,
                                args.repeats
                            );
                            per_dynamic[index].2.push(format!("{error:#}"));
                        }
                    }
                }
            }
            for (dynamic, samples, errors) in per_dynamic {
                groups.push(GroupReport {
                    connections,
                    dynamic,
                    samples,
                    errors,
                });
            }
        }
        print_group_tables(spec, &groups, args.dynamic);
        runs.push(ScenarioRun {
            spec: spec.clone(),
            groups,
        });
    }

    if let Some(out) = &args.out {
        let report = report_json(&args, started_unix, &runs);
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建输出目录失败：{}", parent.display()))?;
            }
        }
        let text = serde_json::to_string_pretty(&report).context("序列化 JSON 失败")?;
        std::fs::write(out, text).with_context(|| format!("写 JSON 失败：{}", out.display()))?;
        println!("\nJSON written to {}", out.display());
    }

    let failures: usize = runs
        .iter()
        .flat_map(|run| run.groups.iter())
        .map(|group| group.errors.len())
        .sum();
    if failures > 0 {
        bail!("{failures} 次运行失败（详见上表与 JSON 的 failures 字段）");
    }
    Ok(())
}
