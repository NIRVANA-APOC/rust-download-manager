//! `rdm-gui` 命令行参数解析。
//!
//! 只做纯字符串解析（不碰 gpui / Win32），因此可以在普通单测里覆盖所有分支。
//! 真正的启动行为（静默窗口、单实例投递）在 `main.rs`。
//!
//! 支持的参数：
//! - `--silent`：本次启动静默运行（不显示主窗口，驻留托盘；托盘不可用时回退显示窗口）。
//! - `--autostart`：由 `HKCU\...\Run` 启动项传入（信息性标记，日志用）。
//! - `--help` / `-h`：打印用法。
//! - 第一个非选项参数：启动后要添加的下载 URL。
//!
//! 未知 `-` 开头的参数直接报错（GUI 是 GUI 子系统时看不到 stderr，但调用方仍可拿到
//! 退出码 2；日志会记录）。

/// 新版 UI / 旧版 UI 的选择（`--ui=new|legacy`，环境变量 `RDM_UI` 兜底）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiModeArg {
    /// 新版下载中枢 UI（默认）。
    #[default]
    New,
    /// 旧版 UI（M1–M4 期间的迁移回滚入口）。
    Legacy,
    /// 组件展示窗（M1 验收：六态/亮暗/密度/中英文矩阵）。
    Kit,
}

impl UiModeArg {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "new" | "v2" | "hub" => Ok(Self::New),
            "legacy" | "old" | "v1" => Ok(Self::Legacy),
            "kit" | "showcase" => Ok(Self::Kit),
            other => Err(format!("未知 UI 模式: {other}（可用 new / legacy / kit）")),
        }
    }
}

/// 解析后的命令行参数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliArgs {
    /// 启动后要立即添加的 URL（已去首尾空白，但未做 URL 校验）。
    pub url: Option<String>,
    /// `--silent`：本次启动静默运行。
    pub silent: bool,
    /// `--autostart`：来自开机自启动项。
    pub autostart: bool,
    /// `--help` / `-h`。
    pub help: bool,
    /// `--ui=new|legacy`；`None` 时看环境变量 `RDM_UI`，再默认新版。
    pub ui: Option<UiModeArg>,
}

impl CliArgs {
    /// 解析实际 UI 模式：命令行 > 环境变量 > 默认新版。
    ///
    /// 环境变量只用于本机验收/回滚，不写入用户设置，也不改变公开 CLI 契约。
    pub fn resolved_ui(&self) -> UiModeArg {
        if let Some(ui) = self.ui {
            return ui;
        }
        match std::env::var("RDM_UI") {
            Ok(value) => UiModeArg::parse(&value).unwrap_or_default(),
            Err(_) => UiModeArg::default(),
        }
    }
}

/// 用法文本（`--help` 与参数错误时输出）。
pub const USAGE: &str = "\
用法: rdm-gui [--silent] [--autostart] [URL]

  --silent      启动时不显示主窗口，直接驻留托盘（需托盘可用）
  --autostart   由开机自启动项传入（信息性标记）
  --ui=NEW      UI 模式：new（下载中枢，默认）/ legacy（旧版，回滚用）/ kit（组件展示窗）
  -h, --help    显示本帮助
  URL           启动后立即添加的下载链接（http/https）

说明：GUI 已在运行时，URL 会经单实例 IPC 投递给运行中的窗口。";

/// 解析 `std::env::args().skip(1)` 形式的参数迭代器。
///
/// 出错时返回可直接展示给用户的中文错误（调用方负责 `exit(2)` / 日志）。
pub fn parse_args<I>(args: I) -> Result<CliArgs, String>
where
    I: IntoIterator<Item = String>,
{
    let mut parsed = CliArgs::default();
    for arg in args {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        match arg {
            "--silent" => parsed.silent = true,
            "--autostart" => parsed.autostart = true,
            "-h" | "--help" => parsed.help = true,
            _ if arg.starts_with("--ui=") => {
                let value = &arg["--ui=".len()..];
                parsed.ui = Some(UiModeArg::parse(value)?);
            }
            "--ui" => return Err("--ui 需要一个值：--ui=new|legacy".to_string()),
            _ if arg.starts_with('-') => return Err(format!("未知参数: {arg}")),
            _ => {
                if parsed.url.is_some() {
                    return Err("只能指定一个下载 URL".to_string());
                }
                parsed.url = Some(arg.to_string());
            }
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliArgs, String> {
        parse_args(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn empty_args_are_defaults() {
        assert_eq!(parse(&[]).unwrap(), CliArgs::default());
    }

    #[test]
    fn flags_can_appear_before_or_after_url() {
        let before = parse(&["--silent", "--autostart", "https://example.com/a.bin"]).unwrap();
        assert!(before.silent);
        assert!(before.autostart);
        assert_eq!(before.url.as_deref(), Some("https://example.com/a.bin"));

        let after = parse(&["https://example.com/a.bin", "--silent"]).unwrap();
        assert!(after.silent);
        assert!(!after.autostart);
        assert_eq!(after.url.as_deref(), Some("https://example.com/a.bin"));
    }

    #[test]
    fn help_flag_does_not_require_url() {
        let parsed = parse(&["--help"]).unwrap();
        assert!(parsed.help);
        assert!(parsed.url.is_none());
        assert!(USAGE.contains("--silent"));
    }

    #[test]
    fn unknown_flag_is_an_error() {
        let err = parse(&["--silnet"]).unwrap_err();
        assert!(err.contains("未知参数"), "err = {err}");
        assert!(err.contains("--silnet"), "err = {err}");
    }

    #[test]
    fn second_url_is_rejected() {
        let err = parse(&["https://a.example/a", "https://b.example/b"]).unwrap_err();
        assert!(err.contains("只能指定一个"));
    }

    #[test]
    fn url_is_trimmed() {
        let parsed = parse(&["  https://example.com/x  "]).unwrap();
        assert_eq!(parsed.url.as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn ui_flag_parses_modes() {
        assert_eq!(parse(&["--ui=new"]).unwrap().ui, Some(UiModeArg::New));
        assert_eq!(parse(&["--ui=legacy"]).unwrap().ui, Some(UiModeArg::Legacy));
        assert_eq!(parse(&["--ui=kit"]).unwrap().ui, Some(UiModeArg::Kit));
        assert!(parse(&["--ui=wat"]).unwrap_err().contains("未知 UI 模式"));
        assert!(parse(&["--ui"]).unwrap_err().contains("需要一个值"));
    }

    #[test]
    fn blank_arguments_are_ignored() {
        let parsed = parse(&["", "  ", "--silent"]).unwrap();
        assert!(parsed.silent);
        assert!(parsed.url.is_none());
    }
}
