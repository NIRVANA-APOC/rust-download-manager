# Firefox 扩展与下载接管调研

> 调研日期：**2026-10-19**（所有网页结论以当天可访问的 MDN / Firefox Extension Workshop /
> Chromium 文档为准；Firefox 版本号按当天稳定版级别理解，具体行为仍以实际浏览器为准）。
>
> 目标：为 RDM 的 Firefox 集成（`extension/firefox/` + `crates/rdm-native/`）确定可行方案、
> 明确限制与风险。本文只讨论"浏览器侧如何把下载交给本机下载器"，不重复下载引擎设计
> （见 `docs/architecture.md`）。

## 0. 结论摘要（TL;DR）

1. Firefox 的 Manifest V3 已 GA（Firefox 109），但**它不是 Chrome MV3 的镜像**：
   - `background.service_worker` **不支持**（[MDN background][mdn-background]，Firefox bug 1573659），
     MV3 后台仍用 `background.scripts` 事件页；
   - MV3 下 `webRequest` **只能观察、不能阻塞**（`webRequestBlocking` 仅策略安装扩展可用，
     [MDN permissions][mdn-permissions]），因此**自动接管下载只能采用
     "`downloads.onCreated` 后取消"的竞态方案**；
   - Firefox **没有** Chromium 的 `downloads.onDeterminingFilename`（[MDN downloads][mdn-downloads]），
     无法在文件命名阶段更早介入。
2. 原生消息（Native Messaging）是唯一可靠的"浏览器 → 本机进程"通道：
   `allowed_extensions` 限定扩展 ID、Windows 用 HKCU 注册表指向 host manifest、
   4 字节本机字节序长度前缀 + UTF-8 JSON（[MDN Native messaging][mdn-native]）。
   Firefox 会在扩展断开时把宿主进程所在 **Job 对象整个杀掉**，宿主若想让 GUI 活下来，
   必须用 `CREATE_BREAKAWAY_FROM_JOB` 启动子进程（同上）。
3. 对"自动接管"需求，可选的三种方案对比如下（详见 §3）：
   | 方案 | MV3 可用 | 自动 | 可靠性 | 结论 |
   | --- | --- | --- | --- | --- |
   | `downloads.onCreated` + cancel/erase + native | ✅ | ✅ | 中高（小文件竞态） | **本仓库采用** |
   | `webRequest` 阻塞拦截 | ❌（策略扩展例外） | ✅ | — | MV3 不可用 |
   | 右键菜单/手动转交 | ✅ | ❌ | 最高 | 作为补充功能 |
4. 长期分发必须经 AMO 签名；MV3 必须显式 `gecko.id`，且 2025-11-03 起新提交还必须声明
   `browser_specific_settings.gecko.data_collection_permissions`
   （[MDN browser_specific_settings][mdn-bss]）。临时载入（about:debugging）只适合开发。
5. 隐私边界：Cookie 只在"扩展 → 宿主 → GUI"内存链路中传递、日志脱敏；GUI 长时间离线且
   无法启动时，宿主落盘 spool 前会**剥离凭据**（清空 `cookie`、移除
   `authorization`/`proxy-authorization`/`cookie` 头），并在 `queued` 响应里返回
   `credentials_stripped: true`；代价是排队的登录态下载可能失败（详见 §5.4）。
6. 真实组件链路（`rdm-native` → `rdm-gui` → 引擎）已完成过 8 MiB fixture 的端到端验证：
   enqueued/download_id、IPC status、downloads.json completed、SHA-256 与独立参考一致、
   queue 为空、日志无 cookie 值（§6.3）。扩展在真实 Firefox 中的自动接管行为仍需人工验证。

## 1. Firefox Manifest V3 现状（截至 2026-10-19）

### 1.1 后台页：`background.scripts` 而不是 service worker

- MV3 在 **Firefox 109** 正式可用（Firefox 101 起为开发者预览）
  （[Extension Workshop: MV3 migration][ew-mv3]）。
- Firefox **不支持** `background.service_worker`，见
  [MDN background][mdn-background] 与 Bugzilla 1573659；MV3 后台是**非持久事件页**
  （event page）。扩展应：
  - 在**顶层同步**注册所有事件监听器（事件页被唤醒后不会重放错过的事件）；
  - 状态写 `storage`，定时任务用 `alarms`（`setTimeout` 在事件页卸载后丢失）；
  - `contextMenus.create` 放在 `runtime.onInstalled`/`onStartup`，点击用
    `contextMenus.onClicked`，不能再用 `onclick` 参数
    （[MV3 migration guide][ew-mv3]）。
