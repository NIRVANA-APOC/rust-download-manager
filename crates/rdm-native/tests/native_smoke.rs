//! End-to-end smoke tests for the `rdm-native` binary.
//!
//! These tests deliberately drive the **real executable** over piped
//! stdin/stdout exactly like Firefox does:
//!
//! * a mock RDM GUI IPC server listens on `127.0.0.1:0` and writes a matching
//!   `ipc.json` into a per-test temp state directory;
//! * the child process gets `LOCALAPPDATA` pointed at that temp directory, so
//!   tests run in parallel and never touch the developer's real
//!   `%LOCALAPPDATA%\RDM`;
//! * no environment variables are mutated inside the test process, so a
//!   parallel test binary cannot observe them.
//!
//! Every helper cleans up its child process and mock server; no background
//! processes are left behind even when an assertion fails.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rdm_ipc::{Envelope, IpcInfo, Request};
use rdm_native::framing::{read_frame, write_frame};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Temp state directory (LOCALAPPDATA override for the child)
// ---------------------------------------------------------------------------

struct TempState {
    root: PathBuf,
}

impl TempState {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "rdm-native-it-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("RDM")).expect("create temp state dir");
        TempState { root }
    }

    fn local_appdata(&self) -> &Path {
        &self.root
    }

    fn rdm_dir(&self) -> PathBuf {
        self.root.join("RDM")
    }

    fn write_ipc_info(&self, info: &IpcInfo) {
        let path = self.rdm_dir().join("ipc.json");
        let json = serde_json::to_vec_pretty(info).expect("serialize IpcInfo");
        std::fs::write(&path, json).expect("write ipc.json");
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.rdm_dir().join("logs").join("native.log")).unwrap_or_default()
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Mock RDM GUI IPC server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RecordedRequest {
    token: Option<String>,
    request: Request,
}

struct MockIpcServer {
    port: u16,
    token: String,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockIpcServer {
    fn start(token: &str) -> Self {
        Self::start_with_status_data(token, default_status_data())
    }

    /// Start a mock GUI whose `Request::Status` answer is `status_data`.
    ///
    /// Used to emulate an **old GUI** (before the additive thread-count
    /// fields) while keeping every other method identical.
    fn start_with_status_data(token: &str, status_data: serde_json::Value) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock IPC listener");
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let port = listener.local_addr().expect("local addr").port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let handle = {
            let recorded = Arc::clone(&recorded);
            let stop = Arc::clone(&stop);
            let expected_token = token.to_string();
            let status_data = Arc::new(status_data);
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            if let Err(err) =
                                handle_connection(stream, &expected_token, &recorded, &status_data)
                            {
                                eprintln!("mock IPC connection error: {err}");
                            }
                        }
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        MockIpcServer {
            port,
            token: token.to_string(),
            recorded,
            stop,
            handle: Some(handle),
        }
    }

    fn ipc_info(&self) -> IpcInfo {
        IpcInfo {
            protocol: rdm_ipc::PROTOCOL_VERSION,
            port: self.port,
            token: self.token.clone(),
            pid: std::process::id(),
            app_version: "test-gui".to_string(),
        }
    }

    fn recorded(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().expect("lock recorded").clone()
    }

