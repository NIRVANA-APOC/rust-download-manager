//! The RDM agent tool catalog (`docs/agent-integration.md` §3.1).
//!
//! This module is the **single source of truth** for the 17 tools exposed by
//! the MCP server (`tools/list`, `rdm-mcp`) and by the CLI (`rdm schema`).
//! Both consumers build their machine contract from [`tool_definitions`], so
//! the two surfaces cannot drift apart.
//!
//! Each definition carries:
//! * `inputSchema` / `outputSchema` — JSON Schema 2020-12 subset (objects,
//!   properties, required, enum, arrays, numbers with bounds, `null` unions);
//! * MCP `annotations` (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
//!   `openWorldHint`);
//! * a `description` that tells an agent **when to use**, **when not to use**
//!   and the important boundaries, because tool descriptions are often the only
//!   documentation an agent reads.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    ApprovalTokenRecord, ErrorCode, Policy, TaskState, PROTOCOL_VERSIONS, SCHEMA_VERSION,
    SERVER_NAME,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    #[serde(rename = "readOnlyHint")]
    pub read_only_hint: bool,
    #[serde(rename = "destructiveHint")]
    pub destructive_hint: bool,
    #[serde(rename = "idempotentHint")]
    pub idempotent_hint: bool,
    #[serde(rename = "openWorldHint")]
    pub open_world_hint: bool,
}

impl ToolAnnotations {
    fn new(read_only: bool, destructive: bool, idempotent: bool, open_world: bool) -> Self {
        Self {
            read_only_hint: read_only,
            destructive_hint: destructive,
            idempotent_hint: idempotent,
            open_world_hint: open_world,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub title: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema")]
    pub output_schema: Value,
    pub annotations: ToolAnnotations,
}

// ---------------------------------------------------------------------------
// Schema building blocks
// ---------------------------------------------------------------------------

fn obj(properties: Value, required: &[&str], additional: bool) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": additional,
    })
}

fn nullable(schema: Value) -> Value {
    json!({ "anyOf": [schema, { "type": "null" }] })
}

fn string_array(description: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": description,
    })
}

fn error_code_names() -> Vec<&'static str> {
    ErrorCode::all().iter().map(|code| code.as_str()).collect()
}

fn task_state_names() -> Vec<&'static str> {
    TaskState::all()
        .iter()
        .map(|state| state.as_str())
        .collect()
}

fn error_schema() -> Value {
    json!({
        "type": "object",
        "required": ["code", "message", "retryable"],
        "properties": {
            "code": { "type": "string", "enum": error_code_names() },
            "message": { "type": "string" },
            "retryable": { "type": "boolean" },
            "hint": { "type": "string" },
            "details": { "type": "object" }
        },
        "additionalProperties": true
    })
}

/// Every tool returns the same envelope in `structuredContent`, whether it
/// succeeded or not (contract §3.1). `data` carries the tool payload and is
/// `null` on failure; `error` carries the stable [`crate::AgentErrorInfo`].
fn output_envelope(data: Value) -> Value {
    json!({
        "type": "object",
        "required": ["schema_version", "ok", "data", "error"],
        "properties": {
            "schema_version": { "type": "string", "const": SCHEMA_VERSION },
            "ok": { "type": "boolean" },
            "task_id": nullable(json!({ "type": "string" })),
            "data": nullable(data),
            "error": {
                "anyOf": [ { "type": "null" }, error_schema() ]
            }
        },
        "additionalProperties": false
    })
}

fn task_result_schema() -> Value {
    json!({
        "type": "object",
        "required": ["path", "size", "elapsed_ms", "avg_speed_bps"],
        "properties": {
            "path": { "type": "string" },
            "size": { "type": "integer", "minimum": 0 },
            "sha256": { "type": "string" },
            "elapsed_ms": { "type": "integer", "minimum": 0 },
            "avg_speed_bps": { "type": "number", "minimum": 0 },
            "peak_connections": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": true
    })
}

fn task_view_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "schema_version", "id", "state", "source", "url", "file_name",
            "dest_dir", "path", "total_bytes", "downloaded_bytes", "connections",
            "chunk_size", "range_supported", "http_version", "speed_bps",
            "eta_secs", "warnings", "error", "result", "idempotency_key",
            "created_at", "updated_at", "finished_at"
        ],
        "properties": {
            "schema_version": { "type": "string" },
            "id": { "type": "string" },
            "state": { "type": "string", "enum": task_state_names() },
            "source": { "type": "string" },
            "url": { "type": "string" },
            "file_name": { "type": "string" },
            "dest_dir": { "type": "string" },
            "path": nullable(json!({ "type": "string" })),
            "total_bytes": nullable(json!({ "type": "integer", "minimum": 0 })),
            "downloaded_bytes": { "type": "integer", "minimum": 0 },
            "connections": { "type": "integer", "minimum": 1 },
            "peak_connections": { "type": "integer", "minimum": 0 },
            "dynamic_connections": { "type": "boolean" },
            "chunk_size": nullable(json!({ "type": "integer", "minimum": 1 })),
            "range_supported": nullable(json!({ "type": "boolean" })),
            "http_version": nullable(json!({ "type": "string" })),
            "speed_bps": { "type": "number", "minimum": 0 },
            "eta_secs": nullable(json!({ "type": "number", "minimum": 0 })),
            "warnings": string_array("Non-fatal engine warnings (e.g. range_unsupported)"),
            "error": { "anyOf": [ { "type": "null" }, error_schema() ] },
            "result": nullable(task_result_schema()),
            "idempotency_key": nullable(json!({ "type": "string" })),
            "created_at": { "type": "string" },
            "updated_at": { "type": "string" },
            "finished_at": nullable(json!({ "type": "string" }))
        },
        "additionalProperties": true
    })
}

fn task_summary_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "id", "state", "url", "file_name", "dest_dir", "path",
            "total_bytes", "downloaded_bytes", "connections", "speed_bps",
            "created_at", "updated_at"
        ],
        "properties": {
            "id": { "type": "string" },
            "state": { "type": "string", "enum": task_state_names() },
            "url": { "type": "string" },
            "file_name": { "type": "string" },
            "dest_dir": { "type": "string" },
            "path": nullable(json!({ "type": "string" })),
            "total_bytes": nullable(json!({ "type": "integer", "minimum": 0 })),
            "downloaded_bytes": { "type": "integer", "minimum": 0 },
            "connections": { "type": "integer", "minimum": 1 },
            "peak_connections": { "type": "integer", "minimum": 0 },
            "speed_bps": { "type": "number", "minimum": 0 },
            "created_at": { "type": "string" },
            "updated_at": { "type": "string" }
        },
        "additionalProperties": true
    })
}

fn task_event_schema() -> Value {
    json!({
        "type": "object",
        "required": ["schema_version", "type", "task_id", "seq", "ts", "data"],
        "properties": {
            "schema_version": { "type": "string" },
            "type": { "type": "string" },
            "task_id": { "type": "string" },
            "seq": { "type": "integer", "minimum": 0 },
            "ts": { "type": "string" },
            "data": { "type": "object" }
        },
        "additionalProperties": true
    })
}

fn task_data_schema() -> Value {
    obj(
        json!({
            "task": task_view_schema(),
            "events": {
                "type": "array",
                "items": task_event_schema(),
                "description": "Recent redacted audit events (only when include_events=true)"
            }
        }),
        &["task"],
        true,
    )
}

