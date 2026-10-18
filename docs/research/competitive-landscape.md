# 开源多线程下载器竞争格局与 RDM 差异化调研

> **调研日期**：2026-10-19（Asia/Shanghai）｜**调研人**：RDM 研究分析师
> **数据口径**：GitHub REST API v3（未认证，2026-10-19 01:16–01:20 +08:00 抓取）。`stars` / `open_issues` 为 API 返回值；`最近提交` 使用 `pushed_at`（任意分支最近一次 push，**不等同**默认分支 HEAD commit 时间）；`open_issues_count` **包含 PR**。闭源产品无法审计，标注方式见各行。抓取命令与原始值见 §8。
> **状态快照（重要，随并行开发快速变化）**：本报告的"当前证据 / 可执行性"基于 **2026-10-19 01:16–01:20 快照**。
> - 该时点：`rdm-core` 仅有公开类型与 API 契约（`probe`/`download` 返回 `Unsupported`，引擎为桩）；`rdm-ipc` 有线协议类型 + 阻塞客户端（2 个单测）；`rdm-gui` 仅可开窗骨架；`docs/` 两份契约；CLI / native host / fixture server / Firefox 扩展均为桩或缺失。
> - **定稿复核（01:22–01:24）**：仓库正被并行实现——`rdm-core` 新增 `client/event/filename/plan/rate/state` 模块（`lib.rs` 已把 `probe`/`download` 接到 `probe::probe_with`/`transfer::run`，但复核瞬间 `probe.rs`/`transfer.rs` 尚未落盘）；`rdm-fixture-server` 已出现约 29 KB 实现（`without_ranges`/`with_delay`/`with_abort_on`/`with_retry_after(429)`/`Stats` 等故障注入 API）；`rdm-native` 出现 framing/protocol/spool/gui 模块；`rdm-gui` 出现 42 KB `model.rs`；CLI、`rdm-native/main.rs`、`extension/` 仍为桩/缺失；**仓库内尚无 `tests/` 集成测试目录**。
> - **本轮未执行 `cargo build` / `cargo test`**：以上仅是文件存在性观察，**不表示可编译、可运行或测试通过**。§5 的【契约】/【未实现】标注按 01:20 快照给出，未逐一回写 01:24 之后的变化，引用前请以当时 commit 重新核对（复核对照见 §5.2）。
> - **对外材料纪律**：在 CI 通过前，不得写"已实现 / 已测试 / 比 X 快"；本报告只作竞品与设计依据，不背书本仓库实现状态。

---

## 0. 一页结论（TL;DR）

1. **同名查证结论：名字已被占用，且存在高度同题项目。** GitHub 搜索 `download in:name,description rdm` 共 **36** 个结果，其中至少 7 个是下载器：`rdm-dl/rdm`（Rust，MIT，**1★**，2026-10-13 创建、10-16 push；README 明示"默认 8 连接 + 动态拆分 + 浏览器扩展"，技术定位与本项目几乎重合）、`canmi21/rdm`（Rust，**24★**）、`SubhajitCode/rdm`（Rust，3★）、`MasterZack69/rdm`（Rust CLI，0★）、`Japerz12138/re-download-manager`（JS，2★）、`sudosz/rdm`（Rakhsh Download Manager，0★）、`roivy/rdm-super-downloader-release`（1★）。此外 "RDM" 缩写长期被 **Remote Desktop Manager**、**Redis Desktop Manager** 占用。我们计划使用的 `rdm-download/rdm` **目前 404 可用**，但"RDM"没有品牌护城河——README 与路演必须靠可验证事实，而不是名号。
2. **品类的头部不是"下载器"，而是"壳"或"协议引擎"**：Motrix（**57,012★**）= Electron + aria2 子进程；imfile（**4,526★**）= Motrix 的 fork，同架构；自研引擎的代表是 aria2（**43,167★**，C++/GPL-2.0，无官方 GUI）、Gopeed（**26,995★**，Go 内核 + Flutter UI）、Axel（**3,419★**，C CLI）、XDM（**8,018★**，Java/Kotlin）。
3. **"开源多线程下载器少"是伪命题，真问题是"活下来的少"。** Rust 生态 `download manager` 检索 **344** 个仓库，但 Top1 仅 **298★**（`OpenSelena/omniget`），≥150★ 只有 4 个，且大多 2026 年才出现（Tauri/Tokio 模板化）。长尾极长、头部极窄，**Rust 原生项目尚未有任何头部**。
4. **"为什么少"的证据强度排序**：服务器侧限制与连接数风控（Gopeed #1592/#1593 明确出现"服务器拒绝额外连接返回 403 / chunk 被 403 拒绝"）> 浏览器集成成本（DownThemAll 转 WebExtension 后多线程下载不再可能——公开资料/百科转引，原始出处待核；uGet 必须单独维护 `uget-integrator` 原生宿主，349★ 但 2019 年后停更）> 恢复/校验正确性 > 商业与维护成本（Persepolis 距最近 push 已近一年；imfile README 直接把 fork 动机写成"Motrix 长期未更新、issue 堆积"）> 法律合规。
5. **最反直觉发现：Electron + aria2 成为事实标准，是"法律 + 工程"双捷径。** aria2 是 GPL-2.0；以**独立子进程 + JSON-RPC** 使用它，既避免 GPL 传染（进程隔离而非链接），又免费获得完整的 HTTP/FTP/BT 引擎；UI 用 Web 技术复用。代价是体积/内存/供应链/攻击面：aria2 最新 issue 中就有 **#2402「未鉴权 JSON-RPC → 任意文件写入 → RCE」**、#2400「BT `piece_length=0` 触发 SIGFPE」；Motrix 侧则有 **#2354「大文件每次卡死 99%」**。——这解释了这个组合为什么"能起来"，也解释了它为什么"做不精"。
6. **第二反直觉：多线程不是免费的，HTTP/2 可能是性能陷阱。** 若 8 个 Range 请求经 HTTP/2 复用**同一条 TCP 连接**，它们共享同一个拥塞窗口，理论上带宽增益≈0（只是消除请求串行）。RDM 架构 §3.5 明确写了"h2 时 N 个流复用同一 TCP 连接，仍能并发"——**这是设计风险，不是优势**。基准 B1 必须加 h1/h2 对照组，分段模式应显式优先 HTTP/1.1 多连接。
7. **RDM 的真实差异化不在功能清单（全是行业标配），而在四件事**：①**可验证的工程正确性**（fixture 故障注入矩阵 + 可复现基准，竞品几乎没有公开故障矩阵）；②**无外部进程的原生栈**（启动/内存/供应链，但**必须实测**才有说服力，且 Gopeed 的原生 Go+Flutter 部分抵消"非 Electron"叙事）；③**Firefox-first 集成空位**（多数产品 Chromium-first，Firefox native messaging 安装摩擦恰好是护城河）；④**诚实降级语义**（Range 不支持时明确提示，不假装加速）。
8. **建议路演卖点排序**：正确性（不损坏、可恢复、可复现测试）> 原生栈实测数据（启动/内存/体积）> Firefox 空位 > Rust 叙事。**不要把"多线程下载"本身当卖点**——1990 年代的属性，竞品全有。

---

## 1. 开源/免费下载器全景（2026-10-19 实测数据）

### 1.1 主表