    /// Wait until at least `count` requests were answered.
    fn wait_for_requests(&self, count: usize, timeout: Duration) -> Vec<RecordedRequest> {
        let deadline = Instant::now() + timeout;
        loop {
            let requests = self.recorded();
            if requests.len() >= count || Instant::now() >= deadline {
                return requests;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for MockIpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// `Request::Status` payload of a current (v0.2) GUI, including the additive
/// thread-count fields (`active_connections`/`target_connections`/
/// `peak_connections`).
fn default_status_data() -> serde_json::Value {
    serde_json::json!({
        "downloads": [{
            "id": "dl_000001",
            "file_name": "a.bin",
            "state": "downloading",
            "downloaded": 1_048_576u64,
            "total": 10_485_760u64,
            "speed_bps": 524_288.0,
            "connections": 8u64,
            "active_connections": 6u64,
            "target_connections": 12u64,
            "peak_connections": 16u64,
        }],
        "global_speed_bps": 1_048_576.0,
        "active": 2u64,
    })
}

fn handle_connection(
    stream: TcpStream,
    expected_token: &str,
    recorded: &Arc<Mutex<Vec<RecordedRequest>>>,
    status_data: &serde_json::Value,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let envelope: Envelope = match serde_json::from_str(line.trim()) {
        Ok(envelope) => envelope,
        Err(err) => {
            let response = format!(
                "{{\"id\":0,\"ok\":false,\"error\":\"bad envelope: {}\"}}\n",
                err.to_string().replace('"', "'")
            );
            writer.write_all(response.as_bytes())?;
            return Ok(());
        }
    };

    recorded
        .lock()
        .expect("lock recorded")
        .push(RecordedRequest {
            token: envelope.token.clone(),
            request: envelope.request.clone(),
        });

    if envelope.token.as_deref() != Some(expected_token) {
        let response = format!(
            "{{\"id\":{},\"ok\":false,\"error\":\"unauthorized\"}}\n",
            envelope.id
        );
        writer.write_all(response.as_bytes())?;
        writer.flush()?;
        return Ok(());
    }

    let response = match &envelope.request {
        Request::Ping => format!(
            "{{\"id\":{},\"ok\":true,\"data\":{{\"app_version\":\"test-gui\"}}}}\n",
            envelope.id
        ),
        Request::Enqueue { .. } => format!(
            "{{\"id\":{},\"ok\":true,\"data\":{{\"download_id\":\"dl_42\"}}}}\n",
            envelope.id
        ),
        Request::AiParse { .. } => format!(
            "{{\"id\":{},\"ok\":true,\"data\":{{\"accepted\":true,\"request_id\":12}}}}\n",
            envelope.id
        ),
        Request::Status => {
            format!(
                "{{\"id\":{},\"ok\":true,\"data\":{status_data}}}\n",
                envelope.id
            )
        }
        _ => format!("{{\"id\":{},\"ok\":true,\"data\":null}}\n", envelope.id),
    };
    writer.write_all(response.as_bytes())?;
    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Host child process harness
// ---------------------------------------------------------------------------

struct HostProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: std::sync::mpsc::Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
}

impl HostProcess {
    fn spawn(state: &TempState, gui_path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rdm-native"))
            .env("LOCALAPPDATA", state.local_appdata())
            .env("RDM_GUI_PATH", gui_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is intentionally discarded: the host mirrors warnings
            // there for the browser console, and an undrained pipe can
            // deadlock a chatty child.
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rdm-native");
        let stdin = child.stdin.take().expect("child stdin");
        let mut stdout = child.stdout.take().expect("child stdout");
        let (sender, responses) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            while let Ok(Some(frame)) = read_frame(&mut stdout) {
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        HostProcess {
            child,
            stdin: Some(stdin),
            responses,
            reader: Some(reader),
        }
    }

    fn send_json(&mut self, json: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        write_frame(stdin, json.as_bytes()).expect("write native-message frame");
        stdin.flush().expect("flush frame");
    }

    fn send_raw_frame(&mut self, payload: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        write_frame(stdin, payload).expect("write raw native-message frame");
        stdin.flush().expect("flush frame");
    }

    fn recv_json(&self) -> serde_json::Value {
        let frame = self
            .responses
            .recv_timeout(RESPONSE_TIMEOUT)
            .expect("host must answer every request");
        serde_json::from_slice(&frame).unwrap_or_else(|err| panic!("response is not JSON ({err})"))
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => return status,
                None if Instant::now() >= deadline => {
                    panic!("rdm-native did not exit within {timeout:?}")
                }
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn unreachable_gui_path(state: &TempState) -> PathBuf {
    state
        .local_appdata()
        .join("no-such-gui")
        .join("rdm-gui.exe")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn ping_reports_gui_offline_without_starting_anything() {
    let state = TempState::new("ping");
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    host.send_json(r#"{"type":"ping","id":1}"#);
    let response = host.recv_json();
    assert_eq!(response["type"], "pong");
    assert_eq!(response["id"], 1);
    assert_eq!(response["gui_running"], false);
    assert_eq!(
        response["protocol"].as_u64(),
        Some(rdm_ipc::PROTOCOL_VERSION as u64)
    );
    assert_eq!(response["version"], env!("CARGO_PKG_VERSION"));

    host.close_stdin();
    let status = host.wait_for_exit(EXIT_TIMEOUT);
    assert!(status.success(), "clean shutdown must exit 0: {status:?}");

    let log = state.log_text();
    assert!(log.contains("ping: RDM GUI offline"), "log: {log}");
}

#[test]
fn status_end_to_end_through_mock_gui() {
    let state = TempState::new("status");
    let server = MockIpcServer::start("test-token-status");
    state.write_ipc_info(&server.ipc_info());
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    host.send_json(r#"{"type":"status","id":51}"#);
    let response = host.recv_json();
    assert_eq!(response["type"], "status");
    assert_eq!(response["id"], 51);
    assert_eq!(response["ok"], true);
    assert_eq!(response["gui_running"], true);
    assert_eq!(response["active"], 2);
    assert_eq!(
        response["global_speed_bps"].as_f64(),
        Some(1_048_576.0),
        "global_speed_bps must stay a floating point field"
    );
    let downloads = response["downloads"]
        .as_array()
        .expect("status response must carry a downloads array");
    assert_eq!(downloads.len(), 1);
    let download = &downloads[0];
    assert_eq!(download["id"], "dl_000001");
    assert_eq!(download["file_name"], "a.bin");
    assert_eq!(download["state"], "downloading");
    assert_eq!(download["downloaded"], 1_048_576);
    assert_eq!(download["total"], 10_485_760);
    assert_eq!(download["speed_bps"].as_f64(), Some(524_288.0));
    assert_eq!(download["connections"], 8);
    // Additive v0.2 thread counts must be forwarded verbatim.
    assert_eq!(download["active_connections"], 6);
    assert_eq!(download["target_connections"], 12);
    assert_eq!(download["peak_connections"], 16);

    // A status poll is a direct IPC query: it must not probe with `ping`
    // first (the extension polls it once per second), and it must not spool.
    let requests = server.wait_for_requests(1, RESPONSE_TIMEOUT);
    assert_eq!(
        requests.len(),
        1,
        "status must be the only request, got {requests:?}"
    );
    assert!(
        matches!(&requests[0].request, Request::Status),
        "expected Request::Status, got {:?}",
        requests[0].request
    );
    assert_eq!(requests[0].token.as_deref(), Some("test-token-status"));
    assert!(
        !state.rdm_dir().join("queue").exists(),
        "status polls must never spool"
    );

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());
}

#[test]
fn status_from_old_gui_defaults_thread_fields_to_zero() {
    let state = TempState::new("status-legacy");
    // Pre-v0.2 GUI: the status payload has none of the additive thread-count
    // fields. The host must still answer with the full shape (0 = unknown).
    let legacy_status_data = serde_json::json!({
        "downloads": [{
            "id": "dl_legacy",
            "file_name": "legacy.bin",
            "state": "downloading",
            "downloaded": 1024u64,
            "total": 2048u64,
            "speed_bps": 64.0,
            "connections": 8u64,
        }],
        "global_speed_bps": 64.0,
        "active": 1u64,
    });
    let server = MockIpcServer::start_with_status_data("test-token-legacy", legacy_status_data);
    state.write_ipc_info(&server.ipc_info());
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    host.send_json(r#"{"type":"status","id":61}"#);
    let response = host.recv_json();
    assert_eq!(response["type"], "status");
    assert_eq!(response["id"], 61);
    assert_eq!(response["ok"], true);
    assert_eq!(response["gui_running"], true);
    let downloads = response["downloads"]
        .as_array()
        .expect("status response must carry a downloads array");
    assert_eq!(downloads.len(), 1);
    let download = &downloads[0];
    // Existing fields keep being forwarded unchanged.
    assert_eq!(download["id"], "dl_legacy");
    assert_eq!(download["connections"], 8);
    assert_eq!(download["total"], 2048);
    // The three thread-count keys are always serialized; an old GUI yields 0,
    // which the extension interprets as "unknown" (falls back to connections).
    assert_eq!(download["active_connections"], 0);
    assert_eq!(download["target_connections"], 0);
    assert_eq!(download["peak_connections"], 0);

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());
}

#[test]
fn status_without_gui_returns_empty_snapshot() {
    let state = TempState::new("status-offline");
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    let started = Instant::now();
    host.send_json(r#"{"type":"status","id":52}"#);
    let response = host.recv_json();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a missing GUI must fail fast for a status poll"
    );
    assert_eq!(response["type"], "status");
    assert_eq!(response["id"], 52);
    assert_eq!(
        response["ok"], true,
        "GUI offline must still be ok:true so a 1 Hz poll does not spam errors"
    );
    assert_eq!(response["gui_running"], false);
    assert_eq!(response["active"], 0);
    assert_eq!(response["global_speed_bps"].as_f64(), Some(0.0));
    assert_eq!(
        response["downloads"]
            .as_array()
            .map(|downloads| downloads.len()),
        Some(0)
    );
    assert!(
        !state.rdm_dir().join("queue").exists(),
        "a status poll must never spool, even when the GUI is missing"
    );

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());

    let log = state.log_text();
    assert!(log.contains("status: RDM GUI unavailable"), "log: {log}");
}

#[test]
fn enqueue_end_to_end_through_mock_gui() {
    let state = TempState::new("enqueue");
    let server = MockIpcServer::start("test-token-123");
    state.write_ipc_info(&server.ipc_info());
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    host.send_json(
        r#"{
            "type": "enqueue",
            "id": 7,
            "launch_gui": true,
            "request": {
                "url": "https://example.com/files/big.iso?token=secret-signature",
                "file_name": "big.iso",
                "output_dir": null,
                "connections": 99,
                "headers": {"Accept": "application/octet-stream", "Authorization": "must-not-leak"},
                "cookie": "sid=topsecret; theme=dark",
                "referer": "https://example.com/page",
                "user_agent": "Mozilla/5.0 (test)",
                "speed_limit_bps": 0,
                "source": "firefox",
                "source_page": "https://example.com/page"
            }
        }"#,
    );
    let response = host.recv_json();
    assert_eq!(response["type"], "enqueued");
    assert_eq!(response["id"], 7);
    assert_eq!(response["download_id"], "dl_42");
    assert_eq!(response["gui_running"], true);

    let requests = server.wait_for_requests(2, RESPONSE_TIMEOUT);
    assert!(
        requests.len() >= 2,
        "expected a ping followed by an enqueue, got {requests:?}"
    );
    assert!(matches!(&requests[0].request, Request::Ping));
    for recorded in &requests {
        assert_eq!(
            recorded.token.as_deref(),
            Some("test-token-123"),
            "every request must carry the token from ipc.json"
        );
    }
    let enqueue = requests
        .iter()
        .find_map(|recorded| match &recorded.request {
            Request::Enqueue { item } => Some(item),
            _ => None,
        })
        .expect("an enqueue request must reach the GUI");
    assert_eq!(
        enqueue.download.url,
        "https://example.com/files/big.iso?token=secret-signature"
    );
    assert_eq!(enqueue.download.file_name.as_deref(), Some("big.iso"));
    assert_eq!(
        enqueue.download.connections,
        rdm_core::MAX_CONNECTIONS,
        "connections must be clamped host-side"
    );
    assert_eq!(
        enqueue.download.cookie.as_deref(),
        Some("sid=topsecret; theme=dark")
    );
    assert_eq!(
        enqueue.download.referer.as_deref(),
        Some("https://example.com/page")
    );
    assert_eq!(
        enqueue.download.user_agent.as_deref(),
        Some("Mozilla/5.0 (test)")
    );
    assert_eq!(
        enqueue.download.headers.get("Accept").map(String::as_str),
        Some("application/octet-stream")
    );
    assert!(
        !enqueue.download.headers.contains_key("Authorization"),
        "Authorization must be filtered out"
    );
    assert_eq!(enqueue.source, "firefox");
    assert_eq!(
        enqueue.source_page.as_deref(),
        Some("https://example.com/page")
    );

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());

    // Privacy: secrets must not appear in the log file.
    let log = state.log_text();
    assert!(
        log.contains("cookie accepted: host=example.com"),
        "log: {log}"
    );
    assert!(!log.contains("topsecret"), "cookie value leaked: {log}");
    assert!(!log.contains("secret-signature"), "url query leaked: {log}");
    assert!(!log.contains("must-not-leak"), "header value leaked: {log}");
}

#[test]
fn enqueue_spools_when_gui_cannot_be_started() {
    let state = TempState::new("spool");
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    let started = Instant::now();
    host.send_json(
        r#"{"type":"enqueue","id":11,"request":{
            "url":"https://example.com/spooled.bin",
            "file_name":"spooled.bin",
            "connections":4,
            "headers":{
                "Accept":"application/octet-stream",
                "Authorization":"Bearer must-not-persist",
                "Cookie":"sid=a=1"
            },
            "cookie":"a=1",
            "source":"firefox",
            "source_page":"https://example.com/list"
        }}"#,
    );
    let response = host.recv_json();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a missing GUI binary must fail fast, not wait for the 8 s GUI timeout"
    );
    assert_eq!(response["type"], "queued");
    assert_eq!(response["id"], 11);
    assert_eq!(response["reason"], "gui_unavailable");
    assert_eq!(
        response["credentials_stripped"], true,
        "queued response must report that credentials were stripped before spooling"
    );
    let spool_path = PathBuf::from(response["spool"].as_str().expect("spool path"));
    assert!(
        spool_path.is_file(),
        "spool file must exist: {spool_path:?}"
    );
    assert!(spool_path.starts_with(state.rdm_dir().join("queue")));

    let text = std::fs::read_to_string(&spool_path).unwrap();
    let item: rdm_ipc::EnqueueItem = serde_json::from_str(&text).expect("spool = EnqueueItem JSON");
    assert_eq!(item.download.url, "https://example.com/spooled.bin");
    assert_eq!(item.download.file_name.as_deref(), Some("spooled.bin"));
    assert_eq!(item.download.connections, 4);
    assert_eq!(item.source, "firefox");
    assert_eq!(
        item.source_page.as_deref(),
        Some("https://example.com/list")
    );
    // Privacy: the spool must not contain cookies or authorization material,
    // while non-credential headers survive.
    assert_eq!(
        item.download.cookie, None,
        "cookie must never be persisted to the spool"
    );
    assert_eq!(
        item.download.headers.get("Accept").map(String::as_str),
        Some("application/octet-stream")
    );
    for name in ["Authorization", "Cookie"] {
        assert!(
            !item.download.headers.contains_key(name),
            "{name} must never be persisted to the spool"
        );
    }
    for secret in ["a=1", "must-not-persist"] {
        assert!(
            !text.contains(secret),
            "spool file leaked credential material: {secret}"
        );
    }

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());

    // The log must not contain credential values either.
    let log = state.log_text();
    assert!(
        log.contains("credentials_stripped=true"),
        "log must mention stripped credentials: {log}"
    );
    for secret in ["a=1", "must-not-persist"] {
        assert!(
            !log.contains(secret),
            "native.log leaked credential material: {secret}"
        );
    }
}

#[test]
fn invalid_requests_get_structured_errors_and_host_keeps_serving() {
    let state = TempState::new("invalid");
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    // `file:` URLs must never reach the download engine.
    host.send_json(r#"{"type":"enqueue","id":21,"request":{"url":"file:///C:/Windows/win.ini"}}"#);
    let response = host.recv_json();
    assert_eq!(response["type"], "error");
    assert_eq!(response["id"], 21);
    assert_eq!(response["code"], "invalid_request");

    // Malformed JSON: id is unknown, but the host must answer and continue.
    host.send_raw_frame(b"{not json}");
    let response = host.recv_json();
    assert_eq!(response["type"], "error");
    assert_eq!(response["code"], "invalid_request");

    // The channel is still usable afterwards.
    host.send_json(r#"{"type":"ping","id":22}"#);
    let response = host.recv_json();
    assert_eq!(response["type"], "pong");
    assert_eq!(response["id"], 22);
    assert_eq!(response["gui_running"], false);

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());
}

#[test]
fn ai_parse_end_to_end_through_mock_gui() {
    let state = TempState::new("ai-parse");
    let server = MockIpcServer::start("test-token-ai");
    state.write_ipc_info(&server.ipc_info());
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    host.send_json(
        r#"{
            "type": "ai_parse",
            "id": 31,
            "url": "https://example.com/page?signature=secret-signature",
            "title": "Example Page",
            "source_page": "https://example.com/list",
            "include_credentials": true,
            "cookie": "sid=topsecret; theme=dark",
            "referer": "https://example.com/list",
            "user_agent": "Mozilla/5.0 (test)",
            "page_url": "https://example.com/page?signature=secret-signature",
            "page_title": "Example Page",
            "page_links": ["https://cdn.example.com/a.mp4", "https://cdn.example.com/b.zip?token=secret-signature"]
        }"#,
    );
    let response = host.recv_json();
    assert_eq!(response["type"], "ai_parse_accepted");
    assert_eq!(response["id"], 31);
    assert_eq!(response["ok"], true);
    assert_eq!(response["request_id"], 12);
    assert_eq!(response["gui_running"], true);

    let requests = server.wait_for_requests(2, RESPONSE_TIMEOUT);
    assert!(
        requests.len() >= 2,
        "expected a ping followed by an ai_parse, got {requests:?}"
    );
    assert!(matches!(&requests[0].request, Request::Ping));
    let ai_parse = requests
        .iter()
        .find_map(|recorded| match &recorded.request {
            Request::AiParse {
                url,
                source_page,
                title,
                include_credentials,
                cookie,
                referer,
                user_agent,
                page_url,
                page_title,
                page_links,
                ..
            } => Some((
                url.clone(),
                source_page.clone(),
                title.clone(),
                *include_credentials,
                cookie.clone(),
                referer.clone(),
                user_agent.clone(),
                page_url.clone(),
                page_title.clone(),
                page_links.clone(),
            )),
            _ => None,
        })
        .expect("an ai_parse request must reach the GUI");
    assert_eq!(
        ai_parse.0, "https://example.com/page?signature=secret-signature",
        "the URL (including its query) must reach the GUI verbatim"
    );
    assert_eq!(ai_parse.1.as_deref(), Some("https://example.com/list"));
    assert_eq!(ai_parse.2.as_deref(), Some("Example Page"));
    assert!(ai_parse.3, "include_credentials must be forwarded");
    assert_eq!(ai_parse.4.as_deref(), Some("sid=topsecret; theme=dark"));
    assert_eq!(ai_parse.5.as_deref(), Some("https://example.com/list"));
    assert_eq!(ai_parse.6.as_deref(), Some("Mozilla/5.0 (test)"));
    // P4b-b2: the validated page link snapshot reaches the GUI.
    assert_eq!(
        ai_parse.7.as_deref(),
        Some("https://example.com/page?signature=secret-signature")
    );
    assert_eq!(ai_parse.8.as_deref(), Some("Example Page"));
    assert_eq!(
        ai_parse.9,
        vec![
            "https://cdn.example.com/a.mp4".to_string(),
            "https://cdn.example.com/b.zip?token=secret-signature".to_string()
        ]
    );

    // AI parse is a live GUI-only operation: nothing may be spooled to disk.
    assert!(
        !state.rdm_dir().join("queue").exists(),
        "AI parse requests must never be spooled"
    );

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());

    // Privacy: the log may mention the request but never credential values,
    // URL query parameters or snapshot link values.
    let log = state.log_text();
    assert!(log.contains("ai_parse request accepted"), "log: {log}");
    for secret in [
        "topsecret",
        "secret-signature",
        "sid=",
        "cdn.example.com/a.mp4",
    ] {
        assert!(!log.contains(secret), "native.log leaked {secret}: {log}");
    }
}

#[test]
fn ai_parse_without_gui_returns_gui_unavailable_and_never_spools() {
    let state = TempState::new("ai-parse-offline");
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    let started = Instant::now();
    host.send_json(
        r#"{"type":"ai_parse","id":32,"url":"https://example.com/page",
            "include_credentials":true,"cookie":"sid=topsecret"}"#,
    );
    let response = host.recv_json();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a missing GUI must fail fast for AI parse"
    );
    assert_eq!(response["type"], "error");
    assert_eq!(response["id"], 32);
    assert_eq!(response["code"], "gui_unavailable");
    assert!(
        !state.rdm_dir().join("queue").exists(),
        "AI parse must not fall back to the spool queue"
    );

