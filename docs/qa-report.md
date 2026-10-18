# RDM 独立验收报告（QA）

> 验收方：独立 QA（不参与开发、不修改源码）｜验收日期：2026-10-19（Asia/Shanghai）
> **验证时点**：`b8dfc88`（2026-10-19 01:47:57 +0800，源码冻结提交，工作区干净）
> 开发期动态验证起点：`4d04ba1` + 21 个未提交改动（01:30 时点）；全部数字均在下方标注测量方式与时间。
>
> 本报告只记录**我自己跑出来的命令与输出**；队友自测结论一律标注为"队友证据（未独立复跑）"。
> 唯一写入的受版本控制文件就是本报告（构建产物在 `target-qa/`，未提交）。

---

## 0. 结论摘要

| 维度 | 结果 |
|---|---|
| 构建（debug workspace / release CLI+fixture） | **PASS** |
| 测试矩阵（140 个测试） | **PASS，0 failed** |
| `cargo fmt --all --check` | **PASS，0 diff**（冻结时点；开发中期曾 44 diff，冻结前已修） |
| `cargo clippy`（core/cli/ipc） | **PASS，0 error / 0 warning**（开发中期曾有 1 warning，冻结前已修） |
| 引擎核心行为（独立 harness + CLI，release） | **PASS，7/7 场景** |
| GUI 端到端（隔离环境，22 项断言） | **PASS，22/22** |
| 浏览器侧静态验证 | **PASS**（node --check、20 JS 测试、web-ext lint 0 error/0 warning、manifest 字段审计、native host 5 集成测试） |
| 文档一致性 | **基本一致；发现 3 处需修的 README 表述/数字（见 §7）** |
| 阻塞项 | **无** |

关键数字（全部为 release 构建实测，详见 §4）：

- **1 vs 8 连接（64 MiB，每 256 KiB 延迟 20 ms，5 次中位，冻结提交复跑）**：
  1 连接 **7,972 ms（8.03 MiB/s）** → 8 连接 **1,026 ms（62.38 MiB/s）**，**加速比 7.77x**；
  10 次运行输出 SHA-256 全部一致（冻结前同配置复测 7,995 ms / 1,027 ms / 7.78x，波动 < 0.5%）。
- 断点恢复：暂停后恢复**线上字节 = 缺失字节（差值 0）**；`kill -Force` 崩溃后恢复 `resumed_chunks=16/32`、哈希正确。
- 200 忽略 Range 降级：单流、`range_supported=false`、SHA-256 正确。
- 错误 SHA-256：`ChecksumMismatch`、**目标文件/`.part`/状态文件零残留**。
- 全局限速 2 MiB/s × 8 连接：实测平均 **2.118 MiB/s（1.06x，+5.9%）**，阈值 1.5x 内。
- GUI：ipc.json 发布 **378 ms**、空载 WorkingSet **69.1 MB**；64 MiB 下载、暂停/继续、单实例投递、cancel 全部通过；截图见 `docs/evidence/rdm-gui-final.png`。

---

## 1. 环境与验证方法

**机器**：Windows 11 专业版 10.0.26300（x86_64），AMD Ryzen 7 9700X（8C/16T），31.2 GB RAM，C: 约 1 TB 空闲。

**工具链**（本机实测）：
`rustc 1.99.1 (58cf8f9c3 2026-10-13) x86_64-pc-windows-gnu`，`cargo 1.99.1`，MinGW-w64（WinLibs UCRT POSIX，gcc 在 PATH），`node v24.19.0` / `npm 11.17.0`。

**构建目录**：`CARGO_TARGET_DIR=C:\Users\nirvana\Documents\RDM\target-qa`（独立 QA 构建目录，不与他人 target 混用；debug GUI 产物约 520 MB，target-qa 峰值约 6.6 GB）。
所有命令均在 PATH 前缀加入 WinLibs mingw64\bin 与 `%USERPROFILE%\.cargo\bin` 后执行。

