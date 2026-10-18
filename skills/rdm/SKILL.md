---
name: rdm
description: 使用 RDM（多连接下载管理器）可靠地下载文件、批量下载、断点续传、SHA-256 校验、诊断失败与维护状态。优先通过 MCP 工具（rdm_capabilities→rdm_probe→rdm_download→rdm_wait_task→rdm_verify_file）操作；没有 MCP 时用 `rdm` CLI 的 `--output json`。适用于 agent 驱动的任意 http(s) 文件下载、大文件两步确认、续传与批量任务；不适用于网页抓取、流媒体/DRM、浏览器内交互或非 http(s) 协议。
---

# RDM 下载 Skill（agent 速查）

> 目标：不读源码，按本文件即可完成 90% 下载任务。
> 唯一契约：`docs/agent-integration.md`；完整参数/错误码：`skills/rdm/reference.md`。
> 机器接口二选一：**MCP 工具**（`rdm-mcp.exe`，推荐）或 **CLI**（`rdm ... --output json`）。

## 0. 30 秒决策树

1. 不知道环境有什么 → `rdm_capabilities`（CLI：`rdm capabilities --output json`）；要确认引擎健康 → `rdm_selftest`。
2. 只有一个 URL 要下载：
   - 小文件（≤ 策略审批阈值）→ `rdm_download {url, wait:true, expected_sha256?}` → 拿 `task_id`、`path`、`sha256`。
   - 大文件/需要先确认 → `rdm_plan_download {url}` → 若 `approval_required`，拿 `plan_id`+`approval_token` → `rdm_apply_plan`。
3. 不要轮询 `rdm_get_task`。等待用 `rdm_wait_task`（可带 `_meta.progressToken` 收进度）。
4. 下载完成后 → `rdm_verify_file {task_id, expected_sha256}`（MCP）；CLI 路径没有 verify 子命令，用系统 `Get-FileHash`/`sha256sum` 校验；失败 → 读 `error.code/retryable/hint` 再决定重试/恢复/放弃。
5. 进程重启/任务丢失 → `rdm_list_tasks` 找 `task_id`；暂停/取消的任务 → `rdm_resume_task`。
6. 环境异常/网络异常 → `rdm_doctor`；状态目录疑似损坏 → `rdm_state_maintenance`（默认 dry-run）。

## 1. 入口选择

| 场景 | 用 MCP | 用 CLI |
|---|---|---|
| agent 运行时支持 MCP | ✅ 首选，结构化结果+进度通知 | — |
| shell/CI/无 MCP | — | ✅ `rdm … --output json` |
| 只需一条命令、不要持久任务 | — | `rdm download URL --dir D --output json` |
| 长任务/批量/恢复 | ✅ `task_id` 持久化跨进程 | ✅ `rdm run --input tasks.json --wait --output json` |

MCP 配置见 `docs/agent-mcp-reference.md`（stdio 启动：`rdm-mcp.exe --state-dir <可选>`）。

## 2. MCP 工具速查（16 个）

