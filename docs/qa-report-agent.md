# RDM Agent 接口独立验收报告（MCP / CLI / SKILL）

> 验收方：独立 QA（不参与 agent 接口实现、不修改产品源码）｜日期：2026-10-19（Asia/Shanghai）
> **最终验证时点：commit `5cd4ac1`**（`fix(agent): idempotency race, paused/EOF progress, CLI policy gate and invalid_url ordering, modern MCP error code`），工作区干净。
> 首轮冻结时点：`dfda934`（本报告 §2 保留两轮结果以体现「发现 → 修复 → 复验」闭环）。
> 本报告只记录 QA 自己跑出的命令与输出；实现者自测数字未直接引用。

---

## 0. 结论摘要（最终 `5cd4ac1`）

| 维度 | 结果 |
|---|---|
| `cargo build --workspace` | **PASS** |
| `cargo test --workspace --no-fail-fast` | **PASS：21 suites / 277 passed / 0 failed**（既有 140 不回归） |
| `cargo fmt --all --check` | **PASS（0 diff）** |
| `cargo clippy -p rdm-core -p rdm-cli -p rdm-agent -p rdm-mcp --all-targets` | **PASS（0 warning）** |
| MCP 协议 + 真实 e2e + CLI 契约（独立 harness） | **PASS：292 / 292 checks** |
| conformance 入口 7 个步骤（构建/MCP/回归 4 项/汇总） | **7 / 7 PASS** |
| SKILL 只读执行评测 | **PASS**（5 命令步 + 1 policy 步；16 MiB 真实下载成功、4/4 selftest） |
| 文档一致性 | 一致（首轮发现的 1 处契约小偏差已在 `5cd4ac1` 修正；仅余 SKILL 措辞建议） |
| 阻塞项 / 可发布性 | **无阻塞；agent 接口范围可以发布**（建议合并 3 条文档措辞修订，见 §7.2） |

**首轮（dfda934）发现 8 FAIL → 修复后全部转 PASS（QA 独立复验）**：

| # | 首轮 FAIL | 根因/归属 | 修复后证据（5cd4ac1） |
|---|---|---|---|
| A1 | idempotency 同 key 两次调用返回不同 task | rdm-mcp manager 只查磁盘 store（未落盘竞态）；task-10 | `first=tsk_1792353409287_0003 second=tsk_1792353409287_0003`（**同 id**），list 中仅 1 行 |
| A2 | 同 key 任务数断言（QA 口径问题，非产品 bug） | TaskSummary 按契约无 idempotency_key；QA 改断言 | `occurrences=1` PASS |
| B1 | pause 后 TaskRecord `downloaded_bytes=0` | 暂停未用引擎计数更新 record；task-10 | pause 时 `downloaded_bytes=4,153,752 / 67,108,864`（非 0），resume→completed 且 SHA 正确 |
| B2 | stdin-EOF 重启后进度丢失 | 同上 | 重启后 `downloaded_bytes=1,572,864`（非 0），可 resume 到 completed |
| C1/C2 | CLI 显式 policy 不生效（私网/覆盖均 exit 0） | 策略函数 dead-code 未接线；task-11 | `policy_denied`→**exit 4**；`conflict`→**exit 4**，错误码/消息/hint 正确 |
| C3* | （根因）rdm-cli 9 个策略函数 dead_code | task-11 | 接线后 clippy **0 warning** |
| 新回归† | C 修复导致 `probe ftp://` 变为 exit 4 + policy_denied | task-11（QA 预验证发现） | 修复后 `invalid_url`→**exit 2** + 正确 message/hint/details |
| 文档† | modern 缺 `_meta` 实际 -32002 vs 契约 -32602 | Lead/agent-dev | 修复后 **-32602**，与契约 §3.5 一致 |

† 首轮后 QA 预验证发现/复验项。

---

## 1. 环境与方法

