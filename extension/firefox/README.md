# RDM Firefox 扩展（Manifest V3）

把 Firefox 的下载交给 [RDM](../../README.md) 多线程下载器：自动接管浏览器下载、
右键菜单转交、透传 Cookie/Referer/UA，原生宿主不可用时自动回退浏览器下载。

- 扩展 ID：`rdm@rdm.download`（`browser_specific_settings.gecko.id`）
- 原生宿主名：`com.rdm.download`（见 `crates/rdm-native/`）
- 协议：`docs/extension-protocol.md`（Firefox native messaging 4 字节小端分帧 + GUI IPC）

## 文件结构

```
extension/firefox/
├─ manifest.json          # MV3，background.scripts（Firefox 不支持 service worker）
├─ background.js          # 事件页：webRequest 观察、downloads 接管、native 通信、徽章、右键菜单
├─ lib/pure.js            # 无浏览器依赖的纯逻辑（文件名/过滤/决策），可被 Node 单测
├─ popup.html/js/css      # 工具栏弹窗：在线状态、最近转交、开关、设置入口
├─ options.html/js/css    # 设置页：接管开关、大小/扩展名过滤、连接数、目录、限速、自动启动 GUI
├─ icons/                 # 16/32/48/96 PNG + generate-icons.py（Pillow 重新生成）
├─ test/pure.test.js      # node --test：纯逻辑单测（12 项）
└─ test/background.test.js # node --test：browser API stub 下的后台装配冒烟（8 项）
```

## 安装步骤

### 1. 构建原生宿主

在仓库根目录（Windows PowerShell）：

```powershell
$env:PATH = "C:\Users\<你>\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_*\mingw64\bin;C:\Users\<你>\.cargo\bin;$env:PATH"
$env:CARGO_TARGET_DIR = "C:\Users\<你>\Documents\RDM\target-native"
cargo build -p rdm-native
```

> `target-native` 只是本项目并行开发时约定的独立 target 目录，可换成任意目录；
> install 脚本会自动在 `target-native\`、`target\` 以及其它 `target-*\` 中查找
> `release\`/`debug\` 构建产物。

### 2. 安装原生宿主（无需管理员权限）

```powershell
# 使用已构建的 exe（自动找 target-*/release 或 debug）
pwsh -File scripts/install-firefox-host.ps1

# 或者先构建再安装（release）
pwsh -File scripts/install-firefox-host.ps1 -Build -Configuration Release

# 只安装宿主，不复制 rdm-gui.exe
pwsh -File scripts/install-firefox-host.ps1 -NoGui
```

脚本会：

1. 复制 `rdm-native.exe` 到 `%LOCALAPPDATA%\RDM\rdm-native.exe`（可选复制 `rdm-gui.exe` 便于自动启动）；
2. 写 `%LOCALAPPDATA%\RDM\com.rdm.download.json`，`allowed_extensions = ["rdm@rdm.download"]`；
3. 注册 `HKCU\Software\Mozilla\NativeMessagingHosts\com.rdm.download` 默认值为 manifest 绝对路径；
4. 执行 `rdm-native.exe --version` 冒烟检查。

卸载：

```powershell
pwsh -File scripts/uninstall-firefox-host.ps1              # 清注册表/manifest/exe，保留 queue/
pwsh -File scripts/uninstall-firefox-host.ps1 -KeepBinary  # 只清注册表/manifest
pwsh -File scripts/uninstall-firefox-host.ps1 -Purge       # 额外清 logs/、ipc.json（queue/ 始终保留）
```

### 3. 临时载入扩展

1. Firefox 地址栏打开 `about:debugging#/runtime/this-firefox`
2. 点击「临时载入附加组件…」
3. 选择本目录的 `manifest.json`
4. 点击工具栏 RDM 图标，状态应显示「原生宿主：已连接」

> 临时载入的扩展在 **Firefox 重启后消失**，需要重新载入。若状态显示「未安装」，
> 先重启 Firefox 让 HKCU 中的宿主注册生效，再检查 `about:debugging` 控制台错误。
>
> 要长期使用必须经 AMO 签名（自行分发也要 unlisted 签名）；本仓库目前只支持临时载入。

## 使用

- **自动接管**：开启后，http(s) 下载会先被浏览器创建，扩展随即取消并抹掉这条浏览器
  下载，通过原生宿主交给 RDM；失败时自动回退为浏览器下载（不会形成循环）。
- **右键菜单**：链接/图片/音视频/选中文本 →「使用 RDM 下载」。
- **工具栏徽章**：`ON` = 宿主与 GUI 均在线；`Q` = GUI 未运行（下载会排队）；
  `!` = 宿主未安装/连接异常；`off` = 接管已关闭。
- **弹窗**：在线状态、最近 8 条转交记录、接管开关、重新检测、设置入口。
- **设置页**（弹窗「设置」或 `about:addons` → 扩展 → 选项）：
  - 启用下载接管 / 自动启动 GUI / 右键菜单 / 转发 Cookie
  - 最小文件大小（MiB）、仅接管扩展名、永不接管扩展名
  - 默认连接数（1–32）、保存目录（绝对路径，留空用 RDM 默认）、限速（KiB/s）

## 权限说明

| 权限 | 用途 |
| --- | --- |
| `downloads` | 取消被接管的浏览器下载、清理下载记录；失败回退时重新发起下载 |
| `webRequest` | **只读观察**请求/响应头（UA、Referer、Content-Disposition、Content-Length）；MV3 下不能阻塞请求 |
| `cookies` | 读取目标 URL 的 Cookie 并在用户开启「转发 Cookie」时传给 RDM |
| `nativeMessaging` | 与本机 `com.rdm.download` 宿主通信 |
| `storage` | 保存设置、最近转交记录、宿主状态 |
| `contextMenus` | 右键菜单「使用 RDM 下载」 |
| `alarms` | 每 5 分钟刷新一次宿主/ GUI 在线状态 |
| `host_permissions: http(s)://*` | 观察任意 http(s) 请求、读取对应站点 Cookie；Firefox 127+ 安装时授予 |