fn plan_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "schema_version", "plan_id", "url", "dest_dir", "file_name",
            "connections", "chunk_size", "expected_sha256", "speed_limit_bps",
            "total_bytes", "requires_approval", "reason", "created_at",
            "expires_at", "idempotency_key"
        ],
        "properties": {
            "schema_version": { "type": "string" },
            "plan_id": { "type": "string" },
            "url": { "type": "string" },
            "dest_dir": nullable(json!({ "type": "string" })),
            "file_name": nullable(json!({ "type": "string" })),
            "connections": { "type": "integer", "minimum": 1 },
            "dynamic_connections": { "type": "boolean" },
            "single_stream": { "type": "boolean" },
            "chunk_size": nullable(json!({ "type": "integer", "minimum": 1 })),
            "expected_sha256": nullable(json!({ "type": "string" })),
            "speed_limit_bps": nullable(json!({ "type": "integer", "minimum": 0 })),
            "total_bytes": nullable(json!({ "type": "integer", "minimum": 0 })),
            "requires_approval": { "type": "boolean" },
            "reason": nullable(json!({ "type": "string" })),
            "created_at": { "type": "string" },
            "expires_at": { "type": "string" },
            "idempotency_key": nullable(json!({ "type": "string" }))
        },
        "additionalProperties": true
    })
}

// -- input building blocks --------------------------------------------------

fn prop_url() -> Value {
    json!({
        "type": "string",
        "pattern": "^https?://",
        "description": "Absolute http(s) URL. Redirects are followed; the final URL is reported by rdm_probe."
    })
}

fn prop_dest_dir() -> Value {
    json!({
        "type": "string",
        "description": "Destination directory. Must resolve inside policy.allowed_dirs. Defaults to the first allowed dir (the user's Downloads folder)."
    })
}

fn prop_file_name() -> Value {
    json!({
        "type": "string",
        "description": "Override the server-derived file name (sanitized for Windows). Defaults to Content-Disposition / URL file name."
    })
}

fn prop_connections() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": 32,
        "description": "Parallel connections (1 = single stream). The engine falls back to 1 automatically when the server ignores Range requests."
    })
}

fn prop_chunk_size() -> Value {
    json!({
        "type": "integer",
        "minimum": 262144,
        "description": "Chunk size in bytes (>= 256 KiB). Omit to derive it from file size and connections."
    })
}

fn prop_expected_sha256() -> Value {
    json!({
        "type": "string",
        "pattern": "^[0-9a-fA-F]{64}$",
        "description": "Expected lowercase/uppercase hex SHA-256 of the finished file. On mismatch the engine deletes the target and fails with checksum_mismatch."
    })
}

fn prop_speed_limit() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "description": "Global bandwidth cap in bytes/second (0 or omitted = unlimited)."
    })
}

fn prop_headers() -> Value {
    json!({
        "type": "object",
        "additionalProperties": { "type": "string" },
        "description": "Extra request headers. Sensitive header names (Cookie, Authorization, Proxy-Authorization) are redacted from every output and log."
    })
}

fn prop_cookie() -> Value {
    json!({
        "type": "string",
        "description": "Cookie header value captured from a browser. Kept only in the local task store, never echoed by list/get/wait or logs."
    })
}

fn prop_referer() -> Value {
    json!({ "type": "string", "description": "Referer header value." })
}

fn prop_user_agent() -> Value {
    json!({ "type": "string", "description": "User-Agent override (default: the RDM user agent)." })
}

fn prop_proxy() -> Value {
    json!({
        "type": "string",
        "description": "Proxy URL, e.g. http://127.0.0.1:7890. Credentials in the URL are redacted from outputs."
    })
}

fn prop_insecure() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Accept invalid TLS certificates. Debug/public mirrors only; never for sensitive downloads."
    })
}

fn prop_resume() -> Value {
    json!({
        "type": "boolean",
        "default": true,
        "description": "Resume from an existing <file>.part + <file>.part.json when validators match."
    })
}

fn prop_dynamic_connections() -> Value {
    json!({
        "type": "boolean",
        "default": true,
        "description": "Dynamically add/remove parallel connections based on observed throughput (default true). Set false to use a fixed `connections` count for the whole transfer."
    })
}

fn prop_single_stream() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Download with exactly one plain `GET` and NO `Range` header: for one-time/signed links and anti-hotlink servers that reject ranged requests. Forces connections=1, dynamic_connections=false and no chunk plan (a pre-existing .part is restarted from zero). The probe still sends its normal HEAD first."
    })
}

fn prop_idempotency_key() -> Value {
    json!({
        "type": "string",
        "description": "Repeatable key: repeating a call with the same key returns the existing task instead of downloading twice."
    })
}

fn prop_wait() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Wait for the task to finish (or the timeout) before returning. Progress notifications are sent while waiting."
    })
}

fn prop_wait_timeout() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": 3600000,
        "default": 60000,
        "description": "Maximum time to wait, in milliseconds (1..3,600,000). On timeout the tool returns code=timeout but the task keeps running."
    })
}

fn prop_task_id() -> Value {
    json!({
        "type": "string",
        "description": "Task id returned by rdm_download / rdm_list_tasks / rdm_apply_plan, e.g. tsk_1792345678901_a1b2."
    })
}

fn prop_include_events() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Include the recent redacted audit events of the task."
    })
}

fn prop_event_limit() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": 500,
        "default": 50,
        "description": "How many recent events to include (newest last)."
    })
}

fn prop_plan_id() -> Value {
    json!({
        "type": "string",
        "description": "Plan id returned inside details.plan of rdm_download/rdm_plan_download (plan_<millis>_<hex>)."
    })
}

fn prop_approval_token() -> Value {
    json!({
        "type": "string",
        "description": "Single-use approval token returned with the plan. Expires after 10 minutes; a plan cannot be applied twice."
    })
}

fn prop_dry_run() -> Value {
    json!({
        "type": "boolean",
        "default": true,
        "description": "When true (default) only report findings. Set false to actually delete orphan/corrupt partial files."
    })
}

fn prop_size_bytes(default: u64, max: u64) -> Value {
    json!({
        "type": "integer",
        "minimum": 65536,
        "maximum": max,
        "default": default,
        "description": "Payload size in bytes for the local fixture run."
    })
}

// -- network input groups ---------------------------------------------------

fn network_props() -> Value {
    json!({
        "headers": prop_headers(),
        "cookie": prop_cookie(),
        "referer": prop_referer(),
        "user_agent": prop_user_agent(),
        "proxy": prop_proxy(),
        "insecure": prop_insecure()
    })
}

/// Merge two `"properties"` JSON objects into one.
fn merge_props(base: Value, extra: Value) -> Value {
    let mut map = match base {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            map.insert(key, value);
        }
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// All 17 tool names in contract order.
pub const TOOL_NAMES: [&str; 17] = [
    "rdm_capabilities",
    "rdm_probe",
    "rdm_resolve",
    "rdm_download",
    "rdm_plan_download",
    "rdm_apply_plan",
    "rdm_list_tasks",
    "rdm_get_task",
    "rdm_wait_task",
    "rdm_pause_task",
    "rdm_resume_task",
    "rdm_cancel_task",
    "rdm_verify_file",
    "rdm_doctor",
    "rdm_selftest",
    "rdm_bench",
    "rdm_state_maintenance",
];

/// Full tool catalog consumed by MCP `tools/list` and CLI `rdm schema`.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        capabilities_tool(),
        probe_tool(),
        resolve_tool(),
        download_tool(),
        plan_download_tool(),
        apply_plan_tool(),
        list_tasks_tool(),
        get_task_tool(),
        wait_task_tool(),
        pause_task_tool(),
        resume_task_tool(),
        cancel_task_tool(),
        verify_file_tool(),
        doctor_tool(),
        selftest_tool(),
        bench_tool(),
        state_maintenance_tool(),
    ]
}

/// Look up one tool definition by exact name.
pub fn tool_by_name(name: &str) -> Option<ToolDefinition> {
    tool_definitions()
        .into_iter()
        .find(|tool| tool.name == name)
}

