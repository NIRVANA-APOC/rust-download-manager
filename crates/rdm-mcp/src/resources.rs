//! MCP resources and prompts (contract §3.2 / §3.3).
//!
//! * `rdm://capabilities` — same manifest as `rdm_capabilities`
//! * `rdm://policy` — effective (redacted) policy + where it came from
//! * `rdm://schema` — every tool input/output schema
//! * `rdm://tasks` — recent task summaries
//! * `rdm://tasks/{task_id}` — one task detail + recent events
//!
//! Prompts guide an agent through a safe download, a batch, or a failure
//! diagnosis without having to read the repository.

use std::sync::Arc;

use rdm_agent::{capabilities_for, TaskFilter, TaskState};
use serde_json::{json, Map, Value};

use crate::protocol::RpcError;
use crate::server::ServerState;

const JSON_MIME: &str = "application/json";

pub fn resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": "rdm://capabilities",
                "name": "RDM capabilities",
                "description": "Version, tools, limits, policy summary, transports and MCP protocol versions.",
                "mimeType": JSON_MIME,
            },
            {
                "uri": "rdm://policy",
                "name": "Effective agent policy",
                "description": "The policy currently enforced by this server (allowed hosts/dirs, size limits, concurrency, overwrite).",
                "mimeType": JSON_MIME,
            },
            {
                "uri": "rdm://schema",
                "name": "Tool schemas",
                "description": "Machine-readable inputSchema/outputSchema for all 17 RDM tools.",
                "mimeType": JSON_MIME,
            },
            {
                "uri": "rdm://tasks",
                "name": "Recent tasks",
                "description": "Most recent durable download tasks (redacted summaries).",
                "mimeType": JSON_MIME,
            }
        ]
    })
}

pub fn resource_templates_list() -> Value {
    json!({
        "resourceTemplates": [
            {
                "uriTemplate": "rdm://tasks/{task_id}",
                "name": "Task detail",
                "description": "One task (TaskView) plus its most recent audit events.",
                "mimeType": JSON_MIME,
            }
        ]
    })
}

fn content(uri: &str, value: Value) -> Value {
    json!({
        "contents": [{
            "uri": uri,
            "mimeType": JSON_MIME,
            "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        }]
    })
}

pub fn read_resource(server: &Arc<ServerState>, uri: &str) -> Result<Value, RpcError> {
    match uri {
        "rdm://capabilities" => Ok(content(uri, capabilities_for(&server.policy))),
        "rdm://policy" => {
            let path = rdm_agent::Policy::path_for(&server.state_root);
            let source = if path.exists() { "file" } else { "defaults" };
            let value = json!({
                "schema_version": rdm_agent::SCHEMA_VERSION,
                "source": source,
                "path": path.display().to_string(),
                "effective": server.policy.summary(),
                "notes": [
                    "Agent-visible policy is normalised; edit policy.json and restart the server to change it.",
                    "Loopback/private/cloud-metadata hosts are rejected unless allow_private_hosts=true and allowed_hosts matches."
                ]
            });
            Ok(content(uri, value))
        }
        "rdm://schema" => {
            let tools: Vec<Value> = rdm_agent::tool_definitions()
                .into_iter()
                .map(|tool| serde_json::to_value(tool).unwrap_or(Value::Null))
                .collect();
            Ok(content(
                uri,
                json!({
                    "schema_version": rdm_agent::SCHEMA_VERSION,
                    "tools": tools,
                }),
            ))
        }
        "rdm://tasks" => {
            let tasks = server
                .manager
                .store()
                .list(&TaskFilter {
                    limit: Some(20),
                    ..TaskFilter::default()
                })
                .map_err(|error| RpcError::internal(format!("cannot list tasks: {error}")))?;
            let summaries: Vec<Value> = tasks
                .iter()
                .map(|record| {
                    serde_json::to_value(rdm_agent::TaskSummary::from(record))
                        .unwrap_or(Value::Null)
                })
                .collect();
            Ok(content(
                uri,
                json!({
                    "schema_version": rdm_agent::SCHEMA_VERSION,
                    "tasks": summaries,
                }),
            ))
        }
        _ => {
            let task_id = uri
                .strip_prefix("rdm://tasks/")
                .ok_or_else(|| RpcError::resource_not_found(uri))?;
            if !rdm_agent::TaskStore::is_valid_id(task_id) {
                return Err(RpcError::resource_not_found(uri));
            }
            let record = server
                .manager
                .store()
                .get(task_id)
                .map_err(|_| RpcError::resource_not_found(uri))?;
            let events = server
                .manager
                .store()
                .events(task_id, 20)
                .unwrap_or_default();
            let value = json!({
                "schema_version": rdm_agent::SCHEMA_VERSION,
                "task": record.view(),
                "events": events,
            });
            Ok(content(uri, value))
        }
    }
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

pub fn prompts_list() -> Value {
    json!({
        "prompts": [
            {
                "name": "download",
                "title": "Safe single download",
                "description": "Plan and execute one download with RDM, including policy/approval handling and checksum verification.",
                "arguments": [
                    { "name": "url", "description": "http(s) URL to download", "required": true },
                    { "name": "dest_dir", "description": "Optional destination directory inside policy.allowed_dirs", "required": false }
                ]
            },
            {
                "name": "batch_download",
                "title": "Batch download",
                "description": "Download several URLs with bounded concurrency, idempotency keys and error handling.",
                "arguments": [
                    { "name": "urls", "description": "Comma-separated http(s) URLs", "required": true }
                ]
            },
            {
                "name": "diagnose_failure",
                "title": "Diagnose a failed task",
                "description": "Use task records, events and doctor output to decide whether to retry, resume or give up.",
                "arguments": [
                    { "name": "task_id", "description": "Task id (tsk_...)", "required": true }
                ]
            }
        ]
    })
}

pub fn get_prompt(params: &Value) -> Result<Value, RpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("prompts/get params must be an object"))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("missing prompt `name`"))?;
    let arguments = object
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required = |field: &str| -> Result<String, RpcError> {
        arguments
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                RpcError::invalid_params(format!("prompt `{name}` requires argument `{field}`"))
            })
    };
    let text = match name {
        "download" => {
            let url = required("url")?;
            let dest = arguments
                .get("dest_dir")
                .and_then(Value::as_str)
                .map(str::to_owned);
            download_prompt(&url, dest.as_deref())
        }
        "batch_download" => batch_prompt(&required("urls")?),
        "diagnose_failure" => diagnose_prompt(&required("task_id")?),
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown prompt `{other}`"
            )))
        }
    };
    Ok(json!({
        "description": format!("RDM guidance for {name}"),
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": text },
        }],
    }))
}

