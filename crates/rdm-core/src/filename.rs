//! Filename derivation and Windows-safe target resolution (architecture §3.2/§3.4).
//!
//! Priority: `Content-Disposition` `filename*` (RFC 5987) → `filename` → the
//! final URL's last path segment → `download.bin`. Names are sanitised for
//! Windows and de-duplicated with a ` (n)` suffix. A resumable `.part.json`
//! for the base name wins over de-duplication so resume always finds its state.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

use crate::state;

/// Fallback used whenever the server gives us nothing usable.
pub(crate) const FALLBACK_NAME: &str = "download.bin";
/// Upper bound to stay comfortably below Windows path limits.
const MAX_NAME_CHARS: usize = 180;

/// Pick the server-derived filename for a probe response.
pub(crate) fn derive(disposition: Option<&str>, final_url: &str) -> String {
    let from_header = disposition.and_then(parse_content_disposition);
    let candidate = from_header.or_else(|| from_url(final_url));
    match candidate {
        Some(raw) => {
            let clean = sanitize(&raw);
            if clean.is_empty() {
                FALLBACK_NAME.to_string()
            } else {
                clean
            }
        }
        None => FALLBACK_NAME.to_string(),
    }
}

/// Parse `Content-Disposition` and return the best filename, if any.
pub(crate) fn parse_content_disposition(value: &str) -> Option<String> {
    let mut star: Option<String> = None;
    let mut plain: Option<String> = None;

    for param in split_params(value) {
        let Some((name, raw_value)) = param.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let raw_value = raw_value.trim();
        match name.as_str() {
            "filename*" => {
                if let Some(decoded) = decode_ext_value(raw_value) {
                    star = Some(decoded);
                }
            }
            "filename" => {
                let unquoted = unquote(raw_value);
                if !unquoted.is_empty() {
                    plain = Some(unquoted);
                }
            }
            _ => {}
        }
    }

    star.or(plain)
}