| 项目 | 语言 / 许可证 | 最近提交（pushed_at） | Stars | 核心架构 | 真·多线程分段 | 主要维护风险 |
|---|---|---|---|---|---|---|
| **aria2** | C++ / GPL-2.0 | 2026-06-25 | **43,167** | 自研引擎；HTTP/FTP/BT/Metalink；JSON-RPC 守护进程；无官方 GUI | **是**（但 `-x/--max-connection-per-server` **默认 1**，需显式开启；`--split` 默认 5 受此约束） | 维护者带宽与安全面：1182 open（含 PR）；#2402 未鉴权 RPC RCE、#2400 BT 解析崩溃；GPL 传染性；无官方 GUI 生态碎片化 |
| **Motrix** | TypeScript(Electron) / MIT（API 未识别 SPDX） | 2026-10-18 | **57,012** | **Electron + aria2 子进程**（README 明示 Electron/Aria2/BitTorrent） | 是（能力来自 aria2，Motrix 只是配置壳） | 曾长期停滞（imfile 的 fork 理由即"长期未更新、issue 堆积"）；#2354 大文件卡死 99%；Electron 体积/内存；aria2 二进制与 RPC 供应链/安全面 |
| **Gopeed** | Go 内核 + Flutter UI（仓库语言字段 Dart）/ GPL-3.0 | 2026-10-18 | **26,995** | **自研引擎**（根目录 `go.mod`/`cmd`/`internal`/`pkg` + `ui`），非 aria2 | 是（自研 chunk 并发；issue 讨论 chunk 级 403 重试） | 安全类 issue 密集：#1590 BT 路径穿越、#1589 未鉴权 `/api/web/proxy` 开放代理 SSRF、#1591 配置损坏；349 open；跨端一致性 |
| **uGet** | C/GTK（主库在 SourceForge；GitHub 为发布镜像）/ GPL-2.0（项目声明，API 未识别） | 2024-10-04（GitHub 镜像；主库活跃度未核） | 151（主仓）/ **349**（`uget-integrator`） | 自研引擎 + 插件后端（aria2/curl）；浏览器集成需单独装 native messaging 宿主 | 部分（多连接配置存在；最成熟路径是 aria2 插件，按版本核） | 事实上的半停滞；`uget-integrator` 2019-01 后无 push；依赖独立安装器，用户转化链路长 |
| **Persepolis** | Python(PyQt) / GPL-3.0 | 2025-10-09（**近一年无 push**） | 7,475 | PyQt GUI + **aria2 后端** | 是（来自 aria2） | 单人/小团队维护；319 open；更新停滞；Linux 桌面生态萎缩 |
| **DownThemAll** | TypeScript(WebExtension) / 未识别(NOASSERTION) | 2026-10-17 | 1,121 | 纯浏览器扩展（MV3），无 native 加速 | **否**——官方/百科口径："Multithreaded downloading is no longer possible with the transition to the WebExtension version"（同时丢失 checksum/metalink） | 浏览器下载 API 回归频繁：#619 文件只有 0.9KB、#616 文件名全变 `true`；无原生能力后只剩批量抓取价值 |
| **Axel** | C / GPL-2.0 | 2026-09-30 | 3,419 | CLI 自研 HTTP/FTP 分段下载器（经典 `-n` 多连接） | **是**（教科书式实现） | 仅 CLI、无浏览器集成/无 GUI；87 open；小团队维护 |
| **wget2** | C / LGPL-3.0 | GitHub 镜像 2019-05-17（**镜像停更**，主库在 GitLab） | 15（GitHub 镜像，无参考意义） | GNU 自研；HTTP/2、多线程、Metalink | 是（`--max-threads` 等） | 主库不在 GitHub、发行版采用慢；定位 CLI/镜像站，非桌面产品 |
| **yt-dlp** | Python / Unlicense | 2026-10-10 | **199,134** | 流媒体解析器 + 分片下载（HLS/DASH），可外接 aria2/ffmpeg | 部分（并发分片；不是通用 HTTP Range 分段器） | 与站点反爬持续对抗（高频发版）；法务前史（RIAA/DMCA）；不解决"普通大文件"场景 |
| **imfile** | JavaScript(Electron) / MIT | 2026-10-18 | 4,526 | **Motrix fork**（README 明示），Electron + aria2 | 是（继承 aria2） | 与 Motrix 同源同债；Electron 体积；仅 39 open 说明"壳+aria2"复刻门槛确实低 |
| **XDM (Xtreme Download Manager)** | Kotlin/Java / GPL-3.0 | 2026-10-17 | 8,018 | 自研 Java 桌面端 + 浏览器扩展 | 是 | 865 open；JVM 桌面打包/多平台重；浏览器商店与 native 集成限制 |
| **NDM (Neat Download Manager)** | **闭源免费**（官网发布，未找到公开源码仓库） | 不适用（闭源） | 不适用 | 闭源，多连接分段 + 浏览器集成 | 是（产品宣称；不可审计） | 不可审计；跨平台支持以官网为准（本次未核）；闭源单点维护 |
| **Free Download Manager (FDM)** | **闭源免费**（FDM 5/6）；GitHub 仅有第三方 SVN 历史镜像 `mirror/freedownload`（C，37★，非官方、2016 年前代码） | 不适用（闭源） | 不适用 | 闭源，多连接分段 + 浏览器集成 | 是（产品宣称；不可审计） | 闭源；官网分发渠道与捆绑争议需自行核验；旧开源镜像不能代表现版本 |

> 说明：'真·多线程分段'指"对同一资源并发多个 Range 请求并写同一文件"，而非"同时下载多个文件"（并发队列是行业标配，不构成差异化）。

### 1.2 逐项要点（用于 README/路演引用）

- **aria2 是"协议引擎"，不是产品**：能力最强、生态最广（AriaNg、Motrix、Persepolis、uGet 插件、各类 NAS 面板都接它），但**默认极保守**（`-x` 默认 1），因为它首先服务于"不惹服务器"的批量/镜像场景。**"默认 8 连接"本身就是与 aria2 默认值的产品级差异**，但这是配置差异，不是技术壁垒。
- **Motrix/imfile 证明"壳+aria2"可以做到 5 万星**：这既证明需求真实存在，也证明**头部产品的护城河不是技术**。imfile 的 README 原话（fork 动机）是竞品维护风险的最佳证据。
- **Gopeed 是新一代"自研引擎 + 现代 UI + 跨端"开源项目里最强的一个**：Go 内核 + Flutter，协议覆盖比 RDM 宽（含 BT），代价是安全 issue 密集（SSRF/路径穿越/配置损坏）。它是 RDM 最值得对标的"技术型竞品"（XDM 同为自研引擎但 JVM 老栈；Axel 仅 CLI）。
- **DownThemAll 的退化是"浏览器内多线程"路线的死亡证明**：公开资料记载 MV2→WebExtension 后其放弃多线程分段与校验（原始出处待核）。**任何"浏览器插件做多线程"的方案在今天都不成立**，必须 native 进程——这正是 RDM 的架构前提。
- **Persepolis/uGet 是"GUI over aria2"的生命周期样本**：都能用、都缺人、都停在半路。**维护成本 > 开发成本**是本品类第一规律。
- **FDM/NDM 的存在说明商业闭环在闭源侧**：免费闭源靠捆绑/导流/付费版变现；开源侧只能靠捐赠/周边（Gopeed 有 Pro 商业化尝试）。RDM 若无商业模式设计，需默认"社区项目"预期。
- **yt-dlp 的 199k★ 说明需求迁移**：用户要的是"拿到想看的内容"，不是"更快下载一个大文件"。通用下载器必须接受自己是**基础设施**而非流量入口。

### 1.3 Rust 生态：344 个仓库的长尾（含同名 RDM 查证）

GitHub 搜索 `download manager language:rust`：**total_count = 344**；按 stars 排序 Top 15（2026-10-19 抓取）：

