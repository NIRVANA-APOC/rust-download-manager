# MCP 双代际实现清单 + 安全默认反转 + 1–2 天 P1 实施项

> **调研日期**：2026-10-19（Asia/Shanghai）｜**调研人**：RDM 研究分析师
> **用途**：`docs/research/agent-tool-design.md` 的补充材料，直接给 `rdm-mcp` / `rdm-agent` 实现者照着写。
> **范围**：只写文档，不改代码。规范结论均给官方 URL；拿不到的标 **未获取**，推断标 **[中]**。
> **策略前提（来自 Lead）**：`rdm-mcp` 仍在实现中 → **Phase 1 先只跑通 legacy `2025-06-18`**；Phase 2 再叠加 modern `2026-07-28`（dual-era）。本文把两阶段拆开，Phase 2 全部是**增量**，不推翻 Phase 1。
> **当前代码事实（03:18 快照）**：`crates/rdm-agent/src/lib.rs:22-24` 的 `PROTOCOL_VERSIONS = ["2025-06-18","2025-03-26","2024-11-05"]`、`LATEST_PROTOCOL_VERSION = "2025-06-18"`；`scripts/mcp_e2e.mjs` 写死 `initialize('2025-06-18')` 与多处 `ping`；`rdm-mcp/src/main.rs` 仍是 205 B 桩。

---

## 1. MCP 2026-07-28 双代际实现清单

### 1.1 术语与版本矩阵（规范原文）

规范把版本分两类（https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning）：

- **Modern**：`2026-07-28` 及以后 —— 版本/身份/能力全部走**每请求 `_meta`**，无握手。
- **Legacy**：`2025-11-25` 及以前 —— `initialize` 握手建立会话。
- **Dual-era**：同时支持两者；由**客户端如何开场**决定：带 modern `_meta` 的请求走无状态现代路径；`initialize` 请求走 legacy 语义（stdio 进程内 / HTTP 会话内）。**允许同进程并发服务两种代**。

官方兼容矩阵（直接照抄，作为验收表）：

| 客户端 | 服务端 | 结果 |
|---|---|---|
| Modern | Modern | 可用；`server/discover` 可选；版本不匹配返回 `-32022` 后客户端换版本重试 |
| Modern | Legacy | 失败；stdio 上客户端**应先**发 `server/discover` 以确定性失败 |
| Modern | Dual-era | 可用；stdio 探测返回 `DiscoverResult`，客户端保持 modern |
| Legacy | Dual-era | **可用**；服务端回应 `initialize`，按协商版本服务 |
| Legacy | Modern | 失败；服务端对 `initialize` 返回 JSON-RPC 错误；legacy 客户端无前向机制 |
| Legacy | Legacy | 按 legacy 版本工作 |

**双代际服务端选择规则（规范原文）**：
- 带 modern 每请求 `_meta` 的请求 → 无状态现代语义；
- `initialize` 请求 → legacy 语义（stdio 进程内 / HTTP 会话内）。

### 1.2 Phase 1：legacy（`2025-06-18`）逐方法清单

> 目标：先跑通"`initialize` → `tools/list` → `tools/call`（含 progress/cancel）"最小闭环，并让 `scripts/mcp_e2e.mjs` 的 legacy 部分全绿。legacy 规范入口：https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle（其余方法路径同目录结构；**置信度 [高]**，本次未逐页抓取，依据 2025-06-18 稳定结构）。

