# RDM Agent 集成契约（v1，2026-10）

> 目标：让任意 coding agent / 自动化流程**无需阅读源码**就能可靠地驱动 RDM：
> 发现能力 → 规划 → 执行 → 观察进度 → 取消/恢复 → 校验 → 诊断 → 维护。
> 本文是 CLI、MCP server（`rdm-mcp`）、共享库（`rdm-agent`）和 SKILL 的唯一契约。
> 修改公开字段/工具名/错误码前必须先改本文并通知 Lead。

## 0. 设计原则

1. **机器优先**：所有 agent 可见输出都有稳定 schema、稳定错误码、稳定 ID；人类可读文本只是摘要。
2. **可发现**：`rdm capabilities --json` / `rdm schema --json` / MCP `tools/list`+`resources` 自描述；
   agent 不需要猜参数。
3. **可恢复**：长任务有持久化 `task_id`，进程重启后 `get_task`/`resume` 仍可用；下载本身复用
   `.part` + `.part.json` 的崩溃安全状态。
4. **安全默认**：策略文件限制 host/目录/大小/并发；大文件走 "plan → approval token → apply"；
   输出与日志一律脱敏（Cookie/Authorization 永不回显、不写日志）。
5. **token 高效**：列表默认分页 + 摘要；详情按需展开事件；大文件不把二进制塞进上下文。
6. **不假装**：服务器忽略 Range、无 Content-Length、h2 单流等都会在 `warnings`/`range_supported`
   中如实标注；多线程不总是更快。
7. **幂等**：`idempotency_key` 重复调用返回同一个 task，不重复下载。
8. **可测试**：一切依赖可替换（时间/传输/策略/fixture），`rdm selftest` 是 agent 的 readiness probe。

## 1. 三层交付物

| 层 | 产物 | 用途 |
|---|---|---|
| **MCP** | `crates/rdm-mcp`（`rdm-mcp.exe`，stdio JSON-RPC） | 支持 MCP 的 agent 直接调用工具/资源/提示词 |
| **CLI** | `rdm`（`--output json/ndjson` + agent 子命令） | shell/子进程型 agent、CI、无 MCP 环境 |
| **SKILL** | `skills/rdm/SKILL.md` + `skills/rdm/reference.md` | 读取 SKILL 的 agent harness（如 DeepSeek Harness） |
| **共享库** | `crates/rdm-agent` | Task store / 错误码 / 工具 schema / policy，CLI 与 MCP 共用，避免两套契约漂移 |

## 2. JSON 输出契约

### 2.1 统一信封（CLI `--output json`）

```jsonc
{
  "schema_version": "rdm.agent/v1",
  "ok": true,
  "command": "download",            // 子命令/工具名
  "task_id": "tsk_1792345678901_a1b2", // 可空
  "data": { /* 见各命令/工具 */ },
  "error": null,
  "meta": {
    "rdm_version": "0.1.0",
    "elapsed_ms": 1234,
    "ts": "2026-10-19T03:20:00+08:00",
    "trace_id": "tr_..."            // 可空；用于日志关联
  }
}
```

失败时 `ok=false`、`data=null`、`error` 为结构化错误：

```jsonc
{
  "code": "range_unsupported",      // 稳定枚举，见 §2.3
  "message": "服务器忽略 Range，已降级为单连接",
  "retryable": false,
  "hint": "若必须多线程，请更换支持 Range 的镜像；否则 --connections 1 直接单流下载",
  "details": { "url": "https://...", "status": 200 }
}
```

### 2.2 NDJSON 事件流（CLI `--output ndjson`，`--json` 为兼容别名）

每行一个事件，事件与信封共享 `schema_version`、`task_id`、`ts`、`seq`：

```jsonc
{"schema_version":"rdm.agent/v1","type":"probe","task_id":"tsk_...","seq":1,"ts":"...","data":{...}}
{"schema_version":"rdm.agent/v1","type":"progress","task_id":"tsk_...","seq":2,"ts":"...","data":{"downloaded":1048576,"total":67108864,"speed_bps":1234567.0,"eta_secs":52.0,"connections":8}}
{"schema_version":"rdm.agent/v1","type":"retry","task_id":"tsk_...","seq":3,"ts":"...","data":{"attempt":1,"delay_ms":500,"reason":"HTTP 429"}}
{"schema_version":"rdm.agent/v1","type":"completed","task_id":"tsk_...","seq":4,"ts":"...","data":{"path":"...","size":67108864,"sha256":"...","elapsed_ms":1234}}
{"schema_version":"rdm.agent/v1","type":"failed","task_id":"tsk_...","seq":5,"ts":"...","data":{"error":{...}}}
```

