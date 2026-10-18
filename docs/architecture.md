# RDM 架构设计（v0.1）

> 状态：契约文档。GUI / CLI / 扩展 / 引擎均按本文与 `crates/rdm-core/src/lib.rs` 中的公开类型实现。
> 修改公开契约前必须先通知 Lead，避免并行开发冲突。

## 1. 目标与边界

RDM（Rust Download Manager）是一个 **纯 Rust** 的桌面多线程下载器：

- 下载内核：`rdm-core`（异步、分块、多连接、可恢复、可限速、可校验）。
- 桌面 GUI：`rdm-gui`（GPUI，GPU 渲染，不依赖 Electron/CEF）。
- 命令行：`rdm`（`rdm-cli`），用于无头下载、自动化测试与基准测试。
- 浏览器集成：Firefox WebExtension + `rdm-native` 本地原生消息宿主 + GUI 本地 IPC。
- 测试/基准：`rdm-fixture-server` 提供可控 Range/延迟/故障注入的本地 HTTP 服务。

不做的（当前版本）：BitTorrent、视频站点解析（yt-dlp 类）、云盘协议、代理池、跨设备同步。
这些留给插件层，核心只做 HTTP(S) 分段传输。

## 2. 工作区

```
RDM/
├─ Cargo.toml                 # workspace（6 个 crate）
├─ crates/
│  ├─ rdm-core/               # 引擎库（唯一拥有下载状态机的 crate）
│  ├─ rdm-ipc/                # GUI <-> 原生宿主 的本地 IPC 协议与客户端
│  ├─ rdm-cli/                # `rdm` 命令行
│  ├─ rdm-gui/                # GPUI 桌面端（同时是 IPC server）
│  ├─ rdm-native/             # Firefox native messaging host
│  └─ rdm-fixture-server/     # 集成测试/基准用本地 HTTP 服务器
├─ extension/firefox/         # Manifest V3 WebExtension
├─ docs/                      # 架构、协议、调研、验证报告
└─ scripts/                   # 安装、基准、演示脚本
```

依赖方向：`rdm-cli -> rdm-core`；`rdm-gui -> rdm-core + rdm-ipc`；`rdm-native -> rdm-ipc (+ rdm-core 类型)`；
`rdm-ipc -> rdm-core`（仅复用 `DownloadRequest` 类型，不引入网络栈）。

## 3. 下载内核设计（rdm-core）

### 3.1 公开 API（冻结）

```rust
pub async fn probe(&DownloadRequest) -> Result<RemoteMeta, DownloadError>;
pub async fn download(DownloadRequest, Option<UnboundedSender<DownloadEvent>>)
    -> Result<DownloadSummary, DownloadError>;
pub async fn download_with_handle(DownloadRequest, Option<UnboundedSender<DownloadEvent>>,
                                  DownloadHandle) -> Result<DownloadSummary, DownloadError>;
```

- `DownloadHandle::{pause, cancel}`：跨任务控制。暂停/取消不是失败，返回
  `DownloadError::Paused{downloaded}` / `Cancelled{downloaded}`。
- 恢复 = 用**同一个** `DownloadRequest` 再次调用 `download`；引擎自动发现
  `<target>.part` 与 `<target>.part.json` 并校验后继续。
- 事件通过 `tokio::sync::mpsc::UnboundedSender<DownloadEvent>` 推送；UI 侧 250ms 节流渲染。

### 3.2 元数据探测（probe）

1. 先发 `HEAD`（带 UA / Referer / Cookie / Proxy / `Accept-Encoding: identity`）。
2. 若 `HEAD` 不给 `Content-Length` 或 `Accept-Ranges`，退化为 `GET Range: bytes=0-0`：
   - `206` + `Content-Range: bytes 0-0/N` → 支持 Range，总长 N；
   - `200` + 完整 `Content-Length` → 不支持 Range（多线程不可用），总长 = 该值。
3. 跟随重定向到最终 URL（reqwest 自动跟随，取 `response.url()`）。
4. 解析文件名优先级：
   `Content-Disposition` 的 `filename*`(RFC 5987) → `filename` → 最终 URL path 末段 →
   `download.bin`。做 Windows 非法字符替换、重名加 ` (n)`。
5. 记录 `ETag` / `Last-Modified` 作为恢复校验器。
6. 不可达/4xx/5xx → `DownloadError::Status`（429/503 在下载阶段才重试）。

### 3.3 分块与工作窃取

- 块大小：`chunk_size.unwrap_or(clamp(total/ (connections*4), MIN_CHUNK_SIZE, DEFAULT_CHUNK_SIZE))`，
  默认 4 MiB；太小调度开销大，太大尾延迟高。
- 共 `total_chunks = ceil(total / chunk_size)`；所有块进入一个原子索引队列。
- N 个 worker（`connections`，默认 8，上限 32）循环 `fetch_add` 领取下一块，
  **块级工作窃取**：慢连接只拖慢自己手上的块，不会造成固定分片方案的尾部空转。
