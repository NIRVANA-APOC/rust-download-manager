//! Process-level end-to-end tests: spawn the real `rdm-mcp` binary, speak
//! newline-delimited JSON-RPC on its stdin/stdout, and drive the in-process
//! fixture HTTP server. This is the protocol contract test (µdocs §3/§8).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rdm_fixture_server::{Config, TestServer};
use serde_json::{json, Value};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

fn temp_root(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let index = SEQ.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("rdm-mcp-it-{tag}-{}-{index}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    root
}

fn write_policy(root: &Path, overrides: Value) {
    let mut policy = json!({
        "schema_version": "rdm.agent/v1",
        "allowed_hosts": ["127.0.0.1"],
        "allowed_dirs": [root.join("downloads")],
        "max_bytes_per_task": 10u64 * 1024 * 1024 * 1024,
        "require_approval_over_bytes": 10u64 * 1024 * 1024 * 1024,
        "max_concurrent_tasks": 2,
        "default_connections": 4,
        "allow_private_hosts": true,
        "allow_overwrite": false
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut policy, overrides) {
        for (key, value) in extra {
            base.insert(key, value);
        }
    }
    std::fs::write(
        root.join("policy.json"),
        serde_json::to_string_pretty(&policy).unwrap(),
    )
    .unwrap();
}

/// Minimal MCP stdio client used to drive the real binary.
struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    notifications: Vec<Value>,
    next_id: u64,
}

impl Client {
    fn start(state_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rdm-mcp"))
            .arg("--state-dir")
            .arg(state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rdm-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => {
                        if tx.send(value).is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
        });
        Self {
            child,
            stdin,
            rx,
            notifications: Vec::new(),
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write to rdm-mcp stdin");
        self.stdin.flush().expect("flush stdin");
    }

    fn request_with_id(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.await_response(&id)
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = Value::from(self.next_id);
        self.next_id += 1;
        self.request_with_id(id, method, params)
    }

    fn await_response(&mut self, id: &Value) -> Value {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for response to id {id}");
            }
            match self.rx.recv_timeout(remaining) {
                Ok(message) => {
                    if message.get("id") == Some(id)
                        && (message.get("result").is_some() || message.get("error").is_some())
                    {
                        return message;
                    }
                    if message.get("id").is_none() {
                        self.notifications.push(message);
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for id {id}"),
                Err(RecvTimeoutError::Disconnected) => panic!("rdm-mcp exited unexpectedly"),
            }
        }
    }

    fn initialize(&mut self, version: &str) -> Value {
        let message = self.request(
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": { "name": "rdm-integration-test", "version": "1.0.0" }
            }),
        );
        self.send(
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
        );
        message
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    fn tool_with_progress(&mut self, name: &str, arguments: Value, progress_token: &str) -> Value {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
                "_meta": { "progressToken": progress_token }
            }),
        )
    }

    fn close(mut self) -> i32 {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => return status.code().unwrap_or(-1),
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    return -1;
                }
                None => thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

fn structured(message: &Value) -> &Value {
    &message["result"]["structuredContent"]
}

fn assert_tool_ok(message: &Value) {
    assert_eq!(
        message["result"]["isError"], false,
        "tool failed: {message}"
    );
    assert_eq!(structured(message)["ok"], true);
}