| 仓库 | Stars | 最近 push | 说明 |
|---|---:|---|---|
| `OpenSelena/omniget` | 298 | 2026-10-18 | Tauri 2.0 + Rust + Svelte，课程/视频下载器（非通用 HTTP 分段） |
| `nimbold/Firelink` | 210 | 2026-10-18 | MIT；Rust + Tauri + React；topics 含 `aria2`、`yt-dlp`、`segmented-downloads`——**Rust 外壳 + 外部引擎**路线 |
| `rgreinho/trauma` | 179 | 2026-09-27 | MIT；Tokio 异步下载管理器（CLI 向） |
| `PBhadoo/QDM` | 115 | 2026-03-27 | 跨平台 GUI 下载器 |
| `YueMiyuki/Risuko` | 110 | 2026-10-18 | 全功能下载器 |
| `agourlay/dlm` | 108 | 2026-10-12 | Apache-2.0；极简 HTTP 下载器（CLI，工程参考价值高） |
| `gnipbao/FFDownloadManager` | 61 | 2026-10-18 | Rust 桌面下载器 |
| `sumoduduk/terminusdm` | 52 | 2024-08-22 | 终端 TUI 下载器 |
| `davimf721/KGet` | 50 | 2026-05-25 | Rust CLI + 原生应用 |
| `pomagrenate/fetchr` | 1 | 2026-09-11 | "adaptive parallel range requests / resume recovery"——与 RDM 同题的新仓库 |
| **`rdm-dl/rdm`** | **1** | **2026-10-16** | **同名同题**：Rust/MIT，topics `browser-extension,cross-platform,dash,download-manager,downloader,egui,hls,rust`；README：默认 8 连接、"空闲连接接手剩余工作量最大分段的一半"、浏览器扩展、站点抓取、网页 rdm-dl.github.io |
| `canmi21/rdm` | 24 | 2026-10-04 | Rust，"A refined download manager" |
| `SubhajitCode/rdm` | 3 | 2026-09-26 | "blazing fast file download manager written in rust" |
| `MasterZack69/rdm` | 0 | 2026-10-07 | "Rust Download Manager - CLI" |
| `Japerz12138/re-download-manager` | 2 | 2025-12-30 | JS，"Official repository for RDM" |

**同名查证补充**：
- 我们 `Cargo.toml` 中的预期仓库地址 `https://github.com/rdm-download/rdm` **返回 404（当前可用）**；但 `rdm-dl/rdm` 的命名空间与定位高度重合，**"RDM = Rust Download Manager" 已不是独占命名**。
- "RDM" 在其他领域已有强势占位：Devolutions **Remote Desktop Manager**（商业）、**Redis Desktop Manager**（RESP.app）。SEO 与搜索结果会被稀释。
- 结论：**名称可用但无保护**；对外应始终写全 "RDM — Rust Download Manager" 并配一句可验证定位（见 §5.4），不要依赖缩写。

---

## 2. 为什么"能长期维护"的开源多线程下载器这么少

> 先修正问题本身：**不是"少"，而是"长尾极长、头部极窄、长期维护者极少"**（Rust 344 仓库 vs 头部 298★；Motrix 57k★ 是壳，aria2 43k★ 是引擎，真正的"自研引擎+活跃 GUI"开源项目只有 Gopeed 一个）。以下按机制拆解，标注置信度。

### 2.1 十二条机制与证据

1. **HTTP Range 只是"允许"，不是"保证"** —— 服务器可以合法忽略 `Range` 返回 `200` 全量；可以返回 `206` 但 `Content-Range` 与请求不符；可以只支持单区间；`Accept-Encoding: gzip` 会让字节偏移语义变复杂（Range 针对编码后表示）。任何竞品都必须实现"探测 → 200 则优雅降级"，这直接砍掉了大量 URL 的加速收益。**[高]** —— RFC 9110 §14；RDM 契约 `docs/architecture.md` §3.2/§3.3 已规定降级路径。
2. **服务器按连接数/速率风控是真实且高频的失败模式** —— Gopeed 最新 issue：#1592「服务器拒绝额外连接返回 403 时，下载永远停在 running」、#1593「重试被 connection-limited server 以 403 拒绝的 chunk」。这不是边缘 case，而是**多线程下载器的日常**。**[高]** —— GitHub issue 实证。
3. **CDN 边缘不一致 / 签名 URL 过期 / 防盗链** —— 同一对象在不同边缘节点可能内容不同（对象更新中的窗口期），签名 URL 有 TTL，`Referer`/Cookie/UA 任一缺失即 403；多连接把"同一请求"放大成 N 个可能落到不同边缘的请求，**拼接出损坏文件的风险随连接数上升**。开源头部的通用对策是：校验器（ETag/Last-Modified）+ 完成后哈希，**没有更好的通用解**。**[中]** —— 机制清晰、公开报告分散；RDM 契约 §3.4 采用同一策略。
4. **断点续传的正确性远比看起来难** —— 需要同时处理：强/弱 ETag 语义、`Last-Modified` 秒级精度、`If-Range`、416 的 `Content-Range: bytes */N`、状态文件与实际落盘的一致性（先写数据再标记完成，崩溃窗口内不能出现"已标记但未 flush"的块）、跨卷 rename（Unix `EXDEV`）。任何一个环节错，用户看到的是**静默损坏**——最伤口碑的 bug 类型。**[高]** —— RFC 9110 §13/§14；各项目 issue 长期主题。
5. **浏览器集成是"第二个产品"，不是功能** —— 三个硬约束叠加：(a) **MV3 后浏览器内无法做多线程分段**（DownThemAll 的公开资料口径；百科转引，原始出处待核），必须 native 进程；(b) native messaging 宿主**不能由扩展自动安装**，必须系统安装器写注册表/manifest（`uget-integrator` 就是为此单独存在的 349★ 项目，且 2019 年后停更）；(c) 商店审核 + 权限说明 + 企业策略。**[高]** —— DTA 百科口径、uGet 仓库、MDN native messaging 文档。
6. **安装摩擦直接杀死转化率** —— 即使宿主装好，Firefox 在 flatpak/snap 沙箱下还会找不到 `~/.mozilla/native-messaging-hosts`（NixOS #352248 即此类问题：需手动拷贝 manifest）。这意味着"下载器 + 浏览器接管"的完整链路在 Linux 上长期是半坏的。**[高]** —— NixOS issue 实证。
7. **跨平台 GUI/打包/签名/杀软误报是持续成本** —— Windows 需 Authenticode（EV 证书年费 + 硬件令牌/SmartScreen 声誉积累），macOS 需 notarization + hardened runtime + `get-task-allow`（RDM 架构已记录 GPUI release 需要 `fxc.exe` 编译着色器这类具体坑）；未签名新安装包的杀软误报与 SmartScreen 拦截会直接摧毁首装体验。**[中]** —— 行业共识与各项目 release issue，缺少单一权威统计；RDM 自身架构文档 §9 可作旁证。
8. **测试与故障复现极难** —— 需要可控的 fixture server（延迟/限速/丢块/返回 200/ETag 变化/签名过期/连接数拒绝），否则只能靠用户环境复现。**这正是 Motrix/Gopeed 类"99% 卡死""永远 running"bug 长期存在的原因**。开源头部的现实是：几乎所有项目都缺故障注入测试矩阵。**[高]** —— Motrix #2354、Gopeed #1592；RDM 契约 §3.6 把 fixture 列为必须项（但 fixture server 仍是桩）。
9. **商业化与维护成本** —— 用户预期"免费、无广告、比浏览器快"，付费意愿极低；闭源侧靠捆绑/导流（FDM/NDM）或买断（IDM）变现，开源侧只能捐赠/Pro（Gopeed 尝试）。直接后果：**项目死在"作者毕业/换工作"**。**[高]** —— Persepolis 近一年无 push；uGet 半停滞；imfile fork 动机原文。
10. **流媒体时代需求结构改变** —— 用户要的是"内容"而非"大文件传输"：yt-dlp **199,134★** vs aria2 **43,167★**、Motrix **57,012★**。HLS/DASH 天然是"多分片请求"，浏览器/播放器已内置；"通用 HTTP 分段下载器"退化为**基础设施**（ISO/驱动/数据集/网盘直链），用户感知弱、增长故事弱。**[高]** —— stars 对比；yt-dlp 采用率。
11. **法律与合规风险** —— 2020 年 RIAA 对 youtube-dl 的 DMCA 通知导致 GitHub 下架（后恢复），直接催生 yt-dlp fork；下载器天然被关联到"绕过付费/盗版"，商店与杀软风控更严。**纯 HTTP(S) 分段器风险显著低于视频站点解析器**，但仍需在条款/隐私/版权上保持克制。**[高]** —— RIAA/GitHub DMCA 事件为公开史实（链接见 §7）；产品边界选择见 RDM 架构 §1。
12. **Electron + aria2 组合的"事实标准"及其代价** —— 该组合同时解决四件事：完整协议引擎（aria2）、GPL 隔离（子进程 + RPC 而非链接）、UI 复用（Web 技术）、跨平台打包（electron-builder）。代价：**体积与内存**、**aria2 进程/二进制供应链**、**RPC 攻击面**（aria2 #2402 未鉴权 RPC RCE）、**壳层不可控 bug**（Motrix #2354）。**这是"为什么头部是壳"的答案，也是"为什么壳做不精"的答案。** **[中高]** —— 架构与 issue 均为实证；"GPL 隔离动机"为合理推断（未见厂商自述），标注为推断。

