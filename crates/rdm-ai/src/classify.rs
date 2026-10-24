//! Download classification — deterministic by file extension, with a
//! Content-Type fallback (`docs/design/16-reuse-and-simplification.md` §4).
//!
//! **AI classification is gone**: there is no prompt, no model call and no token
//! accounting on this path. Classification is a pure function of the configured
//! suffix lists, so it is free, instant and explainable.
//!
//! Single source of truth:
//! * [`CategoryRule`] + [`default_category_rules`] — one bucket per resource kind
//!   with its display label (also the first-level directory name) and the
//!   suffixes that belong to it. `docs/design/16` §4.1 requires the GUI filters,
//!   the download engine and the settings editor to all derive from this list;
//! * [`category_for_with_rules`] / [`classify_local_with_rules`] — the lookups
//!   used by the engine and the GUI.
//!
//! Rules never panic and always return a Chinese directory label (`""` = save to
//! the root directory).

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::truncate_chars;

/// Directory name used when nothing matches (the `other` rule's default label).
pub const CATEGORY_OTHER: &str = "其他";

/// Kinds known to the classifier and the GUI, in display order.
///
/// `other` must stay last: it is the catch-all rule and carries no extensions.
pub const CATEGORY_KINDS: [&str; 7] = [
    "video", "audio", "document", "image", "archive", "program", "other",
];

/// Longest accepted directory name (characters).
pub const MAX_DIR_NAME_CHARS: usize = 40;

/// Upper bound for a normalized display filename (characters).
pub const MAX_FILE_NAME_CHARS: usize = 180;

/// Longest accepted suffix (characters); matches what the settings editor may
/// store so a configured suffix can actually be matched against a file name.
pub const MAX_EXTENSION_CHARS: usize = 16;

/// Upper bound for one category's suffix list.
pub const MAX_EXTENSIONS_PER_CATEGORY: usize = 64;

// ---------------------------------------------------------------------------
// Category rules (the single source of truth)
// ---------------------------------------------------------------------------

/// One classification bucket (`docs/design/16-reuse-and-simplification.md` §4.1).
///
/// `kind` is the stable machine name (`video`/`audio`/…/`other`), `label` is both
/// the UI display name and the on-disk first-level directory, and `extensions`
/// are lowercase suffixes without a leading dot (`mp4`, `tar`, …). Order = display
/// order; the `other` rule is the catch-all and has no extensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryRule {
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub extensions: Vec<String>,
}

impl CategoryRule {
    /// Convenience constructor used by [`default_category_rules`] and tests.
    pub fn new(kind: impl Into<String>, label: impl Into<String>, extensions: &[&str]) -> Self {
        Self {
            kind: kind.into(),
            label: label.into(),
            extensions: extensions
                .iter()
                .map(|extension| (*extension).to_owned())
                .collect(),
        }
    }
}

/// Fallback used when a caller passes a rule list that has no `other` rule at
/// all (hand-edited JSON); an empty label means "save to the root directory".
static FALLBACK_RULE: CategoryRule = CategoryRule {
    kind: String::new(),
    label: String::new(),
    extensions: Vec::new(),
};

fn build_default_category_rules() -> Vec<CategoryRule> {
    // Merges the pre-existing `rdm-ai` KINDS tables with the extension lists the
    // GUI `FileCategory` used to hard-code (rmvb / dmg / pkg / json / xml / html).
    // A suffix belongs to exactly one category: the first rule in this order
    // claims it (hence `iso` is an archive, not a program).
    vec![
        CategoryRule::new(
            "video",
            "视频",
            &[
                "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "rmvb", "ts", "mpg",
                "mpeg", "3gp",
            ],
        ),
        CategoryRule::new(
            "audio",
            "音频",
            &["mp3", "flac", "wav", "aac", "ogg", "m4a", "opus", "wma"],
        ),
        CategoryRule::new(
            "document",
            "文档",
            &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "csv", "json",
                "xml", "html", "epub", "mobi", "azw3",
            ],
        ),
        CategoryRule::new(
            "image",
            "图片",
            &[
                "jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff", "heic",
                "avif",
            ],
        ),
        CategoryRule::new(
            "archive",
            "压缩包",
            &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso"],
        ),
        CategoryRule::new(
            "program",
            "程序安装包",
            &[
                "exe", "msi", "msix", "appx", "apk", "ipa", "dmg", "pkg", "deb", "rpm", "appimage",
            ],
        ),
        CategoryRule::new("other", CATEGORY_OTHER, &[]),
    ]
}

fn static_default_rules() -> &'static [CategoryRule] {
    static DEFAULT_RULES: OnceLock<Vec<CategoryRule>> = OnceLock::new();
    DEFAULT_RULES.get_or_init(build_default_category_rules)
}

/// The built-in classification rules (one bucket per [`CATEGORY_KINDS`] entry,
/// `other` last and extension-less).
pub fn default_category_rules() -> Vec<CategoryRule> {
    static_default_rules().to_vec()
}