**独立性**：引擎行为由我另写的 harness（`target-qa/qa-harness/`，独立 workspace，仅通过 path 依赖项目 crate，**不修改项目任何文件**）用 fixture 的进程内 `stats()`（请求数 / body 字节 / Range 头）断言；另用 release `rdm.exe` 走真实 CLI 复测 1v8 与崩溃恢复，两条路径互相交叉验证（结果一致）。
GUI e2e 脚本为 `scripts/e2e.ps1`（已随冻结提交入库），使用隔离 `LOCALAPPDATA=%TEMP%\rdm-qa-localappdata`，不触碰真实 `%LOCALAPPDATA%\RDM`。

---

## 2. 构建与测试矩阵（验证时点 `b8dfc88`）

### 2.1 构建

| 命令 | 结果 |
|---|---|
| `cargo build --workspace`（target-qa） | **PASS**，exit 0（首次冷构建 2m45s；冻结时点增量 7s） |
| `cargo build --release -p rdm-cli -p rdm-fixture-server` | **PASS**，exit 0（冷构建 43.5s） |
| `cargo build -p rdm-gui` | **PASS**，exit 0（GPUI debug 520 MB；`build.rs` manifest workaround 生效，未做任何特殊处理） |
| 产物 | `rdm.exe`、`rdm-native.exe`、`rdm-gui.exe`、`rdm-fixture-server.exe` 均生成 |

### 2.2 测试矩阵（`cargo test`，验证时点 `b8dfc88`）

| 套件 | 测试数 | 结果 | 备注 |
|---|---:|---|---|
| `rdm-core` 单元测试 | 18 | **18 passed** | 1.27 s |
| `rdm-core` 集成测试 `tests/engine.rs` | 12 | **12 passed** | 1.79 s（并行）；覆盖架构 §3.6 九项 + probe 边界 |
| `rdm-ipc` 单元测试 | 2 | **2 passed** | — |
| `rdm-native` 单元测试 | 53 | **53 passed** | 0.21 s（含 01:37 修复后的 GUI 探测注入测试，不再依赖本机真实 GUI） |
| `rdm-native` 集成测试 `native_smoke.rs` | 5 | **5 passed** | 真实 exe + 管道 stdio + mock IPC；含凭据不落盘断言 |
| `rdm-gui` 单元测试 | 48 | **48 passed** | — |
| `rdm-fixture-server` 单元测试 | 2 | **2 passed** | — |
| doc-tests | 0 | **0 passed** | — |
| **合计** | **140** | **0 failed** | 与 Lead 的冻结验证一致 |

`rdm-core` 集成测试逐项单独运行墙钟（**测量于 01:36 快照**；`checksum` 测试随后被 core-dev 改名为 `checksum_mismatch_leaves_no_files` 并改为"零残留"断言，最终套件已全绿）：

| 测试（当前名） | 单独运行墙钟 |
|---|---:|
| `global_rate_limit_is_respected` | 2,030 ms |
| `etag_change_invalidates_resume` | 1,565 ms |
| `resume_only_downloads_missing_chunks` | 1,408 ms |
| `concurrency_beats_single_connection` | 860 ms |
| `retries_429_with_retry_after` | 454 ms |
| `multi_connection_matches_single_connection` | 318 ms |
| `range_ignored_falls_back_to_single_stream` | 318 ms |
| `unknown_length_uses_single_stream` | 291 ms |
| `probe_falls_back_to_ranged_get_without_accept_ranges` | 268 ms |
| `probe_reports_metadata_and_follows_redirects` | 264 ms |
| `content_disposition_filename_is_used` | 254 ms |
| `checksum_mismatch_leaves_no_files`（当时名 `…does_not_rename`） | 248 ms |

### 2.3 静态检查