### 2.2 反直觉结论（可直接用于路演）

- **真正的稀缺资源不是"多线程代码"，而是"敢公开的故障测试矩阵 + 长期维护承诺"。** 多线程分段算法 200 行就能写出来；把它在 403/200/ETag 变更/断网/崩溃/杀软扫描下做对，才是护城河。
- **"头部产品"的技术护城河很浅**：Motrix 57k★ 的核心是 aria2；这说明**用户不为技术付费，为体验/信任付费**。RDM 若只宣传"纯 Rust 多线程"，是在最不稀缺的维度竞争。
- **"缺一个 Firefox 好用的下载器"是真实空位**：Chromium 侧有 IDM/NDM/Motrix/XDM 争夺，Firefox 侧 DownThemAll 已放弃多线程，uGet 半停滞。**这是 RDM 最低成本、最高确定性的差异化入口。**

---

## 3. 多线程下载器真正的技术难点清单

> 判定标准：能导致"静默损坏 / 不可恢复 / 用户可见错误 / 无法测试"的，才算难点。营销属性（"AI 加速""智能多线程"）不计入。

### 3.1 协议层

| 难点 | 难在哪 | 开源项目如何绕开 / 没做好 |
|---|---|---|
| Range 探测与降级 | HEAD 常缺 `Accept-Ranges`；`GET Range: bytes=0-0` 的 206/200 双分支；服务器可能对 HEAD 与 GET 行为不一致 | aria2 有成熟探测；多数 GUI 壳直接把 URL 丢给 aria2，**自己不处理**降级语义，UI 也不提示"未加速" |
| `Content-Range` 校验 | 服务器可能返回`206`但区间错/长度错；必须逐块校验并丢弃越界响应 | 不少实现只看 206 状态码；RDM 契约要求逐块校验（§3.3），**未实现** |
| `Accept-Encoding` 与字节偏移 | gzip/br 传输下 `Content-Length` 与实体长度不一致，Range 语义针对编码后表示；不强制 `identity` 会导致偏移错位 | 多数实现直接 `Accept-Encoding: identity`（RDM 契约如此）；不处理 br/zstd 的服务器可能仍压缩 |
| 校验器语义 | 强/弱 ETag、`Last-Modified` 秒级、`If-Range`、416 的 `bytes */N`、对象在下载中更新 | aria2 用 `If-Range` + 尺寸/时间戳；GUI 壳通常完全依赖 aria2，**跨会话恢复失败时用户只看到"重新下载"** |
| 签名 URL / 防盗链 / Cookie | URL 带一次性 token，N 个并发请求可能各自失效；Referer/UA/Cookie 组合缺一即 403；CDN 按 IP 限连接 | 头部策略是"从浏览器抓头 + 失败重试 + 降级单连接"；**没有通用解法**；Gopeed #1592/#1593 证明 403 处理仍会卡死 |
| HTTP/2 多路复用陷阱 | h2 下 N 个流共享 1 条 TCP 连接 → 共享拥塞窗口，**带宽不随"连接数"增长**；RDM 若按流数当连接数会得到"假多线程" | 多数基于 aria2 的产品不受影响（aria2 默认 HTTP/1.1 多 TCP）；**自研且用 reqwest 的项目最容易踩**（RDM 风险项） |
| 429/503 与 `Retry-After` | 需要指数退避 + 抖动 + 尊重 `Retry-After`；连接数越多越容易触发 | aria2 有退避；自研实现常缺抖动或全局风暴控制；RDM 契约 §3.3 有（未实现） |
| 重定向与最终 URL | 原始 URL 与 final URL 不同；恢复时用哪个？Cookie 作用域变化 | aria2 记录 final URL；RDM 契约记录 `final_url`（类型已有，逻辑未实现） |

### 3.2 并发与调度

| 难点 | 难在哪 | 开源项目如何绕开 / 没做好 |
|---|---|---|
| 固定分片 vs 工作窃取 | 固定 N 等分时，最慢分片决定总时长（CDN 边缘差异、跨洋链路）；需要"块级队列 + 空闲 worker 领块" | aria2 的 split/piece 选择已较成熟；**多数自研实现用固定等分**，尾延迟差；rdm-dl/rdm 宣称"空闲连接接手剩余最多分段的一半"（同为动态策略） |
| 块大小选择 | 太小→请求开销/调度抖动；太大→尾延迟、失败重下成本高 | aria2 有 `--min-split-size` 等；RDM 契约默认 4 MiB、下限 256 KiB（未实现） |
| 全局 vs 每连接限速 | 每连接令牌桶在 N 连接下会超速 N 倍；必须全局共享预算 | aria2 有 `--max-overall-download-limit`；自研实现常做错（基准 B3 专门测） |
| 事件风暴与背压 | 每 64 KiB 一个进度事件 × 8 连接 = 高频；`UnboundedSender` 无背压可能堆积 | 头部普遍靠 UI 节流；RDM 契约是 250ms 节流 + `UnboundedSender`（**背压设计需复核**） |
| 暂停/取消的竞态 | 取消时正在写入的块、状态落盘、worker 退出顺序；"暂停"不能变成"失败" | 各项目自研；RDM 契约用 CancellationToken + 原子标志（未实现） |
| 连接数自适应 | 服务器拒绝多余连接（403/429）时应降连接数而非死循环重试 | **普遍没做好**：Gopeed #1592 就是"403 后永远 running"；这是可量化的差异化点 |
| 单服务器 vs 多镜像 | aria2 支持多源（Metalink/多 URL），失败块换源 | 除 aria2 外基本没有；RDM 架构明确不做多镜像（§9） |

### 3.3 磁盘与持久化

| 难点 | 难在哪 | 开源项目如何绕开 / 没做好 |
|---|---|---|
| 随机写与预分配 | `pwrite` 在偏移处写；`set_len` 预分配；Windows `seek_write` 与 Unix `FileExt` 差异；稀疏文件与 NTFS 碎片 | aria2 落盘成熟；自研需处理跨平台 API（RDM 契约 §3.3 提及，未实现） |
| 状态文件一致性 | 位图必须先于"数据已 flush"之后更新；原子写（tmp+rename）；进程被杀时的窗口 | 常见做法是周期性落盘（RDM 契约 500ms/16 块），**崩溃会丢少量块，不能丢正确性** |
| 落盘顺序与 fsync | 完成时 `flush+sync_all` 再 rename；目录 fsync（Unix）；Windows rename 语义 | 多数实现只 rename 不 fsync，掉电可能丢文件 |
| 跨卷/长路径/保留名 | 临时目录与目标目录不同卷 → `EXDEV`；Windows 保留名（CON/NUL）、260 字符、非法字符 | 需显式处理；RDM 契约 §3.2 有文件名清洗（类型/文档，未实现） |
| 杀软/索引器扫描 `.part` | 实时扫描造成写入抖动；某些杀软直接锁文件 | 无法根治，只能提示/加排除项；这是 Windows 桌面下载器的隐形性能税 |

