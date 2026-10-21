//! Dependency-free HTML scanning (`docs/design-next-features.md` §3.4).
//!
//! This is intentionally a *lightweight* scanner, not a spec-compliant parser:
//! it walks tags and attributes by hand because adding a third-party HTML
//! parser is out of scope. It extracts:
//!
//! * the page `<title>`,
//! * candidate download URLs from `<a href>`, `<img src>`, `<video src>`,
//!   `<source src>`, `<audio src>` and `<link href>`,
//! * `http(s)` URLs embedded in inline JSON/`data-*` attributes/scripts,
//! * a readable text summary (scripts/styles/comments/tags removed, ≤ 6 KiB).

use std::collections::HashSet;

use url::Url;

/// Candidates kept per page (contract upper bound).
pub const MAX_CANDIDATES: usize = 200;
/// Readable text cap (contract: page text truncated to 6 KiB).
pub const MAX_TEXT_BYTES: usize = 6 * 1024;
/// Attribute values longer than this are ignored as garbage.
const MAX_ATTRIBUTE_VALUE: usize = 4096;

/// Result of scanning one HTML document.
#[derive(Debug, Clone, Default)]
pub struct ScannedPage {
    /// `<title>` content, entity-decoded and whitespace-collapsed.
    pub title: Option<String>,
    /// Absolute, de-duplicated `http(s)` URLs in document order.
    pub links: Vec<String>,
    /// Readable text (no markup), at most [`MAX_TEXT_BYTES`] bytes.
    pub text: String,
}

/// Scan `html`, resolving relative references against `base`.
pub fn scan_page(html: &str, base: &Url) -> ScannedPage {
    let title = extract_title(html);
    let effective_base = find_base_href(html, base).unwrap_or_else(|| base.clone());

    let mut links = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = 0;
    while let Some((tag, next)) = parse_next_tag(html, cursor) {
        cursor = next;
        let Some(raw) = tag_link_value(&tag) else {
            continue;
        };
        if let Some(absolute) = resolve_candidate(raw, &effective_base) {
            if seen.insert(absolute.clone()) {
                links.push(absolute);
                if links.len() >= MAX_CANDIDATES {
                    break;
                }
            }
        }
    }

    if links.len() < MAX_CANDIDATES {
        let effective_base_url = effective_base.to_string();
        for raw in scan_inline_urls(html) {
            if let Some(absolute) = resolve_candidate(&raw, &effective_base) {
                // The `<base href>` itself is a resolution aid, not a resource.
                if absolute == effective_base_url {
                    continue;
                }
                if seen.insert(absolute.clone()) {
                    links.push(absolute);
                    if links.len() >= MAX_CANDIDATES {
                        break;
                    }
                }
            }
        }
    }

    ScannedPage {
        title,
        links,
        text: readable_text(html),
    }
}

// ---------------------------------------------------------------------------
// Text + title
// ---------------------------------------------------------------------------

/// Strip markup, scripts, styles and comments; decode entities and collapse
/// whitespace. The result is guaranteed to be valid UTF-8 and ≤ 6 KiB.
pub(crate) fn readable_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len().min(MAX_TEXT_BYTES * 2));
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with("<!--") {
            match lower[i + 4..].find("-->") {
                Some(offset) => {
                    i = i + 4 + offset + 3;
                    out.push(' ');
                    continue;
                }
                None => break,
            }
        }
        let skipped_tag = if lower[i..].starts_with("<script") {
            Some("</script")
        } else if lower[i..].starts_with("<style") {
            Some("</style")
        } else if lower[i..].starts_with("<template") {
            Some("</template")
        } else {
            None
        };
        if let Some(closing) = skipped_tag {
            match lower[i..].find(closing) {
                Some(offset) => {
                    i += offset + closing.len();
                    if let Some(gt) = html[i..].find('>') {
                        i += gt + 1;
                    }
                    out.push(' ');
                    continue;
                }
                None => break,
            }
        }
        if bytes[i] == b'<' {
            match html[i..].find('>') {
                Some(offset) => {
                    i += offset + 1;
                    out.push(' ');
                    continue;
                }
                None => break,
            }
        }
        let next = match html[i..].find('<') {
            Some(offset) => i + offset,
            None => html.len(),
        };
        out.push_str(&html[i..next]);
        i = next;
    }

    let decoded = decode_entities(&out);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_bytes(&collapsed, MAX_TEXT_BYTES)
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let after_name = open + "<title".len();
    // Guard against `<titlebar …>`.
    match lower.as_bytes().get(after_name) {
        Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') => {}
        _ => return None,
    }
    let start = after_name + html[after_name..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let decoded = decode_entities(html[start..end].trim());
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(crate::truncate_chars(&collapsed, 300))
    }
}

fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim_end().to_owned()
}

// ---------------------------------------------------------------------------
// Tags and attributes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
}