- 每块一次 `GET` + `Range: bytes=start-end`，校验返回 `206` 且 `Content-Range` 与请求一致；
  流式读入 64 KiB 缓冲，用 `write_all_at(offset)` 落盘（Windows `seek_write` / Unix `FileExt` 抽象）。
- 单块失败：指数退避 + 抖动重试（默认 6 次，1s→32s 上限，`Retry-After` 优先）。
  连续失败且已有块完成 → 保留 `.part`，返回错误，可恢复。
- 服务器返回 `200`（忽略 Range）时：终止分块，降级为单连接从头下载（覆盖写），
  事件流里挂一条 `Retrying/降级` 说明，最终 `DownloadSummary.range_supported = false`。
- `Content-Length` 未知：单连接顺序下载，不支持断点续传。

### 3.4 落盘与断点状态

- 数据文件：`<target>.part`，开始前 `set_len(total)`（预分配，避免碎片）。
- 状态文件：`<target>.part.json`（原子写：写 `.tmp` 后 rename），结构：

```json
{
  "version": 1,
  "url": "...",
  "final_url": "...",
  "file_name": "...",
  "total_bytes": 123456789,
  "etag": "\"abc\"",
  "last_modified": "...",
  "chunk_size": 4194304,
  "total_chunks": 30,
  "completed_chunks": [0,1,2],
  "downloaded_bytes": 12582912
}
```

- 状态每完成一块更新内存位图，**每 500ms 或每 16 块**原子落盘一次（崩溃最多重下少量块）。
- 恢复校验：`total_bytes`、`ETag`、`Last-Modified` 与本次 probe 一致才续传；否则丢弃状态、
  从 0 开始（并把旧 `.part` 截断），避免拼出损坏文件。
- 完成：`flush + sync_all` → 校验 SHA-256（若请求）→ rename `.part` → 目标名 → 删除状态文件。
- 状态文件损坏：忽略并从零开始（记录 warn），不影响主流程。

### 3.5 并发、限速与进度

- 共享一个 `reqwest::Client`：连接池、HTTP/2 多路复用（服务器支持 h2 时 N 个 Range
  流复用同一 TCP 连接，仍能并发）、超时（连接 15s，读 60s）。
- 限速：全局令牌桶（`speed_limit_bps`），worker 在写盘前按字节数 `acquire`，
  多连接共享同一预算（不是每连接限速）。
- 进度：原子计数（下载字节、活跃 worker、完成块）；单独 reporter 任务每 250ms
  计算滑动窗口速度（EWMA）与 ETA，推 `DownloadEvent::Progress`。
- 取消：worker 在每 64 KiB 读循环检查 `handle.is_cancelled()/is_paused()`；CancellationToken
  + 原子标志双保险。停止前 flush 状态。

### 3.6 正确性/健壮性清单（必须有测试）

1. 多块下载结果与单连接完全一致（哈希相等）。
2. Range 被忽略（200）自动降级且文件正确。
3. 断点恢复：中途取消 → 再次调用 → 文件哈希正确，且只补下缺失块（fixture 服务器统计请求字节）。
4. ETag 变化导致恢复失效并从零开始。
5. 校验和不匹配返回 `ChecksumMismatch` 且不重命名。
6. 慢速 fixture（每块 40ms 延迟）下 8 连接明显快于 1 连接（宽阈值断言，避免 flaky）。
7. 429/503 带 `Retry-After` 的重试路径。
8. 文件名解析：`filename*`、中文、非法字符、重名。
9. 限速：平均速率不超过配置的 1.5 倍（宽松阈值）。

## 4. CLI（rdm）

```
rdm download <URL> [--dir DIR] [--name NAME] [-c/--connections N] [--chunk-size BYTES]
             [--limit BPS] [--sha256 HEX] [--header "K: V"]... [--cookie C] [--referer R]
             [--user-agent UA] [--proxy URL] [--json] [--quiet]
rdm probe <URL> [--json]
```

- `--json`：stdout 每行一个 JSON 事件（`{"event":"progress",...}`），供基准脚本解析；
  最终输出 `{"event":"completed","path":...}`；错误 `{"event":"failed","error":...}`。
- 退出码：0 成功；2 下载失败；3 被信号中断（Ctrl-C 触发 pause 保存状态）。
- `--limit`、`--connections`、`--chunk-size` 用于验证不同策略。

## 5. 本地 IPC（GUI <-> 原生宿主）

- GUI 启动时监听 `127.0.0.1:0`，随机 token，原子写入 `%LOCALAPPDATA%\RDM\ipc.json`：

```json
{"protocol":1,"port":53421,"token":"hex32","pid":1234,"app_version":"0.1.0"}
```