/// Normalize a suffix typed by a user: trim, drop leading dots, lowercase.
/// Rejects empty values, non-alphanumeric values and overly long suffixes.
fn sanitize_extension(raw: &str) -> Option<String> {
    let extension = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    if extension.is_empty() || extension.chars().count() > MAX_EXTENSION_CHARS {
        return None;
    }
    if !extension.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(extension)
}

/// Sanitize a complete rule list (`Settings::normalized` + GUI snapshot).
///
/// Guarantees, in this order:
/// 1. exactly one rule per [`CATEGORY_KINDS`] entry, in that canonical order
///    (unknown kinds are dropped, missing kinds fall back to their default);
/// 2. every label is a safe Windows directory name (invalid → that kind's default
///    label) and `other` keeps its extension-less catch-all semantics;
/// 3. suffixes are lowercase/alphanumeric/no-leading-dot, deduped globally so a
///    suffix is claimed by exactly one category (first in canonical order wins),
///    bounded to [`MAX_EXTENSION_CHARS`] and [`MAX_EXTENSIONS_PER_CATEGORY`].
///
/// An empty/invalid input degrades to [`default_category_rules`], so a broken
/// `settings.json` can never leave the user without classification.
pub fn sanitize_category_rules(rules: &[CategoryRule]) -> Vec<CategoryRule> {
    let defaults = static_default_rules();
    let mut sanitized = Vec::with_capacity(CATEGORY_KINDS.len());
    let mut claimed: HashSet<String> = HashSet::new();

    for kind in CATEGORY_KINDS {
        let default_rule = defaults
            .iter()
            .find(|rule| rule.kind == kind)
            .expect("default rules cover every category kind");
        let provided = rules
            .iter()
            .find(|rule| rule.kind.trim().eq_ignore_ascii_case(kind));
        let source = provided.unwrap_or(default_rule);
        let label = sanitize_dir_name(&source.label).unwrap_or_else(|| default_rule.label.clone());

        let mut extensions = Vec::new();
        if kind != "other" {
            for raw in &source.extensions {
                if extensions.len() >= MAX_EXTENSIONS_PER_CATEGORY {
                    break;
                }
                let Some(extension) = sanitize_extension(raw) else {
                    continue;
                };
                if !claimed.insert(extension.clone()) {
                    continue;
                }
                extensions.push(extension);
            }
        }
        sanitized.push(CategoryRule {
            kind: (*kind).to_owned(),
            label,
            extensions,
        });
    }
    sanitized
}

// ---------------------------------------------------------------------------
// Content-Type fallback table
// ---------------------------------------------------------------------------

/// Content-Type → kind fallback (only used when a file name has no usable suffix,
/// or to resolve suffix-less downloads such as signed CDN URLs).
struct ContentKindSpec {
    kind: &'static str,
    prefixes: &'static [&'static str],
    types: &'static [&'static str],
}

const EMPTY: &[&str] = &[];