| # | 方法 / 通知 | 请求形状（关键字段） | 响应 / 行为 | 备注与来源置信度 |
|---|---|---|---|---|
| L1 | `initialize` | `params: { protocolVersion: "2025-06-18", capabilities: {...}, clientInfo: {name, version} }` | `result: { protocolVersion, capabilities, serverInfo: {name, version}, instructions? }` | 服务端回自己支持的版本；客户端若只支持请求版本且不接受回值则断开。[高] |
| L2 | `notifications/initialized` | 通知，无 `id` | 不回应 | 收到后才开始正常服务（[高]） |
| L3 | `ping` | `params: {}` | `result: {}` | **仅 legacy 路径实现**；modern 路径已删除（[高]，2026-07-28 changelog） |
| L4 | `logging/setLevel` | `params: { level: "debug"|... }` | `result: {}` | **仅 legacy**；modern 改为每请求 `_meta.logLevel`（[高]） |
| L5 | `tools/list` | `params: { cursor? }` | `result: { tools: [...], nextCursor? }` | 每个 tool：`name/title?/description/inputSchema/outputSchema?/annotations?/icons?`；分页稳定（[高]） |
| L6 | `tools/call` | `params: { name, arguments }` | `result: { content: [...], structuredContent?, isError? }` | 未知 tool/参数非法用 JSON-RPC error（建议 `-32602`）；**工具执行失败用 `isError:true` + content 文本**（[高]） |
| L7 | `resources/list` / `resources/templates/list` / `resources/read` | 标准分页 / `{uri}` | `result: { resources \| resourceTemplates \| contents: [...] }` | `subscribe` 不必实现（契约 `subscribe=false`）；[高] |
| L8 | `prompts/list` / `prompts/get` | `{cursor?}` / `{name, arguments?}` | `result: { prompts }` / `{ description?, messages: [...] }` | [高] |
| L9 | `notifications/progress`（服务端→客户端） | 依赖请求 `params._meta.progressToken` | `params: { progressToken, progress, total?, message? }`；RDM 另把 speed/ETA 放 message | 只在请求带 token 时发；建议 250–500ms 节流（[高]） |
| L10 | `notifications/cancelled`（客户端→服务端） | `params: { requestId, reason? }` | 只取消**等待/请求**，不隐式取消已创建下载任务 | 与 RDM 契约"cancel wait ≠ cancel task"一致（[高]） |
| L11 | `notifications/message`（日志） | `params: { level, logger?, data }` | 仅 legacy 且客户端 `setLevel` 后发 | modern 不允许未 opt-in 时发（[高]） |
| L12 | 版本协商细节 | 客户端 protocolVersion ∈ {2024-11-05, 2025-03-26, 2025-06-18} | 服务端回**自己实际支持**的版本 | Phase 1 建议只声明 `["2025-06-18"]`，别虚报；**2025-03-26/2024-11-05 等 Phase 2 再逐一验证**（[中]） |
| L13 | `instructions` | — | `initialize.result.instructions`：5–10 行"典型流程 + 安全边界" | agent 依赖它做规划（[中]：字段存在性确认，格式自由） |
| L14 | stdio 帧格式 | 每行一条 JSON（newline-delimited），stdout 只走协议，日志写 stderr | — | 契约 §3 已定（[高]） |
| L15 | 并发 | 多个 `tools/call` 可并发；响应按 `id` 关联 | — | 契约要求（[高]） |

**Phase 1 能力声明建议**（legacy 路径专用）：

```jsonc
// initialize result.capabilities
{
  "tools":     { "listChanged": false },
  "resources": { "subscribe": false, "listChanged": false },
  "prompts":   { "listChanged": false },
  "logging":   {}                    // 仅 legacy；modern 路径不要携带（见 M8）
}
```

**Phase 1 完成定义（DoD）**：
1. `scripts/mcp_e2e.mjs` 的 legacy 段全绿（含 `initialize` echo、`ping`、16 工具 schema、progress/cancel、错误码、脱敏）；
2. 不出现任何 `2026-07-28` 字段/常量（叶节点不预埋）；
3. 唯一需要预埋的是**入口分派点**：解析首行 → 若是 `initialize` 走 legacy 分支，否则进入 `modern_or_error` 分支（Phase 1 可先只返回"unknown method"），确保 Phase 2 是**加法**而非重写（见 §1.6）。

### 1.3 Phase 2 增量：modern `2026-07-28` 必须实现项

> 规范入口：https://modelcontextprotocol.io/specification/2026-07-28/ （changelog: .../changelog）