### 3.4 恢复与校验

| 难点 | 难在哪 | 开源项目如何绕开 / 没做好 |
|---|---|---|
| 只补缺失块 | 需要块级完成位图 + 与实际文件对账；恢复时校验器必须匹配 | aria2 有；GUI 壳继承；自研常退化成"从头再下" |
| 校验器变化 | ETag/Last-Modified/长度任一变化 → 丢弃状态重下（否则拼坏） | RDM 契约 §3.4 明确（未实现）；**很多产品只比文件大小** |
| 内容哈希 | 服务器通常不给哈希；需要用户提供 SHA-256；不匹配时**不能重命名** | aria2 `--checksum`；多数 GUI 无；RDM 契约有（未实现） |
| 状态文件损坏/版本升级 | 需向前兼容的 version 字段 + 忽略损坏状态 | 各项目自研；Gopeed #1591（配置损坏卡"未准备"）是同类问题 |
| 恢复后 URL 失效 | 签名过期需重新取 URL（浏览器/鉴权），不能静默失败 | 几乎无通用方案；RDM 目前也不做（架构 §9 记录为限制） |

### 3.5 UI 数据流与 IPC

| 难点 | 难在哪 | 开源项目如何绕开 / 没做好 |
|---|---|---|
| 高频事件 → 稳定 UI | 8 连接 × 64 KiB 事件；必须聚合/节流/EWMA 平滑；否则 CPU 飙高、ETA 抖动 | Electron 类用 IPC 批量推送；GPUI 需自己实现（RDM 契约 250ms，未实现） |
| 状态机一致性 | 引擎状态（下载中/暂停/完成/失败）与 UI/持久化三处必须一致；重启后"进行中→暂停" | 常见 bug 源；Gopeed #1591 即配置/状态不一致 |
| 本地 IPC 安全 | 必须 loopback + 随机 token + 拒绝局域网投递；token 文件权限 | aria2 RPC 历史上常泄露（默认无鉴权/弱 secret）；**RDM 的 token+loopback 设计是正确方向**，但服务端未实现 |
| 单实例与外部唤起 | 浏览器/命令行唤起已运行实例；冷启动竞态（ipc.json 未写完） | 各家自研；RDM 契约有（未实现） |
| 多窗口/托盘/通知 | 跨平台差异大 | 普遍简化处理 |

### 3.6 跨平台、打包与分发

| 难点 | 难在哪 | 开源项目如何绕开 / 没做好 |
|---|---|---|
| Windows 签名与 SmartScreen | EV 证书成本 + 硬件令牌；新包无声誉会被拦截 | 开源项目普遍不签或自签；首装转化损失 |
| macOS 公证 | notarization + hardened runtime；调试器附加需 `get-task-allow` | 开源项目常直接放弃商店/公证 |
| Linux 沙箱 | flatpak/snap 下 native messaging 路径、portal 限制 | NixOS #352248 类问题；uGet/Persepolis 受害 |
| 浏览器商店 | MV3 审核、权限说明、native 宿主无法随扩展分发 | DownThemAll 的退化为标志性事件 |
| GUI 技术栈成本 | Electron 快但重；Tauri/GPUI 省资源但生态/驱动坑多（RDM：GPUI release 需 `fxc.exe`） | 头部选 Electron（成本最低）；Gopeed 选 Flutter+Go；Rust 项目在 Tauri/egui/GPUI 间分裂 |

---

## 4. 多线程在什么条件下真的更快（定性模型）

### 4.1 模型

把单文件下载速率记为：

```
单连接稳态吞吐  B1 ≈ min( BDP/RTT_eff , MSS/RTT · C/√p , P_conn , C_total )
N 连接聚合吞吐  BN ≈ min( C_total , N · P_conn , N · B1 , S_per_ip )
```

- `BDP = C_total × RTT`（带宽时延积）；`p` 为丢包率；`MSS/RTT·C/√p` 是 Mathis 近似（丢包受限时的单流吞吐）；`P_conn` 为服务器每连接限速；`S_per_ip` 为服务器/风控对单 IP 的总限额。
- **慢启动**：单连接从 `cwnd=10 MSS` 增长到 BDP 需要约 `log2(BDP/MSS)` 个 RTT；高 RTT 链路上这段时间是纯浪费。N 条连接并行慢启动，**管道填充时间近似不变，但填充后的总窗口是 N 倍**——这是"多连接在长肥管道上有效"的第一性原因。
- **随机丢包**：每条连接独立经历丢包与独立回退，聚合吞吐近似线性增长，直到 `C_total` 或 `S_per_ip`。
- **拥塞丢包**：多流更快触发拥塞，聚合≈`C_total`，额外连接只增加公平性/风控风险，**不再提升**。

### 4.2 收益矩阵（定性，**待基准 B1 实测**）

| 场景 | 1 → 8 连接预期 | 原因 |
|---|---|---|
| 跨洋/高 RTT、随机丢包、无每连接限速 | **2–6×**（可能更高） | 多流独立慢启动 + Mathis 型单流受限 |
| 服务器/网盘**每连接限速**（如 1 MB/s/conn） | **≈N× 直到总带宽或每 IP 限额** | `N·P_conn` 生效 |
| CDN 边缘性能不均（慢边 100ms+/块） | **1.5–3×** | 工作窃取让慢边只拖自己手上的块；固定等分则接近无收益 |
| 健康链路、单连接已跑满带宽 | **≈1.0×（±10%）** | 已到 `C_total`，多连接只增加开销与风险 |
| 服务器忽略 Range（200） | **0×**（必须降级） | 协议硬限制 |
| HTTP/2 单连接复用（自研 reqwest 常见） | **≈1.0×** | 共享拥塞窗口；**"连接数"是假的** |
| 小文件（< 4 MiB 或 < 1 个块） | **可能更慢** | 探测 + TLS/连接 + 调度开销 |
| 服务器按 IP 限连接/风控（403/429） | **0× 或负收益** | Gopeed #1592/#1593 实证；应降连接而非猛重试 |
| 本机磁盘/CPU/杀软瓶颈 | **0×** | 瓶颈不在网络 |

### 4.3 结论（直接可写进 README）

- **多线程加速的本质是"绕过单流拥塞控制与单连接限速"，不是"让网络变快"。** 当瓶颈在 `C_total`、服务器总限额或本机 I/O 时，它**不会更快，甚至更慢**。
- **诚实的产品应做到**：探测 Range → 不支持则不假装加速；连接数可调且可自适应降；UI 标明"是否处于加速模式"；限速是全局的。这四条比"默认 8 连接"更难、更值钱。
- **别用 h2 的"流"冒充"连接"**：分段下载应显式优先 HTTP/1.1 多 TCP；若必须 h2，应实测并诚实标注收益。

---

## 5. RDM 的独特优势映射表

> 判定规则：**【差异】** = 竞品普遍缺失或明显做差；**【标配】** = 行业已有（不能让用户"因为这条买你"）；**【伪差异】** = 听起来是优势但用户不感知或无法验证。当前证据按 **01:20 快照**标注；01:22 后的变化见文首与 §5.2。

