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
        Self::start_with_env(state_dir, &[])
    }

    fn start_with_localappdata(state_dir: &Path, localappdata: &Path) -> Self {
        Self::start_with_env(
            state_dir,
            &[("LOCALAPPDATA", localappdata.to_string_lossy().into_owned())],
        )
    }

    fn start_with_env(state_dir: &Path, envs: &[(&str, String)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rdm-mcp"));
        command
            .arg("--state-dir")
            .arg(state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("RDM_AGENT_STATE_DIR")
            // Never let a developer's real AI key leak into the isolated tests.
            .env_remove("RDM_AI_API_KEY");
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn rdm-mcp");
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
    assert_eq!(names.len(), 17);
    assert_eq!(names[0], "rdm_capabilities");
    assert_eq!(names[2], "rdm_resolve");
    assert_eq!(names[16], "rdm_state_maintenance");

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
            "idempotency_key": "it-lifecycle-1",
            "dynamic_connections": false
        }),
        "it-progress-1",
    );
    assert_tool_ok(&download);
    let task = &structured(&download)["data"]["task"];
    assert_eq!(
        task["state"], "completed",
        "download did not complete: {download}"
    );
    // The explicit dynamic_connections=false is honoured end-to-end and the
    // realised concurrency peak is exposed on the TaskView/result.
    assert_eq!(task["dynamic_connections"], false);
    let peak = task["peak_connections"].as_u64().unwrap();
    assert!(
        (2..=4).contains(&peak),
        "unexpected peak_connections {peak}: {task}"
    );
    assert_eq!(task["result"]["peak_connections"], peak);
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
    assert_eq!(
        structured(&detail)["data"]["task"]["dynamic_connections"],
        false
    );
    assert_eq!(
        structured(&detail)["data"]["task"]["peak_connections"],
        peak,
        "get_task must expose the same realised peak as the download result"
    );
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
    // v1.4 additive: read-only recipe self-test report is always present.
    let recipes = &structured(&selftest)["data"]["recipes"];
    assert!(recipes.is_object(), "{selftest}");
    assert!(recipes["dir"].as_str().is_some(), "{selftest}");
    assert!(recipes["checked"].as_u64().is_some(), "{selftest}");
    assert!(recipes["failures"].is_array(), "{selftest}");
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
        json!({ "url": url, "file_name": "planned.bin", "dynamic_connections": false }),
    );
    assert_tool_ok(&planned);
    let plan_data = &structured(&planned)["data"];
    assert_eq!(plan_data["approval_required"], true);
    assert_eq!(plan_data["plan"]["dynamic_connections"], false);
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
    assert_eq!(
        structured(&planned_apply)["data"]["task"]["dynamic_connections"],
        false,
        "apply must preserve the planned dynamic_connections flag"
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

// ---------------------------------------------------------------------------
// rdm_resolve (v1.2 link resolution)
// ---------------------------------------------------------------------------

/// Minimal direct-link recipe: fetch the input URL once and emit it as the
/// resource; the resolver verifier (HEAD + bounded ranged GET) does the rest.
const DIRECT_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "fixture-direct",
  "version": 1,
  "name": "Fixture direct link",
  "match": { "hosts": ["127.0.0.1"], "url_pattern": "^(?P<input_url>http://.+)$" },
  "limits": { "requests": 8, "bytes_read": 262144, "wall_ms": 15000, "redirects": 3 },
  "steps": [
    {
      "id": "fetch",
      "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 1024 },
      "expect": { "status_in": [200, 206] }
    }
  ],
  "emit": { "url": "{{input_url}}" }
}"#;

/// Isolated `%LOCALAPPDATA%` root: the resolver looks up recipes under
/// `<LOCALAPPDATA>/RDM/resolve/recipes`, so tests must never touch the real
/// user profile.
fn localappdata_root(root: &Path) -> PathBuf {
    let local = root.join("localappdata");
    std::fs::create_dir_all(&local).unwrap();
    local
}

