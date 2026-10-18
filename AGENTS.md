# AGENTS.md — 给 coding agent 的仓库指南

本仓库是 RDM（Rust Download Manager）：纯 Rust 下载引擎 + GPUI 桌面端 + Firefox 扩展 +
MCP server。人类向文档见 [README.md](README.md)、[docs/architecture.md](docs/architecture.md)；
**agent 驱动下载的契约见 [docs/agent-integration.md](docs/agent-integration.md) 与
[skills/rdm/SKILL.md](skills/rdm/SKILL.md)**。

## 我想"用 RDM 下载一个文件"

优先走机器接口，不要读源码猜参数：

```powershell
# 一次性命令（JSON 信封）
rdm download "https://example.com/file.iso" --dir C:\Downloads --output json

# 批量 / 长任务（task id 可持久化查询/恢复）
rdm run --input tasks.json --wait --output json
rdm tasks --output json
rdm task <task_id> --events 50 --output json

# 自检与诊断
rdm selftest --output json
rdm doctor --url "https://example.com/file.iso" --output json
```

支持 MCP 的运行时：把 `rdm-mcp.exe` 配为 stdio server（示例见
[docs/agent-mcp-reference.md](docs/agent-mcp-reference.md)），工具清单：`rdm_capabilities`、
`rdm_probe`、`rdm_download`、`rdm_plan_download`、`rdm_apply_plan`、`rdm_list_tasks`、
`rdm_get_task`、`rdm_wait_task`、`rdm_pause_task`、`rdm_resume_task`、`rdm_cancel_task`、
`rdm_verify_file`、`rdm_doctor`、`rdm_selftest`、`rdm_bench`、`rdm_state_maintenance`。

## 构建与测试（Windows）

前置：Rust stable（本机 1.99.1，`x86_64-pc-windows-gnu`）+ MinGW-w64 GCC 在 `PATH`。
本仓库的 GPUI 需要 `windows-manifest` feature 与 `crates/rdm-gui/build.rs` 的 manifest
workaround；不要移除（根因见 docs/architecture.md §8）。

```powershell
$env:PATH = "C:\Users\nirvana\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;$env:USERPROFILE\.cargo\bin;$env:PATH"

cargo build --workspace
cargo test --workspace --no-fail-fast
cargo fmt --all --check
cargo clippy -p rdm-core -p rdm-cli -p rdm-agent -p rdm-mcp --all-targets
```

首次 GUI 构建约 4–5 分钟；debug GUI 约 500MB，`target/` 已被 gitignore。
构建时间紧时用独立 `CARGO_TARGET_DIR` 并在结束清理进程；不要提交 `target*`。

## 代码地图

| 路径 | 职责 |
|---|---|
| `crates/rdm-core` | 下载引擎（probe/工作窃取/Range/续传/限速/校验） |
| `crates/rdm-cli` | `rdm` CLI（含 agent 子命令与 JSON 输出） |
| `crates/rdm-agent` | agent 共享契约（TaskStore/错误码/Policy/工具 schema） |
| `crates/rdm-mcp` | MCP stdio server（16 个工具 + resources + prompts） |
| `crates/rdm-gui` | GPUI 桌面端 + 本机 IPC server |
| `crates/rdm-native` | Firefox native messaging host |
| `crates/rdm-ipc` | GUI ↔ native host 本地协议 |
| `crates/rdm-fixture-server` | 可控 HTTP fixture（测试/基准） |
| `extension/firefox` | Firefox MV3 扩展 |
| `docs/` | 架构/协议/调研/验收；`docs/agent-integration.md` 是 agent 契约 |
| `skills/rdm/` | agent skill（决策树 + 工具速查 + 配方） |

## 约定

- 公开 JSON/工具契约变更必须先改 `docs/agent-integration.md` 并同步 `rdm-agent`；
  agent 可见输出必须 `TaskRecord::redacted()`，Cookie/Authorization 不落日志。
- 引擎行为用 `rdm-fixture-server` 做**字节级**测试，不要依赖真实公网；测试端口随机。
- 涉及目录：`.part`/`.part.json` 是断点状态，`%LOCALAPPDATA%\RDM\agent` 是任务 store；
  `rdm state inspect` 可诊断，`rdm state repair` 默认 dry-run。
- 提交前：fmt、test、必要时 clippy；不要提交构建产物/临时下载文件。
- 已知限制见 README §11 与各 `docs/research/*`；不要声称未验证的能力（Linux/macOS、
  release GUI、真实 Firefox 接管均未验证）。
