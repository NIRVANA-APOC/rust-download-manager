# RDM MCP Server 参考（配置 / 启动 / 资源 / 提示词）

> 目标读者：想让 coding agent 直接调用 RDM 的用户与 agent harness 维护者。
> 契约与版本化字段见 [docs/agent-integration.md](agent-integration.md)；
> agent 用法速查见 [skills/rdm/SKILL.md](../skills/rdm/SKILL.md)。
> 版本：RDM 0.1.0 · MCP legacy `2025-06-18`（兼容 `2024-11-05`/`2025-03-26`）· modern `2026-07-28`（dual-era）

## 1. 获取二进制

```powershell
$env:PATH = "C:\Users\<me>\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo build -p rdm-mcp --release
# 产物：target\release\rdm-mcp.exe
```

Windows 上 MCP 客户端通常需要**绝对路径**（避免 PATH 差异）。

## 2. 启动方式

```text
rdm-mcp [--state-dir DIR] [--log-level LEVEL]
  --state-dir DIR     agent 状态目录；默认 %LOCALAPPDATA%\RDM\agent，
                      也可用环境变量 RDM_AGENT_STATE_DIR 覆盖
  --log-level LEVEL   stderr 日志过滤（默认 warn；RUST_LOG 优先）
  -V/--version        版本；-h/--help
```

- stdio：stdout **只**输出 newline-delimited JSON-RPC 2.0；日志一律 stderr。
- 环境变量：`RDM_AGENT_STATE_DIR`（状态目录）、`RUST_LOG`（日志）；策略文件是
  `<state-dir>\policy.json`（可选）。
- 可选前置：`rdm` CLI 与 MCP 可同时运行，共享同一状态目录（每任务独立文件）。

## 3. 客户端配置示例

### 3.1 Claude Code（项目 `.mcp.json` 或用户级配置）

```json
{
  "mcpServers": {
    "rdm": {
      "type": "stdio",
      "command": "C:\\Users\\me\\Documents\\RDM\\target\\release\\rdm-mcp.exe",
      "args": []
    }
  }
}
```

### 3.2 Codex CLI（`~/.codex/config.toml`）

```toml
[mcp_servers.rdm]
command = "C:\\Users\\me\\Documents\\RDM\\target\\release\\rdm-mcp.exe"
args = []
```

### 3.3 DeepSeek Harness / 其它支持 `mcpServers` 的客户端

```json
{
  "mcpServers": {
    "rdm": {
      "command": "C:\\Users\\me\\Documents\\RDM\\target\\release\\rdm-mcp.exe",
      "args": ["--state-dir", "C:\\Users\\me\\AppData\\Local\\RDM\\agent"],
      "env": {}
    }
  }
}
```

> 说明：不同 harness 的配置文件名/字段名可能不同（例如 DSH 按其版本文档的 MCP 配置节），
> 共同点是「stdio + command + args」。验证方式：启动后发送 `initialize`，应收到
> `serverInfo.name = "rdm"`。

### 3.4 手工调试（stdio 逐行）

```powershell
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}' |
  & .\target\release\rdm-mcp.exe
```

输出即一行 `{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18",...}}`。

## 4. 初始化握手（legacy）

请求：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":"2025-06-18",
  "capabilities":{},
  "clientInfo":{"name":"my-agent","version":"1.0.0"}}}
```

响应（关键字段）：

```jsonc
{"jsonrpc":"2.0","id":1,"result":{
  "protocolVersion":"2025-06-18",          // echo 客户端支持的版本，否则服务端最新
  "capabilities":{
    "tools":{"listChanged":false},
    "resources":{"subscribe":false},
    "prompts":{},
    "logging":{}
  },
  "serverInfo":{"name":"rdm","version":"0.1.0"},
  "instructions":"RDM is a durable multi-connection download manager. ..."
}}
```

随后必须发一次通知：`{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}`。
在 initialize 之前，其它请求返回 `-32002 server not initialized`。

### Dual-era（modern `2026-07-28`，增量）

modern 客户端无需 initialize，发：

```json
{"jsonrpc":"2.0","id":"d1","method":"server/discover","params":{"_meta":{
  "io.modelcontextprotocol/protocolVersion":"2026-07-28",
  "io.modelcontextprotocol/clientInfo":{"name":"my-agent","version":"1.0.0"},
  "io.modelcontextprotocol/clientCapabilities":{}}}}
```

响应含 `resultType:"complete"`、`supportedVersions:["2026-07-28"]`、`capabilities`、
`instructions`、`ttlMs`、`cacheScope`、`_meta.serverInfo`。modern 请求需在**每个**请求的
`params._meta` 中带协议版本与客户端能力；版本不支持 → `-32022`（`data.supported/requested`）。
modern 路径下 `ping`/`logging/setLevel` 返回 `-32601`。legacy 客户端行为不变。

## 5. 工具 / 资源 / 提示词清单

工具（16，详见 `skills/rdm/reference.md` 与 `resources/read rdm://schema`）：

`rdm_capabilities`, `rdm_probe`, `rdm_download`, `rdm_plan_download`, `rdm_apply_plan`,
`rdm_list_tasks`, `rdm_get_task`, `rdm_wait_task`, `rdm_pause_task`, `rdm_resume_task`,
`rdm_cancel_task`, `rdm_verify_file`, `rdm_doctor`, `rdm_selftest`, `rdm_bench`,
`rdm_state_maintenance`。

