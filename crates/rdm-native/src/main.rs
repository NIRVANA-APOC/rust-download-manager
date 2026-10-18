//! Firefox native messaging host entry point.
//!
//! Launched by Firefox as `rdm-native.exe <path-to-app-manifest> <extension-id>`
//! (Firefox 55+); the arguments are informational and unused. stdin/stdout are
//! the protocol channel, so nothing but native-messaging frames may be written
//! there. `--version` / `--help` are for manual debugging only.

use std::io::{stdin, stdout};
use std::process::ExitCode;

use rdm_native::logger::Logger;

const USAGE: &str = "\
rdm-native — RDM Firefox native messaging host

This executable is launched by Firefox; it reads native-messaging frames from
stdin and writes responses to stdout. stdout is reserved for the protocol.

Usage:
  rdm-native            run the host (invoked by Firefox)
  rdm-native --version  print the host version
  rdm-native --help     print this help

Environment:
  RDM_GUI_PATH   override the rdm-gui executable used for auto-start
  LOCALAPPDATA   state directory root (ipc.json, logs, queue)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("rdm-native {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let state_dir = rdm_ipc::state_dir();
    let logger = Logger::new(&state_dir);
    logger.info(&format!(
        "rdm-native {} starting (pid {}, state_dir {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        state_dir.display()
    ));

    let stdin = stdin();
    let stdout = stdout();
    let result = rdm_native::host::run(stdin.lock(), stdout.lock(), &logger, &state_dir);
    match result {
        Ok(()) => {
            logger.info("rdm-native stopped");
            ExitCode::SUCCESS
        }
        Err(err) => {
            logger.error(&format!("rdm-native stopped with error: {err}"));
            ExitCode::FAILURE
        }
    }
}