| # | 要求 | 精确内容 | 来源 / 置信度 |
|---|---|---|---|
| M1 | **每请求 `_meta` 校验** | 每个**请求**（request，不是 notification）`params._meta` 必须含：`io.modelcontextprotocol/protocolVersion`（string，**Required**）、`io.modelcontextprotocol/clientCapabilities`（ClientCapabilities，**Required**）；`clientInfo`（Optional，SHOULD）、`logLevel`（Optional）。缺任一必填 → `-32602 Invalid params`（HTTP 另需 400） | [高] basic/index#meta |
| M2 | **`server/discover`**（MUST 实现） | 见 §1.4 精确 JSON | [高] server/discover |
| M3 | **`resultType`** | 每个 `result` 顶层必须有 `resultType`（string）。核心值：`"complete"`、`"input_required"`；扩展可加（如 Tasks 的 `"task"`）。客户端遇未知值 MUST 视为非法；**legacy 服务端缺省时客户端按 `"complete"` 处理** | [高] basic/index#resulttype |
| M4 | **错误码分区** | `-32020 HeaderMismatch`、`-32021 MissingRequiredClientCapability`（`data.requiredCapabilities` 列出缺失能力）、`-32022 UnsupportedProtocolVersion`（`data.supported` 列表 + `data.requested`）；不得再发 `-32002`（资源未找到改 `-32602`）与 `-32042`（仅 2025-11-25） | [高] basic/index#error-codes、versioning |
| M5 | **`serverInfo` 回传** | 每个 result 的 `result._meta["io.modelcontextprotocol/serverInfo"] = {name, version}`（SHOULD）；`server/discover` 同 | [高] basic/index#per-response-protocol-fields |
| M6 | **能力不可假设** | 只用客户端 `clientCapabilities` 声明过的能力；不足 → `-32021` + `data.requiredCapabilities` | [高] basic/index |
| M7 | **取消** | stdio 仍用 `notifications/cancelled`；HTTP modern 用"关闭该请求响应流"表达取消 | [高] Inspector protocol-eras |
| M8 | **删除 `ping` / `logging/setLevel` / `notifications/roots/list_changed`** | modern 路径收到这些方法 → JSON-RPC `-32601 Method not found`（或明确错误）；modern 能力里**不要**声明 `logging` | [高] changelog |
| M9 | **每请求日志** | 不要主动发 `notifications/message`；仅当请求 `_meta.logLevel` 存在才允许；stdio 日志写 stderr | [高] changelog / logging（2026-07-28） |
| M10 | **列表缓存字段** | `tools/list`、`prompts/list`、`resources/list`、`resources/read`、`resources/templates/list` 结果需带 `ttlMs`（毫秒，整数）与 `cacheScope`（`"public"`/`"private"`） | [高] changelog 次要变更 #5 |
| M11 | **确定性顺序** | `tools/list` 等 SHOULD 按稳定顺序返回（建议按 name 排序），利于客户端缓存与 prompt cache | [高] tools 页 |
| M12 | **`resources/subscribe` 移除** | 订阅统一走 `subscriptions/listen` 长连接 POST 响应流；不实现可保持 `listChanged=false` 并依赖客户端轮询 | [高] changelog |
| M13 | **MRTR（审批）** | `tools/call` 可返回 `resultType:"input_required"`，含 `inputRequests`（如 `elicitation/create` form）与 `requestState`；客户端用**新的 `id`** 重试原调用并带 `inputResponses`/`requestState` | [高] tools 页 + elicitation 页 |
| M14 | **form elicitation 约束** | `requestedSchema` 仅扁平对象 + 原始类型；**MUST NOT** 收集密码/API key/token；敏感信息用 URL 模式（本机 stdio 场景可不用 URL 模式） | [高] elicitation 页 |
| M15 | **stdio 传输细节** | 现代请求仍是 newline-delimited JSON-RPC；`_meta` 在 `params` 内；无 HTTP header 相关（`Mcp-Protocol-Version`/`x-mcp-header` 仅 HTTP） | [高/中] |
| M16 | **`server/discover` 的 `ttlMs/cacheScope`** | DiscoverResult 支持缓存（示例含 `ttlMs: 3600000, cacheScope:"public"`） | [高] server/discover |

### 1.4 `server/discover` 精确形状（可直接抄）

请求（无业务参数，只有标准 `_meta`）：

```jsonc
{
  "jsonrpc": "2.0",
  "id": "discover-1",
  "method": "server/discover",
  "params": {
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientInfo": { "name": "ExampleClient", "version": "1.0.0" },
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}
```

响应：

```jsonc
{
  "jsonrpc": "2.0",
  "id": "discover-1",
  "result": {
    "resultType": "complete",
    "supportedVersions": ["2026-07-28"],
    "capabilities": {
      "tools": {},
      "resources": {},
      "prompts": {},
      // 仅当实现 Tasks 扩展（P1）时添加：
      "extensions": { "io.modelcontextprotocol/tasks": {} }
    },
    "_meta": {
      "io.modelcontextprotocol/serverInfo": { "name": "rdm", "version": "0.1.0" }
    },
    "instructions": "RDM download manager. Flow: rdm_probe → rdm_plan_download → (approval) rdm_apply_plan → rdm_wait_task → rdm_verify_file. Never requests credentials.",
    "ttlMs": 3600000,
    "cacheScope": "public"
  }
}
```

要点：
- 请求本身**必须**带 `_meta`（协议版本 + 客户端能力），否则按 M1 返回 `-32602`；
- `supportedVersions` 只列**现代**版本（建议 `["2026-07-28"]`；是否列 legacy 版本规范未明确 → **[中]，建议不列**，legacy 由 `initialize` 选择）；
- `instructions` 是 agent 的"使用说明"，RDM 应写 5–10 行决策树 + 安全边界；
- 服务端 identity 放 `result._meta` 而非 result 顶层。

### 1.5 `resultType` 枚举、位置与兼容规则

| 值 | 出现位置 | 含义 | RDM 何时用 |
|---|---|---|---|
| `"complete"` | 任意 `result.resultType` | 普通成功结果 | 所有普通响应（modern 路径） |
| `"input_required"` | `result.resultType` | 请求未完成，需要客户端补充输入；result 内含 `InputRequiredResult`（`inputRequests` / `requestState`） | 审批：`rdm_apply_plan` 或 `rdm_download` 需要人工确认时（modern）；legacy 走 `needs_approval` 错误 + token |
| `"task"`（扩展） | `result.resultType` | 长任务已创建（`CreateTaskResult`），客户端改轮询 `tasks/get` | 若采用 Tasks 扩展（Phase 3，P1） |

规则（规范原文要点）：
- 每个 `result` **MUST** 有 `resultType`；扩展只能用**已声明**的能力引入新值；
- 未知值客户端 MUST 视为非法；
- **legacy 服务端（≤2025-11-25）不带该字段 → 客户端 MUST 按 `"complete"` 处理**。因此 RDM 的 legacy 路径**不要**加 `resultType`（避免旧客户端 schema 校验失败；规范未禁止但保守起见按代际区分）**[中]**。