| 检查 | 结果 |
|---|---|
| `cargo fmt --all --check` | **PASS，0 diff**（冻结时点实测两次均 0）；开发中期（01:40）曾 **FAIL：44 处 diff / 9 个文件**（8 个 `rdm-gui` 文件 + `rdm-ipc/src/lib.rs`，其中后者在提交 `4d04ba1` 就未格式化），已由 Lead 在冻结前统一修复 |
| `cargo clippy -p rdm-core -p rdm-cli -p rdm-ipc --all-targets` | **PASS：0 error / 0 warning**（开发中期曾有 1 个 `clippy::large_enum_variant`（`rdm-ipc::Request::Enqueue` 320 B），冻结前已处理） |

> 结论：**开发过程中发现的 fmt/clippy 问题均在冻结前修复**，冻结提交的交付质量达标。

---

## 3. 引擎核心行为（独立验证）

> 构建：release（`target-qa/release`，含冻结源码）。**未使用**项目自带集成测试的断言作为数据来源；数字来自 harness 或 CLI 的独立测量。
> 测量时点：01:44–01:50（源码内容与冻结提交一致：rdm-core 最后一次改动 01:35，冻结提交只做格式化/文档）。

### 3.1 汇总表

| # | 场景 | 结果 | 关键数据 |
|---|---|---|---|
| 1 | probe 元数据（含 HEAD Content-Length 回归） | **PASS** | 64 MiB fixture：`total_bytes=67,108,864`（=实际大小，**不再为 0**）；redirect 探测同值；`ranges=true`；`filename=probe.bin`；ETag/Last-Modified/HTTP1.1 正确 |
| 2 | 1 vs 8 连接基准（64 MiB，20 ms/256 KiB，进程内 harness，5 次；冻结提交复跑） | **PASS** | 1c 中位 **7,933 ms / 8.13 MiB/s**（7,929/10,021/8,050/7,933/7,886，其中 10,021 为并发收尾时的瞬时离群值，中位数稳健）；8c 中位 **1,026 ms / 62.38 MiB/s**（1,017/1,014/1,038/1,026/1,026）；**加速比 7.73x**；10 次 SHA-256 全一致；fixture 见 240 个 `206`、全部 HTTP/1.1（冻结前同配置：7,970 ms / 1,024 ms / 7.78x） |
| 3 | 1 vs 8 连接基准（**真实 CLI** `rdm.exe --json`，同配置，5 次；冻结提交复跑） | **PASS** | 1c 中位 **7,972 ms / 8.03 MiB/s**（7,959/7,961/7,972/7,987/7,999）；8c 中位 **1,026 ms / 62.38 MiB/s**（1,021/1,023/1,026/1,027/1,027）；**加速比 7.77x**；全部输出 `sha256 = E5A54547…F60F` 一致、`http_version=HTTP/1.1`（冻结前同配置：7,995 ms / 1,027 ms / 7.78x） |
| 4 | Range 被忽略（服务器回 200）自动降级 | **PASS** | 8 MiB：`connections=1`、`range_supported=false`、`206=0`、`200=2`、SHA-256 与参考一致 |
| 5 | 暂停→恢复只补缺失块（hi 精度夹具统计） | **PASS** | 8 MiB / 1 MiB 块 / 2 连接：暂停于 **2,097,152 B**（完成 2/8 块）；恢复 `resumed_chunks=2`；**第二次运行线上 body = 6,291,456 B = 缺失字节，差值 0**；Range 请求 6 = 缺失块数；`.part` 预分配 = 8,388,608 B；最终哈希正确 |
| 6 | 崩溃恢复（CLI `kill -Force`，非优雅暂停） | **PASS** | 64 MiB / 8 连接：kill 于完成 **16/32 块**（状态文件 `downloaded_bytes=33,554,432` 有效）；再次运行 `resumed_chunks=16/32`，exit 0，哈希 `E5A54547…F60F` 正确 |
| 7 | ETag 变化 → 恢复失效、从零重下 | **PASS** | 暂停于 2,097,152 B 后服务端 ETag v1→v2：`resumed_chunks=0`；第二次线上 body = **8,388,608 B（=全长）**；新 Range 从 0 开始；哈希正确 |
| 8 | 错误 SHA-256 | **PASS** | 返回 `ChecksumMismatch`（expected 64 个 0，actual 64 hex）；**目标文件、`.part`、`.part.json` 一个都不存在**（零残留契约，transfer.rs `verify_checksum` 有明确注释） |
| 9 | 全局限速（2 MiB/s × 8 连接） | **PASS** | 8 MiB：`summary.avg_speed=2,220,825 B/s` = **2.118 MiB/s（1.06x 配置值，+5.9%）**；墙钟 3.78 s（理论 4.0 s）；哈希正确；阈值 1.5x 内 |

