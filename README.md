# RDM — Rust Download Manager

> 纯 Rust 编写、GPUI 渲染、带 Firefox 接管能力的多线程下载器。
> 目标不是"又一个 aria2 套壳"，而是把**分段下载的正确性、可观测性和浏览器交接体验**做扎实。

## 这是什么

RDM 是一个从零实现的 HTTP(S) 多线程下载器：

- **引擎**（`rdm-core`）：Range 探测、块级工作窃取、N 连接并发、断点续传（ETag /
  Last-Modified / 长度三重校验）、无 Range 自动降级、限速、SHA-256 校验、进度事件流。
- **桌面端**（`rdm-gui`）：Zed 的 **GPUI** GPU 渲染框架，暗色三栏界面、虚拟列表、
  实时速度曲线、暂停/继续/取消/打开文件夹、列表持久化、IPC 单实例。
- **命令行**（`rdm`）：无头下载 + JSONL 事件输出，自动化测试与基准测试的一等公民。
- **浏览器集成**：Firefox Manifest V3 扩展 + 原生消息宿主（`rdm-native`），
  接管浏览器下载时把 **Cookie / Referer / 请求头** 一并交给 RDM。
- **可复现验证**：`rdm-fixture-server` 提供可控延迟、Range 忽略、故障注入的本地 HTTP
  服务，让"多线程到底有没有用"可以用数字回答，而不是口号。

## 快速开始

```powershell
# 1) 构建（Windows + Rust stable；GUI 首次编译较久）
cargo build -p rdm-cli -p rdm-native -p rdm-gui

# 2) 命令行下载
cargo run -p rdm-cli -- download https://example.com/big.iso -d downloads -c 8

# 3) 启动 GUI
cargo run -p rdm-gui

# 4) Firefox 集成（见下文）
powershell -ExecutionPolicy Bypass -File scripts/install-firefox-host.ps1
```

## 架构

```
                    ┌──────────────────────────────┐
  Firefox 扩展 ─────►│ rdm-native (native messaging)│
  (MV3, JS)         └──────────────┬───────────────┘
                                   │ JSONL + token (127.0.0.1)
                    ┌──────────────▼───────────────┐
                    │ rdm-gui (GPUI, IPC server)    │
                    │  RdmApp ── DownloadManager    │
                    └──────────────┬───────────────┘
                                   │ rdm-core API
                    ┌──────────────▼───────────────┐
                    │ rdm-core                     │
                    │ probe → plan → N workers     │
                    │  work-stealing chunk queue   │
                    │  .part + resume state        │
                    └──────────────┬───────────────┘
                                   │ HTTP Range / HTTP2
                             远程服务器 / CDN

  rdm (CLI) ────────────────► rdm-core（同一引擎，无 GUI 依赖）
```

详细设计见 [docs/architecture.md](docs/architecture.md)，浏览器协议见
[docs/extension-protocol.md](docs/extension-protocol.md)。

## 为什么不是"又一个下载器"

调研结论（完整版见 [docs/research/competitive-landscape.md](docs/research/competitive-landscape.md)）：

- 开源多线程下载器少，不是因为没人需要，而是因为**难点不在"开 N 个线程"**：
  服务器忽略 Range、CDN 边缘内容不一致、签名 URL 过期、断点续传校验、
  浏览器接管与安装门槛、跨平台 GUI/打包/签名、可复现测试的成本，才是护城河。
- 很多现成产品是 **aria2 内核 + Electron 壳**：能力有，但体积/内存/启动成本高，
  UI 与引擎之间隔着 RPC，细粒度状态（每连接速度、块进度）拿不到。
- RDM 的选择：**引擎与 UI 同语言、同进程、同一套类型**；块级工作窃取而不是静态分片；
  把"服务器不支持 Range"当作一等公民来展示而不是假装加速。

## 基准测试

TODO: 填入 fixture server 实测数据（1 vs 8 连接、断点恢复补下字节数、限速误差）。

## 目录

```
crates/rdm-core      下载引擎（库）
crates/rdm-cli       `rdm` 命令行
crates/rdm-gui       GPUI 桌面端
crates/rdm-ipc       GUI ↔ 原生宿主本地 IPC
crates/rdm-native    Firefox native messaging host
crates/rdm-fixture-server  测试/基准用 HTTP 服务器
extension/firefox    Firefox MV3 扩展
docs/                架构、协议、调研、验证
scripts/             安装与基准脚本
```

## Firefox 集成

1. 构建并安装原生宿主：

```powershell
cargo build -p rdm-native
powershell -ExecutionPolicy Bypass -File scripts/install-firefox-host.ps1
```

2. Firefox 打开 `about:debugging#/runtime/this-firefox` → 「临时载入附加组件」→
   选择 `extension/firefox/manifest.json`。
3. 扩展弹窗中开启"接管浏览器下载"。之后普通下载会被取消并转交 RDM（保留 Cookie /
   Referer / Content-Disposition 文件名）。GUI 未运行时会自动拉起。

## 开发与测试

```powershell
cargo test -p rdm-core          # 引擎单测 + fixture 集成测试
cargo test -p rdm-ipc -p rdm-native
cargo run -p rdm-fixture-server -- --size 67108864 --delay-ms 20
```

## 已知限制

- 只处理 HTTP(S) 分段下载；BT / 视频站点解析不在核心范围。
- 服务器忽略 Range 时退化为单连接，UI 会明确标记"服务器不支持加速"。
- GPUI 仍处于 pre-1.0；release 构建的 Windows 着色器编译需要 Windows SDK 的 `fxc.exe`
  （开发构建使用运行时编译，无需 SDK）。
- Firefox 扩展需要原生宿主手动安装；上架 AMO 需签名与审核。

## License

MIT OR Apache-2.0