### 2.3 错误码（稳定）

| code | 含义 | retryable |
|---|---|---|
| `invalid_request` | 参数/schema 不合法 | false |
| `invalid_url` | URL 非法（非 http/https） | false |
| `policy_denied` | 被 agent-policy 拒绝（host/目录/大小） | false |
| `needs_approval` | 超过审批阈值，需 plan + approval token | false |
| `not_found` | task/plan 不存在 | false |
| `conflict` | 目标文件已存在且未允许覆盖 / 任务状态冲突 | false |
| `network_error` | DNS/TLS/连接失败 | true |
| `http_status` | 服务器 4xx/5xx（details.status） | 5xx/408/429 → true |
| `range_unsupported` | 服务器忽略 Range（已降级，仅警告场景） | false |
| `no_content_length` | 无 Content-Length（单流） | false |
| `checksum_mismatch` | SHA-256 不符 | false |
| `resume_invalid` | `.part.json` 损坏或校验器不匹配（可重下） | true |
| `remote_changed` | ETag/Last-Modified/长度变化 | true |
| `disk_full` / `permission_denied` | 磁盘/权限 | true/false |
| `cancelled` / `paused` | 控制流，不是失败 | false |
| `timeout` | 调用方超时 | true |
| `unsupported` | 平台/协议不支持 | false |
| `internal` | 未分类内部错误 | false |

### 2.4 Task 模型（CLI tasks / MCP 工具共用）

```jsonc
{
  "schema_version": "rdm.agent/v1",
  "id": "tsk_1792345678901_a1b2",
  "state": "queued|probing|downloading|paused|completed|failed|cancelled",
  "source": "mcp|cli|agent",
  "url": "https://example.com/big.iso",
  "file_name": "big.iso",
  "dest_dir": "C:\\Users\\me\\Downloads",
  "path": null,                        // completed 后为最终路径
  "total_bytes": 67108864,             // 未知为 null
  "downloaded_bytes": 1048576,
  "connections": 8,
  "chunk_size": 2097152,
  "range_supported": true,
  "http_version": "HTTP/1.1",
  "speed_bps": 1234567.0,
  "eta_secs": 52.0,
  "warnings": ["range_unsupported"],   // 非致命提示
  "error": null,                       // {code,message,retryable,hint,details}
  "result": null,                      // {path,size,sha256,elapsed_ms,avg_speed_bps}
  "idempotency_key": "agent-run-42",
  "created_at": "2026-10-19T03:20:00+08:00",
  "updated_at": "2026-10-19T03:20:01+08:00",
  "finished_at": null
}
```

- 请求中的 `cookie`/`authorization`/`proxy-authorization` **永不**出现在 task/list/get 输出或日志；
  task store 内的完整请求仅本机状态目录可见。
- 列表返回 `TaskSummary`（不含 headers/cookie，事件默认不含）；`get_task` 可按 `include_events` 展开最近 N 条。

### 2.5 Task 持久化（多进程安全）

```
%LOCALAPPDATA%\RDM\agent\
├─ tasks\<id>.json        # 每个任务独立文件，原子写（tmp+rename）
├─ events\<id>.jsonl      # 脱敏事件日志（可按大小截断）
├─ plans\<plan_id>.json   # 待审批计划（带过期时间）
└─ policy.json            # 可选，见 §4
```

不共享单个 `tasks.json`，因此 `rdm` CLI 与 `rdm-mcp` 可同时运行；列举 = 扫描目录。

## 3. MCP Server（`rdm-mcp`）

- 传输：**stdio**，每行一个 JSON-RPC 2.0 消息（newline-delimited）。
- 协议版本：优先 `2025-06-18`，并兼容客户端给出的 `2024-11-05`/`2025-03-26`（echo 客户端版本）。
- 能力：`tools`（listChanged=false）、`resources`（subscribe=false）、`prompts`、`logging`。
- `serverInfo`: `{ "name": "rdm", "version": "0.1.0" }`；`instructions` 用 5-10 行说明典型流程与安全边界。
- 实现要求：并发处理 `tools/call`；响应按 `id` 关联；stdin EOF → 暂停所有任务并持久化后退出。

### 3.1 Tools（16 个）

