//! Reusable-template abstraction for resolution experiences (design 18 §2.2).
//!
//! A v1 recipe is a *specific* trace: its matcher pins the exact URL and often
//! its steps hardcode sample values. This module turns such a recipe into a
//! parameterized template:
//!
//! * the URL path is classified segment by segment (`BV…` → `bvid`, digits →
//!   `id`, UUIDs, mixed alphanumerics → `p{n}`, everything else static);
//! * the matcher becomes `^https?://<host>/<static prefix>/(?P<name>…)…` with
//!   the query/fragment ignored;
//! * every captured sample value in `steps[].args`, guards and `emit` is
//!   replaced with `{{name}}`;
//! * internal ids that cannot be derived from the input (e.g. `cid`) are only
//!   *reported* (`RecipeGeneralization::leftover_hardcoded`), never guessed.
//!
//! Everything here is deterministic, model-free and network-free, so the same
//! inputs always produce byte-identical recipes (idempotent migrations).

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use url::Url;

use crate::contract::now_ms;
use crate::recipe::{Recipe, RecipeGeneralization, RecipeStep};
use crate::registry::RecipeRegistry;

/// Abstracted shape of one sample URL (design 18 §2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlShape {
    /// Ready to be used as `Recipe.match.url_pattern`.
    pub pattern: String,
    /// Capture name -> sample value, in path-segment order.
    pub captures: Vec<(String, String)>,
    /// `host + static path prefix` (UI display / same-class grouping key).
    pub scope: String,
    /// `true` when at least one variable segment was abstracted.
    pub reusable: bool,
}

/// Result of [`generalize_recipe`].
#[derive(Debug, Clone)]
pub struct GeneralizeReport {
    pub recipe: Recipe,
    /// `true` when the recipe differs from the input (matcher/template/
    /// generalization changed).
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// Result of [`migrate_experience_library`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Experiences considered (every visible experience without a migration
    /// marker is scanned; already generalized ones are counted as `skipped`).
    pub scanned: usize,
    /// Experiences that received a new active template version.
    pub generalized: usize,
    /// Experiences that already had a template (or could not be migrated).
    pub skipped: usize,
    /// Non-fatal per-experience failures (migration never aborts on one item).
    pub errors: Vec<String>,
}

/// Abstract one concrete URL into a reusable matcher shape (design 18 §2.2).
///
/// Only `http`/`https` URLs are accepted. Query and fragment are ignored: a
/// new URL with different tracking parameters must still match.
pub fn abstract_url(raw: &str) -> Result<UrlShape, String> {
    let url = Url::parse(raw.trim()).map_err(|error| format!("invalid URL: {error}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported URL scheme {other:?} (expected http/https)"
            ))
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_owned())?
        .to_ascii_lowercase();
    let authority = match url.port() {
        Some(port) => format!("{}:{port}", regex::escape(&host)),
        None => regex::escape(&host),
    };

    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();

    let mut captures: Vec<(String, String)> = Vec::new();
    let mut used_names: BTreeSet<String> = BTreeSet::new();
    let mut pattern = format!("^https?://{authority}");
    let mut mixed_index = 0usize;

    for segment in &segments {
        let Some(kind) = classify_variable(segment) else {
            pattern.push('/');
            pattern.push_str(&regex::escape(segment));
            continue;
        };
        let base = match kind {
            VariableKind::Bvid => "bvid".to_owned(),
            VariableKind::Id => "id".to_owned(),
            VariableKind::Mixed => {
                mixed_index += 1;
                format!("p{mixed_index}")
            }
        };
        let name = unique_capture_name(&base, &mut used_names);
        let fragment = match kind {
            VariableKind::Bvid => "BV[0-9A-Za-z]+".to_owned(),
            VariableKind::Id => numeric_or_uuid_fragment(segment),
            VariableKind::Mixed => "[0-9A-Za-z]+".to_owned(),
        };
        pattern.push('/');
        pattern.push_str(&format!("(?P<{name}>{fragment})"));
        captures.push((name, segment.to_string()));
    }
    pattern.push_str("/?(?:\\?.*)?(?:#.*)?$");

    // Scope = host + the static path prefix before the first variable segment
    // ("www.bilibili.com/video"). Without variables the whole path is static.
    let first_variable = segments
        .iter()
        .position(|segment| classify_variable(segment).is_some());
    let scope_segments = match first_variable {
        Some(index) => &segments[..index],
        None => &segments[..],
    };
    let mut scope = host;
    if !scope_segments.is_empty() {
        scope.push('/');
        scope.push_str(
            &scope_segments
                .iter()
                .map(|segment| (*segment).to_owned())
                .collect::<Vec<String>>()
                .join("/"),
        );
    }

    let reusable = !captures.is_empty();
    Ok(UrlShape {
        pattern,
        captures,
        scope,
        reusable,
    })
}

