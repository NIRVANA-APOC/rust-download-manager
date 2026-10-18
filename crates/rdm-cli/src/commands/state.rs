//! `rdm state inspect` / `rdm state repair` — maintenance for leftover
//! `.part` / `.part.json` resume state.
//!
//! `inspect` is read-only and reports orphan parts/states, corrupt JSON and
//! stale pairs. `repair` is **dry-run by default**; with `--no-dry-run` the
//! problematic files are moved into `<dir>/.rdm-quarantine/<timestamp>/`
//! (never deleted in place, so a human can still inspect/recover them).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::{StateArgs, StateCommand, StateRepairArgs, StateScanArgs};
use crate::output::{invalid_request, OutputMode, Reporter};
use crate::util;

const MAX_FILES: usize = 20_000;
const MAX_DEPTH: usize = 8;
const SKIP_DIRS: &[&str] = &[
    "target",
    "target-core",
    "target-cli-agent",
    "target-qa",
    "target-gui",
    "target-native",
    "target-bench",
    "node_modules",
    ".git",
    ".rdm-quarantine",
    "System Volume Information",
    "$Recycle.Bin",
];

#[derive(Debug, Clone, Serialize)]
struct Issue {
    id: String,
    kind: &'static str,
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    paired: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

struct ScanReport {
    roots: Vec<PathBuf>,
    missing_roots: Vec<PathBuf>,
    files: usize,
    parts: usize,
    states: usize,
    issues: Vec<Issue>,
    truncated: bool,
    warnings: Vec<String>,
}

pub fn run(mode: OutputMode, args: StateArgs) -> u8 {
    match args.action {
        StateCommand::Inspect(scan_args) => inspect(mode, scan_args),
        StateCommand::Repair(repair_args) => repair(mode, repair_args),
    }
}

fn inspect(mode: OutputMode, args: StateScanArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "state");
    let report = scan(&args.dirs);
    let summary = summarize(&report.issues);