fn write_direct_recipe(localappdata: &Path) {
    let recipes = localappdata.join("RDM").join("resolve").join("recipes");
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(recipes.join("fixture-direct@1.json"), DIRECT_RECIPE).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_tool_success_and_failure_paths() {
    let fixture = TestServer::start(Config::new(96 * 1024)).await.unwrap();
    let url = fixture.file_url();
    let mut html_config = Config::new(8 * 1024);
    html_config.content_type = "text/html".to_string();
    let html_fixture = TestServer::start(html_config).await.unwrap();
    let html_url = html_fixture.file_url();

    // --- success: recipe replay + built-in verification ----------------
    let root = temp_root("resolve-ok");
    write_policy(&root, json!({}));
    let local = localappdata_root(&root);
    write_direct_recipe(&local);
    let mut client = Client::start_with_localappdata(&root, &local);
    let initialize = client.initialize("2025-06-18");
    assert_eq!(initialize["result"]["serverInfo"]["name"], "rdm");
    let resolved = client.tool("rdm_resolve", json!({ "url": url, "mode": "recipe_only" }));
    assert_tool_ok(&resolved);
    let data = &structured(&resolved)["data"];
    assert_eq!(data["status"], "succeeded");
    assert!(data["run_id"].as_str().unwrap().starts_with("run_"));
    assert_eq!(data["recipe"]["id"], "fixture-direct");
    assert_eq!(data["diagnostics"]["llm_calls"], 0);
    let resource = &data["manifest"]["resources"][0];
    assert_eq!(resource["url"], url);
    assert_eq!(resource["verification"]["status"], "verified");
    assert_eq!(resource["size_bytes"], 96 * 1024);
    assert_eq!(resource["transfer"]["resumable"], true);

    // --- invalid_url: bad input, never a policy mask -------------------
    let bad = client.tool("rdm_resolve", json!({ "url": "not-a-url" }));
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert_eq!(structured(&bad)["error"]["code"], "invalid_url");
    assert_eq!(structured(&bad)["data"], Value::Null);
    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();

    // --- no recipe: auto falls back to built-in direct verification ----
    let root = temp_root("resolve-fail");
    write_policy(&root, json!({}));
    let local = localappdata_root(&root); // no recipes installed
    let mut client = Client::start_with_localappdata(&root, &local);
    client.initialize("2025-06-18");
    let direct = client.tool("rdm_resolve", json!({ "url": url }));
    assert_tool_ok(&direct);
    let direct_data = &structured(&direct)["data"];
    assert_eq!(direct_data["status"], "succeeded");
    assert_eq!(direct_data["recipe"], Value::Null, "{direct_data}");
    assert_eq!(
        direct_data["manifest"]["resources"][0]["verification"]["status"],
        "verified"
    );

    // --- failed resolution still returns the full ResolveOutcome ------
    let failed = client.tool("rdm_resolve", json!({ "url": html_url }));
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    let failed_structured = structured(&failed);
    assert_eq!(failed_structured["error"]["code"], "resolve_failed");
    assert_eq!(failed_structured["error"]["details"]["kind"], "not_a_file");
    assert!(failed_structured["error"]["details"]["run_id"].is_string());
    assert_eq!(failed_structured["data"]["status"], "failed");
    assert!(failed_structured["data"]["run_id"].is_string());
    assert!(failed_structured["data"]["diagnostics"]["error"].is_object());

    // recipe_only never falls back: an empty library is recipe_no_match.
    let no_recipe = client.tool("rdm_resolve", json!({ "url": url, "mode": "recipe_only" }));
    assert_eq!(no_recipe["result"]["isError"], true, "{no_recipe}");
    assert_eq!(structured(&no_recipe)["error"]["code"], "resolve_failed");
    assert_eq!(
        structured(&no_recipe)["error"]["details"]["kind"],
        "recipe_no_match"
    );
    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();

    // --- policy_denied: loopback host outside the policy --------------
    let root = temp_root("resolve-policy");
    write_policy(
        &root,
        json!({ "allowed_hosts": ["*.example.com"], "allow_private_hosts": false }),
    );
    let mut client = Client::start(&root);
    client.initialize("2025-06-18");
    let denied = client.tool("rdm_resolve", json!({ "url": url }));
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert_eq!(structured(&denied)["error"]["code"], "policy_denied");
    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();

    fixture.shutdown().await;
    html_fixture.shutdown().await;
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
    assert_eq!(with_meta["result"]["tools"].as_array().unwrap().len(), 17);

    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_stream_download_and_plan_apply() {
    // Engine `single_stream`: one plain GET with no Range header; the plan ->
    // apply path must preserve the flag (plan summary normalized to 1 conn).
    let fixture = TestServer::start(Config::new(128 * 1024)).await.unwrap();
    let url = fixture.file_url();
    let root = temp_root("single-stream");
    write_policy(&root, json!({}));
    let mut client = Client::start(&root);
    client.initialize("2025-06-18");

    // --- rdm_download single_stream=true ------------------------------
    fixture.reset_stats();
    let download = client.tool(
        "rdm_download",
        json!({
            "url": url,
            "file_name": "single.bin",
            "wait": true,
            "wait_timeout_ms": 60000,
            "single_stream": true,
            // Single-stream wins over explicit connection knobs.
            "connections": 8,
            "dynamic_connections": true
        }),
    );
    assert_tool_ok(&download);
    let task = &structured(&download)["data"]["task"];
    assert_eq!(task["state"], "completed", "{download}");
    assert_eq!(task["connections"], 1);
    assert_eq!(task["dynamic_connections"], false);
    let stats = fixture.stats();
    // prepare_download probes once for policy/metadata (HEAD, and
    // single_stream skips the ranged probe), then the engine's own probe
    // (HEAD) and the single plain GET.
    assert_eq!(
        stats.methods,
        vec!["HEAD".to_string(), "HEAD".to_string(), "GET".to_string()],
        "single_stream must be HEAD probes + one plain GET: {stats:?}"
    );
    assert!(
        stats.range_headers.is_empty(),
        "single_stream must never send a Range header: {:?}",
        stats.range_headers
    );

    // --- invalid type names the field -------------------------------
    let bad = client.tool(
        "rdm_download",
        json!({ "url": url, "single_stream": "yes" }),
    );
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert_eq!(structured(&bad)["error"]["code"], "invalid_request");
    assert!(structured(&bad)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("single_stream"));
    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();

    // --- plan -> apply preserves single_stream ----------------------
    let root = temp_root("single-stream-plan");
    write_policy(&root, json!({ "require_approval_over_bytes": 1 }));
    let mut client = Client::start(&root);
    client.initialize("2025-06-18");
    fixture.reset_stats();
    let planned = client.tool(
        "rdm_plan_download",
        json!({ "url": url, "file_name": "planned.bin", "single_stream": true }),
    );
    assert_tool_ok(&planned);
    let plan = &structured(&planned)["data"]["plan"];
    assert_eq!(plan["single_stream"], true, "{planned}");
    assert_eq!(plan["connections"], 1);
    assert_eq!(plan["dynamic_connections"], false);
    let plan_id = plan["plan_id"].as_str().unwrap().to_string();
    let token = structured(&planned)["data"]["approval_token"]
        .as_str()
        .unwrap()
        .to_string();

    let applied = client.tool(
        "rdm_apply_plan",
        json!({
            "plan_id": plan_id,
            "approval_token": token,
            "wait": true,
            "wait_timeout_ms": 60000
        }),
    );
    assert_tool_ok(&applied);
    assert_eq!(structured(&applied)["data"]["task"]["state"], "completed");
    let stats = fixture.stats();
    // plan probe (HEAD) + apply re-probe (HEAD) + engine probe (HEAD) + the
    // single plain GET; no ranged request anywhere.
    assert_eq!(
        stats.methods,
        vec![
            "HEAD".to_string(),
            "HEAD".to_string(),
            "HEAD".to_string(),
            "GET".to_string()
        ],
        "plan -> apply must keep the no-Range single-GET shape: {stats:?}"
    );
    assert!(stats.range_headers.is_empty(), "{:?}", stats.range_headers);
    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// rdm_resolve v1.3: mode=explore with a user-configured AI provider
// ---------------------------------------------------------------------------

/// Write the user's AI provider config (`<LOCALAPPDATA>\RDM\ai.json`).
fn write_ai_config(localappdata: &Path, base_url: &str, api_key: &str) {
    let state = localappdata.join("RDM");
    std::fs::create_dir_all(&state).unwrap();
    let config = json!({
        "enabled": true,
        "base_url": base_url,
        "model": "mock-model",
        "api_key": api_key,
        "timeout_secs": 10
    });
    std::fs::write(
        state.join("ai.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
}

/// Minimal share page that links to the fixture's direct file URL.
async fn spawn_share_page(file_url: &str) -> (String, tokio::sync::watch::Sender<bool>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind share page");
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/share/abc");
    let html = format!(
        "<!doctype html><html><head><title>Share page</title></head><body>\
         <h1>RDM fixture share page</h1><a href=\"{file_url}\">download the file</a></body></html>"
    );
    let (shutdown, mut rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = rx.changed() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                break;
            };
            let html = html.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    head.extend_from_slice(&chunk[..read]);
                    if head.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let is_head = head.starts_with(b"HEAD ");
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    html.len()
                )
                .into_bytes();
                if !is_head {
                    response.extend_from_slice(html.as_bytes());
                }
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            });
        }
    });
    (url, shutdown)
}