async fn wait_until_json<F>(mut predicate: F, timeout: Duration)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "wait_until_json timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_end_to_end_download_lifecycle() {
    let root = temp_root("lifecycle");
    write_policy(&root, json!({}));
    let fixture = TestServer::start(Config::new(8 * 1024 * 1024).with_delay(60))
        .await
        .unwrap();
    let url = fixture.file_url();
    let expected_sha = sha256_of(&fixture.content());
    let mut client = Client::start(&root);

    // --- initialize (legacy handshake) --------------------------------
    let initialize = client.initialize("2025-06-18");
    assert_eq!(initialize["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(initialize["result"]["serverInfo"]["name"], "rdm");
    assert!(initialize["result"]["capabilities"]["tools"].is_object());
    assert!(initialize["result"]["capabilities"]["resources"].is_object());
    assert!(initialize["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("rdm_capabilities"));

    // --- tools/list ---------------------------------------------------
    let tools = client.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 16);
    assert_eq!(names[0], "rdm_capabilities");
    assert_eq!(names[15], "rdm_state_maintenance");

    // --- probe --------------------------------------------------------
    let probe = client.tool("rdm_probe", json!({ "url": url }));
    assert_tool_ok(&probe);
    assert_eq!(structured(&probe)["data"]["total_bytes"], 8 * 1024 * 1024);
    assert_eq!(structured(&probe)["data"]["range_supported"], true);

    // --- download with wait + progress notifications ------------------
    let download = client.tool_with_progress(
        "rdm_download",
        json!({
            "url": url,
            "file_name": "it.bin",
            "wait": true,
            "wait_timeout_ms": 60000,
            "expected_sha256": expected_sha,
            "cookie": "session=integration-secret",
            "headers": { "Authorization": "Bearer integration-secret" },
            "idempotency_key": "it-lifecycle-1"
        }),
        "it-progress-1",
    );
    assert_tool_ok(&download);
    let task = &structured(&download)["data"]["task"];
    assert_eq!(
        task["state"], "completed",
        "download did not complete: {download}"
    );
    let task_id = structured(&download)["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = task["path"].as_str().unwrap().to_string();
    assert!(Path::new(&path).exists(), "downloaded file missing: {path}");
    let sha256 = task["result"]["sha256"].as_str().unwrap().to_string();
    assert_eq!(sha256.len(), 64);
    assert_eq!(sha256, expected_sha);
    let progress: Vec<&Value> = client
        .notifications
        .iter()
        .filter(|note| note["method"] == "notifications/progress")
        .collect();
    assert!(!progress.is_empty(), "expected progress notifications");
    assert!(progress
        .iter()
        .any(|note| note["params"]["progressToken"] == "it-progress-1"));
    // Secrets never cross the agent boundary.
    let visible = serde_json::to_string(&client.notifications).unwrap();
    assert!(!visible.contains("integration-secret"));
    let download_json = serde_json::to_string(&download).unwrap();
    assert!(!download_json.contains("integration-secret"));

    // --- verify_file --------------------------------------------------
    let verify = client.tool(
        "rdm_verify_file",
        json!({ "task_id": task_id, "expected_sha256": sha256 }),
    );
    assert_tool_ok(&verify);
    assert_eq!(structured(&verify)["data"]["match"], true);

    // --- list / get (events) -----------------------------------------
    let list = client.tool("rdm_list_tasks", json!({ "limit": 10 }));
    assert_tool_ok(&list);
    assert!(structured(&list)["data"]["total"].as_u64().unwrap() >= 1);
    let detail = client.tool(
        "rdm_get_task",
        json!({ "task_id": task_id, "include_events": true, "event_limit": 50 }),
    );
    assert_tool_ok(&detail);
    let events = structured(&detail)["data"]["events"].as_array().unwrap();
    assert!(!events.is_empty());
    assert!(!serde_json::to_string(events)
        .unwrap()
        .contains("integration-secret"));

    // --- resources + prompts -----------------------------------------
    let resources = client.request("resources/list", json!({}));
    assert_eq!(
        resources["result"]["resources"].as_array().unwrap().len(),
        4
    );
    let capability_resource =
        client.request("resources/read", json!({ "uri": "rdm://capabilities" }));
    assert!(capability_resource["result"]["contents"][0]["text"]
        .as_str()
        .unwrap()
        .contains("rdm_capabilities"));
    let task_resource = client.request(
        "resources/read",
        json!({ "uri": format!("rdm://tasks/{task_id}") }),
    );
    assert!(task_resource["result"]["contents"][0]["text"]
        .as_str()
        .unwrap()
        .contains(&task_id));
    let prompts = client.request("prompts/list", json!({}));
    assert_eq!(prompts["result"]["prompts"].as_array().unwrap().len(), 3);
    let prompt = client.request(
        "prompts/get",
        json!({ "name": "download", "arguments": { "url": url } }),
    );
    assert!(prompt["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("rdm_probe"));

    // --- diagnostics --------------------------------------------------
    let doctor = client.tool("rdm_doctor", json!({}));
    assert_tool_ok(&doctor);
    assert!(
        structured(&doctor)["data"]["checks"]
            .as_array()
            .unwrap()
            .len()
            >= 5
    );
    let selftest = client.tool("rdm_selftest", json!({ "size_bytes": 1024 * 1024 }));
    assert_tool_ok(&selftest);
    let cases = structured(&selftest)["data"]["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    assert!(
        cases.iter().all(|case| case["status"] == "pass"),
        "selftest cases failed: {selftest}"
    );
    let bench = client.tool(
        "rdm_bench",
        json!({ "size_bytes": 256 * 1024, "connections": [1, 2], "repeats": 1 }),
    );
    assert_tool_ok(&bench);
    assert!(
        structured(&bench)["data"]["medians"]
            .as_array()
            .unwrap()
            .len()
            == 2
    );
    let maintenance = client.tool("rdm_state_maintenance", json!({}));
    assert_tool_ok(&maintenance);
    assert_eq!(structured(&maintenance)["data"]["dry_run"], true);
    // dry-run reports an orphan .part; repair deletes it.
    let orphan = root.join("downloads").join("orphan-it.part");
    std::fs::write(&orphan, b"orphan").unwrap();
    let dry = client.tool("rdm_state_maintenance", json!({ "dry_run": true }));
    assert_tool_ok(&dry);
    assert!(structured(&dry)["data"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["kind"] == "orphan_part"));
    assert!(orphan.exists(), "dry-run must not delete files");
    let repaired = client.tool("rdm_state_maintenance", json!({ "dry_run": false }));
    assert_tool_ok(&repaired);
    assert!(!orphan.exists(), "repair deletes the orphan .part");
    assert!(structured(&repaired)["data"]["repaired"].as_u64().unwrap() >= 1);

    // --- modern (dual-era) layer -------------------------------------
    let modern_meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let discover = client.request("server/discover", json!({ "_meta": modern_meta }));
    assert_eq!(discover["result"]["resultType"], "complete");
    assert_eq!(discover["result"]["supportedVersions"][0], "2026-07-28");
    let modern_tools = client.request("tools/list", json!({ "_meta": modern_meta }));
    assert_eq!(modern_tools["result"]["resultType"], "complete");
    let modern_ping = client.request("ping", json!({ "_meta": modern_meta }));
    assert_eq!(modern_ping["error"]["code"], -32601);
    // This client completed the legacy handshake, so after probing the modern
    // era a request without `_meta` still falls back to legacy semantics.
    let legacy_fallback = client.request("tools/list", json!({}));
    assert!(
        legacy_fallback.get("result").is_some(),
        "initialized legacy client must keep working after a modern probe: {legacy_fallback}"
    );

    // --- stdin EOF: pause running tasks, persist, exit cleanly -------
    let background = client.tool(
        "rdm_download",
        json!({ "url": url, "file_name": "eof-paused.bin", "wait": false }),
    );
    assert_tool_ok(&background);
    let background_id = structured(&background)["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Wait until bytes are actually transferred so the EOF assertion checks
    // persisted progress rather than a still-queued task.
    let mut progressed = false;
    for _ in 0..200 {
        let current = client.tool("rdm_get_task", json!({ "task_id": background_id }));
        if structured(&current)["data"]["task"]["downloaded_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0
        {
            progressed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        progressed,
        "background task never transferred bytes before EOF"
    );
    let exit = client.close();
    assert_eq!(exit, 0, "rdm-mcp should exit 0 on stdin EOF");
    let mut eof_record = None;
    for entry in std::fs::read_dir(root.join("tasks")).unwrap().flatten() {
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
        if value["id"] == background_id {
            eof_record = Some(value);
        }
    }
    let eof_record = eof_record.expect("EOF task must be persisted");
    assert_eq!(
        eof_record["state"], "paused",
        "EOF must pause running tasks before exit: {eof_record}"
    );
    assert!(
        eof_record["downloaded_bytes"].as_u64().unwrap_or(0) > 0,
        "EOF paused task must persist downloaded_bytes > 0: {eof_record}"
    );
    fixture.shutdown().await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_flow_conflict_and_safe_defaults() {
    let fixture = TestServer::start(Config::new(512 * 1024)).await.unwrap();
    let url = fixture.file_url();

    // -- approval flow -------------------------------------------------
    let root = temp_root("approval");
    write_policy(&root, json!({ "require_approval_over_bytes": 1 }));
    let mut client = Client::start(&root);
    client.initialize("2025-03-26");

    // -- explicit plan -> apply flow ----------------------------------
    let planned = client.tool(
        "rdm_plan_download",
        json!({ "url": url, "file_name": "planned.bin" }),
    );
    assert_tool_ok(&planned);
    let plan_data = &structured(&planned)["data"];
    assert_eq!(plan_data["approval_required"], true);
    let planned_plan_id = plan_data["plan"]["plan_id"].as_str().unwrap().to_string();
    let planned_token = plan_data["approval_token"].as_str().unwrap().to_string();
    let planned_apply = client.tool(
        "rdm_apply_plan",
        json!({
            "plan_id": planned_plan_id,
            "approval_token": planned_token,
            "wait": true,
            "wait_timeout_ms": 60000
        }),
    );
    assert_tool_ok(&planned_apply);
    assert_eq!(
        structured(&planned_apply)["data"]["task"]["state"],
        "completed"
    );

    // -- download returns needs_approval -> apply ---------------------
    let download = client.tool(
        "rdm_download",
        json!({ "url": url, "file_name": "approval.bin", "wait": false }),
    );
    assert_eq!(download["result"]["isError"], true);
    let error = &structured(&download)["error"];
    assert_eq!(error["code"], "needs_approval");
    let plan_id = error["details"]["plan"]["plan_id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = error["details"]["approval_token"]
        .as_str()
        .unwrap()
        .to_string();

    let applied = client.tool(
        "rdm_apply_plan",
        json!({ "plan_id": plan_id, "approval_token": token, "wait": true, "wait_timeout_ms": 60000 }),
    );
    assert_tool_ok(&applied);
    assert_eq!(structured(&applied)["data"]["task"]["state"], "completed");

    let replay = client.tool(
        "rdm_apply_plan",
        json!({ "plan_id": plan_id, "approval_token": token }),
    );
    assert_eq!(replay["result"]["isError"], true);
    assert_eq!(structured(&replay)["error"]["code"], "not_found");

    let conflict = client.tool(
        "rdm_download",
        json!({ "url": url, "file_name": "approval.bin", "wait": false }),
    );
    assert_eq!(conflict["result"]["isError"], true);
    assert_eq!(structured(&conflict)["error"]["code"], "conflict");
    assert_eq!(client.close(), 0);

    // -- safe defaults: no policy file => private hosts denied ---------
    let defaults_root = temp_root("defaults");
    let mut client = Client::start(&defaults_root);
    client.initialize("2024-11-05");
    let probe = client.tool("rdm_probe", json!({ "url": url }));
    assert_eq!(probe["result"]["isError"], true);
    assert_eq!(structured(&probe)["error"]["code"], "policy_denied");
    let download = client.tool("rdm_download", json!({ "url": url }));
    assert_eq!(download["result"]["isError"], true);
    assert_eq!(structured(&download)["error"]["code"], "policy_denied");
    assert_eq!(client.close(), 0);

    fixture.shutdown().await;
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&defaults_root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_cancellation_and_pause_resume_over_stdio() {
    let root = temp_root("controls");
    write_policy(&root, json!({}));
    let fixture = TestServer::start(
        Config::new(8 * 1024 * 1024)
            .with_delay(40)
            .with_delay_every(64 * 1024),
    )
    .await
    .unwrap();
    let url = fixture.file_url();
    let mut client = Client::start(&root);
    client.initialize("2025-06-18");

    // -- notifications/cancelled only aborts the wait -----------------
    let started = client.tool(
        "rdm_download",
        json!({ "url": url, "file_name": "wait-cancel.bin", "wait": false }),
    );
    assert_tool_ok(&started);
    let task_id = structured(&started)["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let wait_id = Value::from(77001);
    client.send(&json!({
        "jsonrpc": "2.0",
        "id": wait_id,
        "method": "tools/call",
        "params": {
            "name": "rdm_wait_task",
            "arguments": { "task_id": task_id, "timeout_ms": 30000 },
            "_meta": { "progressToken": "wait-token" }
        }
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": wait_id, "reason": "test" }
    }));
    let wait_response = client.await_response(&wait_id);
    assert_eq!(wait_response["result"]["isError"], true);
    assert_eq!(structured(&wait_response)["error"]["code"], "cancelled");
    let still_running = client.tool("rdm_get_task", json!({ "task_id": task_id }));
    let state = structured(&still_running)["data"]["task"]["state"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !["cancelled", "completed", "failed"].contains(&state.as_str()),
        "cancelling the wait must not cancel the task (state={state})"
    );

    // -- cancel_task (keeps partial state) ----------------------------
    let cancelled = client.tool(
        "rdm_cancel_task",
        json!({ "task_id": task_id, "discard_partial": false }),
    );
    assert_tool_ok(&cancelled);
    assert_eq!(structured(&cancelled)["data"]["task"]["state"], "cancelled");
    let resumed = client.tool("rdm_resume_task", json!({ "task_id": task_id }));
    assert_tool_ok(&resumed);
    assert_ne!(structured(&resumed)["data"]["task"]["state"], "cancelled");
    let settled = client.tool(
        "rdm_wait_task",
        json!({ "task_id": task_id, "timeout_ms": 60000 }),
    );
    assert_tool_ok(&settled);
    assert_eq!(structured(&settled)["data"]["task"]["state"], "completed");

    // -- pause / resume -----------------------------------------------
    let started = client.tool(
        "rdm_download",
        json!({ "url": url, "file_name": "pause.bin", "wait": false }),
    );
    assert_tool_ok(&started);
    let pause_id = structured(&started)["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let client_ref = &mut client;
    wait_until_json(
        || {
            let current = client_ref.tool("rdm_get_task", json!({ "task_id": pause_id }));
            let task = &structured(&current)["data"]["task"];
            task["state"] == "downloading" && task["downloaded_bytes"].as_u64().unwrap_or(0) > 0
        },
        Duration::from_secs(30),
    )
    .await;
    let paused = client.tool("rdm_pause_task", json!({ "task_id": pause_id }));
    assert_tool_ok(&paused);
    assert_eq!(structured(&paused)["data"]["task"]["state"], "paused");
    assert!(
        structured(&paused)["data"]["task"]["downloaded_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "paused task must expose downloaded_bytes > 0: {paused}"
    );
    let persisted_pause = client.tool("rdm_get_task", json!({ "task_id": pause_id }));
    assert!(
        structured(&persisted_pause)["data"]["task"]["downloaded_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    let resumed = client.tool("rdm_resume_task", json!({ "task_id": pause_id }));
    assert_tool_ok(&resumed);
    let settled = client.tool(
        "rdm_wait_task",
        json!({ "task_id": pause_id, "timeout_ms": 60000 }),
    );
    assert_tool_ok(&settled);
    assert_eq!(structured(&settled)["data"]["task"]["state"], "completed");

    assert_eq!(client.close(), 0);
    fixture.shutdown().await;
    std::fs::remove_dir_all(&root).ok();
}

/// SHA-256 of the fixture payload (byte-level equality check).
fn sha256_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modern_only_client_requires_per_request_meta() {
    let root = temp_root("modern-meta");
    let mut client = Client::start(&root);
    // Modern clients do not perform the legacy initialize handshake.
    let modern_meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let discover = client.request("server/discover", json!({ "_meta": modern_meta }));
    assert_eq!(discover["result"]["resultType"], "complete");

    // Missing per-request `_meta` on a modern-only connection is malformed
    // params: -32602, never the legacy "not initialized" -32002.
    let no_meta = client.request("tools/list", json!({}));
    assert_eq!(no_meta["error"]["code"], -32602, "{no_meta}");
    assert_ne!(no_meta["error"]["code"], -32002);

    // A modern request with the required _meta keeps working.
    let with_meta = client.request("tools/list", json!({ "_meta": modern_meta }));
    assert_eq!(with_meta["result"]["resultType"], "complete");
    assert_eq!(with_meta["result"]["tools"].as_array().unwrap().len(), 16);

    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();
}
