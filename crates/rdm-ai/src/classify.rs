//! Download classification (`docs/design-next-features.md` §3.6).
//!
//! Two layers:
//! * deterministic local rules (file extension first, then Content-Type),
//! * optional AI classification that may suggest an existing or a new directory.
//!
//! Both never panic and both return a Chinese directory name (`""` = save to
//! the root directory).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::client::AiClient;
use crate::config::AiConfig;
use crate::{extract_json_object, truncate_chars};

/// Directory name used when nothing matches.
pub const CATEGORY_OTHER: &str = "其他";

/// Longest accepted directory name (characters).
pub const MAX_DIR_NAME_CHARS: usize = 40;

/// One classification bucket: resource kind + Chinese directory + matching rules.
struct KindSpec {
    kind: &'static str,
    category: &'static str,
    extensions: &'static [&'static str],
    content_prefixes: &'static [&'static str],
    content_types: &'static [&'static str],
}

const EMPTY: &[&str] = &[];

/// Single source of truth for extension / Content-Type → kind/category mapping.
const KINDS: &[KindSpec] = &[
    KindSpec {
        kind: "video",
        category: "视频",
        extensions: &[
            "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mpg", "mpeg", "3gp",
        ],
        content_prefixes: &["video/"],
        content_types: EMPTY,
    },
    KindSpec {
        kind: "audio",
        category: "音频",
        extensions: &["mp3", "flac", "wav", "aac", "ogg", "m4a", "opus", "wma"],
        content_prefixes: &["audio/"],
        content_types: EMPTY,
    },
    KindSpec {
        kind: "image",
        category: "图片",
        extensions: &[
            "jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff", "heic", "avif",
        ],
        content_prefixes: &["image/"],
        content_types: EMPTY,
    },
    KindSpec {
        kind: "archive",
        category: "压缩包",
        extensions: &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso"],
        content_prefixes: EMPTY,
        content_types: &[
            "application/zip",
            "application/x-zip-compressed",
            "application/x-7z-compressed",
            "application/x-rar-compressed",
            "application/vnd.rar",
            "application/x-tar",
            "application/gzip",
            "application/x-gzip",
            "application/x-bzip2",
            "application/x-xz",
            "application/zstd",
            "application/x-zstd",
            "application/x-iso9660-image",
        ],
    },
    KindSpec {
        kind: "document",
        category: "文档",
        extensions: &[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "csv", "epub", "mobi",
            "azw3",
        ],
        content_prefixes: &["text/"],
        content_types: &[
            "application/pdf",
            "application/epub+zip",
            "application/msword",
            "application/rtf",
            "application/vnd.ms-excel",
            "application/vnd.ms-powerpoint",
            "application/vnd.openxmlformats-officedocument",
        ],
    },
    KindSpec {
        kind: "program",
        category: "程序安装包",
        extensions: &[
            "exe", "msi", "msix", "appx", "apk", "ipa", "deb", "rpm", "appimage",
        ],
        content_prefixes: EMPTY,
        content_types: &[
            "application/vnd.android.package-archive",
            "application/x-msdownload",
            "application/x-msi",
            "application/vnd.msi",
            "application/msix",
            "application/vnd.ms-appx",
            "application/x-debian-package",
            "application/x-rpm",
        ],
    },
];

/// English resource kind (`video`/`audio`/`image`/`archive`/`document`/
/// `program`/`other`). Extension wins over Content-Type.
pub(crate) fn resource_kind(file_name: &str, content_type: Option<&str>) -> &'static str {
    if let Some(ext) = extension_of(file_name) {
        if let Some(spec) = spec_for_extension(&ext) {
            return spec.kind;
        }
    }
    if let Some(content_type) = content_type {
        if let Some(spec) = spec_for_content_type(content_type) {
            return spec.kind;
        }
    }
    "other"
}

fn spec_for_extension(extension: &str) -> Option<&'static KindSpec> {
    KINDS
        .iter()
        .find(|spec| spec.extensions.contains(&extension))
}

fn spec_for_content_type(content_type: &str) -> Option<&'static KindSpec> {
    let normalized = normalize_content_type(content_type);
    if normalized.is_empty() {
        return None;
    }
    KINDS.iter().find(|spec| {
        spec.content_prefixes
            .iter()
            .any(|prefix| normalized.starts_with(prefix))
            || spec.content_types.iter().any(|known| normalized == *known)
    })
}

fn spec_for_kind(kind: &str) -> Option<&'static KindSpec> {
    KINDS.iter().find(|spec| spec.kind == kind)
}

