# RDM Agent 接口独立验收报告（MCP / CLI / SKILL）

> 验收方：独立 QA（不参与 agent 接口实现、不修改产品源码）｜日期：2026-10-19
> **验证时点：commit `dfda934`**（`feat(agent): MCP server + CLI agent mode + SKILL and agent contracts`，
> 2026-10-19 03:41:57 +0800；源代码/文档/脚本冻结提交）
> 说明：本报告执行期间（03:43–03:48）工作区已陆续收到 task-10/task-11 的修复改动（见 §9），
> 因此 §4 的结果是 **dfda934 冻结内容**上的结果；修复 commit 到达后按 §9 复跑。
> 本报告只写 QA 自己的命令与输出；实现者自测（如 "MCP stdio e2e 164 assertions"）未直接引用。

---

## 0. 结论摘要

| 维度 | dfda934 结果 |
|---|---|
| 构建 `cargo build --workspace` | **PASS** |
| 全量测试 `cargo test --workspace --no-fail-fast` | **PASS：21 suites / 255 passed / 0 failed**（既有 140 不回归） |
| `cargo fmt --all --check` | **PASS（0 diff）** |
| `cargo clippy -p rdm-core -p rdm-cli -p rdm-agent -p rdm-mcp --all-targets` | **FAIL：10 dead-code warnings**（rdm-cli 9 + rdm-mcp 1，见 §4.1/§9） |
| MCP 协议 + 真实 e2e + CLI 契约（独立 harness） | **284 / 292 checks PASS，8 FAIL**（全部为下方已分派项） |
| SKILL 只读执行评测 | **PASS**（5 命令步 + 1 policy 步；16 MiB 真实下载成功） |
| 文档一致性 | 基本一致；1 处契约小偏差（modern 缺 `_meta` → -32002 vs 契约 -32602）+ 若干文档澄清项 |
| 阻塞项 | **无产品级阻塞**；4 类 FAIL 已由 Lead 分派 task-10（A/B）/task-11（C） |

**8 个 FAIL（全部已分派修复，不属于协议主体）**：

| ID | 问题 | 影响 | 归属 |
|---|---|---|---|
| A1/A2 | MCP `idempotency_key` 连续两次 `rdm_download(wait=false)`（间隔 ~23ms）返回不同 task（`manager.rs` 仅查磁盘 store，首任务未落盘前存在竞态） | 幂等契约 §0.7 被违反 | task-10（agent-dev） |
| B1/B2 | pause / stdin-EOF 后 TaskRecord 的 `downloaded_bytes=0`（暂停前实测已下 4,174,028 B）；数据与续传正确，仅可观测性字段错误 | §2.4 Task 模型字段语义 | task-10（agent-dev） |
| C1/C2 | CLI 显式 `policy.json` 下 `rdm download` 对 127.0.0.1（allow_private_hosts=false + host 黑名单）仍 exit 0 成功；目标已存在（allow_overwrite=false）也 exit 0 覆盖；应为 exit 4 + `policy_denied` / `conflict` | CLI policy 契约（docs/agent-cli-reference.md） | task-11（core-dev） |
| C3* | 根因证据：dfda934 的 `rdm-cli` 有 9 个策略函数 **dead_code 未接线**（`load_effective_policy/policy_precheck/policy_gate_size/policy_overwrite_gate/...`） | 同 C1/C2 | task-11（core-dev） |

---

## 1. 环境与方法

