//! `rdm` — command line driver for the RDM engine.
//!
//! `download` emits a human progress line (or one JSON object per line with
//! `--json`), pauses (and persists resume state) on Ctrl-C, and uses the exit
//! codes from architecture §4: 0 success, 2 failure, 3 interrupted.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use rdm_core::{DownloadEvent, DownloadHandle, DownloadRequest, RemoteMeta};
use serde_json::json;
use tokio::sync::mpsc;

#[derive(Debug, Parser)]
#[command(
    name = "rdm",
    version,
    about = "RDM — multi-connection download manager",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download a URL with multiple connections.
    Download(DownloadArgs),
    /// Inspect remote metadata without downloading.
    Probe(ProbeArgs),
}

#[derive(Debug, Args)]
struct NetArgs {
    /// Extra request header, `Name: Value` (repeatable).
    #[arg(long = "header", value_name = "K: V")]
    headers: Vec<String>,
    /// `Cookie` header value.
    #[arg(long)]
    cookie: Option<String>,
    /// `Referer` header value.
    #[arg(long)]
    referer: Option<String>,
    /// `User-Agent` header value.
    #[arg(long)]
    user_agent: Option<String>,
    /// Proxy URL, e.g. `http://127.0.0.1:7890`.
    #[arg(long)]
    proxy: Option<String>,
    /// Accept invalid TLS certificates (debug mirrors only).
    #[arg(long)]
    insecure: bool,
}

#[derive(Debug, Args)]
struct DownloadArgs {
    /// URL to download.
    url: String,
    /// Directory that receives the file (default: current directory).
    #[arg(short = 'd', long = "dir", value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Override the server-derived filename.
    #[arg(long)]
    name: Option<String>,
    /// Number of parallel connections.
    #[arg(short = 'c', long = "connections", default_value_t = rdm_core::DEFAULT_CONNECTIONS)]
    connections: usize,
    /// Chunk size in bytes (default: derived from file size and connections).
    #[arg(long, value_name = "BYTES")]
    chunk_size: Option<u64>,
    /// Global bandwidth cap in bytes/second.
    #[arg(long, value_name = "BPS")]
    limit: Option<u64>,
    /// Expected SHA-256 (hex) of the finished file.
    #[arg(long, value_name = "HEX")]
    sha256: Option<String>,
    /// Emit one JSON event per line on stdout.
    #[arg(long)]
    json: bool,
    /// Suppress progress output (still prints/serialises the final result).
    #[arg(short = 'q', long)]
    quiet: bool,
    #[command(flatten)]
    net: NetArgs,
}

#[derive(Debug, Args)]
struct ProbeArgs {
    /// URL to inspect.
    url: String,
    /// Print the metadata as a single JSON object.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    net: NetArgs,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match Cli::parse().command {
        Command::Download(args) => run_download(args).await,
        Command::Probe(args) => run_probe(args).await,
    }
}

fn init_tracing() {
    if std::env::var_os("RUST_LOG").is_some() {
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    }
}

// ---------------------------------------------------------------------------
// download
// ---------------------------------------------------------------------------

async fn run_download(args: DownloadArgs) -> ExitCode {
    let json = args.json;
    let quiet = args.quiet;
    let request = match args.into_request() {
        Ok(request) => request,
        Err(message) => {
            report_failure(json, &message);
            return ExitCode::from(2);
        }
    };

    let handle = DownloadHandle::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut printer = EventPrinter::new(json, quiet);

    let download = rdm_core::download_with_handle(request, Some(tx), handle.clone());
    tokio::pin!(download);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut interrupted = false;
    let result;
    loop {
        tokio::select! {
            biased;
            outcome = &mut download => {
                result = outcome;
                break;
            }
            Some(event) = rx.recv() => printer.handle(&event),
            signal = &mut ctrl_c, if !interrupted => {
                match signal {
                    Ok(()) => {
                        interrupted = true;
                        printer.interrupted(&handle);
                    }
                    Err(error) => {
                        interrupted = true;
                        eprintln!("rdm: could not listen for Ctrl-C: {error}");
                    }
                }
            }
        }
    }
    while let Ok(event) = rx.try_recv() {
        printer.handle(&event);
    }
    printer.finish();

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) if error.is_control_flow() => ExitCode::from(3),
        Err(_) => ExitCode::from(2),
    }
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