fn capabilities_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_capabilities".to_string(),
        title: "RDM capabilities".to_string(),
        description: "Return the machine-readable capability manifest: RDM version, the 17 tools, \
resources/prompts, hard limits, effective policy summary, transports and supported MCP protocol \
versions. Call this first in a new session instead of guessing parameters. Read-only and cheap. \
Do not use it as a network or environment health check — use rdm_doctor for that."
            .to_string(),
        input_schema: obj(json!({}), &[], false),
        output_schema: output_envelope(json!({
            "type": "object",
            "properties": {
                "schema_version": { "type": "string" },
                "rdm_version": { "type": "string" },
                "server": { "type": "object" },
                "tools": { "type": "array", "items": { "type": "string" } },
                "resources": { "type": "array", "items": { "type": "string" } },
                "prompts": { "type": "array", "items": { "type": "string" } },
                "limits": { "type": "object" },
                "transports": { "type": "array", "items": { "type": "string" } },
                "features": { "type": "array", "items": { "type": "string" } },
                "error_codes": { "type": "array", "items": { "type": "string" } },
                "policy": { "type": "object" }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn probe_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_probe".to_string(),
        title: "Probe remote URL".to_string(),
        description:
            "Inspect a remote URL without downloading or creating a task: final URL after \
redirects, server-derived file name, total size, Range support, content type, ETag/Last-Modified \
and HTTP version. Use it before rdm_download to pick a connection count or to confirm the URL is \
reachable, and to preview the resolved destination. It writes no files. Boundaries are reported \
truthfully: servers that ignore Range yield range_supported=false and servers without \
Content-Length yield total_bytes=null; those are facts, not errors."
                .to_string(),
        input_schema: obj(
            merge_props(
                json!({
                    "url": prop_url()
                }),
                network_props(),
            ),
            &["url"],
            false,
        ),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": [
                "url", "final_url", "file_name", "total_bytes",
                "range_supported", "policy"
            ],
            "properties": {
                "url": { "type": "string" },
                "final_url": { "type": "string" },
                "file_name": { "type": "string" },
                "total_bytes": nullable(json!({ "type": "integer", "minimum": 0 })),
                "range_supported": { "type": "boolean" },
                "content_type": nullable(json!({ "type": "string" })),
                "etag": nullable(json!({ "type": "string" })),
                "last_modified": nullable(json!({ "type": "string" })),
                "http_version": nullable(json!({ "type": "string" })),
                "policy": {
                    "type": "object",
                    "properties": {
                        "host_allowed": { "type": "boolean" },
                        "requires_approval": { "type": "boolean" },
                        "max_bytes_per_task": nullable(json!({ "type": "integer" })),
                        "approval_threshold_bytes": nullable(json!({ "type": "integer" }))
                    },
                    "additionalProperties": true
                }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, true),
    }
}

fn resolve_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_resolve".to_string(),
        title: "Resolve a link to a resource manifest".to_string(),
        description: "Resolve a page/link URL into a verified manifest of downloadable resources WITHOUT \
creating a download task and without writing any file. Use it when the URL may be an HTML page, a \
share link, a signed/cookie-gated URL or anything whose real file URL is unknown: it replays a local \
deterministic recipe (when one matches in <state_dir>/resolve/recipes), verifies direct-file input \
with a HEAD + bounded ranged GET, and in mode=auto/explore falls back to model-driven exploration \
bound by max_llm_calls when the page is not a file and the user has configured an AI provider \
(<state_dir>/ai.json or RDM_AI_API_KEY). Exploration sends page titles/links/truncated text and \
response-header summaries to that provider, never Cookie/Authorization or the whole file body, and \
the API key never appears in the result. The manifest returns resource URLs, size/Range/ETag/\
verification plus single_stream / one_time / resumable transfer hints. Do NOT use it as a substitute \
for rdm_download (it never consumes the URL). mode=explore without a configured provider returns \
code=resolve_failed with details.kind=model_unavailable; no matching recipe returns \
details.kind=recipe_no_match. When the outcome comes from a library recipe (state matched/published), \
data.recipe_health reports that method's health (ok/degraded/broken/untested, last_failure, \
consecutive_failures): treat degraded/broken as a signal to roll the method back (rdm recipe rollback) \
or switch to mode=explore. Resolution never creates tasks and is bounded by \
policy.allowed_hosts / allow_private_hosts and a per-run budget."
            .to_string(),
        input_schema: obj(
            json!({
                "url": prop_url(),
                "goal": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 512,
                    "description": "Optional natural-language goal, e.g. \"Windows x64 installer\". Recorded as a hint for recipe selection and for the exploration prompt; it never forces a model call by itself."
                },
                "mode": {
                    "type": "string",
                    "enum": ["auto", "recipe_only", "explore"],
                    "default": "auto",
                    "description": "auto = use a matching local recipe, else verify the URL as a direct file and (for pages, only when an AI provider is configured) explore; recipe_only = never explore and make zero model calls; explore = always use the user's configured AI (resolve_failed/details.kind=model_unavailable when none is configured)."
                },
                "recipe_id": {
                    "type": "string",
                    "description": "Force one recipe by id (debugging/replay). An unknown id fails with resolve_failed/details.kind=recipe_no_match."
                },
                "max_resources": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 40,
                    "default": 40,
                    "description": "Maximum number of resources to return in the manifest (1..40)."
                },
                "max_llm_calls": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 32,
                    "default": 8,
                    "description": "Maximum model calls for exploration (1..32, default 8). Only used by mode=explore and by the mode=auto page fallback; recipe replay and direct-file verification make zero model calls."
                },
                "budget": {
                    "type": "object",
                    "description": "Per-run hard limits (never shared with download-task budgets). Omitted fields use the defaults shown.",
                    "properties": {
                        "wall_ms": {
                            "type": "integer",
                            "minimum": 1000,
                            "maximum": 300000,
                            "default": 60000,
                            "description": "Wall-clock deadline for the whole resolution (1,000..300,000 ms)."
                        },
                        "http_requests": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 64,
                            "default": 24,
                            "description": "Maximum HTTP requests (including redirect hops and verification)."
                        },
                        "bytes_read": {
                            "type": "integer",
                            "minimum": 65536,
                            "maximum": 33554432,
                            "default": 4194304,
                            "description": "Maximum body bytes read across the run (default 4 MiB)."
                        },
                        "redirects": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 10,
                            "default": 5,
                            "description": "Maximum redirect hops."
                        }
                    },
                    "required": [],
                    "additionalProperties": false
                }
            }),
            &["url"],
            false,
        ),
        output_schema: resolve_output_schema(),
        annotations: ToolAnnotations::new(true, false, true, true),
    }
}

/// Output schema of `rdm_resolve`: the shared envelope whose `data` is a
/// `ResolveOutcome` (`rdm.resolve/v1`, contract §3.1).
fn resolve_output_schema() -> Value {
    output_envelope(json!({
        "type": "object",
        "required": ["run_id", "status", "manifest", "recipe", "diagnostics"],
        "properties": {
            "run_id": {
                "type": "string",
                "description": "Stable run id (run_...), also reported in error.details.run_id."
            },
            "status": {
                "type": "string",
                "enum": [
                    "succeeded", "partial", "waiting_user", "failed", "budget_exhausted", "cancelled"
                ]
            },
            "manifest": nullable(resolve_manifest_schema()),
            "recipe": nullable(resolve_recipe_ref_schema()),
            // v1.6 optional additive field: method health of the library recipe
            // that produced this outcome (absent for direct-link/draft runs).
            "recipe_health": nullable(resolve_recipe_health_schema()),
            "diagnostics": resolve_diagnostics_schema()
        },
        "additionalProperties": true
    }))
}