### 1.6 双代际入口分派（伪代码，Phase 1 就预留）

```
on stdio line:
  msg = parse_json(line)
  if msg.method == "initialize":                     # legacy
      legacy_state = handle_initialize(msg); emit(result); return
  if msg.method == "server/discover":                # modern discovery
      if modern_enabled: handle_discover(msg)
      else: error(-32601 or -32022 with supported=[legacy...])   # Phase 1: 明确失败
      return
  if msg.method == "notifications/initialized":      # legacy
      legacy_state.mark_initialized(); return
  if msg.params?._meta?["io.modelcontextprotocol/protocolVersion"] exists:   # modern
      if modern_enabled and version supported: handle_modern(msg)
      else: error(-32022, data={supported:[...], requested: v})
      return
  # 无 _meta、非 initialize → 按 legacy 语义处理（这才能让旧客户端工作）
  handle_legacy(msg)
```

旧客户端 fallback 结论（直接回答 Lead 的问题）：
- **legacy 客户端不能连接"只实现 modern"的 server**（规范矩阵：Legacy × Modern = 失败；legacy 无前向机制）。所以 **RDM 必须保留 legacy 路径**，dual-era 是唯一可行路线。
- modern 客户端连 legacy server 会失败 → 双代际客户端会先探测 `server/discover`，识别到"非现代错误"后 fallback 到 `initialize`。RDM 作为**服务端**只需：`initialize` 存在即被 legacy 客户端使用；`server/discover` 存在即被 dual-era 客户端识别为现代。**两个方法必须能同时响应，不能二选一。**
- 若未来 RDM 想出"modern-only"构建：对 `initialize` 的响应错误中**SHOULD 写明支持的版本**（规范建议，legacy 客户端只能展示该错误）。

### 1.7 `2025-11-25` vs `2025-06-18` 差异（legacy 桶内）

> 来源：https://modelcontextprotocol.io/specification/2025-11-25/changelog（[高]）。
> 结论：**2025-11-25 = 2025-06-18 + 可选增强**，对 stdio 下载器几乎无强制影响。Phase 1 只声明 2025-06-18 即可；是否接受 2025-11-25 客户端见下。

| 变化 | 对 RDM 的影响 | 建议 |
|---|---|---|
| **URL 模式 elicitation**（`mode:"url"`；新增 `-32042 URLElicitationRequired`，仅 2025-11-25） | 本机 stdio 用不到；form 模式仍可用 | Phase 1 不实现；**modern 禁发 `-32042`**（规范明确 2026 实现 MUST NOT emit） |
| **experimental tasks**（`capabilities.tasks`、`tasks/list`、阻塞 `tasks/result`） | 与 2026 官方 Tasks 扩展**不同且已被取代** | **不实现**；RDM 用自有 task 工具（legacy）+ Tasks 扩展（modern，P1） |
| JSON Schema 2020-12 成为默认方言 | RDM 的 `inputSchema`/`outputSchema` 已用 2020-12 子集，向前兼容 | 保持；不要用只有 draft-07 支持的写法 |
| icons 元数据（tool/resource/prompt/implementation） | 可选，GUI 可用 | 可忽略 |
| `Implementation.description` 字段 | 可选展示 | 可忽略 |
| OIDC Discovery / Client ID Metadata Documents / 增量 scope（授权相关） | 仅 HTTP 远程场景 | stdio 阶段 N/A |
| 输入校验错误应作为 tool execution error（SEP-1303） | 与 RDM 契约一致 | 保持 `isError:true` + hint |
| `scopes`/采样工具等 | N/A | 忽略 |

**建议**：Phase 2 时把 legacy `supportedVersions` 扩为 `["2025-11-25","2025-06-18"]`，仅当你能通过 2025-11-25 客户端测试；否则保持只声明 2025-06-18 **[中]**。

### 1.8 测试矩阵（照这张表验收）

| 测试 | legacy 2025-06-18 | modern 2026-07-28 | 工具 |
|---|---|---|---|
| `initialize` 握手 + echo | ✅ 必过 | —（应返回错误，或按 dual-era 策略走 legacy） | `mcp_e2e` legacy 段；MCP Inspector legacy |
| `ping` | ✅ 返回 `{}` | ❌ 应 `-32601` | Inspector modern |
| `logging/setLevel` | ✅ 可选 | ❌ 应 `-32601`；且不发未 opt-in 的 message | Inspector modern |
| `server/discover` | —（未定义） | ✅ 返回 DiscoverResult（含 supportedVersions/capabilities/serverInfo/instructions/ttlMs） | Inspector modern；`mcp_e2e` 新段 |
| 缺 `_meta.protocolVersion` | 正常（legacy 无此要求） | ✅ `-32602` | 自写负例 |
| 缺 `clientCapabilities` | 正常 | ✅ `-32602` | 自写负例 |
| 未知版本 `1900-01-01` | — | ✅ `-32022` + `data.supported` | 自写负例 |
| `resultType` 存在性 | 不添加 | ✅ 每个 result 有 `"complete"` | `mcp_e2e` modern 断言 |
| `subtasks`/`tasks/get` | — | 可选（P1，Tasks 扩展） | ext-tasks 测试 |
| 审批 | `needs_approval` 错误 + token | `input_required` + MRTR 重试 | 双套 e2e |
| 工具列表顺序 | 稳定（建议） | 稳定 + `ttlMs/cacheScope` | 连续两次 list 比对 |

