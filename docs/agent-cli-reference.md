# RDM CLI agent 参考（v1）

> 面向 agent / 自动化的 `rdm` 命令参考。契约总纲见 [agent-integration.md](agent-integration.md)；
> MCP 工具见 [agent-mcp-reference.md](agent-mcp-reference.md)；SKILL 见
> [../skills/rdm/SKILL.md](../skills/rdm/SKILL.md)。

## 全局参数

| 参数 | 说明 |
|---|---|
| `--output text\|json\|ndjson` | 输出格式。`text` 人类可读；`json` 单个信封；`ndjson` 事件流 |
| `--json` | `--output ndjson` 的兼容别名（可前置/后置） |
| `--state-dir <DIR>` | 任务 store 目录（默认 `%LOCALAPPDATA%\RDM\agent`，可用 `RDM_AGENT_STATE_DIR` 覆盖） |
| `-q/--quiet` | 抑制 text 模式进度 |

退出码：`0` 成功；`2` 一般失败；`3` paused/cancelled；`4` policy_denied / needs_approval / conflict。

## 命令

### `rdm capabilities --output json`

返回 schema 版本、工具清单、限制、当前 policy 摘要与 features。agent 的**第一步自检**。

### `rdm schema --output json`

返回 envelope/error/task/task_summary/task_event 的 JSON Schema + 16 个 MCP tool 定义
（input/output schema、annotations）。用于代码生成与参数校验。

### `rdm probe <URL> [--output json]`

探测元数据：`final_url`、`file_name`、`total_bytes`、`supports_ranges`、`etag`、
`last_modified`、`content_type`、`http_version`。只读，不创建任务。

### `rdm download <URL> [选项] [--output json|ndjson]`

选项：`-d/--dir`、`--name`、`-c/--connections`、`--chunk-size`、`--limit`（B/s）、
`--sha256`、`--header "K: V"`、`--cookie`、`--referer`、`--user-agent`、`--proxy`、
`--insecure`、`--idempotency-key`。

- `--output json`：一个信封，`task_id` + `data.task`（`TaskView`，已脱敏）。
- `--output ndjson`：事件流 `probe/metadata/started/progress/retry/completed/failed/paused/cancelled`
  + 末尾 `result`；每行含 `schema_version/type/task_id/seq/ts`。
- 任务会写入 task store（`source=cli`），可用 `rdm tasks/task` 查询。
- 管道安全：`| head` 不 panic。
- 若 state-dir 下存在 `policy.json`，CLI 也执行策略（否则 warning `policy-file-absent`）。

### `rdm run --input <file|-> [--wait] [--output json]`

批量 JSON 清单（见 [agent-integration.md](agent-integration.md) §5）；支持并发上限、
`stop_on_error`、`idempotency_key`（重复任务复用）、stdin `-`。
**当前同步等待全部任务**（契约 `wait=false` 时返回 warning `cli-run-is-synchronous`）。

### `rdm tasks [--state S] [--limit N]` / `rdm task <id> [--events N]`

读取共享 TaskStore，输出 `TaskSummary`/`TaskView`（无 cookie/request）。不存在 →
`not_found`。`--state` 可重复/逗号分隔：`queued,probing,downloading,paused,completed,failed,cancelled`。

### `rdm doctor [--url URL] [--output json]`

13 项检查（`env.os`、`disk.write`、`disk.space`、`proxy.env`、`policy`、`state.dir`、
`gui.ipc`、`firefox.native_host`、`network.dns/http/tls/range` 等），每项
`{id,status:pass|warn|fail|skip,summary,details?,hint?}` + summary。带 `--url` 时做真实
DNS/TLS/HEAD/Range 检查。

### `rdm selftest [--keep] [--output json]`

内置 fixture（随机端口）跑 4 个 case：多连接哈希一致、200 降级、resume 只补缺失块
（fixture 字节 delta 精确）、checksum 失败零残留。agent 在开始真实下载前应先跑它。

### `rdm bench [--size 64M] [--connections 1,8] [--runs 3] [--delay-ms 20] [--delay-every 256K]`

内置 fixture 1 vs N 连接：每档 `median_ms/min/max/MiB/s` + `speedup` + `sha256` +
`hash_consistent`。用于向用户解释"多线程是否真的更快"。

### `rdm state inspect|repair [--dir DIR] [--no-dry-run]`

扫描 `.part`/`.part.json`：孤儿、损坏、无主状态。`repair` 默认 dry-run，`--no-dry-run`
把问题文件隔离到 `<dir>/.rdm-quarantine/<timestamp>/`（不原地删除）。

## JSON 示例

```jsonc
// rdm capabilities --output json（截断）
{"schema_version":"rdm.agent/v1","ok":true,"command":"capabilities",
 "task_id":null,"data":{"tools":["rdm_capabilities","rdm_probe","rdm_download", "..."],
 "limits":{"max_connections":32,"default_connections":8},"policy":{...}},
 "error":null,"meta":{"rdm_version":"0.1.0","elapsed_ms":4,"ts":"2026-10-19T03:26:00.000Z"}}

// rdm download ... --output json（截断）
{"schema_version":"rdm.agent/v1","ok":true,"command":"download","task_id":"tsk_...",
 "data":{"id":"tsk_...","state":"completed","file_name":"file","path":"C:\\...",
   "total_bytes":524288,"result":{"path":"...","size":524288,"elapsed_ms":21,
   "avg_speed_bps":12345678.0},"warnings":[]},
 "error":null,"meta":{...}}

// 错误（not_found）
{"schema_version":"rdm.agent/v1","ok":false,"command":"task","task_id":null,
 "data":null,"error":{"code":"not_found","message":"task `tsk_nope` not found",
 "retryable":false,"hint":"run `rdm tasks --output json` to list tasks",
 "details":{"task_id":"tsk_nope"}},"meta":{...}}
```

## 设计偏差（v1 已知）

1. `doctor/selftest/bench/run` 失败时 `ok=false` 但保留 `data` 诊断内容（契约 §2.1 的 `data=null`
   在此刻意放宽，便于 agent 读失败明细；退出码仍按 §5.1）。
2. NDJSON 的旧 `"event"` 字段已改为契约 `"type"`（旧脚本可能不兼容）。
3. CLI 不提供 plan/apply 审批；超过阈值时 `needs_approval` 作为终态错误返回（MCP 负责 token 流程）。
4. policy 仅在显式 `policy.json` 存在时由 CLI 强制执行；默认策略由 MCP 强制。
5. `state repair` 隔离而非删除；`rdm replay`、detached run、CLI 审批为 P1。