fn resolve_manifest_schema() -> Value {
    json!({
        "type": "object",
        "required": ["schema_version", "manifest_id", "source", "resources", "created_ms"],
        "properties": {
            "schema_version": { "type": "string", "const": "rdm.resolve/v1" },
            "manifest_id": { "type": "string" },
            "source": {
                "type": "object",
                "properties": {
                    "input_url": { "type": "string" },
                    "final_url": nullable(json!({ "type": "string" })),
                    "title": nullable(json!({ "type": "string" })),
                    "recipe_id": nullable(json!({ "type": "string" }))
                },
                "additionalProperties": true
            },
            "resources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["resource_id", "url", "transfer", "verification"],
                    "properties": {
                        "resource_id": { "type": "string" },
                        "url": { "type": "string" },
                        "final_url": nullable(json!({ "type": "string" })),
                        "name": nullable(json!({ "type": "string" })),
                        "kind": nullable(json!({ "type": "string" })),
                        "content_type": nullable(json!({ "type": "string" })),
                        "size_bytes": nullable(json!({ "type": "integer", "minimum": 0 })),
                        "supports_ranges": nullable(json!({ "type": "boolean" })),
                        "etag": nullable(json!({ "type": "string" })),
                        "last_modified": nullable(json!({ "type": "string" })),
                        "expected_sha256": nullable(json!({ "type": "string" })),
                        "transfer": resolve_transfer_schema(),
                        "verification": resolve_verification_schema(),
                        "evidence": { "type": "array", "items": { "type": "string" } },
                        "notes": { "type": "array", "items": { "type": "string" } }
                    },
                    "additionalProperties": true
                }
            },
            "notes": { "type": "array", "items": { "type": "string" } },
            "created_ms": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": true
    })
}

fn resolve_transfer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "single_stream": { "type": "boolean" },
            "connections": nullable(json!({ "type": "integer", "minimum": 1 })),
            "dynamic_connections": nullable(json!({ "type": "boolean" })),
            "chunk_size": nullable(json!({ "type": "integer", "minimum": 1 })),
            "resumable": { "type": "boolean" },
            "one_time": { "type": "boolean" },
            "expires_ms": nullable(json!({ "type": "integer", "minimum": 0 }))
        },
        "additionalProperties": true
    })
}

fn resolve_verification_schema() -> Value {
    json!({
        "type": "object",
        "required": ["status"],
        "properties": {
            "status": {
                "type": "string",
                "enum": [
                    "verified", "head_only_verified", "login_required",
                    "forbidden_or_expired", "not_found", "rate_limited",
                    "html_page", "not_a_file", "empty_body",
                    "unknown_length", "server_error", "network_error",
                    "policy_denied"
                ]
            },
            "method": {
                "type": "string",
                "enum": ["head_only", "ranged_get", "bounded_get"]
            },
            "http_status": nullable(json!({ "type": "integer" })),
            "final_url": nullable(json!({ "type": "string" })),
            "content_type": nullable(json!({ "type": "string" })),
            "size_bytes": nullable(json!({ "type": "integer", "minimum": 0 })),
            "supports_ranges": nullable(json!({ "type": "boolean" })),
            "etag": nullable(json!({ "type": "string" })),
            "evidence": { "type": "array", "items": { "type": "string" } },
            "checked_ms": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": true
    })
}

fn resolve_recipe_ref_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "version": { "type": "integer", "minimum": 1 },
            "state": { "type": "string" }
        },
        "additionalProperties": true
    })
}

/// Optional `data.recipe_health` (v1.6 §8.6): method-library health of the
/// recipe that produced the outcome (absent for direct-link/draft runs).
fn resolve_recipe_health_schema() -> Value {
    json!({
        "type": "object",
        "required": ["status", "last_ok_ms", "last_failure", "consecutive_failures", "evidence"],
        "properties": {
            "status": {
                "type": "string",
                "enum": ["ok", "degraded", "broken", "untested"],
                "description": "ok = last replay succeeded; degraded = two consecutive structural failures; broken = three or more; untested = never run / no health recorded."
            },
            "last_ok_ms": nullable(json!({ "type": "integer", "minimum": 0 })),
            "last_failure": nullable(json!({
                "type": "object",
                "required": ["run_id", "kind", "observed_ms"],
                "properties": {
                    "run_id": { "type": "string" },
                    "kind": { "type": "string", "description": "rdm.resolve error kind (e.g. recipe_stale, selector_miss, not_a_file)." },
                    "step_id": nullable(json!({ "type": "string" })),
                    "observed_ms": { "type": "integer", "minimum": 0 }
                },
                "additionalProperties": true
            })),
            "consecutive_failures": { "type": "integer", "minimum": 0 },
            "evidence": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": true
    })
}

fn resolve_diagnostics_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string" },
            "error": nullable(json!({ "type": "object" })),
            "steps": { "type": "integer", "minimum": 0 },
            "http_requests": { "type": "integer", "minimum": 0 },
            "bytes_read": { "type": "integer", "minimum": 0 },
            "llm_calls": { "type": "integer", "minimum": 0 },
            "elapsed_ms": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": true
    })
}

fn download_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_download".to_string(),
        title: "Download file".to_string(),
        description: "Start a durable multi-connection download task and optionally wait for it. \
Returns a TaskView with a stable task_id usable by rdm_get_task / rdm_wait_task / rdm_pause_task \
/ rdm_resume_task / rdm_cancel_task. Use this for real transfers. Do NOT use it to only inspect a \
URL (rdm_probe) and do not retry blindly: failures carry a stable error.code plus retryable/hint. \
Destination must resolve inside policy.allowed_dirs and the host must pass policy.allowed_hosts; \
otherwise error code=policy_denied. Files above policy.require_approval_over_bytes return \
code=needs_approval with details.plan and details.approval_token — execute them with \
rdm_apply_plan. With allow_overwrite=false an existing target returns code=conflict and is never \
silently replaced. Pass idempotency_key to make retries safe. For one-time/signed links or anti-hotlink \
servers set single_stream=true: exactly one plain GET with no Range header (connections are forced to 1 \
and a previous .part is restarted from zero)."
            .to_string(),
        input_schema: obj(
            merge_props(
                json!({
                    "url": prop_url(),
                    "dest_dir": prop_dest_dir(),
                    "file_name": prop_file_name(),
                    "connections": prop_connections(),
                    "dynamic_connections": prop_dynamic_connections(),
                    "single_stream": prop_single_stream(),
                    "chunk_size": prop_chunk_size(),
                    "expected_sha256": prop_expected_sha256(),
                    "speed_limit_bps": prop_speed_limit(),
                    "resume": prop_resume(),
                    "idempotency_key": prop_idempotency_key(),
                    "wait": prop_wait(),
                    "wait_timeout_ms": prop_wait_timeout()
                }),
                network_props(),
            ),
            &["url"],
            false,
        ),
        output_schema: output_envelope(task_data_schema()),
        annotations: ToolAnnotations::new(false, true, false, true),
    }
}

fn plan_download_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_plan_download".to_string(),
        title: "Plan download (pre-check)".to_string(),
        description:
            "Probe a URL and run the complete policy pre-check WITHOUT creating a task or \
writing a file. Returns a plan: resolved destination, size, whether approval is required, the \
policy reason, and — when the size exceeds the approval threshold — an approval_token. Use it when \
you must decide before transferring (e.g. large files or an agent that needs human sign-off). For \
small files you may skip it and call rdm_download directly; do not use it just to read metadata \
(rdm_probe is cheaper because it creates no plan). Pass single_stream=true for one-time/signed links: \
the plan records it, and rdm_apply_plan executes the same no-Range single-GET transfer. Plans expire \
after 10 minutes."
                .to_string(),
        input_schema: obj(
            merge_props(
                json!({
                    "url": prop_url(),
                    "dest_dir": prop_dest_dir(),
                    "file_name": prop_file_name(),
                    "connections": prop_connections(),
                    "dynamic_connections": prop_dynamic_connections(),
                    "single_stream": prop_single_stream(),
                    "chunk_size": prop_chunk_size(),
                    "expected_sha256": prop_expected_sha256(),
                    "speed_limit_bps": prop_speed_limit(),
                    "resume": prop_resume(),
                    "idempotency_key": prop_idempotency_key()
                }),
                network_props(),
            ),
            &["url"],
            false,
        ),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": ["plan", "approval_token", "approval_required"],
            "properties": {
                "plan": plan_schema(),
                "approval_token": nullable(json!({ "type": "string" })),
                "approval_required": { "type": "boolean" }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, true),
    }
}

