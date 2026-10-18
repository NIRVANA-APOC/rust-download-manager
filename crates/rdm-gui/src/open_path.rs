//! 打开文件 / 在资源管理器中定位文件。
//!
//! Windows 用 `explorer /select,<path>`（文件存在时高亮选中；不存在时退化为打开父目录），
//! 「打开文件」用 `cmd /C start "" <path>`。macOS / Linux 分别用 `open` / `xdg-open`。
//! 命令只做一次性 spawn，不阻塞 GUI 主线程。

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    fn display(&self) -> String {
        format!("{} {}", self.program, self.args.join(" "))
    }
}

/// 生成「在文件管理器中定位」的命令（不执行）。
pub fn folder_spec(file: &Path) -> CommandSpec {
    #[cfg(target_os = "windows")]
    {
        if file.is_file() {
            CommandSpec::new("explorer", vec![format!("/select,{}", file.display())])
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
    Command::new(&spec.program)
        .args(&spec.args)
        .spawn()
        .map(|_child| ())
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
        assert_eq!(spec.args.len(), 1);
        assert!(spec.args[0].starts_with("/select,"));
        assert!(spec.args[0].contains("a.bin"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn folder_spec_falls_back_to_parent_for_missing_file() {
        let dir = temp_dir("fallback");
        let file = dir.join("missing.bin");
        let spec = folder_spec(&file);
        assert_eq!(spec.program, "explorer");
        assert!(!spec.args[0].starts_with("/select,"));
        assert!(spec.args[0].contains(dir.file_name().unwrap().to_str().unwrap()));
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