### 3.2 CLI 契约（真实命令）

| 检查 | 结果 |
|---|---|
| 成功下载退出码 | `0`；`--json` 的 completed 事件含 `path/total_bytes/elapsed_ms/avg_speed_bps/connections/http_version/resumed_chunks/range_supported/sha256` |
| 校验失败退出码 | `2`，`{"event":"failed","error":"checksum mismatch: …"}`，无输出文件 |
| `probe --json` | 字段与 `RemoteMeta` 契约一致；HEAD 探测 `total_bytes` 正确 |
| Ctrl-C → 退出码 3 | **未自动化验证**（需要交互式控制台信号）；代码审查：`error.is_control_flow() → ExitCode::from(3)`，Ctrl-C 处理调用 `handle.pause()` 保存状态 |

### 3.3 与 README §05 的交叉对照

README §05 引用 core-dev 在 **debug 构建、32 MiB、30 ms/256 KiB** 下的 3 次数据（1c 5.05–5.12 s、8c 0.639–0.709 s、约 7.5x）。我的 release 数据（64 MiB、20 ms/256 KiB）为 **7.77x（冻结提交复跑；此前 7.78x）**，加速比同一量级；**两者配置不同，不可逐字节对比**，但"8 连接 ≈ 7.5–7.8x、单/多连接哈希一致、均 HTTP/1.1"的结论一致。

---

## 4. GUI 端到端（隔离环境，22/22 PASS）

脚本：`scripts/e2e.ps1`（`pwsh -NoProfile -File scripts/e2e.ps1`），隔离 `LOCALAPPDATA`，只用自己启动的 PID，结束删除隔离状态目录。被测二进制：`target-qa/debug/rdm-gui.exe`（构建于 01:47:26；GUI 源码最后改动 01:43，冻结提交相对其仅有格式化差异）。fixture：`target-qa/release/rdm-fixture-server.exe --port 18131 --size 64M --delay-ms 50`。

| # | 断言 | 结果 | 数据 |
|---|---|---|---|
| 1 | ipc.json 发布且 pid/协议/token 正确 | PASS | port=53583，pid 与 GUI 进程一致，token 64 hex，protocol=1 |
| 2 | IPC ping | PASS | version 0.1.0、protocol 1、gui_running=true |
| 3 | 初始 status 为空 | PASS | downloads=[] |
| 4 | 坏 token 被拒 | PASS | `ok:false, error:"unauthorized"` |
| 5 | enqueue 64 MiB（带 expected_sha256） | PASS | 分配 `dl_000001` |
| 6 | progress → completed | PASS | 67,108,864 / 67,108,864；过程中观察到 downloading 与 downloaded 增长 |
| 7 | 目标文件哈希 | PASS | `E5A54547…F60F` = CLI 独立参考下载 |
| 8 | downloads.json 持久化 completed + sha256 | PASS | state=completed、sha256 与文件一致 |
| 9 | pause 中途生效 | PASS | 暂停于 8,388,608 / 67,108,864 B（status + downloads.json 均 paused） |
| 10 | 暂停状态已持久化 | PASS | downloads.json: state=paused, downloaded=8,388,608 |
| 11 | 截图 | PASS | `docs/evidence/rdm-gui-final.png`（1477×989，44,840 B；已人工核对：完成/暂停两行、SHA-256、过滤器计数、速度图、状态栏） |
| 12 | resume → completed | PASS | 67,108,864 / 67,108,864；最终哈希一致 |
| 13 | 单实例 `rdm-gui.exe <URL>` 投递 | PASS | 第二进程 exit 0，stdout「RDM 已在运行，URL 已投递（download_id=dl_000003）」 |
| 14 | 单实例投递后 status 出现新项 | PASS | `dl_000003` downloading |
| 15 | cancel | PASS | state=cancelled |
| 16 | 仅有 1 个 GUI 实例存活 | PASS | instances=1 |

