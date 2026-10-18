# RDM Agent 工具设计调研：MCP 现状、机器接口最佳实践与下载器 agent 能力

> **调研日期**：2026-10-19（Asia/Shanghai）｜**调研人**：RDM 研究分析师
> **方法**：MCP 官方规范（modelcontextprotocol.io，当前修订 **2026-07-28**）逐页核对 + 官方 llms.txt 索引 + GitHub REST API 实测（2026-10-19 03:15–03:19 +08:00）+ 本仓库代码复核（03:10–03:18 快照，含并行开发变化）。
> **本仓库现状声明（重要，且正在并行变化）**：`docs/agent-integration.md` 是**契约**（v1，03:14 版 342 行），不是实现证明。**03:18 复核**：`rdm-agent` 从 30 KB 增长到 `lib.rs` 51.6 KB + `tools.rs` 57.2 KB（工具目录已填充、`ApprovalToken`/`DownloadPlan`/`Policy`/`ErrorCode`/脱敏均有实现与单测）；`rdm-cli` 已拆出 `cli.rs/output.rs/tracker.rs/util.rs`，子命令扩展到 `doctor/selftest/bench/tasks/task/state/run`；但 `rdm-mcp/src/main.rs` 仍是 **205 字节桩**；`skills/`、`llms.txt` 不存在；`scripts/mcp_e2e.mjs` 按**旧协议**写死（`initialize('2025-06-18')` + 多处 `ping`）；且 `crates/rdm-agent/src/lib.rs:22-24` 的 `PROTOCOL_VERSIONS` / `LATEST_PROTOCOL_VERSION` 仍钉在 `2025-06-18`。**本轮未执行 cargo build/test**，状态仅为文件存在性观察；写作期间仍在变化，引用前请复核。
> **写作纪律**：不把契约当已实现；拿不到的数据标 **未获取**；所有规范结论给官方链接。

---

## 0. 一页结论（TL;DR）

1. **最反直觉发现：RDM 的 agent 契约把 MCP 版本钉在 `2025-06-18`（initialize 握手、兼容 2024-11-05/2025-03-26），而官方当前修订已是 `2026-07-28`（第 5 版，2026-07 发布）**。这不是文档笔误：`crates/rdm-agent/src/lib.rs:22-24` 的 `PROTOCOL_VERSIONS = ["2025-06-18","2025-03-26","2024-11-05"]`、`LATEST_PROTOCOL_VERSION = "2025-06-18"` 已在代码里固化。新版**移除了 `initialize` 握手、会话、`ping`、`logging/setLevel`**，改为"无状态核心 + 每请求 `_meta` 携带协议版本/客户端能力 + `server/discover` 强制 RPC"。换句话说：**按现有契约实现出来的 server 会与最新客户端"看起来能连、代际错位"**；而 `scripts/mcp_e2e.mjs` 把 `initialize('2025-06-18')` 与多处 `ping` 写死为验收项（第 653–675、1387–1408 行），在新协议下是**反规范**的。（证据见 §1.1、§1.5）
2. **第二反直觉：RDM 的自研 durable `task_id` 模型，恰好命中新版 MCP 的设计哲学。** 2026-07-28 明确"协议没有会话；需要跨调用状态就用显式 handle 传参"（SEP-2567），并官方提供 **Tasks 扩展**（`io.modelcontextprotocol/tasks`）描述长任务生命周期（working / input_required / completed / failed / cancelled，`tasks/get` 轮询、`tasks/update` 补输入、`tasks/cancel` 协作式取消）。RDM 的"持久 task + 事件 journal + 可恢复"不是包袱，而是**最值钱的 agent 资产**——但要把它映射到标准扩展，而不是只做私有工具集。
3. **生态竞争点已从"工具数量"转向"技能 / 长任务 / UI"**（GitHub API 实测）：`anthropics/skills` **181,960★**、`a2aproject/A2A` **26,195★**（A2A v1.0.0，2026-03 发布）、MCP 规范主仓 **9,474★**、官方 MCP Apps 扩展仓 `ext-apps` **2,972★**、Tasks 扩展仓 `ext-tasks` **57★**（2026-10-18 仍活跃的新扩展，采纳仍少 → **先发窗口**）、`AnswerDotAI/llms-txt` **2,662★**、`agentsmd/agents.md` **24,982★**。
4. **agent 不关心"多线程"；agent 关心"不可靠的外部动作变成可审批、可恢复、可观测的 task"。** 下载器接 agent 的第一价值是**受控执行外部 I/O**（fetch/verify/state），第二才是速度。把产品叙事写成"agent 的下载执行器"比"多线程下载器"更贴合使用场景。
5. **第一风险不是协议，是默认策略**：契约 §4 默认 `allowed_hosts=["*"]`、`allow_private_hosts=true`。一个被 agent 驱动的"任意 URL 下载器"= 现成的 **SSRF / 内网穿透 / 云元数据服务探测**工具（同类真实案例：Gopeed 曾被报 `/api/web/proxy` 开放代理 SSRF，见 `GopeedLab/gopeed#1589`）。**建议默认反转：deny private/metadata、按 host 显式 allow、DNS 解析后二次校验、每跳重定向再校验。**
6. **凭据不能进 tool 参数**：MCP 2026-07-28 规定 form elicitation **MUST NOT** 收集密码/API key/token；而契约 §3.1 的 `rdm_download` 直接把 `cookie` 字符串暴露为工具参数——它会进入 LLM 上下文、客户端日志、transcript。应改为 **secret handle / 环境变量引用**，工具输出永不含 cookie/authorization（`rdm-agent` 的 `redact_request` 已实现，是正确方向）。
7. **审批语义应"可移植核心 + 渐进增强"**：`plan → approval token → apply_plan` 在所有协议代际都成立（建议 token 绑定 plan 哈希、单次、10 分钟过期）；新版再用 **MRTR**（`input_required` + `elicitation/create` + 重试）做体验增强。**不要让模型自己批准自己**，也不要把 elicitation 当唯一审批路径（旧客户端不支持）。
8. **实现进度：共享契约层已经落地，协议入口仍是桩。** 03:18 复核：`rdm-agent`（lib 51.6 KB + tools 57.2 KB）已有 16 工具目录/JSON Schema、ApprovalToken/DownloadPlan、Policy、TaskStore、错误码、脱敏；CLI 已有 `doctor/selftest/bench/tasks/task/state/run` 子命令骨架。**仍缺**：`rdm-mcp` 全部（205 B 桩）、`capabilities/schema` 两个 CLI 子命令、`skills/`、`llms.txt`、双代际协议、审批端到端接线、agent 评测；且 `AGENTS.md` 引用了不存在的 `docs/agent-mcp-reference.md`（文档漂移）。**先把"能跑通的最小纵向切片"（MCP 桩→probe→download→wait→verify）做出来，再扩能力。**
9. **评测是差异化**：同类下载器（aria2/yt-dlp/gh 等）都只有"机器输出"，没有"agent 使用质量"评测。RDM 可用 `fixture server + mcp_e2e.mjs + selftest/bench` 组成 **10 场景 pass@1/步数/token/错误恢复率**基准——这比"多 4 个工具"更能证明 agent 友好性。
10. **一句话定位（可直接路演）**："RDM 把下载变成 agent 可安全委托的外部动作：计划可审批、执行可恢复、结果可校验、失败可解释。"

