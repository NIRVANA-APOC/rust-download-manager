//! `rdm tasks` / `rdm task <id>` — read the durable task store.

use rdm_agent::{TaskFilter, TaskRecord, TaskStore, TaskSummary};

use crate::cli::{TaskArgs, TasksArgs};
use crate::output::{invalid_request, OutputMode, Reporter};
use crate::util;

/// List tasks (summaries only; no credentials by construction).
pub fn list(mode: OutputMode, store_root: &std::path::Path, args: TasksArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "tasks");
    let states = match util::parse_states(&args.states) {
        Ok(states) => states,
        Err(message) => return reporter.finish_err(invalid_request(message)),
    };
    if args.limit == 0 || args.limit > 10_000 {
        return reporter.finish_err(invalid_request("--limit must be between 1 and 10000"));
    }

    let store = TaskStore::new(store_root);
    let filter = TaskFilter {
        states,
        limit: Some(args.limit),
        cursor: None,
    };
    let records = match store.list(&filter) {
        Ok(records) => records,
        Err(error) => return reporter.finish_err(super::store_error(error)),
    };
    let tasks: Vec<TaskSummary> = records
        .iter()
        .map(|record| TaskSummary::from(&record.redacted()))
        .collect();

    if reporter.is_text() {
        if tasks.is_empty() {
            crate::outln!("no tasks in {}", store_root.display());
            crate::outln!("hint: `rdm download <url> --output json` creates one");
        } else {
            for task in &tasks {
                crate::outln!(
                    "{}  {:<11}  {:<24}  {}/{}  {}",
                    task.id,
                    task.state.as_str(),
                    truncate(&task.file_name, 24),
                    util::format_bytes(task.downloaded_bytes),
                    task.total_bytes
                        .map(util::format_bytes)
                        .unwrap_or_else(|| "?".to_string()),
                    task.url,
                );
            }
            crate::outln!("{} task(s) in {}", tasks.len(), store_root.display());
        }
    }

    let data = serde_json::json!({
        "tasks": tasks,
        "count": tasks.len(),
        "limit": args.limit,
        "state_dir": store_root.display().to_string(),
    });
    reporter.finish_ok(data)
}

/// Show one task (redacted) and optionally its event journal.
pub fn show(mode: OutputMode, store_root: &std::path::Path, args: TaskArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "task");
    reporter.set_task_id(args.id.clone());
    let store = TaskStore::new(store_root);

    let record: TaskRecord = match store.get(&args.id) {
        Ok(record) => record,
        Err(error) => return reporter.finish_err(super::store_error(error)),
    };
    let events = if args.events > 0 {
        match store.events(&args.id, args.events) {
            Ok(events) => events,
            Err(error) => return reporter.finish_err(super::store_error(error)),
        }
    } else {
        Vec::new()
    };

    let visible = record.redacted().view();
    if reporter.is_text() {
        crate::outln!("task    {}", visible.id);
        crate::outln!("state   {}", visible.state.as_str());
        crate::outln!("url     {}", visible.url);
        crate::outln!("file    {}", visible.file_name);
        crate::outln!("dest    {}", visible.dest_dir.display());
        if let Some(path) = &visible.path {
            crate::outln!("path    {}", path.display());
        }
        crate::outln!(
            "bytes   {} / {}",
            util::format_bytes(visible.downloaded_bytes),
            visible
                .total_bytes
                .map(util::format_bytes)
                .unwrap_or_else(|| "?".to_string())
        );
        crate::outln!("links   {} connection(s)", visible.connections);
        if let Some(version) = &visible.http_version {
            crate::outln!("http    {version}");
        }
        if !visible.warnings.is_empty() {
            crate::outln!("warns   {}", visible.warnings.join(", "));
        }
        if let Some(result) = &visible.result {
            crate::outln!(
                "result  {} ({}, sha256 {})",
                result.path,
                util::format_bytes(result.size),
                result.sha256.as_deref().unwrap_or("n/a")
            );
        }
        if let Some(error) = &visible.error {
            crate::outln!("error   [{}] {}", error.code.as_str(), error.message);
            if let Some(hint) = &error.hint {
                crate::outln!("hint    {hint}");
            }
        }
        crate::outln!("created {}", visible.created_at);
        crate::outln!("updated {}", visible.updated_at);
        if !events.is_empty() {
            crate::outln!("-- last {} event(s) --", events.len());
            for event in &events {
                crate::outln!("#{:<3} {:<12} {}", event.seq, event.r#type, event.data);
            }
        }
    }

    let data = serde_json::json!({
        "task": visible,
        "events": events,
        "state_dir": store_root.display().to_string(),
    });
    reporter.finish_ok(data)
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_utf8_safely() {
        assert_eq!(truncate("short", 10), "short");
        let long = "中文文件名中文文件名中文文件名";
        let truncated = truncate(long, 5);
        assert_eq!(truncated.chars().count(), 5);
        assert!(truncated.ends_with('…'));
    }
}
