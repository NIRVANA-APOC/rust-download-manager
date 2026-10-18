# RDM agent 参考（MCP + CLI 完整参数）

> 本文件是 `skills/rdm/SKILL.md` 的展开版。契约与版本化字段以
> `docs/agent-integration.md` 为准；本文补充**实际实现**的参数、输出与坑。
> 版本：rdm 0.1.0 · schema `rdm.agent/v1` · MCP legacy `2025-06-18`（兼容
> `2024-11-05`/`2025-03-26`）+ modern `2026-07-28`（dual-era，增量）。

## 1. MCP 传输与握手

- stdio，每行一个 JSON-RPC 2.0 消息（newline-delimited）；stdout 只走协议，日志走 stderr。
- Legacy：`initialize` → `notifications/initialized` → 其它请求。
  - `initialize` 请求：`{protocolVersion, capabilities, clientInfo}`
  - 响应：`{protocolVersion, capabilities:{tools,resources,prompts,logging}, serverInfo:{name:"rdm",version}, instructions}`
  - 服务端优先 `2025-06-18`；客户端给 `2024-11-05`/`2025-03-26`/`2025-06-18` 时 echo 该版本；其它回 `2025-06-18`。
- `ping`（legacy）→ `{}`；`logging/setLevel`（legacy）接受并记录。
- 进度：请求 `params._meta.progressToken` → 服务端在 `rdm_download(wait=true)`/`rdm_wait_task` 期间发
  `notifications/progress {progressToken, progress(已下载字节), total?, message}`（引擎 250ms 采样）。
- 取消等待：`notifications/cancelled {requestId, reason?}` 只中止对应请求的等待，任务继续。
- 关键状态变化发 `notifications/message {level, logger:"rdm-mcp", data}`（脱敏）。
- stdin EOF：暂停运行中任务并持久化，退出码 0。

### Dual-era（modern 2026-07-28，增量）

| 能力 | 状态 |
|---|---|
| `server/discover`（需 `params._meta` 协议版本+客户端能力） | ✅ 返回 `{resultType:"complete", supportedVersions:["2026-07-28"], capabilities, instructions, ttlMs, cacheScope, _meta.serverInfo}` |
| modern 每请求 `_meta` 校验 | ✅ 缺失/版本不支持 → `-32602` / `-32022`（`data.supported/requested`） |
| `resultType:"complete"` | ✅ modern 路径的 tools/resources/prompts 结果 |
| 列表缓存字段 `ttlMs`/`cacheScope` | ✅ tools/resources/prompts 列表 |
| `ping`/`logging/setLevel` | modern 路径返回 `-32601`（已从新版移除） |
| `resultType:"input_required"`（MRTR 人工审批） | ❌ P1（当前用 needs_approval + token 两步确认） |
| Tasks 扩展（`io.modelcontextprotocol/tasks`） | ❌ P1（当前用 `task_id` + `rdm_wait_task` 轮询语义） |
| modern 按请求 `_meta.logLevel` opt-in 日志 | ⚠️ 部分（仍按 legacy 发送 notifications/message；未 opt-in 的现代客户端可忽略） |

## 2. 工具输出信封

`tools/call` 成功与失败都返回：

```jsonc
{
  "content": [{"type":"text","text":"人类摘要/错误文本"}],
  "structuredContent": {
    "schema_version": "rdm.agent/v1",
    "ok": true,
    "task_id": "tsk_...|null",
    "data": { /* 工具特定 */ } | null,
    "error": null | {
      "code": "policy_denied", "message": "...", "retryable": false,
      "hint": "...", "details": { }
    }
  },
  "isError": false
}
```

- 工具失败：`isError:true` + `error`，**不是** JSON-RPC error。
- 参数/工具名不合法：JSON-RPC `-32602`（`invalid params`）、未知 method `-32601`。
- 每个工具均有 `inputSchema`/`outputSchema`（JSON Schema 2020-12 子集），可从
  `tools/list`、`rdm schema`、`resources/read rdm://schema` 获取。

## 3. 工具参数与输出（16 个）