---

## 1. MCP 现状（截至 2026-10-19）

### 1.1 协议修订时间线（5 个正式修订 + 当前）

| 修订 | 关键变化（与本主题相关） | 对 RDM 的含义 |
|---|---|---|
| **2024-11-05**（初版） | stdio + HTTP+SSE 传输；`initialize`/能力协商；tools/resources/prompts；sampling/roots/logging | 最老客户端仍在用；legacy 路径必须兼容 |
| **2025-03-26** | 引入 **Streamable HTTP**（替代 SSE）、OAuth 2.1 授权、**tool annotations**（readOnly/destructive/idempotent/openWorld hints） | annotations 字段契约正确；但客户端**必须视为不可信**除非信任该 server |
| **2025-06-18** | **structuredContent + outputSchema**、**elicitation**、resource links、security best practices | RDM 契约当前对齐的就是这一版 |
| **2025-11-25** | URL-mode elicitation、icons、OIDC Discovery、JSON Schema 2020-12 成为默认方言、**experimental tasks**（`capabilities.tasks`、`tasks/list`、阻塞式 `tasks/result`） | ⚠️ 这一版的 tasks 是**实验性且已被 2026-07-28 取代**；RDM **不应实现**它 |
| **2026-07-28**（当前） | **去握手、去会话、去 ping**：每请求 `_meta` 携带 protocolVersion/clientCapabilities；`server/discover` 为 MUST；结果必须带 `resultType`；`resources/subscribe` → `subscriptions/listen`；**Tasks 移为官方扩展**；**MRTR** 取代 server→client 请求；删除 `logging/setLevel`、`ping`、`roots/list_changed`；弃用 Roots/Sampling/Logging；OAuth DCR 让位 Client ID Metadata Documents | **必须 dual-era**：legacy（≤2025-11-25）+ modern（2026-07-28）；`scripts/mcp_e2e.mjs` 需双模式 |

| 版本 | 官方入口 |
|---|---|
| 当前 2026-07-28 | https://modelcontextprotocol.io/specification/2026-07-28/ |
| 变更日志 | https://modelcontextprotocol.io/specification/2026-07-28/changelog |
| 版本与协商 | https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning |
| 2025-11-25 变更 | https://modelcontextprotocol.io/specification/2025-11-25/changelog |

### 1.2 传输与生命周期

- **stdio**：每行一个 JSON-RPC 2.0 消息（newline-delimited），stdout 只走协议，日志写 stderr。本地桌面下载器（RDM 当前定位）应**只做 stdio**；远程/多机再考虑 Streamable HTTP。
- **Streamable HTTP**：2026-07-28 已**移除会话与 `Mcp-Session-Id`**、移除 SSE 可恢复性（断流即丢请求，客户端须以新 id 重发）；新增 `Mcp-Method`/`Mcp-Name` header，工具参数可经 `x-mcp-header` 镜像为 `Mcp-Param-*` header（仅 HTTP 传输有意义）。
- **legacy 生命周期**：`initialize` → `notifications/initialized` → 正常请求；能力在 initialize 结果里协商一次。
- **modern 生命周期**：**没有 initialize**。每个请求自带 `_meta`：
  - `io.modelcontextprotocol/protocolVersion`（必填）
  - `io.modelcontextprotocol/clientInfo`、`io.modelcontextprotocol/clientCapabilities`（SHOULD/按需）
  - 服务端在**每个结果的 `_meta.io.modelcontextprotocol/serverInfo`** 标识自己
  - 版本不支持 → 返回 `-32022 UnsupportedProtocolVersionError`（`data.supported` 列出支持版本）
  - `server/discover` 是 **MUST 实现**的 RPC：返回 supportedVersions、capabilities（含 extensions）、instructions、身份；客户端可选在首个请求前调用，也可直接请求再按错误重试。

**对 RDM 的结论**：契约里"优先 2025-06-18、echo 客户端版本"的写法必须升级为"**双代际（dual-era）**"：收到首条消息若为 `initialize` → legacy 路径；若含 2026-07-28 `_meta.protocolVersion` → modern 路径；并实现 `server/discover`。MCP Inspector 已把这件事产品化（`--protocol-era legacy|auto|modern`），可直接作为验收工具（见 https://modelcontextprotocol.io/docs/2026-07-28/tools/inspector/protocol-eras）。

### 1.3 tools / resources / prompts（2026-07-28 关键约束）

- **tools**
  - `tools/list` 结果**不得随连接变化**（可按授权不同），**SHOULD 确定性排序**（利于 prompt cache）；列表/读取结果需带 **`ttlMs` + `cacheScope`**（CacheableResult，2026-07-28 新增）。
  - tool 名称：1–128 字符，建议仅 `[A-Za-z0-9_.-]`；`rdm_*` 合法。
  - 每个 tool：`name`/`title`/`description`/`inputSchema`/`outputSchema`/`annotations`/`icons`（可选）。
  - **annotations 只作提示**，客户端 MUST 视为不可信（除非信任 server）——所以审批不能只靠 `destructiveHint`。
  - **成功/失败两条通道**：协议错误（未知 tool、参数非法→`-32602`）用 JSON-RPC error；**工具执行错误**用 `result.isError=true` + 人类可读 `content`，便于模型自我纠正。
  - **`outputSchema` + `structuredContent`**：结构化结果是"约定"而非强制；**建议同时返回序列化 JSON 的 text content 以兼容旧客户端**。2026-07-28 放宽为任意 JSON Schema 2020-12 关键字/任意 JSON 值（含 `$ref` 解析要求）。
- **resources**：`resources/list`/`read`/`templates/list` 仍在；`resources/subscribe`/`unsubscribe` **已被移除**，订阅统一走 **`subscriptions/listen`**（长连接 POST 响应流，opt-in `resourcesListChanged`/`resourceSubscriptions` 等）。
- **prompts**：保留；适合放"下载/诊断"标准流程模板（RDM 契约已有 3 个 prompt，方向正确）。
- **进度 / 取消**
  - 进度：请求带 `progressToken`（RDM 契约 `_meta.progressToken`），服务端在同请求的响应流上发 `notifications/progress`；现代 HTTP 下不再有独立 GET 流。
  - 取消：stdio 仍发 `notifications/cancelled`；HTTP modern 取消信号是**关闭该请求的响应流**。**取消"等待"≠取消"任务"**——RDM 契约明确"cancel wait 不隐式取消下载"，与规范语义一致，值得保留并在 SKILL 里写清。