| tool | 读/写 | 说明 |
|---|---|---|
| `rdm_capabilities` | readOnly | 版本、命令、限制、MCP 能力、策略摘要 |
| `rdm_probe` | readOnly | 探测 URL 元数据（总长/Range/文件名/ETag/协议） |
| `rdm_download` | destructive | 启动下载任务；`wait` 可选；大文件可能返回 `needs_approval` |
| `rdm_plan_download` | readOnly | probe + policy 预检，返回 plan 与 `approval_token`（若需审批） |
| `rdm_apply_plan` | destructive | 用 token 执行已审批计划 |
| `rdm_list_tasks` | readOnly | 分页列出任务摘要 |
| `rdm_get_task` | readOnly | 任务详情（可选事件） |
| `rdm_wait_task` | readOnly | 等待任务结束；发 `notifications/progress` |
| `rdm_pause_task` | destructive | 暂停（引擎确认后持久化） |
| `rdm_resume_task` | destructive | 继续（幂等） |
| `rdm_cancel_task` | destructive | 取消并保留 `.part` 供后续恢复（可选 `discard_partial`） |
| `rdm_verify_file` | readOnly | 计算/比对 SHA-256（大文件可限制 max_bytes） |
| `rdm_doctor` | readOnly | 环境/网络/策略/状态/GUI-扩展健康检查 |
| `rdm_selftest` | readOnly | 本地 fixture 快速自检（引擎正确性 readiness） |
| `rdm_bench` | readOnly | 1 vs N 连接基准（本地 fixture） |
| `rdm_state_maintenance` | destructive | 扫描/修复孤儿 `.part`、损坏状态、过期任务（默认 dry-run） |

工具定义要求：
- 每个 tool 同时给 `inputSchema`、`outputSchema`（JSON Schema 2020-12 子集）；
- `annotations`: `readOnlyHint` / `destructiveHint` / `idempotentHint` / `openWorldHint`；
- `description` 说明**何时用/何时不要用/边界**（agent 最依赖描述做选择）；
- 成功与失败都返回 `structuredContent`；失败同时 `isError: true` 且 `content[0].text` 为人类摘要；
- 所有金额/大小用字节，时间用 ISO-8601 带时区；ID 稳定。

`rdm_download` 输入参数（与 `DownloadRequest` 对齐 + agent 字段）：

```jsonc
{
  "url": "https://...",                 // required
  "dest_dir": "C:\\...",                // 可选，受 policy 限制
  "file_name": "x.iso",                 // 可选
  "connections": 8,                     // 1..32
  "chunk_size": 2097152,                // 可选
  "expected_sha256": "hex64",           // 可选
  "speed_limit_bps": 0,                 // 可选
  "headers": {"Accept-Language": "zh-CN"},
  "cookie": "...",                      // 仅内存/本机 store，不回显
  "referer": "https://...",
  "user_agent": "...",
  "proxy": "http://127.0.0.1:7890",
  "resume": true,
  "idempotency_key": "agent-run-42",
  "wait": false,
  "wait_timeout_ms": 60000
}
```

`rdm_wait_task` 参数：`{task_id, timeout_ms?=60000, include_events?=false}`。

### 3.2 Resources

| URI | 内容 |
|---|---|
| `rdm://capabilities` | 与 `rdm_capabilities` 相同 |
| `rdm://policy` | 生效策略（脱敏后的绝对路径/DNS 名） |
| `rdm://schema` | 所有 tool 的 input/output schema（机器可读） |
| `rdm://tasks` | 最近任务摘要（分页由 `rdm_list_tasks` 提供） |
| `rdm://tasks/{task_id}` | 单任务详情（template） |

### 3.3 Prompts

| prompt | 参数 | 用途 |
|---|---|---|
| `download` | `url`, `dest_dir?` | 引导 agent 完成一次安全下载（probe → policy → download → wait → verify） |
| `batch_download` | `urls`（逗号分隔） | 批量下载规划与并发控制 |
| `diagnose_failure` | `task_id` | 根据 task/event/doctor 结果定位失败原因 |

### 3.4 进度、取消、日志

- `tools/call` 的 `_meta.progressToken` 被记录；`rdm_download(wait=true)`/`rdm_wait_task` 期间每
  250-500ms 发 `notifications/progress`（`progress` 为已下载字节，`total` 为总字节，`message` 含 speed/ETA）。
- `notifications/cancelled` 只取消**等待**，不隐式取消已创建任务；要停止下载必须显式 `rdm_cancel_task`。
- 关键状态变化发 `notifications/message`（level info/warning/error），消息脱敏。
- 所有工具调用产生审计事件（task store events + MCP 日志）。

### 3.5 协议代际（legacy + modern）

- **legacy（默认支持）**：MCP `2025-06-18`（兼容 `2025-03-26`/`2024-11-05`）——
  `initialize` 握手 + `notifications/initialized` + `ping` + `logging/setLevel`；
  `rdm-mcp` 以此为主路径，通过 QA harness。
