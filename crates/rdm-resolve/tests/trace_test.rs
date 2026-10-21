//! Sanitized run traces (`docs/architecture.md` §11.10, schema
//! `rdm.resolve.trace/v1`): round-trip, hard privacy rules, truncation caps,
//! retention and tolerant loading.
//!
//! Every test uses an isolated scratch directory under Cargo's target tmp dir
//! (see `tests/common/mod.rs`); nothing touches the OS temp dir or the network.

mod common;

use std::path::Path;

use common::ScratchDir;
use rdm_resolve::{
    digest_args, error_kind_name, redact_excerpt, redact_url, RecipeRef, ResolveErrorKind, RunStep,
    RunTrace, TraceStore, MAX_EXCERPT_CHARS, MAX_STEPS, RETENTION_RUNS, TRACE_MAX_BYTES,
    TRACE_SCHEMA,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn step(index: u32, op: &str) -> RunStep {
    RunStep {
        index,
        op: op.to_owned(),
        args_digest: digest_args(&format!("{op}:{index}")),
        args_excerpt: Some(format!("GET https://api.example.com/d/{index}")),
        outcome: "ok".to_owned(),
        duration_ms: 7,
        obs_id: Some(format!("obs_{index}")),
        note: None,
    }
}

fn sample_trace(run_id: &str) -> RunTrace {
    RunTrace {
        run_id: run_id.to_owned(),
        created_ms: 1_700_000_000_123,
        mode: "recipe".to_owned(),
        status: "succeeded".to_owned(),
        source_url: "https://example.com/file.bin".to_owned(),
        recipe: Some(RecipeRef {
            id: "example.com-file".to_owned(),
            version: 3,
            state: "matched".to_owned(),
        }),
        llm_calls: 0,
        http_requests: 3,
        bytes_read: 4096,
        elapsed_ms: 187,
        error_kind: None,
        steps: vec![step(0, "recipe_step")],
        truncated: false,
    }
}

fn first_line(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).expect("read trace file");
    let line = raw.lines().next().expect("trace header line");
    serde_json::from_str(line).expect("trace header is JSON")
}

fn runs_dir(root: &Path) -> std::path::PathBuf {
    TraceStore::runs_dir(root)
}

fn jsonl_names(runs: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(runs)
        .expect("list runs dir")
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// 1. write/load round-trip of the header and the steps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_and_load_round_trip_keeps_all_header_fields() {
    let scratch = ScratchDir::new("trace-roundtrip");
    let mut trace = sample_trace("run_1_0");
    trace.mode = "explore".to_owned();
    trace.status = "partial".to_owned();
    trace.source_url = "https://Example.com:8443/dl/file.bin?token=SECRET#frag".to_owned();
    trace.llm_calls = 3;
    trace.http_requests = 5;
    trace.bytes_read = 8192;
    trace.elapsed_ms = 456;
    trace.error_kind = Some("recipe_stale".to_owned());
    trace.steps = vec![step(0, "http_request"), step(1, "emit_manifest")];

    let path = TraceStore::write(scratch.path(), &trace).expect("write trace");
    assert_eq!(path, runs_dir(scratch.path()).join("run_1_0.jsonl"));

    let header = first_line(&path);
    assert_eq!(header["schema"], TRACE_SCHEMA);
    assert_eq!(header["run_id"], "run_1_0");
    assert_eq!(header["created_ms"], 1_700_000_000_123_u64);
    assert_eq!(header["mode"], "explore");
    assert_eq!(header["status"], "partial");
    assert_eq!(header["source_url"], "https://example.com:8443/dl/file.bin");
    assert_eq!(header["recipe"]["id"], "example.com-file");
    assert_eq!(header["recipe"]["version"], 3);
    assert_eq!(header["recipe"]["state"], "matched");
    assert_eq!(header["llm_calls"], 3);
    assert_eq!(header["http_requests"], 5);
    assert_eq!(header["bytes_read"], 8192_u64);
    assert_eq!(header["elapsed_ms"], 456);
    assert_eq!(header["error_kind"], "recipe_stale");
    assert_eq!(header["truncated"], false);

    let loaded = TraceStore::load(scratch.path(), "run_1_0").expect("load trace");
    assert_eq!(loaded.run_id, "run_1_0");
    assert_eq!(loaded.created_ms, 1_700_000_000_123);
    assert_eq!(loaded.mode, "explore");
    assert_eq!(loaded.status, "partial");
    assert_eq!(loaded.source_url, "https://example.com:8443/dl/file.bin");
    assert_eq!(loaded.recipe.as_ref().map(|recipe| recipe.version), Some(3));
    assert_eq!(loaded.llm_calls, 3);
    assert_eq!(loaded.http_requests, 5);
    assert_eq!(loaded.bytes_read, 8192);
    assert_eq!(loaded.elapsed_ms, 456);
    assert_eq!(loaded.error_kind.as_deref(), Some("recipe_stale"));
    assert!(!loaded.truncated);
    assert_eq!(loaded.steps.len(), 2);
    assert_eq!(loaded.steps[1].index, 1);
    assert_eq!(loaded.steps[1].op, "emit_manifest");
}