- **logging**：2026-07-28 **弃用 Logging 能力**；改为**每请求** `_meta["io.modelcontextprotocol/logLevel"]`，且**未 opt-in 的请求不得发日志**；stdio 用 stderr。RDM 契约把 `logging` 列为能力并向客户端发 `notifications/message` → **需改**。
- **elicitation**：两种模式——**form**（扁平对象、仅原始类型）与 **url**（敏感信息/第三方授权，不经过 client/LLM）。响应三态 `accept`/`decline`/`cancel`。**form 模式 MUST NOT 收集密码/密钥/token**；**敏感信息必须走 URL 模式**。现代代际通过 **MRTR**（`InputRequiredResult`，`resultType:"input_required"`，客户端用**新 JSON-RPC id** 重试并带 `inputResponses`/`requestState`）实现；旧代际是 server→client 请求（如 `elicitation/create`）。

### 1.4 官方扩展（2026 生态的关键）

> 2026-07-28 起，实验/新能力走**官方扩展**：`ClientCapabilities.extensions`/`ServerCapabilities.extensions` 声明；server 在 `server/discover` 里广告，client 在**每请求** `_meta` 里声明。

| 扩展 | 用途 | 对 RDM |
|---|---|---|
| **Tasks**（`io.modelcontextprotocol/tasks`） | 长任务标准：`CreateTaskResult`（`resultType:"task"`，含 `taskId`/`ttlMs`/`pollIntervalMs`）→ `tasks/get` 轮询 → `tasks/update` 补输入 → `tasks/cancel` 协作取消；状态 working/input_required/completed/failed/cancelled；通知经 `subscriptions/listen`（`notifications/tasks`） | **RDM 长下载的天然映射**；"server 自主决定何时返回 task"，client 只需声明一次能力 |
| **Skills**（SEP-2640） | 通过 MCP **发现/读取 Agent Skills**（SKILL.md） | RDM 规划的 `skills/rdm/SKILL.md` 可直接经 MCP 暴露，无需用户手工找文件 |
| **MCP Apps** | 在 host 内渲染交互 UI（进度/审批面板） | P2：GUI 已存在，可先读同一 task store；再评估 Apps |
| **Authorization 扩展** | OAuth client credentials / 企业托管授权 | 远程 MCP（P2）才需要 |
| **Registry** | server.json 发布/发现 | P2：让 agent host 能"搜到" RDM |

链接：Extensions 总览 https://modelcontextprotocol.io/extensions/overview ｜Tasks https://modelcontextprotocol.io/extensions/tasks/overview ｜ext-tasks 仓库 https://github.com/modelcontextprotocol/ext-tasks ｜MCP Apps 仓库 https://github.com/modelcontextprotocol/ext-apps ｜Registry https://modelcontextprotocol.io/registry/about ｜规范主仓 https://github.com/modelcontextprotocol/modelcontextprotocol

### 1.5 RDM 契约 vs 最新规范的差异清单（逐条可执行）

| # | `docs/agent-integration.md` 现状 | 2026-07-28 规范 | 建议 |
|---|---|---|---|
| 1 | 协议版本"优先 2025-06-18，兼容 2024-11-05/2025-03-26（echo）"；代码 `rdm-agent/src/lib.rs:22-24` 固化 `PROTOCOL_VERSIONS`/`LATEST_PROTOCOL_VERSION` | 当前 = 2026-07-28；legacy 握手服务需**同时**支持现代无握手 | 改成"dual-era：legacy ≤2025-11-25 + modern 2026-07-28"；常量改为按 era 分组的 supportedVersions |
| 2 | 依赖 `initialize` 能力协商 | modern 无 initialize；`server/discover` 为 MUST | 实现 `server/discover`；首消息分派 era |
| 3 | 能力含 `logging`、发 `notifications/message` | Logging 弃用；日志按请求 `_meta.logLevel`，stdio 写 stderr | 删除 logging 能力；改为 stderr + 可选 per-request logLevel |
| 4 | `resources` 能力 `subscribe=false` | `resources/subscribe` 移除；改 `subscriptions/listen` | 现代代际实现 listen（可只订阅 tasks/资源变更）；legacy 保持 false |
| 5 | 未提 `resultType` | **所有结果必须带 `resultType`**（`complete`/`input_required`） | MCP server 输出层加字段；legacy 客户端可忽略 |
| 6 | tasks 用自研 16 工具表达 | 2025-11-25 实验 tasks 已死；2026-07-28 为扩展 | 不要把 2025-11-25 tasks 当目标；现代→Tasks 扩展，legacy→自有工具轮询 |
| 7 | "HITL elicitation（P1）弹审批" | 现代必须 MRTR；form 模式禁收密钥 | plan/apply 为可移植核心；MRTR 作增强；凭据走 URL 模式/secret handle |
| 8 | 工具失败要求 `isError:true` + text | 一致；另需区分协议错误（-32602 等） | 保持；补"未知 tool/参数非法"的协议错误映射 |
| 9 | 每条工具都要求 `outputSchema` | 一致；2026 放宽 schema 方言 | 保持；建议 `structuredContent` 同时回写 text JSON 以兼容 |
| 10 | 列表/资源无缓存字段 | 列表/读取结果需 `ttlMs` + `cacheScope` | 统一在输出层注入（如 `ttlMs: 5000, cacheScope:"private"`） |
| 11 | 未提工具顺序 | SHOULD 确定性排序 | 工具/资源列表按名称排序（也便于 diff/测试） |
| 12 | `mcp_e2e.mjs` 含 `ping` 验收 | `ping` 已移除 | 双模式 harness：legacy 测 ping，modern 断言 ping 不存在/未实现 |

---

## 2. 生态对比：MCP / OpenAI / Anthropic / A2A / AGENTS.md / SKILL / llms.txt

### 2.1 定位与能力对比