impl Tag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(attr_name, _)| attr_name == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Parse the next opening tag at/after `from`, returning it and the index after
/// its closing `>`.
fn parse_next_tag(html: &str, mut from: usize) -> Option<(Tag, usize)> {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    while from < html.len() {
        if bytes[from] != b'<' {
            from += lower[from..].find('<')?;
        }
        if lower[from..].starts_with("<!--") {
            from += 4 + lower[from + 4..].find("-->")? + 3;
            continue;
        }
        if lower[from..].starts_with("<!") || lower[from..].starts_with("<?") {
            from += lower[from..].find('>')? + 1;
            continue;
        }
        if lower[from..].starts_with("</") {
            from += lower[from..].find('>')? + 1;
            continue;
        }
        let mut name_end = from + 1;
        while name_end < bytes.len()
            && (bytes[name_end].is_ascii_alphanumeric()
                || bytes[name_end] == b'-'
                || bytes[name_end] == b':')
        {
            name_end += 1;
        }
        if name_end == from + 1 {
            from += 1;
            continue;
        }
        let name = lower[from + 1..name_end].to_owned();
        // Find the tag-closing `>`, skipping quoted attribute values.
        let mut cursor = name_end;
        let mut quote: Option<u8> = None;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            match quote {
                Some(active) if byte == active => quote = None,
                Some(_) => {}
                None => {
                    if byte == b'"' || byte == b'\'' {
                        quote = Some(byte);
                    } else if byte == b'>' {
                        break;
                    }
                }
            }
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }
        let attrs = parse_attrs(&html[name_end..cursor]);
        return Some((
            Tag { name, attrs },
            cursor + 1, // past `>`
        ));
    }
    None
}

fn parse_attrs(source: &str) -> Vec<(String, String)> {
    let bytes = source.as_bytes();
    let mut attrs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let name = source[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = source[value_start..i].to_owned();
                if i < bytes.len() {
                    i += 1; // past the closing quote
                }
            } else {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                value = source[value_start..i].to_owned();
            }
        }
        if !name.is_empty() && !value.is_empty() && value.len() <= MAX_ATTRIBUTE_VALUE {
            attrs.push((name, decode_entities(&value)));
        }
    }
    attrs
}

fn tag_link_value(tag: &Tag) -> Option<&str> {
    let attribute = match tag.name.as_str() {
        "a" | "link" => "href",
        "img" | "video" | "source" | "audio" => "src",
        _ => return None,
    };
    tag.attr(attribute)
}