- **modern（已实现基础兼容层）**：MCP `2026-07-28` 的 `server/discover`
  （`supportedVersions:["2026-07-28"]`、`resultType:"complete"`、`capabilities`、
  `instructions`、`ttlMs`/`cacheScope`）、每请求 `_meta` 校验（缺失 → `-32602`，版本不支持 →
  `-32022` + `data.supported`）、modern 结果 `result.resultType`、列表缓存字段；
  modern 下 `ping`/`logging.setLevel` 返回 `-32601`。legacy 行为保持不变。
- **P1 未做**：MRTR `input_required`（人工审批）、官方 Tasks 扩展、modern 每请求
  logLevel opt-in、`subscriptions/listen`。在完成前对外只承诺 "legacy 全兼容 + modern
  discover/resultType 基础兼容"。
- 两者共享同一套 tools/resources/prompts 与 `rdm.agent/v1` 输出契约。

## 4. 策略与审批（安全边界）

`%LOCALAPPDATA%\RDM\agent\policy.json`（可选；缺失时用安全默认）：

```jsonc
{
  "schema_version": "rdm.agent/v1",
  "allowed_hosts": ["*.example.com", "127.0.0.1", "localhost"],
  "allowed_dirs": ["C:\\Users\\me\\Downloads\\agent"],
  "max_bytes_per_task": 10737418240,
  "require_approval_over_bytes": 1073741824,
  "max_concurrent_tasks": 3,
  "default_connections": 8,
  "allow_private_hosts": false,
  "allow_overwrite": false
}
```

- 默认（无 policy 文件）：`allowed_hosts=["*"]`、`allowed_dirs=[用户 Downloads]`、`max_bytes_per_task=10GiB`、
  `require_approval_over_bytes=1GiB`、`max_concurrent_tasks=3`、`default_connections=8`、
  `allow_overwrite=false`、**`allow_private_hosts=false`**。
- **SSRF/内网防护（默认开启）**：host 解析到私有/回环/链路本地/云 metadata（含
  `169.254.169.254`）时拒绝，除非 `allow_private_hosts=true` 且 host 在 `allowed_hosts`；
  probe 后对**重定向后的 final_url** 再做一次 host 校验；未知 Content-Length 时按
  `max_bytes_per_task` 流式监控并取消（超过即 `policy_denied`）。
- **凭据**：Cookie/Authorization 在内存与本机 task store 中可用（供续传），但任何 agent 可见
  输出、事件 journal、日志都脱敏；secret handle（外部注入凭据、agent 不接触明文）为 P1。
- `rdm_download` 在 probe 后判断：超审批阈值 → 返回 `needs_approval`（`details.plan` 与
  `details.approval_token`），调用 `rdm_apply_plan` 才执行；token 10 分钟过期、单次使用。
  注意：v1 的 token 由 server 返回给调用方，因此它是**两步确认/防误触**，**不是人工审批**；
  `approval_mode=human`（外部 CLI/GUI 签发、agent 不可自批）为 P1。
- host/目录越界 → `policy_denied`（不可用 token 绕过，只能改 policy 文件）。
- 目标文件已存在且 `allow_overwrite=false` → `conflict`；agent 需显式改名或请求人工处理。

## 5. CLI agent 子命令

```
rdm capabilities [--json]                  # 机器可读能力清单（等价 MCP）
rdm schema [--json]                        # 所有输入/输出 JSON Schema 包
rdm doctor [--url URL] [--json]            # 环境/网络/策略/状态诊断
rdm selftest [--json]                      # 本地 fixture 端到端自检
rdm bench [--size 64M] [--connections 1,8] [--json]
rdm tasks [--state S] [--limit N] [--json]
rdm task <id> [--events N] [--json]
rdm state inspect [--json]
rdm state repair [--dry-run] [--json]
rdm run --input <file|-> [--wait] [--json] # 批量 JSON 清单（agent 友好）
rdm download <url> --output json|ndjson|text
rdm probe <url> --output json
```

`rdm run` 输入（JSON）：

```jsonc
{
  "schema_version": "rdm.agent/v1",
  "tasks": [
    {"url":"https://...","dest_dir":"...","file_name":"a.iso"},
    {"url":"https://...","dest_dir":"...","expected_sha256":"..."}
  ],
  "max_concurrency": 3,
  "wait": true,
  "stop_on_error": false
}
```

输出 `data`: `{ "tasks": [Task...], "failed": 0, "completed": 2 }`；`--wait` 时等待全部结束并附 result。

### 5.1 退出码（CLI，稳定）