通用网络参数（probe/download/plan 子集）：`headers`（object<string,string>）、`cookie`、
`referer`、`user_agent`、`proxy`、`insecure`（bool，debug 镜像）。

### rdm_capabilities
- 输入：无。
- 输出 `data`：`schema_version, rdm_version, server{name,version,protocol_versions,preferred_protocol_version,mcp_capabilities}, tools[], tool_details[], resources[], prompts[], limits{max_connections=32,default_connections=8,min_chunk_size,default_chunk_size,max_concurrent_tasks,max_wait_timeout_ms=3600000,max_list_limit=200,approval_token_ttl_minutes=10}, transports[], features[], error_codes[], task_states[], policy{...}, approval{required_over_bytes,token_ttl_minutes,single_use}, instructions`。

### rdm_probe
- 输入：`url`（required）、通用网络参数。
- 输出 `data`：`url, final_url, file_name, total_bytes|null, range_supported, content_type|null, etag|null, last_modified|null, http_version|null, policy{host_allowed, final_url_allowed, requires_approval, max_bytes_per_task, approval_threshold_bytes, allowed_dirs}, warnings[]`。
- 说明：不落文件、不建任务；`final_url_allowed=false` 表示重定向跨出策略（下载会被拒）。
- 策略：`policy_denied` 会在发请求前拦截；probe 本身仍需 host 通过策略。

### rdm_download
- 输入：`url`（required）、`dest_dir`、`file_name`、`connections`（1..32，默认策略值）、
  `chunk_size`（≥262144）、`expected_sha256`、`speed_limit_bps`、`resume`（默认 true）、
  `headers/cookie/referer/user_agent/proxy/insecure`、`idempotency_key`、`wait`（默认 false）、
  `wait_timeout_ms`（1..3600000，默认 60000）。
- 输出 `data`：`{task: TaskView, events?: [...]}`（`wait=true` 时返回终态；超时返回 error `timeout` 且任务继续）。
- 失败先例：`invalid_request`/`invalid_url`/`policy_denied`/`needs_approval`/`conflict`/`network_error`/`http_status`。
- `allow_overwrite=false` 时目标存在 → `conflict`（不会静默改名/覆盖）；`resume=true` 且存在 `.part` → 续传。
- `idempotency_key` 相同：直接返回已有 task（不重复下载）。

### rdm_plan_download
- 输入：同 download（`dest_dir/file_name/connections/chunk_size/expected_sha256/speed_limit_bps/resume/idempotency_key/headers/...`），无 `wait`。
- 输出 `data`：`{plan: {schema_version,plan_id,url,dest_dir,file_name,connections,chunk_size,expected_sha256,speed_limit_bps,total_bytes,requires_approval,reason,created_at,expires_at,idempotency_key}, approval_token: string|null, approval_required: bool}`。
- 不创建 task；plan 持久化 10 分钟；同 URL 已存在目标会先报 `conflict`。

### rdm_apply_plan
- 输入：`plan_id`（required）、`approval_token`（plan 需要审批时 required）、`idempotency_key?`、`wait?`、`wait_timeout_ms?`。
- 行为：校验 token（单次、10 分钟）→ 用当前 policy 重新校验 URL/目录/大小与重定向 → 重新 probe → 冲突检查 → 创建任务（默认不等待）→ 删除 plan。
- 输出：`data.task`；错误：`not_found`（plan 不存在/过期/已用）、`needs_approval`（token 缺失/无效/过期）、`policy_denied`（策略变化）、`conflict`。

### rdm_list_tasks
- 输入：`states?`（数组，`queued|probing|downloading|paused|completed|failed|cancelled`）、`limit?`（1..200，默认 50）、`cursor?`。
- 输出：`{tasks:[TaskSummary], next_cursor: string|null, total: int}`；按 `created_at` 倒序，分页游标是上一页最后一个 task id。

### rdm_get_task
- 输入：`task_id`（required）、`include_events?`（默认 false）、`event_limit?`（0..500，默认 50）。
- 输出：`{task: TaskView, events?: TaskEvent[]}`；事件类型 `created/probing/metadata/started/progress/retry/paused/resumed/cancelled/completed/failed/engine-failed/stale`。