fn normalize_content_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Extract a lowercase, plausible extension from a file name.
pub(crate) fn extension_of(file_name: &str) -> Option<String> {
    let name = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name)
        .trim();
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let extension = extension.trim().to_ascii_lowercase();
    if extension.is_empty()
        || extension.len() > 12
        || !extension.chars().all(|ch| ch.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(extension)
}

/// Local extension/Content-Type → Chinese category directory name.
pub fn category_for(file_name: &str, content_type: Option<&str>) -> String {
    let kind = resource_kind(file_name, content_type);
    spec_for_kind(kind)
        .map(|spec| spec.category.to_owned())
        .unwrap_or_else(|| CATEGORY_OTHER.to_owned())
}

/// First-level sub-directories of `root` (directories only, no hidden ones,
/// sorted). A missing/unreadable root yields an empty list.
pub fn list_subdirs(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return names;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(_) => entry.path().is_dir(),
        };
        if is_dir {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// Clean a directory name suggested by an LLM.
///
/// Rejects empty values, control characters, path separators, Windows-reserved
/// characters, absolute paths, `.`/`..`, trailing/leading dots or whitespace and
/// Windows device names; accepts 1..=[`MAX_DIR_NAME_CHARS`] characters.
pub fn sanitize_dir_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().any(|ch| {
        ch.is_control()
            || matches!(
                ch,
                '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '\0'
            )
    }) {
        return None;
    }
    let cleaned = trimmed.trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    let length = cleaned.chars().count();
    if length == 0 || length > MAX_DIR_NAME_CHARS {
        return None;
    }
    let stem = cleaned.split('.').next().unwrap_or(cleaned).trim();
    if is_windows_reserved(stem) {
        return None;
    }
    Some(cleaned.to_owned())
}

fn is_windows_reserved(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let bytes = upper.as_bytes();
    if bytes.len() == 4
        && (&bytes[..3] == b"COM" || &bytes[..3] == b"LPT")
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
    {
        return true;
    }
    false
}

/// Input of the classification pipeline.
#[derive(Debug, Clone)]
pub struct ClassifyInput {
    pub url: String,
    pub file_name: String,
    pub content_type: Option<String>,
    pub root_dir: PathBuf,
    /// Usually `list_subdirs(&root_dir)`.
    pub existing_dirs: Vec<String>,
}

/// Classification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyOutcome {
    /// `""` = save directly to `root_dir`.
    pub dir_name: String,
    /// Short user-facing reason.
    pub reason: String,
    pub ai_used: bool,
    /// `true` = the directory has to be created (engine falls back to
    /// `create_dir_all`).
    pub new_dir: bool,
}

/// Pure local rules: extension/Content-Type bucket; reuses an existing directory
/// whose name matches case-insensitively.
pub fn classify_local(input: &ClassifyInput) -> ClassifyOutcome {
    let Some((category, matched)) = matched_rule(&input.file_name, input.content_type.as_deref())
    else {
        return ClassifyOutcome {
            dir_name: String::new(),
            reason: "未识别的文件类型，保存到根目录".to_owned(),
            ai_used: false,
            new_dir: false,
        };
    };
    let (dir_name, new_dir) = resolve_existing(&input.existing_dirs, category);
    ClassifyOutcome {
        dir_name,
        reason: format!("按 {matched} 归入「{category}」"),
        ai_used: false,
        new_dir,
    }
}

/// `(category, matched-rule-description)` for the first matching local rule.
fn matched_rule(file_name: &str, content_type: Option<&str>) -> Option<(&'static str, String)> {
    if let Some(extension) = extension_of(file_name) {
        if let Some(spec) = spec_for_extension(&extension) {
            return Some((spec.category, format!(".{extension}")));
        }
    }
    if let Some(content_type) = content_type {
        if let Some(spec) = spec_for_content_type(content_type) {
            return Some((spec.category, normalize_content_type(content_type)));
        }
    }
    None
}

fn resolve_existing(existing_dirs: &[String], desired: &str) -> (String, bool) {
    for existing in existing_dirs {
        let trimmed = existing.trim();
        if !trimmed.is_empty() && trimmed.eq_ignore_ascii_case(desired) {
            return (trimmed.to_owned(), false);
        }
    }
    (desired.to_owned(), true)
}