async fn run_probe(args: ProbeArgs) -> ExitCode {
    let request = match request_from(args.url, args.net) {
        Ok(request) => request,
        Err(message) => {
            report_failure(args.json, &message);
            return ExitCode::from(2);
        }
    };

    match rdm_core::probe(&request).await {
        Ok(meta) => {
            if args.json {
                match serde_json::to_string(&meta) {
                    Ok(line) => println!("{line}"),
                    Err(error) => {
                        report_failure(true, &format!("could not serialise metadata: {error}"));
                        return ExitCode::from(2);
                    }
                }
            } else {
                print_meta(&meta);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            report_failure(args.json, &error.to_string());
            ExitCode::from(2)
        }
    }
}

fn print_meta(meta: &RemoteMeta) {
    println!("url:           {}", meta.final_url);
    println!("file name:     {}", meta.file_name);
    println!(
        "size:          {}",
        match meta.total_bytes {
            Some(bytes) => format!("{} ({} bytes)", format_bytes(bytes), bytes),
            None => "unknown".to_string(),
        }
    );
    println!(
        "ranges:        {}",
        if meta.supports_ranges { "yes" } else { "no" }
    );
    if let Some(content_type) = &meta.content_type {
        println!("content type:  {content_type}");
    }
    if let Some(etag) = &meta.etag {
        println!("etag:          {etag}");
    }
    if let Some(last_modified) = &meta.last_modified {
        println!("last modified: {last_modified}");
    }
    if let Some(version) = &meta.http_version {
        println!("http version:  {version}");
    }
}

// ---------------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------------

impl DownloadArgs {
    fn into_request(self) -> Result<DownloadRequest, String> {
        let DownloadArgs {
            url,
            dir,
            name,
            connections,
            chunk_size,
            limit,
            sha256,
            json: _,
            quiet: _,
            net,
        } = self;
        let mut request = request_from(url, net)?;
        request.output_dir = dir;
        request.file_name = name;
        request.connections = connections;
        request.chunk_size = chunk_size;
        request.speed_limit_bps = limit;
        if let Some(expected) = sha256 {
            validate_sha256(&expected)?;
            request.expected_sha256 = Some(expected);
        }
        Ok(request)
    }
}

fn request_from(url: String, net: NetArgs) -> Result<DownloadRequest, String> {
    let mut request = DownloadRequest::new(url);
    request.cookie = net.cookie;
    request.referer = net.referer;
    request.user_agent = net.user_agent;
    request.proxy = net.proxy;
    request.insecure = net.insecure;
    let mut headers = BTreeMap::new();
    for raw in net.headers {
        let Some((name, value)) = raw.split_once(':') else {
            return Err(format!("invalid --header `{raw}`: expected `Name: Value`"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("invalid --header `{raw}`: empty header name"));
        }
        headers.insert(name.to_string(), value.trim().to_string());
    }
    request.headers = headers;
    Ok(request)
}

fn validate_sha256(value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid --sha256 `{value}`: expected 64 hexadecimal characters"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn report_failure(json_mode: bool, message: &str) {
    if json_mode {
        println!("{}", json!({"event": "failed", "error": message}));
    }
    eprintln!("rdm: {message}");
}

struct EventPrinter {
    json: bool,
    quiet: bool,
    tty: bool,
    progress_visible: bool,
    progress_width: usize,
    progress_ticks: u64,
    completed_printed: bool,
}

impl EventPrinter {
    fn new(json: bool, quiet: bool) -> Self {
        Self {
            json,
            quiet,
            tty: std::io::stdout().is_terminal(),
            progress_visible: false,
            progress_width: 0,
            progress_ticks: 0,
            completed_printed: false,
        }
    }

    fn handle(&mut self, event: &DownloadEvent) {
        if self.json {
            self.emit_json(event);
        } else {
            self.render_human(event);
        }
    }

    fn interrupted(&mut self, handle: &DownloadHandle) {
        handle.pause();
        if !self.json {
            self.clear_progress();
            eprintln!("interrupt received: pausing and saving resume state...");
        }
    }

    fn emit_json(&self, event: &DownloadEvent) {
        if self.quiet
            && matches!(
                event,
                DownloadEvent::Progress { .. } | DownloadEvent::Retrying { .. }
            )
        {
            return;
        }
        let value = match event {
            DownloadEvent::Probing => json!({"event": "probing"}),
            DownloadEvent::Metadata(meta) => json!({
                "event": "metadata",
                "final_url": meta.final_url,
                "file_name": meta.file_name,
                "total_bytes": meta.total_bytes,
                "supports_ranges": meta.supports_ranges,
                "content_type": meta.content_type,
                "etag": meta.etag,
                "last_modified": meta.last_modified,
                "http_version": meta.http_version,
            }),
            DownloadEvent::Started {
                connections,
                chunk_size,
                total_chunks,
                resumed_chunks,
            } => json!({
                "event": "started",
                "connections": connections,
                "chunk_size": chunk_size,
                "total_chunks": total_chunks,
                "resumed_chunks": resumed_chunks,
            }),
            DownloadEvent::Progress {
                downloaded,
                total,
                speed_bps,
                eta_secs,
                active_workers,
            } => json!({
                "event": "progress",
                "downloaded": downloaded,
                "total": total,
                "speed_bps": speed_bps,
                "eta_secs": eta_secs,
                "active_workers": active_workers,
            }),
            DownloadEvent::Retrying {
                chunk,
                attempt,
                delay_ms,
                reason,
            } => json!({
                "event": "retrying",
                "chunk": chunk,
                "attempt": attempt,
                "delay_ms": delay_ms,
                "reason": reason,
            }),
            DownloadEvent::Completed { summary } => json!({
                "event": "completed",
                "path": summary.path.display().to_string(),
                "total_bytes": summary.total_bytes,
                "elapsed_ms": summary.elapsed_ms,
                "avg_speed_bps": summary.avg_speed_bps,
                "connections": summary.connections,
                "sha256": summary.sha256,
                "resumed_chunks": summary.resumed_chunks,
                "range_supported": summary.range_supported,
                "http_version": summary.http_version,
            }),
            DownloadEvent::Failed { error } => json!({"event": "failed", "error": error}),
            DownloadEvent::Paused { downloaded } => {
                json!({"event": "paused", "downloaded": downloaded})
            }
        };
        println!("{value}");
        let _ = std::io::stdout().flush();
    }

    fn render_human(&mut self, event: &DownloadEvent) {
        match event {
            DownloadEvent::Probing => {}
            DownloadEvent::Metadata(meta) => {
                if !self.quiet {
                    println!(
                        "file: {}  size: {}  ranges: {}",
                        meta.file_name,
                        meta.total_bytes
                            .map(format_bytes)
                            .unwrap_or_else(|| "unknown".to_string()),
                        if meta.supports_ranges { "yes" } else { "no" },
                    );
                }
            }
            DownloadEvent::Started {
                connections,
                chunk_size,
                total_chunks,
                resumed_chunks,
            } => {
                if !self.quiet {
                    if *resumed_chunks > 0 {
                        println!(
                            "resuming: {resumed_chunks}/{total_chunks} chunks already complete, \
                             {connections} connection(s), chunk size {}",
                            format_bytes(*chunk_size)
                        );
                    } else {
                        println!(
                            "starting: {connections} connection(s), {total_chunks} chunks of {}",
                            format_bytes(*chunk_size)
                        );
                    }
                }
            }
            DownloadEvent::Progress {
                downloaded,
                total,
                speed_bps,
                eta_secs,
                active_workers: _,
            } => {
                if !self.quiet {
                    self.progress(*downloaded, *total, *speed_bps, *eta_secs);
                }
            }
            DownloadEvent::Retrying {
                chunk,
                attempt,
                delay_ms,
                reason,
            } => {
                if !self.quiet {
                    self.clear_progress();
                    eprintln!(
                        "retrying chunk {chunk} (attempt {attempt}) in {delay_ms} ms: {reason}"
                    );
                }
            }
            DownloadEvent::Completed { summary } => {
                self.clear_progress();
                self.completed_printed = true;
                let seconds = summary.elapsed_ms as f64 / 1000.0;
                println!(
                    "saved {} ({} in {seconds:.2}s, {}, {} connection(s){})",
                    summary.path.display(),
                    format_bytes(summary.total_bytes),
                    format_speed(summary.avg_speed_bps),
                    summary.connections,
                    if summary.range_supported {
                        ""
                    } else {
                        ", single stream (server has no range support)"
                    },
                );
            }
            DownloadEvent::Failed { error } => {
                self.clear_progress();
                eprintln!("rdm: {error}");
            }
            DownloadEvent::Paused { downloaded } => {
                self.clear_progress();
                println!(
                    "paused at {} — run the same command again to resume",
                    format_bytes(*downloaded)
                );
            }
        }
    }

    fn progress(&mut self, downloaded: u64, total: Option<u64>, speed: f64, eta: Option<f64>) {
        self.progress_ticks += 1;
        let percent = match total {
            Some(total) if total > 0 => {
                format!("{:5.1}%", downloaded as f64 * 100.0 / total as f64)
            }
            _ => "  ?.?%".to_string(),
        };
        let line = format!(
            "{percent}  {} / {}  {}  ETA {}",
            format_bytes(downloaded),
            total.map(format_bytes).unwrap_or_else(|| "?".to_string()),
            format_speed(speed),
            format_eta(eta),
        );
        if self.tty {
            print!("\r{line}");
            let _ = std::io::stdout().flush();
            self.progress_width = line.chars().count();
            self.progress_visible = true;
        } else if self.progress_ticks.is_multiple_of(4) {
            println!("{line}");
        }
    }

    fn clear_progress(&mut self) {
        if self.progress_visible {
            print!("\r{}\r", " ".repeat(self.progress_width));
            let _ = std::io::stdout().flush();
            self.progress_visible = false;
            self.progress_width = 0;
        }
    }

    fn finish(&mut self) {
        self.clear_progress();
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec.max(0.0) as u64))
}

fn format_eta(eta_secs: Option<f64>) -> String {
    match eta_secs {
        Some(seconds) if seconds.is_finite() => {
            let seconds = seconds.max(0.0).round() as u64;
            if seconds >= 3600 {
                format!(
                    "{:02}:{:02}:{:02}",
                    seconds / 3600,
                    (seconds % 3600) / 60,
                    seconds % 60
                )
            } else {
                format!("{:02}:{:02}", seconds / 60, seconds % 60)
            }
        }
        _ => "--:--".to_string(),
    }
}
