//! One module per CLI subcommand.

pub mod bench;
pub mod capabilities;
pub mod doctor;
pub mod download;
pub mod probe;
pub mod recipe;
pub mod resolve;
pub mod run;
pub mod schema;
pub mod selftest;
pub mod state;
pub mod tasks;

use rdm_agent::{AgentErrorInfo, AgentStoreError, ErrorCode};

/// Map a task-store failure onto the shared agent error taxonomy.
pub fn store_error(error: AgentStoreError) -> AgentErrorInfo {
    match error {
        AgentStoreError::NotFound(id) => {
            AgentErrorInfo::new(ErrorCode::NotFound, format!("task `{id}` not found"))
                .with_hint("run `rdm tasks --output json` to list known task ids")
                .with_details(serde_json::json!({ "task_id": id }))
        }
        AgentStoreError::Io(error) => AgentErrorInfo::new(
            ErrorCode::PermissionDenied,
            format!("task store i/o error: {error}"),
        )
        .with_hint("verify the task store path is writable, or pass `--state-dir <DIR>`"),
        AgentStoreError::Serde(error) => AgentErrorInfo::new(
            ErrorCode::Internal,
            format!("task store contains invalid JSON: {error}"),
        )
        .with_hint("run `rdm state inspect` or remove the offending file under <state-dir>/tasks"),
        AgentStoreError::InvalidId(id) => AgentErrorInfo::new(
            ErrorCode::InvalidRequest,
            format!("invalid task/plan id `{id}`"),
        )
        .with_hint(
            "ids look like `tsk_1792345678901_a1b2`; list them with `rdm tasks --output json`",
        ),
    }
}

/// Validate a user-supplied connection count for agent-facing inputs.
pub fn validate_connections(connections: usize) -> Result<usize, AgentErrorInfo> {
    if connections == 0 || connections > rdm_core::MAX_CONNECTIONS {
        return Err(AgentErrorInfo::new(
            ErrorCode::InvalidRequest,
            format!(
                "connections must be between 1 and {} (got {connections})",
                rdm_core::MAX_CONNECTIONS
            ),
        )
        .with_hint("use -c 8 for eight parallel connections"));
    }
    Ok(connections)
}