- JSONL：客户端发 `Envelope`，服务端回 `Response`。方法：
  `ping / enqueue / status / pause / resume / cancel / open_folder / activate`。
- 每个请求必须带 token；绑定 loopback，防止局域网其它机器投递下载。
- GUI 退出时删除 `ipc.json`（尽力而为）。

详见 `docs/extension-protocol.md`。

## 6. Firefox 扩展 + 原生宿主

- Manifest V3，`browser_specific_settings.gecko.id = "rdm@rdm.download"`。
- 拦截 `browser.downloads.onCreated`：若开启接管且 URL 为 http(s)：
  1. 从 `webRequest` 缓存取请求/响应头（Referer、Cookie、UA、Content-Type 等）；
  2. `browser.downloads.cancel(id)` + `erase` 浏览器原生下载；
  3. 通过 `runtime.connectNative("com.rdm.download")` 发 `{type:"enqueue", request:{...}}`；
  4. 原生宿主转发到 GUI IPC；若 GUI 未运行则启动 GUI 并重试，失败则落盘队列等待下次启动。
- 右键菜单「使用 RDM 下载」、弹窗（状态/最近转交/开关）、选项页（接管开关、最小文件大小、
  扩展名过滤、连接数、保存目录、限速）。
- 原生消息协议：4 字节小端长度 + UTF-8 JSON（Firefox 规范）。
- 注册：`HKCU\Software\Mozilla\NativeMessagingHosts\com.rdm.download` 默认值指向
  `com.rdm.download.json`（无需管理员权限），manifest 里 `allowed_extensions` 限定扩展 ID。

详见 `docs/extension-protocol.md`。

## 7. GUI（GPUI）

窗口 1180x760，暗色主题（可切浅色）。三栏：

```
┌──────────────────────────────────────────────────────────────┐
│ RDM   [ URL 输入框 .................. ] [添加] [设置]          │
├───────────┬──────────────────────────────────────────────────┤
│ 全部 12   │ 文件名.bin           4.2 MB/s  ETA 00:32  ⏸ ✕     │
│ 下载中 5  │ ██████████████░░░░░░░░  68%   8 连接            │
│ 已完成 6  │ ...                                              │
│ 已暂停 1  │ （uniform_list 虚拟列表）                          │
│           │                                                  │
│ 全局速度  │                                                  │
│ 18.4 MB/s │                                                  │
├───────────┴──────────────────────────────────────────────────┤
│ 状态栏：磁盘写入 / 活动连接数 / 版本                            │
└──────────────────────────────────────────────────────────────┘
```

- 数据流：`RdmApp`（Entity）持有 `Vec<DownloadItem>`；每个 item 的 engine task 在 tokio runtime
  里运行，事件经 `tokio::sync::mpsc` 汇入后台 task，再 `this.update(cx, ...)` + `cx.notify()`。
- 操作：添加（URL/可选文件名/连接数）、暂停、继续、取消、打开文件夹、删除记录、清空已完成。
- 设置：默认连接数、默认目录、全局限速、SHA-256、主题、自动开始。
- 持久化：`%LOCALAPPDATA%\RDM\downloads.json`（列表 + 状态），重启恢复（进行中→暂停）。
- 单实例：启动时若 `ipc.json` 指向的 GUI 活着，则把命令行 URL 通过 IPC 投递后退出。
- 速度曲线：最近 60 个采样点的柱状/面积图（用 GPUI `canvas` 或 div 柱状条实现，二选一）。

## 8. 构建与运行

前置：Rust stable（本机为 `1.99.1`，GNU 工具链）+ MinGW-w64；GPUI 在 Windows 上
debug 构建使用运行时 D3D 着色器编译（release 构建需要 `fxc.exe`，Windows SDK 自带）。

```powershell
cargo build -p rdm-cli -p rdm-native          # 不需要 GPU 依赖
cargo test -p rdm-core                        # 引擎单测 + 集成测试
cargo build -p rdm-gui                        # GPUI（首次编译较久）
cargo run -p rdm-cli -- download https://example.com/file.bin -d downloads -c 8
```

Firefox 侧安装：`scripts/install-firefox-host.ps1`（写原生宿主 manifest + 注册表），
然后在 `about:debugging` 临时加载 `extension/firefox/manifest.json`。

## 9. 已知限制 / 后续

- 仅 HTTP(S)；BT、视频站点解析不在核心范围。
- 服务器忽略 Range 时无法加速（这是协议层硬限制，UI 必须诚实提示）。
- 签名 URL 过期、CDN 边缘内容不一致：靠校验器检测并重下，不做多镜像合并。
- release 构建的 GPUI 着色器需要 Windows SDK 的 fxc（开发/CI 用 debug 构建规避）。
- Firefox 原生消息宿主需要用户手动安装（脚本完成）；上架 AMO 需要签名与审核。