fn apply_plan_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_apply_plan".to_string(),
        title: "Apply approved plan".to_string(),
        description: "Execute a plan previously returned by rdm_plan_download or rdm_download \
(needs_approval details). Requires the exact approval_token when the plan needs approval; tokens \
are single-use and expire after 10 minutes. The plan is deleted after a successful apply. Use it \
as the second half of the approval flow only — for ordinary downloads call rdm_download. Errors: \
not_found (plan missing/expired), conflict (plan already used or target appeared), policy_denied \
(policy changed since planning)."
            .to_string(),
        input_schema: obj(
            json!({
                "plan_id": prop_plan_id(),
                "approval_token": prop_approval_token(),
                "idempotency_key": prop_idempotency_key(),
                "wait": prop_wait(),
                "wait_timeout_ms": prop_wait_timeout()
            }),
            &["plan_id"],
            false,
        ),
        output_schema: output_envelope(task_data_schema()),
        annotations: ToolAnnotations::new(false, true, false, true),
    }
}

fn list_tasks_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_list_tasks".to_string(),
        title: "List download tasks".to_string(),
        description: "List durable task summaries, newest first, with optional state filter and \
cursor pagination. Use it to recover task ids after a restart, to check what is still running, or \
to show progress. Summaries never contain cookies or headers. Do not poll it in a tight loop to \
wait for one task — rdm_wait_task is the intended primitive (it emits progress notifications)."
            .to_string(),
        input_schema: obj(
            json!({
                "states": {
                    "type": "array",
                    "items": { "type": "string", "enum": task_state_names() },
                    "description": "Only return tasks in these states (any state when omitted)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 50,
                    "description": "Maximum number of summaries to return."
                },
                "cursor": {
                    "type": "string",
                    "description": "Opaque cursor from a previous page (use next_cursor from the result)."
                }
            }),
            &[],
            false,
        ),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": ["tasks", "next_cursor", "total"],
            "properties": {
                "tasks": { "type": "array", "items": task_summary_schema() },
                "next_cursor": nullable(json!({ "type": "string" })),
                "total": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn get_task_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_get_task".to_string(),
        title: "Get task detail".to_string(),
        description:
            "Return one durable task (TaskView) plus optionally the newest redacted audit \
events. Use it to inspect a task after a restart, in another process, or after a failure. The \
returned record is identical to the one rdm_wait_task/rdm_download return. Do not use it to wait \
for completion (rdm_wait_task); do not expect cookies or request headers — they are always \
redacted."
                .to_string(),
        input_schema: obj(
            json!({
                "task_id": prop_task_id(),
                "include_events": prop_include_events(),
                "event_limit": prop_event_limit()
            }),
            &["task_id"],
            false,
        ),
        output_schema: output_envelope(task_data_schema()),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn wait_task_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_wait_task".to_string(),
        title: "Wait for task".to_string(),
        description: "Block until one task reaches a terminal state (completed/failed/cancelled) \
or paused, or until the timeout. While waiting it emits notifications/progress when the caller \
supplied _meta.progressToken, and notifications/cancelled only aborts the wait — it never cancels \
the download. Use it instead of polling rdm_get_task. On timeout it returns code=timeout with \
details.task_id; the task keeps running and can be waited on again. Read-only: it never starts, \
pauses or cancels work."
            .to_string(),
        input_schema: obj(
            json!({
                "task_id": prop_task_id(),
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 3600000,
                    "default": 60000,
                    "description": "Maximum wait in milliseconds. On timeout the task keeps running."
                },
                "include_events": prop_include_events(),
                "event_limit": prop_event_limit()
            }),
            &["task_id"],
            false,
        ),
        output_schema: output_envelope(task_data_schema()),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn pause_task_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_pause_task".to_string(),
        title: "Pause task".to_string(),
        description: "Pause a running download after the engine confirms it (the .part state is \
persisted, so rdm_resume_task continues from the same byte offset). Pausing is control flow, not a \
failure: the returned task has state=paused and no error. Use it to yield bandwidth or on user \
request; do not use it to stop a task permanently (rdm_cancel_task). Awaiting a pause is bounded \
(about 5 s)."
            .to_string(),
        input_schema: obj(json!({ "task_id": prop_task_id() }), &["task_id"], false),
        output_schema: output_envelope(task_data_schema()),
        annotations: ToolAnnotations::new(false, false, true, false),
    }
}

fn resume_task_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_resume_task".to_string(),
        title: "Resume task".to_string(),
        description: "Resume a paused (or cancelled/failed-retryable) task from its saved .part \
state. Idempotent: resuming a task that is already running or completed returns it unchanged. Use \
it after rdm_pause_task, after a process restart, or after fixing a retryable network failure. \
Validators (ETag/Last-Modified/length) are re-checked; if the remote file changed the engine \
discards the partial state and fails with code=remote_changed (retryable)."
            .to_string(),
        input_schema: obj(json!({ "task_id": prop_task_id() }), &["task_id"], false),
        output_schema: output_envelope(task_data_schema()),
        annotations: ToolAnnotations::new(false, false, true, true),
    }
}

fn cancel_task_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_cancel_task".to_string(),
        title: "Cancel task".to_string(),
        description:
            "Cancel a running or paused task. By default the partial .part/.part.json are \
kept so a later rdm_resume_task or a fresh rdm_download can resume; set discard_partial=true to \
delete them. Cancelling is control flow, not a failure: the task ends in state=cancelled. \
Cancelling an already terminal task returns code=conflict. This does not remove a completed file."
                .to_string(),
        input_schema: obj(
            json!({
                "task_id": prop_task_id(),
                "discard_partial": {
                    "type": "boolean",
                    "default": false,
                    "description": "Also delete <file>.part and <file>.part.json after cancelling."
                }
            }),
            &["task_id"],
            false,
        ),
        output_schema: output_envelope(task_data_schema()),
        annotations: ToolAnnotations::new(false, true, true, false),
    }
}

fn verify_file_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_verify_file".to_string(),
        title: "Verify file SHA-256".to_string(),
        description: "Compute the SHA-256 of a local file (streaming, no binary content in the \
response) and optionally compare it with expected_sha256. Identify the file with task_id (uses the \
task's completed result path, always allowed) or with an explicit path inside \
policy.allowed_dirs. Use it to validate a download end-to-end. For huge files pass max_bytes to \
bound hashing cost: a truncated hash is reported with truncated=true and is never presented as a \
match. Read-only."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Verify the completed file of this task."
                },
                "path": {
                    "type": "string",
                    "description": "Explicit file path inside policy.allowed_dirs (when task_id is omitted)."
                },
                "expected_sha256": {
                    "type": "string",
                    "pattern": "^[0-9a-fA-F]{64}$",
                    "description": "Expected hex SHA-256 to compare against."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1_099_511_627_776u64,
                    "description": "Hash at most this many bytes. A truncated hash is not a valid match."
                }
            },
            "required": [],
            "oneOf": [
                { "required": ["task_id"] },
                { "required": ["path"] }
            ],
            "additionalProperties": false
        }),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": [
                "path", "size", "sha256", "expected_sha256", "match",
                "bytes_hashed", "truncated"
            ],
            "properties": {
                "path": { "type": "string" },
                "size": { "type": "integer", "minimum": 0 },
                "sha256": nullable(json!({ "type": "string" })),
                "expected_sha256": nullable(json!({ "type": "string" })),
                "match": nullable(json!({ "type": "boolean" })),
                "bytes_hashed": { "type": "integer", "minimum": 0 },
                "truncated": { "type": "boolean" }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn doctor_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_doctor".to_string(),
        title: "Environment diagnostics".to_string(),
        description: "Run a structured health check: version/platform, agent state directory \
writability, policy file validity, task-store health (stale/corrupt entries), allowed-directory \
existence, GUI/extension IPC liveness, and — when url is supplied — DNS/TLS/HTTP reachability plus \
the policy verdict for that URL. Use it first when something fails in an unfamiliar environment, or \
as a readiness gate before batch work. Read-only; it never downloads."
            .to_string(),
        input_schema: obj(
            json!({
                "url": {
                    "type": "string",
                    "pattern": "^https?://",
                    "description": "Optional URL to include in the network reachability check."
                }
            }),
            &[],
            false,
        ),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": ["ok", "checks", "summary"],
            "properties": {
                "ok": { "type": "boolean" },
                "checks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name", "status"],
                        "properties": {
                            "name": { "type": "string" },
                            "status": { "type": "string", "enum": ["ok", "warning", "error", "skipped"] },
                            "detail": { "type": "string" }
                        },
                        "additionalProperties": true
                    }
                },
                "summary": { "type": "object" }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, true),
    }
}