// ---------------------------------------------------------------------------
// 2. `source_url` never keeps query/fragment tokens in the file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn source_url_is_redacted_in_the_persisted_file() {
    let scratch = ScratchDir::new("trace-source-url");
    let mut trace = sample_trace("run_source");
    trace.source_url =
        "http://127.0.0.1:8000/download/a.bin?sig=SUPERSECRET&user=me#frag".to_owned();
    trace.steps = Vec::new();

    let path = TraceStore::write(scratch.path(), &trace).expect("write trace");
    let raw = std::fs::read_to_string(&path).expect("read trace");
    assert!(
        !raw.contains("SUPERSECRET") && !raw.contains("sig="),
        "query token leaked into the trace: {raw}"
    );
    assert!(!raw.contains('#'), "fragment leaked into the trace: {raw}");
    assert!(
        !raw.contains('?'),
        "query marker leaked into the trace: {raw}"
    );

    let loaded = TraceStore::load(scratch.path(), "run_source").expect("load");
    assert_eq!(loaded.source_url, "http://127.0.0.1:8000/download/a.bin");
}

// ---------------------------------------------------------------------------
// 3. `redact_url` pure-function contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redact_url_keeps_scheme_host_port_and_path() {
    assert_eq!(
        redact_url("https://API.Example.com:8443/d/123?token=abc&x=1#frag"),
        "https://api.example.com:8443/d/123"
    );
    assert_eq!(
        redact_url("http://127.0.0.1:8000/files/a.bin?Signature=x"),
        "http://127.0.0.1:8000/files/a.bin"
    );
    // A bare origin keeps the trailing slash (Url normalisation).
    assert_eq!(redact_url("https://example.com"), "https://example.com/");
    // Userinfo is dropped: it usually carries credentials.
    assert_eq!(
        redact_url("https://user:pass@example.com/private/x.bin?k=v"),
        "https://example.com/private/x.bin"
    );
    // Non-URL text is explicitly marked, never echoed.
    assert_eq!(redact_url("not a url"), "<invalid-url>");
    assert_eq!(redact_url(""), "<invalid-url>");
    assert_eq!(redact_url("   "), "<invalid-url>");
}

// ---------------------------------------------------------------------------
// 4. `redact_excerpt`: credentials + embedded URLs + length cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redact_excerpt_redacts_credentials_and_url_queries() {
    let raw = "GET https://api.example.com/d/1?token=SECRET1 \
               Cookie: session=SECRET2; Authorization: Bearer SECRET3 \
               proxy-authorization: SECRET4";
    let sanitized = redact_excerpt(raw);
    for secret in ["SECRET1", "SECRET2", "SECRET3", "SECRET4"] {
        assert!(!sanitized.contains(secret), "{secret} leaked: {sanitized}");
    }
    assert!(sanitized.contains("[redacted]"), "{sanitized}");
    // The URL keeps only scheme://host/path (no query).
    assert!(
        sanitized.contains("https://api.example.com/d/1"),
        "{sanitized}"
    );

    // The same rules apply to a persisted step: re-sanitized on write.
    let scratch = ScratchDir::new("trace-excerpt-step");
    let mut trace = sample_trace("run_excerpt");
    let mut dirty = step(0, "http_request");
    dirty.args_excerpt =
        Some("GET https://api.example.com/d/1?token=SECRET5 Cookie: sid=SECRET6".to_owned());
    dirty.note = Some("Authorization: Bearer SECRET7".to_owned());
    trace.steps = vec![dirty];
    let path = TraceStore::write(scratch.path(), &trace).expect("write");
    let raw = std::fs::read_to_string(&path).expect("read");
    for secret in ["SECRET5", "SECRET6", "SECRET7"] {
        assert!(
            !raw.contains(secret),
            "{secret} leaked into the file: {raw}"
        );
    }
    assert!(raw.contains("[redacted]"), "{raw}");
    let loaded = TraceStore::load(scratch.path(), "run_excerpt").expect("load");
    let excerpt = loaded.steps[0].args_excerpt.clone().unwrap_or_default();
    let note = loaded.steps[0].note.clone().unwrap_or_default();
    assert!(excerpt.contains("[redacted]"), "{excerpt}");
    assert!(!excerpt.contains("SECRET5"), "{excerpt}");
    assert!(note.contains("[redacted]"), "{note}");
    assert!(!note.contains("SECRET7"), "{note}");
}

