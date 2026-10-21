//! 打开文件 / 在资源管理器中定位文件。
//!
//! Windows 用 `explorer /select,<path>`（文件存在时高亮选中；不存在时退化为打开父目录），
//! 「打开文件」用 `cmd /C start "" <path>`。macOS / Linux 分别用 `open` / `xdg-open`。
//! 命令只做一次性 spawn，不阻塞 GUI 主线程。
//!
//! **Explorer 的引号规则（2026-10-20 修复）**：`explorer` 不用标准 `CommandLineToArgvW`
//! 解析，而是自己的命令行解析器；`/select,` 后面的路径必须写成 `/select,"<path>"`——引号
//! 只包住路径。若让 `std::process::Command` 按默认规则处理含空格的参数，它会生成
//! `"/select,C:\...\sel file (1).exe"`（整个参数被引号包住），Explorer 解析失败后**静默
//! 回退到默认 shell 文件夹（主文件夹/文档）**，表现为「点『目录』打开的是家目录」。
//! 因此这里用 `CommandExt::raw_arg` 原样追加 `/select,"<path>"`。
//! 参考：Geoff Chappell《The Windows Explorer Command Line》§2（双引号用于分隔字段）。

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Windows 专用：以「原始命令行片段」追加、不再加引号的参数（`CommandExt::raw_arg`）。
    ///
    /// 只用于 `explorer /select,"<path>"` 这种必须自己控制引号位置的场景；其它平台为
    /// `None`，`spawn_spec` 会忽略它。
    pub raw_arg: Option<OsString>,
}

impl CommandSpec {
    fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            raw_arg: None,
        }
    }

    fn with_raw_arg(mut self, raw_arg: OsString) -> Self {
        self.raw_arg = Some(raw_arg);
        self
    }

    fn display(&self) -> String {
        let mut text = format!("{} {}", self.program, self.args.join(" "));
        if let Some(raw) = &self.raw_arg {
            text.push(' ');
            text.push_str(&raw.to_string_lossy());
        }
        text
    }
}

/// 生成「在文件管理器中定位」的命令（不执行）。
pub fn folder_spec(file: &Path) -> CommandSpec {
    #[cfg(target_os = "windows")]
    {
        if file.is_file() {
            // 只给路径加引号：`/select,"C:\dir with space\a.bin"`。
            CommandSpec::new("explorer", Vec::new()).with_raw_arg(select_arg(file))
        } else if let Some(parent) = file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            CommandSpec::new("explorer", vec![parent.display().to_string()])
        } else {
            CommandSpec::new("explorer", vec![file.display().to_string()])
        }
    }
    #[cfg(target_os = "macos")]
    {
        if file.is_file() {
            CommandSpec::new("open", vec!["-R".to_string(), file.display().to_string()])
        } else if let Some(parent) = file.parent() {
            CommandSpec::new("open", vec![parent.display().to_string()])
        } else {
            CommandSpec::new("open", vec![file.display().to_string()])
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = if file.is_dir() {
            file.to_path_buf()
        } else {
            file.parent().unwrap_or(file).to_path_buf()
        };
        CommandSpec::new("xdg-open", vec![dir.display().to_string()])
    }
}

/// 构造 Explorer 的 `/select,"<path>"` 原始参数。
///
/// 用 `OsString` 拼接（而不是 `format!` + `display()`），保证非 UTF-8 路径也原样传给
/// Explorer；Windows 路径本来可以是任意 UTF-16，不能在这里丢字节。
#[cfg(target_os = "windows")]
fn select_arg(file: &Path) -> OsString {
    let mut raw = OsString::from("/select,\"");
    raw.push(file.as_os_str());
    raw.push("\"");
    raw
}

/// 生成「用默认程序打开文件」的命令（不执行）。
pub fn file_spec(file: &Path) -> CommandSpec {
    #[cfg(target_os = "windows")]
    {
        // 空标题参数必须保留，否则 start 会把带引号的路径当成窗口标题。
        CommandSpec::new(
            "cmd",
            vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                file.display().to_string(),
            ],
        )
    }
    #[cfg(target_os = "macos")]
    {
        CommandSpec::new("open", vec![file.display().to_string()])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        CommandSpec::new("xdg-open", vec![file.display().to_string()])
    }
}

/// 在文件管理器中定位文件（失败只记录日志，不弹错误）。
pub fn open_folder(path: &Path) -> io::Result<()> {
    spawn_spec(&folder_spec(path))
}