const CONTENT_KINDS: &[ContentKindSpec] = &[
    ContentKindSpec {
        kind: "video",
        prefixes: &["video/"],
        types: EMPTY,
    },
    ContentKindSpec {
        kind: "audio",
        prefixes: &["audio/"],
        types: EMPTY,
    },
    ContentKindSpec {
        kind: "image",
        prefixes: &["image/"],
        types: EMPTY,
    },
    ContentKindSpec {
        kind: "archive",
        prefixes: EMPTY,
        types: &[
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
    ContentKindSpec {
        kind: "document",
        prefixes: &["text/"],
        types: &[
            "application/pdf",
            "application/epub+zip",
            "application/msword",
            "application/rtf",
            "application/vnd.ms-excel",
            "application/vnd.ms-powerpoint",
            "application/vnd.openxmlformats-officedocument",
        ],
    },
    ContentKindSpec {
        kind: "program",
        prefixes: EMPTY,
        types: &[
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

/// Extensions that are page/metadata formats rather than download payloads.
///
/// The *parse* heuristics deliberately keep treating them as `other` (a JSON/XML
/// API link or an HTML page is not a downloadable file), even though a saved
/// `.json`/`.html` file is classified as 文档 by the configurable rules above.
const PARSE_HEURISTIC_EXCLUDED_EXTENSIONS: &[&str] = &["json", "xml", "html", "htm"];

// ---------------------------------------------------------------------------
// Extension helpers
// ---------------------------------------------------------------------------

/// Registered Content-Type → canonical suffix (exact match, parameters ignored).
const CONTENT_TYPE_EXTENSIONS: &[(&str, &str)] = &[
    ("video/mp4", "mp4"),
    ("video/webm", "webm"),
    ("video/quicktime", "mov"),
    ("video/x-matroska", "mkv"),
    ("video/x-msvideo", "avi"),
    ("video/mp2t", "ts"),
    ("video/mpeg", "mpg"),
    ("video/3gpp", "3gp"),
    ("audio/mpeg", "mp3"),
    ("audio/mp4", "m4a"),
    ("audio/x-m4a", "m4a"),
    ("audio/aac", "aac"),
    ("audio/flac", "flac"),
    ("audio/x-flac", "flac"),
    ("audio/ogg", "ogg"),
    ("audio/opus", "opus"),
    ("audio/wav", "wav"),
    ("audio/x-wav", "wav"),
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("image/bmp", "bmp"),
    ("image/svg+xml", "svg"),
    ("image/avif", "avif"),
    ("image/heic", "heic"),
    ("application/pdf", "pdf"),
    ("application/epub+zip", "epub"),
    ("application/zip", "zip"),
    ("application/x-zip-compressed", "zip"),
    ("application/x-7z-compressed", "7z"),
    ("application/x-rar-compressed", "rar"),
    ("application/vnd.rar", "rar"),
    ("application/x-tar", "tar"),
    ("application/gzip", "gz"),
    ("application/x-gzip", "gz"),
    ("application/x-bzip2", "bz2"),
    ("application/x-xz", "xz"),
    ("application/zstd", "zst"),
    ("application/x-zstd", "zst"),
    ("application/x-iso9660-image", "iso"),
    ("application/vnd.android.package-archive", "apk"),
    ("application/x-msdownload", "exe"),
    ("application/x-msi", "msi"),
    ("application/vnd.msi", "msi"),
    ("application/msix", "msix"),
    ("application/vnd.ms-appx", "appx"),
    ("application/x-debian-package", "deb"),
    ("application/x-rpm", "rpm"),
];

/// Extension-less generic suffixes RDM still treats as a real file suffix.
/// Used only to decide whether a name already has an extension; an unknown
/// dotted suffix like `v1.2` must not stop normalization.
const GENERIC_EXTENSIONS: &[&str] = &[
    "bin", "json", "xml", "txt", "log", "dat", "db", "sqlite", "torrent", "srt", "ass", "vtt",
    "nfo", "yml", "yaml", "toml", "ini", "cfg", "html", "htm", "js", "css", "wasm",
];

/// True when `extension` is a suffix RDM recognizes as a real file extension.
/// Unknown suffixes (`v1.2` → `2`) are intentionally rejected.
pub fn is_known_extension(extension: &str) -> bool {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    !extension.is_empty()
        && (static_default_rules()
            .iter()
            .any(|rule| rule_claims_extension(rule, &extension))
            || GENERIC_EXTENSIONS.iter().any(|known| *known == extension))
}

/// Map a Content-Type to a canonical extension (`video/mp4` → `mp4`).
///
/// Exact registered types win; unknown `video/*`, `audio/*`, `image/*` and
/// `text/*` fall back to a conservative container/format suffix.
pub fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let normalized = normalize_content_type(content_type);
    if normalized.is_empty() {
        return None;
    }
    if let Some((_, extension)) = CONTENT_TYPE_EXTENSIONS
        .iter()
        .find(|(known, _)| *known == normalized)
    {
        return Some(extension);
    }
    if normalized.starts_with("video/") {
        return Some("mp4");
    }
    if normalized.starts_with("audio/") {
        return Some("m4a");
    }
    if normalized.starts_with("image/") {
        return Some("jpg");
    }
    if normalized.starts_with("text/") {
        return Some("txt");
    }
    None
}

fn default_extension_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "video" => Some("mp4"),
        "audio" => Some("m4a"),
        "image" => Some("jpg"),
        "archive" => Some("zip"),
        // Unknown document/program/other: `.bin` is the honest answer; do not
        // invent `.pdf`/`.exe` from a kind alone.
        "document" | "program" | "other" => Some("bin"),
        _ => None,
    }
}

fn kind_for_content_type(content_type: &str) -> Option<&'static str> {
    let normalized = normalize_content_type(content_type);
    if normalized.is_empty() {
        return None;
    }
    CONTENT_KINDS.iter().find_map(|spec| {
        let prefix_hit = spec
            .prefixes
            .iter()
            .any(|prefix| normalized.starts_with(prefix));
        let exact_hit = spec.types.iter().any(|known| normalized == *known);
        (prefix_hit || exact_hit).then_some(spec.kind)
    })
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
        || extension.chars().count() > MAX_EXTENSION_CHARS
        || !extension.chars().all(|ch| ch.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(extension)
}

fn extension_matches(rule_extension: &str, extension: &str) -> bool {
    let rule_extension = rule_extension.trim().trim_start_matches('.');
    !rule_extension.is_empty() && rule_extension.eq_ignore_ascii_case(extension)
}

/// `true` when `rule` claims `extension`; the catch-all `other` never claims one.
fn rule_claims_extension(rule: &CategoryRule, extension: &str) -> bool {
    rule.kind != "other"
        && rule
            .extensions
            .iter()
            .any(|rule_extension| extension_matches(rule_extension, extension))
}

// ---------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------

/// Content-Type fallback: prefer the configured rule that claims the canonical
/// suffix (`text/plain` → `txt` → whichever rule owns `txt`), then the kind
/// table (`text/*` → document), so a customized suffix list keeps working.
fn rule_for_content_type<'a>(
    content_type: &str,
    rules: &'a [CategoryRule],
) -> Option<&'a CategoryRule> {
    let normalized = normalize_content_type(content_type);
    if normalized.is_empty() {
        return None;
    }
    if let Some(extension) = extension_for_content_type(&normalized) {
        if let Some(rule) = rules
            .iter()
            .find(|rule| rule_claims_extension(rule, extension))
        {
            return Some(rule);
        }
    }
    let kind = kind_for_content_type(&normalized)?;
    rules.iter().find(|rule| rule.kind == kind)
}

