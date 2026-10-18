//! `rdm schema` — JSON Schema bundle for the agent contract.
//!
//! Tool schemas come from [`rdm_agent::tool_definitions`] (single source shared
//! with MCP, so the CLI and MCP can never drift). Error codes and task states
//! are enumerated from the shared enums, not hand-copied.

use serde_json::{json, Value};

use crate::output::{OutputMode, Reporter};

pub fn run(mode: OutputMode) -> u8 {
    let mut reporter = Reporter::new(mode, "schema");
    let tools = rdm_agent::tool_definitions();
    let data = json!({
        "schema_version": rdm_agent::SCHEMA_VERSION,
        "envelope": envelope_schema(),
        "error": error_schema(),
        "task": task_schema(),
        "task_summary": task_summary_schema(),
        "task_event": task_event_schema(),
        "tools": tools,
        "tools_count": tools.len(),
        "notes": [
            "JSON Schema 2020-12 subset; object schemas are permissive on additionalProperties",
            "task objects use the redacted TaskView shape (raw DownloadRequest is never exposed)",
            "NDJSON result events are the envelope fields plus `type` and `seq`",
        ],
    });

    if reporter.is_text() {
        crate::outln!(
            "schema {}: envelope, error, task, task_event + {} tool definition(s)",
            rdm_agent::SCHEMA_VERSION,
            tools.len()
        );
        for tool in &tools {
            crate::outln!("  tool: {:<22} {}", tool.name, tool.title);
        }
        crate::outln!("machine output: rdm schema --output json");
    }

    reporter.finish_ok(data)
}

fn error_codes() -> Vec<&'static str> {
    rdm_agent::ErrorCode::all()
        .iter()
        .map(|code| code.as_str())
        .collect()
}

fn task_states() -> Vec<&'static str> {
    rdm_agent::TaskState::all()
        .iter()
        .map(|state| state.as_str())
        .collect()
}

fn envelope_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "rdm.agent Envelope",
        "type": "object",
        "required": ["schema_version", "ok", "command", "data", "error", "meta"],
        "properties": {
            "schema_version": { "type": "string", "const": rdm_agent::SCHEMA_VERSION },
            "ok": { "type": "boolean" },
            "command": { "type": "string", "description": "Subcommand/tool name" },
            "task_id": { "type": ["string", "null"], "description": "Stable task id (tsk_...)" },
            "data": { "description": "Command-specific payload; null on failure" },
            "error": { "oneOf": [ { "type": "null" }, { "$ref": "#/$defs/agent_error" } ] },
            "meta": {
                "type": "object",
                "required": ["rdm_version", "elapsed_ms", "ts"],
                "properties": {
                    "rdm_version": { "type": "string" },
                    "elapsed_ms": { "type": "integer", "minimum": 0 },
                    "ts": { "type": "string", "description": "ISO-8601 UTC timestamp" },
                    "trace_id": { "type": ["string", "null"] }
                }
            }
        },
        "$defs": { "agent_error": error_schema() }
    })
}

fn error_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "rdm.agent AgentErrorInfo",
        "type": "object",
        "required": ["code", "message", "retryable"],
        "properties": {
            "code": { "type": "string", "enum": error_codes() },
            "message": { "type": "string" },
            "retryable": { "type": "boolean" },
            "hint": { "type": "string", "description": "Suggested next command/action" },
            "details": { "type": ["object", "null"], "description": "Structured context (status codes, paths, ...)" }
        }
    })
}