| # | 拟宣传优势 | 对应的真实瓶颈（§2/§3） | 验证方式（fixture/benchmark/代码位置） | 当前证据（状态） | 类别 |
|---|---|---|---|---|---|
| 1 | 纯 Rust 单二进制：GPUI 渲染，**无 Electron/CEF、无 aria2 子进程** | 体积/内存/启动/供应链/攻击面（§2.12、§3.6） | 同机冷启动到首帧、空闲 RSS、8 连接下载期 RSS、安装包体积，对比 Motrix/imfile 官方 release；`cargo build --release` 产物 | `Cargo.toml` 依赖可核验（无 electron/aria2；gpui 0.2.2）；**【选型已定，未实测】** | **【差异】**（须有数据；Gopeed 原生 Go+Flutter 部分抵消） |
| 2 | 块级工作窃取（4 MiB 块、原子索引 `fetch_add` 动态领取），尾延迟低 | 固定分片的尾部空转（§3.2） | fixture：1 个 worker 每块 200 ms、其余 10 ms，对比"固定等分"对照组；指标=总时长、p95 块完成时间 | `architecture.md` §3.3 契约；**【契约，引擎未实现】** | **【差异】**（但 Firelink/rdm-dl 也宣称动态拆分，非独占） |
| 3 | 跨连接**全局令牌桶**限速（不是每连接限速） | 限速器在 N 连接下超速 N 倍（§3.2） | fixture 8 连接 + `--limit 2MiB/s`，60 s，10 s 桶均值与 p95 瞬时值 | 契约 §3.5；**【契约】** | **【标配】**（aria2 `--max-overall-download-limit` 已有） |
| 4 | 崩溃安全恢复：`.part` + 原子状态文件 + 校验器（ETag/Last-Modified/长度）+ **只补缺失块** | 断点续传正确性（§3.4） | fixture 统计"恢复后实际请求字节"；`kill -9` 后重启；哈希与单连接一致；状态文件损坏注入 | 契约 §3.4；**【契约】**；（`DownloadRequest`/`RemoteMeta` 类型已冻结） | **【标配】**（aria2 有）；**差异在"可复现验证"** |
| 5 | Range 被忽略时**自动降级 + 明确提示"，`summary.range_supported=false` | 协议硬限制（§2.1、§4） | fixture 强制 `200`：文件必须正确、UI 事件含降级说明、不重试 | 契约 §3.3；`DownloadSummary.range_supported` 字段已存在；**【契约】** | **【标配】**（aria2 也可单流）；**诚实语义是差异点** |
| 6 | 下载后 SHA-256 校验，不匹配**不重命名** | 静默损坏（§3.4） | fixture 篡改 1 字节，断言 `ChecksumMismatch` 且目标文件不存在 | 契约 §3.4；`DownloadError::ChecksumMismatch` 已定义；**【类型已实现，逻辑未实现】** | **【标配】**（aria2 `--checksum`） |
| 7 | **Firefox-first**：MV3 扩展 + native messaging 宿主 + 本机 IPC + spool 队列（GUI 不在也能排队） | 浏览器集成成本/空位（§2.5/§2.6、§3.5） | `about:debugging` 端到端：拦截 → 宿主 → GUI；GUI 未启动时落 spool 并在下次启动自动入队；无 token 请求必须 `unauthorized` | `extension-protocol.md` 完整协议；`rdm-ipc` 类型 + 阻塞客户端 + 2 个单测；**【扩展/宿主/GUI 服务端均未实现】** | **【差异】**（"市场空位"为判断，置信度中；且依赖"用户愿意装宿主"的转化率） |
| 8 | 本机 IPC loopback + 随机 token + 拒绝局域网投递 | IPC 安全（§3.5） | 代码审计 + 无 token/错 token 请求应被拒；抓包确认仅 127.0.0.1 | `rdm-ipc` 已实现 `Envelope.token` 校验路径与 `is_gui_alive`；**【单测】**（本轮未执行构建） | **【差异小】**（aria2 RPC 默认 secret 也做） |
| 9 | **fixture server 驱动的可复现测试矩阵**（延迟/故障/200/ETag 变化/限速） | 测试与故障复现（§2.8） | CI 中故障注入用例数量与通过率；每个 bug 先写成 fixture 用例 | `crates/rdm-fixture-server` 在 01:20 快照时仅 `placeholder()`；01:22 复核已出现故障注入实现（见 §5.2）；**【快照时未实现】** | **【真差异】**（竞品几乎无公开故障矩阵） |
| 10 | 250 ms 节流 + EWMA 速度/ETA，事件流稳定 | UI 数据流（§3.5） | 8 连接下载 60 s：UI 帧时间、事件数/秒、CPU 占用 | 契约 §3.5；`DownloadEvent::Progress` 已定义；**【契约】** | **【标配】** |
| 11 | 单实例 + URL 转发 + 浏览器唤起 | 体验（§3.5） | 手工 e2e：二次启动投递 URL 后退出 | 契约 §7；**【未实现】** | **【标配】** |
| 12 | GPUI GPU 渲染 + `uniform_list` 虚拟列表（大列表不卡） | UI 性能（§3.6） | 10,000 条记录滚动帧率/内存 | 仅依赖选型与骨架；**【骨架可开窗，列表未实现】** | **【伪差异】**（用户不感知技术栈，只感知流畅） |

### 5.1 诚实的差异化结论

- **行业标配（不能说成优势）**：多连接分段、断点续传、限速、浏览器接管、校验和、并发队列、单实例。
- **RDM 可能成为真差异的四条（按可辩护性排序）**：
  1. **可验证的正确性**：fixture 故障注入矩阵 + 公开 benchmark 脚本（竞品普遍缺失，且直接命中"99% 卡死/静默损坏/403 后卡 running"这些真实差评）。
  2. **无外部进程的原生栈**：体积/内存/启动/攻击面（**必须先有同机实测数据**，否则只是口号；且 Gopeed 已证明原生栈可行）。
  3. **Firefox-first 集成**：MV3 + native messaging + spool 的完整链路（空位明确）。
  4. **诚实降级/限速语义**：Range 不支持不假装加速、全局限速准确、UI 明确状态（小而可信）。
- **风险/反面**：`rdm-dl/rdm` 已在 2026-10 占据"Rust + 8 连接 + 动态拆分 + 浏览器扩展"的公开叙事（1★，但同题）；"Rust 下载器"正在模板化（344 个仓库、多数 2026 年新建）。**RDM 必须用可复现数据而非技术标签竞争。**
- **不能宣传的**（当前状态）：任何"已实现/已支持/已测试"的功能（引擎仍在落地，且无集成测试）、"比 aria2 快"（无数据）、"企业级安全"（仅设计）、"全平台"（仅 Windows 优先开发）。

### 5.2 状态复核（2026-10-19 01:22–01:24，仅文件存在性观察，未构建/未运行）

| §5 行 | 报告快照（01:20）标注 | 复核观察（01:24） | 引用建议 |
|---|---|---|---|
| 2 工作窃取 | 【契约，引擎未实现】 | `lib.rs` 已接 `transfer::run`；`transfer.rs` 尚未落盘（复核瞬间） | 待 CI 通过后再写"已实现" |
| 4 崩溃安全恢复 | 【契约】 | `state.rs`（7.7 KB，含 4 个单测）已出现 | 需集成测试证明"只补缺失块"后再对外 |
| 6 SHA-256 校验 | 【类型已实现，逻辑未实现】 | 逻辑随 `transfer` 落地中 | 同上 |
| 7 Firefox 集成 | 【扩展/宿主/GUI 服务端均未实现】 | native 侧 framing/protocol/spool/gui 模块已出现（含单测），`main.rs` 未接线；`extension/` 仍缺失 | 可讲"宿主协议已建模"，不可讲"可用" |
| 9 fixture 测试矩阵 | 【未实现】 | fixture server 已实现故障注入 API（约 29 KB） | **优先**把 B1/B2/B4 变成 CI 用例 |
| 10 进度节流 | 【契约】 | `event.rs`（4.7 KB）已出现 | 未验证 |
| 12 GPUI 列表 | 【骨架可开窗，列表未实现】 | `model.rs`（42 KB）已出现 | 未运行 |