#[tokio::test]
async fn redact_excerpt_caps_the_length() {
    assert_eq!(MAX_EXCERPT_CHARS, 256);
    let long = format!(
        "see (https://example.com/a?x=1) {}",
        "y".repeat(MAX_EXCERPT_CHARS * 2)
    );
    let excerpt = redact_excerpt(&long);
    assert_eq!(excerpt.chars().count(), MAX_EXCERPT_CHARS);
    assert!(
        excerpt.starts_with("see (https://example.com/a)"),
        "{excerpt}"
    );
}

// ---------------------------------------------------------------------------
// 5. Truncation at MAX_STEPS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_truncates_at_max_steps_and_marks_the_file() {
    let scratch = ScratchDir::new("trace-truncate");
    let mut trace = sample_trace("run_truncated");
    trace.steps = (0..MAX_STEPS as u32 + 25)
        .map(|index| step(index, "http_request"))
        .collect();

    let path = TraceStore::write(scratch.path(), &trace).expect("write");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert!(
        raw.trim_end().ends_with("{\"truncated\":true}"),
        "missing truncation marker: {}",
        &raw[raw.len().saturating_sub(80)..]
    );
    assert_eq!(
        raw.lines().count(),
        1 + MAX_STEPS + 1,
        "header + {MAX_STEPS} steps + marker"
    );

    let loaded = TraceStore::load(scratch.path(), "run_truncated").expect("load");
    assert!(loaded.truncated);
    assert_eq!(loaded.steps.len(), MAX_STEPS);
}

// ---------------------------------------------------------------------------
// 6. Truncation at the byte cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_enforces_the_byte_cap() {
    assert_eq!(TRACE_MAX_BYTES, 512 * 1024);
    let scratch = ScratchDir::new("trace-byte-cap");
    let mut trace = sample_trace("run_bytes");
    trace.steps = (0..MAX_STEPS as u32)
        .map(|index| {
            let mut item = step(index, "http_request");
            item.args_digest = "d".repeat(4 * 1024);
            item
        })
        .collect();

    let path = TraceStore::write(scratch.path(), &trace).expect("write");
    let bytes = std::fs::metadata(&path).expect("metadata").len() as usize;
    assert!(
        bytes <= TRACE_MAX_BYTES + 64,
        "trace is {bytes} bytes (cap {TRACE_MAX_BYTES})"
    );
    let loaded = TraceStore::load(scratch.path(), "run_bytes").expect("load");
    assert!(loaded.truncated);
    assert!(loaded.steps.len() < MAX_STEPS);
}

// ---------------------------------------------------------------------------
// 7. Retention: at most RETENTION_RUNS files, oldest pruned
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retention_prunes_the_oldest_runs() {
    assert_eq!(RETENTION_RUNS, 200);
    let scratch = ScratchDir::new("trace-retention");
    let runs = runs_dir(scratch.path());
    std::fs::create_dir_all(&runs).expect("runs dir");
    let seeded = RETENTION_RUNS + 10;
    for index in 0..seeded {
        std::fs::write(
            runs.join(format!("run_seed_{index:03}.jsonl")),
            "{\"schema\":\"rdm.resolve.trace/v1\"}\n",
        )
        .expect("seed run file");
    }

    let path =
        TraceStore::write(scratch.path(), &sample_trace("run_newest")).expect("write new trace");
    assert!(path.exists());

    let names = jsonl_names(&runs);
    assert_eq!(
        names.len(),
        RETENTION_RUNS,
        "retention must keep exactly {RETENTION_RUNS} runs"
    );
    assert!(names.iter().any(|name| name == "run_newest.jsonl"));
    let seed_kept = names
        .iter()
        .filter(|name| name.starts_with("run_seed_"))
        .count();
    assert_eq!(seed_kept, RETENTION_RUNS - 1);
    // The oldest files (lexicographically first, also written first) are gone.
    for index in 0..(seeded - (RETENTION_RUNS - 1)) {
        let name = format!("run_seed_{index:03}.jsonl");
        assert!(!runs.join(&name).exists(), "{name} should have been pruned");
    }
}