fn find_base_href(html: &str, base: &Url) -> Option<Url> {
    let mut cursor = 0;
    while let Some((tag, next)) = parse_next_tag(html, cursor) {
        cursor = next;
        if tag.name == "base" {
            if let Some(href) = tag.attr("href") {
                let href = href.trim();
                if !href.is_empty() {
                    if let Ok(resolved) = base.join(href) {
                        if matches!(resolved.scheme(), "http" | "https") {
                            return Some(resolved);
                        }
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// URL resolution
// ---------------------------------------------------------------------------

fn resolve_candidate(raw: &str, base: &Url) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_ATTRIBUTE_VALUE * 4 {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with('#')
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with("blob:")
        || lower.starts_with("about:")
        || lower.starts_with("file:")
    {
        return None;
    }
    let target = if raw.starts_with("//") {
        format!("{}:{raw}", base.scheme())
    } else {
        raw.to_owned()
    };
    let joined = base.join(&target).ok()?;
    if !matches!(joined.scheme(), "http" | "https") {
        return None;
    }
    let mut joined = joined;
    joined.set_fragment(None);
    Some(joined.to_string())
}

/// Find bare `http(s)://…` URLs in inline JSON, `data-*` attributes and
/// scripts. JSON-escaped slashes (`https:\/\/…`) are handled.
fn scan_inline_urls(html: &str) -> Vec<String> {
    let unescaped = html.replace("\\/", "/");
    let lower = unescaped.to_ascii_lowercase();
    let hay = unescaped.as_bytes();
    let lower_bytes = lower.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < hay.len() {
        let prefix_len = if starts_with_at(lower_bytes, i, b"https://") {
            8
        } else if starts_with_at(lower_bytes, i, b"http://") {
            7
        } else {
            i += 1;
            continue;
        };
        let mut end = i + prefix_len;
        while end < hay.len() && !is_url_terminator(hay[end]) {
            end += 1;
        }
        let candidate = unescaped[i..end].trim_end_matches(['.', ',', ';', '!', ':', ')']);
        if candidate.len() >= 10 {
            found.push(candidate.to_owned());
        }
        i = end.max(i + 1);
    }
    found
}

fn starts_with_at(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    at + needle.len() <= haystack.len() && &haystack[at..at + needle.len()] == needle
}

fn is_url_terminator(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t'
            | b'\r'
            | b'\n'
            | b'"'
            | b'\''
            | b'<'
            | b'>'
            | b'`'
            | b'\\'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b'('
            | b')'
            | b','
            | b';'
    )
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(position) = rest.find('&') {
        out.push_str(&rest[..position]);
        let tail = &rest[position + 1..];
        let terminator = tail.find(';').filter(|index| *index <= 12);
        if let Some(terminator) = terminator {
            if let Some(decoded) = decode_entity(&tail[..terminator]) {
                out.push_str(&decoded);
                rest = &tail[terminator + 1..];
                continue;
            }
        }
        out.push('&');
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    let character = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" | "#039" | "#x27" | "#X27" => '\'',
        "nbsp" => ' ',
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                char::from_u32(u32::from_str_radix(hex, 16).ok()?)?
            } else {
                let decimal = entity.strip_prefix('#')?;
                char::from_u32(decimal.parse().ok()?)?
            }
        }
    };
    Some(character.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/pages/index.html").unwrap()
    }

    #[test]
    fn extracts_tag_links_and_resolves_relative_urls() {
        let html = r##"
            <html><head>
              <link rel="stylesheet" href="/style.css">
            </head><body>
              <a href="files/movie.mp4">movie</a>
              <a href='https://cdn.example.com/b.zip'>zip</a>
              <img src="//cdn.example.com/pic.png">
              <video src="/media/clip.webm"></video>
              <source src="../other/audio.mp3">
              <audio src="/media/song.flac"></audio>
              <a href="javascript:void(0)">no</a>
              <a href="data:text/plain,hi">no</a>
              <a href="mailto:a@b.c">no</a>
              <a href="#fragment">no</a>
              <a href="files/movie.mp4#t=10">dupe with fragment</a>
            </body></html>"##;
        let page = scan_page(html, &base());
        assert_eq!(
            page.links,
            vec![
                "https://example.com/style.css".to_owned(),
                "https://example.com/pages/files/movie.mp4".to_owned(),
                "https://cdn.example.com/b.zip".to_owned(),
                "https://cdn.example.com/pic.png".to_owned(),
                "https://example.com/media/clip.webm".to_owned(),
                "https://example.com/other/audio.mp3".to_owned(),
                "https://example.com/media/song.flac".to_owned(),
            ]
        );
    }

    #[test]
    fn extracts_urls_from_inline_json_and_scripts() {
        let html = r#"<html><body>
            <script>
              var player = {"src": "https:\/\/cdn.example.com\/hls\/master.m3u8", "poster": "/poster.jpg"};
            </script>
            <div data-json='{"file":"https://example.com/api/file/42?token=abc"}'>x</div>
        </body></html>"#;
        let page = scan_page(html, &base());
        assert!(
            page.links
                .contains(&"https://cdn.example.com/hls/master.m3u8".to_owned()),
            "{:?}",
            page.links
        );
        assert!(page
            .links
            .contains(&"https://example.com/api/file/42?token=abc".to_owned()));
    }

    #[test]
    fn honors_base_href() {
        let html = r#"<head><base href="https://cdn.example.com/assets/"></head>
                      <body><a href="file.mp4">x</a></body>"#;
        let page = scan_page(html, &base());
        assert_eq!(
            page.links,
            vec!["https://cdn.example.com/assets/file.mp4".to_owned()]
        );
    }

    #[test]
    fn title_is_decoded_and_cleaned() {
        let html = "<html><head><title>  Best &amp; Greatest\n Files </title></head></html>";
        assert_eq!(
            scan_page(html, &base()).title.as_deref(),
            Some("Best & Greatest Files")
        );
        assert_eq!(scan_page("<html></html>", &base()).title, None);
    }

    #[test]
    fn readable_text_strips_markup_and_scripts() {
        let html = r#"<html><head><style>body{color:red}</style>
            <script>var secret = "ignore me";</script></head>
            <body><!-- hidden --><h1>Hello</h1><p>World &amp; friends</p></body></html>"#;
        let text = readable_text(html);
        assert_eq!(text, "Hello World & friends");
        assert!(!text.contains("ignore me"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("hidden"));
    }

    #[test]
    fn readable_text_is_capped_at_six_kib() {
        let html = format!("<p>{}</p>", "abcdefghij".repeat(2000));
        let text = readable_text(&html);
        assert!(text.len() <= MAX_TEXT_BYTES, "{}", text.len());
        assert!(!text.is_empty());
    }

    #[test]
    fn candidate_count_is_capped() {
        let anchors = (0..(MAX_CANDIDATES + 50))
            .map(|index| format!(r#"<a href="/file-{index}.mp4">x</a>"#))
            .collect::<String>();
        let page = scan_page(&anchors, &base());
        assert_eq!(page.links.len(), MAX_CANDIDATES);
    }

    #[test]
    fn decode_entities_variants() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&unknown; &"), "&unknown; &");
    }
}