补充测量：GUI 启动→发布 ipc.json **378 ms**；空载 WorkingSet **69.1 MB**（Private 92.6 MB，82 线程）。→ README §04「debug 构建实测启动 ~1s、空载 ~70MB」**准确**。

其他：
- GUI 正常关闭删除 `ipc.json`：**未自动化验证**（本机 `CloseMainWindow` 无法定位 GPUI 窗口句柄）；代码审查 `main.rs`：事件循环退出后 `ipc_server::cleanup_info_file()`，符合协议文档。
- 单实例与 downloads.json 重启恢复（进行中→暂停）由 GUI 单元测试覆盖（48 项含 `downloads_roundtrip_recovers_active_as_paused`）。
- **队内证据（未独立复跑）**：firefox-dev 用隔离 LOCALAPPDATA 跑过 native host→真实 GUI 的端到端（enqueued→downloading→completed、8 MiB 哈希 `68f56485…`、spool 空、日志无 cookie），我未复跑以避免 GUI 窗口/进程名冲突。

---

## 5. 浏览器侧（Firefox 扩展 + native host）

| 检查 | 结果 | 细节 |
|---|---|---|
| `node --check` background/popup/options/lib/pure | **PASS** | 5 个 JS 文件全部语法通过 |
| `node extension/firefox/test/pure.test.js` | **PASS** | 12/12 |
| `node extension/firefox/test/background.test.js` | **PASS** | 8/8（含 native 缺失降级、data:/blob: 跳过、设置归一化等） |
| `node --test "extension/firefox/test/*.test.js"` | **PASS** | **20/20** |
| `npx web-ext@8 lint --source-dir extension/firefox` | **PASS** | **errors 0 / warnings 0**，notices 2（`MISSING_DATA_COLLECTION_PERMISSIONS`：AMO 未来要求，README 已列为上架前置；`FLAGGED_FILE_TYPE`：图标二进制复核提示，非错误） |
| `manifest.json` 字段审计（脚本逐项断言） | **PASS** | MV3；`gecko.id=rdm@rdm.download`；`strict_min_version=128.0`；Firefox 正确使用 `background.scripts`（非 service_worker）；7 项权限（alarms/contextMenus/cookies/downloads/nativeMessaging/storage/webRequest）与实际调用的 `browser.*` API 一一对应；`host_permissions` http/https；4 个尺寸 icon、popup/options 文件均存在；background 脚本文件存在 |
| `cargo test -p rdm-native` | **PASS** | 53 lib + 5 native_smoke；覆盖 1 MiB 分帧上限、超长/截断/非法 JSON、ping/enqueue、spool 落盘与凭据剥离、日志脱敏、`--version` 干净退出 |
| 宿主分帧实现审查 | **PASS** | `MAX_FRAME_BYTES = 1024*1024`；u32 LE 前缀；`--version` 实测输出 `rdm-native 0.1.0` |
| 安装/卸载脚本 | **PASS（仅语法/内容审查）** | PowerShell Parser 语法通过；只写 HKCU + `%LOCALAPPDATA%\RDM`；manifest 字段（name/path 绝对路径/type=stdio/allowed_extensions）与 `docs/extension-protocol.md` 一致；含扩展 ID 一致性检查与 `--version` 冒烟。**未实际执行**（会写注册表，按任务要求不做）；脚本不支持 `-WhatIf`，建议未来加。 |
| 真实 Firefox 接管 | **未验证** | 需要真实浏览器交互；README 已如实标注"需人工验证" |

---

## 6. 文档一致性审计

### 6.1 与实测一致（抽查通过）