- 机器：Windows 11 10.0.26300，Ryzen 7 9700X（8C/16T），31.2 GB RAM。
- 工具链：rustc/cargo **1.99.1**（x86_64-pc-windows-gnu），MinGW WinLibs UCRT，Node **v24.19.0**。
- 构建目录：`CARGO_TARGET_DIR=C:\Users\nirvana\Documents\RDM\target-qa-agent`（独立 QA target，不干扰实现者）。
- **harness 先于实现完成**：`scripts/mcp_e2e.mjs`（Node 标准库、零第三方依赖；自写 stdio newline-delimited JSON-RPC 2.0 客户端 + JSON-Schema 子集校验器）与入口 `scripts/mcp-conformance.ps1`。断言全部依据 `docs/agent-integration.md` 契约编写，不照实现写测试。
- 隔离与安全默认：每个 MCP server/CLI 使用独立 `%TEMP%\rdm-qa-agent-*` 作为 `LOCALAPPDATA`；因产品默认 `allow_private_hosts=false`（SSRF 防护），fixture(127.0.0.1) 下载前显式写临时 `policy.json`（`allow_private_hosts:true`、`allowed_hosts:["127.0.0.1","localhost"]`、`allowed_dirs`），并另设专门用例验证**默认策略拒绝私网 host**。
- 独立参考值：参考 SHA-256 由 harness 用 `node:http` 流式读取 fixture 计算（不经过 RDM）；文件哈希用 `node:crypto`。
- fixture：`rdm-fixture-server.exe` 两实例（fast 无延迟 / slow 50ms，182xx 随机端口），64 MiB 载荷；每次运行结束按 PID 清理。

---

## 2. MCP 协议一致性 + 真实 e2e（最终 292/292）

命令：
```powershell
pwsh -NoProfile -File scripts/mcp-conformance.ps1 -SizeMb 64
# 内部：cargo build -p rdm-mcp -p rdm-cli -p rdm-fixture-server（增量 0.3s）
#       node scripts/mcp_e2e.mjs ... --size-mb 64   → 63.9 s
#       cargo build --workspace / test --workspace / fmt / clippy
```

| 分组 | checks | dfda934 | 5cd4ac1 | 关键证据（最终） |
|---|---:|---|---|---|
| negotiation（initialize/ping/版本协商） | 13 | 13 PASS | **13 PASS** | `2025-06-18` 回显；独立进程 `2024-11-05` 回显；serverInfo `{rdm,0.1.0}`；capabilities 4 项；instructions 827 字符；EOF exit 0 |
| tools/list | 12 | 12 PASS | **12 PASS** | 16 工具与契约 §3.1 完全一致（无缺/无多）；input/outputSchema 子集校验通过；4 annotations；description 均 ≥40 字符；required 正确 |
| resources | 16 | 16 PASS | **16 PASS** | 4 个固定 URI + `rdm://tasks/{task_id}` template；`rdm://schema` 含 16 工具；未知 URI 错误且 server 存活 |
| prompts | 7 | 7 PASS | **7 PASS** | download/batch_download/diagnose_failure + 带参 get 非空 |
| read-only tools | 18 | 18 PASS | **18 PASS** | capabilities/doctor/state_maintenance(dry-run)/selftest **4/4**/bench(1&8/medians/speedup) |
| probe | 6 | 6 PASS | **6 PASS** | `total_bytes=67,108,864`、ranges=true、HTTP/1.1；`ftp://`→`isError:true`+`invalid_url`（无 JSON-RPC error） |
| download(wait)+verify+tasks | 27 | 27 PASS | **27 PASS** | 64 MiB completed；文件 SHA-256 `e5a54547…f60f` = 独立参考；**7 条 progress 通知**（单调/total/message 含 speed）；verify(task_id & path)/list/get/事件/not_found 全对 |
| idempotency | 6 | 4/6 | **6 PASS** | 同 key 两次→**同一 task id**；list 仅 1 行；不同 key 新建 |
| control（pause/resume/cancel/discard） | 16 | 15/16 | **16 PASS** | pause 时 `downloaded_bytes=4,153,752`；resume→completed 且 SHA 正确；cancel 保留 `.part`；`discard_partial` 删除；重复 cancel 幂等 |
| policy（拒绝/审批/冲突） | 10 | 10 PASS | **10 PASS** | 默认策略 `policy_denied`；needs_approval→plan→apply；token 单次使用；conflict；全部 `isError:true` + structuredContent（无 JSON-RPC error） |
| redaction | 12 | 12 PASS | **12 PASS** | 扫 **164 条 server→client 行** + stderr + events journal + 非 task 状态文件 + resources：cookie/Bearer **零泄漏** |
| robustness | 16 | 15/16 | **16 PASS** | 非法 JSON→-32700 且存活；未知 method→-32601；缺参/未知 tool 错误；并发 id 关联；`notifications/cancelled` 只取消等待；EOF exit 0 + 重启 paused(进度 1,572,864) + resume completed |
| modern dual-era（2026-07-28） | 20 | 19/20（-32002） | **20 PASS** | discover(resultType/supportedVersions/ttlMs=3600000/cacheScope=public/serverInfo)；modern 列表 resultType=complete + ttlMs=300000；modern ping→-32601；错版本→-32022+data；缺 `_meta`→**-32602**（与契约一致） |
| `--state-dir`（SKILL 声称） | 4 | 4 PASS | **4 PASS** | flag 可用、task store 落在指定目录 |
| CLI agent 契约 | 95 | 91/95 | **95 PASS** | 信封/单信封 stdout/ndjson(type,seq,ts,task_id,completed)/schema 16/run stdin+tasks filter/state inspect+repair/退出码 0·2·4/probe·selftest 4/4·bench |
| SKILL/docs 静态 | 13 | 13 PASS | **13 PASS** | frontmatter、工具/CLI/错误码覆盖、reference/mcp-reference/llms.txt/AGENTS.md 一致 |