| 工具 | 何时用 | 关键参数 | 读/写 |
|---|---|---|---|
| `rdm_capabilities` | 会话开始、确认限制/策略 | 无 | 只读 |
| `rdm_probe` | 下载前看总大小/Range/文件名/最终 URL | `url` | 只读 |
| `rdm_download` | 真正下载（可等待） | `url`, `wait`, `file_name`, `dest_dir`, `connections`, `expected_sha256`, `idempotency_key`, `cookie/headers` | 写 |
| `rdm_plan_download` | 大文件先规划/两步确认 | 同 download 的子集（无 `wait`） | 只读（生成 plan） |
| `rdm_apply_plan` | 执行已批准 plan | `plan_id`, `approval_token`, `wait` | 写 |
| `rdm_list_tasks` | 找/列任务 | `states?`, `limit?`, `cursor?` | 只读 |
| `rdm_get_task` | 查单任务+事件 | `task_id`, `include_events?`, `event_limit?` | 只读 |
| `rdm_wait_task` | 等任务结束/暂停（不要轮询） | `task_id`, `timeout_ms?`, `include_events?` | 只读 |
| `rdm_pause_task` | 让出带宽/暂停 | `task_id` | 写 |
| `rdm_resume_task` | 继续暂停/取消/可重试失败的任务 | `task_id` | 写（幂等） |
| `rdm_cancel_task` | 取消（默认保留 .part） | `task_id`, `discard_partial?` | 写 |
| `rdm_verify_file` | 校验完成文件 | `task_id` 或 `path`, `expected_sha256?`, `max_bytes?` | 只读 |
| `rdm_doctor` | 环境/策略/网络/GUI 诊断 | `url?` | 只读 |
| `rdm_selftest` | readiness 自检（本地 fixture，无需公网） | `size_bytes?` | 只读 |
| `rdm_bench` | 1 vs N 连接基准（本地 fixture） | `size_bytes?`, `connections?`, `repeats?` | 只读 |
| `rdm_state_maintenance` | 孤儿 `.part`/损坏状态/过期 plan | `dry_run?`（默认 true）, `stale_task_hours?` | 写（默认 dry-run） |

返回统一信封（structuredContent）：`{schema_version, ok, task_id, data, error}`；失败 `isError:true`，`error={code,message,retryable,hint?,details?}`。
**工具失败不是 MCP 协议错误**：不要重试 JSON-RPC，读 `error.code`。

### 标准单文件下载（推荐）

```jsonc
// 1) 探测（可选但推荐）
rdm_probe {"url":"https://example.com/a.iso"}
// 2) 下载并等待（wait=true 时用 _meta.progressToken 收进度）
rdm_download {"url":"https://example.com/a.iso","wait":true,"wait_timeout_ms":60000,
              "expected_sha256":"<64 hex>","idempotency_key":"job-42"}
// 3) 校验（用 2) 返回的 task_id 和 sha256）
rdm_verify_file {"task_id":"tsk_...","expected_sha256":"<64 hex>"}
```

### 大文件两步确认（不是人工审批）

```jsonc
rdm_plan_download {"url":"https://example.com/big.iso"}
// -> data: {plan:{plan_id,...}, approval_token:"<32 hex>", approval_required:true}
rdm_apply_plan {"plan_id":"plan_...","approval_token":"<token>","wait":true}
// token 单次使用、10 分钟过期；不能绕过 policy_denied
```

### 批量

并发受策略 `max_concurrent_tasks`（默认 3）限制，服务端自动排队：

1. 逐个 `rdm_probe`，跳过非法/策略拒绝 URL 并记录原因。
2. 逐个 `rdm_download {wait:false, idempotency_key:"batch-<hash>"}`。
3. `rdm_wait_task` 最多同时等 3 个；超时≠失败，继续等或 `rdm_get_task`。
4. `rdm_list_tasks` 汇总 + `rdm_verify_file` 校验。

## 3. CLI 配方（无 MCP 时）

```powershell
# 能力/健康/自检
rdm capabilities --output json
rdm schema --output json                    # 所有工具 input/output schema
rdm doctor --url "https://example.com/a.iso" --output json
rdm selftest --output json
rdm bench --size 16M --connections 1,8 --output json

# 单次下载（JSON 信封）/ NDJSON 事件流
rdm download "https://example.com/a.iso" --dir C:\Downloads --sha256 <hex> --output json
rdm download "https://example.com/a.iso" --json            # NDJSON 事件

# 探测
rdm probe "https://example.com/a.iso" --output json

# 持久任务
rdm tasks --state downloading --limit 20 --output json
rdm task tsk_... --events 50 --output json

# 批量（agent 友好）
rdm run --input tasks.json --wait --output json
# tasks.json: {"schema_version":"rdm.agent/v1","tasks":[{"url":"...","dest_dir":"...","file_name":"a.iso"}],
#              "max_concurrency":3,"wait":true,"stop_on_error":false}

# 状态维护（默认 dry-run）
rdm state inspect --output json
rdm state repair --dry-run --output json
```