/// Ask the LLM to classify the file. Returns `None` when AI is not configured,
/// the call fails, or the suggested directory is unusable — callers then fall
/// back to [`classify_local`].
pub async fn classify_with_ai(config: &AiConfig, input: &ClassifyInput) -> Option<ClassifyOutcome> {
    if !config.is_configured() {
        return None;
    }
    let client = AiClient::new(crate::client::bounded_config(config)).ok()?;

    let system = "你是 RDM 下载管理器的下载目录分类助手。根据文件名、链接和候选目录，\
选择最合适的一级目录，或建议一个新的目录名。只输出一个 JSON 对象，不要输出解释或 Markdown：\
{\"dir\":\"目录名\",\"create\":false,\"reason\":\"简短理由\"}。\
dir 为空字符串表示直接保存到根目录；目录名不能包含路径分隔符、绝对路径或 Windows 保留字符。";

    let mut existing = input
        .existing_dirs
        .iter()
        .map(|dir| dir.trim())
        .filter(|dir| !dir.is_empty())
        .take(50)
        .collect::<Vec<_>>()
        .join("、");
    if existing.is_empty() {
        existing.push_str("（无）");
    }
    let content_type = input.content_type.as_deref().unwrap_or("（未知）");
    let user = format!(
        "文件名: {}\n下载链接: {}\nContent-Type: {}\n现有可选目录: {}\n请输出 JSON。",
        input.file_name,
        truncate_chars(&input.url, 500),
        truncate_chars(content_type, 120),
        existing
    );

    let reply = client.chat(system, &user).await.ok()?;
    let json = extract_json_object(&reply)?;
    let value: Value = serde_json::from_str(json).ok()?;
    let dir_raw = value.get("dir")?.as_str()?.trim();
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(|reason| truncate_chars(reason, 120))
        .unwrap_or_else(|| "AI 分类".to_owned());

    // Empty `dir` is the model's way of saying "save to the root directory".
    if dir_raw.is_empty() {
        return Some(ClassifyOutcome {
            dir_name: String::new(),
            reason,
            ai_used: true,
            new_dir: false,
        });
    }

    let dir_name = sanitize_dir_name(dir_raw)?;
    let (dir_name, new_dir) = resolve_existing(&input.existing_dirs, &dir_name);
    tracing::debug!(
        file = %input.file_name,
        dir = %dir_name,
        "rdm-ai classify: using the AI suggestion"
    );
    Some(ClassifyOutcome {
        dir_name,
        reason,
        ai_used: true,
        new_dir,
    })
}