/// Classification of one URL path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariableKind {
    /// `BV…` video id.
    Bvid,
    /// Plain number or UUID.
    Id,
    /// Long mixed alphanumeric segment (`p{n}`).
    Mixed,
}

/// Variable kind for a path segment, or `None` for a static segment.
fn classify_variable(segment: &str) -> Option<VariableKind> {
    if is_bvid(segment) {
        return Some(VariableKind::Bvid);
    }
    if !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit()) {
        return Some(VariableKind::Id);
    }
    if is_uuid(segment) {
        return Some(VariableKind::Id);
    }
    let long_enough = segment.chars().count() >= 6;
    let has_letter = segment
        .chars()
        .any(|character| character.is_ascii_alphabetic());
    let has_digit = segment.chars().any(|character| character.is_ascii_digit());
    if long_enough
        && has_letter
        && has_digit
        && segment.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Some(VariableKind::Mixed);
    }
    None
}

/// `BV` + at least 8 alphanumerics (bilibili video id shape).
fn is_bvid(segment: &str) -> bool {
    let Some(rest) = segment.strip_prefix("BV") else {
        return false;
    };
    rest.chars().count() >= 8
        && rest
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn is_uuid(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

/// Regex body for an `id` capture: keep the matched sample's alphabet
/// (`\d+` for plain numbers, the UUID skeleton for UUIDs).
fn numeric_or_uuid_fragment(segment: &str) -> String {
    if is_uuid(segment) {
        "[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}".to_owned()
    } else {
        "\\d+".to_owned()
    }
}

/// `p1`, `p2`, … for mixed alphanumeric segments; numeric ids all become `id`
/// and need a suffix when the pattern already used that name.
fn unique_capture_name(base: &str, used: &mut BTreeSet<String>) -> String {
    if used.insert(base.to_owned()) {
        return base.to_owned();
    }
    let mut suffix = 2usize;
    loop {
        let candidate = format!("{base}{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Turn one concrete recipe into a reusable template (design 18 §2.2).
///
/// The input is never mutated; the returned recipe has an overwritten matcher
/// (`hosts` + generalized `url_pattern`), `captures`, templated steps/guards/
/// emit and a [`RecipeGeneralization`]. On an invalid sample URL the recipe is
/// returned unchanged with a warning, so migrations can skip it safely.
pub fn generalize_recipe(recipe: &Recipe, sample_url: &str) -> GeneralizeReport {
    let shape = match abstract_url(sample_url) {
        Ok(shape) => shape,
        Err(error) => {
            return GeneralizeReport {
                recipe: recipe.clone(),
                changed: false,
                warnings: vec![format!("无法抽象样本 URL：{error}")],
            };
        }
    };

    let mut generalized = recipe.clone();
    // Longest sample value first: a value that is a prefix of another one must
    // not be replaced inside it (e.g. two ids of different lengths).
    let mut replacements: Vec<(String, String)> = shape
        .captures
        .iter()
        .map(|(name, value)| (value.clone(), format!("{{{{{name}}}}}")))
        .collect();
    replacements.sort_by(|left, right| {
        right
            .0
            .len()
            .cmp(&left.0.len())
            .then_with(|| left.0.cmp(&right.0))
    });

    let capture_values: Vec<String> = shape
        .captures
        .iter()
        .map(|(_, value)| value.clone())
        .collect();
    for step in &mut generalized.steps {
        template_value(&mut step.args, &replacements);
        if let Some(when) = step.when.as_mut() {
            if let Some(expected) = when.expected.as_mut() {
                *expected = replace_capture_values(expected, &replacements);
            }
        }
    }
    generalized.emit.url = replace_capture_values(&generalized.emit.url, &replacements);
    // Emit names are derived per sample (BV id, cid, sticker hash, …). Keeping
    // such a name after generalization would suggest the old file after a
    // migration, so only pure static names survive; everything else is dropped
    // and the runtime derives the name from the verified final URL.
    generalized.emit.name = match recipe.emit.name.as_deref() {
        Some(name) if emit_name_is_static(name, &capture_values) => {
            Some(replace_capture_values(name, &replacements))
        }
        Some(_) => None,
        None => None,
    };

    generalized.matcher.hosts = vec![Url::parse(sample_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()]
    .into_iter()
    .filter(|host| !host.is_empty())
    .collect();
    generalized.matcher.url_pattern = Some(shape.pattern.clone());
    generalized.captures = shape
        .captures
        .iter()
        .map(|(name, _)| name.clone())
        .collect();

    let leftover_hardcoded = detect_leftover_hardcoded(&generalized.steps, &capture_values);

    let mut warnings = Vec::new();
    if !shape.reusable {
        warnings.push("未从样本 URL 抽象出变量，经验仅匹配精确地址".to_owned());
    }
    for name in &leftover_hardcoded {
        warnings.push(format!(
            "步骤仍写死内部参数：{name}（建议使用经验助手整理）"
        ));
    }

    generalized.generalization = Some(RecipeGeneralization {
        scope: shape.scope,
        captures: generalized.captures.clone(),
        leftover_hardcoded,
        reusable: shape.reusable,
    });

    let changed = serde_json::to_value(recipe).ok() != serde_json::to_value(&generalized).ok();
    GeneralizeReport {
        recipe: generalized,
        changed,
        warnings,
    }
}

/// Recursively replace sample values in a JSON value (arrays and objects).
fn template_value(value: &mut Value, replacements: &[(String, String)]) {
    match value {
        Value::String(text) => *text = replace_capture_values(text, replacements),
        Value::Array(items) => {
            for item in items {
                template_value(item, replacements);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                template_value(item, replacements);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn replace_capture_values(text: &str, replacements: &[(String, String)]) -> String {
    let mut output = text.to_owned();
    for (value, placeholder) in replacements {
        if !value.is_empty() && output.contains(value.as_str()) {
            output = output.replace(value.as_str(), placeholder);
        }
    }
    output
}

/// Whether an `emit.name` is sample-independent and may be kept after
/// generalization: it must not embed a captured sample value nor a long
/// internal id (≥5 consecutive digits, e.g. a cid or a sticker hash).
fn emit_name_is_static(name: &str, capture_values: &[String]) -> bool {
    if capture_values
        .iter()
        .any(|value| !value.is_empty() && name.contains(value.as_str()))
    {
        return false;
    }
    !digit_runs(name).iter().any(|(start, end)| end - start >= 5)
}

/// Internal ids that survive templating (design 18 §2.2 algorithm step 6).
///
/// Only short identifiers such as `cid` or a `step.args.url` field label are
/// returned: URLs, query values and credentials are never persisted.
fn detect_leftover_hardcoded(steps: &[RecipeStep], capture_values: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for step in steps {
        collect_leftovers_from_value(
            &step.args,
            &format!("{}.args", step.id),
            capture_values,
            &mut found,
        );
        if let Some(expected) = step.when.as_ref().and_then(|when| when.expected.as_deref()) {
            collect_leftovers_from_text(
                expected,
                &format!("{}.when.expected", step.id),
                capture_values,
                &mut found,
            );
        }
    }
    found
}

fn collect_leftovers_from_value(
    value: &Value,
    path: &str,
    capture_values: &[String],
    found: &mut Vec<String>,
) {
    match value {
        Value::String(text) => collect_leftovers_from_text(text, path, capture_values, found),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_leftovers_from_value(
                    item,
                    &format!("{path}[{index}]"),
                    capture_values,
                    found,
                );
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_leftovers_from_value(item, &format!("{path}.{key}"), capture_values, found);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_leftovers_from_text(
    text: &str,
    field_path: &str,
    capture_values: &[String],
    found: &mut Vec<String>,
) {
    for (start, end) in digit_runs(text) {
        if end - start < 5 {
            continue;
        }
        let digits = &text[start..end];
        // A run that is (part of) a capture value is not an internal parameter:
        // it may have survived in a longer literal that templating could not
        // rewrite as a whole.
        if capture_values.iter().any(|value| value.contains(digits)) {
            continue;
        }
        let label = parameter_name_before(text, start)
            .unwrap_or_else(|| format!("{} 含内部参数", field_path.trim_end_matches('.')));
        if !found.contains(&label) {
            found.push(label);
        }
    }
}

/// Byte ranges of ASCII digit runs.
fn digit_runs(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            runs.push((start, index));
        } else {
            index += 1;
        }
    }
    runs
}

/// Identifier immediately preceding `digits` (`cid=429…` -> `cid`).
fn parameter_name_before(text: &str, start: usize) -> Option<String> {
    let before = text.get(..start)?;
    let trimmed = before.trim_end_matches(['=', ':', '/', '&', '?']);
    if trimmed.len() == before.len() {
        return None;
    }
    let name: String = trimmed
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
    {
        return None;
    }
    Some(name)
}

/// One-shot, idempotent migration of the on-disk experience library
/// (design 18 §2.2).
///
/// Every visible experience whose active recipe has no
/// [`RecipeGeneralization`] receives a new active version:
///
/// * a URL that can be abstracted becomes a reusable template;
/// * a URL without variable segments keeps its exact path but ignores the query
///   (`reusable = false`).
///
/// Old versions stay on disk for rollback. The migration never accesses the
/// network and never replays a recipe; one failing experience is recorded in
/// [`MigrationReport::errors`] without aborting the rest.
pub fn migrate_experience_library(resolve_root: &Path) -> MigrationReport {
    let recipes_root = resolve_root.join("recipes");
    let mut registry = RecipeRegistry::with_root(&recipes_root);
    let mut report = MigrationReport::default();
    for id in registry.visible_ids() {
        let (active_version, source_url) = match registry.entry(&id) {
            Some(entry) => (
                if entry.active_version != 0 {
                    entry.active_version
                } else {
                    registry.latest_version(&id).unwrap_or(0)
                },
                entry.source_url.clone(),
            ),
            None => (registry.latest_version(&id).unwrap_or(0), None),
        };
        let Some(recipe) = registry
            .get(&id, Some(active_version))
            .or_else(|| registry.get(&id, None))
            .cloned()
        else {
            report.skipped += 1;
            continue;
        };
        if recipe.generalization.is_some() {
            report.skipped += 1;
            continue;
        }
        report.scanned += 1;
        let sample = source_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                recipe
                    .tests
                    .first()
                    .map(|test| test.input_url.trim().to_owned())
                    .filter(|url| !url.is_empty())
            });
        let Some(sample) = sample else {
            report.skipped += 1;
            report
                .errors
                .push(format!("{id}: 没有可用的样本 URL，跳过迁移"));
            continue;
        };
        let generated = generalize_recipe(&recipe, &sample);
        if !generated.changed {
            report.skipped += 1;
            report
                .errors
                .push(format!("{id}: 样本 URL 无法抽象（{sample}）"));
            continue;
        }
        let Some(next) = registry
            .latest_version(&id)
            .and_then(|latest| latest.checked_add(1))
        else {
            report.skipped += 1;
            report.errors.push(format!("{id}: 版本号已用尽，跳过迁移"));
            continue;
        };
        let mut recipe = generated.recipe;
        recipe.version = next;
        let path = match registry.save(&recipe) {
            Ok(path) => path,
            Err(error) => {
                report
                    .errors
                    .push(format!("{id}: 无法保存迁移后的经验文件：{error}"));
                continue;
            }
        };
        if let Err(error) = registry.insert(recipe.clone()) {
            let _ = std::fs::remove_file(&path);
            report
                .errors
                .push(format!("{id}: 无法更新解析经验索引：{error}"));
            continue;
        }
        if let Err(error) = registry.note_publish(&id, next, 0, Some(&sample), now_ms(), false) {
            report
                .errors
                .push(format!("{id}: 无法写入迁移元数据：{error}"));
        }
        report.generalized += 1;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bvid_is_abstracted_and_query_is_ignored() {
        let shape =
            abstract_url("https://www.bilibili.com/video/BV1e3ny6KE3m/?trackid=1&spm=2").unwrap();
        assert_eq!(shape.scope, "www.bilibili.com/video");
        assert_eq!(
            shape.captures,
            vec![("bvid".to_owned(), "BV1e3ny6KE3m".to_owned())]
        );
        assert!(shape.reusable);
        assert!(shape
            .pattern
            .starts_with("^https?://www\\.bilibili\\.com/video/"));
        assert!(shape.pattern.contains("(?P<bvid>BV[0-9A-Za-z]+)"));
    }

    #[test]
    fn uuid_and_numeric_segments_become_ids() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let shape = abstract_url(&format!("https://example.com/item/{uuid}/42")).unwrap();
        let names: Vec<&str> = shape
            .captures
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, vec!["id", "id2"]);
        assert!(shape.pattern.contains("\\d+"));
    }

    #[test]
    fn invalid_scheme_is_rejected() {
        assert!(abstract_url("ftp://example.com/x").is_err());
        assert!(abstract_url("not a url").is_err());
        assert!(abstract_url("https://example.com/").is_ok());
    }

    #[test]
    fn leftover_detection_keeps_only_short_parameter_names() {
        let recipe: Recipe = serde_json::from_value(serde_json::json!({
            "schema": "rdm.recipe/v1",
            "id": "x",
            "version": 1,
            "match": { "hosts": ["example.com"] },
            "steps": [{
                "id": "s0",
                "op": "http_request",
                "args": { "url": "https://api.example.com/play?cid=42944954746&qn=80" }
            }],
            "emit": { "url": "https://cdn.example.com/a.bin" }
        }))
        .unwrap();
        let report = generalize_recipe(&recipe, "https://example.com/watch/abcDEF123");
        let leftover = &report
            .recipe
            .generalization
            .as_ref()
            .unwrap()
            .leftover_hardcoded;
        assert_eq!(leftover, &vec!["cid".to_owned()]);
        assert!(leftover
            .iter()
            .all(|entry| !entry.contains("http") && !entry.contains("42944954746")));
    }
}
