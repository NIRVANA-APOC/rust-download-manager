//! `rdm` — agent-first command line interface for the RDM download engine.
//!
//! Contract: `docs/agent-integration.md` §2 (output/errors/tasks) and §5 (CLI
//! agent subcommands). Shared types live in `rdm_agent`; the engine lives in
//! `rdm_core`.

mod cli;
mod commands;
mod output;
mod tracker;
mod util;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{resolve_output_mode, resolve_store_root, Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    let mode = resolve_output_mode(&cli);
    let store_root = resolve_store_root(&cli);

    let code: u8 = match cli.command {
        Command::Download(args) => commands::download::run(mode, &store_root, args).await,
        Command::Probe(args) => commands::probe::run(mode, &store_root, args).await,
        Command::Resolve(args) => commands::resolve::run(mode, &store_root, args).await,
        Command::Capabilities => commands::capabilities::run(mode, &store_root),
        Command::Schema => commands::schema::run(mode),
        Command::Doctor(args) => commands::doctor::run(mode, &store_root, args).await,
        Command::Selftest(args) => commands::selftest::run(mode, args).await,
        Command::Bench(args) => commands::bench::run(mode, args).await,
        Command::Tasks(args) => commands::tasks::list(mode, &store_root, args),
        Command::Task(args) => commands::tasks::show(mode, &store_root, args),
        Command::State(args) => commands::state::run(mode, args),
        Command::Run(args) => commands::run::run(mode, &store_root, args).await,
        Command::Recipe(args) => commands::recipe::run(mode, &store_root, args).await,
    };
    ExitCode::from(code)
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