    let log = state.log_text();
    assert!(log.contains("AI parse cannot be queued"), "log: {log}");
    assert!(!log.contains("topsecret"), "cookie value leaked: {log}");

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());
}

#[test]
fn ai_parse_drops_cookie_when_credentials_are_not_requested() {
    let state = TempState::new("ai-parse-nocreds");
    let server = MockIpcServer::start("test-token-ai-nocreds");
    state.write_ipc_info(&server.ipc_info());
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    host.send_json(
        r#"{"type":"ai_parse","id":33,"url":"https://example.com/page",
            "include_credentials":false,"cookie":"sid=topsecret"}"#,
    );
    let response = host.recv_json();
    assert_eq!(response["type"], "ai_parse_accepted");

    let requests = server.wait_for_requests(2, RESPONSE_TIMEOUT);
    let cookie = requests
        .iter()
        .find_map(|recorded| match &recorded.request {
            Request::AiParse {
                include_credentials,
                cookie,
                ..
            } => Some((*include_credentials, cookie.clone())),
            _ => None,
        });
    let (include_credentials, cookie) = cookie.expect("ai_parse must reach the GUI");
    assert!(!include_credentials);
    assert_eq!(cookie, None, "cookie must not be forwarded without consent");

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());
    let log = state.log_text();
    assert!(!log.contains("topsecret"), "cookie value leaked: {log}");
}