- 机器：Windows 11 10.0.26300，Ryzen 7 9700X（8C/16T），31.2 GB RAM。
- 工具链：rustc/cargo **1.99.1**（x86_64-pc-windows-gnu），MinGW WinLibs UCRT，Node **v24.19.0**。
- 构建目录：`CARGO_TARGET_DIR=C:\Users\nirvana\Documents\RDM\target-qa-agent`（独立 QA target，不干扰实现者 target-agent/target-cli-agent）。
- **harness 先于实现完成**：`scripts/mcp_e2e.mjs`（Node，零第三方依赖；自写 stdio newline-delimited JSON-RPC 2.0 客户端）与入口 `scripts/mcp-conformance.ps1`。全部断言依据 `docs/agent-integration.md`（当时为 330 行契约版）+ 后续冻结前增补的 §3.5/§5.1/§4 安全默认，不读实现代码编写。
- 隔离与安全：每个 MCP server/CLI 用独立 `%TEMP%\rdm-qa-agent-*` 作为 `LOCALAPPDATA`；按**安全默认反转后**的行为，fixture(127.0.0.1) 下载前在隔离 state dir 写临时 `policy.json`（`allow_private_hosts:true`、`allowed_hosts:["127.0.0.1","localhost"]`、`allowed_dirs:[...]`），另有专门测试验证**默认策略拒绝私有 host**（PASS）。
- 独立参考值：参考 SHA-256 由 harness 自己用 `node:http` 流式读取 fixture 计算，**不经过 RDM**；真实文件哈希由 `node:crypto` 计算。
- fixture：`target-qa-agent\debug\rdm-fixture-server.exe` 两个实例（fast 无延迟 / slow 50ms 延迟，182xx 随机端口），64 MiB 载荷。

---

## 2. MCP 协议一致性（legacy baseline + 真实 e2e）

命令（入口脚本）：
```powershell
pwsh -NoProfile -File scripts/mcp-conformance.ps1 -SizeMb 64
# 内部：cargo build -p rdm-mcp -p rdm-cli -p rdm-fixture-server
#       node scripts/mcp_e2e.mjs --rdm-mcp target-qa-agent\debug\rdm-mcp.exe --size-mb 64
```
总耗时：MCP+CLI harness **64.6 s（不含构建）**；逐项结果见 `target-qa-agent\mcp-e2e-results.json`、`target-qa-agent\qa-agent-logs\*-mcp-e2e.log`。