    if reporter.is_text() {
        for root in &report.roots {
            crate::outln!("scan {}  ({} file(s))", root.display(), report.files);
        }
        if !report.missing_roots.is_empty() {
            crate::outln!(
                "note: {} root(s) do not exist: {}",
                report.missing_roots.len(),
                report
                    .missing_roots
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for issue in &report.issues {
            crate::outln!("{:<14} {}", issue.kind, issue.path.display());
            if let Some(reason) = &issue.reason {
                crate::outln!("               reason: {reason}");
            }
            if let Some(hint) = &issue.hint {
                crate::outln!("               hint: {hint}");
            }
        }
        crate::outln!(
            "issues: {} (orphan_part={}, orphan_state={}, corrupt_state={}, stale_part={}), dry-run only",
            report.issues.len(),
            summary.orphan_part,
            summary.orphan_state,
            summary.corrupt_state,
            summary.stale_part,
        );
    }

    let data = json!({
        "roots": report.roots,
        "missing_roots": report.missing_roots,
        "scanned": {
            "files": report.files,
            "parts": report.parts,
            "states": report.states,
        },
        "issues": report.issues,
        "summary": summary.to_json(),
        "truncated": report.truncated,
        "warnings": report.warnings,
        "dry_run": true,
    });
    reporter.finish_ok(data)
}

fn repair(mode: OutputMode, args: StateRepairArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "state");
    if args.dry_run && args.no_dry_run {
        return reporter.finish_err(invalid_request(
            "--dry-run and --no-dry-run are mutually exclusive",
        ));
    }
    let dry_run = !args.no_dry_run;
    let report = scan(&args.dirs);

    // Plan quarantine targets.
    let mut planned: BTreeMap<PathBuf, (&'static str, PathBuf)> = BTreeMap::new();
    for issue in &report.issues {
        let mut paths = vec![(issue.path.clone(), issue.kind)];
        if let Some(paired) = &issue.paired {
            let paired_text = paired.to_string_lossy();
            if paired_text.ends_with(".part") || paired_text.ends_with(".part.json") {
                paths.push((paired.clone(), issue.kind));
            }
        }
        for (path, kind) in paths {
            planned.entry(path).or_insert((kind, PathBuf::new()));
        }
    }

    let mut actions: Vec<Value> = Vec::new();
    let mut applied = 0usize;
    let mut failed = 0usize;
    let timestamp = sanitize_timestamp(&rdm_agent::now_iso8601());

    for (path, (kind, _)) in planned {
        let quarantine_dir = quarantine_dir_for(&path).join(&timestamp);
        let destination = unique_destination(&quarantine_dir, &path);
        if dry_run {
            actions.push(json!({
                "action": "quarantine",
                "kind": kind,
                "path": path,
                "to": destination,
                "applied": false,
            }));
            continue;
        }
        match move_to_quarantine(&path, &destination) {
            Ok(destination) => {
                applied += 1;
                actions.push(json!({
                    "action": "quarantine",
                    "kind": kind,
                    "path": path,
                    "to": destination,
                    "applied": true,
                }));
            }
            Err(error) => {
                failed += 1;
                actions.push(json!({
                    "action": "quarantine",
                    "kind": kind,
                    "path": path,
                    "to": destination,
                    "applied": false,
                    "error": error,
                }));
            }
        }
    }

    if reporter.is_text() {
        if actions.is_empty() {
            crate::outln!("nothing to repair");
        } else {
            for action in &actions {
                let path = action.get("path").and_then(Value::as_str).unwrap_or("?");
                let to = action.get("to").and_then(Value::as_str).unwrap_or("?");
                let applied = action
                    .get("applied")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if dry_run {
                    crate::outln!("would quarantine {path} -> {to}");
                } else if let Some(error) = action.get("error").and_then(Value::as_str) {
                    crate::outln!("FAILED {path}: {error}");
                } else {
                    crate::outln!(
                        "{} {path} -> {to}",
                        if applied { "quarantined" } else { "skipped" }
                    );
                }
            }
        }
        if dry_run {
            crate::outln!("dry run: pass --no-dry-run to apply");
        }
    }

    let data = json!({
        "dry_run": dry_run,
        "actions": actions,
        "planned": actions.len(),
        "applied": applied,
        "failed": failed,
        "quarantine_root": if dry_run { Value::Null } else { json!(planned_quarantine_roots(&report.roots, &timestamp)) },
        "summary": summarize(&report.issues).to_json(),
    });
    if failed > 0 {
        reporter.finish_err(
            rdm_agent::AgentErrorInfo::new(
                rdm_agent::ErrorCode::PermissionDenied,
                format!("{failed} quarantine action(s) failed"),
            )
            .with_hint("check file permissions, then re-run `rdm state repair --no-dry-run`"),
        )
    } else {
        reporter.finish_ok(data)
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

fn scan(custom_dirs: &[PathBuf]) -> ScanReport {
    let mut roots: Vec<PathBuf> = if custom_dirs.is_empty() {
        let mut default_roots = vec![
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            util::default_download_dir(),
        ];
        if let Ok(store_root) = std::env::var("RDM_STATE_DIR") {
            default_roots.push(PathBuf::from(store_root));
        }
        default_roots
    } else {
        custom_dirs.to_vec()
    };
    // Stable de-duplication.
    let mut seen = BTreeSet::new();
    roots.retain(|root| seen.insert(normalize(root)));

    let mut report = ScanReport {
        roots,
        missing_roots: Vec::new(),
        files: 0,
        parts: 0,
        states: 0,
        issues: Vec::new(),
        truncated: false,
        warnings: Vec::new(),
    };
    for root in report.roots.clone() {
        if !root.exists() {
            report.missing_roots.push(root);
        }
    }
    let existing_roots: Vec<PathBuf> = report
        .roots
        .iter()
        .filter(|root| root.exists())
        .cloned()
        .collect();
    if existing_roots.is_empty() {
        report
            .warnings
            .push("no scan roots exist; pass --dir <DIR>".to_string());
        return report;
    }

    let mut found_parts: BTreeSet<PathBuf> = BTreeSet::new();
    let mut found_states: BTreeSet<PathBuf> = BTreeSet::new();
    let mut visited = 0usize;
    'roots: for root in &existing_roots {
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) => {
                    report
                        .warnings
                        .push(format!("cannot read {}: {error}", dir.display()));
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(_) => continue,
                };
                if file_type.is_dir() {
                    if depth < MAX_DEPTH
                        && !SKIP_DIRS
                            .iter()
                            .any(|skip| entry.file_name().to_string_lossy() == *skip)
                    {
                        stack.push((path, depth + 1));
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                visited += 1;
                if visited > MAX_FILES {
                    report.truncated = true;
                    report.warnings.push(format!(
                        "file scan capped at {MAX_FILES} entries; narrow with --dir"
                    ));
                    break 'roots;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".part.json") {
                    found_states.insert(path);
                } else if name.ends_with(".part") {
                    found_parts.insert(path);
                }
            }
        }
    }
    report.files = visited;
    report.parts = found_parts.len();
    report.states = found_states.len();

    let mut issues = Vec::new();
    let mut reported_states: BTreeSet<PathBuf> = BTreeSet::new();

    for part in &found_parts {
        let Some(target) = strip_suffix(part, ".part") else {
            continue;
        };
        let state_path = append_suffix(&target, ".part.json");
        if state_path.exists() {
            if reported_states.insert(state_path.clone()) {
                if let Err(reason) = validate_state(&state_path) {
                    issues.push(Issue {
                        id: format!("corrupt_state:{}", state_path.display()),
                        kind: "corrupt_state",
                        path: state_path.clone(),
                        paired: Some(part.clone()),
                        reason: Some(reason),
                        hint: Some(
                            "the engine ignores corrupt state and restarts from zero; quarantine it to keep the directory tidy"
                                .to_string(),
                        ),
                    });
                }
            }
            if target.exists() {
                issues.push(Issue {
                    id: format!("stale_part:{}", part.display()),
                    kind: "stale_part",
                    path: part.clone(),
                    paired: Some(target.clone()),
                    reason: Some(
                        "the completed target file exists next to a .part/.part.json pair"
                            .to_string(),
                    ),
                    hint: Some(
                        "safe to quarantine the .part and .part.json; the target file is complete"
                            .to_string(),
                    ),
                });
            }
        } else {
            issues.push(Issue {
                id: format!("orphan_part:{}", part.display()),
                kind: "orphan_part",
                path: part.clone(),
                paired: Some(target.clone()),
                reason: Some("no .part.json resume state next to this .part".to_string()),
                hint: Some(
                    "without state the engine restarts from zero; delete/quarantine the .part"
                        .to_string(),
                ),
            });
        }
    }

    for state_path in &found_states {
        let Some(target) = strip_suffix(state_path, ".part.json") else {
            continue;
        };
        let part = append_suffix(&target, ".part");
        if reported_states.contains(state_path) {
            continue;
        }
        if !part.exists() {
            issues.push(Issue {
                id: format!("orphan_state:{}", state_path.display()),
                kind: "orphan_state",
                path: state_path.clone(),
                paired: Some(target.clone()),
                reason: Some("resume state without its .part payload".to_string()),
                hint: Some("safe to quarantine; the payload it described is gone".to_string()),
            });
        } else if reported_states.insert(state_path.clone()) {
            if let Err(reason) = validate_state(state_path) {
                issues.push(Issue {
                    id: format!("corrupt_state:{}", state_path.display()),
                    kind: "corrupt_state",
                    path: state_path.clone(),
                    paired: Some(part),
                    reason: Some(reason),
                    hint: Some(
                        "the engine ignores corrupt state and restarts from zero".to_string(),
                    ),
                });
            }
        }
    }

    report.issues = issues;
    report
}

/// Validate the shared `.part.json` resume-state shape (engine internal, v1).
fn validate_state(path: &Path) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("cannot read state: {error}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("invalid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "state is not a JSON object".to_string())?;

    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing numeric `version`".to_string())?;
    if version != 1 {
        return Err(format!("unsupported state version {version} (expected 1)"));
    }
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing string `url`".to_string())?;
    if url.is_empty() {
        return Err("empty `url`".to_string());
    }
    let total_bytes = object
        .get("total_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing numeric `total_bytes`".to_string())?;
    let chunk_size = object
        .get("chunk_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing numeric `chunk_size`".to_string())?;
    if chunk_size == 0 {
        return Err("`chunk_size` must be > 0".to_string());
    }
    let total_chunks = object
        .get("total_chunks")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing numeric `total_chunks`".to_string())?;
    let expected_chunks = if total_bytes == 0 {
        0
    } else {
        total_bytes.div_ceil(chunk_size)
    };
    if total_chunks != expected_chunks {
        return Err(format!(
            "`total_chunks` is {total_chunks}, expected {expected_chunks} for {total_bytes} bytes / {chunk_size} chunk"
        ));
    }
    let completed = object
        .get("completed_chunks")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing array `completed_chunks`".to_string())?;
    for index in completed {
        let index = index
            .as_u64()
            .ok_or_else(|| "`completed_chunks` contains a non-integer".to_string())?;
        if index >= total_chunks {
            return Err(format!(
                "`completed_chunks` contains out-of-range index {index} (total {total_chunks})"
            ));
        }
    }
    let downloaded = object
        .get("downloaded_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| "missing numeric `downloaded_bytes`".to_string())?;
    if downloaded > total_bytes {
        return Err(format!(
            "`downloaded_bytes` ({downloaded}) exceeds `total_bytes` ({total_bytes})"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Summary / quarantine helpers
// ---------------------------------------------------------------------------

struct Summary {
    orphan_part: usize,
    orphan_state: usize,
    corrupt_state: usize,
    stale_part: usize,
    total: usize,
}

impl Summary {
    fn to_json(&self) -> Value {
        json!({
            "orphan_part": self.orphan_part,
            "orphan_state": self.orphan_state,
            "corrupt_state": self.corrupt_state,
            "stale_part": self.stale_part,
            "total": self.total,
        })
    }
}

fn summarize(issues: &[Issue]) -> Summary {
    let mut summary = Summary {
        orphan_part: 0,
        orphan_state: 0,
        corrupt_state: 0,
        stale_part: 0,
        total: issues.len(),
    };
    for issue in issues {
        match issue.kind {
            "orphan_part" => summary.orphan_part += 1,
            "orphan_state" => summary.orphan_state += 1,
            "corrupt_state" => summary.corrupt_state += 1,
            "stale_part" => summary.stale_part += 1,
            _ => {}
        }
    }
    summary
}

fn normalize(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn strip_suffix(path: &Path, suffix: &str) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy();
    let stem = name.strip_suffix(suffix)?;
    if stem.is_empty() {
        return None;
    }
    Some(path.with_file_name(stem))
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn sanitize_timestamp(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn quarantine_dir_for(path: &Path) -> PathBuf {
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".rdm-quarantine")
}

fn unique_destination(quarantine_dir: &Path, path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut candidate = quarantine_dir.join(&name);
    let mut suffix = 1u32;
    while candidate.exists() {
        candidate = quarantine_dir.join(format!("{name}.{suffix}"));
        suffix += 1;
    }
    candidate
}

fn move_to_quarantine(path: &Path, destination: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err("source disappeared".to_string());
    }
    let dest_dir = destination
        .parent()
        .ok_or_else(|| "invalid quarantine destination".to_string())?;
    std::fs::create_dir_all(dest_dir)
        .map_err(|error| format!("cannot create {}: {error}", dest_dir.display()))?;
    let final_path = destination.to_path_buf();
    match std::fs::rename(path, &final_path) {
        Ok(()) => Ok(final_path),
        Err(rename_error) => {
            // Cross-device or locked file: fall back to copy+remove.
            std::fs::copy(path, &final_path).map_err(|copy_error| {
                format!("rename failed ({rename_error}); copy failed: {copy_error}")
            })?;
            std::fs::remove_file(path)
                .map_err(|error| format!("copied but could not remove source: {error}"))?;
            Ok(final_path)
        }
    }
}

/// Root-level quarantine directories that would be used (for reporting).
fn planned_quarantine_roots(roots: &[PathBuf], timestamp: &str) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|root| root.exists())
        .map(|root| root.join(".rdm-quarantine").join(timestamp))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_good_state_and_rejects_bad() {
        let dir = util::unique_temp_dir("rdm-state-validate");
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("file.bin.part.json");
        std::fs::write(
            &state,
            r#"{"version":1,"url":"http://x/file","final_url":"http://x/file","file_name":"file",
                "total_bytes":1024,"etag":null,"last_modified":null,"chunk_size":256,
                "total_chunks":4,"completed_chunks":[0,1],"downloaded_bytes":512}"#,
        )
        .unwrap();
        assert!(validate_state(&state).is_ok());

        std::fs::write(&state, "not json").unwrap();
        assert!(validate_state(&state).is_err());

        std::fs::write(
            &state,
            r#"{"version":1,"url":"http://x/file","total_bytes":1024,"chunk_size":0,
                "total_chunks":4,"completed_chunks":[],"downloaded_bytes":0}"#,
        )
        .unwrap();
        assert!(validate_state(&state).unwrap_err().contains("chunk_size"));

        std::fs::write(
            &state,
            r#"{"version":1,"url":"http://x/file","total_bytes":1024,"chunk_size":256,
                "total_chunks":4,"completed_chunks":[9],"downloaded_bytes":0}"#,
        )
        .unwrap();
        assert!(validate_state(&state).unwrap_err().contains("out-of-range"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suffix_helpers_roundtrip() {
        let part = Path::new("C:\\dl\\file.bin.part");
        let target = strip_suffix(part, ".part").unwrap();
        assert_eq!(target, Path::new("C:\\dl\\file.bin"));
        assert_eq!(
            append_suffix(&target, ".part.json"),
            Path::new("C:\\dl\\file.bin.part.json")
        );
        assert!(strip_suffix(Path::new("file.bin"), ".part").is_none());
    }

    #[test]
    fn sanitizes_timestamps_for_windows_paths() {
        let value = sanitize_timestamp("2026-10-19T03:20:00.123Z");
        assert!(!value.contains(':'));
        assert!(!value.contains('.'));
        assert!(value.starts_with("2026-10-19"));
    }
}