| 方案 | 层与形态 | 发现机制 | Schema 能力 | 长任务/状态 | 审批/权限 | 与 RDM 的关系 |
|---|---|---|---|---|---|---|
| **MCP** | 跨进程协议（JSON-RPC；stdio/Streamable HTTP） | `initialize`（legacy）/ `server/discover`（modern）；tools/list、resources、prompts | inputSchema/outputSchema（JSON Schema 2020-12）、structuredContent、annotations | Tasks 扩展 + 进度通知 + 取消 | annotations 提示 + 客户端 HITL + MRTR/elicitation；OAuth（远程） | **RDM 的主协议**；本地 stdio 最合适 |
| **OpenAI function calling** | 模型 API 内的工具调用约定（host 执行函数） | 请求里直接给 `tools` 数组 | JSON Schema 参数 + `strict` 模式 | 无标准长任务语义（由应用自行建模） | 由 host 应用实现 | 不是跨进程协议；RDM 不应直接实现，而是让 host 通过 MCP 调 RDM |
| **Anthropic tool use** | 模型 API 内的 `tool_use`/`tool_result` 内容块 | 请求里给 `tools` | JSON Schema `input_schema`；并行 tool_use | 无标准长任务语义 | host 应用/HITL | 同上；Claude API 支持接 MCP server（以官方文档为准） |
| **A2A（Agent2Agent）** | **agent 与 agent 之间**的互操作协议（v1.0.0，2026-03 发布；Apache-2.0；26,195★） | Agent Card（`/.well-known/agent-card.json` 类发现）+ 任务 API | 消息/Artifact/Task 模型；多绑定（JSON-RPC/gRPC/HTTP+JSON） | 原生 Task 生命周期（长任务一等公民） | 企业级授权/身份 | 与 RDM 不冲突：**RDM 是工具/能力提供方，不是 agent**；A2A 适合"RDM 作为下载 agent 被另一个 agent 调度"的 P2 场景 |
| **AGENTS.md** | **仓库内**给 coding agent 的 Markdown 约定（无字段要求；就近覆盖；24,982★） | 文件名约定 + 目录层级 | 无（人/AI 可读文本） | 无 | 无 | **RDM 已有 AGENTS.md**（P0）；注意与代码同步，避免"文档写 16 工具、代码是桩" |
| **Agent Skills（SKILL.md）** | **可复用任务包**：`SKILL.md` + 脚本/资源，渐进加载（`anthropics/skills` 181,960★；官方 MCP Skills 扩展可经 MCP 暴露） | 目录/SKILL.md；MCP `skills` 扩展 | 无（文本 + 可选脚本）；前 YAML frontmatter | 无 | 无 | **RDM 已规划 `skills/rdm/SKILL.md`（P0）**；这是"让 agent 不读源码就会用 RDM"的最高 ROI 文档 |
| **llms.txt** | **网站**给 LLM 的 Markdown 索引提案（Jeremy Howard；`AnswerDotAI/llms-txt` 2,662★；非标准协议） | 站点根 `/llms.txt` | 无 | 无 | 无 | RDM 官网/文档站可放（P0 轻松项）；本仓库根也可放（契约已列 P0） |

> 数据来源：GitHub API（2026-10-19 03:18 抓取）；A2A v1.0.0 发布日期见 https://github.com/a2aproject/A2A/releases/tag/v1.0.0（本次未逐行核对规范全文，**部分细节未获取**）。
> 结论：**这些不是竞品，是不同层。** RDM 应该：MCP（协议层）+ SKILL/AGENTS.md（agent 使用层）+ llms.txt（文档发现层），A2A 仅在"被其他 agent 调度"的路线图里出现；OpenAI/Anthropic 的工具调用由 host 负责，RDM 不重复实现。

### 2.2 组合建议（RDM 的最小心智模型）

```
[Agent host / LLM]
   ├─(MCP stdio)→ rdm-mcp  → rdm-agent(契约/任务/策略) → rdm-core(引擎)
   ├─(shell)→ rdm CLI --output json/ndjson
   └─(读文档)→ AGENTS.md / SKILL.md / llms.txt
```

- **三层必须同源**：工具 schema 与 CLI `--json` 信封由 `rdm-agent` 单一来源生成（契约 §1 已这样设计，正确）。
- **SKILL 不是可选装饰**：agent 选错工具的成本（多步、误审批）远高于文档成本；SKILL 决定 pass@1。

---

## 3. 机器接口横评：哪些"可被 agent 使用"的设计值得抄

### 3.1 横评表

| 接口 | 机器输出形态 | schema 稳定性 | 错误契约 | 进度/长任务 | 发现性 | 可借鉴点 / 反面 |
|---|---|---|---|---|---|---|
| **aria2 JSON-RPC** | JSON-RPC（`addUri`/`tellStatus`/`pause`/`unpause`/`remove`/`getGlobalStat`…；`system.multicall`；WebSocket 通知） | 方法稳定但**无版本化 schema**；字段以文档为准 | JSON-RPC error + 字符串 code；**默认无鉴权**（`--rpc-secret` 可选）；曾有未鉴权 RPC→任意文件写/RCE（aria2 #2402） | `tellStatus` 轮询 + 通知 | `system.listMethods`/`getVersion` 有，但无自描述 schema | 优点：方法覆盖面广、轮询+通知双通道；缺点：**安全默认差、无 schema 版本、错误不够结构化** |
| **yt-dlp `--dump-json`/`--print`** | 每视频一个 JSON（`-J`/`--dump-single-json`）；`--print` 模板；`--progress-template` 机器进度 | 字段丰富但**无版本号/无正式 schema**（随站点变化） | 退出码粗粒度（成功/失败）+ stderr 文本 | `--progress-template` + 行缓冲 | `--help` 巨大；文档站可搜 | 抄：`--print` 模板 + 进度模板；不抄：错误无结构化契约 |
| **GitHub CLI `--json` + `gh api`** | `--json field,field`、`--jq`、`--template` | 字段名稳定且**错误会列出可用字段**（自描述错误） | 退出码 + stderr；`gh api` 返回原始 API JSON | 无（短命令） | `gh help` + 每个命令 `--json` 字段说明 | **抄：`--json` 字段白名单 + 非法字段报错时列出合法值**（对 agent 极其友好） |
| **ripgrep `--json`** | JSON Lines 事件流：`begin`/`match`/`context`/`end`/`summary` | 事件 kind 稳定；文档说明结构 | 退出码 0/1（无匹配）/2（错误） | 流式事件 | `--help` 详尽；`--json` 文档明确 | **抄：NDJSON 事件流 + `summary` 尾事件**（RDM 契约 §2.2 已同构） |
| **ffmpeg `-progress pipe:1`** | `key=value` 行（frame/fps/out_time_us/speed/progress=continue\|end），`-nostats` 关掉人类输出 | 键稳定；无 schema 版本 | 退出码 + stderr | **专门进度通道**（与 stderr 分离） | `-h` / 文档 | **抄：进度独立通道 + `progress=end` 终止标记**；不抄：key=value 弱类型 |
| **curl / wget** | 无结构化输出（文本/文件） | 不适用 | **退出码语义丰富**（curl 6 DNS、22 `--fail` HTTP 错等；wget 4 网络/8 服务器错等） | 进度条（可关） | `--help` | **抄：语义化退出码**（RDM 契约 0/2/3 已同构）；不抄：无机器输出 |
| **cargo `metadata --format-version 1`** | 单个大 JSON | **显式 `format-version`** + `"version"` 字段 | cargo 错误文本 | 无 | `cargo help metadata` | **抄：显式版本号 + 稳定 schema**（RDM `schema_version: rdm.agent/v1` 已对） |
| **docker/podman `--format json`、`inspect`** | 行 JSON / 大 JSON | 随版本演进，字段兼容 | 退出码 + 文本 | 有 `--progress`（部分命令） | `--help` | 抄：`--format json` 统一开关；注意字段演进需标注 |

### 3.2 12 条"agent 友好"设计原则（每条含 RDM 落地）