/// Replace characters Windows forbids, drop control chars, strip trailing
/// dots/spaces, escape reserved device names and bound the length.
pub(crate) fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().chars() {
        if ch.is_control() {
            out.push('_');
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
    let out = out.trim_start().to_string();
    if out.is_empty() || out == "." || out == ".." {
        return String::new();
    }

    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let out = if reserved { format!("_{out}") } else { out };

    truncate_chars(out, MAX_NAME_CHARS)
}

/// Resolve the final path: sanitise, then find a free (or resumable) name.
pub(crate) fn resolve_target(dir: &Path, desired: &str, resume: bool) -> (PathBuf, String) {
    let desired = {
        let clean = sanitize(desired);
        if clean.is_empty() {
            FALLBACK_NAME.to_string()
        } else {
            clean
        }
    };

    for attempt in 0u32.. {
        let name = if attempt == 0 {
            desired.clone()
        } else {
            numbered(&desired, attempt)
        };
        let path = dir.join(&name);
        if resume && state::state_path(&path).exists() {
            return (path, name);
        }
        if !path.exists() {
            return (path, name);
        }
        if attempt >= 9999 {
            // Absurdly unlikely; fall back to a numeric suffix to stay unique.
            let fallback = format!("{}-{}", desired, std::process::id());
            return (dir.join(&fallback), fallback);
        }
    }
    unreachable!("loop always returns")
}

fn numbered(name: &str, n: u32) -> String {
    let path = Path::new(name);
    match (path.file_stem(), path.extension()) {
        (Some(stem), Some(ext)) => format!(
            "{} ({n}).{}",
            stem.to_string_lossy(),
            ext.to_string_lossy()
        ),
        _ => format!("{name} ({n})"),
    }
}

/// Split a header value on top-level semicolons, preserving quoted strings.
fn split_params(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => {
                current.push(ch);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            ';' if !quoted => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    out.push(current.trim().to_string());
    out
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let inner = &value[1..value.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        value.to_string()
    }
}

/// Decode an RFC 5987 extended value (`UTF-8''percent-encoded`).
fn decode_ext_value(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"');
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?.trim();
    let _language = parts.next()?;
    let encoded = parts.next()?;
    let bytes: Vec<u8> = percent_decode_str(encoded).collect();
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" => Some(String::from_utf8_lossy(&bytes).into_owned()),
        "iso-8859-1" | "latin1" => Some(bytes.iter().map(|b| *b as char).collect()),
        _ => None,
    }
}

fn from_url(final_url: &str) -> Option<String> {
    let parsed = url::Url::parse(final_url).ok()?;
    let segments: Vec<&str> = parsed.path_segments()?.collect();
    let last = segments.iter().rev().find(|segment| !segment.is_empty())?;
    let decoded = percent_decode_str(last).decode_utf8_lossy().into_owned();
    if decoded.trim().is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn truncate_chars(mut value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    let cutoff = value
        .char_indices()
        .nth(max)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    value.truncate(cutoff);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_plain_filename() {
        let value = r#"attachment; filename="report final.pdf""#;
        assert_eq!(parse_content_disposition(value).as_deref(), Some("report final.pdf"));
    }

    #[test]
    fn prefers_rfc5987_filename_star() {
        let value = "attachment; filename=\"fallback.bin\"; \
                     filename*=UTF-8''%E4%B8%AD%E6%96%87%20file.bin";
        assert_eq!(
            parse_content_disposition(value).as_deref(),
            Some("中文 file.bin")
        );
    }

    #[test]
    fn decodes_latin1_ext_value() {
        let value = "attachment; filename*=ISO-8859-1'en'%A3%20rates.txt";
        assert_eq!(parse_content_disposition(value).as_deref(), Some("£ rates.txt"));
    }

    #[test]
    fn falls_back_to_url_segment() {
        let name = derive(None, "https://example.com/files/My%20File.tar.gz?token=x");
        assert_eq!(name, "My File.tar.gz");
        let name = derive(None, "https://example.com/");
        assert_eq!(name, FALLBACK_NAME);
    }

    #[test]
    fn sanitizes_windows_illegal_characters() {
        assert_eq!(sanitize(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("name.  "), "name");
        assert_eq!(sanitize("   "), "");
        assert_eq!(sanitize(".."), "");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("lpt9.txt"), "_lpt9.txt");
        assert_eq!(sanitize("console.txt"), "console.txt");
        assert_eq!(sanitize("a\tb"), "a_b");
    }

    #[test]
    fn truncates_long_names_on_char_boundary() {
        let name = sanitize(&"汉".repeat(500));
        assert!(name.chars().count() <= MAX_NAME_CHARS);
        assert!(name.is_char_boundary(name.len()));
    }

    #[test]
    fn deduplicates_existing_targets() {
        let dir = std::env::temp_dir().join(format!("rdm-filename-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let (path, name) = resolve_target(&dir, "file.bin", true);
        assert_eq!(name, "file.bin");
        fs::write(&path, b"x").unwrap();

        let (path2, name2) = resolve_target(&dir, "file.bin", true);
        assert_eq!(name2, "file (1).bin");
        assert!(!path2.exists());

        let (_, name3) = resolve_target(&dir, "file.bin", true);
        assert_eq!(name3, "file (1).bin");
        fs::write(&path2, b"x").unwrap();
        let (_, name4) = resolve_target(&dir, "file.bin", true);
        assert_eq!(name4, "file (2).bin");

        // A resume state for the base name wins over de-duplication.
        fs::write(state::state_path(&dir.join("file.bin")), b"{}").unwrap();
        let (resume_path, resume_name) = resolve_target(&dir, "file.bin", true);
        assert_eq!(resume_name, "file.bin");
        assert_eq!(resume_path, dir.join("file.bin"));
        // ...but is ignored when resume is off.
        let (_, no_resume) = resolve_target(&dir, "file.bin", false);
        assert_eq!(no_resume, "file (2).bin");

        let _ = fs::remove_dir_all(&dir);
    }
}