fn selftest_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_selftest".to_string(),
        title: "Run self-test".to_string(),
        description: "Run a fast end-to-end engine self-check against a loopback fixture server \
started on a random port (no public network): multi-connection hash consistency, graceful 200 \
fallback when the server ignores Range, resume byte-delta after a pause, and checksum-mismatch \
cleanup with zero leftover files. Use it as a readiness probe (e.g. after install or in CI) before \
trusting downloads in an environment. v1.4 also reports `data.recipes`: a read-only self-test of \
the local recipe library (<state>/resolve/recipes) built from pure applicability checks \
(no network, no model, no step execution); recipe failures are reported but do NOT change \
`ok`/`passed` or the exit code (v1 only reports). Read-only with respect to the user's data; \
temporary files live in the agent state dir and are removed. This validates the engine, not the \
remote URL — use rdm_doctor/rdm_probe for that."
            .to_string(),
        input_schema: obj(
            json!({
                "size_bytes": prop_size_bytes(4 * 1024 * 1024, 64 * 1024 * 1024)
            }),
            &[],
            false,
        ),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": ["ok", "cases", "fixture", "elapsed_ms"],
            "properties": {
                "ok": { "type": "boolean" },
                "cases": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name", "status"],
                        "properties": {
                            "name": { "type": "string" },
                            "status": { "type": "string", "enum": ["pass", "fail", "skipped"] },
                            "detail": { "type": "string" },
                            "elapsed_ms": { "type": "integer", "minimum": 0 }
                        },
                        "additionalProperties": true
                    }
                },
                "fixture": { "type": "object" },
                "elapsed_ms": { "type": "integer", "minimum": 0 },
                "recipes": {
                    "type": "object",
                    "description": "Read-only recipe self-test report (v1.4, additive): pure matching checks over <state>/resolve/recipes. No network/model; recipe failures are reported here but never reflected in `ok`/`passed`.",
                    "required": ["dir", "checked", "passed", "failed", "skipped", "failures"],
                    "properties": {
                        "dir": { "type": "string" },
                        "checked": { "type": "integer", "minimum": 0 },
                        "passed": { "type": "integer", "minimum": 0 },
                        "failed": { "type": "integer", "minimum": 0 },
                        "skipped": { "type": "integer", "minimum": 0 },
                        "failures": {
                            "type": "array",
                            "maxItems": 50,
                            "items": {
                                "type": "object",
                                "required": ["recipe_id", "version", "test", "message"],
                                "properties": {
                                    "recipe_id": { "type": "string" },
                                    "version": { "type": "integer", "minimum": 0 },
                                    "test": { "type": "string" },
                                    "message": { "type": "string" }
                                },
                                "additionalProperties": true
                            }
                        }
                    },
                    "additionalProperties": true
                }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn bench_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_bench".to_string(),
        title: "Benchmark connections".to_string(),
        description: "Measure single-stream vs multi-connection throughput against the loopback \
fixture server, repeating each configuration and reporting the median. Use it to choose a \
connection count for a host or to prove that N connections actually help on this machine. Do not \
use it against public URLs (it is fixture-only) and do not read one noisy run as a guarantee: the \
result includes every run and the median. Read-only."
            .to_string(),
        input_schema: obj(
            json!({
                "size_bytes": prop_size_bytes(16 * 1024 * 1024, 256 * 1024 * 1024),
                "connections": {
                    "type": "array",
                    "items": { "type": "integer", "minimum": 1, "maximum": 32 },
                    "minItems": 1,
                    "maxItems": 8,
                    "default": [1, 8],
                    "description": "Connection counts to compare (default [1, 8])."
                },
                "repeats": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 7,
                    "default": 3,
                    "description": "Runs per configuration; the median is reported."
                }
            }),
            &[],
            false,
        ),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": ["fixture", "runs", "medians", "comparison"],
            "properties": {
                "fixture": { "type": "object" },
                "runs": { "type": "array", "items": { "type": "object" } },
                "medians": { "type": "array", "items": { "type": "object" } },
                "comparison": { "type": "object" }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn state_maintenance_tool() -> ToolDefinition {
    ToolDefinition {
        name: "rdm_state_maintenance".to_string(),
        title: "State maintenance".to_string(),
        description: "Scan the agent state directory and the policy-allowed download directories \
for orphan or corrupt .part/.part.json files, unreadable task records and stale non-terminal \
tasks. Default is dry_run=true: it only reports findings. Set dry_run=false to delete orphan and \
corrupt partial files and to prune expired plans/tasks, or to mark stale tasks as failed. Use it \
after crashes or when rdm_doctor reports store warnings; never run repair blindly while another \
process is actively downloading — check rdm_list_tasks first."
            .to_string(),
        input_schema: obj(
            json!({
                "dry_run": prop_dry_run(),
                "stale_task_hours": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 8760,
                    "default": 24,
                    "description": "A non-terminal task untouched for this many hours is reported as stale."
                },
                "max_scan_files": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200000,
                    "default": 20000,
                    "description": "Upper bound on directory entries examined per allowed dir."
                }
            }),
            &[],
            false,
        ),
        output_schema: output_envelope(json!({
            "type": "object",
            "required": ["dry_run", "findings", "repaired", "skipped", "scanned"],
            "properties": {
                "dry_run": { "type": "boolean" },
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["kind", "path", "action"],
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": [
                                    "orphan_part", "orphan_state", "corrupt_state",
                                    "corrupt_task", "stale_task", "expired_plan"
                                ]
                            },
                            "path": { "type": "string" },
                            "detail": { "type": "string" },
                            "action": { "type": "string" }
                        },
                        "additionalProperties": true
                    }
                },
                "repaired": { "type": "integer", "minimum": 0 },
                "skipped": { "type": "integer", "minimum": 0 },
                "scanned": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": true
        })),
        annotations: ToolAnnotations::new(false, true, true, false),
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// `rdm capabilities` / MCP `rdm_capabilities` with default (safe) policy.
pub fn capabilities() -> Value {
    capabilities_for(&Policy::default())
}