| 分组 | checks | dfda934 结果 | 关键证据 |
|---|---:|---|---|
| negotiation（initialize/ping 版本协商） | 13 | **13 PASS** | `initialize` 回显 `2025-06-18`；独立进程验证 `2024-11-05` 回显；serverInfo `{rdm,0.1.0}`；capabilities tools(listChanged=false)/resources(subscribe=false)/prompts/logging；instructions 827 字符；EOF exit 0 |
| tools/list | 12 | **12 PASS** | 16 个工具与契约 §3.1 完全一致（无缺/无多）；每个 inputSchema/outputSchema 通过 JSON-Schema 子集校验；4 个 annotations 齐全；description 均 ≥40 字符；`rdm_download.required` 含 url、`rdm_get_task.required` 含 task_id |
| resources | 16 | **16 PASS** | `rdm://capabilities/policy/schema/tasks` 全部列出并可读；`resources/templates/list` 含 `rdm://tasks/{task_id}`；`rdm://schema` 含全部 16 工具；未知 URI 返回错误且 server 存活 |
| prompts | 7 | **7 PASS** | 3 个 prompt 列出且 `prompts/get` 带参返回非空 messages |
| read-only tools | 18 | **18 PASS** | capabilities（rdm_version/limits/policy/server.mcp_capabilities）、doctor（checks+summary）、state_maintenance 默认 dry-run、selftest **4/4 cases PASS**、bench（medians/comparison/speedup） |
| probe | 6 | **6 PASS** | 64 MiB fixture：`total_bytes=67,108,864`、`supports_ranges=true`、`file_name="file"`、`http_version=HTTP/1.1`；`ftp://` → `isError:true` + `invalid_url`（**无 JSON-RPC error**，符合 §3.1 错误语义） |
| download(wait)+verify+tasks | 27 | **27 PASS** | 64 MiB enqueue→`completed`；目标文件 SHA-256=`e5a54547…f60f` = 独立 HTTP 参考值；`result.sha256/size/path` 一致；**7 条 notifications/progress**（单调递增、total=67,108,864、message 含 speed/ETA）；`rdm_verify_file`（path 与 task_id 两种入参）match=true；错误 SHA→`isError`+`checksum_mismatch`；list 含该 task；get(include_events) 返回事件；get 不存在→`not_found`+`isError`；task 带 `schema_version=rdm.agent/v1` |
| idempotency | 6 | **4 PASS / 2 FAIL** | 同 key 两次调用返回 `tsk_…_0004` vs `tsk_…_0003`（不同）→ FAIL（A1）；list 中同 key 任务无法被 TaskSummary 核对（设计如此）→ FAIL 属断言口径（A2 同源）；不同 key 正确新建 |
| control（pause/resume/cancel/discard） | 16 | **15 PASS / 1 FAIL** | 慢 fixture 下载中 pause→resume→completed 且 SHA 正确；cancel→`cancelled` 且 `.part` 保留；`discard_partial:true` 删除 `.part`；对已 cancelled 再 cancel 幂等；**FAIL：pause 后 TaskRecord/TaskView `downloaded_bytes=0`**（pause 前实测 4,174,028/67,108,864）→ B1 |
| policy（拒绝/审批/冲突） | 10 | **10 PASS** | 默认安全策略 `policy_denied`；`needs_approval` + plan/approval_token；`rdm_apply_plan` 执行完成；token 单次使用（第二次失败）；`conflict`；全部 `isError:true` + structuredContent.error（无 JSON-RPC error） |
| redaction | 12 | **12 PASS** | 带 cookie（`…secret_cookie…`）与 `Authorization: Bearer …` 的下载：扫描 **164 条 server→client 行**、server stderr、`events/*.jsonl`、state 目录非 task 文件、`rdm://tasks` 与单任务 resource —— **零泄漏**；tasks/<id>.json 允许保存（契约） |
| robustness | 16 | **15 PASS / 1 FAIL** | 非法 JSON→-32700 且存活；未知 method→-32601；缺参/未知 tool→错误；并发请求 id 正确关联；`notifications/cancelled` 只取消 wait（任务继续 downloading）；EOF→exit 0 且重启后 task 持久化/paused/可 resume 到 completed；**FAIL：重启后 `downloaded_bytes=0`**（B2，与 B1 同源） |
| modern dual-era（2026-07-28） | 20 | **20 PASS** | `server/discover` 无需 initialize：`resultType=complete`、`supportedVersions=["2026-07-28"]`、capabilities、instructions、`ttlMs=3600000`、`cacheScope=public`、`_meta["io.modelcontextprotocol/serverInfo"]`；modern tools/resources/prompts `resultType=complete` + `ttlMs=300000`；modern `ping`→**-32601**；错误版本→**-32022** 且 `data.supported/requested`；缺 `_meta`→**-32002**（见 §7 差异） |
| `--state-dir`（SKILL 声称） | 4 | **4 PASS** | `rdm-mcp --state-dir <tmp>` 可 initialize、工具可用、task store 落在指定目录 |
| CLI agent 契约 | 95 | **91 PASS / 4 FAIL** | 信封字段（schema_version/ok/command/data/error/meta；task_id 可缺省）、`--output json` 单信封、ndjson（type/seq/ts/task_id/completed）、`schema` 含 16 工具、`run --input -`（stdin+wait）、`tasks` filter、`task 不存在`→exit 2+not_found、probe/exit 2；**FAIL：policy_denied 与 conflict 均 exit 0**（C1/C2） |
| SKILL/docs 静态 | 13 | **13 PASS** | SKILL frontmatter(name:rdm/description)、提及工具/CLI/错误码；reference.md、agent-mcp-reference.md、llms.txt、AGENTS.md 引用一致 |

**协议主体结论**：legacy 基线、16 工具、resources/prompts、policy/审批、脱敏、现代 dual-era 基础路径全部通过；失败集中在幂等竞态、暂停进度字段、CLI policy 接线三处。

---

## 3. 真实 e2e（MCP 驱动，64 MiB）