| 退出码 | 含义 |
|---|---|
| `0` | 成功（含 `--wait` 下全部任务完成） |
| `2` | 一般失败（`error.code` 见 §2.3；JSON 信封仍会输出） |
| `3` | 控制流：被暂停 / 被取消（`paused`/`cancelled`） |
| `4` | 未执行：策略拒绝 / 需要审批 / 目标冲突（`policy_denied`/`needs_approval`/`conflict`） |

`--output json` 时无论成功失败都输出且只输出**一个**信封到 stdout（进度/诊断写 stderr）；
`--output ndjson` 输出事件流，控制流/失败事件后进程退出码同表。

## 6. SKILL

`skills/rdm/SKILL.md`（YAML frontmatter：`name: rdm`、`description`、触发条件；正文：决策树、
MCP 工具速查、CLI 配方、错误处理、策略/审批、验收清单）。`skills/rdm/reference.md` 提供完整
参数/错误码/示例。目标：agent 读取后能在**不读源码**的情况下完成 90% 下载任务。

## 7. 其他面向 agent 的能力（本次落地 / 路线图）

| 能力 | 说明 | 状态 |
|---|---|---|
| 策略/审批 token | 限 host/目录/大小/并发 + plan→apply（token 为两步确认，非人工审批） | **P0** |
| SSRF 防护 | 默认拒绝私有/回环/链路本地/云 metadata；重定向 final_url 复检；未知长度按预算流式取消 | **P0** |
| 幂等键 | 重试不重复下载 | **P0** |
| 持久 task + 事件 journal | 重启后仍可查/恢复 | **P0** |
| 结构化错误 + retryable/hint | agent 可自动决策重试/降级 | **P0** |
| MCP 进度/取消/日志通知 | 长任务可观测、可打断等待 | **P0** |
| `doctor` / `selftest` / `bench` | agent readiness & 环境诊断 | **P0** |
| `state inspect/repair` | 孤儿 `.part`、损坏状态修复（dry-run 默认） | **P0** |
| 脱敏输出/日志 | Cookie/Authorization 永不回显 | **P0** |
| `llms.txt` / `AGENTS.md` | 让 coding agent 快速理解仓库 | **P0** |
| 任务恢复/重放（`rdm replay`） | 用 journal 复现失败任务 | P1 |
| 问题诊断包 `rdm report` | 一键生成脱敏诊断 zip（版本/日志/状态/环境） | P1 |
| GUI 展示 agent 任务 | GUI 读取同一 task store | P1 |
| HITL elicitation | MCP elicitation 弹审批 | P1 |
| MCP 2026-07-28 dual-era | `server/discover`/per-request `_meta`/`resultType` 已实现基础兼容；MRTR/Tasks 扩展/subscriptions 未做 | **P0 部分 / P1 完整** |
| secret handle（外部凭据） | agent 不接触 Cookie/Authorization 明文 | P1 |
| 多源镜像/回退 | 失败块换源 | P1 |
| 调度优先级/配额 | 多 agent 公平调度 | P1 |
| 后处理 hooks | 解压/移动/通知 | P2 |
| 评测集（agent evals） | 20+ 场景回归 agent 决策质量 | P2 |

## 8. 验收标准（本迭代）

1. `cargo build --workspace`、`cargo test --workspace`、`cargo fmt --check` 全绿。
2. `rdm-mcp` 通过 MCP 协议测试：initialize / tools/list / tools/call / resources / prompts /
   progress / cancelled / 错误码；工具 schema 可被标准 JSON Schema 校验器接受（允许子集）；
   legacy 2025-06-18 与 modern 2026-07-28 的 `server/discover`/`resultType` 基础路径均通过。
3. 通过 MCP 完成真实下载（fixture）：`rdm_selftest` → `rdm_probe` → `rdm_download` →
   `rdm_wait_task` → `rdm_verify_file`；覆盖 pause/resume/cancel、idempotency、policy 拒绝与
   审批流程、SSRF 默认拒绝、未知长度预算；输出无 Cookie/Authorization 泄漏。
4. CLI `capabilities/schema/doctor/selftest/tasks/state/bench/run` 的 `--json` 输出符合 §2；
   退出码符合 §5.1。
5. `skills/rdm/SKILL.md` 能被 agent 只读执行完成一次真实下载（QA 按"不读源码"方式评测）。
6. 独立 QA 报告：`docs/qa-report-agent.md`，含协议测试、e2e、SKILL 评测、文档对照、回归
   （既有 140 + 新增 agent 测试）。
7. 本迭代已知残余风险（需在报告/README 明示）：引擎内部重定向无法逐跳回调校验、
   modern MCP 仅基础兼容、跨进程 pause/cancel 仅 owner 有效、secret handle 未实现。