### rdm_wait_task
- 输入：`task_id`（required）、`timeout_ms?`（1..3600000，默认 60000）、`include_events?`、`event_limit?`。
- 返回条件：终态（completed/failed/cancelled）或 `paused`；超时 → error `timeout`（`details.task_id/state/downloaded_bytes`）。
- 期间若请求带 `_meta.progressToken`，每约 250ms 发 `notifications/progress`。
- 只读：不会 pause/cancel 任务。

### rdm_pause_task / rdm_resume_task
- 输入：`task_id`。
- `pause`：引擎确认（≤5s）后返回 `task.state=paused`；已终态 → `conflict`。
- `resume`：从 `.part` 恢复；幂等（running/completed 原样返回）；`failed` 仅在 `retryable` 时允许；非本进程拥有的 task → `conflict`。

### rdm_cancel_task
- 输入：`task_id`、`discard_partial?`（默认 false）。
- 默认保留 `.part/.part.json`（后续 `resume` 可继续）；终态任务 → `conflict`；已 cancelled → 幂等返回。

### rdm_verify_file
- 输入：`task_id` **或** `path`（二选一，`oneOf`；显式 path 需在 allowed_dirs 内）、`expected_sha256?`、`max_bytes?`。
- 输出：`{path,size,sha256,expected_sha256,match:bool|null,bytes_hashed,truncated,source:"task"|"path"}`。
- `max_bytes` 截断时 `truncated=true` 且 `match=null`；不匹配 → `checksum_mismatch`（含 details，不返回大文件内容）。

### rdm_doctor
- 输入：`url?`。
- 检查项：version、state_dir、policy_file、download_dir、task_store（含 corrupt/stale）、plans、gui_ipc、network（仅当给 url）。
- 输出：`{ok, checks:[{name,status:ok|warning|error|skipped,detail}], summary{...}}`；doctor 本身不因检查失败而报错。

### rdm_selftest
- 输入：`size_bytes?`（64KiB..64MiB，默认 4MiB）。
- 4 个用例（loopback fixture 随机端口）：`multi_connection_hash`（多连接哈希一致）、
  `range_fallback`（服务器忽略 Range 时 200 降级）、`pause_resume_delta`（暂停后续传只补缺失字节）、
  `checksum_cleanup`（校验失败零残留）。
- 输出：`{ok, cases:[{name,status:pass|fail,detail,elapsed_ms}], passed, failed, fixture, elapsed_ms}`。

### rdm_bench
- 输入：`size_bytes?`（64KiB..256MiB，默认 16MiB）、`connections?`（数组 1..32，默认 `[1,8]`，最多 8 个）、`repeats?`（1..7，默认 3）。
- 输出：`{fixture, size_bytes, repeats, runs[], medians[{connections,median_avg_speed_bps,median_elapsed_ms}], comparison{baseline,best,speedup,note}}`。

### rdm_state_maintenance
- 输入：`dry_run?`（默认 true）、`stale_task_hours?`（1..8760，默认 24）、`max_scan_files?`（默认 20000）。
- 扫描：`tasks/corrupt_task`、`stale_task`、`plans/expired_plan`、allowed_dirs 下
  `orphan_part`/`orphan_state`/`corrupt_state`。
- 输出：`{dry_run, findings:[{kind,path,detail,action}], repaired, skipped, scanned, stale_task_hours, max_scan_files}`。
- `dry_run=false`：删除孤儿/损坏 partial、隔离损坏 task 记录（改扩展名 `.json.corrupt`）、删除过期 plan、把 stale 任务标记为 `failed`（`resume_invalid`，可 `resume`）。

## 4. TaskView / TaskSummary / 事件

TaskView（契约 §2.4，所有输出已脱敏）：