/// Scripted OpenAI-compatible mock (`POST /chat/completions`): turn 1 reads the
/// share page, later turns emit the fixture URL with the real observation id.
struct MockLlm {
    base_url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl MockLlm {
    async fn start(share_url: &str, file_url: &str) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock llm");
        let addr = listener.local_addr().unwrap();
        let share_url = share_url.to_string();
        let file_url = file_url.to_string();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let (shutdown, mut rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = rx.changed() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, _)) = accepted else {
                    break;
                };
                let recorded = std::sync::Arc::clone(&recorded);
                let share_url = share_url.clone();
                let file_url = file_url.clone();
                tokio::spawn(async move {
                    let body = read_http_body(&mut stream).await;
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    recorded.lock().unwrap().push(request.clone());
                    let content = scripted_reply(&request, &share_url, &file_url);
                    let payload = json!({
                        "choices": [{"message": {"role": "assistant", "content": content}}]
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    use tokio::io::AsyncWriteExt;
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
            shutdown,
        }
    }

    fn recorded_requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn shutdown(self) {
        let _ = self.shutdown.send(true);
    }
}

async fn read_http_body(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return buffer;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..position]).to_ascii_lowercase();
            let expected = head
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while buffer.len() < position + 4 + expected {
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return buffer[position + 4..].to_vec();
        }
    }
}