**MCP 协议结论**：legacy `2025-06-18` 基线完整；modern `2026-07-28` 基础兼容（discover/per-request `_meta`/resultType/ttlMs/cacheScope/-32022/-32602/modern ping -32601）全部通过；错误语义严格符合契约（工具失败 = `isError:true` + structuredContent，协议/参数错误才用 JSON-RPC error）。

---

## 3. 真实 e2e 数据（最终）

- `rdm_selftest`：**4/4 PASS**（multi_connection_hash / range_ignored_fallback / resume_missing_chunks / checksum_no_residue，~1.2 s）。
- 64 MiB MCP 全链路：probe → `rdm_download(wait=true)` → 7 条进度通知 → completed → `rdm_verify_file`（task_id 与 path 两种）match → list/get(events)；文件 SHA-256 `e5a54547…f60f` 与 harness 独立 HTTP 参考一致。
- pause→resume：暂停于 4,153,752 B（进度不再清零）→ resume → completed → SHA 一致。
- cancel / discard_partial / 幂等 / policy 审批 token 单次使用：全部通过。
- 基准（独立 CLI `rdm bench`，SKILL §3 命令 `--size 16M --connections 1,8`）：**1 连接中位 2,009 ms（7.96 MiB/s）→ 8 连接中位 274 ms（58.39 MiB/s），加速比 7.33x，`hash_consistent=true`**；conformance 内 CLI 64 MiB/20ms 基准 speedup **7.74x**（与既有引擎 release 基准 7.77x 同量级）。README 的 8 MiB/5.75x 为另一参数口径，不矛盾。

---

## 4. 回归矩阵（最终 `5cd4ac1`）

| 检查 | 结果 |
|---|---|
| `cargo build --workspace` | **PASS**（4.9 s，增量） |
| `cargo test --workspace --no-fail-fast` | **PASS：21 suites / 277 passed / 0 failed / 18.4 s** |
| `cargo fmt --all --check` | **PASS（0 diff）** |
| `cargo clippy -p rdm-core -p rdm-cli -p rdm-agent -p rdm-mcp --all-targets` | **PASS（0 warning）** |

分项（实测）：rdm-agent **28** ｜ rdm-cli **31** unit + **34** `tests/agent_cli.rs` ｜ rdm-mcp **40** unit + **4** `tests/stdio_e2e.rs` ｜ 既有：core **18+12**、fixture **2**、gui **48**、ipc **2**、native **53+5** → **277**。既有 140 项**无回归**。
> 首轮 dfda934 为 255 项且 clippy 10 warnings（rdm-cli 策略函数 dead-code + rdm-mcp idempotency_lock）；修复后函数接线、dead-code 消失，clippy 归零。

---

## 5. CLI agent 契约（最终全 PASS）