// ---------------------------------------------------------------------------
// 8. Tolerant loading: bad step lines, corrupt/missing headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn load_tolerates_bad_step_lines_but_rejects_a_bad_header() {
    let scratch = ScratchDir::new("trace-tolerant");
    let mut trace = sample_trace("run_tolerant");
    trace.steps = vec![step(0, "recipe_step"), step(1, "recipe_step")];
    let path = TraceStore::write(scratch.path(), &trace).expect("write");

    // Garbage between valid lines and an explicit truncation marker.
    let raw = std::fs::read_to_string(&path).expect("read");
    let mut lines: Vec<&str> = raw.lines().collect();
    lines.insert(1, "this is not json");
    lines.push("{\"truncated\":true}");
    std::fs::write(&path, lines.join("\n")).expect("rewrite");

    let loaded = TraceStore::load(scratch.path(), "run_tolerant").expect("load tolerant");
    assert_eq!(loaded.steps.len(), 2, "bad step lines are skipped");
    assert!(loaded.truncated, "the marker still marks the run truncated");

    // Missing file -> NotFound; unreadable header -> InvalidData.
    let missing = TraceStore::load(scratch.path(), "run_missing").expect_err("missing run");
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

    let broken_header = runs_dir(scratch.path()).join("run_broken_header.jsonl");
    std::fs::write(&broken_header, "not a json header\n").expect("write broken header");
    let error = TraceStore::load(scratch.path(), "run_broken_header").expect_err("bad header");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    let empty = runs_dir(scratch.path()).join("run_empty.jsonl");
    std::fs::write(&empty, "").expect("write empty trace");
    let error = TraceStore::load(scratch.path(), "run_empty").expect_err("empty trace");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

// ---------------------------------------------------------------------------
// 9. Pure helpers: digest_args / error_kind_name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_and_error_kind_helpers_are_stable() {
    let digest = digest_args("GET https://example.com/a");
    assert!(digest.starts_with("sha256:"), "{digest}");
    assert_eq!(digest.len(), "sha256:".len() + 64);
    assert_eq!(digest, digest_args("GET https://example.com/a"));
    assert_ne!(digest, digest_args("GET https://example.com/b"));

    for (kind, name) in [
        (ResolveErrorKind::RecipeStale, "recipe_stale"),
        (ResolveErrorKind::SelectorMiss, "selector_miss"),
        (ResolveErrorKind::ExpectFailed, "expect_failed"),
        (ResolveErrorKind::BudgetExhausted, "budget_exhausted"),
        (ResolveErrorKind::ModelUnavailable, "model_unavailable"),
        (ResolveErrorKind::NoResources, "no_resources"),
        (ResolveErrorKind::Internal, "internal"),
    ] {
        assert_eq!(error_kind_name(&kind), name, "{kind:?}");
        assert_eq!(ResolveErrorKind::from_name(name), Some(kind), "{name}");
    }
}

// ---------------------------------------------------------------------------
// 10. Run ids cannot escape the runs directory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trace_file_names_cannot_escape_the_runs_directory() {
    let scratch = ScratchDir::new("trace-run-id");
    let mut trace = sample_trace("../../etc/passwd");
    trace.steps = Vec::new();

    let path = TraceStore::write(scratch.path(), &trace).expect("write");
    assert_eq!(
        path.parent(),
        Some(runs_dir(scratch.path()).as_path()),
        "the trace must stay inside <root>/runs"
    );
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".._.._etc_passwd.jsonl")
    );
    assert_eq!(jsonl_names(&runs_dir(scratch.path())).len(), 1);

    let loaded = TraceStore::load(scratch.path(), "../../etc/passwd").expect("load by run id");
    assert_eq!(loaded.run_id, "../../etc/passwd");
}

// ---------------------------------------------------------------------------
// 11. Write failures are returned, never panics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_reports_an_unusable_root_instead_of_panicking() {
    let scratch = ScratchDir::new("trace-blocked-root");
    let blocked = scratch.join("blocked-root");
    std::fs::write(&blocked, b"this is a file, not a directory").expect("blocking file");

    let error = TraceStore::write(&blocked, &sample_trace("run_blocked"))
        .expect_err("a file root must fail the write");
    assert!(
        blocked.is_file(),
        "the blocking file must stay untouched: {error}"
    );
    assert!(TraceStore::load(&blocked, "run_blocked").is_err());
}