## 已知限制

1. **MV3 没有 blocking webRequest**：接管是「下载先开始 → `downloads.onCreated` 里取消」，
   不是请求层面的拦截。极小文件可能已经下载完成；此时扩展会删除浏览器下载的文件
   （`downloads.removeFile` + `erase`）后再交给 RDM 重新下载。
2. 被接管的下载会从 Firefox 下载历史中消失（这是接管的本意），RDM 中也不会标注
   "来自 Firefox" 的额外元数据（`source_page` 会保存来源页面）。
3. `blob:` / `data:` / `file:` / `about:` 链接不会接管（RDM 只处理 http(s)）；POST
   表单或 JS 生成的临时 URL 也可能无法复用。
4. 需要登录态的下载依赖 Cookie 透传；如果站点还要求一次性 token 或特殊签名头，
   可能仍然失败（扩展只转发 Cookie/Referer/UA/Accept/Accept-Language）。
5. **GUI 离线且无法自动启动时**：宿主会把请求落盘到
   `%LOCALAPPDATA%\RDM\queue\*.json` 等待 GUI 下次启动（`queued` 响应带
   `credentials_stripped: true`）。**落盘前会剥离凭据**：`cookie` 被清空，
   `authorization` / `proxy-authorization` / `cookie` 请求头被移除；因此排队的下载
   如果依赖登录态，GUI 启动后可能会失败——弹窗会提示「登录态/授权信息不会保存；
   如需要登录态请先启动 RDM 后重新下载」。URL/Referer/UA 等非凭据信息仍会落盘
   （用户目录私有，日志中不打印 Cookie/Authorization 值）。
6. 原生宿主启动的 GUI 使用 `CREATE_BREAKAWAY_FROM_JOB` 脱离 Firefox 的 Job 对象；
   若宿主无法 breakaway（Firefox Job 限制），GUI 可能随 Firefox 关闭被一起结束。
7. 本扩展严格限定 Firefox 128+：Firefox 127 之前 MV3 host permissions 不在安装时授予，
   `webRequest`/`cookies` 需要额外运行时授权逻辑，未实现。

## 调试

- 扩展日志：`about:debugging` → 本扩展「检查」→ 控制台（前缀 `[RDM ...]`）。
- 原生宿主日志：`%LOCALAPPDATA%\RDM\logs\native.log`（WARN/ERROR 也会出现在浏览器控制台）。
- 宿主手动检查：

  ```powershell
  & "$env:LOCALAPPDATA\RDM\rdm-native.exe" --version
  Get-Content "$env:LOCALAPPDATA\RDM\com.rdm.download.json" -Raw
  Get-Item "HKCU:\Software\Mozilla\NativeMessagingHosts\com.rdm.download"
  ```

- GUI 未运行时查看排队：`Get-ChildItem "$env:LOCALAPPDATA\RDM\queue"`。

## 验证

```powershell
# JS 语法
node --check extension/firefox/lib/pure.js
node --check extension/firefox/background.js
node --check extension/firefox/popup.js
node --check extension/firefox/options.js
# 纯逻辑 + 后台装配测试（20 项：12 纯逻辑 + 8 后台监听/消息/queued 文案）
node --test extension/firefox/test/pure.test.js extension/firefox/test/background.test.js
# manifest / 扩展静态校验（需联网下载 web-ext）
npx --yes web-ext@8 lint --source-dir extension/firefox
# 原生宿主单测 + 端到端冒烟（mock GUI IPC）
cargo test -p rdm-native
```

`test/pure.test.js` 覆盖：文件名清理、扩展名过滤、Content-Disposition 解析、
设置归一化、接管决策矩阵、Cookie 头拼接与上限、URL 脱敏、大小/时间格式化。
`test/background.test.js` 用 stub 的 `browser` API 加载真实 `background.js`，验证事件监听器
顶层注册、`getState`/`setSettings`/`testConnection` 消息、宿主缺失归类、非 http 下载跳过，
以及 `queued`（凭据已剥离）时的用户提示文案。
原生宿主测试全部使用临时 `LOCALAPPDATA` 或注入式 readiness probe，**本机有真实
`rdm-gui` 在运行时测试依然全绿**（隔离验证）。

`web-ext lint` 当前结果：**0 errors / 0 warnings / 2 notices**：
- `MISSING_DATA_COLLECTION_PERMISSIONS`：AMO 未来提交要求，临时载入阶段可忽略（见上）；
- `FLAGGED_FILE_TYPE`：开发用 `icons/generate-icons.py`，打包上架时应排除。

此外已用 `web-ext run` 在 **Firefox 158.0 headless** 下临时载入本扩展验证（输出
`Installed … as a temporary add-on`，无 manifest/权限错误）；真实下载接管行为仍需人工在
Firefox 中验证（见「已知限制」）。

原生宿主 → **真实 rdm-gui** 的端到端也已用 8 MiB fixture 验证（隔离 `LOCALAPPDATA`，
手工 native message 喂 stdin）：auto-launch GUI 后 542 ms 返回
`enqueued / download_id=dl_000001`，GUI `status` 与 `downloads.json` 显示任务
`completed`，文件 SHA-256 与独立参考一致（`68f56485…f03ad`），`queue/` 为空，
`native.log` 无 cookie 值，宿主 exit 0。