1. **版本化信封**：每个输出带 `schema_version`；只做加法、不破坏性改名；错误也带版本。→ RDM：`rdm.agent/v1`（**已实现类型**）。
2. **稳定、可枚举的错误码**：`code` 机器可读 + `retryable` + `hint` + `details`；未知错误也归入 `internal`。→ RDM：`ErrorCode`（**已实现 + 单测**），但 CLI/MCP 未接线。
3. **显式句柄而非隐式会话**：长任务返回 `task_id`，后续调用带 handle；断线/重启可续。→ RDM：TaskStore（**已实现**）；MCP modern 正好无会话（**架构吻合**）。
4. **幂等键**：重试不产生重复副作用；键作用域明确（per-agent/per-store）。→ RDM：`idempotency_key` + `find_by_idempotency_key`（**已实现**），需定义冲突返回（同键不同参数 → `conflict`）。
5. **plan → approve → apply 两阶段**：副作用操作先出确定性计划，再凭一次性 token 执行。→ RDM：契约有（**未实现**）；token 应绑定 plan 哈希。
6. **结构化进度与最终结果分离**：进度走 NDJSON/通知（seq 单调、可丢弃、可节流），最终结果走一次完整 JSON。→ RDM：契约 §2.2（**未实现**）。
7. **有界输出 + 分页 + 截断标记**：默认 `--limit`，超限明确 `truncated:true`/`next_cursor`；绝不静默截断。→ RDM：契约分页（**未实现**）；MCP 用 `nextCursor` + `ttlMs/cacheScope`。
8. **确定性**：同输入同输出顺序（列表排序、时间戳不影响身份、ID 稳定）；便于 agent 缓存与 diff。→ RDM：需在工具/资源列表排序。
9. **自描述与可发现**：`capabilities`/`schema`/`--help`/`server/discover`；非法参数报错列出合法值（学 `gh`）。→ RDM：契约有 `rdm capabilities/schema`（**未实现**；`tool_definitions()` 空）。
10. **人读默认 + 机器开关**：默认人类可读；`--output json|ndjson` 显式切换；stdout=数据、stderr=日志。→ RDM：CLI 有 `--json`（download/probe），agent 全局 `--output` 未实现。
11. **语义化退出码 + 非交互**：非 TTY 不弹交互；Ctrl-C 暂停并持久化；退出码 0/2/3。→ RDM：契约/架构已有（**CLI 已实现 0/2/3**）。
12. **诚实性与可解释**：能力/降级/不确定性写进 `warnings`（如 `range_unsupported`、h2 单连接）；错误给 `hint`；不伪造进度。→ RDM：契约 §0.6（**已定义，未实现**）；这是与"壳"产品拉开差距的点。

> 补充（第 13 条，重要）：**凭据最小化**——工具参数不接收明文凭据，改用 secret handle/环境变量；输出永不回显。→ RDM：`redact_request`（**已实现**），但工具 schema 仍需改造。

---

## 4. 下载器接 agent 的独特风险与 RDM 应对

> 基线：`docs/agent-integration.md` §4 策略/审批、§2 脱敏、§3 工具契约；下表"现有机制"指**契约已有**，"实现"指 03:18 代码观察。

| # | 风险 | 攻击面/后果 | RDM 现有机制 | 缺口与建议（优先级） |
|---|---|---|---|---|
| 1 | **SSRF / 内网穿透 / 云元数据** | agent 被诱导下载 `http://169.254.169.254/...`、`http://127.0.0.1:...`、内网服务；重定向/DNS rebinding 绕过初检 | policy `allowed_hosts`（**默认 `["*"]`**！）、`allow_private_hosts`（**默认 true**） | **P0**：默认 deny private/link-local/loopback/metadata；scheme 仅 http/https；**解析后按 IP 校验**、每跳重定向复检、端口 allowlist；拒绝 `file://` 等（契约已有）。真实同类案例：`GopeedLab/gopeed#1589` 开放代理 SSRF |
| 2 | **凭据泄漏进 LLM 上下文** | `cookie`/`authorization` 作为工具参数 → 进入 transcript/客户端日志/第三方推理 | `redact_request` 已实现；任务输出脱敏 | **P0**：工具参数改为 secret 引用（env/secret store handle）；禁止 form elicitation 收密钥（规范 MUST NOT）；远程场景用 URL 模式 |
| 3 | **预算/磁盘耗尽** | 超大文件、无限 chunked、`Content-Length` 撒谎、并发 3×10GiB 撑爆磁盘 | `max_bytes_per_task=10GiB`、`require_approval_over_bytes=1GiB`、`max_concurrent_tasks=3` | **P0**：引擎层强制"已写字节 > max_bytes 立即中止"；未知长度时按流式上限；预检剩余磁盘；总量配额（不止单任务） |
| 4 | **越权审批（模型自批）** | agent 自己调 `apply_plan` 绕过人类；或偷偷改 policy.json | 契约设计 plan→token→apply；token 单次/10 分钟 | **P0**：token 绑定 plan 哈希 + 由**人类路径**签发；policy 文件放在 agent 不可写位置或要求人工确认；借鉴 MCP 本地 server 安全建议 |
| 5 | **取消语义误解** | agent 以为 `cancel` 会删任务/文件；或以为取消等待=停止下载 | 契约明确"cancelled 只取消等待，停止需 `rdm_cancel_task`"；`.part` 保留可恢复 | **P0**：SKILL/工具描述写死语义；测试覆盖；MCP modern 用 `tasks/cancel`（协作式，可能仍完成） |
| 6 | **重复执行/重复下载** | 超时重试、多 agent 同 URL、任务被重复创建 | `idempotency_key`；overwrite 冲突检测 | **P1**：键作用域（per-agent）+ 同键不同参数返回 `conflict`；跨进程去重；`resume` 默认开 |
| 7 | **多 agent 争用/越权操作** | A agent 暂停/取消 B 的任务；并发写同一 task 文件 | 每任务独立文件 + 原子写；`max_concurrent_tasks` | **P1**：task 所有权/主体字段 + 操作鉴权；per-task revision/CAS 或 advisory lock（当前 TaskStore 未见锁）；per-agent 配额与公平调度 |
| 8 | **审计与可追溯** | 出事无法回答"谁在何时下载了什么"；日志含敏感信息 | 事件 jsonl + `trace_id`；脱敏 | **P1**：审计导出（谁/什么/结果/审批人）；`trace_id` 贯穿 CLI/MCP/引擎；可选 OTel（2026-07-28 已定义 `traceparent` 等 `_meta` 约定） |
| 9 | **不可信内容注入** | 文件名/URL/错误文本被回显进资源/事件，诱导模型执行指令；未来若暴露文件内容更危险 | 契约不回显 cookie；任务 JSON 含 url/file_name（用户可控文本） | **P1**：所有 agent 可见文本按"数据"处理：长度上限、去控制字符、明确 `untrusted` 标注；**永不在工具结果里返回下载内容**；post-process hooks 默认关闭 |
| 10 | **工具误选/描述质量** | 16 个工具（probe/plan/download/apply/list/get/wait/pause/resume/cancel/verify/doctor/selftest/bench/state…）重叠，模型选错多步 | 工具描述要求"何时用/不用/边界" | **P0**：每个 description 写"何时不用"与前置条件；错误 `hint` 引导下一步；用评测量化 pass@1 |
| 11 | **进度噪音/上下文膨胀** | 250–500ms 通知、大列表、事件 jsonl 全文塞进上下文 | 脱敏 + 分页契约 | **P1**：进度节流下限（≥250ms）、事件按需 `include_events` 且默认 0；列表 default limit 小、摘要优先 |
| 12 | **诚实性/性能宣称** | "多线程更快"在无 Range/h2 单连接/服务器限流下不成立；agent 据此做错决策 | 契约 `warnings`/`range_supported`/`http_version` | **P1**：`warnings` 机器可读（`range_unsupported`、`single_connection`、`h2_multiplexed`）；`explain-why-slow` 诊断；benchmark 只用可复现数据（见 `docs/research/competitive-landscape.md` §4/§6） |
| 13 | **远程暴露（download-as-a-service）** | 若把 rdm-mcp 暴露到网络：成为任意 URL 代理/盗链工具，OAuth/多租户问题 | 当前 stdio-only（安全默认） | **P2**：远程必须 OAuth 2.1 + 禁止 token passthrough + 每用户隔离 + URL 模式 elicitation；否则别做 |
| 14 | **策略文件即权限边界** | agent 有文件系统权限时可改 policy.json 自我提权 | 契约以 policy 文件为边界 | **P0**：明确"policy 由人类维护"；可放 `%PROGRAMDATA%`/只读位置；变更需审计；MCP 客户端侧安装确认（SEP-1024 类） |