> 观察到的趋势：**fixture server 先于引擎落地**（01:19 vs 01:22+），与 §8.3 建议一致；若保持"测试先行"，§5.1 的"可验证正确性"差异化最有可能成立。

### 5.3 可直接引用的 README/路演短句（基于本报告事实）

- "同类开源产品的头部不是引擎，而是壳：Motrix（57k★）= Electron + aria2，imfile（4.5k★）= 它的 fork。"
- "DownThemAll 转 WebExtension 后多线程分段已不可行——浏览器内做多线程这条路走不通，必须原生进程。"
- "我们不发明新功能；我们把行业标配做对：不损坏、可恢复、可复现测试。"
- "Rust 生态有 344 个下载器仓库，Top1 只有 298★。这个品类不缺代码，缺长期维护与可验证的正确性。"
- "多线程只在'单流受限'时有效；Range 被忽略、服务器按 IP 风控、h2 单连接复用时，它不会更快——我们会告诉你。"

---

## 6. 可量化基准测试设计（3–5 个）

> 全部基于 `rdm-fixture-server`（报告快照时为桩；01:22 复核已出现故障注入实现，见文首）。**本轮未执行任何构建或基准**，下表"当前可执行性"按 01:20 快照给出，实际状态请以当时 commit 为准。每个基准需配套：固定文件（64 MiB，可复算 SHA-256）、固定种子、N≥5 次取中位数、CI 可跑、结果 JSON 落盘。**"对照组"优先同机实测，不引用二手营销数字。**

### B1. 1 vs 8 连接吞吐（含 h1/h2 分组）—— 验证"多线程何时有效"

- **目的**：证明收益来自真实多 TCP 流；同时暴露 h2 复用陷阱与"服务器每连接限速"边界。
- **场景**：fixture 三种模式 —— (a) 每连接限速 1 MiB/s、无任务延迟；(b) 每块 40 ms 人工延迟（模拟 RTT/慢边）；(c) 无限制本地模式（应≈1.0×，用于证明不吹牛）。
- **参数**：64 MiB 文件；连接数 ∈ {1, 2, 4, 8, 16}；HTTP/1.1 与 HTTP/2 各一组；每组 5 次。
- **指标**：墙钟时间、平均吞吐、p95 块完成时间、失败重试次数。
- **通过标准**：模式 (a) 8 连接吞吐 ≥ 3× 1 连接（阈值宽松防 flaky）；模式 (c) 8 连接不劣于 1 连接 15% 以上；**h2 组若与 h1 差异 <10%，必须把"分段默认 h1"写进实现并如实说明**。
- **当前可执行性**：**不可执行**（引擎与 fixture 均为桩）。

### B2. 断点恢复：只补缺失块（字节级验证）

- **目的**：把"断点续传"从口号变成可审计数字。
- **场景**：下载到 ~50% 时 `kill -9`（真实进程杀，非优雅暂停）；重启后同一请求恢复；fixture 记录每个 Range 请求与返回字节数。
- **指标**：恢复后实际传输字节 / 剩余字节；哈希是否等于基准；状态文件损坏注入（截断/改 JSON）后是否安全重下。
- **通过标准**：恢复传输量 ≤ 剩余字节 + 1 个块（4 MiB）；哈希一致；损坏状态不产生错误文件、不崩溃。
- **附加**：ETag 变化注入 → 必须丢弃状态从零开始且最终哈希正确。
- **当前可执行性**：**不可执行**。

### B3. 全局限速准确性（8 连接）

- **目的**：验证跨连接令牌桶，而非每连接限速。
- **场景**：fixture 不限速（本地），`--limit 2MiB/s`，8 连接，60 s；同时做一个"错误实现对照"（每连接 2MiB/s）用于展示 8× 超速。
- **指标**：10 s 桶均值、全程均值、p95 瞬时速率（1 s 窗口）。
- **通过标准**：全程均值 ∈ [1.8, 2.2] MiB/s（±10%）；p95 瞬时 ≤ 2.5 MiB/s；错误对照需显著超速（证明测试有效）。
- **当前可执行性**：**不可执行**（契约 §3.6 已把阈值放宽到 1.5×，建议收紧并公开方法）。

### B4. 服务器拒绝/忽略 Range 的降级正确性

- **目的**：验证"协议不支持时诚实降级"，而非假装多线程。
- **场景**：fixture 三种响应：(a) 强制 `200`；(b) 对第 3 个并发 Range 返回 `403`（模拟 Gopeed #1592）；(c) 两个"边缘"返回不同字节（模拟 CDN 不一致）。
- **指标**：文件哈希；`range_supported` 标志；事件流中是否有明确提示；403 场景是否降连接数而非死循环。
- **通过标准**：(a)(b) 文件哈希正确且不卡死、UI/JSON 事件含降级原因；(c) 必须检测到校验器/哈希不一致并报错，**绝不产出静默损坏文件**。
- **当前可执行性**：**不可执行**；这是最能体现"正确性差异化"的用例，建议优先实现。

### B5. 资源占用与启动时间（对照 Electron 类产品，同机实测）

- **目的**：为"无 Electron/无 aria2 子进程"提供可引用数据。
- **场景**：同一台 Windows 机器；被测：`rdm-gui`（release，debug 构建替代需注明）、Motrix 官方 release、imfile 官方 release。
- **指标**：冷启动到可交互时间（`Stopwatch` + 进程创建时间/首帧事件）；空闲 60 s 后 RSS/私有工作集；8 连接下载 64 MiB 期间峰值 RSS；安装包体积；进程数（Motrix 常驻 aria2 子进程）。
- **通过标准**：不设硬阈值，输出对照表；**若 RDM 未显著优于 Electron 类，则 README 不得使用"更省资源"表述**。
- **公开数据**：**未获取**。本报告不引用任何"Electron 占 X MB"的二手数字；结论必须来自同机同镜像实测（可用脚本记录 OS 版本、版本号、构建号）。
- **当前可执行性**：仅 GPUI 空窗可测启动（不含下载）；其余需产品化后补。

> 可选 B6（若时间允许）：**尾延迟/工作窃取对照**——fixture 让 1 个"慢边"每块 200 ms、其余 10 ms，比较"固定等分 8 片" vs "4 MiB 块工作窃取"，指标为总时长与最后一块完成时间差。该用例能直接量化 §5 第 2 条优势。

---

## 7. 参考链接（真实可访问；GitHub 数据链接均于 2026-10-19 经 API 验证）

**项目与仓库**

- aria2：https://github.com/aria2/aria2 ｜手册：https://aria2.github.io/manual/en/html/index.html
- Motrix：https://github.com/agalwood/Motrix
- Gopeed：https://github.com/GopeedLab/gopeed
- uGet：https://github.com/ugetdm/uget ｜浏览器集成宿主：https://github.com/ugetdm/uget-integrator ｜官网：https://ugetdm.com/ ｜SourceForge：https://sourceforge.net/projects/urlget/
- Persepolis：https://github.com/persepolisdm/persepolis
- DownThemAll：https://github.com/downthemall/downthemall ｜官网：https://www.downthemall.net/
- Axel：https://github.com/axel-download-accelerator/axel
- wget2（主库 GitLab）：https://gitlab.com/gnuwget/wget2 ｜GitHub 镜像（停更）：https://github.com/gnuwget/wget2
- yt-dlp：https://github.com/yt-dlp/yt-dlp
- imfile：https://github.com/imfile-io/imfile-desktop ｜官网：https://imfile.org/
- XDM：https://github.com/subhra74/xdm
- NDM（闭源）：https://www.neatdownloadmanager.com/
- FDM（闭源）：https://www.freedownloadmanager.org/ ｜历史 SVN 镜像（非官方）：https://github.com/mirror/freedownload
- Rust 生态检索：https://github.com/search?q=download+manager+language%3Arust&type=repositories ｜同名检索：https://github.com/search?q=rdm+download&type=repositories
- 同名项目：https://github.com/rdm-dl/rdm ｜https://github.com/canmi21/rdm ｜https://github.com/SubhajitCode/rdm ｜https://github.com/Japerz12138/re-download-manager ｜https://github.com/sudosz/rdm