- `rdm_selftest`：**4/4 PASS**（multi_connection_hash / range_ignored_fallback / resume_missing_chunks / checksum_no_residue，1.2 s）。
- `rdm_probe → rdm_download(wait) → rdm_verify_file → rdm_list_tasks/get`：64 MiB 下载 completed，文件 SHA-256 `e5a54547…f60f` 与独立参考一致；7 条进度通知；`rdm_verify_file`（task_id/path 两种）match=true。
- pause→resume→completed：数据正确（SHA 与参考一致），但 paused 记录 `downloaded_bytes=0`（B1）。
- cancel：`cancelled`、`.part` 保留、`discard_partial:true` 可删除。
- policy 审批：`needs_approval`→`plan_download`→`apply_plan`→completed；token 单次使用。
- `rdm_bench`（工具）：16 MiB、3 repeats，返回 medians/comparison/speedup。
- 独立 `rdm bench`（CLI，SKILL §3 命令 `--size 16M --connections 1,8`）：**1 连接中位 2,009 ms（7.96 MiB/s）→ 8 连接中位 274 ms（58.39 MiB/s），加速比 7.33x，`hash_consistent=true`**（与既有引擎 64 MiB release 基准 7.77x 一致量级；README 的 5.75x 是 8 MiB/另一参数，不可直接对比）。

---

## 4. 回归矩阵（dfda934）

### 4.1 构建 / 测试 / fmt / clippy

| 检查 | 结果 |
|---|---|
| `cargo build --workspace` | **PASS**（exit 0） |
| `cargo test --workspace --no-fail-fast` | **PASS：21 suites / 255 passed / 0 failed / 11.9 s** |
| `cargo fmt --all --check` | **PASS（0 diff）** |
| `cargo clippy -p rdm-core -p rdm-cli -p rdm-agent -p rdm-mcp --all-targets` | **FAIL：10 warnings**（exit 0）。`rdm-cli` 9 个 dead-code：`load_effective_policy`/`policy_precheck`/`policy_gate_size`/`policy_overwrite_gate`/`policy_size_exceeded_error`/`needs_approval_error`/`candidate_target_path`/`sanitize_file_name`/`POLICY_ABSENT_WARNING`（util.rs:415-564，**未被 download.rs 调用**→ C 的根因）；`rdm-mcp` 1 个：`manager.rs:61 field idempotency_lock is never read`（A 修复中间态） |

测试分项（与 README 声明一致）：rdm-agent **28**、rdm-cli unit **29** + `tests/agent_cli.rs` **21**、rdm-mcp unit **34** + `tests/stdio_e2e.rs` **3**、既有 core 18+12、fixture 2、gui 48、ipc 2、native 53+5 → **255**。

### 4.2 既有 140 测试不回归
core 18+12、fixture 2、gui 48、ipc 2、native 53+5 共 140 全部 PASS（见上表分项），**无回归**。

---

## 5. CLI agent 契约（独立验证）

- `--output json` 单信封：`schema_version=rdm.agent/v1`、`ok`、`command`、`data`、`error`、`meta{rdm_version,elapsed_ms,ts}`；`task_id` 空时可缺省（docs 行为，非 FAIL）；stdout 只有信封（进度/诊断在 stderr）——**PASS**。
- `--output ndjson` / `--json` 别名：每行事件，字段 `type`（不是旧 `event`）、`seq` 递增、`ts`、`task_id`、`schema_version`；`completed.data` 含 path/size/total_bytes/sha256/elapsed_ms 等——**PASS**（`--sha256` 时 sha256 正确）。文档已声明 NDJSON 字段改名。
- `rdm run --input -`：stdin 清单 + `--wait`，`data.tasks/completed/failed` 正确，文件哈希正确——**PASS**。
- `capabilities/schema/doctor/selftest/bench/tasks/task/state inspect/state repair --dry-run`：信封/字段/退出码 0/2 —— **PASS**（schema 含 16 工具；selftest 4/4；bench 见 §3）。
- **FAIL（C1/C2）**：显式 `%LOCALAPPDATA%\RDM\agent\policy.json` 下，`rdm download` 私网 URL 不被拒（exit 0），已存在目标不报 conflict（exit 0）。与 `docs/agent-cli-reference.md`「若 state-dir 下存在 policy.json，CLI 也执行策略」不符；根因是冻结提交里策略函数未被 `download.rs` 调用（§4.1 clippy dead-code 佐证）。
- 退出码 **3**（paused/cancelled）：需交互式 Ctrl-C，未能自动化；MCP 侧 pause/cancel 语义已覆盖，报告标注为**未自动化**。
- 文档声明的已知偏差已确认不冲突：`run` 当前同步等待全部任务（`--wait` 语义为主）、失败时保留 `data` 诊断（仍按 §5.1 退出码）、CLI 不提供 plan/apply 审批（needs_approval 为终态错误）。