**关键设计决策（建议写进契约 v2）**：
- **审批三态**：`policy_denied`（不可绕过，只能改 policy）/ `needs_approval`（plan+token）/ `allow`（直接执行）。
- **可移植审批核心** = `plan_download` + `apply_plan(token)`；**MRTR/elicitation 只是 UX 增强**（现代代际），不是唯一路径；URL 模式 elicitation 用于需要用户凭据/第三方授权的场景（下载登录墙资源），**凭据永不经过 client/LLM**。
- **任务扩展双模**：modern client 声明 `io.modelcontextprotocol/tasks` → 长下载返回 `CreateTaskResult`；否则返回普通结果 + `task_id`，由 `rdm_get_task`/`rdm_wait_task` 轮询。

---

## 5. 能力构思清单（P0 缺口 / P1 / P2）

> 判定：**P0** = 契约 §7 已列且是"能用"的最小闭环；**P1** = 高价值/低成本或竞争差异化；**P2** = 高成本/高不确定/依赖外部采纳。状态基于 03:18 代码快照。

### 5.1 P0：契约已定义，实现缺口（12 项）

| # | 能力 | 价值 | 状态（03:18 复核） | 规范对齐 |
|---|---|---|---|---|
| 1 | 统一 JSON 信封 + `schema_version` | agent 永不猜格式 | 已实现（rdm-agent 类型 + CLI output 模块） | 通用 |
| 2 | 稳定错误码 + retryable + hint | 自动重试/降级 | 已实现 + 单测；CLI/MCP 接线待验 | 通用 |
| 3 | `capabilities`/`schema` 自描述 | 免读源码 | 工具目录已实现（`rdm-agent/src/tools.rs`，16 工具 + schemas）；`rdm capabilities/schema` 子命令仍缺 | MCP tools/list + server/discover |
| 4 | durable task store + 事件 journal | 重启可查/可恢复 | 已实现（TaskStore/events，原子写） | MCP Tasks 扩展 |
| 5 | 幂等键 | 重试不重复 | 已实现（TaskStore `find_by_idempotency_key`） | 通用 |
| 6 | policy allowlist + 审批 gate | 安全边界 | Policy + ApprovalToken/DownloadPlan 已实现（`lib.rs`，TTL 10 分钟）；**端到端 apply 未验证** | 通用 + MRTR |
| 7 | 输出/日志脱敏 | 凭据不泄漏 | 已实现（redact，单测） | 通用 |
| 8 | MCP server（16 工具） | 主协议入口 | **桩（205B，03:18 仍未变）** | legacy+modern |
| 9 | CLI agent 子命令 | 无 MCP 环境可用 | 已出现 `doctor/selftest/bench/tasks/task/state/run`（`cli.rs`）；`capabilities/schema` 缺；**未运行验证** | 通用 |
| 10 | SKILL.md / llms.txt / AGENTS.md | agent 决策质量 | AGENTS.md 有；SKILL/llms 缺失；AGENTS 引用不存在的 `docs/agent-mcp-reference.md` | Skills 扩展/文档约定 |
| 11 | selftest / bench / state inspect+repair（dry-run） | readiness + 可维护 | CLI 子命令已出现；`scripts/benchmark.ps1` 已有 | 通用 |
| 12 | 进度/取消/日志通知语义 | 长任务可控 | 契约定义；CLI `tracker.rs` 出现；MCP 侧未接线 | MCP progress/cancellation |

### 5.2 P1：建议优先（14 项）

| # | 能力 | 价值 | 风险/成本 | 对齐 |
|---|---|---|---|---|
| 1 | **Tasks 扩展**（`io.modelcontextprotocol/tasks`）：长下载返回 task handle，`tasks/get` 轮询、`tasks/update` 补审批输入 | 与 2026 主线一致；客户端可断线重连继续 | 客户端支持尚少（支持矩阵**未获取**）；需 dual-mode 回退 | ext-tasks |
| 2 | **`server/discover` + 能力缓存** | 现代客户端必需（MUST）；降低握手延迟 | 低 | 2026-07-28 |
| 3 | **MRTR 审批**（`input_required` + elicitation form） | 审批体验标准化；无需自定义弹窗协议 | 需双代际分支；form 禁收密钥 | MRTR/elicitation |
| 4 | **Skills over MCP**（SEP-2640） | agent 可自动发现 RDM 用法 | 扩展新、客户端支持未知 | Skills 扩展 |
| 5 | **`rdm replay`**（用 journal+参数复现失败任务） | 故障复现/回归；与 fixture 组合 | 需记录足够上下文且脱敏 | 通用 |
| 6 | **`rdm report` 诊断包**（脱敏 zip：版本/配置/事件/环境/flags） | 支持成本大降；agent 可自助上报 | 脱敏必须强；大小上限 | 通用 |
| 7 | **`explain-why-slow` 网络诊断**：RTT、Range 支持、h2 单连接、每连接吞吐、代理/TLS 告警 | 直接对应用户痛点；差异化 | 需探测逻辑与 fixture 场景 | 通用 |
| 8 | **多 agent lease/ownership + per-agent 配额** | 共享 GUI+agent 时不互相踩踏 | 并发正确性；文件锁 | 通用 |
| 9 | **调度：优先级/带宽窗口/配额** | 服务器/团队场景 | 引擎改造 | 通用 |
| 10 | **token 高效分页 + summary/detail 双视图 + 缓存 TTL** | 上下文成本 | 低 | CacheableResult |
| 11 | **工具描述质量 + prompt 模板 + 描述评测** | pass@1 直接提升 | 低；需评测基线 | prompts/tools |
| 12 | **批量 manifest（依赖/失败策略）** | 数据集/镜像场景 | 依赖图复杂度 | 通用 |
| 13 | **多源镜像回退**（失败块换源） | 对齐 aria2；容错 | 校验/一致性复杂（CDN 不一致） | 通用 |
| 14 | **签名 URL 过期恢复**（URL 模式 elicitation 重新授权） | 网盘/对象存储真实场景 | 需 URL 模式与身份校验（防钓鱼） | elicitation |