fn download_prompt(url: &str, dest_dir: Option<&str>) -> String {
    let dest = dest_dir
        .map(|dir| format!("到目录 `{dir}`"))
        .unwrap_or_else(|| "到 policy 允许的默认目录（通常是 Downloads）".to_string());
    format!(
        r#"请使用 RDM MCP 工具安全地下载 `{url}` {dest}。严格按以下流程执行，不要猜参数：

1. 先调用 `rdm_capabilities`，确认工具/限制/策略摘要（尤其 allowed_dirs、allowed_hosts、allow_private_hosts、require_approval_over_bytes）。
2. 调用 `rdm_probe`（只传 url 及必要的 headers/cookie）获取最终 URL、文件名、总大小、Range 支持、ETag/HTTP 版本。若返回 `policy_denied`，停止并把原因告诉用户；若 `final_url` 被重定向到策略外主机，不要下载。
3. 若总大小超过审批阈值：调用 `rdm_plan_download` 拿到 `plan.plan_id` 与 `approval_token`，再调用 `rdm_apply_plan`（同时传 plan_id 与 approval_token；token 单次有效、10 分钟过期）。若未超阈值，直接调用 `rdm_download`。
4. `rdm_download` 调用时建议：`connections` 用 8（服务器不支持 Range 时引擎会自动降级为 1）；大文件传 `expected_sha256`；用 `idempotency_key` 让重试安全；`wait: true` 并在 `_meta.progressToken` 传入 token 以接收进度通知。
5. 若返回 `needs_approval`，资料在 `details.plan`/`details.approval_token`，按第 3 步 apply；不要重复创建任务。
6. 下载结束后用 `rdm_verify_file`（task_id + expected_sha256）校验；失败用 `rdm_get_task`/`rdm_doctor` 诊断。
7. 错误处理：按 `error.code`/`error.retryable` 决策；`network_error`/`timeout`/`http_status`(5xx/429) 可退避重试，`policy_denied`/`checksum_mismatch`/`conflict` 不要盲目重试。pause/cancel 是控制流，不是失败。
安全：cookie/authorization 只作为参数传入，不会出现在输出或日志；不要把它们写进对话或文件。"#
    )
}