- `--output json`：单信封（`schema_version=rdm.agent/v1`/`ok`/`command`/`data`/`error`/`meta{rdm_version,elapsed_ms,ts}`），stdout 只有信封（进度/诊断走 stderr）；`task_id` 空时缺省与文档一致。
- `--output ndjson` / `--json` 别名：`type`/`seq` 递增/`ts`/`task_id`/`schema_version`；completed 字段含 path/size/sha256 等。
- `rdm run --input -`（stdin 清单 + `--wait`）、`tasks` 过滤、`task <id>`、`state inspect/repair --dry-run`、`capabilities/schema/doctor/selftest/bench`：信封与退出码全部符合契约。
- 退出码实测：成功 0；一般失败/not_found/invalid_url **2**；policy_denied/conflict **4**（错误码/消息/hint/details 正确）。**exit 3（paused/cancelled）需交互式 Ctrl-C，未自动化**（MCP 侧 pause/cancel 语义已覆盖）。
- 文档声明的已知偏差不冲突：`run` 当前同步等待、失败时保留 `data` 诊断、NDJSON 字段改名、CLI 无 plan/apply 审批（needs_approval 终态）。

---

## 6. SKILL 只读执行评测（PASS）

方法：**只读 `skills/rdm/SKILL.md`**（含它明确指引的 policy 操作），不读源码/其它文档，对本地 fixture 做一次真实下载。

| 步 | 依据 | 命令 | 结果 |
|---|---|---|---|
| 0 | §4/§8.1 | 在隔离 `LOCALAPPDATA\RDM\agent\policy.json` 显式放行 127.0.0.1/localhost + allowed_dirs | OK（SKILL 正确提示默认策略会拒私网） |
| 1 | §0.1/§3 | `rdm capabilities --output json` | exit 0，工具/限制/policy 自描述（0.1 s） |
| 2 | §0.1 | `rdm selftest --output json` | **4/4 PASS**（1.2 s） |
| 3 | §0.2/§3 | `rdm probe <fixture> --output json` | `total_bytes=16,777,216`、ranges=true、HTTP/1.1 |
| 4 | §3 | `rdm download <fixture> --dir D --connections 8 --output json` | exit 0、completed、16,777,216 B、SHA-256 `E4C7AD0F…CA5E` |
| 5 | §3 | `rdm tasks --limit 5 --output json` | 持久化 task 可见（completed） |

**结论**：不读源码可按步骤完成真实下载；决策树 → 能力 → 自检 → 探测 → 下载 → 任务确认闭环完整。
**歧义/改进建议**：
1. §3 CLI 配方的退出码只写 `0/2/3`，**漏 4**（policy_denied/needs_approval/conflict，契约 §5.1/README 有）→ 建议补齐。
2. §0.4/§7 要求完成后 `rdm_verify_file`，但 CLI 无 verify 子命令（仅 MCP）→ 建议给 CLI 加 `rdm verify`，或在 SKILL 注明 CLI 路径用系统 sha256 校验。
3. §2 表把 pause/resume 归为"写"；MCP annotations 为 `destructiveHint=false, idempotentHint=true`（契约 §3.1 已在 `5cd4ac1` 补注解表，措辞可对齐）。

---

## 7. 文档一致性对照（以 `5cd4ac1` 的契约 + 三份 agent 文档 + README §08 为准）

### 7.1 已一致（抽查通过）

| 声称 | 实测 |
|---|---|
| README §08：16 工具 / 4 resources / 3 prompts / progress / 取消等待 / 结构化错误 | 全 PASS |
| README §10：`cargo test --workspace` 277 passed | 独立复跑 21 suites/277 passed/0 failed |
| README：legacy 2025-06-18 完整 + modern 2026-07-28 基础兼容 | legacy 13/13；modern 20/20 |
| 契约 §3.5：modern 缺 `_meta` → -32602；错版本 → -32022+data | 实测一致（-32602 / -32022+supported/requested） |
| 契约 §3.1 注解表：pause/resume `destructiveHint=false`、`idempotentHint=true` | 实测一致 |
| 契约 §2.4：TaskSummary 无 idempotency_key（设计） | 实测一致 |
| docs/agent-cli-reference：显式 policy.json 时 CLI 执行策略；退出码 0/2/3/4 | 实测一致（policy_denied/conflict→4；not_found/invalid_url→2） |
| docs/agent-mcp-reference：modern 请求每请求 `_meta`、stdio 配置、错误码 | 实测一致 |
| llms.txt / AGENTS.md 引用文件（SKILL/reference/契约/MCP 参考） | 均存在且链接一致 |
| README §10：rdm-agent 28 + rdm-cli 31+34 + rdm-mcp 40+4 | 实测一致 |