fn scripted_reply(request: &Value, share_url: &str, file_url: &str) -> String {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let assistant_turns = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .count();
    if assistant_turns == 0 {
        return json!({"action": "page_read", "url": share_url}).to_string();
    }
    let conversation = serde_json::to_string(request).unwrap_or_default();
    match find_observation_id(&conversation) {
        Some(observation) => json!({
            "action": "emit_manifest",
            "resources": [{
                "url": file_url,
                "evidence": format!("{observation}#links/0"),
                "name": "file"
            }],
            "notes": ["scripted mock"]
        })
        .to_string(),
        None => json!({
            "action": "finish",
            "reason": "mock saw no observation id",
            "kind": "no_resources"
        })
        .to_string(),
    }
}

fn find_observation_id(conversation: &str) -> Option<String> {
    let mut search = conversation;
    while let Some(start) = search.find("obs_") {
        let rest = &search[start..];
        let end = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        // Real ids are `obs_<pid>_<seq>`; skip the prompt's placeholder forms.
        let parts: Vec<&str> = candidate.split('_').collect();
        if parts.len() >= 3
            && parts[1..]
                .iter()
                .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        {
            return Some(candidate.to_string());
        }
        search = &rest[end..];
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_tool_explore_uses_configured_ai() {
    let fixture = TestServer::start(Config::new(64 * 1024)).await.unwrap();
    let (share_url, share_shutdown) = spawn_share_page(&fixture.file_url()).await;
    let mock = MockLlm::start(&share_url, &fixture.file_url()).await;
    let root = temp_root("resolve-explore-ai");
    write_policy(&root, json!({}));
    let local = localappdata_root(&root);
    write_ai_config(&local, &mock.base_url, "mcp-test-key-987654");
    let mut client = Client::start_with_localappdata(&root, &local);
    client.initialize("2025-06-18");

    let resolved = client.tool(
        "rdm_resolve",
        json!({ "url": share_url, "mode": "explore", "max_llm_calls": 3 }),
    );
    assert_tool_ok(&resolved);
    let data = &structured(&resolved)["data"];
    assert_eq!(data["status"], "succeeded", "{resolved}");
    // v1.5: an expressible exploration distills a local draft instead of null.
    assert_eq!(data["recipe"]["state"], "draft", "{resolved}");
    assert!(
        data["recipe"]["id"].as_str().unwrap().starts_with("draft_"),
        "{resolved}"
    );
    assert_eq!(data["manifest"]["resources"][0]["url"], fixture.file_url());
    assert_eq!(
        data["manifest"]["resources"][0]["verification"]["status"],
        "verified"
    );
    let llm_calls = data["diagnostics"]["llm_calls"].as_u64().unwrap();
    assert!((2..=3).contains(&llm_calls), "{resolved}");

    // Privacy: neither the tool result nor the prompt bodies carry the key.
    let raw = serde_json::to_string(&resolved).unwrap();
    assert!(
        !raw.contains("mcp-test-key-987654"),
        "tool result leaked key"
    );
    let recorded = mock.recorded_requests();
    assert!(!recorded.is_empty());
    for (index, request) in recorded.iter().enumerate() {
        let text = serde_json::to_string(request).unwrap();
        assert!(
            !text.contains("mcp-test-key-987654"),
            "prompt body leaked the key: {text}"
        );
        if index > 0 {
            assert!(
                text.contains("untrusted_content"),
                "tool results must be untrusted_content: {text}"
            );
        }
    }

    // Schema/capabilities contract (still 17 tools).
    let capabilities = client.tool("rdm_capabilities", json!({}));
    assert_tool_ok(&capabilities);
    let features = structured(&capabilities)["data"]["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let link = features
        .iter()
        .position(|feature| *feature == "link-resolution")
        .expect("link-resolution feature");
    assert_eq!(
        features.get(link + 1).copied(),
        Some("link-resolution-explore"),
        "link-resolution-explore must follow link-resolution: {features:?}"
    );
    let tools = client.request("tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 17);
    let resolve_tool = tools
        .iter()
        .find(|tool| tool["name"] == "rdm_resolve")
        .expect("rdm_resolve tool");
    let property = &resolve_tool["inputSchema"]["properties"]["max_llm_calls"];
    assert_eq!(property["type"], "integer");
    assert_eq!(property["minimum"], 1);
    assert_eq!(property["maximum"], 32);
    assert_eq!(property["default"], 8);

    // Out-of-range max_llm_calls is a typed invalid_request.
    let out_of_range = client.tool(
        "rdm_resolve",
        json!({ "url": share_url, "mode": "explore", "max_llm_calls": 33 }),
    );
    assert_eq!(out_of_range["result"]["isError"], true, "{out_of_range}");
    assert_eq!(
        structured(&out_of_range)["error"]["code"],
        "invalid_request"
    );
    assert!(structured(&out_of_range)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("max_llm_calls"));

    assert_eq!(client.close(), 0);
    mock.shutdown();
    let _ = share_shutdown.send(true);
    std::fs::remove_dir_all(&root).ok();
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_tool_explore_without_ai_is_model_unavailable() {
    let mut config = Config::new(8 * 1024);
    config.content_type = "text/html".to_string();
    let fixture = TestServer::start(config).await.unwrap();
    let root = temp_root("resolve-explore-noai");
    write_policy(&root, json!({}));
    let local = localappdata_root(&root);
    let mut client = Client::start_with_localappdata(&root, &local);
    client.initialize("2025-06-18");
    let resolved = client.tool(
        "rdm_resolve",
        json!({ "url": fixture.file_url(), "mode": "explore" }),
    );
    assert_eq!(resolved["result"]["isError"], true, "{resolved}");
    let payload = structured(&resolved);
    assert_eq!(payload["error"]["code"], "resolve_failed");
    assert_eq!(
        payload["error"]["details"]["kind"], "model_unavailable",
        "{resolved}"
    );
    // The failed call still carries the full ResolveOutcome for diagnostics.
    assert_eq!(payload["data"]["status"], "failed");
    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();
    fixture.shutdown().await;
}

// ---------------------------------------------------------------------------
// rdm_selftest v1.4: read-only recipe self-test report
// ---------------------------------------------------------------------------

/// Recipe whose pure matching self-tests pass (one match, one no-match, one skip).
const SELFTEST_PASS_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "selftest-pass",
  "version": 1,
  "name": "Selftest passing recipe",
  "match": { "hosts": ["example.com"], "url_pattern": "^https://example\\.com/file/(?P<id>[0-9]+)$" },
  "steps": [
    { "id": "fetch", "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 1024 } }
  ],
  "emit": { "url": "{{input_url}}" },
  "tests": [
    { "name": "matches-file-url", "input_url": "https://example.com/file/42" },
    { "name": "rejects-other-host", "input_url": "https://mirror.example.net/file/42",
      "expect_no_match": true },
    { "name": "skipped-needs-login", "input_url": "https://example.com/file/43",
      "skip": "requires login" }
  ]
}"#;

/// Recipe with one deliberately failing `expect_no_match` self-test.
const SELFTEST_FAILING_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "selftest-failing",
  "version": 2,
  "name": "Selftest failing recipe",
  "match": { "hosts": ["fail.example"], "url_pattern": "^https://fail\\.example/" },
  "steps": [
    { "id": "fetch", "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 1024 } }
  ],
  "emit": { "url": "{{input_url}}" },
  "tests": [
    { "name": "must-not-match", "input_url": "https://fail.example/file/1",
      "expect_no_match": true }
  ]
}"#;

fn write_selftest_recipes(localappdata: &Path) {
    let recipes = localappdata.join("RDM").join("resolve").join("recipes");
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(recipes.join("selftest-pass@1.json"), SELFTEST_PASS_RECIPE).unwrap();
    std::fs::write(
        recipes.join("selftest-failing@2.json"),
        SELFTEST_FAILING_RECIPE,
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selftest_tool_reports_recipe_self_tests() {
    let root = temp_root("selftest-recipes");
    write_policy(&root, json!({}));
    let local = localappdata_root(&root);
    write_selftest_recipes(&local);
    let mut client = Client::start_with_localappdata(&root, &local);
    client.initialize("2025-06-18");

    let selftest = client.tool("rdm_selftest", json!({ "size_bytes": 512 * 1024 }));
    assert_tool_ok(&selftest);
    let data = &structured(&selftest)["data"];
    // Engine result is untouched by recipe failures.
    assert_eq!(data["passed"], 4, "{selftest}");
    assert_eq!(data["failed"], 0, "{selftest}");
    let recipes = &data["recipes"];
    let expected_dir = local.join("RDM").join("resolve").join("recipes");
    assert_eq!(recipes["dir"], expected_dir.display().to_string());
    assert_eq!(recipes["checked"], 2, "{selftest}");
    assert_eq!(recipes["passed"], 2, "{selftest}");
    assert_eq!(recipes["failed"], 1, "{selftest}");
    assert_eq!(recipes["skipped"], 1, "{selftest}");
    let failures = recipes["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "{selftest}");
    assert_eq!(failures[0]["recipe_id"], "selftest-failing");
    assert_eq!(failures[0]["version"], 2);
    assert_eq!(failures[0]["test"], "must-not-match");
    assert!(failures[0]["message"]
        .as_str()
        .unwrap()
        .contains("must not match but did"));

    // Tool count/schema contract: still 17 tools, recipes schema is additive.
    let tools = client.request("tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 17);
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "rdm_selftest")
        .expect("rdm_selftest tool");
    let data_schema = &tool["outputSchema"]["properties"]["data"]["anyOf"][0];
    let recipes_schema = &data_schema["properties"]["recipes"];
    assert_eq!(recipes_schema["type"], "object", "{tool}");
    assert_eq!(recipes_schema["properties"]["failures"]["maxItems"], 50);
    assert!(
        !data_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "recipes"),
        "recipes is an additive/optional field: {tool}"
    );

    // Missing recipe library → checked=0 (still a successful selftest).
    let empty_root = temp_root("selftest-no-recipes");
    write_policy(&empty_root, json!({}));
    let empty_local = localappdata_root(&empty_root);
    let mut empty_client = Client::start_with_localappdata(&empty_root, &empty_local);
    empty_client.initialize("2025-06-18");
    let empty = empty_client.tool("rdm_selftest", json!({ "size_bytes": 512 * 1024 }));
    assert_tool_ok(&empty);
    let empty_recipes = &structured(&empty)["data"]["recipes"];
    assert_eq!(empty_recipes["checked"], 0, "{empty}");
    assert_eq!(empty_recipes["passed"], 0);
    assert_eq!(empty_recipes["failed"], 0);
    assert!(empty_recipes["failures"].as_array().unwrap().is_empty());
    assert_eq!(empty_client.close(), 0);
    std::fs::remove_dir_all(&empty_root).ok();

    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// rdm_resolve v1.6 §8.6: library method health (`recipe_health`)
// ---------------------------------------------------------------------------

/// Library recipe that replays a fixture URL and emits it. Against an HTML
/// fixture the replay succeeds but verification yields `partial`/`not_a_file`
/// (a non-structural failure), so a seeded health status stays observable.
const HEALTH_RECIPE: &str = r#"{
  "schema": "rdm.recipe/v1",
  "id": "fixture-html",
  "version": 1,
  "name": "Fixture HTML page",
  "match": { "hosts": ["127.0.0.1"], "url_pattern": "^(?P<input_url>http://.+)$" },
  "limits": { "requests": 4, "bytes_read": 131072, "wall_ms": 10000, "redirects": 2 },
  "steps": [
    {
      "id": "fetch",
      "op": "http_request",
      "args": { "method": "GET", "url": "{{input_url}}", "max_bytes": 512 },
      "expect": { "status_in": [200] }
    }
  ],
  "emit": { "url": "{{input_url}}" }
}"#;

fn write_health_recipe(localappdata: &Path) {
    let recipes = localappdata.join("RDM").join("resolve").join("recipes");
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(recipes.join("fixture-html@1.json"), HEALTH_RECIPE).unwrap();
}

fn write_health_index(localappdata: &Path, status: &str, consecutive_failures: u32) {
    let resolve = localappdata.join("RDM").join("resolve");
    std::fs::create_dir_all(&resolve).unwrap();
    let index = json!({
        "schema": "rdm.recipe-index/v1",
        "updated_ms": 1_792_345_678_901u64,
        "entries": {
            "fixture-html": {
                "active_version": 1,
                "updated_ms": 1_792_345_678_901u64,
                "health": {
                    "status": status,
                    "last_ok_ms": null,
                    "last_failure": null,
                    "consecutive_failures": consecutive_failures,
                    "evidence": []
                }
            }
        }
    });
    std::fs::write(
        resolve.join("index.json"),
        serde_json::to_string_pretty(&index).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_reports_library_method_health() {
    let mut config = Config::new(8 * 1024);
    config.content_type = "text/html".to_string();
    let fixture = TestServer::start(config).await.unwrap();
    let url = fixture.file_url();
    let root = temp_root("resolve-health");
    write_policy(&root, json!({}));
    let local = localappdata_root(&root);
    write_health_recipe(&local);
    write_health_index(&local, "degraded", 2);

    let mut client = Client::start_with_localappdata(&root, &local);
    client.initialize("2025-06-18");
    let resolved = client.tool(
        "rdm_resolve",
        json!({ "url": url, "mode": "recipe_only", "recipe_id": "fixture-html" }),
    );
    assert_tool_ok(&resolved);
    let data = &structured(&resolved)["data"];
    assert_eq!(data["status"], "partial", "{resolved}");
    assert_eq!(data["recipe"]["id"], "fixture-html");
    assert_eq!(data["recipe"]["state"], "matched");
    let health = &data["recipe_health"];
    assert_eq!(health["status"], "degraded", "{resolved}");
    assert_eq!(health["consecutive_failures"], 2);
    assert_eq!(health["last_failure"]["kind"], "not_a_file");
    assert!(health["evidence"].is_array());
    // No new tool: the field is additive on the existing rdm_resolve data.
    let tools = client.request("tools/list", json!({}));
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 17);

    assert_eq!(client.close(), 0);
    std::fs::remove_dir_all(&root).ok();
    fixture.shutdown().await;
}