> CLI JSON 信封字段见 `docs/agent-integration.md` §2（`schema_version/ok/command/task_id/data/error/meta`）。
> 退出码：0 成功，2 失败（含 invalid_url/invalid_request），3 被中断/暂停，4 policy_denied/needs_approval/conflict（未执行）。

## 4. 策略、安全与审批（默认值）

策略文件：`%LOCALAPPDATA%\RDM\agent\policy.json`（可选；缺失=安全默认）：

| 字段 | 默认 | 说明 |
|---|---|---|
| `allowed_hosts` | `["*"]` | host 白名单，支持 `*.example.com`（也匹配 apex） |
| `allowed_dirs` | `[用户 Downloads]` | 目标目录必须在其内（可尚不存在子目录；`..` 不能逃逸） |
| `max_bytes_per_task` | `10 GiB` | 超过→`policy_denied`（未知长度也会被运行时监控并取消） |
| `require_approval_over_bytes` | `1 GiB` | 超过→`needs_approval`（两步确认 token） |
| `max_concurrent_tasks` | `3` | 排队上限 |
| `default_connections` | `8` | 1..32 |
| `allow_private_hosts` | **`false`** | 默认拒绝 loopback/私网/链路本地/云 metadata（除非显式开启且 host 在 allowed_hosts） |
| `allow_overwrite` | `false` | 目标存在→`conflict`（不会静默覆盖） |

- **重定向后再校验**：probe 得到 `final_url` 后会对重定向后的 host 再查一次策略；跨出策略 → `policy_denied`。
- **脱敏**：`cookie`/`authorization`/`proxy-authorization` 不会出现在任何工具输出、事件或日志；URL/proxy 中的 `user:pass@` 会被抹掉。
- 需要放行私有地址（本机 fixture/内网镜像）：编辑 `policy.json` 为 `"allow_private_hosts": true` 且 host 在 `allowed_hosts`，重启 server。
- 审批 token 由 MCP server 返回给 agent，agent 自行 apply（防误触，不是人工审批环节）；人工审批（GUI/CLI 外部确认）为 P1。

## 5. 错误处理（稳定错误码）

| code | 含义 | 该怎么做 |
|---|---|---|
| `invalid_request` | 参数/schema 错 | 按 `hint` 修正参数；不要盲重试 |
| `invalid_url` | 非 http(s) URL | 换 URL |
| `policy_denied` | host/目录/大小/私有地址被拒 | 不要重试；向用户报告并调整 policy 或目标 |
| `needs_approval` | 超审批阈值 | 用 `details.plan`+`details.approval_token` 调 `rdm_apply_plan` |
| `conflict` | 目标已存在/状态冲突 | 换 `file_name`/`dest_dir`；或 `resume`；或请求人工处理 |
| `not_found` | task/plan 不存在或过期 | `rdm_list_tasks` 或重新 plan（plan 10 分钟过期） |
| `network_error` | DNS/TLS/连接 | `retryable`：退避重试或 `rdm_resume_task` |
| `http_status` | 4xx/5xx（看 `details.status`） | 429/5xx 可退避重试；4xx 换源/换 URL |
| `range_unsupported` | 服务器忽略 Range（已降级单流） | 不是失败；要并行请换镜像或 `connections:1` |
| `no_content_length` | 无 Content-Length（单流） | 正常继续；不要指望进度总长 |
| `checksum_mismatch` | SHA-256 不符 | 文件已清理，重新下载并核对 expected_sha256 |
| `resume_invalid` / `remote_changed` | 续传状态无效/远端变化 | `retryable`：`rdm_resume_task` 或重新 `rdm_download` |
| `disk_full` / `permission_denied` | 磁盘/权限 | 需要用户处理，重试无意义 |
| `cancelled` / `paused` | 控制流，不是失败 | 需要就 `rdm_resume_task` |
| `timeout` | 调用方等待超时 | 任务仍在跑：继续 `rdm_wait_task` 或 `rdm_get_task` |
| `internal` / `unsupported` | 内部/不支持 | `rdm_doctor`；报告错误信息 |

## 6. 生命周期与控制语义