MCP Inspector 用 `--protocol-era legacy|auto|modern` 分别测（来源：https://modelcontextprotocol.io/docs/2026-07-28/tools/inspector/protocol-eras）；`scripts/mcp_e2e.mjs` 需拆成 `--era legacy|modern|both`（Phase 2 验收的一部分）。

---

## 2. 安全默认反转（具体建议）

> 现状：契约 §4 默认 `allowed_hosts=["*"]`、`allow_private_hosts=true`。对一个被 agent 驱动的任意 URL 下载器，这是**内网探测/SSRF/云元数据读取**的默认开启状态。以下为 v2 建议值，需与 Lead 确认后写入契约。

### 2.1 `policy.json` 建议默认值（安全默认）

```jsonc
{
  "schema_version": "rdm.agent/v1",
  "allowed_hosts": [],                    // 空 = 全部拒绝；用户用 GUI/CLI 显式添加（支持 "*.example.com"）
  "allow_any_host": false,               // 若用户坚持要 "*"，必须显式打开并审计
  "allowed_schemes": ["https"],          // http 需显式加入；禁 file/data/ftp/gopher/ws
  "allowed_ports": [443],                // http 启用时再加 80；非默认端口显式列
  "allowed_dirs": ["%USERPROFILE%\\Downloads\\agent"],
  "allow_private_hosts": false,          // 127/8, ::1, 10/8, 172.16/12, 192.168/16, 100.64/10, 169.254/16, fc00::/7, fe80::/10 ...
  "deny_metadata_endpoints": true,       // 见 §2.2 清单
  "dns_rebind_protection": true,         // 解析后校验 + 连接固定到校验过的 IP
  "revalidate_redirects": true,          // 每一跳重新做 host/scheme/port/IP 校验
  "max_redirects": 5,
  "max_bytes_per_task": 2147483648,      // 2 GiB（原 10 GiB 太大）
  "require_approval_over_bytes": 536870912,  // 512 MiB 起审批
  "max_unknown_length_bytes": 2147483648,    // 未知 Content-Length 的流式硬上限
  "max_total_bytes_per_hour": 10737418240,   // 全局配额，防"多任务小额"绕过
  "max_concurrent_tasks": 2,             // 原 3；加 per-agent 配额
  "allow_overwrite": false,
  "approval_ttl_minutes": 10,
  "agent_identity": "local"              // 预留：per-agent 任务所有权/配额
}
```

同时建议新增结构化错误码：`budget_exceeded`（retryable=false，`hint` 指向提高 policy 或换小文件）与 `host_not_allowed`（含 `details.host`/`details.reason`），避免 agent 把安全拒绝误判为可重试。

### 2.2 URL 校验算法（逐跳执行，伪代码）

```
validate(url, redirect_depth=0):
  u = parse(url)
  require u.scheme in policy.allowed_schemes
  require u.port in allowed_ports (or default)
  reject u.username/u.password present          # 防 userinfo 混淆
  host = u.host
  if is_ip_literal(host):
      ip = parse(host); require is_public(ip); require not is_metadata(ip)
  else:
      addrs = resolve_a_aaaa(host)               # 必须全部 A/AAAA
      require addrs 非空
      for ip in addrs:
          require is_public(ip) and not is_metadata(ip)
      pin = choose(addrs)                        # 实际连接固定到该 IP（Host/SNI 保持 hostname）
      # 若客户端不支持 resolve-pin：则每次连接前重新解析并校验，且要求两次解析结果一致
  # 重定向时：follow 手动模式，Location -> validate(target, depth+1)；跨 origin 时丢弃 Cookie/Authorization
  # 代理场景：显式 proxy 配置视为 policy 的一部分（代理会二次解析，DNS 校验需在代理侧同样成立，文档写明）
```

**必须拒绝/特殊处理的目标（[中高]，按云厂商公开文档核对后固化）**：

