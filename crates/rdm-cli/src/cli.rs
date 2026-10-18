//! Clap definitions for the `rdm` binary.
//!
//! Every command accepts the global `--output text|json|ndjson` (plus the
//! legacy `--json` alias for `ndjson`) and `--state-dir` to point the durable
//! task store somewhere else (tests/CI rely on this).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::output::OutputMode;

const ABOUT: &str = "RDM — multi-connection download manager with an agent-first JSON interface";

const LONG_ABOUT: &str = "\
RDM downloads files over HTTP(S) with segmented, resumable, rate-limited \
transfers. It is designed to be driven by coding agents: every command has a \
stable JSON envelope (--output json), a streaming NDJSON mode (--output ndjson \
or --json), stable error codes, deterministic exit codes (0 ok, 2 failure, \
3 paused/cancelled, 4 policy denied) and a durable task store.

Start with `rdm capabilities` / `rdm schema`, make sure the environment is \
healthy with `rdm doctor`, and use `rdm selftest` for an end-to-end readiness \
probe.";

const AFTER_HELP: &str = "\
Examples:
  rdm capabilities --output json
  rdm schema --json | jq '.data.tools_count'
  rdm doctor --url https://example.com/file.iso --output json
  rdm selftest --output json
  rdm bench --size 64M --connections 1,8 --output json
  rdm download https://example.com/file.iso -d C:\\Downloads -c 8 --output json
  rdm probe https://example.com/file.iso --output json
  rdm tasks --state completed --limit 20 --output json
  rdm task tsk_1792345678901_a1b2 --events 50 --output json
  rdm state inspect --dir C:\\Downloads --output json
  rdm state repair --dir C:\\Downloads --no-dry-run --output json
  rdm run --input batch.json --wait --output json

Exit codes: 0 success, 2 failure, 3 paused/cancelled, 4 policy denied.";

#[derive(Debug, Parser)]
#[command(
    name = "rdm",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP,
    propagate_version = true
)]
pub struct Cli {
    /// Output format: text (default), json (single envelope) or ndjson (event stream).
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "text",
        value_name = "MODE"
    )]
    pub output: OutputMode,

    /// Compatibility alias for `--output ndjson`.
    #[arg(long, global = true, help = "Alias for --output ndjson (legacy)")]
    pub json: bool,

    /// Override the agent task-store root (default: %LOCALAPPDATA%\RDM\agent).
    #[arg(long, global = true, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download a URL into a durable task (resumable, multi-connection).
    Download(DownloadArgs),
    /// Inspect remote metadata without downloading anything.
    Probe(ProbeArgs),
    /// Print the machine-readable capability manifest.
    Capabilities,
    /// Print JSON Schemas for the envelope, errors, tasks, events and tools.
    Schema,
    /// Diagnose environment, network, policy, GUI IPC and download state.
    Doctor(DoctorArgs),
    /// Run a 4-case end-to-end self check against an embedded fixture server.
    Selftest(SelftestArgs),
    /// Benchmark 1 vs N connections against an embedded fixture server.
    Bench(BenchArgs),
    /// List durable tasks from the shared task store.
    Tasks(TasksArgs),
    /// Show one durable task (optionally with its event journal).
    Task(TaskArgs),
    /// Inspect or repair leftover `.part` / `.part.json` state.
    State(StateArgs),
    /// Execute a batch of downloads from a JSON file or stdin.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    /// URL to download (http/https).
    pub url: String,

    /// Directory that receives the file (default: current directory).
    #[arg(short = 'd', long = "dir", value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Override the server-derived filename.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Number of parallel connections (1..32).
    #[arg(short = 'c', long = "connections", default_value_t = rdm_core::DEFAULT_CONNECTIONS, value_name = "N")]
    pub connections: usize,

    /// Chunk size in bytes (default: derived from size and connections).
    #[arg(long, value_name = "BYTES")]
    pub chunk_size: Option<u64>,

    /// Global bandwidth cap in bytes/second.
    #[arg(long, value_name = "BPS")]
    pub limit: Option<u64>,

    /// Expected SHA-256 (64 hex chars) of the finished file.
    #[arg(long, value_name = "HEX")]
    pub sha256: Option<String>,

    /// Idempotency key: a completed task with this key is reused instead of
    /// downloading again.
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,

    /// Suppress progress output (errors and final result are still printed).
    #[arg(short = 'q', long)]
    pub quiet: bool,

    #[command(flatten)]
    pub net: NetArgs,
}

