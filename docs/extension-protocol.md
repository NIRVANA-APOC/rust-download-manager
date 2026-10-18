# RDM 浏览器集成协议（Firefox，v0.1）

本文定义三条链路：

1. 扩展 → 原生宿主：Firefox Native Messaging（4 字节小端长度前缀 + UTF-8 JSON）。
2. 原生宿主 → GUI：本机 TCP JSONL + 随机 token（`rdm-ipc` crate）。
3. GUI 内部语义：`Request` / `Response` / `DownloadRequest`。

## 1. Firefox Native Messaging

### 1.1 宿主注册（Windows，无需管理员）

原生消息宿主名：`com.rdm.download`。

1. 扩展 ID：`rdm@rdm.download`（`browser_specific_settings.gecko.id`）。
2. 宿主 manifest 文件名：`com.rdm.download.json`，内容：

```json
{
  "name": "com.rdm.download",
  "description": "RDM native messaging host",
  "path": "C:\\Users\\<user>\\AppData\\Local\\RDM\\rdm-native.exe",
  "type": "stdio",
  "allowed_extensions": ["rdm@rdm.download"]
}
```

3. 注册表（HKCU，用户级）：
   `HKEY_CURRENT_USER\Software\Mozilla\NativeMessagingHosts\com.rdm.download`
   默认值（REG_SZ）= manifest 的绝对路径。
4. Firefox 只接受**绝对路径**；manifest 与 exe 建议都放在 `%LOCALAPPDATA%\RDM\`。
5. 安装/卸载脚本：`scripts/install-firefox-host.ps1` / `scripts/uninstall-firefox-host.ps1`。

### 1.2 消息分帧

- 每帧：`u32_le length` + `length` 字节 UTF-8 JSON（length 不含前缀）。
- 单条消息上限 1 MiB（Firefox 限制 4 GiB 级，但我们对 request 设 1 MiB 防御）。
- 宿主从 stdin 读、向 stdout 写；日志只能写 stderr / 文件（stdout 是协议通道）。

### 1.3 扩展 → 宿主消息

```jsonc
// 探测
{"type":"ping","id":1}
// 结果
{"type":"pong","id":1,"version":"0.1.0","gui_running":true,"protocol":1}

// 转交下载
{
  "type":"enqueue",
  "id":2,
  "request":{
    "url":"https://example.com/big.iso",
    "file_name":"big.iso",
    "output_dir":null,
    "connections":8,
    "headers":{"User-Agent":"...","Accept-Language":"zh-CN"},
    "cookie":"a=1; b=2",
    "referer":"https://example.com/page",
    "user_agent":"Mozilla/5.0 ...",
    "source":"firefox",
    "source_page":"https://example.com/page"
  }
}
// 结果
{"type":"enqueued","id":2,"download_id":"...","gui_running":true}
// 或（GUI 未运行且启动失败，已落入磁盘队列）
{"type":"queued","id":2,"reason":"gui_unavailable","spool":"C:\\...\\queue\\1729.json"}
// 或错误
{"type":"error","id":2,"code":"invalid_request","message":"..."}
```

宿主行为：

1. `ping`：读 `ipc.json`，向 GUI 发 `ping`；返回 GUI 是否在线。
2. `enqueue`：
   - GUI 在线 → `rdm_ipc::send(Request::Enqueue{..})`，返回 GUI 分配的 download id。
   - GUI 不在线 → 尝试启动同目录 / `RDM_GUI_PATH` 指定的 `rdm-gui.exe`，轮询 `ipc.json`
     最多 8 秒；成功则投递；仍失败则写 spool 文件并返回 `queued`。
3. 任何异常都回结构化 `error`，绝不静默退出（否则 Firefox 侧只看到断开）。

spool 目录：`%LOCALAPPDATA%\RDM\queue\*.json`；GUI 启动时扫描并自动加入队列。

## 2. 宿主 ↔ GUI IPC（本机 TCP JSONL）

### 2.1 发现与鉴权

- GUI 监听 `127.0.0.1:0`，生成 32 字节随机 token（hex），原子写 `%LOCALAPPDATA%\RDM\ipc.json`：

```json
{"protocol":1,"port":53421,"token":"...","pid":1234,"app_version":"0.1.0"}
```

- 客户端连接后发送一行 JSON；`token` 必须匹配，否则 `ok:false, error:"unauthorized"` 并断开。
- 读超时 5s，写超时 5s；每连接一问一答，服务端可并发处理多连接。

### 2.2 请求信封

```jsonc
{"id":1,"token":"...","method":"ping"}
{"id":2,"token":"...","method":"enqueue","item":{
  "download":{ /* rdm_core::DownloadRequest */ },
  "source":"firefox",
  "source_page":"https://example.com/page"
}}
{"id":3,"token":"...","method":"status"}
{"id":4,"token":"...","method":"pause","id":"dl_01"}
{"id":5,"token":"...","method":"resume","id":"dl_01"}
{"id":6,"token":"...","method":"cancel","id":"dl_01"}
{"id":7,"token":"...","method":"open_folder","id":"dl_01"}
{"id":8,"token":"...","method":"activate"}
```

### 2.3 响应

```jsonc
{"id":2,"ok":true,"data":{"download_id":"dl_01"}}
{"id":2,"ok":false,"error":"invalid url: ..."}
```

`status` 的 `data`：

```jsonc
{"downloads":[{"id":"dl_01","file_name":"big.iso","state":"downloading",
  "downloaded":123,"total":456,"speed_bps":1000.0,"connections":8}],
 "global_speed_bps":1000.0,"active":1}
```

## 3. 数据模型

`EnqueueItem` 与 `rdm_core::DownloadRequest` 一一对应（见 `crates/rdm-core/src/lib.rs`）。
字段全部可 JSON 序列化；未知字段忽略（向前兼容）。ID 由 GUI 生成（`dl_` + 递增序号）。

## 4. 安全与隐私

- IPC 只绑定 `127.0.0.1`，必须带 token；token 文件权限尽力收紧。
- Cookie 只在扩展 → 宿主 → GUI 内存中传递，不落盘（日志脱敏）。
- 扩展仅在用户开启接管时拦截；支持站点/扩展名黑名单；`about:`、`blob:`、`data:` 不拦截。
- 原生宿主只接受 `allowed_extensions` 中扩展 ID 的连接（Firefox 强制）。
- 下载 URL 必须 http(s)；拒绝 `file://`，避免本地文件被"下载"。

## 5. 错误码

| code | 含义 |
| --- | --- |
| `gui_unavailable` | GUI 未运行且自动启动失败 |
| `unauthorized` | IPC token 不匹配 |
| `invalid_request` | JSON 结构或 URL 非法 |
| `duplicate` | 同一 URL 正在下载（可选，由 GUI 决定） |
| `internal` | 其它异常 |