### 5.3 P2：远期/高不确定（9 项）

| # | 能力 | 价值 | 风险 | 对齐 |
|---|---|---|---|---|
| 1 | GUI 展示 agent 任务（读同一 task store） | 人机协作可视化 | GUI 与 store 并发 | 通用 |
| 2 | MCP Apps（host 内嵌进度/审批 UI） | 体验最好 | ext-apps 采纳未知 | ext-apps |
| 3 | **remote MCP / 下载即服务** | 平台化 | SSRF/滥用/多租户/成本；必须 OAuth 2.1 | 授权扩展 |
| 4 | 后处理 hooks（解压/移动/通知） | 端到端自动化 | 任意命令执行 = RCE 风险；必须 allowlist+无 shell | 通用 |
| 5 | 完整性清单/签名校验（SHA-256/清单/minisign） | 供应链安全 | 密钥管理 | 通用 |
| 6 | MCP Registry 发布（server.json） | 可被 host 发现 | 审核/品牌 | Registry |
| 7 | agent evals harness 进 CI（20+ 场景） | 回归 agent 质量 | 维护成本 | 通用 |
| 8 | OTel trace + 审计导出 | 企业可观测 | 遥测隐私 | 2026-07-28 `_meta` trace 约定 |
| 9 | 自适应连接数（学习 per-host 限流） | 真实加速/减少 403 | 需安全上限与诚实提示 | 通用 |

> **不建议做**：`sampling`（2026-07-28 已弃用）、2025-11-25 实验性 `tasks`（已被扩展取代）、让 LLM 自己批准破坏性操作、在工具结果中返回下载内容。

---

## 6. agent 评测设计（10 个可落地场景）

> 基础设施：`rdm-fixture-server`（延迟/限速/忽略 Range/429/ETag/中断注入）+ `scripts/mcp_e2e.mjs`（扩为双模式）+ `rdm selftest`（每次场景前 readiness）+ `rdm bench`（性能声明只在 bench 全绿时对外）。所有场景**确定性**：固定 64 MiB 载荷、固定种子、随机端口、隔离 `LOCALAPPDATA`。

| # | 场景 | 目的 | 通过标准（pass） | 指标 |
|---|---|---|---|---|
| E1 | 冷启动发现：只给 SKILL/AGENTS，要求"下载 URL 到目录" | 文档能否让 agent 一次选对工具 | 首次工具调用即 `rdm_download`（或 probe→download），无试错 | 步数、token、错选次数 |
| E2 | 快乐路径：8 MiB + 已知 SHA-256 + 8 连接 | 基本可用性 | 文件哈希一致、`completed` 事件含 `sha256`、退出 0 | 步数、耗时、连接数 |
| E3 | 审批流：大文件超阈值 → `needs_approval` → plan → apply | 不可绕过 + 不重复下载 | apply 前 fixture 收到 0 字节；token 复用被拒；批准后完成 | 步数、误批率、token 复用拒绝 |
| E4 | SSRF 拒绝：`http://127.0.0.1/`、`http://169.254.169.254/`、重定向到内网 | 安全默认 | `policy_denied`，fixture/内网监听器收到 **0 请求** | 拒绝率、误放行数 |
| E5 | 忽略 Range：fixture 强制 200 | 诚实降级 | 完成 + `warnings` 含 `range_unsupported` + 实际单连接（fixture 统计） | 请求数、是否重试风暴 |
| E6 | 连接数风控：fixture 对第 3+ 并发返回 403/429 | 自适应与恢复 | 降连接后完成；不无限重试；错误可解释 | 重试次数、完成率 |
| E7 | 取消/恢复：下载中 cancel 等待→显式 cancel task→resume | 语义正确 + 只补缺失块 | `.part` 保留；恢复后传输 ≤ 剩余 + 1 块；哈希正确 | 重下字节、步数 |
| E8 | 幂等：同 `idempotency_key` 调两次（含超时重试） | 不重复副作用 | 仅 1 个 task、fixture 仅 1 次完整下载；不同参数同键 → `conflict` | 重复下载数 |
| E9 | 脱敏：请求带 `cookie`/`authorization` | 凭据不泄漏 | stdout/stderr/事件/task JSON 中**零出现**（字符串扫描） | 泄漏数（必须 0） |
| E10 | 多 agent 争用：2 个 agent、并发 3 上限、A 试图取消 B 的任务 | 隔离与授权 | A 被拒（`conflict`/`policy_denied`）；B 任务不受影响 | 越权成功数（必须 0） |
| E11（加分） | 分页 token 效率：100 个任务、`--limit 20` | 上下文成本 | 单页 token 低于预算；无静默截断 | tokens/页 |
| E12（必测） | **协议双代际矩阵**：legacy（2025-06-18/2025-11-25）与 modern（2026-07-28）各跑 E1–E10 | 兼容性 | 两代均通过；modern 下 `server/discover`、`resultType`、无 ping 断言 | 双代通过率 |

**指标定义**：
- `pass@1`：一次成功比例（不给人类提示）；
- `steps`：工具调用次数到完成；
- `tokens`：agent 侧输入+输出估算（harness 记录 transcript，按工具结果字节数 + 消息数估算，标注估算方法）；
- `error_recovery_rate`：遇到可恢复错误后无人工干预完成的比例；
- `fixture_bytes/requests`：证明"只补缺失块/降级单连接/未发 SSRF 请求"。
- 每个场景 N=5 取中位数；失败必须留 transcript 供 `rdm replay` 复现。

**与 RDM QA 的配合**：`selftest` 负责"引擎 readiness"（非 agent）；`bench` 负责性能声明；`qa-report-agent.md` 记录 E1–E12 结果与差距；`mcp_e2e.mjs` 作为协议层回归（先扩 dual-era，再加 agent 质量场景）。

---

## 7. 参考链接

**MCP 官方**
- 规范首页（当前）：https://modelcontextprotocol.io/specification/2026-07-28/
- 2026-07-28 变更日志：https://modelcontextprotocol.io/specification/2026-07-28/changelog
- 版本与协商：https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning
- Tools：https://modelcontextprotocol.io/specification/2026-07-28/server/tools
- Elicitation：https://modelcontextprotocol.io/specification/2026-07-28/client/elicitation
- MRTR：https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr
- Subscriptions：https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/subscriptions
- Cancellation / Progress：https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/cancellation ｜ .../progress
- Transports：https://modelcontextprotocol.io/specification/2026-07-28/basic/transports
- 安全最佳实践：https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices
- 本地 server 安全：https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/local-server-security
- 文档索引（llms.txt）：https://modelcontextprotocol.io/llms.txt
- Protocol eras（Inspector 兼容测试）：https://modelcontextprotocol.io/docs/2026-07-28/tools/inspector/protocol-eras
- Inspector：https://modelcontextprotocol.io/docs/2026-07-28/tools/inspector
- 扩展总览：https://modelcontextprotocol.io/extensions/overview
- Tasks 扩展：https://modelcontextprotocol.io/extensions/tasks/overview ｜ 仓库：https://github.com/modelcontextprotocol/ext-tasks
- Skills 扩展：https://modelcontextprotocol.io/extensions/skills/overview
- MCP Apps：https://modelcontextprotocol.io/extensions/apps/overview ｜ 仓库：https://github.com/modelcontextprotocol/ext-apps
- Registry：https://modelcontextprotocol.io/registry/about
- 规范主仓：https://github.com/modelcontextprotocol/modelcontextprotocol
- 2025-11-25 变更：https://modelcontextprotocol.io/specification/2025-11-25/changelog