- README §05 性能/行为数字：量级与结论和我的独立 release 数据一致（见 §3.3）；"checksum 不残留"、"Range 忽略降级"、"HTTP/1.1 分段"均被独立复现。
- README §04「debug 启动 ~1s、空载 ~70 MB」→ 实测 378 ms / 69.1 MB。
- README §06 快速开始：`cargo build -p rdm-cli -p rdm-native`、`cargo run -p rdm-cli -- probe … --json`、`cargo run -p rdm-cli -- download … -c 8 --json`、`cargo build -p rdm-gui` **全部真实执行成功**（probe 输出 `total_bytes=4194304`；download completed 事件正常）。
- README §09：`cargo test -p rdm-core`（18+12）正确；GUI 48 项、浏览器侧 53+5 / 20 JS、web-ext lint 0/0 与实测一致。
- `docs/architecture.md`：块大小公式（`clamp(total/(connections*4), 256KiB, 4MiB)`，代码一致）；状态落盘"每 500ms 或 16 块"（`SAVE_INTERVAL=500ms`、`SAVE_EVERY_CHUNKS=16`）；连接/读超时 15s/60s；`Accept-Encoding: identity`；分段强制 HTTP/1.1；重试 6 次 1s→32s；`MAX_CONNECTIONS=32`；状态文件字段逐一对应；GUI 60 个速度采样点、250ms tick。
- `docs/extension-protocol.md`：1 MiB 分帧、token 长度/鉴权、spool 命名 `{unix_ms}-{pid}-{seq}.json` 与 `queue/rejected/`、`credentials_stripped` 响应字段、5 个错误码、http(s) 限制、Cookie 不写日志——均与代码/测试一致。**文档已包含 v1 控制方法"重复 id 键"的在线格式警告**（我在 e2e 中实际踩到并按该格式构造后通过）。

### 6.2 需要修改的表述/数字（给 Lead 的精确清单）

| 位置 | 现状 | 建议 |
|---|---|---|
| `README.md:84` | "`web-ext lint` 0 error、**19 项 JS 测试**、**47+5 项 Rust 测试**…" | 改为 **20 项 JS 测试、53+5 项 Rust 测试**（与同文件 197/206 行一致；实测 native 53+5、JS 20） |
| `README.md:197` | `node --test extension/firefox/test/     # 19 项 JS 测试` | 该命令在 Node v24.19.0 下**直接失败**：`Cannot find module …\test`（目录被当作模块）。改为 `node --test "extension/firefox/test/*.test.js"`（实测 20/20）或写成两个文件路径；注释同步 20 项 |
| `README.md:159` | "GUI 未运行时会自动拉起（**8s 轮询**）" | 建议"8s 超时、每 200ms 轮询"（`GUI_START_TIMEOUT=8s`，`POLL_INTERVAL=200ms`） |
| `docs/architecture.md` §3.3 | "流式读入 **64 KiB** 缓冲 … 每 64 KiB 检查取消" | 实现为 `response.chunk()`（reqwest 网络块）+ `tokio::select!` 每块检查取消/暂停，**没有固定 64 KiB 常量**；建议改为"按网络分块流式读取（每次读取后检查取消/暂停）"，或补上真实常量 |
| `extension/firefox/README.md` | 已写 12 项 + 8 项 = 20 项，且给出正确 node --test 命令 | 无需改（可作为 README 197 行的修正参考） |

> 另有 2 处**措辞风险**（不必改代码，已知限制已覆盖）：README §04 行 3 的"headless Firefox 临时加载、安装脚本实测"——本次 QA 未复跑真实 Firefox 加载与安装脚本（后者会写注册表）；如需对外宣传建议标注"由扩展作者自测"。README §05 断点恢复举例 `resumed_chunks=8/32` 与我 kill 时机不同（16/32），属示例性数字，保留可接受。

---

## 7. 未验证 / 风险清单