| 类别 | 范围/地址 | 说明 |
|---|---|---|
| IPv4 loopback/私有/保留 | `127.0.0.0/8`、`10/8`、`172.16/12`、`192.168/16`、`100.64/10`（CGNAT）、`0.0.0.0/8`、`192.0.0.0/24` 部分、`198.18/15`、`240/4` | 内网探测 |
| IPv6 | `::1`、`fc00::/7`（ULA）、`fe80::/10`（link-local）、`::ffff:0:0/96` 映射（解映射后再判） | 同上 |
| 链路本地/元数据 | `169.254.0.0/16`（含 `169.254.169.254`）、AWS IPv6 `fd00:ec2::254`、ECS `169.254.170.2`、阿里云 `100.100.100.200`、Oracle `192.0.0.192`、GCP `metadata.google.internal` | 云凭据窃取面 |
| 特殊主机名 | `localhost`、`*.localhost`、`*.local`、`*.internal`（视 policy）、单标签主机名 | mDNS/内网 |
| 协议 | `file:`、`data:`、`ftp:`、`gopher:`、`ws:`、`javascript:` | 非下载语义 |

### 2.3 未知 Content-Length 与预算强制

- probe 阶段：`Content-Length` 存在且 > `max_bytes_per_task` → 直接 `policy_denied`（或 `needs_approval` 视阈值）。
- 未知长度 / chunked：引擎按已写字节计数，达到 `max_unknown_length_bytes`（或剩余配额，取小）**立即中止**并保留 `.part` 状态，返回 `budget_exceeded`；不得"下完再说"。
- `Content-Length` 撒谎（实际 > 声明）：以实际写入字节为准，超声明值 + 容差即中止。
- 预检磁盘：`free_bytes < max_bytes_per_task * 1.05` 时拒绝并提示；下载中每 N MiB 复查。
- 全局配额：`max_total_bytes_per_hour` 在 TaskStore 侧累计（跨 agent），防止多任务绕过单任务上限。

### 2.4 secret handle API 形状（agent 不能传明文凭据）

**设计目标**：Cookie/Authorization 永不进入 LLM 上下文、tool result、日志、transcript；agent 只能"引用"用户预先授权的凭据。

**工具参数（`rdm_download` / `rdm_plan_download`）**：

```jsonc
{
  "url": "https://example.com/private.iso",
  "dest_dir": "C:\\Users\\me\\Downloads\\agent",
  "auth_ref": "sec_01J8Z3K4N5Q6R7S8T9V0W1X2Y3",   // 可选；句柄而非明文
  "headers": { "Accept-Language": "zh-CN" },        // 仅非敏感头；保留名拒绝
  "idempotency_key": "run-42"
}
```

- **删除** `cookie` 字段；`headers` 中禁止 `cookie` / `authorization` / `proxy-authorization`（返回 `invalid_request` 并提示改用 `auth_ref`）。
- `auth_ref` 是不透明句柄（`sec_<ULID>`），**不携带任何凭据内容**；无效/过期返回 `needs_approval` 或 `invalid_request`（`hint`："请让用户通过 GUI/CLI 重新授权"）。
- 任务输出只回显 `auth_ref`（脱敏后），永不回显解析后的头。

**secret 存储与生命周期（建议）**：

| 项 | 建议 |
|---|---|
| 位置 | `%LOCALAPPDATA%\RDM\agent\secrets.json`（ACL 仅当前用户；Windows 用 DPAPI `CryptProtectData` 加密；未来接 OS keychain） |
| 记录 | `{ id, kind: "cookie"|"authorization"|"header_set", value_encrypted, allowed_hosts: ["*.example.com"], expires_at, created_by, created_at, last_used_at, one_task_only }` |
| 创建 | **仅人类路径**：GUI"授权给 agent"对话框，或 `rdm secret add --name X --type cookie` 从 **TTY/stdin** 读值（禁止命令行参数、禁止 MCP 工具、禁止 agent 调用） |
| 查询 | `rdm secret list` 只返回元数据（name/kind/hosts/expiry），**永不返回 value**；MCP 不暴露 secret 工具 |
| 使用 | 引擎在发请求前解析并注入；`allowed_hosts` 不匹配 → `policy_denied`（防止用 GitHub 凭据访问攻击者域名） |
| 过期 | 默认不自动过期（用户管理），但支持 `--ttl 1h` 与 `one_task_only`；任务结束即删（若标记） |
| 浏览器交接 | **最优路径**：Firefox native host 把 Cookie 从扩展内存直接交给 GUI 进程内存，仅当次下载使用、不落盘、不经过 MCP/LLM；agent 只见 `auth_ref: "browser-session"`（或干脆不见） |

**为什么这样设计**：MCP 规范明确 form elicitation **MUST NOT** 收集密码/API key/token（https://modelcontextprotocol.io/specification/2026-07-28/client/elicitation），凭据进入 tool 参数必然进入 LLM 上下文——secret handle 是唯一既保留"agent 能下载登录墙资源"又不泄漏的做法。

### 2.5 审批 token 的人类参与方式（防自批）