fn task_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "rdm.agent TaskView (redacted, agent-visible)",
        "type": "object",
        "required": ["schema_version", "id", "state", "source", "url", "file_name", "dest_dir", "downloaded_bytes", "connections", "speed_bps", "warnings", "created_at", "updated_at"],
        "properties": {
            "schema_version": { "type": "string", "const": rdm_agent::SCHEMA_VERSION },
            "id": { "type": "string", "pattern": "^tsk_" },
            "state": { "type": "string", "enum": task_states() },
            "source": { "type": "string", "description": "cli|mcp|agent|firefox|..." },
            "url": { "type": "string", "description": "request URL with userinfo redacted" },
            "file_name": { "type": "string" },
            "dest_dir": { "type": "string" },
            "path": { "type": ["string", "null"] },
            "total_bytes": { "type": ["integer", "null"] },
            "downloaded_bytes": { "type": "integer", "minimum": 0 },
            "connections": { "type": "integer", "minimum": 1, "maximum": 32 },
            "chunk_size": { "type": ["integer", "null"] },
            "range_supported": { "type": ["boolean", "null"] },
            "http_version": { "type": ["string", "null"] },
            "speed_bps": { "type": "number" },
            "eta_secs": { "type": ["number", "null"] },
            "warnings": { "type": "array", "items": { "type": "string" } },
            "error": { "oneOf": [ { "type": "null" }, { "$ref": "#/$defs/agent_error" } ] },
            "result": {
                "type": ["object", "null"],
                "properties": {
                    "path": { "type": "string" },
                    "size": { "type": "integer" },
                    "sha256": { "type": ["string", "null"] },
                    "elapsed_ms": { "type": "integer" },
                    "avg_speed_bps": { "type": "number" }
                }
            },
            "idempotency_key": { "type": ["string", "null"] },
            "created_at": { "type": "string" },
            "updated_at": { "type": "string" },
            "finished_at": { "type": ["string", "null"] }
        },
        "$defs": { "agent_error": error_schema() }
    })
}

fn task_summary_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "rdm.agent TaskSummary (list view)",
        "type": "object",
        "required": ["id", "state", "url", "file_name", "dest_dir", "downloaded_bytes", "connections", "speed_bps", "created_at", "updated_at"],
        "properties": {
            "id": { "type": "string" },
            "state": { "type": "string", "enum": task_states() },
            "url": { "type": "string" },
            "file_name": { "type": "string" },
            "dest_dir": { "type": "string" },
            "path": { "type": ["string", "null"] },
            "total_bytes": { "type": ["integer", "null"] },
            "downloaded_bytes": { "type": "integer" },
            "connections": { "type": "integer" },
            "speed_bps": { "type": "number" },
            "created_at": { "type": "string" },
            "updated_at": { "type": "string" }
        }
    })
}

fn task_event_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "rdm.agent TaskEvent (NDJSON line / task journal entry)",
        "type": "object",
        "required": ["schema_version", "type", "task_id", "seq", "ts", "data"],
        "properties": {
            "schema_version": { "type": "string", "const": rdm_agent::SCHEMA_VERSION },
            "type": { "type": "string", "description": "probe|metadata|started|progress|retry|completed|failed|paused|cancelled|task_started|task_completed|result|..." },
            "task_id": { "type": "string" },
            "seq": { "type": "integer", "minimum": 1 },
            "ts": { "type": "string" },
            "data": { "type": ["object", "array", "string", "number", "boolean", "null"] }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_come_from_shared_contract() {
        assert_eq!(error_codes().len(), rdm_agent::ErrorCode::all().len());
        assert!(error_codes().contains(&"invalid_url"));
        assert_eq!(task_states().len(), 7);
        assert!(task_states().contains(&"downloading"));
        for code in error_codes() {
            let parsed: rdm_agent::ErrorCode = serde_json::from_value(Value::String(code.into()))
                .unwrap_or_else(|error| panic!("`{code}` is not a shared ErrorCode: {error}"));
            assert_eq!(parsed.as_str(), code);
        }
    }

    #[test]
    fn schema_sections_are_objects() {
        assert!(envelope_schema().get("properties").is_some());
        assert!(error_schema().get("properties").is_some());
        assert!(task_schema().get("properties").is_some());
        assert!(task_summary_schema().get("properties").is_some());
        assert!(task_event_schema().get("properties").is_some());
    }
}