#[test]
fn ai_parse_rejects_invalid_and_oversized_urls_and_keeps_serving() {
    let state = TempState::new("ai-parse-badurl");
    let mut host = HostProcess::spawn(&state, &unreachable_gui_path(&state));

    for (id, url) in [
        (41, "file:///C:/Windows/win.ini".to_string()),
        (42, format!("https://example.com/{}", "a".repeat(5000))),
    ] {
        let message = serde_json::json!({
            "type": "ai_parse",
            "id": id,
            "url": url,
        });
        host.send_json(&message.to_string());
        let response = host.recv_json();
        assert_eq!(response["type"], "error");
        assert_eq!(response["id"], id);
        assert_eq!(response["code"], "invalid_request");
    }

    // The channel stays usable after a rejected AI parse.
    host.send_json(r#"{"type":"ping","id":43}"#);
    let response = host.recv_json();
    assert_eq!(response["type"], "pong");
    assert_eq!(response["id"], 43);

    host.close_stdin();
    assert!(host.wait_for_exit(EXIT_TIMEOUT).success());
}

#[test]
fn version_flag_prints_without_touching_the_protocol_channel() {
    let output = Command::new(env!("CARGO_BIN_EXE_rdm-native"))
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "unexpected --version output: {stdout:?}"
    );
}