/// The rule that classifies `file_name` under `rules` (extension first, then
/// Content-Type, then the `other` catch-all).
///
/// Falls back to an empty catch-all rule only when a hand-edited rule list has
/// no `other` rule at all — callers then save to the root directory.
pub fn category_for_with_rules<'a>(
    file_name: &str,
    content_type: Option<&str>,
    rules: &'a [CategoryRule],
) -> &'a CategoryRule {
    if let Some(extension) = extension_of(file_name) {
        if let Some(rule) = rules
            .iter()
            .find(|rule| rule_claims_extension(rule, &extension))
        {
            return rule;
        }
    }
    if let Some(content_type) = content_type {
        if let Some(rule) = rule_for_content_type(content_type, rules) {
            return rule;
        }
    }
    rules
        .iter()
        .find(|rule| rule.kind == "other")
        .unwrap_or(&FALLBACK_RULE)
}

/// Local extension/Content-Type → Chinese category directory name, using the
/// built-in rules (kept for callers that do not need configurable buckets, e.g.
/// resolver report mapping).
pub fn category_for(file_name: &str, content_type: Option<&str>) -> String {
    let rule = category_for_with_rules(file_name, content_type, static_default_rules());
    if rule.label.trim().is_empty() {
        CATEGORY_OTHER.to_owned()
    } else {
        rule.label.clone()
    }
}

/// English resource kind (`video`/`audio`/`image`/`archive`/`document`/
/// `program`/`other`) for the *parse* heuristics. Extension wins over
/// Content-Type. Deliberately narrower than [`category_for`]: page/metadata
/// suffixes (`json`/`xml`/`html`) stay `other` so they are not offered as
/// download candidates.
pub(crate) fn resource_kind(file_name: &str, content_type: Option<&str>) -> &'static str {
    if let Some(extension) = extension_of(file_name) {
        if PARSE_HEURISTIC_EXCLUDED_EXTENSIONS.contains(&extension.as_str()) {
            return "other";
        }
        if let Some(rule) = static_default_rules()
            .iter()
            .find(|rule| rule_claims_extension(rule, &extension))
        {
            return rule.kind.as_str();
        }
    }
    if let Some(content_type) = content_type {
        if let Some(kind) = kind_for_content_type(content_type) {
            return kind;
        }
    }
    "other"
}

// ---------------------------------------------------------------------------
// File-name normalization (unchanged behavior, shared by parse/resolver/GUI)
// ---------------------------------------------------------------------------

fn url_last_segment(url: &str) -> Option<&str> {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let segment = without_query.rsplit('/').next().unwrap_or("").trim();
    (!segment.is_empty()).then_some(segment)
}

fn extension_from_url(url: &str) -> Option<String> {
    let segment = url_last_segment(url)?;
    extension_of(segment).filter(|extension| is_known_extension(extension))
}

fn fit_existing_file_name(name: &str, extension: &str) -> String {
    if name.chars().count() <= MAX_FILE_NAME_CHARS {
        return name.to_owned();
    }
    let (stem, original_suffix) = name.rsplit_once('.').unwrap_or((name, extension));
    let max_stem = MAX_FILE_NAME_CHARS.saturating_sub(original_suffix.chars().count() + 1);
    let stem: String = stem.chars().take(max_stem).collect();
    let stem = stem.trim_end_matches([' ', '.']);
    if stem.is_empty() {
        format!("download.{original_suffix}")
    } else {
        format!("{stem}.{original_suffix}")
    }
}

fn fit_file_name(name: &str, extension: Option<&str>) -> String {
    let Some(extension) = extension else {
        return truncate_chars(name, MAX_FILE_NAME_CHARS);
    };
    let suffix = format!(".{extension}");
    let max_stem = MAX_FILE_NAME_CHARS.saturating_sub(suffix.chars().count());
    let stem: String = name.chars().take(max_stem).collect();
    let stem = stem.trim_end_matches([' ', '.']);
    if stem.is_empty() {
        return format!("download{suffix}");
    }
    format!("{stem}{suffix}")
}

/// Strip directories/control characters and Windows-forbidden characters from
/// a display filename. Does not add an extension or truncate (see
/// [`ensure_file_extension`]); an empty result means "nothing usable".
pub fn sanitize_file_name(raw: &str) -> String {
    let name = raw.trim();
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() {
            continue;
        }
        match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            _ => out.push(ch),
        }
    }
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }
    out.trim_start().to_string()
}