fn batch_prompt(urls: &str) -> String {
    format!(
        r#"请用 RDM 批量下载以下 URL（逗号分隔）：`{urls}`。要求：

1. 先 `rdm_capabilities` + 对每个 URL `rdm_probe`，跳过非法/策略拒绝/明显不可用的 URL，并记录原因。
2. 对每个 URL 调用 `rdm_download`（`wait:false`），传稳定的 `idempotency_key`（如 `batch-<url 的 sha256 前 12 位>`）；若返回 `needs_approval`，逐个 `rdm_plan_download` → `rdm_apply_plan`，没有 token 不要绕过。
3. 并发受 `policy.max_concurrent_tasks` 限制（服务端自动排队），不要自己用超过 3 个的并行循环同时灌任务；推荐最多同时等 3 个 `rdm_wait_task`。
4. 用 `rdm_wait_task` 等待（`timeout_ms` 300000 级别），用 `_meta.progressToken` 接收进度；超时不是失败，继续 `rdm_wait_task` 或 `rdm_get_task`。
5. 全部结束后 `rdm_list_tasks` 汇总 completed/failed/cancelled，对 completed 的用 `rdm_verify_file` 校验（有 expected_sha256 时）。
6. 输出机器可读汇总：每个 URL 的 task_id、state、path、sha256、error.code/retryable。部分失败不要中断其余任务（除非用户要求 stop_on_error）。"#
    )
}

fn diagnose_prompt(task_id: &str) -> String {
    format!(
        r#"请诊断 RDM 任务 `{task_id}` 的失败/异常。按顺序：

1. `rdm_get_task`（include_events:true, event_limit:50）读取状态、error.code/message/hint/retryable、warnings、下载进度。
2. 若为 failed：判断 retryable。`network_error`/`timeout`/`http_status`(5xx/429)/`resume_invalid`/`remote_changed` 可先 `rdm_resume_task`（部分错误需重新 `rdm_download`）；`checksum_mismatch` 说明文件损坏，需重新下载并核对 expected_sha256；`policy_denied`/`conflict`/`permission_denied` 需调整 policy/目标路径，不能重试绕过。
3. 若为 paused：`rdm_resume_task` 继续；若为 cancelled：确认用户意图后 `rdm_resume_task`（保留 .part）或放弃。
4. 仍无法定位时调用 `rdm_doctor`（必要时带 url）检查策略/状态目录/网络/GUI，再 `rdm_state_maintenance`（dry_run=true）检查孤儿/corrupt 状态。
5. 给出结论：根因、下一步动作（含具体工具参数）、是否需要用户介入修改 policy.json；不要编造未观测到的原因。"#
    )
}

/// Static list of task states (schema use).
pub fn task_state_values() -> Vec<&'static str> {
    TaskState::all()
        .iter()
        .map(|state| state.as_str())
        .collect()
}

/// Helper used by tests: prompt argument map.
pub fn prompt_arguments(value: &Value) -> Map<String, Value> {
    value
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_lists_have_expected_uris() {
        let list = resources_list();
        let uris: Vec<&str> = list["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|resource| resource["uri"].as_str())
            .collect();
        assert!(uris.contains(&"rdm://capabilities"));
        assert!(uris.contains(&"rdm://policy"));
        assert!(uris.contains(&"rdm://schema"));
        assert!(uris.contains(&"rdm://tasks"));
        let templates = resource_templates_list();
        assert_eq!(
            templates["resourceTemplates"][0]["uriTemplate"],
            "rdm://tasks/{task_id}"
        );
    }

    #[test]
    fn prompts_are_complete() {
        let list = prompts_list();
        let names: Vec<&str> = list["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|prompt| prompt["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec!["download", "batch_download", "diagnose_failure"]
        );
        for prompt in list["prompts"].as_array().unwrap() {
            assert!(prompt["description"].as_str().unwrap().len() > 20);
            assert!(prompt["arguments"].is_array());
        }
    }

    #[test]
    fn prompt_get_builds_text_and_validates_args() {
        let value = get_prompt(&json!({
            "name": "download",
            "arguments": { "url": "https://example.com/file.iso", "dest_dir": "C:\\Downloads\\agent" }
        }))
        .unwrap();
        let text = value["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("rdm_probe"));
        assert!(text.contains("rdm_apply_plan"));
        assert!(text.contains("C:\\Downloads\\agent"));
        let error = get_prompt(&json!({ "name": "download", "arguments": {} })).unwrap_err();
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
        let error = get_prompt(&json!({ "name": "nope", "arguments": {} })).unwrap_err();
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
    }

    #[test]
    fn prompt_arguments_helper_defaults_to_empty() {
        assert!(prompt_arguments(&json!({})).is_empty());
        assert_eq!(
            prompt_arguments(&json!({"arguments": {"a": "b"}}))
                .get("a")
                .unwrap(),
            "b"
        );
    }

    #[test]
    fn task_state_values_are_stable() {
        assert!(task_state_values().contains(&"completed"));
    }
}
