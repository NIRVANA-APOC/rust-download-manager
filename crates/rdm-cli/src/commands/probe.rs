//! `rdm probe` — remote metadata without downloading.

use serde_json::{json, Value};

use crate::cli::ProbeArgs;
use crate::output::{engine_error, invalid_request, OutputMode, Reporter};
use crate::util;

pub async fn run(mode: OutputMode, args: ProbeArgs) -> u8 {
    let mut reporter = Reporter::new(mode, "probe");
    let request = match super::download::request_from_parts(args.url, args.net) {
        Ok(request) => request,
        Err(message) => return reporter.finish_err(invalid_request(message)),
    };

    match rdm_core::probe(&request).await {
        Ok(meta) => {
            let mut warnings: Vec<&str> = Vec::new();
            if !meta.supports_ranges {
                warnings.push("range_unsupported");
            }
            if meta.total_bytes.is_none() {
                warnings.push("no_content_length");
            }
            let data = json!({
                "final_url": meta.final_url,
                "file_name": meta.file_name,
                "total_bytes": meta.total_bytes,
                "supports_ranges": meta.supports_ranges,
                "content_type": meta.content_type,
                "etag": meta.etag,
                "last_modified": meta.last_modified,
                "http_version": meta.http_version,
                "warnings": warnings,
            });

            if reporter.is_text() {
                print_meta(&meta, &warnings);
            }
            if reporter.is_ndjson() {
                reporter.event("probe", data.clone());
            }
            reporter.finish_ok(data)
        }
        Err(error) => {
            let info = engine_error(&error);
            reporter.finish_err(redecorate_probe_hint(info))
        }
    }
}

fn print_meta(meta: &rdm_core::RemoteMeta, warnings: &[&str]) {
    crate::outln!("url:           {}", meta.final_url);
    crate::outln!("file name:     {}", meta.file_name);
    crate::outln!(
        "size:          {}",
        match meta.total_bytes {
            Some(bytes) => format!("{} ({} bytes)", util::format_bytes(bytes), bytes),
            None => "unknown".to_string(),
        }
    );
    crate::outln!(
        "ranges:        {}",
        if meta.supports_ranges { "yes" } else { "no" }
    );
    if let Some(content_type) = &meta.content_type {
        crate::outln!("content type:  {content_type}");
    }
    if let Some(etag) = &meta.etag {
        crate::outln!("etag:          {etag}");
    }
    if let Some(last_modified) = &meta.last_modified {
        crate::outln!("last modified: {last_modified}");
    }
    if let Some(version) = &meta.http_version {
        crate::outln!("http version:  {version}");
    }
    if !warnings.is_empty() {
        crate::outln!("warnings:      {}", warnings.join(", "));
    }
}

fn redecorate_probe_hint(mut info: rdm_agent::AgentErrorInfo) -> rdm_agent::AgentErrorInfo {
    if info.hint.is_none() {
        info.hint = Some("run `rdm doctor --url <URL> --output json` to diagnose".to_string());
    }
    // Preserve any structured details; probes are read-only.
    info
}

#[allow(dead_code)]
fn _value(_: Value) {}