```jsonc
{
  "schema_version":"rdm.agent/v1","id":"tsk_...","state":"queued|probing|downloading|paused|completed|failed|cancelled",
  "source":"mcp|cli|test","url":"https://...","file_name":"a.iso","dest_dir":"C:\\...",
  "path":"C:\\...\\a.iso"|null,"total_bytes":67108864|null,"downloaded_bytes":1048576,
  "connections":8,"chunk_size":2097152,"range_supported":true,"http_version":"HTTP/1.1",
  "speed_bps":1234567.0,"eta_secs":52.0,"warnings":["range_unsupported"],
  "error":null,"result":{"path":"...","size":67108864,"sha256":"...","elapsed_ms":1234,"avg_speed_bps":...}|null,
  "idempotency_key":"job-42"|null,"created_at":"...","updated_at":"...","finished_at":null
}
```

TaskSummary = `{id,state,url,file_name,dest_dir,path,total_bytes,downloaded_bytes,connections,speed_bps,created_at,updated_at}`。

> `paused`/`cancelled` 的 `downloaded_bytes` 是 `.part.json` 中**实际可续传**的字节数（精确值，
> 不含正在传输但被回滚的块）；因此可能略小于最后一次 progress 通知的数值。

TaskEvent：`{"schema_version","type","task_id","seq","ts","data"}`（`data` 已防御性脱敏）。

## 5. Resources 与 Prompts

| resource URI | 内容 |
|---|---|
| `rdm://capabilities` | 同 `rdm_capabilities` |
| `rdm://policy` | `{source:"file"|"defaults", path, effective:{...}, notes[]}` |
| `rdm://schema` | `{schema_version, tools:[ToolDefinition...]}` |
| `rdm://tasks` | `{schema_version, tasks:[TaskSummary]}`（最近 20） |
| `rdm://tasks/{task_id}` | `{schema_version, task:TaskView, events:[最近 20]}`；不存在 → `-32002` |

| prompt | 参数 | 用途 |
|---|---|---|
| `download` | `url`(required), `dest_dir?` | 安全单文件下载流程（probe→policy/approval→download→wait→verify） |
| `batch_download` | `urls`(逗号分隔, required) | 批量规划/并发/汇总 |
| `diagnose_failure` | `task_id`(required) | 失败诊断与重试/恢复决策 |

## 6. 策略文件（完整字段）

`%LOCALAPPDATA%\RDM\agent\policy.json`（存在则覆盖默认；非法 JSON 回退默认）：

```jsonc
{
  "schema_version": "rdm.agent/v1",
  "allowed_hosts": ["*.example.com", "127.0.0.1", "localhost"], // "*" = 任意（私有地址仍受 allow_private_hosts 限制）
  "allowed_dirs": ["C:\\Users\\me\\Downloads\\agent"],
  "max_bytes_per_task": 10737418240,
  "require_approval_over_bytes": 1073741824,
  "max_concurrent_tasks": 3,
  "default_connections": 8,
  "allow_private_hosts": false,
  "allow_overwrite": false
}
```

- `allowed_hosts` 匹配：精确、`*`、`*.suffix`（也匹配 apex）。大小写不敏感、忽略末尾点。
- `allowed_dirs`：canonicalize 最近存在祖先 + 词法追加，`..` 不能逃逸；目录可尚不存在。
- `allow_private_hosts=false` 拒绝：`localhost`、`*.localhost`、`*.internal`、`metadata*`、RFC1918、loopback、link-local（含 `169.254.169.254`）、unspecified、IPv6 ULA/link-local。
- probe 后对 `final_url` 的 host 再校验；跨出策略 → `policy_denied`（stage=`final_url`）。
- 未知长度下载在任务管理器内按 `max_bytes_per_task` 监控，超限取消并标记 `failed/policy_denied`。

## 7. 状态目录布局