### 7.2 建议 Lead 合并的文档修订（3 条，非阻断）

1. `skills/rdm/SKILL.md` §3：退出码补 `4`（policy_denied/needs_approval/conflict）。
2. `skills/rdm/SKILL.md` §0.4/§7：说明"CLI 路径无 verify 子命令，用系统 sha256 校验"（或后续加 `rdm verify`，P1）。
3. `README.md` §08 的 `rdm bench` 数字（8 MiB/5.75x）建议注明"本机 8 MiB 参数口径；不同 size/延迟不可横向对比"；QA 独立 16 MiB 为 7.33x、64 MiB/20ms 为 7.74x。
   （另：`docs/agent-cli-reference.md` 的 doctor「13 项检查」本次只验证整体形状，未逐项核对——如需严格可后续补。）

---

## 8. 未验证项 / 残余风险

1. **CLI exit 3**（paused/cancelled）需交互式 Ctrl-C，未自动化；MCP pause/cancel 已覆盖。
2. **MCP MRTR `input_required`、官方 Tasks 扩展、`subscriptions/listen`** 为 P1（文档已声明），未测试。
3. **跨进程控制权**（CLI/MCP 互操作 pause/cancel → conflict）未构造双进程场景。
4. **真实公网 / 大文件 / 重定向 SSRF 复检 / 未知长度预算**：仅在 loopback fixture + 单测/代码层验证，未用公网。
5. **secret handle（P1）/ human approval（P1）** 未实现（文档已声明）。
6. **doctor 13 项检查**未逐项核对。
7. 本机环境限定 Windows + GNU 工具链；Linux/macOS、release 打包未验证（既有结论）。

---

## 9. 「发现 → 修复 → 复验」时间线

| 时点 | 事件 | 结果 |
|---|---|---|
| 03:41 `dfda934` | 首轮冻结验收（292 checks） | 284 PASS / **8 FAIL**（A/B/C 三类） |
| 03:44–03:49 | QA 报告 8 FAIL 根因（含 clippy dead-code 佐证 C）；Lead 分派 task-10/task-11 | — |
| 03:50–03:56 | QA 预验证工作区修复，发现新回归 `probe ftp://`→exit 4 + policy_denied，并修正自己的 idempotency 断言口径 | 上报 Lead/core-dev |
| 03:57 `5cd4ac1` | 修复提交（工作区干净） | — |
| 03:57–03:59 | **QA 最终全量复跑**：conformance 7/7 steps、MCP/CLI **292/292**、回归 **277/0**、fmt 0、clippy 0 | **全 PASS** |

---

## 10. 复现产物

- Harness：`scripts/mcp_e2e.mjs`（292 checks，Node 标准库）、`scripts/mcp-conformance.ps1`（入口：构建 → MCP/CLI e2e → workspace build/test/fmt/clippy → 汇总）。
- 结果：`target-qa-agent\mcp-e2e-results.json`（292/292）、`target-qa-agent\mcp-conformance-summary.json`（7/7）、`target-qa-agent\qa-agent-logs\`（逐步日志）。
- harness 在 `5cd4ac1` 中已入库（QA 后续仅做断言口径/覆盖增强，如需再改由 Lead 决定是否再 commit）。
- 清理：QA 启动的 fixture/MCP/CLI 进程均按 PID 停止；隔离 `%TEMP%\rdm-qa-agent-*` 已清理；未触碰实现者 target 与真实 `%LOCALAPPDATA%`；未修改产品源码。

---

*报告生成：2026-10-19 04:00（Asia/Shanghai）｜最终验证 commit：`5cd4ac1`（工作区干净）｜首轮：`dfda934`｜QA 独立执行。*