---

## 6. SKILL 只读执行评测

方法：**只读 `skills/rdm/SKILL.md`**（含它明确指引的 policy 操作），不读源码/其它文档，按 SKILL 步骤对本地 fixture 做一次真实下载。

执行路径（1 个前置 + 5 个命令步，全部一次成功）：

1. §4/§8.1：在隔离 `%LOCALAPPDATA%\RDM\agent\policy.json` 写 `allow_private_hosts=true` + `allowed_hosts:["127.0.0.1","localhost"]` + `allowed_dirs`（SKILL 明确要求 fixture 需显式放行私有地址）。
2. §0.1/§3 `rdm capabilities --output json` → exit 0，返回工具/限制/policy/features（0.1 s）。
3. §0.1 `rdm selftest --output json` → **4/4 PASS**（1.2 s）。
4. §0.2/§3 `rdm probe <fixture> --output json` → `total_bytes=16,777,216`、`supports_ranges=true`、`http_version=HTTP/1.1`。
5. §3 `rdm download <fixture> --dir D --name skill-eval.bin --connections 8 --output json` → exit 0、`state=completed`、16,777,216 B、SHA-256 `E4C7AD0F…CA5E`。
6. §3 `rdm tasks --limit 5 --output json` → 持久化 task 可见（`state=completed`, `count=1`, `state_dir` 正确）。

**结论**：SKILL 可用性 **PASS**（agent 不读源码可按步骤完成真实下载；决策树→能力→自检→探测→下载→任务确认闭环）。
**歧义/改进建议（3 条）**：
1. §3「CLI 配方」退出码只写 `0 成功、2 失败、3 被中断/暂停`，**漏了 4（policy_denied/needs_approval/conflict）**（契约 §5.1/README 有）；建议补齐。
2. §0.4/§7 验收清单要求完成后 `rdm_verify_file`，但 CLI 没有 verify 子命令（仅 MCP）；CLI-only agent 需自行计算 SHA-256。建议：a) 给 CLI 加 `rdm verify`，或 b) 在 SKILL 明确「CLI 路径用系统 sha256 校验」。
3. §2 表格写 `rdm_pause_task/rdm_resume_task` 为"写"，MCP annotations 实际 `destructiveHint=false, idempotentHint=true`（MCP 语义更准确）；建议与契约一起统一措辞（Lead 已计划更新契约表）。

---

## 7. 文档一致性对照（以 dfda934 的三份 agent 文档 + README §08 为准）

| 文档声称 | 实测 | 结论 |
|---|---|---|
| README §08：16 工具 / 4 resources / 3 prompts / progress / 取消 / 结构化错误 | 全部实测通过 | ✅ |
| README §10：`cargo test --workspace` 255 passed / 0 failed | 独立复跑 21 suites / 255 passed / 0 failed | ✅ |
| README：legacy `2025-06-18` 完整 + modern `2026-07-28` discover/resultType 基础兼容 | legacy 13/13；modern 20/20 | ✅ |
| README：`rdm selftest` 4/4、`rdm bench` 加速（README 举 8 MiB 5.75x） | selftest 4/4；独立 bench 16 MiB **7.33x**（参数不同，量级一致） | ✅（数字口径不同，不可逐字对比） |
| docs/agent-cli-reference：显式 policy.json 时 CLI 执行策略 | **未执行**（C1/C2 FAIL） | ❌ 待 task-11 修复 |
| docs/agent-cli-reference：13 项 doctor 检查 `{id,status,summary,...}` | doctor 实测返回 checks+summary（本次未逐项数 13 项） | ⚠️ 未逐项核对 |
| docs/agent-integration §3.5：modern 缺 `_meta` → **-32602** | 实测 **-32002**（invalid params 家族，语义正确但码不同）；其余 modern 字段全对 | ⚠️ 契约/实现小偏差（建议契约改 -32002 或实现改 -32602） |
| 契约 §3.1 表：pause/resume 标记 destructive | 实现 `destructiveHint=false, idempotentHint=true`（MCP 语义正确；Lead 已计划改契约） | ⚠️ 文档澄清项（非 FAIL） |
| 契约 §2.4 TaskSummary 不含 idempotency_key | 实现一致（列表无该字段） | ✅（文档将注明） |
| llms.txt / AGENTS.md 引用（SKILL/reference/契约/MCP 参考） | 文件均存在、链接一致 | ✅ |
| docs/agent-mcp-reference：modern 请求需每请求 `_meta`、错误码、stdio 配置 | 与实测一致（含 -32022 data、modern ping -32601） | ✅ |