- 跨浏览器兼容的写法是**同时**写 `background.scripts` 与 `background.service_worker`：
  Firefox 用 scripts，Chrome 121+ 在 MV3 中忽略 scripts 只用 service_worker
  （[MDN background][mdn-background]）。本仓库只面向 Firefox，故只写 `scripts`。
- Firefox 121 之前，如果 manifest 同时写了 `service_worker` 会**根本不启动**后台页
  （Firefox bug 1860304），这也是只写 scripts 的原因之一。

### 1.2 `webRequest` 在 MV3 的阻塞能力

- `webRequest` API 本身在 Firefox MV3 中可用，但**只有观察能力**：
  `onBeforeRequest`/`onBeforeSendHeaders`/`onHeadersReceived` 等注册时
  **不能带 `"blocking"`**。`webRequestBlocking` 权限在 MV3 下仅对**策略安装**
  （`ExtensionInstallForcelist` 等企业策略）的扩展开放（[MDN permissions][mdn-permissions]；
  社区讨论见 [uBlock-issues #3561][ublock-mv3]）。
- Firefox 对 declarativeNetRequest（DNR）的支持也不完整，且 DNR 只能在浏览器内部
  **重定向/阻断请求**，无法把请求交给外部程序。
- 推论：浏览器内不存在"请求还没开始时把它转交外部下载器"的 MV3 API。要自动接管，
  只能等浏览器创建下载项后再取消（§3.1）。

### 1.3 `downloads` API 的行为与竞态

- `downloads.onCreated` 在**浏览器开始下载时**触发（`DownloadItem` 携带 `id`、`url`、
  `filename`、`state`、`totalBytes` 等；`totalBytes` 在早期可能为 `-1` 表示未知）
  （[MDN downloads][mdn-downloads]）。
- `downloads.cancel(id)`：**只对仍在进行的下载有效**；如果下载已完成/已中断，Promise 会
  reject（[MDN downloads.cancel][mdn-cancel]）。因此 `onCreated` 里 cancel 存在"小文件
  已经下完"的竞态。
- `downloads.erase(query)`：从下载历史里抹掉记录，但 **Firefox 不删除磁盘文件**
  （[MDN downloads.erase][mdn-erase]）；要删文件必须**先** `downloads.removeFile(id)`
  再 erase（顺序反了 removeFile 会报找不到文件）。Firefox 支持 `removeFile`
  （[MDN downloads.removeFile][mdn-removefile]，Firefox 48+）。
  注意：**Chromium 的 `chrome.downloads.erase` 语义不同**——对已完成的下载它会删除文件
  （[Chrome downloads][chrome-downloads]），跨浏览器实现不能照抄。
- Firefox **没有** `downloads.onDeterminingFilename`（该事件是 Chromium 扩展点）；
  MDN 的 downloads API 事件只有 `onChanged`/`onCreated`/`onErased`
  （[MDN downloads][mdn-downloads]）。所以无法在"确定文件名"阶段提前取消。
- 下载状态变化的可靠观察点是 `downloads.onChanged`（`state` 字段），
  cancel 后可以用 `downloads.search({id})` 轮询到终态再 erase，避免取消/抹除竞争。

### 1.4 `cookies` API 权限

- 使用 `browser.cookies` 需要 manifest 里的 `"cookies"` API 权限，**并且**对目标 URL
  有 host 权限——MV3 中 host 权限写在 `host_permissions`
  （[MDN host_permissions][mdn-hostperm]）。
- `cookies.getAll({url})` 返回该 URL 可见的 Cookie（含 HttpOnly），足以拼出 `Cookie`
  请求头；`buildCookieHeader` 需过滤 CR/LF 并限制总长度（本仓库实现 16 KiB 上限）。
- 另一种来源是 `webRequest.onBeforeSendHeaders` 的 `requestHeaders`，但浏览器不保证
  在非阻塞监听中暴露 `Cookie` 头；**结论：以 cookies API 为主，webRequest 观察到的
  Cookie 头仅作为补充**。

### 1.5 Native Messaging 与 host manifest

- 扩展 manifest 需要 `"nativeMessaging"` 权限；Firefox 还要求扩展有**显式 ID**
  （`browser_specific_settings.gecko.id`），因为 host manifest 用它做白名单
  （[MDN Native messaging][mdn-native]）。
- Host manifest（`com.rdm.download.json`）关键字段：

  ```json
  {
    "name": "com.rdm.download",
    "description": "RDM native messaging host",
    "path": "C:\\Users\\<user>\\AppData\\Local\\RDM\\rdm-native.exe",
    "type": "stdio",
    "allowed_extensions": ["rdm@rdm.download"]
  }
  ```

  - `path` 必须是**绝对路径**；
  - Firefox 用 `allowed_extensions`（扩展 ID 列表），Chrome 用
    `allowed_origins`（`chrome-extension://<id>/` 列表）——**跨浏览器不能共用同一份
    manifest**（[MDN Native messaging][mdn-native]）。
- Windows 注册表位置（Firefox）：`HKCU\Software\Mozilla\NativeMessagingHosts\<name>` 或
  `HKLM\Software\Mozilla\NativeMessagingHosts\<name>`，默认值为 host manifest 的绝对路径
  （[MDN Native messaging][mdn-native]）。HKCU 无需管理员权限，适合用户级安装。
  Chrome/Edge 在 `HKCU\Software\Google\Chrome\NativeMessagingHosts\...` 等位置。
- 消息格式：`u32` **本机字节序**（Windows 即小端）长度前缀 + UTF-8 JSON；
  **app → browser 单条消息上限 1 MiB**，browser → app 可达 4 GB
  （[MDN Native messaging][mdn-native]）。本仓库双向都按 1 MiB 防御性限制。
- 生命周期与 Job 对象（关键坑）：Firefox 在 Windows 上把 native host 放进 Job 对象，
  扩展断开/浏览器退出时**杀死整个 Job**；宿主启动的 GUI 若想继续运行，必须
  `CREATE_BREAKAWAY_FROM_JOB`（[MDN Native messaging][mdn-native]）。
  若 Job 不允许 breakaway（`JOB_OBJECT_LIMIT_BREAKAWAY_OK` 未设置），`CreateProcess` 会
  失败，需要退回 `DETACHED_PROCESS` 并接受"随浏览器退出"的风险。本仓库实现三级回退策略。
- 调试：宿主写到 stderr 的内容会被 Firefox 转发到浏览器控制台；但 stdout 是协议通道，
  **任何日志都不能写 stdout**。宿主被 `<name>` 找不到时，浏览器报
  `No such native application <name>`。

### 1.6 `browser_specific_settings.gecko`

- `gecko.id`：MV3 下**签名必需**（AMO 不再自动分配 MV3 扩展 ID），格式为类邮箱字符串或
  GUID（[MDN browser_specific_settings][mdn-bss]）。
- `strict_min_version`：本仓库取 **128.0**，原因：
  - Firefox 127 起 MV3 的 `host_permissions` 才会在安装提示中展示并在安装时授予；
    126 及更早需要在运行时额外请求 host 权限（[MDN host_permissions][mdn-hostperm]、
    [MV3 migration guide][ew-mv3]）；
  - 2025 年 3 月根证书过期事件后，能正常接收签名更新的最低版本为 115 ESR / 128
    （[MDN browser_specific_settings][mdn-bss]）。
- `data_collection_permissions`：**2025-11-03 起** AMO 新提交必须声明数据收集情形；
  不收集任何数据也必须写 `required: ["none"]`（[MDN browser_specific_settings][mdn-bss]）。
  本仓库当前面向临时载入，未写该键以最大化旧版兼容；**上架 AMO 前必须补上**。

### 1.7 AMO 签名、审核与临时载入

- 所有扩展（包括自行分发的 unlisted 版本）都必须经 AMO 签名才能在 release/beta 版
  Firefox 中安装；提交后自动校验，可能进入人工审核，最长可能需要 24 小时以上
  （[Extension Workshop: Signing][ew-signing]）。
- MV3 的 CSP 禁止远程代码、禁止 `unsafe-eval`，`script-src` 只允许 `'self'` /
  `'wasm-unsafe-eval'`（[MV3 migration guide][ew-mv3]）。任何动态 `eval`/远程脚本都会导致
  审核拒绝。
- 临时载入（`about:debugging` → This Firefox → Load Temporary Add-on）：
  - 选择 manifest.json 或 zip，扩展**保留到 Firefox 重启**；
  - 未提供 `gecko.id` 时使用临时 ID，会导致 native messaging 白名单无法匹配；
    本仓库显式声明了 ID，所以临时载入也能连原生宿主；
  - 临时安装不会展示安装期权限请求，行为与签名安装不完全一致
    （[Extension Workshop: Temporary installation][ew-temp]）。
- 自签名（`web-ext sign` / AMO unlisted）适合小范围分发；长期分发建议 AMO listed +
  自动更新。

## 2. 与 Chrome / Edge 的关键差异

| 维度 | Firefox（本仓库目标） | Chrome / Edge（Chromium） |
| --- | --- | --- |
| MV3 后台 | `background.scripts` 事件页；`service_worker` 不支持（[MDN][mdn-background]） | 只支持 `service_worker`；121+ 忽略 MV3 的 `scripts`（[MDN][mdn-background]） |
| 宿主 manifest 白名单 | `allowed_extensions: ["rdm@rdm.download"]` | `allowed_origins: ["chrome-extension://<id>/"]`（[MDN][mdn-native]、[Chrome][chrome-native]） |
| Windows 注册表 | `HKCU\Software\Mozilla\NativeMessagingHosts\<name>` | `HKCU\Software\Google\Chrome\NativeMessagingHosts\<name>`（Edge 类似，键名不同） |
| host 权限授予 | 127+ 安装时展示并授予；126- 需运行时请求（[MDN][mdn-hostperm]） | 安装时授予（MV3） |
| `webRequest` 阻塞 | MV3 仅观察；`webRequestBlocking` 仅策略扩展（[MDN][mdn-permissions]） | MV3 仅观察；blocking 不可用（DNR 替代） |
| `downloads.erase` | 只删历史，不删文件（[MDN][mdn-erase]） | 已完成的下载会删文件（[Chrome][chrome-downloads]） |
| `onDeterminingFilename` | **不支持**（[MDN downloads][mdn-downloads]） | 支持（命名阶段介入） |
| 临时载入 | 重启即失效（[EW][ew-temp]） | `chrome://extensions` 加载已解压扩展，重启仍在 |
| 商店与签名 | AMO 签名（listed/unlisted），MV3 强制 ID、2025-11 起强制数据收集声明（[EW][ew-signing]、[MDN][mdn-bss]） | Chrome Web Store / Edge Add-ons 审核，签名机制不同 |

对本项目的影响：扩展脚本不能假设 Chrome 行为（服务 worker、erase 语义、块级 webRequest），
原生宿主安装器也必须按浏览器分别注册（本仓库只实现 Firefox）。

## 3. 下载接管方案对比

### 3.1 方案 A：`downloads.onCreated` + cancel/erase + native messaging（本仓库采用）

流程（`extension/firefox/background.js`）：

```
webRequest 观察 → 缓存 UA/Referer/Content-Disposition/Content-Length
用户点击/页面触发下载
  └─ downloads.onCreated(item)
       ├─ 设置过滤（开关/大小/扩展名/回退防循环）
       ├─ buildRequest：拼 DownloadRequest（含 cookies API 读到的 Cookie）
       ├─ downloads.cancel(item.id)；轮询到终态
       │    ├─ 已中断 → erase 历史
       │    └─ 已完成（小文件竞态）→ removeFile 删文件 → erase
       ├─ native messaging enqueue → 宿主 → GUI
       └─ 失败（宿主缺失/GUI 起不来）→ browser.downloads.download(url) 回退
```

优点：

- 跨站点通用，不需要每个站点适配；
- 能拿到浏览器已解析的最终 URL、响应头（Content-Disposition/Content-Length）、
  cookies API 的完整 Cookie 与 Referer；
- 不需要在页面注入 content script，权限面相对小。

缺点（都是 API 层硬限制）：

- **不是请求级拦截**：下载已经开始了；极大文件浪费少量字节，极小文件可能已完成；
- Firefox 下载管理器中会短暂出现该条目，被接管后消失；
- 取消/回退/防循环的状态机是竞态代码，需要仔细处理（本仓库用 `fallbackGuard` TTL +
  `byExtensionId` 双保险）；
- 页面用 POST/JS 生成的临时 URL 无法保证可重放（不属于 http(s) GET 的范畴）。

### 3.2 方案 B：`webRequest` 阻塞拦截

- MV2 扩展或企业策略扩展可以在 `onBeforeRequest` 中 `redirectUrl`/`cancel`；
  但这只能在浏览器内部处理，**无法直接交给外部程序**——若想交给下载器，仍要
  cancel 请求 + 通过 native messaging 重建请求。页面导航会失败（用户看到错误页），
  且 POST 流式请求无法重建。
- MV3 下 `webRequestBlocking` 对普通扩展不可用（[MDN permissions][mdn-permissions]），
  所以方案 B 在当前 Firefox 上**不可行**。
- 即使可用，它的可靠性也未必高于方案 A（需要处理请求取消后的页面状态）。

### 3.3 方案 C：右键菜单/手动转交

- `contextMenus` 提供 `info.linkUrl` / `info.srcUrl` / `info.selectionText` 和 `tab.url`，
  通过 native messaging 直接 enqueue，无需 cancel，无竞态。
- 缺点是**不自动**：用户必须每次手动选择，且无法覆盖 JS 触发的下载。
- 本仓库把它作为方案 A 的补充，而不是替代。

### 3.4 其他被讨论过但不可行的方案

- **DNR redirect 到外部程序**：DNR 只能改成浏览器内的 URL；不能把数据交给本机进程。
- **content script 拦截 `<a download>` 点击**：只能覆盖部分静态触发路径，
  对 `window.open`、表单、JS `blob:` 下载无效，且需要更宽的站点权限。
- **注册自定义协议处理器**：需要用户确认且会打断浏览器默认下载行为，体验更差。

**结论**：在 Firefox MV3 下，要"自动接管"，方案 A 是唯一符合平台能力的可行方案；
把方案 C 作为可靠的手动兜底。这也是为什么浏览器集成常被认为是下载器最难的部分（§4）。

## 4. 为什么浏览器集成是下载器最难的部分之一

按影响程度从高到低：

1. **平台能力与竞态**：MV3 移除了阻塞式网络拦截，自动接管只能"先让浏览器开始下载再取消"，
   小文件竞态、取消/回退状态机、`erase` 跨浏览器语义差异都会制造难以复现的 bug
   （证据：§1.2/§1.3；置信度：高）。
2. **安装与分发**：原生宿主必须用户级安装（注册表 + 绝对路径 + 白名单 ID），
   扩展必须签名/审核，AMO/Chrome Web Store 的规则与审核周期都会拖慢版本迭代
   （证据：[EW signing][ew-signing]、[MDN bss][mdn-bss]；置信度：高）。
3. **认证与反爬**：下载往往依赖登录态（Cookie）、Referer、UA、一次性签名 URL；
   下载器复用这些凭据既涉及兼容性也涉及隐私，稍有不慎就是 403/风控或凭据泄露
   （证据：本仓库 cookies/回退设计；置信度：高）。
4. **多浏览器分裂**：manifest 键、背景页模型、native messaging 白名单格式、注册表位置、
   `erase` 语义全都不同，需要多套适配与测试矩阵（证据：§2 表格；置信度：高）。
5. **调试与可观测性**：native host 是浏览器启动→断开的短生命周期进程，日志只能走 stderr/
   文件；Job 对象会顺手杀掉宿主子进程；用户侧错误往往只剩一句
   `No such native application`（证据：[MDN native messaging][mdn-native]；置信度：高）。
6. **安全审核与杀软误报**：常驻原生宿主 + 注册表 + 下载任意 URL 的组合容易触发
   SmartScreen/杀软误报，也会被商店要求解释每一权限（证据：签名/审核要求；置信度：中）。
7. **测试困难**：真实下载行为依赖浏览器 UI 状态，自动化测试只能覆盖扩展逻辑与原生宿主
   协议；"Firefox 里点一下会不会接管"通常需要人工验证（本仓库即如此，§6.3；置信度：高）。

## 5. 安全与隐私分析

### 5.1 威胁模型

- **恶意网页**：不能直接调用 native messaging（只有扩展上下文可以）；可以诱导下载、
  构造恶意文件名/URL/Cookie。缓解：只接受 http(s)、文件名清洗、拒绝控制字符、
  host 侧再次校验（本仓库 `protocol.rs`）。
- **恶意扩展**：Firefox 强制 host manifest 的 `allowed_extensions`；其他扩展无法连接
  `com.rdm.download`。缓解：扩展 ID 固定 `rdm@rdm.download`，不发布宽松白名单。
- **同用户恶意进程**：本机 IPC 绑定 `127.0.0.1` + 随机 token（`ipc.json`）。
  同用户的任意进程可读 `ipc.json` 与 `queue/`、日志，这是**同用户信任边界**；
  token 防的是跨用户/局域网投递，不防同用户恶意软件（置信度：高）。
- **本地文件滥用**：若允许 `file://` 交给引擎，恶意页面（或用户误操作）可能让下载器
  读取/复制本地文件；本仓库明确拒绝非 http(s)（`url_util.rs`）。

### 5.2 Cookie 与凭据传递

- 传递链路：扩展 `cookies.getAll` → native message → 宿主内存 → `ipc.json` token 鉴权 →
  GUI 内存。**日志中只记录长度/域名，不记录 Cookie 值**（`protocol.rs` 与 `native.log`
  行为，已由集成测试断言）。
- 只在用户开启「转发 Cookie」时传递；关闭后必须重新登录的站点会失败（可选择）。
- `Authorization` 等敏感请求头**不透传**（宿主侧 allow-list 只放 Accept/Accept-Language 等）。
- spool 落盘前剥离凭据（见 §5.4）；日志与 spool 均不包含 Cookie/Authorization 值。

### 5.3 Token 与本地 IPC

- GUI 启动写 `%LOCALAPPDATA%\RDM\ipc.json`：`{"protocol":1,"port":<随机>,"token":"<hex32>",
  "pid":..., "app_version":...}`；只监听 `127.0.0.1`；每次重启 token 变化；
  请求必须带 token（`crates/rdm-ipc`）。
- 风险：`ipc.json` 位于用户目录，同用户进程可读；不承诺防御同用户恶意程序。
- 建议（后续）：`ipc.json` 用 Windows DPAPI 加密；queue 已剥离凭据，但仍含 URL/Referer，
  如需更强保护可一并加密；对 GUI 增加请求来源审计日志（不记录敏感值）。

### 5.4 凭据落盘策略：spool 前剥离（已决策并实现）

`docs/extension-protocol.md` §4 要求 Cookie 不落盘、日志脱敏。**GUI 离线且无法自动启动**
时，宿主必须把任务写进 `%LOCALAPPDATA%\RDM\queue\*.json` 才能不丢任务；如果原样写入，
登录态就会落到磁盘。Lead 已决策（2026-10-19）**安全优先：落盘前剥离凭据**，实现为：

1. `download.cookie = None`；
2. 从 `download.headers` 中移除键名（不区分大小写）为
   `authorization` / `proxy-authorization` / `cookie` 的项；
3. 保留 `referer` / `user_agent` / `Accept` 等非凭据上下文；
4. `queued` 响应新增 `credentials_stripped: true`，扩展弹窗据此提示用户
   「登录态/授权信息不会保存；如需要登录态请先启动 RDM 后重新下载」。

实现位置：`crates/rdm-native/src/spool.rs`（`strip_credentials` / `spool_item`）。
单元测试与端到端冒烟测试都断言 spool 文件、JSON 文本、`native.log` 中**不出现**
Cookie 值或 `Authorization` 头值。

已知取舍与残余边界：

- 排队的下载若依赖登录态，GUI 启动后可能返回 403/401（用户可按提示重新下载）；
- URL 本身（可能含一次性签名 query）、Referer、UA 仍会落盘——它们不是 Cookie，
  但也可能带有短期凭据；日志中 URL 已去 query/fragment；
- 宿主的正常路径（GUI 在线/可启动）不产生 spool，Cookie 只在内存链路中；
- 后续如要同时满足"隐私 + 排队登录态可下载"，可用 DPAPI 加密 spool（未实现）。

### 5.5 其他防护措施

- URL 长度上限 8 KiB；只允许 http(s) 且必须有 host；
- 文件名去掉路径分隔符、Windows 非法字符、控制字符，限长 255 字节；
- `output_dir` 必须是绝对路径；
- Cookie 拒绝 CR/LF/NUL，上限 16 KiB；Referer/UA 拒绝控制字符；
- 宿主所有响应都是结构化 JSON，异常不静默退出（避免浏览器只看到 "native host exited"）。

## 6. 本仓库实现与验证映射

### 6.1 实现选择 ↔ 平台限制

| 平台限制 | 实现对应 |
| --- | --- |
| Firefox 不支持 `background.service_worker` | `manifest.json` 用 `background.scripts: [lib/pure.js, background.js]` |
| MV3 webRequest 只能观察 | `background.js` 的 4 个非阻塞 webRequest 监听器只填充请求缓存 |
| `onCreated` 竞态 | `cancelAndEraseBrowserDownload()`：cancel → 轮询终态 → `removeFile`+`erase` |
| 回退防循环 | `fallbackGuard`（URL + 30s TTL）+ 扩展 `byExtensionId` 检查 |
| Firefox Windows Job 杀宿主 | `gui.rs` 用 `DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP \| CREATE_BREAKAWAY_FROM_JOB`，失败降级 |
| 宿主 stdout 即协议 | 日志写 `%LOCALAPPDATA%\RDM\logs\native.log`，仅 WARN/ERROR 镜像 stderr |
| GUI 离线不能丢任务 | spool 到 `%LOCALAPPDATA%\RDM\queue\*.json`（裸 `EnqueueItem`，**落盘前剥离 cookie/authorization 凭据**） |
| AMO 未来要求 | 研究/README 记录：上架前需补 `data_collection_permissions` 并签名 |

### 6.2 静态与单元验证（本机可自动执行）

```powershell
node --check extension/firefox/background.js
node --test extension/firefox/test/pure.test.js extension/firefox/test/background.test.js
# 20 项（12 纯逻辑 + 8 后台装配冒烟，含 queued 凭据提示文案）
npx --yes web-ext@8 lint --source-dir extension/firefox
# 0 errors / 0 warnings / 2 notices（数据收集声明、开发用 Python 图标生成脚本）
cargo test -p rdm-native
# 53 项单测 + 5 项端到端冒烟
```

`crates/rdm-native/tests/native_smoke.rs` 用真实 exe + 进程内 mock GUI IPC server
（`127.0.0.1:0` + 临时 `LOCALAPPDATA`）验证：分帧、ping、enqueue 全链路、spool 落盘（含
凭据剥离断言）、结构化错误与退出码；测试并行安全且不污染真实 `%LOCALAPPDATA%\RDM`。

**测试隔离**：`gui::ensure_gui` 的 readiness 检查被抽成可注入 probe
（`ensure_gui_with`），单元测试用 `|| true/false` + tempdir 且从不读取真实
`ipc.json`；因此**本机有真实 `rdm-gui` 运行时 `cargo test -p rdm-native` 仍全绿**
（已在 GUI 运行中实测）。集成测试则通过给子进程注入临时 `LOCALAPPDATA` 实现隔离。

另外用 `web-ext run` 在 **Firefox 158.0 headless** 下临时载入本扩展验证了真实浏览器
可加载性（输出 `Installed … as a temporary add-on`，无 manifest/权限错误）：

```powershell
npx --yes web-ext@8 run --no-reload --no-input --source-dir extension/firefox `
  --firefox "C:\Program Files\Mozilla Firefox\firefox.exe" `
  --arg=--headless --arg=--no-remote
```

这一步只能证明"Firefox 接受该 manifest/权限并启动后台脚本"，不能证明下载接管行为。

### 6.3 真实 rdm-gui 端到端验证（2026-10-19，通过）

宿主的 mock IPC 只覆盖协议；为验证"native host → 真实 GUI → 真实下载引擎"整条链路，
用 `%TEMP%\rdm-e2e-native-gui.ps1`（仅测试用，不入库）跑了一次完全隔离的端到端：

1. `rdm-fixture-server --port 18160 --size 8M --delay-ms 20` 提供确定性 8 MiB 内容；
2. 手工构造 Firefox native message（`u32 LE` 长度 + JSON）经 stdin 喂给
   `target-native\debug\rdm-native.exe`，消息带 cookie / Accept / Referer / UA；
3. 宿主通过 `RDM_GUI_PATH` **自动拉起真实 `rdm-gui.exe`**（env `LOCALAPPDATA` 指向临时目录，
   真实用户状态零污染），542 ms 后收到
   `{"type":"enqueued","id":1,"download_id":"dl_000001","gui_running":true}`；
4. 带 token 的 IPC `status` 返回 `dl_000001` `downloading`，4 连接、总量 8 MiB；
5. 轮询 GUI 的 `downloads.json` 到 `completed`：文件 8,388,608 B，
   SHA-256 `68f5648506659ef2d03ebdcca49c078b7886fab81e7b2e5b7084cd052b5f03ad`
   与独立下载的参考文件**完全一致**；
6. `queue/` 为空（GUI 在线未走 spool）、`native.log` 只记录
   `cookie accepted: host=127.0.0.1 bytes=23 entries=1` 而无 cookie 值、
   `handoff accepted by GUI (download_id=dl_000001)`，宿主 stdin 关闭后 exit code 0；
7. 只终止本测试启动的 fixture/GUI PID，删除临时目录。

结论：native messaging 分帧、`launch_gui` 自动启动、token 鉴权 IPC、enqueue 字段传递、
引擎下载与完成状态写盘均已在真实组件上打通。

### 6.4 仍未在本机自动验证的部分（未决风险）

1. **真实 Firefox 中的下载接管行为**（弹出下载项后取消的速度、极小文件竞态、
   是否真的不出现 Firefox 下载条目）需要人工在 Firefox + 真实站点上验证；
   本仓库只验证了扩展能被 Firefox 158 加载 + 逻辑单测（§6.2）。
2. **Firefox → native host 的浏览器侧链路**只做了静态/加载验证与 mock 冒烟；真实
   `connectNative`、宿主进程随浏览器任务退出退出等行为仍建议真机点一次下载确认。
3. **AMO 审核**：未提交，无法预知审核意见（尤其 `webRequest`/`cookies`/nativeMessaging
   权限组合与"下载接管"用途的说明）。
4. **杀软误报**：未在干净 Windows 环境验证。

## 7. 参考资料

- [MDN: manifest.json/background][mdn-background]
- [MDN: manifest.json/host_permissions][mdn-hostperm]
- [MDN: manifest.json/permissions（含 webRequestBlocking 的 MV3 限制）][mdn-permissions]
- [MDN: manifest.json/browser_specific_settings][mdn-bss]
- [MDN: WebExtensions API/downloads][mdn-downloads]
- [MDN: downloads.cancel()][mdn-cancel]
- [MDN: downloads.erase()][mdn-erase]
- [MDN: downloads.removeFile()][mdn-removefile]
- [MDN: Native messaging][mdn-native]
- [Extension Workshop: Manifest V3 migration guide][ew-mv3]
- [Extension Workshop: Temporary installation in Firefox][ew-temp]
- [Extension Workshop: Signing and distributing your add-on][ew-signing]
- [Chrome for Developers: chrome.downloads][chrome-downloads]
- [Chrome for Developers: Native messaging][chrome-native]
- [uBlock-issues #3561（MV3 与 webRequestBlocking 的现状讨论）][ublock-mv3]
- [Firefox Bug 1573659: Implement background.service_worker][bug-1573659]

[mdn-background]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/manifest.json/background
[mdn-hostperm]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/manifest.json/host_permissions
[mdn-permissions]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/manifest.json/permissions
[mdn-bss]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/manifest.json/browser_specific_settings
[mdn-downloads]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/API/downloads
[mdn-cancel]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/API/downloads/cancel
[mdn-erase]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/API/downloads/erase
[mdn-removefile]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/API/downloads/removeFile
[mdn-native]: https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/Native_messaging
[ew-mv3]: https://extensionworkshop.com/documentation/develop/manifest-v3-migration-guide/
[ew-temp]: https://extensionworkshop.com/documentation/develop/temporary-installation-in-firefox/
[ew-signing]: https://extensionworkshop.com/documentation/publish/signing-and-distribution-overview/
[chrome-downloads]: https://developer.chrome.com/docs/extensions/reference/api/downloads
[chrome-native]: https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging
[ublock-mv3]: https://github.com/uBlockOrigin/uBlock-issues/discussions/3561
[bug-1573659]: https://bugzilla.mozilla.org/show_bug.cgi?id=1573659