/// Capability manifest for a concrete effective policy.
pub fn capabilities_for(policy: &Policy) -> Value {
    let catalog = tool_definitions();
    let tools: Vec<Value> = catalog
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "title": tool.title,
                "read_only": tool.annotations.read_only_hint,
                "destructive": tool.annotations.destructive_hint,
            })
        })
        .collect();
    let tool_names: Vec<String> = catalog.iter().map(|tool| tool.name.clone()).collect();
    json!({
        "schema_version": SCHEMA_VERSION,
        "rdm_version": env!("CARGO_PKG_VERSION"),
        "server": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_versions": PROTOCOL_VERSIONS,
            "preferred_protocol_version": crate::LATEST_PROTOCOL_VERSION,
            "transports": ["stdio"],
            "mcp_capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": false },
                "prompts": {},
                "logging": {}
            }
        },
        "tools": tool_names,
        "tool_details": tools,
        "resources": [
            "rdm://capabilities",
            "rdm://policy",
            "rdm://schema",
            "rdm://tasks",
            "rdm://tasks/{task_id}"
        ],
        "prompts": ["download", "batch_download", "diagnose_failure"],
        "limits": {
            "max_connections": rdm_core::MAX_CONNECTIONS,
            "default_connections": rdm_core::DEFAULT_CONNECTIONS,
            "min_chunk_size": rdm_core::MIN_CHUNK_SIZE,
            "default_chunk_size": rdm_core::DEFAULT_CHUNK_SIZE,
            "max_concurrent_tasks": policy.max_concurrent_tasks,
            "max_wait_timeout_ms": 3_600_000,
            "max_list_limit": 200,
            "approval_token_ttl_minutes": crate::APPROVAL_TOKEN_TTL_MINUTES,
        },
        "transports": ["mcp-stdio", "cli-json"],
        "features": [
            "range-probe", "segmented-download", "work-stealing", "resume",
            "dynamic-connections", "single-stream", "link-resolution", "link-resolution-explore",
            "rate-limit",
            "sha256-verify", "policy",
            "approval-token", "idempotency", "durable-tasks",
            "progress-notifications", "doctor", "selftest", "bench",
            "state-maintenance", "redaction"
        ],
        "error_codes": error_code_names(),
        "task_states": task_state_names(),
        "policy": policy.summary(),
        "approval": {
            "required_over_bytes": policy.require_approval_over_bytes,
            "token_ttl_minutes": crate::APPROVAL_TOKEN_TTL_MINUTES,
            "single_use": true,
        },
        "instructions": "Call rdm_capabilities first, then rdm_probe (or rdm_resolve for page/share \
    links that are not a direct file; rdm_resolve only returns a verified manifest and never creates \
    a task), then rdm_download (or rdm_plan_download -> rdm_apply_plan for large files), and \
    rdm_wait_task/rdm_verify_file to finish. Treat policy_denied/needs_approval as stable contract \
    signals, not transient errors."
    })
}

