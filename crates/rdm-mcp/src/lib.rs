//! `rdm-mcp` — Model Context Protocol stdio server for RDM.
//!
//! The binary is a thin wrapper around [`run_stdio`]; the library target exists
//! so integration tests can drive the same protocol stack in-process.

pub mod diagnostics;
pub mod manager;
pub mod protocol;
pub mod resources;
pub mod server;
pub mod tools;
pub mod util;

pub use manager::TaskManager;
pub use server::{run_stdio, ServerConfig, ServerState};

/// Semver of this server (also reported in `serverInfo`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