`%LOCALAPPDATA%\RDM\agent\`（`--state-dir` / `RDM_AGENT_STATE_DIR` 可覆盖）：

```
tasks/<id>.json      # 每任务独立、原子写（tmp+rename）
events/<id>.jsonl    # 脱敏事件日志
plans/<plan_id>.json # 待执行 plan（10 分钟过期）
policy.json          # 可选
```

`rdm-agent` 拒绝非法 id（`../` 穿越），id 字符集 `[A-Za-z0-9_-]{1,160}`。
下载文件本体及 `.part/.part.json` 在用户目标目录，不在 store 内。

## 8. CLI 速查（`--output json` 信封）

```
rdm capabilities --output json
rdm schema --output json
rdm probe <url> --output json
rdm download <url> [--dir D] [--name N] [--connections 8] [--sha256 HEX] [--output json|ndjson]
rdm tasks [--state S] [--limit N] [--output json]
rdm task <id> [--events N] [--output json]
rdm run --input <file|-> [--wait] [--output json]
rdm doctor [--url URL] --output json
rdm selftest --output json
rdm bench [--size 64M] [--connections 1,8] --output json
rdm state inspect --output json
rdm state repair [--dry-run] --output json
```

信封：`{schema_version:"rdm.agent/v1", ok, command, task_id?, data, error, meta{rdm_version,elapsed_ms,ts,trace_id?}}`。
NDJSON：每行事件 `{schema_version,type,task_id,seq,ts,data}`（probe/progress/retry/completed/failed）。
退出码：0 成功、2 失败、3 中断/暂停。

`rdm run` 输入：

```jsonc
{"schema_version":"rdm.agent/v1",
 "tasks":[{"url":"https://...","dest_dir":"...","file_name":"a.iso"}],
 "max_concurrency":3,"wait":true,"stop_on_error":false}
```

## 9. 错误码（稳定枚举）

`invalid_request, invalid_url, policy_denied, needs_approval, not_found, conflict, network_error,
http_status, range_unsupported, no_content_length, checksum_mismatch, resume_invalid, remote_changed,
disk_full, permission_denied, cancelled, paused, timeout, unsupported, internal`

`retryable=true`：`network_error, timeout, resume_invalid, remote_changed`（`http_status` 的 5xx/408/429 与 `disk_full` 依上下文）。
`range_unsupported`/`no_content_length` 是警告类事实，通常挂在 `warnings` 而非失败。

## 10. 常见问题

- **Q: `rdm_probe` 返回 `policy_denied`（127.0.0.1）**：默认拒绝私有地址；测试 fixture 需
  `policy.json` 显式 `allow_private_hosts:true` + `allowed_hosts:["127.0.0.1"]`。
- **Q: `needs_approval` 是不是要人工点确认？** 不是 v1 语义：server 返回 plan+token，agent 自己 apply（两步防误触）。人工审批 P1。
- **Q: 为什么多连接没加速？** 服务器忽略 Range（已自动降级单流）或 HTTP/2/瓶颈在服务端；`rdm_bench` 只用 loopback 测本机引擎，不代表公网。
- **Q: 进度通知收不到？** 请求 `params._meta.progressToken` 必须提供；且仅在 `wait=true`/`wait_task` 期间发送。
- **Q: 任务在另一个进程里 pause/cancel 报 conflict？** 控制权属创建进程；跨进程只能 `get/list`/`verify`，等待可读 store。
- **Q: 重启后任务还在吗？** 在：`tasks/*.json` 持久化；EOF/退出时会先暂停并持久化。
- **Q: 下载的文件名/路径从哪来？** 来自 TaskView 的 `file_name/path`；不要自己拼路径。
- **Q: `verify_file` 报 checksum_mismatch？** 引擎在校验失败时已删除 `.part` 与状态；重新下载并核对 `expected_sha256`。
- **Q: 支持非 http(s)/BT/流媒体吗？** 不支持：`invalid_url`/`unsupported`。

## 11. 版本与兼容

- RDM `0.1.0`；schema `rdm.agent/v1` 稳定；错误码/工具名/字段变更需先改 `docs/agent-integration.md`。
- MCP legacy：`2025-06-18`（首选）、兼容 `2025-03-26`、`2024-11-05`。
- MCP modern：`2026-07-28` 的 `server/discover`、每请求 `_meta`、`resultType`、列表 `ttlMs/cacheScope` 已实现；MRTR/Tasks 扩展为 P1。
