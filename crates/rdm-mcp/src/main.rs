//! `rdm-mcp` entry point.
//!
//! ```text
//! rdm-mcp [--state-dir DIR] [--log-level LEVEL]
//! ```
//!
//! stdout is reserved for newline-delimited JSON-RPC; logs go to stderr.

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: rdm-mcp [OPTIONS]

MCP (Model Context Protocol) stdio server for RDM.

Options:
  --state-dir DIR    agent state directory (default: %LOCALAPPDATA%\\RDM\\agent,
                     overridable with RDM_AGENT_STATE_DIR)
  --log-level LEVEL  stderr log filter (default: warn; RUST_LOG wins)
  -h, --help         print this help
  -V, --version      print the version
";

fn main() -> ExitCode {
    let mut state_dir: Option<PathBuf> = None;
    let mut log_level: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => match args.next() {
                Some(value) => state_dir = Some(PathBuf::from(value)),
                None => {
                    eprintln!("rdm-mcp: --state-dir requires a directory");
                    return ExitCode::from(2);
                }
            },
            "--log-level" => match args.next() {
                Some(value) => log_level = Some(value),
                None => {
                    eprintln!("rdm-mcp: --log-level requires a value");
                    return ExitCode::from(2);
                }
            },
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" | "-V" => {
                println!("rdm-mcp {}", rdm_mcp::version());
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("rdm-mcp: unknown argument `{other}`\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    init_tracing(log_level.as_deref());
    let config = rdm_mcp::ServerConfig {
        state_dir,
        log_level,
    };
    match rdm_mcp::run_stdio(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rdm-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(level: Option<&str>) {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("RUST_LOG")
        .ok()
        .or_else(|| level.map(str::to_owned))
        .unwrap_or_else(|| "warn".to_string());
    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