/// 用默认程序打开文件。
pub fn open_file(path: &Path) -> io::Result<()> {
    spawn_spec(&file_spec(path))
}

fn spawn_spec(spec: &CommandSpec) -> io::Result<()> {
    tracing::info!(command = %spec.display(), "spawn");
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    #[cfg(target_os = "windows")]
    if let Some(raw) = &spec.raw_arg {
        // 原样追加，不让 Command 再加/移动引号（见模块文档的 Explorer 引号规则）。
        use std::os::windows::process::CommandExt;
        command.raw_arg(raw);
    }
    command.spawn().map(|_child| ())
}

/// 打开下载项的目录：优先 `<output_dir>`，其次已完成文件所在目录，最后 `state_dir`。
pub fn resolve_folder_target(
    completed_path: Option<&Path>,
    output_dir: Option<&Path>,
    state_dir: &Path,
) -> PathBuf {
    if let Some(path) = completed_path {
        return path.to_path_buf();
    }
    if let Some(dir) = output_dir {
        return dir.to_path_buf();
    }
    state_dir.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "rdm-gui-open-{tag}-{}-{}",
            std::process::id(),
            crate::model::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn folder_spec_selects_existing_file() {
        let dir = temp_dir("select");
        let file = dir.join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let spec = folder_spec(&file);
        assert_eq!(spec.program, "explorer");
        assert!(
            spec.args.is_empty(),
            "/select 必须走 raw_arg（只给路径加引号），不能作为普通参数：{:?}",
            spec.args
        );
        let raw = spec.raw_arg.expect("existing file must use /select");
        let raw = raw.to_string_lossy();
        assert!(raw.starts_with("/select,\""), "raw_arg={raw}");
        assert!(raw.ends_with('"'), "raw_arg={raw}");
        assert!(raw.contains("a.bin"), "raw_arg={raw}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 回归测试：路径含空格时**整参数加引号**会让 Explorer 回退到家目录/文档；
    /// 必须只给路径加引号（`/select,"C:\dir with space\a.bin"`）。
    #[cfg(target_os = "windows")]
    #[test]
    fn folder_spec_quotes_only_the_path_not_the_whole_select_argument() {
        let dir = temp_dir("with space");
        let file = dir.join("sel file (1).bin");
        std::fs::write(&file, b"x").unwrap();
        let spec = folder_spec(&file);
        let raw = spec
            .raw_arg
            .as_ref()
            .expect("existing file must use /select")
            .to_string_lossy()
            .into_owned();
        assert_eq!(raw, format!("/select,\"{}\"", file.display()));
        // 防回归：不能出现 `"/select,...`（引号在 /select 之前，Explorer 解析失败）。
        assert!(!raw.starts_with('"'), "raw_arg={raw}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn folder_spec_falls_back_to_parent_for_missing_file() {
        let dir = temp_dir("fallback");
        let file = dir.join("missing.bin");
        let spec = folder_spec(&file);
        assert_eq!(spec.program, "explorer");
        // 缺失文件退化为「打开父目录」：普通参数，不带 /select，也不走 raw_arg。
        assert_eq!(spec.args.len(), 1);
        assert!(!spec.args[0].starts_with("/select,"));
        assert!(spec.args[0].contains(dir.file_name().unwrap().to_str().unwrap()));
        assert!(spec.raw_arg.is_none(), "fallback must not use raw_arg");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn file_spec_uses_start_with_empty_title() {
        let spec = file_spec(Path::new(r"C:\downloads\a.bin"));
        assert_eq!(spec.program, "cmd");
        assert_eq!(spec.args[0], "/C");
        assert_eq!(spec.args[1], "start");
        assert_eq!(spec.args[2], "");
        assert_eq!(spec.args[3], r"C:\downloads\a.bin");
    }

    #[test]
    fn resolves_folder_target_precedence() {
        let state_dir = Path::new(r"C:\state");
        assert_eq!(
            resolve_folder_target(
                Some(Path::new(r"C:\downloads\a.bin")),
                Some(Path::new(r"D:\other")),
                state_dir
            ),
            PathBuf::from(r"C:\downloads\a.bin")
        );
        assert_eq!(
            resolve_folder_target(None, Some(Path::new(r"D:\other")), state_dir),
            PathBuf::from(r"D:\other")
        );
        assert_eq!(
            resolve_folder_target(None, None, state_dir),
            PathBuf::from(r"C:\state")
        );
    }
}