/// Ensure a parsed/verified resource filename carries a real extension.
///
/// Inference order (first hit wins):
/// 1. an existing known extension is preserved;
/// 2. `Content-Type` (`video/mp4` → `.mp4`, `audio/mp4` → `.m4a`, …);
/// 3. the final resource URL's last path segment (signed URLs often end in
///    `.mp4` even when the page title has no suffix);
/// 4. a conservative per-kind default (video→`.mp4`, audio→`.m4a`, image→`.jpg`,
///    archive→`.zip`, other→`.bin`).
///
/// Path separators/control chars are removed first; the result is bounded to
/// [`MAX_FILE_NAME_CHARS`] characters while keeping the suffix. This is the
/// shared normalizer for resolver manifests, legacy AI parse reports and the
/// GUI task names, so an extensionless model title like a Bilibili video title
/// becomes `标题 (720P).mp4` instead of a document-looking `标题 (720P)`.
pub fn ensure_file_extension(
    name: &str,
    content_type: Option<&str>,
    url: Option<&str>,
    kind: Option<&str>,
) -> String {
    let mut name = sanitize_file_name(name);
    if name.is_empty() {
        if let Some(url) = url {
            if let Some(segment) = url_last_segment(url) {
                name = sanitize_file_name(segment);
            }
        }
    }
    if name.is_empty() {
        name = "download.bin".to_owned();
    }

    if let Some(existing) = extension_of(&name) {
        if is_known_extension(&existing) {
            return fit_existing_file_name(&name, &existing);
        }
    }

    let inferred: Option<String> = content_type
        .and_then(extension_for_content_type)
        .map(str::to_owned)
        .or_else(|| url.and_then(extension_from_url))
        .or_else(|| kind.and_then(default_extension_for_kind).map(str::to_owned));
    match inferred {
        Some(extension) => fit_file_name(&name, Some(&extension)),
        None => fit_file_name(&name, None),
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Input of [`classify_local_with_rules`].
#[derive(Debug, Clone, Default)]
pub struct ClassifyInput {
    /// Best-known file name (server metadata / URL segment).
    pub file_name: String,
    /// HTTP `Content-Type`, when a probe already ran.
    pub content_type: Option<String>,
    /// Usually `list_subdirs(&root_dir)`; used to reuse the on-disk spelling of
    /// an existing category directory (case-insensitive match).
    pub existing_dirs: Vec<String>,
}

/// Classification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyOutcome {
    /// `""` = save directly to the root directory.
    pub dir_name: String,
    /// Short user-facing reason.
    pub reason: String,
    /// `true` = the directory has to be created (engine falls back to
    /// `create_dir_all`).
    pub new_dir: bool,
}

/// Expand the reason with the rule that matched (`.mp4` or `video/mp4`).
fn matched_description(
    file_name: &str,
    content_type: Option<&str>,
    rule: &CategoryRule,
) -> Option<String> {
    if let Some(extension) = extension_of(file_name) {
        if rule_claims_extension(rule, &extension) {
            return Some(format!(".{extension}"));
        }
    }
    let content_type = content_type.map(normalize_content_type);
    content_type.filter(|content_type| !content_type.is_empty())
}

/// Deterministic classification: extension first, then Content-Type, then the
/// `other` catch-all (root directory). No AI, no network, no token usage.
///
/// `rules` should be the sanitized [`CategoryRule`] list from settings; an empty
/// slice degrades to [`default_category_rules`] so callers can never end up with
/// an unclassified file system by accident.
pub fn classify_local_with_rules(input: &ClassifyInput, rules: &[CategoryRule]) -> ClassifyOutcome {
    let fallback;
    let rules = if rules.is_empty() {
        fallback = default_category_rules();
        fallback.as_slice()
    } else {
        rules
    };
    let rule = category_for_with_rules(&input.file_name, input.content_type.as_deref(), rules);
    if rule.kind == "other" || rule.label.trim().is_empty() {
        return ClassifyOutcome {
            dir_name: String::new(),
            reason: "未识别的文件类型，保存到根目录".to_owned(),
            new_dir: false,
        };
    }
    let label = rule.label.trim();
    let reason = match matched_description(&input.file_name, input.content_type.as_deref(), rule) {
        Some(matched) => format!("按 {matched} 归入「{label}」"),
        None => format!("归入「{label}」"),
    };
    let (dir_name, new_dir) = resolve_existing(&input.existing_dirs, label);
    ClassifyOutcome {
        dir_name,
        reason,
        new_dir,
    }
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

/// Validate a directory name (also used for category labels in the settings
/// editor: `label` == first-level directory name).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(file_name: &str, existing_dirs: &[&str]) -> ClassifyInput {
        ClassifyInput {
            file_name: file_name.to_owned(),
            content_type: None,
            existing_dirs: existing_dirs.iter().map(|dir| (*dir).to_owned()).collect(),
        }
    }

    fn input_with_content_type(file_name: &str, content_type: &str) -> ClassifyInput {
        ClassifyInput {
            content_type: Some(content_type.to_owned()),
            ..input(file_name, &[])
        }
    }

    fn custom_rules(rules: &[(&str, &str, &[&str])]) -> Vec<CategoryRule> {
        rules
            .iter()
            .map(|(kind, label, extensions)| CategoryRule::new(*kind, *label, extensions))
            .collect()
    }

    #[test]
    fn extension_mapping_is_complete() {
        let cases = [
            ("a.mp4", "视频"),
            ("a.MKV", "视频"),
            ("a.3gp", "视频"),
            ("a.rmvb", "视频"),
            ("a.mp3", "音频"),
            ("a.FLAC", "音频"),
            ("a.jpeg", "图片"),
            ("a.avif", "图片"),
            ("a.zip", "压缩包"),
            ("a.tar.gz", "压缩包"),
            ("a.iso", "压缩包"),
            ("a.pdf", "文档"),
            ("a.epub", "文档"),
            ("a.json", "文档"),
            ("a.exe", "程序安装包"),
            ("a.APK", "程序安装包"),
            ("a.dmg", "程序安装包"),
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
    fn default_rules_cover_every_kind_in_display_order() {
        let rules = default_category_rules();
        let kinds: Vec<&str> = rules.iter().map(|rule| rule.kind.as_str()).collect();
        assert_eq!(kinds, CATEGORY_KINDS);
        assert_eq!(
            rules.last().map(|rule| rule.kind.as_str()),
            Some("other"),
            "other 必须是兜底且排在最后"
        );
        let other = rules.last().unwrap();
        assert_eq!(other.label, CATEGORY_OTHER);
        assert!(other.extensions.is_empty(), "other 不列后缀");

        let mut seen = HashSet::new();
        for rule in &rules {
            assert!(!rule.label.trim().is_empty(), "{rule:?}");
            for extension in &rule.extensions {
                assert_eq!(*extension, extension.to_ascii_lowercase(), "{rule:?}");
                assert!(!extension.starts_with('.'), "{rule:?}");
                assert!(seen.insert(extension.clone()), "后缀重复: {extension}");
            }
        }
    }

    #[test]
    fn custom_rules_override_the_default_mapping() {
        // Only `zzz` is a video now; the old `mp4` bucket is gone → root.
        let rules = custom_rules(&[
            ("video", "Movies", &["zzz"]),
            ("other", CATEGORY_OTHER, &[]),
        ]);
        let outcome = classify_local_with_rules(&input("clip.zzz", &[]), &rules);
        assert_eq!(outcome.dir_name, "Movies");
        assert!(outcome.new_dir);
        assert!(outcome.reason.contains(".zzz"), "{}", outcome.reason);

        let outcome = classify_local_with_rules(&input("clip.mp4", &[]), &rules);
        assert_eq!(outcome.dir_name, "", "未识别类型保存到根目录");
        assert!(outcome.reason.contains("根目录"));
    }

    #[test]
    fn custom_label_becomes_the_directory_name() {
        let rules = custom_rules(&[
            ("video", "视频收藏", &["mp4"]),
            ("other", CATEGORY_OTHER, &[]),
        ]);
        let outcome = classify_local_with_rules(&input("movie.mp4", &[]), &rules);
        assert_eq!(outcome.dir_name, "视频收藏");
    }

    #[test]
    fn empty_rules_degrade_to_the_builtin_defaults() {
        let outcome = classify_local_with_rules(&input("movie.mp4", &[]), &[]);
        assert_eq!(outcome.dir_name, "视频");
        assert!(outcome.new_dir);
    }

    #[test]
    fn content_type_fallback_uses_the_configured_rule_even_without_extensions() {
        // `document` keeps its label but lists no suffixes: only Content-Type can
        // still route a `text/plain` download into it.
        let rules = custom_rules(&[("document", "文档", &[]), ("other", CATEGORY_OTHER, &[])]);
        let outcome =
            classify_local_with_rules(&input_with_content_type("file.bin", "text/plain"), &rules);
        assert_eq!(outcome.dir_name, "文档");
        assert!(outcome.reason.contains("text/plain"), "{}", outcome.reason);
    }

    #[test]
    fn content_type_fallback_respects_a_moved_extension() {
        // User moved `iso` from 压缩包 to 程序安装包: the canonical suffix from
        // the Content-Type must win over the built-in kind table.
        let rules = custom_rules(&[
            ("archive", "压缩包", &["zip"]),
            ("program", "程序安装包", &["iso"]),
            ("other", CATEGORY_OTHER, &[]),
        ]);
        let outcome = classify_local_with_rules(
            &input_with_content_type("file.bin", "application/x-iso9660-image"),
            &rules,
        );
        assert_eq!(outcome.dir_name, "程序安装包");
    }

    #[test]
    fn local_classification_reuses_existing_directory() {
        let outcome = classify_local_with_rules(&input("movie.mp4", &["videos", "视频"]), &[]);
        assert_eq!(outcome.dir_name, "视频");
        assert!(!outcome.new_dir);
        assert!(outcome.reason.contains("视频"));

        let outcome = classify_local_with_rules(&input("movie.mp4", &[]), &[]);
        assert_eq!(outcome.dir_name, "视频");
        assert!(outcome.new_dir);

        let outcome = classify_local_with_rules(&input("random.bin", &["视频"]), &[]);
        assert_eq!(outcome.dir_name, "");
        assert!(!outcome.new_dir);
        assert!(outcome.reason.contains("根目录"));
    }

    #[test]
    fn local_classification_ignores_blank_existing_dirs() {
        let outcome = classify_local_with_rules(&input("song.mp3", &["", "   "]), &[]);
        assert_eq!(outcome.dir_name, "音频");
        assert!(outcome.new_dir);
    }

    #[test]
    fn sanitize_rules_normalizes_extensions_and_guarantees_every_kind() {
        let raw = custom_rules(&[
            ("video", " 视频 ", &[".MP4", "mp4", " MKV ", "bad-char", ""]),
            ("audio", "", &["mp3"]),
            ("unknown-kind", "X", &["zzz"]),
            ("other", CATEGORY_OTHER, &["should-disappear"]),
        ]);
        let sanitized = sanitize_category_rules(&raw);
        let kinds: Vec<&str> = sanitized.iter().map(|rule| rule.kind.as_str()).collect();
        assert_eq!(kinds, CATEGORY_KINDS, "缺失分类补默认、未知 kind 丢弃");

        let video = &sanitized[0];
        assert_eq!(video.label, "视频", "label 保留并 trim");
        assert_eq!(
            video.extensions,
            vec!["mp4", "mkv"],
            "小写/去点/去重/丢非法"
        );

        let audio = &sanitized[1];
        assert_eq!(audio.label, "音频", "空 label 回落到默认分类名");
        assert_eq!(audio.extensions, vec!["mp3"]);

        let image = &sanitized[3];
        assert_eq!(
            image.extensions,
            default_category_rules()[3].extensions,
            "缺失分类使用默认后缀"
        );

        let other = sanitized.last().unwrap();
        assert_eq!(other.kind, "other");
        assert!(other.extensions.is_empty(), "other 永远不列后缀");
    }

    #[test]
    fn sanitize_rules_removes_cross_category_duplicates() {
        let raw = custom_rules(&[
            ("video", "视频", &["zzz"]),
            ("audio", "音频", &["ZZZ", "mp3"]),
        ]);
        let sanitized = sanitize_category_rules(&raw);
        assert_eq!(sanitized[0].extensions, vec!["zzz"]);
        assert_eq!(
            sanitized[1].extensions,
            vec!["mp3"],
            "同一后缀只归属一个分类"
        );
        assert!(
            sanitized
                .iter()
                .flat_map(|rule| rule.extensions.iter())
                .all(|extension| sanitized
                    .iter()
                    .filter(|rule| rule.extensions.contains(extension))
                    .count()
                    == 1),
            "全局唯一"
        );
    }

    #[test]
    fn sanitize_rules_limits_lengths_and_falls_back_on_empty_input() {
        let long_extension = "a".repeat(MAX_EXTENSION_CHARS + 1);
        let many: Vec<String> = (0..(MAX_EXTENSIONS_PER_CATEGORY + 10))
            .map(|index| format!("x{index}"))
            .collect();
        let mut rules = custom_rules(&[("video", "视频", &[long_extension.as_str(), "ok"])]);
        rules[0].extensions.extend(many);
        let sanitized = sanitize_category_rules(&rules);
        assert_eq!(sanitized[0].extensions[0], "ok");
        assert!(sanitized[0].extensions.len() <= MAX_EXTENSIONS_PER_CATEGORY);
        assert!(!sanitized[0]
            .extensions
            .iter()
            .any(|ext| ext.len() > MAX_EXTENSION_CHARS));

        assert_eq!(
            sanitize_category_rules(&[]),
            default_category_rules(),
            "空分类列表回落默认规则"
        );
        // 幂等：清洗过的规则再清洗一次不变。
        assert_eq!(sanitize_category_rules(&sanitized), sanitized);
    }

    #[test]
    fn sanitize_rules_rejects_unsafe_labels() {
        let raw = custom_rules(&[("video", "../evil", &["mp4"])]);
        let sanitized = sanitize_category_rules(&raw);
        assert_eq!(sanitized[0].label, "视频", "非法目录名回落到默认 label");
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
    fn content_type_maps_to_canonical_extensions() {
        assert_eq!(extension_for_content_type("video/mp4"), Some("mp4"));
        assert_eq!(
            extension_for_content_type("video/mp4; codecs=avc1"),
            Some("mp4")
        );
        assert_eq!(extension_for_content_type("audio/mp4"), Some("m4a"));
        assert_eq!(extension_for_content_type("audio/mpeg"), Some("mp3"));
        assert_eq!(extension_for_content_type("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for_content_type("application/pdf"), Some("pdf"));
        assert_eq!(extension_for_content_type("application/octet-stream"), None);
        // Unknown vendor types fall back to conservative media containers.
        assert_eq!(extension_for_content_type("video/x-custom"), Some("mp4"));
        assert_eq!(extension_for_content_type("audio/x-custom"), Some("m4a"));
        assert_eq!(extension_for_content_type("text/markdown"), Some("txt"));
    }

    #[test]
    fn known_extension_rejects_version_like_suffixes() {
        assert!(is_known_extension("mp4"));
        assert!(is_known_extension(".MP4"));
        assert!(is_known_extension("bin"));
        assert!(is_known_extension("rmvb"));
        assert!(!is_known_extension("2"));
        assert!(!is_known_extension("2024"));
        assert!(!is_known_extension(""));
    }

    #[test]
    fn parse_heuristic_keeps_page_formats_as_other() {
        // Saved `.json`/`.html` files are 文档, but the parse heuristics must not
        // offer page/metadata formats as download candidates.
        assert_eq!(category_for("data.json", None), "文档");
        assert_eq!(resource_kind("data.json", None), "other");
        assert_eq!(resource_kind("page.html", None), "other");
        assert_eq!(resource_kind("feed.xml", None), "other");
        assert_eq!(resource_kind("movie.mp4", None), "video");
        assert_eq!(resource_kind("archive.tar.gz", None), "archive");
        assert_eq!(resource_kind("file.bin", Some("video/mp4")), "video");
    }

    #[test]
    fn bilibili_title_without_suffix_gets_mp4_from_content_type() {
        // Real-world case: the exploration model emits the page title as the
        // resource name; the verified resource is an MP4 stream.
        let name = "饿急眼了煮两包泡面，烤个手枪腿几根肉肠，哎妈太香了 (720P)";
        let normalized = ensure_file_extension(name, Some("video/mp4"), None, Some("video"));
        assert_eq!(normalized, format!("{name}.mp4"));
    }

    #[test]
    fn signed_url_extension_is_used_when_the_title_has_none() {
        let normalized = ensure_file_extension(
            "标题 (720P)",
            None,
            Some("https://cdn.example.com/upgcxcode/05/11/42924311105/42924311105-1-192.mp4?e=sig"),
            None,
        );
        assert_eq!(normalized, "标题 (720P).mp4");
    }

    #[test]
    fn existing_real_extension_is_preserved() {
        assert_eq!(
            ensure_file_extension("movie.MP4", Some("video/mp4"), None, None),
            "movie.MP4"
        );
        assert_eq!(
            ensure_file_extension("archive.tar.gz", None, None, Some("archive")),
            "archive.tar.gz"
        );
    }

    #[test]
    fn version_like_suffix_is_not_treated_as_an_extension() {
        assert_eq!(
            ensure_file_extension("视频 v1.2", Some("video/mp4"), None, None),
            "视频 v1.2.mp4"
        );
    }

    #[test]
    fn content_type_wins_over_a_misleading_url_suffix() {
        assert_eq!(
            ensure_file_extension("cover", Some("image/png"), Some("https://x/y.mp4"), None),
            "cover.png"
        );
    }

    #[test]
    fn kind_fallback_only_for_known_media_and_other() {
        assert_eq!(
            ensure_file_extension("clip", None, None, Some("audio")),
            "clip.m4a"
        );
        assert_eq!(
            ensure_file_extension("clip", None, None, Some("video")),
            "clip.mp4"
        );
        assert_eq!(
            ensure_file_extension("blob", None, None, Some("other")),
            "blob.bin"
        );
        // Unknown kind: keep the name, never invent an extension.
        assert_eq!(
            ensure_file_extension("mystery", None, None, Some("weird-kind")),
            "mystery"
        );
    }

    #[test]
    fn sanitize_file_name_strips_directories_and_windows_chars() {
        assert_eq!(
            ensure_file_extension("../dir/../evil:name?.mp4", Some("video/mp4"), None, None),
            "evil_name_.mp4"
        );
        assert_eq!(sanitize_file_name(r"  a\b  "), "b");
        assert_eq!(sanitize_file_name(""), "");
        assert_eq!(
            ensure_file_extension("", Some("video/mp4"), Some("https://x/a.mp4"), None),
            "a.mp4"
        );
        assert_eq!(ensure_file_extension("", None, None, None), "download.bin");
    }

    #[test]
    fn long_names_are_truncated_but_keep_their_extension() {
        let long = "很".repeat(240);
        let normalized = ensure_file_extension(&long, Some("video/mp4"), None, None);
        assert!(normalized.ends_with(".mp4"));
        assert!(normalized.chars().count() <= MAX_FILE_NAME_CHARS);
        assert!(!normalized.trim_end_matches(".mp4").ends_with('.'));
    }
}