/// Convenience: a redacted agent-visible plan + optional approval token.
pub fn plan_with_token(plan: &crate::DownloadPlan, token: Option<&ApprovalTokenRecord>) -> Value {
    json!({
        "plan": plan.summary(),
        "approval_token": token.map(|record| record.token.clone()),
        "approval_required": plan.requires_approval,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalog_has_seventeen_unique_tools() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 17);
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, TOOL_NAMES.to_vec());
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), 17, "tool names must be unique");
        for tool in &tools {
            assert_eq!(
                tool_by_name(&tool.name).map(|found| found.name),
                Some(tool.name.clone())
            );
        }
        assert!(tool_by_name("rdm_nope").is_none());
    }

    #[test]
    fn every_tool_has_schemas_and_annotations() {
        for tool in tool_definitions() {
            assert!(!tool.title.trim().is_empty(), "{} title", tool.name);
            assert!(
                tool.description.len() > 120,
                "{} description too short: {}",
                tool.name,
                tool.description.len()
            );
            assert!(
                tool.description.contains("use") || tool.description.contains("Use"),
                "{} description should explain when to use it",
                tool.name
            );
            // Both schemas are serializable JSON and have the MCP-required
            // top-level object shape.
            assert!(!tool.input_schema.to_string().is_empty());
            assert_eq!(
                tool.input_schema.get("type").and_then(Value::as_str),
                Some("object"),
                "{} inputSchema must be an object schema",
                tool.name
            );
            assert_eq!(
                tool.output_schema.get("type").and_then(Value::as_str),
                Some("object"),
                "{} outputSchema must be an object schema",
                tool.name
            );
            let props = tool
                .output_schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("outputSchema properties");
            assert!(props.contains_key("ok"));
            assert!(props.contains_key("data"));
            assert!(props.contains_key("error"));
            // Annotations follow the contract table.
            let read_only = matches!(
                tool.name.as_str(),
                "rdm_capabilities"
                    | "rdm_probe"
                    | "rdm_resolve"
                    | "rdm_plan_download"
                    | "rdm_list_tasks"
                    | "rdm_get_task"
                    | "rdm_wait_task"
                    | "rdm_verify_file"
                    | "rdm_doctor"
                    | "rdm_selftest"
                    | "rdm_bench"
            );
            assert_eq!(
                tool.annotations.read_only_hint, read_only,
                "{} readOnlyHint",
                tool.name
            );
            if tool.annotations.read_only_hint {
                assert!(
                    !tool.annotations.destructive_hint,
                    "{} read-only tools must not be destructive",
                    tool.name
                );
            }
            // openWorld: tools that talk to a remote origin (or may, for
            // doctor's optional URL) are open-world; local state tools are not.
            let open_world = matches!(
                tool.name.as_str(),
                "rdm_probe"
                    | "rdm_resolve"
                    | "rdm_download"
                    | "rdm_plan_download"
                    | "rdm_apply_plan"
                    | "rdm_resume_task"
                    | "rdm_doctor"
            );
            assert_eq!(
                tool.annotations.open_world_hint, open_world,
                "{} openWorldHint",
                tool.name
            );
        }
    }

    #[test]
    fn input_schemas_document_required_fields() {
        let download = tool_by_name("rdm_download").unwrap();
        let required = download
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert!(required.contains(&"url"));
        let probe = tool_by_name("rdm_probe").unwrap();
        assert!(probe.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "url"));
        // Optional tools take no required args.
        for name in [
            "rdm_capabilities",
            "rdm_list_tasks",
            "rdm_doctor",
            "rdm_selftest",
            "rdm_bench",
            "rdm_state_maintenance",
        ] {
            let tool = tool_by_name(name).unwrap();
            assert_eq!(
                tool.input_schema["required"].as_array().map(Vec::len),
                Some(0),
                "{name} should not require args"
            );
        }
        // verify_file accepts exactly one of task_id/path.
        let verify = tool_by_name("rdm_verify_file").unwrap();
        assert!(verify.input_schema.get("oneOf").is_some());
        // Enumerations in schemas are the stable taxonomy.
        let pause = tool_by_name("rdm_pause_task").unwrap();
        assert!(pause.input_schema["properties"]["task_id"]["description"]
            .as_str()
            .unwrap()
            .contains("tsk_"));
    }

    #[test]
    fn capabilities_manifest_is_complete() {
        let capabilities = capabilities_for(&Policy::default());
        assert_eq!(
            capabilities["schema_version"].as_str(),
            Some(SCHEMA_VERSION)
        );
        assert_eq!(capabilities["tools"].as_array().unwrap().len(), 17);
        assert_eq!(
            capabilities["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            TOOL_NAMES.to_vec()
        );
        assert_eq!(capabilities["limits"]["max_connections"].as_u64(), Some(32));
        assert_eq!(
            capabilities["limits"]["default_connections"].as_u64(),
            Some(rdm_core::DEFAULT_CONNECTIONS as u64)
        );
        assert_eq!(
            capabilities["limits"]["max_concurrent_tasks"].as_u64(),
            Some(3)
        );
        assert_eq!(
            capabilities["policy"]["allow_overwrite"].as_bool(),
            Some(false)
        );
        assert!(capabilities["server"]["mcp_capabilities"].is_object());
        assert_eq!(
            capabilities["server"]["protocol_versions"]
                .as_array()
                .unwrap()
                .len(),
            PROTOCOL_VERSIONS.len()
        );
        let error_codes = capabilities["error_codes"].as_array().unwrap();
        assert_eq!(error_codes.len(), ErrorCode::all().len());
        assert!(
            capabilities["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "dynamic-connections"),
            "capabilities.features must advertise dynamic-connections"
        );
        let features: Vec<&str> = capabilities["features"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let single = features
            .iter()
            .position(|feature| *feature == "single-stream")
            .expect("capabilities.features must advertise single-stream");
        let link = features
            .iter()
            .position(|feature| *feature == "link-resolution")
            .expect("capabilities.features must advertise link-resolution");
        let link_explore = features
            .iter()
            .position(|feature| *feature == "link-resolution-explore")
            .expect("capabilities.features must advertise link-resolution-explore");
        assert_eq!(
            features.get(single.wrapping_sub(1)).copied(),
            Some("dynamic-connections"),
            "single-stream must come right after dynamic-connections"
        );
        assert_eq!(
            features.get(link.wrapping_sub(1)).copied(),
            Some("single-stream"),
            "link-resolution must come right after single-stream"
        );
        assert_eq!(
            features.get(link_explore.wrapping_sub(1)).copied(),
            Some("link-resolution"),
            "link-resolution-explore must come right after link-resolution"
        );
        // Every advertised tool actually appears in the catalog.
        for name in capabilities["tools"].as_array().unwrap() {
            assert!(tool_by_name(name.as_str().unwrap()).is_some());
        }
    }

    #[test]
    fn resolve_tool_schema_matches_contract() {
        let tool = tool_by_name("rdm_resolve").unwrap();
        assert_eq!(tool.input_schema["required"][0], "url");
        assert_eq!(
            tool.input_schema["properties"]["mode"]["enum"],
            json!(["auto", "recipe_only", "explore"])
        );
        assert_eq!(
            tool.input_schema["properties"]["budget"]["additionalProperties"],
            false
        );
        assert_eq!(
            tool.input_schema["properties"]["max_resources"]["maximum"],
            40
        );
        assert_eq!(
            tool.input_schema["properties"]["max_llm_calls"]["minimum"],
            1
        );
        assert_eq!(
            tool.input_schema["properties"]["max_llm_calls"]["maximum"],
            32
        );
        assert_eq!(
            tool.input_schema["properties"]["max_llm_calls"]["default"],
            8
        );
        assert!(
            !tool.input_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("max_llm_calls")),
            "max_llm_calls must stay optional"
        );
        assert_eq!(tool.input_schema["additionalProperties"], false);
        assert!(tool.annotations.read_only_hint);
        assert!(!tool.annotations.destructive_hint);
        assert!(tool.annotations.idempotent_hint);
        assert!(tool.annotations.open_world_hint);
        // A failed resolution still carries the full ResolveOutcome payload, so
        // `data` must accept the manifest/diagnostics shape (not only null).
        let data = &tool.output_schema["properties"]["data"]["anyOf"][0];
        assert!(data["properties"]["run_id"].is_object());
        assert!(data["properties"]["manifest"].is_object());
        assert!(data["properties"]["diagnostics"].is_object());
        // v1.6: optional method health (absent for direct-link/draft runs).
        let health = &data["properties"]["recipe_health"];
        assert_eq!(health["anyOf"][0]["type"], "object");
        assert_eq!(health["anyOf"][1]["type"], "null");
        let health_schema = &health["anyOf"][0];
        assert_eq!(
            health_schema["properties"]["status"]["enum"],
            json!(["ok", "degraded", "broken", "untested"])
        );
        assert!(health_schema["properties"]["last_failure"]["anyOf"][0].is_object());
        assert!(
            !data["required"]
                .as_array()
                .unwrap()
                .contains(&json!("recipe_health")),
            "recipe_health must stay optional"
        );
    }

    #[test]
    fn selftest_schema_advertises_the_recipe_report() {
        let tool = tool_by_name("rdm_selftest").unwrap();
        // Read-only + local fixture + the additive recipe report.
        assert!(tool.annotations.read_only_hint);
        assert!(!tool.annotations.open_world_hint);
        let data = &tool.output_schema["properties"]["data"]["anyOf"][0];
        let recipes = &data["properties"]["recipes"];
        assert_eq!(recipes["type"], "object");
        for key in ["dir", "checked", "passed", "failed", "skipped", "failures"] {
            assert!(
                recipes["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == key),
                "recipes schema must require {key}"
            );
        }
        assert_eq!(recipes["properties"]["failures"]["maxItems"], 50);
        assert_eq!(
            recipes["properties"]["failures"]["items"]["required"],
            json!(["recipe_id", "version", "test", "message"])
        );
        // The field is additive/optional at the envelope level, so v1.3 clients
        // that validate against the old schema keep working.
        assert!(
            !data["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "recipes"),
            "recipes must stay additive, not required"
        );
        assert!(tool
            .description
            .contains("read-only self-test of the local recipe library"));
    }

    #[test]
    fn single_stream_is_advertised_in_tool_and_plan_schemas() {
        for name in ["rdm_download", "rdm_plan_download"] {
            let tool = tool_by_name(name).unwrap();
            let prop = &tool.input_schema["properties"]["single_stream"];
            assert_eq!(prop["type"], "boolean", "{name} single_stream type");
            assert_eq!(prop["default"], false, "{name} single_stream default");
            assert!(
                prop["description"].as_str().unwrap().contains("one-time"),
                "{name} single_stream description"
            );
            assert_eq!(
                tool.input_schema["additionalProperties"], false,
                "{name} must keep additionalProperties=false"
            );
        }
        // The plan carries the flag so plan -> apply preserves it.
        let plan = tool_by_name("rdm_plan_download").unwrap();
        let plan_data = &plan.output_schema["properties"]["data"]["anyOf"][0];
        assert_eq!(
            plan_data["properties"]["plan"]["properties"]["single_stream"]["type"],
            "boolean"
        );
    }

    #[test]
    fn dynamic_connections_is_advertised_in_tool_schemas() {
        for name in ["rdm_download", "rdm_plan_download"] {
            let tool = tool_by_name(name).unwrap();
            let prop = &tool.input_schema["properties"]["dynamic_connections"];
            assert_eq!(prop["type"], "boolean", "{name} dynamic_connections type");
            assert_eq!(prop["default"], true, "{name} dynamic_connections default");
            assert!(
                prop["description"]
                    .as_str()
                    .unwrap()
                    .contains("Dynamically"),
                "{name} dynamic_connections description"
            );
        } // The plan output carries the flag so approval flows preserve it.
        let plan = tool_by_name("rdm_plan_download").unwrap();
        let plan_data = &plan.output_schema["properties"]["data"]["anyOf"][0];
        assert_eq!(
            plan_data["properties"]["plan"]["properties"]["dynamic_connections"]["type"],
            "boolean"
        );
        // Agent-visible task shapes expose the observed peak.
        let view = task_view_schema();
        assert_eq!(view["properties"]["peak_connections"]["type"], "integer");
        assert_eq!(view["properties"]["dynamic_connections"]["type"], "boolean");
        assert_eq!(
            task_summary_schema()["properties"]["peak_connections"]["type"],
            "integer"
        );
        assert_eq!(
            task_result_schema()["properties"]["peak_connections"]["type"],
            "integer"
        );
    }

    #[test]
    fn plan_with_token_is_redacted() {
        let mut request = rdm_core::DownloadRequest::new("https://user:pass@example.com/a.bin");
        request.cookie = Some("secret=1".to_string());
        request.output_dir = Some(std::path::PathBuf::from("C:\\tmp"));
        request.single_stream = true;
        let plan = crate::DownloadPlan::new(
            request,
            Some(123),
            true,
            Some("over threshold".to_string()),
            None,
            false,
            "mcp",
        );
        let token = ApprovalTokenRecord::for_plan(&plan);
        let payload = plan_with_token(&plan, Some(&token));
        assert_eq!(payload["approval_required"], true);
        assert_eq!(payload["approval_token"], token.token);
        assert!(!payload.to_string().contains("user:pass"));
        assert!(!payload.to_string().contains("secret=1"));
        let summary = plan.summary();
        assert_eq!(summary["plan_id"], plan.plan_id);
        assert_eq!(summary["requires_approval"], true);
        // single_stream plans are reported normalized: one connection, no
        // dynamic planning, no chunk plan.
        assert_eq!(summary["single_stream"], true);
        assert_eq!(summary["connections"], 1);
        assert_eq!(summary["dynamic_connections"], false);
        assert_eq!(summary["chunk_size"], Value::Null);
    }
}