#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// URL to inspect (http/https).
    pub url: String,

    #[command(flatten)]
    pub net: NetArgs,
}

#[derive(Debug, Args, Default)]
pub struct NetArgs {
    /// Extra request header, `Name: Value` (repeatable).
    #[arg(long = "header", value_name = "K: V")]
    pub headers: Vec<String>,

    /// `Cookie` header value (never echoed in output/task store).
    #[arg(long, value_name = "COOKIE")]
    pub cookie: Option<String>,

    /// `Referer` header value.
    #[arg(long, value_name = "URL")]
    pub referer: Option<String>,

    /// `User-Agent` header value.
    #[arg(long, value_name = "UA")]
    pub user_agent: Option<String>,

    /// Proxy URL, e.g. `http://127.0.0.1:7890`.
    #[arg(long, value_name = "URL")]
    pub proxy: Option<String>,

    /// Accept invalid TLS certificates (debug mirrors only).
    #[arg(long)]
    pub insecure: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Also check DNS/TLS/HEAD/Range against this URL.
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
}

#[derive(Debug, Args)]
pub struct SelftestArgs {
    /// Keep the temporary directories/files created by the cases.
    #[arg(long)]
    pub keep: bool,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Payload size, e.g. `64M` or `1048576`.
    #[arg(long, default_value = "64M", value_name = "SIZE")]
    pub size: String,

    /// Comma-separated connection counts, e.g. `1,8`.
    #[arg(long, default_value = "1,8", value_name = "LIST")]
    pub connections: String,

    /// Runs per connection level (median is reported); must be >= 1.
    #[arg(long, default_value_t = 3, value_name = "N")]
    pub runs: usize,

    /// Artificial latency in ms per `--delay-every` bytes (simulates per-connection throttling).
    #[arg(long, default_value_t = 20, value_name = "MS")]
    pub delay_ms: u64,

    /// Latency granule size, e.g. `256K`.
    #[arg(long, default_value = "256K", value_name = "BYTES")]
    pub delay_every: String,
}

#[derive(Debug, Args)]
pub struct TasksArgs {
    /// Filter by state(s): queued,probing,downloading,paused,completed,failed,cancelled.
    #[arg(long = "state", value_name = "STATE")]
    pub states: Vec<String>,

    /// Maximum number of tasks to return.
    #[arg(long, default_value_t = 50, value_name = "N")]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct TaskArgs {
    /// Task id (`tsk_...`).
    pub id: String,

    /// Include the last N events from the task journal (0 = no events).
    #[arg(long, default_value_t = 0, value_name = "N")]
    pub events: usize,
}

#[derive(Debug, Args)]
pub struct StateArgs {
    #[command(subcommand)]
    pub action: StateCommand,
}

#[derive(Debug, Subcommand)]
pub enum StateCommand {
    /// Report orphan/corrupt partial-download state.
    Inspect(StateScanArgs),
    /// Quarantine problematic partial-download state (dry-run by default).
    Repair(StateRepairArgs),
}

#[derive(Debug, Args)]
pub struct StateScanArgs {
    /// Directory to scan (repeatable; default: current dir + default download dir).
    #[arg(long = "dir", value_name = "DIR")]
    pub dirs: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct StateRepairArgs {
    /// Directory to scan (repeatable; default: current dir + default download dir).
    #[arg(long = "dir", value_name = "DIR")]
    pub dirs: Vec<PathBuf>,

    /// Explicitly request a dry run (the default).
    #[arg(long)]
    pub dry_run: bool,

    /// Actually move problematic files into `.rdm-quarantine`.
    #[arg(long = "no-dry-run")]
    pub no_dry_run: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Batch JSON file, or `-` to read it from stdin.
    #[arg(long, value_name = "FILE")]
    pub input: String,

    /// Wait for every task (CLI-run is synchronous; this flag is an explicit
    /// confirmation of that behaviour).
    #[arg(long)]
    pub wait: bool,
}

/// Resolve the effective output mode: `--json` (legacy) wins over `--output`.
pub fn resolve_output_mode(cli: &Cli) -> OutputMode {
    if cli.json {
        OutputMode::Ndjson
    } else {
        cli.output
    }
}

/// Effective durable task-store root.
pub fn resolve_store_root(cli: &Cli) -> PathBuf {
    cli.state_dir
        .clone()
        .unwrap_or_else(rdm_agent::TaskStore::default_root)
}