**协议、平台与事件（证据）**

- RFC 9110（HTTP 语义；§13 条件请求、§14 Range）：https://www.rfc-editor.org/rfc/rfc9110.html
- RFC 5681（TCP 拥塞控制；慢启动/丢包模型）：https://www.rfc-editor.org/rfc/rfc5681
- MDN Native messaging：https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/Native_messaging
- MDN manifest_version（MV3）：https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/manifest.json/manifest_version
- Chrome MV3 阻塞式 webRequest 迁移：https://developer.chrome.com/docs/extensions/develop/migrate/blocking-web-requests
- DownThemAll 转 WebExtension 后失去多线程（第三方百科转引）：https://handwiki.org/wiki/Software:DownThemAll!
- Firefox 沙箱下 native messaging 宿主路径问题：https://github.com/NixOS/nixpkgs/issues/352248
- RIAA 对 youtube-dl 的 DMCA 通知（GitHub DMCA 仓库目录）：https://github.com/github/dmca/tree/master/2020/10 ｜EFF 恢复报道：https://www.eff.org/deeplinks/2020/11/github-reinstates-youtube-dl-after-riaa-takedown

**关键 issue 实证（用于"多线程失败模式"）**

- aria2 #2402（未鉴权 JSON-RPC → 任意文件写入 → RCE）：https://github.com/aria2/aria2/issues/2402
- aria2 #2400（BT `piece_length=0` 触发 SIGFPE）：https://github.com/aria2/aria2/issues/2400
- Gopeed #1592（服务器 403 拒绝额外连接 → 永远 running）：https://github.com/GopeedLab/gopeed/issues/1592
- Gopeed #1593（chunk 被 connection-limited server 403 拒绝的重试）：https://github.com/GopeedLab/gopeed/issues/1593
- Gopeed #1589（`/api/web/proxy` 开放代理 SSRF）：https://github.com/GopeedLab/gopeed/issues/1589
- Gopeed #1590（BT 路径穿越写任意文件）：https://github.com/GopeedLab/gopeed/issues/1590
- Motrix #2354（大文件卡死 99%）：https://github.com/agalwood/Motrix/issues/2354
- DownThemAll #619（下载文件仅 0.9KB）：https://github.com/downthemall/downthemall/issues/619
- DownThemAll #616（文件名全变 `true`）：https://github.com/downthemall/downthemall/issues/616

**GitHub API（数据来源）**

- REST API 文档：https://docs.github.com/en/rest
- 仓库状态示例：`https://api.github.com/repos/aria2/aria2`、`.../agalwood/Motrix`、`.../GopeedLab/gopeed`、`.../ugetdm/uget`、`.../persepolisdm/persepolis`、`.../downthemall/downthemall`、`.../axel-download-accelerator/axel`、`.../gnuwget/wget2`、`.../yt-dlp/yt-dlp`、`.../imfile-io/imfile-desktop`（均带 `User-Agent`）

---

## 8. 附录：数据抓取与查证记录

### 8.1 抓取方式

```powershell
$h = @{ 'User-Agent' = 'rdm-research-analyst'; 'Accept' = 'application/vnd.github+json' }
Invoke-RestMethod 'https://api.github.com/repos/<owner>/<repo>' -Headers $h
Invoke-RestMethod 'https://api.github.com/search/repositories?q=download+manager+language:rust&sort=stars&order=desc&per_page=15' -Headers $h
```

- 抓取时间：2026-10-19 01:16–01:20 +08:00；未认证配额消耗约 30/60（core）、若干 search（10/min）；抓取后剩余 core 27。
- `pushed_at` = 最近一次 push（任意分支）；`open_issues_count` 含 PR；`license.spdx_id = NOASSERTION` 表示 API 未识别（不代表无许可证）。
- 维护风险判断综合：最近 push 距今、open 数、是否有长期停滞/被 fork 替代的公开记录、架构依赖（aria2/Electron 的外部进程与安全面）。
- **issue 类别**来自每个仓库"最新 8 条 open issue"抽样，非全量统计（仅用于定性归纳，不作比例结论）。

### 8.2 未获取 / 未核实的项目（诚实标注）

| 项 | 状态 |
|---|---|
| aria2 各版本默认参数 | 依据官方手册通识（`-x` 默认 1）；**未逐版本核对 2026-06 快照** |
| uGet 主库（SourceForge）最近提交 | **未获取**（GitHub 仅为镜像；主库在 SourceForge，未抓取） |
| uGet / Motrix 的许可证文本 | GitHub API 对两仓库均返回 `NOASSERTION`；表中 GPL-2.0 / MIT 依据项目公开声明，**本次未读 LICENSE 全文** |
| FDM / NDM 源码与许可证 | 闭源免费产品；**未找到官方源码仓库**；FDM 的 `mirror/freedownload` 为第三方历史镜像，不代表现版本 |
| Motrix / imfile 是否开启 aria2 多连接的具体默认配置 | **未核实**（需解包/读配置；本报告只断言"能力来自 aria2"） |
| Electron 类产品的启动时间/内存公开数据 | **未获取**；报告禁止引用二手数字，基准 B5 要求同机实测 |
| DownThemAll 当前许可证 SPDX | API 返回 NOASSERTION，**未读 LICENSE 全文** |
| wget2 主库（GitLab）最近提交 | **未获取**（GitHub 镜像停更于 2019，不可作为活跃度依据） |
| 各项目 issue 类别占比 | **未统计**（仅抽样 8 条/仓库） |
| RDM 引擎性能 | **未获取**：调研时点（01:20）引擎为桩；01:22 复核实现进行中；本轮未构建、未跑基准 |

### 8.3 对后续工作的建议（按优先级）

0. **文档纪律（新增，定稿时发现）**：定稿复核时 `README.md` 已按"已具备"口径描述引擎能力（Range 探测 / 断点续传 / 限速 / SHA-256 等），但代码仍在落地且仓库内无 `tests/` 集成测试。建议给 README 增加"开发中（未验证）"状态徽标，或等 `cargo test` 全绿后再用现在这套口径；§5.2 可直接作为核对清单。

1. **先实现 fixture server + B1/B2/B4**：没有这三个用例，"正确性"差异化无法证明；也是 README 唯一能引用的硬数据。
2. **在实现引擎前先定 h1/h2 策略**（§4.3）：否则可能写出"8 连接但实际 1 条 TCP"的假多线程，事后返工。
3. **把"降级/403 自适应降连接/全局限速"写成契约级验收项**（可直接转化为 integration test 名称，便于 README 展示）。
4. **产品叙事与文档解耦**：先让 `architecture.md` 的"契约"落地成测试，再改 README；本报告 §5 的【契约】项在实现前不得出现在对外材料。
5. **命名策略**：`RDM` 缩写已被占用且同题项目存在；对外全称 "RDM — Rust Download Manager"，仓库建议用 `rdm-download/rdm`（当前 404 可用），并在 README 首段明确与 `rdm-dl/rdm` 等无关。