资源：

| URI | 内容 |
|---|---|
| `rdm://capabilities` | 能力/限制/策略摘要 |
| `rdm://policy` | 生效策略（绝对路径） + 来源（file/defaults） |
| `rdm://schema` | 全部工具 input/output JSON Schema |
| `rdm://tasks` | 最近 20 条任务摘要 |
| `rdm://tasks/{task_id}` | 单任务 TaskView + 最近 20 条事件（不存在 → `-32002`） |

提示词：

| prompt | 参数 | 用途 |
|---|---|---|
| `download` | `url`, `dest_dir?` | probe→policy/approval→download→wait→verify 全流程 |
| `batch_download` | `urls`（逗号分隔） | 批量规划、并发上限、汇总、部分失败处理 |
| `diagnose_failure` | `task_id` | 依据 error/events/doctor 决策重试/恢复/放弃 |

## 6. 典型 MCP 调用序列

```jsonc
// 1) 能力
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"rdm_capabilities","arguments":{}}}
// 2) 探测
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"rdm_probe",
  "arguments":{"url":"https://example.com/a.iso"}}}
// 3) 下载并等待（带进度 token）
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"rdm_download",
  "arguments":{"url":"https://example.com/a.iso","wait":true,"wait_timeout_ms":60000,
               "expected_sha256":"<64 hex>","idempotency_key":"job-42"},
  "_meta":{"progressToken":"p1"}}}
// 服务端 → 客户端（等待期间 250-500ms 一条）：
// {"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"p1",
//   "progress":1048576,"total":67108864,"message":"downloading 1.0 MiB / 64.0 MiB ..."}}
// 4) 校验
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"rdm_verify_file",
  "arguments":{"task_id":"tsk_...","expected_sha256":"<64 hex>"}}}
// 5) 列表
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"rdm_list_tasks","arguments":{"limit":20}}}
```

大文件两步确认：

```jsonc
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"rdm_plan_download",
  "arguments":{"url":"https://example.com/big.iso"}}}
// → data.plan.plan_id + data.approval_token
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"rdm_apply_plan",
  "arguments":{"plan_id":"plan_...","approval_token":"<token>","wait":true}}}
```

取消等待（不取消任务）：

```jsonc
{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4,"reason":"user moved on"}}
```

## 7. 安全边界

- 目标目录必须在 `policy.allowed_dirs` 内（默认用户 Downloads）；host 必须在 `allowed_hosts`。
- **默认拒绝** loopback/私网/链路本地/云 metadata（`allow_private_hosts=false`）；重定向后的
  `final_url` 会再次校验。需要本机 fixture/内网镜像时显式改 policy 并重启 server。
- 超过 `require_approval_over_bytes`（默认 1 GiB）→ `needs_approval` + plan + approval_token
  （单次、10 分钟；这是**两步确认防误触**，不是人工审批；人工审批 P1）。
- 未知长度下载按 `max_bytes_per_task` 运行时监控，超限取消并置 `failed/policy_denied`。
- `cookie`/`authorization`/`proxy-authorization` 永不回显、不进事件/日志；URL/proxy 的
  `user:pass@` 会脱敏。任务 store 内完整请求仅本机状态目录可见。
- `allow_overwrite=false`（默认）时目标存在返回 `conflict`，不会静默覆盖。

## 8. 故障排查

| 现象 | 处理 |
|---|---|
| 客户端启动即退出 | 用 `rdm-mcp --version` / 手工 `initialize` 验证；检查 exe 路径是否为绝对路径 |
| `-32002 server not initialized` | 先 `initialize` 并发送 `notifications/initialized` |
| 看不到 `notifications/progress` | 请求需带 `params._meta.progressToken`；仅 `wait=true`/`wait_task` 期间发送 |
| `policy_denied`（127.0.0.1） | 默认拒绝私有地址；测试用临时 `policy.json` 显式 `allow_private_hosts:true` |
| `needs_approval` 反复出现 | 用返回的 plan+token `rdm_apply_plan`；plan 10 分钟过期需重新 plan |
| 客户端收到 `-32022` | 客户端发了 legacy 版本进 modern `_meta`；modern 仅支持 `2026-07-28` |
| stdio 输出混入非 JSON | 确认没有把 `RUST_LOG` 日志重定向到 stdout；server 日志只走 stderr |
| 任务在另一个进程控制报 `conflict` | 控制权属创建进程；跨进程用 `get/list/verify`，等待可读 task store |

自检：`tools/call rdm_doctor`、`rdm_selftest`；状态异常：`rdm_state_maintenance`（默认 dry-run）。

## 9. README agent 段落草稿（供合并）

> **让 agent 直接下载**：RDM 提供 MCP stdio server（`rdm-mcp.exe`）与 JSON-first CLI。
> 16 个工具覆盖 probe → plan/approval → download → wait/progress → pause/resume/cancel →
> verify → doctor/selftest/bench → state 维护；任务持久化、幂等键、稳定错误码、输出脱敏。
> 支持 MCP 的 harness 按 [docs/agent-mcp-reference.md](agent-mcp-reference.md) 配置；
> 其它环境使用 `rdm download <url> --output json` 或 `rdm run --input tasks.json`。
> 更完整的 agent 用法见 [skills/rdm/SKILL.md](../skills/rdm/SKILL.md)，
> 契约见 [docs/agent-integration.md](agent-integration.md)。