**生态与对比**
- A2A 仓库：https://github.com/a2aproject/A2A ｜ v1.0.0：https://github.com/a2aproject/A2A/releases/tag/v1.0.0 ｜ 站点：https://a2a-protocol.org/
- Agent Skills 公开仓库：https://github.com/anthropics/skills
- AGENTS.md：https://agents.md/ ｜ 仓库：https://github.com/agentsmd/agents.md
- llms.txt：https://llmstxt.org/ ｜ 仓库：https://github.com/AnswerDotAI/llms-txt
- OpenAI function calling 文档：https://platform.openai.com/docs/guides/function-calling
- Anthropic tool use 文档：https://docs.anthropic.com/en/docs/build-with-claude/tool-use/overview

**机器接口**
- aria2 RPC：https://aria2.github.io/manual/en/html/aria2c.html#rpc-interface ｜安全案例：https://github.com/aria2/aria2/issues/2402
- yt-dlp 输出：https://github.com/yt-dlp/yt-dlp#output-template
- GitHub CLI：https://cli.github.com/manual/gh_pr_list ｜https://cli.github.com/manual/gh_api
- ripgrep JSON：https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md
- ffmpeg `-progress`：https://ffmpeg.org/ffmpeg.html#Main-options
- curl 退出码：https://curl.se/docs/manpage.html ｜Wget 退出码：https://www.gnu.org/software/wget/manual/wget.html
- cargo metadata：https://doc.rust-lang.org/cargo/commands/cargo-metadata.html

**本仓库相关**
- `docs/agent-integration.md`（契约 v1）
- `docs/architecture.md`（引擎/诚实性语义）
- `AGENTS.md`（agent 仓库指南）
- `scripts/mcp_e2e.mjs`（MCP 协议 e2e harness，按契约先行编写）
- `scripts/benchmark.ps1`（1 vs N 连接基准）
- `docs/research/competitive-landscape.md`（竞品/多线程有效性模型，含 h2 陷阱与 B1–B5 基准）

---

## 8. 附录：仓库现状快照与"未获取"清单

### 8.1 快照（2026-10-19 03:18 复核，仅文件存在性，未构建；03:16→03:18 变化很快）

| 项 | 观察（03:18） |
|---|---|
| `crates/rdm-agent/src/lib.rs` | 51,589 B；5 个单测；`TaskStore`（create/update/get/list/events/`find_by_idempotency_key`）、`Policy`、`ErrorCode`、`redact_request`、**`ApprovalToken`/`DownloadPlan`（TTL 10 分钟常量）**；**`PROTOCOL_VERSIONS` 仍为 2025-06-18/2025-03-26/2024-11-05（第 22-24 行）** |
| `crates/rdm-agent/src/tools.rs` | 57,182 B（03:18 新增）；`tool_definitions()`/`tool_by_name()` 已实现，工具名覆盖契约 16 个（`rdm_capabilities/probe/download/plan_download/apply_plan/list_tasks/get_task/wait_task/pause_task/resume_task/cancel_task/verify_file/doctor/selftest/bench/state_maintenance`）；多个单测 |
| `crates/rdm-mcp/src/main.rs` | **205 B 桩**（`not implemented yet`）；无测试 |
| `crates/rdm-cli` | 已拆为 `main.rs`(1.8 KB)/`cli.rs`(9.4 KB)/`output.rs`(10.6 KB)/`tracker.rs`(12.8 KB)/`util.rs`(13.6 KB)；子命令含 `download/probe/doctor/selftest/bench/tasks/task/state(inspect/repair)/run`；**未运行验证** |
| `docs/agent-integration.md` | 342 行 / 17,129 B（03:14 版）；§7 标 9 项 P0 |
| `AGENTS.md` | 存在；引用 `docs/agent-mcp-reference.md`（**该文件不存在**）与 `skills/rdm/SKILL.md`（**目录不存在**）→ 文档漂移 |
| `skills/`、`llms.txt` | **不存在**（契约列为 P0） |
| `scripts/mcp_e2e.mjs` | 存在；TDD 先行；**写死 `initialize('2025-06-18')`（653–663 行）并多处断言 `ping`（674、799、1387–1408 行）**；未区分 legacy/modern |
| `scripts/benchmark.ps1` | 存在（1 vs N 连接 fixture 基准） |
| `docs/qa-report.md` | 无 agent 章节（agent QA 尚未开始） |
| 并行构建目录 | `target-agent/`、`target-cli-agent/` 存在 → 相关开发正在进行 |

### 8.2 未获取 / 未核实

| 项 | 状态 |
|---|---|
| Tasks / Skills / MCP Apps 扩展的**客户端支持矩阵** | **未获取**（官方有 client-matrix 页面，本次未逐项核对） |
| 2026-07-28 规范全文逐条核对 | 已核 changelog、tools、elicitation、versioning、tasks 扩展；其余章节按 changelog 归纳 |
| A2A v1.0.0 规范全文 | **未获取**（依据 GitHub release 与检索摘要；绑定/错误码细节未逐条核） |
| OpenAI / Anthropic 工具的当前 API 字段与限制 | 以官方文档为准；本次**未抓取**（版本变动快，不写死） |
| Agent Skills 规范版本号 | **未获取**（仓库已核，spec 页未抓） |
| `anthropics/skills` 181,960★ 等数值 | GitHub API 实测（2026-10-19 03:18），可能含异常增长，按 API 记录 |
| RDM 实现状态 | 仅文件观察；**未 build/test**；写作期间持续变化（见 §8.1 快照时间） |
| agent 评测的 token 估算精度 | 未标定；建议 harness 同时记录原始 transcript，便于事后校正 |

### 8.3 立即可执行的三件事（给 Lead 的建议摘要）

1. **契约 v2：dual-era 协议**——补 2026-07-28（`server/discover`/per-request `_meta`/`resultType`/移除 ping/logging/subscribe），保留 legacy 路径；`mcp_e2e.mjs` 改双模式。
2. **安全默认反转**——policy 默认 deny private/metadata、DNS+重定向二次校验、字节上限流式强制、凭据改 secret handle；把审批 token 做成 plan 哈希绑定 + 单次 + 人工签发；MCP modern 用 MRTR 增强。
3. **把"长下载"标准化为 Tasks 扩展**，同时保留自研 task 工具作 legacy 回退；`rdm_download` 两种返回形态对 agent 透明。