---

## 8. 未验证项 / 残余风险

1. **CLI exit 3（paused/cancelled）** 需交互式 Ctrl-C，未自动化；MCP pause/cancel 语义已覆盖。
2. **MCP MRTR `input_required`、官方 Tasks 扩展、`subscriptions/listen`** 为 P1（文档已声明），未测试。
3. **跨进程控制权**（CLI/MCP 互相 pause/cancel → `conflict`）未构造双进程场景验证。
4. **真实公网/大文件/重定向 SSRF 复检** 未用公网验证（本地 fixture 只覆盖 loopback；重定向 final_url 复检属代码+单测范围）。
5. **secret handle（P1）**、human approval（P1）未实现，文档已声明。
6. **doctor 13 项检查**未逐项核对（只验证整体 checks/summary 形状）。
7. MCP 报文为 newline-delimited JSON；若服务端把日志写到 stdout 会被判协议错误（本次 stderr 干净，未触发）。
8. 本报告数字基于 **dfda934 + 工作区修复中间态之前的构建**（构建于 03:43:34）；修复 commit 后按 §9 复跑。

---

## 9. 修复后复跑（待补）

Lead 已将 FAIL 分派：A/B → task-10（agent-dev），C → task-11（core-dev）。修复 commit 后 QA 复跑：

```powershell
pwsh -NoProfile -File scripts/mcp-conformance.ps1 -SkipBuild -OnlyMcp   # 或
node scripts/mcp_e2e.mjs --rdm-mcp target-qa-agent\debug\rdm-mcp.exe `
  --rdm target-qa-agent\debug\rdm.exe --fixture target-qa-agent\debug\rdm-fixture-server.exe `
  --size-mb 64 --only "idempotency|control|robustness|policy|cli"
# 另跑 clippy（期望 0 warning：dead-code 在接线后消失）
cargo clippy -p rdm-core -p rdm-cli -p rdm-agent -p rdm-mcp --all-targets
```

> 本节在复跑后由 QA 填写最终数字（预期：A1/A2、B1/B2、C1/C2 全部转 PASS；clippy 0 warning；总分 292/292 或仅剩文档性说明）。

---

## 10. 复现产物

- Harness：`scripts/mcp_e2e.mjs`（Node，292 checks）、`scripts/mcp-conformance.ps1`（入口：构建→MCP/CLI→回归→汇总）。
- 结果：`target-qa-agent\mcp-e2e-results.json`、`target-qa-agent\mcp-conformance-summary.json`、`target-qa-agent\qa-agent-logs\`（每步完整日志）。
- 说明：`scripts/mcp_e2e.mjs`/`mcp-conformance.ps1` 在 freeze 后由 QA 修改（envelope 解包、modern/state-dir 覆盖、隔离 policy、修复 regression 的 cargo 调用 bug），**未改任何产品源码**；建议 Lead 在下一 commit 一并收编，或保留工作区由 Lead 合并。
- 清理：QA 启动的 fixture/MCP/CLI 进程均按 PID 停止；隔离 `%TEMP%\rdm-qa-agent-*` 已清理（`--keep-temp` 可保留）；未触碰实现者 target 目录与真实 `%LOCALAPPDATA%`。

---

*报告生成：2026-10-19 03:5x（Asia/Shanghai）｜验证 commit：`dfda934`｜QA 独立执行，未修改产品源码。*
