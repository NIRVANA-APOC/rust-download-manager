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

/// 生成「打开目录本身」的命令（不执行）。
///
/// **必须与 [`reveal_spec`] 区分**：旧实现把「目录」误当成文件走 `parent()`，导致
/// 「打开下载目录/状态目录」实际打开的是上一级目录（2026-10-22 用户回归）。
pub fn directory_spec(dir: &Path) -> CommandSpec {
    #[cfg(target_os = "windows")]
    {
        // Explorer 对普通目录参数接受标准引号；含空格/中文由 Command 自动加引号。
        CommandSpec::new("explorer", vec![dir.display().to_string()])
    }
    #[cfg(target_os = "macos")]
    {
        CommandSpec::new("open", vec![dir.display().to_string()])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        CommandSpec::new("xdg-open", vec![dir.display().to_string()])
    }
}

/// 生成「在文件管理器中显示 `path`」的命令（不执行）。
///
/// - `path` 是目录（存在）→ 打开该目录本身；
/// - `path` 是文件 → Explorer `/select,"<path>"` 高亮选中；
/// - `path` 不存在 → 打开其父目录（父目录存在时），否则打开 `path`。
///
/// 下载项「打开文件夹」传入的是已完成文件路径，因此会高亮文件；监控页「打开下载目录」
/// 应使用 [`directory_spec`]，不要经过这里。
pub fn reveal_spec(path: &Path) -> CommandSpec {
    if path.is_dir() {
        return directory_spec(path);
    }
    #[cfg(target_os = "windows")]
    {
        if path.is_file() {
            // 只给路径加引号：`/select,"C:\dir with space\a.bin"`。
            CommandSpec::new("explorer", Vec::new()).with_raw_arg(select_arg(path))
        } else if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            CommandSpec::new("explorer", vec![parent.display().to_string()])
        } else {
            CommandSpec::new("explorer", vec![path.display().to_string()])
        }
    }
    #[cfg(target_os = "macos")]
    {
        if path.is_file() {
            CommandSpec::new("open", vec!["-R".to_string(), path.display().to_string()])
        } else if let Some(parent) = path.parent() {
            CommandSpec::new("open", vec![parent.display().to_string()])
        } else {
            CommandSpec::new("open", vec![path.display().to_string()])
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent().unwrap_or(path).to_path_buf()
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

/// 打开一个**目录本身**（监控页「打开下载目录/状态目录」、托盘菜单等）。
///
/// 目录不存在时返回 `NotFound`，由调用方 toast 明确告知；绝不静默退回父目录
/// （旧实现就是这样导致「打开下载目录」跳到上一级）。
pub fn open_directory(dir: &Path) -> io::Result<()> {
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("目录不存在：{}", dir.display()),
        ));
    }
    spawn_spec(&directory_spec(dir))
}

/// 在文件管理器中显示文件/目录：文件高亮选中，目录直接打开。
///
/// 下载项「打开文件夹」与检查器入口统一走这里；目录会打开自身而不是父目录。
pub fn reveal_path(path: &Path) -> io::Result<()> {
    spawn_spec(&reveal_spec(path))
}

/// 用默认程序打开文件（文件不存在时报错，避免 `cmd start` 静默失败）。
pub fn open_file(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("文件不存在：{}", path.display()),
        ));
    }
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

/// 「打开文件夹」的定位目标 —— **完成项返回文件路径**（交给 [`reveal_path`] 在
/// Explorer 里高亮），未完成项返回目录（交给 [`open_directory`] 打开）。
///
/// 审计 F-18：旧名 `resolve_folder_target` 容易被误读成「总是返回目录」；这里的返回值
/// 是「reveal/open 的目标」，调用方必须先判断它是文件还是目录，不能直接当目录打开。
pub fn reveal_target_for_item(
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
    fn reveal_spec_selects_existing_file() {
        let dir = temp_dir("select");
        let file = dir.join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let spec = reveal_spec(&file);
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
    fn reveal_spec_quotes_only_the_path_not_the_whole_select_argument() {
        let dir = temp_dir("with space");
        let file = dir.join("sel file (1).bin");
        std::fs::write(&file, b"x").unwrap();
        let spec = reveal_spec(&file);
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
    fn reveal_spec_falls_back_to_parent_for_missing_file() {
        let dir = temp_dir("fallback");
        let file = dir.join("missing.bin");
        let spec = reveal_spec(&file);
        assert_eq!(spec.program, "explorer");
        // 缺失文件退化为「打开父目录」：普通参数，不带 /select，也不走 raw_arg。
        assert_eq!(spec.args.len(), 1);
        assert!(!spec.args[0].starts_with("/select,"));
        assert!(spec.args[0].contains(dir.file_name().unwrap().to_str().unwrap()));
        assert!(spec.raw_arg.is_none(), "fallback must not use raw_arg");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 回归测试（2026-10-22 用户反馈）：目录必须打开自身，不能走 `parent()`。
    #[test]
    fn directory_spec_targets_the_directory_itself() {
        let dir = temp_dir("dir-target");
        let nested = dir.join("RDM").join("Downloads with space");
        std::fs::create_dir_all(&nested).unwrap();
        let spec = directory_spec(&nested);
        assert_eq!(
            spec.args.last().map(String::as_str),
            Some(nested.to_string_lossy().as_ref()),
            "directory_spec must open the directory itself, got {spec:?}"
        );
        assert!(spec.raw_arg.is_none(), "directory must not use /select");
        // 新 API 的语义回归：reveal(existing dir) 也必须打开目录自身。
        let revealed = reveal_spec(&nested);
        assert_eq!(
            revealed.args.last().map(String::as_str),
            Some(nested.to_string_lossy().as_ref()),
            "reveal_spec(dir) must open the directory itself, got {revealed:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_directory_reports_missing_directory_without_spawning() {
        let missing = Path::new(r"C:\definitely-not-exists-rdm\Downloads");
        let err = open_directory(missing).expect_err("missing directory must fail fast");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("目录不存在"), "{err}");
    }

    #[test]
    fn open_file_reports_missing_file_without_spawning() {
        let missing = Path::new(r"C:\definitely-not-exists-rdm\a.bin");
        let err = open_file(missing).expect_err("missing file must fail fast");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("文件不存在"), "{err}");
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
    fn reveal_target_precedence_for_completed_and_pending_items() {
        let state_dir = Path::new(r"C:\state");
        assert_eq!(
            reveal_target_for_item(
                Some(Path::new(r"C:\downloads\a.bin")),
                Some(Path::new(r"D:\other")),
                state_dir
            ),
            PathBuf::from(r"C:\downloads\a.bin")
        );
        assert_eq!(
            reveal_target_for_item(None, Some(Path::new(r"D:\other")), state_dir),
            PathBuf::from(r"D:\other")
        );
        assert_eq!(
            reveal_target_for_item(None, None, state_dir),
            PathBuf::from(r"C:\state")
        );
    }
}