| 环节 | 设计 |
|---|---|
| 触发 | `rdm_download`/`rdm_plan_download` 命中 `require_approval_over_bytes` 或策略要求 → 返回 `needs_approval`（legacy）或 `input_required`（modern/MRTR），**不执行下载** |
| 计划 | 服务端生成 `plan_id` + 规范化 `plan_json`（url/final_host/size/dest_dir/connections/sha256/estimated_time）+ `plan_hash`（SHA-256） |
| **签发（只有人类能签）** | ① GUI：弹"agent 请求下载"对话框，显示 host、大小、目标目录、来源 agent；用户点"批准"→ 签发 token；② CLI：`rdm approve <plan_id>` **必须 TTY**（非 TTY 直接拒绝），打印计划并要求输入 `yes` 确认 |
| token | `apr_<base64url(HMAC-SHA256(key, plan_hash|expires|nonce))>`；**单次使用**、10 分钟过期、绑定 `plan_hash` + policy 版本；服务端记录 `approvals/<plan_id>.json` |
| 消费 | `rdm_apply_plan(plan_id, approval_token)`：校验 token 签名/过期/plan_hash/未使用 → 立即标记 used → 启动下载 |
| 防自批 | - MCP **不提供**"批准"工具；agent 无法获得 token，只能等人类从 GUI/CLI 给<br>- token 不可由 agent 通过文件系统推断（HMAC key 在 DPAPI/keystore）<br>- `plan_hash` 绑定 URL/目标/大小 → 改 URL 后 token 失效<br>- 审计记录：`approver`（本地 SID/用户名）、签发渠道（gui/cli）、plan_hash、token_hash、时间 |
| 远程场景 | 若未来做 remote MCP：审批必须发生在 **client host UI**（MRTR/URL 模式 elicitation），服务端不得根据 agent 声称"用户已批准"自行放行；多租户下 token 绑定 OAuth `sub` |

### 2.6 安全验收测试（新增到 `mcp_e2e` 或独立测试）

1. `auth_ref` 不存在/过期 → 结构化错误，**不含**任何 secret 内容；
2. 工具参数 `cookie` → `invalid_request`（schema 拒绝）；`headers.Authorization` → `invalid_request`；
3. `http://127.0.0.1:port/`、`http://169.254.169.254/`、`metadata.google.internal`、IPv6 `::1`、`fd00:ec2::254` → `policy_denied`，且 fixture/监听器收到 **0 请求**；
4. 公网域名解析到私有 IP（fixture 域名 + hosts 注入或测试 resolver）→ 拒绝；DNS 两次解析变化（rebinding 模拟）→ 拒绝；
5. 302 跳到内网/换 scheme/换端口 → 每跳复检并拒绝；跨 origin 重定向不携带 `auth_ref` 对应凭据；
6. chunked 无限流 → 达到上限中止，任务状态 `failed: budget_exceeded`，`.part` 保留；
7. 审批：无 token 调 `apply_plan` → `needs_approval`；复用 token → 拒绝；改 plan 后旧 token → 拒绝；非 TTY `rdm approve` → 拒绝；
8. token 签发/使用审计事件存在且脱敏。

---

## 3. 5 个 1–2 天可实现的 P1 项（含验收标准）

> 从 `agent-tool-design.md` §5.2 的 14 个 P1 中挑选：**依赖已就绪、边界清晰、1–2 人日可交付**。第 1 项与 dual-era 绑定，其余可在 legacy 阶段直接做。

### P1-A `server/discover` + 能力缓存字段（1 天）— dual-era 里程碑内
- **范围**：实现 §1.4 的 `server/discover`（含 `ttlMs/cacheScope`、`serverInfo`、`instructions`）；`tools/list`/`resources/list`/`prompts/list`/`read` 增加 `ttlMs`+`cacheScope`；列表按 name 排序。
- **不做**：Tasks 扩展、HTTP 传输。
- **验收**：
  - `mcp_e2e --era modern` 通过 discover 段；`initialize` + discover 在同一进程都能响应；
  - 缺 `_meta` → `-32602`；未知版本 → `-32022` 且 `data.supported` 含 `2026-07-28`；
  - 连续两次 `tools/list` 字节级顺序一致；结果含 `ttlMs`/`cacheScope`；
  - Inspector legacy 模式全部旧测试仍绿（无回归）。

### P1-B token 高效分页 + summary/detail 双视图（1 天）
- **范围**：`rdm_list_tasks` 默认 `limit=20`、`max=100`、`cursor` 稳定；只返回 `TaskSummary`（id/state/file_name/size/downloaded/updated_at）；`rdm_get_task` 的 `include_events` 默认 0、`events_limit` 上限 200；`has_more`/`next_cursor` 显式。
- **验收**：
  - 造 100 个任务：一页响应 < 8 KB（UTF-8 字节，断言）；分页游标往返无重复/遗漏；
  - 超限时返回 `truncated:true` + `next_cursor`，**不静默截断**；
  - `get_task` 默认输出不含事件数组；`include_events=5` 时只含最近 5 条且 `seq` 单调；
  - agent 侧 token 估算（可选）下降 ≥ 80% vs 无分页。

