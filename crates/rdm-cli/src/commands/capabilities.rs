//! `rdm capabilities` — machine-readable capability manifest, shared with MCP.
//!
//! Uses the effective policy (from the task-store root) so the manifest
//! reflects the same limits/hosts that would actually be enforced.

use std::path::Path;

use serde_json::Value;

use crate::output::{OutputMode, Reporter};

pub fn run(mode: OutputMode, store_root: &Path) -> u8 {
    let mut reporter = Reporter::new(mode, "capabilities");
    let policy = rdm_agent::Policy::load_or_default(store_root);
    let capabilities = rdm_agent::capabilities_for(&policy);

    if reporter.is_text() {
        let version = capabilities
            .get("rdm_version")
            .and_then(Value::as_str)
            .unwrap_or(env!("CARGO_PKG_VERSION"));
        let schema = capabilities
            .get("schema_version")
            .and_then(Value::as_str)
            .unwrap_or(rdm_agent::SCHEMA_VERSION);
        crate::outln!("RDM {version}  (schema {schema})");
        if let Some(tools) = capabilities.get("tools").and_then(Value::as_array) {
            let names: Vec<&str> = tools.iter().filter_map(Value::as_str).collect();
            crate::outln!("tools: {} ({})", names.len(), names.join(", "));
        }
        if let Some(limits) = capabilities.get("limits") {
            crate::outln!("limits: {limits}");
        }
        if let Some(policy) = capabilities.get("policy") {
            crate::outln!("policy: {policy}");
        }
        if let Some(transports) = capabilities.get("transports").and_then(Value::as_array) {
            let names: Vec<&str> = transports.iter().filter_map(Value::as_str).collect();
            crate::outln!("transports: {}", names.join(", "));
        }
        crate::outln!("machine output: rdm capabilities --output json");
    }

    reporter.finish_ok(capabilities)
}