/// Unified entry point: try AI when enabled+configured, otherwise (or on any AI
/// failure) use the local rules. Never panics.
pub async fn classify(input: &ClassifyInput, config: &AiConfig, use_ai: bool) -> ClassifyOutcome {
    if use_ai && config.is_configured() {
        if let Some(outcome) = classify_with_ai(config, input).await {
            return outcome;
        }
    }
    classify_local(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(file_name: &str, existing_dirs: &[&str]) -> ClassifyInput {
        ClassifyInput {
            url: "https://example.com/path".to_owned(),
            file_name: file_name.to_owned(),
            content_type: None,
            root_dir: PathBuf::from("C:/Downloads"),
            existing_dirs: existing_dirs.iter().map(|dir| (*dir).to_owned()).collect(),
        }
    }

    #[test]
    fn extension_mapping_is_complete() {
        let cases = [
            ("a.mp4", "视频"),
            ("a.MKV", "视频"),
            ("a.3gp", "视频"),
            ("a.mp3", "音频"),
            ("a.FLAC", "音频"),
            ("a.jpeg", "图片"),
            ("a.avif", "图片"),
            ("a.zip", "压缩包"),
            ("a.tar.gz", "压缩包"),
            ("a.iso", "压缩包"),
            ("a.pdf", "文档"),
            ("a.epub", "文档"),
            ("a.exe", "程序安装包"),
            ("a.APK", "程序安装包"),
            ("a.unknown", CATEGORY_OTHER),
            ("no_extension", CATEGORY_OTHER),
            (".hidden", CATEGORY_OTHER),
        ];
        for (name, expected) in cases {
            assert_eq!(category_for(name, None), expected, "{name}");
        }
    }

    #[test]
    fn content_type_mapping() {
        let cases = [
            ("file.bin", Some("video/mp4"), "视频"),
            ("file.bin", Some("audio/mpeg; charset=binary"), "音频"),
            ("file.bin", Some("image/svg+xml"), "图片"),
            ("file.bin", Some("application/pdf"), "文档"),
            ("file.bin", Some("text/plain"), "文档"),
            ("file.bin", Some("application/zip"), "压缩包"),
            ("file.bin", Some("application/x-7z-compressed"), "压缩包"),
            (
                "file.bin",
                Some("application/vnd.android.package-archive"),
                "程序安装包",
            ),
            ("file.bin", Some("application/octet-stream"), CATEGORY_OTHER),
            ("file.bin", None, CATEGORY_OTHER),
        ];
        for (name, content_type, expected) in cases {
            assert_eq!(
                category_for(name, content_type),
                expected,
                "{name} {content_type:?}"
            );
        }
        // Extension wins over Content-Type.
        assert_eq!(
            category_for("clip.mp4", Some("application/octet-stream")),
            "视频"
        );
    }

    #[test]
    fn list_subdirs_only_visible_directories() {
        let root = std::env::temp_dir().join(format!(
            "rdm-ai-list-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        std::fs::create_dir_all(root.join("Videos")).unwrap();
        std::fs::create_dir_all(root.join("music")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();
        let dirs = list_subdirs(&root);
        assert_eq!(dirs, vec!["Videos".to_owned(), "music".to_owned()]);
        let _ = std::fs::remove_dir_all(&root);
        assert!(list_subdirs(Path::new("C:/does/not/exist/anywhere")).is_empty());
    }

    #[test]
    fn sanitize_accepts_valid_names() {
        assert_eq!(sanitize_dir_name("视频").as_deref(), Some("视频"));
        assert_eq!(
            sanitize_dir_name("  Music 2024 ").as_deref(),
            Some("Music 2024")
        );
        assert_eq!(sanitize_dir_name("...docs...").as_deref(), Some("docs"));
        assert_eq!(sanitize_dir_name("a.b").as_deref(), Some("a.b"));
        let forty = "a".repeat(40);
        assert_eq!(sanitize_dir_name(&forty).as_deref(), Some(forty.as_str()));
    }

    #[test]
    fn sanitize_rejects_invalid_names() {
        let cases = [
            "",
            "   ",
            ".",
            "..",
            "/absolute",
            "\\absolute",
            "a/b",
            "a\\b",
            "C:\\Windows",
            "a:b",
            "a*b",
            "a?b",
            "a|b",
            "a\"b",
            "a<b",
            "a>b",
            "line\nbreak",
            "CON",
            "con",
            "PRN",
            "NUL",
            "COM1",
            "lpt9",
            "CON.txt",
            "..\\..\\evil",
        ];
        for case in cases {
            assert!(sanitize_dir_name(case).is_none(), "should reject {case:?}");
        }
        // Leading/trailing dots are stripped, not rejected.
        assert_eq!(sanitize_dir_name("名字.").as_deref(), Some("名字"));
        assert!(sanitize_dir_name(&"a".repeat(41)).is_none());
        // `COM0` / `COM10` are ordinary names on Windows.
        assert!(sanitize_dir_name("COM0").is_some());
        assert!(sanitize_dir_name("COM10").is_some());
    }

    #[test]
    fn resolve_existing_is_case_insensitive() {
        let (name, new_dir) = resolve_existing(&["Videos".to_owned()], "videos");
        assert_eq!(name, "Videos");
        assert!(!new_dir);
        let (name, new_dir) = resolve_existing(&["other".to_owned()], "movies");
        assert_eq!(name, "movies");
        assert!(new_dir);
    }

    #[test]
    fn local_classification_reuses_existing_directory() {
        let outcome = classify_local(&input("movie.mp4", &["videos", "视频"]));
        assert_eq!(outcome.dir_name, "视频");
        assert!(!outcome.new_dir);
        assert!(!outcome.ai_used);
        assert!(outcome.reason.contains("视频"));

        let outcome = classify_local(&input("movie.mp4", &[]));
        assert_eq!(outcome.dir_name, "视频");
        assert!(outcome.new_dir);

        let outcome = classify_local(&input("random.bin", &["视频"]));
        assert_eq!(outcome.dir_name, "");
        assert!(!outcome.new_dir);
        assert!(outcome.reason.contains("根目录"));
    }

    #[test]
    fn local_classification_ignores_blank_existing_dirs() {
        let outcome = classify_local(&input("song.mp3", &["", "   "]));
        assert_eq!(outcome.dir_name, "音频");
        assert!(outcome.new_dir);
    }

    #[tokio::test]
    async fn classify_without_ai_uses_local_rules() {
        // AI is disabled in the default config, so the environment variable
        // cannot affect this path.
        let config = AiConfig::default();
        let outcome = classify(&input("movie.mkv", &[]), &config, true).await;
        assert_eq!(outcome.dir_name, "视频");
        assert!(!outcome.ai_used);
    }
}