### P1-C `rdm report` 脱敏诊断包（2 天）
- **范围**：`rdm report [--out PATH] [--dry-run]`：收集版本/OS/策略（脱敏）/最近 N 条事件/doctor 摘要/任务摘要，打包 zip（或 tar.gz）；默认不包含 URL 查询串、cookie、authorization、绝对用户路径（可用 `%USERPROFILE%` 替换）。
- **验收**：
  - fixture 失败任务后生成报告 < 2 MB；
  - 用测试公开的 `cookie`/`authorization`/`?token=...` 字符串扫描整个包 → 0 命中；
  - `--dry-run` 列出将收集的文件与大小，不写盘；
  - 报告含 `rdm_version`/OS/错误码/最近事件，足以定位一次 403/429 失败。

### P1-D `explain-why-slow` 网络诊断（2 天）
- **范围**：对目标 URL 做小样本（≤4 MiB）诊断：DNS/TLS 握手时延、probe 结果（Range/长度/ETag/http_version）、1 连接 vs N 连接分块吞吐、每连接限速迹象、h2 单连接提示、代理/重定向链；输出结构化 `diagnosis` + 人类摘要。
- **验收**：
  - fixture 每连接限速 1 MiB/s：结论含 `single_connection_limited`（且 8 连接实测吞吐 ≈ N×）；
  - fixture `without_ranges`：结论含 `range_unsupported`，且不产生重试风暴；
  - 健康链路：结论为 `no_bottleneck_detected`（诚实，不硬说"慢"）；
  - 总下载量 ≤ 4 MiB + 探测开销；输出 schema 稳定、`warnings` 可机读。

### P1-E `rdm replay` 失败任务重放（2 天）
- **范围**：基于 TaskStore 的请求快照（脱敏后）重放同一任务到隔离目录（默认新目录，`--same-dir` 才覆盖）；支持 `--force`（已完成任务）、`--idempotency-key` 覆盖；输出与首次相同的 `TaskRecord` 结构。
- **验收**：
  - fixture 先 500 后成功：首次失败、修复后 `replay` 成功且 SHA-256 一致；
  - replay 的 task 是**新 task_id**，事件 journal 独立；
  - 请求快照中无 cookie/authorization；`--force` 对已完成任务必须显式；
  - 失败时退出码与首次一致（0/2/3 语义）。

> 近失（未入选，因依赖较大）：多 agent lease/ownership（2–3 天，需 TaskStore 加 owner/CAS）、镜像回退（3+ 天，引擎改动）、Tasks 扩展（3–5 天，需客户端矩阵验证）。

---

## 4. 参考链接（规范）

- 2026-07-28 规范首页：https://modelcontextprotocol.io/specification/2026-07-28/
- 变更日志（删除项全集）：https://modelcontextprotocol.io/specification/2026-07-28/changelog
- 版本与兼容（dual-era/矩阵/`-32022`）：https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning
- 基础协议与 `_meta` 必填表 / `resultType` / 错误码：https://modelcontextprotocol.io/specification/2026-07-28/basic/index
- `server/discover`：https://modelcontextprotocol.io/specification/2026-07-28/server/discover
- Tools（annotations / structuredContent / MRTR / isError）：https://modelcontextprotocol.io/specification/2026-07-28/server/tools
- Elicitation（form/url、三态、禁密钥）：https://modelcontextprotocol.io/specification/2026-07-28/client/elicitation
- MRTR：https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr
- Subscriptions（`subscriptions/listen`）：https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/subscriptions
- stdio 传输（含 backward compatibility）：https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio
- Protocol eras（Inspector 验收工具）：https://modelcontextprotocol.io/docs/2026-07-28/tools/inspector/protocol-eras
- 2025-11-25 变更：https://modelcontextprotocol.io/specification/2025-11-25/changelog
- 2025-06-18 生命周期（legacy initialize）：https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle
- Tasks 扩展：https://modelcontextprotocol.io/extensions/tasks/overview ｜ https://github.com/modelcontextprotocol/ext-tasks
- 安全最佳实践：https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices
- 本地 server 安全：https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/local-server-security

---

## 5. 未获取 / 待核

| 项 | 状态 |
|---|---|
| `initialize` 在 2025-06-18 的**逐字段** schema（本次按稳定通识写，未逐页抓取） | **未获取全文**；字段表 [高] 置信度但建议实现时对一遍 lifecycle 页 |
| 2025-11-25 的 `initialize` 是否与 2025-06-18 完全一致 | **未核**（差异表来自 changelog，未列 initialize 变化 → 推断一致 [中]） |
| `supportedVersions` 是否应包含 legacy 版本 | **规范未明确** [中]；建议只列现代版本 |
| legacy 响应是否允许携带 `resultType` | **规范未明确** [中]；建议按代际区分（legacy 不加） |
| Tasks 扩展 `CreateTaskResult` 的精确 JSON schema | 概述页已覆盖字段名，**逐字段 schema 未获取**（ext-tasks repo 为准） |
| 云元数据 IP 全集 | [中高]；按各厂商文档核对后固化（AWS/GCP/Azure/阿里云/Oracle） |
| `rdm-mcp` 当前实现进度 | 03:18 复核仍为 205 B 桩；本文件按"待实现"编写 |
