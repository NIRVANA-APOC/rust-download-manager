//! RDM Firefox native messaging host.
//!
//! Architecture (`docs/architecture.md` §6, `docs/extension-protocol.md` §1):
//!
//! ```text
//! Firefox extension
//!   └─ connectNative("com.rdm.download")   (stdio, u32_le length + JSON)
//!        └─ rdm-native (this crate)
//!             ├─ ping    ──► rdm-ipc ping     ──► RDM GUI (loopback TCP JSONL)
//!             └─ enqueue ──► rdm-ipc enqueue  ──► RDM GUI
//!                              ├─ GUI offline → spawn rdm-gui.exe, poll ipc.json ≤ 8 s
//!                              └─ still offline → spool to state_dir()/queue/*.json
//! ```
//!
//! Design rules:
//! * stdout is the protocol channel — all logging goes to
//!   `state_dir()/logs/native.log` (warnings/errors mirrored to stderr, which
//!   Firefox shows in the browser console).
//! * Never let a handoff be lost silently: if the GUI cannot accept it, write
//!   an `EnqueueItem` JSON file that the GUI picks up on next start.
//! * Never log secrets: URLs are redacted (query/fragment/credentials),
//!   cookies are logged as size/host only.

pub mod framing;
pub mod gui;
pub mod host;
pub mod ipc_client;
pub mod logger;
pub mod protocol;
pub mod spool;
pub mod url_util;