- 状态：`queued → probing → downloading → (paused) → completed|failed|cancelled`。
- `pause`/`cancel` 是控制流：工具返回成功且 task 状态为 `paused`/`cancelled`，**不是 error**。
- `notifications/cancelled` 只取消**等待**，不会隐式取消下载任务；要停止必须显式 `rdm_cancel_task`。
- `cancel` 默认保留 `.part`/`.part.json`（可 `rdm_resume_task` 继续）；`discard_partial:true` 才删除。
- `paused`/`cancelled` 任务返回的 `downloaded_bytes` 是 `.part` 中**实际可续传**的精确字节数（可能略小于最后一次 progress 数值）。
- `idempotency_key` 相同 → 返回同一个 task，不重复下载；重试前优先带上。
- 退出/EOF：MCP server 收到 stdin EOF 会暂停运行中任务并持久化；重启后 `rdm_list_tasks`/`resume` 可继续。
- 多进程安全：CLI 与 MCP 共用 `%LOCALAPPDATA%\RDM\agent`（每 task 独立文件），可同时读；但**同一 task 的控制权属创建它的进程**，跨进程 pause/cancel 会返回 `conflict`。

## 7. 验收清单（agent 自检）

完成任何"用 RDM 下载"的任务前，确认：

- [ ] 调过 `rdm_capabilities`（或 CLI `capabilities`），知道 allowed_dirs/审批阈值。
- [ ] 下载前 `rdm_probe` 或 `rdm_plan_download`（至少拿到总大小/文件名）。
- [ ] 用 `wait:true` 或 `rdm_wait_task` 等待，未用轮询。
- [ ] 完成后校验 SHA-256（MCP：`rdm_verify_file`；CLI：系统 `Get-FileHash`/`sha256sum`；有 expected_sha256 时 match 必须 true）。
- [ ] 返回给用户的路径/大小/`task_id` 来自工具输出，不编造。
- [ ] 失败时给出 `error.code` + `retryable` + 下一步动作，而不是笼统"下载失败"。
- [ ] 不声称未验证的能力（真实公网大文件、非 http(s)、release/GUI 接管等）。

## 8. 常见坑

1. **不要用 `localhost`/`127.0.0.1` 做真实下载**：默认 `allow_private_hosts=false` 会 `policy_denied`（测试 fixture 需临时 policy 显式 allow）。
2. **HTTP/2 服务器**：RDM 分块下载强制 HTTP/1.1 以让 N 个连接真的是 N 条 TCP；单流可能显示 h2。
3. **服务器忽略 Range**：引擎自动降级单流并在 `range_supported:false`/`warnings` 如实标注；`rdm_bench` 的加速比不保证公网成立。
4. **进度通知**只在请求 `_meta.progressToken` 时发送；没有 token 不会收到 `notifications/progress`。
5. **plan 10 分钟过期、token 单次使用**：apply 失败 `not_found`/`needs_approval` 时重新 plan，不要复用。
6. **`wait_timeout_ms`/`timeout_ms` 到期不是任务失败**（`error.code=timeout`，任务仍在跑）。
7. **`dest_dir` 必须已/将位于 allowed_dirs 内**；不存在会由引擎创建，但 `..` 逃逸会被策略拒绝。
8. **不要尝试用 cookie 参数做登录态持久化**：cookie 仅内存/本机 store，不落日志、不回显；跨调用请重新提供。
9. **bench/selftest 只用 loopback fixture**，不要拿结果当公网性能承诺。
10. **现代 MCP（2026-07-28）**：本 server 为 dual-era——legacy `initialize` 全功能；modern `server/discover` + 每请求 `_meta` + `resultType` 已支持（部分字段见 reference.md）。旧客户端不要发 modern `_meta`。

## 9. 更多

- 完整参数/输出 schema：`rdm schema --output json` / `resources/read rdm://schema`
- 错误码/资源/提示词/策略字段：`skills/rdm/reference.md`
- MCP 配置（DSH/Claude Code/Codex）：`docs/agent-mcp-reference.md`
- 契约：`docs/agent-integration.md`