1. **真实 Firefox 浏览器接管**未做（无浏览器自动化；仅扩展静态检查 + native host 集成测试 + 队友的隔离 e2e 证据）。AMO 签名/`data_collection_permissions` 未做（README 已列）。
2. **GUI 正常关闭删除 ipc.json** 未自动化（自动化发 WM_CLOSE 未命中 GPUI 窗口句柄）；冻结后可用人工关闭一次验证。
3. **CLI Ctrl-C → exit 3 / 优雅暂停** 未自动化（需交互式控制台）；代码路径审查通过。
4. **release GUI 未构建验证**：GPUI release 着色器需 Windows SDK `fxc.exe`（本机未装）——README/architecture 已声明，交付以 debug 验证。
5. **Linux/macOS** 未验证（README 已声明）；本报告全部结论仅对 Windows + GNU 工具链成立。
6. **HTTP/2 单流路径**未实测：fixture 只有 HTTP/1.1；"分段强制 h1、单流允许 h2"中 h1 已实测（240 个 206 全 h1），h2 属代码审查。
7. **安装脚本**未真实执行（会写 HKCU）；仅语法 + 内容 + 字段一致性审查。不支持 `-WhatIf`。
8. **外部调研数据**（GitHub star 数、竞品行为）未独立复核（调研文档自述快照时间 2026-10-19 01:16–01:20，QA 无法离线重放 GitHub API 全量快照；属研究交付物范畴）。
9. `rdm-ipc` v1 控制方法需**手工拼接重复 `id` 键的原始 JSON**（`docs/extension-protocol.md` §2.2 已注明；`parse_envelope` 有单测），第三方集成需注意；v2 计划改 `download_id`。
10. 本次 QA 期间源码多次变动（01:30→01:47），§2/§3 数字对应的最后源码改动时间已在各节标注；**冻结提交 `b8dfc88` 相对我的 debug GUI 构建仅有格式化差异**（GUI 源码 01:43 后未再改）。

---

## 8. 复现与产物

**复现命令**（在仓库根目录，PATH 加 MinGW+cargo，`CARGO_TARGET_DIR=…\target-qa`）：

```powershell
cargo build --workspace
cargo test -p rdm-core ; cargo test -p rdm-ipc -p rdm-native -p rdm-gui
cargo fmt --all --check
cargo clippy -p rdm-core -p rdm-cli -p rdm-ipc --all-targets
# 基准（release）：CLI 1v8（64 MiB / 20 ms / 5 次）
pwsh -File target-qa/bench.ps1 -SizeMB 64 -DelayMs 20 -Runs 5 -Port 18130
# 引擎行为 harness（release）：probe/bench/fallback/resume/etag/checksum/rate
target-qa/release/rdm-qa-harness.exe
# 崩溃恢复：kill -Force 后 resume
pwsh -File target-qa/kill-resume.ps1
# GUI e2e（隔离 LOCALAPPDATA，22 项断言）
pwsh -File scripts/e2e.ps1 -SizeMB 64 -Port 18131
# 浏览器侧
node --test "extension/firefox/test/*.test.js"
npx web-ext lint --source-dir extension/firefox
```

**本报告引用的结果文件**（`target-qa/`，未提交）：
`bench-results.json`、`qa-harness-results-frozen.json`、`kill-resume-results.json`、`gui-e2e-results.json`、`gui-e2e-artifacts/`（GUI stderr/downloads.json/单实例日志）、`qa-logs/`（各命令完整日志）。

**截图**：`docs/evidence/rdm-gui-final.png`（READY，随冻结提交入库，本报告 §4 已人工核对）。

**清理**：所有 fixture/GUI/CLI 进程均为 QA 自己启动并按 PID 停止（结束检查 `rdm-gui`/`rdm-fixture-server`/`rdm` 均为 0）；隔离 `%TEMP%` 目录已删除；未触碰其他 agent 的进程或真实 `%LOCALAPPDATA%\RDM`。qa 构建目录 `target-qa/` 保留（未提交）。

---

*报告生成：2026-10-19 02:05（Asia/Shanghai）｜验证 commit：`b8dfc88`｜QA 独立执行，未修改源码。*


